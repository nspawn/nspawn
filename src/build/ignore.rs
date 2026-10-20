//! Pattern matching for ignore files and for include and exclude lists, and the reader
//! of an ignore file such as .dockerignore. A port of moby/patternmatcher and its
//! ignorefile package (upstream/SOURCE names the versions).
//!
//! Patterns match slash-separated paths relative to a root. `*` and `?` stay within one
//! path element, `**` crosses any number of them, `[...]` is a character class and `\`
//! escapes the next character. A leading `!` makes a pattern an exception: a path is
//! matched when the last pattern that applies to it is not an exception. A pattern also
//! matches everything below a directory it matches.
//!
//! The lexical path helpers of the reference (clean, join, split, dir, base) live here
//! too, as `lexical`, since the walker and the copier use them as well.
// The port keeps the reference's API whole; its tests use what the builder does not.
#![allow(dead_code)]

use std::borrow::Cow;
use std::fmt;
use std::io::{self, Read};

/// The failures of the matcher, with the reference's messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternError {
    /// The pattern is malformed (filepath.ErrBadPattern in the reference).
    BadPattern,
    /// A pattern that is a lone `!`.
    IllegalExclusion,
    /// A `MatchInfo` from a matcher with a different number of patterns.
    WrongParentCount,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PatternError::BadPattern => "syntax error in pattern",
            PatternError::IllegalExclusion => "illegal exclusion pattern: \"!\"",
            PatternError::WrongParentCount => "wrong number of values in parentMatched",
        })
    }
}

impl std::error::Error for PatternError {}

/// How a compiled pattern is evaluated. The reference avoids regular expressions for
/// the shapes that do not need one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchType {
    Exact,
    Prefix,
    Suffix,
    Regexp,
}

#[derive(Debug, Clone)]
enum Compiled {
    Exact,
    Prefix,
    Suffix,
    Regexp(Regexp),
    // The reference compiles lazily and reports a pattern whose regular expression
    // does not compile only when it is evaluated; this keeps that timing.
    Bad,
}

/// One pattern of a matcher.
#[derive(Debug, Clone)]
pub struct Pattern {
    compiled: Compiled,
    cleaned: String,
    dirs: Vec<String>,
    exclusion: bool,
}

impl Pattern {
    /// The pattern as cleaned by `PatternMatcher::new`, without its `!`.
    pub fn as_str(&self) -> &str {
        &self.cleaned
    }

    /// Whether the pattern is an exception (it was written with a leading `!`).
    pub fn exclusion(&self) -> bool {
        self.exclusion
    }

    fn matches(&self, path: &str) -> Result<bool, PatternError> {
        // The trailing or leading `**` is cut by bytes, as the reference does, which
        // also copes with a pattern that starts with a byte order mark.
        let cleaned = self.cleaned.as_bytes();
        let bytes = path.as_bytes();
        Ok(match &self.compiled {
            Compiled::Exact => bytes == cleaned,
            Compiled::Prefix => bytes.starts_with(&cleaned[..cleaned.len().saturating_sub(2)]),
            Compiled::Suffix => {
                let suffix = cleaned.get(2..).unwrap_or_default();
                // `**/foo` matches `foo` itself as well.
                bytes.ends_with(suffix) || suffix.strip_prefix(b"/") == Some(bytes)
            }
            Compiled::Regexp(re) => re.is_match(path),
            Compiled::Bad => return Err(PatternError::BadPattern),
        })
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.cleaned)
    }
}

/// What a matcher learned about a directory, handed back when matching its entries so
/// that patterns already matched by the directory are not evaluated again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchInfo {
    parent_matched: Vec<bool>,
}

/// An ordered list of patterns, exceptions included.
#[derive(Debug, Clone)]
pub struct PatternMatcher {
    patterns: Vec<Pattern>,
    exclusions: bool,
}

impl PatternMatcher {
    /// Compiles `patterns`. Empty patterns (after trimming) are dropped; each one is
    /// cleaned lexically and checked for syntax with the reference's glob rules.
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Result<PatternMatcher, PatternError> {
        let mut pm = PatternMatcher {
            patterns: Vec::with_capacity(patterns.len()),
            exclusions: false,
        };
        for p in patterns {
            let p = p.as_ref().trim();
            if p.is_empty() {
                continue;
            }
            let mut p = lexical::clean_str(p);
            let mut exclusion = false;
            if let Some(rest) = p.strip_prefix('!') {
                if rest.is_empty() {
                    return Err(PatternError::IllegalExclusion);
                }
                exclusion = true;
                p = rest.to_string();
                pm.exclusions = true;
            }
            glob_match(&p, ".")?;
            let compiled = match compile(&p, '/') {
                Ok((MatchType::Exact, _)) => Compiled::Exact,
                Ok((MatchType::Prefix, _)) => Compiled::Prefix,
                Ok((MatchType::Suffix, _)) => Compiled::Suffix,
                Ok((MatchType::Regexp, Some(re))) => Compiled::Regexp(re),
                Ok((MatchType::Regexp, None)) | Err(_) => Compiled::Bad,
            };
            pm.patterns.push(Pattern {
                compiled,
                dirs: p.split('/').map(str::to_string).collect(),
                cleaned: p,
                exclusion,
            });
        }
        Ok(pm)
    }

    /// Whether `file` matches, checking only one parent directory: the one with as many
    /// elements as each pattern. The reference keeps it for compatibility and calls it
    /// buggy; `matches_or_parent_matches` checks every parent.
    pub fn matches(&self, file: &str) -> Result<bool, PatternError> {
        let mut matched = false;
        let parent_path = lexical::dir_str(file);
        let parent_dirs: Vec<&str> = parent_path.split('/').collect();
        for pattern in &self.patterns {
            if pattern.exclusion != matched {
                continue;
            }
            let mut m = pattern.matches(file)?;
            if !m && parent_path != "." && pattern.dirs.len() <= parent_dirs.len() {
                m = pattern
                    .matches(&parent_dirs[..pattern.dirs.len()].join("/"))
                    .unwrap_or(false);
            }
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok(matched)
    }

    /// Whether `file` or one of its parent directories matches.
    pub fn matches_or_parent_matches(&self, file: &str) -> Result<bool, PatternError> {
        let mut matched = false;
        let parent_path = lexical::dir_str(file);
        let parent_dirs: Vec<&str> = parent_path.split('/').collect();
        for pattern in &self.patterns {
            // An inclusion cannot change a path that already matched, nor an exception
            // one that did not.
            if pattern.exclusion != matched {
                continue;
            }
            let mut m = pattern.matches(file)?;
            if !m && parent_path != "." {
                m = parent_matches(pattern, &parent_dirs);
            }
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok(matched)
    }

    /// Whether `file` matches, given whether its parent directory did. The reference
    /// deprecates it: a parent's single result loses which patterns matched it.
    pub fn matches_using_parent_result(
        &self,
        file: &str,
        parent_matched: bool,
    ) -> Result<bool, PatternError> {
        let mut matched = parent_matched;
        for pattern in &self.patterns {
            if pattern.exclusion != matched {
                continue;
            }
            if pattern.matches(file)? {
                matched = !pattern.exclusion;
            }
        }
        Ok(matched)
    }

    /// Whether `file` matches, given what matching its parent directory returned. A
    /// default `MatchInfo` means nothing is known about the parents, and they are
    /// checked as `matches_or_parent_matches` does.
    pub fn matches_using_parent_results(
        &self,
        file: &str,
        parent: &MatchInfo,
    ) -> Result<(bool, MatchInfo), PatternError> {
        let parent_matched = &parent.parent_matched;
        if !parent_matched.is_empty() && parent_matched.len() != self.patterns.len() {
            return Err(PatternError::WrongParentCount);
        }
        let mut matched = false;
        let mut info = MatchInfo {
            parent_matched: vec![false; self.patterns.len()],
        };
        for (i, pattern) in self.patterns.iter().enumerate() {
            let mut m = parent_matched.get(i).copied().unwrap_or(false);
            if !m {
                if pattern.exclusion != matched {
                    continue;
                }
                m = pattern.matches(file)?;
                if !m && parent_matched.is_empty() {
                    let parent_path = lexical::dir_str(file);
                    if parent_path != "." {
                        let parent_dirs: Vec<&str> = parent_path.split('/').collect();
                        m = parent_matches(pattern, &parent_dirs);
                    }
                }
            }
            info.parent_matched[i] = m;
            if m {
                matched = !pattern.exclusion;
            }
        }
        Ok((matched, info))
    }

    /// Whether any pattern is an exception.
    pub fn exclusions(&self) -> bool {
        self.exclusions
    }

    pub fn patterns(&self) -> &[Pattern] {
        &self.patterns
    }
}

fn parent_matches(pattern: &Pattern, parent_dirs: &[&str]) -> bool {
    (1..=parent_dirs.len()).any(|i| {
        pattern
            .matches(&parent_dirs[..i].join("/"))
            .unwrap_or(false)
    })
}

/// `PatternMatcher::matches` with a matcher built for one call; "." never matches.
pub fn matches<S: AsRef<str>>(file: &str, patterns: &[S]) -> Result<bool, PatternError> {
    let pm = PatternMatcher::new(patterns)?;
    let file = lexical::clean_str(file);
    if file == "." {
        return Ok(false);
    }
    pm.matches(&file)
}

/// `PatternMatcher::matches_or_parent_matches` with a matcher built for one call; "."
/// never matches.
pub fn matches_or_parent_matches<S: AsRef<str>>(
    file: &str,
    patterns: &[S],
) -> Result<bool, PatternError> {
    let pm = PatternMatcher::new(patterns)?;
    let file = lexical::clean_str(file);
    if file == "." {
        return Ok(false);
    }
    pm.matches_or_parent_matches(&file)
}

/// Turns a cleaned pattern into its match type and, for `MatchType::Regexp`, the regular
/// expression, spelled exactly as the reference spells it. `sl` is the path separator
/// the pattern uses.
fn compile(pattern: &str, sl: char) -> Result<(MatchType, Option<Regexp>), PatternError> {
    let mut re = String::from("^");
    let mut detected = MatchType::Exact;
    let esc_sl = if sl == '\\' {
        "\\\\".to_string()
    } else {
        sl.to_string()
    };
    let mut chars = pattern.chars().peekable();
    // The reference reads the pattern with a scanner that drops a leading byte order
    // mark.
    if chars.peek() == Some(&'\u{feff}') {
        chars.next();
    }
    let mut i = 0;
    while let Some(ch) = chars.next() {
        if ch == '*' {
            if chars.peek() == Some(&'*') {
                chars.next();
                // `**/` is treated as `**`.
                if chars.peek() == Some(&sl) {
                    chars.next();
                }
                if chars.peek().is_none() {
                    // A trailing `**` accepts everything, as in .gitignore.
                    if detected == MatchType::Exact {
                        detected = MatchType::Prefix;
                    } else {
                        re.push_str(".*");
                        detected = MatchType::Regexp;
                    }
                } else {
                    re.push_str(&format!("(.*{esc_sl})?"));
                    detected = MatchType::Regexp;
                }
                if i == 0 {
                    detected = MatchType::Suffix;
                }
            } else {
                re.push_str(&format!("[^{esc_sl}]*"));
                detected = MatchType::Regexp;
            }
        } else if ch == '?' {
            re.push_str(&format!("[^{esc_sl}]"));
            detected = MatchType::Regexp;
        // The reference's escape set loses the bits of `{`, `|` and `}` (their byte
        // over eight does not fit its table), so those reach the regular expression as
        // they are: an alternation and a counted repetition.
        } else if ".+()$".contains(ch) {
            re.push('\\');
            re.push(ch);
        } else if ch == '\\' {
            if sl == '\\' {
                re.push_str(&esc_sl);
                i += 1;
                continue;
            }
            match chars.next() {
                Some(next) => {
                    re.push('\\');
                    re.push(next);
                    detected = MatchType::Regexp;
                }
                None => re.push('\\'),
            }
        } else if ch == '[' || ch == ']' {
            re.push(ch);
            detected = MatchType::Regexp;
        } else {
            re.push(ch);
        }
        i += 1;
    }
    if detected == MatchType::Regexp {
        re.push('$');
        return Ok((detected, Some(Regexp::new(&re)?)));
    }
    Ok((detected, None))
}

/// The regular expressions `compile` produces, read with the syntax and semantics of
/// the reference's engine (Go's regexp with its default Perl flags): classes with
/// ranges, POSIX names and the ASCII Perl classes, `.` that does not match a newline,
/// `^` and `$` at the ends of the text, `\b` on ASCII words, `\Q...\E`, and the
/// repetitions `*` `+` `?`. Unicode property classes (`\p`) are not supported and make
/// the pattern malformed. Matching runs an NFA over the characters of the path, so it
/// takes linear time whatever the pattern.
#[derive(Debug, Clone)]
pub(crate) struct Regexp {
    source: String,
    prog: Vec<Inst>,
}

#[derive(Debug, Clone)]
enum Node {
    Char(CharTest),
    Assert(Assertion),
    Group(Vec<Node>),
    /// Branches, the first that matches taken.
    Alt(Vec<Vec<Node>>),
    Repeat(Box<Node>, RepeatKind),
    /// A counted repetition written out, with what Go's parser sizes it at.
    Counted(Vec<Node>, usize),
}

#[derive(Debug, Clone, Copy)]
enum RepeatKind {
    Star,
    Plus,
    Quest,
}

#[derive(Debug, Clone)]
enum CharTest {
    Lit(char),
    AnyNotNewline,
    Class(Class),
}

#[derive(Debug, Clone)]
struct Class {
    items: Vec<ClassItem>,
    negated: bool,
}

#[derive(Debug, Clone)]
enum ClassItem {
    Range(u32, u32),
    Set(&'static [(u32, u32)], bool),
}

#[derive(Debug, Clone, Copy)]
enum Assertion {
    BeginText,
    EndText,
    WordBoundary,
    NoWordBoundary,
}

#[derive(Debug, Clone)]
enum Inst {
    Char(CharTest),
    Assert(Assertion),
    Split(usize, usize),
    Jmp(usize),
    Match,
}

const PERL_DIGIT: &[(u32, u32)] = &[(0x30, 0x39)];
const PERL_SPACE: &[(u32, u32)] = &[(0x09, 0x0a), (0x0c, 0x0d), (0x20, 0x20)];
const PERL_WORD: &[(u32, u32)] = &[(0x30, 0x39), (0x41, 0x5a), (0x5f, 0x5f), (0x61, 0x7a)];

fn posix_group(name: &str) -> Option<&'static [(u32, u32)]> {
    Some(match name {
        "alnum" => &[(0x30, 0x39), (0x41, 0x5a), (0x61, 0x7a)],
        "alpha" => &[(0x41, 0x5a), (0x61, 0x7a)],
        "ascii" => &[(0x00, 0x7f)],
        "blank" => &[(0x09, 0x09), (0x20, 0x20)],
        "cntrl" => &[(0x00, 0x1f), (0x7f, 0x7f)],
        "digit" => PERL_DIGIT,
        "graph" => &[(0x21, 0x7e)],
        "lower" => &[(0x61, 0x7a)],
        "print" => &[(0x20, 0x7e)],
        "punct" => &[(0x21, 0x2f), (0x3a, 0x40), (0x5b, 0x60), (0x7b, 0x7e)],
        "space" => &[(0x09, 0x0d), (0x20, 0x20)],
        "upper" => &[(0x41, 0x5a)],
        "word" => PERL_WORD,
        "xdigit" => &[(0x30, 0x39), (0x41, 0x46), (0x61, 0x66)],
        _ => return None,
    })
}

fn perl_class(c: char) -> Option<(&'static [(u32, u32)], bool)> {
    Some(match c {
        'd' => (PERL_DIGIT, false),
        'D' => (PERL_DIGIT, true),
        's' => (PERL_SPACE, false),
        'S' => (PERL_SPACE, true),
        'w' => (PERL_WORD, false),
        'W' => (PERL_WORD, true),
        _ => return None,
    })
}

fn in_ranges(ranges: &[(u32, u32)], c: u32) -> bool {
    ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi)
}

impl CharTest {
    fn matches(&self, c: char) -> bool {
        match self {
            CharTest::Lit(l) => *l == c,
            CharTest::AnyNotNewline => c != '\n',
            CharTest::Class(class) => {
                let c = c as u32;
                let hit = class.items.iter().any(|item| match item {
                    ClassItem::Range(lo, hi) => *lo <= c && c <= *hi,
                    ClassItem::Set(ranges, negated) => in_ranges(ranges, c) != *negated,
                });
                hit != class.negated
            }
        }
    }
}

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(|c| in_ranges(PERL_WORD, c as u32))
}

struct RegexpParser<'a> {
    s: &'a [char],
    pos: usize,
    /// What the counted repetitions so far come to, as Go's parser sizes them, and the
    /// most they may.
    written: usize,
    limit: usize,
}

impl RegexpParser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.pos).copied()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s.get(self.pos + n).copied()
    }

    fn bump(&mut self) -> Result<char, PatternError> {
        let c = self.peek().ok_or(PatternError::BadPattern)?;
        self.pos += 1;
        Ok(c)
    }

    /// Branches separated by `|` up to the end of the group or of the expression.
    fn seq(&mut self, in_group: bool) -> Result<Vec<Node>, PatternError> {
        let mut branches: Vec<Vec<Node>> = Vec::new();
        let mut nodes: Vec<Node> = Vec::new();
        let end = |mut branches: Vec<Vec<Node>>, nodes: Vec<Node>| {
            if branches.is_empty() {
                return nodes;
            }
            branches.push(nodes);
            vec![Node::Alt(branches)]
        };
        loop {
            let Some(c) = self.peek() else {
                if in_group {
                    return Err(PatternError::BadPattern);
                }
                return Ok(end(branches, nodes));
            };
            match c {
                ')' => {
                    if !in_group {
                        return Err(PatternError::BadPattern);
                    }
                    self.pos += 1;
                    return Ok(end(branches, nodes));
                }
                '|' => {
                    self.pos += 1;
                    branches.push(std::mem::take(&mut nodes));
                }
                '{' => match self.repeat_count() {
                    Some((min, max)) => {
                        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT || m < min) {
                            return Err(PatternError::BadPattern);
                        }
                        let last = match nodes.pop() {
                            None | Some(Node::Repeat(..) | Node::Counted(..)) => {
                                return Err(PatternError::BadPattern)
                            }
                            Some(last) => last,
                        };
                        if self.peek() == Some('?') {
                            self.pos += 1;
                        }
                        // Refused before it is written out, as Go refuses an expression
                        // too large.
                        let size = counted_size(&last, min, max);
                        self.written = self.written.saturating_add(size);
                        if self.written > self.limit {
                            return Err(PatternError::BadPattern);
                        }
                        nodes.push(counted(last, min, max, size));
                    }
                    // A brace that makes no count is itself.
                    None => {
                        self.pos += 1;
                        nodes.push(Node::Char(CharTest::Lit('{')));
                    }
                },
                '(' => {
                    self.pos += 1;
                    if self.peek() == Some('?') {
                        // Flags and non-capturing groups never come out of `compile`.
                        return Err(PatternError::BadPattern);
                    }
                    let inner = self.seq(true)?;
                    nodes.push(Node::Group(inner));
                }
                '[' => {
                    let class = self.class()?;
                    nodes.push(Node::Char(CharTest::Class(class)));
                }
                '.' => {
                    self.pos += 1;
                    nodes.push(Node::Char(CharTest::AnyNotNewline));
                }
                '^' => {
                    self.pos += 1;
                    nodes.push(Node::Assert(Assertion::BeginText));
                }
                '$' => {
                    self.pos += 1;
                    nodes.push(Node::Assert(Assertion::EndText));
                }
                '*' | '+' | '?' => {
                    self.pos += 1;
                    let kind = match c {
                        '*' => RepeatKind::Star,
                        '+' => RepeatKind::Plus,
                        _ => RepeatKind::Quest,
                    };
                    let last = match nodes.pop() {
                        None | Some(Node::Repeat(..) | Node::Counted(..)) => {
                            return Err(PatternError::BadPattern)
                        }
                        Some(last) => last,
                    };
                    // A non-greedy marker changes nothing for a yes or no answer.
                    if self.peek() == Some('?') {
                        self.pos += 1;
                    }
                    nodes.push(Node::Repeat(Box::new(last), kind));
                }
                '\\' => self.escape(&mut nodes)?,
                _ => {
                    self.pos += 1;
                    nodes.push(Node::Char(CharTest::Lit(c)));
                }
            }
        }
    }

    /// `{n}`, `{n,}` or `{n,m}` at the position, consumed; None, and nothing consumed,
    /// when the brace starts no count.
    fn repeat_count(&mut self) -> Option<(u32, Option<u32>)> {
        let start = self.pos;
        let mut i = start + 1;
        let number = |i: &mut usize| -> Option<u32> {
            let from = *i;
            while self.s.get(*i).is_some_and(char::is_ascii_digit) {
                *i += 1;
            }
            // Go's parseInt takes no leading zero: such a brace is itself.
            if *i == from || (*i - from > 1 && self.s[from] == '0') {
                return None;
            }
            // Past what any count may be, as the reference's own parse gives up.
            let text: String = self.s[from..*i].iter().collect();
            Some(text.parse().unwrap_or(u32::MAX))
        };
        let min = number(&mut i)?;
        let max = match self.s.get(i) {
            Some(',') => {
                i += 1;
                if self.s.get(i) == Some(&'}') {
                    None
                } else {
                    Some(number(&mut i)?)
                }
            }
            _ => Some(min),
        };
        if self.s.get(i) != Some(&'}') {
            return None;
        }
        self.pos = i + 1;
        Some((min, max))
    }

    fn escape(&mut self, nodes: &mut Vec<Node>) -> Result<(), PatternError> {
        let Some(c) = self.peek_at(1) else {
            return Err(PatternError::BadPattern);
        };
        let assertion = match c {
            'A' => Some(Assertion::BeginText),
            'z' => Some(Assertion::EndText),
            'b' => Some(Assertion::WordBoundary),
            'B' => Some(Assertion::NoWordBoundary),
            _ => None,
        };
        if let Some(a) = assertion {
            self.pos += 2;
            nodes.push(Node::Assert(a));
            return Ok(());
        }
        match c {
            'C' | 'p' | 'P' => return Err(PatternError::BadPattern),
            'Q' => {
                self.pos += 2;
                while self.pos < self.s.len() {
                    if self.peek() == Some('\\') && self.peek_at(1) == Some('E') {
                        self.pos += 2;
                        break;
                    }
                    nodes.push(Node::Char(CharTest::Lit(self.s[self.pos])));
                    self.pos += 1;
                }
                return Ok(());
            }
            _ => {}
        }
        if let Some((set, negated)) = perl_class(c) {
            self.pos += 2;
            nodes.push(Node::Char(CharTest::Class(Class {
                items: vec![ClassItem::Set(set, negated)],
                negated: false,
            })));
            return Ok(());
        }
        let r = self.escaped_char()?;
        nodes.push(Node::Char(CharTest::Lit(r)));
        Ok(())
    }

    /// One escaped character, positioned on the backslash.
    fn escaped_char(&mut self) -> Result<char, PatternError> {
        self.pos += 1;
        let c = self.bump()?;
        let octal = |c: Option<char>| c.is_some_and(|c| ('0'..='7').contains(&c));
        let r = match c {
            // A lone digit would be a back reference, which the reference rejects.
            '1'..='7' if !octal(self.peek()) => return Err(PatternError::BadPattern),
            '0'..='7' => {
                let mut r = c as u32 - '0' as u32;
                for _ in 1..3 {
                    if !octal(self.peek()) {
                        break;
                    }
                    r = r * 8 + (self.bump()? as u32 - '0' as u32);
                }
                r
            }
            'x' => {
                let first = self.bump()?;
                if first == '{' {
                    let mut r: u32 = 0;
                    let mut count = 0;
                    loop {
                        let d = self.bump()?;
                        if d == '}' {
                            break;
                        }
                        r = r * 16 + d.to_digit(16).ok_or(PatternError::BadPattern)?;
                        if r > 0x10ffff {
                            return Err(PatternError::BadPattern);
                        }
                        count += 1;
                    }
                    if count == 0 {
                        return Err(PatternError::BadPattern);
                    }
                    r
                } else {
                    let second = self.bump()?;
                    match (first.to_digit(16), second.to_digit(16)) {
                        (Some(x), Some(y)) => x * 16 + y,
                        _ => return Err(PatternError::BadPattern),
                    }
                }
            }
            'a' => 7,
            'f' => 12,
            'n' => 10,
            'r' => 13,
            't' => 9,
            'v' => 11,
            c if c.is_ascii() && !c.is_ascii_alphanumeric() => c as u32,
            _ => return Err(PatternError::BadPattern),
        };
        char::from_u32(r).ok_or(PatternError::BadPattern)
    }

    fn class(&mut self) -> Result<Class, PatternError> {
        self.pos += 1;
        let mut class = Class {
            items: Vec::new(),
            negated: false,
        };
        if self.peek() == Some('^') {
            self.pos += 1;
            class.negated = true;
        }
        // `]` right after the opening bracket is a member.
        let mut first = true;
        loop {
            match self.peek() {
                None => return Err(PatternError::BadPattern),
                Some(']') if !first => {
                    self.pos += 1;
                    return Ok(class);
                }
                _ => {}
            }
            first = false;
            if self.peek() == Some('[') && self.peek_at(1) == Some(':') {
                // The name ends at the first `:]` anywhere further on, as in the
                // reference.
                let rest = &self.s[self.pos + 2..];
                if let Some(i) = rest.windows(2).position(|w| w == [':', ']']) {
                    let name: String = rest[..i].iter().collect();
                    let (name, negated) = match name.strip_prefix('^') {
                        Some(n) => (n.to_string(), true),
                        None => (name, false),
                    };
                    let set = posix_group(&name).ok_or(PatternError::BadPattern)?;
                    class.items.push(ClassItem::Set(set, negated));
                    self.pos += 2 + i + 2;
                    continue;
                }
            }
            if self.peek() == Some('\\') {
                match self.peek_at(1) {
                    Some('p') | Some('P') => return Err(PatternError::BadPattern),
                    Some(c) => {
                        if let Some((set, negated)) = perl_class(c) {
                            self.pos += 2;
                            class.items.push(ClassItem::Set(set, negated));
                            continue;
                        }
                    }
                    None => {}
                }
            }
            let lo = self.class_char()?;
            let mut hi = lo;
            if self.peek() == Some('-') && self.peek_at(1).is_some_and(|c| c != ']') {
                self.pos += 1;
                hi = self.class_char()?;
                if hi < lo {
                    return Err(PatternError::BadPattern);
                }
            }
            class.items.push(ClassItem::Range(lo as u32, hi as u32));
        }
    }

    fn class_char(&mut self) -> Result<char, PatternError> {
        match self.peek() {
            None => Err(PatternError::BadPattern),
            Some('\\') => self.escaped_char(),
            Some(c) => {
                self.pos += 1;
                Ok(c)
            }
        }
    }
}

/// The largest count of a repetition, as the reference takes it.
const MAX_REPEAT: u32 = 1000;

/// The largest expression Go's regexp parser takes, in the units of its calcSize: a
/// program of 128 MiB of instructions.
const GO_MAX_SIZE: usize = (128 << 20) / 40;

/// The size Go's parser gives a node (calcSize).
fn go_size(node: &Node) -> usize {
    let seq = |nodes: &[Node]| nodes.iter().map(go_size).sum::<usize>().max(1);
    match node {
        Node::Char(_) | Node::Assert(_) => 1,
        Node::Group(inner) => 2 + seq(inner),
        Node::Alt(branches) => {
            branches.iter().map(|b| seq(b)).sum::<usize>() + branches.len().saturating_sub(1)
        }
        Node::Repeat(inner, RepeatKind::Star) => 2 + go_size(inner),
        Node::Repeat(inner, _) => 1 + go_size(inner),
        Node::Counted(_, size) => *size,
    }
}

/// What Go's parser sizes `node{min,max}` at.
fn counted_size(node: &Node, min: u32, max: Option<u32>) -> usize {
    let sub = go_size(node);
    let (min, max) = (min as usize, max.map(|m| m as usize));
    match max {
        None if min == 0 => 2 + sub,
        None => 1 + min * sub,
        Some(max) => max * sub + (max - min),
    }
    .max(1)
}

/// `node` repeated `min` times and up to `max` (no bound without it), written out as the
/// plain repetitions the program knows.
fn counted(node: Node, min: u32, max: Option<u32>, size: usize) -> Node {
    let mut out: Vec<Node> = (0..min).map(|_| node.clone()).collect();
    match max {
        None => out.push(Node::Repeat(Box::new(node), RepeatKind::Star)),
        Some(max) => {
            // x{2,4} is xx(x(x)?)?: each optional copy inside the one before.
            let mut tail: Option<Node> = None;
            for _ in min..max {
                let mut inner = vec![node.clone()];
                inner.extend(tail.take());
                tail = Some(Node::Repeat(
                    Box::new(Node::Group(inner)),
                    RepeatKind::Quest,
                ));
            }
            out.extend(tail);
        }
    }
    Node::Counted(out, size)
}

fn emit(nodes: &[Node], prog: &mut Vec<Inst>) {
    for node in nodes {
        match node {
            Node::Char(test) => prog.push(Inst::Char(test.clone())),
            Node::Assert(a) => prog.push(Inst::Assert(*a)),
            Node::Group(inner) | Node::Counted(inner, _) => emit(inner, prog),
            Node::Alt(branches) => {
                // Split to this branch or the next; each ends with a jump past them all.
                let mut jumps = Vec::new();
                for (i, branch) in branches.iter().enumerate() {
                    if i + 1 == branches.len() {
                        emit(branch, prog);
                        break;
                    }
                    let split = prog.len();
                    prog.push(Inst::Split(0, 0));
                    emit(branch, prog);
                    jumps.push(prog.len());
                    prog.push(Inst::Jmp(0));
                    prog[split] = Inst::Split(split + 1, prog.len());
                }
                let end = prog.len();
                for j in jumps {
                    prog[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat(inner, kind) => {
                let start = prog.len();
                match kind {
                    RepeatKind::Star => {
                        prog.push(Inst::Split(0, 0));
                        emit(std::slice::from_ref(inner), prog);
                        prog.push(Inst::Jmp(start));
                        let end = prog.len();
                        prog[start] = Inst::Split(start + 1, end);
                    }
                    RepeatKind::Plus => {
                        emit(std::slice::from_ref(inner), prog);
                        let end = prog.len() + 1;
                        prog.push(Inst::Split(start, end));
                    }
                    RepeatKind::Quest => {
                        prog.push(Inst::Split(0, 0));
                        emit(std::slice::from_ref(inner), prog);
                        let end = prog.len();
                        prog[start] = Inst::Split(start + 1, end);
                    }
                }
            }
        }
    }
}

impl Regexp {
    pub fn new(source: &str) -> Result<Regexp, PatternError> {
        Regexp::within(source, GO_MAX_SIZE)
    }

    fn within(source: &str, limit: usize) -> Result<Regexp, PatternError> {
        let chars: Vec<char> = source.chars().collect();
        let mut parser = RegexpParser {
            s: &chars,
            pos: 0,
            written: 0,
            limit,
        };
        let nodes = parser.seq(false)?;
        if nodes.iter().map(go_size).sum::<usize>() > limit {
            return Err(PatternError::BadPattern);
        }
        let mut prog = Vec::new();
        emit(&nodes, &mut prog);
        prog.push(Inst::Match);
        Ok(Regexp {
            source: source.to_string(),
            prog,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Whether the expression matches anywhere in `text`. The expressions of `compile`
    /// are anchored at both ends, which makes it a match of the whole text.
    pub fn is_match(&self, text: &str) -> bool {
        let input: Vec<char> = text.chars().collect();
        let mut seen = vec![usize::MAX; self.prog.len()];
        let mut current: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        for pos in 0..=input.len() {
            let prev = pos.checked_sub(1).map(|i| input[i]);
            let here = input.get(pos).copied();
            // A thread starts at every position: the search is not anchored by itself.
            if self.add(&mut current, &mut seen, pos, 0, prev, here) {
                return true;
            }
            let Some(c) = here else {
                break;
            };
            next.clear();
            let after = input.get(pos + 1).copied();
            for &pc in &current {
                if let Inst::Char(test) = &self.prog[pc] {
                    if test.matches(c)
                        && self.add(&mut next, &mut seen, pos + 1, pc + 1, Some(c), after)
                    {
                        return true;
                    }
                }
            }
            std::mem::swap(&mut current, &mut next);
        }
        false
    }

    /// Adds the thread at `pc` and everything reachable from it without reading a
    /// character; true once the match instruction is reached.
    fn add(
        &self,
        list: &mut Vec<usize>,
        seen: &mut [usize],
        pos: usize,
        pc: usize,
        prev: Option<char>,
        here: Option<char>,
    ) -> bool {
        if seen[pc] == pos {
            return false;
        }
        seen[pc] = pos;
        match &self.prog[pc] {
            Inst::Match => true,
            Inst::Char(_) => {
                list.push(pc);
                false
            }
            Inst::Jmp(to) => self.add(list, seen, pos, *to, prev, here),
            Inst::Split(a, b) => {
                self.add(list, seen, pos, *a, prev, here)
                    || self.add(list, seen, pos, *b, prev, here)
            }
            Inst::Assert(a) => {
                let holds = match a {
                    Assertion::BeginText => prev.is_none(),
                    Assertion::EndText => here.is_none(),
                    Assertion::WordBoundary => is_word_char(prev) != is_word_char(here),
                    Assertion::NoWordBoundary => is_word_char(prev) == is_word_char(here),
                };
                holds && self.add(list, seen, pos, pc + 1, prev, here)
            }
        }
    }
}

/// The reference's single-path glob (Go's filepath.Match): `*` and `?` do not match a
/// slash, `[...]` and `[^...]` are classes with ranges, `\` escapes. What follows the
/// chunk where the name failed is not looked at, as filepath.Match does not (path.Match
/// does): `obj/*\` is no error against ".".
pub fn glob_match(pattern: &str, name: &str) -> Result<bool, PatternError> {
    glob_match_bytes(pattern.as_bytes(), name.as_bytes())
}

pub(crate) fn glob_match_bytes(pattern: &[u8], name: &[u8]) -> Result<bool, PatternError> {
    let mut pattern = pattern;
    let mut name = name;
    'pattern: while !pattern.is_empty() {
        let (star, chunk, rest) = scan_chunk(pattern);
        pattern = rest;
        if star && chunk.is_empty() {
            // A trailing star matches the rest unless it has a slash.
            return Ok(!name.contains(&b'/'));
        }
        let (t, err) = match match_chunk(chunk, name) {
            Ok(t) => (t, None),
            Err(e) => (None, Some(e)),
        };
        // The last chunk must use up the name, or a star could still match more.
        if let Some(t) = t {
            if t.is_empty() || !pattern.is_empty() {
                name = t;
                continue;
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
        if star {
            let mut i = 0;
            while i < name.len() && name[i] != b'/' {
                match match_chunk(chunk, &name[i + 1..]) {
                    Ok(Some(t)) => {
                        if pattern.is_empty() && !t.is_empty() {
                            i += 1;
                            continue;
                        }
                        name = t;
                        continue 'pattern;
                    }
                    Ok(None) => {}
                    Err(e) => return Err(e),
                }
                i += 1;
            }
        }
        return Ok(false);
    }
    Ok(name.is_empty())
}

fn scan_chunk(pattern: &[u8]) -> (bool, &[u8], &[u8]) {
    let mut pattern = pattern;
    let mut star = false;
    while let Some(rest) = pattern.strip_prefix(b"*") {
        pattern = rest;
        star = true;
    }
    let mut in_range = false;
    let mut i = 0;
    while i < pattern.len() {
        match pattern[i] {
            b'\\' => {
                // A trailing backslash is reported by match_chunk.
                if i + 1 < pattern.len() {
                    i += 1;
                }
            }
            b'[' => in_range = true,
            b']' => in_range = false,
            b'*' if !in_range => break,
            _ => {}
        }
        i += 1;
    }
    (star, &pattern[..i], &pattern[i..])
}

fn match_chunk<'a>(chunk: &[u8], s: &'a [u8]) -> Result<Option<&'a [u8]>, PatternError> {
    let mut chunk = chunk;
    let mut s = s;
    // Once the match has failed the chunk is still read to the end, to check it.
    let mut failed = false;
    while let Some(&c) = chunk.first() {
        if !failed && s.is_empty() {
            failed = true;
        }
        match c {
            b'[' => {
                let mut r = 0u32;
                if !failed {
                    let (rr, n) = decode_rune(s);
                    r = rr;
                    s = &s[n..];
                }
                chunk = &chunk[1..];
                let mut negated = false;
                if chunk.first() == Some(&b'^') {
                    negated = true;
                    chunk = &chunk[1..];
                }
                let mut matched = false;
                let mut nrange = 0;
                loop {
                    if chunk.first() == Some(&b']') && nrange > 0 {
                        chunk = &chunk[1..];
                        break;
                    }
                    let (lo, rest) = get_esc(chunk)?;
                    chunk = rest;
                    let mut hi = lo;
                    if chunk.first() == Some(&b'-') {
                        let (h, rest) = get_esc(&chunk[1..])?;
                        hi = h;
                        chunk = rest;
                    }
                    if lo <= r && r <= hi {
                        matched = true;
                    }
                    nrange += 1;
                }
                if matched == negated {
                    failed = true;
                }
            }
            b'?' => {
                if !failed {
                    if s[0] == b'/' {
                        failed = true;
                    }
                    let (_, n) = decode_rune(s);
                    s = &s[n..];
                }
                chunk = &chunk[1..];
            }
            _ => {
                if c == b'\\' {
                    chunk = &chunk[1..];
                    if chunk.is_empty() {
                        return Err(PatternError::BadPattern);
                    }
                }
                if !failed {
                    if chunk[0] != s[0] {
                        failed = true;
                    }
                    s = &s[1..];
                }
                chunk = &chunk[1..];
            }
        }
    }
    if failed {
        return Ok(None);
    }
    Ok(Some(s))
}

fn get_esc(chunk: &[u8]) -> Result<(u32, &[u8]), PatternError> {
    let mut chunk = chunk;
    match chunk.first() {
        None | Some(b'-') | Some(b']') => return Err(PatternError::BadPattern),
        Some(b'\\') => {
            chunk = &chunk[1..];
            if chunk.is_empty() {
                return Err(PatternError::BadPattern);
            }
        }
        _ => {}
    }
    let (r, n) = decode_rune(chunk);
    if r == 0xfffd && n == 1 {
        return Err(PatternError::BadPattern);
    }
    let rest = &chunk[n..];
    if rest.is_empty() {
        return Err(PatternError::BadPattern);
    }
    Ok((r, rest))
}

/// Decodes the first character the way the reference does: an invalid byte is U+FFFD
/// of width one.
fn decode_rune(s: &[u8]) -> (u32, usize) {
    let Some(&b0) = s.first() else {
        return (0xfffd, 0);
    };
    let n = match b0 {
        0x00..=0x7f => return (b0 as u32, 1),
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return (0xfffd, 1),
    };
    match s.get(..n).map(std::str::from_utf8) {
        Some(Ok(st)) => st.chars().next().map_or((0xfffd, 1), |c| (c as u32, n)),
        _ => (0xfffd, 1),
    }
}

/// Whether a path element has glob characters that are not escaped.
pub(crate) fn contains_wildcards(name: &[u8]) -> bool {
    let mut i = 0;
    while i < name.len() {
        match name[i] {
            b'\\' => i += 1,
            b'*' | b'?' | b'[' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Text from bytes that may not be UTF-8, each invalid byte becoming U+FFFD, which is
/// how the reference's string matching sees such names.
pub(crate) fn lossy(bytes: &[u8]) -> Cow<'_, str> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&rest[..valid]));
                out.push('\u{fffd}');
                rest = &rest[valid + 1..];
            }
        }
    }
    Cow::Owned(out)
}

/// Reads an ignore file: one pattern per line, a leading byte order mark dropped, lines
/// starting with `#` skipped, whitespace trimmed, empty lines dropped, each pattern
/// cleaned lexically and a leading `/` removed (the root of the context is implied).
/// `!` is kept in front of the cleaned pattern.
pub fn read_all<R: Read>(mut reader: R) -> io::Result<Vec<String>> {
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    let mut lines: Vec<&[u8]> = data.split(|&b| b == b'\n').collect();
    if data.ends_with(b"\n") || data.is_empty() {
        lines.pop();
    }
    let mut excludes = Vec::new();
    for (n, line) in lines.into_iter().enumerate() {
        let mut line = line.strip_suffix(b"\r").unwrap_or(line);
        if n == 0 {
            line = line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line);
        }
        let line = lossy(line);
        if line.starts_with('#') {
            continue;
        }
        let mut pattern = line.trim();
        if pattern.is_empty() {
            continue;
        }
        let invert = pattern.starts_with('!');
        if invert {
            pattern = pattern[1..].trim();
        }
        let mut cleaned = String::new();
        if !pattern.is_empty() {
            cleaned = lexical::clean_str(pattern);
            if cleaned.len() > 1 && cleaned.starts_with('/') {
                cleaned.remove(0);
            }
        }
        if invert {
            cleaned.insert(0, '!');
        }
        excludes.push(cleaned);
    }
    Ok(excludes)
}

/// Lexical path operations with the reference's results (Go's path/filepath on Unix),
/// on bytes so that names that are not UTF-8 survive.
pub(crate) mod lexical {
    /// The shortest equivalent path: repeated slashes, `.` elements and `..` after a
    /// real element are removed, `..` at the root is dropped, "" becomes ".".
    pub fn clean(path: &[u8]) -> Vec<u8> {
        if path.is_empty() {
            return b".".to_vec();
        }
        let rooted = path[0] == b'/';
        let n = path.len();
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let (mut r, mut dotdot) = (0, 0);
        if rooted {
            out.push(b'/');
            r = 1;
            dotdot = 1;
        }
        while r < n {
            if path[r] == b'/' || (path[r] == b'.' && (r + 1 == n || path[r + 1] == b'/')) {
                r += 1;
            } else if path[r] == b'.' && path[r + 1] == b'.' && (r + 2 == n || path[r + 2] == b'/')
            {
                r += 2;
                if out.len() > dotdot {
                    let mut w = out.len() - 1;
                    while w > dotdot && out[w] != b'/' {
                        w -= 1;
                    }
                    out.truncate(w);
                } else if !rooted {
                    if !out.is_empty() {
                        out.push(b'/');
                    }
                    out.extend_from_slice(b"..");
                    dotdot = out.len();
                }
            } else {
                if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                    out.push(b'/');
                }
                while r < n && path[r] != b'/' {
                    out.push(path[r]);
                    r += 1;
                }
            }
        }
        if out.is_empty() {
            out.push(b'.');
        }
        out
    }

    pub fn clean_str(path: &str) -> String {
        to_string(clean(path.as_bytes()))
    }

    /// The non-empty elements joined with slashes and cleaned; "" when all are empty.
    pub fn join(elems: &[&[u8]]) -> Vec<u8> {
        let parts: Vec<&[u8]> = elems.iter().copied().filter(|e| !e.is_empty()).collect();
        if parts.is_empty() {
            return Vec::new();
        }
        clean(&parts.join(&b'/'))
    }

    pub fn join_str(elems: &[&str]) -> String {
        let elems: Vec<&[u8]> = elems.iter().map(|e| e.as_bytes()).collect();
        to_string(join(&elems))
    }

    /// Splits after the last slash: the directory keeps its trailing slash.
    pub fn split(path: &[u8]) -> (&[u8], &[u8]) {
        let i = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
        (&path[..i], &path[i..])
    }

    /// Everything but the last element, cleaned.
    pub fn dir(path: &[u8]) -> Vec<u8> {
        clean(split(path).0)
    }

    pub fn dir_str(path: &str) -> String {
        to_string(dir(path.as_bytes()))
    }

    /// The last element, trailing slashes ignored; "." for "", "/" for slashes only.
    pub fn base(path: &[u8]) -> Vec<u8> {
        if path.is_empty() {
            return b".".to_vec();
        }
        let mut p = path;
        while p.len() > 1 && p[p.len() - 1] == b'/' {
            p = &p[..p.len() - 1];
        }
        if p == b"/" {
            return b"/".to_vec();
        }
        split(p).1.to_vec()
    }

    pub fn is_abs(path: &[u8]) -> bool {
        path.first() == Some(&b'/')
    }

    /// `targ` relative to `base`, both cleaned first, as Go's filepath.Rel does it.
    pub fn rel(base: &[u8], targ: &[u8]) -> Option<Vec<u8>> {
        let base = clean(base);
        let targ = clean(targ);
        if base == targ {
            return Some(b".".to_vec());
        }
        let base: &[u8] = if base == b"." { b"" } else { &base };
        if is_abs(base) != is_abs(&targ) {
            return None;
        }
        let bl = base.len();
        let tl = targ.len();
        let (mut b0, mut bi, mut t0, mut ti) = (0, 0, 0, 0);
        loop {
            while bi < bl && base[bi] != b'/' {
                bi += 1;
            }
            while ti < tl && targ[ti] != b'/' {
                ti += 1;
            }
            if targ[t0..ti] != base[b0..bi] {
                break;
            }
            if bi < bl {
                bi += 1;
            }
            if ti < tl {
                ti += 1;
            }
            b0 = bi;
            t0 = ti;
        }
        if &base[b0..bi] == b".." {
            return None;
        }
        if b0 != bl {
            let seps = base[b0..bl].iter().filter(|&&b| b == b'/').count();
            let mut out = b"..".to_vec();
            for _ in 0..seps {
                out.extend_from_slice(b"/..");
            }
            if t0 != tl {
                out.push(b'/');
                out.extend_from_slice(&targ[t0..]);
            }
            return Some(out);
        }
        Some(targ[t0..].to_vec())
    }

    fn to_string(bytes: Vec<u8>) -> String {
        match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches() {
        assert!(matches("fileutils.go", &["*"]).unwrap());
    }

    #[test]
    fn pattern_matches() {
        assert!(matches("fileutils.go", &["*.go"]).unwrap());
    }

    #[test]
    fn braces_and_bars_reach_the_expression_as_the_reference_has_it() {
        // `^[^/]*\.pem|[^/]*\.key$`: a top-level name that starts like a .pem, or a
        // .key anywhere.
        let keys = ["*.pem|*.key"];
        assert!(matches("server.pem", &keys).unwrap());
        assert!(matches("certs/server.key", &keys).unwrap());
        assert!(matches("id.pemx", &keys).unwrap());
        assert!(!matches("certs/server.pem", &keys).unwrap());
        assert!(!matches("notes.txt", &keys).unwrap());
        // In a pattern that is an expression (one with a wildcard), a count repeats and
        // a brace that makes none is itself; without a wildcard all of it is literal.
        assert!(matches("aa", &["a{2}*"]).unwrap());
        assert!(matches("aab", &["a{2}*"]).unwrap());
        assert!(!matches("ab", &["a{2}*"]).unwrap());
        assert!(matches("abbbc", &["ab{2,3}c*"]).unwrap());
        assert!(!matches("abbbbc", &["ab{2,3}c*"]).unwrap());
        assert!(matches("x{y}z", &["x{y}*"]).unwrap());
        assert!(matches("x{,2}", &["x{,2}*"]).unwrap());
        assert!(matches("a|b", &["a|b"]).unwrap());
        // Counts the reference refuses.
        for bad in ["a{1001}*", "a{3,2}*", "a{2}{3}*"] {
            assert!(matches("a", &[bad]).is_err(), "{bad}");
        }
        // A count with a leading zero is no count to Go: the brace is itself.
        assert!(!matches("aa", &["a{02}*"]).unwrap());
        assert!(matches("a{02}", &["a{02}*"]).unwrap());
        assert!(!matches("a", &["a{1,02}*"]).unwrap());
        assert!(matches("a{1,02}", &["a{1,02}*"]).unwrap());
        assert!(matches("b", &["a{0}b*"]).unwrap());
        assert!(matches("ab", &["a{0,}b*"]).unwrap());
    }

    #[test]
    fn an_expression_too_large_for_go_is_refused() {
        assert_eq!(GO_MAX_SIZE, 3_355_443);
        // a{1000} is 1000 to Go's parser, a{2,5} 5 * 1 + 3, a* 2 + 1.
        let mut parser = RegexpParser {
            s: &[],
            pos: 0,
            written: 0,
            limit: usize::MAX,
        };
        assert_eq!(
            counted_size(&Node::Char(CharTest::Lit('a')), 1000, Some(1000)),
            1000
        );
        assert_eq!(counted_size(&Node::Char(CharTest::Lit('a')), 2, Some(5)), 8);
        assert_eq!(counted_size(&Node::Char(CharTest::Lit('a')), 0, None), 3);
        assert_eq!(counted_size(&Node::Char(CharTest::Lit('a')), 0, Some(0)), 1);
        let source: Vec<char> = "^a{2}[^/]*$".chars().collect();
        parser.s = &source;
        let nodes = parser.seq(false).unwrap();
        assert_eq!(nodes.iter().map(go_size).sum::<usize>(), 1 + 2 + 3 + 1);
        // Past the limit, refused before what is past it is written out.
        let ten = "a{1000}".repeat(10);
        assert!(Regexp::within(&format!("^{ten}$"), 10_002).is_ok());
        assert!(Regexp::within(&format!("^{ten}$"), 10_001).is_err());
        assert!(Regexp::within(&format!("^{ten}a{{1000}}$"), 10_500).is_err());
    }

    #[test]
    fn exclusion_pattern_matches_pattern_before() {
        assert!(matches("fileutils.go", &["!fileutils.go", "*.go"]).unwrap());
    }

    #[test]
    fn pattern_matches_folder_exclusions() {
        assert!(!matches("docs/README.md", &["docs", "!docs/README.md"]).unwrap());
    }

    #[test]
    fn pattern_matches_folder_with_slash_exclusions() {
        assert!(!matches("docs/README.md", &["docs/", "!docs/README.md"]).unwrap());
    }

    #[test]
    fn pattern_matches_folder_wildcard_exclusions() {
        assert!(!matches("docs/README.md", &["docs/*", "!docs/README.md"]).unwrap());
    }

    #[test]
    fn exclusion_pattern_matches_pattern_after() {
        assert!(!matches("fileutils.go", &["*.go", "!fileutils.go"]).unwrap());
    }

    #[test]
    fn exclusion_pattern_matches_whole_directory() {
        assert!(!matches(".", &["*.go"]).unwrap());
    }

    #[test]
    fn single_exclamation_error() {
        assert_eq!(
            matches("fileutils.go", &["!"]),
            Err(PatternError::IllegalExclusion)
        );
    }

    #[test]
    fn matches_with_no_patterns() {
        assert!(!matches::<&str>("/any/path/there", &[]).unwrap());
    }

    #[test]
    fn matches_with_malformed_patterns() {
        assert_eq!(
            matches("/any/path/there", &["["]),
            Err(PatternError::BadPattern)
        );
        // filepath.Match does not look past where "." failed: these lines are taken
        // and match nothing, as a .dockerignore written on Windows may have them.
        for pattern in ["obj/*\\", "x*\\"] {
            assert_eq!(matches("obj/x", &[pattern]), Ok(false), "{pattern}");
        }
    }

    const MATCH_CASES: &[(&str, &str, bool)] = &[
        ("**", "file", true),
        ("**", "file/", true),
        ("**/", "file", true),
        ("**/", "file/", true),
        ("**", "/", true),
        ("**/", "/", true),
        ("**", "dir/file", true),
        ("**/", "dir/file", true),
        ("**", "dir/file/", true),
        ("**/", "dir/file/", true),
        ("**/**", "dir/file", true),
        ("**/**", "dir/file/", true),
        ("dir/**", "dir/file", true),
        ("dir/**", "dir/file/", true),
        ("dir/**", "dir/dir2/file", true),
        ("dir/**", "dir/dir2/file/", true),
        ("**/dir", "dir", true),
        ("**/dir", "dir/file", true),
        ("**/dir2/*", "dir/dir2/file", true),
        ("**/dir2/*", "dir/dir2/file/", true),
        ("**/dir2/**", "dir/dir2/dir3/file", true),
        ("**/dir2/**", "dir/dir2/dir3/file/", true),
        ("**file", "file", true),
        ("**file", "dir/file", true),
        ("**/file", "dir/file", true),
        ("**file", "dir/dir/file", true),
        ("**/file", "dir/dir/file", true),
        ("**/file*", "dir/dir/file", true),
        ("**/file*", "dir/dir/file.txt", true),
        ("**/file*txt", "dir/dir/file.txt", true),
        ("**/file*.txt", "dir/dir/file.txt", true),
        ("**/file*.txt*", "dir/dir/file.txt", true),
        ("**/**/*.txt", "dir/dir/file.txt", true),
        ("**/**/*.txt2", "dir/dir/file.txt", false),
        ("**/*.txt", "file.txt", true),
        ("**/**/*.txt", "file.txt", true),
        ("a**/*.txt", "a/file.txt", true),
        ("a**/*.txt", "a/dir/file.txt", true),
        ("a**/*.txt", "a/dir/dir/file.txt", true),
        ("a/*.txt", "a/dir/file.txt", false),
        ("a/*.txt", "a/file.txt", true),
        ("a/*.txt**", "a/file.txt", true),
        ("a[b-d]e", "ae", false),
        ("a[b-d]e", "ace", true),
        ("a[b-d]e", "aae", false),
        ("a[^b-d]e", "aze", true),
        (".*", ".foo", true),
        (".*", "foo", false),
        ("abc.def", "abcdef", false),
        ("abc.def", "abc.def", true),
        ("abc.def", "abcZdef", false),
        ("abc?def", "abcZdef", true),
        ("abc?def", "abcdef", false),
        ("a\\\\", "a\\", true),
        ("**/foo/bar", "foo/bar", true),
        ("**/foo/bar", "dir/foo/bar", true),
        ("**/foo/bar", "dir/dir2/foo/bar", true),
        ("abc/**", "abc", false),
        ("abc/**", "abc/def", true),
        ("abc/**", "abc/def/ghi", true),
        ("**/.foo", ".foo", true),
        ("**/.foo", "bar.foo", false),
        ("a(b)c/def", "a(b)c/def", true),
        ("a(b)c/def", "a(b)c/xyz", false),
        ("a.|)$(}+{bc", "a.|)$(}+{bc", true),
        (
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            true,
        ),
        (
            "dist/*.whl",
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            true,
        ),
        ("a\\*b", "a*b", true),
    ];

    const MULTI_CASES: &[(&[&str], &str, bool)] = &[
        (&["**", "!util/build/web"], "util/build/web/foo", false),
        (
            &["**", "!util/build/web", "util/build/web/foo"],
            "util/build/web/foo",
            true,
        ),
        (
            &[
                "**",
                "!dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            ],
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            false,
        ),
        (
            &["**", "!dist/*.whl"],
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            false,
        ),
    ];

    fn parent_dirs(text: &str) -> Option<Vec<String>> {
        let parent = lexical::dir_str(text);
        (parent != ".").then(|| parent.split('/').map(str::to_string).collect())
    }

    #[test]
    fn matches_or_parent_matches_table() {
        for &(pattern, text, pass) in MATCH_CASES {
            let pm = PatternMatcher::new(&[pattern]).unwrap();
            let res = pm.matches_or_parent_matches(text).unwrap_or(false);
            assert_eq!(res, pass, "pattern={pattern:?} text={text:?}");
        }
        for &(patterns, text, pass) in MULTI_CASES {
            let pm = PatternMatcher::new(patterns).unwrap();
            let res = pm.matches_or_parent_matches(text).unwrap_or(false);
            assert_eq!(res, pass, "patterns={patterns:?} text={text:?}");
        }
    }

    #[test]
    fn matches_using_parent_result_table() {
        for &(pattern, text, pass) in MATCH_CASES {
            let pm = PatternMatcher::new(&[pattern]).unwrap();
            let mut parent_matched = false;
            if let Some(dirs) = parent_dirs(text) {
                for i in 0..dirs.len() {
                    parent_matched = pm
                        .matches_using_parent_result(&dirs[..=i].join("/"), parent_matched)
                        .unwrap_or(false);
                }
            }
            let res = pm
                .matches_using_parent_result(text, parent_matched)
                .unwrap_or(false);
            assert_eq!(res, pass, "pattern={pattern:?} text={text:?}");
        }
    }

    fn check_parent_results(pm: &PatternMatcher, text: &str, pass: bool, desc: &str) {
        let mut info = MatchInfo::default();
        if let Some(dirs) = parent_dirs(text) {
            for i in 0..dirs.len() {
                info = pm
                    .matches_using_parent_results(&dirs[..=i].join("/"), &info)
                    .map(|(_, info)| info)
                    .unwrap_or_default();
            }
        }
        let res = pm
            .matches_using_parent_results(text, &info)
            .map(|(m, _)| m)
            .unwrap_or(false);
        assert_eq!(res, pass, "{desc}");
    }

    #[test]
    fn matches_using_parent_results_table() {
        for &(pattern, text, pass) in MATCH_CASES {
            let pm = PatternMatcher::new(&[pattern]).unwrap();
            check_parent_results(&pm, text, pass, &format!("{pattern:?} {text:?}"));
        }
        for &(patterns, text, pass) in MULTI_CASES {
            let pm = PatternMatcher::new(patterns).unwrap();
            check_parent_results(&pm, text, pass, &format!("{patterns:?} {text:?}"));
        }
    }

    #[test]
    fn matches_using_parent_results_no_context() {
        for &(pattern, text, pass) in MATCH_CASES {
            let pm = PatternMatcher::new(&[pattern]).unwrap();
            let (res, _) = pm
                .matches_using_parent_results(text, &MatchInfo::default())
                .unwrap_or_default();
            assert_eq!(res, pass, "pattern={pattern:?} text={text:?}");
        }
        for &(patterns, text, pass) in MULTI_CASES {
            let pm = PatternMatcher::new(patterns).unwrap();
            let (res, _) = pm
                .matches_using_parent_results(text, &MatchInfo::default())
                .unwrap_or_default();
            assert_eq!(res, pass, "patterns={patterns:?} text={text:?}");
        }
    }

    #[test]
    fn clean_patterns() {
        let pm = PatternMatcher::new(&["docs", "config"]).unwrap();
        assert_eq!(pm.patterns().len(), 2);
    }

    #[test]
    fn clean_patterns_strip_empty_patterns() {
        let pm = PatternMatcher::new(&["docs", "config", ""]).unwrap();
        assert_eq!(pm.patterns().len(), 2);
    }

    #[test]
    fn clean_patterns_exception_flag() {
        let pm = PatternMatcher::new(&["docs", "!docs/README.md"]).unwrap();
        assert!(pm.exclusions());
    }

    #[test]
    fn clean_patterns_leading_space_trimmed() {
        let pm = PatternMatcher::new(&["docs", "  !docs/README.md"]).unwrap();
        assert!(pm.exclusions());
    }

    #[test]
    fn clean_patterns_trailing_space_trimmed() {
        let pm = PatternMatcher::new(&["docs", "!docs/README.md  "]).unwrap();
        assert!(pm.exclusions());
    }

    #[test]
    fn clean_patterns_error_single_exception() {
        assert_eq!(
            PatternMatcher::new(&["!"]).unwrap_err(),
            PatternError::IllegalExclusion
        );
    }

    // The glob cases the reference borrows from Go's filepath.Match tests.
    const GLOB_CASES: &[(&str, &str, bool, bool)] = &[
        ("abc", "abc", true, false),
        ("*", "abc", true, false),
        ("*c", "abc", true, false),
        ("a*", "a", true, false),
        ("a*", "abc", true, false),
        ("a*", "ab/c", true, false),
        ("a*/b", "abc/b", true, false),
        ("a*/b", "a/c/b", false, false),
        ("a*b*c*d*e*/f", "axbxcxdxe/f", true, false),
        ("a*b*c*d*e*/f", "axbxcxdxexxx/f", true, false),
        ("a*b*c*d*e*/f", "axbxcxdxe/xxx/f", false, false),
        ("a*b*c*d*e*/f", "axbxcxdxexxx/fff", false, false),
        ("a*b?c*x", "abxbbxdbxebxczzx", true, false),
        ("a*b?c*x", "abxbbxdbxebxczzy", false, false),
        ("ab[c]", "abc", true, false),
        ("ab[b-d]", "abc", true, false),
        ("ab[e-g]", "abc", false, false),
        ("ab[^c]", "abc", false, false),
        ("ab[^b-d]", "abc", false, false),
        ("ab[^e-g]", "abc", true, false),
        ("a\\*b", "a*b", true, false),
        ("a\\*b", "ab", false, false),
        ("a?b", "a\u{263a}b", true, false),
        ("a[^a]b", "a\u{263a}b", true, false),
        ("a???b", "a\u{263a}b", false, false),
        ("a[^a][^a][^a]b", "a\u{263a}b", false, false),
        ("[a-\u{3b6}]*", "\u{3b1}", true, false),
        ("*[a-\u{3b6}]", "A", false, false),
        ("a?b", "a/b", false, false),
        ("a*b", "a/b", false, false),
        ("[\\]a]", "]", true, false),
        ("[\\-]", "-", true, false),
        ("[x\\-]", "x", true, false),
        ("[x\\-]", "-", true, false),
        ("[x\\-]", "z", false, false),
        ("[\\-x]", "x", true, false),
        ("[\\-x]", "-", true, false),
        ("[\\-x]", "a", false, false),
        ("[]a]", "]", false, true),
        ("[-]", "-", false, true),
        ("[x-]", "x", false, true),
        ("[x-]", "-", false, true),
        ("[x-]", "z", false, true),
        ("[-x]", "x", false, true),
        ("[-x]", "-", false, true),
        ("[-x]", "a", false, true),
        ("\\", "a", false, true),
        ("[a-b-c]", "a", false, true),
        ("[", "a", false, true),
        ("[^", "a", false, true),
        ("[^bc", "a", false, true),
        ("a[", "a", false, true),
        ("a[", "ab", false, true),
        ("*x", "xxx", true, false),
    ];

    #[test]
    fn glob_cases_through_matches() {
        for &(pattern, s, want, bad) in GLOB_CASES {
            let got = matches(s, &[pattern]);
            if bad {
                assert_eq!(got, Err(PatternError::BadPattern), "{pattern:?} {s:?}");
            } else {
                assert_eq!(got, Ok(want), "{pattern:?} {s:?}");
            }
        }
    }

    const COMPILE_CASES: &[(&str, MatchType, &str, &str)] = &[
        ("*", MatchType::Regexp, "^[^/]*$", "^[^\\\\]*$"),
        ("file*", MatchType::Regexp, "^file[^/]*$", "^file[^\\\\]*$"),
        ("*file", MatchType::Regexp, "^[^/]*file$", "^[^\\\\]*file$"),
        ("a*/b", MatchType::Regexp, "^a[^/]*/b$", "^a[^\\\\]*\\\\b$"),
        ("**", MatchType::Suffix, "", ""),
        ("**/**", MatchType::Regexp, "^(.*/)?.*$", "^(.*\\\\)?.*$"),
        ("dir/**", MatchType::Prefix, "", ""),
        ("**/dir", MatchType::Suffix, "", ""),
        (
            "**/dir2/*",
            MatchType::Regexp,
            "^(.*/)?dir2/[^/]*$",
            "^(.*\\\\)?dir2\\\\[^\\\\]*$",
        ),
        (
            "**/dir2/**",
            MatchType::Regexp,
            "^(.*/)?dir2/.*$",
            "^(.*\\\\)?dir2\\\\.*$",
        ),
        ("**file", MatchType::Suffix, "", ""),
        (
            "**/file*txt",
            MatchType::Regexp,
            "^(.*/)?file[^/]*txt$",
            "^(.*\\\\)?file[^\\\\]*txt$",
        ),
        (
            "**/**/*.txt",
            MatchType::Regexp,
            "^(.*/)?(.*/)?[^/]*\\.txt$",
            "^(.*\\\\)?(.*\\\\)?[^\\\\]*\\.txt$",
        ),
        ("a[b-d]e", MatchType::Regexp, "^a[b-d]e$", "^a[b-d]e$"),
        (".*", MatchType::Regexp, "^\\.[^/]*$", "^\\.[^\\\\]*$"),
        ("abc.def", MatchType::Exact, "", ""),
        (
            "abc?def",
            MatchType::Regexp,
            "^abc[^/]def$",
            "^abc[^\\\\]def$",
        ),
        ("**/foo/bar", MatchType::Suffix, "", ""),
        ("a(b)c/def", MatchType::Exact, "", ""),
        ("a.|)$(}+{bc", MatchType::Exact, "", ""),
        (
            "dist/proxy.py-2.4.0rc3.dev36+g08acad9-py3-none-any.whl",
            MatchType::Exact,
            "",
            "",
        ),
    ];

    #[test]
    fn compile_slash_and_backslash() {
        for sl in ['/', '\\'] {
            for &(pattern, want_type, want_re, want_win_re) in COMPILE_CASES {
                let mut pattern = lexical::clean_str(pattern);
                if sl != '/' {
                    pattern = pattern.replace('/', "\\");
                }
                let (got_type, re) = compile(&pattern, sl).unwrap();
                assert_eq!(got_type, want_type, "pattern {pattern:?}");
                if want_type == MatchType::Regexp {
                    let want = if sl == '/' { want_re } else { want_win_re };
                    assert_eq!(re.unwrap().as_str(), want, "pattern {pattern:?}");
                }
            }
        }
    }

    // A pattern that passes the glob syntax check but whose regular expression does
    // not compile fails at every evaluation, not only the first.
    #[test]
    fn malformed_pattern_fails_on_repeated_calls() {
        let pm = PatternMatcher::new(&["[Local-Only]/"]).unwrap();
        assert_eq!(
            pm.matches_or_parent_matches("x"),
            Err(PatternError::BadPattern)
        );
        assert_eq!(
            pm.matches_or_parent_matches("x"),
            Err(PatternError::BadPattern)
        );
    }

    #[test]
    fn escapes_read_as_the_reference_reads_them() {
        // \d is an ASCII digit for the reference's engine, \< a plain `<`.
        assert!(matches_or_parent_matches("a1", &["a\\d"]).unwrap());
        assert!(!matches_or_parent_matches("a\u{663}", &["a\\d"]).unwrap());
        assert!(matches_or_parent_matches("a<b", &["a\\<b"]).unwrap());
        // `&&` and `[` inside a class are plain characters.
        assert!(matches_or_parent_matches("a&", &["a[&&b]"]).unwrap());
        assert!(matches_or_parent_matches("a[", &["a[[x]"]).unwrap());
        assert!(matches_or_parent_matches("ab]", &["ab]"]).unwrap());
    }

    #[test]
    fn regexp_reads_the_reference_syntax() {
        let m = |re: &str, text: &str| Regexp::new(re).unwrap().is_match(text);
        assert!(m("^[[:alpha:]]$", "x"));
        assert!(!m("^[[:alpha:]]$", "1"));
        assert!(m("^[[:^digit:]]$", "x"));
        assert!(m("^a\\Q.*\\E$", "a.*"));
        assert!(m("^\\Qab$", "ab$"));
        assert!(m("^a\\x41$", "aA"));
        assert!(m("^a\\012$", "a\n"));
        assert!(m("^a\\bb$", "a b") == m("^a\\bb$", "ab"));
        assert!(m("^.$", "\u{263a}"));
        assert!(!m("^.$", "\n"));
        assert!(m("^[^/]$", "\n"));
        assert!(m("^a(b)?c$", "ac"));
        assert!(m("^a+$", "aaa"));
        assert!(m("x^", "") == m("x^", "x"));
        for bad in [
            "[a",
            "a**",
            "(a",
            "a)",
            "[b-a]",
            "\\pL",
            "[[:nope:]]",
            "\\1",
            "\\e",
        ] {
            assert!(Regexp::new(bad).is_err(), "{bad:?}");
        }
        // The search takes linear time where a backtracking matcher would not end.
        let re = Regexp::new(&format!("^{}b$", "[^/]*a".repeat(30))).unwrap();
        assert!(!re.is_match(&"a".repeat(5000)));
    }

    #[test]
    fn posix_classes_in_patterns() {
        assert!(matches_or_parent_matches("x", &["[[:alpha:]]"]).unwrap());
        assert!(!matches_or_parent_matches("1", &["[[:alpha:]]"]).unwrap());
    }

    #[test]
    fn non_utf8_names_match_as_replacement_characters() {
        let name = lossy(b"a\xffb");
        assert_eq!(name, "a\u{fffd}b");
        assert!(matches_or_parent_matches(&name, &["a?b"]).unwrap());
        assert_eq!(lossy(b"\xe2\x82A"), "\u{fffd}\u{fffd}A");
    }

    #[test]
    fn read_all_ignore_file() {
        assert!(read_all(&b""[..]).unwrap().is_empty());
        let content = "test1\n/test2\n/a/file/here\n\nlastfile\n# this is a comment\n! /inverted/abs/path\n!\n! ";
        let expected = [
            "test1",
            "test2",
            "a/file/here",
            "lastfile",
            "!inverted/abs/path",
            "!",
            "!",
        ];
        assert_eq!(read_all(content.as_bytes()).unwrap(), expected);
    }

    #[test]
    fn read_all_bom_and_crlf() {
        let content = b"\xef\xbb\xbffoo\r\n  bar/../baz  \r\n#x\n\xef\xbb\xbfqux\n";
        assert_eq!(
            read_all(&content[..]).unwrap(),
            ["foo", "baz", "\u{feff}qux"]
        );
    }

    #[test]
    fn lexical_clean() {
        let cases = [
            ("", "."),
            ("abc", "abc"),
            ("abc/def", "abc/def"),
            ("a/b/c", "a/b/c"),
            (".", "."),
            ("..", ".."),
            ("../..", "../.."),
            ("../../abc", "../../abc"),
            ("/abc", "/abc"),
            ("/", "/"),
            ("abc/", "abc"),
            ("abc/def/", "abc/def"),
            ("a/b/c/", "a/b/c"),
            ("./", "."),
            ("../", ".."),
            ("../../", "../.."),
            ("/abc/", "/abc"),
            ("abc//def//ghi", "abc/def/ghi"),
            ("//abc", "/abc"),
            ("///abc", "/abc"),
            ("//abc//", "/abc"),
            ("abc//", "abc"),
            ("abc/./def", "abc/def"),
            ("/./abc/def", "/abc/def"),
            ("abc/.", "abc"),
            ("abc/def/ghi/../jkl", "abc/def/jkl"),
            ("abc/def/../ghi/../jkl", "abc/jkl"),
            ("abc/def/..", "abc"),
            ("abc/def/../..", "."),
            ("/abc/def/../..", "/"),
            ("abc/def/../../..", ".."),
            ("/abc/def/../../..", "/"),
            ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
            ("/../abc", "/abc"),
            ("abc/./../def", "def"),
            ("abc//./../def", "def"),
            ("abc/../../././../def", "../../def"),
        ];
        for (input, want) in cases {
            assert_eq!(lexical::clean_str(input), want, "clean({input:?})");
        }
    }

    #[test]
    fn lexical_split_dir_base_rel() {
        assert_eq!(lexical::split(b"a/b/c"), (&b"a/b/"[..], &b"c"[..]));
        assert_eq!(lexical::split(b"c"), (&b""[..], &b"c"[..]));
        assert_eq!(lexical::split(b"/a/"), (&b"/a/"[..], &b""[..]));
        assert_eq!(lexical::dir(b"/a/b"), b"/a");
        assert_eq!(lexical::dir(b"file"), b".");
        assert_eq!(lexical::dir(b"/"), b"/");
        assert_eq!(lexical::base(b"/a/b/"), b"b");
        assert_eq!(lexical::base(b"///"), b"/");
        assert_eq!(lexical::base(b""), b".");
        assert_eq!(lexical::join_str(&["", "a", "", "b/../c"]), "a/c");
        assert_eq!(lexical::join_str(&["", ""]), "");
        assert_eq!(lexical::rel(b"/a/b", b"/a/b/c/d").unwrap(), b"c/d");
        assert_eq!(lexical::rel(b"/a/b", b"/a/b").unwrap(), b".");
        assert_eq!(lexical::rel(b"/a/b", b"/a/x").unwrap(), b"../x");
        assert!(lexical::rel(b"a", b"/a").is_none());
    }
}
