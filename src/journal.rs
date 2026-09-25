//! Structured journal entries, sent as sd_journal_send() does: one datagram of
//! FIELD=value lines. A value with a newline uses the binary form (name, newline, length
//! as u64 little endian, value, newline).

use std::os::unix::net::UnixDatagram;

use anyhow::{Context, Result};

const SOCKET: &str = "/run/systemd/journal/socket";
/// systemd's "unit process exited" message: EXIT_CODE and EXIT_STATUS of the process.
/// A process that exits 0 gets it at debug level only, so the unit's "deactivated
/// successfully" message stands for that ending.
const UNIT_PROCESS_EXIT: &str = "98e322203f7a4ed290d09fe03c09fe15";
const UNIT_SUCCESS: &str = "7ad2d189f7e94e70a38c781354912448";

/// How a unit's main process last ended, from what systemd logged when it reaped it:
/// (CLD_EXITED, CLD_KILLED or CLD_DUMPED, status). Outlives the unit's own ExecMainCode=,
/// which goes when the unit is unloaded.
pub async fn last_main_exit(unit: &str) -> Option<(i32, i32)> {
    let output = tokio::process::Command::new("journalctl")
        .args([
            "--no-pager",
            "--quiet",
            "--output=json",
            "--lines=1",
            "--reverse",
        ])
        .arg(format!("--unit={unit}"))
        .arg("_PID=1")
        .arg(format!("MESSAGE_ID={UNIT_PROCESS_EXIT}"))
        .arg(format!("MESSAGE_ID={UNIT_SUCCESS}"))
        .output()
        .await
        .ok()?;
    parse_main_exit(std::str::from_utf8(&output.stdout).ok()?)
}

fn parse_main_exit(line: &str) -> Option<(i32, i32)> {
    let entry: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if entry["MESSAGE_ID"].as_str() == Some(UNIT_SUCCESS) {
        return Some((1, 0));
    }
    let code = match entry["EXIT_CODE"].as_str()? {
        "exited" => 1,
        "killed" => 2,
        "dumped" => 3,
        _ => return None,
    };
    let status = entry["EXIT_STATUS"].as_str()?.parse().ok()?;
    Some((code, status))
}

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
    fn the_last_exit_is_read_from_the_journal_entry() {
        let exited = r#"{"UNIT": "systemd-nspawn@web.service", "EXIT_CODE": "exited", "COMMAND": "ExecStart", "EXIT_STATUS": "3", "MESSAGE_ID": "98e322203f7a4ed290d09fe03c09fe15"}"#;
        assert_eq!(parse_main_exit(exited), Some((1, 3)));
        let killed = exited
            .replace("\"exited\"", "\"killed\"")
            .replace("\"3\"", "\"9\"");
        assert_eq!(parse_main_exit(&killed), Some((2, 9)));
        assert_eq!(parse_main_exit(""), None);
        assert_eq!(parse_main_exit(r#"{"EXIT_CODE": "exited"}"#), None);
        let success = r#"{"UNIT": "systemd-nspawn@web.service", "MESSAGE_ID": "7ad2d189f7e94e70a38c781354912448", "MESSAGE": "systemd-nspawn@web.service: Deactivated successfully."}"#;
        assert_eq!(parse_main_exit(success), Some((1, 0)));
    }

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
