//! Reading a list, a struct or a map out of the text it prints as.
//!
//! `'[1, 2]'::INTEGER[]`, `'{a: 1}'::STRUCT(a INTEGER)` and `'{a=1}'::MAP(VARCHAR, INTEGER)` split
//! the text into the pieces of the value, each still text, and the cast then takes every piece to
//! the type it has to be. This file is the splitting and nothing else, and it follows the pin's
//! splitter byte for byte, because the rules are not the ones anybody would guess. Quotes of either
//! kind group a piece and are dropped from it, a backslash before a quote keeps the quote, a
//! bracket of any kind keeps its commas to itself and stays in the piece, an unquoted `NULL` in any
//! case is a null, and an empty piece is the empty string rather than a null.
//!
//! Every function answers `None` for text that is not the shape it reads, and the cast turns that
//! into the pin's refusal naming the whole string.

/// A cursor over the text, and whether the byte before it was a backslash that escapes this one.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    escaped: bool,
}

/// The pin's whitespace, which is the six ASCII spaces C knows about.
fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Whether the piece between `start` and `end` is the word null, in any case.
fn is_null(buf: &[u8], start: usize, end: usize) -> bool {
    end == start + 4 && buf[start..end].eq_ignore_ascii_case(b"null")
}

fn closer(open: u8) -> u8 {
    match open {
        b'[' => b']',
        b'{' => b'}',
        _ => b')',
    }
}

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Self { buf: text.as_bytes(), pos: 0, escaped: false }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn byte(&self) -> u8 {
        self.buf[self.pos]
    }

    fn skip_space(&mut self) {
        while !self.at_end() && space(self.byte()) {
            self.pos += 1;
            self.escaped = false;
        }
    }

    /// From an opening quote to the matching one, leaving the cursor on it.
    fn skip_quoted(&mut self) -> bool {
        let quote = self.byte();
        self.pos += 1;
        while !self.at_end() {
            let mut escapes = false;
            if self.byte() == b'\\' {
                escapes = !self.escaped;
            } else if self.byte() == quote && !self.escaped {
                return true;
            }
            self.escaped = escapes;
            self.pos += 1;
        }
        false
    }

    /// From an opening bracket to the one that closes it, leaving the cursor on that.
    fn skip_bracketed(&mut self) -> bool {
        let mut open: Vec<u8> = Vec::new();
        while !self.at_end() {
            let mut escapes = false;
            let byte = self.byte();
            if byte == b'"' || byte == b'\'' {
                if !self.escaped && !self.skip_quoted() {
                    return false;
                }
            } else if matches!(byte, b'[' | b'{' | b'(') {
                open.push(closer(byte));
            } else if open.last() == Some(&byte) {
                open.pop();
                if open.is_empty() {
                    return true;
                }
            } else if byte == b'\\' {
                escapes = true;
            }
            self.escaped = escapes;
            self.pos += 1;
        }
        false
    }

    /// One byte of a piece, widening the piece to take it in unless it is space.
    fn step(&mut self, start: &mut Option<usize>, end: &mut usize) -> bool {
        let byte = self.byte();
        let mut escapes = false;
        if byte == b'"' || byte == b'\'' {
            start.get_or_insert(self.pos);
            if !self.escaped && !self.skip_quoted() {
                return false;
            }
            *end = self.pos;
        } else if matches!(byte, b'{' | b'(' | b'[') {
            start.get_or_insert(self.pos);
            if !self.skip_bracketed() {
                return false;
            }
            *end = self.pos;
        } else if byte == b'\\' {
            start.get_or_insert(self.pos);
            escapes = true;
            *end = self.pos;
        } else if !space(byte) {
            start.get_or_insert(self.pos);
            *end = self.pos;
        }
        self.escaped = escapes;
        self.pos += 1;
        true
    }

    /// The piece up to the first of `stops` outside quotes and brackets, as its bounds, or `None`
    /// when the text ends first. An empty piece is `(0, 0)`, which reads as the empty string.
    fn piece(&mut self, stops: &[u8]) -> Option<(usize, usize)> {
        let mut start = None;
        let mut end = 0;
        while !self.at_end() && !stops.contains(&self.byte()) {
            if !self.step(&mut start, &mut end) {
                return None;
            }
        }
        if self.at_end() {
            return None;
        }
        Some(start.map_or((0, 0), |start| (start, end + 1)))
    }

    /// The piece as text, or `None` when it is a null.
    fn text(&self, (start, end): (usize, usize)) -> Option<String> {
        (!is_null(self.buf, start, end)).then(|| unquote(self.buf, start, end, true))
    }

    /// The next piece read as text, `Some(None)` for a null and `None` when the text ends first.
    fn next(&mut self, stops: &[u8]) -> Option<Option<String>> {
        let bounds = self.piece(stops)?;
        Some(self.text(bounds))
    }

    /// Past the closing byte, with nothing but space after it.
    fn finished(&mut self) -> bool {
        self.pos += 1;
        self.skip_space();
        self.at_end()
    }
}

/// A piece with its quotes and escapes taken out.
///
/// Inside a bracket nothing is taken out, since the bracket is a nested value that goes through
/// this again on its own. A struct's keys are not nested values, so for them a bracket is only a
/// character.
fn unquote(buf: &[u8], start: usize, end: usize, scopes: bool) -> String {
    let mut out = Vec::with_capacity(end - start);
    let mut escaped = false;
    let mut quote = None;
    let mut open: Vec<u8> = Vec::new();
    for at in start..end {
        let byte = buf[at];
        if escaped {
            out.push(byte);
            escaped = false;
            continue;
        }
        if open.is_empty() && byte == b'\\' {
            let next_is_quote = at + 1 < end && matches!(buf[at + 1], b'\'' | b'"');
            if quote.is_some() || next_is_quote {
                escaped = true;
                continue;
            }
        }
        if open.is_empty() && (byte == b'\'' || byte == b'"') {
            match quote {
                Some(q) if q == byte => {
                    quote = None;
                    continue;
                }
                None => {
                    quote = Some(byte);
                    continue;
                }
                Some(_) => {}
            }
        }
        if quote.is_none() && open.last() == Some(&byte) {
            open.pop();
        }
        if quote.is_none() && scopes && matches!(byte, b'[' | b'{' | b'(') {
            open.push(closer(byte));
        }
        out.push(byte);
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The elements of `[a, b, c]`, each as text or as a null.
pub(crate) fn list(text: &str) -> Option<Vec<Option<String>>> {
    let mut reader = Reader::new(text);
    reader.skip_space();
    if reader.at_end() || reader.byte() != b'[' {
        return None;
    }
    reader.pos += 1;
    reader.skip_space();
    let mut items = Vec::new();
    while !reader.at_end() {
        let mut start = None;
        let mut end = 0;
        while !reader.at_end() && !matches!(reader.byte(), b',' | b']') {
            if !reader.step(&mut start, &mut end) {
                return None;
            }
        }
        if reader.at_end() {
            return None;
        }
        // `[]` is no elements, and `[1,]` is a one and an empty string.
        if reader.byte() != b']' || start.is_some() || !items.is_empty() {
            items.push(reader.text(start.map_or((0, 0), |start| (start, end + 1))));
        }
        if reader.byte() == b']' {
            break;
        }
        reader.pos += 1;
        reader.skip_space();
    }
    reader.finished().then_some(items)
}

/// The entries of `{k=v, k2=v2}`, keys as text and values as text or a null. A null key is not a
/// map the pin will read.
pub(crate) fn map(text: &str) -> Option<Vec<(String, Option<String>)>> {
    let mut reader = Reader::new(text);
    reader.skip_space();
    if reader.at_end() || reader.byte() != b'{' {
        return None;
    }
    reader.pos += 1;
    reader.skip_space();
    if reader.at_end() {
        return None;
    }
    let mut entries = Vec::new();
    if reader.byte() == b'}' {
        return reader.finished().then_some(entries);
    }
    while !reader.at_end() {
        let key = reader.next(b"=")??;
        reader.pos += 1;
        reader.skip_space();
        let value = reader.next(b",}")?;
        entries.push((key, value));
        if reader.byte() == b'}' {
            break;
        }
        reader.pos += 1;
        reader.skip_space();
    }
    reader.finished().then_some(entries)
}

/// The fields of `{name: value}` or `(value, value)`, in the order of `names`, each as text or a
/// null. A field the text does not give is a null, and a name the struct does not have, or more
/// places than it has fields, is not a struct the pin will read. `unnamed` is a struct whose names
/// are all empty, which only the second spelling reaches.
pub(crate) fn fields(text: &str, names: &[&str], unnamed: bool) -> Option<Vec<Option<String>>> {
    let mut reader = Reader::new(text);
    let mut out = vec![None; names.len()];
    reader.skip_space();
    if reader.at_end() || !matches!(reader.byte(), b'{' | b'(') {
        return None;
    }
    let close = closer(reader.byte());
    reader.pos += 1;
    reader.skip_space();
    if reader.at_end() {
        return None;
    }
    if reader.byte() == close {
        return reader.finished().then_some(out);
    }
    if close == b'}' {
        while !reader.at_end() {
            let (start, end) = named_key(&mut reader)?;
            if is_null(reader.buf, start, end) || unnamed {
                return None;
            }
            let key = unquote(reader.buf, start, end, false);
            let at = names.iter().position(|name| *name == key)?;
            reader.pos += 1;
            reader.skip_space();
            out[at] = reader.next(b",}")?;
            if reader.byte() == b'}' {
                break;
            }
            reader.pos += 1;
            reader.skip_space();
        }
    } else {
        let mut at = 0;
        while !reader.at_end() {
            if at == names.len() {
                return None;
            }
            out[at] = reader.next(b",)")?;
            if reader.byte() == b')' {
                break;
            }
            at += 1;
            reader.pos += 1;
            reader.skip_space();
            // `(1,)` is a one-field tuple and the comma is allowed to trail.
            if !reader.at_end() && reader.byte() == b')' {
                break;
            }
        }
    }
    reader.finished().then_some(out)
}

/// A struct key, up to its colon. Brackets are not special in a key, only quotes and escapes, and
/// a key cannot be empty.
fn named_key(reader: &mut Reader<'_>) -> Option<(usize, usize)> {
    let mut start = None;
    let mut end = 0;
    while !reader.at_end() && reader.byte() != b':' {
        let byte = reader.byte();
        let mut escapes = false;
        if reader.escaped {
            start.get_or_insert(reader.pos);
            end = reader.pos;
        } else if byte == b'"' || byte == b'\'' {
            start.get_or_insert(reader.pos);
            if !reader.skip_quoted() {
                return None;
            }
            end = reader.pos;
        } else if byte == b'\\' {
            start.get_or_insert(reader.pos);
            escapes = true;
            end = reader.pos;
        } else if !space(byte) {
            start.get_or_insert(reader.pos);
            end = reader.pos;
        }
        reader.escaped = escapes;
        reader.pos += 1;
    }
    if reader.at_end() {
        return None;
    }
    Some((start?, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(items: &[&str]) -> Vec<Option<String>> {
        items.iter().map(|item| Some((*item).to_string())).collect()
    }

    #[test]
    fn a_list_splits_the_way_the_pin_splits_it() {
        assert_eq!(list("[]"), Some(vec![]));
        assert_eq!(list("  [ 1 , 2 ]  "), Some(some(&["1", "2"])));
        assert_eq!(list("[1,,2]"), Some(some(&["1", "", "2"])));
        assert_eq!(list("[1,]"), Some(some(&["1", ""])));
        assert_eq!(list("[ null , NULL, \"null\"]"), Some(vec![None, None, Some("null".into())]));
        assert_eq!(
            list(r#"["a, b", 'c', d\,e, [x, y]]"#),
            Some(some(&["a, b", "c", r"d\", "e", "[x, y]"]))
        );
        assert_eq!(list(r#"[\"a]"#), Some(some(&["\"a"])));
        assert_eq!(list(r"[a\b]"), Some(some(&[r"a\b"])));
        assert_eq!(list("[1, 2"), None);
        assert_eq!(list("1, 2]"), None);
        assert_eq!(list("[1] x"), None);
    }

    #[test]
    fn a_map_and_a_struct_split_the_way_the_pin_splits_them() {
        assert_eq!(map("{}"), Some(vec![]));
        assert_eq!(
            map("  { a = 1 ,b= 2 }  "),
            Some(vec![("a".into(), Some("1".into())), ("b".into(), Some("2".into()))])
        );
        assert_eq!(map("{a=}"), Some(vec![("a".into(), Some(String::new()))]));
        assert_eq!(map("{a=NULL}"), Some(vec![("a".into(), None)]));
        assert_eq!(map("{NULL=1}"), None);
        assert_eq!(map("{a=1"), None);
        assert_eq!(fields("{b: 1}", &["a", "b"], false), Some(vec![None, Some("1".into())]));
        assert_eq!(fields("{a: 1, a: 2}", &["a"], false), Some(some(&["2"])));
        assert_eq!(fields("{A: 1}", &["a"], false), None);
        assert_eq!(fields("(1)", &["a", "b"], false), Some(vec![Some("1".into()), None]));
        assert_eq!(fields("(1,)", &["", ""], true), Some(vec![Some("1".into()), None]));
        assert_eq!(fields("(1,2,3)", &["a", "b"], false), None);
        assert_eq!(fields("{a: {b: [1, 2]}}", &["a"], false), Some(some(&["{b: [1, 2]}"])));
    }
}
