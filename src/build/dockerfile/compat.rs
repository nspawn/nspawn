//! Behaviour of the Go standard library and of the small dependencies that
//! the reference relies on, reproduced where it shows in results or in
//! messages: strconv quoting and number parsing, time.ParseDuration, the
//! Unicode-aware strings helpers, buildkit's util/suggest, go-csvvalue and
//! go-units.

use std::fmt::Write as _;

/// strconv.Quote.
pub(crate) fn quote(s: &str) -> String {
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

/// The %q verb applied to a []string.
pub(crate) fn quote_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| quote(s)).collect();
    format!("[{}]", quoted.join(" "))
}

/// unicode.IsPrint: letters, marks, numbers, punctuation, symbols and the
/// ASCII space. The standard library's Debug escaping leaves exactly those
/// categories alone; a char placed after another one is not escaped as a
/// grapheme extender, so the check only depends on the category.
pub(crate) fn is_print(c: char) -> bool {
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    let mut s = String::with_capacity(5);
    s.push('a');
    s.push(c);
    s.escape_debug().eq(s.chars())
}

fn lower_char(c: char) -> char {
    // The only full lowercase mapping longer than one char; the simple
    // mapping the reference uses gives a plain i.
    if c == '\u{130}' {
        return 'i';
    }
    c.to_lowercase().next().unwrap_or(c)
}

fn upper_char(c: char) -> char {
    let mut up = c.to_uppercase();
    if up.len() == 1 {
        return up.next().unwrap_or(c);
    }
    // Chars whose full uppercase mapping has several chars keep their simple
    // mapping, which only differs from the char itself for the Greek letters
    // with ypogegrammeni.
    match c as u32 {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => {
            char::from_u32(c as u32 + 8).unwrap_or(c)
        }
        0x1FB3 => '\u{1FBC}',
        0x1FC3 => '\u{1FCC}',
        0x1FF3 => '\u{1FFC}',
        _ => c,
    }
}

/// strings.ToLower.
pub(crate) fn to_lower(s: &str) -> String {
    s.chars().map(lower_char).collect()
}

/// strings.ToUpper.
pub(crate) fn to_upper(s: &str) -> String {
    s.chars().map(upper_char).collect()
}

/// strings.EqualFold.
pub(crate) fn equal_fold(a: &str, b: &str) -> bool {
    let mut a = a.chars();
    let mut b = b.chars();
    loop {
        match (a.next(), b.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if x == y {
                    continue;
                }
                // The dotted and dotless i have no simple case folding.
                if matches!(x, '\u{130}' | '\u{131}') || matches!(y, '\u{130}' | '\u{131}') {
                    return false;
                }
                if lower_char(upper_char(x)) != lower_char(upper_char(y)) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// strconv.ParseBool.
pub(crate) fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumErrorKind {
    Syntax,
    Range,
}

/// strconv.ParseUint for a base from 2 to 36 (no prefixes, no underscores).
/// On a range error the reference also returns the largest value.
pub(crate) fn parse_uint(s: &str, base: u32, bit_size: u32) -> Result<u64, (NumErrorKind, u64)> {
    if s.is_empty() {
        return Err((NumErrorKind::Syntax, 0));
    }
    let base64 = u64::from(base);
    let cutoff = u64::MAX / base64 + 1;
    let max_val = if bit_size >= 64 {
        u64::MAX
    } else {
        (1u64 << bit_size) - 1
    };
    let mut n: u64 = 0;
    for c in s.bytes() {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            _ if c.to_ascii_lowercase().is_ascii_lowercase() => c.to_ascii_lowercase() - b'a' + 10,
            _ => return Err((NumErrorKind::Syntax, 0)),
        };
        if u32::from(d) >= base {
            return Err((NumErrorKind::Syntax, 0));
        }
        if n >= cutoff {
            return Err((NumErrorKind::Range, max_val));
        }
        n *= base64;
        let n1 = n.wrapping_add(u64::from(d));
        if n1 < n || n1 > max_val {
            return Err((NumErrorKind::Range, max_val));
        }
        n = n1;
    }
    Ok(n)
}

/// strconv.ParseInt in base 10, with the reference's error text.
pub(crate) fn parse_int(s: &str, bit_size: u32) -> Result<i64, String> {
    let fail = |kind: NumErrorKind| {
        let what = match kind {
            NumErrorKind::Syntax => "invalid syntax",
            NumErrorKind::Range => "value out of range",
        };
        format!("strconv.ParseInt: parsing {}: {what}", quote(s))
    };
    if s.is_empty() {
        return Err(fail(NumErrorKind::Syntax));
    }
    let (neg, digits) = match s.as_bytes()[0] {
        b'+' => (false, &s[1..]),
        b'-' => (true, &s[1..]),
        _ => (false, s),
    };
    let un = match parse_uint(digits, 10, bit_size) {
        Ok(v) => v,
        Err((NumErrorKind::Range, v)) => v,
        Err((kind, _)) => return Err(fail(kind)),
    };
    let cutoff = 1u64 << (bit_size - 1);
    if !neg && un >= cutoff {
        return Err(fail(NumErrorKind::Range));
    }
    if neg && un > cutoff {
        return Err(fail(NumErrorKind::Range));
    }
    let n = un as i64;
    Ok(if neg { n.wrapping_neg() } else { n })
}

/// strconv.ParseFloat(s, 64), giving `None` wherever the reference reports
/// an error, out-of-range values included.
pub(crate) fn parse_float(s: &str) -> Option<f64> {
    if let Some(v) = special_float(s) {
        return v;
    }
    let b = s.as_bytes();
    let mut i = 0;
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let hex = i + 2 < b.len() && b[i] == b'0' && b[i + 1].eq_ignore_ascii_case(&b'x');
    if hex {
        i += 2;
    }
    let mut saw_dot = false;
    let mut saw_digits = false;
    let mut underscores = false;
    let mut mantissa = 0f64;
    let mut frac_digits = 0i32;
    while i < b.len() {
        let c = b[i];
        if c == b'_' {
            underscores = true;
        } else if c == b'.' {
            if saw_dot {
                break;
            }
            saw_dot = true;
        } else if c.is_ascii_digit() || (hex && c.is_ascii_hexdigit()) {
            saw_digits = true;
            if hex {
                mantissa = mantissa * 16.0 + f64::from((c as char).to_digit(16).unwrap_or(0));
                if saw_dot {
                    frac_digits += 1;
                }
            }
        } else {
            break;
        }
        i += 1;
    }
    if !saw_digits {
        return None;
    }
    let exp_char = if hex { b'p' } else { b'e' };
    let mut exponent = 0i32;
    if i < b.len() && b[i].to_ascii_lowercase() == exp_char {
        i += 1;
        if i >= b.len() {
            return None;
        }
        let mut esign = 1;
        if b[i] == b'+' {
            i += 1;
        } else if b[i] == b'-' {
            i += 1;
            esign = -1;
        }
        if i >= b.len() || !b[i].is_ascii_digit() {
            return None;
        }
        let mut e = 0i32;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            if b[i] == b'_' {
                underscores = true;
            } else if e < 10000 {
                e = e * 10 + i32::from(b[i] - b'0');
            }
            i += 1;
        }
        exponent = e * esign;
    } else if hex {
        return None;
    }
    if i != b.len() || (underscores && !underscore_ok(&s[..i])) {
        return None;
    }
    let value = if hex {
        let v = mantissa * 2f64.powi(exponent - 4 * frac_digits);
        if neg {
            -v
        } else {
            v
        }
    } else {
        let cleaned: String = s.chars().filter(|&c| c != '_').collect();
        cleaned.parse::<f64>().ok()?
    };
    value.is_finite().then_some(value)
}

/// The inf, infinity and nan spellings ParseFloat accepts: `Some` when `s`
/// starts like one of them, with the outcome for the whole string.
fn special_float(s: &str) -> Option<Option<f64>> {
    let b = s.as_bytes();
    let (sign, rest, signed) = match b.first() {
        Some(b'+') => (1.0, &s[1..], true),
        Some(b'-') => (-1.0, &s[1..], true),
        _ => (1.0, s, false),
    };
    let lower = rest.to_ascii_lowercase();
    if lower.starts_with("inf") {
        let n = if lower.starts_with("infinity") { 8 } else { 3 };
        return Some((n == rest.len()).then_some(sign * f64::INFINITY));
    }
    if !signed && lower.starts_with("nan") {
        return Some((rest.len() == 3).then_some(f64::NAN));
    }
    None
}

/// strconv's underscoreOK: underscores only between digits, or between a
/// base prefix and a digit.
fn underscore_ok(s: &str) -> bool {
    let mut b = s.as_bytes();
    if let Some(&c) = b.first() {
        if c == b'+' || c == b'-' {
            b = &b[1..];
        }
    }
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if b.len() >= 2 && b[0] == b'0' && matches!(b[1].to_ascii_lowercase(), b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0';
        hex = b[1].eq_ignore_ascii_case(&b'x');
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() || (hex && c.is_ascii_hexdigit()) {
            saw = b'0';
        } else if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
        } else {
            if saw == b'_' {
                return false;
            }
            saw = b'!';
        }
        i += 1;
    }
    saw != b'_'
}

/// The quoting of the time package, which escapes every byte outside
/// printable ASCII.
fn time_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if (c as u32) >= 0x80 || c < ' ' {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "\\x{b:02x}");
            }
        } else {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out.push('"');
    out
}

const NANOSECOND: u64 = 1;
const MICROSECOND: u64 = 1000 * NANOSECOND;
const MILLISECOND: u64 = 1000 * MICROSECOND;
const SECOND: u64 = 1000 * MILLISECOND;

/// time.ParseDuration, in nanoseconds.
pub(crate) fn parse_duration(orig: &str) -> Result<i64, String> {
    let invalid = || format!("time: invalid duration {}", time_quote(orig));
    let mut s = orig.as_bytes();
    let mut d: u64 = 0;
    let mut neg = false;
    if let Some(&c) = s.first() {
        if c == b'-' || c == b'+' {
            neg = c == b'-';
            s = &s[1..];
        }
    }
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    while !s.is_empty() {
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return Err(invalid());
        }
        let pl = s.len();
        let (mut v, rest) = leading_int(s).ok_or_else(invalid)?;
        s = rest;
        let pre = pl != s.len();
        let mut f = 0u64;
        let mut scale = 1f64;
        let mut post = false;
        if !s.is_empty() && s[0] == b'.' {
            s = &s[1..];
            let pl = s.len();
            let (x, sc, rest) = leading_fraction(s);
            f = x;
            scale = sc;
            s = rest;
            post = pl != s.len();
        }
        if !pre && !post {
            return Err(invalid());
        }
        let i = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if i == 0 {
            return Err(format!(
                "time: missing unit in duration {}",
                time_quote(orig)
            ));
        }
        let unit_name = String::from_utf8_lossy(&s[..i]).into_owned();
        s = &s[i..];
        let unit = match unit_name.as_str() {
            "ns" => NANOSECOND,
            "us" | "\u{b5}s" | "\u{3bc}s" => MICROSECOND,
            "ms" => MILLISECOND,
            "s" => SECOND,
            "m" => 60 * SECOND,
            "h" => 3600 * SECOND,
            _ => {
                return Err(format!(
                    "time: unknown unit {} in duration {}",
                    time_quote(&unit_name),
                    time_quote(orig)
                ))
            }
        };
        if v > (1u64 << 63) / unit {
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1 << 63 {
                return Err(invalid());
            }
        }
        d = d.wrapping_add(v);
        if d > 1 << 63 {
            return Err(invalid());
        }
    }
    if neg {
        return Ok((d as i64).wrapping_neg());
    }
    if d > (1 << 63) - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}

fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let mut x: u64 = 0;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if x > (1 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(s[i] - b'0');
        if x > 1 << 63 {
            return None;
        }
        i += 1;
    }
    Some((x, &s[i..]))
}

fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let mut x: u64 = 0;
    let mut scale = 1f64;
    let mut overflow = false;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if !overflow {
            if x > ((1u64 << 63) - 1) / 10 {
                overflow = true;
            } else {
                let y = x * 10 + u64::from(s[i] - b'0');
                if y > 1 << 63 {
                    overflow = true;
                } else {
                    x = y;
                    scale *= 10.0;
                }
            }
        }
        i += 1;
    }
    (x, scale, &s[i..])
}

/// Levenshtein distance over chars with unit costs (agext/levenshtein with
/// default parameters).
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
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

/// suggest.Search: the closest option within two edits, or none when there
/// is none or when `val` is itself one of the options.
pub(crate) fn suggest(val: &str, options: &[&str], case_sensitive: bool) -> Option<String> {
    let orig = val;
    let val = if case_sensitive {
        val.to_string()
    } else {
        to_lower(val)
    };
    let mut found = String::new();
    let mut mindist = 3;
    for opt in options {
        let opt = if case_sensitive {
            (*opt).to_string()
        } else {
            to_lower(opt)
        };
        if val == opt {
            return None;
        }
        let dist = levenshtein(&val, &opt);
        if dist < mindist {
            found = if case_sensitive {
                opt
            } else {
                match_case(&opt, orig)
            };
            mindist = dist;
        }
    }
    (!found.is_empty()).then_some(found)
}

fn match_case(val: &str, orig: &str) -> String {
    if orig == to_lower(orig) {
        return to_lower(val);
    }
    if orig == to_upper(orig) {
        return to_upper(val);
    }
    val.to_string()
}

/// suggest.WrapError applied to an error message.
pub(crate) fn with_suggestion(
    message: String,
    val: &str,
    options: &[&str],
    case_sensitive: bool,
) -> String {
    match suggest(val, options, case_sensitive) {
        Some(m) => format!("{message} (did you mean {m}?)"),
        None => message,
    }
}

/// csvvalue.Fields with the default parser: one CSV record, `,` separated,
/// strict quotes, a trailing newline allowed.
pub(crate) fn csv_fields(input: &str) -> Result<Vec<String>, String> {
    const BARE_QUOTE: &str = "bare \" in non-quoted-field";
    const QUOTE: &str = "extraneous or missing \" in quoted-field";
    let parse_err =
        |pos: usize, err: &str| format!("parse error on line 1, column {}: {err}", pos + 1);

    let mut line = input;
    if let Some(l) = line.strip_suffix('\n') {
        line = l.strip_suffix('\r').unwrap_or(l);
    }
    if line.is_empty() {
        return Err("EOF".to_string());
    }
    let mut dst: Vec<String> = Vec::new();
    let mut pos = 0usize;
    'field: loop {
        if !line.starts_with('"') {
            let i = line.find(',');
            let field = match i {
                Some(i) => &line[..i],
                None => line,
            };
            if let Some(j) = field.find('"') {
                return Err(parse_err(pos + j, BARE_QUOTE));
            }
            dst.push(field.to_string());
            match i {
                Some(i) => {
                    line = &line[i + 1..];
                    pos += i + 1;
                    continue;
                }
                None => break,
            }
        }
        line = &line[1..];
        pos += 1;
        let mut half_open = false;
        loop {
            let Some(i) = line.find('"') else {
                return Err(parse_err(pos, QUOTE));
            };
            if half_open {
                if let Some(last) = dst.last_mut() {
                    last.push_str(&line[..i]);
                }
            } else {
                dst.push(line[..i].to_string());
            }
            line = &line[i + 1..];
            pos += i + 1;
            match line.chars().next() {
                Some('"') => {
                    if let Some(last) = dst.last_mut() {
                        last.push('"');
                    }
                    line = &line[1..];
                    pos += 1;
                    half_open = true;
                }
                Some(',') => {
                    line = &line[1..];
                    pos += 1;
                    continue 'field;
                }
                None => break 'field,
                Some(_) => return Err(parse_err(pos - 1, QUOTE)),
            }
        }
    }
    Ok(dst)
}

/// Go's int64(x) of a float64 on amd64: what does not fit, NaN too, is the smallest
/// int64, which a size then reads as none.
fn go_i64(x: f64) -> i64 {
    const TWO63: f64 = 9_223_372_036_854_775_808.0;
    if (-TWO63..TWO63).contains(&x) {
        x as i64
    } else {
        i64::MIN
    }
}

/// units.RAMInBytes: a size with an optional binary suffix (k, m, g, t, p,
/// with an optional b or ib), case-insensitive.
pub(crate) fn ram_in_bytes(size: &str) -> Result<i64, String> {
    let Some(sep) = size.bytes().rposition(|c| b"01234567890. ".contains(&c)) else {
        return Err(format!("invalid size: '{size}'"));
    };
    let (num, sfx) = if size.as_bytes()[sep] != b' ' {
        (&size[..sep + 1], &size[sep + 1..])
    } else {
        (&size[..sep], &size[sep + 1..])
    };
    let Some(mut value) = parse_float(num) else {
        return Err(format!(
            "strconv.ParseFloat: parsing {}: invalid syntax",
            quote(num)
        ));
    };
    if value < 0.0 {
        return Err(format!("invalid size: '{size}'"));
    }
    if sfx.is_empty() {
        return Ok(go_i64(value));
    }
    if sfx.len() > 3 {
        return Err(format!("invalid suffix: '{sfx}'"));
    }
    let sfx = to_lower(sfx);
    let b = sfx.as_bytes();
    let bad = || Err(format!("invalid suffix: '{sfx}'"));
    if b[0] == b'b' {
        if b.len() > 1 {
            return bad();
        }
        return Ok(go_i64(value));
    }
    let mul: i64 = match b[0] {
        b'k' => 1 << 10,
        b'm' => 1 << 20,
        b'g' => 1 << 30,
        b't' => 1 << 40,
        b'p' => 1 << 50,
        _ => return bad(),
    };
    value *= mul as f64;
    if (b.len() == 2 && b[1] != b'b') || (b.len() == 3 && &b[1..] != b"ib") {
        return bad();
    }
    Ok(go_i64(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_matches_strconv() {
        assert_eq!(quote("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(
            quote("\u{0}\x07\x08\x0c\n\r\t\x0b\x7f"),
            r#""\x00\a\b\f\n\r\t\v\x7f""#
        );
        assert_eq!(
            quote("\u{2665} \u{e9} \u{d55c}"),
            "\"\u{2665} \u{e9} \u{d55c}\""
        );
        let u = |hex: &str| format!("\\u{hex}");
        assert_eq!(
            quote("\u{a0}\u{ad}\u{e000}"),
            format!("\"{}{}{}\"", u("00a0"), u("00ad"), u("e000"))
        );
        assert_eq!(quote("\u{f0000}"), r#""\U000f0000""#);
        assert_eq!(quote("e\u{301}"), "\"e\u{301}\"");
        assert_eq!(
            quote_list(&["--a".into(), "b c".into()]),
            r#"["--a" "b c"]"#
        );
    }

    #[test]
    fn case_mapping_is_simple() {
        assert_eq!(to_upper("stra\u{df}e"), "STRA\u{df}E");
        assert_eq!(to_lower("\u{130}"), "i");
        assert_eq!(to_lower("\u{3a3}\u{391}\u{3a3}"), "\u{3c3}\u{3b1}\u{3c3}");
        assert_eq!(to_lower("\u{212a}"), "k");
        assert_eq!(to_upper("\u{1f80}"), "\u{1f88}");
        assert!(equal_fold("AS", "as") && equal_fold("a\u{17f}", "AS"));
        assert!(!equal_fold("\u{131}", "i") && !equal_fold("\u{130}", "i"));
        assert!(!equal_fold("as", "a"));
    }

    #[test]
    fn integers() {
        assert_eq!(parse_int("42", 32), Ok(42));
        assert_eq!(parse_int("-2147483648", 32), Ok(-2147483648));
        assert_eq!(
            parse_int("2147483648", 32),
            Err("strconv.ParseInt: parsing \"2147483648\": value out of range".into())
        );
        assert_eq!(
            parse_int("99999999999999999999x", 32),
            Err("strconv.ParseInt: parsing \"99999999999999999999x\": value out of range".into())
        );
        assert_eq!(
            parse_int("x", 32),
            Err("strconv.ParseInt: parsing \"x\": invalid syntax".into())
        );
        assert_eq!(
            parse_int("", 32),
            Err("strconv.ParseInt: parsing \"\": invalid syntax".into())
        );
        assert_eq!(parse_uint("755", 8, 32), Ok(0o755));
        assert!(parse_uint("8", 8, 32).is_err());
        assert!(parse_uint("+1", 10, 32).is_err());
        assert!(parse_uint("4294967296", 10, 32).is_err());
    }

    #[test]
    fn floats() {
        assert_eq!(parse_float("1.5"), Some(1.5));
        assert_eq!(parse_float("1_000"), Some(1000.0));
        assert_eq!(parse_float(".5"), Some(0.5));
        assert_eq!(parse_float("5."), Some(5.0));
        assert_eq!(parse_float("1e3"), Some(1000.0));
        assert_eq!(parse_float("0x1p4"), Some(16.0));
        assert_eq!(parse_float("inf"), Some(f64::INFINITY));
        assert_eq!(parse_float("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_float("infinit"), None);
        assert!(parse_float("NaN").is_some_and(f64::is_nan));
        assert_eq!(parse_float("+nan"), None);
        assert_eq!(parse_float("1e400"), None);
        assert_eq!(parse_float("."), None);
        assert_eq!(parse_float("1__0"), None);
        assert_eq!(parse_float("0x10"), None);
        assert_eq!(parse_float(""), None);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("5s"), Ok(5_000_000_000));
        assert_eq!(parse_duration("1h30m"), Ok(5_400_000_000_000));
        assert_eq!(parse_duration("1.5ms"), Ok(1_500_000));
        assert_eq!(parse_duration("2\u{b5}s"), Ok(2000));
        assert_eq!(parse_duration("-1s"), Ok(-1_000_000_000));
        assert_eq!(parse_duration("0"), Ok(0));
        assert_eq!(parse_duration("50ns"), Ok(50));
        assert_eq!(
            parse_duration("1"),
            Err("time: missing unit in duration \"1\"".into())
        );
        assert_eq!(
            parse_duration("1x"),
            Err("time: unknown unit \"x\" in duration \"1x\"".into())
        );
        assert_eq!(
            parse_duration(""),
            Err("time: invalid duration \"\"".into())
        );
        assert_eq!(
            parse_duration(".s"),
            Err("time: invalid duration \".s\"".into())
        );
        assert_eq!(
            parse_duration("9999999999h"),
            Err("time: invalid duration \"9999999999h\"".into())
        );
    }

    #[test]
    fn suggestions() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("\u{d55c}\u{ad6d}", "\u{d55c}\u{adf9}"), 1);
        assert_eq!(
            suggest("FORM", &["ADD", "FROM"], false),
            Some("FROM".into())
        );
        assert_eq!(
            suggest("form", &["ADD", "FROM"], false),
            Some("from".into())
        );
        assert_eq!(
            suggest("Form", &["ADD", "FROM"], false),
            Some("from".into())
        );
        assert_eq!(suggest("from", &["FROM"], false), None);
        assert_eq!(suggest("xyzzy", &["FROM"], false), None);
        assert_eq!(
            suggest("mont", &["mount", "network"], true),
            Some("mount".into())
        );
    }

    #[test]
    fn csv() {
        assert_eq!(csv_fields("a,b"), Ok(vec!["a".into(), "b".into()]));
        assert_eq!(csv_fields("a,"), Ok(vec!["a".into(), String::new()]));
        assert_eq!(
            csv_fields("\"a,b\",c\n"),
            Ok(vec!["a,b".into(), "c".into()])
        );
        assert_eq!(csv_fields("\"a\"\"b\""), Ok(vec!["a\"b".into()]));
        assert_eq!(csv_fields(""), Err("EOF".into()));
        assert_eq!(
            csv_fields("ab\"c"),
            Err("parse error on line 1, column 3: bare \" in non-quoted-field".into())
        );
        assert_eq!(
            csv_fields("\"ab"),
            Err("parse error on line 1, column 2: extraneous or missing \" in quoted-field".into())
        );
        assert_eq!(
            csv_fields("\"a\"b"),
            Err("parse error on line 1, column 3: extraneous or missing \" in quoted-field".into())
        );
    }

    #[test]
    fn ram_sizes() {
        assert_eq!(ram_in_bytes("32"), Ok(32));
        assert_eq!(ram_in_bytes("32b"), Ok(32));
        assert_eq!(ram_in_bytes("32k"), Ok(32 << 10));
        assert_eq!(ram_in_bytes("32KiB"), Ok(32 << 10));
        assert_eq!(ram_in_bytes("1.5m"), Ok(3 << 19));
        assert_eq!(ram_in_bytes("64 MB"), Ok(64 << 20));
        assert_eq!(ram_in_bytes("1g"), Ok(1 << 30));
        // Out of range, as Go converts it on amd64: no size at all, not the largest.
        assert_eq!(ram_in_bytes("1e19"), Ok(i64::MIN));
        assert_eq!(ram_in_bytes("16777216t"), Ok(i64::MIN));
        assert!(ram_in_bytes("").is_err());
        assert!(ram_in_bytes("m").is_err());
        assert!(ram_in_bytes("-1").is_err());
        assert!(ram_in_bytes("1x").is_err());
        assert!(ram_in_bytes("1kbb").is_err());
        assert!(ram_in_bytes("1kx").is_err());
    }
}
