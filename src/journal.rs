//! Structured journal entries, sent as sd_journal_send() does: one datagram of
//! FIELD=value lines. A value with a newline uses the binary form (name, newline, length
//! as u64 little endian, value, newline).

use std::os::unix::net::UnixDatagram;

use anyhow::{Context, Result};

const SOCKET: &str = "/run/systemd/journal/socket";

pub fn send(fields: &[(&str, &str)]) -> Result<()> {
    let socket = UnixDatagram::unbound().context("creating a socket for the journal")?;
    socket
        .send_to(&encode(fields), SOCKET)
        .with_context(|| format!("writing to the journal through {SOCKET}"))?;
    Ok(())
}

fn encode(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(name.as_bytes());
        if value.contains('\n') {
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
            out.extend_from_slice(value.as_bytes());
        } else {
            out.push(b'=');
            out.extend_from_slice(value.as_bytes());
        }
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_values_are_lines_and_multiline_ones_carry_their_length() {
        assert_eq!(
            encode(&[("MESSAGE", "hi"), ("PRIORITY", "6")]),
            b"MESSAGE=hi\nPRIORITY=6\n"
        );
        let mut expected = b"MESSAGE\n".to_vec();
        expected.extend_from_slice(&3u64.to_le_bytes());
        expected.extend_from_slice(b"a\nb\n");
        assert_eq!(encode(&[("MESSAGE", "a\nb")]), expected);
    }
}
