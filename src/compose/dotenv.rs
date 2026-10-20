//! `.env` files as compose-go's dotenv package reads them: `KEY=value` lines (or
//! `KEY: value`), an optional `export`, comments, a bare `KEY` taking the value from
//! the environment, single quotes kept as they are, double quotes with escapes, and
//! `${}` substituted in unquoted and double-quoted values.

use std::collections::BTreeMap;

use super::template::{self, Lookup};

pub type Env = BTreeMap<String, String>;

/// Reads `text` into `out`, a variable taking its value from `lookup` first, then from
/// the keys before it. `unset` hears of the variables neither has.
pub fn parse(
    text: &str,
    out: &mut Env,
    lookup: &dyn Lookup,
    unset: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut parser = Parser { line: 1 };
    let mut cutset = text;
    loop {
        cutset = parser.statement_start(cutset);
        if cutset.is_empty() {
            return Ok(());
        }
        let (key, left, inherited) = parser.key(cutset)?;
        if key.contains(' ') {
            return Err(format!("line {}: key cannot contain a space", parser.line));
        }
        if inherited {
            if let Some(value) = lookup.lookup(&key) {
                out.insert(key, value);
            }
            cutset = left;
            continue;
        }
        let (value, left) = parser.value(left, out, lookup, unset)?;
        out.insert(key, value);
        cutset = left;
    }
}

/// Go's unicode.IsSpace.
fn is_unicode_space(c: char) -> bool {
    super::gostr::is_space(c)
}

/// A space that is not a line break.
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{85}' | '\u{A0}'
    )
}

struct Parser {
    line: usize,
}

impl Parser {
    /// Where the next statement starts, past spaces and comment lines.
    fn statement_start<'a>(&mut self, mut src: &'a str) -> &'a str {
        loop {
            let Some(pos) = src.find(|c: char| {
                if c == '\n' {
                    self.line += 1;
                }
                !is_unicode_space(c)
            }) else {
                return "";
            };
            src = &src[pos..];
            if !src.starts_with('#') {
                return src;
            }
            match src.find('\n') {
                Some(pos) => src = &src[pos..],
                None => return "",
            }
        }
    }

    fn key<'a>(&mut self, mut src: &'a str) -> Result<(String, &'a str, bool), String> {
        let export = src
            .strip_prefix("export")
            .is_some_and(|rest| rest.starts_with(['\t', '\n', '\u{0C}', '\r', ' ']));
        if export {
            src = src["export".len()..].trim_start_matches(is_space);
        }
        let mut key = "";
        let mut offset = 0;
        let mut inherited = false;
        for (i, c) in src.char_indices() {
            if is_space(c) {
                continue;
            }
            match c {
                '=' | ':' | '\n' => {
                    key = &src[..i];
                    offset = i + 1;
                    inherited = c == '\n';
                    break;
                }
                '_' | '.' | '-' | '[' | ']' => {}
                c if super::gostr::is_letter(c) || super::gostr::is_number(c) => {}
                c => {
                    return Err(format!(
                        "line {}: unexpected character {} in variable name {}",
                        self.line,
                        super::gostr::quote(&super::gostr::rune(c).to_string()),
                        super::gostr::quote(src.split('\n').next().unwrap_or(""))
                    ));
                }
            }
        }
        if inherited && !key.contains(' ') {
            self.line += 1;
        }
        let key = key.trim_end_matches(is_unicode_space).to_string();
        let rest = src[offset..].trim_start_matches(is_space);
        Ok((key, rest, inherited))
    }

    fn value<'a>(
        &mut self,
        src: &'a str,
        env: &Env,
        lookup: &dyn Lookup,
        unset: &mut dyn FnMut(&str),
    ) -> Result<(String, &'a str), String> {
        let quote = match src.as_bytes().first() {
            Some(q @ (b'"' | b'\'')) => *q,
            _ => {
                let (value, rest) = src.split_once('\n').unwrap_or((src, ""));
                self.line += 1;
                let value = value.split_once(" #").map_or(value, |(v, _)| v);
                let value = value.trim_end_matches(is_unicode_space);
                return Ok((expand(value, env, lookup, unset)?, rest));
            }
        };
        let bytes = src.as_bytes();
        let mut chars: Vec<u8> = Vec::new();
        let mut escaped = false;
        for i in 1..bytes.len() {
            let c = bytes[i];
            if c == b'\n' {
                self.line += 1;
            }
            if c != quote {
                if !escaped && c == b'\\' {
                    escaped = true;
                    continue;
                }
                if escaped {
                    escaped = false;
                    chars.push(b'\\');
                }
                chars.push(c);
                continue;
            }
            if escaped {
                escaped = false;
                chars.push(c);
                continue;
            }
            let mut value = String::from_utf8_lossy(&chars).into_owned();
            if quote == b'"' {
                value = expand(&expand_escapes(&value), env, lookup, unset)?;
            }
            return Ok((value, &src[i + 1..]));
        }
        let end = src.find('\n').unwrap_or(src.len());
        Err(format!(
            "line {}: unterminated quoted value {}",
            self.line,
            &src[..end]
        ))
    }
}

fn expand(
    value: &str,
    env: &Env,
    lookup: &dyn Lookup,
    unset: &mut dyn FnMut(&str),
) -> Result<String, String> {
    let resolve = |name: &str| lookup.lookup(name).or_else(|| env.get(name).cloned());
    template::substitute(value, &resolve, unset).map_err(|e| e.to_string())
}

/// The escapes of a double-quoted value: `\a \b \f \n \r \t \v \" \\`, `\$` for a `$`
/// that stays one, and `\0NNN` in octal; any other backslash stays.
fn expand_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let Some(c) = after.chars().next() else {
            out.push('\\');
            rest = after;
            break;
        };
        let simple = match c {
            'a' => Some("\u{07}"),
            'b' => Some("\u{08}"),
            'f' => Some("\u{0C}"),
            'n' => Some("\n"),
            'r' => Some("\r"),
            't' => Some("\t"),
            'v' => Some("\u{0B}"),
            '"' => Some("\""),
            '\\' => Some("\\"),
            '$' => Some("$$"),
            // `\c` matches compose-go's pattern but is no Go escape: it stays.
            'c' => Some("\\c"),
            _ => None,
        };
        if let Some(replacement) = simple {
            out.push_str(replacement);
            rest = &after[1..];
            continue;
        }
        if c == '0' {
            let digits: String = after[1..]
                .chars()
                .take(3)
                .take_while(char::is_ascii_digit)
                .collect();
            let consumed = 1 + digits.len();
            // `\0NNN` is Go's `\NNN`: three octal digits, or the text with `\0` cut to `\`.
            let octal = (digits.len() == 3 && digits.bytes().all(|b| (b'0'..=b'7').contains(&b)))
                .then(|| u32::from_str_radix(&digits, 8).ok())
                .flatten()
                .filter(|v| *v <= 255)
                .and_then(char::from_u32);
            match octal {
                Some(ch) => out.push(ch),
                None => {
                    out.push('\\');
                    out.push_str(&digits);
                }
            }
            rest = &after[consumed..];
            continue;
        }
        out.push('\\');
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<Env, String> {
        let lookup = |name: &str| (name == "HOME").then(|| "/home/me".to_string());
        let mut env = Env::new();
        parse(text, &mut env, &lookup, &mut |_| {})?;
        Ok(env)
    }

    #[test]
    fn plain_values() {
        let env = read(
            "# comment\nA=1\nexport B = two words # comment\nC:3\n\nD=\nE=${A}-$HOME\nHOME\nNOPE\n",
        )
        .unwrap();
        assert_eq!(env["A"], "1");
        assert_eq!(env["B"], "two words");
        assert_eq!(env["C"], "3");
        assert_eq!(env["D"], "");
        assert_eq!(env["E"], "1-/home/me");
        assert_eq!(env["HOME"], "/home/me");
        assert!(!env.contains_key("NOPE"));
    }

    #[test]
    fn quoted_values() {
        let env = read(
            "A='single ${HOME} \\n'\nB=\"double\\n${HOME} \\$HOME \\\"q\\\" \\0101\"\nC=\"multi\nline\"\nD='it\\'s'\n",
        )
        .unwrap();
        assert_eq!(env["A"], "single ${HOME} \\n");
        assert_eq!(env["B"], "double\n/home/me $HOME \"q\" A");
        assert_eq!(env["C"], "multi\nline");
        assert_eq!(env["D"], "it's");
    }

    #[test]
    fn broken_lines_say_where() {
        assert_eq!(
            read("A=1\nB C=2\n").unwrap_err(),
            "line 2: key cannot contain a space"
        );
        assert_eq!(
            read("A=1\nB%=2\n").unwrap_err(),
            "line 2: unexpected character \"%\" in variable name \"B%=2\""
        );
        assert_eq!(
            read("A=1\nB=\"open\n").unwrap_err(),
            "line 3: unterminated quoted value \"open"
        );
    }
}
