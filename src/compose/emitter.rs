//! The emitter of go.yaml.in/yaml/v4 (its port of libyaml's), with the settings of the
//! encoders the compose model is written with: events in, text out, laid out as
//! go-yaml lays it out, the style asked for a scalar kept or changed as go-yaml changes
//! it, and long scalars folded past the width of the line. No anchors, comments or
//! directives come here, no tag but the !!binary of text that is not UTF-8, and no
//! scalar is asked to be folded, so those parts of the emitter are left out.

/// The style a scalar is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarStyle {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    StreamStart,
    StreamEnd,
    DocumentStart,
    DocumentEnd,
    SequenceStart {
        flow: bool,
    },
    SequenceEnd,
    MappingStart {
        flow: bool,
    },
    MappingEnd,
    /// A scalar; `binary` for !!binary, base64 of bytes that are not UTF-8.
    Scalar {
        value: String,
        style: ScalarStyle,
        binary: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    StreamStart,
    FirstDocumentStart,
    DocumentStart,
    DocumentContent,
    DocumentEnd,
    FlowSequenceFirstItem,
    FlowSequenceItem,
    FlowMappingFirstKey,
    FlowMappingKey,
    FlowMappingSimpleValue,
    FlowMappingValue,
    BlockSequenceFirstItem,
    BlockSequenceItem,
    BlockMappingFirstKey,
    BlockMappingKey,
    BlockMappingSimpleValue,
    BlockMappingValue,
    End,
}

/// What go-yaml's analyzeScalar finds in a value.
#[derive(Debug, Default)]
struct Analysis {
    multiline: bool,
    flow_plain_allowed: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
}

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
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

fn is_space(b: &[u8], i: usize) -> bool {
    at(b, i) == b' '
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

fn is_blank_or_zero(b: &[u8], i: usize) -> bool {
    is_blank(b, i) || at(b, i) == 0 || is_line_break(b, i)
}

/// go-yaml's isBOM, which looks at the start of the value whatever the position: a
/// value that starts with a BOM has every character escaped in double quotes.
fn is_bom(b: &[u8], _i: usize) -> bool {
    at(b, 0) == 0xEF && at(b, 1) == 0xBB && at(b, 2) == 0xBF
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

pub struct Emitter {
    best_indent: usize,
    /// Past this column a long scalar goes on the next line.
    best_width: usize,
    out: Vec<u8>,
    events: Vec<Event>,
    head: usize,
    state: State,
    states: Vec<State>,
    indents: Vec<i64>,
    indent: i64,
    flow_level: usize,
    root_context: bool,
    simple_key_context: bool,
    mapping_context: bool,
    column: usize,
    whitespace: bool,
    indention: bool,
    value: Vec<u8>,
    analysis: Analysis,
    style: ScalarStyle,
}

/// The text go-yaml writes for these events, at `indent` spaces a level, folding long
/// scalars past `width` columns (None: never).
pub fn emit(events: Vec<Event>, indent: usize, width: Option<usize>) -> String {
    let best_indent = if (2..=9).contains(&indent) { indent } else { 2 };
    let mut emitter = Emitter {
        best_indent,
        best_width: match width {
            Some(w) if w <= best_indent * 2 => 80,
            Some(w) => w,
            None => i32::MAX as usize,
        },
        out: Vec::new(),
        events,
        head: 0,
        state: State::StreamStart,
        states: Vec::new(),
        indents: Vec::new(),
        indent: -1,
        flow_level: 0,
        root_context: false,
        simple_key_context: false,
        mapping_context: false,
        column: 0,
        whitespace: true,
        indention: true,
        value: Vec::new(),
        analysis: Analysis::default(),
        style: ScalarStyle::Plain,
    };
    while emitter.head < emitter.events.len() {
        let event = emitter.events[emitter.head].clone();
        emitter.analyze_event(&event);
        emitter.state_machine(&event);
        emitter.head += 1;
    }
    String::from_utf8(emitter.out).expect("UTF-8 in, UTF-8 out")
}

impl Emitter {
    fn pop_state(&mut self) {
        self.state = self.states.pop().unwrap_or(State::End);
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().unwrap_or(-1);
    }

    fn state_machine(&mut self, event: &Event) {
        match self.state {
            State::StreamStart => self.emit_stream_start(),
            State::FirstDocumentStart | State::DocumentStart => self.emit_document_start(event),
            State::DocumentContent => self.emit_document_content(event),
            State::DocumentEnd => self.emit_document_end(),
            State::FlowSequenceFirstItem => self.emit_flow_sequence_item(event, true),
            State::FlowSequenceItem => self.emit_flow_sequence_item(event, false),
            State::FlowMappingFirstKey => self.emit_flow_mapping_key(event, true),
            State::FlowMappingKey => self.emit_flow_mapping_key(event, false),
            State::FlowMappingSimpleValue => self.emit_flow_mapping_value(event, true),
            State::FlowMappingValue => self.emit_flow_mapping_value(event, false),
            State::BlockSequenceFirstItem => self.emit_block_sequence_item(event, true),
            State::BlockSequenceItem => self.emit_block_sequence_item(event, false),
            State::BlockMappingFirstKey => self.emit_block_mapping_key(event, true),
            State::BlockMappingKey => self.emit_block_mapping_key(event, false),
            State::BlockMappingSimpleValue => self.emit_block_mapping_value(event, true),
            State::BlockMappingValue => self.emit_block_mapping_value(event, false),
            State::End => {}
        }
    }

    fn emit_stream_start(&mut self) {
        self.indent = -1;
        self.column = 0;
        self.whitespace = true;
        self.indention = true;
        self.state = State::FirstDocumentStart;
    }

    fn emit_document_start(&mut self, event: &Event) {
        match event {
            Event::DocumentStart => {
                // Only a document after the first starts with `---`.
                if self.state == State::DocumentStart {
                    self.write_indent();
                    self.write_indicator(b"---", true, false, false);
                    self.write_indent();
                }
                self.state = State::DocumentContent;
            }
            _ => self.state = State::End,
        }
    }

    fn emit_document_content(&mut self, event: &Event) {
        self.states.push(State::DocumentEnd);
        self.emit_node(event, true, false, false, false);
    }

    fn emit_document_end(&mut self) {
        self.write_indent();
        self.state = State::DocumentStart;
    }

    fn emit_flow_sequence_item(&mut self, event: &Event, first: bool) {
        if first {
            self.write_indicator(b"[", true, true, false);
            self.increase_indent(true, false, false);
            self.flow_level += 1;
        }
        if *event == Event::SequenceEnd {
            self.flow_level -= 1;
            self.pop_indent();
            if self.column == 0 {
                self.write_indent();
            }
            self.write_indicator(b"]", false, false, false);
            self.pop_state();
            return;
        }
        if !first {
            self.write_indicator(b",", false, false, false);
        }
        if self.column == 0 || self.column > self.best_width {
            self.write_indent();
        }
        self.states.push(State::FlowSequenceItem);
        self.emit_node(event, false, true, false, false);
    }

    fn emit_flow_mapping_key(&mut self, event: &Event, first: bool) {
        if first {
            self.write_indicator(b"{", true, true, false);
            self.increase_indent(true, false, false);
            self.flow_level += 1;
        }
        if *event == Event::MappingEnd {
            self.flow_level -= 1;
            self.pop_indent();
            self.write_indicator(b"}", false, false, false);
            self.pop_state();
            return;
        }
        if !first {
            self.write_indicator(b",", false, false, false);
        }
        if self.column == 0 || self.column > self.best_width {
            self.write_indent();
        }
        if self.check_simple_key() {
            self.states.push(State::FlowMappingSimpleValue);
            self.emit_node(event, false, false, true, true);
            return;
        }
        self.write_indicator(b"?", true, false, false);
        self.states.push(State::FlowMappingValue);
        self.emit_node(event, false, false, true, false);
    }

    fn emit_flow_mapping_value(&mut self, event: &Event, simple: bool) {
        if simple {
            self.write_indicator(b":", false, false, false);
        } else {
            if self.column > self.best_width {
                self.write_indent();
            }
            self.write_indicator(b":", true, false, false);
        }
        self.states.push(State::FlowMappingKey);
        self.emit_node(event, false, false, true, false);
    }

    fn emit_block_sequence_item(&mut self, event: &Event, first: bool) {
        if first {
            // Without go-yaml's compact sequence indentation, which compose-go's encoder
            // leaves off.
            self.increase_indent(false, false, false);
        }
        if *event == Event::SequenceEnd {
            self.pop_indent();
            self.pop_state();
            return;
        }
        self.write_indent();
        self.write_indicator(b"-", true, false, true);
        self.states.push(State::BlockSequenceItem);
        self.emit_node(event, false, true, false, false);
    }

    fn emit_block_mapping_key(&mut self, event: &Event, first: bool) {
        if first {
            self.increase_indent(false, false, false);
        }
        if *event == Event::MappingEnd {
            self.pop_indent();
            self.pop_state();
            return;
        }
        self.write_indent();
        if self.check_simple_key() {
            self.states.push(State::BlockMappingSimpleValue);
            self.emit_node(event, false, false, true, true);
            return;
        }
        self.write_indicator(b"?", true, false, true);
        self.states.push(State::BlockMappingValue);
        self.emit_node(event, false, false, true, false);
    }

    fn emit_block_mapping_value(&mut self, event: &Event, simple: bool) {
        if simple {
            self.write_indicator(b":", false, false, false);
        } else {
            self.write_indent();
            self.write_indicator(b":", true, false, true);
        }
        self.states.push(State::BlockMappingKey);
        self.emit_node(event, false, false, true, false);
    }

    fn emit_node(
        &mut self,
        event: &Event,
        root: bool,
        _sequence: bool,
        mapping: bool,
        simple_key: bool,
    ) {
        self.root_context = root;
        self.mapping_context = mapping;
        self.simple_key_context = simple_key;
        match event {
            Event::Scalar { style, binary, .. } => self.emit_scalar(*style, *binary),
            Event::SequenceStart { flow } => {
                if self.flow_level > 0 || *flow || self.check_empty(Event::SequenceEnd) {
                    self.state = State::FlowSequenceFirstItem;
                } else {
                    self.state = State::BlockSequenceFirstItem;
                }
            }
            Event::MappingStart { flow } => {
                if self.flow_level > 0 || *flow || self.check_empty(Event::MappingEnd) {
                    self.state = State::FlowMappingFirstKey;
                } else {
                    self.state = State::BlockMappingFirstKey;
                }
            }
            _ => {}
        }
    }

    fn emit_scalar(&mut self, style: ScalarStyle, binary: bool) {
        self.select_scalar_style(style);
        if binary {
            // processTag, with the handle of tag:yaml.org,2002:.
            if !self.whitespace {
                self.put(b' ');
            }
            self.write_all(b"!!binary");
            self.whitespace = false;
            self.indention = false;
        }
        self.increase_indent(true, false, false);
        self.process_scalar();
        self.pop_indent();
        self.pop_state();
    }

    /// A collection opened by the current event and closed by the next.
    fn check_empty(&self, end: Event) -> bool {
        self.events.get(self.head + 1) == Some(&end)
    }

    fn check_simple_key(&self) -> bool {
        match &self.events[self.head] {
            Event::Scalar { binary, .. } => {
                let tag = if *binary { "!!binary".len() } else { 0 };
                !self.analysis.multiline && tag + self.value.len() <= 128
            }
            Event::SequenceStart { .. } => self.check_empty(Event::SequenceEnd),
            Event::MappingStart { .. } => self.check_empty(Event::MappingEnd),
            _ => false,
        }
    }

    fn analyze_event(&mut self, event: &Event) {
        self.value.clear();
        if let Event::Scalar { value, .. } = event {
            self.analyze_scalar(value.as_bytes().to_vec());
        }
    }

    fn analyze_scalar(&mut self, value: Vec<u8>) {
        self.value = value;
        let value = &self.value;
        if value.is_empty() {
            self.analysis = Analysis {
                multiline: false,
                flow_plain_allowed: false,
                block_plain_allowed: true,
                single_quoted_allowed: true,
                block_allowed: false,
            };
            return;
        }
        let (mut block_indicators, mut flow_indicators) = (false, false);
        let (mut line_breaks, mut special_characters, mut tab_characters) = (false, false, false);
        let (mut leading_space, mut leading_break) = (false, false);
        let (mut trailing_space, mut trailing_break) = (false, false);
        let (mut break_space, mut space_break) = (false, false);
        let (mut previous_space, mut previous_break) = (false, false);
        if value.starts_with(b"---") || value.starts_with(b"...") {
            block_indicators = true;
            flow_indicators = true;
        }
        let mut preceded_by_whitespace = true;
        let mut i = 0;
        while i < value.len() {
            let w = width(value[i]).max(1);
            let followed_by_whitespace = i + w >= value.len() || is_blank(value, i + w);
            if i == 0 {
                match value[i] {
                    b'#' | b',' | b'[' | b']' | b'{' | b'}' | b'&' | b'*' | b'!' | b'|' | b'>'
                    | b'\'' | b'"' | b'%' | b'@' | b'`' => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    b'?' | b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'-' if followed_by_whitespace => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    _ => {}
                }
            } else {
                match value[i] {
                    b',' | b'?' | b'[' | b']' | b'{' | b'}' => flow_indicators = true,
                    b':' => {
                        flow_indicators = true;
                        if followed_by_whitespace {
                            block_indicators = true;
                        }
                    }
                    b'#' if preceded_by_whitespace => {
                        flow_indicators = true;
                        block_indicators = true;
                    }
                    _ => {}
                }
            }
            if value[i] == b'\t' {
                tab_characters = true;
            } else if !is_printable(value, i) {
                special_characters = true;
            }
            if is_space(value, i) {
                if i == 0 {
                    leading_space = true;
                }
                if i + w == value.len() {
                    trailing_space = true;
                }
                if previous_break {
                    break_space = true;
                }
                previous_space = true;
                previous_break = false;
            } else if is_line_break(value, i) {
                line_breaks = true;
                if i == 0 {
                    leading_break = true;
                }
                if i + w == value.len() {
                    trailing_break = true;
                }
                if previous_space {
                    space_break = true;
                }
                previous_space = false;
                previous_break = true;
            } else {
                previous_space = false;
                previous_break = false;
            }
            preceded_by_whitespace = is_blank_or_zero(value, i);
            i += w;
        }
        let mut a = Analysis {
            multiline: line_breaks,
            flow_plain_allowed: true,
            block_plain_allowed: true,
            single_quoted_allowed: true,
            block_allowed: true,
        };
        if leading_space || leading_break || trailing_space || trailing_break {
            a.flow_plain_allowed = false;
            a.block_plain_allowed = false;
        }
        if trailing_space {
            a.block_allowed = false;
        }
        if break_space {
            a.flow_plain_allowed = false;
            a.block_plain_allowed = false;
            a.single_quoted_allowed = false;
        }
        if space_break || tab_characters || special_characters {
            a.flow_plain_allowed = false;
            a.block_plain_allowed = false;
            a.single_quoted_allowed = false;
        }
        if space_break || special_characters {
            a.block_allowed = false;
        }
        if line_breaks {
            a.flow_plain_allowed = false;
            a.block_plain_allowed = false;
        }
        if flow_indicators {
            a.flow_plain_allowed = false;
        }
        if block_indicators {
            a.block_plain_allowed = false;
        }
        self.analysis = a;
    }

    fn select_scalar_style(&mut self, asked: ScalarStyle) {
        let mut style = asked;
        if self.simple_key_context && self.analysis.multiline {
            style = ScalarStyle::DoubleQuoted;
        }
        // The quotes compose-go's encoder falls back to when a plain scalar cannot be.
        let required = ScalarStyle::SingleQuoted;
        if style == ScalarStyle::Plain {
            if (self.flow_level > 0 && !self.analysis.flow_plain_allowed)
                || (self.flow_level == 0 && !self.analysis.block_plain_allowed)
            {
                style = required;
            }
            if self.value.is_empty() && (self.flow_level > 0 || self.simple_key_context) {
                style = required;
            }
        }
        if style == ScalarStyle::SingleQuoted && !self.analysis.single_quoted_allowed {
            style = ScalarStyle::DoubleQuoted;
        }
        if style == ScalarStyle::Literal
            && (!self.analysis.block_allowed || self.flow_level > 0 || self.simple_key_context)
        {
            style = ScalarStyle::DoubleQuoted;
        }
        self.style = style;
    }

    fn process_scalar(&mut self) {
        let value = std::mem::take(&mut self.value);
        let allow_breaks = !self.simple_key_context;
        match self.style {
            ScalarStyle::Plain => self.write_plain_scalar(&value, allow_breaks),
            ScalarStyle::SingleQuoted => self.write_single_quoted_scalar(&value, allow_breaks),
            ScalarStyle::DoubleQuoted => self.write_double_quoted_scalar(&value, allow_breaks),
            ScalarStyle::Literal => self.write_literal_scalar(&value),
        }
        self.value = value;
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool, compact_seq: bool) {
        self.indents.push(self.indent);
        let best = self.best_indent as i64;
        if self.indent < 0 {
            self.indent = if flow { best } else { 0 };
        } else if !indentless {
            if self.states.last() == Some(&State::BlockSequenceItem) {
                self.indent += 2;
            } else {
                self.indent = best * ((self.indent + best) / best);
                if compact_seq {
                    self.indent -= 2;
                }
            }
        }
    }

    // --- Writing --------------------------------------------------------------------

    fn put(&mut self, c: u8) {
        self.out.push(c);
        self.column += 1;
    }

    fn put_line_break(&mut self) {
        self.out.push(b'\n');
        self.column = 0;
        self.indention = true;
    }

    /// The character at `i` of `s`, as one column.
    fn write(&mut self, s: &[u8], i: &mut usize) {
        let w = width(s[*i]).max(1);
        self.out.extend_from_slice(&s[*i..(*i + w).min(s.len())]);
        self.column += 1;
        *i += w;
    }

    fn write_all(&mut self, s: &[u8]) {
        let mut i = 0;
        while i < s.len() {
            self.write(s, &mut i);
        }
    }

    fn write_line_break(&mut self, s: &[u8], i: &mut usize) {
        if s[*i] == b'\n' {
            self.put_line_break();
            *i += 1;
        } else {
            self.write(s, i);
            self.column = 0;
            self.indention = true;
        }
    }

    fn write_indent(&mut self) {
        let indent = self.indent.max(0) as usize;
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_line_break();
        }
        while self.column < indent {
            self.put(b' ');
        }
        self.whitespace = true;
    }

    fn write_indicator(
        &mut self,
        indicator: &[u8],
        need_whitespace: bool,
        is_whitespace: bool,
        is_indention: bool,
    ) {
        if need_whitespace && !self.whitespace {
            self.put(b' ');
        }
        self.write_all(indicator);
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
    }

    fn write_plain_scalar(&mut self, value: &[u8], allow_breaks: bool) {
        if !value.is_empty() && !self.whitespace {
            self.put(b' ');
        }
        let (mut spaces, mut breaks) = (false, false);
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && !is_space(value, i + 1)
                {
                    self.write_indent();
                    i += width(value[i]).max(1);
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else if is_line_break(value, i) {
                if !breaks && value[i] == b'\n' {
                    self.put_line_break();
                }
                self.write_line_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut i);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        if !value.is_empty() {
            self.whitespace = false;
        }
        self.indention = false;
    }

    fn write_single_quoted_scalar(&mut self, value: &[u8], allow_breaks: bool) {
        self.write_indicator(b"'", true, false, false);
        let (mut spaces, mut breaks) = (false, false);
        let mut i = 0;
        while i < value.len() {
            if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len() - 1
                    && !is_space(value, i + 1)
                {
                    self.write_indent();
                    i += width(value[i]).max(1);
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else if is_line_break(value, i) {
                if !breaks && value[i] == b'\n' {
                    self.put_line_break();
                }
                self.write_line_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                if value[i] == b'\'' {
                    self.put(b'\'');
                }
                self.write(value, &mut i);
                self.indention = false;
                spaces = false;
                breaks = false;
            }
        }
        self.write_indicator(b"'", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_double_quoted_scalar(&mut self, value: &[u8], allow_breaks: bool) {
        let mut spaces = false;
        self.write_indicator(b"\"", true, false, false);
        let mut i = 0;
        while i < value.len() {
            if !is_printable(value, i)
                || is_bom(value, i)
                || is_line_break(value, i)
                || value[i] == b'"'
                || value[i] == b'\\'
            {
                let octet = value[i];
                let w = width(octet).max(1);
                let mut v: u32 = match w {
                    1 => (octet & 0x7F) as u32,
                    2 => (octet & 0x1F) as u32,
                    3 => (octet & 0x0F) as u32,
                    _ => (octet & 0x07) as u32,
                };
                for k in 1..w {
                    v = (v << 6) + (at(value, i + k) & 0x3F) as u32;
                }
                i += w;
                self.put(b'\\');
                let short = match v {
                    0x00 => Some(b'0'),
                    0x07 => Some(b'a'),
                    0x08 => Some(b'b'),
                    0x09 => Some(b't'),
                    0x0A => Some(b'n'),
                    0x0B => Some(b'v'),
                    0x0C => Some(b'f'),
                    0x0D => Some(b'r'),
                    0x1B => Some(b'e'),
                    0x22 => Some(b'"'),
                    0x5C => Some(b'\\'),
                    0x85 => Some(b'N'),
                    0xA0 => Some(b'_'),
                    0x2028 => Some(b'L'),
                    0x2029 => Some(b'P'),
                    _ => None,
                };
                match short {
                    Some(c) => self.put(c),
                    None => {
                        let (letter, digits) = if v <= 0xFF {
                            (b'x', 2)
                        } else if v <= 0xFFFF {
                            (b'u', 4)
                        } else {
                            (b'U', 8)
                        };
                        self.put(letter);
                        for k in (0..digits).rev() {
                            let digit = ((v >> (k * 4)) & 0x0F) as u8;
                            self.put(if digit < 10 {
                                digit + b'0'
                            } else {
                                digit + b'A' - 10
                            });
                        }
                    }
                }
                spaces = false;
            } else if is_space(value, i) {
                if allow_breaks
                    && !spaces
                    && self.column > self.best_width
                    && i > 0
                    && i < value.len() - 1
                {
                    self.write_indent();
                    if is_space(value, i + 1) {
                        self.put(b'\\');
                    }
                    i += width(value[i]).max(1);
                } else {
                    self.write(value, &mut i);
                }
                spaces = true;
            } else {
                self.write(value, &mut i);
                spaces = false;
            }
        }
        self.write_indicator(b"\"", false, false, false);
        self.whitespace = false;
        self.indention = false;
    }

    fn write_block_scalar_hints(&mut self, value: &[u8]) {
        if is_space(value, 0) {
            let hint = [b'0' + self.best_indent as u8];
            self.write_indicator(&hint, false, false, false);
        }
        let mut chomp = None;
        if value.is_empty() {
            chomp = Some(b'-');
        } else {
            let mut i = value.len() - 1;
            while value[i] & 0xC0 == 0x80 {
                i -= 1;
            }
            if !is_line_break(value, i) {
                chomp = Some(b'-');
            } else if i == 0 {
                chomp = Some(b'+');
            } else {
                i -= 1;
                while value[i] & 0xC0 == 0x80 {
                    i -= 1;
                }
                if is_line_break(value, i) {
                    chomp = Some(b'+');
                }
            }
        }
        if let Some(c) = chomp {
            self.write_indicator(&[c], false, false, false);
        }
    }

    fn write_literal_scalar(&mut self, value: &[u8]) {
        self.write_indicator(b"|", true, false, false);
        self.write_block_scalar_hints(value);
        self.put_line_break();
        self.whitespace = true;
        let mut breaks = true;
        let mut i = 0;
        while i < value.len() {
            if is_line_break(value, i) {
                self.write_line_break(value, &mut i);
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.write(value, &mut i);
                self.indention = false;
                breaks = false;
            }
        }
    }
}
