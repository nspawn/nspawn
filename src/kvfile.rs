//! Files of KEY=VALUE lines, as `--env-file` and `--label-file` take them and compose's
//! `env_file` with `format: raw`: the values as they are, with no quoting, escaping or
//! interpolation.

use std::path::Path;

use anyhow::{Context, Result};

/// kvfile: `KEY=value` lines read with bufio.Scanner (lines of up to 64 KiB, a
/// carriage return before each line end dropped), each line UTF-8, a BOM at the start
/// of the first dropped, a `KEY` alone taking the value lookup has.
pub fn parse(bytes: &[u8], lookup: &dyn Fn(&str) -> Option<String>) -> Result<Vec<String>, String> {
    const MAX_TOKEN: usize = 64 * 1024;
    let mut lines = Vec::new();
    let mut rest = bytes;
    let mut number = 1;
    while !rest.is_empty() {
        let (mut line, next) = match rest.iter().position(|&b| b == b'\n') {
            Some(i) if i < MAX_TOKEN => (&rest[..i], &rest[i + 1..]),
            None if rest.len() < MAX_TOKEN => (rest, &rest[rest.len()..]),
            _ => return Err("bufio.Scanner: token too long".to_string()),
        };
        rest = next;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let Ok(text) = std::str::from_utf8(line) else {
            let decimal: Vec<String> = line.iter().map(u8::to_string).collect();
            return Err(format!(
                "invalid utf8 bytes at line {number}: [{}]",
                decimal.join(" ")
            ));
        };
        let text = if number == 1 {
            text.strip_prefix('\u{feff}').unwrap_or(text)
        } else {
            text
        };
        number += 1;
        let line = text.trim_start_matches(crate::compose::gostr::is_space);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, has_value) = match line.split_once('=') {
            Some((key, _)) => (key, true),
            None => (line, false),
        };
        if key.is_empty() {
            return Err(format!("no variable name on line '{line}'"));
        }
        if key.contains([' ', '\t']) {
            return Err(format!("variable '{key}' contains whitespaces"));
        }
        if has_value {
            lines.push(line.to_string());
        } else if let Some(value) = lookup(line) {
            lines.push(format!("{key}={value}"));
        }
    }
    Ok(lines)
}

/// The variables of `--env-file` files, then those of `-e`, which come after and so win:
/// a KEY alone in a file takes this environment's value, or is left out without one.
pub fn env_with_files(files: &[impl AsRef<Path>], env: &[String]) -> Result<Vec<String>> {
    let lookup = |key: &str| std::env::var(key).ok();
    with_files(files, env, &lookup, "")
}

/// The labels of `--label-file` files, then those of `--label`; a KEY alone is left out.
pub fn labels_with_files(files: &[impl AsRef<Path>], labels: &[String]) -> Result<Vec<String>> {
    with_files(files, labels, &|_| None, "--label-file: ")
}

fn with_files(
    files: &[impl AsRef<Path>],
    given: &[String],
    lookup: &dyn Fn(&str) -> Option<String>,
    prefix: &str,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for file in files {
        let path = file.as_ref();
        let bytes =
            std::fs::read(path).with_context(|| format!("{prefix}open {}", path.display()))?;
        let lines = parse(&bytes, lookup)
            .map_err(|e| anyhow::anyhow!("{prefix}invalid env file ({}): {e}", path.display()))?;
        out.extend(lines);
    }
    out.extend(given.iter().cloned());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Result<Vec<String>, String> {
        parse(text.as_bytes(), &|k| {
            (k == "FROM_ENV").then(|| "here".to_string())
        })
    }

    /// The cases of the reference's kvfile_test.go.
    #[test]
    fn as_docker_reads_them() {
        assert_eq!(
            lines(
                "\u{feff}# comment\n  FOO=bar baz  \n\nEMPTY=\nQUOTED=\"x\"\r\nFROM_ENV\nNOT_SET\n"
            )
            .unwrap(),
            ["FOO=bar baz  ", "EMPTY=", "QUOTED=\"x\"", "FROM_ENV=here"]
        );
        assert_eq!(
            lines("F OO=bar").unwrap_err(),
            "variable 'F OO' contains whitespaces"
        );
        assert_eq!(
            lines("=bar").unwrap_err(),
            "no variable name on line '=bar'"
        );
        assert!(parse(b"A=\xff", &|_| None)
            .unwrap_err()
            .starts_with("invalid utf8 bytes at line 1"));
        let long = format!("A={}", "x".repeat(70_000));
        assert_eq!(lines(&long).unwrap_err(), "bufio.Scanner: token too long");
    }

    #[test]
    fn files_then_flags() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("e.env");
        std::fs::write(&file, "A=1\nB=2\n").unwrap();
        assert_eq!(
            env_with_files(&[&file], &["B=3".to_string()]).unwrap(),
            ["A=1", "B=2", "B=3"]
        );
        std::fs::write(&file, "BAD KEY=1\n").unwrap();
        let e = labels_with_files(&[&file], &[]).unwrap_err().to_string();
        assert!(e.starts_with("--label-file: invalid env file ("), "{e}");
        assert!(env_with_files(&[dir.path().join("missing")], &[]).is_err());
    }
}
