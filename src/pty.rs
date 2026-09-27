//! Attaches the terminal to a pseudo terminal handed over by systemd-machined.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::Winsize;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use nix::unistd::{dup, isatty};

/// The terminal's attributes before they were changed, for a signal to put back.
static SAVED: OnceLock<libc::termios> = OnceLock::new();
static GUARDED: AtomicBool = AtomicBool::new(false);

extern "C" fn restore_and_exit(signal: libc::c_int) {
    // Only calls the signal-safe list allows: tcsetattr and _exit.
    if GUARDED.load(Ordering::SeqCst) {
        if let Some(saved) = SAVED.get() {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, saved) };
        }
    }
    unsafe { libc::_exit(128 + signal) };
}

/// Puts the terminal back as it was and exits, should SIGINT, SIGTERM or SIGHUP arrive
/// while its attributes are changed: killed in raw mode or without echo, the shell the
/// command ran from would be left unusable. The previous dispositions come back when
/// the guard is dropped.
pub struct TerminalGuard {
    previous: Vec<(Signal, SigAction)>,
}

pub fn guard_terminal(original: &Termios) -> TerminalGuard {
    let _ = SAVED.set(original.clone().into());
    GUARDED.store(true, Ordering::SeqCst);
    let action = SigAction::new(
        SigHandler::Handler(restore_and_exit),
        SaFlags::empty(),
        SigSet::empty(),
    );
    let previous = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP]
        .into_iter()
        .filter_map(|signal| {
            // SAFETY: the handler only calls signal-safe functions.
            unsafe { sigaction(signal, &action) }
                .ok()
                .map(|previous| (signal, previous))
        })
        .collect();
    TerminalGuard { previous }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        GUARDED.store(false, Ordering::SeqCst);
        for (signal, previous) in self.previous.drain(..) {
            // SAFETY: putting back what was there.
            let _ = unsafe { sigaction(signal, &previous) };
        }
    }
}

nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, Winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);

/// Pumps bytes between the local terminal and `pty` until the remote side closes it.
/// The terminal is put into raw mode when standard input is a TTY. Without
/// `interactive` nothing of standard input goes to the pty, as docker's -t alone.
pub fn run_session(pty: OwnedFd, interactive: bool) -> Result<()> {
    let stdin = io::stdin();
    let saved = if isatty(&stdin).unwrap_or(false) {
        let original = termios::tcgetattr(&stdin).context("reading terminal attributes")?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        raw.local_flags.remove(LocalFlags::ECHO);
        termios::tcsetattr(&stdin, SetArg::TCSANOW, &raw)
            .context("switching the terminal to raw mode")?;
        propagate_window_size(&pty);
        Some(original)
    } else {
        None
    };
    let guard = saved.as_ref().map(guard_terminal);

    let result = pump(pty, interactive);

    drop(guard);
    if let Some(original) = saved {
        restore(&original);
    }
    result
}

fn pump(pty: OwnedFd, interactive: bool) -> Result<()> {
    // Set once the pty is gone: the input thread must not swallow what is typed next.
    let done = Arc::new(AtomicBool::new(false));
    let reader_fd = dup(&pty).context("duplicating the PTY descriptor")?;
    // PTY -> stdout runs in its own thread; the session ends when it sees EOF or EIO.
    let reader = thread::spawn(move || {
        let mut from_pty = File::from(reader_fd);
        let mut stdout = io::stdout().lock();
        let mut buf = [0u8; 8192];
        loop {
            match from_pty.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout
                        .write_all(&buf[..n])
                        .and_then(|_| stdout.flush())
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    // stdin -> PTY. Polling with a timeout lets the thread notice terminal resizes; on
    // stdin's EOF the slave gets the line discipline's end-of-file character, what a user
    // would type as Ctrl-D, so that a command reading its input finishes.
    let ended = done.clone();
    thread::spawn(move || {
        let mut to_pty = File::from(pty);
        let stdin = io::stdin();
        let mut last_size = window_size();
        let mut buf = [0u8; 8192];
        loop {
            if ended.load(Ordering::SeqCst) {
                break;
            }
            if !interactive {
                thread::sleep(Duration::from_millis(250));
                resize(&to_pty, &mut last_size);
                continue;
            }
            let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(250u16)) {
                Ok(0) => {
                    resize(&to_pty, &mut last_size);
                    continue;
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
                Ok(_) => {}
            }
            if ended.load(Ordering::SeqCst) {
                break;
            }
            match stdin.lock().read(&mut buf) {
                Ok(0) | Err(_) => {
                    send_eof(&mut to_pty);
                    break;
                }
                Ok(n) => {
                    if to_pty.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let joined = reader.join();
    done.store(true, Ordering::SeqCst);
    joined.map_err(|_| anyhow::anyhow!("PTY reader thread panicked"))?;
    Ok(())
}

/// Passes a change of the terminal's size on to the pty.
fn resize(pty: &File, last_size: &mut Option<(u16, u16)>) {
    let size = window_size();
    if size != *last_size {
        *last_size = size;
        if let Some((rows, cols)) = size {
            let ws = Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let _ = unsafe { tiocswinsz(pty.as_raw_fd(), &ws) };
        }
    }
}

fn propagate_window_size(pty: &OwnedFd) {
    if let Some((rows, cols)) = window_size() {
        let ws = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let _ = unsafe { tiocswinsz(pty.as_raw_fd(), &ws) };
    }
}

/// The local terminal's size, when standard output is one.
pub fn window_size() -> Option<(u16, u16)> {
    let stdout = io::stdout();
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    (unsafe { tiocgwinsz(stdout.as_raw_fd(), &mut ws) }.is_ok() && ws.ws_row > 0)
        .then_some((ws.ws_row, ws.ws_col))
}

/// Delivers end-of-file to the slave: its VEOF character while the line discipline is
/// canonical; a raw-mode program reads bytes and would only see a stray control byte.
fn send_eof(pty: &mut File) {
    if let Ok(attrs) = termios::tcgetattr(&*pty) {
        if attrs.local_flags.contains(LocalFlags::ICANON) {
            let eof = attrs.control_chars[termios::SpecialCharacterIndices::VEOF as usize];
            let _ = pty.write_all(&[eof]);
        }
    }
}

fn restore(original: &Termios) {
    let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, original);
}
