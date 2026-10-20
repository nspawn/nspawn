//! The `${}` of compose files, as compose-go's template package substitutes them: `$$`
//! is a `$`; `$VAR` and `${VAR}` the variable's value; `${VAR:-default}`,
//! `${VAR-default}`, `${VAR:+alternative}`, `${VAR+alternative}`, `${VAR:?error}` and
//! `${VAR?error}` the rest, the argument substituted in turn. compose-go finds them
//! with a regular expression; the scanner here takes the same matches.

use std::fmt;

/// The value of a variable, None when it is not set.
pub trait Lookup {
    fn lookup(&self, name: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String>> Lookup for F {
    fn lookup(&self, name: &str) -> Option<String> {
        self(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A `$` that starts nothing compose-go reads, with the template it is in.
    Invalid(String),
    /// A `${VAR:?reason}` without its variable.
    Missing { variable: String, reason: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(template) => {
                write!(f, "Invalid template: {}", super::gostr::quote(template))
            }
            Error::Missing { variable, reason } if reason.is_empty() => {
                write!(f, "required variable {variable} is missing a value")
            }
            Error::Missing { variable, reason } => {
                write!(
                    f,
                    "required variable {variable} is missing a value: {reason}"
                )
            }
        }
    }
}

/// What a `$` at the start of `text` matches, as compose-go's pattern
/// `\$(?i:(?P<escaped>\$)|(?P<named>[_a-z][_a-z0-9]*)|{(?:(?P<braced>[_a-z][_a-z0-9]*(?::?[-+?](.*))?)}|(?P<invalid>)))`
/// does: the length of the match, or None where it matches nothing.
fn match_at(text: &str) -> Option<usize> {
    debug_assert!(text.starts_with('$'));
    // [_a-z] under (?i), as Go's regexp folds it: the Kelvin sign is a k and the long s
    // an s.
    let name_start =
        |c: char| c == '_' || c.is_ascii_alphabetic() || c == '\u{17f}' || c == '\u{212a}';
    let name_rest = |c: char| name_start(c) || c.is_ascii_digit();
    let name_end = |from: usize| {
        text[from..]
            .char_indices()
            .find(|&(_, c)| !name_rest(c))
            .map_or(text.len(), |(i, _)| from + i)
    };
    let second = text[1..].chars().next()?;
    match second {
        '$' => Some(2),
        c if name_start(c) => Some(name_end(1 + c.len_utf8())),
        '{' => {
            let braced = (|| {
                let first = text[2..].chars().next()?;
                if !name_start(first) {
                    return None;
                }
                let end = name_end(2 + first.len_utf8());
                let bytes = text.as_bytes();
                if bytes.get(end) == Some(&b'}') {
                    return Some(end + 1);
                }
                let mut op = end;
                if bytes.get(op) == Some(&b':') {
                    op += 1;
                }
                if !matches!(bytes.get(op)?, b'-' | b'+' | b'?') {
                    return None;
                }
                // `(.*)` then `}`: the last `}` on the line.
                let line_end = text[op + 1..]
                    .find('\n')
                    .map_or(text.len(), |at| op + 1 + at);
                text[op + 1..line_end].rfind('}').map(|at| op + 1 + at + 1)
            })();
            // Otherwise the empty `invalid` group matches after `${`.
            Some(braced.unwrap_or(2))
        }
        _ => None,
    }
}

/// What a match of the pattern holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group<'a> {
    Escaped,
    Named(&'a str),
    Braced(&'a str),
    Invalid,
}

/// Every match of compose-go's pattern in `text`, left to right, as FindAllStringSubmatch
/// gives them.
pub fn find_all(text: &str) -> Vec<(&str, Group<'_>)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..].find('$') {
        let start = at + offset;
        let candidate = &text[start..];
        match match_at(candidate) {
            None => at = start + 1,
            Some(len) => {
                let matched = &candidate[..len];
                let group = if matched == "$$" {
                    Group::Escaped
                } else if let Some(inner) = matched.strip_prefix("${") {
                    match inner.strip_suffix('}') {
                        Some(braced) if !braced.is_empty() => Group::Braced(braced),
                        _ => Group::Invalid,
                    }
                } else {
                    Group::Named(&matched[1..])
                };
                found.push((matched, group));
                at = start + len;
            }
        }
    }
    found
}

/// compose-go's getFirstBraceClosingIndex: where the first brace closes, the
/// character after each `{` skipped.
pub fn first_closing_brace(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut open = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'}' {
            open -= 1;
            if open == 0 {
                return Some(i);
            }
        }
        if bytes[i] == b'{' {
            open += 1;
            i += 1;
        }
        i += 1;
    }
    None
}

/// Substitutes `template`, reporting each variable that is not set through `unset`.
pub fn substitute(
    template: &str,
    lookup: &dyn Lookup,
    unset: &mut dyn FnMut(&str),
) -> Result<String, Error> {
    let mut out = String::with_capacity(template.len());
    let mut first_error = None;
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        let Some(len) = match_at(candidate) else {
            out.push('$');
            rest = &candidate[1..];
            continue;
        };
        match replace(&candidate[..len], lookup, unset) {
            Ok(replacement) => out.push_str(&replacement),
            Err(error) => {
                let error = match error {
                    Error::Invalid(t) if t.is_empty() => Error::Invalid(template.to_string()),
                    other => other,
                };
                first_error.get_or_insert(error);
            }
        }
        rest = &candidate[len..];
    }
    out.push_str(rest);
    match first_error {
        Some(error) => Err(error),
        None => Ok(out),
    }
}

/// compose-go's DefaultReplacementAppliedFunc for one match.
fn replace(
    matched: &str,
    lookup: &dyn Lookup,
    unset: &mut dyn FnMut(&str),
) -> Result<String, Error> {
    if matched == "$$" {
        return Ok("$".to_string());
    }
    let function = operator_for(matched);
    let (substring, rest) = match first_closing_brace(matched) {
        Some(at) => (&matched[..=at], &matched[at + 1..]),
        None => (matched, ""),
    };
    let len = match_at(substring).unwrap_or(0);
    let substring = &substring[..len];
    let (substitution, braced) = if let Some(inner) = substring.strip_prefix("${") {
        (inner.strip_suffix('}').unwrap_or(""), true)
    } else {
        (&substring[1..], false)
    };
    if substitution.is_empty() || (braced && len <= 2) {
        return Err(Error::Invalid(String::new()));
    }
    if braced {
        if let Some(value) = apply(function, substitution, lookup, unset)? {
            let nested = substitute(rest, lookup, unset)?;
            return Ok(value + &nested);
        }
    }
    match lookup.lookup(substitution) {
        Some(value) => Ok(value),
        None => {
            unset(substitution);
            Ok(String::new())
        }
    }
}

#[derive(Clone, Copy)]
enum Operator {
    RequiredNotEmpty,
    Required,
    DefaultEmpty,
    Default,
    AlternativeNotEmpty,
    Alternative,
}

const OPERATORS: [(&str, Operator); 6] = [
    (":?", Operator::RequiredNotEmpty),
    ("?", Operator::Required),
    (":-", Operator::DefaultEmpty),
    ("-", Operator::Default),
    (":+", Operator::AlternativeNotEmpty),
    ("+", Operator::Alternative),
];

/// The operator found first in the whole match, as compose-go picks it.
fn operator_for(matched: &str) -> Operator {
    OPERATORS
        .iter()
        .filter_map(|(sep, op)| matched.find(sep).map(|at| (at, *op)))
        .min_by_key(|(at, _)| *at)
        .map_or(Operator::RequiredNotEmpty, |(_, op)| op)
}

fn apply(
    operator: Operator,
    substitution: &str,
    lookup: &dyn Lookup,
    unset: &mut dyn FnMut(&str),
) -> Result<Option<String>, Error> {
    let sep = OPERATORS
        .iter()
        .find(|(_, op)| std::mem::discriminant(op) == std::mem::discriminant(&operator))
        .map(|(sep, _)| *sep)
        .unwrap();
    let Some((name, argument)) = substitution.split_once(sep) else {
        return Ok(None);
    };
    let value = lookup.lookup(name);
    match operator {
        Operator::DefaultEmpty | Operator::Default => {
            let empty_counts = matches!(operator, Operator::DefaultEmpty);
            match value {
                Some(value) if !(empty_counts && value.is_empty()) => Ok(Some(value)),
                _ => substitute(argument, lookup, unset).map(Some),
            }
        }
        Operator::AlternativeNotEmpty | Operator::Alternative => {
            let not_empty = matches!(operator, Operator::AlternativeNotEmpty);
            match value {
                Some(value) if !(not_empty && value.is_empty()) => {
                    substitute(argument, lookup, unset).map(Some)
                }
                value => Ok(Some(value.unwrap_or_default())),
            }
        }
        Operator::RequiredNotEmpty | Operator::Required => {
            let not_empty = matches!(operator, Operator::RequiredNotEmpty);
            match value {
                Some(value) if !(not_empty && value.is_empty()) => Ok(Some(value)),
                _ => Err(Error::Missing {
                    variable: name.to_string(),
                    reason: substitute(argument, lookup, unset)?,
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env() -> HashMap<&'static str, &'static str> {
        HashMap::from([("FOO", "first"), ("BAR", ""), ("JSON", "{\"a\":1}")])
    }

    fn sub(template: &str) -> Result<String, Error> {
        let env = env();
        let lookup = |name: &str| env.get(name).map(|v| v.to_string());
        substitute(template, &lookup, &mut |_| {})
    }

    #[test]
    fn variables_take_their_values() {
        assert_eq!(
            sub("$FOO ${FOO} $$FOO $1 $}").unwrap(),
            "first first $FOO $1 $}"
        );
        assert_eq!(sub("${FOO}-x$FOO-bar").unwrap(), "first-xfirst-bar");
        assert_eq!(sub("${MISSING}").unwrap(), "");
        assert_eq!(sub("^x$").unwrap(), "^x$");
    }

    #[test]
    fn defaults_and_alternatives() {
        assert_eq!(sub("${BAR:-x} ${BAR-x} ${NO-x}").unwrap(), "x  x");
        assert_eq!(
            sub("${FOO:+alt} ${BAR:+alt} ${BAR+alt} ${NO+alt}").unwrap(),
            "alt  alt "
        );
        assert_eq!(sub("${NO:-${FOO:-x}}").unwrap(), "first");
        assert_eq!(sub("${NO:-{\"json\":1}}").unwrap(), "{\"json\":1}");
        assert_eq!(sub("${NO:-a} and ${FOO}").unwrap(), "a and first");
        assert_eq!(sub("${NO:-a}-${NO2:-b}").unwrap(), "a-b");
    }

    #[test]
    fn required_variables_say_what_is_missing() {
        assert_eq!(
            sub("${BAR:?need it}").unwrap_err().to_string(),
            "required variable BAR is missing a value: need it"
        );
        assert_eq!(sub("${BAR?x}").unwrap(), "");
        assert_eq!(
            sub("${NO?}").unwrap_err().to_string(),
            "required variable NO is missing a value"
        );
    }

    #[test]
    fn a_dollar_that_starts_nothing_valid_is_refused() {
        for (template, shown) in [
            ("${", "${"),
            ("${}", "${}"),
            ("${ x}", "${ x}"),
            ("a ${A:-x} b ${!x}", " b ${!x}"),
        ] {
            assert_eq!(
                sub(template).unwrap_err(),
                Error::Invalid(shown.to_string()),
                "{template}"
            );
        }
    }

    #[test]
    fn unset_variables_are_reported() {
        let env = env();
        let lookup = |name: &str| env.get(name).map(|v| v.to_string());
        let mut seen = Vec::new();
        substitute("$A ${B} ${C:-x} ${FOO}", &lookup, &mut |name| {
            seen.push(name.to_string())
        })
        .unwrap();
        assert_eq!(seen, ["A", "B"]);
    }
}
