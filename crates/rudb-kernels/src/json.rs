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
//! is read, written and walked without running out of stack.

use rudb_common::{Error, LogicalType, Result, SessionTimeZone, Value};
use rudb_vector::Vector;

use crate::cast::cast_value;

/// One value in a document.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Null,
    Bool(bool),
    /// A whole number that is not negative and fits in 64 bits.
    Unsigned(u64),
    /// A negative whole number that fits in 64 bits, and `-0`.
    Signed(i64),
    Real(f64),
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
                Ok(value) if value.is_finite() => Node::Real(value),
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
                if !children.contains(&child) {
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
    let mut reader = Reader { bytes: text.as_bytes(), cur: 0 };
    let mut nodes = Vec::new();
    let outcome = (|| {
        if text.is_empty() {
            return Err((0, Kind::Empty, "input length is 0"));
        }
        reader.skip();
        if reader.cur >= reader.bytes.len() {
            return Err((0, Kind::Empty, "input data is empty"));
        }
        match reader.at(reader.cur) {
            b'{' | b'[' => reader.container(&mut nodes)?,
            _ => {
                let node = reader.scalar()?;
                nodes.push(node);
            }
        }
        reader.skip();
        if reader.cur < reader.bytes.len() {
            return Err((reader.cur, Kind::Content, "unexpected content after document"));
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => Ok(Document { nodes }),
        Err((at, kind, message)) => {
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
        // Each entry is a container and how many of its children are written.
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut next = Some(root);
        loop {
            if let Some(at) = next.take() {
                match &self.nodes[at] {
                    Node::Array(_) => {
                        out.push('[');
                        stack.push((at, 0));
                    }
                    Node::Object(_) => {
                        out.push('{');
                        stack.push((at, 0));
                    }
                    node => scalar_text(node, out),
                }
            }
            let Some((at, done)) = stack.last_mut() else { return };
            let (count, child) = match &self.nodes[*at] {
                Node::Array(children) => (children.len(), children.get(*done).copied()),
                Node::Object(children) => {
                    (children.len(), children.get(*done).map(|entry| entry.1))
                }
                _ => (0, None),
            };
            if *done == count {
                out.push(if matches!(self.nodes[*at], Node::Array(_)) { ']' } else { '}' });
                stack.pop();
                continue;
            }
            if *done > 0 {
                out.push(',');
            }
            if let Node::Object(children) = &self.nodes[*at] {
                string_text(&children[*done].0, out);
                out.push(':');
            }
            *done += 1;
            next = child;
        }
    }

    fn written(&self, at: usize) -> String {
        let mut out = String::new();
        self.write(at, &mut out);
        out
    }
}

fn scalar_text(node: &Node, out: &mut String) {
    match node {
        Node::Null => out.push_str("null"),
        Node::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Node::Unsigned(number) => out.push_str(&number.to_string()),
        Node::Signed(number) => out.push_str(&number.to_string()),
        Node::Real(number) => real_text(*number, out),
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
        (Value::Struct(fields), _) => {
            let types = match ty {
                LogicalType::Struct(types) | LogicalType::Union(types) => Some(types),
                _ => None,
            };
            if let (LogicalType::Union(_), Some(types)) = (ty, types) {
                // A union is the one member it holds, which is the first that is not null after
                // the tag.
                let member =
                    fields.iter().skip(1).zip(types).find(|((_, value), _)| !value.is_null());
                return match member {
                    Some(((_, value), field)) => value_text(value, &field.ty, zone, out),
                    None => {
                        out.push_str("null");
                        Ok(())
                    }
                };
            }
            out.push('{');
            for (index, (name, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                string_text(name, out);
                out.push(':');
                let field = types.and_then(|types| types.get(index)).map(|field| &field.ty);
                let held = item.logical_type();
                value_text(item, field.unwrap_or(&held), zone, out)?;
            }
            out.push('}');
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
        Node::Real(_) | Node::Raw(_) => "DOUBLE",
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
    document.convert(0, target, try_cast)
}

impl Document {
    /// The value at a position read as a type. A `TRY_CAST` is lenient the way the pin's is, where
    /// what does not convert is a null in its place rather than the whole value, so a struct read
    /// from an array is a struct of nulls and a key the struct has no field for is passed over.
    fn convert(&self, at: usize, target: &LogicalType, lenient: bool) -> Result<Value> {
        let node = &self.nodes[at];
        if matches!(node, Node::Null) {
            return Ok(Value::Null);
        }
        let outcome = self.converted(at, target, lenient);
        if lenient && outcome.is_err() {
            if let LogicalType::Struct(fields) = target {
                let nulls = fields.iter().map(|field| (field.name.clone(), Value::Null)).collect();
                return Ok(Value::Struct(nulls));
            }
            return Ok(Value::Null);
        }
        outcome
    }

    fn converted(&self, at: usize, target: &LogicalType, lenient: bool) -> Result<Value> {
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
                    .map(|&child| self.convert(child, element, lenient))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::List { element: element.as_ref().clone(), values })
            }
            LogicalType::Struct(fields) => {
                let Node::Object(children) = node else { return Err(expected("OBJECT")) };
                for (key, _) in children {
                    if !lenient && !fields.iter().any(|field| field.name == *key) {
                        return Err(Error::conversion(format!(
                            "Object {} has unknown key \"{key}\"",
                            self.written(at)
                        )));
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
                    values.push((field.name.clone(), self.convert(*child, &field.ty, lenient)?));
                }
                Ok(Value::Struct(values))
            }
            LogicalType::Map(key, value) => {
                let Node::Object(children) = node else { return Err(expected("OBJECT")) };
                let entries = children
                    .iter()
                    .map(|(name, child)| {
                        let name = cast_value(&Value::Varchar(name.clone()), key, false)?;
                        Ok((name, self.convert(*child, value, lenient)?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::map(key.as_ref().clone(), value.as_ref().clone(), entries))
            }
            _ if target.is_numeric() => {
                let failed = || {
                    Error::conversion(format!(
                        "Failed to cast value to numerical: {}",
                        self.written(at)
                    ))
                };
                let scalar = self.scalar_value(node).ok_or_else(failed)?;
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

    /// The SQL value a scalar of a document is, and nothing for a container.
    fn scalar_value(&self, node: &Node) -> Option<Value> {
        Some(match node {
            Node::Null => Value::Null,
            Node::Bool(flag) => Value::Boolean(*flag),
            Node::Unsigned(number) => Value::UBigInt(*number),
            Node::Signed(number) => Value::BigInt(*number),
            Node::Real(number) => Value::Double(*number),
            Node::Raw(text) | Node::Str(text) => Value::Varchar(text.clone()),
            Node::Array(_) | Node::Object(_) => return None,
        })
    }
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
