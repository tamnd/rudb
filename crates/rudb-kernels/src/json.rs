//! `JSON`: reading a document the way the pin's yyjson does, writing one back the way it does, the
//! paths that pick a value out of one, and the functions built on those.
//!
//! The reader takes what the pin's reader takes, which is RFC 8259 with three additions: a trailing
//! comma in an array or an object, `NaN` and `Infinity` in any case, and a number too big for 64
//! bits, which is kept as the text it was written as. It refuses what the pin refuses at the byte
//! the pin names, with the pin's words, since `'{"a":'::JSON` is refused with both. A document that
//! stops early is refused at its end as `unexpected end of data` rather than wherever the parse got
//! to, which is a rule the pin's reader applies after the fact and so this one does too.
//!
//! A document is held flat, every value in one vector and a container holding the positions of
//! what is in it. Nothing here recurses on the depth of a document, so `[[[[...]]]]` a million deep
//! is read, written, walked, merged and matched without running out of stack.

use rudb_common::{Error, Field, LogicalType, Result, SessionTimeZone, Value};
use rudb_vector::Vector;

use crate::cast::cast_value;

pub mod scan;

/// One value in a document.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Null,
    Bool(bool),
    /// A whole number that is not negative and fits in 64 bits.
    Unsigned(u64),
    /// A negative whole number that fits in 64 bits, and `-0`.
    Signed(i64),
    /// A number with a fraction or an exponent, and the digits as written when there are more than
    /// fifteen of them, since a double does not hold that many and a cast to a decimal reads them.
    Real(f64, Option<Box<str>>),
    /// A number held as it was written, which is one too big for 64 bits, `NaN` and `Infinity`.
    Raw(String),
    Str(String),
    /// The positions of the elements, in order.
    Array(Vec<usize>),
    /// The keys and the positions of their values, in order and with any repeated key kept.
    Object(Vec<(String, usize)>),
}

/// A document, its root at position zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    nodes: Vec<Node>,
}

/// Where a document stopped parsing and why, in the pin's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed {
    /// The byte the pin names.
    pub at: usize,
    /// What it says went wrong there.
    pub message: &'static str,
}

impl Malformed {
    /// The pin's sentence for this failure in this input.
    #[must_use]
    pub fn describe(&self, input: &str) -> String {
        let shown = if input.len() > 50 {
            let mut end = 47;
            while !input.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &input[..end])
        } else {
            input.to_string()
        };
        format!(
            "Malformed JSON at byte {} of input: {}.  Input: \"{}\"",
            self.at,
            self.message,
            shown.replace('\r', "\\r")
        )
    }
}

/// The family of an error, which decides whether the end of the input explains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Character,
    Number,
    Literal,
    Text,
    Content,
    Empty,
}

type Failed = (usize, Kind, &'static str);

struct Reader<'a> {
    bytes: &'a [u8],
    cur: usize,
}

/// One container being read, and the key its next value goes under when it is an object.
struct Frame {
    at: usize,
    key: Option<String>,
}

const fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

impl Reader<'_> {
    /// The byte at a position, and zero past the end, which is how the pin's padded buffer reads.
    fn at(&self, index: usize) -> u8 {
        self.bytes.get(index).copied().unwrap_or(0)
    }

    fn skip(&mut self) {
        while space(self.at(self.cur)) && self.cur < self.bytes.len() {
            self.cur += 1;
        }
    }

    fn rest_is(&self, from: usize, word: &[u8], fold: bool) -> bool {
        let rest = &self.bytes[from.min(self.bytes.len())..];
        rest.len() >= word.len()
            && if fold {
                rest[..word.len()].eq_ignore_ascii_case(word)
            } else {
                &rest[..word.len()] == word
            }
    }

    /// `inf`, `infinity` or `nan` in any case, with a minus sign in front if one was written.
    fn inf_or_nan(&mut self) -> Option<Node> {
        let start = self.cur;
        let mut at = start;
        if self.at(at) == b'-' {
            at += 1;
        }
        let length = if self.rest_is(at, b"infinity", true) {
            8
        } else if self.rest_is(at, b"inf", true) || self.rest_is(at, b"nan", true) {
            3
        } else {
            return None;
        };
        self.cur = at + length;
        Some(Node::Raw(String::from_utf8_lossy(&self.bytes[start..self.cur]).into_owned()))
    }

    fn literal(&mut self, word: &[u8], node: Node) -> Option<Node> {
        if self.rest_is(self.cur, word, false) {
            self.cur += word.len();
            Some(node)
        } else {
            None
        }
    }

    /// A value that is not a container, at a byte that is not `{` or `[`.
    fn scalar(&mut self) -> std::result::Result<Node, Failed> {
        let byte = self.at(self.cur);
        if byte == b'-' || byte.is_ascii_digit() {
            return self.number();
        }
        match byte {
            b'"' => self.string().map(Node::Str),
            b't' => self.literal(b"true", Node::Bool(true)).ok_or((
                self.cur,
                Kind::Literal,
                "invalid literal",
            )),
            b'f' => self.literal(b"false", Node::Bool(false)).ok_or((
                self.cur,
                Kind::Literal,
                "invalid literal",
            )),
            b'n' => self.literal(b"null", Node::Null).or_else(|| self.inf_or_nan()).ok_or((
                self.cur,
                Kind::Literal,
                "invalid literal",
            )),
            _ => self.inf_or_nan().ok_or((self.cur, Kind::Character, "unexpected character")),
        }
    }

    fn number(&mut self) -> std::result::Result<Node, Failed> {
        let start = self.cur;
        let negative = self.at(self.cur) == b'-';
        if negative {
            self.cur += 1;
        }
        if !self.at(self.cur).is_ascii_digit() {
            self.cur = start;
            if let Some(node) = self.inf_or_nan() {
                return Ok(node);
            }
            return Err((start + 1, Kind::Number, "no digit after minus sign"));
        }
        if self.at(self.cur) == b'0' && self.at(self.cur + 1).is_ascii_digit() {
            return Err((self.cur, Kind::Number, "number with leading zero is not allowed"));
        }
        while self.at(self.cur).is_ascii_digit() {
            self.cur += 1;
        }
        let mut real = false;
        if self.at(self.cur) == b'.' {
            real = true;
            self.cur += 1;
            if !self.at(self.cur).is_ascii_digit() {
                return Err((self.cur, Kind::Number, "no digit after decimal point"));
            }
            while self.at(self.cur).is_ascii_digit() {
                self.cur += 1;
            }
        }
        if matches!(self.at(self.cur), b'e' | b'E') {
            real = true;
            self.cur += 1;
            if matches!(self.at(self.cur), b'+' | b'-') {
                self.cur += 1;
            }
            if !self.at(self.cur).is_ascii_digit() {
                return Err((self.cur, Kind::Number, "no digit after exponent sign"));
            }
            while self.at(self.cur).is_ascii_digit() {
                self.cur += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.cur]).unwrap_or_default();
        let raw = || Node::Raw(text.to_string());
        if real {
            return Ok(match text.parse::<f64>() {
                Ok(value) if value.is_finite() => Node::Real(value, precise(text)),
                _ => raw(),
            });
        }
        let digits = if negative { &text[1..] } else { text };
        Ok(match digits.parse::<u64>() {
            Ok(magnitude) if !negative => Node::Unsigned(magnitude),
            Ok(magnitude) if magnitude <= 1 << 63 => {
                Node::Signed(0_i64.wrapping_sub_unsigned(magnitude))
            }
            _ => raw(),
        })
    }

    fn hex(&self, from: usize) -> Option<u32> {
        let mut value = 0;
        for index in from..from + 4 {
            value = value * 16 + char::from(self.at(index)).to_digit(16)?;
        }
        Some(value)
    }

    /// A string, from its opening quote, with its escapes undone.
    fn string(&mut self) -> std::result::Result<String, Failed> {
        let mut out = Vec::new();
        let mut cur = self.cur + 1;
        loop {
            let byte = self.at(cur);
            match byte {
                b'"' => break,
                b'\\' => {
                    let escaped = match self.at(cur + 1) {
                        b'"' => Some(b'"'),
                        b'\\' => Some(b'\\'),
                        b'/' => Some(b'/'),
                        b'b' => Some(0x08),
                        b'f' => Some(0x0c),
                        b'n' => Some(b'\n'),
                        b'r' => Some(b'\r'),
                        b't' => Some(b'\t'),
                        b'u' => None,
                        _ => {
                            return Err((
                                cur + 1,
                                Kind::Text,
                                "invalid escaped character in string",
                            ));
                        }
                    };
                    if let Some(escaped) = escaped {
                        out.push(escaped);
                        cur += 2;
                        continue;
                    }
                    let bad = (cur, Kind::Text, "invalid escaped sequence in string");
                    let high = self.hex(cur + 2).ok_or(bad)?;
                    let code = if (0xdc00..0xe000).contains(&high) {
                        return Err((cur, Kind::Text, "invalid high surrogate in string"));
                    } else if (0xd800..0xdc00).contains(&high) {
                        let low_at = cur + 6;
                        if self.at(low_at) != b'\\' || self.at(low_at + 1) != b'u' {
                            return Err((low_at, Kind::Text, "no low surrogate in string"));
                        }
                        let bad = (low_at, Kind::Text, "invalid escaped sequence in string");
                        let low = self.hex(low_at + 2).ok_or(bad)?;
                        if !(0xdc00..0xe000).contains(&low) {
                            return Err((low_at, Kind::Text, "invalid low surrogate in string"));
                        }
                        cur = low_at + 6;
                        0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                    } else {
                        cur += 6;
                        high
                    };
                    let mut buffer = [0; 4];
                    let character = char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER);
                    out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                }
                _ if byte < 0x20 => {
                    return Err((cur, Kind::Text, "unexpected control character in string"));
                }
                _ => {
                    out.push(byte);
                    cur += 1;
                }
            }
        }
        self.cur = cur + 1;
        Ok(String::from_utf8(out)
            .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()))
    }

    /// A container, from its opening bracket, read with a stack of its own rather than the call
    /// stack, so the depth of a document is a size and not a crash.
    fn container(&mut self, nodes: &mut Vec<Node>) -> std::result::Result<(), Failed> {
        let mut stack: Vec<Frame> = Vec::new();
        self.open(nodes, &mut stack);
        loop {
            self.skip();
            let byte = self.at(self.cur);
            let top = stack.last_mut().map(|frame| frame.at).unwrap_or_default();
            let closed = if matches!(nodes[top], Node::Array(_)) {
                if byte == b']' {
                    self.cur += 1;
                    true
                } else if byte == b'{' || byte == b'[' {
                    self.open(nodes, &mut stack);
                    continue;
                } else {
                    let node = self.element()?;
                    self.attach(nodes, &mut stack, node);
                    false
                }
            } else if byte == b'}' {
                self.cur += 1;
                true
            } else if byte == b'"' {
                let key = self.string()?;
                self.skip();
                if self.at(self.cur) != b':' {
                    return Err((self.cur, Kind::Character, "unexpected character"));
                }
                self.cur += 1;
                self.skip();
                if let Some(frame) = stack.last_mut() {
                    frame.key = Some(key);
                }
                let byte = self.at(self.cur);
                if byte == b'{' || byte == b'[' {
                    self.open(nodes, &mut stack);
                    continue;
                }
                let node = self.element()?;
                self.attach(nodes, &mut stack, node);
                false
            } else {
                return Err((self.cur, Kind::Character, "unexpected character"));
            };
            if closed {
                stack.pop();
                if stack.is_empty() {
                    return Ok(());
                }
                let child = top;
                self.link(nodes, &mut stack, child);
            }
            // After a value, a comma goes on to the next one and a bracket closes the container,
            // which may close every container above it in turn.
            loop {
                self.skip();
                let byte = self.at(self.cur);
                let top = stack.last().map(|frame| frame.at).unwrap_or_default();
                let close = if matches!(nodes[top], Node::Array(_)) { b']' } else { b'}' };
                if byte == b',' {
                    self.cur += 1;
                    break;
                }
                if byte != close {
                    return Err((self.cur, Kind::Character, "unexpected character"));
                }
                self.cur += 1;
                stack.pop();
                if stack.is_empty() {
                    return Ok(());
                }
                self.link(nodes, &mut stack, top);
            }
        }
    }

    fn open(&mut self, nodes: &mut Vec<Node>, stack: &mut Vec<Frame>) {
        let node = if self.at(self.cur) == b'[' {
            Node::Array(Vec::new())
        } else {
            Node::Object(Vec::new())
        };
        let at = nodes.len();
        nodes.push(node);
        if !stack.is_empty() {
            self.link(nodes, stack, at);
        }
        stack.push(Frame { at, key: None });
        self.cur += 1;
    }

    /// A value inside a container that is not itself a container.
    fn element(&mut self) -> std::result::Result<Node, Failed> {
        let byte = self.at(self.cur);
        if byte == b'-'
            || byte.is_ascii_digit()
            || matches!(byte, b'"' | b't' | b'f' | b'n' | b'i' | b'I' | b'N')
        {
            return self.scalar();
        }
        Err((self.cur, Kind::Character, "unexpected character"))
    }

    fn attach(&self, nodes: &mut Vec<Node>, stack: &mut [Frame], node: Node) {
        let at = nodes.len();
        nodes.push(node);
        self.link(nodes, stack, at);
    }

    /// Puts the value at a position into the container on top of the stack.
    fn link(&self, nodes: &mut [Node], stack: &mut [Frame], child: usize) {
        let Some(frame) = stack.last_mut() else { return };
        match &mut nodes[frame.at] {
            Node::Array(children) => {
                if children.last() != Some(&child) {
                    children.push(child);
                }
            }
            Node::Object(children) => {
                if let Some(key) = frame.key.take() {
                    children.push((key, child));
                }
            }
            _ => {}
        }
    }

    /// Whether an error is the input running out, which the pin reports at its end.
    fn truncated(&self, at: usize, kind: Kind) -> bool {
        let length = self.bytes.len();
        if at >= length {
            return true;
        }
        let prefix = |from: usize, word: &[u8], fold: bool| {
            let rest = &self.bytes[from.min(length)..];
            !rest.is_empty()
                && rest.len() < word.len()
                && if fold {
                    rest.eq_ignore_ascii_case(&word[..rest.len()])
                } else {
                    rest == &word[..rest.len()]
                }
        };
        if kind == Kind::Literal
            && (prefix(at, b"true", false)
                || prefix(at, b"false", false)
                || prefix(at, b"null", false))
        {
            return true;
        }
        if matches!(kind, Kind::Character | Kind::Number | Kind::Literal) {
            let from = if self.bytes[at] == b'-' { at + 1 } else { at };
            if prefix(from, b"infinity", true) || prefix(from, b"nan", true) {
                return true;
            }
        }
        if kind == Kind::Content && at >= 3 && prefix(at - 3, b"infinity", true) {
            return true;
        }
        if kind == Kind::Text && self.bytes[at] == b'\\' {
            let rest = &self.bytes[at..];
            if rest.len() == 1 {
                return true;
            }
            if rest.len() <= 5 && rest[1] == b'u' && rest[2..].iter().all(u8::is_ascii_hexdigit) {
                return true;
            }
        }
        false
    }
}

/// Reads a document.
///
/// # Errors
///
/// Where and why the pin's reader stops on the same text.
pub fn read(text: &str) -> std::result::Result<Document, Malformed> {
    let mut nodes = Vec::new();
    read_into(text, &mut nodes, false)?;
    Ok(Document { nodes })
}

/// Reads one document onto the end of `nodes`, answering where its root went and how many bytes of
/// the text it took. With `stop` the read ends at the end of the document, as the pin's
/// `YYJSON_READ_STOP_WHEN_DONE` does, and whatever follows is left for the caller to judge.
/// Nothing is added when the text is malformed.
fn read_into(
    text: &str,
    nodes: &mut Vec<Node>,
    stop: bool,
) -> std::result::Result<(usize, usize), Malformed> {
    let mut reader = Reader { bytes: text.as_bytes(), cur: 0 };
    let base = nodes.len();
    let outcome = (|| {
        if text.is_empty() {
            return Err((0, Kind::Empty, "input length is 0"));
        }
        reader.skip();
        if reader.cur >= reader.bytes.len() {
            return Err((0, Kind::Empty, "input data is empty"));
        }
        match reader.at(reader.cur) {
            b'{' | b'[' => reader.container(nodes)?,
            _ => {
                let node = reader.scalar()?;
                nodes.push(node);
            }
        }
        if stop {
            return Ok(());
        }
        reader.skip();
        if reader.cur < reader.bytes.len() {
            return Err((reader.cur, Kind::Content, "unexpected content after document"));
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok((base, reader.cur)),
        Err((at, kind, message)) => {
            nodes.truncate(base);
            if kind != Kind::Empty && reader.truncated(at, kind) {
                Err(Malformed { at: text.len(), message: "unexpected end of data" })
            } else {
                Err(Malformed { at, message })
            }
        }
    }
}

/// Reads a document a function was handed, refusing a malformed one as the functions do.
fn document(text: &str) -> Result<Document> {
    read(text).map_err(|malformed| Error::invalid_input(malformed.describe(text)))
}

/// Whether text is a document, which is what `json_valid` answers.
#[must_use]
pub fn valid(text: &str) -> bool {
    read(text).is_ok()
}

/// Checks that text is a document for a cast to `JSON`, which keeps the text as it was.
///
/// # Errors
///
/// A conversion error naming where the text stops being one, unless the cast is a `TRY_CAST`, which
/// answers null instead.
pub fn checked(text: &str, try_cast: bool) -> Result<Value> {
    match read(text) {
        Ok(_) => Ok(Value::Varchar(text.to_string())),
        Err(_) if try_cast => Ok(Value::Null),
        Err(malformed) => Err(Error::conversion(malformed.describe(text))),
    }
}

impl Document {
    /// The document written out with no spaces, which is how the pin writes one back.
    #[must_use]
    pub fn minified(&self) -> String {
        let mut out = String::new();
        self.write(0, &mut out);
        out
    }

    /// Writes the value at a position, with a stack of its own for the containers.
    fn write(&self, root: usize, out: &mut String) {
        self.write_styled(root, Style::default(), out);
    }

    /// Writes the value at a position in a style, which is how `json_pretty` and
    /// `json_strip_nulls` share the one writer.
    fn write_styled(&self, root: usize, style: Style, out: &mut String) {
        // Each entry is a container, how many of its children have been looked at and how many of
        // those were written, which differ once a null member is left out.
        let mut stack: Vec<(usize, usize, usize)> = Vec::new();
        let mut next = Some(root);
        loop {
            if let Some(at) = next.take() {
                match &self.nodes[at] {
                    Node::Array(_) => {
                        out.push('[');
                        stack.push((at, 0, 0));
                    }
                    Node::Object(_) => {
                        out.push('{');
                        stack.push((at, 0, 0));
                    }
                    node => scalar_text(node, out),
                }
            }
            let depth = stack.len();
            let Some((at, done, wrote)) = stack.last_mut() else { return };
            let (count, child) = match &self.nodes[*at] {
                Node::Array(children) => (children.len(), children.get(*done).copied()),
                Node::Object(children) => {
                    (children.len(), children.get(*done).map(|entry| entry.1))
                }
                _ => (0, None),
            };
            if *done == count {
                if style.pretty && *wrote > 0 {
                    indent(out, depth - 1);
                }
                out.push(if matches!(self.nodes[*at], Node::Array(_)) { ']' } else { '}' });
                stack.pop();
                continue;
            }
            let object = matches!(self.nodes[*at], Node::Object(_));
            *done += 1;
            if style.strip && object && child.is_some_and(|child| self.nodes[child] == Node::Null) {
                continue;
            }
            if *wrote > 0 {
                out.push(',');
            }
            *wrote += 1;
            if style.pretty {
                indent(out, depth);
            }
            if let Node::Object(children) = &self.nodes[*at] {
                string_text(&children[*done - 1].0, out);
                out.push_str(if style.pretty { ": " } else { ":" });
            }
            next = child;
        }
    }

    fn written(&self, at: usize) -> String {
        let mut out = String::new();
        self.write(at, &mut out);
        out
    }
}

/// How a document is written out.
#[derive(Debug, Clone, Copy, Default)]
struct Style {
    /// One value to a line, indented four spaces a level, as yyjson's pretty writer does.
    pretty: bool,
    /// Every member of an object whose value is null left out, at any depth.
    strip: bool,
}

/// A new line and the indent for a depth.
fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("    ");
    }
}

/// The digits of a real number as written when a double may not hold them all, which is when
/// there are more than fifteen of them.
fn precise(text: &str) -> Option<Box<str>> {
    let mantissa = text.split(['e', 'E']).next().unwrap_or(text);
    let digits = mantissa.bytes().filter(u8::is_ascii_digit).skip_while(|&digit| digit == b'0');
    (digits.count() > 15).then(|| text.into())
}

fn scalar_text(node: &Node, out: &mut String) {
    match node {
        Node::Null => out.push_str("null"),
        Node::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Node::Unsigned(number) => out.push_str(&number.to_string()),
        Node::Signed(number) => out.push_str(&number.to_string()),
        Node::Real(number, _) => real_text(*number, out),
        Node::Raw(text) => out.push_str(text),
        Node::Str(text) => string_text(text, out),
        Node::Array(_) | Node::Object(_) => {}
    }
}

/// A string with the pin's escapes: the quote, the backslash and the control characters, which go
/// out as their short form where JSON has one and as `\u00XX` in upper case where it does not.
fn string_text(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", u32::from(control)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// A double the way yyjson writes one: the shortest digits that read back as the same number, in
/// plain notation between a millionth and 10^21 and with an exponent outside that, and a whole
/// number always with a `.0`.
fn real_text(number: f64, out: &mut String) {
    if number.is_nan() {
        out.push_str("NaN");
        return;
    }
    if number.is_infinite() {
        out.push_str(if number > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    if number.is_sign_negative() {
        out.push('-');
    }
    let magnitude = number.abs();
    if magnitude == 0.0 {
        out.push_str("0.0");
        return;
    }
    if magnitude.fract() == 0.0 && magnitude < 9_007_199_254_740_992.0 {
        out.push_str(&format!("{magnitude:.0}.0"));
        return;
    }
    let scientific = format!("{magnitude:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or_default();
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let point = exponent + 1;
    if -6 < point && point <= 21 {
        if point <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', point.unsigned_abs() as usize));
            out.push_str(&digits);
        } else {
            let point = point as usize;
            if digits.len() <= point {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push_str(&format!("e{exponent}"));
    }
}

/// The text of a value made into a document, which is what a cast to `JSON` from anything that is
/// not a string answers.
///
/// A string is a JSON string, a number is a number, a list is an array and a struct or a map is an
/// object, which is the pin's `to_json`. A value already of type `JSON` goes in as the document it
/// holds. A decimal of more than 15 digits is written as its digits, so that it is not rounded on
/// the way, and a narrower one goes through a double, as the pin's does.
///
/// # Errors
///
/// If a value cannot be written as text, which a well formed value always can.
pub fn to_json(value: &Value, ty: &LogicalType, zone: Option<SessionTimeZone>) -> Result<String> {
    let mut out = String::new();
    value_text(value, ty, zone, &mut out)?;
    Ok(out)
}

fn value_text(
    value: &Value,
    ty: &LogicalType,
    zone: Option<SessionTimeZone>,
    out: &mut String,
) -> Result<()> {
    match (value, ty) {
        (Value::Null, _) => out.push_str("null"),
        (Value::Varchar(text), LogicalType::Json) => match read(text) {
            Ok(document) => document.write(0, out),
            Err(_) => string_text(text, out),
        },
        (Value::Boolean(flag), _) => out.push_str(if *flag { "true" } else { "false" }),
        (
            Value::TinyInt(_)
            | Value::SmallInt(_)
            | Value::Integer(_)
            | Value::BigInt(_)
            | Value::HugeInt(_)
            | Value::UTinyInt(_)
            | Value::USmallInt(_)
            | Value::UInteger(_)
            | Value::UBigInt(_)
            | Value::UHugeInt(_),
            _,
        ) => out.push_str(&value.to_string()),
        (Value::Float(number), _) => real_text(f64::from(*number), out),
        (Value::Double(number), _) => real_text(*number, out),
        (Value::Decimal { width, .. }, _) if *width > 15 => out.push_str(&value.to_string()),
        (Value::Decimal { .. }, _) => match cast_value(value, &LogicalType::Double, false)? {
            Value::Double(number) => real_text(number, out),
            _ => out.push_str("null"),
        },
        (Value::Varchar(text), _) => string_text(text, out),
        (Value::List { element, values }, _) => {
            let element = match ty {
                LogicalType::List(element) | LogicalType::Array(element, _) => element.as_ref(),
                _ => element,
            };
            out.push('[');
            for (index, item) in values.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                value_text(item, element, zone, out)?;
            }
            out.push(']');
        }
        // A union is an object of the one member it holds, under the member's name.
        (Value::Union { members, tag, value }, _) => {
            let member = members
                .get(usize::from(*tag))
                .ok_or_else(|| Error::internal("a union tag names no member"))?;
            out.push('{');
            string_text(&member.name, out);
            out.push(':');
            value_text(value, &member.ty, zone, out)?;
            out.push('}');
        }
        (Value::Struct(fields), _) => {
            let types = match ty {
                LogicalType::Struct(types) => Some(types),
                _ => None,
            };
            // An unnamed struct, which the pin calls a TUPLE, is an array of its fields.
            let unnamed = !fields.is_empty() && fields.iter().all(|(name, _)| name.is_empty());
            out.push(if unnamed { '[' } else { '{' });
            for (index, (name, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                if !unnamed {
                    string_text(name, out);
                    out.push(':');
                }
                let field = types.and_then(|types| types.get(index)).map(|field| &field.ty);
                let held = item.logical_type();
                value_text(item, field.unwrap_or(&held), zone, out)?;
            }
            out.push(if unnamed { ']' } else { '}' });
        }
        (Value::Map { key, value: held, entries }, _) => {
            out.push('{');
            let mut first = true;
            for (name, item) in entries {
                if name.is_null() {
                    continue;
                }
                if !first {
                    out.push(',');
                }
                first = false;
                let name = match cast_value(name, &LogicalType::Varchar, false)? {
                    Value::Varchar(text) => text,
                    other => other.to_string(),
                };
                let _ = key;
                string_text(&name, out);
                out.push(':');
                value_text(item, held, zone, out)?;
            }
            out.push('}');
        }
        (Value::TimestampTz(micros), _) if zone.is_some() => {
            let zone = zone.unwrap_or_default();
            string_text(&value.to_string_at_offset(zone.offset_seconds_at(*micros)), out);
        }
        _ => match cast_value(value, &LogicalType::Varchar, false)? {
            Value::Varchar(text) => string_text(&text, out),
            other => string_text(&other.to_string(), out),
        },
    }
    Ok(())
}

/// A value a cast to `JSON` reads: a string has to parse and is kept as it is, and anything else is
/// made into a document.
///
/// # Errors
///
/// What [`checked`] and [`to_json`] report.
pub fn cast_to_json(
    value: &Value,
    from: &LogicalType,
    try_cast: bool,
    zone: Option<SessionTimeZone>,
) -> Result<Value> {
    match (value, from) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::Varchar(text), LogicalType::Varchar | LogicalType::Json | LogicalType::Null) => {
            if *from == LogicalType::Json {
                Ok(value.clone())
            } else {
                checked(text, try_cast)
            }
        }
        _ => to_json(value, from, zone).map(Value::Varchar),
    }
}

/// Whether a cast from one type to another is one of the casts here.
fn involved(from: &LogicalType, target: &LogicalType) -> bool {
    let listed = matches!(from, LogicalType::List(element) | LogicalType::Array(element, _) if **element == LogicalType::Json);
    *target == LogicalType::Json
        || *from == LogicalType::Json
        || (listed && *target == LogicalType::Varchar)
}

/// One value cast into or out of `JSON`, and nothing for a cast that has nothing to do with it,
/// which is what folding a constant cast needs, since a value of `JSON` is held as a string and
/// only its type says it is a document.
///
/// A cast into `JSON` checks a string and writes anything else as a document. A cast out of one to
/// a string is the text as it was, and to anything else reads the document into the target. A list
/// of `JSON` written as a string has its elements as they were written, with no quotes put round
/// them, as the pin's does.
///
/// # Errors
///
/// What [`cast_to_json`] and [`cast_from_json`] report.
pub fn cast_typed(
    value: &Value,
    from: &LogicalType,
    target: &LogicalType,
    try_cast: bool,
    zone: Option<SessionTimeZone>,
) -> Option<Result<Value>> {
    if !involved(from, target) {
        return None;
    }
    Some(match (from, target, value) {
        (_, _, Value::Null) => Ok(Value::Null),
        (_, LogicalType::Json, _) => cast_to_json(value, from, try_cast, zone),
        (LogicalType::Json, _, Value::Varchar(text)) => cast_from_json(text, target, try_cast),
        (_, LogicalType::Varchar, Value::List { values, .. }) => {
            let items: Vec<String> = values
                .iter()
                .map(|item| match item {
                    Value::Varchar(text) => text.clone(),
                    _ => "NULL".to_string(),
                })
                .collect();
            Ok(Value::Varchar(format!("[{}]", items.join(", "))))
        }
        _ => cast_value(value, target, try_cast),
    })
}

/// A column cast into or out of `JSON`, and nothing for a cast that has nothing to do with it, as
/// [`cast_typed`] reads one value.
///
/// # Errors
///
/// What [`cast_typed`] reports.
pub fn cast_vector(
    input: &Vector,
    target: &LogicalType,
    try_cast: bool,
    zone: Option<SessionTimeZone>,
) -> Result<Option<Vector>> {
    let from = input.logical_type();
    if !involved(from, target) {
        return Ok(None);
    }
    // row at a time: a document is read and written whole, so there is no column of numbers to
    // sweep, and every row is a parse or a write either way.
    let mut values = Vec::with_capacity(input.len());
    for index in 0..input.len() {
        let value = input.try_value_at(index)?;
        values.push(cast_typed(&value, from, target, try_cast, zone).unwrap_or(Ok(value))?);
    }
    Vector::from_values(target.clone(), &values).map(Some)
}

/// The name the pin's `json_type` gives a value.
fn type_name(node: &Node) -> &'static str {
    match node {
        Node::Null => "NULL",
        Node::Bool(_) => "BOOLEAN",
        Node::Unsigned(_) => "UBIGINT",
        Node::Signed(_) => "BIGINT",
        Node::Real(..) | Node::Raw(_) => "DOUBLE",
        Node::Str(_) => "VARCHAR",
        Node::Array(_) => "ARRAY",
        Node::Object(_) => "OBJECT",
    }
}

/// A value of a document a cast out of `JSON` reads.
///
/// # Errors
///
/// The pin's refusals: a container where a scalar was wanted, the wrong kind of container, a struct
/// key the object does not have or has too many of, and a scalar that does not convert.
pub fn cast_from_json(text: &str, target: &LogicalType, try_cast: bool) -> Result<Value> {
    if matches!(target, LogicalType::Varchar | LogicalType::Json) {
        return Ok(Value::Varchar(text.to_string()));
    }
    let document = match read(text) {
        Ok(document) => document,
        Err(_) if try_cast => return Ok(Value::Null),
        Err(malformed) => return Err(Error::conversion(malformed.describe(text))),
    };
    document.convert(0, target, if try_cast { Reading::Try } else { Reading::Cast })
}

/// How strictly a document is read into a type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// A cast, which refuses a key the struct has no field for as well as all that `Strict` does.
    Cast,
    /// `json_transform_strict`, which refuses a missing key, a repeated one and a value that does
    /// not convert, and passes over a key the struct has no field for.
    Strict,
    /// `json_transform`, where what does not convert is a null in its place.
    Lenient,
    /// A `TRY_CAST`, which is lenient but for a key a map cannot read, which it still raises.
    Try,
}

/// The functions that read a document into the type a constant structure names, which the binder
/// works out from the structure and hands over as the type of the answer.
pub const TRANSFORMS: &[&str] =
    &["json_transform", "json_transform_strict", "from_json", "from_json_strict"];

/// Calls one of the [`TRANSFORMS`] on one row, and nothing for a name that is not one of them.
fn transform(name: &str, args: &[Value], returns: &LogicalType) -> Result<Option<Value>> {
    if !TRANSFORMS.contains(&name) {
        return Ok(None);
    }
    let Some(Value::Varchar(text)) = args.first() else { return Ok(Some(Value::Null)) };
    let document = document(text)?;
    if *returns == LogicalType::Null {
        return Ok(Some(Value::Null));
    }
    let reading = if name.ends_with("_strict") { Reading::Strict } else { Reading::Lenient };
    match document.convert(0, returns, reading) {
        Ok(value) => Ok(Some(value)),
        // The pin raises a key it cannot read as it is, where anything else it reports as input.
        Err(error) if error.message().ends_with(NULL_KEY) => Err(error),
        Err(error) => Err(Error::invalid_input(error.message())),
    }
}

/// What the pin adds to the reason a key of an object could not be read into the key type of a
/// map, since a map has no place for a null key.
const NULL_KEY: &str = ". Cannot default to NULL, because map keys cannot be NULL";

/// The type a structure given to `json_transform` names, where an array of one element is a list
/// of it, an object is a struct with a field for each key, and a string is a type name, which
/// `named` reads.
///
/// # Errors
///
/// A malformed structure and the pin's refusals of one that names no type, and what `named`
/// reports for a type name it does not know.
pub fn structure_type(
    text: &str,
    named: &mut dyn FnMut(&str) -> Result<LogicalType>,
) -> Result<LogicalType> {
    document(text)?.structure_type(0, named)
}

impl Document {
    /// The value at a position read as a type. A `TRY_CAST` is lenient the way the pin's is, where
    /// what does not convert is a null in its place rather than the whole value, so a struct read
    /// from an array is a struct of nulls and a key the struct has no field for is passed over.
    fn convert(&self, at: usize, target: &LogicalType, reading: Reading) -> Result<Value> {
        let lenient = matches!(reading, Reading::Lenient | Reading::Try);
        let node = &self.nodes[at];
        if matches!(node, Node::Null) {
            return Ok(Value::Null);
        }
        let outcome = self.converted(at, target, reading);
        if let Err(error) = &outcome
            && reading == Reading::Try
            && error.message().ends_with(NULL_KEY)
        {
            return outcome;
        }
        if lenient && outcome.is_err() {
            if let LogicalType::Struct(fields) = target
                && !Field::unnamed(fields)
            {
                let nulls = fields.iter().map(|field| (field.name.clone(), Value::Null)).collect();
                return Ok(Value::Struct(nulls));
            }
            return Ok(Value::Null);
        }
        outcome
    }

    fn converted(&self, at: usize, target: &LogicalType, reading: Reading) -> Result<Value> {
        let lenient = matches!(reading, Reading::Lenient | Reading::Try);
        let node = &self.nodes[at];
        let expected = |wanted: &str| {
            Error::conversion(format!(
                "Expected {wanted}, but got {}: {}",
                type_name(node),
                self.written(at)
            ))
        };
        match target {
            LogicalType::Json => Ok(Value::Varchar(self.written(at))),
            LogicalType::Varchar => Ok(Value::Varchar(match node {
                Node::Str(text) => text.clone(),
                _ => self.written(at),
            })),
            LogicalType::List(element) | LogicalType::Array(element, _) => {
                let Node::Array(children) = node else { return Err(expected("ARRAY")) };
                if let LogicalType::Array(_, size) = target
                    && children.len() != *size as usize
                {
                    return Err(Error::conversion(format!(
                        "Expected array of size {size}, but got '{}' with size {}",
                        self.written(at),
                        children.len()
                    )));
                }
                let values = children
                    .iter()
                    .map(|&child| self.convert(child, element, reading))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::List { element: element.as_ref().clone(), values })
            }
            // A TUPLE is read from an array by position, where a missing element is a null and
            // one past the last field is passed over, as the pin's is.
            LogicalType::Struct(fields) if Field::unnamed(fields) => {
                let Node::Array(children) = node else { return Err(expected("ARRAY")) };
                let mut values = Vec::with_capacity(fields.len());
                for (index, field) in fields.iter().enumerate() {
                    let value = match children.get(index) {
                        Some(&child) => self.convert(child, &field.ty, reading)?,
                        None => Value::Null,
                    };
                    values.push((field.name.clone(), value));
                }
                Ok(Value::Struct(values))
            }
            LogicalType::Struct(fields) => {
                let Node::Object(children) = node else { return Err(expected("OBJECT")) };
                let mut seen = vec![false; fields.len()];
                for (key, _) in children {
                    match fields.iter().position(|field| field.name == *key) {
                        Some(field) if seen[field] && !lenient => {
                            return Err(Error::conversion(format!(
                                "Object {} has duplicate key \"{key}\"",
                                self.written(at)
                            )));
                        }
                        Some(field) => seen[field] = true,
                        None if reading == Reading::Cast => {
                            return Err(Error::conversion(format!(
                                "Object {} has unknown key \"{key}\"",
                                self.written(at)
                            )));
                        }
                        None => {}
                    }
                }
                let mut values = Vec::with_capacity(fields.len());
                for field in fields {
                    let Some((_, child)) = children.iter().find(|(key, _)| *key == field.name)
                    else {
                        if lenient {
                            values.push((field.name.clone(), Value::Null));
                            continue;
                        }
                        return Err(Error::conversion(format!(
                            "Object {} does not have key \"{}\"",
                            self.written(at),
                            field.name
                        )));
                    };
                    values.push((field.name.clone(), self.convert(*child, &field.ty, reading)?));
                }
                Ok(Value::Struct(values))
            }
            // A union is read from an object of one key, the member's name, as the pin writes it.
            LogicalType::Union(members) => {
                let Node::Object(children) = node else {
                    // The pin names the kind the way its parser does, a boolean by its value.
                    let got = match node {
                        Node::Bool(true) => "true",
                        Node::Bool(false) => "false",
                        Node::Unsigned(_) => "uint",
                        Node::Signed(_) => "sint",
                        Node::Real(..) | Node::Raw(_) => "real",
                        Node::Str(_) => "string",
                        Node::Array(_) => "array",
                        Node::Null | Node::Object(_) => "null",
                    };
                    return Err(Error::conversion(format!(
                        "Expected an object representing a union, got {got}"
                    )));
                };
                let (key, child) = match &children[..] {
                    [] => return Err(Error::conversion("Found empty object, instead of union")),
                    [(key, child)] => (key, *child),
                    _ => {
                        return Err(Error::conversion(
                            "Found object containing more than one key, instead of union",
                        ));
                    }
                };
                let Some(at) = members.iter().position(|member| member.name == *key) else {
                    return Err(Error::conversion(format!(
                        "Found object containing unknown key, instead of union: {key}"
                    )));
                };
                Ok(Value::Union {
                    members: members.clone(),
                    tag: u8::try_from(at)
                        .map_err(|_| Error::internal("a union has too many members"))?,
                    value: Box::new(self.convert(child, &members[at].ty, reading)?),
                })
            }
            LogicalType::Map(key, value) => {
                let Node::Object(children) = node else { return Err(expected("OBJECT")) };
                let entries = children
                    .iter()
                    .map(|(name, child)| {
                        let name = Document { nodes: vec![Node::Str(name.clone())] }
                            .converted(0, key, reading)
                            .map_err(|error| {
                                Error::conversion(format!("{}{NULL_KEY}", error.message()))
                            })?;
                        Ok((name, self.convert(*child, value, reading)?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::map(key.as_ref().clone(), value.as_ref().clone(), entries))
            }
            // A boolean is read the way a number is, as the pin does.
            _ if target.is_numeric() || *target == LogicalType::Boolean => {
                let kind = if matches!(target, LogicalType::Decimal { .. }) {
                    "decimal"
                } else {
                    "numerical"
                };
                // The pin reads a decimal from the digits as written, which a double may have
                // rounded, and names those digits when they do not fit.
                let digits = match node {
                    Node::Real(_, Some(text)) if kind == "decimal" => Some(text.to_string()),
                    _ => None,
                };
                let failed = || {
                    let shown = digits.clone().unwrap_or_else(|| self.written(at));
                    Error::conversion(format!("Failed to cast value to {kind}: {shown}"))
                };
                let scalar = match &digits {
                    Some(text) => Value::Varchar(text.clone()),
                    None => self.scalar_value(node).ok_or_else(failed)?,
                };
                if let Value::Varchar(text) = &scalar
                    && reading != Reading::Lenient
                    && !strict_text(text, target)
                {
                    return Err(failed());
                }
                match cast_value(&scalar, target, true) {
                    Ok(Value::Null) | Err(_) => Err(failed()),
                    Ok(value) => Ok(value),
                }
            }
            _ => {
                let scalar =
                    self.scalar_value(node).unwrap_or_else(|| Value::Varchar(self.written(at)));
                cast_value(&scalar, target, false).map_err(|error| {
                    Error::conversion(format!(
                        "{}\n If this error occurred during read_json, line/object number information is approximate",
                        error.message()
                    ))
                })
            }
        }
    }

    /// The type the structure at a position names, as [`structure_type`] reads it.
    fn structure_type(
        &self,
        at: usize,
        named: &mut dyn FnMut(&str) -> Result<LogicalType>,
    ) -> Result<LogicalType> {
        match &self.nodes[at] {
            Node::Array(children) => match children[..] {
                [only] => Ok(LogicalType::list(self.structure_type(only, named)?)),
                _ => Err(Error::binder("Too many values in array of JSON structure")),
            },
            Node::Object(children) => {
                let mut fields: Vec<Field> = Vec::with_capacity(children.len());
                for (key, child) in children {
                    if fields.iter().any(|field| field.name == *key) {
                        return Err(Error::invalid_input(format!(
                            "Duplicate keys in object in JSON structure: {}",
                            self.written(*child)
                        )));
                    }
                    fields.push(Field::new(key.clone(), self.structure_type(*child, named)?));
                }
                if fields.is_empty() {
                    return Err(Error::binder("Empty object in JSON structure"));
                }
                Ok(LogicalType::Struct(fields))
            }
            Node::Str(name) => named(name),
            _ => Err(Error::binder("invalid JSON structure")),
        }
    }

    /// The SQL value a scalar of a document is, and nothing for a container.
    fn scalar_value(&self, node: &Node) -> Option<Value> {
        Some(match node {
            Node::Null => Value::Null,
            Node::Bool(flag) => Value::Boolean(*flag),
            Node::Unsigned(number) => Value::UBigInt(*number),
            Node::Signed(number) => Value::BigInt(*number),
            Node::Real(number, _) => Value::Double(*number),
            Node::Raw(text) | Node::Str(text) => Value::Varchar(text.clone()),
            Node::Array(_) | Node::Object(_) => return None,
        })
    }
}

/// Whether a string a strict reading turns into a number is written the way the pin's strict cast
/// takes one. That refuses a leading plus, a leading zero, and for a whole number a fraction, an
/// exponent or an underscore between digits, while a hexadecimal or binary one is still taken. A
/// real number may not have spaces after it, and a boolean may not be `y`, `n`, `1` or `0`. A
/// decimal is read the same either way.
fn strict_text(text: &str, target: &LogicalType) -> bool {
    let space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r');
    let body = text.trim_start_matches(space);
    let bytes = body.as_bytes();
    let leading_zero = bytes.len() > 1 && bytes[0] == b'0' && bytes[1].is_ascii_digit();
    if *target == LogicalType::Boolean {
        return !matches!(text.to_ascii_lowercase().as_str(), "y" | "n" | "1" | "0");
    }
    if body.starts_with('+') {
        return false;
    }
    if target.is_integer() {
        let body = body.trim_end_matches(space);
        if let Some(digits) = body.strip_prefix('-') {
            return digits.bytes().all(|byte| byte.is_ascii_digit());
        }
        if matches!(bytes, [b'0', b'x' | b'X' | b'b' | b'B', ..]) {
            return true;
        }
        return !leading_zero && body.bytes().all(|byte| byte.is_ascii_digit());
    }
    if matches!(target, LogicalType::Float | LogicalType::Double) {
        return !leading_zero && !body.ends_with(space);
    }
    true
}

/// One step of a `$` path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(String),
    /// `[n]`, counted from the front.
    Index(u64),
    /// `[-n]` or `[#-n]`, counted from the back.
    Back(u64),
    /// `[#]`, which is one past the end and so never there.
    Append,
}

/// What reading a key gave: how many bytes it took, whether it was recursive and the key, where
/// the key `*` is the wildcard.
struct KeyRead {
    taken: usize,
    recursive: bool,
    key: String,
}

impl KeyRead {
    fn wild(&self) -> bool {
        self.key == "*"
    }
}

fn read_key(bytes: &[u8], from: usize) -> Option<KeyRead> {
    let end = bytes.len();
    let mut at = from;
    if bytes[at] == b'*' {
        let double = at + 1 != end && bytes[at + 1] == b'*';
        return Some(KeyRead {
            taken: if double { 2 } else { 1 },
            recursive: double,
            key: "*".into(),
        });
    }
    let mut recursive = false;
    if bytes[at] == b'.' {
        let next = bytes.get(at + 1).copied().unwrap_or(0);
        if next == b'*' {
            return Some(KeyRead { taken: 2, recursive: true, key: "*".into() });
        }
        if next == b'[' {
            return Some(KeyRead { taken: 1, recursive: true, key: "*".into() });
        }
        at += 1;
        recursive = true;
    }
    if at == end {
        return None;
    }
    let quoted = bytes[at] == b'"';
    if quoted {
        at += 1;
    }
    let before = at;
    let mut key = Vec::new();
    if quoted {
        let mut backslash = false;
        while at != end {
            if backslash {
                if bytes[at] != b'"' && bytes[at] != b'\\' {
                    key.push(b'\\');
                }
                backslash = false;
            } else if bytes[at] == b'"' {
                break;
            } else if bytes[at] == b'\\' {
                backslash = true;
                at += 1;
                continue;
            }
            key.push(bytes[at]);
            at += 1;
        }
        if at == end || backslash {
            return None;
        }
    } else {
        while at != end && bytes[at] != b'.' && bytes[at] != b'[' {
            at += 1;
        }
        key.extend_from_slice(&bytes[before..at]);
    }
    let mut taken = at - before;
    if taken == 0 {
        return None;
    }
    if quoted {
        taken += 2;
    }
    if recursive {
        taken += 1;
    }
    Some(KeyRead { taken, recursive, key: String::from_utf8_lossy(&key).into_owned() })
}

/// What an index in brackets is, with the reader moved past it as the pin's moves.
enum IndexRead {
    Wild,
    Step(Step),
}

fn read_index(bytes: &[u8], at: &mut usize) -> Option<IndexRead> {
    let end = bytes.len();
    if bytes[*at] == b'*' {
        *at += 1;
        if *at == end || bytes[*at] != b']' {
            return None;
        }
        *at += 1;
        return Some(IndexRead::Wild);
    }
    let mut back = false;
    if bytes[*at] == b'#' {
        *at += 1;
        if *at == end {
            return None;
        }
        if bytes[*at] == b']' {
            *at += 1;
            return Some(IndexRead::Step(Step::Append));
        }
        if bytes[*at] != b'-' {
            return None;
        }
    }
    if bytes[*at] == b'-' {
        *at += 1;
        back = true;
    }
    let before = *at;
    let mut index: u64 = 0;
    let mut cursor = *at;
    for _ in 0..19 {
        if cursor == end {
            return None;
        }
        if bytes[cursor] == b']' {
            break;
        }
        let digit = bytes[cursor].wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        index = index * 10 + u64::from(digit);
        cursor += 1;
    }
    let length = cursor - before;
    if length == 0 || index == u64::MAX {
        return None;
    }
    *at = cursor + 1;
    Some(IndexRead::Step(if back { Step::Back(index) } else { Step::Index(index) }))
}

/// The pin's refusal of a path, which names it from the byte it gave up at.
fn path_error(bytes: &[u8], at: usize, binder: bool) -> Error {
    let message = format!(
        "JSON path error near '{}'",
        String::from_utf8_lossy(&bytes[at.saturating_sub(1)..])
    );
    if binder { Error::binder(message) } else { Error::invalid_input(message) }
}

/// The steps of a `$` path and whether it has a wildcard, refused the way the pin refuses one.
fn parse_path(path: &str, binder: bool) -> Result<(Vec<Step>, bool)> {
    let bytes = path.as_bytes();
    let mut at = 1;
    let mut steps = Vec::new();
    let mut wild = false;
    while at != bytes.len() {
        let byte = bytes[at];
        at += 1;
        if at == bytes.len() {
            return Err(path_error(bytes, at, binder));
        }
        match byte {
            b'.' => {
                let key = read_key(bytes, at).ok_or_else(|| path_error(bytes, at, binder))?;
                at += key.taken;
                if key.recursive || key.wild() {
                    wild = true;
                } else {
                    steps.push(Step::Key(key.key));
                }
            }
            b'[' => match read_index(bytes, &mut at) {
                Some(IndexRead::Wild) => wild = true,
                Some(IndexRead::Step(step)) => steps.push(step),
                None => return Err(path_error(bytes, at, binder)),
            },
            _ => return Err(path_error(bytes, at, binder)),
        }
    }
    Ok((steps, wild))
}

/// What a path argument means once it is written as one: the whole document, a pointer, or a `$`
/// path with its steps.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Path {
    Root,
    Pointer(String),
    Steps(Vec<Step>),
    /// A `$` path with a wildcard in it, kept as text since it is walked as text.
    Wild(String),
}

/// The path a path argument is, which is the pin's reading of one: nothing is the whole document,
/// a `/` is a pointer and a `$` a path, a number is an index, and anything else is a key, which is
/// a pointer to it when it has a quote in it.
///
/// # Errors
///
/// A `$` path the pin refuses, as a binder error when the path is a constant being bound and as an
/// invalid input error when it is a value met on a row.
fn path_of(path: &Value, binder: bool) -> Result<Option<Path>> {
    let text = match path {
        Value::Null => return Ok(None),
        Value::Varchar(text) => text.clone(),
        other if other.as_i64().is_some() => format!("$[{other}]"),
        other => other.to_string(),
    };
    Ok(Some(written_path(&text, binder)?))
}

fn written_path(text: &str, binder: bool) -> Result<Path> {
    if text.is_empty() {
        return Ok(Path::Root);
    }
    match text.as_bytes()[0] {
        b'/' => Ok(Path::Pointer(text.to_string())),
        b'$' => {
            let (steps, wild) = parse_path(text, binder)?;
            Ok(if wild { Path::Wild(text.to_string()) } else { Path::Steps(steps) })
        }
        _ if text.contains('"') => Ok(Path::Pointer(format!("/{text}"))),
        _ => written_path(&format!("$.\"{text}\""), binder),
    }
}

/// Whether a constant path argument has a wildcard in it, read at bind time, which decides that a
/// call answers a list of every match rather than one of them.
///
/// # Errors
///
/// The pin's binder error for a path it cannot read.
pub fn wild_path(path: &Value) -> Result<bool> {
    Ok(matches!(path_of(path, true)?, Some(Path::Wild(_))))
}

impl Document {
    fn child(&self, at: usize, step: &Step) -> Option<usize> {
        match (&self.nodes[at], step) {
            (Node::Object(children), Step::Key(key)) => {
                children.iter().find(|(name, _)| name == key).map(|entry| entry.1)
            }
            (Node::Array(children), Step::Index(index)) => {
                children.get(usize::try_from(*index).ok()?).copied()
            }
            (Node::Array(children), Step::Back(index)) => {
                let position =
                    if *index == 0 { 0 } else { (children.len() as u64).wrapping_sub(*index) };
                children.get(usize::try_from(position).ok()?).copied()
            }
            _ => None,
        }
    }

    fn pointer(&self, pointer: &str) -> Option<usize> {
        let bytes = pointer.as_bytes();
        let mut at = 0;
        let mut node = 0;
        while at < bytes.len() {
            let start = at + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'/' {
                end += 1;
            }
            let raw = &bytes[start..end];
            let mut token = Vec::with_capacity(raw.len());
            let mut index = 0;
            while index < raw.len() {
                if raw[index] == b'~' {
                    match raw.get(index + 1) {
                        Some(b'0') => token.push(b'~'),
                        Some(b'1') => token.push(b'/'),
                        _ => return None,
                    }
                    index += 2;
                } else {
                    token.push(raw[index]);
                    index += 1;
                }
            }
            node = match &self.nodes[node] {
                Node::Object(children) => children
                    .iter()
                    .find(|(name, _)| name.as_bytes() == token)
                    .map(|entry| entry.1)?,
                Node::Array(children) => {
                    let position = array_token(&token)?;
                    *children.get(position)?
                }
                _ => return None,
            };
            at = end;
        }
        Some(node)
    }

    /// The value a path without a wildcard picks, if it is there.
    fn find(&self, path: &Path) -> Option<usize> {
        match path {
            Path::Root => Some(0),
            Path::Pointer(pointer) => self.pointer(pointer),
            Path::Steps(steps) => steps.iter().try_fold(0, |at, step| self.child(at, step)),
            Path::Wild(_) => None,
        }
    }

    /// Every value a path with a wildcard picks, in the pin's order.
    fn every(&self, path: &str) -> Vec<usize> {
        let mut found = Vec::new();
        self.wild_walk(0, path.as_bytes(), 1, &mut found);
        found
    }

    fn wild_walk(&self, mut node: usize, bytes: &[u8], mut at: usize, found: &mut Vec<usize>) {
        let end = bytes.len();
        while at != end {
            let byte = bytes[at];
            at += 1;
            match byte {
                b'.' => {
                    let Some(key) = read_key(bytes, at) else { return };
                    if key.recursive {
                        if key.wild() {
                            at += key.taken;
                        }
                        let mut queue = vec![node];
                        let mut index = 0;
                        while index < queue.len() {
                            let current = queue[index];
                            match &self.nodes[current] {
                                Node::Array(children) => queue.extend(children.iter().copied()),
                                Node::Object(children) => {
                                    queue.extend(children.iter().map(|entry| entry.1));
                                }
                                _ => {}
                            }
                            if index > 0 || at != end {
                                self.wild_walk(current, bytes, at, found);
                            }
                            index += 1;
                        }
                        return;
                    }
                    at += key.taken;
                    let Node::Object(children) = &self.nodes[node] else { return };
                    if key.wild() {
                        for (_, child) in children {
                            self.wild_walk(*child, bytes, at, found);
                        }
                        return;
                    }
                    match children.iter().find(|(name, _)| *name == key.key) {
                        Some((_, child)) => node = *child,
                        None => return,
                    }
                }
                b'[' => {
                    let Node::Array(children) = &self.nodes[node] else { return };
                    match read_index(bytes, &mut at) {
                        Some(IndexRead::Wild) => {
                            for child in children {
                                self.wild_walk(*child, bytes, at, found);
                            }
                            return;
                        }
                        Some(IndexRead::Step(step)) => match self.child(node, &step) {
                            Some(child) => node = child,
                            None => return,
                        },
                        None => return,
                    }
                }
                _ => return,
            }
        }
        found.push(node);
    }
}

/// The position an array token of a pointer names, which is `0` or digits with no leading zero.
fn array_token(token: &[u8]) -> Option<usize> {
    if token.is_empty() || token.len() > 19 || token == b"-" {
        return None;
    }
    if token[0] == b'0' {
        return (token.len() == 1).then_some(0);
    }
    let mut number: u64 = 0;
    for &byte in token {
        let digit = byte.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        number = number * 10 + u64::from(digit);
    }
    usize::try_from(number).ok()
}

/// The functions here, by name, which is every name `call` answers.
pub const NAMES: &[&str] = &[
    "json",
    "json_valid",
    "json_type",
    "json_array_length",
    "json_keys",
    "json_extract",
    "json_extract_path",
    "json_extract_string",
    "json_extract_path_text",
    "->>",
    "json_value",
    "json_exists",
];

/// What a function answers for one value it found.
fn answer(name: &str, document: &Document, at: Option<usize>) -> Value {
    let Some(at) = at else {
        return if name == "json_exists" { Value::Boolean(false) } else { Value::Null };
    };
    let node = &document.nodes[at];
    match name {
        "json_exists" => Value::Boolean(true),
        "json_type" => Value::Varchar(type_name(node).to_string()),
        "json_array_length" => Value::UBigInt(match node {
            Node::Array(children) => children.len() as u64,
            _ => 0,
        }),
        "json_keys" => Value::List {
            element: LogicalType::Varchar,
            values: match node {
                Node::Object(children) => {
                    children.iter().map(|(key, _)| Value::Varchar(key.clone())).collect()
                }
                _ => Vec::new(),
            },
        },
        "json_extract_string" | "json_extract_path_text" | "->>" => match node {
            Node::Null => Value::Null,
            Node::Str(text) => Value::Varchar(text.clone()),
            _ => Value::Varchar(document.written(at)),
        },
        "json_value" => match node {
            Node::Null | Node::Array(_) | Node::Object(_) => Value::Null,
            _ => Value::Varchar(document.written(at)),
        },
        _ => Value::Varchar(document.written(at)),
    }
}

/// The type one answer of a function is.
fn answer_type(name: &str) -> LogicalType {
    match name {
        "json_exists" | "json_valid" => LogicalType::Boolean,
        "json_type" | "json_extract_string" | "json_extract_path_text" | "->>" | "json_value" => {
            LogicalType::Varchar
        }
        "json_array_length" => LogicalType::UBigInt,
        "json_keys" => LogicalType::list(LogicalType::Varchar),
        _ => LogicalType::Json,
    }
}

/// Calls one of the functions here on one row, and nothing for a name that is not one of them.
///
/// `returns` says whether a constant path with a wildcard was bound, which answers a list of every
/// value the path picks: the call's type is a list of what one answer is then, where a list of paths
/// also answers a list but has a list for its argument.
///
/// # Errors
///
/// A malformed document, and a path a row holds that the pin refuses.
pub fn call(name: &str, args: &[Value], returns: &LogicalType) -> Result<Option<Value>> {
    if let Some(answer) = transform(name, args, returns)? {
        return Ok(Some(answer));
    }
    if let Some(answer) = whole(name, args)? {
        return Ok(Some(answer));
    }
    if let Some(answer) = edit(name, args)? {
        return Ok(Some(answer));
    }
    if !NAMES.contains(&name) {
        return Ok(None);
    }
    let Some(Value::Varchar(text)) = args.first() else {
        return Ok(Some(Value::Null));
    };
    if name == "json_valid" {
        return Ok(Some(Value::Boolean(valid(text))));
    }
    let document = document(text)?;
    let path = match args.get(1) {
        None => Path::Root,
        Some(Value::List { values, .. }) => {
            let element = answer_type(name);
            let mut answers = Vec::with_capacity(values.len());
            for path in values {
                let Some(path) = path_of(path, false)? else {
                    answers.push(Value::Null);
                    continue;
                };
                answers.push(answer(name, &document, document.find(&path)));
            }
            return Ok(Some(Value::List { element, values: answers }));
        }
        Some(path) => match path_of(path, false)? {
            Some(path) => path,
            None => return Ok(Some(Value::Null)),
        },
    };
    if name == "json" {
        return Ok(Some(Value::Varchar(document.minified())));
    }
    if let Path::Wild(text) = &path {
        if !matches!(returns, LogicalType::List(_)) || *returns == answer_type(name) {
            return Err(Error::invalid_input(
                "JSON path cannot contain wildcards if the path is not a constant parameter",
            ));
        }
        let values =
            document.every(text).into_iter().map(|at| answer(name, &document, Some(at))).collect();
        return Ok(Some(Value::List { element: answer_type(name), values }));
    }
    Ok(Some(answer(name, &document, document.find(&path))))
}

/// The functions that make a document out of values of any type, which need the types and not only
/// the values, since a string and a `JSON` are both held as text and are written differently.
pub const BUILDERS: &[&str] =
    &["to_json", "json_quote", "array_to_json", "row_to_json", "json_array", "json_object"];

/// Calls one of the [`BUILDERS`] on one row, and nothing for a name that is not one of them.
///
/// `to_json` and the two that check their argument answer null for a null, where `json_array`
/// writes a null as `null` and `json_object` refuses one as a key.
///
/// # Errors
///
/// A null key, and a value that cannot be written as text.
pub fn build(
    name: &str,
    args: &[Value],
    types: &[LogicalType],
    zone: Option<SessionTimeZone>,
) -> Result<Option<Value>> {
    if !BUILDERS.contains(&name) {
        return Ok(None);
    }
    let mut out = String::new();
    match name {
        "json_array" => {
            out.push('[');
            for (index, (value, ty)) in args.iter().zip(types).enumerate() {
                if index > 0 {
                    out.push(',');
                }
                value_text(value, ty, zone, &mut out)?;
            }
            out.push(']');
        }
        "json_object" => {
            out.push('{');
            // row at a time: not rows at all, the key and value pairs of one call's arguments.
            for (index, pair) in args.chunks(2).zip(types.chunks(2)).enumerate() {
                let ([key, value], [_, ty]) = pair else { continue };
                let Value::Varchar(key) = key else {
                    return Err(Error::invalid_input("JSON key cannot be NULL"));
                };
                if index > 0 {
                    out.push(',');
                }
                string_text(key, &mut out);
                out.push(':');
                value_text(value, ty, zone, &mut out)?;
            }
            out.push('}');
        }
        _ => match (args, types) {
            ([Value::Null], _) | ([], _) => return Ok(Some(Value::Null)),
            ([value, ..], [ty, ..]) => value_text(value, ty, zone, &mut out)?,
            _ => return Ok(Some(Value::Null)),
        },
    }
    Ok(Some(Value::Varchar(out)))
}

/// Calls one of the [`BUILDERS`] on a batch, and nothing for a name that is not one of them.
///
/// # Errors
///
/// What [`build`] reports.
pub fn build_vectors<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    zone: Option<SessionTimeZone>,
) -> Option<Result<Vector>> {
    if !BUILDERS.contains(&name) {
        return None;
    }
    let types: Vec<LogicalType> =
        args.iter().map(|arg| arg.as_ref().logical_type().clone()).collect();
    let rows = args.first().map_or(1, |arg| arg.as_ref().len());
    let answer = (|| {
        let mut row = Vec::with_capacity(args.len());
        let mut values = Vec::with_capacity(rows);
        // row at a time: each row is a document written whole from values of any type.
        for index in 0..rows {
            row.clear();
            for arg in args {
                row.push(arg.as_ref().try_value_at(index)?);
            }
            values.push(build(name, &row, &types, zone)?.unwrap_or(Value::Null));
        }
        Vector::from_values(LogicalType::Json, &values)
    })();
    Some(answer)
}

/// The functions that read whole documents and answer one, or answer something about them, which
/// [`call`] answers along with the ones that take a path.
pub const WHOLE: &[&str] = &[
    "json_merge_patch",
    "json_deep_merge",
    "json_merge_patch_diff",
    "json_pretty",
    "json_normalize",
    "json_strip_nulls",
    "json_contains",
    "json_structure",
];

/// One of the [`WHOLE`] functions on one row, and nothing for a name that is not one of them.
fn whole(name: &str, args: &[Value]) -> Result<Option<Value>> {
    if !WHOLE.contains(&name) {
        return Ok(None);
    }
    let text = |value: &Value| match value {
        Value::Varchar(text) => Some(text.clone()),
        _ => None,
    };
    let answer = match (name, args) {
        ("json_merge_patch" | "json_deep_merge", [first, rest @ ..]) => {
            let mut merged: Option<(Document, usize)> = match text(first) {
                Some(first) => Some((document(&first)?, 0)),
                None => None,
            };
            for patch in rest {
                let patch = match text(patch) {
                    Some(patch) => document(&patch)?,
                    None => {
                        merged = None;
                        continue;
                    }
                };
                merged = Some(match merged {
                    None => (patch, 0),
                    Some((original, root)) => {
                        let (mut joined, patched) = joined(original, &patch);
                        let root = if name == "json_merge_patch" {
                            joined.merge_patch(root, patched)
                        } else {
                            joined.deep_merge(root, patched)
                        };
                        (joined, root)
                    }
                });
            }
            merged.map(|(document, root)| Value::Varchar(document.written(root)))
        }
        ("json_merge_patch_diff", [old, new]) => match (text(old), text(new)) {
            (_, None) => None,
            (None, Some(new)) => Some(Value::Varchar(document(&new)?.minified())),
            (Some(old), Some(new)) => {
                let new = document(&new)?;
                let (mut joined, new) = joined(document(&old)?, &new);
                let root = joined.diff(0, new);
                Some(Value::Varchar(joined.written(root)))
            }
        },
        ("json_contains", [haystack, needle]) => match (text(haystack), text(needle)) {
            (Some(haystack), Some(needle)) => {
                let needle = document(&needle)?;
                let (joined, needle) = joined(document(&haystack)?, &needle);
                Some(Value::Boolean((0..needle).any(|at| joined.fuzzy(at, needle))))
            }
            _ => None,
        },
        (_, [only]) => match text(only) {
            None => None,
            Some(only) => {
                let document = document(&only)?;
                let mut out = String::new();
                match name {
                    "json_pretty" => {
                        document.write_styled(0, Style { pretty: true, strip: false }, &mut out);
                    }
                    "json_strip_nulls" => {
                        document.write_styled(0, Style { pretty: false, strip: true }, &mut out);
                    }
                    "json_normalize" => {
                        let mut document = document;
                        document.sort_keys();
                        out = document.minified();
                    }
                    _ => out = document.structure(),
                }
                Some(Value::Varchar(out))
            }
        },
        _ => None,
    };
    Ok(Some(answer.unwrap_or(Value::Null)))
}

/// The functions that change one place in a document: `json_set` writes a value there whether or
/// not there was one, `json_insert` only where there was none, `json_replace` only where there was
/// one, and `json_remove` takes it out.
pub const EDITS: &[&str] = &["json_set", "json_insert", "json_replace", "json_remove"];

/// Which of the [`EDITS`] a call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Set,
    Insert,
    Replace,
    Remove,
}

impl Edit {
    /// Whether the edit makes the containers missing on the way to the place it writes, the way
    /// SQLite does, which an edit that only touches what is there does not.
    fn creates(self) -> bool {
        matches!(self, Self::Set | Self::Insert)
    }
}

/// One of the [`EDITS`] on one row, and nothing for a name that is not one of them.
///
/// The place is the pin's: nothing is the whole document, a `/` starts a JSON pointer, a `$` a
/// path, and anything else is the one key it spells. A path with a wildcard is refused, and so is
/// one the pin cannot read, both when a row reaches them and not when the call is bound, so that a
/// `TRY` around the call catches them.
fn edit(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let edit = match name {
        "json_set" => Edit::Set,
        "json_insert" => Edit::Insert,
        "json_replace" => Edit::Replace,
        "json_remove" => Edit::Remove,
        _ => return Ok(None),
    };
    let mut texts = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Value::Varchar(text) => texts.push(text.as_str()),
            _ => return Ok(Some(Value::Null)),
        }
    }
    let (place, mut document, value) = match texts[..] {
        [document_text, place] => (place, document(document_text)?, 0),
        [document_text, place, value] => {
            let value = document(value)?;
            let (joined, value) = joined(document(document_text)?, &value);
            (place, joined, value)
        }
        _ => return Ok(Some(Value::Null)),
    };
    let root = if place.is_empty() {
        root_after(edit, value)
    } else if place.starts_with('/') {
        document.edit_pointer(place, edit, value);
        Some(0)
    } else {
        let steps = if place.starts_with('$') {
            let (steps, wild) = parse_path(place, false)?;
            if wild {
                return Err(Error::invalid_input(
                    "JSON path wildcards are not supported in JSON modification functions",
                ));
            }
            steps
        } else {
            vec![Step::Key(place.to_string())]
        };
        if steps.is_empty() {
            root_after(edit, value)
        } else {
            document.edit_steps(&steps, edit, value);
            Some(0)
        }
    };
    Ok(Some(root.map_or(Value::Null, |root| Value::Varchar(document.written(root)))))
}

/// The root once an edit at the whole document is made: a new one for a set or a replace, the same
/// one for an insert, since there is always a root, and none for a remove.
fn root_after(edit: Edit, value: usize) -> Option<usize> {
    match edit {
        Edit::Set | Edit::Replace => Some(value),
        Edit::Insert => Some(0),
        Edit::Remove => None,
    }
}

/// Where an array step lands in an array of `length` elements, which may be one past the end and
/// no further: `[#]` and `[-0]` are the end itself, and `[-n]` is `n` back from it.
fn landing(length: usize, step: &Step) -> Option<usize> {
    let at = match *step {
        Step::Index(index) => usize::try_from(index).ok()?,
        Step::Back(back) => length.checked_sub(usize::try_from(back).ok()?)?,
        Step::Append => length,
        Step::Key(_) => return None,
    };
    (at <= length).then_some(at)
}

/// A JSON pointer token where an array is, which is a whole number with no leading zero or the `-`
/// that is one past the end.
fn pointer_landing(length: usize, token: &str) -> Option<usize> {
    let at = if token == "-" { length } else { array_token(token.as_bytes())? };
    (at <= length).then_some(at)
}

impl Document {
    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// Writes `value` under `key` in an object, over the first member of that name, with the others
    /// of that name dropped, or after the last member when there is none.
    fn put(&mut self, object: usize, key: &str, value: usize) {
        let Node::Object(members) = &mut self.nodes[object] else { return };
        match members.iter().position(|(name, _)| name == key) {
            Some(first) => {
                members[first].1 = value;
                let mut at = 0;
                members.retain(|(name, _)| {
                    at += 1;
                    at - 1 == first || name != key
                });
            }
            None => members.push((key.to_string(), value)),
        }
    }

    /// Hangs a container made on the way down under the step that was missing, which for an array
    /// is always its end.
    fn hang(&mut self, parent: usize, step: &Step, child: usize) {
        match (&mut self.nodes[parent], step) {
            (Node::Object(_), Step::Key(key)) => self.put(parent, key, child),
            (Node::Array(items), _) => items.push(child),
            _ => {}
        }
    }

    /// An edit at a `$` path. The containers missing on the way are made, an object where the next
    /// step is a key and an array where it is not, but are only hung in the document once the edit
    /// at the end of the path is made, so a set that ends up changing nothing leaves nothing behind.
    fn edit_steps(&mut self, steps: &[Step], edit: Edit, value: usize) {
        let mut at = 0;
        let mut made: Option<(usize, usize, usize)> = None;
        for (index, pair) in steps.windows(2).enumerate() {
            let (step, next) = (&pair[0], &pair[1]);
            let child = match (&self.nodes[at], step) {
                (Node::Object(members), Step::Key(key)) => {
                    members.iter().find(|(name, _)| name == key).map(|member| member.1)
                }
                (Node::Array(items), _) => match landing(items.len(), step) {
                    Some(position) => items.get(position).copied(),
                    None => return,
                },
                _ => return,
            };
            at = match child {
                Some(child) => child,
                None if edit.creates() => {
                    let fresh = self.push(if matches!(next, Step::Key(_)) {
                        Node::Object(Vec::new())
                    } else {
                        Node::Array(Vec::new())
                    });
                    match made {
                        None => made = Some((at, index, fresh)),
                        Some(_) => self.hang(at, step, fresh),
                    }
                    fresh
                }
                None => return,
            };
        }
        let Some(last) = steps.last() else { return };
        if self.edit_at(at, last, edit, value)
            && let Some((parent, index, fresh)) = made
        {
            self.hang(parent, &steps[index], fresh);
        }
    }

    /// The edit at the last step of a path, and whether it changed anything.
    fn edit_at(&mut self, parent: usize, step: &Step, edit: Edit, value: usize) -> bool {
        match (&mut self.nodes[parent], step) {
            (Node::Object(members), Step::Key(key)) => {
                let there = members.iter().any(|(name, _)| name == key);
                match edit {
                    Edit::Insert if there => false,
                    Edit::Replace | Edit::Remove if !there => false,
                    Edit::Remove => {
                        members.retain(|(name, _)| name != key);
                        true
                    }
                    _ => {
                        self.put(parent, key, value);
                        true
                    }
                }
            }
            (Node::Array(items), _) => {
                let Some(position) = landing(items.len(), step) else { return false };
                edit_item(items, position, edit, value)
            }
            _ => false,
        }
    }

    /// An edit at a JSON pointer, which is read the way yyjson reads one: the containers missing on
    /// the way are objects whatever the token, and are made for a set or an insert only.
    fn edit_pointer(&mut self, pointer: &str, edit: Edit, value: usize) {
        let mut tokens = Vec::new();
        for raw in pointer[1..].split('/') {
            let mut token = String::with_capacity(raw.len());
            let mut chars = raw.chars();
            while let Some(next) = chars.next() {
                if next != '~' {
                    token.push(next);
                    continue;
                }
                match chars.next() {
                    Some('0') => token.push('~'),
                    Some('1') => token.push('/'),
                    _ => return,
                }
            }
            tokens.push(token);
        }
        let Some((last, walk)) = tokens.split_last() else { return };
        let mut at = 0;
        for token in walk {
            let child = match &self.nodes[at] {
                Node::Object(members) => {
                    members.iter().find(|(name, _)| name == token).map(|m| m.1)
                }
                Node::Array(items) => match pointer_landing(items.len(), token) {
                    Some(position) => items.get(position).copied(),
                    None => return,
                },
                _ => return,
            };
            at = match child {
                Some(child) => child,
                None if edit.creates() => {
                    let fresh = self.push(Node::Object(Vec::new()));
                    match &mut self.nodes[at] {
                        Node::Array(items) => items.push(fresh),
                        _ => self.put(at, token, fresh),
                    }
                    fresh
                }
                None => return,
            };
        }
        match &mut self.nodes[at] {
            Node::Object(members) => {
                let there = members.iter().any(|(name, _)| name == last);
                match edit {
                    Edit::Insert if there => {}
                    Edit::Replace | Edit::Remove if !there => {}
                    Edit::Remove => members.retain(|(name, _)| name != last),
                    _ => self.put(at, last, value),
                }
            }
            Node::Array(items) => {
                if let Some(position) = pointer_landing(items.len(), last) {
                    edit_item(items, position, edit, value);
                }
            }
            _ => {}
        }
    }
}

/// An edit at a position in an array that is at most one past its end, and whether it changed
/// anything. Only a set and an insert write past the end, and only an insert leaves an element that
/// is there alone.
fn edit_item(items: &mut Vec<usize>, position: usize, edit: Edit, value: usize) -> bool {
    let there = position < items.len();
    match edit {
        Edit::Set | Edit::Insert if !there => items.push(value),
        Edit::Set | Edit::Replace if there => items[position] = value,
        Edit::Remove if there => {
            items.remove(position);
        }
        _ => return false,
    }
    true
}

/// Two documents in one, the second after the first, and where the second's root is now, so that a
/// value made out of both can point into either without copying.
fn joined(mut first: Document, second: &Document) -> (Document, usize) {
    let offset = first.nodes.len();
    first.nodes.extend(second.nodes.iter().map(|node| match node {
        Node::Array(children) => Node::Array(children.iter().map(|at| at + offset).collect()),
        Node::Object(children) => {
            Node::Object(children.iter().map(|(key, at)| (key.clone(), at + offset)).collect())
        }
        other => other.clone(),
    }));
    (first, offset)
}

/// Where one of a document's values comes from while a merge or a diff is put together: a slot
/// that is to hold the merge of a value, if there is one, with another.
type Pending = (usize, Option<usize>, usize);

/// An array, or an object, that a needle is being matched against: the needle's child it is on and,
/// for an array, the haystack's element that child is being tried against.
#[derive(Debug, Clone, Copy)]
struct Matching {
    haystack: usize,
    needle: usize,
    looking: usize,
    trying: usize,
}

impl Document {
    fn is_object(&self, at: usize) -> bool {
        matches!(self.nodes[at], Node::Object(_))
    }

    fn is_null(&self, at: usize) -> bool {
        matches!(self.nodes[at], Node::Null)
    }

    /// The first value an object has for a key, which is the one yyjson finds.
    fn member(&self, object: usize, key: &str) -> Option<usize> {
        match &self.nodes[object] {
            Node::Object(children) => {
                children.iter().find(|(name, _)| name == key).map(|entry| entry.1)
            }
            _ => None,
        }
    }

    fn members(&self, object: usize) -> Vec<(String, usize)> {
        match &self.nodes[object] {
            Node::Object(children) => children.clone(),
            _ => Vec::new(),
        }
    }

    fn slot(&mut self) -> usize {
        self.nodes.push(Node::Null);
        self.nodes.len() - 1
    }

    /// `json_merge_patch`, which is RFC 7386 when both sides are objects and the patch otherwise,
    /// as MySQL's is. A null in the patch takes the key out and an object in it merges into what
    /// the key held, and the keys the patch does not name come first in the order they were in.
    fn merge_patch(&mut self, original: usize, patch: usize) -> usize {
        if !self.is_object(original) || !self.is_object(patch) {
            return patch;
        }
        let root = self.slot();
        let mut pending: Vec<Pending> = vec![(root, Some(original), patch)];
        while let Some((slot, original, patch)) = pending.pop() {
            if !self.is_object(patch) {
                self.nodes[slot] = self.nodes[patch].clone();
                continue;
            }
            let original = original.filter(|&at| self.is_object(at));
            let mut entries = Vec::new();
            if let Some(original) = original {
                for (key, value) in self.members(original) {
                    if self.member(patch, &key).is_none() {
                        entries.push((key, value));
                    }
                }
            }
            for (key, value) in self.members(patch) {
                if self.is_null(value) {
                    continue;
                }
                let held = original.and_then(|original| self.member(original, &key));
                let child = self.slot();
                pending.push((child, held, value));
                entries.push((key, child));
            }
            self.nodes[slot] = Node::Object(entries);
        }
        root
    }

    /// `json_deep_merge`, where a null in the patch keeps what was there rather than taking it out
    /// and objects on both sides merge.
    fn deep_merge(&mut self, original: usize, patch: usize) -> usize {
        if !self.is_object(original) || !self.is_object(patch) {
            return if self.is_null(patch) { original } else { patch };
        }
        let root = self.slot();
        let mut pending: Vec<Pending> = vec![(root, Some(original), patch)];
        while let Some((slot, Some(original), patch)) = pending.pop() {
            let mut entries = Vec::new();
            for (key, value) in self.members(original) {
                if self.member(patch, &key).is_none_or(|at| self.is_null(at)) {
                    entries.push((key, value));
                }
            }
            for (key, value) in self.members(patch) {
                if self.is_null(value) {
                    continue;
                }
                match self.member(original, &key) {
                    Some(held) if self.is_object(held) && self.is_object(value) => {
                        let child = self.slot();
                        pending.push((child, Some(held), value));
                        entries.push((key, child));
                    }
                    _ => entries.push((key, value)),
                }
            }
            self.nodes[slot] = Node::Object(entries);
        }
        root
    }

    /// `json_merge_patch_diff`, the smallest patch that makes the old document the new one: a key
    /// taken out is a null, a value that changed is the new one, two objects are the diff of them
    /// and are left out when it is empty, and a key whose value is the same is left out.
    fn diff(&mut self, old: usize, new: usize) -> usize {
        if !self.is_object(old) || !self.is_object(new) {
            return new;
        }
        let root = self.slot();
        let mut made = Vec::new();
        let mut pending: Vec<Pending> = vec![(root, Some(old), new)];
        while let Some((slot, Some(old), new)) = pending.pop() {
            made.push(slot);
            let mut entries = Vec::new();
            for (key, _) in self.members(old) {
                if self.member(new, &key).is_none() {
                    let removed = self.slot();
                    entries.push((key, removed));
                }
            }
            for (key, value) in self.members(new) {
                match self.member(old, &key) {
                    Some(held) if self.is_object(held) && self.is_object(value) => {
                        let child = self.slot();
                        pending.push((child, Some(held), value));
                        entries.push((key, child));
                    }
                    Some(held) if self.equal(held, value) => {}
                    _ => entries.push((key, value)),
                }
            }
            self.nodes[slot] = Node::Object(entries);
        }
        // An object is made after the one that holds it, so going back over them sees every one
        // after everything in it, and an object left with nothing in it goes from its parent.
        let mut empty = vec![false; self.nodes.len()];
        for &slot in made.iter().rev() {
            if let Node::Object(entries) = &mut self.nodes[slot] {
                entries.retain(|(_, at)| !empty[*at]);
                empty[slot] = entries.is_empty() && slot != root;
            }
        }
        root
    }

    /// Whether two values are the same as yyjson compares them: numbers by their kind and bits, so
    /// `1` is not `1.0`, objects by their keys in any order and arrays element by element.
    fn equal(&self, left: usize, right: usize) -> bool {
        let mut pending = vec![(left, right)];
        while let Some((left, right)) = pending.pop() {
            let same = match (&self.nodes[left], &self.nodes[right]) {
                (Node::Null, Node::Null) => true,
                (Node::Bool(a), Node::Bool(b)) => a == b,
                (Node::Unsigned(a), Node::Unsigned(b)) => a == b,
                (Node::Signed(a), Node::Signed(b)) => a == b,
                (Node::Unsigned(a), Node::Signed(b)) | (Node::Signed(b), Node::Unsigned(a)) => {
                    u64::try_from(*b).is_ok_and(|b| b == *a)
                }
                (Node::Real(a, _), Node::Real(b, _)) => a.to_bits() == b.to_bits(),
                (Node::Raw(a), Node::Raw(b)) | (Node::Str(a), Node::Str(b)) => a == b,
                (Node::Array(a), Node::Array(b)) => {
                    pending.extend(a.iter().copied().zip(b.iter().copied()));
                    a.len() == b.len()
                }
                (Node::Object(a), Node::Object(b)) => {
                    a.len() == b.len()
                        && a.iter().all(|(key, at)| match self.member(right, key) {
                            Some(other) => {
                                pending.push((*at, other));
                                true
                            }
                            None => false,
                        })
                }
                _ => false,
            };
            if !same {
                return false;
            }
        }
        true
    }

    /// Whether a needle is in a value the way `json_contains` reads it: the same value, or an
    /// array with an element in it for each of the needle's, or an object with each of the
    /// needle's keys holding what the needle's does.
    fn fuzzy(&self, haystack: usize, needle: usize) -> bool {
        // `answer` is what the last match that finished said, which the frame under it reads.
        let mut stack: Vec<Matching> = Vec::new();
        let mut answer = self.begin(haystack, needle, &mut stack);
        while let Some(&frame) = stack.last() {
            let Matching { haystack, needle, mut looking, mut trying } = frame;
            match (&self.nodes[haystack], &self.nodes[needle]) {
                (Node::Array(elements), Node::Array(wanted)) => {
                    match answer.take() {
                        Some(true) => {
                            looking += 1;
                            trying = 0;
                        }
                        Some(false) => trying += 1,
                        None => {}
                    }
                    if looking == wanted.len() || trying == elements.len() {
                        stack.pop();
                        answer = Some(looking == wanted.len());
                        continue;
                    }
                    *stack.last_mut().expect("the frame is there") =
                        Matching { haystack, needle, looking, trying };
                    answer = self.begin(elements[trying], wanted[looking], &mut stack);
                }
                (Node::Object(_), Node::Object(wanted)) => {
                    match answer.take() {
                        Some(false) => {
                            stack.pop();
                            answer = Some(false);
                            continue;
                        }
                        Some(true) => looking += 1,
                        None => {}
                    }
                    let held =
                        wanted.get(looking).map(|(key, at)| (self.member(haystack, key), *at));
                    match held {
                        None => {
                            stack.pop();
                            answer = Some(true);
                        }
                        Some((None, _)) => {
                            stack.pop();
                            answer = Some(false);
                        }
                        Some((Some(held), wanted)) => {
                            *stack.last_mut().expect("the frame is there") =
                                Matching { haystack, needle, looking, trying };
                            answer = self.begin(held, wanted, &mut stack);
                        }
                    }
                }
                _ => {
                    stack.pop();
                    answer = Some(false);
                }
            }
        }
        answer.unwrap_or(false)
    }

    /// Starts matching a needle against a value, answering at once when the two are equal or cannot
    /// match, and otherwise pushing the frame that goes through the needle's children.
    fn begin(&self, haystack: usize, needle: usize, stack: &mut Vec<Matching>) -> Option<bool> {
        if self.equal(haystack, needle) {
            return Some(true);
        }
        match (&self.nodes[haystack], &self.nodes[needle]) {
            (Node::Array(_), Node::Array(_)) | (Node::Object(_), Node::Object(_)) => {
                stack.push(Matching { haystack, needle, looking: 0, trying: 0 });
                None
            }
            _ => Some(false),
        }
    }
}

/// What `json_structure` says one kind of value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Null,
    Boolean,
    UBigInt,
    BigInt,
    HugeInt,
    Double,
    Varchar,
    List,
    Struct,
}

impl Shape {
    fn of(node: &Node) -> Self {
        match node {
            Node::Null => Self::Null,
            Node::Bool(_) => Self::Boolean,
            Node::Unsigned(_) => Self::UBigInt,
            Node::Signed(_) => Self::BigInt,
            Node::Real(..) | Node::Raw(_) => Self::Double,
            Node::Str(_) => Self::Varchar,
            Node::Array(_) => Self::List,
            Node::Object(_) => Self::Struct,
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::UBigInt | Self::BigInt | Self::HugeInt | Self::Double)
    }

    /// The number type two different ones meet at, where a signed and an unsigned one need a
    /// `HUGEINT` between them.
    fn widest(self, other: Self) -> Self {
        if self == Self::Double || other == Self::Double {
            Self::Double
        } else if self == Self::HugeInt
            || other == Self::HugeInt
            || matches!(
                (self, other),
                (Self::BigInt, Self::UBigInt) | (Self::UBigInt, Self::BigInt)
            )
        {
            Self::HugeInt
        } else {
            Self::BigInt
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Boolean => "BOOLEAN",
            Self::UBigInt => "UBIGINT",
            Self::BigInt => "BIGINT",
            Self::HugeInt => "HUGEINT",
            Self::Double => "DOUBLE",
            Self::Varchar => "VARCHAR",
            Self::List => "LIST",
            Self::Struct => "STRUCT",
        }
    }
}

/// One kind of value seen at a place in a document, and for a list or a struct where its element or
/// its fields are described.
#[derive(Debug)]
struct Described {
    shape: Shape,
    element: Option<usize>,
    fields: Vec<(String, usize)>,
}

/// Every kind of value seen at one place, in the order they were first seen. More than one kind is
/// a `JSON` in the answer.
#[derive(Debug, Default)]
struct Place {
    kinds: Vec<Described>,
}

/// The places of a structure, the root first.
#[derive(Debug)]
struct Structure {
    places: Vec<Place>,
}

impl Structure {
    fn place(&mut self) -> usize {
        self.places.push(Place::default());
        self.places.len() - 1
    }

    /// The kind a value of a shape is at a place, which is the pin's rules: a null alone is replaced
    /// by what comes next, a null after anything is not recorded, and two numbers are one kind.
    fn kind(&mut self, place: usize, shape: Shape) -> usize {
        let kinds = &mut self.places[place].kinds;
        let fresh = Described { shape, element: None, fields: Vec::new() };
        if kinds.is_empty() {
            kinds.push(fresh);
            return 0;
        }
        if kinds.len() == 1 && kinds[0].shape == Shape::Null {
            kinds[0].shape = shape;
            return 0;
        }
        if shape == Shape::Null {
            return kinds.len() - 1;
        }
        for (index, kind) in kinds.iter_mut().enumerate() {
            if kind.shape == shape {
                return index;
            }
            if shape.numeric() && kind.shape.numeric() {
                kind.shape = shape.widest(kind.shape);
                return index;
            }
        }
        kinds.push(fresh);
        kinds.len() - 1
    }
}

impl Document {
    /// `json_structure`: the type of every place in the document, as the pin infers it when it
    /// reads one, with a list's elements merged into one and the objects at a place merged into
    /// one struct whose fields are every key any of them has.
    fn structure(&self) -> String {
        let mut structure = Structure { places: vec![Place::default()] };
        // In the order a walk down the document meets values, which is the order the pin sees
        // them in and so the order the keys of a struct come in.
        let mut pending = vec![(0, 0)];
        while let Some((at, place)) = pending.pop() {
            let shape = Shape::of(&self.nodes[at]);
            let kind = structure.kind(place, shape);
            match &self.nodes[at] {
                Node::Array(children) => {
                    let element = match structure.places[place].kinds[kind].element {
                        Some(element) => element,
                        None => {
                            let element = structure.place();
                            structure.places[place].kinds[kind].element = Some(element);
                            element
                        }
                    };
                    pending.extend(children.iter().rev().map(|&child| (child, element)));
                }
                Node::Object(children) => {
                    let mut found = Vec::with_capacity(children.len());
                    for (key, child) in children {
                        let known = structure.places[place].kinds[kind]
                            .fields
                            .iter()
                            .find(|(name, _)| name == key)
                            .map(|field| field.1);
                        let field = match known {
                            Some(field) => field,
                            None => {
                                let field = structure.place();
                                structure.places[place].kinds[kind]
                                    .fields
                                    .push((key.clone(), field));
                                field
                            }
                        };
                        found.push((*child, field));
                    }
                    pending.extend(found.into_iter().rev());
                }
                _ => {}
            }
        }
        // The answer is a document of its own, one node for each place, written by the writer.
        let mut answer = Document { nodes: vec![Node::Null] };
        let mut pending = vec![(0, 0)];
        while let Some((place, slot)) = pending.pop() {
            let kinds = &structure.places[place].kinds;
            let named = |name: &str| Node::Str(name.to_string());
            answer.nodes[slot] = match kinds.as_slice() {
                [] => named("NULL"),
                [kind] => match kind.shape {
                    Shape::List => {
                        let child = answer.slot();
                        pending.push((kind.element.unwrap_or_default(), child));
                        Node::Array(vec![child])
                    }
                    Shape::Struct if kind.fields.is_empty() => named("JSON"),
                    Shape::Struct => {
                        let mut entries = Vec::with_capacity(kind.fields.len());
                        for (key, field) in &kind.fields {
                            let child = answer.slot();
                            pending.push((*field, child));
                            entries.push((key.clone(), child));
                        }
                        Node::Object(entries)
                    }
                    shape => named(shape.name()),
                },
                _ => named("JSON"),
            };
        }
        answer.minified()
    }
}

impl Document {
    /// Puts the keys of every object in byte order, keeping a repeated key's values in the order
    /// they were written, which is what `json_normalize` does before it writes a document out.
    fn sort_keys(&mut self) {
        for node in &mut self.nodes {
            if let Node::Object(members) = node {
                members.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            }
        }
    }
}

/// One row of `json_each` or `json_tree`, in the order of the columns the pin gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The key in its object, the index in its array as text, or nothing for the value the walk
    /// started at.
    pub key: Option<String>,
    /// The value written out.
    pub value: String,
    /// What `json_type` says the value is.
    pub kind: &'static str,
    /// The value written out when it is a scalar other than null.
    pub atom: Option<String>,
    /// Where the value sits in the pin's reading of the whole document, counting keys too.
    pub id: u64,
    /// The id of the container the value is in, which only `json_tree` says.
    pub parent: Option<u64>,
    /// The path to the value.
    pub fullkey: String,
    /// The path to the container the value is in.
    pub path: String,
}

/// The pin's refusal of a path `json_each` and `json_tree` cannot start from.
fn not_from_root() -> Error {
    Error::binder("JSON path must start with '$' for json_each/json_tree")
}

/// The rows `json_each` makes of a document, or `json_tree` when `tree` is set, starting at the
/// value a path picks, which is the whole document when there is no path.
///
/// `json_each` makes one row for each value inside the one the path picks, or one for that value
/// when it is a scalar. `json_tree` makes one for it and then one for everything under it, depth
/// first. A path that picks nothing makes no rows.
///
/// # Errors
///
/// A path that is not a `$` path or has a wildcard in it, as the pin's binder errors although the
/// pin raises them for each row, and a malformed document.
pub fn entries(document_text: &str, path: Option<&str>, tree: bool) -> Result<Vec<Entry>> {
    let base = match path {
        None => "$".to_string(),
        Some(text) => match written_path(text, true)? {
            Path::Wild(_) => {
                return Err(Error::binder(
                    "Wildcard JSON path not supported in json_each/json_tree",
                ));
            }
            Path::Root | Path::Pointer(_) => return Err(not_from_root()),
            Path::Steps(_) if text.starts_with('$') => text.to_string(),
            // A bare key, which the pin reads as the quoted key it stands for and then writes out
            // that way in every path it gives back.
            Path::Steps(_) => format!("$.\"{text}\""),
        },
    };
    let document = document(document_text)?;
    let Path::Steps(steps) = written_path(&base, true)? else {
        return Err(Error::internal("a json_each path that is not a $ path"));
    };
    let Some(start) = steps.iter().try_fold(0, |at, step| document.child(at, step)) else {
        return Ok(Vec::new());
    };
    let ids = document.yyjson_ids();
    let mut rows = Vec::new();
    let container = matches!(document.nodes[start], Node::Array(_) | Node::Object(_));
    if !container || tree {
        rows.push(document.entry(&ids, start, None, None, base.clone(), base.clone()));
    }
    if !container {
        return Ok(rows);
    }
    // The containers being walked, each with its own path and how far into it the walk is, so a
    // document a million deep is walked without recursing.
    let mut stack: Vec<(usize, String, usize)> = vec![(start, base, 0)];
    while let Some((parent, path, next)) = stack.last_mut() {
        let parent = *parent;
        let found = match &document.nodes[parent] {
            Node::Array(items) => items.get(*next).map(|&item| (None, item)),
            Node::Object(members) => members.get(*next).map(|(key, value)| (Some(key), *value)),
            _ => None,
        };
        let Some((key, child)) = found else {
            stack.pop();
            continue;
        };
        let index = *next;
        *next += 1;
        let path = path.clone();
        let mut fullkey = path.clone();
        let key = match key {
            Some(key) => {
                push_path_key(key, &mut fullkey);
                key.to_string()
            }
            None => {
                fullkey.push_str(&format!("[{index}]"));
                index.to_string()
            }
        };
        let above = tree.then(|| ids[parent]);
        rows.push(document.entry(&ids, child, Some(key), above, fullkey.clone(), path));
        if tree && matches!(document.nodes[child], Node::Array(_) | Node::Object(_)) {
            stack.push((child, fullkey, 0));
        }
    }
    Ok(rows)
}

/// Writes `.key` onto a path, quoting the key unless it is an ASCII letter followed by ASCII
/// letters, digits and underscores, which is when the pin leaves it bare.
fn push_path_key(key: &str, path: &mut String) {
    path.push('.');
    let bytes = key.as_bytes();
    let bare = bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes[1..].iter().all(|&byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if bare {
        path.push_str(key);
        return;
    }
    path.push('"');
    for character in key.chars() {
        if character == '"' || character == '\\' {
            path.push('\\');
        }
        path.push(character);
    }
    path.push('"');
}

impl Document {
    /// The position yyjson gives every value, which is its place in a walk of the document in order
    /// where an object's keys take a place each too.
    fn yyjson_ids(&self) -> Vec<u64> {
        let mut ids = vec![0; self.nodes.len()];
        let mut next = 0;
        let mut stack = vec![0];
        while let Some(at) = stack.pop() {
            ids[at] = next;
            next += 1;
            match &self.nodes[at] {
                Node::Array(items) => stack.extend(items.iter().rev()),
                // The key takes the place before its value, which is the same as the value taking
                // one more.
                Node::Object(members) => {
                    for (_, value) in members.iter().rev() {
                        stack.push(*value);
                        stack.push(usize::MAX);
                    }
                }
                _ => {}
            }
            while stack.last() == Some(&usize::MAX) {
                stack.pop();
                next += 1;
            }
        }
        ids
    }

    fn entry(
        &self,
        ids: &[u64],
        at: usize,
        key: Option<String>,
        parent: Option<u64>,
        fullkey: String,
        path: String,
    ) -> Entry {
        let node = &self.nodes[at];
        let value = self.written(at);
        let atom = match node {
            Node::Null | Node::Array(_) | Node::Object(_) => None,
            _ => Some(value.clone()),
        };
        Entry { key, value, kind: type_name(node), atom, id: ids[at], parent, fullkey, path }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(text: &str) -> String {
        read(text).unwrap_err().describe(text)
    }

    fn round(text: &str) -> String {
        read(text).unwrap().minified()
    }

    #[test]
    fn a_document_is_written_back_minified() {
        assert_eq!(
            round(" { \"a\" : [1, 2.5, \"x\\u001fy\", null, true,], \"b\": {} } "),
            "{\"a\":[1,2.5,\"x\\u001Fy\",null,true],\"b\":{}}"
        );
        assert_eq!(
            round(
                "[1e-7, 1E+2, 0.5, 10.0, -0, 1.0e400, 18446744073709551616, -9223372036854775809]"
            ),
            "[1e-7,100.0,0.5,10.0,0,1.0e400,18446744073709551616,-9223372036854775809]"
        );
        assert_eq!(round("[nan, -Infinity, inf]"), "[nan,-Infinity,inf]");
        assert_eq!(round("\"\\u00e9\\/\\ud83d\\ude00\""), "\"é/😀\"");
    }

    #[test]
    fn a_malformed_document_is_refused_where_the_pin_refuses_it() {
        assert_eq!(
            refused("{\"a\":"),
            "Malformed JSON at byte 5 of input: unexpected end of data.  Input: \"{\"a\":\""
        );
        assert_eq!(
            refused("x\"y"),
            "Malformed JSON at byte 0 of input: unexpected character.  Input: \"x\"y\""
        );
        assert_eq!(read("").unwrap_err().message, "input length is 0");
        assert_eq!(read("   ").unwrap_err().message, "input data is empty");
        assert_eq!(
            read("tru").unwrap_err(),
            Malformed { at: 3, message: "unexpected end of data" }
        );
        assert_eq!(read("trux").unwrap_err(), Malformed { at: 0, message: "invalid literal" });
        assert_eq!(read("01").unwrap_err().message, "number with leading zero is not allowed");
        assert_eq!(
            read("1 2").unwrap_err(),
            Malformed { at: 2, message: "unexpected content after document" }
        );
        assert_eq!(read("infin").unwrap_err().message, "unexpected end of data");
        assert_eq!(
            read("\"\\x\"").unwrap_err(),
            Malformed { at: 2, message: "invalid escaped character in string" }
        );
    }

    #[test]
    fn deep_nesting_does_not_recurse() {
        let deep = format!("{}{}", "[".repeat(200_000), "]".repeat(200_000));
        assert_eq!(round(&deep), deep);
    }

    #[test]
    fn doubles_are_written_as_yyjson_writes_them() {
        let text = |number: f64| {
            let mut out = String::new();
            real_text(number, &mut out);
            out
        };
        assert_eq!(text(1e-7), "1e-7");
        assert_eq!(text(1e21), "1e21");
        assert_eq!(text(1e20), "100000000000000000000.0");
        assert_eq!(text(0.000_001), "0.000001");
        assert_eq!(text(1.5e-10), "1.5e-10");
        assert_eq!(text(-0.0), "-0.0");
        assert_eq!(text(f64::from(0.1_f32)), "0.10000000149011612");
    }

    #[test]
    fn paths_pick_what_the_pin_picks() {
        let document = read("{\"a\":{\"b\":[1,2,3]},\"c\":{\"a\":3},\"a b\":4,\"m~n\":5}").unwrap();
        let find = |path: &str| {
            document.find(&written_path(path, true).unwrap()).map(|at| document.written(at))
        };
        assert_eq!(find("$.a.b[0]"), Some("1".into()));
        assert_eq!(find("$.a.b[-1]"), Some("3".into()));
        assert_eq!(find("$.a.b[#-1]"), Some("3".into()));
        assert_eq!(find("$.a.b[#]"), None);
        assert_eq!(find("a b"), Some("4".into()));
        assert_eq!(find("/m~0n"), Some("5".into()));
        assert_eq!(find(""), Some(document.minified()));
        let every = |path: &str| {
            document.every(path).into_iter().map(|at| document.written(at)).collect::<Vec<_>>()
        };
        assert_eq!(every("$..a"), ["{\"b\":[1,2,3]}", "3"]);
        assert_eq!(every("$.a.b[*]"), ["1", "2", "3"]);
        let error = |path: &str| parse_path(path, true).unwrap_err().to_string();
        assert_eq!(error("$."), "Binder Error: JSON path error near '.'");
        assert_eq!(error("$a"), "Binder Error: JSON path error near 'a'");
        assert_eq!(error("$.a[1"), "Binder Error: JSON path error near '[1'");
    }
}
