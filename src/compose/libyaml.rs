//! The reader, scanner and parser of go.yaml.in/yaml/v4 (its port of libyaml), the
//! release compose-go reads compose files with: the same files are accepted and the
//! same errors are given, at the same point of the input. Events are what this module
//! makes. Comments are tracked as go-yaml tracks them, since where a block ends depends
//! on them, but their text goes nowhere.

use std::fmt;

/// A position in the input: characters from the start, and line and column from 1
/// (0 when unknown).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mark {
    pub index: usize,
    pub line: usize,
    pub column: usize,
}

impl Mark {
    /// go-yaml's shortString: `L3.C5`, `L3`, or `<unknown position>`.
    fn short(&self) -> String {
        if self.line == 0 {
            "<unknown position>".to_string()
        } else if self.column > 0 {
            format!("L{}.C{}", self.line, self.column)
        } else {
            format!("L{}", self.line)
        }
    }

    /// go-yaml's rangeString from this mark to `end`.
    fn range(&self, end: &Mark) -> String {
        let start = self.short();
        if self.line == end.line {
            if self.column == 0 && end.column == 0 {
                return start;
            }
            if self.column > 0 && end.column > 0 {
                if self.column == end.column {
                    return start;
                }
                return format!("{start}-C{}", end.column);
            }
        }
        format!("{start}-{}", end.short())
    }
}

impl fmt::Display for Mark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            return f.write_str("<unknown position>");
        }
        write!(f, "line {}", self.line)?;
        if self.column > 0 {
            write!(f, ", column {}", self.column)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Reader,
    Scanner,
    Parser,
    Composer,
    Resolver,
    Constructor,
}

impl Stage {
    fn name(self) -> &'static str {
        match self {
            Stage::Reader => "reader",
            Stage::Scanner => "scanner",
            Stage::Parser => "parser",
            Stage::Composer => "composer",
            Stage::Resolver => "resolver",
            Stage::Constructor => "constructor",
        }
    }
}

/// go-yaml's LoadError: what went wrong, where, and in what construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub stage: Stage,
    pub message: String,
    pub mark: Mark,
    pub context: Option<(String, Mark)>,
}

impl LoadError {
    pub fn new(stage: Stage, message: impl Into<String>, mark: Mark) -> Self {
        LoadError {
            stage,
            message: message.into(),
            mark,
            context: None,
        }
    }

    fn with_context(
        stage: Stage,
        context: &str,
        context_mark: Mark,
        message: impl Into<String>,
        mark: Mark,
    ) -> Self {
        LoadError {
            stage,
            message: message.into(),
            mark,
            context: Some((context.to_string(), context_mark)),
        }
    }

    /// go-yaml's simpleError, how a list of construct errors shows each one.
    pub fn simple(&self) -> String {
        let mut out = String::new();
        if let Some((context, context_mark)) = &self.context {
            out.push_str(&format!("{context} at {context_mark}: "));
        }
        if self.context.as_ref().is_none_or(|(_, m)| *m != self.mark) {
            if self.mark.line > 0 {
                out.push_str(&format!("line {}: ", self.mark.line));
            } else {
                out.push_str("<unknown position>: ");
            }
        }
        out.push_str(&self.message);
        out
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.context {
            Some((context, context_mark)) => write!(
                f,
                "go-yaml load error in {} ({context}) at {}: {}",
                self.stage.name(),
                context_mark.range(&self.mark),
                self.message
            ),
            None => write!(
                f,
                "go-yaml load error in {} at {}: {}",
                self.stage.name(),
                self.mark.short(),
                self.message
            ),
        }
    }
}

impl std::error::Error for LoadError {}

type Result<T> = std::result::Result<T, LoadError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarStyle {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    StreamStart,
    StreamEnd,
    DocumentStart {
        implicit: bool,
    },
    DocumentEnd {
        implicit: bool,
    },
    Alias {
        anchor: String,
    },
    Scalar {
        anchor: Option<String>,
        tag: String,
        value: String,
        style: ScalarStyle,
        plain_implicit: bool,
        quoted_implicit: bool,
    },
    SequenceStart {
        anchor: Option<String>,
        tag: String,
        implicit: bool,
        flow: bool,
    },
    SequenceEnd,
    MappingStart {
        anchor: Option<String>,
        tag: String,
        implicit: bool,
        flow: bool,
    },
    MappingEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub start: Mark,
    pub end: Mark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    StreamStart,
    StreamEnd,
    VersionDirective,
    TagDirective,
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowSequenceEnd,
    FlowMappingStart,
    FlowMappingEnd,
    BlockEntry,
    FlowEntry,
    Key,
    Value,
    Alias,
    Anchor,
    Tag,
    Scalar,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    start: Mark,
    end: Mark,
    /// The scalar's value, the anchor's name, the tag's handle, the directive's handle.
    value: Vec<u8>,
    /// The tag's suffix.
    suffix: Vec<u8>,
    /// The %TAG directive's prefix.
    prefix: Vec<u8>,
    style: ScalarStyle,
    major: i8,
    minor: i8,
}

impl Token {
    fn new(kind: TokenKind, start: Mark, end: Mark) -> Self {
        Token {
            kind,
            start,
            end,
            value: Vec::new(),
            suffix: Vec::new(),
            prefix: Vec::new(),
            style: ScalarStyle::Plain,
            major: 0,
            minor: 0,
        }
    }
}

/// A comment as go-yaml keeps it while scanning: where it is and what it is attached
/// to.
#[derive(Debug, Clone, Default)]
struct Comment {
    scan_mark: Mark,
    token_mark: Mark,
    start_mark: Mark,
    end_mark: Mark,
    head: Vec<u8>,
    line: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SimpleKey {
    flow_level: usize,
    required: bool,
    token_number: usize,
    mark: Mark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    StreamStart,
    ImplicitDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    BlockNode,
    BlockSequenceFirstEntry,
    BlockSequenceEntry,
    IndentlessSequenceEntry,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingValue,
    FlowSequenceFirstEntry,
    FlowSequenceEntry,
    FlowSequenceEntryMappingKey,
    FlowSequenceEntryMappingValue,
    FlowSequenceEntryMappingEnd,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingValue,
    FlowMappingEmptyValue,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Any,
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// The size of the chunks go-yaml reads its input in, which decides when a bad byte
/// is found.
const RAW_BUFFER_SIZE: usize = 512;
/// go-yaml's DefaultDepthCheck: flow levels and block indentation levels.
const MAX_DEPTH: usize = 10000;
const MAX_NUMBER_LENGTH: i8 = 2;

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn is_alpha(b: &[u8], i: usize) -> bool {
    let c = at(b, i);
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

fn is_flow_indicator(b: &[u8], i: usize) -> bool {
    matches!(at(b, i), b'[' | b']' | b'{' | b'}' | b',')
}

fn is_tag_uri_char(b: &[u8], i: usize, verbatim: bool) -> bool {
    if is_alpha(b, i) {
        return true;
    }
    match at(b, i) {
        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b'.' | b'!' | b'~'
        | b'*' | b'\'' | b'(' | b')' | b'%' => true,
        b',' | b'[' | b']' | b'{' | b'}' => verbatim,
        _ => false,
    }
}

fn is_hex(b: &[u8], i: usize) -> bool {
    at(b, i).is_ascii_hexdigit()
}

fn as_hex(b: &[u8], i: usize) -> u32 {
    let c = at(b, i);
    match c {
        b'A'..=b'F' => (c - b'A' + 10) as u32,
        b'a'..=b'f' => (c - b'a' + 10) as u32,
        _ => (c.wrapping_sub(b'0')) as u32,
    }
}

fn is_printable(b: &[u8], i: usize) -> bool {
    let (c, c1, c2) = (at(b, i), at(b, i + 1), at(b, i + 2));
    c == 0x0A
        || (0x20..=0x7E).contains(&c)
        || (c == 0xC2 && c1 >= 0xA0)
        || (c > 0xC2 && c < 0xED)
        || (c == 0xED && c1 < 0xA0)
        || c == 0xEE
        || (c == 0xEF && !(c1 == 0xBB && c2 == 0xBF) && !(c1 == 0xBF && (c2 == 0xBE || c2 == 0xBF)))
}

fn is_anchor_char(b: &[u8], i: usize, bom: bool) -> bool {
    if at(b, i) == b':' {
        return false;
    }
    is_printable(b, i)
        && !is_line_break(b, i)
        && !is_blank(b, i)
        && !bom
        && !is_flow_indicator(b, i)
        && at(b, i) <= 0x7F
}

fn is_blank(b: &[u8], i: usize) -> bool {
    matches!(at(b, i), b' ' | b'\t')
}

fn is_line_break(b: &[u8], i: usize) -> bool {
    let (c, c1, c2) = (at(b, i), at(b, i + 1), at(b, i + 2));
    c == b'\r'
        || c == b'\n'
        || (c == 0xC2 && c1 == 0x85)
        || (c == 0xE2 && c1 == 0x80 && (c2 == 0xA8 || c2 == 0xA9))
}

fn is_break_or_zero(b: &[u8], i: usize) -> bool {
    at(b, i) == 0 || is_line_break(b, i)
}

fn is_blank_or_zero(b: &[u8], i: usize) -> bool {
    is_blank(b, i) || is_break_or_zero(b, i)
}

fn is_end_of_scalar_in_flow(b: &[u8], i: usize) -> bool {
    match at(b, i) {
        b',' | b'[' | b']' | b'{' | b'}' => true,
        b'?' => is_blank_or_zero(b, i + 1),
        b':' => matches!(at(b, i + 1), b' ' | b',' | b']' | b'}'),
        _ => false,
    }
}

fn width(c: u8) -> usize {
    if c & 0x80 == 0 {
        1
    } else if c & 0xE0 == 0xC0 {
        2
    } else if c & 0xF0 == 0xE0 {
        3
    } else if c & 0xF8 == 0xF0 {
        4
    } else {
        0
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn scanner_error(
    context: &str,
    context_mark: Mark,
    message: impl Into<String>,
    mark: Mark,
) -> LoadError {
    LoadError::with_context(Stage::Scanner, context, context_mark, message, mark)
}

fn parser_error(
    context: &str,
    context_mark: Mark,
    message: impl Into<String>,
    mark: Mark,
) -> LoadError {
    LoadError::with_context(Stage::Parser, context, context_mark, message, mark)
}

/// go-yaml's parser over a whole input, read as go-yaml reads a bytes.Reader.
pub struct Parser<'a> {
    input: &'a [u8],
    // Reader: the raw chunk is input[raw_start..raw_end], decoded up to raw_pos.
    raw_start: usize,
    raw_pos: usize,
    raw_end: usize,
    eof: bool,
    encoding: Encoding,
    /// The input decoded to UTF-8, zeros after its end.
    buffer: Vec<u8>,
    /// Where go-yaml's buffer starts after its last compaction (its BOM check reads there).
    base: usize,
    pos: usize,
    /// Characters decoded and not read yet (go-yaml lets it go below zero).
    unread: i64,
    mark: Mark,
    newlines: usize,
    // Scanner
    stream_start_produced: bool,
    stream_end_produced: bool,
    flow_level: usize,
    tokens: Vec<Token>,
    tokens_head: usize,
    tokens_parsed: usize,
    token_available: bool,
    indent: i64,
    indents: Vec<i64>,
    simple_key_allowed: bool,
    simple_key_possible: bool,
    simple_key: SimpleKey,
    simple_key_stack: Vec<SimpleKey>,
    comments: Vec<Comment>,
    /// The comments the parser took already.
    comments_head: usize,
    // Parser
    state: State,
    states: Vec<State>,
    marks: Vec<Mark>,
    tag_directives: Vec<(Vec<u8>, Vec<u8>)>,
    error: Option<LoadError>,
}

impl<'a> Parser<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Parser {
            input,
            raw_start: 0,
            raw_pos: 0,
            raw_end: 0,
            eof: false,
            encoding: Encoding::Any,
            buffer: Vec::with_capacity(input.len() + 8),
            base: 0,
            pos: 0,
            unread: 0,
            mark: Mark {
                index: 0,
                line: 1,
                column: 1,
            },
            newlines: 0,
            stream_start_produced: false,
            stream_end_produced: false,
            flow_level: 0,
            tokens: Vec::new(),
            tokens_head: 0,
            tokens_parsed: 0,
            token_available: false,
            indent: 0,
            indents: Vec::new(),
            simple_key_allowed: false,
            simple_key_possible: false,
            simple_key: SimpleKey::default(),
            simple_key_stack: Vec::new(),
            comments: Vec::new(),
            comments_head: 0,
            state: State::StreamStart,
            states: Vec::new(),
            marks: Vec::new(),
            tag_directives: Vec::new(),
            error: None,
        }
    }

    // --- Reader -------------------------------------------------------------------

    fn reader_error(message: impl Into<String>, index: usize) -> LoadError {
        LoadError::new(
            Stage::Reader,
            message,
            Mark {
                index,
                line: 0,
                column: 0,
            },
        )
    }

    /// bytes.Reader.Read into what is left of the raw chunk.
    fn update_raw_buffer(&mut self) {
        if self.raw_pos == self.raw_start && self.raw_end - self.raw_start == RAW_BUFFER_SIZE {
            return;
        }
        if self.eof {
            return;
        }
        self.raw_start = self.raw_pos;
        if self.raw_end >= self.input.len() {
            self.eof = true;
            return;
        }
        let room = RAW_BUFFER_SIZE - (self.raw_end - self.raw_start);
        self.raw_end += room.min(self.input.len() - self.raw_end);
    }

    fn determine_encoding(&mut self) {
        while !self.eof && self.raw_end - self.raw_pos < 3 {
            self.update_raw_buffer();
        }
        let raw = &self.input[self.raw_pos..self.raw_end];
        if raw.starts_with(b"\xff\xfe") {
            self.encoding = Encoding::Utf16Le;
            self.raw_pos += 2;
        } else if raw.starts_with(b"\xfe\xff") {
            self.encoding = Encoding::Utf16Be;
            self.raw_pos += 2;
        } else if raw.starts_with(b"\xef\xbb\xbf") {
            self.encoding = Encoding::Utf8;
            self.raw_pos += 3;
        } else {
            self.encoding = Encoding::Utf8;
        }
    }

    /// The next character of the raw chunk: its value and width, None when it is cut
    /// at the chunk's end and more is to come.
    fn decode(&self) -> Result<Option<(u32, usize)>> {
        let raw_unread = self.raw_end - self.raw_pos;
        let input = self.input;
        let p = self.raw_pos;
        match self.encoding {
            Encoding::Utf16Le | Encoding::Utf16Be => {
                let (low, high) = if self.encoding == Encoding::Utf16Le {
                    (0, 1)
                } else {
                    (1, 0)
                };
                if raw_unread < 2 {
                    if self.eof {
                        return Err(Self::reader_error("incomplete UTF-16 character", p));
                    }
                    return Ok(None);
                }
                let value = input[p + low] as u32 + ((input[p + high] as u32) << 8);
                if value & 0xFC00 == 0xDC00 {
                    return Err(Self::reader_error(
                        format!("unexpected low surrogate area (value: {value})"),
                        p,
                    ));
                }
                if value & 0xFC00 == 0xD800 {
                    if raw_unread < 4 {
                        if self.eof {
                            return Err(Self::reader_error("incomplete UTF-16 surrogate pair", p));
                        }
                        return Ok(None);
                    }
                    let value2 = input[p + low + 2] as u32 + ((input[p + high + 2] as u32) << 8);
                    if value2 & 0xFC00 != 0xDC00 {
                        return Err(Self::reader_error(
                            format!("expected low surrogate area (value: {value2})"),
                            p + 2,
                        ));
                    }
                    return Ok(Some((
                        0x10000 + ((value & 0x3FF) << 10) + (value2 & 0x3FF),
                        4,
                    )));
                }
                Ok(Some((value, 2)))
            }
            _ => {
                let octet = input[p];
                let width = width(octet);
                if width == 0 {
                    return Err(Self::reader_error(
                        format!("invalid leading UTF-8 octet (value: {octet})"),
                        p,
                    ));
                }
                if width > raw_unread {
                    if self.eof {
                        return Err(Self::reader_error("incomplete UTF-8 octet sequence", p));
                    }
                    return Ok(None);
                }
                let mut value = match width {
                    1 => octet & 0x7F,
                    2 => octet & 0x1F,
                    3 => octet & 0x0F,
                    _ => octet & 0x07,
                } as u32;
                for k in 1..width {
                    let o = input[p + k];
                    if o & 0xC0 != 0x80 {
                        return Err(Self::reader_error(
                            format!("invalid trailing UTF-8 octet (value: {o})"),
                            p + k,
                        ));
                    }
                    value = (value << 6) + (o & 0x3F) as u32;
                }
                let shortest = match width {
                    1 => true,
                    2 => value >= 0x80,
                    3 => value >= 0x800,
                    _ => value >= 0x10000,
                };
                if !shortest {
                    return Err(Self::reader_error("invalid length of a UTF-8 sequence", p));
                }
                if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
                    return Err(Self::reader_error(
                        format!("invalid Unicode character (value: {value})"),
                        p,
                    ));
                }
                Ok(Some((value, width)))
            }
        }
    }

    /// go-yaml's updateBuffer: at least `length` characters decoded and unread, the
    /// input read and checked a chunk at a time.
    fn update_buffer(&mut self, length: usize) -> Result<()> {
        if self.unread >= length as i64 {
            return Ok(());
        }
        if self.encoding == Encoding::Any {
            self.determine_encoding();
        }
        self.base = self.pos;
        let mut first = true;
        while self.unread < length as i64 {
            if !first || self.raw_pos == self.raw_end {
                self.update_raw_buffer();
            }
            first = false;
            while self.raw_pos != self.raw_end {
                let Some((value, width)) = self.decode()? else {
                    break;
                };
                let allowed = matches!(value, 0x09 | 0x0A | 0x0D)
                    || (0x20..=0x7E).contains(&value)
                    || (0x7F..=0x9F).contains(&value)
                    || (0xA0..=0xD7FF).contains(&value)
                    || (0xE000..=0xFFFD).contains(&value)
                    || (0x10000..=0x10FFFF).contains(&value);
                if !allowed {
                    return Err(Self::reader_error(
                        format!("control characters are not allowed (value: {value})"),
                        self.raw_pos,
                    ));
                }
                self.raw_pos += width;
                let c = char::from_u32(value).unwrap_or('\u{FFFD}');
                let mut utf8 = [0u8; 4];
                self.buffer
                    .extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
                self.unread += 1;
            }
            if self.eof {
                self.buffer.push(0);
                self.unread += 1;
                break;
            }
        }
        Ok(())
    }

    fn ensure(&mut self, length: usize) -> Result<()> {
        if self.unread < length as i64 {
            self.update_buffer(length)?;
        }
        Ok(())
    }

    fn b(&self) -> &[u8] {
        &self.buffer[self.pos.min(self.buffer.len())..]
    }

    fn ch(&self, i: usize) -> u8 {
        at(&self.buffer, self.pos + i)
    }

    fn is_bom(&self) -> bool {
        self.buffer.get(self.base..self.base + 3) == Some(b"\xef\xbb\xbf")
    }

    fn skip(&mut self) {
        if !is_blank(self.b(), 0) {
            self.newlines = 0;
        }
        self.mark.index += 1;
        self.mark.column += 1;
        self.unread -= 1;
        self.pos += width(self.ch(0)).max(1);
    }

    fn skip_line(&mut self) {
        let b = self.b();
        if at(b, 0) == b'\r' && at(b, 1) == b'\n' {
            self.mark.index += 2;
            self.mark.column = 1;
            self.mark.line += 1;
            self.unread -= 2;
            self.pos += 2;
            self.newlines += 1;
        } else if is_line_break(b, 0) {
            self.mark.index += 1;
            self.mark.column = 1;
            self.mark.line += 1;
            self.unread -= 1;
            self.pos += width(self.ch(0)).max(1);
            self.newlines += 1;
        }
    }

    fn read(&mut self, s: &mut Vec<u8>) {
        if !is_blank(self.b(), 0) {
            self.newlines = 0;
        }
        let w = width(self.ch(0)).max(1);
        for k in 0..w {
            s.push(self.ch(k));
        }
        self.pos += w;
        self.mark.index += 1;
        self.mark.column += 1;
        self.unread -= 1;
    }

    fn read_line(&mut self, s: &mut Vec<u8>) {
        let (c, c1, c2) = (self.ch(0), self.ch(1), self.ch(2));
        if c == b'\r' && c1 == b'\n' {
            s.push(b'\n');
            self.pos += 2;
            self.mark.index += 1;
            self.unread -= 1;
        } else if c == b'\r' || c == b'\n' {
            s.push(b'\n');
            self.pos += 1;
        } else if c == 0xC2 && c1 == 0x85 {
            s.push(b'\n');
            self.pos += 2;
        } else if c == 0xE2 && c1 == 0x80 && (c2 == 0xA8 || c2 == 0xA9) {
            s.extend_from_slice(&[c, c1, c2]);
            self.pos += 3;
        } else {
            return;
        }
        self.mark.index += 1;
        self.mark.column = 1;
        self.mark.line += 1;
        self.unread -= 1;
        self.newlines += 1;
    }

    // --- Scanner ------------------------------------------------------------------

    fn insert_token(&mut self, pos: i64, token: Token) {
        if pos < 0 {
            self.tokens.push(token);
        } else {
            self.tokens.insert(self.tokens_head + pos as usize, token);
        }
    }

    fn fetch_more_tokens(&mut self) -> Result<()> {
        loop {
            if self.tokens_head + 2 < self.tokens.len() {
                let first_key = if let Some(key) = self.simple_key_stack.first() {
                    Some(key.token_number)
                } else if self.simple_key_possible {
                    Some(self.simple_key.token_number)
                } else {
                    None
                };
                match first_key {
                    None => break,
                    Some(key) if self.tokens_parsed != key => break,
                    _ => {}
                }
            }
            self.fetch_next_token()?;
        }
        self.token_available = true;
        Ok(())
    }

    fn fetch_next_token(&mut self) -> Result<()> {
        self.ensure(1)?;
        if !self.stream_start_produced {
            self.fetch_stream_start();
            return Ok(());
        }
        let scan_mark = self.mark;
        self.scan_to_next_token()?;
        self.unroll_indent(self.mark.column as i64, scan_mark);
        self.ensure(4)?;
        let (c, c1, c2) = (self.ch(0), self.ch(1), self.ch(2));
        if c == 0 {
            return self.fetch_stream_end();
        }
        if self.mark.column == 1 && c == b'%' {
            return self.fetch_directive();
        }
        if self.mark.column == 1
            && c == b'-'
            && c1 == b'-'
            && c2 == b'-'
            && is_blank_or_zero(self.b(), 3)
        {
            return self.fetch_document_indicator(TokenKind::DocumentStart);
        }
        if self.mark.column == 1
            && c == b'.'
            && c1 == b'.'
            && c2 == b'.'
            && is_blank_or_zero(self.b(), 3)
        {
            return self.fetch_document_indicator(TokenKind::DocumentEnd);
        }
        let comment_mark = match self.tokens.last() {
            Some(last)
                if (self.flow_level == 0 && c == b':') || (self.flow_level > 0 && c == b',') =>
            {
                last.start
            }
            _ => self.mark,
        };
        self.fetch_token(c)?;
        // A comment on the line of the token is taken with it.
        if self.tokens.last().map(|t| t.kind) != Some(TokenKind::BlockEntry) {
            self.scan_line_comment(comment_mark)?;
        }
        Ok(())
    }

    fn fetch_token(&mut self, c: u8) -> Result<()> {
        let blank_next = is_blank_or_zero(self.b(), 1);
        match c {
            b'[' => return self.fetch_flow_collection_start(TokenKind::FlowSequenceStart),
            b'{' => return self.fetch_flow_collection_start(TokenKind::FlowMappingStart),
            b']' => return self.fetch_flow_collection_end(TokenKind::FlowSequenceEnd),
            b'}' => return self.fetch_flow_collection_end(TokenKind::FlowMappingEnd),
            b',' => return self.fetch_flow_entry(),
            _ => {}
        }
        if c == b'-' && blank_next {
            return self.fetch_block_entry();
        }
        if c == b'?' && blank_next {
            return self.fetch_key();
        }
        if c == b':' && ((self.flow_level > 0 && !self.is_flow_sequence()) || blank_next) {
            return self.fetch_value();
        }
        match c {
            b'*' => return self.fetch_anchor(TokenKind::Alias),
            b'&' => return self.fetch_anchor(TokenKind::Anchor),
            b'!' => return self.fetch_tag(),
            b'|' if self.flow_level == 0 => return self.fetch_block_scalar(true),
            b'>' if self.flow_level == 0 => return self.fetch_block_scalar(false),
            b'\'' => return self.fetch_flow_scalar(true),
            b'"' => return self.fetch_flow_scalar(false),
            _ => {}
        }
        let indicator = is_blank_or_zero(self.b(), 0)
            || matches!(
                c,
                b'-' | b'?'
                    | b':'
                    | b','
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
                    | b'#'
                    | b'&'
                    | b'*'
                    | b'!'
                    | b'|'
                    | b'>'
                    | b'\''
                    | b'"'
                    | b'%'
                    | b'@'
                    | b'`'
            );
        if !indicator
            || (c == b'-' && !is_blank(self.b(), 1))
            || ((c == b'?' || c == b':') && !is_blank_or_zero(self.b(), 1))
        {
            return self.fetch_plain_scalar();
        }
        Err(scanner_error(
            "while scanning for the next token",
            self.mark,
            "found character that cannot start any token",
            self.mark,
        ))
    }

    fn is_flow_sequence(&self) -> bool {
        matches!(
            self.tokens.last().map(|t| t.kind),
            Some(TokenKind::FlowEntry) | Some(TokenKind::FlowSequenceStart)
        )
    }

    fn save_simple_key(&mut self) -> Result<()> {
        let required = self.flow_level == 0 && self.indent == self.mark.column as i64;
        if self.simple_key_allowed {
            self.remove_simple_key()?;
            self.simple_key_possible = true;
            self.simple_key = SimpleKey {
                required,
                flow_level: self.flow_level,
                token_number: self.tokens_parsed + (self.tokens.len() - self.tokens_head),
                mark: self.mark,
            };
        }
        Ok(())
    }

    fn remove_simple_key(&mut self) -> Result<()> {
        if self.simple_key.required {
            return Err(scanner_error(
                "while scanning a simple key",
                self.simple_key.mark,
                "could not find expected ':'",
                self.mark,
            ));
        }
        self.simple_key_possible = false;
        Ok(())
    }

    fn increase_flow_level(&mut self) -> Result<()> {
        self.flow_level += 1;
        if self.flow_level > MAX_DEPTH {
            return Err(scanner_error(
                "while increasing flow level",
                self.simple_key.mark,
                format!("exceeded max depth of {MAX_DEPTH}"),
                self.mark,
            ));
        }
        if self.simple_key_possible {
            self.simple_key_stack.push(self.simple_key);
        }
        self.simple_key = SimpleKey::default();
        Ok(())
    }

    fn decrease_flow_level(&mut self) {
        if self.flow_level > 0 {
            self.flow_level -= 1;
            if let Some(last) = self.simple_key_stack.last() {
                if last.flow_level == self.flow_level {
                    self.simple_key = *last;
                    self.simple_key_stack.pop();
                    self.simple_key_possible = true;
                }
            }
        }
    }

    fn roll_indent(&mut self, column: i64, number: i64, kind: TokenKind, mark: Mark) -> Result<()> {
        if self.flow_level > 0 {
            return Ok(());
        }
        if self.indent < column {
            self.indents.push(self.indent);
            self.indent = column;
            if self.indents.len() > MAX_DEPTH {
                return Err(scanner_error(
                    "while increasing indent level",
                    self.simple_key.mark,
                    format!("exceeded max depth of {MAX_DEPTH}"),
                    self.mark,
                ));
            }
            let number = if number > -1 {
                number - self.tokens_parsed as i64
            } else {
                -1
            };
            self.insert_token(number, Token::new(kind, mark, mark));
        }
        Ok(())
    }

    fn unroll_indent(&mut self, column: i64, scan_mark: Mark) {
        if self.flow_level > 0 {
            return;
        }
        let mut mark = scan_mark;
        mark.index = mark.index.wrapping_sub(1);
        while self.indent > column {
            // A comment at the column of the block ends it where the comment starts.
            let mut stop = mark.index;
            for comment in self.comments.iter().rev() {
                if comment.end_mark.index < stop {
                    break;
                }
                if comment.start_mark.column as i64 == self.indent + 1 {
                    mark = comment.start_mark;
                }
                stop = comment.scan_mark.index;
            }
            self.insert_token(-1, Token::new(TokenKind::BlockEnd, mark, mark));
            self.indent = self.indents.pop().unwrap_or(-1);
        }
    }

    fn fetch_stream_start(&mut self) {
        self.indent = -1;
        self.simple_key = SimpleKey::default();
        self.simple_key_stack.clear();
        self.simple_key_allowed = true;
        self.stream_start_produced = true;
        let mark = self.mark;
        self.insert_token(-1, Token::new(TokenKind::StreamStart, mark, mark));
    }

    fn fetch_stream_end(&mut self) -> Result<()> {
        if self.mark.column != 1 {
            self.mark.column = 1;
            self.mark.line += 1;
        }
        self.unroll_indent(-1, self.mark);
        self.remove_simple_key()?;
        self.simple_key = SimpleKey::default();
        self.simple_key_stack.clear();
        self.simple_key_allowed = false;
        let mark = self.mark;
        self.insert_token(-1, Token::new(TokenKind::StreamEnd, mark, mark));
        Ok(())
    }

    fn fetch_directive(&mut self) -> Result<()> {
        self.unroll_indent(-1, self.mark);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_directive()?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_document_indicator(&mut self, kind: TokenKind) -> Result<()> {
        self.unroll_indent(-1, self.mark);
        self.remove_simple_key()?;
        self.simple_key_allowed = false;
        let start = self.mark;
        self.skip();
        self.skip();
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(kind, start, end));
        Ok(())
    }

    fn fetch_flow_collection_start(&mut self, kind: TokenKind) -> Result<()> {
        self.save_simple_key()?;
        self.increase_flow_level()?;
        self.simple_key_allowed = true;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(kind, start, end));
        Ok(())
    }

    fn fetch_flow_collection_end(&mut self, kind: TokenKind) -> Result<()> {
        self.remove_simple_key()?;
        self.decrease_flow_level();
        self.simple_key_allowed = false;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(kind, start, end));
        Ok(())
    }

    fn fetch_flow_entry(&mut self) -> Result<()> {
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(TokenKind::FlowEntry, start, end));
        Ok(())
    }

    fn fetch_block_entry(&mut self) -> Result<()> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(LoadError::new(
                    Stage::Scanner,
                    "block sequence entries are not allowed in this context",
                    self.mark,
                ));
            }
            self.roll_indent(
                self.mark.column as i64,
                -1,
                TokenKind::BlockSequenceStart,
                self.mark,
            )?;
        }
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(TokenKind::BlockEntry, start, end));
        Ok(())
    }

    fn fetch_key(&mut self) -> Result<()> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(LoadError::new(
                    Stage::Scanner,
                    "mapping keys are not allowed in this context",
                    self.mark,
                ));
            }
            self.roll_indent(
                self.mark.column as i64,
                -1,
                TokenKind::BlockMappingStart,
                self.mark,
            )?;
        }
        self.remove_simple_key()?;
        self.simple_key_allowed = self.flow_level == 0;
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(TokenKind::Key, start, end));
        Ok(())
    }

    fn fetch_value(&mut self) -> Result<()> {
        if self.simple_key_possible && self.simple_key.mark.line == self.mark.line {
            let key = self.simple_key;
            self.insert_token(
                key.token_number as i64 - self.tokens_parsed as i64,
                Token::new(TokenKind::Key, key.mark, key.mark),
            );
            self.roll_indent(
                key.mark.column as i64,
                key.token_number as i64,
                TokenKind::BlockMappingStart,
                key.mark,
            )?;
            self.simple_key_possible = false;
            self.simple_key.required = false;
            self.simple_key_allowed = false;
        } else {
            if self.flow_level == 0 {
                if !self.simple_key_allowed {
                    return Err(LoadError::new(
                        Stage::Scanner,
                        "mapping values are not allowed in this context",
                        self.mark,
                    ));
                }
                self.roll_indent(
                    self.mark.column as i64,
                    -1,
                    TokenKind::BlockMappingStart,
                    self.mark,
                )?;
            }
            self.simple_key_allowed = self.flow_level == 0;
        }
        let start = self.mark;
        self.skip();
        let end = self.mark;
        self.insert_token(-1, Token::new(TokenKind::Value, start, end));
        Ok(())
    }

    fn fetch_anchor(&mut self, kind: TokenKind) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_anchor(kind)?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_tag(&mut self) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_tag()?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_block_scalar(&mut self, literal: bool) -> Result<()> {
        self.remove_simple_key()?;
        self.simple_key_allowed = true;
        let token = self.scan_block_scalar(literal)?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, single: bool) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_flow_scalar(single)?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn fetch_plain_scalar(&mut self) -> Result<()> {
        self.save_simple_key()?;
        self.simple_key_allowed = false;
        let token = self.scan_plain_scalar()?;
        self.insert_token(-1, token);
        Ok(())
    }

    fn scan_to_next_token(&mut self) -> Result<()> {
        let scan_mark = self.mark;
        loop {
            self.ensure(1)?;
            if self.mark.column == 1 && self.is_bom() {
                self.skip();
            }
            self.ensure(1)?;
            while self.ch(0) == b' '
                || ((self.flow_level > 0 || !self.simple_key_allowed) && self.ch(0) == b'\t')
            {
                self.skip();
                self.ensure(1)?;
            }
            // A line comment after `- ` heads what follows on the next line instead.
            let n = self.tokens.len();
            if !self.comments.is_empty() && n > 1 {
                let (a, b) = (self.tokens[n - 2].kind, self.tokens[n - 1].kind);
                let at_break = is_line_break(self.b(), 0);
                let line = self.mark.line;
                let mark = self.mark;
                let comment = self.comments.last_mut().expect("a comment");
                if a == TokenKind::BlockSequenceStart
                    && b == TokenKind::BlockEntry
                    && !comment.line.is_empty()
                    && !at_break
                {
                    comment.head = std::mem::take(&mut comment.line);
                    if comment.start_mark.line + 1 == line {
                        comment.token_mark = mark;
                    }
                }
            }
            if self.ch(0) == b'#' {
                self.scan_comments(scan_mark)?;
            }
            if is_line_break(self.b(), 0) {
                self.ensure(2)?;
                self.skip_line();
                if self.flow_level == 0 {
                    self.simple_key_allowed = true;
                }
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Reads the comment `#` at `peek` starts, up to the line break that ends it, into
    /// `text`; the mark where its text starts, if it had none yet.
    fn take_comment(&mut self, peek: usize, text: &mut Vec<u8>) -> Result<Option<Mark>> {
        let seen = self.mark.index + peek;
        let mut start = None;
        loop {
            self.ensure(1)?;
            if is_break_or_zero(self.b(), 0) {
                if self.mark.index >= seen {
                    break;
                }
                self.ensure(2)?;
                self.skip_line();
            } else if self.mark.index >= seen {
                if text.is_empty() {
                    start = Some(self.mark);
                }
                self.read(text);
            } else {
                self.skip();
            }
        }
        Ok(start)
    }

    /// go-yaml's scanLineComment: a comment after a token, on its line.
    fn scan_line_comment(&mut self, token_mark: Mark) -> Result<()> {
        if self.newlines > 0 {
            return Ok(());
        }
        let mut text = Vec::new();
        let mut start_mark = Mark::default();
        let mut peek = 0;
        while peek < 512 {
            if self.unread < (peek + 1) as i64 && self.update_buffer(peek + 1).is_err() {
                break;
            }
            if is_blank(self.b(), peek) {
                peek += 1;
                continue;
            }
            if self.ch(peek) == b'#' {
                if let Some(start) = self.take_comment(peek, &mut text)? {
                    start_mark = start;
                }
            }
            break;
        }
        if !text.is_empty() {
            self.comments.push(Comment {
                scan_mark: token_mark,
                token_mark,
                start_mark,
                end_mark: self.mark,
                line: text,
                ..Default::default()
            });
        }
        Ok(())
    }

    /// go-yaml's scanComments: the comments from here on, through the blank lines
    /// between them, each kept as the foot of what came before or the head of what
    /// comes next.
    fn scan_comments(&mut self, scan_mark: Mark) -> Result<()> {
        let n = self.tokens.len();
        let mut token = &self.tokens[n - 1];
        if token.kind == TokenKind::FlowEntry && n > 1 {
            token = &self.tokens[n - 2];
        }
        let token_kind = token.kind;
        let mut token_mark = token.start;
        let mut scan_mark = scan_mark;
        let mut start_mark = Mark::default();
        let mut next_indent = self.indent.max(0);
        let mut recent_empty = false;
        let mut first_empty = self.newlines <= 1;
        let mut line = self.mark.line as i64;
        let mut column = self.mark.column as i64;
        let mut text: Vec<u8> = Vec::new();
        let mut foot_line: i64 = -1;
        if scan_mark.line > 0 {
            foot_line = self.mark.line as i64 - self.newlines as i64 + 1;
            if self.newlines == 0 && self.mark.column > 1 {
                foot_line += 1;
            }
        }
        let here = |s: &Self, peek: usize, line: i64, column: i64| Mark {
            index: s.mark.index + peek,
            line: line as usize,
            column: column as usize,
        };
        let mut peek = 0usize;
        while peek < 512 {
            if self.unread < (peek + 1) as i64 && self.update_buffer(peek + 1).is_err() {
                break;
            }
            column += 1;
            if is_blank(self.b(), peek) {
                peek += 1;
                continue;
            }
            let c = self.ch(peek);
            let close_flow = self.flow_level > 0 && (c == b']' || c == b'}');
            if close_flow || is_break_or_zero(self.b(), peek) {
                if close_flow || !recent_empty {
                    let start_column = start_mark.column as i64;
                    if close_flow
                        || (first_empty
                            && ((start_mark.line as i64 == foot_line
                                && token_kind != TokenKind::Value)
                                || start_column - 1 < next_indent))
                    {
                        if !text.is_empty() {
                            if start_column - 1 < next_indent {
                                token_mark = start_mark;
                            }
                            let end = here(self, peek, line, column);
                            self.comments.push(Comment {
                                scan_mark,
                                token_mark,
                                start_mark,
                                end_mark: end,
                                ..Default::default()
                            });
                            text.clear();
                            scan_mark = end;
                            token_mark = end;
                        }
                    } else if !text.is_empty() && self.ch(peek) != 0 {
                        text.push(b'\n');
                    }
                }
                if !is_line_break(self.b(), peek) {
                    break;
                }
                first_empty = false;
                recent_empty = true;
                column = 1;
                line += 1;
                peek += 1;
                continue;
            }
            if !text.is_empty()
                && (close_flow || (column - 1 < next_indent && column != start_mark.column as i64))
            {
                let end = here(self, peek, line, column);
                self.comments.push(Comment {
                    scan_mark,
                    token_mark,
                    start_mark,
                    end_mark: end,
                    ..Default::default()
                });
                text.clear();
                scan_mark = end;
                token_mark = end;
            }
            if c != b'#' {
                break;
            }
            if text.is_empty() {
                start_mark = here(self, peek, line, column);
            } else {
                text.push(b'\n');
            }
            recent_empty = false;
            self.take_comment(peek, &mut text)?;
            peek = 0;
            column = 1;
            line = self.mark.line as i64;
            next_indent = self.indent.max(0);
            peek += 1;
        }
        if !text.is_empty() {
            self.comments.push(Comment {
                scan_mark,
                token_mark: start_mark,
                start_mark,
                end_mark: Mark {
                    index: (self.mark.index + peek).wrapping_sub(1),
                    line: line as usize,
                    column: column as usize,
                },
                head: text,
                ..Default::default()
            });
        }
        Ok(())
    }

    fn scan_directive(&mut self) -> Result<Token> {
        let start = self.mark;
        self.skip();
        let name = self.scan_directive_name(start)?;
        let token = if name == b"YAML" {
            let (major, minor) = self.scan_version_directive_value(start)?;
            let mut token = Token::new(TokenKind::VersionDirective, start, self.mark);
            token.major = major;
            token.minor = minor;
            token
        } else if name == b"TAG" {
            let (handle, prefix) = self.scan_tag_directive_value(start)?;
            let mut token = Token::new(TokenKind::TagDirective, start, self.mark);
            token.value = handle;
            token.prefix = prefix;
            token
        } else {
            return Err(scanner_error(
                "while scanning a directive",
                start,
                "found unknown directive name",
                self.mark,
            ));
        };
        self.ensure(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.ensure(1)?;
        }
        if self.ch(0) == b'#' {
            while !is_break_or_zero(self.b(), 0) {
                self.skip();
                self.ensure(1)?;
            }
        }
        if !is_break_or_zero(self.b(), 0) {
            return Err(scanner_error(
                "while scanning a directive",
                start,
                "did not find expected comment or line break",
                self.mark,
            ));
        }
        if is_line_break(self.b(), 0) {
            self.ensure(2)?;
            self.skip_line();
        }
        Ok(token)
    }

    fn scan_directive_name(&mut self, start: Mark) -> Result<Vec<u8>> {
        self.ensure(1)?;
        let mut s = Vec::new();
        while is_alpha(self.b(), 0) {
            self.read(&mut s);
            self.ensure(1)?;
        }
        if s.is_empty() {
            return Err(scanner_error(
                "while scanning a directive",
                start,
                "could not find expected directive name",
                self.mark,
            ));
        }
        if !is_blank_or_zero(self.b(), 0) {
            return Err(scanner_error(
                "while scanning a directive",
                start,
                "found unexpected non-alphabetical character",
                self.mark,
            ));
        }
        Ok(s)
    }

    fn scan_version_directive_value(&mut self, start: Mark) -> Result<(i8, i8)> {
        self.ensure(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.ensure(1)?;
        }
        let major = self.scan_version_directive_number(start)?;
        if self.ch(0) != b'.' {
            return Err(scanner_error(
                "while scanning a %YAML directive",
                start,
                "did not find expected digit or '.' character",
                self.mark,
            ));
        }
        self.skip();
        let minor = self.scan_version_directive_number(start)?;
        Ok((major, minor))
    }

    fn scan_version_directive_number(&mut self, start: Mark) -> Result<i8> {
        self.ensure(1)?;
        let (mut value, mut length) = (0i8, 0i8);
        while self.ch(0).is_ascii_digit() {
            length += 1;
            if length > MAX_NUMBER_LENGTH {
                return Err(scanner_error(
                    "while scanning a %YAML directive",
                    start,
                    "found extremely long version number",
                    self.mark,
                ));
            }
            value = value
                .wrapping_mul(10)
                .wrapping_add((self.ch(0) - b'0') as i8);
            self.skip();
            self.ensure(1)?;
        }
        if length == 0 {
            return Err(scanner_error(
                "while scanning a %YAML directive",
                start,
                "did not find expected version number",
                self.mark,
            ));
        }
        Ok(value)
    }

    fn scan_tag_directive_value(&mut self, start: Mark) -> Result<(Vec<u8>, Vec<u8>)> {
        self.ensure(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.ensure(1)?;
        }
        let handle = self.scan_tag_handle(true, start)?;
        self.ensure(1)?;
        if !is_blank(self.b(), 0) {
            return Err(scanner_error(
                "while scanning a %TAG directive",
                start,
                "did not find expected whitespace",
                self.mark,
            ));
        }
        while is_blank(self.b(), 0) {
            self.skip();
            self.ensure(1)?;
        }
        let prefix = self.scan_tag_uri(true, true, &[], start)?;
        self.ensure(1)?;
        if !is_blank_or_zero(self.b(), 0) {
            return Err(scanner_error(
                "while scanning a %TAG directive",
                start,
                "did not find expected whitespace or line break",
                self.mark,
            ));
        }
        Ok((handle, prefix))
    }

    fn scan_anchor(&mut self, kind: TokenKind) -> Result<Token> {
        let start = self.mark;
        self.skip();
        self.ensure(1)?;
        let mut s = Vec::new();
        while is_anchor_char(self.b(), 0, self.is_bom()) {
            self.read(&mut s);
            self.ensure(1)?;
        }
        let end = self.mark;
        let c = self.ch(0);
        if s.is_empty()
            || !(is_blank_or_zero(self.b(), 0)
                || matches!(c, b'?' | b':' | b',' | b']' | b'}' | b'%' | b'@' | b'`'))
        {
            let context = if kind == TokenKind::Anchor {
                "while scanning an anchor"
            } else {
                "while scanning an alias"
            };
            return Err(scanner_error(
                context,
                start,
                "did not find expected alphabetic or numeric character",
                self.mark,
            ));
        }
        let mut token = Token::new(kind, start, end);
        token.value = s;
        Ok(token)
    }

    fn tag_error(&self, directive: bool, start: Mark, message: impl Into<String>) -> LoadError {
        let context = if directive {
            "while parsing a %TAG directive"
        } else {
            "while parsing a tag"
        };
        scanner_error(context, start, message, self.mark)
    }

    fn scan_tag(&mut self) -> Result<Token> {
        let start = self.mark;
        self.ensure(2)?;
        let (handle, suffix);
        if self.ch(1) == b'<' {
            self.skip();
            self.skip();
            suffix = self.scan_tag_uri(false, true, &[], start)?;
            if self.ch(0) != b'>' {
                return Err(scanner_error(
                    "while scanning a tag",
                    start,
                    "did not find the expected '>'",
                    self.mark,
                ));
            }
            self.skip();
            handle = Vec::new();
        } else {
            let h = self.scan_tag_handle(false, start)?;
            if h[0] == b'!' && h.len() > 1 && h[h.len() - 1] == b'!' {
                suffix = self.scan_tag_uri(false, false, &[], start)?;
                handle = h;
            } else {
                let s = self.scan_tag_uri(false, false, &h, start)?;
                if s.is_empty() {
                    handle = Vec::new();
                    suffix = b"!".to_vec();
                } else {
                    handle = b"!".to_vec();
                    suffix = s;
                }
            }
        }
        self.ensure(1)?;
        if !is_blank_or_zero(self.b(), 0) {
            return Err(scanner_error(
                "while scanning a tag",
                start,
                "did not find expected whitespace or line break",
                self.mark,
            ));
        }
        let mut token = Token::new(TokenKind::Tag, start, self.mark);
        token.value = handle;
        token.suffix = suffix;
        Ok(token)
    }

    fn scan_tag_handle(&mut self, directive: bool, start: Mark) -> Result<Vec<u8>> {
        self.ensure(1)?;
        if self.ch(0) != b'!' {
            return Err(self.tag_error(directive, start, "did not find expected '!'"));
        }
        let mut s = Vec::new();
        self.read(&mut s);
        self.ensure(1)?;
        while is_alpha(self.b(), 0) {
            self.read(&mut s);
            self.ensure(1)?;
        }
        if self.ch(0) == b'!' {
            self.read(&mut s);
        } else if directive && s != b"!" {
            return Err(self.tag_error(directive, start, "did not find expected '!'"));
        }
        Ok(s)
    }

    fn scan_tag_uri(
        &mut self,
        directive: bool,
        verbatim: bool,
        head: &[u8],
        start: Mark,
    ) -> Result<Vec<u8>> {
        let mut s = Vec::new();
        let mut has_tag = !head.is_empty();
        if head.len() > 1 {
            s.extend_from_slice(&head[1..]);
        }
        self.ensure(1)?;
        while is_tag_uri_char(self.b(), 0, verbatim) {
            if self.ch(0) == b'%' {
                self.scan_uri_escapes(directive, start, &mut s)?;
            } else {
                self.read(&mut s);
            }
            self.ensure(1)?;
            has_tag = true;
        }
        if !verbatim {
            let c = self.ch(0);
            if !is_blank_or_zero(self.b(), 0) && (0x20..=0x7E).contains(&c) {
                return Err(self.tag_error(
                    directive,
                    start,
                    format!(
                        "found character '{}' that is not allowed in a YAML tag",
                        c as char
                    ),
                ));
            }
        }
        if !has_tag {
            return Err(self.tag_error(directive, start, "did not find expected tag URI"));
        }
        Ok(s)
    }

    fn scan_uri_escapes(&mut self, directive: bool, start: Mark, s: &mut Vec<u8>) -> Result<()> {
        let mut w = 1024usize;
        while w > 0 {
            self.ensure(3)?;
            let b = self.b();
            if !(at(b, 0) == b'%' && is_hex(b, 1) && is_hex(b, 2)) {
                return Err(self.tag_error(directive, start, "did not find URI escaped octet"));
            }
            let octet = ((as_hex(b, 1) << 4) + as_hex(b, 2)) as u8;
            if w == 1024 {
                w = width(octet);
                if w == 0 {
                    return Err(self.tag_error(
                        directive,
                        start,
                        "found an incorrect leading UTF-8 octet",
                    ));
                }
            } else if octet & 0xC0 != 0x80 {
                return Err(self.tag_error(
                    directive,
                    start,
                    "found an incorrect trailing UTF-8 octet",
                ));
            }
            s.push(octet);
            self.skip();
            self.skip();
            self.skip();
            w -= 1;
        }
        Ok(())
    }

    fn scan_block_scalar(&mut self, literal: bool) -> Result<Token> {
        let start = self.mark;
        self.skip();
        self.ensure(1)?;
        let (mut chomping, mut increment) = (0i32, 0i64);
        let block_error = |s: &Self, message: &str| {
            scanner_error("while scanning a block scalar", start, message, s.mark)
        };
        if self.ch(0) == b'+' || self.ch(0) == b'-' {
            chomping = if self.ch(0) == b'+' { 1 } else { -1 };
            self.skip();
            self.ensure(1)?;
            if self.ch(0).is_ascii_digit() {
                if self.ch(0) == b'0' {
                    return Err(block_error(
                        self,
                        "found an indentation indicator equal to 0",
                    ));
                }
                increment = (self.ch(0) - b'0') as i64;
                self.skip();
            }
        } else if self.ch(0).is_ascii_digit() {
            if self.ch(0) == b'0' {
                return Err(block_error(
                    self,
                    "found an indentation indicator equal to 0",
                ));
            }
            increment = (self.ch(0) - b'0') as i64;
            self.skip();
            self.ensure(1)?;
            if self.ch(0) == b'+' || self.ch(0) == b'-' {
                chomping = if self.ch(0) == b'+' { 1 } else { -1 };
                self.skip();
            }
        }
        self.ensure(1)?;
        while is_blank(self.b(), 0) {
            self.skip();
            self.ensure(1)?;
        }
        if self.ch(0) == b'#' {
            self.scan_line_comment(start)?;
            while !is_break_or_zero(self.b(), 0) {
                self.skip();
                self.ensure(1)?;
            }
        }
        if !is_break_or_zero(self.b(), 0) {
            return Err(block_error(
                self,
                "did not find expected comment or line break",
            ));
        }
        if is_line_break(self.b(), 0) {
            self.ensure(2)?;
            self.skip_line();
        }
        let mut end = self.mark;
        let mut indent = 0i64;
        if increment > 0 {
            indent = if self.indent >= 0 {
                self.indent + increment
            } else {
                increment + 1
            };
        }
        let (mut s, mut leading_break, mut trailing_breaks) = (Vec::new(), Vec::new(), Vec::new());
        self.scan_block_scalar_breaks(&mut indent, &mut trailing_breaks, start, &mut end)?;
        self.ensure(1)?;
        let mut leading_blank = false;
        while self.mark.column as i64 == indent && self.ch(0) != 0 {
            let trailing_blank = is_blank(self.b(), 0);
            if !literal
                && !leading_blank
                && !trailing_blank
                && leading_break.first() == Some(&b'\n')
            {
                if trailing_breaks.is_empty() {
                    s.push(b' ');
                }
            } else {
                s.extend_from_slice(&leading_break);
            }
            leading_break.clear();
            s.extend_from_slice(&trailing_breaks);
            trailing_breaks.clear();
            leading_blank = is_blank(self.b(), 0);
            while !is_break_or_zero(self.b(), 0) {
                self.read(&mut s);
                self.ensure(1)?;
            }
            self.ensure(2)?;
            self.read_line(&mut leading_break);
            self.scan_block_scalar_breaks(&mut indent, &mut trailing_breaks, start, &mut end)?;
        }
        if chomping != -1 {
            s.extend_from_slice(&leading_break);
        }
        if chomping == 1 {
            s.extend_from_slice(&trailing_breaks);
        }
        let mut token = Token::new(TokenKind::Scalar, start, end);
        token.value = s;
        token.style = if literal {
            ScalarStyle::Literal
        } else {
            ScalarStyle::Folded
        };
        Ok(token)
    }

    fn scan_block_scalar_breaks(
        &mut self,
        indent: &mut i64,
        breaks: &mut Vec<u8>,
        start: Mark,
        end: &mut Mark,
    ) -> Result<()> {
        *end = self.mark;
        let mut max_indent = 0i64;
        loop {
            self.ensure(1)?;
            while (*indent == 0 || (self.mark.column as i64) < *indent) && self.ch(0) == b' ' {
                self.skip();
                self.ensure(1)?;
            }
            if self.mark.column as i64 > max_indent {
                max_indent = self.mark.column as i64;
            }
            if (*indent == 0 || (self.mark.column as i64) < *indent) && self.ch(0) == b'\t' {
                return Err(scanner_error(
                    "while scanning a block scalar",
                    start,
                    "found a tab character where an indentation space is expected",
                    self.mark,
                ));
            }
            if !is_line_break(self.b(), 0) {
                break;
            }
            self.ensure(2)?;
            self.read_line(breaks);
            *end = self.mark;
        }
        if *indent == 0 {
            *indent = max_indent;
            if *indent < self.indent + 1 {
                *indent = self.indent + 1;
            }
            if *indent < 1 {
                *indent = 1;
            }
        }
        Ok(())
    }

    fn document_indicator_here(&self) -> bool {
        let b = self.b();
        self.mark.column == 1
            && ((at(b, 0) == b'-' && at(b, 1) == b'-' && at(b, 2) == b'-')
                || (at(b, 0) == b'.' && at(b, 1) == b'.' && at(b, 2) == b'.'))
            && is_blank_or_zero(b, 3)
    }

    fn scan_flow_scalar(&mut self, single: bool) -> Result<Token> {
        let start = self.mark;
        self.skip();
        let quoted_error = |s: &Self, message: &str| {
            scanner_error("while scanning a quoted scalar", start, message, s.mark)
        };
        let (mut s, mut leading_break, mut trailing_breaks, mut whitespaces) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        loop {
            self.ensure(4)?;
            if self.document_indicator_here() {
                return Err(quoted_error(self, "found unexpected document indicator"));
            }
            if self.ch(0) == 0 {
                return Err(quoted_error(self, "found unexpected end of stream"));
            }
            let mut leading_blanks = false;
            while !is_blank_or_zero(self.b(), 0) {
                let (c, c1) = (self.ch(0), self.ch(1));
                if single && c == b'\'' && c1 == b'\'' {
                    s.push(b'\'');
                    self.skip();
                    self.skip();
                } else if (single && c == b'\'') || (!single && c == b'"') {
                    break;
                } else if !single && c == b'\\' && is_line_break(self.b(), 1) {
                    self.ensure(3)?;
                    self.skip();
                    self.skip_line();
                    leading_blanks = true;
                    break;
                } else if !single && c == b'\\' {
                    let mut code_length = 0;
                    match c1 {
                        b'0' => s.push(0),
                        b'a' => s.push(0x07),
                        b'b' => s.push(0x08),
                        b't' | b'\t' => s.push(0x09),
                        b'n' => s.push(0x0A),
                        b'v' => s.push(0x0B),
                        b'f' => s.push(0x0C),
                        b'r' => s.push(0x0D),
                        b'e' => s.push(0x1B),
                        b' ' => s.push(0x20),
                        b'"' => s.push(b'"'),
                        b'\'' => s.push(b'\''),
                        b'\\' => s.push(b'\\'),
                        b'N' => s.extend_from_slice(&[0xC2, 0x85]),
                        b'_' => s.extend_from_slice(&[0xC2, 0xA0]),
                        b'L' => s.extend_from_slice(&[0xE2, 0x80, 0xA8]),
                        b'P' => s.extend_from_slice(&[0xE2, 0x80, 0xA9]),
                        b'x' => code_length = 2,
                        b'u' => code_length = 4,
                        b'U' => code_length = 8,
                        _ => return Err(quoted_error(self, "found unknown escape character")),
                    }
                    self.skip();
                    self.skip();
                    if code_length > 0 {
                        self.ensure(code_length)?;
                        let mut value: u64 = 0;
                        for k in 0..code_length {
                            if !is_hex(self.b(), k) {
                                return Err(quoted_error(
                                    self,
                                    "did not find expected hexadecimal number",
                                ));
                            }
                            value = (value << 4) + as_hex(self.b(), k) as u64;
                        }
                        if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
                            return Err(quoted_error(
                                self,
                                "found invalid Unicode character escape code",
                            ));
                        }
                        let c = char::from_u32(value as u32).unwrap_or('\u{FFFD}');
                        let mut utf8 = [0u8; 4];
                        s.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
                        for _ in 0..code_length {
                            self.skip();
                        }
                    }
                } else {
                    self.read(&mut s);
                }
                self.ensure(2)?;
            }
            self.ensure(1)?;
            let c = self.ch(0);
            if (single && c == b'\'') || (!single && c == b'"') {
                break;
            }
            while is_blank(self.b(), 0) || is_line_break(self.b(), 0) {
                if is_blank(self.b(), 0) {
                    if !leading_blanks {
                        self.read(&mut whitespaces);
                    } else {
                        self.skip();
                    }
                } else {
                    self.ensure(2)?;
                    if !leading_blanks {
                        whitespaces.clear();
                        self.read_line(&mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.read_line(&mut trailing_breaks);
                    }
                }
                self.ensure(1)?;
            }
            if leading_blanks {
                if leading_break.first() == Some(&b'\n') {
                    if trailing_breaks.is_empty() {
                        s.push(b' ');
                    } else {
                        s.extend_from_slice(&trailing_breaks);
                    }
                } else {
                    s.extend_from_slice(&leading_break);
                    s.extend_from_slice(&trailing_breaks);
                }
                trailing_breaks.clear();
                leading_break.clear();
            } else {
                s.extend_from_slice(&whitespaces);
                whitespaces.clear();
            }
        }
        self.skip();
        let mut token = Token::new(TokenKind::Scalar, start, self.mark);
        token.value = s;
        token.style = if single {
            ScalarStyle::SingleQuoted
        } else {
            ScalarStyle::DoubleQuoted
        };
        Ok(token)
    }

    fn scan_plain_scalar(&mut self) -> Result<Token> {
        let (mut s, mut leading_break, mut trailing_breaks, mut whitespaces) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut leading_blanks = false;
        let indent = self.indent + 1;
        let start = self.mark;
        let mut end = self.mark;
        loop {
            self.ensure(4)?;
            if self.document_indicator_here() {
                break;
            }
            if self.ch(0) == b'#' {
                break;
            }
            while !is_blank_or_zero(self.b(), 0) {
                if (self.ch(0) == b':' && is_blank_or_zero(self.b(), 1))
                    || (self.flow_level > 0 && is_end_of_scalar_in_flow(self.b(), 0))
                {
                    break;
                }
                if leading_blanks || !whitespaces.is_empty() {
                    if leading_blanks {
                        if leading_break.first() == Some(&b'\n') {
                            if trailing_breaks.is_empty() {
                                s.push(b' ');
                            } else {
                                s.extend_from_slice(&trailing_breaks);
                            }
                        } else {
                            s.extend_from_slice(&leading_break);
                            s.extend_from_slice(&trailing_breaks);
                        }
                        trailing_breaks.clear();
                        leading_break.clear();
                        leading_blanks = false;
                    } else {
                        s.extend_from_slice(&whitespaces);
                        whitespaces.clear();
                    }
                }
                self.read(&mut s);
                end = self.mark;
                self.ensure(2)?;
            }
            if !(is_blank(self.b(), 0) || is_line_break(self.b(), 0)) {
                break;
            }
            self.ensure(1)?;
            while is_blank(self.b(), 0) || is_line_break(self.b(), 0) {
                if is_blank(self.b(), 0) {
                    if leading_blanks && (self.mark.column as i64) < indent && self.ch(0) == b'\t' {
                        return Err(scanner_error(
                            "while scanning a plain scalar",
                            start,
                            "found a tab character that violates indentation",
                            self.mark,
                        ));
                    }
                    if !leading_blanks {
                        self.read(&mut whitespaces);
                    } else {
                        self.skip();
                    }
                } else {
                    self.ensure(2)?;
                    if !leading_blanks {
                        whitespaces.clear();
                        self.read_line(&mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.read_line(&mut trailing_breaks);
                    }
                }
                self.ensure(1)?;
            }
            if self.flow_level == 0 && (self.mark.column as i64) < indent {
                break;
            }
        }
        let mut token = Token::new(TokenKind::Scalar, start, end);
        token.value = s;
        if leading_blanks {
            self.simple_key_allowed = true;
        }
        Ok(token)
    }

    // --- Parser -------------------------------------------------------------------

    fn peek_token(&mut self) -> Result<Token> {
        if !self.token_available {
            self.fetch_more_tokens()?;
        }
        self.unfold_comments();
        Ok(self.tokens[self.tokens_head].clone())
    }

    fn peek_kind(&mut self) -> Result<TokenKind> {
        if !self.token_available {
            self.fetch_more_tokens()?;
        }
        self.unfold_comments();
        Ok(self.tokens[self.tokens_head].kind)
    }

    /// go-yaml's UnfoldComments: the comments up to the token are taken, and no longer
    /// count where a block ends; a head comment waits for a token that is not the end
    /// of a block.
    fn unfold_comments(&mut self) {
        let token = &self.tokens[self.tokens_head];
        while self.comments_head < self.comments.len()
            && token.start.index >= self.comments[self.comments_head].token_mark.index
        {
            let comment = &mut self.comments[self.comments_head];
            if !comment.head.is_empty() && token.kind == TokenKind::BlockEnd {
                break;
            }
            *comment = Comment::default();
            self.comments_head += 1;
        }
    }

    fn skip_token(&mut self) {
        self.token_available = false;
        self.tokens_parsed += 1;
        self.stream_end_produced = self.tokens[self.tokens_head].kind == TokenKind::StreamEnd;
        self.tokens_head += 1;
    }

    fn pop_state(&mut self) {
        self.state = self.states.pop().unwrap_or(State::End);
    }

    /// The next event, None once the stream ended; an error stays.
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        if self.stream_end_produced || self.state == State::End {
            return Ok(None);
        }
        match self.state_machine() {
            Ok(event) => Ok(Some(event)),
            Err(e) => {
                self.error = Some(e.clone());
                Err(e)
            }
        }
    }

    fn state_machine(&mut self) -> Result<Event> {
        match self.state {
            State::StreamStart => self.parse_stream_start(),
            State::ImplicitDocumentStart => self.parse_document_start(true),
            State::DocumentStart => self.parse_document_start(false),
            State::DocumentContent => self.parse_document_content(),
            State::DocumentEnd => self.parse_document_end(),
            State::BlockNode => self.parse_node(true, false),
            State::BlockSequenceFirstEntry => self.parse_block_sequence_entry(true),
            State::BlockSequenceEntry => self.parse_block_sequence_entry(false),
            State::IndentlessSequenceEntry => self.parse_indentless_sequence_entry(),
            State::BlockMappingFirstKey => self.parse_block_mapping_key(true),
            State::BlockMappingKey => self.parse_block_mapping_key(false),
            State::BlockMappingValue => self.parse_block_mapping_value(),
            State::FlowSequenceFirstEntry => self.parse_flow_sequence_entry(true),
            State::FlowSequenceEntry => self.parse_flow_sequence_entry(false),
            State::FlowSequenceEntryMappingKey => self.parse_flow_sequence_entry_mapping_key(),
            State::FlowSequenceEntryMappingValue => self.parse_flow_sequence_entry_mapping_value(),
            State::FlowSequenceEntryMappingEnd => self.parse_flow_sequence_entry_mapping_end(),
            State::FlowMappingFirstKey => self.parse_flow_mapping_key(true),
            State::FlowMappingKey => self.parse_flow_mapping_key(false),
            State::FlowMappingValue => self.parse_flow_mapping_value(false),
            State::FlowMappingEmptyValue => self.parse_flow_mapping_value(true),
            State::End => unreachable!("no event after the end of the stream"),
        }
    }

    fn event(kind: EventKind, start: Mark, end: Mark) -> Event {
        Event { kind, start, end }
    }

    fn empty_scalar(mark: Mark) -> Event {
        Self::event(
            EventKind::Scalar {
                anchor: None,
                tag: String::new(),
                value: String::new(),
                style: ScalarStyle::Plain,
                plain_implicit: true,
                quoted_implicit: false,
            },
            mark,
            mark,
        )
    }

    fn parse_stream_start(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        if token.kind != TokenKind::StreamStart {
            return Err(LoadError::new(
                Stage::Parser,
                "did not find expected <stream-start>",
                token.start,
            ));
        }
        self.state = State::ImplicitDocumentStart;
        self.skip_token();
        Ok(Self::event(EventKind::StreamStart, token.start, token.end))
    }

    fn parse_document_start(&mut self, implicit: bool) -> Result<Event> {
        let mut token = self.peek_token()?;
        while token.kind == TokenKind::DocumentEnd {
            self.skip_token();
            token = self.peek_token()?;
        }
        if implicit
            && !matches!(
                token.kind,
                TokenKind::VersionDirective
                    | TokenKind::TagDirective
                    | TokenKind::DocumentStart
                    | TokenKind::StreamEnd
            )
        {
            self.process_directives()?;
            self.states.push(State::DocumentEnd);
            self.state = State::BlockNode;
            return Ok(Self::event(
                EventKind::DocumentStart { implicit: true },
                token.start,
                token.end,
            ));
        }
        if token.kind != TokenKind::StreamEnd {
            let start = token.start;
            self.process_directives()?;
            let token = self.peek_token()?;
            if token.kind != TokenKind::DocumentStart {
                return Err(LoadError::new(
                    Stage::Parser,
                    "did not find expected <document start>",
                    token.start,
                ));
            }
            self.states.push(State::DocumentEnd);
            self.state = State::DocumentContent;
            self.skip_token();
            return Ok(Self::event(
                EventKind::DocumentStart { implicit: false },
                start,
                token.end,
            ));
        }
        self.state = State::End;
        self.skip_token();
        Ok(Self::event(EventKind::StreamEnd, token.start, token.end))
    }

    fn parse_document_content(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        if matches!(
            token.kind,
            TokenKind::VersionDirective
                | TokenKind::TagDirective
                | TokenKind::DocumentStart
                | TokenKind::DocumentEnd
                | TokenKind::StreamEnd
        ) {
            self.pop_state();
            return Ok(Self::empty_scalar(token.start));
        }
        self.parse_node(true, false)
    }

    fn parse_document_end(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        let start = token.start;
        let mut end = token.start;
        let mut implicit = true;
        if token.kind == TokenKind::DocumentEnd {
            end = token.end;
            self.skip_token();
            implicit = false;
        }
        self.tag_directives.clear();
        self.state = State::DocumentStart;
        Ok(Self::event(EventKind::DocumentEnd { implicit }, start, end))
    }

    fn process_directives(&mut self) -> Result<()> {
        let mut version_seen = false;
        let mut token = self.peek_token()?;
        while matches!(
            token.kind,
            TokenKind::VersionDirective | TokenKind::TagDirective
        ) {
            if token.kind == TokenKind::VersionDirective {
                if version_seen {
                    return Err(LoadError::new(
                        Stage::Parser,
                        "found duplicate %YAML directive",
                        token.start,
                    ));
                }
                if token.major != 1 || token.minor != 1 {
                    return Err(LoadError::new(
                        Stage::Parser,
                        "found incompatible YAML document",
                        token.start,
                    ));
                }
                version_seen = true;
            } else {
                self.append_tag_directive(
                    token.value.clone(),
                    token.prefix.clone(),
                    false,
                    token.start,
                )?;
            }
            self.skip_token();
            token = self.peek_token()?;
        }
        self.append_tag_directive(b"!".to_vec(), b"!".to_vec(), true, token.start)?;
        self.append_tag_directive(
            b"!!".to_vec(),
            b"tag:yaml.org,2002:".to_vec(),
            true,
            token.start,
        )?;
        Ok(())
    }

    fn append_tag_directive(
        &mut self,
        handle: Vec<u8>,
        prefix: Vec<u8>,
        allow_duplicates: bool,
        mark: Mark,
    ) -> Result<()> {
        if self.tag_directives.iter().any(|(h, _)| *h == handle) {
            if allow_duplicates {
                return Ok(());
            }
            return Err(LoadError::new(
                Stage::Parser,
                "found duplicate %TAG directive",
                mark,
            ));
        }
        self.tag_directives.push((handle, prefix));
        Ok(())
    }

    fn parse_node(&mut self, block: bool, indentless_sequence: bool) -> Result<Event> {
        let mut token = self.peek_token()?;
        if token.kind == TokenKind::Alias {
            self.pop_state();
            self.skip_token();
            return Ok(Self::event(
                EventKind::Alias {
                    anchor: text(&token.value),
                },
                token.start,
                token.end,
            ));
        }
        let mut start = token.start;
        let mut end = token.start;
        let mut tag_token = false;
        let (mut tag_handle, mut tag_suffix, mut anchor) = (Vec::new(), Vec::new(), None);
        let mut tag_mark = Mark::default();
        match token.kind {
            TokenKind::Anchor => {
                anchor = Some(text(&token.value));
                start = token.start;
                end = token.end;
                self.skip_token();
                token = self.peek_token()?;
                if token.kind == TokenKind::Tag {
                    tag_token = true;
                    tag_handle.clone_from(&token.value);
                    tag_suffix.clone_from(&token.suffix);
                    tag_mark = token.start;
                    end = token.end;
                    self.skip_token();
                    token = self.peek_token()?;
                }
            }
            TokenKind::Tag => {
                tag_token = true;
                tag_handle.clone_from(&token.value);
                tag_suffix.clone_from(&token.suffix);
                start = token.start;
                tag_mark = token.start;
                end = token.end;
                self.skip_token();
                token = self.peek_token()?;
                if token.kind == TokenKind::Anchor {
                    anchor = Some(text(&token.value));
                    end = token.end;
                    self.skip_token();
                    token = self.peek_token()?;
                }
            }
            _ => {}
        }
        let mut tag = Vec::new();
        if tag_token {
            if tag_handle.is_empty() {
                tag = tag_suffix;
            } else {
                if let Some((_, prefix)) =
                    self.tag_directives.iter().find(|(h, _)| *h == tag_handle)
                {
                    tag.clone_from(prefix);
                    tag.extend_from_slice(&tag_suffix);
                }
                if tag.is_empty() {
                    return Err(parser_error(
                        "while parsing a node",
                        start,
                        "found undefined tag handle",
                        tag_mark,
                    ));
                }
            }
        }
        let implicit = tag.is_empty();
        let tag_text = text(&tag);
        if indentless_sequence && token.kind == TokenKind::BlockEntry {
            end = token.end;
            self.state = State::IndentlessSequenceEntry;
            return Ok(Self::event(
                EventKind::SequenceStart {
                    anchor,
                    tag: tag_text,
                    implicit,
                    flow: false,
                },
                start,
                end,
            ));
        }
        if token.kind == TokenKind::Scalar {
            end = token.end;
            let (mut plain_implicit, mut quoted_implicit) = (false, false);
            if (tag.is_empty() && token.style == ScalarStyle::Plain) || tag == b"!" {
                plain_implicit = true;
            } else if tag.is_empty() {
                quoted_implicit = true;
            }
            self.pop_state();
            self.skip_token();
            return Ok(Self::event(
                EventKind::Scalar {
                    anchor,
                    tag: tag_text,
                    value: text(&token.value),
                    style: token.style,
                    plain_implicit,
                    quoted_implicit,
                },
                start,
                end,
            ));
        }
        let collection = match token.kind {
            TokenKind::FlowSequenceStart => Some((State::FlowSequenceFirstEntry, false, true)),
            TokenKind::FlowMappingStart => Some((State::FlowMappingFirstKey, true, true)),
            TokenKind::BlockSequenceStart if block => {
                Some((State::BlockSequenceFirstEntry, false, false))
            }
            TokenKind::BlockMappingStart if block => {
                Some((State::BlockMappingFirstKey, true, false))
            }
            _ => None,
        };
        if let Some((state, mapping, flow)) = collection {
            end = token.end;
            self.state = state;
            let kind = if mapping {
                EventKind::MappingStart {
                    anchor,
                    tag: tag_text,
                    implicit,
                    flow,
                }
            } else {
                EventKind::SequenceStart {
                    anchor,
                    tag: tag_text,
                    implicit,
                    flow,
                }
            };
            return Ok(Self::event(kind, start, end));
        }
        if anchor.is_some() || !tag.is_empty() {
            self.pop_state();
            return Ok(Self::event(
                EventKind::Scalar {
                    anchor,
                    tag: tag_text,
                    value: String::new(),
                    style: ScalarStyle::Plain,
                    plain_implicit: implicit,
                    quoted_implicit: false,
                },
                start,
                end,
            ));
        }
        let context = if block {
            "while parsing a block node"
        } else {
            "while parsing a flow node"
        };
        Err(parser_error(
            context,
            start,
            "did not find expected node content",
            token.start,
        ))
    }

    fn parse_block_sequence_entry(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start);
            self.skip_token();
        }
        let token = self.peek_token()?;
        if token.kind == TokenKind::BlockEntry {
            let mark = token.end;
            self.skip_token();
            let next = self.peek_kind()?;
            if next != TokenKind::BlockEntry && next != TokenKind::BlockEnd {
                self.states.push(State::BlockSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = State::BlockSequenceEntry;
            return Ok(Self::empty_scalar(mark));
        }
        if token.kind == TokenKind::BlockEnd {
            self.pop_state();
            self.marks.pop();
            self.skip_token();
            return Ok(Self::event(EventKind::SequenceEnd, token.start, token.end));
        }
        let context_mark = self.marks.pop().unwrap_or_default();
        Err(parser_error(
            "while parsing a block collection",
            context_mark,
            "did not find expected '-' indicator",
            token.start,
        ))
    }

    fn parse_indentless_sequence_entry(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        if token.kind == TokenKind::BlockEntry {
            let mark = token.end;
            self.skip_token();
            let next = self.peek_kind()?;
            if !matches!(
                next,
                TokenKind::BlockEntry | TokenKind::Key | TokenKind::Value | TokenKind::BlockEnd
            ) {
                self.states.push(State::IndentlessSequenceEntry);
                return self.parse_node(true, false);
            }
            self.state = State::IndentlessSequenceEntry;
            return Ok(Self::empty_scalar(mark));
        }
        self.pop_state();
        Ok(Self::event(
            EventKind::SequenceEnd,
            token.start,
            token.start,
        ))
    }

    fn parse_block_mapping_key(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start);
            self.skip_token();
        }
        let token = self.peek_token()?;
        match token.kind {
            TokenKind::Key => {
                let mark = token.end;
                self.skip_token();
                let next = self.peek_kind()?;
                if !matches!(
                    next,
                    TokenKind::Key | TokenKind::Value | TokenKind::BlockEnd
                ) {
                    self.states.push(State::BlockMappingValue);
                    return self.parse_node(true, true);
                }
                self.state = State::BlockMappingValue;
                Ok(Self::empty_scalar(mark))
            }
            TokenKind::BlockEnd => {
                self.pop_state();
                self.marks.pop();
                self.skip_token();
                Ok(Self::event(EventKind::MappingEnd, token.start, token.end))
            }
            _ => {
                let context_mark = self.marks.pop().unwrap_or_default();
                Err(parser_error(
                    "while parsing a block mapping",
                    context_mark,
                    "did not find expected key",
                    token.start,
                ))
            }
        }
    }

    fn parse_block_mapping_value(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        if token.kind == TokenKind::Value {
            let mark = token.end;
            self.skip_token();
            let next = self.peek_kind()?;
            if !matches!(
                next,
                TokenKind::Key | TokenKind::Value | TokenKind::BlockEnd
            ) {
                self.states.push(State::BlockMappingKey);
                return self.parse_node(true, true);
            }
            self.state = State::BlockMappingKey;
            return Ok(Self::empty_scalar(mark));
        }
        self.state = State::BlockMappingKey;
        Ok(Self::empty_scalar(token.start))
    }

    fn parse_flow_sequence_entry(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start);
            self.skip_token();
        }
        let mut token = self.peek_token()?;
        if token.kind != TokenKind::FlowSequenceEnd {
            if !first {
                if token.kind == TokenKind::FlowEntry {
                    self.skip_token();
                    token = self.peek_token()?;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return Err(parser_error(
                        "while parsing a flow sequence",
                        context_mark,
                        "did not find expected ',' or ']'",
                        token.start,
                    ));
                }
            }
            if token.kind == TokenKind::Key {
                self.state = State::FlowSequenceEntryMappingKey;
                self.skip_token();
                return Ok(Self::event(
                    EventKind::MappingStart {
                        anchor: None,
                        tag: String::new(),
                        implicit: true,
                        flow: true,
                    },
                    token.start,
                    token.end,
                ));
            } else if token.kind != TokenKind::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntry);
                return self.parse_node(false, false);
            }
        }
        self.pop_state();
        self.marks.pop();
        self.skip_token();
        Ok(Self::event(EventKind::SequenceEnd, token.start, token.end))
    }

    fn parse_flow_sequence_entry_mapping_key(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        if !matches!(
            token.kind,
            TokenKind::Value | TokenKind::FlowEntry | TokenKind::FlowSequenceEnd
        ) {
            self.states.push(State::FlowSequenceEntryMappingValue);
            return self.parse_node(false, false);
        }
        let mark = token.end;
        self.skip_token();
        self.state = State::FlowSequenceEntryMappingValue;
        Ok(Self::empty_scalar(mark))
    }

    fn parse_flow_sequence_entry_mapping_value(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        if token.kind == TokenKind::Value {
            self.skip_token();
            let next = self.peek_token()?;
            if next.kind != TokenKind::FlowEntry && next.kind != TokenKind::FlowSequenceEnd {
                self.states.push(State::FlowSequenceEntryMappingEnd);
                return self.parse_node(false, false);
            }
        }
        // go-yaml's next token shadows this one there, so an empty value is where the
        // `:` was.
        self.state = State::FlowSequenceEntryMappingEnd;
        Ok(Self::empty_scalar(token.start))
    }

    fn parse_flow_sequence_entry_mapping_end(&mut self) -> Result<Event> {
        let token = self.peek_token()?;
        self.state = State::FlowSequenceEntry;
        Ok(Self::event(EventKind::MappingEnd, token.start, token.start))
    }

    fn parse_flow_mapping_key(&mut self, first: bool) -> Result<Event> {
        if first {
            let token = self.peek_token()?;
            self.marks.push(token.start);
            self.skip_token();
        }
        let mut token = self.peek_token()?;
        if token.kind != TokenKind::FlowMappingEnd {
            if !first {
                if token.kind == TokenKind::FlowEntry {
                    self.skip_token();
                    token = self.peek_token()?;
                } else {
                    let context_mark = self.marks.pop().unwrap_or_default();
                    return Err(parser_error(
                        "while parsing a flow mapping",
                        context_mark,
                        "did not find expected ',' or '}'",
                        token.start,
                    ));
                }
            }
            if token.kind == TokenKind::Key {
                self.skip_token();
                token = self.peek_token()?;
                if !matches!(
                    token.kind,
                    TokenKind::Value | TokenKind::FlowEntry | TokenKind::FlowMappingEnd
                ) {
                    self.states.push(State::FlowMappingValue);
                    return self.parse_node(false, false);
                }
                self.state = State::FlowMappingValue;
                return Ok(Self::empty_scalar(token.start));
            } else if token.kind != TokenKind::FlowMappingEnd {
                self.states.push(State::FlowMappingEmptyValue);
                return self.parse_node(false, false);
            }
        }
        self.pop_state();
        self.marks.pop();
        self.skip_token();
        Ok(Self::event(EventKind::MappingEnd, token.start, token.end))
    }

    fn parse_flow_mapping_value(&mut self, empty: bool) -> Result<Event> {
        let mut token = self.peek_token()?;
        if empty {
            self.state = State::FlowMappingKey;
            return Ok(Self::empty_scalar(token.start));
        }
        if token.kind == TokenKind::Value {
            self.skip_token();
            token = self.peek_token()?;
            if token.kind != TokenKind::FlowEntry && token.kind != TokenKind::FlowMappingEnd {
                self.states.push(State::FlowMappingKey);
                return self.parse_node(false, false);
            }
        }
        self.state = State::FlowMappingKey;
        Ok(Self::empty_scalar(token.start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(m: &Mark) -> String {
        format!("{}:{}:{}", m.line, m.column, m.index)
    }

    fn props(anchor: &Option<String>, tag: &str) -> String {
        let mut out = String::new();
        if let Some(anchor) = anchor {
            out.push_str(&format!(" &{anchor}"));
        }
        if !tag.is_empty() {
            out.push_str(&format!(" <{tag}>"));
        }
        out
    }

    /// The events of an input as go-yaml's formatEvent writes them, with their marks.
    fn dump(input: &[u8]) -> String {
        let mut parser = Parser::new(input);
        let mut out = String::new();
        loop {
            let event = match parser.next_event() {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(e) => {
                    out.push_str(&format!("ERR {e}\n"));
                    break;
                }
            };
            out.push_str(&format!("{}-{} ", mark(&event.start), mark(&event.end)));
            match &event.kind {
                EventKind::StreamStart => out.push_str("+STR"),
                EventKind::StreamEnd => out.push_str("-STR"),
                EventKind::DocumentStart { implicit } => {
                    out.push_str(if *implicit { "+DOC" } else { "+DOC ---" })
                }
                EventKind::DocumentEnd { implicit } => {
                    out.push_str(if *implicit { "-DOC" } else { "-DOC ..." })
                }
                EventKind::Alias { anchor } => out.push_str(&format!("=ALI *{anchor}")),
                EventKind::Scalar {
                    anchor,
                    tag,
                    value,
                    style,
                    plain_implicit,
                    quoted_implicit,
                } => {
                    let style = match style {
                        ScalarStyle::Plain => " :",
                        ScalarStyle::Literal => " |",
                        ScalarStyle::Folded => " >",
                        ScalarStyle::SingleQuoted => " '",
                        ScalarStyle::DoubleQuoted => " \"",
                    };
                    let mut escaped = String::new();
                    for c in value.chars() {
                        match c {
                            '\\' => escaped.push_str("\\\\"),
                            '\n' => escaped.push_str("\\n"),
                            '\t' => escaped.push_str("\\t"),
                            c => escaped.push(c),
                        }
                    }
                    out.push_str(&format!(
                        "=VAL{}{style}{escaped} pi={plain_implicit} qi={quoted_implicit}",
                        props(anchor, tag)
                    ));
                }
                EventKind::SequenceStart {
                    anchor, tag, flow, ..
                } => out.push_str(&format!(
                    "+SEQ{}{}",
                    props(anchor, tag),
                    if *flow { " []" } else { "" }
                )),
                EventKind::SequenceEnd => out.push_str("-SEQ"),
                EventKind::MappingStart {
                    anchor, tag, flow, ..
                } => out.push_str(&format!(
                    "+MAP{}{}",
                    props(anchor, tag),
                    if *flow { " {}" } else { "" }
                )),
                EventKind::MappingEnd => out.push_str("-MAP"),
            }
            out.push('\n');
            if event.kind == EventKind::StreamEnd {
                break;
            }
        }
        out
    }

    /// What go-yaml's own parser makes of these (tests/compose/go-yaml.sh events).
    #[test]
    fn the_reader_refuses_what_go_yaml_refuses() {
        assert_eq!(dump(b"a: 1\n\x01\n"), "ERR go-yaml load error in reader at <unknown position>: control characters are not allowed (value: 1)\n");
        // The input is read 512 bytes at a time: a bad byte past the first chunk is
        // found when the scanner gets there, one in it before anything is parsed.
        let mut late = b"a: [1\nb: ".to_vec();
        late.extend_from_slice(&[b'x'; 600]);
        late.extend_from_slice(b"\n\xff\n");
        assert_eq!(dump(&late), concat!(
                "1:1:0-1:1:0 +STR\n",
                "1:1:0-1:1:0 +DOC\n",
                "1:1:0-1:1:0 +MAP\n",
                "1:1:0-1:2:1 =VAL :a pi=true qi=false\n",
                "1:4:3-1:5:4 +SEQ []\n",
                "ERR go-yaml load error in reader at <unknown position>: invalid leading UTF-8 octet (value: 255)\n",
            ));
        assert_eq!(dump(b"a: [1\nb: \xff\n"), "ERR go-yaml load error in reader at <unknown position>: invalid leading UTF-8 octet (value: 255)\n");
    }

    #[test]
    fn utf16_is_read_after_its_bom() {
        let mut input = vec![0xff, 0xfe];
        for unit in "a: [1, \u{e9}]\n".encode_utf16() {
            input.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            dump(&input),
            concat!(
                "1:1:0-1:1:0 +STR\n",
                "1:1:0-1:1:0 +DOC\n",
                "1:1:0-1:1:0 +MAP\n",
                "1:1:0-1:2:1 =VAL :a pi=true qi=false\n",
                "1:4:3-1:5:4 +SEQ []\n",
                "1:5:4-1:6:5 =VAL :1 pi=true qi=false\n",
                "1:8:7-1:9:8 =VAL :\u{e9} pi=true qi=false\n",
                "1:9:8-1:10:9 -SEQ\n",
                "2:1:9-2:1:9 -MAP\n",
                "2:1:10-2:1:10 -DOC\n",
                "2:1:10-2:1:10 -STR\n",
            )
        );
    }

    #[test]
    fn marks_follow_go_yaml() {
        // The empty value of `[:` is where the `:` is.
        assert_eq!(dump(b"- [:,\n"), concat!(
                "1:1:0-1:1:0 +STR\n",
                "1:1:0-1:1:0 +DOC\n",
                "1:1:0-1:1:0 +SEQ\n",
                "1:3:2-1:4:3 +SEQ []\n",
                "1:4:3-1:4:3 +MAP {}\n",
                "1:4:3-1:4:3 =VAL : pi=true qi=false\n",
                "1:4:3-1:4:3 =VAL : pi=true qi=false\n",
                "1:5:4-1:5:4 -MAP\n",
                "ERR go-yaml load error in parser (while parsing a flow node) at L2.C1: did not find expected node content\n",
            ));
        // A block ends where a comment at its column starts.
        assert_eq!(
            dump(b"---\n? key\n# c\n"),
            concat!(
                "1:1:0-1:1:0 +STR\n",
                "1:1:0-1:4:3 +DOC ---\n",
                "2:1:4-2:1:4 +MAP\n",
                "2:3:6-2:6:9 =VAL :key pi=true qi=false\n",
                "3:2:10-3:2:10 =VAL : pi=true qi=false\n",
                "3:2:10-3:2:10 -MAP\n",
                "4:1:14-4:1:14 -DOC\n",
                "4:1:14-4:1:14 -STR\n",
            )
        );
        assert_eq!(
            dump(b"a:\n  b: 1\n  # foot\nc: 2\n"),
            concat!(
                "1:1:0-1:1:0 +STR\n",
                "1:1:0-1:1:0 +DOC\n",
                "1:1:0-1:1:0 +MAP\n",
                "1:1:0-1:2:1 =VAL :a pi=true qi=false\n",
                "2:3:5-2:3:5 +MAP\n",
                "2:3:5-2:4:6 =VAL :b pi=true qi=false\n",
                "2:6:8-2:7:9 =VAL :1 pi=true qi=false\n",
                "3:4:12-3:4:12 -MAP\n",
                "4:1:19-4:2:20 =VAL :c pi=true qi=false\n",
                "4:4:22-4:5:23 =VAL :2 pi=true qi=false\n",
                "5:1:23-5:1:23 -MAP\n",
                "5:1:24-5:1:24 -DOC\n",
                "5:1:24-5:1:24 -STR\n",
            )
        );
    }

    #[test]
    fn a_tab_before_a_comment_after_a_key_is_taken() {
        assert_eq!(
            dump(b"key:\t# c\nother: 1\n"),
            concat!(
                "1:1:0-1:1:0 +STR\n",
                "1:1:0-1:1:0 +DOC\n",
                "1:1:0-1:1:0 +MAP\n",
                "1:1:0-1:4:3 =VAL :key pi=true qi=false\n",
                "1:5:4-1:5:4 =VAL : pi=true qi=false\n",
                "2:1:9-2:6:14 =VAL :other pi=true qi=false\n",
                "2:8:16-2:9:17 =VAL :1 pi=true qi=false\n",
                "3:1:17-3:1:17 -MAP\n",
                "3:1:18-3:1:18 -DOC\n",
                "3:1:18-3:1:18 -STR\n",
            )
        );
    }

    /// Each input under NSPAWN_YAML_EVENTS against the events go-yaml made of it, in a
    /// file next to it with `.events` added (tests/compose/go-yaml.sh events).
    #[test]
    #[ignore]
    fn the_parser_matches_go_yaml() {
        let root = std::env::var("NSPAWN_YAML_EVENTS").expect("NSPAWN_YAML_EVENTS");
        let mut inputs = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_none_or(|e| e != "events") {
                    inputs.push(path);
                }
            }
        }
        inputs.sort();
        let mut failures = Vec::new();
        let mut compared = 0;
        for input in &inputs {
            let expected = input.with_file_name(format!(
                "{}.events",
                input.file_name().unwrap().to_string_lossy()
            ));
            let Ok(want) = std::fs::read(&expected) else {
                continue;
            };
            compared += 1;
            let want = String::from_utf8_lossy(&want).into_owned();
            let got = dump(&std::fs::read(input).unwrap());
            if want != got {
                let (w, g): (Vec<&str>, Vec<&str>) =
                    (want.lines().collect(), got.lines().collect());
                let at = w
                    .iter()
                    .zip(&g)
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| w.len().min(g.len()));
                failures.push(format!(
                    "{}: event {}\n  go-yaml: {}\n  nspawn:  {}",
                    input.display(),
                    at + 1,
                    w.get(at).unwrap_or(&"(none)"),
                    g.get(at).unwrap_or(&"(none)")
                ));
            }
        }
        assert!(compared > 0, "no inputs with events");
        assert!(
            failures.is_empty(),
            "{} of {compared} inputs differ:\n{}",
            failures.len(),
            failures
                .iter()
                .take(40)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
