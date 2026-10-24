//! Parsing layer of the Dockerfile frontend of moby/buildkit v0.33.1.
//!
//! - [`parser`] turns the bytes of a Dockerfile into the reference's AST
//!   (parser.go, line_parsers.go, split_command.go, directives.go).
//! - [`shell`] expands variables and splits words (lex.go).
//! - [`instructions`] turns the AST into typed commands grouped in stages
//!   (parse.go, commands.go, bflag.go and the RUN flag files).
//!
//! Error messages and the AST dump are the reference's, so its test vectors
//! apply unchanged. The linter hooks of the reference are not part of the
//! port: where it reports a lint warning nothing happens here.
// The port keeps the reference's API whole; its tests use what the builder does not.
#![allow(dead_code)]

mod bflag;
pub(crate) mod compat;
pub mod instructions;
mod json;
pub mod parser;
mod run_flags;
pub mod shell;
mod unicode;

use std::fmt;

/// A point in the Dockerfile. Lines are counted by the parser and are
/// 1-based, except where the reference reports line 0 (errors on the very
/// first line); `character` is always 0, as in the reference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

/// A section of the Dockerfile between two positions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// One range per line from `start` to `end`, or a single one when `end` is
/// not after `start` (toRanges in errors.go).
pub fn to_ranges(start: usize, end: usize) -> Vec<Range> {
    let end = end.max(start);
    (start..=end)
        .map(|line| Range {
            start: Position { line, character: 0 },
            end: Position { line, character: 0 },
        })
        .collect()
}

/// Error of the parsing layer.
///
/// `Display` gives the reference's message. `locations` holds the source
/// ranges attached while the error travelled up, innermost first, like the
/// reference's LocationError.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
    locations: Vec<Vec<Range>>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            locations: Vec::new(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn locations(&self) -> &[Vec<Range>] {
        &self.locations
    }

    /// Adds a location (WithLocation in errors.go).
    pub fn with_location(mut self, location: Vec<Range>) -> Error {
        self.locations.push(location);
        self
    }

    /// Replaces every location with this one (SetLocation in errors.go).
    pub fn set_location(mut self, location: Vec<Range>) -> Error {
        self.locations = vec![location];
        self
    }

    /// Prefixes the message the way errors.Wrap does, keeping the locations.
    pub(crate) fn wrap(mut self, prefix: &str) -> Error {
        self.message = format!("{prefix}: {}", self.message);
        self
    }

    pub(crate) fn at_lines(self, start: usize, end: usize) -> Error {
        self.with_location(to_ranges(start, end))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Directory of the test vectors copied from the reference.
#[cfg(test)]
pub(crate) fn testdata(path: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/build/buildkit")
        .join(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(to_ranges(3, 1), to_ranges(3, 3));
        assert_eq!(to_ranges(2, 4).len(), 3);
        assert_eq!(to_ranges(2, 4)[1].start.line, 3);
    }

    #[test]
    fn error_locations() {
        let e = Error::new("x")
            .with_location(to_ranges(2, 3))
            .with_location(to_ranges(1, 1));
        assert_eq!(e.message(), "x");
        assert_eq!(e.locations(), &[to_ranges(2, 3), to_ranges(1, 1)]);
        let e = e.set_location(to_ranges(5, 0));
        assert_eq!(e.locations(), &[to_ranges(5, 5)]);
        let e = e.wrap("prefix");
        assert_eq!(e.to_string(), "prefix: x");
        assert_eq!(e.locations(), &[to_ranges(5, 5)]);
    }
}
