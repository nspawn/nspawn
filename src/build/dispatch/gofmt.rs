//! What the reference writes with Go's fmt, strconv, strings, time and path packages and
//! with buildkit's util/suggest, reproduced so that history lines and messages come out
//! byte for byte the same.

use std::fmt::Write as _;

use crate::build::ignore::lexical;

/// strconv.Quote, which is also %q of a string.
pub(super) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
            out.push(c);
            continue;
        }
        if is_print(c) {
            out.push(c);
            continue;
        }
        match c {
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c if (c as u32) < 0x10000 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let _ = write!(out, "\\U{:08x}", c as u32);
            }
        }
    }
    out.push('"');
    out
}

/// unicode.IsPrint. Debug escaping leaves exactly the printable categories alone once
/// the char does not start the string (a leading grapheme extender is escaped).
fn is_print(c: char) -> bool {
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    let mut s = String::with_capacity(5);
    s.push('a');
    s.push(c);
    s.escape_debug().eq(s.chars())
}

/// %q of a []string.
pub(super) fn quote_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| quote(s)).collect();
    format!("[{}]", quoted.join(" "))
}

/// %v of a []string.
pub(super) fn list(items: &[String]) -> String {
    format!("[{}]", items.join(" "))
}

/// time.Duration's String.
pub(super) fn duration(d: i64) -> String {
    let mut u = d.unsigned_abs();
    let text = if u < 1_000_000_000 {
        let (prec, unit) = match u {
            0 => return "0s".to_string(),
            1..=999 => (0, "ns"),
            1_000..=999_999 => (3, "\u{b5}s"),
            _ => (6, "ms"),
        };
        let (frac, int) = fraction(u, prec);
        format!("{int}{frac}{unit}")
    } else {
        let (frac, int) = fraction(u, 9);
        u = int;
        let mut text = format!("{}{frac}s", u % 60);
        u /= 60;
        if u > 0 {
            text = format!("{}m{text}", u % 60);
            u /= 60;
            if u > 0 {
                text = format!("{u}h{text}");
            }
        }
        text
    };
    if d < 0 {
        format!("-{text}")
    } else {
        text
    }
}

/// fmtFrac: the fraction v/10^prec without its trailing zeros (nothing for none) and the
/// integer part.
fn fraction(mut v: u64, prec: u32) -> (String, u64) {
    let mut digits = Vec::new();
    let mut print = false;
    for _ in 0..prec {
        let digit = (v % 10) as u8;
        print = print || digit != 0;
        if print {
            digits.push(b'0' + digit);
        }
        v /= 10;
    }
    if print {
        digits.push(b'.');
    }
    digits.reverse();
    (String::from_utf8(digits).unwrap_or_default(), v)
}

/// strconv.Atoi: an optional sign and decimal digits that fit in 64 bits.
pub(super) fn atoi(s: &str) -> Option<i64> {
    let (neg, digits) = match s.as_bytes().first()? {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i128 = digits.parse().ok()?;
    let n = if neg { -n } else { n };
    i64::try_from(n).ok()
}

/// strconv.ParseUint(s, 8, 32).
pub(super) fn parse_octal_u32(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return None;
    }
    let mut n: u64 = 0;
    for b in s.bytes() {
        n = n * 8 + u64::from(b - b'0');
        if n > u64::from(u32::MAX) {
            return None;
        }
    }
    u32::try_from(n).ok()
}

/// strings.SplitN(s, " ", 2) with the first part upper-cased (uppercaseCmd).
pub(super) fn uppercase_cmd(s: &str) -> String {
    match s.split_once(' ') {
        Some((first, rest)) => format!("{} {rest}", first.to_uppercase()),
        None => s.to_uppercase(),
    }
}

/// path.Join.
pub(super) fn join(elems: &[&str]) -> String {
    lexical::join_str(elems)
}

/// path.Clean.
pub(super) fn clean(p: &str) -> String {
    lexical::clean_str(p)
}

/// path.Base.
pub(super) fn base(p: &str) -> String {
    String::from_utf8_lossy(&lexical::base(p.as_bytes())).into_owned()
}

/// strings.TrimSpace: Unicode white space at both ends.
pub(super) fn trim_space(s: &str) -> &str {
    s.trim_matches(char::is_whitespace)
}

/// Levenshtein distance over chars with unit costs.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// suggest.Search: the closest option within two edits; none when `val` is one of them.
pub(super) fn suggest(val: &str, options: &[String], case_sensitive: bool) -> Option<String> {
    let fold = |s: &str| {
        if case_sensitive {
            s.to_string()
        } else {
            s.to_lowercase()
        }
    };
    let wanted = fold(val);
    let mut found = String::new();
    let mut mindist = 3;
    for opt in options {
        let opt = fold(opt);
        if wanted == opt {
            return None;
        }
        let dist = levenshtein(&wanted, &opt);
        if dist < mindist {
            found = if case_sensitive || val == val.to_lowercase() {
                opt
            } else if val == val.to_uppercase() {
                opt.to_uppercase()
            } else {
                opt
            };
            mindist = dist;
        }
    }
    (!found.is_empty()).then_some(found)
}

/// suggest.WrapError applied to a message.
pub(super) fn with_suggestion(
    message: String,
    val: &str,
    options: &[String],
    case_sensitive: bool,
) -> String {
    match suggest(val, options, case_sensitive) {
        Some(m) => format!("{message} (did you mean {m}?)"),
        None => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_print_as_go_prints_them() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(1), "1ns");
        assert_eq!(duration(1_500), "1.5\u{b5}s");
        assert_eq!(duration(100_000_000), "100ms");
        assert_eq!(duration(1_000_000_000), "1s");
        assert_eq!(duration(1_500_000_000), "1.5s");
        assert_eq!(duration(90 * 1_000_000_000), "1m30s");
        assert_eq!(duration(3_600 * 1_000_000_000), "1h0m0s");
        assert_eq!(duration(-2_000_000), "-2ms");
    }

    #[test]
    fn quoting() {
        assert_eq!(
            quote("a\"b\\c\n\u{7f}\u{200b}"),
            "\"a\\\"b\\\\c\\n\\x7f\\u200b\""
        );
        assert_eq!(quote("\u{e9}"), "\"\u{e9}\"");
        assert_eq!(
            quote_list(&["a b".to_string(), "c".to_string()]),
            "[\"a b\" \"c\"]"
        );
        assert_eq!(quote_list(&[]), "[]");
        assert_eq!(list(&["a".to_string(), "b c".to_string()]), "[a b c]");
    }

    #[test]
    fn numbers() {
        assert_eq!(atoi("12"), Some(12));
        assert_eq!(atoi("+1"), Some(1));
        assert_eq!(atoi("-1"), Some(-1));
        assert_eq!(atoi("007"), Some(7));
        assert_eq!(atoi(""), None);
        assert_eq!(atoi("1a"), None);
        assert_eq!(atoi("99999999999999999999"), None);
        assert_eq!(parse_octal_u32("0755"), Some(0o755));
        assert_eq!(parse_octal_u32("8"), None);
        assert_eq!(parse_octal_u32("77777777777"), None);
    }

    #[test]
    fn suggestions() {
        let names = vec!["build".to_string(), "stage-1".to_string()];
        assert_eq!(suggest("buld", &names, true), Some("build".to_string()));
        assert_eq!(suggest("build", &names, true), None);
        assert_eq!(suggest("other", &names, true), None);
        assert_eq!(
            with_suggestion("x".to_string(), "BUIL", &names, false),
            "x (did you mean BUILD?)"
        );
    }

    #[test]
    fn commands_upper_cased() {
        assert_eq!(uppercase_cmd("run echo hi"), "RUN echo hi");
        assert_eq!(uppercase_cmd("workdir"), "WORKDIR");
    }
}
