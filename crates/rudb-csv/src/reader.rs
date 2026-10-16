//! A CSV file as chunks.
//!
//! The same shape as `rudb-parquet`'s reader on purpose, because the operator above them is the same
//! operator with a different constructor: open, ask what the columns are, say which of them you
//! want, then pull chunks until there are none. A caller that can read one can read the other.
//!
//! The file is read in blocks and a record that straddles a block boundary is carried into the next
//! one, so a file larger than memory reads the same as a small one. The sample the sniffer looks at
//! is the first block, which is also the first block the reader then goes on to use, so opening a
//! file reads its front once.
//!
//! A reader can also cover a stretch of a file rather than all of it, which is what [`crate::split`]
//! hands each thread. It starts where it is told a record starts and takes the records that start
//! before its end, reading past the end only to finish the last of them.

use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result};
use rudb_io::File;
use rudb_vector::{Chunk, VECTOR_SIZE};

use crate::convert::{self, Cells};
use crate::dialect::{self, Dialect, Given, Retype};
use crate::infer::{self, Clocks};
use crate::scan::{Records, Span};

/// How much is read at a time, and how much the sniffer gets to look at.
///
/// A megabyte holds well over the twenty thousand rows of the sample for any file with ordinary
/// rows in it, and for a file with enormous rows the sniffer sees fewer of them and says so by
/// getting a wider type rather than by failing.
const BLOCK: usize = 1 << 20;

/// How much is read at a time once a reader of a stretch is past its end.
///
/// All it is finishing there is the one record that crosses the end, which is usually a line, so a
/// whole block would be read to be thrown away. Each read after the first is as large as what is
/// held, so a record that turns out to be long still arrives in a handful of reads.
const TAIL: usize = 64 << 10;

/// How many rows of a chunk are converted, every projected column of them, before the next.
///
/// A row's ranges are eight bytes a field, so for a sixteen column table like `lineitem` a thousand
/// rows is an eighth of a megabyte of ranges and about as much again of the bytes they point at,
/// which fits in a core's second level cache with room for the columns being written.
const BLOCK_ROWS: usize = 1024;

/// A CSV file, positioned at a record boundary.
#[derive(Debug)]
pub struct Reader {
    file: Arc<dyn File>,
    path: String,
    given: Given,
    dialect: Dialect,
    /// The strings that read as a null, from [`Given::null_strings`].
    nulls: Option<Vec<Vec<u8>>>,
    fields: Vec<Field>,
    projection: Vec<usize>,
    buffer: Vec<u8>,
    at: usize,
    offset: u64,
    drained: bool,
    line: u64,
    scratch: Vec<String>,
    records: Records,
    /// Where in the buffer the first of [`Self::records`] starts.
    from: usize,
    block: usize,
    /// Where the reading started, so that what it has read is the offset less this.
    origin: u64,
    /// The first byte a record may not start at, which is the end of the file for a whole one.
    end: u64,
    /// Where a reader gives up rather than reading on to finish a record, which only a guess
    /// sets. See [`crate::split`].
    cap: u64,
    /// How many rows the sniffer saw past the header, and whether the file had nothing in it at
    /// all, which is what the binder weighs when it combines the files of a read.
    sampled: usize,
    empty: bool,
    /// Whether the sample had rows of more than one width, which is where this sniffer and the
    /// pin's part ways, see [`Reader::check_widths`].
    ragged: bool,
    /// The line ending the block under an error shows, which is the call's or the first line's.
    line_ending: &'static str,
    /// The formats the dates and timestamps are read with, the call's or the sniffer's.
    clocks: Clocks,
    /// For each column of the file, whether `force_not_null` named it, or nothing when it named
    /// none.
    forced: Vec<bool>,
}

impl Reader {
    /// Opens a file, works out how it is written, and positions it at the first row.
    ///
    /// The path is kept because the error a bad value produces names it, the way DuckDB's does.
    ///
    /// # Errors
    ///
    /// When the file cannot be read, and when the first block of it does not hold one whole record,
    /// which is a single line longer than a megabyte and is not a CSV file anybody meant to write.
    pub fn open(file: Box<dyn File>, path: &str) -> Result<Self> {
        Self::open_with(file, path, Given::default())
    }

    /// The same, with whatever the caller already knows about how the file is written.
    ///
    /// This is where `read_csv('f.csv', delim=';', header=true)` arrives. A given value replaces the
    /// sniffer's answer rather than seeding it, and it replaces it before the sample is split, so the
    /// types and the column names come out of the file read the way the caller said it is written.
    ///
    /// # Errors
    ///
    /// Everything [`Reader::open`] reports.
    pub fn open_with(file: Box<dyn File>, path: &str, given: Given) -> Result<Self> {
        Self::open_sized(file, path, given, BLOCK)
    }

    /// The same, reading `block` bytes at a time, which the tests make small so that a record
    /// crosses a refill every few lines rather than once a megabyte.
    pub(crate) fn open_sized(
        file: Box<dyn File>,
        path: &str,
        given: Given,
        block: usize,
    ) -> Result<Self> {
        let nulls = given.null_strings();
        let mut reader = Self {
            file: Arc::from(file),
            path: path.to_string(),
            given,
            dialect: Dialect::comma_separated(),
            nulls,
            fields: Vec::new(),
            projection: Vec::new(),
            buffer: Vec::new(),
            at: 0,
            offset: 0,
            drained: false,
            line: 1,
            scratch: Vec::new(),
            records: Records::default(),
            from: 0,
            block,
            origin: 0,
            end: u64::MAX,
            cap: u64::MAX,
            sampled: 0,
            empty: false,
            ragged: false,
            line_ending: "\\n",
            clocks: Clocks::default(),
            forced: Vec::new(),
        };
        reader.fill(0)?;
        reader.skip_lines()?;
        let sample = reader.buffer.clone();
        let given = &reader.given;
        let (quote, delimiter, comment, told) = if given.fixed {
            (
                given.quote.or(Some(b'"')),
                given.delimiter.unwrap_or(b','),
                given.comment.flatten(),
                given.header.or(Some(false)),
            )
        } else {
            let quote = given.quote.or_else(|| dialect::quote(&sample));
            let comments = match given.comment {
                Some(comment) => &[comment][..],
                None => &dialect::COMMENTS[..],
            };
            let (delimiter, comment) = match given.delimiter {
                Some(byte) => {
                    let found = dialect::delimiter_with(&sample, quote, comments, byte);
                    (byte, found)
                }
                None => given
                    .types
                    .as_ref()
                    .map(Vec::len)
                    .or(given.width)
                    .and_then(|width| dialect::delimiter_of_width(&sample, quote, comments, width))
                    .unwrap_or_else(|| dialect::delimiter(&sample, quote, comments)),
            };
            (quote, delimiter, comment, given.header)
        };
        let escape = given.escape.or(quote);
        reader.line_ending = given.new_line.unwrap_or_else(|| dialect::line_ending(&sample));
        reader.dialect = Dialect { delimiter, quote, escape, header: false, comment };
        // The punctuation comes from the first block, as the pin's comes from its first chunk, and
        // only the rows go on to more of the file when the sample asks for more than a block held.
        // A block that came back short was the end of the file, which is nearly every file, so
        // those are not read again to find that out, and its last line is a row whether or not a
        // newline ends it.
        let mut more = reader.buffer.len() >= reader.block;
        let mut rows = reader.sample_rows(&sample, !more)?;
        let empty = sample.is_empty();
        drop(sample);
        while more && rows.len() <= reader.given.sample() {
            more = reader.grow()?;
            rows = reader.sample_rows(&reader.buffer, !more)?;
        }
        // A read that ignores errors is as wide as most of its rows rather than its widest, since
        // the rest are the ones it is going to leave out. Measured on `v2.0.0-dev84237`, where a
        // file of rows of two fields with one of one and one of three reads as two columns with
        // `ignore_errors` and as three with `null_padding` as well.
        if reader.given.ignore_errors && !reader.given.null_padding {
            let width = commonest(&rows);
            rows.retain(|row| row.len() == width);
        }
        let widest = rows.iter().map(Vec::len).max().unwrap_or(0);
        let blank = |row: &Vec<Option<String>>| row.len() == 1 && row[0].is_none();
        reader.ragged = rows.iter().any(|row| row.len() != widest && !blank(row));
        let given = &reader.given;
        let set = given.types.as_deref();
        let ladder = given.candidates.as_deref().unwrap_or(&infer::LADDER);
        let (header, mut fields, clocks) =
            describe(&rows, told, set, &given.clocks()?, !given.fixed, ladder);
        reader.clocks = clocks;
        reader.sampled = rows.len().saturating_sub(usize::from(header));
        reader.empty = empty;
        if let Some(types) = &reader.given.types {
            let width = rows.iter().map(Vec::len).max().unwrap_or(types.len());
            reader.set_widths(&rows, header, width, types.len())?;
        }
        if let Some(names) = &reader.given.names {
            if names.len() > fields.len() {
                return Err(Error::invalid_input(format!(
                    "Error when sniffing file \"{path}\".\nIt was not possible to detect the CSV \
                     Header, due to the header having less columns than expected\nNumber of \
                     expected columns: {}. Actual number of columns {}",
                    names.len(),
                    fields.len()
                )));
            }
            for (field, name) in fields.iter_mut().zip(names) {
                field.name.clone_from(name);
            }
        }
        // The sniffer still ran, because the names come out of the same pass over the front of the
        // file and only the types are overruled. Types the call set stay what it set them to, which
        // was measured: `INTEGER` columns read with `all_varchar=true` as well are still `INTEGER`.
        if reader.given.all_varchar && reader.given.types.is_none() {
            for field in &mut fields {
                field.ty = LogicalType::Varchar;
            }
            reader.clocks = reader.given.clocks()?;
        }
        if let Some(set) = &reader.given.retype {
            retype(&mut fields, set)?;
        }
        reader.dialect.header = header;
        let named = &reader.given.force_not_null;
        if !named.is_empty() {
            reader.forced = fields
                .iter()
                .map(|field| {
                    named.iter().any(|name| name == "*" || name.eq_ignore_ascii_case(&field.name))
                })
                .collect();
        }
        reader.fields = fields;
        reader.projection = (0..reader.fields.len()).collect();
        if header {
            reader.skip_record()?;
        }
        Ok(reader)
    }

    /// Refuses a file that does not have as many columns as the call set types for.
    ///
    /// A file that was sniffed is turned away with the line of the pin's sniffer error that says
    /// so, and the rest of that error is a list of fixes for a sniffer this one is not. A file that
    /// was not sniffed has nothing to say about its width but its rows, so the first row in the
    /// sample that is too short or too long is the error, worded as the pin words it.
    ///
    /// The pin converts a row as it splits it, so a value that does not fit its column ahead of
    /// where the row ran out is the error rather than the width. That row is left for the read to
    /// refuse, which it does with the conversion error the pin gives. Measured on
    /// `v2.0.0-dev84237`: `1,2` read as two `INTEGER` columns with `delim=';'` and
    /// `auto_detect=false` fails to convert `1,2`, and `1,x` read as `INTEGER, VARCHAR, INTEGER`
    /// expects three columns and found two.
    fn set_widths(
        &self,
        rows: &[Vec<Option<String>>],
        header: bool,
        found: usize,
        set: usize,
    ) -> Result<()> {
        if !self.given.fixed {
            if found == set {
                return Ok(());
            }
            let names = self.given.names.as_deref().unwrap_or_default();
            let types = self.given.types.as_deref().unwrap_or_default();
            let columns: Vec<String> =
                names.iter().zip(types).map(|(name, ty)| format!("'{name}' : '{ty}'")).collect();
            return Err(Error::invalid_input(format!(
                "Error when sniffing file \"{}\".\nIt was not possible to automatically detect the \
                 CSV parsing dialect\n* Columns are set as: \"columns = {{ {}}}\", and they \
                 contain: {set} columns. It does not match the number of columns found by the \
                 sniffer: {found}. Verify the columns parameter is correctly set.",
                self.path,
                columns.join(", "),
            )));
        }
        let types = self.given.types.as_deref().unwrap_or_default();
        let fits = |row: &[Option<String>]| {
            row.iter().zip(types).all(|(cell, ty)| match cell {
                Some(text) => infer::fits_in(text, ty, &self.clocks),
                None => true,
            })
        };
        let skipped = usize::from(header);
        let padding = self.given.null_padding;
        let strict = self.given.strict_mode != Some(false);
        let misfit =
            |row: &[Option<String>]| !self.given.ignore_errors && !wide(row, set, padding, strict);
        let Some((at, row)) =
            rows.iter().enumerate().skip(skipped).find(|(_, row)| misfit(row) || !fits(row))
        else {
            return Ok(());
        };
        if !fits(row) {
            return Ok(());
        }
        let written: Vec<&str> = row.iter().map(|cell| cell.as_deref().unwrap_or("")).collect();
        let written = written.join(&char::from(self.dialect.delimiter).to_string());
        let line = self.line + at as u64;
        let settings = self.settings_with(header);
        Err(wrong_width(&self.given, &written, line, set, row.len(), &settings))
    }

    /// How many rows the sniffer saw past the header, which is none for a file that is empty or
    /// is only a header.
    #[must_use]
    pub const fn sampled(&self) -> usize {
        self.sampled
    }

    /// Whether the file had nothing in it past the lines `skip` stepped over.
    #[must_use]
    pub const fn empty(&self) -> bool {
        self.empty
    }

    /// The columns this reader will produce, in order.
    #[must_use]
    pub fn fields(&self) -> Vec<Field> {
        self.projection.iter().map(|&at| self.fields[at].clone()).collect()
    }

    /// Reads only these columns, by position in the file, in this order.
    ///
    /// # Errors
    ///
    /// When a position is past the end of the file's columns.
    pub fn project(&mut self, columns: &[usize]) -> Result<()> {
        for &column in columns {
            if column >= self.fields.len() {
                return Err(Error::io(format!(
                    "column {column} is past the {} the file has",
                    self.fields.len()
                )));
            }
        }
        self.projection = columns.to_vec();
        Ok(())
    }

    /// Reads the projected columns as these types rather than as the ones the sample chose.
    ///
    /// A read that covers several files produces one stream and a stream has one schema, and no
    /// single file's sample is that schema. Every file is sniffed on its own and the answers are
    /// combined by [`crate::across`], so each file is then told what the whole read settled on,
    /// including the first one. Without it a file whose column happens to hold nothing but whole
    /// numbers hands up a BIGINT column into a stream that is DOUBLE because some other file in the
    /// set held a decimal.
    ///
    /// This is not a cast of what was read. The type is what the text is converted with, so saying
    /// it before any row is read converts once rather than converting to the wrong type and again to
    /// the right one. A value that then does not fit is the conversion error, named and lined the
    /// way any other one is.
    ///
    /// # Errors
    ///
    /// When the list is not as long as the projection.
    pub fn retype(&mut self, types: &[LogicalType]) -> Result<()> {
        if types.len() != self.projection.len() {
            return Err(Error::io(format!(
                "{} types for a projection of {} columns",
                types.len(),
                self.projection.len()
            )));
        }
        for (&at, ty) in self.projection.iter().zip(types) {
            self.fields[at].ty = ty.clone();
        }
        Ok(())
    }

    /// How this file is punctuated, which is what the sniffer decided.
    #[must_use]
    pub const fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The line a record of the chunk was split from, without its line ending, which is what an
    /// error shows as its original line.
    ///
    /// The pin shows the whole line rather than the value, measured on `v2.0.0-dev84237` with
    /// `x,3` under a header of `a,b` read with `types={'a': 'INTEGER'}`, whose error has
    /// `Original Line: x,3`. The ranges say where the fields are and not where the line is, so the
    /// records are split again from where the chunk starts, which is only done for an error.
    fn original_line(&self, row: usize) -> String {
        let mut scratch = Vec::new();
        let mut split = |at: usize| {
            crate::scan::record(&self.buffer, at, self.dialect, true, &mut scratch)
                .ok()
                .flatten()
                .unwrap_or(self.at)
        };
        let mut at = self.from;
        for _ in 0..row {
            at = split(at);
        }
        let end = split(at).max(at);
        let line = &self.buffer[at..end];
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        String::from_utf8_lossy(line).into_owned()
    }

    /// The next chunk, or `None` at the end of the file.
    ///
    /// The records are split first, all of them, and converted afterwards a column at a time, which
    /// is also the order the errors come out in: a malformed record anywhere in the chunk is
    /// reported ahead of a value that does not convert, and among values the first projected
    /// column's first bad row is the one named.
    ///
    /// A read that ignores errors leaves out the records that are not as wide as the file, and then
    /// the ones with a value that does not convert, and a chunk it leaves nothing of is passed over
    /// for the next one.
    ///
    /// # Errors
    ///
    /// A read error, a malformed record, a record with too few or too many fields, or a value that
    /// does not fit the type the sample chose for its column.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        loop {
            let rows = self.next_records()?;
            if rows == 0 {
                return Ok(None);
            }
            let first = self.line;
            self.line += rows as u64;
            let mut index = self.check_widths(first)?;
            while !self.records.is_empty() {
                match self.convert_chunk(first, index.as_deref()) {
                    Ok(chunk) => return Ok(Some(chunk)),
                    Err(error) if self.given.ignore_errors => {
                        if !self.leave_out_refused(&mut index) {
                            return Err(error);
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    /// The records of the chunk as columns, where `index` is the row of the chunk as it was split
    /// that each record is, once some have been left out.
    fn convert_chunk(&self, first: u64, index: Option<&[usize]>) -> Result<Chunk> {
        let rows = self.records.len();
        let cells = self.cells();
        let projected: Vec<_> =
            self.projection.iter().map(|&at| (at, &self.fields[at].ty)).collect();
        let mut builders = convert::builders(&cells, &projected);
        let mut start = 0;
        while start < rows {
            let end = rows.min(start + BLOCK_ROWS);
            for (build, &at) in builders.iter_mut().zip(&self.projection) {
                let refuse = |text: &str, row: usize| self.refusal(text, at, first, row, index);
                if let Err(error) = build.rows(&cells, at, start..end, &refuse) {
                    return Err(self.first_bad_value(&cells, first, index).unwrap_or(error));
                }
            }
            start = end;
        }
        let columns = builders.into_iter().map(|build| build.finish()).collect::<Result<_>>()?;
        Chunk::with_rows(columns, rows)
    }

    /// The chunk's fields, the way the conversion reads them.
    fn cells(&self) -> Cells<'_> {
        Cells {
            bytes: &self.buffer,
            records: &self.records,
            dialect: self.dialect,
            nulls: self.nulls.as_deref(),
            clocks: &self.clocks,
            quoted_values: self.given.quoted_values,
            forced: &self.forced,
        }
    }

    /// The error for `text` in column `at` of `row`, where the chunk starts at line `first`.
    fn refusal(
        &self,
        text: &str,
        at: usize,
        first: u64,
        row: usize,
        index: Option<&[usize]>,
    ) -> Error {
        let row = index.map_or(row, |index| index[row]);
        let original = self.original_line(row);
        Error::conversion(self.conversion_error(text, &original, at, first + row as u64))
    }

    /// The error for the first projected column's first value that does not convert, found by
    /// converting the chunk a column at a time the way it used to be.
    ///
    /// A chunk is converted a block of rows at a time, so the first value it trips on can be in a
    /// later column than one with a bad value further down. This is only run once one has been
    /// found, to name the same value the reader always named.
    fn first_bad_value(
        &self,
        cells: &Cells<'_>,
        first: u64,
        index: Option<&[usize]>,
    ) -> Option<Error> {
        self.projection.iter().find_map(|&at| {
            let field = &self.fields[at];
            let refuse = |text: &str, row: usize| self.refusal(text, at, first, row, index);
            convert::column(cells, at, &field.ty, &refuse).err()
        })
    }

    /// Leaves out every record of the chunk with a value in a projected column that does not
    /// convert, and answers whether there was one, which there always is unless the parsers and
    /// the cast disagree.
    fn leave_out_refused(&mut self, index: &mut Option<Vec<usize>>) -> bool {
        let cells = self.cells();
        let mut bad = vec![false; self.records.len()];
        for &at in &self.projection {
            convert::refused(&cells, at, &self.fields[at].ty, &mut bad);
        }
        if !bad.contains(&true) {
            return false;
        }
        let rows = index.take().unwrap_or_else(|| (0..bad.len()).collect());
        *index = Some(
            rows.into_iter().zip(&bad).filter(|(_, bad)| !**bad).map(|(row, _)| row).collect(),
        );
        self.records.retain(|row| !bad[row]);
        true
    }

    /// Holds every record of the chunk to the width of the file, and answers which rows of the
    /// chunk are left when some were left out.
    ///
    /// A record as wide as the file is a row. So is one with more fields than the file when the
    /// ones past its width are empty, and an empty line is no row at all in a file of more than
    /// one column. A record with fewer fields is read with nulls for the rest under
    /// `null_padding`, and otherwise any record of the wrong width is left out under
    /// `ignore_errors` and refused without it. All of it was measured on `v2.0.0-dev84237`: under a
    /// header of `a,b`, both `3,4,,` and `3,"4",` read as `3,4`, `3,4, ` is refused for having
    /// three columns, and `5` is refused for having one with `null_padding` off. Under
    /// `strict_mode=false` a record with too many fields is cut down to the width of the file and
    /// one with too few is still refused, so `6,7,8` under `a,b` reads as `6,7`.
    ///
    /// A read where nothing but the sniffer said how wide the file is, and whose sample already
    /// had rows of more than one width, still pads a short record and cuts a long one down, as
    /// every read did before the widths were held to. The pin's sniffer weighs every dialect it
    /// tries by how many rows agree with it and steps over the lines at the top that do not, and
    /// this one takes the first delimiter every line agrees on, so a ragged sample is where the
    /// two read a file differently and this one would refuse rows the pin's never sees. A sample
    /// of one width is one both sniffers read the same way, so a row after it of another width is
    /// refused: a file of 30000 rows of `n,n` and then `5` fails on the line `5` is on.
    fn check_widths(&mut self, first: u64) -> Result<Option<Vec<usize>>> {
        let width = self.fields.len();
        let given = &self.given;
        let loose = !given.fixed && given.types.is_none() && !given.ignore_errors;
        let loose = loose && !given.null_padding && given.strict_mode.is_none() && self.ragged;
        let strict = given.strict_mode != Some(false);
        let bytes = &self.buffer;
        let records = &self.records;
        let empty = |span: &Span| span.raw(bytes).is_empty();
        // What a record is: a row, a line with nothing on it, or a row of the wrong width.
        let fits = |row: usize| {
            let fields = records.fields(row);
            match fields.len() {
                found if found == width => Some(true),
                1 if width > 1 && empty(&fields[0]) => None,
                found if found > width => {
                    Some(loose || !strict || fields[width..].iter().all(empty))
                }
                _ => Some(loose || self.given.null_padding),
            }
        };
        if (0..records.len()).all(|row| fits(row) == Some(true)) {
            return Ok(None);
        }
        let mut keep = Vec::with_capacity(records.len());
        for row in 0..records.len() {
            match fits(row) {
                Some(true) => keep.push(true),
                None => keep.push(false),
                Some(false) if self.given.ignore_errors => keep.push(false),
                Some(false) => {
                    let found = records.fields(row).len();
                    return Err(self.wrong_width(
                        &self.original_line(row),
                        first + row as u64,
                        found,
                    ));
                }
            }
        }
        let index = (0..keep.len()).filter(|&row| keep[row]).collect();
        self.records.retain(|row| keep[row]);
        Ok(Some(index))
    }

    /// The pin's error for a row of `found` fields at `line` in a file of a different width.
    fn wrong_width(&self, original: &str, line: u64, found: usize) -> Error {
        wrong_width(&self.given, original, line, self.fields.len(), found, &self.settings())
    }

    /// Splits the next chunk's worth of records into [`Self::records`] and answers how many there
    /// are, which is none at the end.
    ///
    /// The end of the file is the end for a whole file. A reader of a stretch stops at the first
    /// record that starts at or after its end, and it cannot see where a record starts until it has
    /// split it, so a chunk that went past the end is split again a record at a time. That happens
    /// once per stretch, on its last chunk.
    fn next_records(&mut self) -> Result<usize> {
        self.records.clear();
        let mut start = self.at;
        let mut careful = false;
        loop {
            if self.here() >= self.end {
                break;
            }
            let limit = if careful { self.records.len() + 1 } else { VECTOR_SIZE };
            self.at = crate::scan::records(
                &self.buffer,
                self.at,
                self.dialect,
                self.drained,
                limit,
                &mut self.records,
            )?;
            if !careful && self.here() > self.end {
                self.records.clear();
                self.at = start;
                careful = true;
                continue;
            }
            if self.records.len() == VECTOR_SIZE {
                break;
            }
            if careful && self.records.len() == limit {
                continue;
            }
            if self.drained {
                break;
            }
            // The records already read point into the buffer, so the refill keeps everything from
            // the start of the chunk and moves their ranges down by whatever it dropped in front.
            // A range only reaches so far, so a chunk that would outgrow that ends early, and a
            // single record that would is refused.
            if self.buffer.len() - start + self.block > Span::MOST {
                if self.records.is_empty() {
                    return Err(Error::io("a record is longer than two gigabytes"));
                }
                break;
            }
            self.fill(start)?;
            self.records.shift(start);
            start = 0;
        }
        self.from = start;
        self.long_line(start)?;
        Ok(self.records.len())
    }

    /// Where in the file the next record starts.
    pub(crate) fn here(&self) -> u64 {
        self.offset - self.buffer.len() as u64 + self.at as u64
    }

    /// How many bytes of the file this reader has read, sniffing included.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.offset - self.origin
    }

    /// A reader over the same file with the same answers, that starts at `from`, which has to be
    /// where a record starts, and takes the records that start before `end`.
    ///
    /// `line` is what the line of the record at `from` is called in an error, which is right only
    /// for a caller that knows it. [`crate::split`] does not, and so never shows the error such a
    /// reader makes.
    pub(crate) fn stretch(&self, from: u64, end: u64, line: u64) -> Self {
        Self {
            file: Arc::clone(&self.file),
            path: self.path.clone(),
            given: self.given.clone(),
            dialect: self.dialect,
            nulls: self.nulls.clone(),
            fields: self.fields.clone(),
            projection: self.projection.clone(),
            buffer: Vec::new(),
            at: 0,
            offset: from,
            drained: false,
            line,
            scratch: Vec::new(),
            records: Records::default(),
            from: 0,
            block: self.block,
            origin: from,
            end,
            cap: u64::MAX,
            sampled: self.sampled,
            empty: self.empty,
            ragged: self.ragged,
            line_ending: self.line_ending,
            clocks: self.clocks.clone(),
            forced: self.forced.clone(),
        }
    }

    /// Makes the reader give up once it has read up to `cap`, with an error nobody is shown.
    pub(crate) fn give_up_at(&mut self, cap: u64) {
        self.cap = cap;
    }

    /// Splits records to the end without converting any of them, and answers where the first
    /// record not taken starts.
    pub(crate) fn skim(&mut self) -> Result<u64> {
        while self.next_records()? > 0 {}
        Ok(self.here())
    }

    /// Steps over the chunks that end at or before `target` without converting them, counting their
    /// lines, so that the chunks after are the ones and the lines a whole read would give.
    ///
    /// The chunk that runs past `target` is left to be read, since it is the same chunk a whole read
    /// would convert and a value in it may be the one that fails.
    pub(crate) fn skip_to(&mut self, target: u64) -> Result<()> {
        loop {
            let from = self.here();
            let rows = self.next_records()?;
            if rows == 0 {
                return Ok(());
            }
            if self.here() > target {
                let front = self.offset - self.buffer.len() as u64;
                self.at = usize::try_from(from - front)
                    .map_err(|_| Error::internal("a chunk start outside the buffer"))?;
                return Ok(());
            }
            self.line += rows as u64;
        }
    }

    /// The line the next record is on, as a conversion error counts them.
    pub(crate) const fn line(&self) -> u64 {
        self.line
    }

    /// The file this reads, for a caller that wants to look at bytes the reader has not.
    pub(crate) fn file(&self) -> &dyn File {
        self.file.as_ref()
    }

    /// DuckDB's message for a value that does not fit the type its column was sniffed as.
    ///
    /// Reproduced whole, including the block of settings at the bottom, because that block is the
    /// answer to the question the message raises. Somebody reading it wants to know what was
    /// guessed and how to override the guess, and a shorter message would send them to the
    /// documentation to find out.
    ///
    /// A line of that block says where its value came from, and a value the call gave is `(Set By
    /// User)` rather than `(Auto-Detected)`, measured on `v2.0.0-dev84237` by reading a file with
    /// `delim=';'` past the sample. Telling somebody that what they wrote down was auto-detected is
    /// the one thing the block could say that would send them looking in the wrong place.
    fn conversion_error(&self, text: &str, original: &str, at: usize, line: u64) -> String {
        let field = &self.fields[at];
        // A type the caller set, which is every column of a `COPY t FROM`, gets the binary's other
        // paragraph, since telling somebody to set the type they set is no help. Measured on
        // `v2.0.0-dev84237` with a `COPY` of a file whose second row does not fit the table, and
        // with `types` setting one column, where only that column gets it.
        let retyped =
            self.given.retype.as_ref().is_some_and(|set| set.of(at, &field.name).is_some());
        let advice = if self.given.typed || retyped {
            "This type was either manually set or derived from an existing table. Select a \
             different type to correctly parse this column."
                .to_string()
        } else {
            format!(
                "This type was auto-detected from the CSV file.\nPossible solutions:\n* Override \
                 the type for this column manually by setting the type explicitly, e.g., \
                 types={{'{}': 'VARCHAR'}}\n* Set the sample size to a larger value to enable the \
                 auto-detection to scan more values, e.g., sample_size=-1\n* Use a COPY statement \
                 to automatically derive types from an existing table.",
                field.name
            )
        };
        format!(
            "CSV Error on Line: {line}\nOriginal Line: {original}\nError when converting column \
             \"{}\". Could not convert string \"{text}\" to '{}'\n\nColumn {} is being converted \
             as type {}\n{advice}\n* Check whether the null string value is set correctly (e.g., \
             nullstr = 'N/A')\n{}",
            field.name,
            field.ty,
            field.name,
            field.ty,
            self.settings(),
        )
    }

    /// The block of settings at the bottom of DuckDB's errors about a line of the file.
    fn settings(&self) -> String {
        self.settings_with(self.dialect.header)
    }

    /// The block, for a file whose first row is a header or not as `header` says, which is the
    /// pin's `CSVReaderOptions::ToString`.
    ///
    /// The sniffer here never steps over lines, so that line says what the pin's says for a file
    /// the call said nothing about. A format is the one the dates or timestamps are read with,
    /// which is empty when they are read with the cast.
    fn settings_with(&self, header: bool) -> String {
        let given = &self.given;
        let skip = given.skip.unwrap_or(0);
        let strict = given.strict_mode.unwrap_or(true);
        format!(
            "\n  file = {}\n  delimiter = {}\n  quote = {}\n  escape = {}\n  new_line = {} {}\n  \
             header = {header} {}\n  skip_rows = {skip} {}\n  comment = {} {}\n  \
             strict_mode = {strict} {}\n  date_format = {} {}\n  timestamp_format = {} {}\n  \
             null_padding = {}\n  sample_size = {}\n  ignore_errors = {}\n  all_varchar = {}\n",
            self.path,
            Given::shown(given.delimiter, Some(self.dialect.delimiter)),
            Given::shown(given.quote, self.dialect.quote),
            Given::shown(given.escape, self.dialect.escape),
            self.line_ending,
            Given::source(given.new_line.is_some()),
            Given::source(given.header.is_some()),
            Given::source(given.skip.is_some()),
            Dialect::shown(self.dialect.comment),
            Given::source(given.comment.is_some()),
            Given::source(given.strict_mode.is_some()),
            shown(self.clocks.date.as_ref()),
            Given::source(given.date_format.is_some()),
            shown(self.clocks.timestamp.as_ref()),
            Given::source(given.timestamp_format.is_some()),
            u8::from(given.null_padding),
            given.sample_shown(),
            given.ignore_errors,
            u8::from(given.all_varchar),
        )
    }

    /// Refuses the first of the records just split that is longer than the call allows, in the
    /// pin's words, where `from` is where the first of them starts in the buffer.
    ///
    /// A chunk no longer than the limit cannot hold a line that is, which is nearly every chunk, so
    /// the records are only measured when the chunk is longer. Each is first measured from where
    /// its first field starts to where the next one's does, which can be out by the quotes around
    /// those two fields, and only a record that comes within two bytes of the limit is split again
    /// to measure it exactly.
    ///
    /// The pin counts the line ending in a line it sniffed and leaves it out of one it did not,
    /// which was measured on `v2.0.0-dev84237`: `abcd,fghij` and a newline is eleven bytes long
    /// there, and ten with `auto_detect=false`. The line number is this file's, where the pin's
    /// sniffer calls every such line line 1.
    fn long_line(&self, from: usize) -> Result<()> {
        let most = self.given.line_limit();
        let rows = self.records.len();
        if rows == 0 || self.at - from <= most {
            return Ok(());
        }
        let starts = |row: usize| self.records.fields(row).first().map(|span| span.start());
        let mut start = from;
        let mut scratch = Vec::new();
        for row in 0..rows {
            let next = if row + 1 < rows { starts(row + 1).unwrap_or(self.at) } else { self.at };
            if next.saturating_sub(start) + 2 > most {
                // The record's own bytes, split again from where the records started, since the
                // ranges say where fields are and not where lines are.
                let mut at = from;
                for _ in 0..row {
                    at = crate::scan::record(&self.buffer, at, self.dialect, true, &mut scratch)?
                        .unwrap_or(self.at);
                }
                let end = crate::scan::record(&self.buffer, at, self.dialect, true, &mut scratch)?
                    .unwrap_or(self.at);
                self.refuse_long(&self.buffer[at..end], self.line + row as u64)?;
            }
            start = next;
        }
        Ok(())
    }

    /// Refuses one line, with its line ending, that is longer than the limit allows.
    fn refuse_long(&self, line: &[u8], number: u64) -> Result<()> {
        let most = self.given.line_limit();
        let bare = line.strip_suffix(b"\n").unwrap_or(line);
        let bare = bare.strip_suffix(b"\r").unwrap_or(bare);
        let size = if self.given.fixed { bare.len() } else { line.len() };
        if size <= most {
            return Ok(());
        }
        Err(Error::invalid_input(format!(
            "CSV Error on Line: {number}\nOriginal Line: {}\n\nMaximum line size of {most} bytes \
             exceeded. Actual Size:{size} bytes.\n\nPossible Solution: Change the maximum length \
             size, e.g., max_line_size={}\n{}",
            String::from_utf8_lossy(bare),
            size + 2,
            self.settings(),
        )))
    }

    /// The next chunk the way it was read before the records were split a chunk at a time, one
    /// record into owned strings and one cell into a `Value` at a time.
    #[cfg(test)]
    fn next_chunk_by_record(&mut self) -> Result<Option<Chunk>> {
        loop {
            let mut rows: Vec<Vec<Option<String>>> = Vec::new();
            let mut lines = Vec::new();
            let mut numbers = Vec::new();
            let mut split = 0;
            while split < VECTOR_SIZE {
                let from = self.here();
                let Some(fields) = self.next_record()? else { break };
                split += 1;
                // An empty line is no row in a file of more than one column, as in
                // `Reader::check_widths`, and the chunk is the rows left of the ones split.
                if self.fields.len() > 1 && fields.len() == 1 && fields[0].is_none() {
                    continue;
                }
                let size = usize::try_from(self.here() - from).unwrap_or(usize::MAX);
                let line = &self.buffer[self.at - size..self.at];
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                lines.push(String::from_utf8_lossy(line).into_owned());
                numbers.push(self.line - 1);
                rows.push(fields);
            }
            if split == 0 {
                return Ok(None);
            }
            if rows.is_empty() {
                continue;
            }
            let mut columns = Vec::with_capacity(self.projection.len());
            for &at in &self.projection {
                let field = &self.fields[at];
                let mut values = Vec::with_capacity(rows.len());
                for (row, held) in rows.iter().enumerate() {
                    let text = held.get(at).and_then(Option::as_deref);
                    values.push(self.convert(text, &lines[row], at, numbers[row])?);
                }
                columns.push(rudb_vector::Vector::from_values(field.ty.clone(), &values)?);
            }
            return Ok(Some(Chunk::with_rows(columns, rows.len())?));
        }
    }

    /// One value, cast from its text to the column's type.
    #[cfg(test)]
    fn convert(
        &self,
        text: Option<&str>,
        original: &str,
        at: usize,
        line: u64,
    ) -> Result<rudb_common::Value> {
        let Some(text) = text else { return Ok(rudb_common::Value::Null) };
        let field = &self.fields[at];
        if field.ty == LogicalType::Varchar {
            return Ok(rudb_common::Value::Varchar(text.to_string()));
        }
        let value = rudb_common::Value::Varchar(text.to_string());
        match rudb_kernels::cast_value(&value, &field.ty, false) {
            Ok(converted) => Ok(converted),
            Err(_) => Err(Error::conversion(self.conversion_error(text, original, at, line))),
        }
    }

    /// The next record, as one entry per field, with an empty field as a null.
    ///
    /// This and [`Reader::next_chunk_by_record`] are how chunks were read before
    /// [`crate::scan::records`], kept for the tests to hold the new path to the old answers.
    #[cfg(test)]
    fn next_record(&mut self) -> Result<Option<Vec<Option<String>>>> {
        let Some(()) = self.advance()? else { return Ok(None) };
        Ok(Some(
            self.scratch
                .iter()
                .map(|text| if self.is_null(text, false) { None } else { Some(text.clone()) })
                .collect(),
        ))
    }

    /// Reads one record into the scratch, filling the buffer when it has to.
    fn advance(&mut self) -> Result<Option<()>> {
        loop {
            let mut scratch = std::mem::take(&mut self.scratch);
            let outcome = crate::scan::record(
                &self.buffer,
                self.at,
                self.dialect,
                self.drained,
                &mut scratch,
            );
            self.scratch = scratch;
            match outcome? {
                Some(next) => {
                    self.at = next;
                    self.line += 1;
                    return Ok(Some(()));
                }
                None if self.drained => return Ok(None),
                None => self.fill(self.at)?,
            }
        }
    }

    /// Reads one record and throws it away, which is what a header is.
    ///
    /// The header is held to the line limit too, as the pin's sniffer holds it: a refill keeps the
    /// buffer from where the record starts, so its bytes are the ones just before where it ends.
    fn skip_record(&mut self) -> Result<()> {
        let (number, from) = (self.line, self.here());
        if self.advance()?.is_some() {
            let size = usize::try_from(self.here() - from).unwrap_or(usize::MAX);
            self.refuse_long(&self.buffer[self.at - size..self.at], number)?;
        }
        Ok(())
    }

    /// Steps over the lines [`Given::skip`] asks for and starts the buffer where they end, so the
    /// sniffer and everything after it see the file as if it began there.
    fn skip_lines(&mut self) -> Result<()> {
        let mut left = self.given.skip.unwrap_or(0);
        while left > 0 {
            match self.buffer[self.at..].iter().position(|&byte| byte == b'\n') {
                Some(end) => {
                    self.at += end + 1;
                    self.line += 1;
                    left -= 1;
                }
                None if self.drained => {
                    self.at = self.buffer.len();
                    break;
                }
                None => {
                    self.at = self.buffer.len();
                    self.fill(self.at)?;
                }
            }
        }
        if self.at > 0 {
            self.fill(self.at)?;
        }
        Ok(())
    }

    /// Drops the bytes before `keep` and reads another block onto the end.
    ///
    /// `keep` is where the first record anything still points at starts, which is the record being
    /// read for one record at a time and the start of the chunk for a chunk.
    fn fill(&mut self, keep: usize) -> Result<()> {
        if self.offset >= self.cap {
            return Err(Error::io("a record runs further than a guessed start is followed"));
        }
        self.buffer.drain(..keep);
        self.at -= keep;
        let held = self.buffer.len();
        let want = match self.end.checked_sub(self.offset) {
            Some(left) if left > 0 => self.block.min(usize::try_from(left).unwrap_or(usize::MAX)),
            _ => self.block.min(TAIL.max(held)),
        };
        self.buffer.resize(held + want, 0);
        let read = self.file.read_at(self.offset, &mut self.buffer[held..])?;
        self.buffer.truncate(held + read);
        self.offset += read as u64;
        if read == 0 {
            self.drained = true;
        }
        Ok(())
    }

    /// Reads as much again as is held onto the end of the buffer, for a sample the first block
    /// did not hold all of, and says whether the file may have more. Doubling keeps the rows split
    /// again each time to twice the sample.
    fn grow(&mut self) -> Result<bool> {
        let (block, held) = (self.block, self.buffer.len());
        let asked = held.max(block);
        self.block = asked;
        let grown = self.fill(0);
        self.block = block;
        grown?;
        Ok(self.buffer.len() == held + asked)
    }

    /// Whether a field read one record at a time is a null, which is the same test
    /// [`convert::is_null`] makes on a range. A field that was `quoted` is never one when the call
    /// turned `allow_quoted_nulls` off.
    fn is_null(&self, text: &str, quoted: bool) -> bool {
        if quoted && self.given.quoted_values {
            return false;
        }
        match &self.nulls {
            None => text.is_empty(),
            Some(nulls) => nulls.iter().any(|null| null.as_slice() == text.as_bytes()),
        }
    }

    /// The records the sniffer gets to look at, which is the sample or the file, whichever is
    /// shorter.
    fn sample_rows(&self, sample: &[u8], eof: bool) -> Result<Vec<Vec<Option<String>>>> {
        let mut rows = Vec::new();
        let mut fields = Vec::new();
        let mut quoted = Vec::new();
        let mut at = 0;
        while rows.len() <= self.given.sample() {
            // Unless the block is the end of the file, a record the block cut in half is simply not
            // part of the sample.
            let dialect = self.dialect;
            let Some(next) =
                crate::scan::record_quoted(sample, at, dialect, eof, &mut fields, &mut quoted)?
            else {
                break;
            };
            at = next;
            rows.push(
                fields
                    .iter()
                    .zip(&quoted)
                    .map(
                        |(text, &quoted)| {
                            if self.is_null(text, quoted) { None } else { Some(text.clone()) }
                        },
                    )
                    .collect(),
            );
        }
        Ok(rows)
    }
}

/// A format as the block under an error shows it, which is nothing for none.
fn shown(written: Option<&infer::Written>) -> &str {
    written.map_or("", |written| written.text.as_str())
}

/// Whether the first row is a header, and what the columns are called and typed.
///
/// The rule is DuckDB's and both halves of it were measured. A file whose columns are all `VARCHAR`
/// once the first row is set aside has a header, because two rows of words is a header and a row.
/// Otherwise the first row is a header exactly when it does not fit the types the rest of the file
/// has, which is what makes `1,2` over `3,4` a file of two rows and `a,b` over `1,2` a file of one.
///
/// `told` is the caller answering the question instead, which is `header=true` or `header=false` on
/// the call. It decides the names and the types as well as the row count, since a first row that is
/// data is a row the types have to fit and a first row that is a header is not.
///
/// `set` is the types the call gave its columns, which the question is then asked against rather
/// than against what the rest of the sample looks like, and which are the types that come back.
/// The question is asked of one row as well then, since the types no longer come from the rest.
///
/// `given` is the date and timestamp formats the call gave, and the formats that come back are the
/// ones the columns are read with, which the sniffer found when the call gave none and `detect` is
/// on. The question about the first row is asked with them, so `a` over `05/06/2019` has a header.
/// The columns the call did not set walk down `ladder`.
fn describe(
    rows: &[Vec<Option<String>>],
    told: Option<bool>,
    set: Option<&[LogicalType]>,
    given: &Clocks,
    detect: bool,
    ladder: &[LogicalType],
) -> (bool, Vec<Field>, Clocks) {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let sniffed = |rows: &[Vec<Option<String>>]| {
        let columns: Vec<Vec<Option<&str>>> = (0..width)
            .map(|at| rows.iter().map(|row| row.get(at).and_then(Option::as_deref)).collect())
            .collect();
        infer::sniff(&columns, set, given, detect, ladder)
    };
    let (body, clocks) = sniffed(&rows[1.min(rows.len())..]);
    let body = set.map_or(body, <[LogicalType]>::to_vec);
    let all_text = body.iter().all(|ty| *ty == LogicalType::Varchar);
    let first_fits = rows.first().is_some_and(|first| {
        first.iter().zip(&body).all(|(text, ty)| match text {
            None => true,
            Some(text) => infer::fits_in(text, ty, &clocks),
        })
    });
    // An empty file has no row to take names from, so it has no header whatever it was told.
    // A single row is a header too once the call set the types, which was measured: `1,2` alone
    // read as two `VARCHAR` columns comes back with no rows. So is a single row that is all text,
    // which was measured as well: `a,b` alone is a header and `a,1` alone is a row.
    let text = || sniffed(rows).0.iter().all(|ty| *ty == LogicalType::Varchar);
    let enough = rows.len() > 1 || set.is_some() || (rows.len() == 1 && text());
    let header = !rows.is_empty() && told.unwrap_or(enough && (all_text || !first_fits));
    if !header {
        // The first row is data, so the types and the formats are the whole sample's.
        let (types, clocks) = match set {
            Some(set) => (set.to_vec(), sniffed(rows).1),
            // An empty file is one text column, which is the pin's answer for one.
            None if rows.is_empty() => (vec![LogicalType::Varchar], clocks),
            None => sniffed(rows),
        };
        let fields = types
            .into_iter()
            .enumerate()
            .map(|(at, ty)| Field::new(format!("column{at}"), ty))
            .collect();
        return (false, fields, clocks);
    }
    let names = unique(&rows[0], width);
    let fields = body.into_iter().zip(names).map(|(ty, name)| Field::new(name, ty)).collect();
    (true, fields, clocks)
}

/// Puts the types a call set in place of the ones the sniffer found, refusing a set that names a
/// column the file does not have or that has more types than the file has columns, in the pin's
/// words.
///
/// The names that are not there are listed last given first, which is the order the pin's hash map
/// gave two of them in on `v2.0.0-dev84237`. It is not an order anybody chose, and with one name it
/// does not matter.
fn retype(fields: &mut [Field], set: &Retype) -> Result<()> {
    match set {
        Retype::Positional(types) if types.len() > fields.len() => {
            return Err(Error::binder(format!(
                "read_csv: {} types were provided, but CSV file only has {} columns",
                types.len(),
                fields.len()
            )));
        }
        Retype::Named(types) => {
            let missing: Vec<String> = types
                .iter()
                .rev()
                .filter(|(name, _)| {
                    let name = name.to_lowercase();
                    !fields.iter().any(|field| field.name.to_lowercase() == name)
                })
                .map(|(name, _)| format!("\"{name}\""))
                .collect();
            if !missing.is_empty() {
                return Err(Error::binder(format!(
                    "COLUMN_TYPES error: Columns with names: {} do not exist in the CSV File",
                    missing.join(",")
                )));
            }
        }
        Retype::Positional(_) => {}
    }
    for (at, field) in fields.iter_mut().enumerate() {
        if let Some(ty) = set.of(at, &field.name) {
            field.ty = ty.clone();
        }
    }
    Ok(())
}

/// The column names a header row gives, with the collisions resolved the way DuckDB resolves them.
///
/// A header is text somebody typed and nothing stops it naming two columns the same thing, so the
/// second one gets `_1`, and the count goes up until the name is free. It has to count rather than
/// stop at one, because the suffix can collide too: a file whose header is `a,a,a_1` comes back as
/// `a`, `a_1`, `a_1_1` from the binary, and it is the second column that took the name the third one
/// was written with.
///
/// The comparison ignores case and the written case is kept, which was measured: `a,a,A` comes back
/// as `a`, `a_1`, `A_2`, so `A` collided with `a` and then `A_1` collided with `a_1`. An empty
/// header cell is a column with no name, and it falls back to the generated one rather than to an
/// empty string that no query could write.
fn unique(header: &[Option<String>], width: usize) -> Vec<String> {
    let mut taken: Vec<String> = Vec::with_capacity(width);
    for at in 0..width {
        let base = match header.get(at).and_then(Option::as_deref) {
            Some(written) => written.to_string(),
            None => format!("column{at}"),
        };
        let mut name = base.clone();
        let mut next = 1;
        while taken.iter().any(|held| held.eq_ignore_ascii_case(&name)) {
            name = format!("{base}_{next}");
            next += 1;
        }
        taken.push(name);
    }
    taken
}

/// The number of fields most of `rows` have, the larger of two that are as common, leaving out
/// empty lines, which are not rows in a file of more than one column.
fn commonest(rows: &[Vec<Option<String>>]) -> usize {
    let mut counts: Vec<usize> = Vec::new();
    for row in rows.iter().filter(|row| !matches!(row[..], [None])) {
        if counts.len() <= row.len() {
            counts.resize(row.len() + 1, 0);
        }
        counts[row.len()] += 1;
    }
    let most = counts.iter().copied().max().unwrap_or(0);
    counts.iter().rposition(|&count| count == most && count > 0).unwrap_or(1)
}

/// The pin's error for a row of `found` fields at `line` in a file of `width` columns, over the
/// block of settings the reader shows.
///
/// The advice to pad with nulls is there whenever padding is off, which was measured: a row with
/// too many fields gets it too, and the only thing that leaves it out is padding being on already.
/// The advice to turn strict mode off is there unless the call turned it off, which leaves the
/// short rows as the only ones refused.
fn wrong_width(
    given: &Given,
    original: &str,
    line: u64,
    width: usize,
    found: usize,
    settings: &str,
) -> Error {
    let strict = if given.strict_mode == Some(false) {
        ""
    } else {
        "* Disable the parser's strict mode (strict_mode=false) to allow reading rows that do not \
         comply with the CSV standard.\n"
    };
    let padding = if given.null_padding {
        ""
    } else {
        "* Enable null padding (null_padding=true) to replace missing values with NULL\n"
    };
    Error::invalid_input(format!(
        "CSV Error on Line: {line}\nOriginal Line: {original}\nExpected Number of Columns: {width} \
         Found: {found}\nPossible fixes:\n{strict}{padding}* Enable ignore errors \
         (ignore_errors=true) to skip this row\n{settings}"
    ))
}

/// Whether a sampled row is as wide as `width` columns, by the rule [`Reader::check_widths`] holds
/// the rows that are read to, with `padding` for `null_padding` and `strict` for `strict_mode`.
fn wide(row: &[Option<String>], width: usize, padding: bool, strict: bool) -> bool {
    match row.len() {
        found if found == width => true,
        1 if width > 1 && row[0].is_none() => true,
        found if found > width => !strict || row[width..].iter().all(Option::is_none),
        _ => padding,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudb_common::Value;
    use rudb_io::{Filesystem, OpenMode, SimFilesystem};
    use std::path::Path;

    fn read(text: &str) -> Reader {
        let filesystem = SimFilesystem::new();
        let path = Path::new("/t.csv");
        let file = filesystem.open(path, OpenMode::Create).expect("creates");
        file.write_at(0, text.as_bytes()).expect("writes");
        drop(file);
        let file = filesystem.open(path, OpenMode::Read).expect("opens");
        Reader::open(file, "/t.csv").expect("sniffs")
    }

    /// The same file opened with something already known about how it is written.
    fn read_with(text: &str, given: Given) -> Reader {
        let filesystem = SimFilesystem::new();
        let path = Path::new("/t.csv");
        let file = filesystem.open(path, OpenMode::Create).expect("creates");
        file.write_at(0, text.as_bytes()).expect("writes");
        drop(file);
        let file = filesystem.open(path, OpenMode::Read).expect("opens");
        Reader::open_with(file, "/t.csv", given).expect("reads")
    }

    fn names_and_types(reader: &Reader) -> Vec<(String, String)> {
        reader.fields().iter().map(|f| (f.name.clone(), f.ty.to_string())).collect()
    }

    fn all(reader: &mut Reader) -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        while let Some(chunk) = reader.next_chunk().expect("reads") {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|at| chunk.value_at(row, at)).collect());
            }
        }
        rows
    }

    #[test]
    fn a_header_that_names_two_columns_the_same_thing_counts_the_second_one_up() {
        let names: Vec<String> =
            read("a,a,A,a_1\n1,2,3,4\nx,y,z,w\n").fields().into_iter().map(|f| f.name).collect();
        // Measured against the binary, all four of them. The last one is the interesting one: the
        // second column took `a_1`, which is the name the fourth column was written with, so the
        // fourth has to keep counting from its own name rather than from `a`.
        assert_eq!(names, ["a", "a_1", "A_2", "a_1_1"]);
    }

    #[test]
    fn a_header_and_three_types_are_what_duckdb_sniffs_for_the_same_bytes() {
        let reader = read("a,b,c\n1,x,2.5\n2,y,3.5\n");
        assert_eq!(
            names_and_types(&reader),
            [
                ("a".to_string(), "BIGINT".to_string()),
                ("b".to_string(), "VARCHAR".to_string()),
                ("c".to_string(), "DOUBLE".to_string()),
            ]
        );
    }

    #[test]
    fn a_file_with_no_header_gets_the_names_duckdb_gives_it() {
        let reader = read("1,x\n2,y\n");
        assert_eq!(
            names_and_types(&reader),
            [
                ("column0".to_string(), "BIGINT".to_string()),
                ("column1".to_string(), "VARCHAR".to_string()),
            ]
        );
    }

    #[test]
    fn two_rows_of_words_are_a_header_and_a_row() {
        let reader = read("a,b\nc,d\n");
        assert_eq!(reader.fields().iter().map(|f| f.name.clone()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn one_column_of_words_under_a_row_of_numbers_is_still_a_header() {
        // `a,2` over `3,4`. One column disagreeing is enough, and the second column is then named
        // `2`, which is the text that was in it.
        let reader = read("a,2\n3,4\n");
        assert_eq!(reader.fields().iter().map(|f| f.name.clone()).collect::<Vec<_>>(), ["a", "2"]);
    }

    #[test]
    fn the_rows_are_the_rows_of_the_file() {
        let mut reader = read("a,b\n1,x\n2,y\n");
        assert_eq!(
            all(&mut reader),
            [
                vec![Value::BigInt(1), Value::Varchar("x".into())],
                vec![Value::BigInt(2), Value::Varchar("y".into())],
            ]
        );
    }

    #[test]
    fn an_empty_field_is_a_null_whether_it_was_quoted_or_not() {
        // Measured. `allow_quoted_nulls` is on by default, so `""` is a null and not the empty
        // string, which is the one place a quoted field and a bare one agree about being nothing.
        let mut reader = read("a,b\n1,\n\"\",y\n");
        assert_eq!(
            all(&mut reader),
            [vec![Value::BigInt(1), Value::Null], vec![Value::Null, Value::Varchar("y".into())],]
        );
    }

    #[test]
    fn a_timestamp_with_an_offset_reads_as_the_instant_it_names() {
        // Measured with `TimeZone` set to UTC: the first is 08:00 UTC and the second 10:00.
        let mut reader = read("a\n2020-01-01 10:00:00+02\n2020-01-01 10:00:00\n");
        assert_eq!(
            names_and_types(&reader),
            [("a".to_string(), "TIMESTAMP WITH TIME ZONE".into())]
        );
        let hour = 3_600_000_000_i64;
        let day = 1_577_836_800_000_000_i64;
        assert_eq!(
            all(&mut reader),
            [vec![Value::TimestampTz(day + 8 * hour)], vec![Value::TimestampTz(day + 10 * hour)]]
        );
    }

    #[test]
    fn a_sniffed_comment_leaves_its_lines_and_the_ends_of_lines_out() {
        let mut reader = read("a,b\n# c\n1,2# three\n#\n3,#4\n");
        assert_eq!(reader.dialect().comment, Some(b'#'));
        assert_eq!(
            all(&mut reader),
            [vec![Value::BigInt(1), Value::BigInt(2)], vec![Value::BigInt(3), Value::Null]]
        );
        // A call that says there is no comment reads the `#` as data.
        let given = Given { comment: Some(None), header: Some(true), ..Given::default() };
        let reader = read_with("a\n#1\n2\n", given);
        assert_eq!(reader.dialect().comment, None);
        assert_eq!(names_and_types(&reader), [("a".to_string(), "VARCHAR".into())]);
    }

    #[test]
    fn allow_quoted_nulls_off_keeps_a_quoted_empty_field_as_the_empty_string() {
        let text = "a,b\nx,\n\"\",y\n";
        let given = Given { quoted_values: true, ..Given::default() };
        let mut reader = read_with(text, given);
        let empty = Value::Varchar(String::new());
        assert_eq!(
            all(&mut reader),
            [
                vec![Value::Varchar("x".into()), Value::Null],
                vec![empty, Value::Varchar("y".into())]
            ]
        );
    }

    #[test]
    fn force_not_null_reads_a_null_in_the_columns_it_names_as_the_empty_string() {
        let text = "a,b\nx,\n\"\",y\n";
        let empty = Value::Varchar(String::new());
        let given = Given { force_not_null: vec!["B".into()], ..Given::default() };
        let mut reader = read_with(text, given);
        assert_eq!(
            all(&mut reader),
            [
                vec![Value::Varchar("x".into()), empty.clone()],
                vec![Value::Null, Value::Varchar("y".into())]
            ]
        );
        let given = Given { force_not_null: vec!["*".into()], ..Given::default() };
        let mut reader = read_with(text, given);
        assert_eq!(
            all(&mut reader),
            [
                vec![Value::Varchar("x".into()), empty.clone()],
                vec![empty, Value::Varchar("y".into())]
            ]
        );
    }

    #[test]
    fn a_projection_picks_columns_out_by_position_and_can_reorder_them() {
        let mut reader = read("a,b,c\n1,x,2.5\n");
        reader.project(&[2, 0]).expect("projects");
        assert_eq!(reader.fields().iter().map(|f| f.name.clone()).collect::<Vec<_>>(), ["c", "a"]);
        assert_eq!(all(&mut reader), [vec![Value::Double(2.5), Value::BigInt(1)]]);
    }

    #[test]
    fn a_projection_of_nothing_still_counts_the_rows() {
        let mut reader = read("a,b\n1,x\n2,y\n3,z\n");
        reader.project(&[]).expect("projects");
        let chunk = reader.next_chunk().expect("reads").expect("a chunk");
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.width(), 0);
    }

    #[test]
    fn a_pipe_separated_file_reads_as_one() {
        let mut reader = read("a|b\n1|x\n");
        assert_eq!(reader.dialect().delimiter, b'|');
        assert_eq!(all(&mut reader), [vec![Value::BigInt(1), Value::Varchar("x".into())]]);
    }

    #[test]
    fn a_quoted_field_with_a_delimiter_in_it_is_one_value() {
        let mut reader = read("a,b\n1,\"x,y\"\n");
        assert_eq!(all(&mut reader), [vec![Value::BigInt(1), Value::Varchar("x,y".into())]]);
    }

    #[test]
    fn more_rows_than_fit_one_chunk_arrive_as_more_than_one_chunk() {
        let mut text = String::from("a\n");
        for row in 0..VECTOR_SIZE + 5 {
            text.push_str(&format!("{row}\n"));
        }
        let mut reader = read(&text);
        let first = reader.next_chunk().expect("reads").expect("a chunk");
        assert_eq!(first.len(), VECTOR_SIZE);
        let second = reader.next_chunk().expect("reads").expect("a second chunk");
        assert_eq!(second.len(), 5);
        assert!(reader.next_chunk().expect("reads").is_none());
    }

    #[test]
    fn a_value_the_sniffer_never_saw_is_an_error_rather_than_a_wider_column() {
        // The value has to be past the sample, because a value inside it would have widened the
        // column to VARCHAR and there would be nothing to fail. Widening after the fact is not an
        // option: the chunks before this one have already gone out with the narrow type on them.
        let mut text = String::from("c\n");
        for row in 0..infer::SAMPLE {
            text.push_str(&format!("{row}\n"));
        }
        text.push_str("oops\n");
        let mut reader = read(&text);
        assert_eq!(reader.fields()[0].ty, LogicalType::BigInt);
        let error = all_or_error(&mut reader).unwrap_err();
        let line = infer::SAMPLE + 2;
        assert!(error.message().starts_with(&format!("CSV Error on Line: {line}")), "{error}");
        assert!(
            error.message().contains("Could not convert string \"oops\" to 'BIGINT'"),
            "{error}"
        );
        assert!(error.message().contains("sample_size = 20480"), "{error}");
    }

    /// A chunk is converted a block of rows at a time, which meets the second column's bad value
    /// in the first block before the first column's in a later one. The error still names the
    /// first column's, as it did when a chunk was converted a column at a time.
    #[test]
    fn the_error_names_the_first_columns_bad_value_even_when_a_later_column_is_bad_sooner() {
        let mut text = String::from("a,b\n");
        for row in 0..infer::SAMPLE {
            text.push_str(&format!("{row},{row}\n"));
        }
        text.push_str("1,late\n");
        for row in 0..BLOCK_ROWS * 2 {
            text.push_str(&format!("{row},{row}\n"));
        }
        text.push_str("early,1\n");
        let mut reader = read(&text);
        let error = all_or_error(&mut reader).unwrap_err();
        assert!(error.message().contains("Could not convert string \"early\""), "{error}");
    }

    /// Measured. The header row becomes a row, so the names are the generated ones and the first
    /// column holds `a`, `1` and `2`, which is text rather than the BIGINT the sniffer would say.
    #[test]
    fn a_file_told_it_has_no_header_reads_its_first_line_as_a_row() {
        let mut reader =
            read_with("a,b\n1,x\n2,y\n", Given { header: Some(false), ..Given::default() });
        assert_eq!(
            names_and_types(&reader),
            [
                ("column0".to_string(), "VARCHAR".to_string()),
                ("column1".to_string(), "VARCHAR".to_string()),
            ]
        );
        assert_eq!(all(&mut reader).len(), 3);
    }

    /// The other way round, on a file the sniffer would call two rows of data.
    #[test]
    fn a_file_told_it_has_a_header_takes_its_first_line_as_the_names() {
        let reader = read_with("1,2\n3,4\n", Given { header: Some(true), ..Given::default() });
        assert_eq!(reader.fields().iter().map(|f| f.name.clone()).collect::<Vec<_>>(), ["1", "2"]);
    }

    /// A given delimiter is used rather than tried, so a file that is really commas is one column.
    #[test]
    fn a_given_delimiter_is_the_delimiter_whatever_the_file_looks_like() {
        let reader = read_with("a,b\n1,x\n", Given { delimiter: Some(b';'), ..Given::default() });
        assert_eq!(reader.dialect().delimiter, b';');
        assert_eq!(reader.fields().len(), 1);
    }

    /// A quote the sniffer would never find, since it only ever looks for the double quote.
    #[test]
    fn a_given_quote_makes_a_field_that_holds_the_delimiter_one_value() {
        let mut reader =
            read_with("a,b\n1,'x,y'\n", Given { quote: Some(b'\''), ..Given::default() });
        assert_eq!(all(&mut reader), [vec![Value::BigInt(1), Value::Varchar("x,y".into())]]);
    }

    /// The block under a conversion error says which of its lines the caller wrote down.
    #[test]
    fn the_block_says_set_by_user_for_what_the_call_gave_it() {
        let mut text = String::from("c;d\n");
        for row in 0..infer::SAMPLE {
            text.push_str(&format!("{row};x\n"));
        }
        text.push_str("oops;x\n");
        let given = Given { delimiter: Some(b';'), ..Given::default() };
        let mut reader = read_with(&text, given);
        let error = all_or_error(&mut reader).unwrap_err();
        assert!(error.message().contains("delimiter = ; (Set By User)"), "{error}");
        assert!(error.message().contains("header = true (Auto-Detected)"), "{error}");
    }

    #[test]
    fn a_file_told_a_wider_type_than_it_sniffed_reads_its_whole_numbers_as_that_type() {
        // What a glob does to every file it names. This file on its own is BIGINT and the set it
        // belongs to is DOUBLE because some other file in it holds a decimal, so the column comes
        // out DOUBLE and the rows come with it rather than the reader being overruled afterwards.
        let mut reader = read("a\n1\n2\n");
        assert_eq!(reader.fields()[0].ty, LogicalType::BigInt);
        reader.retype(&[LogicalType::Double]).expect("one type for one column");
        assert_eq!(reader.fields()[0].ty, LogicalType::Double);
        assert_eq!(all(&mut reader), [[Value::Double(1.0)], [Value::Double(2.0)]]);
    }

    /// Measured on DuckDB `v2.0.0-dev84237`. With a null string that is not the empty one, an
    /// empty field is an empty string, the null string is a null quoted or not, and `""` is an
    /// empty string too, because the quoted form of a null is the null string quoted.
    #[test]
    fn a_null_string_other_than_the_empty_one_leaves_an_empty_field_as_text() {
        let given =
            Given { header: Some(false), nulls: Some(vec!["NA".to_string()]), ..Given::default() };
        let mut reader = read_with("x,NA\n,\"NA\"\n\"\",y\n", given);
        let text = |value: &str| Value::Varchar(value.into());
        assert_eq!(
            all(&mut reader),
            [vec![text("x"), Value::Null], vec![text(""), Value::Null], vec![text(""), text("y")],]
        );
    }

    #[test]
    fn a_list_of_null_strings_makes_each_of_them_a_null() {
        let given = Given {
            header: Some(false),
            nulls: Some(vec!["NA".to_string(), "-".to_string()]),
            ..Given::default()
        };
        let mut reader = read_with("1,NA\n-,2\n", given);
        assert_eq!(
            all(&mut reader),
            [vec![Value::BigInt(1), Value::Null], vec![Value::Null, Value::BigInt(2)]]
        );
    }

    #[test]
    fn given_names_rename_the_first_columns_and_too_many_of_them_are_refused() {
        let names = |list: &[&str]| Some(list.iter().map(ToString::to_string).collect());
        let reader = read_with("1,2,3\n", Given { names: names(&["p"]), ..Given::default() });
        let found: Vec<String> = reader.fields().iter().map(|f| f.name.clone()).collect();
        assert_eq!(found, ["p", "column1", "column2"]);

        let filesystem = SimFilesystem::new();
        let path = Path::new("/t.csv");
        let file = filesystem.open(path, OpenMode::Create).expect("creates");
        file.write_at(0, b"1,2\n").expect("writes");
        drop(file);
        let file = filesystem.open(path, OpenMode::Read).expect("opens");
        let given = Given { names: names(&["a", "b", "c"]), ..Given::default() };
        let Err(error) = Reader::open_with(file, "/t.csv", given) else {
            panic!("three names for two columns are refused")
        };
        assert!(
            error.message().ends_with("Number of expected columns: 3. Actual number of columns 2"),
            "{error}"
        );
    }

    /// The table's types are the caller's, so the advice is DuckDB's for a type that was set rather
    /// than the sample size one it gives a type it guessed.
    #[test]
    fn a_bad_value_in_a_column_whose_type_was_set_gets_the_advice_for_a_set_type() {
        let given = Given { header: Some(false), typed: true, ..Given::default() };
        let mut reader = read_with("1,x\nfoo,y\n", given);
        reader.retype(&[LogicalType::Integer, LogicalType::Varchar]).expect("two for two");
        let error = all_or_error(&mut reader).unwrap_err();
        let message = error.message();
        assert!(message.starts_with("CSV Error on Line: 2"), "{error}");
        assert!(message.contains("Could not convert string \"foo\" to 'INTEGER'"), "{error}");
        assert!(
            message.contains(
                "Column column0 is being converted as type INTEGER\nThis type was either manually \
                 set or derived from an existing table."
            ),
            "{error}"
        );
        assert!(!message.contains("Possible solutions"), "{error}");
    }

    #[test]
    fn a_type_list_that_is_not_as_long_as_the_projection_is_refused() {
        let mut reader = read("a,b\n1,two\n");
        let error = reader.retype(&[LogicalType::Double]).unwrap_err();
        assert!(error.message().contains("1 types for a projection of 2 columns"), "{error}");
    }

    /// Every chunk a reader hands back, as its length and its values written out, or the error
    /// it stopped on. The values are compared as their debug text so that a NaN equals itself.
    fn drained(reader: &mut Reader, old: bool) -> (Vec<(usize, Vec<String>)>, Option<String>) {
        let mut chunks = Vec::new();
        loop {
            let next = if old { reader.next_chunk_by_record() } else { reader.next_chunk() };
            match next {
                Ok(Some(chunk)) => {
                    let mut values = Vec::new();
                    for row in 0..chunk.len() {
                        for at in 0..chunk.width() {
                            values.push(format!("{:?}", chunk.value_at(row, at)));
                        }
                    }
                    chunks.push((chunk.len(), values));
                }
                Ok(None) => return (chunks, None),
                Err(error) => return (chunks, Some(error.to_string())),
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

    /// A typed CSV file with a header, whose values are mostly what their column says and now and
    /// then something only the cast knows what to do with, or something nothing can convert.
    fn typed_file(rng: &mut Rng, rows: usize) -> String {
        const ODD: [&str; 27] = [
            "",
            " 1",
            "1 ",
            "1e3",
            "0x10",
            "inf",
            "-nan",
            "abc",
            "\"12\"",
            "\"a\"\"b\"",
            "\"x,y\"",
            "\"x\ny\"",
            "h\u{e9}llo",
            "99999999999999999999",
            "9999999999999999999",
            "-",
            "+5",
            "007",
            "2020-02-30",
            "2020-02-29",
            "0000-01-01",
            "TRUE",
            "no",
            "1_000",
            "1.5e-3",
            "-0",
            "\"\"",
        ];
        let width = 1 + rng.below(6);
        let kinds: Vec<usize> = (0..width).map(|_| rng.below(5)).collect();
        let mut text: String = (0..width).map(|at| format!("c{at}")).collect::<Vec<_>>().join(",");
        text.push('\n');
        for _ in 0..rows {
            let mut fields = Vec::with_capacity(width);
            for &kind in &kinds {
                let odd = rng.below(60) == 0;
                fields.push(if odd {
                    ODD[rng.below(ODD.len())].to_string()
                } else {
                    let n = rng.next();
                    match kind {
                        0 => format!("{}", (n % 2_000_001) as i64 - 1_000_000),
                        1 => format!("{}.{:02}", n % 100_000, n % 100),
                        2 => format!("{}-{:02}-{:02}", 1990 + n % 20, 1 + n % 12, 1 + n % 28),
                        3 => ["true", "false", "t", "F"][(n % 4) as usize].to_string(),
                        _ => ["x", "hello world", "a longer piece of text", "\"q,\"\"q\""]
                            [(n % 4) as usize]
                            .to_string(),
                    }
                });
            }
            // Short rows are read padded, and one cut down to an empty field is an empty line.
            if rng.below(200) == 0 {
                fields.pop();
            }
            if rng.below(200) == 0 {
                fields.push(String::new());
            }
            text.push_str(&fields.join(","));
            text.push_str(["\n", "\n", "\n", "\r\n", "\r"][rng.below(5)]);
        }
        if rng.below(4) == 0 {
            text.pop();
        }
        text
    }

    fn open_sized(text: &[u8], block: usize) -> Result<Reader> {
        let filesystem = SimFilesystem::new();
        let path = Path::new("/t.csv");
        let file = filesystem.open(path, OpenMode::Create).expect("creates");
        file.write_at(0, text).expect("writes");
        drop(file);
        let file = filesystem.open(path, OpenMode::Read).expect("opens");
        Reader::open_sized(file, "/t.csv", Given { null_padding: true, ..Given::default() }, block)
    }

    /// The chunk at a time reader and the record at a time one, over generated files, read with
    /// blocks small enough that records straddle the refills, projected and retyped at random so
    /// that every type with a parser of its own gets values it takes and values it has to pass on.
    #[test]
    fn generated_files_read_the_same_a_chunk_at_a_time_as_a_record_at_a_time() {
        let types = [
            LogicalType::BigInt,
            LogicalType::Integer,
            LogicalType::SmallInt,
            LogicalType::TinyInt,
            LogicalType::UBigInt,
            LogicalType::UInteger,
            LogicalType::USmallInt,
            LogicalType::UTinyInt,
            LogicalType::Double,
            LogicalType::Float,
            LogicalType::Date,
            LogicalType::Boolean,
            LogicalType::Varchar,
            LogicalType::Timestamp,
            LogicalType::Decimal { width: 18, scale: 3 },
        ];
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for case in 0..200 {
            let rows = if case % 100 == 0 { 8192 + rng.below(1000) } else { rng.below(200) };
            let mut text = typed_file(&mut rng, rows).into_bytes();
            if rng.below(20) == 0 {
                // A quoted field with rubbish after it somewhere, which is a malformed record.
                let at = rng.below(text.len() + 1);
                text.splice(at..at, *b",\"x\"y,");
            }
            for block in [1 << 20, 32 + rng.below(400)] {
                let (Ok(mut new), Ok(mut old)) =
                    (open_sized(&text, block), open_sized(&text, block))
                else {
                    continue;
                };
                assert_eq!(new.fields(), old.fields());
                let width = new.fields().len();
                if width > 0 && rng.below(2) == 0 {
                    let columns: Vec<usize> =
                        (0..rng.below(width + 2)).map(|_| rng.below(width)).collect();
                    new.project(&columns).expect("projects");
                    old.project(&columns).expect("projects");
                    let wanted: Vec<LogicalType> =
                        columns.iter().map(|_| types[rng.below(types.len())].clone()).collect();
                    new.retype(&wanted).expect("retypes");
                    old.retype(&wanted).expect("retypes");
                }
                let expected = drained(&mut old, true);
                let found = drained(&mut new, false);
                assert_eq!(found.1, expected.1, "case {case}, block {block}");
                assert_eq!(found.0, expected.0, "case {case}, block {block}");
            }
        }
    }

    /// How much faster the chunk at a time reader is, on a file shaped like TPC-H's `lineitem`.
    ///
    /// Not run by default, because it is a measurement rather than a check. Run it with
    /// `cargo test --release -p rudb-csv -- --ignored --nocapture reads_lineitem`.
    #[test]
    #[ignore = "a measurement, run by hand"]
    fn reads_lineitem_faster_a_chunk_at_a_time() {
        use rudb_io::RealFilesystem;
        use std::time::Instant;

        const ROWS: usize = 200_000;
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut text = String::from(
            "l_orderkey,l_partkey,l_suppkey,l_linenumber,l_quantity,l_extendedprice,l_discount,\
             l_tax,l_returnflag,l_linestatus,l_shipdate,l_commitdate,l_receiptdate,\
             l_shipinstruct,l_shipmode,l_comment\n",
        );
        let words = ["carefully", "final", "deposits", "furiously", "regular", "ideas", "sleep"];
        for row in 0..ROWS {
            let n = rng.next();
            let date = |shift: u64| {
                format!(
                    "{}-{:02}-{:02}",
                    1992 + (n >> shift) % 7,
                    1 + (n >> shift) % 12,
                    1 + (n >> shift) % 28
                )
            };
            let comment: Vec<&str> =
                (0..3 + n % 4).map(|k| words[((n >> (k * 3)) % 7) as usize]).collect();
            text.push_str(&format!(
                "{},{},{},{},{}.00,{}.{:02},0.0{},0.0{},{},{},{},{},{},{},{},{}\n",
                row / 4 + 1,
                n % 200_000,
                n % 10_000,
                row % 4 + 1,
                1 + n % 50,
                900 + n % 100_000,
                n % 100,
                n % 10,
                (n >> 7) % 9,
                ["A", "N", "R"][(n % 3) as usize],
                ["O", "F"][(n % 2) as usize],
                date(3),
                date(11),
                date(19),
                ["DELIVER IN PERSON", "NONE", "TAKE BACK RETURN"][(n % 3) as usize],
                ["TRUCK", "MAIL", "AIR", "SHIP"][(n % 4) as usize],
                comment.join(" "),
            ));
        }
        let path = std::env::temp_dir().join(format!("rudb-lineitem-{}.csv", std::process::id()));
        std::fs::write(&path, &text).expect("writes");
        let megabytes = text.len() as f64 / 1e6;
        let filesystem = RealFilesystem::new();
        let mut best = [f64::MAX; 2];
        for _ in 0..5 {
            for (slot, old) in [(0, true), (1, false)] {
                let file = filesystem.open(&path, OpenMode::Read).expect("opens");
                let mut reader = Reader::open(file, "lineitem.csv").expect("sniffs");
                let started = Instant::now();
                let mut rows = 0;
                loop {
                    let next =
                        if old { reader.next_chunk_by_record() } else { reader.next_chunk() };
                    let Some(chunk) = next.expect("reads") else { break };
                    rows += chunk.len();
                }
                assert_eq!(rows, ROWS);
                best[slot] = best[slot].min(started.elapsed().as_secs_f64());
            }
        }
        std::fs::remove_file(&path).expect("removes");
        let (old, new) = (megabytes / best[0], megabytes / best[1]);
        println!(
            "{ROWS} rows, {megabytes:.1} MB: a record at a time {old:.1} MB/s, a chunk at a time"
        );
        println!("{new:.1} MB/s, {:.2}x", best[0] / best[1]);
    }

    fn two_integers(given: Given) -> Given {
        Given {
            fixed: true,
            header: Some(true),
            names: Some(vec!["a".into(), "b".into()]),
            types: Some(vec![LogicalType::Integer, LogicalType::Integer]),
            ..given
        }
    }

    fn integers(rows: &[&[Option<i32>]]) -> Vec<Vec<Value>> {
        let value = |cell: &Option<i32>| cell.map_or(Value::Null, Value::Integer);
        rows.iter().map(|row| row.iter().map(value).collect()).collect()
    }

    #[test]
    fn a_row_of_the_wrong_width_is_refused_unless_it_is_padded_or_left_out() {
        let text = "a,b\n1,2\n3\n4,5\n";
        let error = read_or_error(text, two_integers(Given::default())).unwrap_err();
        assert!(
            error.message().starts_with(
                "CSV Error on Line: 3\nOriginal Line: 3\nExpected Number of Columns: 2 Found: 1\n"
            ),
            "{error}"
        );
        assert!(error.message().contains("(null_padding=true)"), "{error}");
        let padded = two_integers(Given { null_padding: true, ..Given::default() });
        assert_eq!(
            all(&mut read_with(text, padded)),
            integers(&[&[Some(1), Some(2)], &[Some(3), None], &[Some(4), Some(5)]])
        );
        let ignored = two_integers(Given { ignore_errors: true, ..Given::default() });
        assert_eq!(
            all(&mut read_with("a,b\n1,2\n3\n4,5,6\n7,8\n", ignored)),
            integers(&[&[Some(1), Some(2)], &[Some(7), Some(8)]])
        );
        // Padding does nothing for a row that is too long, and the advice to pad is left out.
        let padded = two_integers(Given { null_padding: true, ..Given::default() });
        let error = read_or_error("a,b\n1,2\n4,5,6\n", padded).unwrap_err();
        assert!(error.message().contains("Found: 3\n"), "{error}");
        assert!(!error.message().contains("(null_padding=true)"), "{error}");
    }

    #[test]
    fn an_empty_line_and_empty_fields_past_the_last_column_are_not_errors() {
        let text = "a,b\n1,2\n\n3,4,,\n5,\"6\",\n";
        assert_eq!(
            all(&mut read_with(text, two_integers(Given::default()))),
            integers(&[&[Some(1), Some(2)], &[Some(3), Some(4)], &[Some(5), Some(6)]])
        );
        let error = read_or_error("a,b\n1,2\n3,4, \n", two_integers(Given::default())).unwrap_err();
        assert!(error.message().contains("Found: 3\n"), "{error}");
        // A line left out still counts when a later one is named.
        let error = read_or_error("a,b\n1,2\n\n4,x\n", two_integers(Given::default())).unwrap_err();
        assert!(
            error.message().starts_with("CSV Error on Line: 4\nOriginal Line: 4,x\n"),
            "{error}"
        );
    }

    #[test]
    fn ignoring_errors_leaves_out_the_rows_with_a_value_a_read_column_cannot_take() {
        let text = "a,b\n1,2\n3,x\n5,6\n";
        let ignored = || two_integers(Given { ignore_errors: true, ..Given::default() });
        assert_eq!(
            all(&mut read_with(text, ignored())),
            integers(&[&[Some(1), Some(2)], &[Some(5), Some(6)]])
        );
        let mut reader = read_with(text, ignored());
        reader.project(&[0]).expect("projects");
        assert_eq!(all(&mut reader), integers(&[&[Some(1)], &[Some(3)], &[Some(5)]]));
        // Sniffed, the file is as wide as most of its rows and the rest are left out.
        let text = "a,b\n1,2\n3\n4,5\n6,7,8\n9,x\n10,11\n";
        let mut reader = read_with(text, Given { ignore_errors: true, ..Given::default() });
        assert_eq!(
            names_and_types(&reader),
            [("a".into(), "BIGINT".into()), ("b".into(), "VARCHAR".into())]
        );
        assert_eq!(all(&mut reader).len(), 4);
    }

    /// Every row of the file, or the error opening or reading it, since a file that is not sniffed
    /// is held to its width from the first rows on.
    fn read_or_error(text: &str, given: Given) -> Result<Vec<Vec<Value>>> {
        let filesystem = SimFilesystem::new();
        let path = Path::new("/t.csv");
        let file = filesystem.open(path, OpenMode::Create).expect("creates");
        file.write_at(0, text.as_bytes()).expect("writes");
        drop(file);
        let file = filesystem.open(path, OpenMode::Read).expect("opens");
        all_or_error(&mut Reader::open_with(file, "/t.csv", given)?)
    }

    fn all_or_error(reader: &mut Reader) -> Result<Vec<Vec<Value>>> {
        let mut rows = Vec::new();
        while let Some(chunk) = reader.next_chunk()? {
            for row in 0..chunk.len() {
                rows.push((0..chunk.width()).map(|at| chunk.value_at(row, at)).collect());
            }
        }
        Ok(rows)
    }
}
