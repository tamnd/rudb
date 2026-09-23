//! Chunks written to a file so a bucket can be streamed instead of assembled.
//!
//! `spill.rs` next door writes a row at a time as a `&[Value]`, which is the right shape for a hash
//! aggregate: it is holding rows one at a time anyway and it has no chunk to hand over. A bucket in
//! a partitioned write is the other case. It is already a list of chunks, and taking one apart into
//! sixty million `Value`s to write it and building sixty million more to read it back is most of
//! what the spill would cost, with the encoder then rebuilding chunks out of them a third time.
//!
//! So this takes columns and gives chunks back, with the rows in the order they went in. That
//! ordering is the point rather than a convenience. Section 15.6 of `tenx/15-the-partitioned-write.md` is about a
//! measurement that says a sink's peak is inside its own `finalize`, where it holds its input and
//! the answer it is assembling from it at once, and that nothing a downstream reader does can lower
//! a peak it is already past. The way out is an answer that is never assembled, which means the
//! operator hands back a file and something reads it a chunk at a time.
//!
//! # The encoding
//!
//! A column at a time, and this is the part worth explaining because the obvious layout is the
//! other one. A run written as a sequence of chunks cannot be written at all until every column has
//! been laid, since a chunk holds a row's worth of all of them, so the whole sorted output has to
//! exist in memory before any of it reaches the file. The output is bigger than the input that made
//! it: rows arrive from a scan encoded, dictionary coded or page backed, and putting them in order
//! lays them flat. Measured on the lineitem load clustered on the quarter, writing a run cost
//! between 1.55 and 1.66 times the payload the sort was holding, all of it resident at once, and
//! that is memory the sort is spilling because it does not have. Issue #1347 has the numbers.
//!
//! Written a column at a time, a run costs one column of output instead. The caller lays a column,
//! hands it over, and drops it before laying the next, which is what it was already doing with the
//! input.
//!
//! So the file is the first column's blocks end to end, then the second column's, and so on. Per
//! block: a validity byte, a layout byte, the payload. Where each column starts and how long each
//! of its blocks is are kept in this struct rather than in the file, because the value that wrote
//! the file is the value that reads it back and nothing else ever opens it.
//!
//! A reader wants rows, so it reads block `n` of every column and puts them together. That means
//! one cursor a column, each moving forwards through its own stretch of the file, and a seek
//! between them. Seventeen seeks per eight thousand rows is not something a spilled sort notices.
//!
//! The validity byte says all valid, all null, or a mask, and a mask is one byte per row rather
//! than one bit. That is eight times the bytes for a column with some nulls and one byte for a
//! column with none, which is the case worth spending the format on: lineitem has no nulls in any
//! column at all.
//!
//! The layout byte is what the column turned out to be rather than what its type says it should be,
//! and the two are checked against each other on the way back in. That makes the file self checking
//! the same way the per value tag in `spill.rs` does, at one byte per column per block rather than
//! one per value, and it is the check worth having here because a column written as one width and
//! read back as another is a different number rather than an error.
//!
//! A fixed width payload is the values end to end in the machine's own byte order. A string payload
//! is a length and then the bytes, per row, with a null written as an empty string that the
//! validity byte is what actually distinguishes.
//!
//! None of that is portable and none of it needs to be. The file lives under the system temporary
//! directory, it is removed when the [`Runs`] is dropped including on the error path, and nothing
//! outside this process ever reads it. There is no compression for the same kind of reason: what
//! this competes against is holding the bucket in memory rather than in a file, so a pass of
//! compression each way is a cost the memory it saves would have to earn back first.
//!
//! What goes out is flattened. A dictionary, a constant and a sequence are all compact in memory
//! and none of them is worth an encoding of its own on a file that lives for one query, so
//! flattening on the way out keeps this to one payload per layout.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Buffer, Chunk, Data, StringColumn, VECTOR_SIZE, Validity, Vector};

/// Every value is there.
const ALL_VALID: u8 = 0;
/// Every value is null.
const ALL_NULL: u8 = 1;
/// One byte per row follows, non zero meaning the value is there.
const MASK: u8 = 2;

/// A column with no values at all, which is the layout of an untyped `NULL`.
const EMPTY: u8 = 0;
/// A length and then the bytes, per row.
const VARLEN: u8 = 1;

/// A file of rows, written a column at a time and then read back a chunk at a time.
///
/// Writing and reading are separate phases and the type does not enforce that, for the same reason
/// [`Spill`](crate::spill::Spill) does not: the operator writes for as long as its input lasts and
/// reads once, and a value that consumed itself on the way to a reader would have to be moved out
/// of the struct holding it.
#[derive(Debug)]
pub(crate) struct Runs {
    path: PathBuf,
    types: Vec<LogicalType>,
    writer: Option<BufWriter<File>>,
    reader: Option<File>,
    /// Whether [`Runs::begin`] has said how many rows are coming.
    begun: bool,
    rows: usize,
    blocks: usize,
    /// Where each column's stretch of the file starts, one per column once it has been written.
    starts: Vec<u64>,
    /// How many bytes each block of each column takes, by column and then by block.
    spans: Vec<Vec<u64>>,
    /// The column being written, and how many of its blocks have gone out.
    column: usize,
    block: usize,
    /// How many blocks the reader has handed back.
    read: usize,
    /// Where each column's cursor has got to, which is only meaningful once reading has started.
    cursors: Vec<u64>,
    bytes: u64,
}

impl Runs {
    /// A fresh file under the system temporary directory, named after `tag`.
    ///
    /// # Errors
    ///
    /// If the file cannot be created, which on a machine with no room left on the temporary
    /// filesystem is the ordinary way this fails.
    pub(crate) fn new(tag: &str, types: Vec<LogicalType>) -> Result<Self> {
        // The process id and the clock together, because two queries in one process spill at once
        // and two processes share the directory.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path = std::env::temp_dir().join(format!("rudb-{tag}-{}-{unique}", std::process::id()));
        // Opened for reading as well as writing, because the same descriptor is what the reader
        // seeks about in and reads back.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| {
                Error::io(format!("could not open a run file at {}: {e}", path.display()))
            })?;
        Ok(Self {
            path,
            types,
            writer: Some(BufWriter::with_capacity(1 << 20, file)),
            reader: None,
            begun: false,
            rows: 0,
            blocks: 0,
            starts: Vec::new(),
            spans: Vec::new(),
            column: 0,
            block: 0,
            read: 0,
            cursors: Vec::new(),
            bytes: 0,
        })
    }

    /// How many chunks will come back.
    pub(crate) fn chunks(&self) -> u64 {
        self.blocks as u64
    }

    /// How many rows are in the file.
    pub(crate) fn rows(&self) -> u64 {
        self.rows as u64
    }

    /// How many bytes have gone out, which is what the file costs on disk rather than in memory.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Says how many rows the run holds, which has to be settled before any of them are written.
    ///
    /// The row count is what cuts the columns into blocks, and every column is cut the same way, so
    /// it belongs to the file rather than to a column.
    ///
    /// # Errors
    ///
    /// If the file has already been begun.
    pub(crate) fn begin(&mut self, rows: usize) -> Result<()> {
        if self.begun {
            return Err(Error::internal("a run file was begun twice"));
        }
        self.begun = true;
        self.rows = rows;
        self.blocks = rows.div_ceil(VECTOR_SIZE);
        Ok(())
    }

    /// Writes a whole column, which is the ordinary way in.
    ///
    /// The column is cut into blocks here rather than by the caller, since how it is cut is the
    /// file's business and a caller that cut it differently would write a file nothing could read.
    ///
    /// # Errors
    ///
    /// As [`Runs::part`], and if the column is not as long as the file was begun with.
    pub(crate) fn column(&mut self, whole: &Vector) -> Result<()> {
        if whole.len() != self.rows {
            return Err(Error::internal(format!(
                "a run file of {} rows was given a column of {}",
                self.rows,
                whole.len()
            )));
        }
        for block in 0..self.blocks {
            let at = block * VECTOR_SIZE;
            self.part(&whole.slice(at, (self.rows - at).min(VECTOR_SIZE))?)?;
        }
        Ok(())
    }

    /// Writes the next block of the column being written, moving on to the next column after the
    /// last of them.
    ///
    /// This is for a caller that can make a block at a time and would rather not make the whole
    /// column first. The sort's ordering column is the one that wants it: the bytes are already
    /// held as an array and turning the array into a column would hold both at once.
    ///
    /// # Errors
    ///
    /// If the file was not begun, if it has already been read, if every column has already been
    /// written, if the block is not the length its position says it is, if it is not the type its
    /// column was opened with, or if the write fails.
    pub(crate) fn part(&mut self, values: &Vector) -> Result<()> {
        if !self.begun {
            return Err(Error::internal("a run file was written to before it was begun"));
        }
        let Some(ty) = self.types.get(self.column) else {
            return Err(Error::internal(format!(
                "a run file of {} columns was given another one",
                self.types.len()
            )));
        };
        let at = self.block * VECTOR_SIZE;
        let rows = (self.rows - at).min(VECTOR_SIZE);
        if values.len() != rows {
            return Err(Error::internal(format!(
                "a run file block of {rows} rows was given {}",
                values.len()
            )));
        }
        if values.logical_type() != ty {
            return Err(Error::internal(format!(
                "a run file column of {ty} was given {}",
                values.logical_type()
            )));
        }
        // flatten: a file has one layout a column and a dictionary or a constant has two, so a
        // block that kept its encoding would have to write the codes and the values and say which,
        // and a reader that got it back would hand the merge a column it has to decode a row at a
        // time. This is the one place where a flat copy is what the format is.
        let flat = values.flatten()?;
        let Some(writer) = self.writer.as_mut() else {
            return Err(Error::internal("a run file was written to after it was read"));
        };
        let mut out = Sink { writer, written: 0 };
        put_validity(&mut out, &flat, rows)?;
        put_payload(&mut out, &flat, rows)?;
        let written = out.written;
        if self.block == 0 {
            self.starts.push(self.bytes);
            self.spans.push(Vec::with_capacity(self.blocks));
        }
        if let Some(spans) = self.spans.get_mut(self.column) {
            spans.push(written);
        }
        self.bytes += written;
        self.block += 1;
        if self.block == self.blocks {
            self.column += 1;
            self.block = 0;
        }
        Ok(())
    }

    /// The next chunk, or `None` once they have all come back.
    ///
    /// The first call finishes the writing and sets each column's cursor to where its stretch of
    /// the file begins, so there is no separate step for it and no reader borrowing this. That
    /// matters because the thing that reads these holds a stack of them at once, one per run, and a
    /// reader that borrowed its file would make that self referential.
    ///
    /// # Errors
    ///
    /// If the buffered writes cannot be flushed, if fewer columns were written than the file was
    /// opened with, if the read fails, or if the file says a layout the column's type does not
    /// allow, which is a bug in this file rather than anything a query can cause.
    pub(crate) fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush().map_err(|e| Error::io(format!("could not finish a run file: {e}")))?;
            let file = writer
                .into_inner()
                .map_err(|e| Error::io(format!("could not finish a run file: {e}")))?;
            if self.blocks > 0 && self.starts.len() != self.types.len() {
                return Err(Error::internal(format!(
                    "a run file of {} columns was read with {} of them written",
                    self.types.len(),
                    self.starts.len()
                )));
            }
            self.cursors.clone_from(&self.starts);
            self.reader = Some(file);
        }
        if self.read >= self.blocks {
            return Ok(None);
        }
        let block = self.read;
        let at = block * VECTOR_SIZE;
        let rows = (self.rows - at).min(VECTOR_SIZE);
        let mut columns = Vec::with_capacity(self.types.len());
        let mut bytes = Vec::new();
        for (position, ty) in self.types.iter().enumerate() {
            let (Some(file), Some(span), Some(cursor)) = (
                self.reader.as_mut(),
                self.spans.get(position).and_then(|spans| spans.get(block)),
                self.cursors.get_mut(position),
            ) else {
                return Err(Error::internal("a run file was read before it was written"));
            };
            file.seek(SeekFrom::Start(*cursor))
                .map_err(|e| Error::io(format!("could not seek in a run file: {e}")))?;
            bytes.clear();
            bytes.resize(usize::try_from(*span).unwrap_or(usize::MAX), 0);
            file.read_exact(&mut bytes)
                .map_err(|e| Error::io(format!("could not read a run file: {e}")))?;
            *cursor += *span;
            let mut src: &[u8] = &bytes;
            let validity = take_validity(&mut src, rows)?;
            let column = take_payload(&mut src, ty, rows)?;
            columns.push(column.with_validity(validity));
        }
        self.read += 1;
        Ok(Some(Chunk::with_rows(columns, rows)?))
    }
}

impl Drop for Runs {
    fn drop(&mut self) {
        // Best effort on purpose. A temporary file that outlives the process is untidy and a query
        // that fails because it could not delete one is worse.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes the validity byte and, when there is a mask, the row bytes after it.
fn put_validity(out: &mut Sink<'_>, column: &Vector, rows: usize) -> Result<()> {
    let validity = column.validity();
    match validity {
        Validity::AllValid => out.put(&[ALL_VALID]),
        Validity::AllInvalid => out.put(&[ALL_NULL]),
        Validity::Mask(_) => {
            out.put(&[MASK])?;
            let mut bytes = Vec::with_capacity(rows);
            for row in 0..rows {
                bytes.push(u8::from(validity.is_valid(row)));
            }
            out.put(&bytes)
        }
    }
}

/// Reads the validity byte back, and the row bytes when there is a mask.
fn take_validity(src: &mut &[u8], rows: usize) -> Result<Validity> {
    let mut tag = [0u8; 1];
    fill(src, &mut tag)?;
    match tag[0] {
        ALL_VALID => Ok(Validity::AllValid),
        ALL_NULL => Ok(Validity::AllInvalid),
        MASK => {
            let mut bytes = vec![0u8; rows];
            fill(src, &mut bytes)?;
            Ok(Validity::from_iter(rows, |row| bytes[row] != 0))
        }
        other => Err(Error::internal(format!("a run file has {other} where a validity byte goes"))),
    }
}

/// The layout byte and the payload, one arm per [`Data`] variant each way.
///
/// One macro rather than two matches, because the tag a layout writes and the tag it reads back
/// have to be the same number and there is no way to get that wrong if they are written once. The
/// writer's match has no wildcard arm, so a layout added to [`Data`] without being added here is a
/// line in a build log rather than a column that silently writes nothing.
macro_rules! layouts {
    ($(($tag:literal, $variant:ident, $native:ty)),+ $(,)?) => {
        /// Writes the layout byte and then the values of one flat column.
        fn put_payload(out: &mut Sink<'_>, column: &Vector, rows: usize) -> Result<()> {
            let Some(data) = column.data() else {
                return Err(Error::internal(format!(
                    "a run file was given a {} column that did not flatten",
                    column.logical_type()
                )));
            };
            match data {
                $(Data::$variant(values) => {
                    out.put(&[$tag])?;
                    let mut bytes = Vec::with_capacity(values.len() * <$native as Plain>::WIDTH);
                    for value in values.as_slice() {
                        value.put(&mut bytes);
                    }
                    out.put(&bytes)
                })+
                Data::Varlen(strings) => {
                    out.put(&[VARLEN])?;
                    put_strings(out, strings, rows)
                }
                Data::Empty => out.put(&[EMPTY]),
                // [`Data`] is `#[non_exhaustive]`, so this arm is required and the compiler cannot
                // be the thing that notices a layout was added without being added here. Refusing
                // by name is the next best thing: an operator that cannot spill a column says so,
                // where a silent arm would write a file that reads back as the wrong values.
                _ => Err(Error::internal(format!(
                    "a run file has no layout for a {} column",
                    column.logical_type()
                ))),
            }
        }

        /// Reads the layout byte and then the values of one column back.
        fn take_payload(src: &mut &[u8], ty: &LogicalType, rows: usize) -> Result<Vector> {
            let mut tag = [0u8; 1];
            fill(src, &mut tag)?;
            let data = match tag[0] {
                $($tag => {
                    let mut bytes = vec![0u8; rows * <$native as Plain>::WIDTH];
                    fill(src, &mut bytes)?;
                    let mut values = Vec::with_capacity(rows);
                    for at in 0..rows {
                        values.push(<$native as Plain>::get(&bytes[at * <$native as Plain>::WIDTH..]));
                    }
                    Data::$variant(Buffer::from_vec(values))
                })+
                VARLEN => Data::Varlen(take_strings(src, rows)?),
                // Nothing was written for it, and the validity byte before the layout byte is what
                // said every row is null. A constant rather than an empty buffer, because the chunk
                // this goes into still has rows in it.
                EMPTY => return Ok(Vector::constant(ty.clone(), Value::Null, rows)),
                other => {
                    return Err(Error::internal(format!(
                        "a run file has {other} where the layout of a {ty} column goes"
                    )));
                }
            };
            Vector::flat(ty.clone(), data)
        }
    };
}

// The tags are written down rather than counted so that they cannot move when a layout is added in
// the middle, which would make an old file in flight read as the wrong numbers. Nothing keeps a run
// file across a build, so this is belt and braces, and the belt costs one literal per line.
layouts!(
    (2, Bool, bool),
    (3, Int8, i8),
    (4, Int16, i16),
    (5, Int32, i32),
    (6, Int64, i64),
    (7, Int128, i128),
    (8, UInt8, u8),
    (9, UInt16, u16),
    (10, UInt32, u32),
    (11, UInt64, u64),
    (12, UInt128, u128),
    (13, Float32, f32),
    (14, Float64, f64),
    (15, Interval, (i32, i32, i64)),
);

/// Writes a string column as a length and the bytes, per row.
fn put_strings(out: &mut Sink<'_>, strings: &StringColumn, rows: usize) -> Result<()> {
    let mut bytes = Vec::with_capacity(rows * 16);
    for row in 0..rows {
        let text = strings.bytes(row).unwrap_or_default();
        bytes.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(text);
    }
    out.put(&bytes)
}

/// Reads a string column back.
fn take_strings(src: &mut &[u8], rows: usize) -> Result<StringColumn> {
    let mut strings = StringColumn::with_capacity(rows);
    let mut text = Vec::new();
    for _ in 0..rows {
        let len = take_u32(src)? as usize;
        text.clear();
        text.resize(len, 0);
        fill(src, &mut text)?;
        strings.push_bytes(&text);
    }
    Ok(strings)
}

/// A little endian value of a fixed width layout.
///
/// One trait rather than fourteen pairs of arms, because every integer and float goes out the same
/// way and the two that do not, a bool and the months days microseconds triple, are the only ones
/// worth reading.
trait Plain: Sized {
    /// How many bytes one value takes.
    const WIDTH: usize;
    /// Appends this value.
    fn put(&self, out: &mut Vec<u8>);
    /// Reads one back from the front of `bytes`, which is at least `WIDTH` long.
    fn get(bytes: &[u8]) -> Self;
}

macro_rules! plain_numbers {
    ($($native:ty),+ $(,)?) => {
        $(impl Plain for $native {
            const WIDTH: usize = std::mem::size_of::<$native>();

            fn put(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }

            fn get(bytes: &[u8]) -> Self {
                let mut at = [0u8; Self::WIDTH];
                at.copy_from_slice(&bytes[..Self::WIDTH]);
                Self::from_le_bytes(at)
            }
        })+
    };
}

plain_numbers!(i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, f32, f64);

impl Plain for bool {
    const WIDTH: usize = 1;

    fn put(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }

    fn get(bytes: &[u8]) -> Self {
        bytes[0] != 0
    }
}

impl Plain for (i32, i32, i64) {
    const WIDTH: usize = 16;

    fn put(&self, out: &mut Vec<u8>) {
        self.0.put(out);
        self.1.put(out);
        self.2.put(out);
    }

    fn get(bytes: &[u8]) -> Self {
        (i32::get(bytes), i32::get(&bytes[4..]), i64::get(&bytes[8..]))
    }
}

/// Reads one little endian `u32`.
fn take_u32(src: &mut &[u8]) -> Result<u32> {
    let mut bytes = [0u8; 4];
    fill(src, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Reads exactly as many bytes as `into` is long, moving the cursor past them.
///
/// The block was read off the disk in one go, so what is left here is walking a slice. A read that
/// runs off the end is the file disagreeing with the lengths this struct recorded for it, which is
/// a bug rather than a short read to retry.
fn fill(src: &mut &[u8], into: &mut [u8]) -> Result<()> {
    if src.len() < into.len() {
        return Err(Error::internal("a run file block ended in the middle of a value"));
    }
    let (take, left) = src.split_at(into.len());
    into.copy_from_slice(take);
    *src = left;
    Ok(())
}

/// A writer that counts what has gone through it.
struct Sink<'a> {
    writer: &'a mut BufWriter<File>,
    written: u64,
}

impl Sink<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|e| Error::io(format!("could not write to a run file: {e}")))?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::{LogicalType, Value};
    use rudb_vector::VECTOR_SIZE;

    use super::{Buffer, Chunk, Data, Runs, StringColumn, Validity, Vector};

    fn ints(values: &[i32]) -> Vector {
        Vector::flat(LogicalType::Integer, Data::Int32(Buffer::from_vec(values.to_vec())))
            .expect("integers are an i32 layout")
    }

    fn text(values: &[&str]) -> Vector {
        let mut strings = StringColumn::with_capacity(values.len());
        for value in values {
            strings.push(value);
        }
        Vector::flat(LogicalType::Varchar, Data::Varlen(strings)).expect("strings are a varlen")
    }

    /// Writes a whole run of the columns given, which have to be the same length.
    fn wrote(tag: &str, columns: &[Vector]) -> Runs {
        let types = columns.iter().map(|column| column.logical_type().clone()).collect();
        let mut runs = Runs::new(tag, types).expect("a temporary file");
        runs.begin(columns.first().map_or(0, Vector::len)).expect("the row count");
        for column in columns {
            runs.column(column).expect("a column");
        }
        runs
    }

    /// Every row of a run, as values, in the order it reads back.
    fn rows_of(runs: &mut Runs) -> Vec<Vec<Value>> {
        let mut out = Vec::new();
        while let Some(chunk) = runs.next_chunk().expect("readable") {
            for row in 0..chunk.len() {
                out.push(chunk.row(row).collect());
            }
        }
        out
    }

    /// Every row of a chunk, as values, for comparing against what a run gave back.
    fn rows_in(chunk: &Chunk) -> Vec<Vec<Value>> {
        (0..chunk.len()).map(|row| chunk.row(row).collect()).collect()
    }

    /// The whole point: rows written a column at a time come back as rows, in order.
    #[test]
    fn rows_come_back_in_the_order_they_went_in() {
        let chunk =
            Chunk::new(vec![ints(&[1, 2, 3]), text(&["one", "two", "three"])]).expect("three rows");
        let mut runs = wrote("test-order", chunk.columns());
        assert_eq!(runs.chunks(), 1);
        assert_eq!(runs.rows(), 3);
        assert!(runs.bytes() > 0, "something was written");
        assert_eq!(rows_of(&mut runs), rows_in(&chunk));
    }

    /// More rows than a block holds, which is where a column laid end to end has to be cut and the
    /// cursors have to land in the right place. The last block is a short one on purpose.
    #[test]
    fn a_run_longer_than_a_block_comes_back_as_several() {
        let rows = VECTOR_SIZE * 2 + 7;
        let numbers: Vec<i32> = (0..rows as i32).collect();
        let words: Vec<String> = numbers.iter().map(|value| format!("row {value}")).collect();
        let borrowed: Vec<&str> = words.iter().map(String::as_str).collect();
        let mut runs = wrote("test-blocks", &[ints(&numbers), text(&borrowed)]);
        assert_eq!(runs.chunks(), 3);
        assert_eq!(runs.rows(), rows as u64);

        let back = rows_of(&mut runs);
        assert_eq!(back.len(), rows);
        assert_eq!(back[0], vec![Value::Integer(0), Value::Varchar("row 0".into())]);
        assert_eq!(
            back[VECTOR_SIZE],
            vec![Value::Integer(VECTOR_SIZE as i32), Value::Varchar(format!("row {VECTOR_SIZE}"))],
            "the first row of the second block"
        );
        assert_eq!(
            back[rows - 1],
            vec![Value::Integer(rows as i32 - 1), Value::Varchar(format!("row {}", rows - 1))],
            "and the last row of the short one"
        );
    }

    /// A string longer than a view holds is in the arena rather than inline, and the lengths are
    /// what the reader has to get right for the row after it to start in the right place.
    #[test]
    fn long_strings_and_empty_ones_survive() {
        let long = "x".repeat(400);
        let chunk = Chunk::new(vec![text(&[&long, "", "short"])]).expect("three rows");
        let mut runs = wrote("test-strings", chunk.columns());
        assert_eq!(rows_of(&mut runs), rows_in(&chunk));
    }

    /// The three validity cases each take a different path and each has to come back saying the
    /// same thing, because a null read back as a zero is a wrong answer and not a slow one.
    #[test]
    fn the_three_validity_cases_all_come_back() {
        let some = ints(&[7, 8, 9]).with_validity(Validity::from_iter(3, |row| row != 1));
        let none = ints(&[1, 2, 3]).with_validity(Validity::AllInvalid);
        let mut runs = wrote("test-validity", &[ints(&[4, 5, 6]), some, none]);

        let back = runs.next_chunk().expect("readable").expect("a chunk");
        assert_eq!(back.value_at(1, 0), Value::Integer(5), "all valid");
        assert_eq!(back.value_at(0, 1), Value::Integer(7), "either side of the null");
        assert_eq!(back.value_at(1, 1), Value::Null, "the masked null");
        assert_eq!(back.value_at(2, 1), Value::Integer(9));
        assert_eq!(back.value_at(0, 2), Value::Null, "all null");
    }

    /// A dictionary or a constant is compact in memory and has no encoding here, so it is flattened
    /// on the way out and the values are what has to survive rather than the form.
    #[test]
    fn a_column_that_is_not_flat_is_flattened_on_the_way_out() {
        let coded = Vector::dictionary(vec![1, 0, 1], text(&["no", "yes"])).expect("a dictionary");
        let same = Vector::constant(LogicalType::Integer, Value::Integer(42), 3);
        let chunk = Chunk::new(vec![coded, same]).expect("three rows");
        let mut runs = wrote("test-flat", chunk.columns());
        assert_eq!(rows_of(&mut runs), rows_in(&chunk));
    }

    /// The length is checked rather than trusted, because a column of the wrong length would
    /// otherwise be written as a file that reads back as rows nobody wrote.
    #[test]
    fn a_column_of_the_wrong_length_is_refused() {
        let mut runs = Runs::new("test-length", vec![LogicalType::Integer]).expect("a file");
        runs.begin(2).expect("the row count");
        let why = runs.column(&ints(&[1])).expect_err("one row into a file of two");
        assert!(why.to_string().contains("was given a column of 1"), "{why}");
    }

    /// The same for a column whose type is not the one its position was opened with.
    #[test]
    fn a_column_of_the_wrong_type_is_refused() {
        let mut runs = Runs::new("test-type", vec![LogicalType::Varchar]).expect("a file");
        runs.begin(1).expect("the row count");
        let why = runs.column(&ints(&[1])).expect_err("an integer into a varchar column");
        assert!(why.to_string().contains("was given INTEGER"), "{why}");
    }

    /// And for one column more than the file was opened with, which is the other way a caller and
    /// a file can disagree about the shape of a run.
    #[test]
    fn a_column_past_the_last_one_is_refused() {
        let mut runs = wrote("test-width", &[ints(&[1])]);
        let why = runs.column(&ints(&[2])).expect_err("a second column into a file of one");
        assert!(why.to_string().contains("was given another one"), "{why}");
    }

    /// A file read before every column reached it would hand back rows with the wrong values in
    /// them rather than fewer of them, so it is refused instead.
    #[test]
    fn a_run_missing_a_column_is_refused_rather_than_read() {
        let types = vec![LogicalType::Integer, LogicalType::Integer];
        let mut runs = Runs::new("test-missing", types).expect("a file");
        runs.begin(1).expect("the row count");
        runs.column(&ints(&[1])).expect("the first column");
        let why = runs.next_chunk().expect_err("one column of two");
        assert!(why.to_string().contains("with 1 of them written"), "{why}");
    }

    /// Nothing written is not an error, it is a file with no rows in it.
    #[test]
    fn a_file_nobody_wrote_to_reads_back_as_nothing() {
        let mut runs = Runs::new("test-nothing", vec![LogicalType::Integer]).expect("a file");
        assert!(runs.next_chunk().expect("readable").is_none());
    }

    /// A run of no rows is a run all the same, and a merge holding one has to get nothing back
    /// from it rather than an error.
    #[test]
    fn a_run_of_no_rows_reads_back_as_nothing() {
        let mut runs = wrote("test-norows", &[ints(&[])]);
        assert_eq!(runs.chunks(), 0);
        assert!(runs.next_chunk().expect("readable").is_none());
    }

    /// The row count is settled once, because every column is cut by it and a second answer would
    /// cut two columns of one run differently.
    #[test]
    fn a_run_cannot_be_begun_twice() {
        let mut runs = Runs::new("test-begin", vec![LogicalType::Integer]).expect("a file");
        runs.begin(1).expect("the row count");
        let why = runs.begin(2).expect_err("a second row count");
        assert!(why.to_string().contains("begun twice"), "{why}");
    }

    /// The file goes when the value holding it does, including when a query failed on the way.
    #[test]
    fn the_file_is_gone_when_the_runs_are_dropped() {
        let runs = Runs::new("test-drop", vec![LogicalType::Integer]).expect("a file");
        let path = runs.path.clone();
        assert!(path.exists(), "it is there while the runs are");
        drop(runs);
        assert!(!path.exists(), "and gone after");
    }
}
