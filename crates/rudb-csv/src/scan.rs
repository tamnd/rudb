//! Bytes into fields.
//!
//! Two ways of doing it, which agree. [`record`] reads one record into owned strings, and it is
//! what the sniffer runs: working out the delimiter means splitting the sample under each candidate
//! and seeing which one gives every line the same number of fields, and that has to be the same
//! splitting the reader will do or the sniffer answers a question nobody asked. [`records`] reads
//! a chunk's worth of records at once and writes down where each field is rather than copying it,
//! and it is what the reader runs. The tests at the bottom hold the two to the same answer on a
//! few thousand generated files, which is what lets the sniffer keep the simple one.
//!
//! A record ends at a newline that is not inside a quoted field, and the three line endings are all
//! accepted whatever the file mostly uses. DuckDB reports the one it found and reads either, and a
//! file that a text editor has half converted is a real thing.
//!
//! [`records`] finds the delimiters, quotes and line endings sixty four bytes at a time, as one bit
//! a byte in a `u64`, and works out which of them are inside a quoted field with a prefix XOR over
//! the quote bits, the way simdjson and simdcsv do. A field only has to be looked at byte by byte
//! when it has a quote in it, and then only to check that the quotes are where a quoted field puts
//! them. A quote anywhere else, which is a literal character in the middle of a bare field, hands
//! that one record to the byte loop, since the parity trick has no way to know the quote was not
//! meant. A dialect whose escape is not its quote is read by the byte loop throughout.
//!
//! Fields used to arrive as owned strings, a copy per field before the conversion made a second
//! one. The borrowed ranges were the change to make when there was a number saying it mattered, and
//! the number is this: on a 200,000 row file shaped like TPC-H lineitem, 27 MB, one core of an
//! Apple M4 read 95 MB/s a record at a time and 613 MB/s a chunk at a time, with the scan itself
//! running at about 3 GB/s and the rest going to the column conversions. The ignored test
//! `reads_lineitem_faster_a_chunk_at_a_time` in the reader measures it again.

use std::borrow::Cow;

use rudb_common::{Error, Result};

use crate::dialect::Dialect;

/// Reads the record starting at `from` into `out`, and answers where the next one starts.
///
/// `None` means the buffer does not hold a whole record: either it ran out mid line and more bytes
/// may follow, or a quoted field was left open. With `eof` set there are no more bytes, so a last
/// line with no newline on the end is a record like any other and only an open quote is short.
///
/// # Errors
///
/// When a quoted field has something other than a delimiter or a line ending after its closing
/// quote, which is a file that is not the file it claims to be and reading past it would invent
/// data.
pub fn record(
    bytes: &[u8],
    from: usize,
    dialect: Dialect,
    eof: bool,
    out: &mut Vec<String>,
) -> Result<Option<usize>> {
    if from >= bytes.len() {
        // No bytes left is no record, and it is not a record of one empty field. The difference
        // matters at the end of a file, where a reader that took the second reading would hand back
        // an empty row forever, and it matters for the last line of a file that ends in a newline,
        // which is not a row of nothing.
        return Ok(None);
    }
    let quote = dialect.quote_byte();
    let escape = dialect.escape_byte();
    let mut at = from;
    let mut count = 0;
    let mut field = Vec::new();
    loop {
        field.clear();
        if bytes.get(at) == Some(&quote) {
            at += 1;
            loop {
                let Some(&byte) = bytes.get(at) else { return Ok(None) };
                if byte == escape && bytes.get(at + 1) == Some(&quote) {
                    field.push(quote);
                    at += 2;
                    continue;
                }
                if byte == quote {
                    at += 1;
                    break;
                }
                field.push(byte);
                at += 1;
            }
            match bytes.get(at) {
                None if !eof => return Ok(None),
                None => {}
                Some(&byte) if byte == dialect.delimiter || byte == b'\n' || byte == b'\r' => {}
                Some(&byte) => return Err(after_quote(byte)),
            }
        } else {
            while let Some(&byte) = bytes.get(at) {
                if byte == dialect.delimiter || byte == b'\n' || byte == b'\r' {
                    break;
                }
                field.push(byte);
                at += 1;
            }
            if at >= bytes.len() && !eof {
                return Ok(None);
            }
        }
        place(out, count, &field);
        count += 1;
        match bytes.get(at) {
            Some(&byte) if byte == dialect.delimiter => at += 1,
            Some(b'\r') => {
                at += 1;
                if bytes.get(at) == Some(&b'\n') {
                    at += 1;
                } else if at >= bytes.len() && !eof {
                    // A trailing carriage return may be the first half of a `\r\n` that has not
                    // arrived, and guessing wrong here splits one record into two.
                    return Ok(None);
                }
                break;
            }
            Some(b'\n') => {
                at += 1;
                break;
            }
            Some(_) => unreachable!("a field stops at a delimiter, a line ending or the end"),
            None => break,
        }
    }
    out.truncate(count);
    Ok(Some(at))
}

/// Writes the field at `at`, reusing the string that is already there.
///
/// A CSV file is bytes and the encoding is not stated anywhere in it. UTF-8 comes through unchanged
/// because no byte of a multi byte sequence can be a delimiter or a quote. A byte that is not valid
/// UTF-8 becomes the replacement character rather than an error, which is what one bad byte in a
/// text file deserves, and which is also what DuckDB does when it is not told to check.
fn place(out: &mut Vec<String>, at: usize, field: &[u8]) {
    let text = String::from_utf8_lossy(field);
    match out.get_mut(at) {
        Some(held) => {
            held.clear();
            held.push_str(&text);
        }
        None => out.push(text.into_owned()),
    }
}

/// The error for a quoted field with something after its closing quote, shared by both readers so
/// that they cannot come to say it differently.
fn after_quote(byte: u8) -> Error {
    Error::io(format!(
        "a quoted value is followed by '{}' rather than by a delimiter or the end of the line",
        byte as char
    ))
}

/// Where one field's bytes are in the buffer it was read from.
///
/// A quoted field's range is what is between its quotes. Whether a doubled quote or an escaped one
/// is in there is kept too, since then the bytes are not the value yet and [`Span::text`] has to
/// take the escapes out, which is the one case that copies before the value is used.
///
/// Eight bytes, two `u32`s with the escape in the top bit of the end, because a chunk holds one of
/// these for every field of eight thousand records and the converters walk them once per column.
/// That caps a chunk's buffer at [`Span::MOST`] bytes, which the reader keeps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    start: u32,
    end: u32,
}

/// The bit of [`Span::end`] that says the field holds escapes.
const ESCAPED: u32 = 1 << 31;

impl Span {
    /// The furthest into its buffer a range can reach.
    pub const MOST: usize = (ESCAPED - 1) as usize;

    /// The field from `start` to `end`, with escapes in it or not.
    ///
    /// # Panics
    ///
    /// In a debug build, if `end` is past [`Self::MOST`], which the reader never lets a buffer be.
    #[must_use]
    pub fn new(start: usize, end: usize, escaped: bool) -> Self {
        debug_assert!(start <= end && end <= Self::MOST);
        #[allow(clippy::cast_possible_truncation)]
        let (start, end) = (start as u32, end as u32);
        Self { start, end: end | if escaped { ESCAPED } else { 0 } }
    }

    /// The first byte of the field.
    #[must_use]
    pub const fn start(self) -> usize {
        self.start as usize
    }

    /// One past the last byte of the field.
    #[must_use]
    pub const fn end(self) -> usize {
        (self.end & !ESCAPED) as usize
    }

    /// Whether the bytes still hold escapes.
    #[must_use]
    pub const fn escaped(self) -> bool {
        self.end & ESCAPED != 0
    }

    /// Whether the field is empty, which the reader turns into a null.
    ///
    /// A field with an escape in it is never empty once the escape is taken out, since every escape
    /// leaves a quote behind, so the range answers for the value.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start() == self.end()
    }

    /// How many bytes the field has in the buffer, escapes and all.
    #[must_use]
    pub const fn len(self) -> usize {
        self.end() - self.start()
    }

    /// The bytes of the field as they are in the buffer, escapes and all.
    #[must_use]
    pub fn raw(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.start()..self.end()]
    }

    /// The field as text, borrowed from the buffer unless it had an escape or a byte that is not
    /// UTF-8 in it.
    ///
    /// The bytes that are not UTF-8 become the replacement character, exactly as
    /// `String::from_utf8_lossy` makes them, because that is what [`record`] does and the two have
    /// to agree.
    #[must_use]
    pub fn text(self, bytes: &[u8], dialect: Dialect) -> Cow<'_, str> {
        let raw = self.raw(bytes);
        if !self.escaped() {
            return String::from_utf8_lossy(raw);
        }
        Cow::Owned(String::from_utf8_lossy(&unescape(raw, dialect)).into_owned())
    }
}

/// The bytes of a quoted field with its escapes taken out.
///
/// This pairs the bytes up exactly the way the loop that found the field did, from the left, an
/// escape byte followed by a quote being one quote and anything else being itself. The range ends
/// before the closing quote, so there is no closing quote in it to be mistaken for the second half
/// of a pair, and reading it again from the same end gives the same pairs.
fn unescape(raw: &[u8], dialect: Dialect) -> Vec<u8> {
    let quote = dialect.quote_byte();
    let escape = dialect.escape_byte();
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    while let Some(&byte) = raw.get(at) {
        if byte == escape && raw.get(at + 1) == Some(&quote) {
            out.push(quote);
            at += 2;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    out
}

/// A chunk's worth of records, as ranges into the buffer they were read from.
///
/// One list of fields for all of the records and one list of where each record's fields end, which
/// is two allocations that are reused from chunk to chunk rather than one per row.
#[derive(Debug, Clone, Default)]
pub struct Records {
    spans: Vec<Span>,
    ends: Vec<usize>,
}

impl Records {
    /// No records, with the room the last chunk needed kept.
    pub fn clear(&mut self) {
        self.spans.clear();
        self.ends.clear();
    }

    /// How many whole records have been read.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether no whole record has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The fields of record `at`, in order.
    ///
    /// # Panics
    ///
    /// If there is no record `at`.
    #[must_use]
    pub fn fields(&self, at: usize) -> &[Span] {
        let start = if at == 0 { 0 } else { self.ends[at - 1] };
        &self.spans[start..self.ends[at]]
    }

    /// Field `column` of record `row`, or `None` when the record is shorter than that.
    #[must_use]
    pub fn field(&self, row: usize, column: usize) -> Option<Span> {
        let start = if row == 0 { 0 } else { self.ends[row - 1] };
        let at = start + column;
        if at < self.ends[row] { Some(self.spans[at]) } else { None }
    }

    /// Moves every range down by `by` bytes, for a buffer that has just had that many bytes taken
    /// off its front.
    pub fn shift(&mut self, by: usize) {
        if by == 0 {
            return;
        }
        // A shift is by less than the buffer is long, so it fits in the `u32` every range does.
        #[allow(clippy::cast_possible_truncation)]
        let by = by as u32;
        for span in &mut self.spans {
            span.start -= by;
            span.end -= by;
        }
    }
}

/// Reads whole records starting at `from` into `out`, until it holds `limit` of them or the buffer
/// has no whole record left, and answers where the first record it did not take starts.
///
/// The rules are [`record`]'s, including `eof`: without it a record the buffer ends in the middle
/// of is left for when there are more bytes, and with it a last line with no newline is a record
/// and only an open quote is short. Fewer than `limit` records and not at the end of the file means
/// the caller should read more and ask again from the answer.
///
/// # Errors
///
/// The one [`record`] reports, for a quoted field with something after its closing quote. The
/// records before it are in `out` and the one it was found in is not.
pub fn records(
    bytes: &[u8],
    from: usize,
    dialect: Dialect,
    eof: bool,
    limit: usize,
    out: &mut Records,
) -> Result<usize> {
    let quote = dialect.quote_byte();
    let delimiter = dialect.delimiter;
    let structural = |byte: u8| byte == quote || byte == b'\n' || byte == b'\r';
    if dialect.escape_byte() == quote && !structural(delimiter) {
        blocks(bytes, from, dialect, eof, limit, out)
    } else {
        let mut at = from;
        while out.len() < limit {
            let Some(next) = spans(bytes, at, dialect, eof, &mut out.spans)? else { break };
            out.ends.push(out.spans.len());
            at = next;
        }
        Ok(at)
    }
}

/// [`records`] for a dialect whose escape is its quote, sixty four bytes at a time.
///
/// Each block becomes three masks: the quotes, the bytes that can end a field, and the line endings
/// among those. The prefix XOR of the quote mask has a bit set for every byte after an odd number of
/// quotes, which is every byte inside a quoted field, and a doubled quote flips it twice between two
/// adjacent bytes and so changes nothing. Whether the block ended inside a quote is carried into the next one. The field
/// endings left once those bits are taken away are walked in order with `trailing_zeros`.
///
/// That is only right while every quote is where a quoted field puts it: first and last in its
/// field, with any others in pairs. A field with no quote in it cannot be wrong, because the bits
/// were outside a quote at its start and nothing in it flips them. A field with a quote in it is
/// checked, and one that is not shaped like a quoted field goes to the byte loop from the start of
/// its record, after which the blocks start again behind that record with the parity cleared.
fn blocks(
    bytes: &[u8],
    from: usize,
    dialect: Dialect,
    eof: bool,
    limit: usize,
    out: &mut Records,
) -> Result<usize> {
    let delimiter = dialect.delimiter;
    let quote = dialect.quote_byte();
    let len = bytes.len();
    let mut record = from;
    loop {
        // Where the fields of the record being read start in `out`, so that a record that turns
        // out to be short or to need the byte loop can be taken back out.
        let mut mark = out.spans.len();
        let mut field = record;
        let mut quoted = false;
        let mut inside = 0u64;
        let mut block = record;
        'blocks: while block < len && out.len() < limit {
            let end = len.min(block + 64);
            let (quotes, ends, lines) = masks(&bytes[block..end], delimiter, quote);
            let prefix = prefix_xor(quotes) ^ inside;
            inside = 0u64.wrapping_sub(prefix >> 63);
            let mut structural = ends & !prefix;
            while structural != 0 {
                let bit = structural.trailing_zeros() as usize;
                structural &= structural - 1;
                let at = block + bit;
                if at < field {
                    // The `\n` of a `\r\n`, which the `\r` already dealt with.
                    continue;
                }
                let low = field.saturating_sub(block);
                if quoted || quotes & below(bit) & !below(low) != 0 {
                    let Some(escaped) = enclosed(&bytes[field..at], quote) else {
                        break 'blocks;
                    };
                    out.spans.push(Span::new(field + 1, at - 1, escaped));
                    quoted = false;
                } else {
                    out.spans.push(Span::new(field, at, false));
                }
                if lines >> bit & 1 == 0 {
                    field = at + 1;
                    continue;
                }
                let mut next = at + 1;
                if bytes[at] == b'\r' {
                    match bytes.get(next) {
                        Some(b'\n') => next += 1,
                        Some(_) => {}
                        None if eof => {}
                        // The first half of a `\r\n` whose second half has not been read, the
                        // same wait `record` makes.
                        None => {
                            out.spans.truncate(mark);
                            return Ok(record);
                        }
                    }
                }
                out.ends.push(out.spans.len());
                mark = out.spans.len();
                record = next;
                field = next;
                if out.len() == limit {
                    return Ok(record);
                }
            }
            if field < end && quotes >> field.saturating_sub(block) != 0 {
                quoted = true;
            }
            block = end;
        }
        if out.len() >= limit || record >= len {
            return Ok(record);
        }
        // Either a field whose quotes the blocks cannot vouch for, or the end of the buffer in the
        // middle of a record. The byte loop settles both, the second one being where a last line
        // with no newline, a record that is not all here yet and an open quote are told apart.
        out.spans.truncate(mark);
        let Some(next) = spans(bytes, record, dialect, eof, &mut out.spans)? else {
            return Ok(record);
        };
        out.ends.push(out.spans.len());
        record = next;
    }
}

/// The quotes in a block of up to sixty four bytes, the bytes that can end a field, and of those
/// the ones that can end a line, one bit a byte with the first byte in the lowest bit.
///
/// A short block at the end of the buffer is padded and the bits for the padding are cleared
/// afterwards, so the padding byte does not have to be one that cannot be a delimiter.
#[inline]
fn masks(block: &[u8], delimiter: u8, quote: u8) -> (u64, u64, u64) {
    let needles = [quote, delimiter, b'\n', b'\r'];
    let [quotes, delimiters, newlines, returns] = if let Ok(full) = block.try_into() {
        rudb_vector::bytes::masks(full, needles)
    } else {
        let mut padded = [0u8; 64];
        padded[..block.len()].copy_from_slice(block);
        let live = below(block.len());
        rudb_vector::bytes::masks(&padded, needles).map(|mask| mask & live)
    };
    let lines = newlines | returns;
    (quotes, delimiters | lines, lines)
}

/// Every bit set that has an odd number of set bits at or below it in `bits`.
const fn prefix_xor(mut bits: u64) -> u64 {
    bits ^= bits << 1;
    bits ^= bits << 2;
    bits ^= bits << 4;
    bits ^= bits << 8;
    bits ^= bits << 16;
    bits ^= bits << 32;
    bits
}

/// The bits below bit `count`, for a count under sixty four.
const fn below(count: usize) -> u64 {
    (1u64 << count) - 1
}

/// Whether a field is shaped like a quoted field, and if it is, whether it has an escape in it.
///
/// Shaped means a quote first and last and every quote between them one of a doubled pair, which is
/// exactly the field the byte loop would read as quoted and close on the last byte. Pairs are taken
/// from the left the way the byte loop takes them, so `"a"""` is `a"` and `"a""` is not a field the
/// byte loop would have closed where it stops.
fn enclosed(field: &[u8], quote: u8) -> Option<bool> {
    let [first, inner @ .., last] = field else { return None };
    if *first != quote || *last != quote {
        return None;
    }
    let mut escaped = false;
    let mut at = 0;
    while let Some(found) = inner[at..].iter().position(|&byte| byte == quote) {
        if inner.get(at + found + 1) != Some(&quote) {
            return None;
        }
        escaped = true;
        at += found + 2;
    }
    Some(escaped)
}

/// [`record`] writing ranges rather than strings, which is the byte loop [`records`] falls back on.
///
/// The same loop step for step, with the pushes of bytes taken out and a range recorded in their
/// place. On `None` or an error nothing this call added is left in `out`.
fn spans(
    bytes: &[u8],
    from: usize,
    dialect: Dialect,
    eof: bool,
    out: &mut Vec<Span>,
) -> Result<Option<usize>> {
    if from >= bytes.len() {
        return Ok(None);
    }
    let mark = out.len();
    let quote = dialect.quote_byte();
    let escape = dialect.escape_byte();
    let mut at = from;
    loop {
        let span;
        if bytes.get(at) == Some(&quote) {
            at += 1;
            let start = at;
            let mut escaped = false;
            loop {
                let Some(&byte) = bytes.get(at) else {
                    out.truncate(mark);
                    return Ok(None);
                };
                if byte == escape && bytes.get(at + 1) == Some(&quote) {
                    escaped = true;
                    at += 2;
                    continue;
                }
                if byte == quote {
                    break;
                }
                at += 1;
            }
            span = Span::new(start, at, escaped);
            at += 1;
            match bytes.get(at) {
                None if !eof => {
                    out.truncate(mark);
                    return Ok(None);
                }
                None => {}
                Some(&byte) if byte == dialect.delimiter || byte == b'\n' || byte == b'\r' => {}
                Some(&byte) => {
                    out.truncate(mark);
                    return Err(after_quote(byte));
                }
            }
        } else {
            let start = at;
            while let Some(&byte) = bytes.get(at) {
                if byte == dialect.delimiter || byte == b'\n' || byte == b'\r' {
                    break;
                }
                at += 1;
            }
            if at >= bytes.len() && !eof {
                out.truncate(mark);
                return Ok(None);
            }
            span = Span::new(start, at, false);
        }
        out.push(span);
        match bytes.get(at) {
            Some(&byte) if byte == dialect.delimiter => at += 1,
            Some(b'\r') => {
                at += 1;
                if bytes.get(at) == Some(&b'\n') {
                    at += 1;
                } else if at >= bytes.len() && !eof {
                    out.truncate(mark);
                    return Ok(None);
                }
                break;
            }
            Some(b'\n') => {
                at += 1;
                break;
            }
            Some(_) => unreachable!("a field stops at a delimiter, a line ending or the end"),
            None => break,
        }
    }
    Ok(Some(at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(bytes: &[u8], dialect: Dialect) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        let mut fields = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let next = record(bytes, at, dialect, true, &mut fields)
                .expect("splits")
                .expect("a whole record");
            rows.push(fields.clone());
            at = next;
        }
        rows
    }

    fn comma() -> Dialect {
        Dialect { delimiter: b',', quote: Some(b'"'), escape: Some(b'"'), header: true }
    }

    #[test]
    fn a_line_of_fields_is_the_fields_of_that_line() {
        assert_eq!(split(b"a,b,c\n1,2,3\n", comma()), [["a", "b", "c"], ["1", "2", "3"]]);
    }

    #[test]
    fn the_last_line_does_not_need_a_newline_on_it() {
        assert_eq!(split(b"a,b\n1,2", comma()), [["a", "b"], ["1", "2"]]);
    }

    #[test]
    fn all_three_line_endings_end_a_line() {
        assert_eq!(split(b"a\r\nb\rc\n", comma()), [["a"], ["b"], ["c"]]);
    }

    #[test]
    fn a_quoted_field_may_hold_the_delimiter_and_a_newline() {
        assert_eq!(split(b"1,\"x,y\"\n", comma()), [["1", "x,y"]]);
        assert_eq!(split(b"1,\"x\ny\"\n", comma()), [["1", "x\ny"]]);
    }

    #[test]
    fn a_doubled_quote_inside_a_quoted_field_is_one_quote() {
        assert_eq!(split(b"1,\"say \"\"hi\"\"\"\n", comma()), [["1", "say \"hi\""]]);
    }

    #[test]
    fn an_empty_field_is_an_empty_string_here_and_becomes_a_null_above() {
        assert_eq!(split(b"1,,3\n", comma()), [["1", "", "3"]]);
        assert_eq!(split(b"1,\"\",3\n", comma()), [["1", "", "3"]]);
    }

    #[test]
    fn a_trailing_delimiter_makes_a_last_empty_field() {
        assert_eq!(split(b"1|x|\n", Dialect { delimiter: b'|', ..comma() }), [["1", "x", ""]]);
    }

    #[test]
    fn a_quote_in_the_middle_of_a_bare_field_is_just_a_character() {
        assert_eq!(split(b"1,he said \"hi\"\n", comma()), [["1", "he said \"hi\""]]);
    }

    #[test]
    fn a_record_that_the_buffer_does_not_hold_all_of_is_not_a_record_yet() {
        let mut fields = Vec::new();
        assert_eq!(record(b"a,b", 0, comma(), false, &mut fields).unwrap(), None);
        assert_eq!(record(b"a,\"b", 0, comma(), true, &mut fields).unwrap(), None);
        assert_eq!(record(b"a,b\n", 0, comma(), false, &mut fields).unwrap(), Some(4));
    }

    #[test]
    fn rubbish_after_a_closing_quote_is_an_error_rather_than_a_guess() {
        let mut fields = Vec::new();
        let error = record(b"\"x\"y,2\n", 0, comma(), true, &mut fields).unwrap_err();
        assert!(error.message().contains("quoted value"), "{error}");
    }

    #[test]
    fn utf8_survives_being_read_one_byte_at_a_time() {
        assert_eq!(split("a,héllo\n".as_bytes(), comma()), [["a", "héllo"]]);
    }
}

/// The two readers held to the same answers.
///
/// [`record`] is the reference, because it is the loop the sniffer runs and the one every rule in
/// this file was first written down in. Everything [`records`] reports is compared with it: the
/// fields of every record, where the reading stopped, and the error if there was one.
#[cfg(test)]
mod agree {
    use super::*;

    /// What a whole scan found: the records, where it stopped, and the error it stopped on.
    type Outcome = (Vec<Vec<String>>, usize, Option<String>);

    fn by_record(bytes: &[u8], from: usize, dialect: Dialect, eof: bool) -> Outcome {
        let mut rows = Vec::new();
        let mut fields = Vec::new();
        let mut at = from;
        loop {
            match record(bytes, at, dialect, eof, &mut fields) {
                Ok(Some(next)) => {
                    rows.push(fields.clone());
                    at = next;
                }
                Ok(None) => return (rows, at, None),
                Err(error) => return (rows, at, Some(error.to_string())),
            }
        }
    }

    fn by_chunk(bytes: &[u8], from: usize, dialect: Dialect, eof: bool, limit: usize) -> Outcome {
        let mut rows = Vec::new();
        let mut at = from;
        let mut out = Records::default();
        loop {
            out.clear();
            let result = records(bytes, at, dialect, eof, limit, &mut out);
            for row in 0..out.len() {
                rows.push(
                    out.fields(row)
                        .iter()
                        .map(|span| span.text(bytes, dialect).into_owned())
                        .collect(),
                );
            }
            match result {
                Ok(next) => {
                    at = next;
                    if out.len() < limit {
                        return (rows, at, None);
                    }
                }
                Err(error) => {
                    // The error leaves the records before it in `out`, so the position is the
                    // start of the record it was found in, which is where `record` stopped too.
                    let mut from = at;
                    let mut fields = Vec::new();
                    for _ in 0..out.len() {
                        from = record(bytes, from, dialect, eof, &mut fields).unwrap().unwrap();
                    }
                    return (rows, from, Some(error.to_string()));
                }
            }
        }
    }

    /// Both readers over `bytes`, whole and cut at every one of `cuts` the way a refill cuts it,
    /// with a few chunk sizes.
    fn check(bytes: &[u8], dialect: Dialect, cuts: &[usize]) {
        for eof in [true, false] {
            let expected = by_record(bytes, 0, dialect, eof);
            for limit in [1, 2, 3, 7, 8192] {
                assert_eq!(
                    by_chunk(bytes, 0, dialect, eof, limit),
                    expected,
                    "{:?} under {dialect:?}, eof {eof}, {limit} at a time",
                    String::from_utf8_lossy(bytes),
                );
            }
        }
        let whole = by_record(bytes, 0, dialect, true);
        for &cut in cuts {
            let cut = cut.min(bytes.len());
            // The front of the file as a buffer that more bytes will follow, and then the rest of
            // it from wherever that stopped, which is what a refill does.
            let (mut rows, at, error) = by_chunk(&bytes[..cut], 0, dialect, false, 8192);
            if error.is_some() {
                assert_eq!(error, whole.2, "an error in the front is the error in the whole");
                continue;
            }
            let (rest, end, error) = by_chunk(bytes, at, dialect, true, 8192);
            rows.extend(rest);
            assert_eq!(
                (rows, end, error),
                whole,
                "{:?} under {dialect:?} cut at {cut}",
                String::from_utf8_lossy(bytes),
            );
        }
    }

    fn dialects() -> [Dialect; 5] {
        let comma =
            Dialect { delimiter: b',', quote: Some(b'"'), escape: Some(b'"'), header: false };
        [
            comma,
            Dialect { quote: None, escape: None, ..comma },
            Dialect { delimiter: b'|', ..comma },
            Dialect { delimiter: b'\t', quote: Some(b'\''), escape: Some(b'\''), ..comma },
            Dialect { escape: Some(b'\\'), ..comma },
        ]
    }

    #[test]
    fn the_tricky_ones_split_the_same_both_ways() {
        let long = "x".repeat(61);
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"\n".to_vec(),
            b"\n\n".to_vec(),
            b"a".to_vec(),
            b"a,b,c\n1,2,3\n".to_vec(),
            b"a,b\n1,2".to_vec(),
            b"a\r\nb\rc\n".to_vec(),
            b"a\r".to_vec(),
            b"a\r\r\n\n".to_vec(),
            b"1,\"x,y\"\n2,\"x\ny\"\n".to_vec(),
            b"1,\"say \"\"hi\"\"\"\n".to_vec(),
            b"1,,3\n1,\"\",3\n".to_vec(),
            b"1|x|\n1,x,\n".to_vec(),
            b"1,he said \"hi\"\n2,x\n".to_vec(),
            b"1,he said \"hi\n2,x\n".to_vec(),
            b"\"x\"y,2\n".to_vec(),
            b"\"x\"\"\n".to_vec(),
            b"\"x\"\"".to_vec(),
            b"a,\"b".to_vec(),
            b"a,\"b\"".to_vec(),
            b"\"".to_vec(),
            b"\"\"\"\"\n".to_vec(),
            b"\"a\"\"\",b\n".to_vec(),
            b"\"a\\\"b\",c\n\"a\\\\\"\n".to_vec(),
            b"'a,b'\t'c''d'\n".to_vec(),
            b"x\xffy,\"\xfe\"\"\"\n".to_vec(),
            "h\u{e9}llo,w\u{f6}rld\n".as_bytes().to_vec(),
            format!("{long},\"a\nb\",c\n{long}\r\n\"{long}\"\"{long}\",d\n").into_bytes(),
            format!("{long}ab\r\n{long}abc\r\n").into_bytes(),
            format!("\"{long}\"\"\",\"\n\"\n{long},x\"\n").into_bytes(),
        ];
        for bytes in &cases {
            let cuts: Vec<usize> = (0..=bytes.len()).collect();
            for dialect in dialects() {
                check(bytes, dialect, &cuts);
            }
        }
    }

    /// A small generator, since the crate has no dependencies to take one from.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A file that is mostly a CSV file, with the kinds of damage real ones have.
    fn generate(rng: &mut Rng) -> Vec<u8> {
        const PIECES: [&[u8]; 20] = [
            b"a",
            b"1",
            b"-2.5",
            b"xyz",
            b",",
            b"|",
            b"\t",
            b"\"",
            b"\"\"",
            b"'",
            b"\\",
            b"\n",
            b"\r",
            b"\r\n",
            b" ",
            b"\xc3\xa9",
            b"\xff",
            b"2020-01-02",
            b"",
            b"0123456789abcdef",
        ];
        let mut out = Vec::new();
        let rows = rng.below(40);
        for _ in 0..rows {
            let fields = 1 + rng.below(5);
            for field in 0..fields {
                if field > 0 {
                    out.push(b",,,|\t"[rng.below(5)]);
                }
                let mut body = Vec::new();
                for _ in 0..rng.below(6) {
                    let piece = PIECES[rng.below(PIECES.len())];
                    // Long runs of plain bytes now and then, so that fields straddle the blocks.
                    if rng.below(10) == 0 {
                        body.extend(std::iter::repeat_n(b'q', rng.below(90)));
                    }
                    body.extend_from_slice(piece);
                }
                match rng.below(4) {
                    0 => {
                        out.push(b'"');
                        for &byte in &body {
                            if byte == b'"' {
                                out.push(b'"');
                            }
                            out.push(byte);
                        }
                        out.push(b'"');
                    }
                    1 => {
                        out.push(b'"');
                        out.extend_from_slice(&body);
                        out.push(b'"');
                    }
                    _ => out.extend_from_slice(&body),
                }
            }
            out.extend_from_slice([&b"\n"[..], b"\r\n", b"\r"][rng.below(3)]);
        }
        if rng.below(3) == 0 {
            out.truncate(out.len().saturating_sub(1 + rng.below(3)));
        }
        out
    }

    #[test]
    fn thousands_of_generated_files_split_the_same_both_ways() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..3000 {
            let bytes = generate(&mut rng);
            let cuts: Vec<usize> = (0..4).map(|_| rng.below(bytes.len() + 1)).collect();
            for dialect in dialects() {
                check(&bytes, dialect, &cuts);
            }
        }
    }
}
