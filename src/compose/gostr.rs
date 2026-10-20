//! Go's strings, and what Go's standard library makes of them where compose prints or
//! decides on one: %q (strconv.Quote), the classes of package unicode, strconv's number
//! parsing and the conversions of a float to an integer.
//!
//! A Go string is any bytes; text read from files and the environment need not be
//! UTF-8, and what docker compose prints of it depends on its bytes. Here such text is a
//! Rust string where every byte that is not part of a UTF-8 sequence stands as the code
//! point RAW + byte, from the last private use plane: read with `from_bytes`, written
//! back with `to_bytes`. Text that holds those code points itself reads as raw bytes too.

use super::unicode;

const RAW: u32 = 0x10FF00;

/// Text from bytes, each byte that is not UTF-8 kept as its stand-in.
pub fn from_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for &b in chunk.invalid() {
            out.push(char::from_u32(RAW + u32::from(b)).expect("a code point"));
        }
    }
    out
}

/// The bytes text stands for.
pub fn to_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        match raw_byte(c) {
            Some(b) => out.push(b),
            None => {
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out
}

/// The byte a stand-in is for.
pub fn raw_byte(c: char) -> Option<u8> {
    let c = c as u32;
    (RAW + 0x80..=RAW + 0xff)
        .contains(&c)
        .then(|| (c - RAW) as u8)
}

/// utf8.ValidString.
pub fn is_utf8(text: &str) -> bool {
    !text.chars().any(|c| raw_byte(c).is_some())
}

/// The rune Go reads where a string holds `c`: a byte that is not UTF-8 is U+FFFD.
pub fn rune(c: char) -> char {
    if raw_byte(c).is_some() {
        char::REPLACEMENT_CHARACTER
    } else {
        c
    }
}

fn in_table(table: &[(u32, u32)], c: char) -> bool {
    let c = c as u32;
    table
        .binary_search_by(|&(first, last)| {
            if last < c {
                std::cmp::Ordering::Less
            } else if first > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// strconv.IsPrint.
pub fn is_print(c: char) -> bool {
    in_table(unicode::PRINT, c)
}

/// unicode.IsLetter: category L only, unlike char::is_alphabetic.
pub fn is_letter(c: char) -> bool {
    in_table(unicode::LETTER, rune(c))
}

/// unicode.IsDigit: category Nd only, unlike char::is_numeric.
pub fn is_digit(c: char) -> bool {
    in_table(unicode::DIGIT, rune(c))
}

/// unicode.IsNumber: category N, digits and the rest.
pub fn is_number(c: char) -> bool {
    in_table(unicode::NUMBER, rune(c))
}

/// unicode.IsSpace.
pub fn is_space(c: char) -> bool {
    in_table(unicode::SPACE, rune(c))
}

/// unicode.ToLower: one code point for one, as Go maps them.
pub fn to_lower(c: char) -> char {
    let r = c as u32;
    let i = unicode::LOWER.partition_point(|&(first, _, _, _)| first <= r);
    if i > 0 {
        let (first, last, stride, delta) = unicode::LOWER[i - 1];
        if r <= last && (r - first).is_multiple_of(stride) {
            return char::from_u32(r.wrapping_add_signed(delta)).unwrap_or(c);
        }
    }
    c
}

/// strings.ToLower: a byte that is not UTF-8 becomes U+FFFD.
pub fn to_lower_str(text: &str) -> String {
    text.chars().map(|c| to_lower(rune(c))).collect()
}

/// strconv.Quote, which is also %q and %#v of a string.
pub fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        if let Some(b) = raw_byte(c) {
            out.push_str(&format!("\\x{b:02x}"));
            continue;
        }
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
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{b}' => out.push_str("\\v"),
            c if c < ' ' || c == '\u{7f}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// The text Go gives an I/O error: its errno's, as syscall.Errno.Error() words it.
pub fn errno_text(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => usize::try_from(n)
            .ok()
            .and_then(|i| unicode::ERRNO.get(i))
            .map_or_else(|| format!("errno {n}"), |text| (*text).to_string()),
        None => e.to_string(),
    }
}

/// An *os.PathError: the operation, the path, then the errno's text.
pub fn path_error(op: &str, path: &str, e: &std::io::Error) -> String {
    format!("{op} {path}: {}", errno_text(e))
}

/// strconv.Unquote of a double-quoted string, for reading what Go wrote with %q; text
/// that is not one is returned as it is.
#[cfg(test)]
pub fn unquote(text: &str) -> String {
    let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) else {
        return text.to_string();
    };
    let mut out = Vec::new();
    let mut rest = inner;
    let hex = |digits: &str| u32::from_str_radix(digits, 16).ok();
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        if c != '\\' {
            out.extend_from_slice(&to_bytes(&c.to_string()));
            continue;
        }
        let Some(e) = rest.chars().next() else {
            break;
        };
        rest = &rest[e.len_utf8()..];
        let mut push = |c: char| out.extend_from_slice(c.to_string().as_bytes());
        match e {
            'a' => push('\u{7}'),
            'b' => push('\u{8}'),
            'f' => push('\u{c}'),
            'n' => push('\n'),
            'r' => push('\r'),
            't' => push('\t'),
            'v' => push('\u{b}'),
            'x' if rest.len() >= 2 => {
                if let Some(b) = hex(&rest[..2]) {
                    out.push(b as u8);
                }
                rest = &rest[2..];
            }
            'u' | 'U' => {
                let n = if e == 'u' { 4 } else { 8 };
                if let Some(c) = rest.get(..n).and_then(hex).and_then(char::from_u32) {
                    push(c);
                }
                rest = rest.get(n..).unwrap_or("");
            }
            '0'..='7' if rest.len() >= 2 => {
                if let Ok(b) = u8::from_str_radix(&format!("{e}{}", &rest[..2]), 8) {
                    out.push(b);
                }
                rest = &rest[2..];
            }
            other => push(other),
        }
    }
    from_bytes(&out)
}

/// Why strconv refused a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumError {
    Syntax,
    Range,
}

/// strconv.NumError's text: `strconv.ParseFloat: parsing "x": invalid syntax`.
pub fn num_error(function: &str, text: &str, error: NumError) -> String {
    let why = match error {
        NumError::Syntax => "invalid syntax",
        NumError::Range => "value out of range",
    };
    format!("strconv.{function}: parsing {}: {why}", quote(text))
}

fn underscore_ok(s: &[u8]) -> bool {
    let mut saw = b'^';
    let mut i = 0;
    let s = match s.first() {
        Some(b'-' | b'+') => &s[1..],
        _ => s,
    };
    let mut hex = false;
    if s.len() >= 2 && s[0] == b'0' && matches!(s[1] | 0x20, b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0';
        hex = s[1] | 0x20 == b'x';
    }
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c.is_ascii_digit() || hex && (b'a'..=b'f').contains(&(c | 0x20)) {
            saw = b'0';
            continue;
        }
        if c == b'_' {
            if saw != b'0' {
                return false;
            }
            saw = b'_';
            continue;
        }
        if saw == b'_' {
            return false;
        }
        saw = b'!';
    }
    saw != b'_'
}

/// strconv.ParseUint.
pub fn parse_uint(text: &str, base: u32, bits: u32) -> Result<u64, NumError> {
    let s0 = text.as_bytes();
    if s0.is_empty() {
        return Err(NumError::Syntax);
    }
    let base0 = base == 0;
    let mut s = s0;
    let mut base = base;
    if base == 0 {
        base = 10;
        if s[0] == b'0' {
            if s.len() >= 3 && matches!(s[1] | 0x20, b'b' | b'o' | b'x') {
                base = match s[1] | 0x20 {
                    b'b' => 2,
                    b'o' => 8,
                    _ => 16,
                };
                s = &s[2..];
            } else {
                base = 8;
                s = &s[1..];
            }
        }
    }
    let cutoff = u64::MAX / u64::from(base) + 1;
    let max = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let mut underscores = false;
    let mut n: u64 = 0;
    for &c in s {
        let d = match c {
            b'_' if base0 => {
                underscores = true;
                continue;
            }
            b'0'..=b'9' => c - b'0',
            _ if (c | 0x20).is_ascii_lowercase() => (c | 0x20) - b'a' + 10,
            _ => return Err(NumError::Syntax),
        };
        if u32::from(d) >= base {
            return Err(NumError::Syntax);
        }
        if n >= cutoff {
            return Err(NumError::Range);
        }
        n *= u64::from(base);
        let n1 = n.wrapping_add(u64::from(d));
        if n1 < n || n1 > max {
            return Err(NumError::Range);
        }
        n = n1;
    }
    if underscores && !underscore_ok(s0) {
        return Err(NumError::Syntax);
    }
    Ok(n)
}

/// strconv.ParseInt.
pub fn parse_int(text: &str, base: u32, bits: u32) -> Result<i64, NumError> {
    if text.is_empty() {
        return Err(NumError::Syntax);
    }
    let (neg, digits) = match text.as_bytes()[0] {
        b'+' => (false, &text[1..]),
        b'-' => (true, &text[1..]),
        _ => (false, text),
    };
    let un = match parse_uint(digits, base, bits) {
        Ok(n) => n,
        Err(NumError::Range) => u64::MAX,
        Err(e) => return Err(e),
    };
    let cutoff = 1u64 << (bits - 1);
    if (!neg && un >= cutoff) || (neg && un > cutoff) {
        return Err(NumError::Range);
    }
    Ok(if neg {
        (un as i64).wrapping_neg()
    } else {
        un as i64
    })
}

/// strconv.Atoi on a 64-bit host.
pub fn atoi(text: &str) -> Result<i64, NumError> {
    let s = text.as_bytes();
    if !s.is_empty() && s.len() < 19 {
        let digits = match s[0] {
            b'+' | b'-' => &s[1..],
            _ => s,
        };
        if digits.is_empty() {
            return Err(NumError::Syntax);
        }
        let mut n: i64 = 0;
        for &c in digits {
            if !c.is_ascii_digit() {
                return Err(NumError::Syntax);
            }
            n = n * 10 + i64::from(c - b'0');
        }
        return Ok(if s[0] == b'-' { -n } else { n });
    }
    parse_int(text, 10, 64)
}

/// What strconv's readFloat makes of a float: the mantissa and the binary or decimal
/// exponent, or None on bad syntax.
struct Read {
    mantissa: u64,
    exp: i64,
    neg: bool,
    trunc: bool,
    hex: bool,
    len: usize,
}

fn read_float(s: &[u8]) -> Option<Read> {
    let mut i = 0;
    let mut underscores = false;
    let mut neg = false;
    match s.first()? {
        b'+' => i += 1,
        b'-' => {
            i += 1;
            neg = true;
        }
        _ => {}
    }
    let mut base: u64 = 10;
    let mut max_digits = 19;
    let mut exp_char = b'e';
    let mut hex = false;
    if i + 2 < s.len() && s[i] == b'0' && s[i + 1] | 0x20 == b'x' {
        base = 16;
        max_digits = 16;
        i += 2;
        exp_char = b'p';
        hex = true;
    }
    let (mut sawdot, mut sawdigits) = (false, false);
    let (mut nd, mut nd_mant, mut dp): (i64, i64, i64) = (0, 0, 0);
    let mut mantissa: u64 = 0;
    let mut trunc = false;
    while i < s.len() {
        let c = s[i];
        if c == b'_' {
            underscores = true;
        } else if c == b'.' {
            if sawdot {
                break;
            }
            sawdot = true;
            dp = nd;
        } else if c.is_ascii_digit() {
            sawdigits = true;
            if c == b'0' && nd == 0 {
                dp -= 1;
            } else {
                nd += 1;
                if nd_mant < max_digits {
                    mantissa = mantissa * base + u64::from(c - b'0');
                    nd_mant += 1;
                } else if c != b'0' {
                    trunc = true;
                }
            }
        } else if base == 16 && (b'a'..=b'f').contains(&(c | 0x20)) {
            sawdigits = true;
            nd += 1;
            if nd_mant < max_digits {
                mantissa = mantissa * 16 + u64::from((c | 0x20) - b'a' + 10);
                nd_mant += 1;
            } else {
                trunc = true;
            }
        } else {
            break;
        }
        i += 1;
    }
    if !sawdigits {
        return None;
    }
    if !sawdot {
        dp = nd;
    }
    if base == 16 {
        dp *= 4;
        nd_mant *= 4;
    }
    if i < s.len() && s[i] | 0x20 == exp_char {
        i += 1;
        let mut esign = 1;
        match s.get(i)? {
            b'+' => i += 1,
            b'-' => {
                i += 1;
                esign = -1;
            }
            _ => {}
        }
        if !s.get(i)?.is_ascii_digit() {
            return None;
        }
        let mut e: i64 = 0;
        while i < s.len() && (s[i].is_ascii_digit() || s[i] == b'_') {
            if s[i] == b'_' {
                underscores = true;
            } else if e < 10000 {
                e = e * 10 + i64::from(s[i] - b'0');
            }
            i += 1;
        }
        dp += e * esign;
    } else if base == 16 {
        return None;
    }
    let exp = if mantissa != 0 { dp - nd_mant } else { 0 };
    if underscores && !underscore_ok(&s[..i]) {
        return None;
    }
    Some(Read {
        mantissa,
        exp,
        neg,
        trunc,
        hex,
        len: i,
    })
}

/// strconv's special: infinities, with their sign, and NaN, without.
fn special(s: &[u8]) -> Option<(f64, usize)> {
    let prefix = |s: &[u8], word: &[u8]| {
        s.iter()
            .zip(word)
            .take_while(|(a, b)| a.to_ascii_lowercase() == **b)
            .count()
    };
    let (sign, nsign, rest) = match s.first()? {
        b'+' => (1.0, 1, &s[1..]),
        b'-' => (-1.0, 1, &s[1..]),
        b'n' | b'N' => {
            return (prefix(s, b"nan") == 3).then_some((f64::NAN, 3));
        }
        _ => (1.0, 0, s),
    };
    let n = prefix(rest, b"infinity");
    let n = if 3 < n && n < 8 { 3 } else { n };
    (n == 3 || n == 8).then_some((sign * f64::INFINITY, nsign + n))
}

/// strconv's atofHex, at 64 or 32 bits.
fn atof_hex(read: &Read, single: bool) -> (f64, bool) {
    let (mantbits, expbits, bias): (u32, u32, i64) = if single {
        (23, 8, -127)
    } else {
        (52, 11, -1023)
    };
    let max_exp = (1i64 << expbits) + bias - 2;
    let min_exp = bias + 1;
    let mut mantissa = read.mantissa;
    let mut exp = read.exp + i64::from(mantbits);
    while mantissa != 0 && mantissa >> (mantbits + 2) == 0 {
        mantissa <<= 1;
        exp -= 1;
    }
    if read.trunc {
        mantissa |= 1;
    }
    while mantissa >> (1 + mantbits + 2) != 0 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }
    while mantissa > 1 && exp < min_exp - 2 {
        mantissa = mantissa >> 1 | mantissa & 1;
        exp += 1;
    }
    let mut round = mantissa & 3;
    mantissa >>= 2;
    round |= mantissa & 1;
    exp += 2;
    if round == 3 {
        mantissa += 1;
        if mantissa == 1 << (1 + mantbits) {
            mantissa >>= 1;
            exp += 1;
        }
    }
    if mantissa >> mantbits == 0 {
        exp = bias;
    }
    let mut overflow = false;
    if exp > max_exp {
        mantissa = 1 << mantbits;
        exp = max_exp + 1;
        overflow = true;
    }
    let mut bits = mantissa & ((1 << mantbits) - 1);
    bits |= (((exp - bias) & ((1 << expbits) - 1)) as u64) << mantbits;
    if read.neg {
        bits |= 1 << mantbits << expbits;
    }
    let value = if single {
        f64::from(f32::from_bits(bits as u32))
    } else {
        f64::from_bits(bits)
    };
    (value, overflow)
}

/// strconv.ParseFloat at 64 or 32 bits. A value out of range is an error, as in Go,
/// which also gives the infinity it rounded to.
pub fn parse_float(text: &str, bits: u32) -> Result<f64, NumError> {
    let s = text.as_bytes();
    let single = bits == 32;
    if let Some((value, n)) = special(s) {
        return if n == s.len() {
            Ok(value)
        } else {
            Err(NumError::Syntax)
        };
    }
    let read = read_float(s).ok_or(NumError::Syntax)?;
    if read.len != s.len() {
        return Err(NumError::Syntax);
    }
    let (value, overflow) = if read.hex {
        atof_hex(&read, single)
    } else {
        // Rust rounds a decimal to the nearest float of either width as Go does.
        let digits: String = text.chars().filter(|&c| c != '_').collect();
        let value = if single {
            digits.parse::<f32>().map(f64::from)
        } else {
            digits.parse::<f64>()
        }
        .map_err(|_| NumError::Syntax)?;
        (value, value.is_infinite())
    };
    if overflow {
        Err(NumError::Range)
    } else {
        Ok(value)
    }
}

/// 2^63, where Go's conversions of a float to an integer change their ways.
const TWO63: f64 = (1u64 << 63) as f64;

/// Go's int64(x) of a float64 on amd64: NaN and values out of range give MinInt64.
pub fn f64_to_i64(x: f64) -> i64 {
    if (-TWO63..TWO63).contains(&x) {
        x as i64
    } else {
        i64::MIN
    }
}

/// Go's uint64(x) of a float64 on amd64.
pub fn f64_to_u64(x: f64) -> u64 {
    if x < TWO63 {
        f64_to_i64(x) as u64
    } else {
        f64_to_i64(x - TWO63) as u64 | 1 << 63
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_that_are_not_utf8_survive() {
        let bytes = b"caf\xe9 \xc3";
        let text = from_bytes(bytes);
        assert!(!is_utf8(&text));
        assert_eq!(to_bytes(&text), bytes);
        assert_eq!(quote(&text), "\"caf\\xe9 \\xc3\"");
        assert!(is_utf8("café"));
    }

    #[test]
    fn unquote_reads_what_quote_writes() {
        for text in ["a\u{1}b\u{a0}\u{200b}\"\\", "\u{e0001}\t\n", "plain"] {
            assert_eq!(unquote(&quote(text)), text);
        }
        let raw = from_bytes(b"caf\xe9");
        assert_eq!(unquote(&quote(&raw)), raw);
        assert_eq!(unquote("bare"), "bare");
    }

    #[test]
    fn quote_is_go_s() {
        assert_eq!(quote("a\u{1}b"), "\"a\\x01b\"");
        assert_eq!(quote("a\u{a0}b"), "\"a\\u00a0b\"");
        assert_eq!(quote("a\u{200b}"), "\"a\\u200b\"");
        assert_eq!(quote("\u{7}\"\\"), "\"\\a\\\"\\\\\"");
        assert_eq!(quote("\u{e0001}"), "\"\\U000e0001\"");
        assert_eq!(quote("café \u{fffd}"), "\"café \u{fffd}\"");
    }

    #[test]
    fn classes_are_go_s() {
        assert!(is_letter('a') && is_letter('\u{e9}'));
        // Roman numerals are Nl and combining marks M: not letters to Go.
        assert!(!is_letter('\u{216b}') && !is_letter('\u{93e}'));
        assert!(is_digit('\u{663}') && !is_digit('\u{216b}'));
        assert!(is_space('\u{85}') && !is_space('\u{200b}'));
        assert_eq!(to_lower('\u{130}'), 'i');
        assert_eq!(to_lower('\u{212a}'), 'k');
        assert_eq!(to_lower_str("ABC\u{3a3}"), "abc\u{3c3}");
    }

    #[test]
    fn integers_parse_as_strconv_does() {
        assert_eq!(atoi("-12"), Ok(-12));
        assert_eq!(atoi("1_0"), Err(NumError::Syntax));
        assert_eq!(atoi("99999999999999999999x"), Err(NumError::Range));
        assert_eq!(atoi("9223372036854775807"), Ok(i64::MAX));
        assert_eq!(parse_int("-9223372036854775808", 10, 64), Ok(i64::MIN));
        assert_eq!(parse_uint("0x_1f", 0, 64), Ok(31));
        assert_eq!(parse_uint("70000", 10, 16), Err(NumError::Range));
        assert_eq!(
            num_error("Atoi", "x", NumError::Syntax),
            "strconv.Atoi: parsing \"x\": invalid syntax"
        );
    }

    #[test]
    fn floats_parse_as_strconv_does() {
        assert_eq!(parse_float("1_0", 64), Ok(10.0));
        assert_eq!(parse_float("0x1p-2", 64), Ok(0.25));
        assert_eq!(parse_float("0x1_0p0", 64), Ok(16.0));
        assert_eq!(parse_float("+Inf", 64), Ok(f64::INFINITY));
        assert_eq!(parse_float("+nan", 64), Err(NumError::Syntax));
        assert_eq!(parse_float("infx", 64), Err(NumError::Syntax));
        assert_eq!(parse_float("1e400", 64), Err(NumError::Range));
        assert_eq!(parse_float("1e39", 32), Err(NumError::Range));
        assert_eq!(parse_float("._5", 64), Err(NumError::Syntax));
        assert_eq!(parse_float("5_", 64), Err(NumError::Syntax));
        assert_eq!(parse_float("0x1p", 64), Err(NumError::Syntax));
        assert_eq!(
            parse_float("1.00000017", 32),
            Ok(f64::from(1.000_000_1_f32))
        );
        assert!(parse_float("nan", 64).unwrap().is_nan());
    }

    #[test]
    fn floats_convert_to_integers_as_on_amd64() {
        assert_eq!(f64_to_i64(1e19), i64::MIN);
        assert_eq!(f64_to_i64(f64::NAN), i64::MIN);
        assert_eq!(f64_to_i64(-1.5), -1);
        assert_eq!(f64_to_u64(1e19), 10_000_000_000_000_000_000);
        assert_eq!(f64_to_u64(1e20), 1 << 63);
    }
}
