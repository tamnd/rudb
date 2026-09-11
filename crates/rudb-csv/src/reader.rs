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

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_io::File;
use rudb_kernels::cast_value;
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::dialect::{self, Dialect};
use crate::infer;

/// How much is read at a time, and how much the sniffer gets to look at.
///
/// A megabyte holds well over the twenty thousand rows of the sample for any file with ordinary
/// rows in it, and for a file with enormous rows the sniffer sees fewer of them and says so by
/// getting a wider type rather than by failing.
const BLOCK: usize = 1 << 20;

/// A CSV file, positioned at a record boundary.
#[derive(Debug)]
pub struct Reader {
    file: Box<dyn File>,
    path: String,
    dialect: Dialect,
    fields: Vec<Field>,
    projection: Vec<usize>,
    buffer: Vec<u8>,
    at: usize,
    offset: u64,
    drained: bool,
    line: u64,
    scratch: Vec<String>,
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
        let mut reader = Self {
            file,
            path: path.to_string(),
            dialect: Dialect::comma_separated(),
            fields: Vec::new(),
            projection: Vec::new(),
            buffer: Vec::new(),
            at: 0,
            offset: 0,
            drained: false,
            line: 1,
            scratch: Vec::new(),
        };
        reader.fill()?;
        let sample = reader.buffer.clone();
        let quote = dialect::quote(&sample);
        let delimiter = dialect::delimiter(&sample, quote)?;
        reader.dialect = Dialect { delimiter, quote, escape: quote, header: false };
        let rows = reader.sample_rows(&sample)?;
        let (header, fields) = describe(&rows);
        reader.dialect.header = header;
        reader.fields = fields;
        reader.projection = (0..reader.fields.len()).collect();
        if header {
            reader.skip_record()?;
        }
        Ok(reader)
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
    /// One file settles the types of a whole glob, because a scan produces one stream and a stream
    /// has one schema. Every file after the first is sniffed on its own and then told what the
    /// answer already was, which is the only way a second file whose column happens to hold nothing
    /// but integers still comes out as the DOUBLE the first file made it. A value that then does not
    /// fit is the conversion error, named and lined the way any other one is.
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

    /// The next chunk, or `None` at the end of the file.
    ///
    /// # Errors
    ///
    /// A read error, a malformed record, or a value that does not fit the type the sample chose
    /// for its column.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        while rows.len() < VECTOR_SIZE {
            match self.next_record()? {
                Some(fields) => rows.push(fields),
                None => break,
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        let mut columns = Vec::with_capacity(self.projection.len());
        for &at in &self.projection {
            let field = &self.fields[at];
            let mut values = Vec::with_capacity(rows.len());
            for (row, held) in rows.iter().enumerate() {
                let text = held.get(at).and_then(Option::as_deref);
                values.push(self.convert(
                    text,
                    field,
                    self.line - rows.len() as u64 + row as u64,
                )?);
            }
            columns.push(Vector::from_values(field.ty.clone(), &values)?);
        }
        Ok(Some(Chunk::with_rows(columns, rows.len())?))
    }

    /// One value, cast from its text to the column's type.
    fn convert(&self, text: Option<&str>, field: &Field, line: u64) -> Result<Value> {
        let Some(text) = text else { return Ok(Value::Null) };
        if field.ty == LogicalType::Varchar {
            return Ok(Value::Varchar(text.to_string()));
        }
        let value = Value::Varchar(text.to_string());
        match cast_value(&value, &field.ty, false) {
            Ok(converted) => Ok(converted),
            Err(_) => Err(Error::conversion(self.conversion_error(text, field, line))),
        }
    }

    /// DuckDB's message for a value that does not fit the type its column was sniffed as.
    ///
    /// Reproduced whole, including the block of settings at the bottom, because that block is the
    /// answer to the question the message raises. Somebody reading it wants to know what was
    /// guessed and how to override the guess, and a shorter message would send them to the
    /// documentation to find out.
    fn conversion_error(&self, text: &str, field: &Field, line: u64) -> String {
        format!(
            "CSV Error on Line: {line}\nOriginal Line: {text}\nError when converting column \
             \"{}\". Could not convert string \"{text}\" to '{}'\n\nColumn {} is being converted \
             as type {}\nThis type was auto-detected from the CSV file.\nPossible solutions:\n* \
             Override the type for this column manually by setting the type explicitly, e.g., \
             types={{'{}': 'VARCHAR'}}\n* Set the sample size to a larger value to enable the \
             auto-detection to scan more values, e.g., sample_size=-1\n* Use a COPY statement to \
             automatically derive types from an existing table.\n* Check whether the null string \
             value is set correctly (e.g., nullstr = 'N/A')\n\n  file = {}\n  delimiter = {} \
             (Auto-Detected)\n  quote = {} (Auto-Detected)\n  escape = {} (Auto-Detected)\n  \
             header = {} (Auto-Detected)\n  sample_size = {}\n",
            field.name,
            field.ty,
            field.name,
            field.ty,
            field.name,
            self.path,
            Dialect::shown(Some(self.dialect.delimiter)),
            Dialect::shown(self.dialect.quote),
            Dialect::shown(self.dialect.escape),
            self.dialect.header,
            infer::SAMPLE,
        )
    }

    /// The next record, as one entry per field, with an empty field as a null.
    fn next_record(&mut self) -> Result<Option<Vec<Option<String>>>> {
        let Some(()) = self.advance()? else { return Ok(None) };
        Ok(Some(
            self.scratch
                .iter()
                .map(|text| if text.is_empty() { None } else { Some(text.clone()) })
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
                None => self.fill()?,
            }
        }
    }

    /// Reads one record and throws it away, which is what a header is.
    fn skip_record(&mut self) -> Result<()> {
        self.advance()?;
        Ok(())
    }

    /// Drops what has been read and reads another block onto the end.
    fn fill(&mut self) -> Result<()> {
        self.buffer.drain(..self.at);
        self.at = 0;
        let held = self.buffer.len();
        self.buffer.resize(held + BLOCK, 0);
        let read = self.file.read_at(self.offset, &mut self.buffer[held..])?;
        self.buffer.truncate(held + read);
        self.offset += read as u64;
        if read == 0 {
            self.drained = true;
        }
        Ok(())
    }

    /// The records the sniffer gets to look at, which is the sample or the file, whichever is
    /// shorter.
    fn sample_rows(&self, sample: &[u8]) -> Result<Vec<Vec<Option<String>>>> {
        let mut rows = Vec::new();
        let mut fields = Vec::new();
        let mut at = 0;
        while rows.len() <= infer::SAMPLE {
            // The end of the block is not the end of the file, so a record the block cut in half is
            // simply not part of the sample.
            let Some(next) = crate::scan::record(sample, at, self.dialect, false, &mut fields)?
            else {
                break;
            };
            at = next;
            rows.push(
                fields
                    .iter()
                    .map(|text| if text.is_empty() { None } else { Some(text.clone()) })
                    .collect(),
            );
        }
        Ok(rows)
    }
}

/// Whether the first row is a header, and what the columns are called and typed.
///
/// The rule is DuckDB's and both halves of it were measured. A file whose columns are all `VARCHAR`
/// once the first row is set aside has a header, because two rows of words is a header and a row.
/// Otherwise the first row is a header exactly when it does not fit the types the rest of the file
/// has, which is what makes `1,2` over `3,4` a file of two rows and `a,b` over `1,2` a file of one.
fn describe(rows: &[Vec<Option<String>>]) -> (bool, Vec<Field>) {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let body = types(&rows[1.min(rows.len())..], width);
    let all_text = body.iter().all(|ty| *ty == LogicalType::Varchar);
    let first_fits = rows.first().is_some_and(|first| {
        first.iter().zip(&body).all(|(text, ty)| match text {
            None => true,
            Some(text) => infer::fits(text, ty),
        })
    });
    let header = rows.len() > 1 && (all_text || !first_fits);
    if !header {
        let types = types(rows, width);
        let fields = types
            .into_iter()
            .enumerate()
            .map(|(at, ty)| Field::new(format!("column{at}"), ty))
            .collect();
        return (false, fields);
    }
    let names = unique(&rows[0], width);
    let fields = body.into_iter().zip(names).map(|(ty, name)| Field::new(name, ty)).collect();
    (true, fields)
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

/// The type of each of `width` columns, over these rows.
fn types(rows: &[Vec<Option<String>>], width: usize) -> Vec<LogicalType> {
    (0..width)
        .map(|at| {
            let values: Vec<Option<&str>> =
                rows.iter().map(|row| row.get(at).and_then(Option::as_deref)).collect();
            infer::column(&values)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
