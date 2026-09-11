//! Bytes into fields.
//!
//! One function, which reads one record. It is separate from the reader above it because the
//! sniffer runs it too: working out the delimiter means splitting the sample under each candidate
//! and seeing which one gives every line the same number of fields, and that has to be the same
//! splitting the reader will do or the sniffer answers a question nobody asked.
//!
//! A record ends at a newline that is not inside a quoted field, and the three line endings are all
//! accepted whatever the file mostly uses. DuckDB reports the one it found and reads either, and a
//! file that a text editor has half converted is a real thing.
//!
//! Fields arrive as owned strings. That is a copy per field and it is the wrong shape for a fast
//! scan, which wants a range into the buffer and a copy only for the fields that had an escape in
//! them. The borrowed form is the change to make when there is a number saying it matters, and it is
//! a change to this one function.

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
                Some(&byte) => {
                    return Err(Error::io(format!(
                        "a quoted value is followed by '{}' rather than by a delimiter or the end \
                         of the line",
                        byte as char
                    )));
                }
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
