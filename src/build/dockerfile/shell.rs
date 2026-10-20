//! Shell-like word processing (shell/lex.go): quotes, the escape token and
//! `$VAR` / `${VAR...}` expansion over an environment, with the modifiers
//! `:-` `-` `:+` `+` `:?` `?` `#` `##` `%` `%%` `/` and `//`.

use std::collections::{BTreeSet, HashMap};

use super::compat::quote;
use super::unicode::{is_digit, is_letter};
use super::Error;

/// Variable lookup used during expansion (EnvGetter in lex.go).
pub trait EnvGetter {
    fn get(&self, key: &str) -> Option<&str>;
    fn keys(&self) -> Vec<String>;
}

/// An environment built from `KEY=VALUE` entries where the last entry of a
/// key wins; an entry without `=` sets the key to the empty string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvList {
    env: HashMap<String, String>,
    keys: Vec<String>,
}

/// EnvsFromSlice in lex.go.
pub fn envs_from_slice<S: AsRef<str>>(env: &[S]) -> EnvList {
    let mut list = EnvList::default();
    for e in env {
        let e = e.as_ref();
        let (k, v) = e.split_once('=').unwrap_or((e, ""));
        list.keys.push(k.to_string());
        list.env
            .insert(normalize_env_key(k).to_string(), v.to_string());
    }
    list
}

impl EnvGetter for EnvList {
    fn get(&self, key: &str) -> Option<&str> {
        self.env.get(normalize_env_key(key)).map(String::as_str)
    }

    /// Every key in the order given, repeated keys included.
    fn keys(&self) -> Vec<String> {
        self.keys.clone()
    }
}

/// Environment keys are case-sensitive on Unix.
pub fn equal_env_keys(from: &str, to: &str) -> bool {
    from == to
}

pub fn normalize_env_key(key: &str) -> &str {
    key
}

/// Word splitting and variable expansion with a given escape token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lex {
    escape_token: char,
    /// Keep the quote characters in the result.
    pub raw_quotes: bool,
    /// Keep the escape token in the result.
    pub raw_escapes: bool,
    /// Treat quotes as ordinary characters.
    pub skip_process_quotes: bool,
    /// Leave references to unset variables as they are written.
    pub skip_unset_env: bool,
}

/// What ProcessWordWithMatches returns: the expanded word, the words it
/// splits into, and the variables that were found and not found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessWordResult {
    pub result: String,
    pub words: Vec<String>,
    pub matched: BTreeSet<String>,
    pub unmatched: BTreeSet<String>,
}

impl Lex {
    pub fn new(escape_token: char) -> Lex {
        Lex {
            escape_token,
            raw_quotes: false,
            raw_escapes: false,
            skip_process_quotes: false,
            skip_unset_env: false,
        }
    }

    pub fn escape_token(&self) -> char {
        self.escape_token
    }

    /// Expands `word`, keeping the spaces between words, and returns the
    /// variables it referenced that `env` does not have.
    pub fn process_word(
        &self,
        word: &str,
        env: &dyn EnvGetter,
    ) -> Result<(String, BTreeSet<String>), Error> {
        let r = self.process(word, env, true)?;
        Ok((r.result, r.unmatched))
    }

    /// Expands `word` and splits the result into words. Splitting happens
    /// after expansion, and quoted spaces do not split.
    pub fn process_words(&self, word: &str, env: &dyn EnvGetter) -> Result<Vec<String>, Error> {
        Ok(self.process(word, env, false)?.words)
    }

    pub fn process_word_with_matches(
        &self,
        word: &str,
        env: &dyn EnvGetter,
    ) -> Result<ProcessWordResult, Error> {
        self.process(word, env, true)
    }

    fn process(
        &self,
        word: &str,
        env: &dyn EnvGetter,
        capture: bool,
    ) -> Result<ProcessWordResult, Error> {
        let mut sw = ShellWord::new(self, word, env, capture);
        match sw.process_stop_on(None, self.raw_escapes) {
            Ok((result, words)) => Ok(ProcessWordResult {
                result,
                words,
                matched: sw.matches,
                unmatched: sw.nonmatches,
            }),
            Err(e) => Err(e.wrap(&format!("failed to process {}", quote(word)))),
        }
    }
}

/// The text of a char read by the reference's scanner, where the end of the
/// input reads as U+FFFD.
fn rune_string(c: Option<char>) -> String {
    c.unwrap_or('\u{fffd}').to_string()
}

fn is_special_param(c: char) -> bool {
    matches!(c, '@' | '*' | '#' | '?' | '-' | '$' | '!' | '0')
}

#[derive(Default)]
struct Words {
    words: Vec<String>,
    in_word: bool,
}

// The buffer is passed in because the reference shares one buffer between a
// word and the ${VAR<modifier>...} words processed inside it, and the nested
// call clears it: the split words lose any text that comes before such an
// expansion in the same word. That is kept to split words as the reference
// does.
impl Words {
    fn add_char(&mut self, buf: &mut String, ch: char) {
        if ch.is_whitespace() && self.in_word {
            if !buf.is_empty() {
                self.words.push(std::mem::take(buf));
                self.in_word = false;
            }
        } else if !ch.is_whitespace() {
            self.add_raw_char(buf, ch);
        }
    }

    fn add_raw_char(&mut self, buf: &mut String, ch: char) {
        buf.push(ch);
        self.in_word = true;
    }

    fn add_string(&mut self, buf: &mut String, s: &str) {
        for ch in s.chars() {
            self.add_char(buf, ch);
        }
    }

    fn add_raw_string(&mut self, buf: &mut String, s: &str) {
        buf.push_str(s);
        self.in_word = true;
    }

    fn get_words(mut self, buf: &mut String) -> Vec<String> {
        if !buf.is_empty() {
            self.words.push(std::mem::take(buf));
        }
        self.words
    }
}

struct ShellWord<'a> {
    lex: &'a Lex,
    chars: Vec<char>,
    pos: usize,
    envs: &'a dyn EnvGetter,
    raw_escapes: bool,
    capture: bool,
    matches: BTreeSet<String>,
    nonmatches: BTreeSet<String>,
    words_buffer: String,
    /// How many substitutions and quotes the position is inside.
    depth: usize,
}

/// How deep substitutions and quotes may nest: each level is a level of the stack,
/// which the reference grows as far as a goroutine's and nspawn's service cannot.
const MAX_NESTING: usize = 10_000;

impl<'a> ShellWord<'a> {
    fn new(lex: &'a Lex, word: &str, envs: &'a dyn EnvGetter, capture: bool) -> ShellWord<'a> {
        let chars: Vec<char> = word.chars().collect();
        // The reference's text scanner drops a byte order mark at the start.
        let pos = usize::from(chars.first() == Some(&'\u{feff}'));
        ShellWord {
            lex,
            chars,
            pos,
            envs,
            raw_escapes: lex.raw_escapes,
            capture,
            matches: BTreeSet::new(),
            nonmatches: BTreeSet::new(),
            words_buffer: String::new(),
            depth: 0,
        }
    }

    /// Runs `f` a level deeper, refused past the limit.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.depth >= MAX_NESTING {
            return Err(Error::new(format!(
                "substitutions and quotes nest deeper than {MAX_NESTING} levels"
            )));
        }
        self.depth += 1;
        let out = f(self);
        self.depth -= 1;
        out
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// Processes up to `stop` (consumed) or to the end of the input when
    /// `stop` is `None`.
    fn process_stop_on(
        &mut self,
        stop: Option<char>,
        raw_escapes: bool,
    ) -> Result<(String, Vec<String>), Error> {
        let saved = self.raw_escapes;
        self.raw_escapes = raw_escapes;
        let out = self.nested(|w| w.process_stop_on_inner(stop));
        self.raw_escapes = saved;
        out
    }

    fn process_stop_on_inner(
        &mut self,
        stop: Option<char>,
    ) -> Result<(String, Vec<String>), Error> {
        let mut result = String::new();
        self.words_buffer.clear();
        let mut words = Words::default();
        let escape = self.lex.escape_token;

        while let Some(ch) = self.peek() {
            if stop == Some(ch) {
                self.next();
                return Ok((result, words.get_words(&mut self.words_buffer)));
            }
            let quotes = !self.lex.skip_process_quotes;
            let tmp = match ch {
                '$' => Some(self.process_dollar()?),
                '<' => Some(self.process_possible_heredoc()),
                '\'' if quotes => Some(self.process_single_quote()?),
                '"' if quotes => Some(self.process_double_quote()?),
                _ => None,
            };
            if let Some(tmp) = tmp {
                result.push_str(&tmp);
                if ch == '$' {
                    words.add_string(&mut self.words_buffer, &tmp);
                } else {
                    words.add_raw_string(&mut self.words_buffer, &tmp);
                }
                continue;
            }
            let Some(mut ch) = self.next() else { break };
            if ch == escape {
                if self.raw_escapes {
                    words.add_raw_char(&mut self.words_buffer, ch);
                    result.push(ch);
                }
                match self.next() {
                    None => break,
                    Some(c) => {
                        ch = c;
                        words.add_raw_char(&mut self.words_buffer, ch);
                    }
                }
            } else {
                words.add_char(&mut self.words_buffer, ch);
            }
            result.push(ch);
        }
        if let Some(stop) = stop {
            return Err(Error::new(format!(
                "unexpected end of statement while looking for matching {stop}"
            )));
        }
        Ok((result, words.get_words(&mut self.words_buffer)))
    }

    fn process_single_quote(&mut self) -> Result<String, Error> {
        // Everything up to the next single quote is literal, the escape
        // token included.
        let mut result = String::new();
        let open = self.next();
        if self.lex.raw_quotes {
            result.push_str(&rune_string(open));
        }
        loop {
            match self.next() {
                None => {
                    return Err(Error::new(
                        "unexpected end of statement while looking for matching single-quote",
                    ))
                }
                Some('\'') => {
                    if self.lex.raw_quotes {
                        result.push('\'');
                    }
                    return Ok(result);
                }
                Some(c) => result.push(c),
            }
        }
    }

    fn process_double_quote(&mut self) -> Result<String, Error> {
        self.nested(Self::process_double_quote_here)
    }

    fn process_double_quote_here(&mut self) -> Result<String, Error> {
        // Variables expand; the escape token only escapes ", $ and itself.
        let mut result = String::new();
        let escape = self.lex.escape_token;
        let open = self.next();
        if self.lex.raw_quotes {
            result.push_str(&rune_string(open));
        }
        loop {
            match self.peek() {
                None => {
                    return Err(Error::new(
                        "unexpected end of statement while looking for matching double-quote",
                    ))
                }
                Some('"') => {
                    self.next();
                    if self.lex.raw_quotes {
                        result.push('"');
                    }
                    return Ok(result);
                }
                Some('$') => {
                    let value = self.process_dollar()?;
                    result.push_str(&value);
                }
                Some(_) => {
                    let Some(mut ch) = self.next() else { continue };
                    if ch == escape {
                        if self.raw_escapes {
                            result.push(ch);
                        }
                        match self.peek() {
                            None => continue,
                            Some(c) if c == '"' || c == '$' || c == escape => {
                                self.next();
                                ch = c;
                            }
                            Some(_) => {}
                        }
                    }
                    result.push(ch);
                }
            }
        }
    }

    fn process_dollar(&mut self) -> Result<String, Error> {
        self.next();

        if self.peek() != Some('{') {
            let name = self.process_name();
            if name.is_empty() {
                return Ok("$".to_string());
            }
            let (value, found) = self.get_env(&name);
            if !found && self.lex.skip_unset_env {
                return Ok(format!("${name}"));
            }
            return Ok(value);
        }

        self.next();
        match self.peek() {
            None => return Err(Error::new("syntax error: missing '}'")),
            Some('{' | '}' | ':') => return Err(Error::new("syntax error: bad substitution")),
            Some(_) => {}
        }
        let name = self.process_name();
        let mut ch = self.next();
        let mut chs = rune_string(ch);
        let unsupported =
            |chs: &str| Error::new(format!("unsupported modifier ({chs}) in substitution"));

        match ch {
            Some('}') => {
                let (value, set) = self.get_env(&name);
                if !set && self.lex.skip_unset_env {
                    return Ok(format!("${{{name}}}"));
                }
                Ok(value)
            }
            Some(':' | '+' | '-' | '?' | '#' | '%') => {
                let mut null_is_unset = false;
                if ch == Some(':') {
                    null_is_unset = true;
                    ch = self.next();
                    chs.push_str(&rune_string(ch));
                }
                let raw_escapes = matches!(ch, Some('#' | '%'));
                if null_is_unset && raw_escapes {
                    return Err(unsupported(&chs));
                }
                let mut word = match self.process_stop_on(Some('}'), raw_escapes) {
                    Ok((word, _)) => word,
                    Err(e) => {
                        if self.peek().is_none() {
                            return Err(Error::new("syntax error: missing '}'"));
                        }
                        return Err(e);
                    }
                };

                let (value, set) = self.get_env(&name);
                if self.lex.skip_unset_env && !set {
                    return Ok(format!("${{{name}{chs}{word}}}"));
                }

                match ch {
                    Some('-') => {
                        if !set || (null_is_unset && value.is_empty()) {
                            return Ok(word);
                        }
                        Ok(value)
                    }
                    Some('+') => {
                        if !set || (null_is_unset && value.is_empty()) {
                            return Ok(String::new());
                        }
                        Ok(word)
                    }
                    Some('?') => {
                        if !set {
                            let message = if word.is_empty() {
                                "is not allowed to be unset"
                            } else {
                                word.as_str()
                            };
                            return Err(Error::new(format!("{name}: {message}")));
                        }
                        if null_is_unset && value.is_empty() {
                            let message = if word.is_empty() {
                                "is not allowed to be empty"
                            } else {
                                word.as_str()
                            };
                            return Err(Error::new(format!("{name}: {message}")));
                        }
                        Ok(value)
                    }
                    Some(c @ ('%' | '#')) => {
                        // A single % or # removes the shortest match, a
                        // doubled one the longest.
                        let mut greedy = false;
                        if word.starts_with(c) {
                            greedy = true;
                            word.remove(0);
                        }
                        if c == '%' {
                            trim_suffix(&word, &value, greedy)
                        } else {
                            trim_prefix(&word, &value, greedy)
                        }
                    }
                    _ => Err(unsupported(&chs)),
                }
            }
            Some('/') => {
                let replace_all = self.peek() == Some('/');
                if replace_all {
                    self.next();
                }
                let pattern = match self.process_stop_on(Some('/'), true) {
                    Ok((p, _)) => p,
                    Err(e) => {
                        if self.peek().is_none() {
                            return Err(Error::new("syntax error: missing '/' in ${}"));
                        }
                        return Err(e);
                    }
                };
                let replacement = match self.process_stop_on(Some('}'), true) {
                    Ok((r, _)) => r,
                    Err(e) => {
                        if self.peek().is_none() {
                            return Err(Error::new("syntax error: missing '}'"));
                        }
                        return Err(e);
                    }
                };

                let (value, set) = self.get_env(&name);
                if self.lex.skip_unset_env && !set {
                    return Ok(format!("${{{name}/{pattern}/{replacement}}}"));
                }

                let re = ShellPattern::new(&pattern, true, false).map_err(|e| {
                    Error::new(format!("invalid pattern ({pattern}) in substitution: {e}"))
                })?;
                let text: Vec<char> = value.chars().collect();
                if replace_all {
                    return Ok(re.replace_all(&text, &replacement));
                }
                let ends = re.first_ends(&text);
                match re.find(&ends, 0) {
                    Some((start, end)) => {
                        let mut out: String = text[..start].iter().collect();
                        out.push_str(&replacement);
                        out.extend(&text[end..]);
                        Ok(out)
                    }
                    None => Ok(value),
                }
            }
            _ => Err(unsupported(&chs)),
        }
    }

    /// A name made of letters, digits and underscores; a name starting with
    /// a digit takes only digits, and a special parameter is one char.
    fn process_name(&mut self) -> String {
        let mut name = String::new();
        while let Some(ch) = self.peek() {
            if name.is_empty() && is_digit(ch) {
                while let Some(c) = self.peek() {
                    if !is_digit(c) {
                        break;
                    }
                    self.next();
                    name.push(c);
                }
                return name;
            }
            if name.is_empty() && is_special_param(ch) {
                self.next();
                return ch.to_string();
            }
            if !is_letter(ch) && !is_digit(ch) && ch != '_' {
                break;
            }
            self.next();
            name.push(ch);
        }
        name
    }

    fn process_possible_heredoc(&mut self) -> String {
        self.next();
        if self.peek() != Some('<') {
            return "<".to_string();
        }
        self.next();
        // Whitespace between << and the word stays part of the word.
        let mut result = String::from("<<");
        while let Some(c) = self.peek() {
            if !matches!(c, '\t' | '\r' | ' ') {
                break;
            }
            result.push(c);
            self.next();
        }
        result
    }

    fn get_env(&mut self, name: &str) -> (String, bool) {
        match self.envs.get(name) {
            Some(v) => {
                if self.capture {
                    self.matches.insert(name.to_string());
                }
                (v.to_string(), true)
            }
            None => {
                if self.capture {
                    self.nonmatches.insert(name.to_string());
                }
                (String::new(), false)
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternToken {
    Literal(char),
    /// `.`: any char but a newline
    AnyChar,
    /// `.*` or `.*?`
    Star,
}

/// A shell wildcard pattern (`?` one char, `*` any run) compiled to the
/// regular expression the reference builds (convertShellPatternToRegex) and
/// matched with the same leftmost-first semantics.
#[derive(Debug, Clone)]
struct ShellPattern {
    source: String,
    tokens: Vec<PatternToken>,
    greedy: bool,
    anchored: bool,
}

impl ShellPattern {
    fn new(pattern: &str, greedy: bool, anchored: bool) -> Result<ShellPattern, String> {
        let chars: Vec<char> = pattern.chars().collect();
        let mut i = usize::from(chars.first() == Some(&'\u{feff}'));
        let mut source = String::with_capacity(pattern.len() + 4);
        let mut tokens = Vec::new();
        if anchored {
            source.push('^');
        }
        let star = if greedy { ".*" } else { ".*?" };
        while i < chars.len() {
            let tok = chars[i];
            i += 1;
            match tok {
                '*' => {
                    source.push_str(star);
                    // Consecutive stars match exactly what one star does.
                    if tokens.last() != Some(&PatternToken::Star) {
                        tokens.push(PatternToken::Star);
                    }
                    continue;
                }
                '?' => {
                    source.push('.');
                    tokens.push(PatternToken::AnyChar);
                    continue;
                }
                '\\' => {
                    // An escaped } or / belongs to the ${} syntax, not to
                    // the pattern.
                    if matches!(chars.get(i), Some('}' | '/')) {
                        continue;
                    }
                    source.push('\\');
                    let next = chars.get(i).copied();
                    if next.is_some() {
                        i += 1;
                    }
                    match next {
                        Some(c @ ('*' | '?' | '\\')) => {
                            source.push(c);
                            tokens.push(PatternToken::Literal(c));
                        }
                        other => return Err(format!("invalid escape '\\{}'", rune_string(other))),
                    }
                    continue;
                }
                '[' | ']' | '{' | '}' | '.' | '+' | '(' | ')' | '|' | '^' | '$' => {
                    source.push('\\')
                }
                _ => {}
            }
            source.push(tok);
            tokens.push(PatternToken::Literal(tok));
        }
        Ok(ShellPattern {
            source,
            tokens,
            greedy,
            anchored,
        })
    }

    /// For every start position, the end of the match the reference's
    /// regexp would report there, computed token by token from the end of
    /// the pattern so that nothing recurses.
    fn first_ends(&self, text: &[char]) -> Vec<Option<usize>> {
        let n = text.len();
        let mut row: Vec<Option<usize>> = (0..=n).map(Some).collect();
        // Two rows for the whole pattern: one per token would be a value's worth of
        // memory per character of the pattern.
        let mut cur: Vec<Option<usize>> = vec![None; n + 1];
        for tok in self.tokens.iter().rev() {
            cur.fill(None);
            match tok {
                PatternToken::Literal(c) => {
                    for t in 0..n {
                        if text[t] == *c {
                            cur[t] = row[t + 1];
                        }
                    }
                }
                PatternToken::AnyChar => {
                    for t in 0..n {
                        if text[t] != '\n' {
                            cur[t] = row[t + 1];
                        }
                    }
                }
                PatternToken::Star => {
                    // From t a star can stop anywhere up to the next newline.
                    // Greedy takes the furthest stop that lets the rest
                    // match, lazy the nearest.
                    let mut cand: Option<usize> = None;
                    for t in (0..=n).rev() {
                        if t == n || text[t] == '\n' {
                            cand = None;
                        }
                        if row[t].is_some() && (!self.greedy || cand.is_none()) {
                            cand = row[t];
                        }
                        cur[t] = cand;
                    }
                }
            }
            std::mem::swap(&mut row, &mut cur);
            // Matching nowhere, the tokens before cannot match either.
            if row.iter().all(Option::is_none) {
                break;
            }
        }
        row
    }

    /// The end of the match at the start of `text`: the tokens before the first star,
    /// each one char, compared as they are, and only the rest matched against every
    /// position of what follows them.
    fn end_at_start(&self, text: &[char]) -> Option<usize> {
        let fixed = self
            .tokens
            .iter()
            .position(|t| *t == PatternToken::Star)
            .unwrap_or(self.tokens.len());
        if text.len() < fixed {
            return None;
        }
        let same = self.tokens[..fixed]
            .iter()
            .zip(text)
            .all(|(tok, c)| match tok {
                PatternToken::Literal(l) => c == l,
                PatternToken::AnyChar => *c != '\n',
                PatternToken::Star => false,
            });
        if !same {
            return None;
        }
        if fixed == self.tokens.len() {
            return Some(fixed);
        }
        let rest = ShellPattern {
            source: String::new(),
            tokens: self.tokens[fixed..].to_vec(),
            greedy: self.greedy,
            anchored: self.anchored,
        };
        rest.first_ends(&text[fixed..])[0].map(|end| fixed + end)
    }

    /// The leftmost match starting at or after `from`.
    fn find(&self, ends: &[Option<usize>], from: usize) -> Option<(usize, usize)> {
        if self.anchored {
            return if from == 0 {
                ends[0].map(|e| (0, e))
            } else {
                None
            };
        }
        (from..ends.len()).find_map(|t| ends[t].map(|e| (t, e)))
    }

    /// Regexp.ReplaceAllString, with `$0` / `${0}` standing for the match in
    /// `template` and other group references expanding to nothing.
    fn replace_all(&self, text: &[char], template: &str) -> String {
        let ends = self.first_ends(text);
        let n = text.len();
        let mut out = String::new();
        let mut last_end = 0;
        let mut search = 0;
        while search <= n {
            let Some((start, end)) = self.find(&ends, search) else {
                break;
            };
            out.extend(&text[last_end..start]);
            // An empty match right after a previous match is not replaced.
            if end > last_end || start == 0 {
                expand_template(&mut out, template, &text[start..end]);
            }
            last_end = end;
            let width = usize::from(search < n);
            if search + width > end {
                search += width;
            } else if search + 1 > end {
                search += 1;
            } else {
                search = end;
            }
        }
        out.extend(&text[last_end..]);
        out
    }
}

fn expand_template(out: &mut String, template: &str, matched: &[char]) {
    let mut t = template;
    while let Some(idx) = t.find('$') {
        out.push_str(&t[..idx]);
        t = &t[idx + 1..];
        if let Some(rest) = t.strip_prefix('$') {
            out.push('$');
            t = rest;
            continue;
        }
        match template_name(t) {
            None => out.push('$'),
            Some((num, rest)) => {
                t = rest;
                if num == Some(0) {
                    out.extend(matched);
                }
            }
        }
    }
    out.push_str(t);
}

/// A leading `name` or `{name}` after a `$`: its group number when it is
/// one, and the rest of the template.
fn template_name(s: &str) -> Option<(Option<u64>, &str)> {
    let (brace, body) = match s.strip_prefix('{') {
        Some(b) => (true, b),
        None => (false, s),
    };
    let end = body
        .char_indices()
        .find(|&(_, c)| !(is_letter(c) || is_digit(c) || c == '_'))
        .map_or(body.len(), |(i, _)| i);
    if end == 0 {
        return None;
    }
    let name = &body[..end];
    let mut rest = &body[end..];
    if brace {
        rest = rest.strip_prefix('}')?;
    }
    let mut num: Option<u64> = Some(0);
    for b in name.bytes() {
        match num {
            Some(v) if b.is_ascii_digit() && v < 100_000_000 => {
                num = Some(v * 10 + u64::from(b - b'0'));
            }
            _ => {
                num = None;
                break;
            }
        }
    }
    if name.starts_with('0') && name.len() > 1 {
        num = None;
    }
    Some((num, rest))
}

fn trim_prefix(word: &str, value: &str, greedy: bool) -> Result<String, Error> {
    let re = ShellPattern::new(word, greedy, true)
        .map_err(|e| Error::new(format!("invalid pattern ({word}) in substitution: {e}")))?;
    let text: Vec<char> = value.chars().collect();
    Ok(match re.end_at_start(&text) {
        Some(end) => text[end..].iter().collect(),
        None => value.to_string(),
    })
}

/// Reverses a pattern keeping each escape in front of the char it escapes.
fn reverse_pattern(pattern: &str) -> String {
    let runes: Vec<char> = pattern.chars().collect();
    if runes.is_empty() {
        return String::new();
    }
    let last = runes.len() - 1;
    let mut out = vec!['\0'; runes.len()];
    let mut i = 0;
    while i <= last {
        let tok = runes[i];
        let out_idx = last - i;
        if tok == '\\' && i != last {
            out[out_idx - 1] = tok;
            out[out_idx] = runes[i + 1];
            i += 2;
        } else {
            out[out_idx] = tok;
            i += 1;
        }
    }
    out.into_iter().collect()
}

fn reverse_string(s: &str) -> String {
    s.chars().rev().collect()
}

fn trim_suffix(pattern: &str, word: &str, greedy: bool) -> Result<String, Error> {
    // The shortest match at the end is the shortest match at the start of
    // the reversed value with the reversed pattern.
    let pattern = reverse_pattern(pattern);
    let word = reverse_string(word);
    Ok(reverse_string(&trim_prefix(&pattern, &word, greedy)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envs_from_map(m: &[(&str, &str)]) -> EnvList {
        let mut list = EnvList::default();
        for (k, v) in m {
            list.keys.push((*k).to_string());
            list.env.insert((*k).to_string(), (*v).to_string());
        }
        list
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn nesting_past_the_limit_is_an_error_not_an_overflow() {
        let lex = Lex::new('\\');
        let env = envs_from_map(&[]);
        let nest = |n: usize| format!("{}x{}", "${a:-".repeat(n), "}".repeat(n));
        assert_eq!(lex.process_word(&nest(100), &env).unwrap().0, "x");
        // On the stack the service gives it, as the build does.
        crate::build::deep(|| {
            assert_eq!(
                lex.process_word(&nest(MAX_NESTING - 1), &env).unwrap().0,
                "x"
            );
            let err = lex.process_word(&nest(MAX_NESTING + 1), &env).unwrap_err();
            assert!(err.to_string().contains("nest deeper"), "{err}");
            let quoted = format!(
                "{}x{}",
                "\"${a:-".repeat(MAX_NESTING),
                "}\"".repeat(MAX_NESTING)
            );
            assert!(lex.process_word(&quoted, &env).is_err());
        });
    }

    #[test]
    fn convert_shell_pattern_to_regex() {
        let cases = [
            ("*", "^.*"),
            ("?", "^."),
            ("\\*", "^\\*"),
            (
                "(()[]{\\}^$.\\*\\?|\\\\",
                "^\\(\\(\\)\\[\\]\\{\\}\\^\\$\\.\\*\\?\\|\\\\",
            ),
        ];
        for (pattern, expected) in cases {
            let res = ShellPattern::new(pattern, true, true).unwrap();
            assert_eq!(res.source, expected);
        }
        for pattern in ["\\", "\\x", "\\\\\\"] {
            assert!(ShellPattern::new(pattern, true, true).is_err(), "{pattern}");
        }
        assert_eq!(
            ShellPattern::new("\\x", true, true).unwrap_err(),
            "invalid escape '\\x'"
        );
        assert_eq!(
            ShellPattern::new("\\", true, true).unwrap_err(),
            "invalid escape '\\\u{fffd}'"
        );
    }

    #[test]
    fn reverse_string_runes() {
        assert_eq!(reverse_string("54321"), "12345");
        assert_eq!(
            reverse_string("\u{1f596}\u{1f680}\u{1f47d}"),
            "\u{1f47d}\u{1f680}\u{1f596}"
        );
    }

    #[test]
    fn reverse_pattern_cases() {
        let cases = [
            ("a\\*c", "c\\*a"),
            ("\\\\\\ab", "b\\a\\\\"),
            ("ab\\", "\\ba"),
            (
                "\u{1f47d}\\\u{1f680}\u{1f596}",
                "\u{1f596}\\\u{1f680}\u{1f47d}",
            ),
            ("\\\\b", "b\\\\"),
        ];
        for (pattern, expected) in cases {
            assert_eq!(reverse_pattern(pattern), expected, "{pattern}");
        }
    }

    #[test]
    fn mandatory_env_vars() {
        let shlex = Lex::new('\\');
        let set_envs = envs_from_slice(&["VAR=plain", "ARG=x"]);
        let empty_envs = envs_from_slice(&["VAR=", "ARG=x"]);
        let unset_envs = envs_from_slice(&["ARG=x"]);

        let no_empty = "${VAR:?message here$ARG}";
        let no_unset = "${VAR?message here$ARG}";

        assert_eq!(shlex.process_word(no_empty, &set_envs).unwrap().0, "plain");
        let err = shlex.process_word(no_empty, &empty_envs).unwrap_err();
        assert!(err.to_string().contains("message herex"), "{err}");
        let err = shlex.process_word(no_empty, &unset_envs).unwrap_err();
        assert!(err.to_string().contains("message herex"), "{err}");

        assert_eq!(shlex.process_word(no_unset, &set_envs).unwrap().0, "plain");
        assert_eq!(shlex.process_word(no_unset, &empty_envs).unwrap().0, "");
        let err = shlex.process_word(no_unset, &unset_envs).unwrap_err();
        assert!(err.to_string().contains("message herex"), "{err}");
        assert_eq!(
            err.to_string(),
            "failed to process \"${VAR?message here$ARG}\": VAR: message herex"
        );
    }

    #[test]
    fn env_var_test_vectors() {
        let path = crate::build::dockerfile::testdata("shell/envVarTest");
        let content = std::fs::read_to_string(&path).unwrap();
        let shlex = Lex::new('\\');
        let envs = envs_from_slice(&[
            "PWD=/home",
            "SHELL=bash",
            "KOREAN=\u{d55c}\u{ad6d}\u{c5b4}",
            "NULL=",
        ]);
        let mut checked = 0;
        for (n, line) in content.lines().enumerate() {
            let line_count = n + 1;
            if line.starts_with('#') {
                continue;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let words: Vec<&str> = line.split('|').collect();
            assert_eq!(words.len(), 3, "line {line_count}");
            let platform = words[0].trim();
            let source = words[1].trim();
            let expected = words[2].trim();
            assert!(
                matches!(platform, "W" | "A" | "U"),
                "invalid tag {platform} at line {line_count}"
            );
            if platform == "W" {
                continue;
            }
            let res = shlex.process_word(source, &envs);
            if expected == "error" {
                assert!(res.is_err(), "input: {source:?}, result: {res:?}");
            } else {
                let (word, _) = res.unwrap_or_else(|e| panic!("at line {line_count}: {e}"));
                assert_eq!(word, expected, "at line {line_count}");
            }
            checked += 1;
        }
        assert!(checked > 200);
    }

    // The reference runs the file a second time with raw quotes and
    // skip-unset-env, but reuses the exhausted reader, so that pass checks
    // nothing; only the first pass is ported.
    #[test]
    fn words_test_vectors() {
        let path = crate::build::dockerfile::testdata("shell/wordsTest");
        let content = std::fs::read_to_string(&path).unwrap();
        let shlex = Lex::new('\\');
        let mut envs: Vec<String> = Vec::new();
        let mut checked = 0;
        for (n, line) in content.lines().enumerate() {
            let line_num = n + 1;
            if line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("ENV ") {
                envs.push(rest.trim_start_matches(' ').to_string());
                continue;
            }
            let words: Vec<&str> = line.split('|').collect();
            assert_eq!(words.len(), 2, "line {line_num}: {line:?}");
            let test = words[0].trim();
            let expected: Vec<&str> = words[1].trim_start_matches(' ').split(',').collect();
            let result = shlex
                .process_words(test, &envs_from_slice(&envs))
                .unwrap_or_else(|_| vec!["error".to_string()]);
            assert_eq!(result, expected, "line {line_num}: {test:?}");
            checked += 1;
        }
        assert!(checked > 20);
    }

    #[test]
    fn get_env() {
        let lex = Lex::new('\\');
        let lookup = |entries: &[&str], name: &str| {
            let envs = envs_from_slice(entries);
            let mut sw = ShellWord::new(&lex, "", &envs, true);
            sw.get_env(name).0
        };
        assert_eq!(lookup(&[], "foo"), "");
        assert_eq!(lookup(&["foo"], "foo"), "");
        assert_eq!(lookup(&["foo="], "foo"), "");
        assert_eq!(lookup(&["foo=bar"], "foo"), "bar");
        assert_eq!(lookup(&["foo=bar", "car=hat"], "foo"), "bar");
        assert_eq!(lookup(&["foo=bar", "car=hat"], "car"), "hat");
        assert_eq!(lookup(&["foo=bar", "car=hat", "car=bike"], "car"), "bike");
        assert_eq!(
            envs_from_slice(&["a=1", "b", "a=2"]).keys(),
            vec!["a".to_string(), "b".into(), "a".into()]
        );
        assert!(equal_env_keys("A", "A") && !equal_env_keys("A", "a"));
        assert_eq!(normalize_env_key("Path"), "Path");
    }

    struct MatchCase {
        input: &'static str,
        envs: &'static [(&'static str, &'static str)],
        expected: Option<&'static str>,
        matches: &'static [&'static str],
        unmatched: &'static [&'static str],
    }

    const fn ok(
        input: &'static str,
        envs: &'static [(&'static str, &'static str)],
        expected: &'static str,
        matches: &'static [&'static str],
        unmatched: &'static [&'static str],
    ) -> MatchCase {
        MatchCase {
            input,
            envs,
            expected: Some(expected),
            matches,
            unmatched,
        }
    }

    const fn fails(
        input: &'static str,
        envs: &'static [(&'static str, &'static str)],
    ) -> MatchCase {
        MatchCase {
            input,
            envs,
            expected: None,
            matches: &[],
            unmatched: &[],
        }
    }

    const FOO_BAR: &[(&str, &str)] = &[("FOO", "xxx"), ("BAR", "")];

    #[test]
    fn process_with_matches() {
        let shlex = Lex::new('\\');
        let cases = [
            ok("x", &[("DUMMY", "dummy")], "x", &[], &[]),
            ok("x ${UNUSED}", &[("DUMMY", "dummy")], "x ", &[], &["UNUSED"]),
            ok("x ${FOO}", &[("FOO", "y")], "x y", &["FOO"], &[]),
            ok(
                "${FOO-aaa} ${BAR-bbb} ${BAZ-ccc}",
                FOO_BAR,
                "xxx  ccc",
                &["FOO", "BAR"],
                &["BAZ"],
            ),
            ok(
                "${FOO:-aaa} ${BAR:-bbb} ${BAZ:-ccc}",
                FOO_BAR,
                "xxx bbb ccc",
                &["FOO", "BAR"],
                &["BAZ"],
            ),
            ok("${FOO:-}", FOO_BAR, "xxx", &["FOO"], &[]),
            ok("${FOO:-}", &[], "", &[], &["FOO"]),
            ok(
                "${FOO+aaa} ${BAR+bbb} ${BAZ+ccc}",
                FOO_BAR,
                "aaa bbb ",
                &["FOO", "BAR"],
                &["BAZ"],
            ),
            ok(
                "${FOO:+aaa} ${BAR:+bbb} ${BAZ:+ccc}",
                FOO_BAR,
                "aaa  ",
                &["FOO", "BAR"],
                &["BAZ"],
            ),
            ok("${FOO?aaa}", FOO_BAR, "xxx", &["FOO"], &[]),
            ok("${BAR?bbb}", FOO_BAR, "", &["BAR"], &[]),
            fails("${BAZ?ccc}", FOO_BAR),
            ok("${FOO:?aaa}", FOO_BAR, "xxx", &["FOO"], &[]),
            fails("${BAR:?bbb}", FOO_BAR),
            fails("${BAZ:?ccc}", FOO_BAR),
            fails("${BAZ:?}", FOO_BAR),
            fails("${FOO=aaa}", FOO_BAR),
            fails("${FOO=:aaa}", FOO_BAR),
            fails("${FOO=}", &[]),
            ok(
                "${FOO#()[]{\\}^$.\\*\\?|\\\\}",
                &[("FOO", "()[]{}^$.*?|\\x")],
                "x",
                &["FOO"],
                &[],
            ),
            ok("${FOO%%\\**}", &[("FOO", "xx**")], "xx", &["FOO"], &[]),
            ok("${FOO#*x*y}", &[("FOO", "xxyy")], "y", &["FOO"], &[]),
            ok("${FOO#*}", &[("FOO", "xxyy")], "xxyy", &["FOO"], &[]),
            ok(
                "${FOO#$BAR}",
                &[("FOO", "xxyy"), ("BAR", "x")],
                "xyy",
                &["FOO", "BAR"],
                &[],
            ),
            ok(
                "${FOO#$BAR}",
                &[("FOO", "xxyy"), ("BAR", "")],
                "xxyy",
                &["FOO", "BAR"],
                &[],
            ),
            ok("${FOO#}", &[("FOO", "xxyy")], "xxyy", &["FOO"], &[]),
            ok("${FOO##*x}", &[("FOO", "xxyy")], "yy", &["FOO"], &[]),
            ok("${FOO##}", &[("FOO", "xxyy")], "xxyy", &["FOO"], &[]),
            ok("${FOO#?\\?}", &[("FOO", "???y")], "?y", &["FOO"], &[]),
            ok(
                "${ABC:-.}${FOO%x}${ABC:-.}",
                &[("FOO", "xxyy")],
                ".xxyy.",
                &["FOO"],
                &["ABC"],
            ),
            ok("${FOO%%\\**\\*}", &[("FOO", "a***yy*")], "a", &["FOO"], &[]),
            ok("${FOO%}", &[("FOO", "xxyy")], "xxyy", &["FOO"], &[]),
            ok(
                "${FOO%%$BAR}",
                &[("FOO", "xxyy"), ("BAR", "")],
                "xxyy",
                &["FOO", "BAR"],
                &[],
            ),
            fails("${FOO:#}", &[]),
            fails("${FOO:##}", &[]),
            fails("${FOO:%}", &[]),
            fails("${FOO:%%}", &[]),
            ok(
                "${FOO/$NEEDLE/.} - ${FOO//$NEEDLE/.}",
                &[("FOO", "/foo*/*/*.txt"), ("NEEDLE", "\\*/")],
                "/foo.*/*.txt - /foo..*.txt",
                &["FOO", "NEEDLE"],
                &[],
            ),
            ok(
                "${FOO/$NEEDLE/} - ${FOO//$NEEDLE/}",
                &[("FOO", "/tmp/tmp/bar.txt"), ("NEEDLE", "/tmp")],
                "/tmp/bar.txt - /bar.txt",
                &["FOO", "NEEDLE"],
                &[],
            ),
            ok(
                "${FOO/$NEEDLE/$REPLACEMENT} - ${FOO//$NEEDLE/$REPLACEMENT}",
                &[
                    ("FOO", "/a/foo/b/c.txt"),
                    ("NEEDLE", "/?/"),
                    ("REPLACEMENT", "/"),
                ],
                "/foo/b/c.txt - /foo/c.txt",
                &["FOO", "NEEDLE", "REPLACEMENT"],
                &[],
            ),
            ok(
                "${FOO/$NEEDLE/$REPLACEMENT}",
                &[
                    ("FOO", "http://google.de"),
                    ("NEEDLE", "http://"),
                    ("REPLACEMENT", "https://"),
                ],
                "https://google.de",
                &["FOO", "NEEDLE", "REPLACEMENT"],
                &[],
            ),
            ok(
                "${FOO//\\//\\/}",
                &[("FOO", "/tmp/foo.txt")],
                "\\/tmp\\/foo.txt",
                &["FOO"],
                &[],
            ),
            ok(
                "${FOO/$BAR/ww}",
                &[("FOO", "xxyy"), ("BAR", "")],
                "wwxxyy",
                &["FOO", "BAR"],
                &[],
            ),
            fails("${FOO//ww}", &[("FOO", "xxyy")]),
            fails("${FOO//}", &[("FOO", "xxyy")]),
            ok("${FOO///}", &[("FOO", "xxyy")], "xxyy", &["FOO"], &[]),
            ok("${FOO///}", &[], "", &[], &["FOO"]),
        ];
        for c in cases {
            let res = shlex.process_word_with_matches(c.input, &envs_from_map(c.envs));
            let Some(expected) = c.expected else {
                assert!(res.is_err(), "{}: {res:?}", c.input);
                continue;
            };
            let res = res.unwrap_or_else(|e| panic!("{}: {e}", c.input));
            assert_eq!(res.result, expected, "{}", c.input);
            assert_eq!(res.matched, set(c.matches), "{}", c.input);
            assert_eq!(res.unmatched, set(c.unmatched), "{}", c.input);
        }
    }

    #[test]
    fn process_with_matches_platform() {
        let shlex = Lex::new('\\');
        let release = "something-${VERSION}.${TARGETOS}-${TARGETARCH}${TARGETVARIANT:+-${TARGETVARIANT}}.tar.gz";
        let version = "v1.2.3";
        let res = shlex
            .process_word_with_matches(
                release,
                &envs_from_map(&[
                    ("VERSION", version),
                    ("TARGETOS", "linux"),
                    ("TARGETARCH", "arm"),
                    ("TARGETVARIANT", "v7"),
                ]),
            )
            .unwrap();
        assert_eq!(res.result, "something-v1.2.3.linux-arm-v7.tar.gz");
        let res = shlex
            .process_word_with_matches(
                release,
                &envs_from_map(&[
                    ("VERSION", version),
                    ("TARGETOS", "linux"),
                    ("TARGETARCH", "arm64"),
                    ("TARGETVARIANT", ""),
                ]),
            )
            .unwrap();
        assert_eq!(res.result, "something-v1.2.3.linux-arm64.tar.gz");
        let res = shlex
            .process_word_with_matches(
                release,
                &envs_from_map(&[
                    ("VERSION", version),
                    ("TARGETOS", "linux"),
                    ("TARGETARCH", "arm64"),
                ]),
            )
            .unwrap();
        assert_eq!(res.result, "something-v1.2.3.linux-arm64.tar.gz");
    }

    #[test]
    fn a_prefix_is_matched_from_the_start_only() {
        // Every pattern of these tokens on every text of these chars, as the whole
        // table gives it.
        let alphabet = ['a', 'b', '\n'];
        let patterns = [
            "", "a", "ab", "?", "*", "a*", "*a", "a*b", "?*?", "a**b", "*b*", "ab?",
        ];
        let mut texts = vec![String::new()];
        for len in 1..=5 {
            let mut more = Vec::new();
            for t in texts.iter().filter(|t| t.chars().count() == len - 1) {
                for c in alphabet {
                    more.push(format!("{t}{c}"));
                }
            }
            texts.extend(more);
        }
        for pattern in patterns {
            for greedy in [false, true] {
                let re = ShellPattern::new(pattern, greedy, true).unwrap();
                for text in &texts {
                    let chars: Vec<char> = text.chars().collect();
                    let whole = re.find(&re.first_ends(&chars), 0).map(|(_, end)| end);
                    assert_eq!(
                        re.end_at_start(&chars),
                        whole,
                        "{pattern:?} {text:?} {greedy}"
                    );
                }
            }
        }
        // A long literal prefix costs its length, not its length times the value's.
        let value = "a".repeat(1_020_000);
        let prefix = "a".repeat(60_000);
        assert_eq!(trim_prefix(&prefix, &value, false).unwrap().len(), 960_000);
        assert_eq!(trim_suffix(&prefix, &value, true).unwrap().len(), 960_000);
    }

    #[test]
    fn replace_all_matches_go_regexp() {
        let shlex = Lex::new('\\');
        let envs = envs_from_slice(&["V=a.b.c", "E=", "N=x\ny"]);
        let word = |w: &str| shlex.process_word(w, &envs).unwrap().0;
        assert_eq!(word("${V//./-}"), "a-b-c");
        assert_eq!(word("${V/./-}"), "a-b.c");
        assert_eq!(word("${V//?/x}"), "xxxxx");
        assert_eq!(word("${V//*/x}"), "x");
        assert_eq!(word("${E//*/x}"), "x");
        // The escape stays in a replacement and $0 is the match.
        assert_eq!(word("${V//b/[\\$0]}"), "a.[\\b].c");
        let mut skip = Lex::new('\\');
        skip.skip_unset_env = true;
        let skipped = |w: &str| skip.process_word(w, &envs).unwrap().0;
        assert_eq!(skipped("${V//b/<$0>}"), "a.<b>.c");
        assert_eq!(skipped("${V//b/<${0}$1>}"), "a.<b>.c");
        assert_eq!(skipped("${V/b/<$0>}"), "a.<$0>.c");
        assert_eq!(word("${N#*}"), "x\ny");
        assert_eq!(word("${N##*}"), "\ny");
        assert_eq!(word("${N%%*}"), "x\n");
    }

    #[test]
    fn skip_unset_and_raw_options() {
        let mut lex = Lex::new('\\');
        lex.skip_unset_env = true;
        let envs = envs_from_slice(&["SET=1"]);
        let word = |lex: &Lex, w: &str| lex.process_word(w, &envs).unwrap().0;
        assert_eq!(
            word(&lex, "$X ${Y} ${Z:-d} ${W//a/b} $SET"),
            "$X ${Y} ${Z:-d} ${W/a/b} 1"
        );
        lex.raw_quotes = true;
        assert_eq!(word(&lex, "'a b' \"c\""), "'a b' \"c\"");
        lex.raw_escapes = true;
        assert_eq!(word(&lex, "a\\ b"), "a\\ b");
        let mut lex = Lex::new('`');
        lex.skip_process_quotes = true;
        assert_eq!(word(&lex, "'a`$SET'"), "'a$SET'");
        assert_eq!(lex.escape_token(), '`');
    }

    #[test]
    fn nested_modifier_resets_the_word_buffer() {
        let shlex = Lex::new('\\');
        let envs = envs_from_slice::<&str>(&[]);
        assert_eq!(
            shlex.process_words("pre${X:-val} next", &envs).unwrap(),
            vec!["val".to_string(), "next".into()]
        );
    }
}
