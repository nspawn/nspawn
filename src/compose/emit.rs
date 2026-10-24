//! The model written out: YAML with go-yaml's encoder
//! (indentation of 2, its choice of plain, quoted and literal scalars, map keys in its
//! natural order) and JSON with Go's encoding/json (indentation of 2, HTML characters
//! escaped, map keys sorted).

use std::fmt::Write;

use super::emitter::{self, Event, ScalarStyle};
use super::value::go_float;

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Null,
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Float32(f32),
    Str(String),
    /// A time.Time: RFC 3339 with its fractions, plain in YAML.
    Time(super::value::Time),
    Seq(Vec<Out>),
    /// A struct: its fields in their order.
    Struct(Vec<(String, Out)>),
    /// A Go map: each format sorts the keys its own way.
    Dict(Vec<(String, Out)>),
}

impl Out {
    pub fn str(s: impl Into<String>) -> Out {
        Out::Str(s.into())
    }
}

/// A float32 as Go's strconv writes it with the shortest precision ('g').
pub fn go_float32(x: f32) -> String {
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        return go_float(x as f64);
    }
    // The shortest digits of the float32, laid out by the float64 rules.
    let digits: f64 = format!("{x:e}").parse().unwrap();
    go_float(digits)
}

/// go-yaml's natural order of map keys: runs of digits compare as numbers, with Go's
/// classes of letters and digits.
fn natural_less(a: &str, b: &str) -> bool {
    use super::gostr::{is_digit, is_letter, rune};
    let ar: Vec<char> = a.chars().map(rune).collect();
    let br: Vec<char> = b.chars().map(rune).collect();
    let value = |c: char| i64::from(c as u32) - i64::from(b'0');
    let mut digits = false;
    let mut i = 0;
    while i < ar.len() && i < br.len() {
        if ar[i] == br[i] {
            digits = is_digit(ar[i]);
            i += 1;
            continue;
        }
        let (al, bl) = (is_letter(ar[i]), is_letter(br[i]));
        if al && bl {
            return ar[i] < br[i];
        }
        if al || bl {
            return if digits { al } else { bl };
        }
        let (mut an, mut bn): (i64, i64) = (0, 0);
        if ar[i] == '0' || br[i] == '0' {
            let mut j = i;
            while j > 0 && is_digit(ar[j - 1]) {
                j -= 1;
                if ar[j] != '0' {
                    an = 1;
                    bn = 1;
                    break;
                }
            }
        }
        let mut ai = i;
        while ai < ar.len() && is_digit(ar[ai]) {
            an = an.wrapping_mul(10).wrapping_add(value(ar[ai]));
            ai += 1;
        }
        let mut bi = i;
        while bi < br.len() && is_digit(br[bi]) {
            bn = bn.wrapping_mul(10).wrapping_add(value(br[bi]));
            bi += 1;
        }
        if an != bn {
            return an < bn;
        }
        if ai != bi {
            return ai < bi;
        }
        return ar[i] < br[i];
    }
    ar.len() < br.len()
}

/// Sorts in go-yaml's order of map keys, keys equal in it kept as they came. Past an
/// int64 a digit run wraps as it does in the reference, and the order is then no total
/// one, on which the standard sort may panic; a merge sort takes it as it is.
pub fn sort_natural<T>(items: &mut Vec<T>, key: impl Fn(&T) -> &str + Copy) {
    fn merged<T>(items: Vec<T>, key: impl Fn(&T) -> &str + Copy) -> Vec<T> {
        if items.len() < 2 {
            return items;
        }
        let mut left = items;
        let right = left.split_off(left.len() / 2);
        let mut left = merged(left, key).into_iter().peekable();
        let mut right = merged(right, key).into_iter().peekable();
        let mut out = Vec::with_capacity(left.len() + right.len());
        while let (Some(a), Some(b)) = (left.peek(), right.peek()) {
            let next = if natural_less(key(b), key(a)) {
                right.next()
            } else {
                left.next()
            };
            out.extend(next);
        }
        out.extend(left);
        out.extend(right);
        out
    }
    *items = merged(std::mem::take(items), key);
}

fn yaml_sorted(entries: &[(String, Out)]) -> Vec<&(String, Out)> {
    let mut sorted: Vec<&(String, Out)> = entries.iter().collect();
    sort_natural(&mut sorted, |e| &e.0);
    sorted
}

/// go-yaml's Encoder at 2 spaces a level, as compose-go writes a project: lines past
/// 80 columns folded.
pub fn yaml(out: &Out) -> String {
    emitter::emit(events(out), 2, Some(80))
}

/// go-yaml's yaml.Marshal: 4 spaces a level, no line folded.
pub fn marshal(out: &Out) -> String {
    emitter::emit(events(out), 4, None)
}

fn events(out: &Out) -> Vec<Event> {
    let mut events = vec![Event::StreamStart, Event::DocumentStart];
    represent(out, &mut events);
    events.push(Event::DocumentEnd);
    events.push(Event::StreamEnd);
    events
}

/// The events of a value, each scalar with the style go-yaml's representer asks for.
fn represent(out: &Out, events: &mut Vec<Event>) {
    let plain = |value: String| Event::Scalar {
        value,
        style: ScalarStyle::Plain,
        binary: false,
    };
    match out {
        Out::Null => events.push(plain("null".to_string())),
        Out::Bool(b) => events.push(plain(b.to_string())),
        Out::Int(i) => events.push(plain(i.to_string())),
        Out::Uint(u) => events.push(plain(u.to_string())),
        Out::Float(x) => events.push(plain(float_yaml(go_float(*x)))),
        Out::Float32(x) => events.push(plain(float_yaml(go_float32(*x)))),
        Out::Str(s) => events.push(string(s)),
        Out::Time(t) => events.push(plain(t.rfc3339())),
        Out::Seq(items) => {
            events.push(Event::SequenceStart { flow: false });
            for item in items {
                represent(item, events);
            }
            events.push(Event::SequenceEnd);
        }
        Out::Struct(entries) => {
            events.push(Event::MappingStart { flow: false });
            for (key, value) in entries {
                events.push(string(key));
                represent(value, events);
            }
            events.push(Event::MappingEnd);
        }
        Out::Dict(entries) => {
            events.push(Event::MappingStart { flow: false });
            for (key, value) in yaml_sorted(entries) {
                events.push(string(key));
                represent(value, events);
            }
            events.push(Event::MappingEnd);
        }
    }
}

/// A string as go-yaml's representer and desolver leave it for the emitter: a block
/// for several lines, quotes for what YAML 1.1 read as something else or what would
/// read back as another type, plain otherwise; bytes that are not UTF-8 as !!binary.
fn string(s: &str) -> Event {
    let literal = |s: &str| s.len() >= 2 && s.chars().any(|c| !c.is_whitespace());
    if !super::gostr::is_utf8(s) {
        let value = base64_lines(&super::gostr::to_bytes(s));
        let style = if !value.contains('\n') {
            ScalarStyle::Plain
        } else if literal(&value) {
            ScalarStyle::Literal
        } else {
            ScalarStyle::DoubleQuoted
        };
        return Event::Scalar {
            value,
            style,
            binary: true,
        };
    }
    let style = if s.contains('\n') {
        if literal(s) {
            ScalarStyle::Literal
        } else {
            ScalarStyle::DoubleQuoted
        }
    } else if base60_float(s) || old_bool(s) || s == "<<" {
        ScalarStyle::SingleQuoted
    } else if !super::yaml::plain_resolves_to_string(s) {
        ScalarStyle::DoubleQuoted
    } else {
        ScalarStyle::Plain
    };
    Event::Scalar {
        value: s.to_string(),
        style,
        binary: false,
    }
}

/// go-yaml's encodeBase64: lines of 70 characters, each ended by a line break when
/// there is more than one.
fn base64_lines(bytes: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    if encoded.len() < 70 {
        return encoded;
    }
    let mut out = String::new();
    for chunk in encoded.as_bytes().chunks(70) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    out
}

fn float_yaml(text: String) -> String {
    match text.as_str() {
        "+Inf" => ".inf".to_string(),
        "-Inf" => "-.inf".to_string(),
        "NaN" => ".nan".to_string(),
        _ => text,
    }
}

/// `^[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+(?:\.[0-9_]*)?$`
fn base60_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'-' | b'+')) {
        i += 1;
    }
    if !b.get(i).is_some_and(u8::is_ascii_digit) {
        return false;
    }
    while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'_') {
        i += 1;
    }
    let mut groups = 0;
    while b.get(i) == Some(&b':') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() && i - start < 2 {
            i += 1;
        }
        let group = &b[start..i];
        let valid = match group.len() {
            1 => true,
            2 => (b'0'..=b'5').contains(&group[0]),
            _ => false,
        };
        if !valid {
            return false;
        }
        groups += 1;
    }
    if groups == 0 {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(|c| c.is_ascii_digit() || *c == b'_') {
            i += 1;
        }
    }
    i == b.len()
}

fn old_bool(s: &str) -> bool {
    matches!(
        s,
        "y" | "Y"
            | "yes"
            | "Yes"
            | "YES"
            | "on"
            | "On"
            | "ON"
            | "n"
            | "N"
            | "no"
            | "No"
            | "NO"
            | "off"
            | "Off"
            | "OFF"
    )
}

/// Go's json.MarshalIndent(value, "", "  ").
pub fn json(out: &Out) -> Result<String, String> {
    json_with(out, Some("  "), true)
}

/// Go's json.Marshal: no spaces, no line breaks.
pub fn json_compact(out: &Out) -> Result<String, String> {
    json_with(out, None, true)
}

/// encoding/json with an indentation (None for compact) and HTML characters escaped
/// or not; an infinity or NaN is its error, as JSON has no such numbers.
pub fn json_with(out: &Out, indent: Option<&str>, escape_html: bool) -> Result<String, String> {
    let mut text = String::new();
    json_value(out, 0, indent, escape_html, &mut text)?;
    Ok(text)
}

fn json_value(
    out: &Out,
    depth: usize,
    indent: Option<&str>,
    html: bool,
    text: &mut String,
) -> Result<(), String> {
    let newline = |text: &mut String, depth: usize| {
        if let Some(indent) = indent {
            text.push('\n');
            text.push_str(&indent.repeat(depth));
        }
    };
    let finite = |x: f64, single: bool| {
        if x.is_finite() {
            Ok(json_float(x, single))
        } else {
            let text = if x.is_nan() {
                "NaN"
            } else if x > 0.0 {
                "+Inf"
            } else {
                "-Inf"
            };
            Err(format!("json: unsupported value: {text}"))
        }
    };
    match out {
        Out::Null => text.push_str("null"),
        Out::Bool(b) => text.push_str(&b.to_string()),
        Out::Int(i) => text.push_str(&i.to_string()),
        Out::Uint(u) => text.push_str(&u.to_string()),
        Out::Float(x) => text.push_str(&finite(*x, false)?),
        Out::Float32(x) => text.push_str(&finite(f64::from(*x), true)?),
        Out::Str(s) => json_string(s, html, text),
        Out::Time(t) => match t.json_error() {
            Some(e) => {
                return Err(format!(
                    "json: error calling MarshalJSON for type time.Time: {e}"
                ))
            }
            None => json_string(&t.rfc3339(), html, text),
        },
        Out::Seq(items) => {
            if items.is_empty() {
                text.push_str("[]");
                return Ok(());
            }
            text.push('[');
            for (i, item) in items.iter().enumerate() {
                newline(text, depth + 1);
                json_value(item, depth + 1, indent, html, text)?;
                if i + 1 < items.len() {
                    text.push(',');
                }
            }
            newline(text, depth);
            text.push(']');
        }
        Out::Struct(entries) | Out::Dict(entries) => {
            if entries.is_empty() {
                text.push_str("{}");
                return Ok(());
            }
            let mut ordered: Vec<&(String, Out)> = entries.iter().collect();
            if matches!(out, Out::Dict(_)) {
                ordered.sort_by(|a, b| a.0.cmp(&b.0));
            }
            text.push('{');
            for (i, (key, value)) in ordered.iter().enumerate() {
                newline(text, depth + 1);
                json_string(key, html, text);
                text.push(':');
                if indent.is_some() {
                    text.push(' ');
                }
                json_value(value, depth + 1, indent, html, text)?;
                if i + 1 < ordered.len() {
                    text.push(',');
                }
            }
            newline(text, depth);
            text.push('}');
        }
    }
    Ok(())
}

/// encoding/json's floats: plain digits, an exponent below 1e-6 or from 1e21 up.
fn json_float(x: f64, single: bool) -> String {
    let abs = x.abs();
    // floatEncoder: a float32 is compared as a float32.
    let exponent = if single {
        let abs = abs as f32;
        abs != 0.0 && (abs < 1e-6 || abs >= 1e21)
    } else {
        abs != 0.0 && (abs < 1e-6 || abs >= 1e21)
    };
    let digits: String = if single {
        format!("{:e}", x as f32)
    } else {
        format!("{x:e}")
    };
    let (mantissa, exp) = digits.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if exponent {
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{}", exp.abs());
    }
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let point = exp + 1;
    let body = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!(
            "{}.{}",
            &digits[..point as usize],
            &digits[point as usize..]
        )
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

/// encoding/json's string: a byte that is not UTF-8 is written as \ufffd.
pub fn json_string(s: &str, html: bool, text: &mut String) {
    text.push('"');
    for c in s.chars() {
        match c {
            c if super::gostr::raw_byte(c).is_some() => text.push_str("\\ufffd"),
            '"' => text.push_str("\\\""),
            '\\' => text.push_str("\\\\"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            '\u{8}' => text.push_str("\\b"),
            '\u{c}' => text.push_str("\\f"),
            '<' | '>' | '&' if html => {
                let _ = write!(text, "\\u{:04x}", c as u32);
            }
            '\u{2028}' | '\u{2029}' => {
                let _ = write!(text, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(text, "\\u{:04x}", c as u32);
            }
            c => text.push(c),
        }
    }
    text.push('"');
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_float32_at_the_edge_is_written_as_go_writes_it() {
        assert_eq!(json_float(f64::from(1e-6f32), true), "0.000001");
        assert_eq!(json_float(f64::from(9e-7f32), true), "9e-7");
        assert_eq!(json_float(f64::from(1.1e-6f32), true), "0.0000011");
        assert_eq!(json_float(1e-7, false), "1e-7");
    }

    use super::*;

    fn s(text: &str) -> Out {
        Out::str(text)
    }

    #[test]
    fn strings_take_go_yaml_s_quotes() {
        let doc = Out::Struct(vec![
            ("plain".into(), s("nginx:1.25")),
            ("number".into(), s("493")),
            ("bool".into(), s("true")),
            ("old".into(), s("yes")),
            ("empty".into(), s("")),
            ("colon".into(), s("a: b")),
            ("port".into(), s("8080:80")),
            ("sexagesimal".into(), s("1:30")),
            ("lead".into(), s(" x")),
            ("dash".into(), s("- x")),
            ("hash".into(), s("a #b")),
            ("star".into(), s("*x")),
            ("time".into(), s("2001-12-14 00:00:00 +0000 UTC")),
            ("date".into(), s("2001-12-14")),
            ("tab".into(), s("a\tb")),
            ("n".into(), s("x")),
        ]);
        assert_eq!(
            yaml(&doc),
            "plain: nginx:1.25\nnumber: \"493\"\nbool: \"true\"\nold: 'yes'\nempty: \"\"\n\
             colon: 'a: b'\nport: 8080:80\nsexagesimal: '1:30'\nlead: ' x'\ndash: '- x'\nhash: 'a #b'\nstar: '*x'\n\
             time: 2001-12-14 00:00:00 +0000 UTC\ndate: \"2001-12-14\"\ntab: \"a\\tb\"\n'n': x\n"
        );
    }

    #[test]
    fn collections_are_laid_out_as_go_yaml_does() {
        let doc = Out::Struct(vec![(
            "services".into(),
            Out::Dict(vec![
                ("web10".into(), Out::Struct(vec![("image".into(), s("a"))])),
                (
                    "web2".into(),
                    Out::Struct(vec![
                        ("command".into(), Out::Seq(vec![s("sh"), s("-c")])),
                        (
                            "volumes".into(),
                            Out::Seq(vec![Out::Struct(vec![
                                ("type".into(), s("bind")),
                                ("bind".into(), Out::Struct(vec![])),
                            ])]),
                        ),
                        (
                            "nested".into(),
                            Out::Seq(vec![Out::Seq(vec![s("x"), s("z")])]),
                        ),
                        ("script".into(), s("echo a\necho b\n")),
                        ("cpus".into(), Out::Float32(0.1)),
                        ("none".into(), Out::Null),
                    ]),
                ),
            ]),
        )]);
        assert_eq!(
            yaml(&doc),
            "services:\n  web2:\n    command:\n      - sh\n      - -c\n    volumes:\n      - type: bind\n        bind: {}\n    \
             nested:\n      - - x\n        - z\n    script: |\n      echo a\n      echo b\n    cpus: 0.1\n    none: null\n  \
             web10:\n    image: a\n"
        );
    }

    #[test]
    fn json_is_go_s() {
        let doc = Out::Struct(vec![
            ("name".into(), s("x<y>&z")),
            ("cpus".into(), Out::Float32(0.1)),
            ("big".into(), Out::Float(1e21)),
            ("empty".into(), Out::Seq(vec![])),
            (
                "map".into(),
                Out::Dict(vec![("b".into(), Out::Int(1)), ("a".into(), Out::Null)]),
            ),
        ]);
        assert_eq!(
            json(&doc).unwrap(),
            "{\n  \"name\": \"x\\u003cy\\u003e\\u0026z\",\n  \"cpus\": 0.1,\n  \"big\": 1e+21,\n  \"empty\": [],\n  \
             \"map\": {\n    \"a\": null,\n    \"b\": 1\n  }\n}"
        );
        assert_eq!(
            json(&Out::Float32(f32::INFINITY)),
            Err("json: unsupported value: +Inf".to_string())
        );
        let control = Out::str(format!(
            "\u{8}\u{c}{}",
            super::super::gostr::from_bytes(b"\x80")
        ));
        assert_eq!(json(&control).unwrap(), "\"\\b\\f\\ufffd\"");
    }

    fn from_json(value: &serde_json::Value) -> Out {
        match value {
            serde_json::Value::Null => Out::Null,
            serde_json::Value::Bool(b) => Out::Bool(*b),
            serde_json::Value::Number(n) => Out::Float(n.as_f64().unwrap()),
            serde_json::Value::String(s) => Out::str(s.as_str()),
            serde_json::Value::Array(items) => Out::Seq(items.iter().map(from_json).collect()),
            serde_json::Value::Object(map) => {
                Out::Dict(map.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
            }
        }
    }

    fn json_out(text: &str) -> Out {
        from_json(&serde_json::from_str(text).unwrap())
    }

    /// What go-yaml's Encoder and yaml.Marshal write of these (tests/compose/go-yaml.sh
    /// emit): long scalars folded in each style past 80 columns by the Encoder only, a
    /// value that starts with a BOM escaped whole, a key past 128 characters written as
    /// a complex key, block scalars with their hints.
    #[test]
    fn output_is_go_yaml_s() {
        let out = json_out(
            r#"{"services": {"web": {"environment": {"FLAGS": "disable-email-verification enable-smtp enable-prepl-server disable-secure-session-cookies enable-mcp enable-admin-console"}}}}"#,
        );
        assert_eq!(yaml(&out), concat!(
                "services:\n",
                "  web:\n",
                "    environment:\n",
                "      FLAGS: disable-email-verification enable-smtp enable-prepl-server disable-secure-session-cookies\n",
                "        enable-mcp enable-admin-console\n",
            ));
        assert_eq!(marshal(&out), concat!(
                "services:\n",
                "    web:\n",
                "        environment:\n",
                "            FLAGS: disable-email-verification enable-smtp enable-prepl-server disable-secure-session-cookies enable-mcp enable-admin-console\n",
            ));
        let out = json_out(
            r#"{"command": ["sh", "-c", "echo a: b && word word word word word word word word word word word word word word word word word word word word word word word word word"]}"#,
        );
        assert_eq!(yaml(&out), concat!(
                "command:\n",
                "  - sh\n",
                "  - -c\n",
                "  - 'echo a: b && word word word word word word word word word word word word word\n",
                "    word word word word word word word word word word word word'\n",
            ));
        assert_eq!(marshal(&out), concat!(
                "command:\n",
                "    - sh\n",
                "    - -c\n",
                "    - 'echo a: b && word word word word word word word word word word word word word word word word word word word word word word word word word'\n",
            ));
        let out = json_out(
            r#"{"x": "tab\there  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced"}"#,
        );
        assert_eq!(yaml(&out), concat!(
                "x: \"tab\\there  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced\n",
                "  \\ spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced\n",
                "  \\ spaced\"\n",
            ));
        assert_eq!(marshal(&out), "x: \"tab\\there  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced  spaced\"\n");
        let out = json_out(r#"{"x": "﻿ab c"}"#);
        assert_eq!(yaml(&out), "x: \"\\uFEFF\\x61\\x62\\x20\\x63\"\n");
        assert_eq!(marshal(&out), "x: \"\\uFEFF\\x61\\x62\\x20\\x63\"\n");
        let out = json_out(
            r#"{"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk": "v", "short": "w"}"#,
        );
        assert_eq!(yaml(&out), concat!(
                "? kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\n",
                ": v\n",
                "short: w\n",
            ));
        assert_eq!(marshal(&out), concat!(
                "? kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk\n",
                ": v\n",
                "short: w\n",
            ));
        let out = json_out(r#"{"a": " lead\nnext\n", "b": "keep\n\n", "c": "strip\nx"}"#);
        assert_eq!(
            yaml(&out),
            concat!(
                "a: |2\n",
                "   lead\n",
                "  next\n",
                "b: |+\n",
                "  keep\n",
                "\n",
                "c: |-\n",
                "  strip\n",
                "  x\n",
            )
        );
        assert_eq!(
            marshal(&out),
            concat!(
                "a: |4\n",
                "     lead\n",
                "    next\n",
                "b: |+\n",
                "    keep\n",
                "\n",
                "c: |-\n",
                "    strip\n",
                "    x\n",
            )
        );
        let out = json_out(r#"{"a": [["x", "y"], [], {}], "b": {"c": []}}"#);
        assert_eq!(
            yaml(&out),
            concat!(
                "a:\n",
                "  - - x\n",
                "    - 'y'\n",
                "  - []\n",
                "  - {}\n",
                "b:\n",
                "  c: []\n",
            )
        );
        assert_eq!(
            marshal(&out),
            concat!(
                "a:\n",
                "    - - x\n",
                "      - 'y'\n",
                "    - []\n",
                "    - {}\n",
                "b:\n",
                "    c: []\n",
            )
        );
    }

    /// Each JSON file under NSPAWN_YAML_EMIT against the YAML go-yaml made of it, with
    /// the Encoder as compose-go sets it up in a file next to it with `.yaml` added,
    /// with yaml.Marshal in one with `.yaml4` added (tests/compose/go-yaml.sh emit).
    #[test]
    #[ignore]
    fn the_emitter_matches_go_yaml() {
        let root = std::env::var("NSPAWN_YAML_EMIT").expect("NSPAWN_YAML_EMIT");
        let mut failures = Vec::new();
        let mut compared = 0;
        let mut paths: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        for path in paths {
            let expected = path.with_file_name(format!(
                "{}.yaml",
                path.file_name().unwrap().to_string_lossy()
            ));
            let Ok(want) = std::fs::read_to_string(&expected) else {
                continue;
            };
            compared += 1;
            let value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let value = from_json(&value);
            let got = yaml(&value);
            if got != want {
                failures.push(format!(
                    "{}:\n--- go-yaml\n{want}--- nspawn\n{got}",
                    path.display()
                ));
            }
            // yaml.Marshal, in a file with `.yaml4` added.
            let marshalled = expected.with_extension("yaml4");
            if let Ok(want) = std::fs::read_to_string(&marshalled) {
                let got = marshal(&value);
                if got != want {
                    failures.push(format!(
                        "{} (yaml.Marshal):\n--- go-yaml\n{want}--- nspawn\n{got}",
                        path.display()
                    ));
                }
            }
        }
        assert!(compared > 0, "no inputs");
        assert!(
            failures.is_empty(),
            "{} of {compared} differ:\n{}",
            failures.len(),
            failures
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn keys_sort_naturally() {
        assert!(natural_less("web2", "web10"));
        assert!(natural_less("a", "b"));
        assert!(natural_less("a", "ab"));
        assert!(!natural_less("web10", "web2"));
        // Digit runs past an int64 make the order no total one; sorting them must not
        // panic.
        let mut keys: Vec<String> = (0..40u64)
            .map(|i| {
                if i % 2 == 0 {
                    format!(
                        "id-{}",
                        10_000_000_000_000_000_000u128 + u128::from(i) * 7_919_000_000_000_000_001
                    )
                } else {
                    format!("id-{}", i * 104_729)
                }
            })
            .collect();
        sort_natural(&mut keys, |k| k.as_str());
        assert_eq!(keys.len(), 40);
        let mut plain = vec!["web10".to_string(), "web2".to_string(), "a".to_string()];
        sort_natural(&mut plain, |k| k.as_str());
        assert_eq!(plain, ["a", "web2", "web10"]);
    }
}
