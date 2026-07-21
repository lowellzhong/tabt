#!/usr/bin/env node
// Launch and drive TabT.app from a script.
//
// TabT is a native AppKit GUI with no remote-control surface, and macOS gates the
// obvious ways in (screencapture / System Events keystrokes) behind TCC permissions
// that a headless agent does not have. So this driver goes in through the one channel
// the app hands us for free: it is a terminal emulator, so it spawns a login zsh in a
// PTY. We point the app at a scratch $HOME whose .zshrc is ours, and that shell becomes
// our agent inside the running GUI -- it queries the terminal and writes the answers to
// files we read back.
//
// A DSR round-trip (`ESC[6n` -> `ESC[row;colR`) exercises the entire data flow in the
// live app: shell writes to the PTY slave -> GCD dispatch source reads the master ->
// Grid::feed parses -> take_replies() -> write back to the PTY. Preceding it with a CUP
// makes the reply an assertion about the parser, not just a liveness check.
//
// Usage:
//   node .claude/skills/run-tabt/driver.mjs smoke    # build-free full run, asserts, exits nonzero on failure
//   node .claude/skills/run-tabt/driver.mjs launch   # leave it running, prints the pid
//   node .claude/skills/run-tabt/driver.mjs quit     # stop the instance this driver started

import { spawn, execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync, readFileSync, existsSync, realpathSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const APP_BIN = join(REPO, 'TabT.app/Contents/MacOS/tabt');
const HOME = join(process.env.TMPDIR || '/tmp', 'tabt-driver-home');
const PROOF = join(HOME, 'proof');
const PIDFILE = join(HOME, 'tabt.pid');
const MARKER_CWD = join(HOME, 'marker-cwd');

const log = (...a) => console.log(...a);

// ---------------------------------------------------------------------------
// Safety: this session may itself be running inside a TabT window. Killing by
// name (`killall tabt`, which is what `make run` does) would take down the
// terminal hosting the agent. We only ever signal a pid we spawned ourselves.
// ---------------------------------------------------------------------------
function ancestorTabtPids() {
  const pids = new Set();
  let pid = process.pid;
  for (let i = 0; i < 40 && pid > 1; i++) {
    let out;
    try {
      out = execFileSync('ps', ['-o', 'ppid=,comm=', '-p', String(pid)], { encoding: 'utf8' }).trim();
    } catch { break; }
    if (!out) break;
    const m = out.match(/^(\d+)\s+(.*)$/);
    if (!m) break;
    if (/tabt$/.test(m[2].trim())) pids.add(pid);
    pid = Number(m[1]);
  }
  return pids;
}

function killOurs(pid) {
  const forbidden = ancestorTabtPids();
  if (forbidden.has(pid)) {
    throw new Error(`refusing to kill pid ${pid}: it is an ancestor of this process (the TabT hosting this session)`);
  }
  try { process.kill(pid, 'SIGTERM'); } catch { /* already gone */ }
}

// ---------------------------------------------------------------------------
// The fixture $HOME. TabT reads $HOME/.tabt/layout.conf (config.rs `dir()`), and the
// login zsh it spawns reads $HOME/.zshrc -- so one env var isolates the app's config
// from the real ~/.tabt AND gives us our hook inside the tab.
// ---------------------------------------------------------------------------
// NOTE: String.raw stops backslash escapes being eaten by JS, but it does NOT stop
// ${...} interpolation -- so this shell code must avoid ${...} entirely. That is why
// the ESC-byte -> "ESC" prettifying happens in Node (see `readReply`) rather than in
// zsh's ${var//.../...}.
const ZSHRC = String.raw`
# Fixture rc sourced by the login zsh that TabT spawns inside a tab.
# Everything here runs *inside the running GUI app*.
PROOF="$TABT_PROOF"
mkdir -p "$PROOF"

print -r -- "shell=$0 pid=$$ term=$TERM term_program=$TERM_PROGRAM cwd=$PWD" > "$PROOF/boot.txt"

# Ask the terminal a question and read its answer back off the tty.
# Needs raw mode: otherwise the reply is line-buffered and echoed back at us.
tabt_query() {
  local old resp c i
  old=$(stty -g </dev/tty)
  stty raw -echo min 0 time 20 </dev/tty      # up to 2.0s for a reply
  printf '%b' "$1" > /dev/tty
  resp=""
  for i in {1..32}; do
    c=$(dd bs=1 count=1 </dev/tty 2>/dev/null)
    [[ -z $c ]] && break
    resp+="$c"
    [[ $c == "$2" ]] && break
  done
  stty "$old" </dev/tty
  printf '%s' "$resp"
}

# 1. Plain DSR: proves the full PTY -> parser -> reply -> PTY loop is live.
tabt_query '\033[6n' R > "$PROOF/dsr_plain.txt"

# 2. CUP then DSR: the reply is now an assertion that the parser moved the cursor.
tabt_query '\033[10;5H\033[6n' R > "$PROOF/dsr_after_cup.txt"

# 3. Device attributes.
tabt_query '\033[c' c > "$PROOF/da.txt"

# 4. OSC 7 cwd report -- TabT percent-decodes this into Grid.cwd and persists it.
mkdir -p "$TABT_MARKER_CWD"
cd "$TABT_MARKER_CWD"
printf '\033]7;file://%s%s\a' "$HOST" "$PWD"

# 5. Render some real output (SGR + wide chars) so the window is not blank.
print -P '%F{green}TabT driver%f: SGR + wide chars 中文 ok'

print -r -- done > "$PROOF/ready.txt"
`;

function setupHome() {
  rmSync(HOME, { recursive: true, force: true });
  mkdirSync(PROOF, { recursive: true });
  writeFileSync(join(HOME, '.zshrc'), ZSHRC);
  // A login shell also reads .zprofile; keep it quiet and predictable.
  writeFileSync(join(HOME, '.zprofile'), '# intentionally empty (fixture)\n');
}

function launch({ fresh = true } = {}) {
  if (!existsSync(APP_BIN)) {
    throw new Error(`missing ${APP_BIN} -- run \`make\` first (NOT \`make run\`, see SKILL.md)`);
  }
  if (fresh) setupHome();
  const child = spawn(APP_BIN, [], {
    env: {
      ...process.env,
      HOME,
      TABT_PROOF: PROOF,
      TABT_MARKER_CWD: MARKER_CWD,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: true,
  });
  child.unref();
  writeFileSync(PIDFILE, String(child.pid));
  return child;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(file, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(file)) return true;
    await sleep(150);
  }
  return false;
}

const read = (f) => (existsSync(f) ? readFileSync(f, 'utf8').trim() : '');

// Terminal replies are raw bytes; show ESC as "ESC" so they are readable and assertable.
const readReply = (f) => read(f).replace(/\x1b/g, 'ESC');

async function smoke() {
  const results = [];
  const check = (name, ok, detail) => {
    results.push({ name, ok, detail });
    log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  -- ${detail}` : ''}`);
  };

  log(`repo:   ${REPO}`);
  log(`app:    ${APP_BIN}`);
  log(`HOME:   ${HOME}  (isolated; your real ~/.tabt is untouched)`);
  log('');

  const child = launch();
  log(`launched pid ${child.pid}`);
  try {
    const ready = await waitFor(join(PROOF, 'ready.txt'), 20000);
    check('fixture shell ran inside a tab', ready, ready ? '' : 'no proof/ready.txt after 20s');

    const boot = read(join(PROOF, 'boot.txt'));
    check('login shell + TERM wiring', /term=xterm-256color/.test(boot) && /term_program=TabT/.test(boot), boot);

    const plain = readReply(join(PROOF, 'dsr_plain.txt'));
    check('DSR round-trip through the live app', /^ESC\[\d+;\d+R$/.test(plain), JSON.stringify(plain));

    const cup = readReply(join(PROOF, 'dsr_after_cup.txt'));
    check('parser honoured CUP (expect ESC[10;5R)', cup === 'ESC[10;5R', JSON.stringify(cup));

    const da = readReply(join(PROOF, 'da.txt'));
    check('device attributes reply', /^ESC\[\?\d/.test(da), JSON.stringify(da));

    // The app persists layout (incl. OSC 7 cwd) to $HOME/.tabt/layout.conf.
    const conf = join(HOME, '.tabt/layout.conf');
    const confExists = await waitFor(conf, 3000);
    check('wrote layout.conf under the scratch HOME', confExists, conf);

    const alive = (() => { try { process.kill(child.pid, 0); return true; } catch { return false; } })();
    check('app still running (no crash)', alive);
  } finally {
    killOurs(child.pid);
    await sleep(400);
    log(`stopped pid ${child.pid}\n`);
  }

  // -------------------------------------------------------------------------
  // Phase 2: restore. Seed layout.conf with a cwd and relaunch -- the shell must
  // come up *in that directory*. Exercises config parse -> tab restore -> PTY
  // spawn-in-cwd, which is the half of the persistence path we can observe
  // without a clean quit (persist() only runs from applicationWillTerminate:,
  // and SIGTERM skips it -- see Gotchas in SKILL.md).
  // -------------------------------------------------------------------------
  mkdirSync(MARKER_CWD, { recursive: true });
  rmSync(join(PROOF, 'boot.txt'), { force: true });
  rmSync(join(PROOF, 'ready.txt'), { force: true });
  writeFileSync(join(HOME, '.tabt/layout.conf'),
    `[settings]\nstyle = Default\n\n[tabs]\ntab = Restored\ncwd = ${MARKER_CWD}\n`);

  const child2 = launch({ fresh: false });
  log(`relaunched pid ${child2.pid} against a seeded layout`);
  try {
    const ready2 = await waitFor(join(PROOF, 'ready.txt'), 20000);
    check('restored session started', ready2);
    const boot2 = read(join(PROOF, 'boot.txt'));
    const want = realpathSync(MARKER_CWD);
    const got = (boot2.match(/cwd=(.*)$/) || [])[1] || '';
    check('shell spawned in the restored cwd', got === want, `${got || '(none)'} vs ${want}`);
  } finally {
    killOurs(child2.pid);
    await sleep(400);
    log(`stopped pid ${child2.pid}`);
  }

  const failed = results.filter((r) => !r.ok);
  log(`\n${results.length - failed.length}/${results.length} checks passed`);
  return failed.length === 0 ? 0 : 1;
}

const cmd = process.argv[2] || 'smoke';
if (cmd === 'smoke') {
  process.exit(await smoke());
} else if (cmd === 'launch') {
  const c = launch();
  log(`pid ${c.pid}  HOME=${HOME}`);
  log(`quit with: node ${process.argv[1]} quit`);
} else if (cmd === 'quit') {
  const pid = Number(read(PIDFILE));
  if (pid) { killOurs(pid); log(`stopped pid ${pid}`); } else { log('no pidfile'); }
} else {
  log('usage: driver.mjs [smoke|launch|quit]');
  process.exit(2);
}
