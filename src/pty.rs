//! Attaches the terminal to a pseudo terminal handed over by systemd-machined.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::thread;

use anyhow::{Context, Result};
use nix::libc;
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
    // stdin -> PTY. This thread may stay blocked in read(2) after the session ends; the
    // process exits shortly after, which is what machinectl does as well.
    thread::spawn(move || {
        let mut to_pty = File::from(pty);
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
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
    let stdout = io::stdout();
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { tiocgwinsz(stdout.as_raw_fd(), &mut ws) }.is_ok() && ws.ws_row > 0 {
        let _ = unsafe { tiocswinsz(pty.as_raw_fd(), &ws) };
    }
}

fn restore(original: &Termios) {
    let _ = termios::tcsetattr(io::stdin(), SetArg::TCSANOW, original);
}
