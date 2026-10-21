//! `config --variables`: the variables a model uses, with the default, the alternative
//! and whether each must be set, as compose-go's template.ExtractVariables finds them.

use std::collections::BTreeMap;

use super::emit::{self, Out};
use super::template::{find_all, first_closing_brace, Group};
use super::value::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    pub default: String,
    pub presence: String,
    pub required: bool,
}

pub type Variables = BTreeMap<String, Variable>;

pub fn extract(value: &Value) -> Variables {
    let mut found = Variables::new();
    walk(value, &mut found);
    found
}

fn walk(value: &Value, found: &mut Variables) {
    match value {
        Value::Str(s) => {
            for v in extract_from(s) {
                combine(found, v);
            }
        }
        Value::Map(map) => {
            for entry in map.values() {
                walk(entry, found);
            }
        }
        Value::Seq(items) => {
            for item in items {
                walk(item, found);
            }
        }
        _ => {}
    }
}

fn combine(found: &mut Variables, v: Variable) {
    match found.get_mut(&v.name) {
        None => {
            found.insert(v.name.clone(), v);
        }
        Some(existing) => {
            existing.required |= v.required;
            if existing.default.is_empty() {
                existing.default = v.default;
            }
            if existing.presence.is_empty() {
                existing.presence = v.presence;
            }
        }
    }
}

fn extract_from(text: &str) -> Vec<Variable> {
    let mut values = Vec::new();
    for (matched, group) in find_all(text) {
        let mut val = match group {
            Group::Escaped => continue,
            Group::Named(name) => name.to_string(),
            Group::Braced(braced) => braced.to_string(),
            Group::Invalid => String::new(),
        };
        if !matches!(group, Group::Named(_)) {
            if let Some(i) = first_closing_brace(matched).filter(|i| *i > 0) {
                val = matched[2..i].to_string();
                if matched.len() > i {
                    values.extend(extract_from(&matched[i + 1..]));
                }
            }
        }
        let mut variable = Variable {
            name: val.clone(),
            ..Default::default()
        };
        if let Some(i) = val
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|i| *i > 0)
        {
            variable.name = val[..i].to_string();
            let rest = &val[i..];
            if rest.starts_with(":?") || rest.starts_with('?') {
                variable.required = true;
            } else if let Some(d) = rest.strip_prefix(":-") {
                variable.default = d.to_string();
            } else if let Some(d) = rest.strip_prefix('-') {
                variable.default = d.to_string();
            } else if let Some(p) = rest.strip_prefix(":+") {
                variable.presence = p.to_string();
            } else if let Some(p) = rest.strip_prefix('+') {
                variable.presence = p.to_string();
            }
        }
        let (default, presence) = (variable.default.clone(), variable.presence.clone());
        values.push(variable);
        if !default.is_empty() {
            values.extend(extract_from(&default));
        }
        if !presence.is_empty() {
            values.extend(extract_from(&presence));
        }
    }
    values
}

/// The variables printed: YAML for exactly `yaml`, else what formatter.Print takes, a
/// table or JSON, named in any case.
pub fn render(variables: &Variables, format: &str) -> Result<String, String> {
    if format == "yaml" {
        let out = Out::Dict(
            variables
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        Out::Struct(vec![
                            ("name".to_string(), Out::str(&v.name)),
                            ("defaultvalue".to_string(), Out::str(&v.default)),
                            ("presencevalue".to_string(), Out::str(&v.presence)),
                            ("required".to_string(), Out::Bool(v.required)),
                        ]),
                    )
                })
                .collect(),
        );
        return Ok(emit::marshal(&out));
    }
    match super::gostr::to_lower_str(format).as_str() {
        "" | "table" | "pretty" => {
            let mut text = String::from("NAME\tREQUIRED\tDEFAULT VALUE\tALTERNATE VALUE\n");
            for v in variables.values() {
                text.push_str(&format!(
                    "{}\t{}\t{}\t{}\n",
                    v.name, v.required, v.default, v.presence
                ));
            }
            Ok(tabwriter(&text))
        }
        "json" | "{{json.}}" => {
            let out = Out::Dict(
                variables
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            Out::Struct(vec![
                                ("Name".to_string(), Out::str(&v.name)),
                                ("DefaultValue".to_string(), Out::str(&v.default)),
                                ("PresenceValue".to_string(), Out::str(&v.presence)),
                                ("Required".to_string(), Out::Bool(v.required)),
                            ]),
                        )
                    })
                    .collect(),
            );
            Ok(format!(
                "{}\n\n",
                emit::json_with(&out, Some("    "), false)?
            ))
        }
        _ => Err(format!(
            "format value {} could not be parsed: parsing failed",
            super::golib::go_quote(format)
        )),
    }
}

#[derive(Default, Clone, Copy)]
struct Cell {
    size: usize,
    width: usize,
    htab: bool,
}

/// Go's text/tabwriter with a minimum width and a padding of spaces, flags 0, written
/// all at once and flushed: cells end at tabs and line ends, a column's width holds for
/// the run of lines that have it, and text between two 0xff bytes is taken as it is.
struct TabWriter {
    min_width: usize,
    padding: usize,
    buf: Vec<u8>,
    pos: usize,
    cell: Cell,
    escaped: bool,
    lines: Vec<Vec<Cell>>,
    widths: Vec<usize>,
    out: Vec<u8>,
    /// How wide a piece of a cell is.
    measure: fn(&str) -> usize,
}

impl TabWriter {
    fn update_width(&mut self) {
        let text = super::gostr::from_bytes(&self.buf[self.pos..]);
        self.cell.width += (self.measure)(&text);
        self.pos = self.buf.len();
    }

    fn terminate_cell(&mut self, htab: bool) -> usize {
        self.cell.htab = htab;
        let line = self.lines.last_mut().expect("a line");
        line.push(self.cell);
        self.cell = Cell::default();
        line.len()
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut n = 0;
        for (i, &ch) in bytes.iter().enumerate() {
            if !self.escaped {
                match ch {
                    b'\t' | b'\x0b' | b'\n' | b'\x0c' => {
                        self.buf.extend_from_slice(&bytes[n..i]);
                        self.cell.size += i - n;
                        self.update_width();
                        n = i + 1;
                        let cells = self.terminate_cell(ch == b'\t');
                        if ch == b'\n' || ch == b'\x0c' {
                            self.lines.push(Vec::new());
                            if ch == b'\x0c' || cells == 1 {
                                self.flush();
                            }
                        }
                    }
                    0xff => {
                        self.buf.extend_from_slice(&bytes[n..i]);
                        self.cell.size += i - n;
                        self.update_width();
                        n = i;
                        self.escaped = true;
                    }
                    _ => {}
                }
            } else if ch == 0xff {
                self.buf.extend_from_slice(&bytes[n..=i]);
                self.cell.size += i + 1 - n;
                n = i + 1;
                self.end_escape();
            }
        }
        self.buf.extend_from_slice(&bytes[n..]);
        self.cell.size += bytes.len() - n;
    }

    fn end_escape(&mut self) {
        self.update_width();
        // The two 0xff bytes take no room.
        self.cell.width = self.cell.width.saturating_sub(2);
        self.escaped = false;
    }

    fn flush(&mut self) {
        if self.cell.size > 0 {
            if self.escaped {
                self.end_escape();
            }
            self.terminate_cell(false);
        }
        let lines = self.lines.len();
        self.format(0, 0, lines);
        self.buf.clear();
        self.pos = 0;
        self.cell = Cell::default();
        self.lines = vec![Vec::new()];
        self.widths.clear();
    }

    fn write_lines(&mut self, pos0: usize, line0: usize, line1: usize) -> usize {
        let mut pos = pos0;
        for i in line0..line1 {
            let line = self.lines[i].clone();
            for (j, c) in line.iter().enumerate() {
                if c.size > 0 {
                    self.out.extend_from_slice(&self.buf[pos..pos + c.size]);
                    pos += c.size;
                }
                if let Some(&width) = self.widths.get(j) {
                    let pad = width.saturating_sub(c.width);
                    self.out.extend(std::iter::repeat_n(b' ', pad));
                }
            }
            if i + 1 == self.lines.len() {
                self.out
                    .extend_from_slice(&self.buf[pos..pos + self.cell.size]);
                pos += self.cell.size;
            } else {
                self.out.push(b'\n');
            }
        }
        pos
    }

    fn format(&mut self, pos0: usize, mut line0: usize, line1: usize) -> usize {
        let mut pos = pos0;
        let column = self.widths.len();
        let mut this = line0;
        while this < line1 {
            if column + 1 >= self.lines[this].len() {
                this += 1;
                continue;
            }
            pos = self.write_lines(pos, line0, this);
            line0 = this;
            let mut width = self.min_width;
            while this < line1 {
                let line = &self.lines[this];
                if column + 1 >= line.len() {
                    break;
                }
                width = width.max(line[column].width + self.padding);
                this += 1;
            }
            self.widths.push(width);
            pos = self.format(pos, line0, this);
            self.widths.pop();
            line0 = this;
        }
        self.write_lines(pos, line0, line1)
    }
}

/// The table of --variables through Go's tabwriter (columns at least 20 wide, 3
/// spaces of padding).
fn tabwriter(text: &str) -> String {
    table(text, 20, 3)
}

/// Lines of tab-separated cells laid out by Go's tabwriter, which counts characters.
pub fn table(text: &str, min_width: usize, padding: usize) -> String {
    lay_out(text, min_width, padding, |piece| piece.chars().count())
}

/// The same through docker/cli's copy of the tabwriter, which counts terminal columns:
/// compose writes ps and volumes with it.
pub fn columns_table(text: &str, min_width: usize, padding: usize) -> String {
    lay_out(text, min_width, padding, |piece| {
        crate::columns::string_width(&piece.chars().map(super::gostr::rune).collect::<String>())
    })
}

fn lay_out(text: &str, min_width: usize, padding: usize, measure: fn(&str) -> usize) -> String {
    let mut w = TabWriter {
        min_width,
        padding,
        buf: Vec::new(),
        pos: 0,
        cell: Cell::default(),
        escaped: false,
        lines: vec![Vec::new()],
        widths: Vec::new(),
        out: Vec::new(),
        measure,
    };
    w.write(&super::gostr::to_bytes(text));
    w.flush();
    super::gostr::from_bytes(&w.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_laid_out_as_go_s_tabwriter() {
        assert_eq!(
            tabwriter("NAME\tREQUIRED\nA\tfalse\n"),
            "NAME                REQUIRED\nA                   false\n"
        );
        // A tab in a value is one more column, for that line and its neighbours.
        assert_eq!(
            tabwriter("N\tR\tD\nA\tfalse\tx\ty\nB\tfalse\tz\n"),
            "N                   R                   D\n\
             A                   false               x                   y\n\
             B                   false               z\n"
        );
        // That copy counts terminal columns: a wide character takes two.
        assert_eq!(table("A\tB\n\u{4e2d}\tx\n", 1, 1), "A B\n\u{4e2d} x\n");
        assert_eq!(
            columns_table("A\tB\n\u{4e2d}\tx\n", 1, 1),
            "A  B\n\u{4e2d} x\n"
        );
    }

    #[test]
    fn variables_are_found_with_their_defaults() {
        let v = extract(&Value::str("${A:-x} $B ${C:?err} ${D+alt} $$E ${F:-${G}}"));
        assert_eq!(v["A"].default, "x");
        assert!(v.contains_key("B"));
        assert!(v["C"].required);
        assert_eq!(v["D"].presence, "alt");
        assert!(!v.contains_key("E"));
        assert_eq!(v["F"].default, "${G}");
        assert!(v.contains_key("G"));
    }
}
