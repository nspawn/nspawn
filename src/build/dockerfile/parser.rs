//! Dockerfile bytes to AST (parser.go, line_parsers.go, split_command.go,
//! directives.go).
//!
//! The input is split into lines on bytes, each line is then read as UTF-8
//! with invalid sequences replaced by U+FFFD.

use std::collections::HashSet;
use std::fmt;

use super::compat::{quote, quote_list, to_lower};
use super::json::{self, Document};
use super::shell::{EnvGetter, Lex};
use super::{to_ranges, Error, Range};

pub const DEFAULT_ESCAPE_TOKEN: char = '\\';

/// bufio.MaxScanTokenSize: no line of the input may be longer, its line
/// ending included.
const MAX_SCAN_TOKEN_SIZE: usize = 64 * 1024;

const KEY_SYNTAX: &str = "syntax";
const KEY_CHECK: &str = "check";
const KEY_ESCAPE: &str = "escape";

const ERR_NOT_STRING_ARRAY: &str =
    "when using JSON array syntax, arrays must be comprised of strings only";

/// A node of the AST. The root has one child per instruction; an
/// instruction node holds the instruction name in `value` and its arguments
/// as the chain of `next` nodes. ONBUILD keeps its sub-instruction as the
/// only child of its first `next` node.
#[derive(Default)]
pub struct Node {
    pub value: String,
    pub next: Option<Box<Node>>,
    pub children: Vec<Node>,
    pub heredocs: Vec<Heredoc>,
    /// The arguments were written as a JSON array (the reference's
    /// `Attributes["json"]`, the only attribute it sets).
    pub json: bool,
    /// The instruction as written, continuation lines joined.
    pub original: String,
    /// The `--flag` words in front of the arguments.
    pub flags: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    /// The comment lines right above the instruction.
    pub prev_comment: Vec<String>,
}

impl Node {
    fn with_value(value: impl Into<String>) -> Node {
        let mut node = Node::default();
        node.value = value.into();
        node
    }

    /// The nodes of the `next` chain, in order.
    pub fn next_nodes(&self) -> NextNodes<'_> {
        NextNodes {
            cur: self.next.as_deref(),
        }
    }

    /// The source lines of the node.
    pub fn location(&self) -> Vec<Range> {
        to_ranges(self.start_line, self.end_line)
    }

    /// The AST as s-expressions, the format of the reference's test
    /// vectors.
    pub fn dump(&self) -> String {
        let mut s = to_lower(&self.value);
        if !self.flags.is_empty() {
            s.push(' ');
            s.push_str(&quote_list(&self.flags));
        }
        for n in &self.children {
            s.push('(');
            s.push_str(&n.dump());
            s.push_str(")\n");
        }
        for n in self.next_nodes() {
            s.push(' ');
            if n.children.is_empty() {
                s.push_str(&quote(&n.value));
            } else {
                s.push_str(&n.dump());
            }
        }
        s.trim().to_string()
    }

    pub fn add_child(&mut self, mut child: Node, start_line: usize, end_line: usize) {
        child.start_line = start_line;
        child.end_line = end_line;
        if self.children.is_empty() {
            self.start_line = start_line;
        }
        self.end_line = end_line;
        self.children.push(child);
    }

    fn can_contain_heredoc(&self) -> bool {
        let mut node = self;
        if to_lower(&node.value) == "onbuild" {
            if let Some(first) = node.next.as_deref().and_then(|n| n.children.first()) {
                node = first;
            }
        }
        matches!(to_lower(&node.value).as_str(), "add" | "copy" | "run") && !node.json
    }

    /// Every field but `next` and `children`.
    fn fields_eq(&self, other: &Node) -> bool {
        self.value == other.value
            && self.heredocs == other.heredocs
            && self.json == other.json
            && self.original == other.original
            && self.flags == other.flags
            && self.start_line == other.start_line
            && self.end_line == other.end_line
            && self.prev_comment == other.prev_comment
    }

    /// Every field but `next` and `children`.
    fn clone_fields(&self) -> Node {
        Node {
            value: self.value.clone(),
            next: None,
            children: Vec::new(),
            heredocs: self.heredocs.clone(),
            json: self.json,
            original: self.original.clone(),
            flags: self.flags.clone(),
            start_line: self.start_line,
            end_line: self.end_line,
            prev_comment: self.prev_comment.clone(),
        }
    }
}

// Argument chains can be tens of thousands of nodes long and ONBUILD can
// nest thousands of levels deep, so dropping, cloning and comparing work
// with explicit stacks instead of recursion. Debug output and dump() do
// recurse into children.

impl Drop for Node {
    fn drop(&mut self) {
        if self.next.is_none() && self.children.is_empty() {
            return;
        }
        let mut stack: Vec<Node> = Vec::new();
        if let Some(n) = self.next.take() {
            stack.push(*n);
        }
        stack.append(&mut self.children);
        while let Some(mut n) = stack.pop() {
            if let Some(next) = n.next.take() {
                stack.push(*next);
            }
            stack.append(&mut n.children);
        }
    }
}

impl Clone for Node {
    fn clone(&self) -> Node {
        enum Step<'a> {
            Enter(&'a Node),
            Build(&'a Node),
        }
        // Post-order: the copies of the children and of `next` are on `done`
        // when a node is built, `next` on top and the children below it.
        let mut work = vec![Step::Enter(self)];
        let mut done: Vec<Node> = Vec::new();
        while let Some(step) = work.pop() {
            match step {
                Step::Enter(n) => {
                    work.push(Step::Build(n));
                    if let Some(next) = n.next.as_deref() {
                        work.push(Step::Enter(next));
                    }
                    work.extend(n.children.iter().rev().map(Step::Enter));
                }
                Step::Build(n) => {
                    let mut copy = n.clone_fields();
                    if n.next.is_some() {
                        copy.next = done.pop().map(Box::new);
                    }
                    copy.children = done.split_off(done.len() - n.children.len());
                    done.push(copy);
                }
            }
        }
        done.pop().unwrap_or_default()
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Node) -> bool {
        let mut stack = vec![(self, other)];
        while let Some((a, b)) = stack.pop() {
            if !a.fields_eq(b) || a.children.len() != b.children.len() {
                return false;
            }
            match (a.next.as_deref(), b.next.as_deref()) {
                (Some(x), Some(y)) => stack.push((x, y)),
                (None, None) => {}
                _ => return false,
            }
            stack.extend(a.children.iter().zip(&b.children));
        }
        true
    }
}

impl Eq for Node {}

struct NodeFields<'a>(&'a Node);

impl fmt::Debug for NodeFields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        f.debug_struct("Node")
            .field("value", &n.value)
            .field("flags", &n.flags)
            .field("json", &n.json)
            .field("children", &n.children)
            .field("heredocs", &n.heredocs)
            .field("original", &n.original)
            .field("start_line", &n.start_line)
            .field("end_line", &n.end_line)
            .field("prev_comment", &n.prev_comment)
            .finish()
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("node", &NodeFields(self))
            .field(
                "next",
                &self.next_nodes().map(NodeFields).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Iterator over a `next` chain.
pub struct NextNodes<'a> {
    cur: Option<&'a Node>,
}

impl<'a> Iterator for NextNodes<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let n = self.cur?;
        self.cur = n.next.as_deref();
        Some(n)
    }
}

/// A here-document attached to an instruction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Heredoc {
    pub name: String,
    pub file_descriptor: u64,
    /// Variables in the content are expanded (the delimiter is unquoted).
    pub expand: bool,
    /// Leading tabs are stripped (`<<-`).
    pub chomp: bool,
    pub content: String,
}

/// A parser directive (`# syntax=...`, `# escape=...`, `# check=...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub name: String,
    pub value: String,
    pub location: Vec<Range>,
}

/// A warning found while parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub short: String,
    pub detail: Vec<String>,
    pub url: String,
    pub location: Option<Range>,
}

/// The outcome of [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub ast: Node,
    pub escape_token: char,
    /// The parser directives at the top of the file, in order.
    pub directives: Vec<Directive>,
    pub warnings: Vec<Warning>,
}

impl Parsed {
    /// PrintWarnings in parser.go.
    pub fn print_warnings(&self, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        if self.warnings.is_empty() {
            return Ok(());
        }
        for w in &self.warnings {
            writeln!(out, "[WARNING]: {}", w.short)?;
        }
        writeln!(
            out,
            "[WARNING]: Empty continuation lines will become errors in a future release."
        )
    }
}

/// Parses a Dockerfile.
pub fn parse(data: &[u8]) -> Result<Parsed, Error> {
    let mut d = Directives::new();
    let mut current_line = 0usize;
    let mut root = Node::default();
    let mut scanner = LineScanner::new(data, true);
    let mut warnings = Vec::new();
    let mut comments: Vec<String> = Vec::new();

    while let Some(raw) = scanner.scan() {
        let raw = if current_line == 0 {
            discard_bom(raw)
        } else {
            raw
        };
        let text = String::from_utf8_lossy(raw);
        if is_comment(&text) {
            // The reference drops the first byte of the line, not the #, so
            // an indented comment keeps its #.
            let comment = String::from_utf8_lossy(&raw[1..]).trim().to_string();
            if comment.is_empty() {
                comments.clear();
            } else {
                comments.push(comment);
            }
        }
        let (bytes_read, directive_ok) = d
            .process_line(&text, true)
            .map_err(|e| e.at_lines(current_line, 0))?;
        if directive_ok {
            comments.pop();
        }
        current_line += 1;

        let start_line = current_line;
        let (first, mut is_end_of_line) = trim_continuation_character(&bytes_read, d.escape_token);
        if is_end_of_line && first.is_empty() {
            continue;
        }
        let mut line = first;

        let mut has_empty_continuation_line = false;
        while !is_end_of_line {
            let Some(raw) = scanner.scan() else { break };
            let text = String::from_utf8_lossy(raw);
            let (bytes_read, _) = d
                .process_line(&text, false)
                .map_err(|e| e.at_lines(current_line, 0))?;
            current_line += 1;

            if is_comment(&text) {
                continue;
            }
            if is_empty_continuation_line(&bytes_read) {
                has_empty_continuation_line = true;
                continue;
            }
            let (part, eol) = trim_continuation_character(&bytes_read, d.escape_token);
            is_end_of_line = eol;
            line.push_str(&part);
        }

        if has_empty_continuation_line {
            let at = super::Position {
                line: current_line,
                character: 0,
            };
            warnings.push(Warning {
                short: format!("Empty continuation line found in: {line}"),
                detail: vec![
                    "Empty continuation lines will become errors in a future release".to_string(),
                ],
                url: "https://docs.docker.com/go/dockerfile/rule/no-empty-continuation/"
                    .to_string(),
                location: Some(Range { start: at, end: at }),
            });
        }

        let mut child = new_node_from_line(&line, &d, std::mem::take(&mut comments))
            .map_err(|e| e.at_lines(start_line, current_line))?;

        if child.can_contain_heredoc() && line.contains("<<") {
            let heredocs =
                heredocs_from_line(&line).map_err(|e| e.at_lines(start_line, current_line))?;
            for mut heredoc in heredocs {
                let mut terminated = false;
                let mut content = std::mem::take(&mut heredoc.content);
                while let Some(raw) = scanner.scan() {
                    current_line += 1;
                    let text = String::from_utf8_lossy(raw);
                    let mut possible_terminator = trim_newline(&text);
                    if heredoc.chomp {
                        possible_terminator = trim_leading_tabs(possible_terminator);
                    }
                    if possible_terminator == heredoc.name {
                        terminated = true;
                        break;
                    }
                    content.push_str(&text);
                }
                if !terminated {
                    return Err(
                        Error::new("unterminated heredoc").at_lines(start_line, current_line)
                    );
                }
                heredoc.content = content;
                child.heredocs.push(heredoc);
            }
        }

        root.add_child(child, start_line, current_line);
    }

    if root.children.is_empty() {
        return Err(Error::new("file with no instructions").at_lines(current_line, 0));
    }
    if scanner.too_long {
        return Err(Error::new(format!(
            "dockerfile line greater than max allowed size of {}",
            MAX_SCAN_TOKEN_SIZE - 1
        ))
        .at_lines(current_line, 0));
    }

    Ok(Parsed {
        ast: root,
        escape_token: d.escape_token,
        directives: d.found,
        warnings,
    })
}

/// bufio.Scanner over an in-memory input. Once a line exceeds the token
/// limit the scan stops and `too_long` is set.
struct LineScanner<'a> {
    data: &'a [u8],
    pos: usize,
    /// Keep the line ending (the reference's scanLines) or drop it with one
    /// carriage return before it (bufio.ScanLines).
    keep_ending: bool,
    too_long: bool,
}

impl<'a> LineScanner<'a> {
    fn new(data: &'a [u8], keep_ending: bool) -> LineScanner<'a> {
        LineScanner {
            data,
            pos: 0,
            keep_ending,
            too_long: false,
        }
    }

    fn scan(&mut self) -> Option<&'a [u8]> {
        if self.too_long || self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        let window = &rest[..rest.len().min(MAX_SCAN_TOKEN_SIZE)];
        let token = match window.iter().position(|&b| b == b'\n') {
            Some(i) => &rest[..=i],
            None if rest.len() < MAX_SCAN_TOKEN_SIZE => rest,
            None => {
                self.too_long = true;
                return None;
            }
        };
        self.pos += token.len();
        if self.keep_ending {
            return Some(token);
        }
        let token = token.strip_suffix(b"\n").unwrap_or(token);
        Some(token.strip_suffix(b"\r").unwrap_or(token))
    }
}

/// The parsing state the directives control.
struct Directives {
    parser: DirectiveParser,
    escape_token: char,
    found: Vec<Directive>,
}

impl Directives {
    fn new() -> Directives {
        Directives {
            parser: DirectiveParser::default(),
            escape_token: DEFAULT_ESCAPE_TOKEN,
            found: Vec::new(),
        }
    }

    fn set_escape_token(&mut self, s: &str) -> Result<(), Error> {
        match s {
            "`" => self.escape_token = '`',
            "\\" => self.escape_token = '\\',
            _ => {
                return Err(Error::new(format!(
                    "invalid escape token '{s}' does not match ` or \\"
                )))
            }
        }
        Ok(())
    }

    fn possible_parser_directive(&mut self, line: &str) -> Result<bool, Error> {
        let Some(directive) = self.parser.parse_line(line)? else {
            return Ok(false);
        };
        if directive.name == KEY_ESCAPE {
            self.set_escape_token(&directive.value)?;
        }
        self.found.push(directive);
        Ok(true)
    }

    /// processLine in parser.go: the line without its ending (and leading
    /// whitespace when asked), empty for a comment, and whether it was a
    /// parser directive.
    fn process_line(
        &mut self,
        token: &str,
        strip_left_whitespace: bool,
    ) -> Result<(String, bool), Error> {
        let mut token = trim_newline(token);
        if strip_left_whitespace {
            token = trim_leading_whitespace(token);
        }
        let directive_ok = self.possible_parser_directive(token)?;
        let token = if is_comment(token) { "" } else { token };
        Ok((token.to_string(), directive_ok))
    }
}

/// The directive rules of the reference: directives come first, each one
/// at most once, and the first other line ends them.
#[derive(Debug, Clone, Default)]
pub struct DirectiveParser {
    line: usize,
    comment: Option<String>,
    seen: HashSet<String>,
    done: bool,
}

impl DirectiveParser {
    /// The comment prefix directives are written behind, `#` by default.
    pub fn set_comment(&mut self, comment: &str) {
        self.comment = Some(comment.to_string());
    }

    pub fn parse_line(&mut self, line: &str) -> Result<Option<Directive>, Error> {
        self.line += 1;
        if self.done {
            return Ok(None);
        }
        let comment = self.comment.get_or_insert_with(|| "#".to_string());
        let Some(rest) = line.strip_prefix(comment.as_str()) else {
            self.done = true;
            return Ok(None);
        };
        let Some((name, value)) = match_directive(rest.trim_start()) else {
            self.done = true;
            return Ok(None);
        };
        let key = name.to_ascii_lowercase();
        if ![KEY_SYNTAX, KEY_ESCAPE, KEY_CHECK].contains(&key.as_str()) {
            self.done = true;
            return Ok(None);
        }
        if !self.seen.insert(key.clone()) {
            return Err(Error::new(format!(
                "only one {key} parser directive can be used"
            )));
        }
        Ok(Some(Directive {
            name: key,
            value: value.to_string(),
            location: to_ranges(self.line, self.line),
        }))
    }

    /// The directives at the start of `data`. On an error the directives
    /// read before it are returned with it, as the reference does.
    pub fn parse_all(&mut self, data: &[u8]) -> (Vec<Directive>, Option<Error>) {
        let mut scanner = LineScanner::new(data, false);
        let mut directives = Vec::new();
        while let Some(line) = scanner.scan() {
            if self.done {
                break;
            }
            match self.parse_line(&String::from_utf8_lossy(line)) {
                Ok(Some(d)) => directives.push(d),
                Ok(None) => {}
                Err(e) => return (directives, Some(e)),
            }
        }
        (directives, None)
    }
}

fn is_re2_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ')
}

/// `^([a-zA-Z][a-zA-Z0-9]*)\s*=\s*(.+?)\s*$` where `\s` is ASCII and `.`
/// does not match a newline.
fn match_directive(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    if !b.first()?.is_ascii_alphabetic() {
        return None;
    }
    let mut i = 1;
    while i < b.len() && b[i].is_ascii_alphanumeric() {
        i += 1;
    }
    let name = &s[..i];
    while i < b.len() && is_re2_space(b[i] as char) {
        i += 1;
    }
    if b.get(i) != Some(&b'=') {
        return None;
    }
    let rest = &s[i + 1..];
    let leading = rest.len() - rest.trim_start_matches(is_re2_space).len();
    // The greedy \s* gives back whitespace only when nothing else is left
    // for the value.
    for k in (0..=leading).rev() {
        let tail = &rest[k..];
        let Some(first) = tail.chars().next() else {
            continue;
        };
        let trimmed = tail.trim_end_matches(is_re2_space).len();
        let value = &tail[..trimmed.max(first.len_utf8())];
        if !value.contains('\n') {
            return Some((name, value));
        }
    }
    None
}

/// A directive found by [`detect_syntax`] or [`parse_directive`]: the value
/// up to its first space, the whole value, and where it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedDirective {
    pub value: String,
    pub cmdline: String,
    pub location: Vec<Range>,
}

/// The syntax directive of a Dockerfile, also accepted as `// syntax=...`
/// or as the `syntax` member of a JSON document; a leading `#!` line is
/// skipped.
pub fn detect_syntax(dt: &[u8]) -> Option<DetectedDirective> {
    find_directive(KEY_SYNTAX, dt, true)
}

/// The directive `key` of a Dockerfile, in `#` form only.
pub fn parse_directive(key: &str, dt: &[u8]) -> Option<DetectedDirective> {
    find_directive(key, dt, false)
}

fn find_directive(key: &str, dt: &[u8], any_format: bool) -> Option<DetectedDirective> {
    let dt = discard_bom(dt);
    let (dt, had_shebang) = discard_shebang(dt);
    let line = usize::from(had_shebang);

    let parser = DirectiveParser {
        line,
        ..DirectiveParser::default()
    };
    if let Some(found) = directive_from_parser(key, dt, parser) {
        return Some(found);
    }
    if !any_format {
        return None;
    }

    let mut parser = DirectiveParser {
        line,
        ..DirectiveParser::default()
    };
    parser.set_comment("//");
    if let Some(found) = directive_from_parser(key, dt, parser) {
        return Some(found);
    }

    if let Some(Document::Object(members)) = json::decode(&String::from_utf8_lossy(dt)) {
        // The last member of a name wins, as when decoding into a map.
        if let Some((_, Some(v))) = members.iter().rev().find(|(k, _)| k == key) {
            return Some(DetectedDirective {
                value: v.clone(),
                cmdline: v.clone(),
                location: to_ranges(line, line),
            });
        }
    }
    None
}

fn directive_from_parser(
    key: &str,
    dt: &[u8],
    mut parser: DirectiveParser,
) -> Option<DetectedDirective> {
    let (directives, _) = parser.parse_all(dt);
    directives
        .into_iter()
        .find(|d| d.name == key)
        .map(|d| DetectedDirective {
            value: d.value.split(' ').next().unwrap_or_default().to_string(),
            cmdline: d.value,
            location: d.location,
        })
}

fn discard_shebang(dt: &[u8]) -> (&[u8], bool) {
    let (line, rest) = match dt.iter().position(|&b| b == b'\n') {
        Some(i) => (&dt[..i], &dt[i + 1..]),
        None => (dt, &dt[dt.len()..]),
    };
    if line.starts_with(b"#!") {
        (rest, true)
    } else {
        (dt, false)
    }
}

fn discard_bom(dt: &[u8]) -> &[u8] {
    dt.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(dt)
}

fn trim_leading_whitespace(s: &str) -> &str {
    s.trim_start()
}

fn trim_leading_tabs(s: &str) -> &str {
    s.trim_start_matches('\t')
}

fn trim_newline(s: &str) -> &str {
    s.trim_end_matches(['\r', '\n'])
}

fn is_comment(line: &str) -> bool {
    trim_leading_whitespace(line).starts_with('#')
}

fn is_empty_continuation_line(line: &str) -> bool {
    trim_leading_whitespace(trim_newline(line)).is_empty()
}

/// Removes a trailing escape token (and the blanks after it) that continues
/// the line, unless the escape token is itself escaped. The reference's
/// regular expression does not recognise an escaped escape token before
/// that one either, and neither does this.
fn trim_continuation_character(line: &str, escape: char) -> (String, bool) {
    let t = line.trim_end_matches([' ', '\t']);
    if let Some(before) = t.strip_suffix(escape) {
        match before.chars().last() {
            None => return (String::new(), false),
            Some(c) if c != escape => return (before.to_string(), false),
            Some(_) => {}
        }
    }
    (line.to_string(), true)
}

/// Matches the reference's `[\t\v\f\r ]+`.
fn is_split_space(c: char) -> bool {
    matches!(c, '\t' | '\x0b' | '\x0c' | '\r' | ' ')
}

/// Regexp.Split with `[\t\v\f\r ]+`: at most `n` parts when `n` > 0.
fn split_whitespace(s: &str, n: isize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    if s.is_empty() {
        return vec![String::new()];
    }
    let limit = if n > 0 { n as usize } else { usize::MAX };
    let mut matches: Vec<(usize, usize)> = Vec::new();
    let mut run_start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if is_split_space(c) {
            run_start.get_or_insert(i);
        } else if let Some(start) = run_start.take() {
            matches.push((start, i));
            if matches.len() == limit {
                break;
            }
        }
    }
    if let Some(start) = run_start {
        if matches.len() < limit {
            matches.push((start, s.len()));
        }
    }
    let mut out = Vec::new();
    let mut beg = 0;
    let mut end = 0;
    for (start, stop) in matches {
        if n > 0 && out.len() == n as usize - 1 {
            break;
        }
        end = start;
        if stop != 0 {
            out.push(s[beg..end].to_string());
        }
        beg = stop;
    }
    if end != s.len() {
        out.push(s[beg..].to_string());
    }
    out
}

/// unicode.IsSpace applied to a single byte, the way split_command.go and
/// the HEALTHCHECK parser look at bytes: 0x85 and 0xA0 count as spaces even
/// inside a multi-byte character.
fn is_space_byte(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ' | 0x85 | 0xA0)
}

/// The instruction, its flags and its arguments.
fn split_command(line: &str, escape: char) -> (String, Vec<String>, String) {
    let mut parts = split_whitespace(line.trim(), 2);
    let mut flags = Vec::new();
    let mut args = String::new();
    if parts.len() == 2 {
        (args, flags) = extract_builder_flags(&parts[1], escape);
    }
    (parts.swap_remove(0), flags, args.trim().to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Spaces,
    Word,
    Quote,
}

/// The leading `--flag` words and the rest of the line. The reference reads
/// the line byte by byte and stores each byte as the char of that value, so
/// a non-ASCII flag comes out with every byte as its own Latin-1 char; that
/// is kept.
fn extract_builder_flags(line: &str, escape: char) -> (String, Vec<String>) {
    let b = line.as_bytes();
    let n = b.len();
    let rest = |pos: usize| String::from_utf8_lossy(&b[pos..]).into_owned();
    let is_escape = |c: u8| char::from(c) == escape;
    let mut words = Vec::new();
    let mut phase = Phase::Spaces;
    let mut word = String::new();
    let mut quote = 0u8;
    let mut blank_ok = false;
    let mut ch = 0u8;
    let mut pos = 0;

    while pos <= n {
        if pos != n {
            ch = b[pos];
        }
        if phase == Phase::Spaces {
            if pos == n {
                break;
            }
            if is_space_byte(ch) {
                pos += 1;
                continue;
            }
            if ch != b'-' || pos + 1 == n || b[pos + 1] != b'-' {
                return (rest(pos), words);
            }
            phase = Phase::Word;
        }
        if pos == n {
            if word != "--" && (blank_ok || !word.is_empty()) {
                words.push(word);
            }
            break;
        }
        if phase == Phase::Word {
            if is_space_byte(ch) {
                phase = Phase::Spaces;
                if word == "--" {
                    return (rest(pos), words);
                }
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
                blank_ok = false;
                pos += 1;
                continue;
            }
            if ch == b'\'' || ch == b'"' {
                quote = ch;
                blank_ok = true;
                phase = Phase::Quote;
                pos += 1;
                continue;
            }
            if is_escape(ch) {
                if pos + 1 == n {
                    pos += 1;
                    continue;
                }
                pos += 1;
                ch = b[pos];
            }
            word.push(char::from(ch));
            pos += 1;
            continue;
        }
        if ch == quote {
            phase = Phase::Word;
            pos += 1;
            continue;
        }
        if is_escape(ch) {
            if pos + 1 == n {
                phase = Phase::Word;
                pos += 1;
                continue;
            }
            pos += 1;
            ch = b[pos];
        }
        word.push(char::from(ch));
        pos += 1;
    }
    (String::new(), words)
}

fn node_chain<I, S>(values: I) -> Option<Box<Node>>
where
    I: IntoIterator<Item = S>,
    I::IntoIter: DoubleEndedIterator,
    S: Into<String>,
{
    let mut next = None;
    for v in values.into_iter().rev() {
        let mut node = Node::with_value(v);
        node.next = next;
        next = Some(Box::new(node));
    }
    next
}

type LineParse = Result<(Option<Box<Node>>, bool), Error>;

/// How many ONBUILD wrappers a line may nest in the tree.
const MAX_ONBUILD_LEVELS: usize = 64;

/// Builds the node of one instruction line. ONBUILD wraps the instruction
/// that follows it; nested ONBUILDs are unwound in a loop.
fn new_node_from_line(line: &str, d: &Directives, comments: Vec<String>) -> Result<Node, Error> {
    let mut levels: Vec<(String, Vec<String>, String)> = Vec::new();
    let mut current = line.to_string();
    let mut node = loop {
        let (cmd, flags, args) = split_command(&current, d.escape_token);
        let lower = to_lower(&cmd);
        if lower == "onbuild" && !args.is_empty() {
            // Each level keeps the rest of the line, so their cost grows with the square
            // of the depth; the instruction is refused past the second level anyway.
            if levels.len() >= MAX_ONBUILD_LEVELS {
                return Err(Error::new(
                    "Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed".to_string(),
                ));
            }
            levels.push((cmd, flags, std::mem::replace(&mut current, args)));
            continue;
        }
        let (next, json) = dispatch(&lower, &args, d)?;
        let mut node = Node::with_value(cmd);
        node.original = current;
        node.flags = flags;
        node.next = next;
        node.json = json;
        break node;
    };
    while let Some((cmd, flags, original)) = levels.pop() {
        let mut wrapper = Node::default();
        wrapper.children.push(node);
        node = Node::with_value(cmd);
        node.original = original;
        node.flags = flags;
        node.next = Some(Box::new(wrapper));
    }
    node.prev_comment = comments;
    Ok(node)
}

fn dispatch(cmd: &str, args: &str, d: &Directives) -> LineParse {
    match cmd {
        "add" | "copy" | "volume" => parse_maybe_json_to_list(args),
        "arg" => Ok((parse_name_or_name_val(args, d.escape_token), false)),
        "cmd" | "entrypoint" | "run" | "shell" => parse_maybe_json(args),
        "env" => Ok((parse_name_val(args, "ENV", d.escape_token)?, false)),
        "expose" | "from" => Ok((parse_strings_whitespace_delimited(args), false)),
        "healthcheck" => parse_health_config(args),
        "label" => Ok((parse_name_val(args, "LABEL", d.escape_token)?, false)),
        "maintainer" | "stopsignal" | "user" | "workdir" => Ok((parse_string(args), false)),
        // An ONBUILD without arguments; with arguments it is handled by the
        // caller.
        "onbuild" => Ok((None, false)),
        // Unknown instructions keep an empty argument node.
        _ => Ok((Some(Box::new(Node::default())), false)),
    }
}

/// Space separated words, quotes kept; the escape token keeps the char after
/// it in the word together with itself.
fn parse_words(rest: &str, escape: char) -> Vec<String> {
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    let mut words = Vec::new();
    let mut phase = Phase::Spaces;
    let mut quote = '\0';
    let mut blank_ok = false;
    let mut word = String::new();
    let mut ch = '\0';
    let mut pos = 0;

    while pos <= n {
        if pos != n {
            ch = chars[pos];
        }
        if phase == Phase::Spaces {
            if pos == n {
                break;
            }
            if ch.is_whitespace() {
                pos += 1;
                continue;
            }
            phase = Phase::Word;
        }
        if pos == n {
            if blank_ok || !word.is_empty() {
                words.push(word);
            }
            break;
        }
        if phase == Phase::Word {
            if ch.is_whitespace() {
                phase = Phase::Spaces;
                if blank_ok || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                word.clear();
                blank_ok = false;
                pos += 1;
                continue;
            }
            if ch == '\'' || ch == '"' {
                quote = ch;
                blank_ok = true;
                phase = Phase::Quote;
            }
            if ch == escape {
                if pos + 1 == n {
                    pos += 1;
                    continue;
                }
                word.push(ch);
                pos += 1;
                ch = chars[pos];
            }
            word.push(ch);
            pos += 1;
            continue;
        }
        if ch == quote {
            phase = Phase::Word;
        }
        if ch == escape && quote != '\'' {
            if pos + 1 == n {
                phase = Phase::Word;
                pos += 1;
                continue;
            }
            pos += 1;
            word.push(ch);
            ch = chars[pos];
        }
        word.push(ch);
        pos += 1;
    }
    words
}

/// ENV and LABEL: either `KEY value...` or `KEY=value ...`, as triples of
/// key, value and `=` (empty for the first form).
fn parse_name_val(rest: &str, key: &str, escape: char) -> Result<Option<Box<Node>>, Error> {
    let words = parse_words(rest, escape);
    let Some(first) = words.first() else {
        return Ok(None);
    };
    if !first.contains('=') {
        let parts = split_whitespace(rest, 2);
        if parts.len() < 2 {
            return Err(Error::new(format!("{key} must have two arguments")));
        }
        return Ok(node_chain([parts[0].as_str(), parts[1].as_str(), ""]));
    }
    let mut values = Vec::with_capacity(words.len() * 3);
    for word in &words {
        let Some((k, v)) = word.split_once('=') else {
            return Err(Error::new(format!(
                "Syntax error - can't find = in {}. Must be of the form: name=value",
                quote(word)
            )));
        };
        values.extend([k, v, "="]);
    }
    Ok(node_chain(values))
}

/// ARG: words that are names or name=value pairs.
fn parse_name_or_name_val(rest: &str, escape: char) -> Option<Box<Node>> {
    node_chain(parse_words(rest, escape))
}

fn parse_strings_whitespace_delimited(rest: &str) -> Option<Box<Node>> {
    if rest.is_empty() {
        return None;
    }
    node_chain(split_whitespace(rest, -1))
}

fn parse_string(rest: &str) -> Option<Box<Node>> {
    if rest.is_empty() {
        return None;
    }
    Some(Box::new(Node::with_value(rest)))
}

enum JsonArrayError {
    NotJsonArray,
    NotStringArray,
    Invalid,
}

fn parse_json(rest: &str) -> Result<Option<Box<Node>>, JsonArrayError> {
    let rest = rest.trim_start();
    if !rest.starts_with('[') {
        return Err(JsonArrayError::NotJsonArray);
    }
    let Some(Document::Array(items)) = json::decode(rest) else {
        return Err(JsonArrayError::Invalid);
    };
    let values: Option<Vec<String>> = items.into_iter().collect();
    values.map(node_chain).ok_or(JsonArrayError::NotStringArray)
}

/// A JSON array of strings, or else the whole text as one argument.
fn parse_maybe_json(rest: &str) -> LineParse {
    if rest.is_empty() {
        return Ok((None, false));
    }
    match parse_json(rest) {
        Ok(node) => Ok((node, true)),
        Err(JsonArrayError::NotStringArray) => Err(Error::new(ERR_NOT_STRING_ARRAY)),
        Err(JsonArrayError::NotJsonArray | JsonArrayError::Invalid) => {
            Ok((Some(Box::new(Node::with_value(rest))), false))
        }
    }
}

/// A JSON array of strings, or else whitespace separated words.
fn parse_maybe_json_to_list(rest: &str) -> LineParse {
    match parse_json(rest) {
        Ok(node) => Ok((node, true)),
        Err(JsonArrayError::NotStringArray) => Err(Error::new(ERR_NOT_STRING_ARRAY)),
        Err(JsonArrayError::NotJsonArray | JsonArrayError::Invalid) => {
            Ok((parse_strings_whitespace_delimited(rest), false))
        }
    }
}

/// HEALTHCHECK: a type word, then a command as for CMD.
fn parse_health_config(rest: &str) -> LineParse {
    let b = rest.as_bytes();
    let sep = b.iter().position(|&c| is_space_byte(c)).unwrap_or(b.len());
    let next = b[sep..]
        .iter()
        .position(|&c| !is_space_byte(c))
        .map_or(b.len(), |i| sep + i);
    if sep == 0 {
        return Ok((None, false));
    }
    let typ = String::from_utf8_lossy(&b[..sep]);
    let (cmd, json) = parse_maybe_json(&String::from_utf8_lossy(&b[next..]))?;
    let mut node = Node::with_value(typ);
    node.next = cmd;
    Ok((Some(Box::new(node)), json))
}

struct EmptyEnvs;

impl EnvGetter for EmptyEnvs {
    fn get(&self, _key: &str) -> Option<&str> {
        None
    }

    fn keys(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `^(\d*)<<(-?)\s*([^<]*)$`: the file descriptor digits, the chomp dash
/// and the delimiter word.
fn match_heredoc(src: &str) -> Option<(&str, bool, &str)> {
    let digits = src.len() - src.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let rest = src[digits..].strip_prefix("<<")?;
    let (chomp, rest) = match rest.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let rest = rest.trim_start_matches(is_re2_space);
    if rest.contains('<') {
        return None;
    }
    Some((&src[..digits], chomp, rest))
}

/// Reads a heredoc word such as `<<EOF`, `<<-'EOF'` or `3<<EOF`. `None`
/// when the word is not one.
pub fn parse_heredoc(src: &str) -> Result<Option<Heredoc>, Error> {
    let Some((fd, chomp, rest)) = match_heredoc(src) else {
        return Ok(None);
    };
    // ParseUint errors are ignored: no digits give 0, too many the largest
    // value.
    let file_descriptor = if fd.is_empty() {
        0
    } else {
        fd.parse::<u64>().unwrap_or(u64::MAX)
    };
    if rest.is_empty() {
        return Ok(None);
    }

    let mut shlex = Lex::new('\\');
    shlex.skip_unset_env = true;

    // Lexing with and without quotes tells whether part of the word is
    // quoted, which disables expansion of the content.
    shlex.raw_quotes = false;
    let words = shlex.process_words(rest, &EmptyEnvs)?;
    if words.len() != 1 {
        return Ok(None);
    }
    shlex.raw_quotes = true;
    let words_raw = shlex.process_words(rest, &EmptyEnvs)?;
    if words_raw.len() != words.len() {
        return Err(Error::new(format!(
            "internal lexing of heredoc produced inconsistent results: {rest}"
        )));
    }

    let quotes = |w: &str| w.matches(['\'', '"']).count();
    let word = &words[0];
    let expand = quotes(word) == quotes(&words_raw[0]);
    Ok(Some(Heredoc {
        name: word.clone(),
        file_descriptor,
        expand,
        chomp,
        content: String::new(),
    }))
}

/// [`parse_heredoc`] with the error discarded.
pub fn must_parse_heredoc(src: &str) -> Option<Heredoc> {
    parse_heredoc(src).ok().flatten()
}

fn heredocs_from_line(line: &str) -> Result<Vec<Heredoc>, Error> {
    let mut shlex = Lex::new('\\');
    shlex.raw_quotes = true;
    shlex.raw_escapes = true;
    shlex.skip_unset_env = true;
    let words = shlex.process_words(line, &EmptyEnvs).unwrap_or_default();
    let mut docs = Vec::new();
    for word in &words {
        if let Some(heredoc) = parse_heredoc(word)? {
            docs.push(heredoc);
        }
    }
    Ok(docs)
}

/// Strips the leading tabs of every line (`<<-`).
pub fn chomp_heredoc_content(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut at_line_start = true;
    for c in src.chars() {
        if at_line_start && c == '\t' {
            continue;
        }
        at_line_start = c == '\n';
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::dockerfile::{testdata, Position};

    fn dirs(dir: &str) -> Vec<std::path::PathBuf> {
        let mut out: Vec<_> = std::fs::read_dir(testdata(dir))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn parse_error_cases() {
        let dirs = dirs("parser/testfiles-negative");
        assert_eq!(dirs.len(), 4);
        for dir in dirs {
            let dockerfile = std::fs::read(dir.join("Dockerfile")).unwrap();
            assert!(parse(&dockerfile).is_err(), "{}", dir.display());
        }
    }

    #[test]
    fn parse_cases() {
        let dirs = dirs("parser/testfiles");
        assert_eq!(dirs.len(), 33);
        for dir in dirs {
            let dockerfile = std::fs::read(dir.join("Dockerfile")).unwrap();
            let result = std::fs::read_to_string(dir.join("result")).unwrap();
            let parsed = parse(&dockerfile).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            assert_eq!(parsed.ast.dump() + "\n", result, "{}", dir.display());
        }
    }

    #[test]
    fn parse_words_cases() {
        let cases: [(&str, &[&str]); 8] = [
            ("foo", &["foo"]),
            ("foo bar", &["foo", "bar"]),
            ("foo\\ bar", &["foo\\ bar"]),
            ("foo=bar", &["foo=bar"]),
            ("foo bar 'abc xyz'", &["foo", "bar", "'abc xyz'"]),
            ("foo bar \"abc xyz\"", &["foo", "bar", "\"abc xyz\""]),
            ("\u{e0}\u{f6}\u{fb}", &["\u{e0}\u{f6}\u{fb}"]),
            (
                "f\u{f6}o b\u{e0}r \"\u{e2}bc x\u{ff}z\"",
                &["f\u{f6}o", "b\u{e0}r", "\"\u{e2}bc x\u{ff}z\""],
            ),
        ];
        for (input, expect) in cases {
            assert_eq!(parse_words(input, DEFAULT_ESCAPE_TOKEN), expect, "{input}");
        }
    }

    #[test]
    fn parse_includes_line_numbers() {
        let df = std::fs::read(testdata("parser/testfile-line/Dockerfile")).unwrap();
        let ast = parse(&df).unwrap().ast;
        assert_eq!(ast.start_line, 5);
        assert_eq!(ast.end_line, 31);
        assert_eq!(ast.children.len(), 3);
        let expected = [(5, 5), (11, 12), (17, 31)];
        for (i, child) in ast.children.iter().enumerate() {
            assert_eq!((child.start_line, child.end_line), expected[i], "child {i}");
        }
    }

    #[test]
    fn parse_warns_on_empty_continuation_line() {
        let dockerfile = "
FROM alpine:3.6

RUN valid \\
    continuation

RUN something \\

    following \\

    more

RUN another \\

    thing

RUN non-indented \\
# this is a comment
   after-comment

RUN indented \\
    # this is an indented comment
    comment
\t";
        let result = parse(dockerfile.as_bytes()).unwrap();
        let warnings = &result.warnings;
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0]
            .short
            .contains("Empty continuation line found in"));
        assert!(warnings[0]
            .short
            .contains("RUN something     following     more"));
        assert!(warnings[1].short.contains("RUN another     thing"));
        assert!(warnings[0].detail[0].contains("will become errors in a future release"));
        let mut out = Vec::new();
        result.print_warnings(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.starts_with("[WARNING]: Empty continuation line found in: RUN something"));
        assert!(out.ends_with(
            "[WARNING]: Empty continuation lines will become errors in a future release.\n"
        ));
    }

    #[test]
    fn parse_returns_scanner_errors() {
        let label = "a".repeat(MAX_SCAN_TOKEN_SIZE);
        let dockerfile = format!("\n\t\tFROM image\n\t\tLABEL test={label}\n");
        let err = parse(dockerfile.as_bytes()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "dockerfile line greater than max allowed size of 65535"
        );

        // The limit counts the line ending, and an unterminated last line
        // must stay below it.
        let fits = format!("FROM {}\n", "a".repeat(MAX_SCAN_TOKEN_SIZE - 6));
        assert!(parse(fits.as_bytes()).is_ok());
        let unterminated = format!("FROM {}", "a".repeat(MAX_SCAN_TOKEN_SIZE - 5));
        assert!(parse(unterminated.as_bytes()).is_err());
        let unterminated = format!("FROM {}", "a".repeat(MAX_SCAN_TOKEN_SIZE - 6));
        assert!(parse(unterminated.as_bytes()).is_ok());
    }

    #[test]
    fn parse_name_val_old_format() {
        let node = parse_name_val("foo bar", "LABEL", '\0').unwrap().unwrap();
        let expected = node_chain(["foo", "bar", ""]).unwrap();
        assert_eq!(node, expected);
    }

    #[test]
    fn parse_name_val_new_format() {
        let node = parse_name_val("foo=bar thing=star", "LABEL", '\0')
            .unwrap()
            .unwrap();
        let expected = node_chain(["foo", "bar", "=", "thing", "star", "="]).unwrap();
        assert_eq!(node, expected);
    }

    #[test]
    fn parse_name_val_without_val() {
        let err = parse_name_val("foo", "ENV", '\0').unwrap_err();
        assert_eq!(err.to_string(), "ENV must have two arguments");
        let err = parse_name_val("a=b c", "ENV", '\\').unwrap_err();
        assert_eq!(
            err.to_string(),
            "Syntax error - can't find = in \"c\". Must be of the form: name=value"
        );
    }

    #[test]
    fn json_arrays_of_strings() {
        let invalid = [
            r#"["a",42,"b"]"#,
            r#"["a",123.456,"b"]"#,
            r#"["a",{},"b"]"#,
            r#"["a",{"c": "d"},"b"]"#,
            r#"["a",["c"],"b"]"#,
            r#"["a",true,"b"]"#,
            r#"["a",false,"b"]"#,
            r#"["a",null,"b"]"#,
        ];
        let valid: [(&str, &[&str]); 8] = [
            ("[]", &[]),
            (r#"[""]"#, &[""]),
            (r#"["a"]"#, &["a"]),
            (r#"["a","b"]"#, &["a", "b"]),
            (r#"[ "a", "b" ]"#, &["a", "b"]),
            ("[\t\"a\",\t\"b\"\t]", &["a", "b"]),
            ("\t[\t\"a\",\t\"b\"\t]\t", &["a", "b"]),
            (
                "[\"abc 123\", \"\u{2665}\", \"\u{2603}\", \"\\\" \\\\ \\/ \\b \\f \\n \\r \\t \\u0000\"]",
                &["abc 123", "\u{2665}", "\u{2603}", "\" \\ / \u{8} \u{c} \n \r \t \u{0}"],
            ),
        ];
        for (json, expected) in valid {
            let Ok(node) = parse_json(json) else {
                panic!("{json:?} should be a valid JSON array of strings");
            };
            let values: Vec<&str> = node
                .as_deref()
                .map(|n| {
                    std::iter::once(n)
                        .chain(n.next_nodes())
                        .map(|n| n.value.as_str())
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(values, expected, "{json:?}");
        }
        for json in invalid {
            assert!(
                matches!(parse_json(json), Err(JsonArrayError::NotStringArray)),
                "{json:?} should be an invalid JSON array of strings"
            );
        }
    }

    #[test]
    fn parse_extracts_heredoc() {
        let dockerfile = "
FROM alpine:3.6

ENV NAME=me

RUN ls

USER <<INVALID
INVALID

RUN <<EMPTY
EMPTY

RUN 3<<EMPTY2
EMPTY2

RUN \"<<NOHEREDOC\"

RUN <<INDENT
\tfoo
\tbar
INDENT

RUN <<-UNINDENT
\tbaz
\tquux
UNINDENT

RUN <<-UNINDENT2
\tbaz
\tquux
\tUNINDENT2

RUN <<-EXPAND
\texpand $NAME
EXPAND

RUN <<-'NOEXPAND'
\tdon't expand $NAME
NOEXPAND

RUN <<COPY
echo hello world
echo foo bar
COPY

RUN <<COMMENT
# internal comment
echo hello world
echo foo bar # trailing comment
COMMENT

RUN --mount=type=cache,target=/foo <<MOUNT
echo hello
MOUNT

COPY <<FILE1 <<FILE2 /dest
content 1
FILE1
content 2
FILE2

COPY <<EOF /quotes
\"foo\"
'bar'
EOF

COPY <<X <<Y /dest
Y
X
X
Y

RUN <<COMPLEX python3
print('hello world')
COMPLEX

COPY <<file.txt /dest
hello world
file.txt

RUN <<eo'f'
echo foo
eof

RUN <<eo\\'f
echo foo
eo'f

RUN <<'e'o\\'f
echo foo
eo'f

RUN <<'one two'
echo bar
one two

RUN <<$EOF
$EOF

RUN <<  EOF
EOF

RUN <<  EOF  > foo
EOF
\t";
        let h = |name: &str, content: &str, expand: bool, chomp: bool, fd: u64| Heredoc {
            name: name.into(),
            content: content.into(),
            expand,
            chomp,
            file_descriptor: fd,
        };
        let tests: Vec<Vec<Heredoc>> = vec![
            vec![],
            vec![],
            vec![],
            vec![],
            vec![h("EMPTY", "", true, false, 0)],
            vec![h("EMPTY2", "", true, false, 3)],
            vec![],
            vec![h("INDENT", "\tfoo\n\tbar\n", true, false, 0)],
            vec![h("UNINDENT", "\tbaz\n\tquux\n", true, true, 0)],
            vec![h("UNINDENT2", "\tbaz\n\tquux\n", true, true, 0)],
            vec![h("EXPAND", "\texpand $NAME\n", true, true, 0)],
            vec![h("NOEXPAND", "\tdon't expand $NAME\n", false, true, 0)],
            vec![h(
                "COPY",
                "echo hello world\necho foo bar\n",
                true,
                false,
                0,
            )],
            vec![h(
                "COMMENT",
                "# internal comment\necho hello world\necho foo bar # trailing comment\n",
                true,
                false,
                0,
            )],
            vec![h("MOUNT", "echo hello\n", true, false, 0)],
            vec![
                h("FILE1", "content 1\n", true, false, 0),
                h("FILE2", "content 2\n", true, false, 0),
            ],
            vec![h("EOF", "\"foo\"\n'bar'\n", true, false, 0)],
            vec![h("X", "Y\n", true, false, 0), h("Y", "X\n", true, false, 0)],
            vec![h("COMPLEX", "print('hello world')\n", true, false, 0)],
            vec![h("file.txt", "hello world\n", true, false, 0)],
            vec![h("eof", "echo foo\n", false, false, 0)],
            vec![h("eo'f", "echo foo\n", true, false, 0)],
            vec![h("eo'f", "echo foo\n", false, false, 0)],
            vec![h("one two", "echo bar\n", false, false, 0)],
            vec![h("$EOF", "", true, false, 0)],
            vec![h("EOF", "", true, false, 0)],
            vec![h("EOF", "", true, false, 0)],
        ];
        let result = parse(dockerfile.as_bytes()).unwrap();
        for (i, test) in tests.iter().enumerate() {
            assert_eq!(
                &result.ast.children[i + 1].heredocs,
                test,
                "child {}",
                i + 1
            );
        }
    }

    #[test]
    fn parse_json_heredoc() {
        let dockerfile = "
FROM alpine:3.6

RUN [\"whoami\"]
RUN [\"<<EOF\"]
RUN [\"<<'EOF'\"]
\t";
        let result = parse(dockerfile.as_bytes()).unwrap();
        for i in 1..=3 {
            assert!(result.ast.children[i].heredocs.is_empty());
        }
    }

    #[test]
    fn heredoc_chomp() {
        assert_eq!(
            chomp_heredoc_content("\thello\n\tworld\n"),
            "hello\nworld\n"
        );
        assert_eq!(chomp_heredoc_content("\t\ta\tb\n\n\t"), "a\tb\n\n");
    }

    #[test]
    fn parse_heredoc_helpers() {
        let valid = [
            "<<EOF",
            "<<'EOF'",
            "<<\"EOF\"",
            "<<-EOF",
            "<<-'EOF'",
            "<<-\"EOF\"",
            "<<EO\"F\"",
            "<< EOF",
            "<<- EOF",
        ];
        let invalid = ["<<'EOF", "<<\"EOF", "<<EOF'", "<<EOF\""];
        let not_heredocs = ["", "EOF", "<<", "<<-", "<EOF", "<<<EOF", "<<EOF sh"];
        for src in not_heredocs {
            assert_eq!(parse_heredoc(src).unwrap(), None, "{src:?}");
        }
        for src in valid {
            assert_eq!(parse_heredoc(src).unwrap().unwrap().name, "EOF", "{src:?}");
        }
        for src in invalid {
            assert!(parse_heredoc(src).is_err(), "{src:?}");
        }
        assert_eq!(
            parse_heredoc("99999999999999999999<<EOF")
                .unwrap()
                .unwrap()
                .file_descriptor,
            u64::MAX
        );
    }

    #[test]
    fn heredocs_from_line_cases() {
        let cases: [(&str, &[&str]); 10] = [
            ("RUN <<EOF", &["EOF"]),
            ("RUN <<-EOF", &["EOF"]),
            ("RUN <<EOF", &["EOF"]),
            ("RUN <<- EOF", &["EOF"]),
            ("RUN << -EOF", &["-EOF"]),
            ("RUN <<'EOF'", &["EOF"]),
            ("RUN 4<<EOF", &["EOF"]),
            ("RUN <<EOF <<EOF2", &["EOF", "EOF2"]),
            ("RUN '<<EOF'", &[]),
            ("RUN \"<<EOF\"", &[]),
        ];
        // As in the reference, only the heredocs found are compared.
        for (line, names) in cases {
            let heredocs = heredocs_from_line(line).unwrap();
            for (i, heredoc) in heredocs.iter().enumerate() {
                assert_eq!(heredoc.name, names[i], "{line}");
            }
        }
        // The space after <<- splits it from the word, so the reference
        // finds no heredoc in that line.
        assert!(heredocs_from_line("RUN <<- EOF").unwrap().is_empty());
        let names = |line: &str| -> Vec<String> {
            heredocs_from_line(line)
                .unwrap()
                .into_iter()
                .map(|h| h.name)
                .collect()
        };
        assert_eq!(names("RUN << -EOF"), ["-EOF"]);
        assert_eq!(names("RUN <<EOF <<EOF2"), ["EOF", "EOF2"]);
        assert!(names("RUN '<<EOF'").is_empty());
    }

    #[test]
    fn directives() {
        let dt = "#escape=\\\n# key = FOO bar\n\n# smth\n";
        let (d, err) = DirectiveParser::default().parse_all(dt.as_bytes());
        assert!(err.is_none());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "escape");
        assert_eq!(d[0].value, "\\");

        // The directive names are case-insensitive.
        let dt = "# EScape=\\\n# KEY = FOO bar\n\n# smth\n";
        let (d, err) = DirectiveParser::default().parse_all(dt.as_bytes());
        assert!(err.is_none());
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "escape");
        assert_eq!(d[0].value, "\\");

        let (d, err) = DirectiveParser::default().parse_all(b"# syntax=a\n# syntax=b\n");
        assert_eq!(d.len(), 1);
        assert_eq!(
            err.unwrap().to_string(),
            "only one syntax parser directive can be used"
        );
    }

    #[test]
    fn directive_values() {
        assert_eq!(match_directive("escape= "), Some(("escape", " ")));
        assert_eq!(match_directive("escape = ` "), Some(("escape", "`")));
        assert_eq!(match_directive("a=b c  "), Some(("a", "b c")));
        assert_eq!(match_directive("escape="), None);
        assert_eq!(match_directive("1a=b"), None);
        assert_eq!(match_directive("a b=c"), None);
        let err = parse(b"# escape= \nFROM x").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid escape token ' ' does not match ` or \\"
        );
        assert_eq!(err.locations(), &[to_ranges(0, 0)]);
    }

    fn line(n: usize) -> Range {
        Range {
            start: Position {
                line: n,
                character: 0,
            },
            end: Position {
                line: n,
                character: 0,
            },
        }
    }

    #[test]
    fn detect_syntax_cases() {
        let dt = "# syntax = dockerfile:experimental // opts\nFROM busybox\n";
        let found = detect_syntax(dt.as_bytes()).unwrap();
        assert_eq!(found.value, "dockerfile:experimental");
        assert_eq!(found.cmdline, "dockerfile:experimental // opts");
        assert_eq!(found.location, vec![line(1)]);

        let dt = "#!/bin/sh\n# syntax = dockerfile:experimental\nFROM busybox\n";
        let found = detect_syntax(dt.as_bytes()).unwrap();
        assert_eq!(found.value, "dockerfile:experimental");
        assert_eq!(found.location, vec![line(2)]);

        let dt = "#!/bin/sh\n\n# syntax = dockerfile:experimental\n";
        assert_eq!(detect_syntax(dt.as_bytes()), None);

        assert_eq!(detect_syntax(b"FROM busybox\nRUN ls\n"), None);

        assert_eq!(
            detect_syntax(b"//syntax=foo\n//key=value").unwrap().value,
            "foo"
        );
        assert_eq!(detect_syntax(b"#!/bin/sh\n//syntax=x").unwrap().value, "x");
        assert_eq!(
            detect_syntax(br#"{"syntax": "foo", "bar": ["abc"]}"#)
                .unwrap()
                .value,
            "foo"
        );
        assert_eq!(detect_syntax(br#"{"syntax": "foo""#), None);
        assert_eq!(detect_syntax(b"{\"syntax\": \"foo\"}\n# syntax=bar"), None);
    }

    #[test]
    fn detect_syntax_bom() {
        let mut dt = vec![0xEF, 0xBB, 0xBF];
        dt.extend_from_slice(b"# syntax = myfrontend\nFROM busybox\n");
        let found = detect_syntax(&dt).unwrap();
        assert_eq!(found.value, "myfrontend");
        assert_eq!(found.cmdline, "myfrontend");
        assert_eq!(found.location, vec![line(1)]);
    }

    #[test]
    fn parse_directive_cases() {
        let dt = "#check = skip=all // opts\nFROM busybox\n";
        let found = parse_directive("check", dt.as_bytes()).unwrap();
        assert_eq!(found.value, "skip=all");
        assert_eq!(found.cmdline, "skip=all // opts");
        assert_eq!(found.location, vec![line(1)]);

        let dt = "#!/bin/sh\n# check = skip=all\nFROM busybox\n";
        let found = parse_directive("check", dt.as_bytes()).unwrap();
        assert_eq!(found.value, "skip=all");
        assert_eq!(found.location, vec![line(2)]);

        assert_eq!(
            parse_directive("check", b"#!/bin/sh\n\n# check = skip=all\n"),
            None
        );
        assert_eq!(parse_directive("check", b"FROM busybox\nRUN ls\n"), None);
        assert_eq!(
            parse_directive("check", b"//check=skip=all\n//key=value"),
            None
        );
    }

    #[test]
    fn parse_collects_directives_and_escape() {
        let parsed = parse(b"# syntax=x/y\n# escape=`\nFROM a\nRUN b `\n  c\n").unwrap();
        assert_eq!(parsed.escape_token, '`');
        let names: Vec<&str> = parsed.directives.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["syntax", "escape"]);
        assert_eq!(parsed.directives[1].location, vec![line(2)]);
        assert_eq!(parsed.ast.dump(), "(from \"a\")\n(run \"b   c\")");
        assert!(parsed.ast.children[0].prev_comment.is_empty());
    }

    #[test]
    fn error_locations() {
        let err = parse(b"FROM a\nCMD [\"a\", 1]\n").unwrap_err();
        assert_eq!(err.to_string(), ERR_NOT_STRING_ARRAY);
        assert_eq!(err.locations(), &[to_ranges(2, 2)]);

        let err = parse(b"FROM a\nRUN <<EOF\nnever closed\n").unwrap_err();
        assert_eq!(err.to_string(), "unterminated heredoc");
        assert_eq!(err.locations(), &[to_ranges(2, 3)]);

        let err = parse(b"# only a comment\n").unwrap_err();
        assert_eq!(err.to_string(), "file with no instructions");
        assert_eq!(err.locations(), &[to_ranges(1, 1)]);
    }

    #[test]
    fn comments_and_original() {
        // Blank lines do not reset the comments, a bare # does.
        let parsed = parse(b"# one\n  # two\n\n# three\nRUN a \\\n  b\n").unwrap();
        let run = &parsed.ast.children[0];
        assert_eq!(
            run.prev_comment,
            vec!["one".to_string(), "# two".into(), "three".into()]
        );
        assert_eq!(run.original, "RUN a   b");
        let parsed = parse(b"# one\n#\n# two\nRUN a\nRUN b\n").unwrap();
        assert_eq!(parsed.ast.children[0].prev_comment, vec!["two".to_string()]);
        assert!(parsed.ast.children[1].prev_comment.is_empty());
    }

    #[test]
    fn builder_flags_keep_the_reference_byte_handling() {
        let parsed = parse("COPY --from=\u{e9} --chown='a b' x y\n".as_bytes()).unwrap();
        let copy = &parsed.ast.children[0];
        assert_eq!(
            copy.flags,
            vec!["--from=\u{c3}\u{a9}".to_string(), "--chown=a b".into()]
        );
        let parsed = parse(b"COPY --a=\"x\" -- --b y\n").unwrap();
        assert_eq!(
            parsed.ast.children[0].dump(),
            "copy [\"--a=x\"] \"--b\" \"y\""
        );
    }

    #[test]
    fn unknown_instruction_and_onbuild_nodes() {
        let parsed = parse(b"FOO bar\nONBUILD\nONBUILD ONBUILD RUN [\"a\"]\n").unwrap();
        assert_eq!(
            parsed.ast.dump(),
            "(foo \"\")\n(onbuild)\n(onbuild (onbuild (run \"a\")))"
        );
        let inner = &parsed.ast.children[2].next.as_ref().unwrap().children[0];
        assert_eq!(inner.original, "ONBUILD RUN [\"a\"]");
        let run = &inner.next.as_ref().unwrap().children[0];
        assert!(run.json);
    }

    #[test]
    fn deep_nesting_and_long_chains_do_not_recurse() {
        let line = format!("{}RUN x\n", "ONBUILD ".repeat(MAX_ONBUILD_LEVELS));
        let parsed = parse(line.as_bytes()).unwrap();
        let copy = parsed.clone();
        assert_eq!(copy, parsed);
        drop(copy);
        let line = format!("{}RUN x\n", "ONBUILD ".repeat(100_000));
        assert!(parse(line.as_bytes()).is_err());
        let line = format!("EXPOSE {}\n", "1 ".repeat(30000));
        let parsed = parse(line.as_bytes()).unwrap();
        assert_eq!(parsed.ast.children[0].next_nodes().count(), 30000);
        let copy = parsed.clone();
        assert_eq!(copy, parsed);
    }

    #[test]
    fn split_whitespace_like_regexp_split() {
        assert_eq!(split_whitespace("", 2), vec![String::new()]);
        assert_eq!(split_whitespace("a", 2), vec!["a".to_string()]);
        assert_eq!(
            split_whitespace("a  b c", 2),
            vec!["a".to_string(), "b c".into()]
        );
        assert_eq!(
            split_whitespace(" a b ", -1),
            vec![String::new(), "a".into(), "b".into(), String::new()]
        );
        assert_eq!(split_whitespace("a\nb", -1), vec!["a\nb".to_string()]);
    }

    #[test]
    fn continuation_character() {
        assert_eq!(
            trim_continuation_character("a \\", '\\'),
            ("a ".to_string(), false)
        );
        assert_eq!(
            trim_continuation_character("a\\  \t", '\\'),
            ("a".to_string(), false)
        );
        assert_eq!(
            trim_continuation_character("\\ ", '\\'),
            (String::new(), false)
        );
        assert_eq!(
            trim_continuation_character("a\\\\", '\\'),
            ("a\\\\".to_string(), true)
        );
        assert_eq!(
            trim_continuation_character("a `", '`'),
            ("a ".to_string(), false)
        );
        assert_eq!(
            trim_continuation_character("a \\", '`'),
            ("a \\".to_string(), true)
        );
    }
}
