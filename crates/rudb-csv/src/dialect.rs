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

use rudb_common::LogicalType;

use crate::scan::Comments;

/// The delimiters tried, in the order they are tried.
///
/// Order settles a tie and ties happen: a one column file of `a;b` has neither a comma nor a tab in
/// it and is one column under both. Comma first is DuckDB's order and is the one that matters,
/// since the file that arrives with no clue in it is a comma separated one often enough.
pub const DELIMITERS: [u8; 4] = *b",|;\t";

/// The comments the sniffer tries when the call names none, which are the pin's: none, and `#`.
pub const COMMENTS: [Option<u8>; 2] = [None, Some(b'#')];

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
    /// The byte that starts a comment, which runs to the end of its line, if the file has one.
    pub comment: Option<u8>,
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
///
/// The rest are not punctuation, and they are here because they travel the same road: the
/// binder works them out from the call to sniff the file with and the executor works them out again
/// from the plan to read it with, and a second struct beside this one would be a second thing for
/// the two ends to keep in step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Given {
    /// The byte between two fields.
    pub delimiter: Option<u8>,
    /// The byte that opens and closes a field.
    pub quote: Option<u8>,
    /// The byte that makes the next quote a literal one.
    pub escape: Option<u8>,
    /// The byte that starts a comment, which is `comment` on a call and `COMMENT` on a `COPY`.
    ///
    /// `Some(None)` is a call that said there is none, with `comment=''`, and then the sniffer
    /// does not look for one. Unset, it tries the pin's candidates, which are none and `#`.
    pub comment: Option<Option<u8>>,
    /// Whether the first line names the columns.
    pub header: Option<bool>,
    /// The strings that read as a null, which is `nullstr` on a call and `NULL` on a `COPY`.
    ///
    /// `None` is DuckDB's default, where an empty field is a null. A field is compared once its
    /// quotes and escapes are gone, so `"NA"` is as much a null as `NA` is, and with any list here
    /// an empty field is an empty string rather than a null. Both were measured on
    /// `v2.0.0-dev84237`, where `allow_quoted_nulls` is on unless [`Given::quoted_values`] turns
    /// it off.
    pub nulls: Option<Vec<String>>,
    /// Whether a quoted field is the string it spells even when that is a null string, which is
    /// `allow_quoted_nulls=false` on a call and on a `COPY`.
    ///
    /// Then `"null"` with `nullstr='null'` is the word and `""` is the empty string, while `null`
    /// and an empty field with no quotes are still nulls. The sniffer reads the sample the same
    /// way. Measured on `v2.0.0-dev84237`, where `42,""` reads as `42` and the empty string.
    pub quoted_values: bool,
    /// The columns whose nulls are read as empty strings, which is `force_not_null` on a call and
    /// `FORCE_NOT_NULL` on a `COPY`, by name with case not mattering.
    ///
    /// The sniffer does not look at this, so the empty string goes to whatever type the column
    /// has, and a column that is not text with a null in it is an error. The binder checks every
    /// name is a column of the read before any file is read.
    pub force_not_null: Vec<String>,
    /// The types the sniffer tries, in the order it tries them, which is `auto_type_candidates`
    /// on a call. `None` is [`crate::infer::LADDER`].
    pub candidates: Option<Vec<LogicalType>>,
    /// What to call the columns, first column first, in place of what the header or the generator
    /// called them. A list shorter than the file names the columns it reaches.
    pub names: Option<Vec<String>>,
    /// Whether the column types are the caller's rather than the sniffer's, which is what `COPY t
    /// FROM` does with the table's.
    ///
    /// The reader converts to whatever types it is told either way. What this changes is the error
    /// for a value that does not convert, which tells somebody who set the type to look at the data
    /// and somebody who did not to set one.
    pub typed: bool,
    /// How many columns the read has to have when something other than the call said so, which is
    /// the table a `COPY t FROM` loads.
    ///
    /// The delimiter is then the first that splits every line of the sample that many ways, as it
    /// is for a call that set its columns, which is how the pin's sniffer, told the width, reads a
    /// file of bytes with a `|` in them as one column.
    pub width: Option<usize>,
    /// The column types the call set, first column first, which is `columns` on a call.
    ///
    /// The names come with them in [`Given::names`], and the file has to have exactly this many
    /// columns. They also decide whether the first row is a header in place of the types the rest
    /// of the sample has, which is what the pin does: `a,b` over `1,x` is a header for `INTEGER,
    /// VARCHAR` because `a` is not a number, and `1,x` over `2,y` is a header for `VARCHAR,
    /// VARCHAR` because a file whose columns are all text has one.
    pub types: Option<Vec<LogicalType>>,
    /// Whether nothing is sniffed, which is `auto_detect=false` on a call.
    ///
    /// The punctuation is then what was given or the comma and the double quote, and the first row
    /// is a row unless `header` says otherwise. Measured on `v2.0.0-dev84237`, where a file of
    /// `a,b` over `1,x` read with two `VARCHAR` columns and `auto_detect=false` has three rows.
    pub fixed: bool,
    /// The types the call set for some of the columns over what the sniffer found, which is
    /// `types`, `dtypes` or `column_types` on a call.
    ///
    /// They are put in once the file has been sniffed, so the header is still decided by the types
    /// the sample has, and once the names are, so a name here is one `names` gave. Measured on
    /// `v2.0.0-dev84237`, where `types={'a': 'INTEGER'}` with `all_varchar=true` reads `a` as
    /// `INTEGER` and the rest as `VARCHAR`.
    pub retype: Option<Retype>,
    /// Whether every column the call did not set a type for is read as text, which is
    /// `all_varchar` on a call.
    pub all_varchar: bool,
    /// How many bytes a line may have, which is `max_line_size` on a call, or `buffer_size` when
    /// only that is given. `None` is the pin's default of [`Given::MAX_LINE`].
    pub max_line: Option<usize>,
    /// How many lines at the front of the file are stepped over before anything looks at it, which
    /// is `skip` on a call. A line here ends at a newline whatever quotes it has in it, and the
    /// lines stepped over still count when an error names a line. Both were measured on
    /// `v2.0.0-dev84237`, where `"x` then `y",1` read with `skip=1` starts at `y",1`. `None` is
    /// none, and is told apart from `skip=0` only by the block under an error.
    pub skip: Option<usize>,
    /// Whether the files of a read are matched by column name, each sniffed alone, with a null
    /// for a column a file does not have. That is `union_by_name` on a call.
    pub union_by_name: bool,
    /// How many files the binder sniffs to settle the columns of a read of several, which is
    /// `files_to_sniff` on a call. `None` is the pin's default of ten, and `usize::MAX` is `-1`,
    /// which is all of them.
    pub files_to_sniff: Option<usize>,
    /// The file the binder took the names from, which the executor names when a later file does
    /// not have them. It is the first file with a row in it rather than the first file, so the
    /// binder says which one it was.
    pub main: Option<String>,
    /// How many chunks of [`Given::CHUNK`] rows the sniffer looks at, which is `sample_size` on a
    /// call rounded up to a whole chunk, the way the pin counts it. `None` is the pin's default of
    /// ten, and `usize::MAX` is `-1`, which is the whole file.
    pub sample_chunks: Option<usize>,
    /// Whether a row that does not fit the file is left out rather than refused, which is
    /// `ignore_errors` on a call.
    ///
    /// A row with too few or too many fields is left out whatever the query reads of it, and a
    /// value that does not convert only in a column the query reads. Measured on
    /// `v2.0.0-dev84237`, where `count(*)` over such a file counts the rows with a bad value and
    /// not the rows of the wrong width, and `count(b)` counts neither.
    pub ignore_errors: bool,
    /// Whether a row with too few fields is read with nulls for the rest, which is `null_padding`
    /// on a call. A row with too many is refused either way.
    pub null_padding: bool,
    /// Whether a row with more fields than the file is refused, which is `strict_mode` on a call.
    /// `None` is on, which the block under an error calls auto-detected.
    ///
    /// Off, the fields past the last column are dropped and the errors stop suggesting it.
    /// Measured on `v2.0.0-dev84237`, where `strict_mode=false` reads `6,7,8` under `a,b` as
    /// `6,7` and a row of one field is refused all the same.
    pub strict_mode: Option<bool>,
    /// The line ending the call said the file has, which is `new_line` on a call, written the way
    /// the call writes it: `\n`, `\r` or `\r\n` with the backslashes in the text.
    ///
    /// Every line ending ends a line whatever this says, as the pin's scanner does. What it
    /// changes is what the block under an error shows.
    pub new_line: Option<&'static str>,
    /// The format the dates in the file are written in, which is `dateformat` on a call and
    /// `DATEFORMAT` on a `COPY`, in place of the one the sniffer would look for.
    ///
    /// A column the sniffer would have called a `DATE` is one only when its values are in this
    /// format, and a value that is not is refused at read time. Measured on `v2.0.0-dev84237`,
    /// where `05/06/2019` over `13/01/2020` read with `dateformat='%m/%d/%Y'` is a `VARCHAR`
    /// column, and loaded into a `DATE` column is an error at the second row.
    pub date_format: Option<String>,
    /// The format the timestamps in the file are written in, which is `timestampformat` on a call
    /// and `TIMESTAMPFORMAT` on a `COPY`, the same way.
    pub timestamp_format: Option<String>,
}

/// The types a call set for some of a file's columns, by name or by position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retype {
    /// A struct of a type name for each column it names, matched to the columns without regard to
    /// case. A name the file has no column for is an error.
    Named(Vec<(String, LogicalType)>),
    /// A list of types for the first columns, first column first. A list longer than the file has
    /// columns is an error.
    Positional(Vec<LogicalType>),
}

impl Retype {
    /// The type set for the column at `at`, called `name`, if there is one.
    #[must_use]
    pub fn of(&self, at: usize, name: &str) -> Option<&LogicalType> {
        match self {
            Self::Named(set) => {
                let name = name.to_lowercase();
                set.iter().find(|(set, _)| set.to_lowercase() == name).map(|(_, ty)| ty)
            }
            Self::Positional(set) => set.get(at),
        }
    }
}

impl Given {
    /// The pin's longest line when the call does not say, which is two million bytes.
    pub const MAX_LINE: usize = 2_000_000;

    /// How many bytes a line may have.
    #[must_use]
    pub fn line_limit(&self) -> usize {
        self.max_line.unwrap_or(Self::MAX_LINE)
    }

    /// How many rows the pin's sniffer reads at a time, which `sample_size` is counted in.
    pub const CHUNK: usize = 2048;

    /// How many rows the sniffer looks at.
    #[must_use]
    pub fn sample(&self) -> usize {
        self.sample_chunks.map_or(crate::infer::SAMPLE, |chunks| chunks.saturating_mul(Self::CHUNK))
    }

    /// The `sample_size` the block under a conversion error shows, which is the pin's chunk count
    /// times the chunk size. For `-1` that product wraps, and what the pin prints is the wrapped
    /// number.
    #[must_use]
    pub fn sample_shown(&self) -> u64 {
        self.sample_chunks.map_or(crate::infer::SAMPLE as u64, |chunks| {
            (chunks as u64).wrapping_mul(Self::CHUNK as u64)
        })
    }

    /// How a byte is written in the block under a conversion error, and where it came from.
    #[must_use]
    pub fn shown(given: Option<u8>, sniffed: Option<u8>) -> String {
        format!("{} {}", Dialect::shown(sniffed), Self::source(given.is_some()))
    }

    /// The strings that read as a null, with DuckDB's default of the empty string written out as
    /// `None` so that a caller who wrote `NULL ''` gets the same fast test as one who wrote nothing.
    #[must_use]
    pub fn null_strings(&self) -> Option<Vec<Vec<u8>>> {
        match self.nulls.as_deref() {
            None => None,
            Some([only]) if only.is_empty() => None,
            Some(nulls) => Some(nulls.iter().map(|text| text.as_bytes().to_vec()).collect()),
        }
    }

    /// A date or timestamp format a call gave, once it is known to be one.
    ///
    /// # Errors
    ///
    /// When it does not parse, in the pin's words, which say `DATEFORMAT` for a timestamp format as
    /// well. Measured on `v2.0.0-dev84237` with `timestampformat='%Q'`.
    pub fn format(text: &str) -> rudb_common::Result<String> {
        Self::written(text)?;
        Ok(text.to_string())
    }

    fn written(text: &str) -> rudb_common::Result<crate::infer::Written> {
        crate::infer::Written::parse(text).map_err(|why| {
            rudb_common::Error::invalid_input(format!("Could not parse DATEFORMAT: {why}"))
        })
    }

    /// The formats the call gave, taken apart.
    ///
    /// # Errors
    ///
    /// As for [`Given::format`], which a call has been through already.
    pub fn clocks(&self) -> rudb_common::Result<crate::infer::Clocks> {
        Ok(crate::infer::Clocks {
            date: self.date_format.as_deref().map(Self::written).transpose()?,
            timestamp: self.timestamp_format.as_deref().map(Self::written).transpose()?,
        })
    }

    /// How the call writes the line ending it says a file has, in the pin's words when it is not
    /// one of the three.
    ///
    /// # Errors
    ///
    /// When `text` is anything but `\n`, `\r` or `\r\n` with the backslashes written out, which
    /// is what the pin takes. A real newline is refused, as `E'\n'` is on `v2.0.0-dev84237`.
    pub fn new_line(text: &str) -> rudb_common::Result<&'static str> {
        match text {
            "\\n" => Ok("\\n"),
            "\\r" => Ok("\\r"),
            "\\r\\n" => Ok("\\r\\n"),
            _ => Err(rudb_common::Error::invalid_input(format!(
                "This is not accepted as a newline: {text}"
            ))),
        }
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
        Self { delimiter: b',', quote: None, escape: None, header: true, comment: None }
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

/// The line ending the first line of `sample` has, written the way the block under an error writes
/// it, which is `\n` for a sample with no line ending in it.
#[must_use]
pub fn line_ending(sample: &[u8]) -> &'static str {
    match sample.iter().position(|&byte| byte == b'\n' || byte == b'\r') {
        Some(at) if sample[at] == b'\r' && sample.get(at + 1) == Some(&b'\n') => "\\r\\n",
        Some(at) if sample[at] == b'\r' => "\\r",
        _ => "\\n",
    }
}

/// Which delimiter splits `sample` into the most columns, consistently.
///
/// Consistently is the whole test. Every candidate splits every line into some number of fields,
/// and the one to take is the candidate under which all the lines agree, because a delimiter that
/// is really just a character inside the data will land in some lines and not others. Among the
/// candidates that agree, the one that found the most columns wins, since a file is more likely to
/// have three columns separated by something than one column containing it. An empty sample has
/// nothing to count and is a comma.
///
/// Each comment in `comments` is tried with each delimiter, and a comment only counts when the
/// lines it takes out look like comments, which [`comments_fit`] decides. A file with no comment in
/// it splits the same with one as without, and the earlier candidate wins a tie, so the answer is
/// the delimiter and the comment.
#[must_use]
pub fn delimiter(sample: &[u8], quote: Option<u8>, comments: &[Option<u8>]) -> (u8, Option<u8>) {
    let mut best = (1usize, DELIMITERS[0], comments.first().copied().flatten());
    for &comment in comments {
        for candidate in DELIMITERS {
            let dialect =
                Dialect { delimiter: candidate, quote, escape: quote, header: false, comment };
            let Some(width) = consistent_width(sample, dialect, comments.len() > 1) else {
                continue;
            };
            if width > best.0 {
                best = (width, candidate, comment);
            }
        }
    }
    (best.1, best.2)
}

/// The first delimiter that splits every line of `sample` into `columns` fields, for a read that
/// set its columns.
///
/// The pin's sniffer only takes a dialect that finds as many columns as were set, so a file of one
/// column set as one is not split on a `|` that happens to be in it. Measured on
/// `v2.0.0-dev84237`, where a one line file with a `|` and a `;` in it is two columns sniffed and
/// one column read with `columns={'col1': 'BLOB'}`.
#[must_use]
pub fn delimiter_of_width(
    sample: &[u8],
    quote: Option<u8>,
    comments: &[Option<u8>],
    columns: usize,
) -> Option<(u8, Option<u8>)> {
    comments.iter().find_map(|&comment| {
        DELIMITERS.into_iter().find_map(|candidate| {
            let dialect =
                Dialect { delimiter: candidate, quote, escape: quote, header: false, comment };
            let width = consistent_width(sample, dialect, comments.len() > 1);
            (width == Some(columns)).then_some((candidate, comment))
        })
    })
}

/// The comment of `comments` that goes with a delimiter the call set, which is the first that
/// gives every line the same width, or the first of them when none does.
#[must_use]
pub fn delimiter_with(
    sample: &[u8],
    quote: Option<u8>,
    comments: &[Option<u8>],
    delimiter: u8,
) -> Option<u8> {
    let mut best = (0, comments.first().copied().flatten());
    for &comment in comments {
        let dialect = Dialect { delimiter, quote, escape: quote, header: false, comment };
        if let Some(width) = consistent_width(sample, dialect, comments.len() > 1)
            && width > best.0
        {
            best = (width, comment);
        }
    }
    best.1
}

/// The number of fields every line has under `dialect`, when they all have the same number.
///
/// With `doubt` set the dialect's comment is a guess, and a width it gives only counts when the
/// lines the comment took out look like comments.
fn consistent_width(sample: &[u8], dialect: Dialect, doubt: bool) -> Option<usize> {
    let mut at = 0;
    let mut fields = Vec::new();
    let mut width = None;
    let mut lines = 0;
    let mut comments = Comments::default();
    while at < sample.len() {
        let Some(next) =
            crate::scan::record_noting(sample, at, dialect, true, &mut fields, &mut comments)
                .ok()?
        else {
            break;
        };
        at = next;
        lines += 1;
        match width {
            None => width = Some(fields.len()),
            Some(held) if held == fields.len() => {}
            Some(_) => return None,
        }
    }
    let width = if lines == 0 { None } else { width }?;
    if doubt && dialect.comment.is_some() && !comments_fit(&comments, width) {
        return None;
    }
    Some(width)
}

/// Whether what a guessed comment took out of a file looks like comments, the pin's
/// `AreCommentsAcceptable`.
///
/// At least one whole line has to have been a comment, and of the lines the comment touched, three
/// in five have to fit: a whole line that would not have had the file's width, or a line the
/// comment cut short that still has it. A `#` that is only ever part way through a line is data.
#[must_use]
pub fn comments_fit(comments: &Comments, width: usize) -> bool {
    let whole = if width == 1 { 0 } else { comments.lines };
    if whole == 0 {
        return false;
    }
    let cut = comments.mids.iter().filter(|&&found| found == width).count();
    let seen = comments.lines + comments.mids.len();
    (whole + cut) * 5 >= seen * 3
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
        assert_eq!(delimiter(b"a,b,c\n1,2,3\n", None, &[None]).0, b',');
        assert_eq!(delimiter(b"a|b\n1|x\n", None, &[None]).0, b'|');
        assert_eq!(delimiter(b"a;b\n1;x\n", None, &[None]).0, b';');
        assert_eq!(delimiter(b"a\tb\n1\tx\n", None, &[None]).0, b'\t');
    }

    #[test]
    fn a_character_that_lands_in_some_lines_and_not_others_is_not_the_delimiter() {
        // The semicolon splits the first line into two and the second into one, so it is a
        // character in the data. The comma splits both into two and is the answer.
        let sample = b"a,b;c\n1,2\n";
        assert_eq!(delimiter(sample, None, &[None]).0, b',');
    }

    #[test]
    fn the_delimiter_that_finds_more_columns_wins_among_the_ones_that_agree() {
        // Every line is one field under a tab and three under a comma, and both are consistent.
        assert_eq!(delimiter(b"a,b,c\nx,y,z\n", None, &[None]).0, b',');
    }

    #[test]
    fn a_file_with_nothing_to_split_on_is_comma_separated_and_one_column() {
        assert_eq!(delimiter(b"a\nb\n", None, &[None]).0, b',');
    }

    #[test]
    fn the_line_ending_is_the_one_the_first_line_has() {
        assert_eq!(line_ending(b"a,b\r\n1,2\n"), "\\r\\n");
        assert_eq!(line_ending(b"a,b\r1,2\r"), "\\r");
        assert_eq!(line_ending(b"a,b\n1,2\r\n"), "\\n");
        assert_eq!(line_ending(b"a,b"), "\\n");
        assert_eq!(Given::new_line("\\r\\n").expect("takes it"), "\\r\\n");
        let error = Given::new_line("\n").unwrap_err();
        assert_eq!(error.message(), "This is not accepted as a newline: \n");
    }

    #[test]
    fn a_read_that_set_its_columns_takes_the_delimiter_that_finds_that_many() {
        assert_eq!(delimiter(b"a|b;c\n", None, &[None]).0, b'|');
        assert_eq!(delimiter_of_width(b"a|b;c\n", None, &[None], 1), Some((b',', None)));
        assert_eq!(delimiter_of_width(b"a|b;c\n", None, &[None], 2), Some((b'|', None)));
        assert_eq!(delimiter_of_width(b"a|b;c\n", None, &[None], 3), None);
    }

    #[test]
    fn a_comment_is_sniffed_when_the_lines_it_takes_out_look_like_comments() {
        assert_eq!(delimiter(b"a,b\n# c\n1,2\n3,4", None, &COMMENTS), (b',', Some(b'#')));
        assert_eq!(delimiter(b"a;b\n1;2\n#x\n", None, &COMMENTS), (b';', Some(b'#')));
        // A file with no comment in it is read without one.
        assert_eq!(delimiter(b"a,b\n1,2\n", None, &COMMENTS), (b',', None));
        // A `#` that is only ever part way through a line is data.
        assert_eq!(delimiter(b"a,#b,c\n1,#2,3\n", None, &COMMENTS), (b',', None));
        // A comment the call set is taken without the doubt.
        assert_eq!(delimiter(b"a,#b\n1,#2\n", None, &[Some(b'#')]), (b',', Some(b'#')));
        assert_eq!(delimiter_with(b"a|b\n#x\n1|2\n", None, &COMMENTS, b'|'), Some(b'#'));
        let sample = b"#x\na,b\n";
        assert_eq!(delimiter_of_width(sample, None, &COMMENTS, 2), Some((b',', Some(b'#'))));
    }

    #[test]
    fn comments_fit_when_three_in_five_of_the_lines_they_touch_do() {
        let fit = |lines, mids: &[usize], width| {
            comments_fit(&Comments { lines, mids: mids.to_vec() }, width)
        };
        assert!(fit(1, &[], 2));
        assert!(!fit(0, &[2, 2], 2));
        assert!(fit(1, &[2, 3], 2));
        assert!(!fit(1, &[3, 3], 2));
        // A whole line comment in a file of one column has the file's width and says nothing.
        assert!(!fit(3, &[], 1));
    }

    #[test]
    fn a_delimiter_inside_a_quoted_field_does_not_count() {
        let sample = b"a,b\n1,\"x,y\"\n";
        assert_eq!(delimiter(sample, Some(b'"'), &[None]).0, b',');
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
