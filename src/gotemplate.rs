//! Go's text/template evaluated over JSON values, with the template functions
//! and the `--format` handling of lists of docker/cli's templates and formatter
//! packages, the reference below.
//!
//! `ps --format` and `inspect -f` take the formats people already write, so the
//! lexer, the parser, the evaluator, the printing and the error messages follow
//! Go 1.26's text/template, fmt and strconv. Data is seen the way Go sees the
//! result of json.Unmarshal into an interface{}, except that JSON integers are
//! Go ints instead of float64: the reference evaluates these formats over typed
//! structs, where sizes and pids are integers, and Go prints a float64 of
//! 1234567 as 1.234567e+06 and refuses `eq .Pid 0`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

use serde_json::Value as Json;

/// Go allows 100000 nested template calls and 10000 nested parentheses, on
/// stacks that grow; these limits keep the recursion inside a 2 MiB thread
/// stack even in a debug build.
const MAX_EXEC_DEPTH: usize = 100;
const MAX_PAREN_DEPTH: usize = 100;
/// Controls (if, with, range) nested in a template, and nested as it runs across the
/// templates it calls: each level is a few frames of the parser's and the executor's
/// recursion, which Go's growing stacks take and a thread's does not.
const MAX_CONTROL_DEPTH: usize = 100;
const MAX_RUN_DEPTH: usize = 1000;

// ---------------------------------------------------------------------------
// Lexer (text/template/parse/lex.go)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum It {
    Error,
    Bool,
    Char,
    CharConstant,
    Comment,
    Complex,
    Assign,
    Declare,
    Eof,
    Field,
    Identifier,
    LeftDelim,
    LeftParen,
    Number,
    Pipe,
    RawString,
    RightDelim,
    RightParen,
    Space,
    Str,
    Text,
    Variable,
    // Keywords.
    Block,
    Break,
    Continue,
    Dot,
    Define,
    Else,
    End,
    If,
    Nil,
    Range,
    Template,
    With,
}

impl It {
    fn is_keyword(self) -> bool {
        matches!(
            self,
            It::Block
                | It::Break
                | It::Continue
                | It::Dot
                | It::Define
                | It::Else
                | It::End
                | It::If
                | It::Nil
                | It::Range
                | It::Template
                | It::With
        )
    }
}

#[derive(Clone, Debug)]
struct Item {
    typ: It,
    pos: usize,
    val: String,
    line: usize,
}

impl Item {
    fn zero() -> Item {
        Item {
            typ: It::Error,
            pos: 0,
            val: String::new(),
            line: 0,
        }
    }

    fn describe(&self) -> String {
        match self.typ {
            It::Eof => "EOF".to_string(),
            It::Error => self.val.clone(),
            t if t.is_keyword() => format!("<{}>", self.val),
            _ if self.val.len() > 10 => {
                let short: String = self.val.chars().take(10).collect();
                format!("{}...", quote_with(&short, '"', false))
            }
            _ => quote_with(&self.val, '"', false),
        }
    }
}

#[derive(Clone, Copy)]
enum St {
    Text,
    LeftDelim,
    Comment,
    RightDelim,
    InsideAction,
    Space,
    Identifier,
    Field,
    Variable,
    Char,
    Number,
    Quote,
    RawQuote,
}

const LEFT_DELIM: &str = "{{";
const RIGHT_DELIM: &str = "}}";
const SPACE_CHARS: &[char] = &[' ', '\t', '\r', '\n'];

struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    start: usize,
    at_eof: bool,
    paren_depth: i64,
    line: usize,
    start_line: usize,
    item: Item,
    inside_action: bool,
}

fn is_space(r: char) -> bool {
    matches!(r, ' ' | '\t' | '\r' | '\n')
}

fn is_alnum(r: char) -> bool {
    r == '_' || r.is_alphabetic() || r.is_numeric()
}

fn has_left_trim_marker(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0] == b'-' && is_space(b[1] as char)
}

fn has_right_trim_marker(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && is_space(b[0] as char) && b[1] == b'-'
}

fn count_nl(s: &str) -> usize {
    s.matches('\n').count()
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Lexer<'a> {
        Lexer {
            input,
            pos: 0,
            start: 0,
            at_eof: false,
            paren_depth: 0,
            line: 1,
            start_line: 1,
            item: Item::zero(),
            inside_action: false,
        }
    }

    fn next(&mut self) -> Option<char> {
        match self.input.get(self.pos..).and_then(|s| s.chars().next()) {
            Some(c) => {
                self.pos += c.len_utf8();
                if c == '\n' {
                    self.line += 1;
                }
                Some(c)
            }
            None => {
                self.at_eof = true;
                None
            }
        }
    }

    fn peek(&mut self) -> Option<char> {
        let r = self.next();
        self.backup();
        r
    }

    fn backup(&mut self) {
        if !self.at_eof && self.pos > 0 {
            if let Some(c) = self.input[..self.pos].chars().next_back() {
                self.pos -= c.len_utf8();
                if c == '\n' {
                    self.line -= 1;
                }
            }
        }
    }

    fn this_item(&mut self, typ: It) -> Item {
        let i = Item {
            typ,
            pos: self.start,
            val: self.input[self.start..self.pos].to_string(),
            line: self.start_line,
        };
        self.start = self.pos;
        self.start_line = self.line;
        i
    }

    fn emit(&mut self, typ: It) -> Option<St> {
        self.item = self.this_item(typ);
        None
    }

    fn ignore(&mut self) {
        self.line += count_nl(&self.input[self.start..self.pos]);
        self.start = self.pos;
        self.start_line = self.line;
    }

    fn accept(&mut self, valid: &str) -> bool {
        match self.next() {
            Some(c) if valid.contains(c) => true,
            _ => {
                self.backup();
                false
            }
        }
    }

    fn accept_run(&mut self, valid: &str) {
        while let Some(c) = self.next() {
            if !valid.contains(c) {
                break;
            }
        }
        self.backup();
    }

    fn errorf(&mut self, msg: String) -> Option<St> {
        self.item = Item {
            typ: It::Error,
            pos: self.start,
            val: msg,
            line: self.start_line,
        };
        self.start = 0;
        self.pos = 0;
        self.input = "";
        None
    }

    fn next_item(&mut self) -> Item {
        self.item = Item {
            typ: It::Eof,
            pos: self.pos,
            val: "EOF".to_string(),
            line: self.start_line,
        };
        let mut state = if self.inside_action {
            St::InsideAction
        } else {
            St::Text
        };
        while let Some(next) = self.step(state) {
            state = next;
        }
        self.item.clone()
    }

    fn step(&mut self, state: St) -> Option<St> {
        match state {
            St::Text => self.lex_text(),
            St::LeftDelim => self.lex_left_delim(),
            St::Comment => self.lex_comment(),
            St::RightDelim => self.lex_right_delim(),
            St::InsideAction => self.lex_inside_action(),
            St::Space => self.lex_space(),
            St::Identifier => self.lex_identifier(),
            St::Field => self.lex_field_or_variable(It::Field),
            St::Variable => {
                if self.at_terminator() {
                    return self.emit(It::Variable);
                }
                self.lex_field_or_variable(It::Variable)
            }
            St::Char => self.lex_char(),
            St::Number => self.lex_number(),
            St::Quote => self.lex_quote(),
            St::RawQuote => self.lex_raw_quote(),
        }
    }

    fn lex_text(&mut self) -> Option<St> {
        if let Some(x) = self.input[self.pos..].find(LEFT_DELIM) {
            if x > 0 {
                self.pos += x;
                let mut trim = 0;
                let delim_end = self.pos + LEFT_DELIM.len();
                if has_left_trim_marker(&self.input[delim_end..]) {
                    let s = &self.input[self.start..self.pos];
                    trim = s.len() - s.trim_end_matches(SPACE_CHARS).len();
                }
                self.pos -= trim;
                self.line += count_nl(&self.input[self.start..self.pos]);
                let i = self.this_item(It::Text);
                self.pos += trim;
                self.ignore();
                if !i.val.is_empty() {
                    self.item = i;
                    return None;
                }
            }
            return Some(St::LeftDelim);
        }
        self.pos = self.input.len();
        if self.pos > self.start {
            self.line += count_nl(&self.input[self.start..self.pos]);
            return self.emit(It::Text);
        }
        self.emit(It::Eof)
    }

    fn at_right_delim(&self) -> (bool, bool) {
        let rest = &self.input[self.pos..];
        if has_right_trim_marker(rest) && rest[2..].starts_with(RIGHT_DELIM) {
            return (true, true);
        }
        (rest.starts_with(RIGHT_DELIM), false)
    }

    fn left_trim_length(s: &str) -> usize {
        s.len() - s.trim_start_matches(SPACE_CHARS).len()
    }

    fn lex_left_delim(&mut self) -> Option<St> {
        self.pos += LEFT_DELIM.len();
        let trim = has_left_trim_marker(&self.input[self.pos..]);
        let after = if trim { 2 } else { 0 };
        if self.input[self.pos + after..].starts_with("/*") {
            self.pos += after;
            self.ignore();
            return Some(St::Comment);
        }
        let i = self.this_item(It::LeftDelim);
        self.inside_action = true;
        self.pos += after;
        self.ignore();
        self.paren_depth = 0;
        self.item = i;
        None
    }

    fn lex_comment(&mut self) -> Option<St> {
        self.pos += 2;
        match self.input[self.pos..].find("*/") {
            None => return self.errorf("unclosed comment".to_string()),
            Some(x) => self.pos += x + 2,
        }
        let (delim, trim) = self.at_right_delim();
        if !delim {
            return self.errorf("comment ends before closing delimiter".to_string());
        }
        self.line += count_nl(&self.input[self.start..self.pos]);
        self.this_item(It::Comment);
        if trim {
            self.pos += 2;
        }
        self.pos += RIGHT_DELIM.len();
        if trim {
            self.pos += Self::left_trim_length(&self.input[self.pos..]);
        }
        self.ignore();
        Some(St::Text)
    }

    fn lex_right_delim(&mut self) -> Option<St> {
        let (_, trim) = self.at_right_delim();
        if trim {
            self.pos += 2;
            self.ignore();
        }
        self.pos += RIGHT_DELIM.len();
        let i = self.this_item(It::RightDelim);
        if trim {
            self.pos += Self::left_trim_length(&self.input[self.pos..]);
            self.ignore();
        }
        self.inside_action = false;
        self.item = i;
        None
    }

    fn lex_inside_action(&mut self) -> Option<St> {
        let (delim, _) = self.at_right_delim();
        if delim {
            if self.paren_depth == 0 {
                return Some(St::RightDelim);
            }
            return self.errorf("unclosed left paren".to_string());
        }
        match self.next() {
            None => self.errorf("unclosed action".to_string()),
            Some(r) if is_space(r) => {
                self.backup();
                Some(St::Space)
            }
            Some('=') => self.emit(It::Assign),
            Some(':') => {
                if self.next() != Some('=') {
                    return self.errorf("expected :=".to_string());
                }
                self.emit(It::Declare)
            }
            Some('|') => self.emit(It::Pipe),
            Some('"') => Some(St::Quote),
            Some('`') => Some(St::RawQuote),
            Some('$') => Some(St::Variable),
            Some('\'') => Some(St::Char),
            Some('.') => {
                if let Some(&b) = self.input.as_bytes().get(self.pos) {
                    if !b.is_ascii_digit() {
                        return Some(St::Field);
                    }
                }
                self.backup();
                Some(St::Number)
            }
            Some(r) if r == '+' || r == '-' || r.is_ascii_digit() => {
                self.backup();
                Some(St::Number)
            }
            Some(r) if is_alnum(r) => {
                self.backup();
                Some(St::Identifier)
            }
            Some('(') => {
                self.paren_depth += 1;
                self.emit(It::LeftParen)
            }
            Some(')') => {
                self.paren_depth -= 1;
                if self.paren_depth < 0 {
                    return self.errorf("unexpected right paren".to_string());
                }
                self.emit(It::RightParen)
            }
            Some(r) if r.is_ascii() && !r.is_ascii_control() => self.emit(It::Char),
            Some(r) => self.errorf(format!("unrecognized character in action: {}", sharp_u(r))),
        }
    }

    fn lex_space(&mut self) -> Option<St> {
        let mut num = 0;
        while let Some(r) = self.peek() {
            if !is_space(r) {
                break;
            }
            self.next();
            num += 1;
        }
        if has_right_trim_marker(&self.input[self.pos - 1..])
            && self.input[self.pos + 1..].starts_with(RIGHT_DELIM)
        {
            self.backup();
            if num == 1 {
                return Some(St::RightDelim);
            }
        }
        self.emit(It::Space)
    }

    fn lex_identifier(&mut self) -> Option<St> {
        loop {
            match self.next() {
                Some(r) if is_alnum(r) => {}
                r => {
                    self.backup();
                    if !self.at_terminator() {
                        let r = r.unwrap_or('\u{FFFD}');
                        return self.errorf(format!("bad character {}", sharp_u(r)));
                    }
                    let word = &self.input[self.start..self.pos];
                    let typ = match word {
                        "." => It::Dot,
                        "block" => It::Block,
                        "break" => It::Break,
                        "continue" => It::Continue,
                        "define" => It::Define,
                        "else" => It::Else,
                        "end" => It::End,
                        "if" => It::If,
                        "range" => It::Range,
                        "nil" => It::Nil,
                        "template" => It::Template,
                        "with" => It::With,
                        "true" | "false" => It::Bool,
                        _ => It::Identifier,
                    };
                    return self.emit(typ);
                }
            }
        }
    }

    fn lex_field_or_variable(&mut self, typ: It) -> Option<St> {
        if self.at_terminator() {
            return self.emit(if typ == It::Variable {
                It::Variable
            } else {
                It::Dot
            });
        }
        let mut last;
        loop {
            last = self.next();
            match last {
                Some(r) if is_alnum(r) => {}
                _ => {
                    self.backup();
                    break;
                }
            }
        }
        if !self.at_terminator() {
            let r = last.unwrap_or('\u{FFFD}');
            return self.errorf(format!("bad character {}", sharp_u(r)));
        }
        self.emit(typ)
    }

    fn at_terminator(&mut self) -> bool {
        match self.peek() {
            None => true,
            Some(r) if is_space(r) => true,
            Some('.' | ',' | '|' | ':' | ')' | '(') => true,
            _ => self.input[self.pos..].starts_with(RIGHT_DELIM),
        }
    }

    fn lex_char(&mut self) -> Option<St> {
        loop {
            match self.next() {
                Some('\\') => match self.next() {
                    Some(r) if r != '\n' => {}
                    _ => return self.errorf("unterminated character constant".to_string()),
                },
                None | Some('\n') => {
                    return self.errorf("unterminated character constant".to_string())
                }
                Some('\'') => break,
                _ => {}
            }
        }
        self.emit(It::CharConstant)
    }

    fn lex_number(&mut self) -> Option<St> {
        if !self.scan_number() {
            let s = quote_with(&self.input[self.start..self.pos], '"', false);
            return self.errorf(format!("bad number syntax: {}", s));
        }
        if let Some('+' | '-') = self.peek() {
            if !self.scan_number() || self.input.as_bytes().get(self.pos - 1) != Some(&b'i') {
                let s = quote_with(&self.input[self.start..self.pos], '"', false);
                return self.errorf(format!("bad number syntax: {}", s));
            }
            return self.emit(It::Complex);
        }
        self.emit(It::Number)
    }

    fn scan_number(&mut self) -> bool {
        self.accept("+-");
        let mut digits = "0123456789_";
        if self.accept("0") {
            if self.accept("xX") {
                digits = "0123456789abcdefABCDEF_";
            } else if self.accept("oO") {
                digits = "01234567_";
            } else if self.accept("bB") {
                digits = "01_";
            }
        }
        self.accept_run(digits);
        if self.accept(".") {
            self.accept_run(digits);
        }
        if digits.len() == 11 && self.accept("eE") {
            self.accept("+-");
            self.accept_run("0123456789_");
        }
        if digits.len() == 23 && self.accept("pP") {
            self.accept("+-");
            self.accept_run("0123456789_");
        }
        self.accept("i");
        if self.peek().is_some_and(is_alnum) {
            self.next();
            return false;
        }
        true
    }

    fn lex_quote(&mut self) -> Option<St> {
        loop {
            match self.next() {
                Some('\\') => match self.next() {
                    Some(r) if r != '\n' => {}
                    _ => return self.errorf("unterminated quoted string".to_string()),
                },
                None | Some('\n') => return self.errorf("unterminated quoted string".to_string()),
                Some('"') => break,
                _ => {}
            }
        }
        self.emit(It::Str)
    }

    fn lex_raw_quote(&mut self) -> Option<St> {
        loop {
            match self.next() {
                None => return self.errorf("unterminated raw quoted string".to_string()),
                Some('`') => break,
                _ => {}
            }
        }
        self.emit(It::RawString)
    }
}

/// fmt's %#U: U+0023 '#'.
fn sharp_u(r: char) -> String {
    let mut s = format!("U+{:04X}", r as u32);
    if is_print(r) {
        let _ = write!(s, " '{}'", r);
    }
    s
}

// ---------------------------------------------------------------------------
// Parse tree (text/template/parse/node.go)

#[derive(Debug)]
struct List {
    pos: usize,
    nodes: Vec<Node>,
}

#[derive(Debug)]
struct Branch {
    pipe: Pipe,
    list: List,
    else_list: Option<List>,
}

#[derive(Debug)]
enum Node {
    Text {
        pos: usize,
        text: String,
    },
    Action {
        pos: usize,
        pipe: Pipe,
    },
    If(Branch),
    Range(Branch),
    With(Branch),
    Template {
        pos: usize,
        name: String,
        pipe: Option<Pipe>,
    },
    Break {
        pos: usize,
    },
    Continue {
        pos: usize,
    },
    // Only while parsing.
    End {
        pos: usize,
    },
    Else {
        pos: usize,
    },
}

#[derive(Debug)]
struct Pipe {
    pos: usize,
    is_assign: bool,
    decl: Vec<Var>,
    cmds: Vec<Cmd>,
}

#[derive(Debug, Clone)]
struct Var {
    pos: usize,
    ident: Vec<String>,
}

impl Var {
    fn new(pos: usize, ident: &str) -> Var {
        Var {
            pos,
            ident: ident.split('.').map(str::to_string).collect(),
        }
    }
}

#[derive(Debug)]
struct Cmd {
    pos: usize,
    args: Vec<Arg>,
}

#[derive(Debug, Clone, Default)]
struct Number {
    text: String,
    is_int: bool,
    is_uint: bool,
    is_float: bool,
    is_complex: bool,
    int: i64,
    float: f64,
    complex: (f64, f64),
}

#[derive(Debug)]
enum Arg {
    Field {
        pos: usize,
        ident: Vec<String>,
    },
    Chain {
        pos: usize,
        node: Box<Arg>,
        field: Vec<String>,
    },
    Ident {
        pos: usize,
        name: String,
    },
    Dot {
        pos: usize,
    },
    Nil {
        pos: usize,
    },
    Bool {
        pos: usize,
        val: bool,
    },
    Number {
        pos: usize,
        num: Number,
    },
    Str {
        pos: usize,
        quoted: String,
        text: String,
    },
    Var(Var),
    Pipe(Box<Pipe>),
}

impl Arg {
    fn pos(&self) -> usize {
        match self {
            Arg::Field { pos, .. }
            | Arg::Chain { pos, .. }
            | Arg::Ident { pos, .. }
            | Arg::Dot { pos }
            | Arg::Nil { pos }
            | Arg::Bool { pos, .. }
            | Arg::Number { pos, .. }
            | Arg::Str { pos, .. } => *pos,
            Arg::Var(v) => v.pos,
            Arg::Pipe(p) => p.pos,
        }
    }
}

/// A node an error can point at, as Go's state.node.
#[derive(Clone, Copy)]
enum NodeRef<'t> {
    Node(&'t Node),
    List(&'t List),
    Pipe(&'t Pipe),
    Cmd(&'t Cmd),
    Arg(&'t Arg),
}

impl NodeRef<'_> {
    fn pos(&self) -> usize {
        match self {
            NodeRef::Node(n) => match n {
                Node::Text { pos, .. }
                | Node::Action { pos, .. }
                | Node::Template { pos, .. }
                | Node::Break { pos }
                | Node::Continue { pos }
                | Node::End { pos }
                | Node::Else { pos } => *pos,
                Node::If(b) | Node::Range(b) | Node::With(b) => b.pipe.pos,
            },
            NodeRef::List(l) => l.pos,
            NodeRef::Pipe(p) => p.pos,
            NodeRef::Cmd(c) => c.pos,
            NodeRef::Arg(a) => a.pos(),
        }
    }

    fn render(&self) -> String {
        let mut s = String::new();
        match self {
            NodeRef::Node(n) => write_node(n, &mut s),
            NodeRef::List(l) => write_list(l, &mut s),
            NodeRef::Pipe(p) => write_pipe(p, &mut s),
            NodeRef::Cmd(c) => write_cmd(c, &mut s),
            NodeRef::Arg(a) => write_arg(a, &mut s),
        }
        s
    }
}

fn write_list(l: &List, s: &mut String) {
    for n in &l.nodes {
        write_node(n, s);
    }
}

fn write_node(n: &Node, s: &mut String) {
    match n {
        Node::Text { text, .. } => s.push_str(text),
        Node::Action { pipe, .. } => {
            s.push_str("{{");
            write_pipe(pipe, s);
            s.push_str("}}");
        }
        Node::If(b) => write_branch("if", b, s),
        Node::Range(b) => write_branch("range", b, s),
        Node::With(b) => write_branch("with", b, s),
        Node::Template { name, pipe, .. } => {
            s.push_str("{{template ");
            s.push_str(&quote_with(name, '"', false));
            if let Some(p) = pipe {
                s.push(' ');
                write_pipe(p, s);
            }
            s.push_str("}}");
        }
        Node::Break { .. } => s.push_str("{{break}}"),
        Node::Continue { .. } => s.push_str("{{continue}}"),
        Node::End { .. } => s.push_str("{{end}}"),
        Node::Else { .. } => s.push_str("{{else}}"),
    }
}

fn write_branch(name: &str, b: &Branch, s: &mut String) {
    s.push_str("{{");
    s.push_str(name);
    s.push(' ');
    write_pipe(&b.pipe, s);
    s.push_str("}}");
    write_list(&b.list, s);
    if let Some(e) = &b.else_list {
        s.push_str("{{else}}");
        write_list(e, s);
    }
    s.push_str("{{end}}");
}

fn write_pipe(p: &Pipe, s: &mut String) {
    if !p.decl.is_empty() {
        for (i, v) in p.decl.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&v.ident.join("."));
        }
        s.push_str(if p.is_assign { " = " } else { " := " });
    }
    for (i, c) in p.cmds.iter().enumerate() {
        if i > 0 {
            s.push_str(" | ");
        }
        write_cmd(c, s);
    }
}

fn write_cmd(c: &Cmd, s: &mut String) {
    for (i, a) in c.args.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        if let Arg::Pipe(p) = a {
            s.push('(');
            write_pipe(p, s);
            s.push(')');
        } else {
            write_arg(a, s);
        }
    }
}

fn write_arg(a: &Arg, s: &mut String) {
    match a {
        Arg::Field { ident, .. } => {
            for id in ident {
                s.push('.');
                s.push_str(id);
            }
        }
        Arg::Chain { node, field, .. } => {
            if let Arg::Pipe(p) = node.as_ref() {
                s.push('(');
                write_pipe(p, s);
                s.push(')');
            } else {
                write_arg(node, s);
            }
            for f in field {
                s.push('.');
                s.push_str(f);
            }
        }
        Arg::Ident { name, .. } => s.push_str(name),
        Arg::Dot { .. } => s.push('.'),
        Arg::Nil { .. } => s.push_str("nil"),
        Arg::Bool { val, .. } => s.push_str(if *val { "true" } else { "false" }),
        Arg::Number { num, .. } => s.push_str(&num.text),
        Arg::Str { quoted, .. } => s.push_str(quoted),
        Arg::Var(v) => s.push_str(&v.ident.join(".")),
        Arg::Pipe(p) => write_pipe(p, s),
    }
}

fn arg_string(a: &Arg) -> String {
    NodeRef::Arg(a).render()
}

fn is_empty_list(l: &List) -> bool {
    l.nodes.iter().all(|n| match n {
        Node::Text { text, .. } => text.trim().is_empty(),
        _ => false,
    })
}

// ---------------------------------------------------------------------------
// Parser (text/template/parse/parse.go)

/// The per-tree state of Go's parse.Tree: a define or block starts a new one
/// that reads from the same lexer.
struct TreeState {
    token: [Item; 3],
    peek_count: usize,
    vars: Vec<String>,
    action_line: usize,
    range_depth: usize,
    stack_depth: usize,
    control_depth: usize,
}

impl TreeState {
    fn new() -> TreeState {
        TreeState {
            token: [Item::zero(), Item::zero(), Item::zero()],
            peek_count: 0,
            vars: vec!["$".to_string()],
            action_line: 0,
            range_depth: 0,
            stack_depth: 0,
            control_depth: 0,
        }
    }
}

type PResult<T> = Result<T, String>;

struct Parser<'a> {
    lex: Lexer<'a>,
    t: TreeState,
    parse_name: String,
    trees: BTreeMap<String, List>,
}

impl Parser<'_> {
    fn errorf(&self, msg: &str) -> String {
        format!(
            "template: {}:{}: {}",
            self.parse_name, self.t.token[0].line, msg
        )
    }

    fn next(&mut self) -> Item {
        if self.t.peek_count > 0 {
            self.t.peek_count -= 1;
        } else {
            self.t.token[0] = self.lex.next_item();
        }
        self.t.token[self.t.peek_count].clone()
    }

    fn backup(&mut self) {
        self.t.peek_count += 1;
    }

    fn backup2(&mut self, t1: Item) {
        self.t.token[1] = t1;
        self.t.peek_count = 2;
    }

    fn backup3(&mut self, t2: Item, t1: Item) {
        self.t.token[1] = t1;
        self.t.token[2] = t2;
        self.t.peek_count = 3;
    }

    fn peek(&mut self) -> Item {
        if self.t.peek_count > 0 {
            return self.t.token[self.t.peek_count - 1].clone();
        }
        self.t.peek_count = 1;
        self.t.token[0] = self.lex.next_item();
        self.t.token[0].clone()
    }

    fn next_non_space(&mut self) -> Item {
        loop {
            let t = self.next();
            if t.typ != It::Space {
                return t;
            }
        }
    }

    fn peek_non_space(&mut self) -> Item {
        let t = self.next_non_space();
        self.backup();
        t
    }

    fn expect(&mut self, expected: It, context: &str) -> PResult<Item> {
        let t = self.next_non_space();
        if t.typ != expected {
            return Err(self.unexpected(&t, context));
        }
        Ok(t)
    }

    fn unexpected(&self, token: &Item, context: &str) -> String {
        if token.typ == It::Error {
            let mut extra = String::new();
            if self.t.action_line != 0 && self.t.action_line != token.line {
                extra = format!(
                    " in action started at {}:{}",
                    self.parse_name, self.t.action_line
                );
                if token.val.ends_with(" action") {
                    extra = extra[" in action".len()..].to_string();
                }
            }
            return self.errorf(&format!("{}{}", token.describe(), extra));
        }
        self.errorf(&format!("unexpected {} in {}", token.describe(), context))
    }

    /// Adds a finished tree the way parse.Tree.add does: an empty tree never
    /// replaces a defined one, two non-empty ones clash.
    fn add(&mut self, name: String, root: List, line: usize) -> PResult<()> {
        match self.trees.get(&name) {
            Some(old) if !is_empty_list(old) => {
                if !is_empty_list(&root) {
                    return Err(format!(
                        "template: {}:{}: template: multiple definition of template {}",
                        self.parse_name,
                        line,
                        quote_with(&name, '"', false)
                    ));
                }
            }
            _ => {
                self.trees.insert(name, root);
            }
        }
        Ok(())
    }

    fn parse(&mut self) -> PResult<List> {
        let pos = self.peek().pos;
        let mut root = List { pos, nodes: vec![] };
        while self.peek().typ != It::Eof {
            if self.peek().typ == It::LeftDelim {
                let delim = self.next();
                if self.next_non_space().typ == It::Define {
                    let saved = std::mem::replace(&mut self.t, TreeState::new());
                    let r = self.parse_definition();
                    self.t = saved;
                    r?;
                    continue;
                }
                self.backup2(delim);
            }
            let n = self.text_or_action()?;
            match n {
                Node::End { .. } | Node::Else { .. } => {
                    let mut s = String::new();
                    write_node(&n, &mut s);
                    return Err(self.errorf(&format!("unexpected {}", s)));
                }
                _ => root.nodes.push(n),
            }
        }
        Ok(root)
    }

    fn parse_definition(&mut self) -> PResult<()> {
        let context = "define clause";
        let token = self.next_non_space();
        if token.typ != It::Str && token.typ != It::RawString {
            return Err(self.unexpected(&token, context));
        }
        let name = unquote(&token.val).map_err(|e| self.errorf(e))?;
        self.expect(It::RightDelim, context)?;
        let (list, end) = self.item_list()?;
        if !matches!(end, Node::End { .. }) {
            let mut s = String::new();
            write_node(&end, &mut s);
            return Err(self.errorf(&format!("unexpected {} in {}", s, context)));
        }
        let line = self.t.token[0].line;
        self.add(name, list, line)
    }

    fn item_list(&mut self) -> PResult<(List, Node)> {
        let pos = self.peek_non_space().pos;
        let mut list = List { pos, nodes: vec![] };
        while self.peek_non_space().typ != It::Eof {
            let n = self.text_or_action()?;
            match n {
                Node::End { .. } | Node::Else { .. } => return Ok((list, n)),
                _ => list.nodes.push(n),
            }
        }
        Err(self.errorf("unexpected EOF"))
    }

    fn text_or_action(&mut self) -> PResult<Node> {
        let token = self.next_non_space();
        match token.typ {
            It::Text => Ok(Node::Text {
                pos: token.pos,
                text: token.val,
            }),
            It::LeftDelim => {
                self.t.action_line = token.line;
                let r = self.action();
                self.t.action_line = 0;
                r
            }
            _ => Err(self.unexpected(&token, "input")),
        }
    }

    fn action(&mut self) -> PResult<Node> {
        let token = self.next_non_space();
        match token.typ {
            It::Block => return self.block_control(),
            It::Break | It::Continue => return self.break_control(&token),
            It::Else => return self.else_control(),
            It::End => {
                let t = self.expect(It::RightDelim, "end")?;
                return Ok(Node::End { pos: t.pos });
            }
            It::If => return self.if_control(),
            It::Range => {
                let b = self.parse_control("range")?;
                return Ok(Node::Range(b));
            }
            It::Template => return self.template_control(),
            It::With => return self.with_control(),
            _ => {}
        }
        self.backup();
        let token = self.peek();
        let pipe = self.pipeline("command", It::RightDelim)?;
        Ok(Node::Action {
            pos: token.pos,
            pipe,
        })
    }

    fn break_control(&mut self, kw: &Item) -> PResult<Node> {
        let name = if kw.typ == It::Break {
            "break"
        } else {
            "continue"
        };
        let token = self.next_non_space();
        if token.typ != It::RightDelim {
            return Err(self.unexpected(&token, &format!("{{{{{}}}}}", name)));
        }
        if self.t.range_depth == 0 {
            return Err(self.errorf(&format!("{{{{{}}}}} outside {{{{range}}}}", name)));
        }
        Ok(if kw.typ == It::Break {
            Node::Break { pos: kw.pos }
        } else {
            Node::Continue { pos: kw.pos }
        })
    }

    fn pipeline(&mut self, context: &str, end: It) -> PResult<Pipe> {
        let token = self.peek_non_space();
        let mut pipe = Pipe {
            pos: token.pos,
            is_assign: false,
            decl: vec![],
            cmds: vec![],
        };
        'decls: loop {
            let v = self.peek_non_space();
            if v.typ == It::Variable {
                self.next();
                let token_after = self.peek();
                let next = self.peek_non_space();
                if next.typ == It::Assign || next.typ == It::Declare {
                    pipe.is_assign = next.typ == It::Assign;
                    self.next_non_space();
                    pipe.decl.push(Var::new(v.pos, &v.val));
                    self.t.vars.push(v.val.clone());
                } else if next.typ == It::Char && next.val == "," {
                    self.next_non_space();
                    pipe.decl.push(Var::new(v.pos, &v.val));
                    self.t.vars.push(v.val.clone());
                    if context == "range" && pipe.decl.len() < 2 {
                        match self.peek_non_space().typ {
                            It::Variable | It::RightDelim | It::RightParen => continue 'decls,
                            _ => return Err(self.errorf("range can only initialize variables")),
                        }
                    }
                    return Err(self.errorf(&format!("too many declarations in {}", context)));
                } else if token_after.typ == It::Space {
                    self.backup3(v, token_after);
                } else {
                    self.backup2(v);
                }
            }
            break;
        }
        loop {
            let token = self.next_non_space();
            match token.typ {
                t if t == end => {
                    self.check_pipeline(&pipe, context)?;
                    return Ok(pipe);
                }
                It::Bool
                | It::CharConstant
                | It::Complex
                | It::Dot
                | It::Field
                | It::Identifier
                | It::Number
                | It::Nil
                | It::RawString
                | It::Str
                | It::Variable
                | It::LeftParen => {
                    self.backup();
                    let cmd = self.command()?;
                    pipe.cmds.push(cmd);
                }
                _ => return Err(self.unexpected(&token, context)),
            }
        }
    }

    fn check_pipeline(&self, pipe: &Pipe, context: &str) -> PResult<()> {
        if pipe.cmds.is_empty() {
            return Err(self.errorf(&format!("missing value for {}", context)));
        }
        for (i, c) in pipe.cmds.iter().enumerate().skip(1) {
            if matches!(
                c.args.first(),
                Some(
                    Arg::Bool { .. }
                        | Arg::Dot { .. }
                        | Arg::Nil { .. }
                        | Arg::Number { .. }
                        | Arg::Str { .. }
                )
            ) {
                return Err(self.errorf(&format!(
                    "non executable command in pipeline stage {}",
                    i + 1
                )));
            }
        }
        Ok(())
    }

    fn parse_control(&mut self, context: &str) -> PResult<Branch> {
        if self.t.control_depth >= MAX_CONTROL_DEPTH {
            return Err(self.errorf("max nesting depth exceeded"));
        }
        self.t.control_depth += 1;
        let branch = self.parse_control_body(context);
        self.t.control_depth -= 1;
        branch
    }

    fn parse_control_body(&mut self, context: &str) -> PResult<Branch> {
        let vars = self.t.vars.len();
        let pipe = self.pipeline(context, It::RightDelim)?;
        if context == "range" {
            self.t.range_depth += 1;
        }
        let (list, next) = self.item_list()?;
        if context == "range" {
            self.t.range_depth -= 1;
        }
        let mut else_list = None;
        if let Node::Else { pos } = next {
            // {{else if}} and {{else with}} nest the next control in the else
            // list; its {{end}} closes both.
            if context == "if" && self.peek().typ == It::If {
                self.next();
                let n = self.if_control()?;
                else_list = Some(List {
                    pos,
                    nodes: vec![n],
                });
            } else if context == "with" && self.peek().typ == It::With {
                self.next();
                let n = self.with_control()?;
                else_list = Some(List {
                    pos,
                    nodes: vec![n],
                });
            } else {
                let (el, next) = self.item_list()?;
                if !matches!(next, Node::End { .. }) {
                    let mut s = String::new();
                    write_node(&next, &mut s);
                    return Err(self.errorf(&format!("expected end; found {}", s)));
                }
                else_list = Some(el);
            }
        }
        self.t.vars.truncate(vars);
        Ok(Branch {
            pipe,
            list,
            else_list,
        })
    }

    fn if_control(&mut self) -> PResult<Node> {
        Ok(Node::If(self.parse_control("if")?))
    }

    fn with_control(&mut self) -> PResult<Node> {
        Ok(Node::With(self.parse_control("with")?))
    }

    fn else_control(&mut self) -> PResult<Node> {
        let peek = self.peek_non_space();
        if peek.typ == It::If || peek.typ == It::With {
            return Ok(Node::Else { pos: peek.pos });
        }
        let t = self.expect(It::RightDelim, "else")?;
        Ok(Node::Else { pos: t.pos })
    }

    fn block_control(&mut self) -> PResult<Node> {
        let context = "block clause";
        let token = self.next_non_space();
        let name = self.parse_template_name(&token, context)?;
        let pipe = self.pipeline(context, It::RightDelim)?;
        let saved = std::mem::replace(&mut self.t, TreeState::new());
        let r = self.item_list();
        let block = std::mem::replace(&mut self.t, saved);
        let (list, end) = r?;
        if !matches!(end, Node::End { .. }) {
            let mut s = String::new();
            write_node(&end, &mut s);
            return Err(self.errorf(&format!("unexpected {} in {}", s, context)));
        }
        self.add(name.clone(), list, block.token[0].line)?;
        Ok(Node::Template {
            pos: token.pos,
            name,
            pipe: Some(pipe),
        })
    }

    fn template_control(&mut self) -> PResult<Node> {
        let context = "template clause";
        let token = self.next_non_space();
        let name = self.parse_template_name(&token, context)?;
        let mut pipe = None;
        if self.next_non_space().typ != It::RightDelim {
            self.backup();
            pipe = Some(self.pipeline(context, It::RightDelim)?);
        }
        Ok(Node::Template {
            pos: token.pos,
            name,
            pipe,
        })
    }

    fn parse_template_name(&self, token: &Item, context: &str) -> PResult<String> {
        match token.typ {
            It::Str | It::RawString => unquote(&token.val).map_err(|e| self.errorf(e)),
            _ => Err(self.unexpected(token, context)),
        }
    }

    fn command(&mut self) -> PResult<Cmd> {
        let pos = self.peek_non_space().pos;
        let mut cmd = Cmd { pos, args: vec![] };
        loop {
            self.peek_non_space();
            if let Some(op) = self.operand()? {
                cmd.args.push(op);
            }
            let token = self.next();
            match token.typ {
                It::Space => continue,
                It::RightDelim | It::RightParen => self.backup(),
                It::Pipe => {}
                _ => return Err(self.unexpected(&token, "operand")),
            }
            break;
        }
        if cmd.args.is_empty() {
            return Err(self.errorf("empty command"));
        }
        Ok(cmd)
    }

    fn operand(&mut self) -> PResult<Option<Arg>> {
        let node = match self.term()? {
            None => return Ok(None),
            Some(n) => n,
        };
        if self.peek().typ != It::Field {
            return Ok(Some(node));
        }
        let pos = self.peek().pos;
        let mut fields = vec![];
        while self.peek().typ == It::Field {
            let f = self.next().val;
            fields.push(f[1..].to_string());
        }
        Ok(Some(match node {
            Arg::Field { mut ident, .. } => {
                ident.extend(fields);
                Arg::Field { pos, ident }
            }
            Arg::Var(mut v) => {
                v.ident.extend(fields);
                Arg::Var(Var {
                    pos,
                    ident: v.ident,
                })
            }
            Arg::Bool { .. }
            | Arg::Str { .. }
            | Arg::Number { .. }
            | Arg::Nil { .. }
            | Arg::Dot { .. } => {
                let s = quote_with(&arg_string(&node), '"', false);
                return Err(self.errorf(&format!("unexpected . after term {}", s)));
            }
            other => Arg::Chain {
                pos,
                node: Box::new(other),
                field: fields,
            },
        }))
    }

    fn term(&mut self) -> PResult<Option<Arg>> {
        let token = self.next_non_space();
        let pos = token.pos;
        Ok(Some(match token.typ {
            It::Identifier => {
                if lookup_func(&token.val, false).is_none() {
                    let q = quote_with(&token.val, '"', false);
                    return Err(self.errorf(&format!("function {} not defined", q)));
                }
                Arg::Ident {
                    pos,
                    name: token.val,
                }
            }
            It::Dot => Arg::Dot { pos },
            It::Nil => Arg::Nil { pos },
            It::Variable => {
                let v = Var::new(pos, &token.val);
                if !self.t.vars.contains(&v.ident[0]) {
                    let q = quote_with(&v.ident[0], '"', false);
                    return Err(self.errorf(&format!("undefined variable {}", q)));
                }
                Arg::Var(v)
            }
            It::Field => Arg::Field {
                pos,
                ident: token.val[1..].split('.').map(str::to_string).collect(),
            },
            It::Bool => Arg::Bool {
                pos,
                val: token.val == "true",
            },
            It::CharConstant | It::Complex | It::Number => {
                let num = new_number(&token.val, token.typ).map_err(|e| self.errorf(&e))?;
                Arg::Number { pos, num }
            }
            It::LeftParen => {
                if self.t.stack_depth >= MAX_PAREN_DEPTH {
                    return Err(self.errorf("max expression depth exceeded"));
                }
                self.t.stack_depth += 1;
                let r = self.pipeline("parenthesized pipeline", It::RightParen);
                self.t.stack_depth -= 1;
                Arg::Pipe(Box::new(r?))
            }
            It::Str | It::RawString => {
                let text = unquote(&token.val).map_err(|e| self.errorf(e))?;
                Arg::Str {
                    pos,
                    quoted: token.val,
                    text,
                }
            }
            _ => {
                self.backup();
                return Ok(None);
            }
        }))
    }
}

// ---------------------------------------------------------------------------
// Literals (strconv)

const ERR_SYNTAX: &str = "invalid syntax";

/// strconv.UnquoteChar: the value, whether it came from a multibyte form,
/// and the rest of the input.
fn unquote_char(s: &str, quote: u8) -> Result<(u32, bool, &str), &'static str> {
    let b = s.as_bytes();
    let c = *b.first().ok_or(ERR_SYNTAX)?;
    if c == quote && (quote == b'\'' || quote == b'"') {
        return Err(ERR_SYNTAX);
    }
    if c >= 0x80 {
        let r = s.chars().next().ok_or(ERR_SYNTAX)?;
        return Ok((r as u32, true, &s[r.len_utf8()..]));
    }
    if c != b'\\' {
        return Ok((c as u32, false, &s[1..]));
    }
    if b.len() <= 1 {
        return Err(ERR_SYNTAX);
    }
    let c = b[1];
    // A backslash before a character of several bytes escapes nothing Go knows.
    if c >= 0x80 {
        return Err(ERR_SYNTAX);
    }
    let mut rest = &s[2..];
    let mut multibyte = false;
    let value = match c {
        b'a' => 7,
        b'b' => 8,
        b'f' => 12,
        b'n' => 10,
        b'r' => 13,
        b't' => 9,
        b'v' => 11,
        b'x' | b'u' | b'U' => {
            let n = match c {
                b'x' => 2,
                b'u' => 4,
                _ => 8,
            };
            let digits = rest.get(..n).ok_or(ERR_SYNTAX)?;
            let v = u32::from_str_radix(digits, 16).map_err(|_| ERR_SYNTAX)?;
            if !digits.bytes().all(|d| d.is_ascii_hexdigit()) {
                return Err(ERR_SYNTAX);
            }
            rest = &rest[n..];
            if c != b'x' {
                if char::from_u32(v).is_none() {
                    return Err(ERR_SYNTAX);
                }
                multibyte = true;
            }
            v
        }
        b'0'..=b'7' => {
            let digits = rest.get(..2).ok_or(ERR_SYNTAX)?;
            let mut v = (c - b'0') as u32;
            for d in digits.bytes() {
                if !(b'0'..=b'7').contains(&d) {
                    return Err(ERR_SYNTAX);
                }
                v = (v << 3) | (d - b'0') as u32;
            }
            rest = &rest[2..];
            if v > 255 {
                return Err(ERR_SYNTAX);
            }
            v
        }
        b'\\' => b'\\' as u32,
        b'\'' | b'"' => {
            if c != quote {
                return Err(ERR_SYNTAX);
            }
            c as u32
        }
        _ => return Err(ERR_SYNTAX),
    };
    Ok((value, multibyte, rest))
}

/// strconv.Unquote for the string tokens the lexer produces.
fn unquote(s: &str) -> Result<String, &'static str> {
    let b = s.as_bytes();
    if b.len() < 2 || b[0] != b[b.len() - 1] {
        return Err(ERR_SYNTAX);
    }
    let inner = &s[1..s.len() - 1];
    match b[0] {
        b'`' => {
            if inner.contains('`') {
                return Err(ERR_SYNTAX);
            }
            Ok(inner.replace('\r', ""))
        }
        b'"' => {
            let mut out: Vec<u8> = Vec::with_capacity(inner.len());
            let mut rest = inner;
            while !rest.is_empty() {
                if rest.starts_with('\n') {
                    return Err(ERR_SYNTAX);
                }
                let (r, multibyte, tail) = unquote_char(rest, b'"')?;
                if r < 0x80 || !multibyte {
                    out.push(r as u8);
                } else if let Some(c) = char::from_u32(r) {
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                rest = tail;
            }
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        _ => Err(ERR_SYNTAX),
    }
}

/// strconv's check that underscores only separate digits.
fn underscore_ok(s: &str) -> bool {
    let mut b = s.as_bytes();
    if !b.is_empty() && (b[0] == b'-' || b[0] == b'+') {
        b = &b[1..];
    }
    let mut saw = b'^';
    let mut i = 0;
    let mut hex = false;
    if b.len() >= 2 && b[0] == b'0' && matches!(b[1] | 0x20, b'b' | b'o' | b'x') {
        i = 2;
        saw = b'0';
        hex = b[1] | 0x20 == b'x';
    }
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() || (hex && (b'a'..=b'f').contains(&(c | 0x20))) {
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

/// strconv.ParseUint(s, 0, 64).
fn parse_uint(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.is_empty() {
        return None;
    }
    let (base, digits) = if b[0] == b'0' {
        match b.get(1).map(|c| c | 0x20) {
            Some(b'b') if b.len() >= 3 => (2, &b[2..]),
            Some(b'o') if b.len() >= 3 => (8, &b[2..]),
            Some(b'x') if b.len() >= 3 => (16, &b[2..]),
            _ => (8, &b[1..]),
        }
    } else {
        (10, b)
    };
    let mut n: u64 = 0;
    let mut underscores = false;
    for &c in digits {
        let d = match c {
            b'_' => {
                underscores = true;
                continue;
            }
            b'0'..=b'9' => c - b'0',
            b'a'..=b'z' => c - b'a' + 10,
            b'A'..=b'Z' => c - b'A' + 10,
            _ => return None,
        } as u64;
        if d >= base {
            return None;
        }
        n = n.checked_mul(base)?.checked_add(d)?;
    }
    if underscores && !underscore_ok(s) {
        return None;
    }
    Some(n)
}

/// strconv.ParseInt(s, 0, 64).
fn parse_int(s: &str) -> Option<i64> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    let u = parse_uint(body)?;
    if neg {
        if u > 1 << 63 {
            return None;
        }
        Some((u as i64).wrapping_neg())
    } else {
        i64::try_from(u).ok()
    }
}

/// strconv.ParseFloat(s, 64), which also reads hexadecimal mantissas.
fn parse_float(s: &str) -> Option<f64> {
    let (neg, body) = match s.as_bytes().first() {
        Some(b'+') => (false, &s[1..]),
        Some(b'-') => (true, &s[1..]),
        _ => (false, s),
    };
    if body.contains('_') && !underscore_ok(s) {
        return None;
    }
    let clean: String = body.chars().filter(|&c| c != '_').collect();
    let b = clean.as_bytes();
    let v = if b.len() > 2 && b[0] == b'0' && b[1] | 0x20 == b'x' {
        parse_hex_float(&clean[2..])?
    } else {
        if !clean
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            return None;
        }
        clean.parse::<f64>().ok()?
    };
    if v.is_infinite() {
        return None;
    }
    Some(if neg { -v } else { v })
}

fn parse_hex_float(s: &str) -> Option<f64> {
    let (mant, exp) = s.split_once(['p', 'P'])?;
    // Wide enough that no exponent a number can be written with overflows; beyond a
    // few thousand the value is 0 or infinite anyway.
    let exp: i64 = exp.parse().ok()?;
    let mut m: u64 = 0;
    let mut e = exp;
    let mut seen_dot = false;
    let mut digits = false;
    let mut sticky = false;
    for c in mant.chars() {
        if c == '.' {
            if seen_dot {
                return None;
            }
            seen_dot = true;
            continue;
        }
        let d = c.to_digit(16)? as u64;
        digits = true;
        if m >> 60 == 0 {
            m = m * 16 + d;
            if seen_dot {
                e = e.saturating_sub(4);
            }
        } else {
            sticky |= d != 0;
            if !seen_dot {
                e = e.saturating_add(4);
            }
        }
    }
    if !digits {
        return None;
    }
    if sticky {
        m |= 1;
    }
    let mut e = e.clamp(-5000, 5000) as i32;
    let mut v = m as f64;
    while e > 0 {
        let step = e.min(1000);
        v *= 2f64.powi(step);
        e -= step;
    }
    while e < 0 {
        let step = e.max(-1000);
        v *= 2f64.powi(step);
        e -= step;
    }
    Some(v)
}

fn exact_i64(f: f64) -> Option<i64> {
    ((-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) && f.trunc() == f)
        .then_some(f as i64)
}

fn exact_u64(f: f64) -> bool {
    (0.0..18_446_744_073_709_551_616.0).contains(&f) && f.trunc() == f
}

/// parse.newNumber: a constant keeps every type that can represent it.
fn new_number(text: &str, typ: It) -> Result<Number, String> {
    let mut n = Number {
        text: text.to_string(),
        ..Number::default()
    };
    let simplify = |n: &mut Number| {
        n.is_float = n.complex.1 == 0.0;
        if n.is_float {
            n.float = n.complex.0;
            if let Some(i) = exact_i64(n.float) {
                n.is_int = true;
                n.int = i;
            }
            n.is_uint = exact_u64(n.float);
        }
    };
    match typ {
        It::CharConstant => {
            let (r, _, tail) = unquote_char(&text[1..], b'\'').map_err(str::to_string)?;
            if tail != "'" {
                return Err(format!("malformed character constant: {}", text));
            }
            n.int = r as i64;
            n.is_int = true;
            n.is_uint = true;
            n.float = r as f64;
            n.is_float = true;
            return Ok(n);
        }
        It::Complex => {
            // Split where the lexer did: after the first number.
            let mut l = Lexer::new(text);
            l.scan_number();
            let (re, im) = text.split_at(l.pos);
            let re = parse_float(re);
            let im = im.strip_suffix('i').and_then(parse_float);
            match (re, im) {
                (Some(re), Some(im)) => n.complex = (re, im),
                _ => {
                    return Err(format!(
                        "illegal number syntax: {}",
                        quote_with(text, '"', false)
                    ))
                }
            }
            n.is_complex = true;
            simplify(&mut n);
            return Ok(n);
        }
        _ => {}
    }
    if let Some(f) = text.strip_suffix('i').and_then(parse_float) {
        n.is_complex = true;
        n.complex = (0.0, f);
        simplify(&mut n);
        return Ok(n);
    }
    let u = parse_uint(text);
    if u.is_some() {
        n.is_uint = true;
    }
    if let Some(i) = parse_int(text) {
        n.is_int = true;
        n.int = i;
        if i == 0 {
            n.is_uint = true;
        }
    }
    if n.is_int {
        n.is_float = true;
        n.float = n.int as f64;
    } else if let Some(u) = u {
        n.is_float = true;
        n.float = u as f64;
    } else if let Some(f) = parse_float(text) {
        if !text.contains(['.', 'e', 'E', 'p', 'P']) {
            return Err(format!(
                "integer overflow: {}",
                quote_with(text, '"', false)
            ));
        }
        n.is_float = true;
        n.float = f;
        if let Some(i) = exact_i64(f) {
            n.is_int = true;
            n.int = i;
        }
        if exact_u64(f) {
            n.is_uint = true;
        }
    }
    if !n.is_int && !n.is_uint && !n.is_float {
        return Err(format!(
            "illegal number syntax: {}",
            quote_with(text, '"', false)
        ));
    }
    Ok(n)
}

/// strconv.IsPrint, without Go's tables of unassigned code points.
fn is_print(c: char) -> bool {
    let u = c as u32;
    if u < 0x80 {
        return (0x20..0x7f).contains(&u);
    }
    if c.is_control() || c.is_whitespace() {
        return false;
    }
    !matches!(u,
        0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x206F | 0xE000..=0xF8FF | 0xFEFF
        | 0xFFF9..=0xFFFB | 0xFFFE | 0xFFFF | 0x110BD | 0x110CD | 0x13430..=0x1343F
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
        | 0xF0000..=0x10FFFF)
}

/// strconv.Quote, QuoteToASCII and their single-rune forms.
fn quote_with(s: &str, quote: char, ascii_only: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for r in s.chars() {
        escape_rune(&mut out, r, quote, ascii_only);
    }
    out.push(quote);
    out
}

fn escape_rune(out: &mut String, r: char, quote: char, ascii_only: bool) {
    if r == quote || r == '\\' {
        out.push('\\');
        out.push(r);
        return;
    }
    if ascii_only {
        if r.is_ascii() && is_print(r) {
            out.push(r);
            return;
        }
    } else if is_print(r) {
        out.push(r);
        return;
    }
    match r {
        '\x07' => out.push_str("\\a"),
        '\x08' => out.push_str("\\b"),
        '\x0c' => out.push_str("\\f"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\x0b' => out.push_str("\\v"),
        _ => {
            let u = r as u32;
            if u < 0x20 || u == 0x7f {
                let _ = write!(out, "\\x{:02x}", u);
            } else if u < 0x10000 {
                let _ = write!(out, "\\u{:04x}", u);
            } else {
                let _ = write!(out, "\\U{:08x}", u);
            }
        }
    }
}

fn can_backquote(s: &str) -> bool {
    s.chars().all(|r| {
        if r.len_utf8() > 1 {
            return r != '\u{feff}';
        }
        !((r < ' ' && r != '\t') || r == '`' || r == '\u{7f}')
    })
}

// ---------------------------------------------------------------------------
// Values

/// The values a template sees, named after the Go types they stand for.
#[derive(Clone, Debug)]
enum V {
    /// The zero reflect.Value: a missing key or nil data.
    Invalid,
    /// A nil interface{}, JSON's null.
    Nil,
    Bool(bool),
    Int(i64),
    /// uint8, what indexing a string gives.
    Byte(u8),
    Float(f64),
    Complex(f64, f64),
    Str(String),
    /// map[string]interface {}, or map[string]string when the flag is set.
    Map(Rc<BTreeMap<String, V>>, bool),
    List(Rc<Vec<V>>),
    Strs(Rc<Vec<String>>),
}

impl V {
    fn type_name(&self) -> &'static str {
        match self {
            V::Invalid | V::Nil => "<nil>",
            V::Bool(_) => "bool",
            V::Int(_) => "int",
            V::Byte(_) => "uint8",
            V::Float(_) => "float64",
            V::Complex(..) => "complex128",
            V::Str(_) => "string",
            V::Map(_, false) => "map[string]interface {}",
            V::Map(_, true) => "map[string]string",
            V::List(_) => "[]interface {}",
            V::Strs(_) => "[]string",
        }
    }

    fn is_nil(&self) -> bool {
        matches!(self, V::Invalid | V::Nil)
    }

    fn from_json(j: &Json) -> V {
        match j {
            Json::Null => V::Nil,
            Json::Bool(b) => V::Bool(*b),
            Json::Number(n) => match n.as_i64() {
                Some(i) => V::Int(i),
                None => V::Float(n.as_f64().unwrap_or_default()),
            },
            Json::String(s) => V::Str(s.clone()),
            Json::Array(a) => V::List(Rc::new(a.iter().map(V::from_json).collect())),
            Json::Object(o) => V::Map(
                Rc::new(
                    o.iter()
                        .map(|(k, v)| (k.clone(), V::from_json(v)))
                        .collect(),
                ),
                false,
            ),
        }
    }
}

/// A value plus whether its static type is interface{}, as for anything
/// taken out of a JSON object or array. Go strips that wrapper when a command
/// finishes; until then it shows in error messages and in nil handling.
#[derive(Clone, Debug)]
struct Val {
    v: V,
    iface: bool,
}

impl Val {
    fn plain(v: V) -> Val {
        Val { v, iface: false }
    }

    fn invalid() -> Val {
        Val::plain(V::Invalid)
    }

    fn elem(v: &V, iface: bool) -> Val {
        Val {
            v: v.clone(),
            iface,
        }
    }

    fn is_invalid(&self) -> bool {
        !self.iface && matches!(self.v, V::Invalid)
    }

    fn type_name(&self) -> &'static str {
        if self.iface {
            "interface {}"
        } else {
            self.v.type_name()
        }
    }

    /// indirectInterface: what an interface holds, invalid for a nil one.
    fn deref(&self) -> Val {
        if self.iface && self.v.is_nil() {
            Val::invalid()
        } else {
            Val::plain(self.v.clone())
        }
    }
}

fn truth(v: &V) -> bool {
    match v {
        V::Invalid | V::Nil => false,
        V::Bool(b) => *b,
        V::Int(i) => *i != 0,
        V::Byte(b) => *b != 0,
        V::Float(f) => *f != 0.0,
        V::Complex(r, i) => *r != 0.0 || *i != 0.0,
        V::Str(s) => !s.is_empty(),
        V::Map(m, _) => !m.is_empty(),
        V::List(l) => !l.is_empty(),
        V::Strs(l) => !l.is_empty(),
    }
}

// ---------------------------------------------------------------------------
// fmt (fmt/print.go, fmt/format.go)

const LDIGITS: &[u8; 17] = b"0123456789abcdefx";
const UDIGITS: &[u8; 17] = b"0123456789ABCDEFX";

#[derive(Clone, Copy, Default)]
struct Flags {
    wid_present: bool,
    prec_present: bool,
    minus: bool,
    plus: bool,
    sharp: bool,
    space: bool,
    zero: bool,
    sharp_v: bool,
    wid: usize,
    prec: usize,
}

#[derive(Default)]
struct Pp {
    buf: String,
    f: Flags,
    reordered: bool,
    good_arg_num: bool,
}

impl Pp {
    fn write_padding(&mut self, n: usize) {
        let c = if self.f.zero && !self.f.minus {
            '0'
        } else {
            ' '
        };
        self.buf.extend(std::iter::repeat_n(c, n));
    }

    fn pad(&mut self, s: &str) {
        if !self.f.wid_present || self.f.wid == 0 {
            self.buf.push_str(s);
            return;
        }
        let width = self.f.wid.saturating_sub(s.chars().count());
        if self.f.minus {
            self.buf.push_str(s);
            self.write_padding(width);
        } else {
            self.write_padding(width);
            self.buf.push_str(s);
        }
    }

    fn pad_no_zero(&mut self, s: &str) {
        let old = self.f.zero;
        self.f.zero = false;
        self.pad(s);
        self.f.zero = old;
    }

    fn fmt_unicode(&mut self, u: u64) {
        let prec = if self.f.prec_present && self.f.prec > 4 {
            self.f.prec
        } else {
            4
        };
        // By hand: Rust's widths stop at 65535, Go's go on.
        let hex = format!("{u:X}");
        let mut s = format!("U+{}{hex}", "0".repeat(prec.saturating_sub(hex.len())));
        if self.f.sharp && u <= 0x10FFFF {
            if let Some(c) = char::from_u32(u as u32).filter(|c| is_print(*c)) {
                let _ = write!(s, " '{}'", c);
            }
        }
        self.pad_no_zero(&s);
    }

    fn fmt_integer(&mut self, u: u64, base: u64, signed: bool, verb: char, digits: &[u8; 17]) {
        let negative = signed && (u as i64) < 0;
        let mut u = if negative { u.wrapping_neg() } else { u };
        let mut prec: isize = 0;
        if self.f.prec_present {
            prec = self.f.prec as isize;
            if prec == 0 && u == 0 {
                let old = self.f.zero;
                self.f.zero = false;
                self.write_padding(self.f.wid);
                self.f.zero = old;
                return;
            }
        } else if self.f.zero && !self.f.minus && self.f.wid_present {
            prec = self.f.wid as isize;
            if negative || self.f.plus || self.f.space {
                prec -= 1;
            }
        }
        let mut b: Vec<u8> = Vec::new();
        while u >= base {
            b.push(digits[(u % base) as usize]);
            u /= base;
        }
        b.push(digits[u as usize]);
        while (b.len() as isize) < prec {
            b.push(b'0');
        }
        if self.f.sharp {
            match base {
                2 => b.extend_from_slice(b"b0"),
                8 if b.last() != Some(&b'0') => b.push(b'0'),
                16 => {
                    b.push(digits[16]);
                    b.push(b'0');
                }
                _ => {}
            }
        }
        if verb == 'O' {
            b.extend_from_slice(b"o0");
        }
        if negative {
            b.push(b'-');
        } else if self.f.plus {
            b.push(b'+');
        } else if self.f.space {
            b.push(b' ');
        }
        b.reverse();
        self.pad_no_zero(&String::from_utf8_lossy(&b));
    }

    fn truncate_string<'s>(&self, s: &'s str) -> &'s str {
        if self.f.prec_present {
            if let Some((i, _)) = s.char_indices().nth(self.f.prec) {
                return &s[..i];
            }
        }
        s
    }

    fn fmt_s(&mut self, s: &str) {
        let s = self.truncate_string(s).to_string();
        self.pad(&s);
    }

    fn fmt_sx(&mut self, s: &str, digits: &[u8; 17]) {
        let b = s.as_bytes();
        let mut length = b.len();
        if self.f.prec_present && self.f.prec < length {
            length = self.f.prec;
        }
        let mut width = 2 * length;
        if width > 0 {
            if self.f.space {
                if self.f.sharp {
                    width *= 2;
                }
                width += length - 1;
            } else if self.f.sharp {
                width += 2;
            }
        } else {
            if self.f.wid_present {
                self.write_padding(self.f.wid);
            }
            return;
        }
        if self.f.wid_present && self.f.wid > width && !self.f.minus {
            self.write_padding(self.f.wid - width);
        }
        if self.f.sharp {
            self.buf.push('0');
            self.buf.push(digits[16] as char);
        }
        for (i, &c) in b[..length].iter().enumerate() {
            if self.f.space && i > 0 {
                self.buf.push(' ');
                if self.f.sharp {
                    self.buf.push('0');
                    self.buf.push(digits[16] as char);
                }
            }
            self.buf.push(digits[(c >> 4) as usize] as char);
            self.buf.push(digits[(c & 0xF) as usize] as char);
        }
        if self.f.wid_present && self.f.wid > width && self.f.minus {
            self.write_padding(self.f.wid - width);
        }
    }

    fn fmt_q(&mut self, s: &str) {
        let s = self.truncate_string(s).to_string();
        if self.f.sharp && can_backquote(&s) {
            self.pad(&format!("`{}`", s));
            return;
        }
        let q = quote_with(&s, '"', self.f.plus);
        self.pad(&q);
    }

    fn fmt_c(&mut self, c: u64, quoted: bool) {
        let r = u32::try_from(c)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('\u{FFFD}');
        if quoted {
            let mut s = String::from('\'');
            escape_rune(&mut s, r, '\'', self.f.plus);
            s.push('\'');
            self.pad(&s);
        } else {
            self.pad(r.encode_utf8(&mut [0; 4]));
        }
    }

    fn fmt_float_f(&mut self, v: f64, verb: char, prec: i32) {
        let prec = if self.f.prec_present {
            self.f.prec as i32
        } else {
            prec
        };
        let s = append_float(v, verb as u8, prec);
        let mut num: Vec<u8> = Vec::with_capacity(s.len() + 1);
        if !s.starts_with(['-', '+']) {
            num.push(b'+');
        }
        num.extend_from_slice(s.as_bytes());
        if self.f.space && num[0] == b'+' && !self.f.plus {
            num[0] = b' ';
        }
        if num[1] == b'I' || num[1] == b'N' {
            if num[1] == b'N' && !self.f.space && !self.f.plus {
                num.remove(0);
            }
            self.pad_no_zero(&String::from_utf8_lossy(&num));
            return;
        }
        if self.f.sharp && verb != 'b' {
            let mut digits: i32 = 0;
            if matches!(verb, 'v' | 'g' | 'G' | 'x') {
                digits = if prec == -1 { 6 } else { prec };
            }
            let mut tail: Vec<u8> = vec![];
            let mut has_dot = false;
            let mut saw_nonzero = false;
            let mut i = 1;
            while i < num.len() {
                match num[i] {
                    b'.' => has_dot = true,
                    b'p' | b'P' => {
                        tail.extend_from_slice(&num[i..]);
                        num.truncate(i);
                        break;
                    }
                    b'e' | b'E' if verb != 'x' && verb != 'X' => {
                        tail.extend_from_slice(&num[i..]);
                        num.truncate(i);
                        break;
                    }
                    c => {
                        if c != b'0' {
                            saw_nonzero = true;
                        }
                        if saw_nonzero {
                            digits -= 1;
                        }
                    }
                }
                i += 1;
            }
            if !has_dot {
                if num.len() == 2 && num[1] == b'0' {
                    digits -= 1;
                }
                num.push(b'.');
            }
            while digits > 0 {
                num.push(b'0');
                digits -= 1;
            }
            num.extend_from_slice(&tail);
        }
        if self.f.plus || num[0] != b'+' {
            if self.f.zero && !self.f.minus && self.f.wid_present && self.f.wid > num.len() {
                self.buf.push(num[0] as char);
                self.write_padding(self.f.wid - num.len());
                self.buf.push_str(&String::from_utf8_lossy(&num[1..]));
                return;
            }
            self.pad(&String::from_utf8_lossy(&num));
            return;
        }
        self.pad(&String::from_utf8_lossy(&num[1..]));
    }

    fn bad_verb(&mut self, verb: char, v: Option<&V>) {
        self.buf.push_str("%!");
        self.buf.push(verb);
        self.buf.push('(');
        match v {
            Some(v) if !v.is_nil() => {
                self.buf.push_str(v.type_name());
                self.buf.push('=');
                self.print_arg(v, 'v');
            }
            _ => self.buf.push_str("<nil>"),
        }
        self.buf.push(')');
    }

    fn print_arg(&mut self, v: &V, verb: char) {
        if v.is_nil() {
            match verb {
                'T' | 'v' => self.pad("<nil>"),
                _ => self.bad_verb(verb, None),
            }
            return;
        }
        match verb {
            'T' => self.fmt_s(v.type_name()),
            // Go prints the address of a map or slice, which says nothing here.
            'p' => self.bad_verb(verb, Some(v)),
            _ => self.print_value(v, verb, 0),
        }
    }

    fn print_value(&mut self, v: &V, verb: char, depth: usize) {
        match v {
            V::Invalid | V::Nil => {
                if depth == 0 {
                    self.buf.push_str("<invalid reflect.Value>");
                } else if verb == 'v' {
                    self.buf.push_str("<nil>");
                } else {
                    self.bad_verb(verb, None);
                }
            }
            V::Bool(b) => match verb {
                't' | 'v' => self.pad(if *b { "true" } else { "false" }),
                _ => self.bad_verb(verb, Some(v)),
            },
            V::Int(i) => self.print_integer(*i as u64, true, verb, v),
            V::Byte(b) => self.print_integer(*b as u64, false, verb, v),
            V::Float(f) => self.print_float(*f, verb, v),
            V::Complex(re, im) => match verb {
                'v' | 'b' | 'g' | 'G' | 'x' | 'X' | 'f' | 'F' | 'e' | 'E' => {
                    let old = self.f.plus;
                    self.buf.push('(');
                    self.print_float(*re, verb, v);
                    self.f.plus = true;
                    self.print_float(*im, verb, v);
                    self.buf.push_str("i)");
                    self.f.plus = old;
                }
                _ => self.bad_verb(verb, Some(v)),
            },
            V::Str(s) => match verb {
                'v' if self.f.sharp_v => self.fmt_q(s),
                'v' | 's' => self.fmt_s(s),
                'x' => self.fmt_sx(s, LDIGITS),
                'X' => self.fmt_sx(s, UDIGITS),
                'q' => self.fmt_q(s),
                _ => self.bad_verb(verb, Some(v)),
            },
            V::Map(m, strmap) => {
                if self.f.sharp_v {
                    self.buf.push_str(v.type_name());
                    self.buf.push('{');
                } else {
                    self.buf.push_str("map[");
                }
                for (i, (k, e)) in m.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(if self.f.sharp_v { ", " } else { " " });
                    }
                    self.print_value(&V::Str(k.clone()), verb, depth + 1);
                    self.buf.push(':');
                    self.print_elem(e, verb, depth + 1, !strmap);
                }
                self.buf.push(if self.f.sharp_v { '}' } else { ']' });
            }
            V::List(l) => {
                self.open_list(v);
                for (i, e) in l.iter().enumerate() {
                    self.list_sep(i);
                    self.print_elem(e, verb, depth + 1, true);
                }
                self.close_list();
            }
            V::Strs(l) => {
                self.open_list(v);
                for (i, e) in l.iter().enumerate() {
                    self.list_sep(i);
                    self.print_value(&V::Str(e.clone()), verb, depth + 1);
                }
                self.close_list();
            }
        }
    }

    fn open_list(&mut self, v: &V) {
        if self.f.sharp_v {
            self.buf.push_str(v.type_name());
            self.buf.push('{');
        } else {
            self.buf.push('[');
        }
    }

    fn list_sep(&mut self, i: usize) {
        if i > 0 {
            self.buf.push_str(if self.f.sharp_v { ", " } else { " " });
        }
    }

    fn close_list(&mut self) {
        self.buf.push(if self.f.sharp_v { '}' } else { ']' });
    }

    fn print_elem(&mut self, e: &V, verb: char, depth: usize, iface: bool) {
        if iface && e.is_nil() {
            self.buf.push_str(if self.f.sharp_v {
                "interface {}(nil)"
            } else {
                "<nil>"
            });
        } else {
            self.print_value(e, verb, depth + iface as usize);
        }
    }

    fn print_integer(&mut self, u: u64, signed: bool, verb: char, v: &V) {
        match verb {
            'v' if self.f.sharp_v && !signed => {
                let sharp = self.f.sharp;
                self.f.sharp = true;
                self.fmt_integer(u, 16, false, 'v', LDIGITS);
                self.f.sharp = sharp;
            }
            'v' | 'd' => self.fmt_integer(u, 10, signed, verb, LDIGITS),
            'b' => self.fmt_integer(u, 2, signed, verb, LDIGITS),
            'o' | 'O' => self.fmt_integer(u, 8, signed, verb, LDIGITS),
            'x' => self.fmt_integer(u, 16, signed, verb, LDIGITS),
            'X' => self.fmt_integer(u, 16, signed, verb, UDIGITS),
            'c' => self.fmt_c(u, false),
            'q' => self.fmt_c(u, true),
            'U' => self.fmt_unicode(u),
            _ => self.bad_verb(verb, Some(v)),
        }
    }

    fn print_float(&mut self, f: f64, verb: char, v: &V) {
        match verb {
            'v' => self.fmt_float_f(f, 'g', -1),
            'b' | 'g' | 'G' | 'x' | 'X' => self.fmt_float_f(f, verb, -1),
            'f' | 'e' | 'E' => self.fmt_float_f(f, verb, 6),
            'F' => self.fmt_float_f(f, 'f', 6),
            _ => self.bad_verb(verb, Some(v)),
        }
    }

    fn arg_number(
        &mut self,
        arg_num: usize,
        format: &str,
        i: usize,
        num_args: usize,
    ) -> (usize, usize, bool) {
        if format.len() <= i || format.as_bytes()[i] != b'[' {
            return (arg_num, i, false);
        }
        self.reordered = true;
        let (index, wid, ok) = parse_arg_number(&format[i..]);
        if ok && index < num_args {
            return (index, i + wid, true);
        }
        self.good_arg_num = false;
        (arg_num, i + wid, ok)
    }
}

/// fmt's parsenum: digits from start, refusing absurd widths.
fn parsenum(s: &[u8], start: usize, end: usize) -> (usize, bool, usize) {
    if start >= end {
        return (0, false, end);
    }
    let mut num = 0usize;
    let mut isnum = false;
    let mut i = start;
    while i < end && s[i].is_ascii_digit() {
        if num > 1_000_000 {
            return (0, false, end);
        }
        num = num * 10 + (s[i] - b'0') as usize;
        isnum = true;
        i += 1;
    }
    (num, isnum, i)
}

/// An explicit argument index like [2]; the index comes back zero-based,
/// usize::MAX for [0].
fn parse_arg_number(format: &str) -> (usize, usize, bool) {
    let b = format.as_bytes();
    if b.len() < 3 {
        return (0, 1, false);
    }
    for i in 1..b.len() {
        if b[i] == b']' {
            let (width, ok, newi) = parsenum(b, 1, i);
            if !ok || newi != i {
                return (0, i + 1, false);
            }
            return (width.wrapping_sub(1), i + 1, true);
        }
    }
    (0, 1, false)
}

fn int_from_arg(a: &[V], arg_num: usize) -> (i64, bool, usize) {
    if arg_num >= a.len() {
        return (0, false, arg_num);
    }
    let (num, is_int) = match a[arg_num] {
        V::Int(i) => (i, true),
        V::Byte(b) => (b as i64, true),
        _ => (0, false),
    };
    if !(-1_000_000..=1_000_000).contains(&num) {
        return (0, false, arg_num + 1);
    }
    (num, is_int, arg_num + 1)
}

fn sprint(args: &[V]) -> String {
    let mut p = Pp::default();
    let mut prev_string = false;
    for (i, a) in args.iter().enumerate() {
        let is_string = matches!(a, V::Str(_));
        if i > 0 && !is_string && !prev_string {
            p.buf.push(' ');
        }
        p.print_arg(a, 'v');
        prev_string = is_string;
    }
    p.buf
}

fn sprintln(args: &[V]) -> String {
    let mut p = Pp::default();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            p.buf.push(' ');
        }
        p.print_arg(a, 'v');
    }
    p.buf.push('\n');
    p.buf
}

fn sprintf(format: &str, a: &[V]) -> String {
    let mut p = Pp::default();
    let fb = format.as_bytes();
    let end = fb.len();
    let mut arg_num = 0usize;
    let mut i = 0usize;
    'format: while i < end {
        p.good_arg_num = true;
        let lasti = i;
        while i < end && fb[i] != b'%' {
            i += 1;
        }
        if i > lasti {
            p.buf.push_str(&format[lasti..i]);
        }
        if i >= end {
            break;
        }
        i += 1;
        p.f = Flags::default();
        while i < end {
            match fb[i] {
                b'#' => p.f.sharp = true,
                b'0' => p.f.zero = true,
                b'+' => p.f.plus = true,
                b'-' => p.f.minus = true,
                b' ' => p.f.space = true,
                c => {
                    if c.is_ascii_lowercase() && arg_num < a.len() {
                        if c == b'v' || c == b'w' {
                            p.f.sharp_v = p.f.sharp;
                            p.f.sharp = false;
                            p.f.plus = false;
                        }
                        p.print_arg(&a[arg_num], c as char);
                        arg_num += 1;
                        i += 1;
                        continue 'format;
                    }
                    break;
                }
            }
            i += 1;
        }
        let (n, ni, mut after_index) = p.arg_number(arg_num, format, i, a.len());
        arg_num = n;
        i = ni;
        if i < end && fb[i] == b'*' {
            i += 1;
            let (num, ok, na) = int_from_arg(a, arg_num);
            arg_num = na;
            p.f.wid_present = ok;
            if !ok {
                p.buf.push_str("%!(BADWIDTH)");
            }
            if num < 0 {
                p.f.wid = num.unsigned_abs() as usize;
                p.f.minus = true;
                p.f.zero = false;
            } else {
                p.f.wid = num as usize;
            }
            after_index = false;
        } else {
            let (num, isnum, ni) = parsenum(fb, i, end);
            p.f.wid = num;
            p.f.wid_present = isnum;
            i = ni;
            if after_index && p.f.wid_present {
                p.good_arg_num = false;
            }
        }
        if i + 1 < end && fb[i] == b'.' {
            i += 1;
            if after_index {
                p.good_arg_num = false;
            }
            let (n, ni, found) = p.arg_number(arg_num, format, i, a.len());
            arg_num = n;
            i = ni;
            after_index = found;
            if i < end && fb[i] == b'*' {
                i += 1;
                let (num, ok, na) = int_from_arg(a, arg_num);
                arg_num = na;
                p.f.prec_present = ok && num >= 0;
                p.f.prec = if num < 0 { 0 } else { num as usize };
                if !p.f.prec_present {
                    p.buf.push_str("%!(BADPREC)");
                }
                after_index = false;
            } else {
                let (num, isnum, ni) = parsenum(fb, i, end);
                p.f.prec = if isnum { num } else { 0 };
                p.f.prec_present = true;
                i = ni;
            }
        }
        if !after_index {
            let (n, ni, _) = p.arg_number(arg_num, format, i, a.len());
            arg_num = n;
            i = ni;
        }
        let verb = match format.get(i..).and_then(|s| s.chars().next()) {
            Some(c) => c,
            None => {
                p.buf.push_str("%!(NOVERB)");
                break;
            }
        };
        i += verb.len_utf8();
        if verb == '%' {
            p.buf.push('%');
        } else if !p.good_arg_num {
            let _ = write!(p.buf, "%!{}(BADINDEX)", verb);
        } else if arg_num >= a.len() {
            let _ = write!(p.buf, "%!{}(MISSING)", verb);
        } else {
            if verb == 'v' || verb == 'w' {
                p.f.sharp_v = p.f.sharp;
                p.f.sharp = false;
                p.f.plus = false;
            }
            p.print_arg(&a[arg_num], verb);
            arg_num += 1;
        }
    }
    if !p.reordered && arg_num < a.len() {
        p.f = Flags::default();
        p.buf.push_str("%!(EXTRA ");
        for (i, arg) in a[arg_num..].iter().enumerate() {
            if i > 0 {
                p.buf.push_str(", ");
            }
            if arg.is_nil() {
                p.buf.push_str("<nil>");
            } else {
                p.buf.push_str(arg.type_name());
                p.buf.push('=');
                p.print_arg(arg, 'v');
            }
        }
        p.buf.push(')');
    }
    p.buf
}

/// Go's fmt.Sprint of one value, the way an action prints its result.
fn sprint_value(v: &V) -> String {
    let mut p = Pp::default();
    p.print_arg(v, 'v');
    p.buf
}

fn sprint_verb(v: &V, verb: char) -> String {
    let mut p = Pp::default();
    p.print_arg(v, verb);
    p.buf
}

// ---------------------------------------------------------------------------
// strconv.AppendFloat

fn append_float(v: f64, fmt: u8, prec: i32) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Inf" } else { "+Inf" }.to_string();
    }
    let neg = v.is_sign_negative();
    let bits = v.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let mut mant = bits & ((1u64 << 52) - 1);
    let mut exp = biased;
    if biased == 0 {
        exp += 1;
    } else {
        mant |= 1u64 << 52;
    }
    exp -= 1023;
    match fmt {
        b'b' => {
            let e = exp - 52;
            return format!(
                "{}{}p{}{}",
                if neg { "-" } else { "" },
                mant,
                if e >= 0 { "+" } else { "" },
                e
            );
        }
        b'x' | b'X' => return fmt_hex_float(prec, fmt, neg, mant, exp),
        _ => {}
    }
    let shortest = prec < 0;
    let mut prec = prec;
    let (digs, dp) = if mant == 0 {
        (Vec::new(), 0)
    } else if shortest {
        let (digs, dp) = decimal_digits(&format!("{:e}", v.abs()));
        let nd = digs.len() as i32;
        prec = match fmt {
            b'e' | b'E' => (nd - 1).max(0),
            b'f' => (nd - dp).max(0),
            _ => nd,
        };
        (digs, dp)
    } else {
        match fmt {
            // An f64 has at most 1074 digits after the point and 767 significant ones:
            // Rust's precisions stop at 65535, so beyond those the digits are zeros
            // added by hand.
            b'f' => {
                let exact = (prec as usize).min(MAX_FRACTION_DIGITS);
                let mut s = format!("{:.*}", exact, v.abs());
                s.push_str(&"0".repeat(prec as usize - exact));
                return if neg { format!("-{}", s) } else { s };
            }
            b'e' | b'E' => decimal_digits(&format!(
                "{:.*e}",
                (prec as usize).min(MAX_SIGNIFICANT_DIGITS),
                v.abs()
            )),
            _ => {
                if prec == 0 {
                    prec = 1;
                }
                decimal_digits(&format!(
                    "{:.*e}",
                    ((prec - 1) as usize).min(MAX_SIGNIFICANT_DIGITS),
                    v.abs()
                ))
            }
        }
    };
    format_digits(shortest, neg, &digs, dp, prec, fmt)
}

const MAX_FRACTION_DIGITS: usize = 1074;
const MAX_SIGNIFICANT_DIGITS: usize = 767;

/// The digits of Rust's exponent form without trailing zeros, and the
/// position of the decimal point.
fn decimal_digits(s: &str) -> (Vec<u8>, i32) {
    let (m, e) = s.split_once('e').unwrap_or((s, "0"));
    let mut digs: Vec<u8> = m.bytes().filter(u8::is_ascii_digit).collect();
    while digs.last() == Some(&b'0') {
        digs.pop();
    }
    let e: i32 = e.parse().unwrap_or(0);
    (digs, e + 1)
}

fn format_digits(shortest: bool, neg: bool, d: &[u8], dp: i32, prec: i32, fmt: u8) -> String {
    match fmt {
        b'e' | b'E' => fmt_e(neg, d, dp, prec, fmt),
        b'f' => fmt_f(neg, d, dp, prec),
        b'g' | b'G' => {
            let nd = d.len() as i32;
            let mut eprec = prec;
            if eprec > nd && nd >= dp {
                eprec = nd;
            }
            if shortest {
                eprec = 6;
            }
            let exp = dp - 1;
            if exp < -4 || exp >= eprec {
                return fmt_e(neg, d, dp, prec.min(nd) - 1, fmt + b'e' - b'g');
            }
            let p = if prec > dp { nd } else { prec };
            fmt_f(neg, d, dp, (p - dp).max(0))
        }
        _ => format!("%{}", fmt as char),
    }
}

fn fmt_e(neg: bool, d: &[u8], dp: i32, prec: i32, fmt: u8) -> String {
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push(*d.first().unwrap_or(&b'0') as char);
    if prec > 0 {
        s.push('.');
        let m = d.len().min(prec as usize + 1);
        if m > 1 {
            s.push_str(&String::from_utf8_lossy(&d[1..m]));
        }
        for _ in m.max(1)..=prec as usize {
            s.push('0');
        }
    }
    s.push(fmt as char);
    let mut exp = if d.is_empty() { 0 } else { dp - 1 };
    if exp < 0 {
        s.push('-');
        exp = -exp;
    } else {
        s.push('+');
    }
    if exp < 10 {
        s.push('0');
    }
    let _ = write!(s, "{}", exp);
    s
}

fn fmt_f(neg: bool, d: &[u8], dp: i32, prec: i32) -> String {
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if dp > 0 {
        let m = d.len().min(dp as usize);
        s.push_str(&String::from_utf8_lossy(&d[..m]));
        for _ in m..dp as usize {
            s.push('0');
        }
    } else {
        s.push('0');
    }
    if prec > 0 {
        s.push('.');
        for i in 0..prec {
            let j = dp + i;
            let ch = if j >= 0 && (j as usize) < d.len() {
                d[j as usize]
            } else {
                b'0'
            };
            s.push(ch as char);
        }
    }
    s
}

fn fmt_hex_float(prec: i32, fmt: u8, neg: bool, mant: u64, exp: i32) -> String {
    let mut mant = mant;
    let mut exp = if mant == 0 { 0 } else { exp };
    mant <<= 60 - 52;
    while mant != 0 && mant & (1 << 60) == 0 {
        mant <<= 1;
        exp -= 1;
    }
    if (0..15).contains(&prec) {
        let shift = (prec * 4) as u32;
        let extra = (mant << shift) & ((1u64 << 60) - 1);
        mant >>= 60 - shift;
        if extra | (mant & 1) > 1 << 59 {
            mant += 1;
        }
        mant <<= 60 - shift;
        if mant & (1 << 61) != 0 {
            mant >>= 1;
            exp += 1;
        }
    }
    let hex = if fmt == b'X' { UDIGITS } else { LDIGITS };
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    s.push('0');
    s.push(fmt as char);
    s.push((b'0' + ((mant >> 60) & 1) as u8) as char);
    mant <<= 4;
    if prec < 0 && mant != 0 {
        s.push('.');
        while mant != 0 {
            s.push(hex[((mant >> 60) & 15) as usize] as char);
            mant <<= 4;
        }
    } else if prec > 0 {
        s.push('.');
        for _ in 0..prec {
            s.push(hex[((mant >> 60) & 15) as usize] as char);
            mant <<= 4;
        }
    }
    s.push(if fmt == b'x' { 'p' } else { 'P' });
    if exp < 0 {
        s.push('-');
        exp = -exp;
    } else {
        s.push('+');
    }
    let _ = write!(s, "{:02}", exp);
    s
}

// ---------------------------------------------------------------------------
// encoding/json, as the reference's json function calls it

fn json_encode(v: &V, out: &mut String) -> Result<(), String> {
    match v {
        V::Invalid | V::Nil => out.push_str("null"),
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Int(i) => {
            let _ = write!(out, "{}", i);
        }
        V::Byte(b) => {
            let _ = write!(out, "{}", b);
        }
        V::Float(f) => {
            let a = f.abs();
            if a != 0.0 && !(1e-6..1e21).contains(&a) {
                let mut s = append_float(*f, b'e', -1);
                let n = s.len();
                if n >= 4 && &s[n - 4..n - 2] == "e-" && &s[n - 2..n - 1] == "0" {
                    s.remove(n - 2);
                }
                out.push_str(&s);
            } else {
                out.push_str(&append_float(*f, b'f', -1));
            }
        }
        V::Complex(..) => return Err("json: unsupported type: complex128".to_string()),
        V::Str(s) => json_string(s, out),
        V::Map(m, _) => {
            out.push('{');
            for (i, (k, e)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_string(k, out);
                out.push(':');
                json_encode(e, out)?;
            }
            out.push('}');
        }
        V::List(l) => {
            out.push('[');
            for (i, e) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_encode(e, out)?;
            }
            out.push(']');
        }
        V::Strs(l) => {
            out.push('[');
            for (i, e) in l.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_string(e, out);
            }
            out.push(']');
        }
    }
    Ok(())
}

fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' | '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Functions (text/template/funcs.go and the reference's templates.go)

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Func {
    And,
    Or,
    Call,
    Html,
    Index,
    Slice,
    Js,
    Len,
    Not,
    Print,
    Printf,
    Println,
    Urlquery,
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
    Ne,
    Json,
    Split,
    Join,
    Title,
    Lower,
    Upper,
    Pad,
    Truncate,
    // The header stand-ins, which return their first argument.
    HeaderStr,
    HeaderStrStr,
    HeaderStrInt,
}

/// The Go parameter types that matter for argument checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pt {
    /// No type: the receiver of a chain like (pipeline).Field.
    Untyped,
    /// reflect.Value.
    Value,
    /// interface{}.
    Any,
    Str,
    Int,
}

impl Pt {
    fn name(self) -> &'static str {
        match self {
            Pt::Str => "string",
            Pt::Int => "int",
            _ => "interface {}",
        }
    }
}

fn lookup_func(name: &str, header: bool) -> Option<Func> {
    Some(match name {
        "and" => Func::And,
        "or" => Func::Or,
        "call" => Func::Call,
        "html" => Func::Html,
        "index" => Func::Index,
        "slice" => Func::Slice,
        "js" => Func::Js,
        "len" => Func::Len,
        "not" => Func::Not,
        "print" => Func::Print,
        "printf" => Func::Printf,
        "println" => Func::Println,
        "urlquery" => Func::Urlquery,
        "eq" => Func::Eq,
        "ge" => Func::Ge,
        "gt" => Func::Gt,
        "le" => Func::Le,
        "lt" => Func::Lt,
        "ne" => Func::Ne,
        "pad" => Func::Pad,
        "json" | "title" | "lower" | "upper" if header => Func::HeaderStr,
        "split" | "join" if header => Func::HeaderStrStr,
        "truncate" if header => Func::HeaderStrInt,
        "json" => Func::Json,
        "split" => Func::Split,
        "join" => Func::Join,
        "title" => Func::Title,
        "lower" => Func::Lower,
        "upper" => Func::Upper,
        "truncate" => Func::Truncate,
        _ => return None,
    })
}

/// Fixed parameters and the element type of a variadic tail.
fn signature(f: Func) -> (&'static [Pt], Option<Pt>) {
    match f {
        Func::And | Func::Or | Func::Call | Func::Index | Func::Slice | Func::Eq => {
            (&[Pt::Value], Some(Pt::Value))
        }
        Func::Html | Func::Js | Func::Urlquery | Func::Print | Func::Println => {
            (&[], Some(Pt::Any))
        }
        Func::Printf => (&[Pt::Str], Some(Pt::Any)),
        Func::Len | Func::Not => (&[Pt::Value], None),
        Func::Ge | Func::Gt | Func::Le | Func::Lt | Func::Ne => (&[Pt::Value, Pt::Value], None),
        Func::Json => (&[Pt::Any], None),
        Func::Join => (&[Pt::Any, Pt::Str], None),
        Func::Split | Func::HeaderStrStr => (&[Pt::Str, Pt::Str], None),
        Func::Title | Func::Lower | Func::Upper | Func::HeaderStr => (&[Pt::Str], None),
        Func::Pad => (&[Pt::Str, Pt::Int, Pt::Int], None),
        Func::Truncate | Func::HeaderStrInt => (&[Pt::Str, Pt::Int], None),
    }
}

fn str_arg(v: &Val) -> &str {
    match &v.v {
        V::Str(s) => s,
        _ => "",
    }
}

fn int_arg(v: &Val) -> i64 {
    match v.v {
        V::Int(i) => i,
        _ => 0,
    }
}

fn index_arg(index: &Val, cap: usize) -> Result<usize, String> {
    let x = match (&index.v, index.iface) {
        (V::Int(i), false) => *i,
        (V::Byte(b), false) => *b as i64,
        (V::Invalid, false) => return Err("cannot index slice/array with nil".to_string()),
        _ => {
            return Err(format!(
                "cannot index slice/array with type {}",
                index.type_name()
            ))
        }
    };
    if x < 0 || x as u64 > cap as u64 {
        return Err(format!("index out of range: {}", x));
    }
    Ok(x as usize)
}

fn func_index(argv: &[Val]) -> Result<Val, String> {
    let mut item = argv[0].deref();
    if item.is_invalid() {
        return Err("index of untyped nil".to_string());
    }
    for index in &argv[1..] {
        let index = index.deref();
        if item.iface {
            if item.v.is_nil() {
                return Err("index of nil pointer".to_string());
            }
            item = item.deref();
        }
        item = match &item.v {
            V::List(l) => {
                let x = index_arg(&index, l.len())?;
                let e = l.get(x).ok_or("reflect: slice index out of range")?;
                Val::elem(e, true)
            }
            V::Strs(l) => {
                let x = index_arg(&index, l.len())?;
                let e = l.get(x).ok_or("reflect: slice index out of range")?;
                Val::plain(V::Str(e.clone()))
            }
            V::Str(s) => {
                let x = index_arg(&index, s.len())?;
                let b = s
                    .as_bytes()
                    .get(x)
                    .ok_or("reflect: string index out of range")?;
                Val::plain(V::Byte(*b))
            }
            V::Map(m, strmap) => {
                let key = match &index.v {
                    V::Str(k) => k,
                    V::Invalid => return Err("value is nil; should be of type string".to_string()),
                    _ => {
                        return Err(format!(
                            "value has type {}; should be string",
                            index.type_name()
                        ))
                    }
                };
                match m.get(key) {
                    Some(e) => Val::elem(e, !strmap),
                    None if *strmap => Val::plain(V::Str(String::new())),
                    None => Val::elem(&V::Nil, true),
                }
            }
            _ => return Err(format!("can't index item of type {}", item.type_name())),
        };
    }
    Ok(item)
}

/// Go checks slice indexes against the capacity, which for a decoded JSON
/// array is whatever append grew it to; the length is the bound here.
fn func_slice(argv: &[Val]) -> Result<Val, String> {
    let item = argv[0].deref();
    if item.is_invalid() {
        return Err("slice of untyped nil".to_string());
    }
    let indexes = &argv[1..];
    if indexes.len() > 3 {
        return Err(format!("too many slice indexes: {}", indexes.len()));
    }
    let len = match &item.v {
        V::Str(s) => {
            if indexes.len() == 3 {
                return Err("cannot 3-index slice a string".to_string());
            }
            s.len()
        }
        V::List(l) => l.len(),
        V::Strs(l) => l.len(),
        _ => return Err(format!("can't slice item of type {}", item.type_name())),
    };
    let mut idx = [0, len, 0];
    for (i, index) in indexes.iter().enumerate() {
        idx[i] = index_arg(index, len)?;
    }
    if idx[0] > idx[1] {
        return Err(format!("invalid slice index: {} > {}", idx[0], idx[1]));
    }
    if indexes.len() == 3 && idx[1] > idx[2] {
        return Err(format!("invalid slice index: {} > {}", idx[1], idx[2]));
    }
    let (a, b) = (idx[0], idx[1]);
    Ok(Val::plain(match &item.v {
        V::Str(s) => V::Str(String::from_utf8_lossy(&s.as_bytes()[a..b]).into_owned()),
        V::List(l) => V::List(Rc::new(l[a..b].to_vec())),
        V::Strs(l) => V::Strs(Rc::new(l[a..b].to_vec())),
        _ => V::Invalid,
    }))
}

fn func_len(item: &Val) -> Result<Val, String> {
    if item.iface && item.v.is_nil() {
        return Err("len of nil pointer".to_string());
    }
    let n = match &item.v {
        V::Invalid => return Err("reflect: call of reflect.Value.Type on zero Value".to_string()),
        V::Str(s) => s.len(),
        V::Map(m, _) => m.len(),
        V::List(l) => l.len(),
        V::Strs(l) => l.len(),
        v => return Err(format!("len of type {}", v.type_name())),
    };
    Ok(Val::plain(V::Int(n as i64)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Complex,
    Int,
    Float,
    Str,
    Uint,
}

fn basic_kind(v: &V) -> Option<Kind> {
    Some(match v {
        V::Bool(_) => Kind::Bool,
        V::Int(_) => Kind::Int,
        V::Byte(_) => Kind::Uint,
        V::Float(_) => Kind::Float,
        V::Complex(..) => Kind::Complex,
        V::Str(_) => Kind::Str,
        _ => return None,
    })
}

/// reflect.Kind for the values basicKind rejects, to tell maps from slices.
fn other_kind(v: &V) -> u8 {
    match v {
        V::Map(..) => 1,
        V::List(_) | V::Strs(_) => 2,
        _ => 0,
    }
}

fn as_int(v: &V) -> i64 {
    match v {
        V::Int(i) => *i,
        _ => 0,
    }
}

fn as_uint(v: &V) -> u64 {
    match v {
        V::Byte(b) => *b as u64,
        _ => 0,
    }
}

fn func_eq(argv: &[Val]) -> Result<bool, String> {
    let a1 = argv[0].deref().v;
    if argv.len() < 2 {
        return Err("missing argument for comparison".to_string());
    }
    let k1 = basic_kind(&a1);
    for arg in &argv[1..] {
        let a = arg.deref().v;
        let k2 = basic_kind(&a);
        let truth = if k1 != k2 {
            match (k1, k2) {
                (Some(Kind::Int), Some(Kind::Uint)) => {
                    as_int(&a1) >= 0 && as_int(&a1) as u64 == as_uint(&a)
                }
                (Some(Kind::Uint), Some(Kind::Int)) => {
                    as_int(&a) >= 0 && as_uint(&a1) == as_int(&a) as u64
                }
                _ => {
                    if !matches!(a1, V::Invalid) && !matches!(a, V::Invalid) {
                        return Err(format!(
                            "incompatible types for comparison: {} and {}",
                            a1.type_name(),
                            a.type_name()
                        ));
                    }
                    false
                }
            }
        } else {
            match (&a1, &a) {
                (V::Bool(x), V::Bool(y)) => x == y,
                (V::Complex(r1, i1), V::Complex(r2, i2)) => r1 == r2 && i1 == i2,
                (V::Float(x), V::Float(y)) => x == y,
                (V::Int(x), V::Int(y)) => x == y,
                (V::Str(x), V::Str(y)) => x == y,
                (V::Byte(x), V::Byte(y)) => x == y,
                _ => {
                    let nil1 = matches!(a1, V::Invalid);
                    let nil2 = matches!(a, V::Invalid);
                    if !nil1 && !nil2 && other_kind(&a1) != other_kind(&a) {
                        return Err(format!(
                            "non-comparable types {}: {}, {}: {}",
                            sprint_verb(&a1, 's'),
                            a1.type_name(),
                            a.type_name(),
                            sprint_value(&a)
                        ));
                    }
                    if nil1 || nil2 {
                        nil1 == nil2
                    } else {
                        return Err(format!(
                            "non-comparable type {}: {}",
                            sprint_verb(&a, 's'),
                            a.type_name()
                        ));
                    }
                }
            }
        };
        if truth {
            return Ok(true);
        }
    }
    Ok(false)
}

fn func_lt(argv: &[Val]) -> Result<bool, String> {
    let bad = || "invalid type for comparison".to_string();
    let a1 = argv[0].deref().v;
    let k1 = basic_kind(&a1).ok_or_else(bad)?;
    let a2 = argv[1].deref().v;
    let k2 = basic_kind(&a2).ok_or_else(bad)?;
    if k1 != k2 {
        return match (k1, k2) {
            (Kind::Int, Kind::Uint) => Ok(as_int(&a1) < 0 || (as_int(&a1) as u64) < as_uint(&a2)),
            (Kind::Uint, Kind::Int) => Ok(as_int(&a2) >= 0 && as_uint(&a1) < as_int(&a2) as u64),
            _ => Err(format!(
                "incompatible types for comparison: {} and {}",
                a1.type_name(),
                a2.type_name()
            )),
        };
    }
    Ok(match (&a1, &a2) {
        (V::Float(x), V::Float(y)) => x < y,
        (V::Int(x), V::Int(y)) => x < y,
        (V::Str(x), V::Str(y)) => x < y,
        (V::Byte(x), V::Byte(y)) => x < y,
        _ => return Err(bad()),
    })
}

/// evalArgs of funcs.go: one string as is, anything else through Sprint,
/// with a nil argument printed as "<no value>".
fn eval_args(args: &[Val]) -> String {
    if let [Val { v: V::Str(s), .. }] = args {
        return s.clone();
    }
    let vals: Vec<V> = args
        .iter()
        .map(|a| {
            if a.v.is_nil() {
                V::Str("<no value>".to_string())
            } else {
                a.v.clone()
            }
        })
        .collect();
    sprint(&vals)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\0' => out.push('\u{FFFD}'),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003C"),
            '>' => out.push_str("\\u003E"),
            '&' => out.push_str("\\u0026"),
            '=' => out.push_str("\\u003D"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u00{:02X}", c as u32);
            }
            c if c.is_ascii() || is_print(c) => out.push(c),
            c => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
        }
    }
    out
}

fn url_query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

/// Go's one-rune case mappings; Rust's multi-rune results leave the rune
/// unchanged, as Go does.
fn map_case(c: char, upper: bool) -> char {
    if !upper && c == '\u{130}' {
        return 'i';
    }
    let mut it: Box<dyn Iterator<Item = char>> = if upper {
        Box::new(c.to_uppercase())
    } else {
        Box::new(c.to_lowercase())
    };
    match (it.next(), it.next()) {
        (Some(m), None) => m,
        _ => c,
    }
}

fn to_title(c: char) -> char {
    match c {
        '\u{1C4}'..='\u{1C6}' => '\u{1C5}',
        '\u{1C7}'..='\u{1C9}' => '\u{1C8}',
        '\u{1CA}'..='\u{1CC}' => '\u{1CB}',
        '\u{1F1}'..='\u{1F3}' => '\u{1F2}',
        c => map_case(c, true),
    }
}

/// strings.Title: the first letter of every word, words being separated by
/// anything but letters, digits and underscores.
fn title(s: &str) -> String {
    let is_sep = |r: char| {
        if r.is_ascii() {
            return !(r.is_ascii_alphanumeric() || r == '_');
        }
        if r.is_alphabetic() || r.is_numeric() {
            return false;
        }
        r.is_whitespace()
    };
    let mut prev = ' ';
    s.chars()
        .map(|r| {
            let out = if is_sep(prev) { to_title(r) } else { r };
            prev = r;
            out
        })
        .collect()
}

fn split(s: &str, sep: &str) -> Vec<String> {
    if sep.is_empty() {
        return s.chars().map(String::from).collect();
    }
    s.split(sep).map(str::to_string).collect()
}

fn func_join(elems: &V, sep: &str) -> Result<String, String> {
    Ok(match elems {
        V::Invalid | V::Nil => String::new(),
        V::Strs(l) => l.join(sep),
        V::List(l) => l
            .iter()
            .map(|e| sprint(std::slice::from_ref(e)))
            .collect::<Vec<_>>()
            .join(sep),
        V::Map(m, _) => {
            let mut out: Vec<String> = m
                .values()
                .map(|e| sprint(std::slice::from_ref(e)))
                .collect();
            out.sort();
            out.join(sep)
        }
        v => return Err(format!("expected slice, got {}", v.type_name())),
    })
}

/// The functions whose argument handling is the plain one; and, or and call
/// are handled while evaluating the call.
fn call_func(f: Func, argv: &[Val]) -> Result<Val, String> {
    let s = |s: String| Ok(Val::plain(V::Str(s)));
    let b = |b: bool| Ok(Val::plain(V::Bool(b)));
    let any: Vec<V> = argv.iter().map(|a| a.v.clone()).collect();
    match f {
        Func::Not => b(!truth(&argv[0].v)),
        Func::Len => func_len(&argv[0]),
        Func::Index => func_index(argv),
        Func::Slice => func_slice(argv),
        Func::Eq => b(func_eq(argv)?),
        Func::Ne => b(!func_eq(argv)?),
        Func::Lt => b(func_lt(argv)?),
        Func::Le => b(func_lt(argv)? || func_eq(argv)?),
        Func::Gt => b(!(func_lt(argv)? || func_eq(argv)?)),
        Func::Ge => b(!func_lt(argv)?),
        Func::Html => s(html_escape(&eval_args(argv))),
        Func::Js => s(js_escape(&eval_args(argv))),
        Func::Urlquery => s(url_query_escape(&eval_args(argv))),
        Func::Print => s(sprint(&any)),
        Func::Println => s(sprintln(&any)),
        Func::Printf => s(sprintf(str_arg(&argv[0]), &any[1..])),
        Func::Json => {
            let mut out = String::new();
            json_encode(&argv[0].v, &mut out)?;
            s(out)
        }
        Func::Split => Ok(Val::plain(V::Strs(Rc::new(split(
            str_arg(&argv[0]),
            str_arg(&argv[1]),
        ))))),
        Func::Join => s(func_join(&argv[0].v, str_arg(&argv[1]))?),
        Func::Title => s(title(str_arg(&argv[0]))),
        Func::Lower => s(str_arg(&argv[0])
            .chars()
            .map(|c| map_case(c, false))
            .collect()),
        Func::Upper => s(str_arg(&argv[0])
            .chars()
            .map(|c| map_case(c, true))
            .collect()),
        Func::Pad => {
            let src = str_arg(&argv[0]);
            let (pre, post) = (int_arg(&argv[1]), int_arg(&argv[2]));
            if src.is_empty() {
                return s(String::new());
            }
            if pre < 0 || post < 0 {
                return Err("strings: negative Repeat count".to_string());
            }
            // Go would try to allocate whatever it is asked for and die.
            if pre > 1 << 24 || post > 1 << 24 {
                return Err("strings: Repeat output length overflow".to_string());
            }
            s(format!(
                "{}{}{}",
                " ".repeat(pre as usize),
                src,
                " ".repeat(post as usize)
            ))
        }
        Func::Truncate => {
            let src = str_arg(&argv[0]);
            let n = int_arg(&argv[1]);
            if (src.len() as i64) < n {
                return s(src.to_string());
            }
            if n < 0 {
                return Err(format!("runtime error: slice bounds out of range [:{}]", n));
            }
            s(String::from_utf8_lossy(&src.as_bytes()[..n as usize]).into_owned())
        }
        Func::HeaderStr | Func::HeaderStrStr | Func::HeaderStrInt => {
            s(str_arg(&argv[0]).to_string())
        }
        Func::And | Func::Or | Func::Call => Ok(Val::invalid()),
    }
}

// ---------------------------------------------------------------------------
// Execution (text/template/exec.go)

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Normal,
    Break,
    Continue,
}

type EResult<T> = Result<T, String>;

struct Exec<'t> {
    tmpl: &'t Template,
    header: bool,
    name: &'t str,
    node: Option<NodeRef<'t>>,
    vars: Vec<(&'t str, Val)>,
    depth: usize,
    /// Controls and template calls being walked, all templates together.
    run_depth: usize,
    /// A key a map lacks is an error, as a field a struct lacks is in Go: the rows of
    /// a list stand for the reference's structs.
    strict: bool,
    out: String,
}

impl<'t> Exec<'t> {
    fn at(&mut self, n: NodeRef<'t>) {
        self.node = Some(n);
    }

    fn errorf(&self, msg: &str) -> String {
        match self.node {
            None => format!("template: {}: {}", self.name, msg),
            Some(n) => {
                let pos = n.pos();
                let text = self.tmpl.text.get(..pos).unwrap_or(&self.tmpl.text);
                let col = match text.rfind('\n') {
                    None => pos,
                    Some(i) => pos - (i + 1),
                };
                let line = 1 + count_nl(text);
                format!(
                    "template: :{}:{}: executing {} at <{}>: {}",
                    line,
                    col,
                    quote_with(self.name, '"', false),
                    n.render(),
                    msg
                )
            }
        }
    }

    fn var_value(&self, name: &str) -> EResult<Val> {
        match self.vars.iter().rev().find(|(n, _)| *n == name) {
            Some((_, v)) => Ok(v.clone()),
            None => Err(self.errorf(&format!("undefined variable: {}", name))),
        }
    }

    fn set_var(&mut self, name: &str, value: Val) -> EResult<()> {
        if let Some(e) = self.vars.iter_mut().rev().find(|(n, _)| *n == name) {
            e.1 = value;
            return Ok(());
        }
        Err(self.errorf(&format!("undefined variable: {}", name)))
    }

    fn set_top_var(&mut self, n: usize, value: Val) {
        let len = self.vars.len();
        if let Some(e) = len.checked_sub(n).and_then(|i| self.vars.get_mut(i)) {
            e.1 = value;
        }
    }

    fn walk(&mut self, dot: &Val, node: &'t Node) -> EResult<Flow> {
        self.at(NodeRef::Node(node));
        match node {
            Node::Action { pipe, .. } => {
                let val = self.eval_pipeline(dot, Some(pipe))?;
                if pipe.decl.is_empty() {
                    self.at(NodeRef::Node(node));
                    if val.v.is_nil() {
                        self.out.push_str("<no value>");
                    } else {
                        let s = sprint_value(&val.v);
                        self.out.push_str(&s);
                    }
                }
            }
            Node::Break { .. } => return Ok(Flow::Break),
            Node::Continue { .. } => return Ok(Flow::Continue),
            Node::If(_) | Node::With(_) | Node::Range(_) | Node::Template { .. } => {
                if self.run_depth >= MAX_RUN_DEPTH {
                    return Err(
                        self.errorf(&format!("exceeded maximum nesting depth ({MAX_RUN_DEPTH})"))
                    );
                }
                self.run_depth += 1;
                let flow = match node {
                    Node::If(b) => self.walk_if_or_with(false, dot, b),
                    Node::With(b) => self.walk_if_or_with(true, dot, b),
                    Node::Range(b) => self.walk_range(dot, node, b),
                    Node::Template { name, pipe, .. } => {
                        self.walk_template(dot, node, name, pipe.as_ref())
                    }
                    _ => unreachable!("matched above"),
                };
                self.run_depth -= 1;
                return flow;
            }
            Node::Text { text, .. } => self.out.push_str(text),
            Node::End { .. } | Node::Else { .. } => {}
        }
        Ok(Flow::Normal)
    }

    fn walk_list(&mut self, dot: &Val, list: &'t List) -> EResult<Flow> {
        self.at(NodeRef::List(list));
        for n in &list.nodes {
            let flow = self.walk(dot, n)?;
            if flow != Flow::Normal {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn walk_if_or_with(&mut self, with: bool, dot: &Val, b: &'t Branch) -> EResult<Flow> {
        let mark = self.vars.len();
        let val = self.eval_pipeline(dot, Some(&b.pipe))?;
        let flow = if truth(&val.v) {
            if with {
                self.walk_list(&val, &b.list)?
            } else {
                self.walk_list(dot, &b.list)?
            }
        } else if let Some(e) = &b.else_list {
            self.walk_list(dot, e)?
        } else {
            Flow::Normal
        };
        self.vars.truncate(mark);
        Ok(flow)
    }

    fn walk_range(&mut self, dot: &Val, node: &'t Node, b: &'t Branch) -> EResult<Flow> {
        self.at(NodeRef::Node(node));
        let outer = self.vars.len();
        let val = self.eval_pipeline(dot, Some(&b.pipe))?;
        let mark = self.vars.len();
        let mut ran = false;
        match &val.v {
            V::Int(_) | V::Byte(_) => {
                if b.pipe.decl.len() > 1 {
                    return Err(self.errorf(&format!(
                        "can't use {} to iterate over more than one variable",
                        sprint_value(&val.v)
                    )));
                }
                let (n, byte) = match val.v {
                    V::Byte(x) => (x as i64, true),
                    _ => (as_int(&val.v), false),
                };
                for i in 0..n.max(0) {
                    ran = true;
                    let e = if byte { V::Byte(i as u8) } else { V::Int(i) };
                    if self.one_iteration(b, Val::invalid(), Val::plain(e), mark)? {
                        break;
                    }
                }
            }
            V::List(l) => {
                for (i, e) in l.iter().enumerate() {
                    ran = true;
                    let idx = Val::plain(V::Int(i as i64));
                    if self.one_iteration(b, idx, Val::elem(e, true), mark)? {
                        break;
                    }
                }
            }
            V::Strs(l) => {
                for (i, e) in l.iter().enumerate() {
                    ran = true;
                    let idx = Val::plain(V::Int(i as i64));
                    if self.one_iteration(b, idx, Val::plain(V::Str(e.clone())), mark)? {
                        break;
                    }
                }
            }
            V::Map(m, strmap) => {
                for (k, e) in m.iter() {
                    ran = true;
                    let key = Val::plain(V::Str(k.clone()));
                    if self.one_iteration(b, key, Val::elem(e, !strmap), mark)? {
                        break;
                    }
                }
            }
            V::Invalid | V::Nil => {}
            v => return Err(self.errorf(&format!("range can't iterate over {}", sprint_value(v)))),
        }
        if !ran {
            if let Some(e) = &b.else_list {
                self.walk_list(dot, e)?;
            }
        }
        self.vars.truncate(outer);
        Ok(Flow::Normal)
    }

    /// One pass of a range body; true when it ended with {{break}}.
    fn one_iteration(
        &mut self,
        b: &'t Branch,
        index: Val,
        elem: Val,
        mark: usize,
    ) -> EResult<bool> {
        let decl = &b.pipe.decl;
        if !decl.is_empty() {
            if b.pipe.is_assign {
                let v = if decl.len() > 1 {
                    index.clone()
                } else {
                    elem.clone()
                };
                self.set_var(&decl[0].ident[0], v)?;
            } else {
                self.set_top_var(1, elem.clone());
            }
        }
        if decl.len() > 1 {
            if b.pipe.is_assign {
                self.set_var(&decl[1].ident[0], elem.clone())?;
            } else {
                self.set_top_var(2, index);
            }
        }
        let flow = self.walk_list(&elem, &b.list);
        self.vars.truncate(mark);
        Ok(flow? == Flow::Break)
    }

    fn walk_template(
        &mut self,
        dot: &Val,
        node: &'t Node,
        name: &'t str,
        pipe: Option<&'t Pipe>,
    ) -> EResult<Flow> {
        self.at(NodeRef::Node(node));
        let tree = match self.tmpl.trees.get(name) {
            Some(t) => t,
            None => {
                let q = quote_with(name, '"', false);
                return Err(self.errorf(&format!("template {} not defined", q)));
            }
        };
        if self.depth == MAX_EXEC_DEPTH {
            return Err(self.errorf(&format!(
                "exceeded maximum template depth ({})",
                MAX_EXEC_DEPTH
            )));
        }
        let dot = self.eval_pipeline(dot, pipe)?;
        let vars = std::mem::replace(&mut self.vars, vec![("$", dot.clone())]);
        let outer_name = std::mem::replace(&mut self.name, name);
        let outer_node = self.node;
        self.depth += 1;
        let r = self.walk_list(&dot, tree);
        self.depth -= 1;
        self.name = outer_name;
        self.vars = vars;
        self.node = outer_node;
        r?;
        Ok(Flow::Normal)
    }

    fn eval_pipeline(&mut self, dot: &Val, pipe: Option<&'t Pipe>) -> EResult<Val> {
        let pipe = match pipe {
            None => return Ok(Val::invalid()),
            Some(p) => p,
        };
        self.at(NodeRef::Pipe(pipe));
        let mut value: Option<Val> = None;
        for cmd in &pipe.cmds {
            let v = self.eval_command(dot, cmd, value.as_ref())?;
            // A command's result loses its interface{} wrapper.
            value = Some(v.deref());
        }
        let value = value.unwrap_or_else(Val::invalid);
        for var in &pipe.decl {
            if pipe.is_assign {
                self.set_var(&var.ident[0], value.clone())?;
            } else {
                self.vars.push((&var.ident[0], value.clone()));
            }
        }
        Ok(value)
    }

    fn not_a_function(&self, args: &[Arg], fin: Option<&Val>) -> EResult<()> {
        if args.len() > 1 || fin.is_some() {
            let s = args.first().map(arg_string).unwrap_or_default();
            return Err(self.errorf(&format!("can't give argument to non-function {}", s)));
        }
        Ok(())
    }

    fn eval_command(&mut self, dot: &Val, cmd: &'t Cmd, fin: Option<&Val>) -> EResult<Val> {
        let first = match cmd.args.first() {
            Some(a) => a,
            None => return Ok(Val::invalid()),
        };
        match first {
            Arg::Field { ident, .. } => {
                return self.eval_field_node(dot, first, ident, &cmd.args, fin)
            }
            Arg::Chain { .. } => return self.eval_chain_node(dot, first, &cmd.args, fin),
            Arg::Ident { name, .. } => {
                return self.eval_function(
                    dot,
                    first,
                    name,
                    NodeRef::Cmd(cmd),
                    Some(&cmd.args),
                    fin,
                )
            }
            Arg::Pipe(p) => {
                self.not_a_function(&cmd.args, fin)?;
                return self.eval_pipeline(dot, Some(p));
            }
            Arg::Var(v) => return self.eval_variable_node(first, v, &cmd.args, fin),
            _ => {}
        }
        self.at(NodeRef::Arg(first));
        self.not_a_function(&cmd.args, fin)?;
        match first {
            Arg::Bool { val, .. } => Ok(Val::plain(V::Bool(*val))),
            Arg::Dot { .. } => Ok(dot.clone()),
            Arg::Nil { .. } => Err(self.errorf("nil is not a command")),
            Arg::Number { num, .. } => self.ideal_constant(first, num),
            Arg::Str { text, .. } => Ok(Val::plain(V::Str(text.clone()))),
            _ => {
                let q = quote_with(&arg_string(first), '"', false);
                Err(self.errorf(&format!("can't evaluate command {}", q)))
            }
        }
    }

    fn ideal_constant(&mut self, node: &'t Arg, n: &Number) -> EResult<Val> {
        self.at(NodeRef::Arg(node));
        let t = &n.text;
        let hex_int = t.len() > 2
            && t.starts_with('0')
            && matches!(t.as_bytes()[1], b'x' | b'X')
            && !t.contains(['p', 'P']);
        let rune_int = t.starts_with('\'');
        if n.is_complex {
            return Ok(Val::plain(V::Complex(n.complex.0, n.complex.1)));
        }
        if n.is_float && !hex_int && !rune_int && t.contains(['.', 'e', 'E', 'p', 'P']) {
            return Ok(Val::plain(V::Float(n.float)));
        }
        if n.is_int {
            return Ok(Val::plain(V::Int(n.int)));
        }
        if n.is_uint {
            return Err(self.errorf(&format!("{} overflows int", t)));
        }
        Ok(Val::invalid())
    }

    fn eval_field_node(
        &mut self,
        dot: &Val,
        node: &'t Arg,
        ident: &'t [String],
        args: &'t [Arg],
        fin: Option<&Val>,
    ) -> EResult<Val> {
        self.at(NodeRef::Arg(node));
        self.eval_field_chain(dot.clone(), ident, args, fin)
    }

    fn eval_chain_node(
        &mut self,
        dot: &Val,
        node: &'t Arg,
        args: &'t [Arg],
        fin: Option<&Val>,
    ) -> EResult<Val> {
        self.at(NodeRef::Arg(node));
        let (inner, fields) = match node {
            Arg::Chain { node, field, .. } => (node.as_ref(), field),
            _ => return Ok(Val::invalid()),
        };
        if fields.is_empty() {
            return Err(self.errorf("internal error: no fields in evalChainNode"));
        }
        if let Arg::Nil { .. } = inner {
            let s = arg_string(node);
            return Err(self.errorf(&format!("indirection through explicit nil in {}", s)));
        }
        let receiver = self.eval_arg(dot, Pt::Untyped, inner)?;
        self.eval_field_chain(receiver, fields, args, fin)
    }

    fn eval_variable_node(
        &mut self,
        node: &'t Arg,
        var: &'t Var,
        args: &'t [Arg],
        fin: Option<&Val>,
    ) -> EResult<Val> {
        self.at(NodeRef::Arg(node));
        let value = self.var_value(&var.ident[0])?;
        if var.ident.len() == 1 {
            self.not_a_function(args, fin)?;
            return Ok(value);
        }
        self.eval_field_chain(value, &var.ident[1..], args, fin)
    }

    fn eval_field_chain(
        &mut self,
        receiver: Val,
        ident: &'t [String],
        args: &'t [Arg],
        fin: Option<&Val>,
    ) -> EResult<Val> {
        let mut receiver = receiver;
        let n = ident.len();
        for name in &ident[..n.saturating_sub(1)] {
            receiver = self.eval_field(name, &[], None, receiver)?;
        }
        match ident.last() {
            Some(name) => self.eval_field(name, args, fin, receiver),
            None => Ok(receiver),
        }
    }

    fn eval_field(
        &mut self,
        name: &str,
        args: &[Arg],
        fin: Option<&Val>,
        receiver: Val,
    ) -> EResult<Val> {
        if receiver.is_invalid() {
            return Ok(Val::invalid());
        }
        let typ = receiver.type_name();
        if receiver.iface && receiver.v.is_nil() {
            return Err(self.errorf(&format!("nil pointer evaluating {}.{}", typ, name)));
        }
        if let V::Map(m, strmap) = &receiver.v {
            if args.len() > 1 || fin.is_some() {
                return Err(self.errorf(&format!("{} is not a method but has arguments", name)));
            }
            return match m.get(name) {
                Some(e) => Ok(Val::elem(e, !strmap)),
                None if self.strict && !strmap => {
                    Err(self.errorf(&format!("can't evaluate field {} in type {}", name, typ)))
                }
                None => Ok(Val::invalid()),
            };
        }
        Err(self.errorf(&format!("can't evaluate field {} in type {}", name, typ)))
    }

    fn eval_function(
        &mut self,
        dot: &Val,
        ident: &'t Arg,
        name: &str,
        node: NodeRef<'t>,
        args: Option<&'t [Arg]>,
        fin: Option<&Val>,
    ) -> EResult<Val> {
        self.at(NodeRef::Arg(ident));
        let f = match lookup_func(name, self.header) {
            Some(f) => f,
            None => {
                let q = quote_with(name, '"', false);
                return Err(self.errorf(&format!("{} is not a defined function", q)));
            }
        };
        self.eval_call(dot, f, node, name, args, fin)
    }

    fn eval_call(
        &mut self,
        dot: &Val,
        f: Func,
        node: NodeRef<'t>,
        name: &str,
        args: Option<&'t [Arg]>,
        fin: Option<&Val>,
    ) -> EResult<Val> {
        let args: &'t [Arg] = match args {
            Some(a) if !a.is_empty() => &a[1..],
            _ => &[],
        };
        let (fixed, variadic) = signature(f);
        let num_in = args.len() + fin.is_some() as usize;
        let num_fixed = if variadic.is_some() {
            if num_in < fixed.len() {
                return Err(self.errorf(&format!(
                    "wrong number of args for {}: want at least {} got {}",
                    name,
                    fixed.len(),
                    args.len()
                )));
            }
            fixed.len()
        } else {
            if num_in != fixed.len() {
                return Err(self.errorf(&format!(
                    "wrong number of args for {}: want {} got {}",
                    name,
                    fixed.len(),
                    num_in
                )));
            }
            args.len()
        };
        if f == Func::And || f == Func::Or {
            let mut v = Val::invalid();
            for arg in args {
                v = self.eval_arg(dot, Pt::Value, arg)?;
                if truth(&v.deref().v) == (f == Func::Or) {
                    return Ok(v);
                }
            }
            if let Some(fv) = fin {
                v = fv.clone();
            }
            return Ok(v);
        }
        let mut argv = Vec::with_capacity(num_in);
        let mut i = 0;
        while i < num_fixed && i < args.len() {
            argv.push(self.eval_arg(dot, fixed[i], &args[i])?);
            i += 1;
        }
        if let Some(vt) = variadic {
            while i < args.len() {
                argv.push(self.eval_arg(dot, vt, &args[i])?);
                i += 1;
            }
        }
        if let Some(fv) = fin {
            let t = match variadic {
                Some(vt) if num_in > num_fixed => vt,
                _ => fixed[num_in - 1],
            };
            argv.push(self.validate_type(fv, t)?);
        }
        let result = if f == Func::Call {
            let callee = match args.first() {
                Some(a) => arg_string(a),
                None => match fin.map(|v| &v.v) {
                    Some(V::Str(s)) => s.clone(),
                    Some(V::Invalid) | None => "<invalid Value>".to_string(),
                    Some(v) => format!("<{} Value>", v.type_name()),
                },
            };
            let fnv = argv[0].deref();
            if fnv.is_invalid() {
                Err("call of nil".to_string())
            } else {
                Err(format!(
                    "non-function {} of type {}",
                    callee,
                    fnv.type_name()
                ))
            }
        } else {
            call_func(f, &argv)
        };
        result.map_err(|e| {
            self.at(node);
            self.errorf(&format!("error calling {}: {}", name, e))
        })
    }

    fn validate_type(&self, value: &Val, typ: Pt) -> EResult<Val> {
        match typ {
            Pt::Untyped | Pt::Value => Ok(value.clone()),
            Pt::Any => Ok(if value.v.is_nil() {
                Val::plain(V::Nil)
            } else {
                value.deref()
            }),
            Pt::Str | Pt::Int => {
                if value.is_invalid() {
                    return Err(self.errorf(&format!("invalid value; expected {}", typ.name())));
                }
                let fits = |v: &Val| {
                    !v.iface && matches!((&v.v, typ), (V::Str(_), Pt::Str) | (V::Int(_), Pt::Int))
                };
                if fits(value) {
                    return Ok(value.clone());
                }
                let mut v = value.clone();
                if v.iface && !v.v.is_nil() {
                    v = v.deref();
                    if fits(&v) {
                        return Ok(v);
                    }
                }
                Err(self.errorf(&format!(
                    "wrong type for value; expected {}; got {}",
                    typ.name(),
                    v.type_name()
                )))
            }
        }
    }

    fn eval_arg(&mut self, dot: &Val, typ: Pt, n: &'t Arg) -> EResult<Val> {
        self.at(NodeRef::Arg(n));
        let value = match n {
            Arg::Dot { .. } => dot.clone(),
            Arg::Nil { .. } => {
                return match typ {
                    Pt::Str | Pt::Int => {
                        Err(self.errorf(&format!("cannot assign nil to {}", typ.name())))
                    }
                    Pt::Any => Ok(Val::plain(V::Nil)),
                    _ => Ok(Val::invalid()),
                }
            }
            Arg::Field { ident, .. } => {
                self.eval_field_node(dot, n, ident, std::slice::from_ref(n), None)?
            }
            Arg::Var(v) => self.eval_variable_node(n, v, &[], None)?,
            Arg::Pipe(p) => self.eval_pipeline(dot, Some(p))?,
            Arg::Ident { name, .. } => {
                self.eval_function(dot, n, name, NodeRef::Arg(n), None, None)?
            }
            Arg::Chain { .. } => self.eval_chain_node(dot, n, &[], None)?,
            Arg::Bool { val, .. } => match typ {
                Pt::Str => return Err(self.expected("string", n)),
                Pt::Int => return Err(self.expected("integer", n)),
                _ => return Ok(Val::plain(V::Bool(*val))),
            },
            Arg::Number { num, .. } => match typ {
                Pt::Str => return Err(self.expected("string", n)),
                Pt::Int if num.is_int => return Ok(Val::plain(V::Int(num.int))),
                Pt::Int => return Err(self.expected("integer", n)),
                _ => return self.ideal_constant(n, num),
            },
            Arg::Str { text, .. } => match typ {
                Pt::Int => return Err(self.expected("integer", n)),
                _ => return Ok(Val::plain(V::Str(text.clone()))),
            },
        };
        self.validate_type(&value, typ)
    }

    fn expected(&self, what: &str, n: &Arg) -> String {
        self.errorf(&format!("expected {}; found {}", what, arg_string(n)))
    }
}

// ---------------------------------------------------------------------------
// Public API

/// A parsed template with the reference's functions available.
#[derive(Debug)]
pub struct Template {
    text: String,
    trees: BTreeMap<String, List>,
}

impl Template {
    /// Parses a template as the reference's templates.Parse does; errors read like
    /// Go's, `template: :1: ...`.
    pub fn parse(text: &str) -> Result<Template, String> {
        let mut p = Parser {
            lex: Lexer::new(text),
            t: TreeState::new(),
            parse_name: String::new(),
            trees: BTreeMap::new(),
        };
        let root = p.parse()?;
        let line = p.t.token[0].line;
        p.add(String::new(), root, line)?;
        Ok(Template {
            text: text.to_string(),
            trees: p.trees,
        })
    }

    /// Executes the template over `data`; on error Go's message comes back
    /// and the partial output is dropped.
    pub fn execute(&self, data: &Json) -> Result<String, String> {
        let (out, err) = self.run_with(data, false, false);
        match err {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// `execute` over a row of a list, whose fields are all there is.
    fn execute_row(&self, data: &Json) -> Result<String, String> {
        let (out, err) = self.run_with(data, false, true);
        match err {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// The output and the error, if any, of one execution. `header` swaps in
    /// the header functions and reads an object of strings as the
    /// map[string]string the reference passes for a table's header row.
    fn run(&self, data: &Json, header: bool) -> (String, Option<String>) {
        self.run_with(data, header, false)
    }

    fn run_with(&self, data: &Json, header: bool, strict: bool) -> (String, Option<String>) {
        let dot = match data {
            Json::Null => Val::invalid(),
            Json::Object(o) if header && o.values().all(Json::is_string) => Val::plain(V::Map(
                Rc::new(
                    o.iter()
                        .map(|(k, v)| {
                            (
                                k.clone(),
                                V::Str(v.as_str().unwrap_or_default().to_string()),
                            )
                        })
                        .collect(),
                ),
                true,
            )),
            _ => Val::plain(V::from_json(data)),
        };
        let root = match self.trees.get("") {
            Some(r) => r,
            None => return (String::new(), None),
        };
        let mut e = Exec {
            tmpl: self,
            header,
            name: "",
            node: None,
            vars: vec![("$", dot.clone())],
            depth: 0,
            run_depth: 0,
            strict,
            out: String::new(),
        };
        let err = e.walk_list(&dot, root).err();
        (e.out, err)
    }
}

/// The `--format` of a list: "table" (`default_table`, itself starting
/// with "table "), "table TEMPLATE", "json" or a TEMPLATE. Returns the text to
/// print, every line ending in a newline; a table gets a header row, the
/// template run over `headers` (field name to column title) with the header
/// functions, and columns aligned as the reference's tabwriter aligns them.
/// Errors carry the "template parsing error: " prefix.
pub fn format_list(
    format: &str,
    default_table: &str,
    headers: &Json,
    rows: &[Json],
) -> Result<String, String> {
    let format = if format.is_empty() || format == "table" {
        if default_table.starts_with("table") {
            default_table.to_string()
        } else {
            format!("table {}", default_table)
        }
    } else {
        format.to_string()
    };
    let is_table = format.starts_with("table");
    let text = if format == "json" {
        "{{json .}}".to_string()
    } else {
        format
            .strip_prefix("table")
            .unwrap_or(&format)
            .trim_matches(' ')
            .replace("\\t", "\t")
            .replace("\\n", "\n")
    };
    let tmpl = Template::parse(&text).map_err(|e| format!("template parsing error: {}", e))?;
    let mut body = String::new();
    for row in rows {
        let line = tmpl
            .execute_row(row)
            .map_err(|e| format!("template parsing error: {}", e))?;
        body.push_str(&line);
        body.push('\n');
    }
    if !is_table {
        return Ok(body);
    }
    // An error in the header row is ignored, what it printed kept.
    let (header, _) = tmpl.run(headers, true);
    let mut tw = TabWriter::new(10, 3);
    tw.write(&header);
    tw.write("\n");
    tw.write(&body);
    Ok(tw.flush())
}

// ---------------------------------------------------------------------------
// The reference's text/tabwriter fork, with the settings it uses: padding with
// spaces, flags 0, cell widths in terminal columns.

#[derive(Clone, Copy, Default)]
struct Cell {
    size: usize,
    width: usize,
    htab: bool,
}

struct TabWriter {
    minwidth: usize,
    padding: usize,
    buf: String,
    pos: usize,
    cell: Cell,
    lines: Vec<Vec<Cell>>,
    widths: Vec<usize>,
    out: String,
}

/// Terminal columns of a character: none for combining marks and controls,
/// two for East Asian wide and fullwidth forms.
fn char_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7f..0xa0).contains(&u) {
        return 0;
    }
    if matches!(u, 0x300..=0x36F | 0x483..=0x489 | 0x591..=0x5BD | 0x610..=0x61A | 0x64B..=0x65F
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200B..=0x200F | 0x20D0..=0x20FF | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F)
    {
        return 0;
    }
    if matches!(u, 0x1100..=0x115F | 0x231A..=0x231B | 0x2329..=0x232A | 0x23E9..=0x23EC
        | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267F
        | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE
        | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B
        | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0
        | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x2E80..=0x303E | 0x3041..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x16FE0..=0x16FE4 | 0x17000..=0x18CFF
        | 0x1B000..=0x1B2FF | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A
        | 0x1F200..=0x1F251 | 0x1F300..=0x1F64F | 0x1F680..=0x1F6FF | 0x1F7E0..=0x1F7EB
        | 0x1F90C..=0x1F9FF | 0x1FA70..=0x1FAFF | 0x20000..=0x3FFFD)
    {
        return 2;
    }
    1
}

impl TabWriter {
    fn new(minwidth: usize, padding: usize) -> TabWriter {
        TabWriter {
            minwidth,
            padding,
            buf: String::new(),
            pos: 0,
            cell: Cell::default(),
            lines: vec![vec![]],
            widths: vec![],
            out: String::new(),
        }
    }

    fn write(&mut self, text: &str) {
        let mut n = 0;
        for (i, ch) in text.bytes().enumerate() {
            if matches!(ch, b'\t' | b'\x0b' | b'\n' | b'\x0c') {
                self.append(&text[n..i]);
                self.update_width();
                n = i + 1;
                let ncells = self.terminate_cell(ch == b'\t');
                if ch == b'\n' || ch == b'\x0c' {
                    self.lines.push(vec![]);
                    if ch == b'\x0c' || ncells == 1 {
                        self.flush_lines();
                    }
                }
            }
        }
        self.append(&text[n..]);
    }

    fn append(&mut self, s: &str) {
        self.buf.push_str(s);
        self.cell.size += s.len();
    }

    fn update_width(&mut self) {
        self.cell.width += self.buf[self.pos..].chars().map(char_width).sum::<usize>();
        self.pos = self.buf.len();
    }

    fn terminate_cell(&mut self, htab: bool) -> usize {
        self.cell.htab = htab;
        let cell = std::mem::take(&mut self.cell);
        match self.lines.last_mut() {
            Some(line) => {
                line.push(cell);
                line.len()
            }
            None => 0,
        }
    }

    fn flush(mut self) -> String {
        self.flush_lines();
        self.out
    }

    fn flush_lines(&mut self) {
        if self.cell.size > 0 {
            self.terminate_cell(false);
        }
        self.format(0, 0, self.lines.len());
        self.buf.clear();
        self.pos = 0;
        self.cell = Cell::default();
        self.lines = vec![vec![]];
        self.widths.clear();
    }

    fn write_lines(&mut self, pos0: usize, line0: usize, line1: usize) -> usize {
        let mut pos = pos0;
        for i in line0..line1 {
            for j in 0..self.lines[i].len() {
                let c = self.lines[i][j];
                if c.size > 0 {
                    self.out.push_str(&self.buf[pos..pos + c.size]);
                    pos += c.size;
                }
                if let Some(&w) = self.widths.get(j) {
                    self.out
                        .extend(std::iter::repeat_n(' ', w.saturating_sub(c.width)));
                }
            }
            if i + 1 == self.lines.len() {
                self.out.push_str(&self.buf[pos..pos + self.cell.size]);
                pos += self.cell.size;
            } else {
                self.out.push('\n');
            }
        }
        pos
    }

    /// Go's recursive format, a column at a time, with a stack of its own: a line can
    /// have more columns than a thread has stack.
    fn format(&mut self, pos0: usize, line0: usize, line1: usize) -> usize {
        struct Frame {
            line0: usize,
            line1: usize,
            this: usize,
        }
        let mut pos = pos0;
        let mut stack = vec![Frame {
            line0,
            line1,
            this: line0,
        }];
        while let Some(frame) = stack.last_mut() {
            // The frame's column is the one whose width is pushed last.
            let column = self.widths.len();
            let mut this = frame.this;
            while this < frame.line1 && column + 1 >= self.lines[this].len() {
                this += 1;
            }
            if this >= frame.line1 {
                let (line0, end) = (frame.line0, frame.line1);
                pos = self.write_lines(pos, line0, end);
                stack.pop();
                if let Some(parent) = stack.last_mut() {
                    self.widths.pop();
                    parent.line0 = end;
                    parent.this = end + 1;
                }
                continue;
            }
            pos = self.write_lines(pos, frame.line0, this);
            let block = this;
            let mut width = self.minwidth;
            while this < frame.line1 {
                let line = &self.lines[this];
                if column + 1 >= line.len() {
                    break;
                }
                width = width.max(line[column].width + self.padding);
                this += 1;
            }
            frame.line0 = block;
            self.widths.push(width);
            stack.push(Frame {
                line0: block,
                line1: this,
                this: block,
            });
        }
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The expectations below are what Go 1.26's text/template with the reference's
    // functions gives for the same template and data, JSON integers decoded
    // as int. Three documented differences are kept: the depth limits, slice
    // bounds checked against the length rather than a capacity, and a field
    // missing from a row of a list an error, as one missing from the
    // reference's structs is.

    /// Each case: template, data as JSON, and "ok:" with the output, "err:"
    /// with the execution error or "perr:" with the parse error.
    fn check(cases: &[(&str, &str, &str)]) {
        let mut failures = Vec::new();
        for (text, data, want) in cases {
            let data: Json = serde_json::from_str(data).unwrap();
            let got = match Template::parse(text) {
                Err(e) => format!("perr:{e}"),
                Ok(t) => match t.execute(&data) {
                    Ok(o) => format!("ok:{o}"),
                    Err(e) => format!("err:{e}"),
                },
            };
            if got != *want {
                failures.push(format!("{text:?}\n  want {want:?}\n  got  {got:?}"));
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    /// Each case: format, rows as JSON and the expected text or error, with
    /// DEFAULT as the default table and HDR as the column titles.
    fn check_list(cases: &[(&str, &str, &str)]) {
        let hdr: Json = serde_json::from_str(HDR).unwrap();
        let mut failures = Vec::new();
        for (format, rows, want) in cases {
            let rows: Vec<Json> = serde_json::from_str(rows).unwrap();
            let got = match format_list(format, DEFAULT, &hdr, &rows) {
                Ok(o) => format!("ok:{o}"),
                Err(e) => format!("err:{e}"),
            };
            if got != *want {
                failures.push(format!("{format:?}\n  want {want:?}\n  got  {got:?}"));
            }
        }
        assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    }

    fn lex(input: &str) -> Vec<(It, String)> {
        let mut l = Lexer::new(input);
        let mut out = vec![];
        loop {
            let i = l.next_item();
            let done = matches!(i.typ, It::Eof | It::Error);
            out.push((i.typ, i.val));
            if done {
                return out;
            }
        }
    }

    /// Cases of Go's lex_test.go; comments are skipped as text/template does.
    #[test]
    fn lexer_items() {
        use It::{
            Bool, Char, CharConstant, Complex, Declare, Dot, Else, End, Eof, Error, Field,
            Identifier, If, LeftDelim as L, LeftParen, Number, Pipe, Range, RawString,
            RightDelim as R, RightParen, Space, Str, Text, Variable, With,
        };
        let cases: Vec<(&str, Vec<(It, &str)>)> = vec![
            ("", vec![(Eof, "")]),
            (" \t\n", vec![(Text, " \t\n"), (Eof, "")]),
            (
                "hello-{{/* this is a comment */}}-world",
                vec![(Text, "hello-"), (Text, "-world"), (Eof, "")],
            ),
            (
                "{{,@% }}",
                vec![
                    (L, "{{"),
                    (Char, ","),
                    (Char, "@"),
                    (Char, "%"),
                    (Space, " "),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{((3))}}",
                vec![
                    (L, "{{"),
                    (LeftParen, "("),
                    (LeftParen, "("),
                    (Number, "3"),
                    (RightParen, ")"),
                    (RightParen, ")"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            ("{{}}", vec![(L, "{{"), (R, "}}"), (Eof, "")]),
            (
                "{{block \"foo\" .}}",
                vec![
                    (L, "{{"),
                    (It::Block, "block"),
                    (Space, " "),
                    (Str, "\"foo\""),
                    (Space, " "),
                    (Dot, "."),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{`raw\nquote`}}",
                vec![(L, "{{"), (RawString, "`raw\nquote`"), (R, "}}"), (Eof, "")],
            ),
            (
                "{{1 02 0x14 -7.2i 1e3 +1.2e-4 1+2i 1_2 0x1.e_fp4}}",
                vec![
                    (L, "{{"),
                    (Number, "1"),
                    (Space, " "),
                    (Number, "02"),
                    (Space, " "),
                    (Number, "0x14"),
                    (Space, " "),
                    (Number, "-7.2i"),
                    (Space, " "),
                    (Number, "1e3"),
                    (Space, " "),
                    (Number, "+1.2e-4"),
                    (Space, " "),
                    (Complex, "1+2i"),
                    (Space, " "),
                    (Number, "1_2"),
                    (Space, " "),
                    (Number, "0x1.e_fp4"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{'a' '\\n' '\\'' '\\u00FF' '\u{672c}'}}",
                vec![
                    (L, "{{"),
                    (CharConstant, "'a'"),
                    (Space, " "),
                    (CharConstant, "'\\n'"),
                    (Space, " "),
                    (CharConstant, "'\\''"),
                    (Space, " "),
                    (CharConstant, "'\\u00FF'"),
                    (Space, " "),
                    (CharConstant, "'\u{672c}'"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{true false}}",
                vec![
                    (L, "{{"),
                    (Bool, "true"),
                    (Space, " "),
                    (Bool, "false"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{.x . .2 .x.y.z}}",
                vec![
                    (L, "{{"),
                    (Field, ".x"),
                    (Space, " "),
                    (Dot, "."),
                    (Space, " "),
                    (Number, ".2"),
                    (Space, " "),
                    (Field, ".x"),
                    (Field, ".y"),
                    (Field, ".z"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{range if else end with}}",
                vec![
                    (L, "{{"),
                    (Range, "range"),
                    (Space, " "),
                    (If, "if"),
                    (Space, " "),
                    (Else, "else"),
                    (Space, " "),
                    (End, "end"),
                    (Space, " "),
                    (With, "with"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{$c := printf $ $hello $23 $var.Field .Method}}",
                vec![
                    (L, "{{"),
                    (Variable, "$c"),
                    (Space, " "),
                    (Declare, ":="),
                    (Space, " "),
                    (Identifier, "printf"),
                    (Space, " "),
                    (Variable, "$"),
                    (Space, " "),
                    (Variable, "$hello"),
                    (Space, " "),
                    (Variable, "$23"),
                    (Space, " "),
                    (Variable, "$var"),
                    (Field, ".Field"),
                    (Space, " "),
                    (Field, ".Method"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "intro {{echo hi 1.2 |noargs|args 1 \"hi\"}} outro",
                vec![
                    (Text, "intro "),
                    (L, "{{"),
                    (Identifier, "echo"),
                    (Space, " "),
                    (Identifier, "hi"),
                    (Space, " "),
                    (Number, "1.2"),
                    (Space, " "),
                    (Pipe, "|"),
                    (Identifier, "noargs"),
                    (Pipe, "|"),
                    (Identifier, "args"),
                    (Space, " "),
                    (Number, "1"),
                    (Space, " "),
                    (Str, "\"hi\""),
                    (R, "}}"),
                    (Text, " outro"),
                    (Eof, ""),
                ],
            ),
            (
                "{{$v , $w := 3}}",
                vec![
                    (L, "{{"),
                    (Variable, "$v"),
                    (Space, " "),
                    (Char, ","),
                    (Space, " "),
                    (Variable, "$w"),
                    (Space, " "),
                    (Declare, ":="),
                    (Space, " "),
                    (Number, "3"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "{{(.X).Y}}",
                vec![
                    (L, "{{"),
                    (LeftParen, "("),
                    (Field, ".X"),
                    (RightParen, ")"),
                    (Field, ".Y"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "hello- {{- 3 -}} -world",
                vec![
                    (Text, "hello-"),
                    (L, "{{"),
                    (Number, "3"),
                    (R, "}}"),
                    (Text, "-world"),
                    (Eof, ""),
                ],
            ),
            (
                "hello- {{- /* hello */ -}} -world",
                vec![(Text, "hello-"), (Text, "-world"), (Eof, "")],
            ),
            (
                "#{{\u{1}}}",
                vec![
                    (Text, "#"),
                    (L, "{{"),
                    (Error, "unrecognized character in action: U+0001"),
                ],
            ),
            ("{{", vec![(L, "{{"), (Error, "unclosed action")]),
            (
                "{{range",
                vec![(L, "{{"), (Range, "range"), (Error, "unclosed action")],
            ),
            (
                "{{\"\n\"}}",
                vec![(L, "{{"), (Error, "unterminated quoted string")],
            ),
            (
                "{{`xx}}",
                vec![(L, "{{"), (Error, "unterminated raw quoted string")],
            ),
            (
                "{{'\n}}",
                vec![(L, "{{"), (Error, "unterminated character constant")],
            ),
            (
                "{{3k}}",
                vec![(L, "{{"), (Error, "bad number syntax: \"3k\"")],
            ),
            (
                "{{(3}}",
                vec![
                    (L, "{{"),
                    (LeftParen, "("),
                    (Number, "3"),
                    (Error, "unclosed left paren"),
                ],
            ),
            (
                "{{3)}}",
                vec![(L, "{{"), (Number, "3"), (Error, "unexpected right paren")],
            ),
            (
                "{{|||||}}",
                vec![
                    (L, "{{"),
                    (Pipe, "|"),
                    (Pipe, "|"),
                    (Pipe, "|"),
                    (Pipe, "|"),
                    (Pipe, "|"),
                    (R, "}}"),
                    (Eof, ""),
                ],
            ),
            (
                "hello-{{/*/}}-world",
                vec![(Text, "hello-"), (Error, "unclosed comment")],
            ),
            (
                "hello-{{/* */ }}-world",
                vec![
                    (Text, "hello-"),
                    (Error, "comment ends before closing delimiter"),
                ],
            ),
            (
                "hello-{.}}-world",
                vec![(Text, "hello-{.}}-world"), (Eof, "")],
            ),
        ];
        for (input, want) in cases {
            let want: Vec<(It, String)> =
                want.into_iter().map(|(t, v)| (t, v.to_string())).collect();
            assert_eq!(lex(input), want, "{input:?}");
        }
    }

    /// The reference's TestHeaderFunctions: in a header row the functions that would
    /// change a column title return it unchanged.
    #[test]
    fn header_functions_return_their_argument() {
        let data = Json::String("hello world".to_string());
        for text in [
            "{{ json .}}",
            "{{ split . \",\"}}",
            "{{ join . \",\"}}",
            "{{ title .}}",
            "{{ lower .}}",
            "{{ upper .}}",
            "{{ truncate . 2}}",
        ] {
            let t = Template::parse(text).unwrap();
            assert_eq!(
                t.run(&data, true),
                ("hello world".to_string(), None),
                "{text}"
            );
        }
        let t = Template::parse("{{pad . 1 1}}").unwrap();
        assert_eq!(t.run(&data, true).0, " hello world ");
    }

    #[test]
    fn bare_table_uses_the_default() {
        let hdr: Json = serde_json::from_str(HDR).unwrap();
        let rows: Vec<Json> = serde_json::from_str(ROWS).unwrap();
        let want = format_list("table {{.Names}}\\t{{.ID}}", "", &hdr, &rows).unwrap();
        assert_eq!(
            format_list("table", "table {{.Names}}\\t{{.ID}}", &hdr, &rows).unwrap(),
            want
        );
        assert_eq!(
            format_list("table", "{{.Names}}\\t{{.ID}}", &hdr, &rows).unwrap(),
            want
        );
        assert_eq!(
            format_list("", "{{.Names}}\\t{{.ID}}", &hdr, &rows).unwrap(),
            want
        );
        assert_eq!(want, "NAMES                CONTAINER ID\nweb                  3f4e8a1b2c3d\na-much-longer-name   9a8b7c6d5e4f\n");
    }

    #[test]
    fn hostile_literals_and_widths_are_errors_or_output() {
        for text in [r#"{{"\é"}}"#, r#"{{'\é'}}"#] {
            assert!(Template::parse(text).is_err(), "{text}");
        }
        let out = |text: &str| Template::parse(text).unwrap().execute(&Json::Null).unwrap();
        let f = out(r#"{{printf "%.70000f" 1.5}}"#);
        assert_eq!(f.len(), 70002);
        assert!(f.starts_with("1.5000") && f.ends_with('0'));
        let e = out(r#"{{printf "%.70000e" 1.5}}"#);
        assert!(
            e.starts_with("1.5000") && e.ends_with("e+00"),
            "{}",
            &e[..10]
        );
        let u = out(r#"{{printf "%.70000U" 65}}"#);
        assert_eq!(u.len(), 70002);
        assert!(u.starts_with("U+000") && u.ends_with("41"));
        assert_eq!(out("{{0x.1p-2147483648}}"), "0");
        // A row with more columns than a stack has frames for.
        let rows = vec![Json::Null];
        let table = format_list("table {{range 100000}}\\t{{end}}x", "", &Json::Null, &rows);
        assert!(table.is_ok());
    }

    #[test]
    fn nesting_limits() {
        let nested = |n: usize| format!("{{{{{}1{}}}}}", "(".repeat(n), ")".repeat(n));
        let t = Template::parse(&nested(MAX_PAREN_DEPTH)).unwrap();
        assert_eq!(t.execute(&Json::Null).unwrap(), "1");
        assert_eq!(
            Template::parse(&nested(MAX_PAREN_DEPTH + 1)).unwrap_err(),
            "template: :1: max expression depth exceeded"
        );
        let chain = |n: usize| {
            let mut s: String = (0..n)
                .map(|i| {
                    format!(
                        "{{{{define \"t{i}\"}}}}{{{{template \"t{}\" .}}}}{{{{end}}}}",
                        i + 1
                    )
                })
                .collect();
            s.push_str(&format!(
                "{{{{define \"t{n}\"}}}}{{{{.}}}}{{{{end}}}}{{{{template \"t0\" 5}}}}"
            ));
            s
        };
        // Controls nested beyond the limit, and nested through template calls.
        let ifs = |n: usize| format!("{}x{}", "{{if .}}".repeat(n), "{{end}}".repeat(n));
        assert_eq!(
            Template::parse(&ifs(MAX_CONTROL_DEPTH))
                .unwrap()
                .execute(&Json::Bool(true))
                .unwrap(),
            "x"
        );
        assert_eq!(
            Template::parse(&ifs(4000)).unwrap_err(),
            "template: :1: max nesting depth exceeded"
        );
        let deep = format!(
            "{{{{define \"r\"}}}}{}{{{{template \"r\" .}}}}{}{{{{end}}}}{{{{template \"r\" .}}}}",
            "{{if .}}".repeat(MAX_CONTROL_DEPTH - 1),
            "{{end}}".repeat(MAX_CONTROL_DEPTH - 1)
        );
        // Run where the command line runs it, on a thread of the size of a main one.
        let err = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || Template::parse(&deep).unwrap().execute(&Json::Bool(true)))
            .unwrap()
            .join()
            .unwrap()
            .unwrap_err();
        assert!(err.contains("exceeded maximum"), "{err}");
        let t = Template::parse(&chain(MAX_EXEC_DEPTH - 1)).unwrap();
        assert_eq!(t.execute(&Json::Null).unwrap(), "5");
        let t = Template::parse(&chain(MAX_EXEC_DEPTH)).unwrap();
        assert!(t
            .execute(&Json::Null)
            .unwrap_err()
            .ends_with("exceeded maximum template depth (100)"));
    }

    #[test]
    fn float_formatting() {
        let cases: &[(f64, u8, i32, &str)] = &[
            (0.0, b'g', -1, "0"),
            (-0.0, b'g', -1, "-0"),
            (1234567.0, b'g', -1, "1.234567e+06"),
            (123456.0, b'g', -1, "123456"),
            (1e21, b'g', -1, "1e+21"),
            (1e-5, b'g', -1, "1e-05"),
            (0.0001, b'g', -1, "0.0001"),
            (5e-324, b'g', -1, "5e-324"),
            (1.7976931348623157e308, b'g', -1, "1.7976931348623157e+308"),
            (100.0, b'e', -1, "1e+02"),
            (100.0, b'f', -1, "100"),
            (2.5, b'f', 0, "2"),
            (3.5, b'f', 0, "4"),
            (1234.0, b'g', 3, "1.23e+03"),
            (100.0, b'g', 3, "100"),
            (1.0, b'e', 3, "1.000e+00"),
            (1.5, b'b', -1, "6755399441055744p-52"),
            (1.5, b'x', -1, "0x1.8p+00"),
            (1.0, b'x', 0, "0x1p+00"),
            (f64::NAN, b'g', -1, "NaN"),
            (f64::NEG_INFINITY, b'g', -1, "-Inf"),
        ];
        for &(v, fmt, prec, want) in cases {
            assert_eq!(
                append_float(v, fmt, prec),
                want,
                "{v} {} {prec}",
                fmt as char
            );
        }
    }

    /// Checked against the reference's tabwriter with the settings it uses.
    #[test]
    fn tabwriter_columns() {
        let mut tw = TabWriter::new(10, 3);
        tw.write("A\tBB\tC\nlonger value\tx\ty\n\nno tabs here\n");
        tw.write("\u{672c}\u{672c}\tz\n");
        assert_eq!(
            tw.flush(),
            "A              BB        C\nlonger value   x         y\n\nno tabs here\n\u{672c}\u{672c}      z\n"
        );
    }

    const T: &str = "{\"True\":true,\"I\":17,\"U16\":16,\"X\":\"x\",\"S\":\"xyz\",\"U\":{\"V\":\"v\"},\"SI\":[3,4,5],\"SIEmpty\":[],\"SB\":[true,false],\"AI\":[3,4,5],\"MSI\":{\"one\":1,\"two\":2,\"three\":3},\"MSIone\":{\"one\":1},\"MSIEmpty\":{},\"MXI\":{\"one\":1},\"SMSI\":[{\"one\":1,\"two\":2},{\"eleven\":11,\"twelve\":12}],\"Empty0\":null,\"Empty1\":3,\"Empty2\":\"empty2\",\"Empty3\":[7,8],\"Empty4\":{\"V\":\"UinEmpty\"},\"PI\":23,\"PS\":\"a string\",\"PSI\":[21,22,23],\"NIL\":null,\"FloatZero\":0.0,\"Float\":1.5,\"Big\":1234567,\"BigF\":1234567.5,\"Neg\":-3,\"Nested\":{\"A\":{\"B\":{\"C\":\"deep\"}}},\"Names\":[\"a\",\"b\"],\"Labels\":{\"x\":\"1\",\"y\":\"2\"},\"Mixed\":[1,\"two\",null,true,2.5]}";
    const HDR: &str = "{\"ID\":\"CONTAINER ID\",\"Image\":\"IMAGE\",\"Names\":\"NAMES\",\"Status\":\"STATUS\",\"Size\":\"SIZE\",\"Labels\":\"LABELS\"}";
    const ROWS: &str = "[{\"ID\":\"3f4e8a1b2c3d\",\"Image\":\"nginx:latest\",\"Names\":\"web\",\"Status\":\"Up 2 hours\",\"Size\":1234567,\"Labels\":\"a=b\"},{\"ID\":\"9a8b7c6d5e4f\",\"Image\":\"fedora\",\"Names\":\"a-much-longer-name\",\"Status\":\"Exited (0) 3 days ago\",\"Size\":12,\"Labels\":\"\"}]";
    const DEFAULT: &str = "table {{.ID}}\\t{{.Image}}\\t{{.Names}}\\t{{.Status}}";

    /// Fields on maps, missing keys, printing of each kind of value.
    #[test]
    fn exec_basics() {
        check(&[
            ("", "null", "ok:"),
            ("some text", "null", "ok:some text"),
            ("{{nil}}", "null", "err:template: :1:2: executing \"\" at <nil>: nil is not a command"),
            ("-{{.X}}-", T, "ok:-x-"),
            ("-{{.U.V}}-", T, "ok:-v-"),
            ("{{.MSI.one}}", T, "ok:1"),
            ("{{.MSI.two}}", T, "ok:2"),
            ("{{.MSI.NO}}", T, "ok:<no value>"),
            ("{{.MXI.one}}", T, "ok:1"),
            ("{{.MSI.one 1}}", T, "err:template: :1:6: executing \"\" at <.MSI.one>: one is not a method but has arguments"),
            ("<{{.}}>", "13", "ok:<13>"),
            ("<{{.}}>", "15.1", "ok:<15.1>"),
            ("<{{.}}>", "true", "ok:<true>"),
            ("<{{.}}>", "\"hello\"", "ok:<hello>"),
            ("<{{.}}>", "[-1,-2,-3]", "ok:<[-1 -2 -3]>"),
            ("<{{.}}>", "{\"two\":22}", "ok:<map[two:22]>"),
            ("<{{.}}>", "null", "ok:<<no value>>"),
            ("{{.PI}}", T, "ok:23"),
            ("{{.PS}}", T, "ok:a string"),
            ("{{.PSI}}", T, "ok:[21 22 23]"),
            ("{{index .PSI 1}}", T, "ok:22"),
            ("{{.NIL}}", T, "ok:<no value>"),
            ("{{.Empty0}}", T, "ok:<no value>"),
            ("{{.Empty1}}", T, "ok:3"),
            ("{{.Empty2}}", T, "ok:empty2"),
            ("{{.Empty3}}", T, "ok:[7 8]"),
            ("{{.Empty4}}", T, "ok:map[V:UinEmpty]"),
            ("{{.Empty4.V}}", T, "ok:UinEmpty"),
            ("{{.foo}}", "null", "ok:<no value>"),
            ("{{(.).foo}}", "null", "ok:<no value>"),
            ("{{1 2}}", "null", "err:template: :1:2: executing \"\" at <1>: can't give argument to non-function 1"),
            ("{{(1) 2}}", "null", "err:template: :1:2: executing \"\" at <(1) 2>: can't give argument to non-function 1"),
            ("{{(1)}}", "null", "ok:1"),
            ("{{.Nested.A.B.C}}", T, "ok:deep"),
            ("{{.Nested.A.X.C}}", T, "ok:<no value>"),
            ("{{.Missing.X.Y}}", T, "ok:<no value>"),
            ("{{.Mixed}}", T, "ok:[1 two <nil> true 2.5]"),
            ("{{.MSI}}", T, "ok:map[one:1 three:3 two:2]"),
            ("{{.}}", "{\"b\":[1,{\"c\":null}],\"a\":\"x\"}", "ok:map[a:x b:[1 map[c:<nil>]]]"),
            ("{{.Big}} {{.BigF}} {{.Float}} {{.Neg}}", T, "ok:1234567 1.2345675e+06 1.5 -3"),
            ("{{.}}", "1e21", "ok:1e+21"),
            ("{{.}}", "1e-7", "ok:1e-07"),
            ("{{.}}", "123456.0", "ok:123456"),
            ("{{.}}", "1000000.0", "ok:1e+06"),
            ("{{.}}", "0.000123", "ok:0.000123"),
            ("{{.}}", "-0.0", "ok:-0"),
            ("{{.}}", "18446744073709551615", "ok:1.8446744073709552e+19"),
            ("{{.}}", "3.0", "ok:3"),
            ("{{.A.B}}", "{\"A\":null}", "err:template: :1:4: executing \"\" at <.A.B>: nil pointer evaluating interface {}.B"),
            ("{{.A.B}}", "{\"A\":\"s\"}", "err:template: :1:4: executing \"\" at <.A.B>: can't evaluate field B in type interface {}"),
            ("{{.A.B}}", "{\"A\":1}", "err:template: :1:4: executing \"\" at <.A.B>: can't evaluate field B in type interface {}"),
            ("{{.B}}", "\"str\"", "err:template: :1:2: executing \"\" at <.B>: can't evaluate field B in type string"),
            ("{{.B}}", "[1]", "err:template: :1:2: executing \"\" at <.B>: can't evaluate field B in type []interface {}"),
            ("{{.x}} {{.y}}", "{\"x\":99}", "ok:99 <no value>"),
            ("{{.cause}}", "{\"cause\":\"neglect\"}", "ok:neglect"),
            ("{{(nil).True}}", T, "err:template: :1:3: executing \"\" at <nil>: nil is not a command"),
            ("{{$x := nil}}{{$x.anything}}", T, "err:template: :1:8: executing \"\" at <nil>: nil is not a command"),
            ("{{$x := (1.0)}}{{$y := (\"hello\")}}{{$x.anything}}{{$y.true}}", T, "err:template: :1:38: executing \"\" at <$x.anything>: can't evaluate field anything in type float64"),
            ("{{.X 1}}", T, "err:template: :1:2: executing \"\" at <.X>: X is not a method but has arguments"),
            ("{{3 2}}", T, "err:template: :1:2: executing \"\" at <3>: can't give argument to non-function 3"),
            ("{{$x := 1}}{{$x 2}}", T, "err:template: :1:13: executing \"\" at <$x>: can't give argument to non-function $x"),
            ("{{$x := 1}}{{3 | $x}}", T, "err:template: :1:17: executing \"\" at <$x>: can't give argument to non-function $x"),
            ("{{1 | (.X)}}", T, "err:template: :1:2: executing \"\" at <1>: can't give argument to non-function .X"),
            ("{{print (index .Mixed 2).A}}", T, "ok:<nil>"),
            ("{{(index .Mixed 1).A}}", T, "err:template: :1:16: executing \"\" at <1>: can't evaluate field A in type string"),
            ("{{(index .SMSI 0).one}}", T, "ok:1"),
            ("{{(.U).V}}", T, "ok:v"),
            ("{{($).X}}", T, "ok:x"),
            ("{{range .Mixed}}[{{.}}]{{end}}", T, "ok:[1][two][<no value>][true][2.5]"),
            ("{{range .Mixed}}{{.A}}{{end}}", T, "err:template: :1:18: executing \"\" at <.A>: can't evaluate field A in type interface {}"),
            ("{{with .X}}{{.A}}{{end}}", T, "err:template: :1:13: executing \"\" at <.A>: can't evaluate field A in type string"),
            ("{{with $x := .Mixed}}{{$x}}{{end}}", T, "ok:[1 two <nil> true 2.5]"),
        ]);
    }

    /// Declarations, assignments and their scopes.
    #[test]
    fn exec_variables() {
        check(&[
            ("{{$}}", "123", "ok:123"),
            ("{{$.I}}", T, "ok:17"),
            ("{{$.U.V}}", T, "ok:v"),
            ("{{$x := $.U.V}}{{$x}}", T, "ok:v"),
            ("{{$x := 2}}{{$x = 3}}{{$x}}", T, "ok:3"),
            ("{{$x := 2}}{{if true}}{{$x = 3}}{{end}}{{$x}}", T, "ok:3"),
            (
                "{{$x := 1}}{{if true}}{{$x := 2}}{{if true}}{{$x = 3}}{{end}}{{end}}{{$x}}",
                T,
                "ok:1",
            ),
            (
                "{{$x = 1}}",
                T,
                "err:template: :1:7: executing \"\" at <1>: undefined variable: $x",
            ),
            ("{{if $x := .X}}-{{$x}}{{end}}-", T, "ok:-x-"),
            ("{{with $x := .}}{{$x.U.V}}{{end}}", T, "ok:v"),
            ("{{with $x := $}}{{$y := $.U.V}}{{$y}}{{end}}", T, "ok:v"),
            ("{{$v := .Mixed}}{{$v}}", T, "ok:[1 two <nil> true 2.5]"),
            ("{{$v := .Empty0}}{{$v.X}}", T, "ok:<no value>"),
            (
                "{{range $i, $x := .Names}}{{$i}}{{$x}}{{$}}{{end}}",
                "{\"Names\":[\"a\"]}",
                "ok:0amap[Names:[a]]",
            ),
            ("{{$x := 0}}{{range $x = .SI}}{{end}}{{$x}}", T, "ok:5"),
            (
                "{{$i := 0}}{{$x := 0}}{{range $i = .AI}}{{end}}{{$i}}",
                T,
                "ok:5",
            ),
            (
                "{{$k := 0}}{{$v := 0}}{{range $k, $v = .AI}}{{$k}}={{$v}} {{end}}",
                T,
                "ok:0=3 1=4 2=5 ",
            ),
            (
                "{{$k := 0}}{{$v := 0}}{{range $k, $v = .MSI}}{{$k}}={{$v}} {{end}}{{$k}}",
                T,
                "ok:one=1 three=3 two=2 two",
            ),
        ]);
    }

    /// Truth of each kind of value, else, else if, else with.
    #[test]
    fn exec_if_with() {
        check(&[
            ("{{if true}}TRUE{{end}}", T, "ok:TRUE"),
            ("{{if false}}TRUE{{else}}FALSE{{end}}", T, "ok:FALSE"),
            (
                "{{if nil}}TRUE{{end}}",
                T,
                "err:template: :1:5: executing \"\" at <nil>: nil is not a command",
            ),
            ("{{if 1}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:NON-ZERO"),
            ("{{if 0}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:ZERO"),
            ("{{if 1.5}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:NON-ZERO"),
            ("{{if .FloatZero}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:ZERO"),
            ("{{if 1.5i}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:NON-ZERO"),
            ("{{if .PI}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:NON-ZERO"),
            ("{{if .NIL}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:ZERO"),
            ("{{if ``}}NON-EMPTY{{else}}EMPTY{{end}}", T, "ok:EMPTY"),
            (
                "{{if `notempty`}}NON-EMPTY{{else}}EMPTY{{end}}",
                T,
                "ok:NON-EMPTY",
            ),
            (
                "{{if .SIEmpty}}NON-EMPTY{{else}}EMPTY{{end}}",
                T,
                "ok:EMPTY",
            ),
            ("{{if .SI}}NON-EMPTY{{else}}EMPTY{{end}}", T, "ok:NON-EMPTY"),
            (
                "{{if .MSIEmpty}}NON-EMPTY{{else}}EMPTY{{end}}",
                T,
                "ok:EMPTY",
            ),
            (
                "{{if .MSI}}NON-EMPTY{{else}}EMPTY{{end}}",
                T,
                "ok:NON-EMPTY",
            ),
            ("{{if .MXI.none}}NON-ZERO{{else}}ZERO{{end}}", T, "ok:ZERO"),
            (
                "{{if not .MXI.none}}ZERO{{else}}NON-ZERO{{end}}",
                T,
                "ok:ZERO",
            ),
            (
                "{{if $x := true}}{{with $y := .I}}{{$x}},{{$y}}{{end}}{{end}}",
                T,
                "ok:true,17",
            ),
            (
                "{{if $x := true}}{{with $x := .I}}{{$x}},{{end}}{{$x}}{{end}}",
                T,
                "ok:17,true",
            ),
            ("{{if false}}FALSE{{else if true}}TRUE{{end}}", T, "ok:TRUE"),
            (
                "{{if eq 1 3}}1{{else if eq 2 3}}2{{else if eq 3 3}}3{{end}}",
                T,
                "ok:3",
            ),
            ("{{if .Empty0}}non-nil{{else}}nil{{end}}", T, "ok:nil"),
            ("{{with true}}{{.}}{{end}}", T, "ok:true"),
            ("{{with false}}{{.}}{{else}}FALSE{{end}}", T, "ok:FALSE"),
            ("{{with 1}}{{.}}{{else}}ZERO{{end}}", T, "ok:1"),
            ("{{with 0}}{{.}}{{else}}ZERO{{end}}", T, "ok:ZERO"),
            ("{{with 1.5}}{{.}}{{else}}ZERO{{end}}", T, "ok:1.5"),
            ("{{with .FloatZero}}{{.}}{{else}}ZERO{{end}}", T, "ok:ZERO"),
            ("{{with 1.5i}}{{.}}{{else}}ZERO{{end}}", T, "ok:(0+1.5i)"),
            ("{{with ``}}{{.}}{{else}}EMPTY{{end}}", T, "ok:EMPTY"),
            (
                "{{with `notempty`}}{{.}}{{else}}EMPTY{{end}}",
                T,
                "ok:notempty",
            ),
            ("{{with .SIEmpty}}{{.}}{{else}}EMPTY{{end}}", T, "ok:EMPTY"),
            ("{{with .SI}}{{.}}{{else}}EMPTY{{end}}", T, "ok:[3 4 5]"),
            ("{{with .MSIEmpty}}{{.}}{{else}}EMPTY{{end}}", T, "ok:EMPTY"),
            (
                "{{with .MSIone}}{{.}}{{else}}EMPTY{{end}}",
                T,
                "ok:map[one:1]",
            ),
            ("{{with .Empty4}}{{.V}}{{end}}", T, "ok:UinEmpty"),
            ("{{with $x := .I}}{{$x}}{{end}}", T, "ok:17"),
            (
                "{{with 0}}{{.}}{{else with true}}{{.}}{{end}}",
                T,
                "ok:true",
            ),
            (
                "{{with 0}}{{.}}{{else with false}}{{.}}{{else with `notempty`}}{{.}}{{end}}",
                T,
                "ok:notempty",
            ),
            ("{{with $}}{{.X}}{{end}}", T, "ok:x"),
            ("{{if .Missing}}y{{else}}n{{end}}", T, "ok:n"),
        ]);
    }

    /// Range over lists, maps and integers, with variables, break and continue.
    #[test]
    fn exec_range() {
        check(&[
            ("{{range .SI}}-{{.}}-{{end}}", T, "ok:-3--4--5-"),
            ("{{range .SIEmpty}}-{{.}}-{{end}}", T, "ok:"),
            ("{{range .SI}}-{{.}}-{{else}}EMPTY{{end}}", T, "ok:-3--4--5-"),
            ("{{range .SIEmpty}}-{{.}}-{{else}}EMPTY{{end}}", T, "ok:EMPTY"),
            ("{{range .SI}}-{{.}}-{{break}}NOTREACHED{{else}}EMPTY{{end}}", T, "ok:-3-"),
            ("{{range .SI}}-{{.}}-{{continue}}NOTREACHED{{else}}EMPTY{{end}}", T, "ok:-3--4--5-"),
            ("{{range .SB}}-{{.}}-{{end}}", T, "ok:-true--false-"),
            ("{{range .MSI}}-{{.}}-{{end}}", T, "ok:-1--3--2-"),
            ("{{range .MSIEmpty}}-{{.}}-{{end}}", T, "ok:"),
            ("{{range .MSI}}-{{.}}-{{else}}EMPTY{{end}}", T, "ok:-1--3--2-"),
            ("{{range .MSIEmpty}}-{{.}}-{{else}}EMPTY{{end}}", T, "ok:EMPTY"),
            ("{{range .Empty3}}-{{.}}-{{else}}EMPTY{{end}}", T, "ok:-7--8-"),
            ("{{range .Empty0}}-{{.}}-{{end}}", T, "ok:"),
            ("{{range $x := .SI}}<{{$x}}>{{end}}", T, "ok:<3><4><5>"),
            ("{{range $x, $y := .SI}}<{{$x}}={{$y}}>{{end}}", T, "ok:<0=3><1=4><2=5>"),
            ("{{range $x := .MSIone}}<{{$x}}>{{end}}", T, "ok:<1>"),
            ("{{range $x, $y := .MSIone}}<{{$x}}={{$y}}>{{end}}", T, "ok:<one=1>"),
            ("{{range $x := .PSI}}<{{$x}}>{{end}}", T, "ok:<21><22><23>"),
            ("{{range $x := .PSI}}<{{$foo:=$x}}{{$x}}>{{end}}", T, "ok:<21><22><23>"),
            ("{{range $v := 5}}{{printf \"%T%d\" $v $v}}{{end}}", "null", "ok:int0int1int2int3int4"),
            ("{{range 0}}x{{else}}empty{{end}}", "null", "ok:empty"),
            ("{{range $i, $c := 3}}{{$c}}{{end}}", "null", "err:template: :1:18: executing \"\" at <3>: can't use 3 to iterate over more than one variable"),
            ("{{range .}}{{.}}{{end}}", "3", "ok:012"),
            ("{{range .}}x{{end}}", "2.5", "err:template: :1:8: executing \"\" at <.>: range can't iterate over 2.5"),
            ("{{range .}}x{{end}}", "\"str\"", "err:template: :1:8: executing \"\" at <.>: range can't iterate over str"),
            ("{{range .}}x{{end}}", "true", "err:template: :1:8: executing \"\" at <.>: range can't iterate over true"),
            ("{{range .MSIone}}{{if $.U.V}}{{$.U.V}}{{else}}WRONG{{end}}{{end}}", T, "ok:v"),
            ("{{or .SI \"slice is empty\"}}", T, "ok:[3 4 5]"),
            ("{{or .SIEmpty \"slice is empty\"}}", T, "ok:slice is empty"),
            ("{{range .SI}}{{if eq . 4}}{{break}}{{end}}{{.}}{{end}}", T, "ok:3"),
            ("{{range .SI}}{{if eq . 4}}{{continue}}{{end}}{{.}}{{end}}", T, "ok:35"),
            ("{{range .SMSI}}{{range $k, $v := .}}{{$k}}={{$v}};{{end}}{{end}}", T, "ok:one=1;two=2;eleven=11;twelve=12;"),
            ("{{range $i, $e := .Mixed}}{{$i}}:{{$e}} {{end}}", T, "ok:0:1 1:two 2:<no value> 3:true 4:2.5 "),
            ("{{range .Labels}}{{.}}{{end}}", T, "ok:12"),
            ("{{range $k, $v := .Labels}}{{$k}}{{$v}}{{end}}", T, "ok:x1y2"),
            ("{{range .SI}}{{$x := .}}{{end}}{{$x}}", T, "perr:template: :1: undefined variable \"$x\""),
        ]);
    }

    /// define, template and block.
    #[test]
    fn exec_templates() {
        check(&[
            ("{{define \"T1\"}}ONE{{end}}{{define \"T2\"}}TWO{{end}}{{define \"T3\"}}{{template \"T1\"}} {{template \"T2\"}}{{end}}{{template \"T3\"}}", "null", "ok:ONE TWO"),
            ("{{define \"T\"}}[{{.}}]{{end}}{{template \"T\" .X}}{{template \"T\" .I}}{{template \"T\"}}", T, "ok:[x][17][<no value>]"),
            ("{{define \"T\"}}{{$}}{{end}}{{$x := 1}}{{template \"T\" .X}}", T, "ok:x"),
            ("{{define \"T\"}}{{$x}}{{end}}", T, "perr:template: :1: undefined variable \"$x\""),
            ("{{template \"nope\"}}", T, "err:template: :1:11: executing \"\" at <{{template \"nope\"}}>: template \"nope\" not defined"),
            ("a{{block \"inner\" .}}[{{.X}}]{{end}}b", T, "ok:a[x]b"),
            ("{{define \"a\"}}a{{end}}{{define \"a\"}}b{{end}}", T, "perr:template: :1: template: multiple definition of template \"a\""),
            ("{{define \"a\"}}  {{end}}{{define \"a\"}}b{{end}}{{template \"a\"}}", T, "ok:b"),
            ("{{define \"a\"}}b{{end}}{{define \"a\"}} {{end}}{{template \"a\"}}", T, "ok:b"),
            ("{{define \"\"}}main{{end}}", T, "ok:main"),
            ("{{define \"x\"}}{{.Missing.B}}{{end}}{{template \"x\" .}}", T, "ok:<no value>"),
            ("{{define \"x\"}}{{.A.B}}{{end}}\n{{template \"x\" .}}", "{\"A\":\"s\"}", "err:template: :1:18: executing \"x\" at <.A.B>: can't evaluate field B in type interface {}"),
            ("{{define \"r\"}}{{template \"r\"}}{{end}}{{template \"r\"}}", "null", "err:template: :1:25: executing \"r\" at <{{template \"r\"}}>: exceeded maximum template depth (100)"),
            ("{{template `x` 1}}{{define `x`}}{{. | printf \"%d\"}}{{end}}", "null", "ok:1"),
            ("{{block \"b\"}}x{{end}}", "null", "perr:template: :1: missing value for block clause"),
        ]);
    }

    /// Trim markers and comments.
    #[test]
    fn exec_trim_comments() {
        check(&[
            ("x \r\n\t{{- 3}}", "null", "ok:x3"),
            ("{{3 -}}\n\n\ty", "null", "ok:3y"),
            ("x \r\n\t{{- 3 -}}\n\n\ty", "null", "ok:x3y"),
            ("x\n{{-  3   -}}\ny", "null", "ok:x3y"),
            ("x \r\n\t{{- /* hi */}}", "null", "ok:x"),
            ("{{/* hi */ -}}\n\n\ty", "null", "ok:y"),
            ("x \r\n\t{{- /* */ -}}\n\n\ty", "null", "ok:xy"),
            (
                "hello-{{/* this is a comment */}}-world",
                "null",
                "ok:hello--world",
            ),
            ("{{/*\n\n\n*/}}", "null", "ok:"),
            ("{{ $x \n := \n 1 \n }}{{$x}}", "null", "ok:1"),
            ("{{\n\"x\"\n|\nprintf\n}}", "null", "ok:x"),
            ("{{-\n/*\nhello\n*/\n-}}", "null", "ok:"),
            ("{{-3}}", "null", "ok:-3"),
            ("{{- -3 -}}", "null", "ok:-3"),
            ("a {{- .X}} {{.X -}} b", T, "ok:ax xb"),
            ("{{range .SI -}} {{.}} {{- end}}", T, "ok:345"),
            ("{{range .SI}}\n  {{- .}}\n{{- end}}", T, "ok:345"),
            ("hello-{.}}-world", "null", "ok:hello-{.}}-world"),
        ]);
    }

    /// and, or, not and the comparisons, with their errors.
    #[test]
    fn funcs_logic_cmp() {
        check(&[
            ("{{not true}} {{not false}}", "null", "ok:false true"),
            ("{{and false 0}} {{and 1 0}} {{and 0 true}} {{and 1 1}}", "null", "ok:false 0 0 1"),
            ("{{or 0 0}} {{or 1 0}} {{or 0 true}} {{or 1 1}}", "null", "ok:0 1 true 1"),
            ("{{or 0 1 (index .X 3)}}", "null", "ok:1"),
            ("{{and 1 0 (index .X 3)}}", "null", "ok:0"),
            ("{{or 0 0 (index . 3)}}", "null", "err:template: :1:10: executing \"\" at <index . 3>: error calling index: index of untyped nil"),
            ("{{and 1 1 (index . 3)}}", "null", "err:template: :1:11: executing \"\" at <index . 3>: error calling index: index of untyped nil"),
            ("{{1 | and 1}}", "null", "ok:1"),
            ("{{0 | and 1}}", "null", "ok:0"),
            ("{{1 | or 0}}", "null", "ok:1"),
            ("{{0 | or 0}}", "null", "ok:0"),
            ("{{and 1 .Unknown}}", "null", "ok:<no value>"),
            ("{{or 0 .Unknown}}", "null", "ok:<no value>"),
            ("{{and}}", "null", "err:template: :1:2: executing \"\" at <and>: wrong number of args for and: want at least 1 got 0"),
            ("{{if and true 1 `hi`}}TRUE{{else}}FALSE{{end}}", T, "ok:TRUE"),
            ("{{if and true 1 `hi` | not}}TRUE{{else}}FALSE{{end}}", "null", "ok:FALSE"),
            ("{{if true | not | and 1}}TRUE{{else}}FALSE{{end}}", "null", "ok:FALSE"),
            ("{{and .Empty0 .X}}", T, "ok:<no value>"),
            ("{{or .Empty0 .X}}", T, "ok:x"),
            ("{{not}}", "null", "err:template: :1:2: executing \"\" at <not>: wrong number of args for not: want 1 got 0"),
            ("{{not 1 2}}", "null", "err:template: :1:2: executing \"\" at <not>: wrong number of args for not: want 1 got 2"),
            ("{{not .Missing}}", "null", "ok:true"),
            ("{{if eq true true}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq true false}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq 1+2i 1+2i}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq 1+2i 1+3i}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq 1.5 1.5}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq 1.5 2.5}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq 1 1}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq 1 2}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq `xy` `xy`}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq `xy` `xyz`}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq 3 4 5 6 3}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq 3 4 5 6 7}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if ne true true}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if ne true false}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if ne 1.5 2.5}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if ne 1 1}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if ne `xy` `xyz`}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if lt 1.5 1.5}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if lt 1.5 2.5}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if lt 1 1}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if lt 1 2}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if lt `xy` `xy`}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if lt `xy` `xyz`}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if le 1.5 1.5}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if le 2.5 1.5}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if le 1 2}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if le `xyz` `xy`}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if gt 1.5 1.5}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if gt 2 1}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if gt `xy` `xyz`}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if ge 1.5 2.5}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if ge 2 1}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if ge `xyz` `xy`}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq (index `x` 0) 'x'}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq (index `x` 0) 'y'}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if lt (index `x` 0) 200}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if gt 200 (index `x` 0)}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if lt -1 (index `x` 0)}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq .Empty0 0}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq 0 .Empty0}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq .Empty0 .Empty0}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq .Missing nil}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq nil .Missing}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq .MSI .MSI}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .MSI .MSI>: error calling eq: non-comparable type map[one:%!s(int=1) three:%!s(int=3) two:%!s(int=2)]: map[string]interface {}"),
            ("{{if eq .MSI nil}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq .MSI .SI}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .MSI .SI>: error calling eq: non-comparable types map[one:%!s(int=1) three:%!s(int=3) two:%!s(int=2)]: map[string]interface {}, []interface {}: [3 4 5]"),
            ("{{if eq .SI .SI}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .SI .SI>: error calling eq: non-comparable type [%!s(int=3) %!s(int=4) %!s(int=5)]: []interface {}"),
            ("{{if eq .Missing .SI}}true{{else}}false{{end}}", T, "ok:false"),
            ("{{if eq `xy` 1}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq `xy` 1>: error calling eq: incompatible types for comparison: string and int"),
            ("{{if eq 2 2.0}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq 2 2.0>: error calling eq: incompatible types for comparison: int and float64"),
            ("{{if lt true true}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <lt true true>: error calling lt: invalid type for comparison"),
            ("{{if lt 1+0i 1+0i}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <lt 1+0i 1+0i>: error calling lt: invalid type for comparison"),
            ("{{if lt .Missing 1}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <lt .Missing 1>: error calling lt: invalid type for comparison"),
            ("{{if eq .I 17}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq .X `x`}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq .Float 1.5}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if gt .Big 1000}}true{{else}}false{{end}}", T, "ok:true"),
            ("{{if eq .X}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .X>: error calling eq: missing argument for comparison"),
            ("{{if ne .X}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <ne>: wrong number of args for ne: want 2 got 1"),
            ("{{if lt .X .I}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <lt .X .I>: error calling lt: incompatible types for comparison: string and int"),
            ("{{if eq .I 17.0}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .I 17.0>: error calling eq: incompatible types for comparison: int and float64"),
            ("{{if eq .Mixed .Mixed}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .Mixed .Mixed>: error calling eq: non-comparable type [%!s(int=1) two <nil> %!s(bool=true) %!s(float64=2.5)]: []interface {}"),
            ("{{if eq . `x`}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq . `x`>: error calling eq: incompatible types for comparison: map[string]interface {} and string"),
            ("{{if le .Missing 1}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <le .Missing 1>: error calling le: invalid type for comparison"),
            ("{{if eq .SI .Names}}true{{else}}false{{end}}", T, "err:template: :1:5: executing \"\" at <eq .SI .Names>: error calling eq: non-comparable type [a b]: []interface {}"),
        ]);
    }

    /// index, slice and len, with their errors.
    #[test]
    fn funcs_index_slice_len() {
        check(&[
            ("{{index .SI 0}}", T, "ok:3"),
            ("{{index .SI 1}}", T, "ok:4"),
            ("{{index .SI 10}}", T, "err:template: :1:2: executing \"\" at <index .SI 10>: error calling index: index out of range: 10"),
            ("{{index .SI 3}}", T, "err:template: :1:2: executing \"\" at <index .SI 3>: error calling index: reflect: slice index out of range"),
            ("{{index .SI `hello`}}", T, "err:template: :1:2: executing \"\" at <index .SI `hello`>: error calling index: cannot index slice/array with type string"),
            ("{{index .SI nil}}", T, "err:template: :1:2: executing \"\" at <index .SI nil>: error calling index: cannot index slice/array with nil"),
            ("{{index .MSI `one`}}", T, "ok:1"),
            ("{{index .MSI `two`}}", T, "ok:2"),
            ("{{index .MSI `XXX`}}", T, "ok:<no value>"),
            ("{{index .MSI nil}}", T, "err:template: :1:2: executing \"\" at <index .MSI nil>: error calling index: value is nil; should be of type string"),
            ("{{index .MSI ``}}", T, "ok:<no value>"),
            ("{{index .MSI 10}}", T, "err:template: :1:2: executing \"\" at <index .MSI 10>: error calling index: value has type int; should be string"),
            ("{{index .SMSI 1 `eleven`}}", T, "ok:11"),
            ("{{index nil 1}}", T, "err:template: :1:2: executing \"\" at <index nil 1>: error calling index: index of untyped nil"),
            ("{{index .Empty3 0}}", T, "ok:7"),
            ("{{index .Empty0 0}}", T, "err:template: :1:2: executing \"\" at <index .Empty0 0>: error calling index: index of untyped nil"),
            ("{{index .Mixed 2 0}}", T, "err:template: :1:2: executing \"\" at <index .Mixed 2 0>: error calling index: index of nil pointer"),
            ("{{index .X 0}}", T, "ok:120"),
            ("{{index .X 3}}", T, "err:template: :1:2: executing \"\" at <index .X 3>: error calling index: index out of range: 3"),
            ("{{index .I 0}}", T, "err:template: :1:2: executing \"\" at <index .I 0>: error calling index: can't index item of type int"),
            ("{{index .SI -1}}", T, "err:template: :1:2: executing \"\" at <index .SI -1>: error calling index: index out of range: -1"),
            ("{{index .SI 1.5}}", T, "err:template: :1:2: executing \"\" at <index .SI 1.5>: error calling index: cannot index slice/array with type float64"),
            ("{{index .SI}}", T, "ok:[3 4 5]"),
            ("{{index}}", T, "err:template: :1:2: executing \"\" at <index>: wrong number of args for index: want at least 1 got 0"),
            ("{{index .Nested \"A\" \"B\" \"C\"}}", T, "ok:deep"),
            ("{{index .Nested \"A\" \"Z\" \"C\"}}", T, "err:template: :1:2: executing \"\" at <index .Nested \"A\" \"Z\" \"C\">: error calling index: index of nil pointer"),
            ("{{index .SI (index .SI 0 | len)}}", T, "err:template: :1:27: executing \"\" at <len>: error calling len: len of type int"),
            ("{{index .Names 1}}", T, "ok:b"),
            ("{{slice .SI}}", T, "ok:[3 4 5]"),
            ("{{slice .SI 1}}", T, "ok:[4 5]"),
            ("{{slice .SI 1 2}}", T, "ok:[4]"),
            ("{{slice .SI -1}}", T, "err:template: :1:2: executing \"\" at <slice .SI -1>: error calling slice: index out of range: -1"),
            ("{{slice .SI 1 -2}}", T, "err:template: :1:2: executing \"\" at <slice .SI 1 -2>: error calling slice: index out of range: -2"),
            ("{{slice .SI 1 2 -1}}", T, "err:template: :1:2: executing \"\" at <slice .SI 1 2 -1>: error calling slice: index out of range: -1"),
            ("{{slice .SI 2 1}}", T, "err:template: :1:2: executing \"\" at <slice .SI 2 1>: error calling slice: invalid slice index: 2 > 1"),
            ("{{slice .SI 2 2 1}}", T, "err:template: :1:2: executing \"\" at <slice .SI 2 2 1>: error calling slice: invalid slice index: 2 > 1"),
            ("{{slice .SI 1 2 3}}", T, "ok:[4]"),
            ("{{slice .SI 1 2 3 4}}", T, "err:template: :1:2: executing \"\" at <slice .SI 1 2 3 4>: error calling slice: too many slice indexes: 4"),
            ("{{slice .S}}", T, "ok:xyz"),
            ("{{slice .S 0 1}}", T, "ok:x"),
            ("{{slice .S 1}}", T, "ok:yz"),
            ("{{slice .S 1 2}}", T, "ok:y"),
            ("{{slice .S 1 5}}", T, "err:template: :1:2: executing \"\" at <slice .S 1 5>: error calling slice: index out of range: 5"),
            ("{{slice .S 1 2 2}}", T, "err:template: :1:2: executing \"\" at <slice .S 1 2 2>: error calling slice: cannot 3-index slice a string"),
            ("{{slice .Empty3 0 1}}", T, "ok:[7]"),
            ("{{slice nil}}", T, "err:template: :1:2: executing \"\" at <slice nil>: error calling slice: slice of untyped nil"),
            ("{{slice .I}}", T, "err:template: :1:2: executing \"\" at <slice .I>: error calling slice: can't slice item of type int"),
            ("{{slice .MSI}}", T, "err:template: :1:2: executing \"\" at <slice .MSI>: error calling slice: can't slice item of type map[string]interface {}"),
            ("{{slice .SI (index .SI 0)}}", T, "ok:[]"),
            ("{{slice .SI .Empty0}}", T, "err:template: :1:2: executing \"\" at <slice .SI .Empty0>: error calling slice: cannot index slice/array with type interface {}"),
            ("{{len .SI}}", T, "ok:3"),
            ("{{len .MSI }}", T, "ok:3"),
            ("{{len 3}}", T, "err:template: :1:2: executing \"\" at <len 3>: error calling len: len of type int"),
            ("{{len .Empty0}}", T, "err:template: :1:2: executing \"\" at <len .Empty0>: error calling len: len of nil pointer"),
            ("{{len .Empty3}}", T, "ok:2"),
            ("{{len .Missing}}", T, "err:template: :1:2: executing \"\" at <len .Missing>: error calling len: reflect: call of reflect.Value.Type on zero Value"),
            ("{{len .S}}", T, "ok:3"),
            ("{{len \"h\u{e9}llo\"}}", T, "ok:6"),
            ("{{len .Float}}", T, "err:template: :1:2: executing \"\" at <len .Float>: error calling len: len of type float64"),
            ("{{range .Mixed}}{{len .}}{{end}}", T, "err:template: :1:18: executing \"\" at <len .>: error calling len: len of type int"),
            ("{{len (split .S \"y\")}}", T, "ok:2"),
        ]);
    }

    /// print, println, printf and number literals.
    #[test]
    fn funcs_print() {
        check(&[
            ("{{print \"hello, print\"}}", T, "ok:hello, print"),
            ("{{print 1 2 3}}", T, "ok:1 2 3"),
            ("{{print nil}}", T, "ok:<nil>"),
            ("{{println 1 2 3}}", T, "ok:1 2 3\n"),
            ("{{printf \"%04x\" 127}}", T, "ok:007f"),
            ("{{printf \"%g\" 3.5}}", T, "ok:3.5"),
            ("{{printf \"%g\" 1+7i}}", T, "ok:(1+7i)"),
            ("{{printf \"%s\" \"hello\"}}", T, "ok:hello"),
            ("{{printf \"%s\" .U.V}}", T, "ok:v"),
            ("{{with .I}}{{printf \"%d\" .}}{{end}}", T, "ok:17"),
            ("{{with $x := .I}}{{printf \"%d\" $x}}{{end}}", T, "ok:17"),
            ("{{printf \"%d %s %g %s\" 127 \"hello\" 7-3i .X}}", T, "ok:127 hello (7-3i) x"),
            ("{{true|printf}}", T, "err:template: :1:7: executing \"\" at <printf>: wrong type for value; expected string; got bool"),
            ("{{1|printf}}", T, "err:template: :1:4: executing \"\" at <printf>: wrong type for value; expected string; got int"),
            ("{{1.1|printf}}", T, "err:template: :1:6: executing \"\" at <printf>: wrong type for value; expected string; got float64"),
            ("{{'x'|printf}}", T, "err:template: :1:6: executing \"\" at <printf>: wrong type for value; expected string; got int"),
            ("{{0i|printf}}", T, "err:template: :1:5: executing \"\" at <printf>: wrong type for value; expected string; got complex128"),
            ("{{1+2i|printf \"%v\"}}", T, "ok:(1+2i)"),
            ("{{\"aaa\"|printf }}", T, "ok:aaa"),
            ("{{\"x\" | printf}}", "0", "ok:x"),
            ("{{print \"a\" \"b\" 1 2 \"c\" 3}}", T, "ok:ab1 2c3"),
            ("{{print .Empty0 .Missing}}", T, "ok:<nil> <nil>"),
            ("{{print}}", T, "ok:"),
            ("{{println}}", T, "ok:\n"),
            ("{{println \"a\" 1 nil}}", T, "ok:a 1 <nil>\n"),
            ("{{print .Mixed .MSI}}", T, "ok:[1 two <nil> true 2.5] map[one:1 three:3 two:2]"),
            ("{{print 1234}}", T, "ok:1234"),
            ("{{print 12_34}}", T, "ok:1234"),
            ("{{print 0b101}}", T, "ok:5"),
            ("{{print 0b_1_0_1}}", T, "ok:5"),
            ("{{print 0B101}}", T, "ok:5"),
            ("{{print 0377}}", T, "ok:255"),
            ("{{print 0o377}}", T, "ok:255"),
            ("{{print 0o_3_7_7}}", T, "ok:255"),
            ("{{print 0O377}}", T, "ok:255"),
            ("{{print 0x123}}", T, "ok:291"),
            ("{{print 0x1_23}}", T, "ok:291"),
            ("{{print 0X123ABC}}", T, "ok:1194684"),
            ("{{print 123.4}}", T, "ok:123.4"),
            ("{{print 0_0_1_2_3.4}}", T, "ok:123.4"),
            ("{{print +0x1.ep+2}}", T, "ok:7.5"),
            ("{{print +0x_1.e_0p+0_2}}", T, "ok:7.5"),
            ("{{print +0X1.EP+2}}", T, "ok:7.5"),
            ("{{print 1_2_3_4 7.5_00_00_00}}", T, "ok:1234 7.5"),
            ("{{print 1234 0x0_1.e_0p+02}}", T, "ok:1234 7.5"),
            ("{{printf `%T` 0xef}}", T, "ok:int"),
            ("{{printf `%T` 0xEE}}", T, "ok:int"),
            ("{{printf `%T` 0Xef}}", T, "ok:int"),
            ("{{printf `%T` 1.0}}", T, "ok:float64"),
            ("{{printf `%T` 1e1}}", T, "ok:float64"),
            ("{{printf `%T` 1i}}", T, "ok:complex128"),
            ("{{printf `%T` 'a'}}", T, "ok:int"),
            ("{{printf `%T` 0x7fffffffffffffff}}", T, "ok:int"),
            ("{{printf `%T` 0x8000000000000000}}", T, "err:template: :1:14: executing \"\" at <0x8000000000000000>: 0x8000000000000000 overflows int"),
            ("{{print 99999999999999999999}}", T, "perr:template: :1: integer overflow: \"99999999999999999999\""),
            ("{{print 1e400}}", T, "perr:template: :1: illegal number syntax: \"1e400\""),
            ("{{print 089}}", T, "perr:template: :1: integer overflow: \"089\""),
            ("{{print 1__2}}", T, "perr:template: :1: illegal number syntax: \"1__2\""),
            ("{{print 'ab'}}", T, "perr:template: :1: malformed character constant: 'ab'"),
            ("{{print ''}}", T, "perr:template: :1: invalid syntax"),
            ("{{print '\\x41' '\\101' '\\u00e9' '\u{672c}'}}", T, "ok:65 65 233 26412"),
            ("{{print \"\\q\"}}", T, "perr:template: :1: invalid syntax"),
            ("{{print \"\\x41\\u00e9\\U0001F600\\t|\"}}", T, "ok:A\u{e9}\u{1f600}\t|"),
            ("{{print `a\\nb`}}", T, "ok:a\\nb"),
            ("{{print -0x10}}", T, "ok:-16"),
            ("{{print 1e3 1E3 +1.2e-4 4.2i -7.2i 1e-7}}", T, "ok:1000 1000 0.00012 (0+4.2i) (0-7.2i) 1e-07"),
            ("{{print 0x1p-2}}", T, "ok:0.25"),
            ("{{print .5 5.}}", T, "ok:0.5 5"),
        ]);
    }

    /// printf verbs, flags, widths, precisions and argument errors.
    #[test]
    fn funcs_printf() {
        check(&[
            ("{{printf \"%v|%d|%5d|%-5d|%05d|%+d|% d|%x|%X|%o|%O|%b|%#x|%#o|%#b|%c|%q|%U|%#U\" 65 65 65 65 65 65 65 65 65 65 65 65 65 65 65 65 65 65 65}}", T, "ok:65|65|   65|65   |00065|+65| 65|41|41|101|0o101|1000001|0x41|0101|0b1000001|A|'A'|U+0041|U+0041 'A'"),
            ("{{printf \"%d|%v|%x\" -42 -42 -42}}", T, "ok:-42|-42|-2a"),
            ("{{printf \"%.3d|%8.3d|%-8.3d|%08.3d|%.0d|%5.0d\" 7 7 7 7 0 0}}", T, "ok:007|     007|007     |     007||     "),
            ("{{printf \"%v|%f|%.2f|%8.2f|%-8.2f|%08.2f|%+.1f|%e|%E|%.3e|%g|%G|%.3g|%10.4g\" 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159 3.14159}}", T, "ok:3.14159|3.141590|3.14|    3.14|3.14    |00003.14|+3.1|3.141590e+00|3.141590E+00|3.142e+00|3.14159|3.14159|3.14|     3.142"),
            ("{{printf \"%v %v %v %v %v %v %v\" 1e6 1e21 123456.0 0.0001 0.00001 100.0 1e-7}}", T, "ok:1e+06 1e+21 123456 0.0001 1e-05 100 1e-07"),
            ("{{printf \"%g|%g|%g|%.3g|%.3g|%.10g|%.0g\" 100000.0 1000000.0 1e-5 1234.0 100.0 0.1 2.5}}", T, "ok:100000|1e+06|1e-05|1.23e+03|100|0.1|2"),
            ("{{printf \"%#g|%#.3g|%#v|%#f|%#e|%#.0f|%#.0e\" 1.0 1.0 1.5 1.0 1.0 3.0 3.0}}", T, "ok:1.00000|1.00|1.5|1.000000|1.000000e+00|3.|3.e+00"),
            ("{{printf \"%f|%.0f|%.1f|%.2f|%.3f\" 0.5 2.5 0.25 0.125 0.0006}}", T, "ok:0.500000|2|0.2|0.12|0.001"),
            ("{{printf \"%b|%x|%X|%.3x|%#x|%.0x\" 1.5 1.5 1.5 1.5 1.5 1.5}}", T, "ok:6755399441055744p-52|0x1.8p+00|0X1.8P+00|0x1.800p+00|0x1.8000p+00|0x1p+01"),
            ("{{printf \"%x|%x|%.1x\" 0.0 -2.0 0.1}}", T, "ok:0x0p+00|-0x1p+01|0x1.ap-04"),
            ("{{printf \"%v|%.2f|%+.1e|%8.2f\" 1+2i 1+2i 1+2i 1+2i}}", T, "ok:(1+2i)|(1.00+2.00i)|(+1.0e+00+2.0e+00i)|(    1.00   +2.00i)"),
            ("{{printf \"%s|%q|%v|%x|%X|% x|%#x|%# x|%10s|%-10s|%.2s|%5.1s|%010s|%q\" \"h\u{e9}llo\" \"h\u{e9}llo\" \"h\u{e9}llo\" \"hi\" \"hi\" \"hi\" \"hi\" \"hi\" \"h\u{e9}llo\" \"h\u{e9}llo\" \"h\u{e9}llo\" \"h\u{e9}llo\" \"ab\" \"a\\tb\\u00a0c\"}}", T, "ok:h\u{e9}llo|\"h\u{e9}llo\"|h\u{e9}llo|6869|6869|68 69|0x6869|0x68 0x69|     h\u{e9}llo|h\u{e9}llo     |h\u{e9}|    h|00000000ab|\"a\\tb\\u00a0c\""),
            ("{{printf \"%#q|%#q|%+q|%q\" \"abc\" \"a`b\" \"h\u{e9}llo\" \"\\x01\"}}", T, "ok:`abc`|\"a`b\"|\"h\\u00e9llo\"|\"\\x01\""),
            ("{{printf \"%t|%v|%5t|%d\" true false true true}}", T, "ok:true|false| true|%!d(bool=true)"),
            ("{{printf \"%d|%s|%x|%v\" \"x\" 1 true nil}}", T, "ok:%!d(string=x)|%!s(int=1)|%!x(bool=true)|<nil>"),
            ("{{printf \"%d %d\" 1}}", T, "ok:1 %!d(MISSING)"),
            ("{{printf \"%d\" 1 2 \"x\" nil}}", T, "ok:1%!(EXTRA int=2, string=x, <nil>)"),
            ("{{printf \"%!\" 1}}", T, "ok:%!!(int=1)"),
            ("{{printf \"%\" 1}}", T, "ok:%!(NOVERB)%!(EXTRA int=1)"),
            ("{{printf \"%z\" 1}}", T, "ok:%!z(int=1)"),
            ("{{printf \"%[2]d %[1]d\" 1 2}}", T, "ok:2 1"),
            ("{{printf \"%[3]d\" 1 2}}", T, "ok:%!d(BADINDEX)"),
            ("{{printf \"%[0]d\" 1}}", T, "ok:%!d(BADINDEX)"),
            ("{{printf \"%[1]d %d\" 1 2}}", T, "ok:1 2"),
            ("{{printf \"%*d|%-*d|%.*f\" 5 1 5 2 2 3.14159}}", T, "ok:    1|2    |3.14"),
            ("{{printf \"%*d\" \"x\" 1}}", T, "ok:%!(BADWIDTH)1"),
            ("{{printf \"%.*d\" -1 5}}", T, "ok:%!(BADPREC)5"),
            ("{{printf \"%[2]*[1]d\" 12 5}}", T, "ok:   12"),
            ("{{printf \"%[x]d\" 1}}", T, "ok:%!d(BADINDEX)"),
            ("{{printf \"%[2]3d\" 1 2}}", T, "ok:%!d(BADINDEX)"),
            ("{{printf \"%v|%d|%s|%#v|%x\" .SI .SI .SI .SI .SI}}", T, "ok:[3 4 5]|[3 4 5]|[%!s(int=3) %!s(int=4) %!s(int=5)]|[]interface {}{3, 4, 5}|[3 4 5]"),
            ("{{printf \"%v|%s|%#v|%d\" .Mixed .Mixed .Mixed .Mixed}}", T, "ok:[1 two <nil> true 2.5]|[%!s(int=1) two <nil> %!s(bool=true) %!s(float64=2.5)]|[]interface {}{1, \"two\", interface {}(nil), true, 2.5}|[1 %!d(string=two) <nil> %!d(bool=true) %!d(float64=2.5)]"),
            ("{{printf \"%v|%s|%#v|%q\" .MSI .Labels .Labels .Labels}}", T, "ok:map[one:1 three:3 two:2]|map[x:1 y:2]|map[string]interface {}{\"x\":\"1\", \"y\":\"2\"}|map[\"x\":\"1\" \"y\":\"2\"]"),
            ("{{printf \"%v|%#v|%q|%s\" (split \"a,b\" \",\") (split \"a,b\" \",\") (split \"a,b\" \",\") (split \"a,b\" \",\")}}", T, "ok:[a b]|[]string{\"a\", \"b\"}|[\"a\" \"b\"]|[a b]"),
            ("{{printf \"%T %T %T %T %T %T %T %T %T\" .X .I .Float .SI .MSI .Empty0 .True (split \"a\" \",\") (index \"a\" 0)}}", T, "ok:string int float64 []interface {} map[string]interface {} <nil> bool []string uint8"),
            ("{{printf \"%v %d %c %q %x\" (index \"a\" 0) (index \"a\" 0) (index \"a\" 0) (index \"a\" 0) (index \"a\" 0)}}", T, "ok:97 97 a 'a' 61"),
            ("{{printf \"%c|%c|%q|%q|%U|%#U|%c\" 0x1F600 -1 0x1F600 0xD800 0x1F600 0x1F600 0x110000}}", T, "ok:\u{1f600}|\u{fffd}|'\u{1f600}'|'\u{fffd}'|U+1F600|U+1F600 '\u{1f600}'|\u{fffd}"),
            ("{{printf \"%v %s\" .Missing .Missing}}", T, "ok:<nil> %!s(<nil>)"),
            ("{{printf \"%p\" .X}}", T, "ok:%!p(string=x)"),
            ("{{printf \"%w\" .X}}", T, "ok:%!w(string=x)"),
            ("{{printf \"%e\" 0}}", T, "ok:%!e(int=0)"),
            ("{{printf \"%v\" -0.0}}", T, "ok:-0"),
            ("{{printf \"%08.3f|%-08.3f|%+08.3f|% 8.3f|%08v\" -3.14159 3.14159 3.14159 3.14159 3.5}}", T, "ok:-003.142|3.142   |+003.142|   3.142|000003.5"),
            ("{{printf \"%5v|%-5v|%05v\" .SI .SI .SI}}", T, "ok:[    3     4     5]|[3     4     5    ]|[00003 00004 00005]"),
            ("{{printf \"%.3v|%10.3v\" \"abcdef\" 3.14159}}", T, "ok:abc|      3.14"),
            ("{{printf \"%6.2f%%\" 99.5}}", T, "ok: 99.50%"),
        ]);
    }

    /// html, js and urlquery.
    #[test]
    fn funcs_escape() {
        check(&[
            (
                "{{html \"<script>alert(\\\"XSS\\\");</script>\"}}",
                "null",
                "ok:&lt;script&gt;alert(&#34;XSS&#34;);&lt;/script&gt;",
            ),
            (
                "{{printf \"<script>alert(\\\"XSS\\\");</script>\" | html}}",
                "null",
                "ok:&lt;script&gt;alert(&#34;XSS&#34;);&lt;/script&gt;",
            ),
            ("{{html .PS}}", T, "ok:a string"),
            ("{{html .NIL}}", T, "ok:&lt;no value&gt;"),
            ("{{html .Empty0}}", T, "ok:&lt;no value&gt;"),
            ("{{html 1 \"<a>\" 2}}", T, "ok:1&lt;a&gt;2"),
            ("{{html \"a\\x00b\" \"'&\"}}", T, "ok:a\u{fffd}b&#39;&amp;"),
            ("{{js .}}", "\"It'd be nice.\"", "ok:It\\'d be nice."),
            (
                "{{js \"<a href='x'>&=\\\"\\\\\"}}",
                T,
                "ok:\\u003Ca href\\u003D\\'x\\'\\u003E\\u0026\\u003D\\\"\\\\",
            ),
            (
                "{{js \"a\\tb\\u2028c\\u00e9\"}}",
                T,
                "ok:a\\u0009b\\u2028c\u{e9}",
            ),
            ("{{js 1 2}}", T, "ok:1 2"),
            ("{{js .Empty0}}", T, "ok:\\u003Cno value\\u003E"),
            (
                "{{\"http://www.example.org/\"|urlquery}}",
                "null",
                "ok:http%3A%2F%2Fwww.example.org%2F",
            ),
            (
                "{{urlquery \"a b&c=d/\u{e9}~._-\"}}",
                "null",
                "ok:a+b%26c%3Dd%2F%C3%A9~._-",
            ),
            ("{{urlquery 1 \"x\" 2}}", "null", "ok:1x2"),
            ("{{urlquery}}", "null", "ok:"),
        ]);
    }

    /// call, which has no function to call on JSON data.
    #[test]
    fn funcs_call() {
        check(&[
            ("{{call nil}}", T, "err:template: :1:2: executing \"\" at <call nil>: error calling call: call of nil"),
            ("{{call}}", T, "err:template: :1:2: executing \"\" at <call>: wrong number of args for call: want at least 1 got 0"),
            ("{{1 | call}}", T, "err:template: :1:6: executing \"\" at <call>: error calling call: non-function <int Value> of type int"),
            ("{{call .X}}", T, "err:template: :1:2: executing \"\" at <call .X>: error calling call: non-function .X of type string"),
            ("{{call .Missing}}", T, "err:template: :1:2: executing \"\" at <call .Missing>: error calling call: call of nil"),
            ("{{call .I 1 2}}", T, "err:template: :1:2: executing \"\" at <call .I 1 2>: error calling call: non-function .I of type int"),
            ("{{.X | call}}", T, "err:template: :1:7: executing \"\" at <call>: error calling call: non-function x of type string"),
        ]);
    }

    /// Argument count and type errors.
    #[test]
    fn funcs_errors() {
        check(&[
            ("{{.Empty0 | printf}}", T, "err:template: :1:12: executing \"\" at <printf>: invalid value; expected string"),
            ("{{3|lower}}", T, "err:template: :1:4: executing \"\" at <lower>: wrong type for value; expected string; got int"),
            ("{{lower 3}}", T, "err:template: :1:8: executing \"\" at <3>: expected string; found 3"),
            ("{{lower .I}}", T, "err:template: :1:8: executing \"\" at <.I>: wrong type for value; expected string; got int"),
            ("{{lower .Missing}}", T, "err:template: :1:8: executing \"\" at <.Missing>: invalid value; expected string"),
            ("{{lower nil}}", T, "err:template: :1:8: executing \"\" at <nil>: cannot assign nil to string"),
            ("{{range .Mixed}}{{lower .}}{{end}}", T, "err:template: :1:24: executing \"\" at <.>: wrong type for value; expected string; got int"),
            ("{{range .Names}}{{lower .}}{{end}}", T, "ok:ab"),
            ("{{truncate .X `2`}}", T, "err:template: :1:14: executing \"\" at <`2`>: expected integer; found `2`"),
            ("{{truncate .X 2.0}}", T, "ok:x"),
            ("{{truncate .X 2.5}}", T, "err:template: :1:14: executing \"\" at <2.5>: expected integer; found 2.5"),
            ("{{truncate .X .I}}", T, "ok:x"),
            ("{{truncate .X .Float}}", T, "err:template: :1:14: executing \"\" at <.Float>: wrong type for value; expected int; got float64"),
            ("{{truncate .X nil}}", T, "err:template: :1:14: executing \"\" at <nil>: cannot assign nil to int"),
            ("{{truncate .X true}}", T, "err:template: :1:14: executing \"\" at <true>: expected integer; found true"),
            ("{{lower true}}", T, "err:template: :1:8: executing \"\" at <true>: expected string; found true"),
            ("{{lower}}", T, "err:template: :1:2: executing \"\" at <lower>: wrong number of args for lower: want 1 got 0"),
            ("{{lower 1 2}}", T, "err:template: :1:2: executing \"\" at <lower>: wrong number of args for lower: want 1 got 2"),
            ("{{\"a\" | lower \"b\"}}", T, "err:template: :1:8: executing \"\" at <lower>: wrong number of args for lower: want 1 got 2"),
            ("{{printf}}", T, "err:template: :1:2: executing \"\" at <printf>: wrong number of args for printf: want at least 1 got 0"),
            ("{{printf 1}}", T, "err:template: :1:9: executing \"\" at <1>: expected string; found 1"),
            ("{{.X | printf \"%s-%s\" \"a\"}}", T, "ok:a-x"),
            ("{{lower (upper)}}", T, "err:template: :1:9: executing \"\" at <upper>: wrong number of args for upper: want 1 got 0"),
            ("{{nil | lower}}", T, "err:template: :1:2: executing \"\" at <nil>: nil is not a command"),
            ("{{lower .Mixed}}", T, "err:template: :1:8: executing \"\" at <.Mixed>: wrong type for value; expected string; got []interface {}"),
            ("{{upper (index .Mixed 1)}}", T, "ok:TWO"),
            ("{{upper (index .Mixed 0)}}", T, "err:template: :1:22: executing \"\" at <0>: wrong type for value; expected string; got int"),
            ("{{upper (index .Mixed 2)}}", T, "err:template: :1:22: executing \"\" at <2>: invalid value; expected string"),
        ]);
    }

    /// The reference's json, split, join, title, lower, upper, pad and truncate.
    #[test]
    fn extra_funcs() {
        check(&[
            ("{{json .Ports}}", "{\"Ports\":\"0.0.0.0:2->8/udp\"}", "ok:\"0.0.0.0:2->8/udp\""),
            ("{{join (split . \":\") \"/\"}}", "\"text:with:colon\"", "ok:text/with/colon"),
            ("this is a {{ . }}", "\"string\"", "ok:this is a string"),
            ("{{truncate . 5}}", "\"tupx5xzf6hvsrhnruz5cr8gwp\"", "ok:tupx5"),
            ("{{truncate . 25}}", "\"tupx5xzf6hvsrhnruz5cr8gwp\"", "ok:tupx5xzf6hvsrhnruz5cr8gwp"),
            ("{{truncate . 30}}", "\"tupx5xzf6hvsrhnruz5cr8gwp\"", "ok:tupx5xzf6hvsrhnruz5cr8gwp"),
            ("{{pad . 3 3}}", "\"tupx5xzf6hvsrhnruz5cr8gwp\"", "ok:   tupx5xzf6hvsrhnruz5cr8gwp   "),
            ("{{truncate . 5}}", "\"\"", "ok:"),
            ("{{pad . 3 3}}", "\"\"", "ok:"),
            ("{{truncate . 5}}", "null", "err:template: :1:11: executing \"\" at <.>: invalid value; expected string"),
            ("{{pad . 3 3}}", "null", "err:template: :1:6: executing \"\" at <.>: invalid value; expected string"),
            ("output: \"{{- join . \", \" -}}\"", "null", "ok:output: \"\""),
            ("output: \"{{- join . \", \" -}}\"", "\"hello\"", "err:template: :1:13: executing \"\" at <join . \", \">: error calling join: expected slice, got string"),
            ("output: \"{{- join . \", \" -}}\"", "{\"A\":\"1\",\"B\":\"2\",\"C\":\"3\"}", "ok:output: \"1, 2, 3\""),
            ("output: \"{{- join . \", \" -}}\"", "[true,false,true]", "ok:output: \"true, false, true\""),
            ("output: \"{{- join . \", \" -}}\"", "[1,\"a\",null,2.5,[1]]", "ok:output: \"1, a, <nil>, 2.5, [1]\""),
            ("output: \"{{- join . \", \" -}}\"", "{\"b\":2,\"a\":10,\"c\":\"x\"}", "ok:output: \"10, 2, x\""),
            ("{{join . \",\"}}", "3", "err:template: :1:2: executing \"\" at <join . \",\">: error calling join: expected slice, got int"),
            ("{{json .}}", T, "ok:{\"AI\":[3,4,5],\"Big\":1234567,\"BigF\":1234567.5,\"Empty0\":null,\"Empty1\":3,\"Empty2\":\"empty2\",\"Empty3\":[7,8],\"Empty4\":{\"V\":\"UinEmpty\"},\"Float\":1.5,\"FloatZero\":0,\"I\":17,\"Labels\":{\"x\":\"1\",\"y\":\"2\"},\"MSI\":{\"one\":1,\"three\":3,\"two\":2},\"MSIEmpty\":{},\"MSIone\":{\"one\":1},\"MXI\":{\"one\":1},\"Mixed\":[1,\"two\",null,true,2.5],\"NIL\":null,\"Names\":[\"a\",\"b\"],\"Neg\":-3,\"Nested\":{\"A\":{\"B\":{\"C\":\"deep\"}}},\"PI\":23,\"PS\":\"a string\",\"PSI\":[21,22,23],\"S\":\"xyz\",\"SB\":[true,false],\"SI\":[3,4,5],\"SIEmpty\":[],\"SMSI\":[{\"one\":1,\"two\":2},{\"eleven\":11,\"twelve\":12}],\"True\":true,\"U\":{\"V\":\"v\"},\"U16\":16,\"X\":\"x\"}"),
            ("{{json .Mixed}}", T, "ok:[1,\"two\",null,true,2.5]"),
            ("{{json .Missing}}", T, "ok:null"),
            ("{{json .Empty0}}", T, "ok:null"),
            ("{{json \"<a&b>\\u2028\\t\\u0001\u{e9}\"}}", "null", "ok:\"<a&b>\\u2028\\t\\u0001\u{e9}\""),
            ("{{json 1.5}} {{json 1e21}} {{json 1e-7}} {{json 1e20}} {{json 0.000001}} {{json 100.0}}", "null", "ok:1.5 1e+21 1e-7 100000000000000000000 0.000001 100"),
            ("{{json (split \"a,b\" \",\")}} {{json 3}} {{json true}} {{json (index \"a\" 0)}}", "null", "ok:[\"a\",\"b\"] 3 true 97"),
            ("{{json 1i}}", "null", "err:template: :1:2: executing \"\" at <json 1i>: error calling json: json: unsupported type: complex128"),
            ("{{json}}", "null", "err:template: :1:2: executing \"\" at <json>: wrong number of args for json: want 1 got 0"),
            ("{{json .X .X}}", T, "err:template: :1:2: executing \"\" at <json>: wrong number of args for json: want 1 got 2"),
            ("{{split \"a,b,c\" \",\"}} {{split \"abc\" \"\"}} {{split \"\" \",\"}} {{len (split \"\" \"\")}} {{split \"a,,b\" \",\"}}", "null", "ok:[a b c] [a b c] [] 0 [a  b]"),
            ("{{range split \"x y z\" \" \"}}<{{.}}>{{end}}", "null", "ok:<x><y><z>"),
            ("{{index (split \"x y z\" \" \") 1}}", "null", "ok:y"),
            ("{{split .I \",\"}}", T, "err:template: :1:8: executing \"\" at <.I>: wrong type for value; expected string; got int"),
            ("{{join .Names \"-\"}} {{join .SI \"+\"}} {{join .Labels \",\"}}", T, "ok:a-b 3+4+5 1,2"),
            ("{{title \"hello wORLD foo_bar a-b 9x \u{e9}a\"}}", "null", "ok:Hello WORLD Foo_bar A-B 9x \u{c9}a"),
            ("{{lower \"HeLLo \u{c0}\u{c9} \u{3a3}\u{391}\u{3a3}\"}} {{upper \"hello \u{df} \u{e9}a \u{1c6}\"}}", "null", "ok:hello \u{e0}\u{e9} \u{3c3}\u{3b1}\u{3c3} HELLO \u{df} \u{c9}A \u{1c4}"),
            ("{{title \"\u{1c6}emal\"}} {{upper \"\u{130}\"}} {{lower \"\u{130}\"}}", "null", "ok:\u{1c5}emal \u{130} i"),
            ("{{pad \"x\" 0 0}}|{{pad \"x\" 2 1}}|{{pad \"x\" -1 0}}", "null", "err:template: :1:34: executing \"\" at <pad \"x\" -1 0>: error calling pad: strings: negative Repeat count"),
            ("{{truncate \"h\u{e9}llo\" 2}}|{{truncate \"abc\" -1}}", "null", "err:template: :1:26: executing \"\" at <truncate \"abc\" -1>: error calling truncate: runtime error: slice bounds out of range [:-1]"),
            ("{{truncate \"abc\" 3}}|{{truncate \"abc\" 0}}", "null", "ok:abc|"),
            ("{{.Names | join \",\"}}", T, "err:template: :1:16: executing \"\" at <\",\">: wrong type for value; expected string; got []interface {}"),
            ("{{\",\" | join .Names}}", T, "ok:a,b"),
        ]);
    }

    /// Execution errors and the position and node they report.
    #[test]
    fn exec_errors() {
        check(&[
            ("{{.X.Y}}", T, "err:template: :1:4: executing \"\" at <.X.Y>: can't evaluate field Y in type interface {}"),
            ("{{.U.V.W}}", T, "err:template: :1:4: executing \"\" at <.U.V.W>: can't evaluate field W in type interface {}"),
            ("{{.SI.X}}", T, "err:template: :1:5: executing \"\" at <.SI.X>: can't evaluate field X in type interface {}"),
            ("{{.I.X}}", T, "err:template: :1:4: executing \"\" at <.I.X>: can't evaluate field X in type interface {}"),
            ("{{.Float.X}}", T, "err:template: :1:8: executing \"\" at <.Float.X>: can't evaluate field X in type interface {}"),
            ("{{.True.X}}", T, "err:template: :1:7: executing \"\" at <.True.X>: can't evaluate field X in type interface {}"),
            ("{{.Empty0.X}}", T, "err:template: :1:9: executing \"\" at <.Empty0.X>: nil pointer evaluating interface {}.X"),
            ("{{range .SI}}{{.X}}{{end}}", T, "err:template: :1:15: executing \"\" at <.X>: can't evaluate field X in type interface {}"),
            ("{{$.X.Y}}", T, "err:template: :1:3: executing \"\" at <$.X.Y>: can't evaluate field Y in type interface {}"),
            ("{{$x := .X}}{{$x.Y}}", T, "err:template: :1:16: executing \"\" at <$x.Y>: can't evaluate field Y in type string"),
            ("{{range $v := .SI}}{{$v.Y}}{{end}}", T, "err:template: :1:23: executing \"\" at <$v.Y>: can't evaluate field Y in type interface {}"),
            ("{{index .SI 5}}", T, "err:template: :1:2: executing \"\" at <index .SI 5>: error calling index: index out of range: 5"),
            ("{{template \"x\"}}", T, "err:template: :1:11: executing \"\" at <{{template \"x\"}}>: template \"x\" not defined"),
            ("line1\n{{.X.Y}}", T, "err:template: :2:4: executing \"\" at <.X.Y>: can't evaluate field Y in type interface {}"),
            ("line1\n  ab {{.X.Y}}", T, "err:template: :2:9: executing \"\" at <.X.Y>: can't evaluate field Y in type interface {}"),
            ("{{if .X}}\n{{.X.Y}}{{end}}", T, "err:template: :2:4: executing \"\" at <.X.Y>: can't evaluate field Y in type interface {}"),
            ("{{with .MSI}}{{.one.x}}{{end}}", T, "err:template: :1:19: executing \"\" at <.one.x>: can't evaluate field x in type interface {}"),
            ("{{printf \"%d\" .X | len | printf \"%d\" | .Y}}", T, "err:template: :1:39: executing \"\" at <.Y>: Y is not a method but has arguments"),
            ("{{range .X}}{{end}}", T, "err:template: :1:8: executing \"\" at <.X>: range can't iterate over x"),
            ("{{range .I}}{{.}}{{end}}", T, "ok:012345678910111213141516"),
            ("{{range $i, $v := .I}}{{end}}", T, "err:template: :1:18: executing \"\" at <.I>: can't use 17 to iterate over more than one variable"),
            ("{{(.X).Y}}", T, "err:template: :1:3: executing \"\" at <.X>: can't evaluate field Y in type string"),
            ("{{(index .SI 0).Y}}", T, "err:template: :1:13: executing \"\" at <0>: can't evaluate field Y in type int"),
            ("{{(.MSI).one.two}}", T, "err:template: :1:3: executing \"\" at <.MSI>: can't evaluate field two in type interface {}"),
            ("{{.SI | len | printf \"%d\" | index .SI}}", T, "err:template: :1:28: executing \"\" at <index .SI>: error calling index: cannot index slice/array with type string"),
            ("{{index .SI 0 | printf \"%d%%\"}}", T, "ok:3%"),
            ("{{printf \"%d\" (.X.Y)}}", T, "err:template: :1:17: executing \"\" at <.X.Y>: can't evaluate field Y in type interface {}"),
            ("{{\"%\" | printf \"%s%%\" | .Y}}", T, "err:template: :1:24: executing \"\" at <.Y>: Y is not a method but has arguments"),
        ]);
    }

    /// Lexer and parser errors.
    #[test]
    fn parse_errors() {
        check(&[
            ("{{}}", "null", "perr:template: :1: missing value for command"),
            ("hello{{range", "null", "perr:template: :1: unclosed action"),
            ("{{end}}", "null", "perr:template: :1: unexpected {{end}}"),
            ("{{else}}", "null", "perr:template: :1: unexpected {{else}}"),
            ("{{if .X}}hello{{end}}{{else}}", "null", "perr:template: :1: unexpected {{else}}"),
            ("{{if .X}}1{{else}}2{{else}}3{{end}}", "null", "perr:template: :1: expected end; found {{else}}"),
            ("hello{{range .x}}", "null", "perr:template: :1: unexpected EOF"),
            ("hello{{range .x}}{{else}}", "null", "perr:template: :1: unexpected EOF"),
            ("hello{{undefined}}", "null", "perr:template: :1: function \"undefined\" not defined"),
            ("{{$x}}", "null", "perr:template: :1: undefined variable \"$x\""),
            ("{{with $x := 4}}{{end}}{{$x}}", "null", "perr:template: :1: undefined variable \"$x\""),
            ("{{template $v}}", "null", "perr:template: :1: unexpected \"$v\" in template clause"),
            ("{{with $x.Y := 4}}{{end}}", "null", "perr:template: :1: undefined variable \"$x\""),
            ("{{template .X}}", "null", "perr:template: :1: unexpected \".X\" in template clause"),
            ("{{printf 3, 4}}", "null", "perr:template: :1: unexpected \",\" in operand"),
            ("{{with $v, $u := 3}}{{end}}", "null", "perr:template: :1: too many declarations in with"),
            ("{{range $u, $v, $w := 3}}{{end}}", "null", "perr:template: :1: too many declarations in range"),
            ("{{printf (printf .).}}", "null", "perr:template: :1: unexpected <.> in operand"),
            ("{{printf 3`x`}}", "null", "perr:template: :1: unexpected \"`x`\" in operand"),
            ("{{printf `x`.}}", "null", "perr:template: :1: unexpected <.> in operand"),
            ("{{if .X}}a{{else if .Y}}b{{end}}{{end}}", "null", "perr:template: :1: unexpected {{end}}"),
            ("{{range .}}{{end}} {{break}}", "null", "perr:template: :1: {{break}} outside {{range}}"),
            ("{{range .}}{{end}} {{continue}}", "null", "perr:template: :1: {{continue}} outside {{range}}"),
            ("{{range .}}{{else}}{{break}}{{end}}", "null", "perr:template: :1: {{break}} outside {{range}}"),
            ("{{range .}}{{else}}{{continue}}{{end}}", "null", "perr:template: :1: {{continue}} outside {{range}}"),
            ("{{$x += 1}}{{$x}}", "null", "perr:template: :1: undefined variable \"$x\""),
            ("{{$x ! 2}}{{$x}}", "null", "perr:template: :1: undefined variable \"$x\""),
            ("{{$x % 3}}{{$x}}", "null", "perr:template: :1: undefined variable \"$x\""),
            ("{{range $x := $y := 3}}{{end}}", "null", "perr:template: :1: undefined variable \"$y\""),
            ("{{$x:=.}}{{$x!2}}", "null", "perr:template: :1: bad character U+0021 '!'"),
            ("{{$x:=.}}{{$x+2}}", "null", "perr:template: :1: bad character U+002B '+'"),
            ("{{$x:=.}}{{$x +2}}", "null", "err:template: :1:11: executing \"\" at <$x>: can't give argument to non-function $x"),
            ("{{1.E}}", "null", "perr:template: :1: illegal number syntax: \"1.E\""),
            ("{{0.1.E}}", "null", "perr:template: :1: unexpected . after term \"0.1\""),
            ("{{true.E}}", "null", "perr:template: :1: unexpected . after term \"true\""),
            ("{{'a'.any}}", "null", "perr:template: :1: unexpected . after term \"'a'\""),
            ("{{\"hello\".guys}}", "null", "perr:template: :1: unexpected . after term \"\\\"hello\\\"\""),
            ("{{..E}}", "null", "perr:template: :1: unexpected . after term \".\""),
            ("{{nil.E}}", "null", "perr:template: :1: unexpected . after term \"nil\""),
            ("{{12|.}}", "null", "perr:template: :1: non executable command in pipeline stage 2"),
            ("{{.|12|printf}}", "null", "perr:template: :1: non executable command in pipeline stage 2"),
            ("{{.|printf|\"error\"}}", "null", "perr:template: :1: non executable command in pipeline stage 3"),
            ("{{12|printf|'e'}}", "null", "perr:template: :1: non executable command in pipeline stage 3"),
            ("{{.|true}}", "null", "perr:template: :1: non executable command in pipeline stage 2"),
            ("{{'c'|nil}}", "null", "perr:template: :1: non executable command in pipeline stage 2"),
            ("{{printf \"%d\" ( ) }}", "null", "perr:template: :1: missing value for parenthesized pipeline"),
            ("{{block \"foo\"}}hello{{end}}", "null", "perr:template: :1: missing value for block clause"),
            ("line1\n{{", "null", "perr:template: :2: unclosed action"),
            ("line1\n{{define `x`}}line2\n{{", "null", "perr:template: :3: unclosed action"),
            ("line1\n{{\"x\"\n\"y\"\n", "null", "perr:template: :4: unclosed action started at :2"),
            ("{{\n\n\n\n\n", "null", "perr:template: :6: unclosed action started at :1"),
            ("line1\n{{\nx\n}}", "null", "perr:template: :3: function \"x\" not defined"),
            ("{{foo}}", "null", "perr:template: :1: function \"foo\" not defined"),
            ("{{/*}}", "null", "perr:template: :1: unclosed comment"),
            ("{{/*\nhello\n}}", "null", "perr:template: :1: unclosed comment"),
            ("{{.X (1 2 3}}", "null", "perr:template: :1: unclosed left paren"),
            ("{{.X 1 2 3 ) }}", "null", "perr:template: :1: unexpected right paren"),
            ("{{(.X 1 2 3", "null", "perr:template: :1: unclosed action"),
            ("{{`x`3}}", "null", "perr:template: :1: unexpected \"3\" in operand"),
            ("{{a#}}", "null", "perr:template: :1: bad character U+0023 '#'"),
            ("{{'a}}", "null", "perr:template: :1: unterminated character constant"),
            ("{{\"a}}", "null", "perr:template: :1: unterminated quoted string"),
            ("{{`a}}", "null", "perr:template: :1: unterminated raw quoted string"),
            ("{{0xi}}", "null", "perr:template: :1: illegal number syntax: \"0xi\""),
            ("{{range .X}}", "null", "perr:template: :1: unexpected EOF"),
            ("{{$x := 23}}{{with $x.y := 3}}{{$x 23}}{{end}}", "null", "perr:template: :1: unexpected \":=\" in operand"),
            ("{{$a,$b,$c := 23}}", "null", "perr:template: :1: too many declarations in command"),
            ("{{$a}}", "null", "perr:template: :1: undefined variable \"$a\""),
            ("{{true.any}}", "null", "perr:template: :1: unexpected . after term \"true\""),
            ("{{12|false}}", "null", "perr:template: :1: non executable command in pipeline stage 2"),
            ("{{ ( ) }}", "null", "perr:template: :1: missing value for parenthesized pipeline"),
            ("{{ $v := `\n` }} {{", "null", "perr:template: :2: unclosed action"),
            ("{{range $k}}{{end}}", "null", "perr:template: :1: undefined variable \"$k\""),
            ("{{range $k, $v}}{{end}}", "null", "perr:template: :1: undefined variable \"$v\""),
            ("{{range $k,}}{{end}}", "null", "perr:template: :1: missing value for range"),
            ("{{range $k, $v := }}{{end}}", "null", "perr:template: :1: missing value for range"),
            ("{{range $k, .}}{{end}}", "null", "perr:template: :1: range can only initialize variables"),
            ("{{range $k, 123 := .}}{{end}}", "null", "perr:template: :1: range can only initialize variables"),
            ("hello\n\n{{`unterminated\n\n\n\n}}\n some more\n\n", "null", "perr:template: :3: unterminated raw quoted string"),
            ("#{{\u{1}}}", "null", "perr:template: :1: unrecognized character in action: U+0001"),
            ("{{range", "null", "perr:template: :1: unclosed action"),
            ("{{\"\n\"}}", "null", "perr:template: :1: unterminated quoted string"),
            ("{{'\n}}", "null", "perr:template: :1: unterminated character constant"),
            ("{{3k}}", "null", "perr:template: :1: bad number syntax: \"3k\""),
            ("{{(3}}", "null", "perr:template: :1: unclosed left paren"),
            ("{{3)}}", "null", "perr:template: :1: unexpected right paren"),
            ("hello-{{/*/}}-world", "null", "perr:template: :1: unclosed comment"),
            ("hello-{{/* */ }}-world", "null", "perr:template: :1: comment ends before closing delimiter"),
            ("{{|||||}}", "null", "perr:template: :1: unexpected \"|\" in command"),
            ("{{,@% }}", "null", "perr:template: :1: unexpected \",\" in command"),
            ("{{with .X}}{{else if .Y}}{{end}}", "null", "perr:template: :1: unexpected <if> in input"),
            ("{{range .X}}{{else with .Y}}{{end}}", "null", "perr:template: :1: unexpected <with> in input"),
            ("{{define \"x\"}}{{else}}{{end}}", "null", "perr:template: :1: unexpected {{else}} in define clause"),
            ("{{define x}}{{end}}", "null", "perr:template: :1: unexpected \"x\" in define clause"),
            ("{{define \"x\" 1}}{{end}}", "null", "perr:template: :1: unexpected \"1\" in define clause"),
            ("{{template \"x\" 1 2}}", "null", "err:template: :1:11: executing \"\" at <{{template \"x\" 1 2}}>: template \"x\" not defined"),
            ("{{break 1}}", "null", "perr:template: :1: unexpected \"1\" in {{break}}"),
            ("{{range .}}{{break 1}}{{end}}", "null", "perr:template: :1: unexpected \"1\" in {{break}}"),
            ("{{if}}{{end}}", "null", "perr:template: :1: missing value for if"),
            ("{{range}}{{end}}", "null", "perr:template: :1: missing value for range"),
            ("{{with}}{{end}}", "null", "perr:template: :1: missing value for with"),
            ("{{else 1}}", "null", "perr:template: :1: unexpected \"1\" in else"),
            ("{{end 1}}", "null", "perr:template: :1: unexpected \"1\" in end"),
            ("{{.X | }}", "null", "ok:<no value>"),
            ("{{| .X}}", "null", "perr:template: :1: unexpected \"|\" in command"),
            ("{{\"abcdefghijklmnop\" \"x\" | (12)}}", "null", "err:template: :1:2: executing \"\" at <\"abcdefghijklmnop\">: can't give argument to non-function \"abcdefghijklmnop\""),
            ("{{ printf \"%d\" 1 \"abcdefghijkl\".X }}", "null", "perr:template: :1: unexpected . after term \"\\\"abcdefghijkl\\\"\""),
            ("{{range .}}{{define \"x\"}}{{break}}{{end}}{{end}}", "null", "perr:template: :1: unexpected <define> in command"),
            ("{{\"\\q\"}}", "null", "perr:template: :1: invalid syntax"),
            ("{{'\\q'}}", "null", "perr:template: :1: invalid syntax"),
            ("{{print 0x8000000000000000}}", "null", "err:template: :1:8: executing \"\" at <0x8000000000000000>: 0x8000000000000000 overflows int"),
            ("{{print 1e400}}", "null", "perr:template: :1: illegal number syntax: \"1e400\""),
            ("{{print 0x1.8}}", "null", "perr:template: :1: illegal number syntax: \"0x1.8\""),
            ("{{print 1__2}}", "null", "perr:template: :1: illegal number syntax: \"1__2\""),
            ("{{print 'ab'}}", "null", "perr:template: :1: malformed character constant: 'ab'"),
            ("{{print ''}}", "null", "perr:template: :1: invalid syntax"),
            ("{{print 089}}", "null", "perr:template: :1: integer overflow: \"089\""),
            ("{{print 99999999999999999999}}", "null", "perr:template: :1: integer overflow: \"99999999999999999999\""),
            ("{{$x := 1}}{{$x.y.z := 2}}", "null", "perr:template: :1: unexpected \":=\" in operand"),
            ("{{$x , $y := 1}}", "null", "perr:template: :1: too many declarations in command"),
            ("{{range $x , $y := .}}{{$x}}{{$y}}{{end}}", "null", "ok:"),
            ("a\n{{if .X}}\nb{{else}}\n{{end}}\n{{end}}", "null", "perr:template: :5: unexpected {{end}}"),
            ("{{define \"a\"}}x{{end}}\n\n{{define \"a\"}}y{{end}}", "null", "perr:template: :3: template: multiple definition of template \"a\""),
            ("{{block \"a\" .}}x{{end}}{{block \"a\" .}}y{{end}}", "null", "perr:template: :1: template: multiple definition of template \"a\""),
            ("{{\u{e9}}}", "null", "perr:template: :1: function \"\u{e9}\" not defined"),
            ("{{.\u{e9}}}", "null", "ok:<no value>"),
            ("{{$\u{e9} := 1}}{{$\u{e9}}}", "null", "ok:1"),
        ]);
    }

    #[test]
    fn format_list_like_docker() {
        check_list(&[
            ("table", ROWS, "ok:CONTAINER ID   IMAGE          NAMES                STATUS\n3f4e8a1b2c3d   nginx:latest   web                  Up 2 hours\n9a8b7c6d5e4f   fedora         a-much-longer-name   Exited (0) 3 days ago\n"),
            ("table {{.Names}}\\t{{.Status}}", ROWS, "ok:NAMES                STATUS\nweb                  Up 2 hours\na-much-longer-name   Exited (0) 3 days ago\n"),
            ("table {{.Names}}\t{{.Status}}", ROWS, "ok:NAMES                STATUS\nweb                  Up 2 hours\na-much-longer-name   Exited (0) 3 days ago\n"),
            ("table {{.Names}}", ROWS, "ok:NAMES\nweb\na-much-longer-name\n"),
            ("table {{.ID}}\\t{{.Names}}", "[]", "ok:CONTAINER ID   NAMES\n"),
            ("table", "[]", "ok:CONTAINER ID   IMAGE     NAMES     STATUS\n"),
            ("json", ROWS, "ok:{\"ID\":\"3f4e8a1b2c3d\",\"Image\":\"nginx:latest\",\"Labels\":\"a=b\",\"Names\":\"web\",\"Size\":1234567,\"Status\":\"Up 2 hours\"}\n{\"ID\":\"9a8b7c6d5e4f\",\"Image\":\"fedora\",\"Labels\":\"\",\"Names\":\"a-much-longer-name\",\"Size\":12,\"Status\":\"Exited (0) 3 days ago\"}\n"),
            ("{{.Names}}", ROWS, "ok:web\na-much-longer-name\n"),
            ("{{.Names}}: {{.Size}}", ROWS, "ok:web: 1234567\na-much-longer-name: 12\n"),
            ("   {{.Names}}\\n   ", ROWS, "ok:web\n\na-much-longer-name\n\n"),
            ("{{.Names}}\\t{{.ID}}", ROWS, "ok:web\t3f4e8a1b2c3d\na-much-longer-name\t9a8b7c6d5e4f\n"),
            ("table {{upper .Names}}\\t{{truncate .ID 4}}\\t{{json .Size}}\\t{{lower .Status}}", ROWS, "ok:NAMES                CONTAINER ID   SIZE      STATUS\nWEB                  3f4e           1234567   up 2 hours\nA-MUCH-LONGER-NAME   9a8b           12        exited (0) 3 days ago\n"),
            ("table {{pad .Names 1 1}}|\\t{{.Image}}", ROWS, "ok: NAMES |                IMAGE\n web |                  nginx:latest\n a-much-longer-name |   fedora\n"),
            ("table {{.Names}}\\t{{.Nope}}\\t{{.Status}}", ROWS, "err:template parsing error: template: :1:13: executing \"\" at <.Nope>: can't evaluate field Nope in type map[string]interface {}"),
            ("table {{.Names}}\\t{{lower .Nope}}\\t{{.Status}}", ROWS, "err:template parsing error: template: :1:19: executing \"\" at <.Nope>: can't evaluate field Nope in type map[string]interface {}"),
            ("table {{.Names}}\\t\\t{{.Status}}", ROWS, "ok:NAMES                          STATUS\nweb                            Up 2 hours\na-much-longer-name             Exited (0) 3 days ago\n"),
            ("table {{.Names}}\\t{{.Status}}\\t", ROWS, "ok:NAMES                STATUS                  \nweb                  Up 2 hours              \na-much-longer-name   Exited (0) 3 days ago   \n"),
            ("table NAME\\tX\\n{{.Names}}\\t{{.ID}}", ROWS, "ok:NAME                 X\nNAMES                CONTAINER ID\nNAME                 X\nweb                  3f4e8a1b2c3d\nNAME                 X\na-much-longer-name   9a8b7c6d5e4f\n"),
            ("table {{.Names}}\\t{{.Labels}}\\t{{.Image}}", ROWS, "ok:NAMES                LABELS    IMAGE\nweb                  a=b       nginx:latest\na-much-longer-name             fedora\n"),
            ("table {{.Names.X}}", ROWS, "err:template parsing error: template: :1:8: executing \"\" at <.Names.X>: can't evaluate field X in type interface {}"),
            ("table {{.Names", ROWS, "err:template parsing error: template: :1: unclosed action"),
            ("{{.Names.X}}", ROWS, "err:template parsing error: template: :1:8: executing \"\" at <.Names.X>: can't evaluate field X in type interface {}"),
            ("tablex {{.Names}}", ROWS, "ok:x NAMES\nx web\nx a-much-longer-name\n"),
            ("table {{.ID}}\\t\u{672c}\u{672c}\u{672c}\\t{{.Names}}", ROWS, "ok:CONTAINER ID   \u{672c}\u{672c}\u{672c}    NAMES\n3f4e8a1b2c3d   \u{672c}\u{672c}\u{672c}    web\n9a8b7c6d5e4f   \u{672c}\u{672c}\u{672c}    a-much-longer-name\n"),
        ]);
    }
}
