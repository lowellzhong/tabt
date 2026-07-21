# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A native macOS terminal emulator written in Rust on top of AppKit, built as a sequence of numbered milestones (step 0 = window, 1 = PTY echo, 2 = dumb rendering, 3 = VT parser). Steps 0–3 are done: it is a working multi-tab terminal with a self-drawn sidebar, groups, themes, fonts, settings, scrollback, and layout persistence. Remaining known gaps in the core: no custom tab stops, and the scrollback is not reflowed on resize.

Keep new code comments and UI strings in English (the Rust sources are; the Makefile and `Cargo.toml` still carry some older Chinese comments).

## Commands

Run from the repository root (the workspace root — `Cargo.toml` and `Makefile` live here). The Makefile is the canonical entry point:

- `make test` — `tabt-core` unit tests (`cargo test -p tabt-core`). Pure logic, runs on any platform.
- `make run` — build, bundle into `TabT.app`, ad-hoc codesign, kill any old instance, and `open` it. **The GUI must run as a `.app` bundle** — a bare `target/release/tabt` won't get focus or a menu bar (normal macOS behavior for non-bundled processes).
- `make echo` — the step-1 PTY echo loop (`cargo run --release --bin pty-echo`). **Must be run in a real terminal** (Terminal.app/iTerm); it calls `tcgetattr` on stdin and dies immediately without a tty (e.g. an IDE output panel).
- `make cert` — one-time: create a stable local "TabT Dev" signing identity (`bundle/make-dev-cert.sh`). Without it `make run` falls back to ad-hoc signing, which changes the app's designated requirement on every rebuild, so macOS re-prompts for TCC folder access each time.
- `make bloat` — size audit (`cargo bloat`, needs `cargo install cargo-bloat`); the stripped binary should be ~1 MB.
- Single test: `cargo test -p tabt-core <test_name>` (e.g. `cargo test -p tabt-core print_and_wrap`).

There is no separate lint step (no clippy/fmt target).

The release profile is aggressively size-tuned (`opt-level="z"`, `lto`, `codegen-units=1`, `panic="abort"`, `strip`). `panic="abort"` means a panic anywhere terminates the whole process rather than unwinding — relevant when touching code that parses PTY output or user input.

## Architecture

Two-crate Cargo workspace with a strict dependency direction: `tabt-app` → `tabt-core`, never the reverse.

### `tabt-core` — the VT/ANSI engine

`tabt-core/src/lib.rs`, one file, **deliberately zero-dependency** so it compiles and tests on any platform independent of macOS. Defines `Cell`, `Color`, and `Grid` (fixed-size screen grid in a flat `Vec<Cell>`, indexed `row * cols + col`).

`Grid::feed(&[u8])` is a VT500-style parser state machine (ground / escape / csi / osc, modeled on Paul Williams' diagram) covering SGR pen attributes, cursor movement, erase/insert/delete, scroll regions, save/restore cursor, DEC private modes, alternate screen, OSC title and OSC 7 cwd, UTF-8 and wide characters (a wide glyph occupies two cells, the second flagged `WIDE_TRAILER`). Terminal→host replies (DSR/DA) are queued internally and drained by the app layer via `take_replies()`; other state the renderer/input layer reads back through `cwd()`, `cursor_visible()`, `app_cursor_keys()`, `bracketed_paste()`.

Lines scrolled off the top are pushed into a capped `history` deque. **The renderer must read cells through `view_cell()`, not `cell()`** — `view_cell()` resolves the current scrollback viewport (`view_offset()`, moved by `scroll_view()` / `scroll_to_bottom()`); `cell()` is raw screen access. The alt screen has no scrollback, and lines removed by DL/ED must not enter it. `to_lines()` exports rows as trimmed strings, used by the ~40 unit tests at the bottom of the same file.

### `tabt-app` — the macOS/AppKit layer

Two binaries:

- **`src/main.rs` (bin `tabt`)** — process setup and window/layout construction only. Sets `TERM`/`TERM_PROGRAM` env *before* AppKit starts any thread so forked children inherit a clean env (see the comment there about Apple_Terminal's `~/.zsh_sessions` clash). Builds the container view (sidebar | divider | terminal host | floating toggle), then hands off to `AppController`.
- **`src/bin/pty_echo.rs` (bin `pty-echo`)** — a self-contained PTY loop using **raw `libc`**, no AppKit. It exercises the low-level PTY details (`setsid`, controlling terminal, fd inheritance, raw mode, `SIGWINCH`) that were later lifted into the GUI. Kept permanently as a debugging tool.

**Everything runs on the main thread**, enforced at the type level via `MainThreadMarker`. All custom views are `NSView` subclasses declared with `objc2`'s `declare_class!`.

#### The controller / callback pattern (read `app.rs` first)

`AppController` (`app.rs`) is a plain Rust struct in an `Rc`, created in `main()` and kept alive until `app.run()` returns. It owns every session (each tab = one PTY fd + one `TermView` + a shell pid + a GCD reader token) and handles create/switch/move/close/rename, sidebar layout, theme/font changes, and persistence.

Because ObjC objects can't hold Rust `Rc`s, the sidebar, divider, toggle, menu target, and settings dialog each store a **raw `*const AppController`** and call back through it (plus `extern "C"` trampolines for GCD callbacks). This is sound only because the `Rc` outlives the event loop — preserve that invariant when adding a new view. There is intentionally no reference cycle: the controller owns the views, the views only point back.

#### Data flow

PTY master fd → GCD dispatch source (`view.rs`, hand-declared `dispatch_*` externs) → `Grid::feed` → `setNeedsDisplay` → `drawRect:` merges runs of adjacent cells with identical attributes and draws each run once with Core Text (backgrounds/underlines via `NSRectFill`). `keyDown:`/`NSTextInputClient` write back to the fd; `take_replies()` output is written back after each feed; `scrollWheel:` moves the scrollback viewport instead. A window/font/sidebar resize recomputes cols/rows from the cell metrics, reflows the grid, and notifies the shell with `TIOCSWINSZ`.

`pty.rs` spawns each tab's login zsh (`openpty` + `fork` + `setsid` + `TIOCSCTTY` + `execv`), returns a non-blocking master fd, and installs a one-time `SIGCHLD` reaper. A spawn failure must fail only that tab, never the app. `has_foreground_job()` compares `tcgetpgrp` against the shell pid to warn before closing a tab with a running job. A shell that exits does **not** close its tab: the session is marked ended, `TermView` shows a placeholder, and Enter respawns into the tab's last cwd (`AppController::restart_tab`).

#### Views and state

The sidebar, header, divider, toggle, and placeholder are **all self-drawn** (custom `drawRect:`, own hit-testing and drag handling) — the settings dialog (`settings_dialog.rs`) is the lone user of standard AppKit controls. `theme.rs` and `settings.rs` hold main-thread-only global state (current theme; font family/size and derived cell metrics) that the drawing code reads on demand, so views don't depend on each other. `config.rs` persists layout to `~/.tabt/layout.conf` in **hand-written INI** (no serde, to keep the binary small); values are unescaped, so renaming must forbid newlines.

### objc2 version pinning (important)

`objc2 0.5` + `objc2-foundation 0.2` + `objc2-app-kit 0.2` are a **matched set** of APIs (newer major versions exist but are intentionally not used). If a type/method fails to compile, first check whether cargo upgraded a crate out of the set (`cargo tree | grep objc2`) and whether `objc2-app-kit`'s feature list is missing an entry — that crate splits features per Objective-C class, so each `NS*` type used must be enabled as a feature in `tabt-app/Cargo.toml`.

## Bundle

`bundle/Info.plist` is the app manifest (bundle id `dev.local.tabt`, min macOS 12.0, version tracked alongside the crate versions). `make run` copies it plus `bundle/AppIcon.icns` and the binary into `TabT.app/Contents/`.

`bundle/AppIcon.icns` is generated from `tabt.png` — do not hand-edit it. Use the tracked `app-icon` skill (`.claude/skills/app-icon/`), which documents the non-obvious macOS icon constraints. `.gitignore` deliberately tracks `.claude/skills/` while ignoring the rest of `.claude/`.
