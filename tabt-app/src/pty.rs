//! PTY spawning used in step 2.
//!
//! Brings over the openpty / fork / exec details verified one by one in step 1's `pty-echo`,
//! but no longer takes over stdin — the GUI process has no controlling terminal of its own. This only
//! spawns a PTY running a login zsh and returns the master-side fd for the GUI to read/write. SIGWINCH/window-size following
//! is left to a later milestone (the window size is fixed for now, cols/rows computed once).

use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::process::exit;
use std::ptr;
use std::sync::Once;

/// Spawns a PTY running a login zsh and returns the master fd (already set to non-blocking) plus
/// the shell's pid, or `None` if the PTY/process itself could not be created (`openpty`/`fork`
/// failed — e.g. the process is out of file descriptors). A `None` here means only this one new
/// tab fails to open; it must never bring down the rest of the app (existing tabs keep running).
/// The pid is the shell's own pid *and* its process group id (it calls `setsid()`), letting the
/// caller compare against `tcgetpgrp` to detect a foreground job (see `has_foreground_job`).
pub fn spawn(cols: u16, rows: u16, cwd: &str) -> Option<(RawFd, libc::pid_t)> {
    install_reaper();
    unsafe { spawn_inner(cols, rows, cwd) }
}

/// Install a SIGCHLD handler (once) that reaps any exited child shell. Without this, closing a
/// tab or a shell exiting on its own (`exit`) leaves a zombie process behind — closing the
/// master fd alone does not reap the child. Reaping via the signal handler (rather than an
/// explicit `waitpid` in teardown code) means it works regardless of exactly when/how the child
/// exits, and never risks blocking the main thread waiting on a child that's slow to die.
fn install_reaper() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        libc::signal(libc::SIGCHLD, reap_children as *const () as libc::sighandler_t);
    });
}

/// Signal handler: reap every exited child without blocking. Async-signal-safe (only calls the
/// `waitpid` syscall in a loop; no allocation, no I/O).
extern "C" fn reap_children(_sig: i32) {
    loop {
        let pid = unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

unsafe fn spawn_inner(cols: u16, rows: u16, cwd: &str) -> Option<(RawFd, libc::pid_t)> {
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };

    let (mut master, mut slave): (RawFd, RawFd) = (0, 0);
    if libc::openpty(
        &mut master,
        &mut slave,
        ptr::null_mut(),
        ptr::null_mut(), // default termios is fine
        &ws as *const _ as *mut _,
    ) != 0
    {
        // Parent-side failure: fail just this one tab, not the whole app (unlike the child-side
        // die()s below, which only ever terminate the freshly-forked child process).
        warn("openpty");
        return None;
    }

    // Prepare paths before fork (the child only does chdir, no memory allocation after fork):
    // prefer the restored cwd, fall back to HOME.
    let home = std::env::var("HOME").ok().and_then(|h| CString::new(h).ok());
    let start = if cwd.is_empty() { None } else { CString::new(cwd).ok() };

    match libc::fork() {
        -1 => {
            warn("fork");
            libc::close(master);
            libc::close(slave);
            None
        }
        0 => {
            // ---- Child process: make slave the controlling terminal, exec the login shell ----
            libc::close(master);
            // New session + controlling terminal (skip either step and ^C/^Z job control and /dev/tty both break)
            if libc::setsid() == -1 {
                die("setsid");
            }
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                die("ioctl(TIOCSCTTY)");
            }
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            // Prefer entering the restored cwd; on failure (directory deleted) or when not provided, fall back to HOME.
            let entered = start
                .as_ref()
                .map(|d| libc::chdir(d.as_ptr()) == 0)
                .unwrap_or(false);
            if !entered {
                if let Some(ref home) = home {
                    libc::chdir(home.as_ptr());
                }
            }
            // TERM is set by the parent before starting AppKit (see main); the child inherits it directly.
            // argv[0] with a "-" prefix → zsh starts as a login shell, loading ~/.zprofile.
            let path = CString::new("/bin/zsh").unwrap();
            let argv0 = CString::new("-zsh").unwrap();
            let argv = [argv0.as_ptr(), ptr::null()];
            libc::execv(path.as_ptr(), argv.as_ptr());
            die("execv(/bin/zsh)");
        }
        child_pid => {
            // ---- Parent process (GUI): keep master, hand slave to the child ----
            libc::close(slave);
            // Non-blocking: when the GCD read source fires, drain all readable data at once, never blocking the main thread.
            let flags = libc::fcntl(master, libc::F_GETFL, 0);
            libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
            Some((master, child_pid))
        }
    }
}

/// Whether the shell currently has a foreground job other than itself (e.g. vim, a build, ssh)
/// running — used to warn before closing a tab or quitting rather than silently killing it.
/// `shell_pid` is the pid `spawn` returned (also the shell's own process group id, since it
/// calls `setsid()`); if the terminal's foreground process group differs, something else has
/// taken over the foreground. Returns `false` (nothing to protect) if the fd is no longer a
/// valid controlling terminal, e.g. the tab is already being torn down.
pub fn has_foreground_job(master_fd: RawFd, shell_pid: libc::pid_t) -> bool {
    let pgrp = unsafe { libc::tcgetpgrp(master_fd) };
    pgrp > 0 && pgrp != shell_pid
}

/// A child-side failure (setsid/ioctl/execv): only the freshly-forked child process exits here,
/// never the parent GUI process, so it's safe to always terminate on these.
fn die(ctx: &str) -> ! {
    eprintln!("tabt: {} failed: {}", ctx, std::io::Error::last_os_error());
    exit(1);
}

/// A parent-side failure: log it but let the caller handle returning `None` — must never exit
/// the whole process, since other tabs may already be running.
fn warn(ctx: &str) {
    eprintln!("tabt: {} failed: {}", ctx, std::io::Error::last_os_error());
}
