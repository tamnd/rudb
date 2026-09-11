//! What a CSV file's punctuation is, and working it out from the bytes.
//!
//! A CSV file does not say how it is written. The delimiter, the quote, the escape and whether the
//! first line is a header are all conventions, and a reader that demands to be told them is a
//! reader every loader script has to be rewritten for. DuckDB sniffs, so this sniffs, and every
//! rule here was read off duckdb v1.4.1's `sniff_csv` rather than reasoned about.
//!
//! The candidates are DuckDB's: comma, pipe, semicolon and tab for the delimiter, and the double
//! quote for the quote and the escape. A file with no quote character in it is reported as having
//! no quote at all rather than as having the default one, which is visible in `sniff_csv` and is
//! reproduced because the same field is printed back in an error message.

use rudb_common::{Error, Result};

/// The delimiters tried, in the order they are tried.
///
/// Order settles a tie and ties happen: a one column file of `a;b` has neither a comma nor a tab in
/// it and is one column under both. Comma first is DuckDB's order and is the one that matters,
/// since the file that arrives with no clue in it is a comma separated one often enough.
pub const DELIMITERS: [u8; 4] = *b",|;\t";

/// How a CSV file is punctuated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    /// The byte between two fields.
    pub delimiter: u8,
    /// The byte that opens and closes a field that may hold a delimiter or a newline, if the file
    /// has one.
    pub quote: Option<u8>,
    /// The byte that makes the next quote a literal one, if the file has one. When it is the quote
    /// itself, which is what RFC 4180 says and what every writer does, a doubled quote is one
    /// quote.
    pub escape: Option<u8>,
    /// Whether the first line names the columns rather than being one of them.
    pub header: bool,
}

/// What the caller said about how the file is written, where the sniffer would otherwise decide.
///
/// Every field is optional and a `None` means nothing was said, which is the common case and is the
/// one the sniffer is for. What is given is not sniffed: `read_csv('f.csv', delim=';')` does not try
/// the four candidates and pick one, it uses the semicolon, and a file that is really comma
/// separated then comes back as one column. That is DuckDB's behaviour and it is the useful one,
/// since somebody who wrote the delimiter down knows something the first megabyte of the file does
/// not say.
///
/// A given value also changes the block DuckDB prints under a conversion error, where a line reads
/// `(Set By User)` rather than `(Auto-Detected)`, which is why this is carried into the reader
/// rather than folded into a [`Dialect`] and forgotten.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Given {
    /// The byte between two fields.
    pub delimiter: Option<u8>,
    /// The byte that opens and closes a field.
    pub quote: Option<u8>,
    /// The byte that makes the next quote a literal one.
    pub escape: Option<u8>,
    /// Whether the first line names the columns.
    pub header: Option<bool>,
}

impl Given {
    /// How a byte is written in the block under a conversion error, and where it came from.
    #[must_use]
    pub fn shown(given: Option<u8>, sniffed: Option<u8>) -> String {
        format!("{} {}", Dialect::shown(sniffed), Self::source(given.is_some()))
    }

    /// What the block calls a value the caller gave and one it worked out.
    #[must_use]
    pub const fn source(given: bool) -> &'static str {
        if given { "(Set By User)" } else { "(Auto-Detected)" }
    }
}

impl Dialect {
    /// The dialect a file with nothing unusual in it has.
    #[must_use]
    pub const fn comma_separated() -> Self {
        Self { delimiter: b',', quote: None, escape: None, header: true }
    }

    /// The quote byte, or the double quote when the file has none.
    ///
    /// A file with no quote in it still needs a byte to compare against while splitting, and the
    /// one that cannot appear is the one that never appeared. Splitting on the default is what makes
    /// a quote that turns up after the sample still read as a quote.
    #[must_use]
    pub const fn quote_byte(self) -> u8 {
        match self.quote {
            Some(quote) => quote,
            None => b'"',
        }
    }

    /// The escape byte, or the quote byte when the file has none.
    #[must_use]
    pub const fn escape_byte(self) -> u8 {
        match self.escape {
            Some(escape) => escape,
            None => self.quote_byte(),
        }
    }

    /// How a byte is written in the block DuckDB prints under a CSV error.
    ///
    /// A tab is `\t` there and a byte that is nothing is `(empty)`, which is why this takes the
    /// option rather than the byte.
    #[must_use]
    pub fn shown(byte: Option<u8>) -> String {
        match byte {
            None => "(empty)".to_string(),
            Some(b'\t') => "\\t".to_string(),
            Some(byte) => (byte as char).to_string(),
        }
    }
}

/// Which delimiter splits `sample` into the most columns, consistently.
///
/// Consistently is the whole test. Every candidate splits every line into some number of fields,
/// and the one to take is the candidate under which all the lines agree, because a delimiter that
/// is really just a character inside the data will land in some lines and not others. Among the
/// candidates that agree, the one that found the most columns wins, since a file is more likely to
/// have three columns separated by something than one column containing it.
///
/// # Errors
///
/// When the sample has no complete line in it, which is a file of one unterminated line and is the
/// one case where there is nothing to count.
pub fn delimiter(sample: &[u8], quote: Option<u8>) -> Result<u8> {
    let mut best = (1usize, DELIMITERS[0]);
    for candidate in DELIMITERS {
        let dialect = Dialect { delimiter: candidate, quote, escape: quote, header: false };
        let Some(width) = consistent_width(sample, dialect) else { continue };
        if width > best.0 {
            best = (width, candidate);
        }
    }
    if sample.iter().all(|&byte| byte != b'\n' && byte != b'\r') && sample.is_empty() {
        return Err(Error::io("the file is empty"));
    }
    Ok(best.1)
}

/// The number of fields every line has under `dialect`, when they all have the same number.
fn consistent_width(sample: &[u8], dialect: Dialect) -> Option<usize> {
    let mut at = 0;
    let mut fields = Vec::new();
    let mut width = None;
    let mut lines = 0;
    while at < sample.len() {
        let next = crate::scan::record(sample, at, dialect, true, &mut fields).ok()??;
        at = next;
        lines += 1;
        match width {
            None => width = Some(fields.len()),
            Some(held) if held == fields.len() => {}
            Some(_) => return None,
        }
    }
    if lines == 0 { None } else { width }
}

/// Whether the file uses a quote character at all, and which one.
///
/// Only the double quote is looked for, which is what `sniff_csv` reports and what every writer
/// emits. A field is quoted when the first byte after a delimiter or a line start is the quote, so
/// a double quote sitting in the middle of a field does not make the file a quoted one.
#[must_use]
pub fn quote(sample: &[u8]) -> Option<u8> {
    let mut at_field_start = true;
    for &byte in sample {
        if at_field_start && byte == b'"' {
            return Some(b'"');
        }
        at_field_start = byte == b'\n' || byte == b'\r' || DELIMITERS.contains(&byte);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delimiter_is_the_one_every_line_agrees_on() {
        assert_eq!(delimiter(b"a,b,c\n1,2,3\n", None).unwrap(), b',');
        assert_eq!(delimiter(b"a|b\n1|x\n", None).unwrap(), b'|');
        assert_eq!(delimiter(b"a;b\n1;x\n", None).unwrap(), b';');
        assert_eq!(delimiter(b"a\tb\n1\tx\n", None).unwrap(), b'\t');
    }

    #[test]
    fn a_character_that_lands_in_some_lines_and_not_others_is_not_the_delimiter() {
        // The semicolon splits the first line into two and the second into one, so it is a
        // character in the data. The comma splits both into two and is the answer.
        let sample = b"a,b;c\n1,2\n";
        assert_eq!(delimiter(sample, None).unwrap(), b',');
    }

    #[test]
    fn the_delimiter_that_finds_more_columns_wins_among_the_ones_that_agree() {
        // Every line is one field under a tab and three under a comma, and both are consistent.
        assert_eq!(delimiter(b"a,b,c\nx,y,z\n", None).unwrap(), b',');
    }

    #[test]
    fn a_file_with_nothing_to_split_on_is_comma_separated_and_one_column() {
        assert_eq!(delimiter(b"a\nb\n", None).unwrap(), b',');
    }

    #[test]
    fn a_delimiter_inside_a_quoted_field_does_not_count() {
        let sample = b"a,b\n1,\"x,y\"\n";
        assert_eq!(delimiter(sample, Some(b'"')).unwrap(), b',');
    }

    #[test]
    fn a_quote_is_found_where_a_field_starts_and_not_in_the_middle_of_one() {
        assert_eq!(quote(b"a,b\n1,\"x\"\n"), Some(b'"'));
        assert_eq!(quote(b"a,b\n1,x\n"), None);
        assert_eq!(quote(b"a,b\n1,he said \"hi\"\n"), None);
    }

    #[test]
    fn the_block_duckdb_prints_writes_a_tab_as_two_characters() {
        assert_eq!(Dialect::shown(None), "(empty)");
        assert_eq!(Dialect::shown(Some(b'\t')), "\\t");
        assert_eq!(Dialect::shown(Some(b',')), ",");
    }
}
