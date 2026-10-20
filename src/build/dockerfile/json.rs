//! The part of Go's encoding/json that the parser depends on: strict syntax,
//! a nesting limit of 10000, U+FFFD for invalid surrogate escapes, and
//! numbers that must fit a float64 (a number that does not makes the whole
//! document fail to decode).
//!
//! Only the top level is materialised: the reference decodes JSON arrays of
//! strings (exec form) and objects with string values (syntax directive).

const MAX_DEPTH: usize = 10000;

/// A JSON value element or member: its text when it is a string.
pub(crate) type Item = Option<String>;

#[derive(Debug, PartialEq)]
pub(crate) enum Document {
    Array(Vec<Item>),
    Object(Vec<(String, Item)>),
    Null,
    Other,
}

/// Decodes a whole document, `None` where the reference's Unmarshal fails.
pub(crate) fn decode(input: &str) -> Option<Document> {
    let mut p = Decoder {
        b: input.as_bytes(),
        s: input,
        i: 0,
    };
    p.ws();
    let doc = match p.peek()? {
        b'[' => {
            p.i += 1;
            let mut items = Vec::new();
            p.ws();
            if p.peek() == Some(b']') {
                p.i += 1;
            } else {
                loop {
                    items.push(p.item()?);
                    p.ws();
                    match p.peek()? {
                        b',' => p.i += 1,
                        b']' => {
                            p.i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            Document::Array(items)
        }
        b'{' => {
            p.i += 1;
            let mut members = Vec::new();
            p.ws();
            if p.peek() == Some(b'}') {
                p.i += 1;
            } else {
                loop {
                    p.ws();
                    let key = p.string()?;
                    p.ws();
                    if p.peek()? != b':' {
                        return None;
                    }
                    p.i += 1;
                    members.push((key, p.item()?));
                    p.ws();
                    match p.peek()? {
                        b',' => p.i += 1,
                        b'}' => {
                            p.i += 1;
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            Document::Object(members)
        }
        b'n' => {
            p.skip_value(0)?;
            Document::Null
        }
        _ => {
            p.skip_value(0)?;
            Document::Other
        }
    };
    p.ws();
    (p.i == p.b.len()).then_some(doc)
}

struct Decoder<'a> {
    b: &'a [u8],
    s: &'a str,
    i: usize,
}

impl Decoder<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    /// An element of the top-level container, which sits at depth 1.
    fn item(&mut self) -> Option<Item> {
        self.ws();
        if self.peek()? == b'"' {
            return self.string().map(Some);
        }
        self.skip_value(1)?;
        Some(None)
    }

    /// Validates one value without keeping it, iteratively so that deep
    /// nesting cannot exhaust the stack.
    fn skip_value(&mut self, depth: usize) -> Option<()> {
        // true for an array, false for an object
        let mut stack: Vec<bool> = Vec::new();
        'value: loop {
            self.ws();
            match self.peek()? {
                b'[' => {
                    self.i += 1;
                    if depth + stack.len() + 1 > MAX_DEPTH {
                        return None;
                    }
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                    } else {
                        stack.push(true);
                        continue 'value;
                    }
                }
                b'{' => {
                    self.i += 1;
                    if depth + stack.len() + 1 > MAX_DEPTH {
                        return None;
                    }
                    self.ws();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                    } else {
                        stack.push(false);
                        self.member_key()?;
                        continue 'value;
                    }
                }
                b'"' => {
                    self.string()?;
                }
                b't' => self.literal("true")?,
                b'f' => self.literal("false")?,
                b'n' => self.literal("null")?,
                _ => self.number()?,
            }
            // A value is complete: close containers until one continues.
            loop {
                let Some(&is_array) = stack.last() else {
                    return Some(());
                };
                self.ws();
                match self.peek()? {
                    b',' => {
                        self.i += 1;
                        if !is_array {
                            self.member_key()?;
                        }
                        continue 'value;
                    }
                    b']' if is_array => {
                        self.i += 1;
                        stack.pop();
                    }
                    b'}' if !is_array => {
                        self.i += 1;
                        stack.pop();
                    }
                    _ => return None,
                }
            }
        }
    }

    fn member_key(&mut self) -> Option<()> {
        self.ws();
        self.string()?;
        self.ws();
        if self.peek()? != b':' {
            return None;
        }
        self.i += 1;
        Some(())
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Some(())
        } else {
            None
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - start
    }

    fn number(&mut self) -> Option<()> {
        let start = self.i;
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek()? {
            b'0' => self.i += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if self.digits() == 0 {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return None;
            }
        }
        let value: f64 = self.s[start..self.i].parse().ok()?;
        value.is_finite().then_some(())
    }

    fn hex4(&self, at: usize) -> Option<u32> {
        let h = self.b.get(at..at + 4)?;
        let h = std::str::from_utf8(h).ok()?;
        if !h.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(h, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.peek()?;
            match c {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    let e = *self.b.get(self.i + 1)?;
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\x08'),
                        b'f' => out.push('\x0c'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let r = self.hex4(self.i)?;
                            self.i += 4;
                            if (0xD800..0xE000).contains(&r) {
                                // A high surrogate followed by an escaped low
                                // one is a pair; anything else is replaced.
                                let low = if self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                    self.hex4(self.i + 2)
                                } else {
                                    None
                                };
                                match low {
                                    Some(lo)
                                        if (0xD800..0xDC00).contains(&r)
                                            && (0xDC00..0xE000).contains(&lo) =>
                                    {
                                        let c = 0x10000 + ((r - 0xD800) << 10) + (lo - 0xDC00);
                                        out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                                        self.i += 6;
                                    }
                                    _ => out.push('\u{fffd}'),
                                }
                            } else {
                                out.push(char::from_u32(r).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => return None,
                    }
                }
                0x00..=0x1f => return None,
                _ => {
                    let ch = self.s[self.i..].chars().next()?;
                    out.push(ch);
                    self.i += ch.len_utf8();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Document {
        Document::Array(items.iter().map(|s| Some((*s).to_string())).collect())
    }

    #[test]
    fn arrays() {
        assert_eq!(decode("[]"), Some(Document::Array(vec![])));
        assert_eq!(decode(" [ \"a\" , \"b\" ] "), Some(strings(&["a", "b"])));
        assert_eq!(
            decode(r#"["a", 1, {"x": [true, null]}, -0.5e3]"#),
            Some(Document::Array(vec![Some("a".into()), None, None, None]))
        );
        assert_eq!(decode(r#"["a" "b"]"#), None);
        assert_eq!(decode(r#"["a",]"#), None);
        assert_eq!(decode(r#"["a"] x"#), None);
        assert_eq!(decode(r#"["a", 01]"#), None);
        assert_eq!(decode(r#"["a", 1e400]"#), None);
        assert_eq!(decode("[\"\t\"]"), None);
        assert_eq!(decode(r#"['a']"#), None);
    }

    #[test]
    fn string_escapes() {
        let u = |hex: &str| format!("\\u{hex}");
        assert_eq!(
            decode(&format!(
                r#"["\" \\ \/ \b \f \n \r \t {} {}"]"#,
                u("0000"),
                u("2665")
            )),
            Some(strings(&["\" \\ / \u{8} \u{c} \n \r \t \u{0} \u{2665}"]))
        );
        assert_eq!(
            decode(&format!(r#"["{}{}"]"#, u("d83d"), u("de00"))),
            Some(strings(&["\u{1f600}"]))
        );
        assert_eq!(
            decode(&format!(r#"["{}x"]"#, u("d83d"))),
            Some(strings(&["\u{fffd}x"]))
        );
        assert_eq!(
            decode(&format!(r#"["{}{}"]"#, u("de00"), u("d83d"))),
            Some(strings(&["\u{fffd}\u{fffd}"]))
        );
        assert_eq!(decode("[\"\u{1f600}\"]"), Some(strings(&["\u{1f600}"])));
        assert_eq!(decode(r#"["\x"]"#), None);
        assert_eq!(decode(r#"["\u12"]"#), None);
    }

    #[test]
    fn nesting_limit() {
        let ok = format!(
            "[{}{}]",
            "[".repeat(MAX_DEPTH - 1),
            "]".repeat(MAX_DEPTH - 1)
        );
        assert!(decode(&ok).is_some());
        let deep = format!("[{}{}]", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert_eq!(decode(&deep), None);
    }

    #[test]
    fn objects() {
        assert_eq!(
            decode(r#"{"syntax": "foo", "bar": ["abc"], "syntax": "baz"}"#),
            Some(Document::Object(vec![
                ("syntax".into(), Some("foo".into())),
                ("bar".into(), None),
                ("syntax".into(), Some("baz".into())),
            ]))
        );
        assert_eq!(decode(r#"{"syntax": "foo""#), None);
        assert_eq!(decode("null"), Some(Document::Null));
        assert_eq!(decode("nul"), None);
        assert_eq!(decode("\"x\""), Some(Document::Other));
    }
}
