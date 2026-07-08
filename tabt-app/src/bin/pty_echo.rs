//! Step 1: PTY echo loop (does not touch any UI).
//!
//! Nest a real zsh inside your current terminal:
//!     cargo run --bin pty-echo        (type exit or ⌃D to quit)
//!
//! Acceptance criteria: inside it you can run commands normally, run `vim` and `htop`,
//! and after resizing the window the `stty size` values follow along. This verifies all
//! the key details of the PTY layer:
//! setsid / controlling terminal / fd inheritance / raw mode / SIGWINCH forwarding.
//! This logic will later be moved verbatim into the GUI app; this tool is kept permanently as a debugger.

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::process::exit;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigwinch(_: libc::c_int) {
    WINCH.store(true, Ordering::SeqCst);
}

fn errno_die(ctx: &str) -> ! {
    eprintln!("pty-echo: {} failed: {}", ctx, std::io::Error::last_os_error());
    exit(1);
}

fn main() {
    unsafe { real_main() }
}

unsafe fn real_main() -> ! {
    // ---- 1. Read the current terminal's attributes and window size as the child PTY's initial state ----
    let mut term = MaybeUninit::<libc::termios>::uninit();
    if libc::tcgetattr(libc::STDIN_FILENO, term.as_mut_ptr()) != 0 {
        errno_die("tcgetattr (are you running inside a real terminal?)");
    }
    let orig_term = term.assume_init();

    let mut ws = MaybeUninit::<libc::winsize>::zeroed();
    libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr());
    let ws = ws.assume_init();

    // ---- 2. openpty: obtain the master/slave pair in one call ----
    let (mut master, mut slave) = (0, 0);
    if libc::openpty(
        &mut master,
        &mut slave,
        ptr::null_mut(),
        &orig_term as *const _ as *mut _,
        &ws as *const _ as *mut _,
    ) != 0
    {
        errno_die("openpty");
    }

    // ---- 3. fork + child process execs the login shell ----
    match libc::fork() {
        -1 => errno_die("fork"),
        0 => {
            // ---- child process ----
            libc::close(master);
            // New session + set slave as the controlling terminal (skip either step and the shell's
            // job control ^C/^Z and /dev/tty will both break)
            if libc::setsid() == -1 {
                errno_die("setsid");
            }
            if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) == -1 {
                errno_die("ioctl(TIOCSCTTY)");
            }
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }

            let term_var = CString::new("TERM=xterm-256color").unwrap();
            libc::putenv(term_var.into_raw());

            // argv[0] with a "-" prefix → zsh starts as a login shell, loading ~/.zprofile
            let path = CString::new("/bin/zsh").unwrap();
            let argv0 = CString::new("-zsh").unwrap();
            let argv = [argv0.as_ptr(), ptr::null()];
            libc::execv(path.as_ptr(), argv.as_ptr());
            errno_die("execv(/bin/zsh)");
        }
        child_pid => {
            // ---- parent process ----
            libc::close(slave);

            // Switch our own stdin to raw mode: all keystrokes (including ^C) pass through verbatim to the inner shell
            let mut raw = orig_term;
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);

            libc::signal(libc::SIGWINCH, on_sigwinch as *const () as libc::sighandler_t);

            let mut buf = [0u8; 8192];
            let mut fds = [
                libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: master, events: libc::POLLIN, revents: 0 },
            ];

            loop {
                // Window size changed → forward to the PTY (the shell will receive SIGWINCH)
                if WINCH.swap(false, Ordering::SeqCst) {
                    let mut nws = MaybeUninit::<libc::winsize>::zeroed();
                    if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, nws.as_mut_ptr()) == 0 {
                        libc::ioctl(master, libc::TIOCSWINSZ, nws.as_ptr());
                    }
                }

                fds[0].revents = 0;
                fds[1].revents = 0;
                let n = libc::poll(fds.as_mut_ptr(), 2, -1);
                if n == -1 {
                    if *libc::__error() == libc::EINTR {
                        continue; // interrupted by SIGWINCH, go back to the top of the loop to handle it
                    }
                    break;
                }

                // keyboard → PTY
                if fds[0].revents & libc::POLLIN != 0 {
                    let r = libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len());
                    if r <= 0 {
                        break;
                    }
                    if write_all(master, &buf[..r as usize]).is_err() {
                        break;
                    }
                }

                // PTY → screen. After the child shell exits, read returns 0 or EIO, ending the loop.
                if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    let r = libc::read(master, buf.as_mut_ptr().cast(), buf.len());
                    if r <= 0 {
                        break;
                    }
                    if write_all(libc::STDOUT_FILENO, &buf[..r as usize]).is_err() {
                        break;
                    }
                }
            }

            // ---- cleanup: restore terminal attributes, reap the child process ----
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &orig_term);
            let mut status = 0;
            libc::waitpid(child_pid, &mut status, 0);
            eprintln!("\r\n[pty-echo] shell exited");
            exit(0);
        }
    }
}

unsafe fn write_all(fd: libc::c_int, mut data: &[u8]) -> Result<(), ()> {
    while !data.is_empty() {
        let w = libc::write(fd, data.as_ptr().cast(), data.len());
        if w <= 0 {
            if w == -1 && *libc::__error() == libc::EINTR {
                continue;
            }
            return Err(());
        }
        data = &data[w as usize..];
    }
    Ok(())
}
