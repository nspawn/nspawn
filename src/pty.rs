//! Attaches the terminal to a pseudo terminal handed over by systemd-machined.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::thread;

use anyhow::{Context, Result};
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::pty::Winsize;
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use nix::unistd::{dup, isatty};

nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, Winsize);
nix::ioctl_write_ptr_bad!(tiocswinsz, libc::TIOCSWINSZ, Winsize);

/// Pumps bytes between the local terminal and `pty` until the remote side closes it.
/// The terminal is put into raw mode when standard input is a TTY.
pub fn run_session(pty: OwnedFd) -> Result<()> {
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

    let result = pump(pty);

    if let Some(original) = saved {
        restore(&original);
    }
    result
}

fn pump(pty: OwnedFd) -> Result<()> {
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
    thread::spawn(move || {
        let mut to_pty = File::from(pty);
        let stdin = io::stdin();
        let mut last_size = window_size();
        let mut buf = [0u8; 8192];
        loop {
            let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(250u16)) {
                Ok(0) => {
                    let size = window_size();
                    if size != last_size {
                        last_size = size;
                        if let Some((rows, cols)) = size {
                            let ws = Winsize {
                                ws_row: rows,
                                ws_col: cols,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            let _ = unsafe { tiocswinsz(to_pty.as_raw_fd(), &ws) };
                        }
                    }
                    continue;
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => break,
                Ok(_) => {}
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
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("PTY reader thread panicked"))?;
    Ok(())
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
