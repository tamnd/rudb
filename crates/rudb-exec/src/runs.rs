//! Chunks written to a file so a bucket can be streamed instead of assembled.
//!
//! `spill.rs` next door writes a row at a time as a `&[Value]`, which is the right shape for a hash
//! aggregate: it is holding rows one at a time anyway and it has no chunk to hand over. A bucket in
//! a partitioned write is the other case. It is already a list of chunks, and taking one apart into
//! sixty million `Value`s to write it and building sixty million more to read it back is most of
//! what the spill would cost, with the encoder then rebuilding chunks out of them a third time.
//!
//! So this takes a chunk and gives a chunk back, in the order they went in. That ordering is the
//! point rather than a convenience. Section 15.6 of `tenx/15-the-partitioned-write.md` is about a
//! measurement that says a sink's peak is inside its own `finalize`, where it holds its input and
//! the answer it is assembling from it at once, and that nothing a downstream reader does can lower
//! a peak it is already past. The way out is an answer that is never assembled, which means the
//! operator hands back a file and something reads it a chunk at a time.
//!
//! # The encoding
//!
//! Per chunk, the row count, then each column in turn: a validity byte, a layout byte, the payload.
//!
//! The validity byte says all valid, all null, or a mask, and a mask is one byte per row rather
//! than one bit. That is eight times the bytes for a column with some nulls and one byte for a
//! column with none, which is the case worth spending the format on: lineitem has no nulls in any
//! column at all.
//!
//! The layout byte is what the column turned out to be rather than what its type says it should be,
//! and the two are checked against each other on the way back in. That makes the file self checking
//! the same way the per value tag in `spill.rs` does, at one byte per column per chunk rather than
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
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::{Buffer, Chunk, Data, StringColumn, Validity, Vector};

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

/// A file of chunks, written once and then read back in order.
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
    reader: Option<BufReader<File>>,
    chunks: u64,
    /// How many chunks the reader has left, which is only meaningful once reading has started.
    left: u64,
    rows: u64,
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
        // rewinds and reads back.
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
            chunks: 0,
            left: 0,
            rows: 0,
            bytes: 0,
        })
    }

    /// How many chunks have gone out.
    pub(crate) fn chunks(&self) -> u64 {
        self.chunks
    }

    /// How many rows have gone out.
    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }

    /// How many bytes have gone out, which is what the file costs on disk rather than in memory.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Appends one chunk.
    ///
    /// An empty chunk goes out the same as any other rather than being skipped, because a reader
    /// that counts chunks is entitled to get back as many as went in.
    ///
    /// # Errors
    ///
    /// If the chunk is not the width the file was opened at, if a column is not the type its
    /// position says it is, or if the write fails.
    pub(crate) fn write(&mut self, chunk: &Chunk) -> Result<()> {
        if chunk.width() != self.types.len() {
            return Err(Error::internal(format!(
                "a run file of {} columns was given a chunk of {}",
                self.types.len(),
                chunk.width()
            )));
        }
        // flatten: a file has one layout a column and a dictionary or a constant has two, so a
        // chunk that kept its encoding would have to write the codes and the values and say which,
        // and a reader that got it back would hand the merge a column it has to decode a row at a
        // time. This is the one place where a flat copy is what the format is.
        let flat = chunk.flatten()?;
        let rows = flat.len();
        let types = std::mem::take(&mut self.types);
        let written = self.putting(&flat, &types, rows);
        self.types = types;
        self.bytes += written?;
        self.rows += rows as u64;
        self.chunks += 1;
        Ok(())
    }

    /// The body of [`Runs::write`], split out so the types can be lent to it while the writer is
    /// borrowed mutably.
    fn putting(&mut self, flat: &Chunk, types: &[LogicalType], rows: usize) -> Result<u64> {
        let Some(writer) = self.writer.as_mut() else {
            return Err(Error::internal("a run file was written to after it was read"));
        };
        let mut out = Sink { writer, written: 0 };
        out.put(&u32::try_from(rows).unwrap_or(u32::MAX).to_le_bytes())?;
        for (column, ty) in flat.columns().iter().zip(types) {
            if column.logical_type() != ty {
                return Err(Error::internal(format!(
                    "a run file column of {ty} was given {}",
                    column.logical_type()
                )));
            }
            put_validity(&mut out, column, rows)?;
            put_payload(&mut out, column, rows)?;
        }
        Ok(out.written)
    }

    /// The next chunk, or `None` once they have all come back.
    ///
    /// The first call finishes the writing and rewinds, so there is no separate step for it and no
    /// reader borrowing this. That matters because the thing that reads these holds a stack of them
    /// at once, one per run, and a reader that borrowed its file would make that self referential.
    ///
    /// # Errors
    ///
    /// If the buffered writes cannot be flushed, if the read fails, or if the file says a layout
    /// the column's type does not allow, which is a bug in this file rather than anything a query
    /// can cause.
    pub(crate) fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush().map_err(|e| Error::io(format!("could not finish a run file: {e}")))?;
            let mut file = writer
                .into_inner()
                .map_err(|e| Error::io(format!("could not finish a run file: {e}")))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|e| Error::io(format!("could not rewind a run file: {e}")))?;
            self.reader = Some(BufReader::with_capacity(1 << 20, file));
            self.left = self.chunks;
        }
        if self.left == 0 {
            return Ok(None);
        }
        let Some(reader) = self.reader.as_mut() else {
            return Err(Error::internal("a run file was read before anything was written"));
        };
        self.left -= 1;
        let rows = take_u32(reader)? as usize;
        let mut columns = Vec::with_capacity(self.types.len());
        for ty in &self.types {
            let validity = take_validity(reader, rows)?;
            let column = take_payload(reader, ty, rows)?;
            columns.push(column.with_validity(validity));
        }
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
fn take_validity(reader: &mut BufReader<File>, rows: usize) -> Result<Validity> {
    let mut tag = [0u8; 1];
    fill(reader, &mut tag)?;
    match tag[0] {
        ALL_VALID => Ok(Validity::AllValid),
        ALL_NULL => Ok(Validity::AllInvalid),
        MASK => {
            let mut bytes = vec![0u8; rows];
            fill(reader, &mut bytes)?;
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
        fn take_payload(
            reader: &mut BufReader<File>,
            ty: &LogicalType,
            rows: usize,
        ) -> Result<Vector> {
            let mut tag = [0u8; 1];
            fill(reader, &mut tag)?;
            let data = match tag[0] {
                $($tag => {
                    let mut bytes = vec![0u8; rows * <$native as Plain>::WIDTH];
                    fill(reader, &mut bytes)?;
                    let mut values = Vec::with_capacity(rows);
                    for at in 0..rows {
                        values.push(<$native as Plain>::get(&bytes[at * <$native as Plain>::WIDTH..]));
                    }
                    Data::$variant(Buffer::from_vec(values))
                })+
                VARLEN => Data::Varlen(take_strings(reader, rows)?),
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
fn take_strings(reader: &mut BufReader<File>, rows: usize) -> Result<StringColumn> {
    let mut strings = StringColumn::with_capacity(rows);
    let mut text = Vec::new();
    for _ in 0..rows {
        let len = take_u32(reader)? as usize;
        text.clear();
        text.resize(len, 0);
        fill(reader, &mut text)?;
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
fn take_u32(reader: &mut BufReader<File>) -> Result<u32> {
    let mut bytes = [0u8; 4];
    fill(reader, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Reads exactly as many bytes as `into` is long.
fn fill(reader: &mut BufReader<File>, into: &mut [u8]) -> Result<()> {
    reader.read_exact(into).map_err(|e| Error::io(format!("could not read a run file: {e}")))
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

    fn rows_of(chunk: &Chunk) -> Vec<Vec<Value>> {
        (0..chunk.len()).map(|row| chunk.row(row).collect()).collect()
    }

    /// The whole point: what goes in comes back a chunk at a time and in the order it went.
    #[test]
    fn chunks_come_back_in_the_order_they_went_in() {
        let types = vec![LogicalType::Integer, LogicalType::Varchar];
        let mut runs = Runs::new("test-order", types).expect("a temporary file");
        let first = Chunk::new(vec![ints(&[1, 2]), text(&["one", "two"])]).expect("two rows");
        let second = Chunk::new(vec![ints(&[3]), text(&["three"])]).expect("one row");
        runs.write(&first).expect("the first chunk");
        runs.write(&second).expect("the second");
        assert_eq!(runs.chunks(), 2);
        assert_eq!(runs.rows(), 3);
        assert!(runs.bytes() > 0, "something was written");

        let back = runs.next_chunk().expect("readable").expect("the first chunk");
        assert_eq!(rows_of(&back), rows_of(&first));
        let back = runs.next_chunk().expect("readable").expect("the second chunk");
        assert_eq!(rows_of(&back), rows_of(&second));
        assert!(runs.next_chunk().expect("readable").is_none(), "and no third");
    }

    /// A string longer than a view holds is in the arena rather than inline, and the lengths are
    /// what the reader has to get right for the row after it to start in the right place.
    #[test]
    fn long_strings_and_empty_ones_survive() {
        let long = "x".repeat(400);
        let types = vec![LogicalType::Varchar];
        let mut runs = Runs::new("test-strings", types).expect("a temporary file");
        let chunk = Chunk::new(vec![text(&[&long, "", "short"])]).expect("three rows");
        runs.write(&chunk).expect("the chunk");

        let back = runs.next_chunk().expect("readable").expect("a chunk");
        assert_eq!(rows_of(&back), rows_of(&chunk));
    }

    /// The three validity cases each take a different path and each has to come back saying the
    /// same thing, because a null read back as a zero is a wrong answer and not a slow one.
    #[test]
    fn the_three_validity_cases_all_come_back() {
        let types = vec![LogicalType::Integer, LogicalType::Integer, LogicalType::Integer];
        let mut runs = Runs::new("test-validity", types).expect("a temporary file");
        let some = ints(&[7, 8, 9]).with_validity(Validity::from_iter(3, |row| row != 1));
        let none = ints(&[1, 2, 3]).with_validity(Validity::AllInvalid);
        let chunk = Chunk::new(vec![ints(&[4, 5, 6]), some, none]).expect("three rows");
        runs.write(&chunk).expect("the chunk");

        let back = runs.next_chunk().expect("readable").expect("a chunk");
        assert_eq!(back.value_at(1, 0), Value::Integer(5), "all valid");
        assert_eq!(back.value_at(0, 1), Value::Integer(7), "either side of the null");
        assert_eq!(back.value_at(1, 1), Value::Null, "the masked null");
        assert_eq!(back.value_at(2, 1), Value::Integer(9));
        assert_eq!(back.value_at(0, 2), Value::Null, "all null");
    }

    /// A chunk with no rows in it is still a chunk, and a reader counting them gets it back.
    #[test]
    fn an_empty_chunk_is_still_a_chunk() {
        let types = vec![LogicalType::Integer];
        let mut runs = Runs::new("test-empty", types).expect("a temporary file");
        runs.write(&Chunk::new(vec![ints(&[])]).expect("no rows")).expect("the chunk");
        runs.write(&Chunk::new(vec![ints(&[1])]).expect("one row")).expect("the chunk");

        let back = runs.next_chunk().expect("readable").expect("the empty chunk");
        assert_eq!(back.len(), 0);
        let back = runs.next_chunk().expect("readable").expect("the other one");
        assert_eq!(back.value_at(0, 0), Value::Integer(1));
    }

    /// A dictionary or a constant is compact in memory and has no encoding here, so it is flattened
    /// on the way out and the values are what has to survive rather than the form.
    #[test]
    fn a_column_that_is_not_flat_is_flattened_on_the_way_out() {
        let types = vec![LogicalType::Varchar, LogicalType::Integer];
        let mut runs = Runs::new("test-flat", types).expect("a temporary file");
        let coded = Vector::dictionary(vec![1, 0, 1], text(&["no", "yes"])).expect("a dictionary");
        let same = Vector::constant(LogicalType::Integer, Value::Integer(42), 3);
        let chunk = Chunk::new(vec![coded, same]).expect("three rows");
        runs.write(&chunk).expect("the chunk");

        let back = runs.next_chunk().expect("readable").expect("a chunk");
        assert_eq!(rows_of(&back), rows_of(&chunk));
    }

    /// The width is checked rather than trusted, because a chunk of the wrong shape would otherwise
    /// be written as a file that reads back as rows nobody wrote.
    #[test]
    fn a_chunk_of_the_wrong_width_is_refused() {
        let types = vec![LogicalType::Integer];
        let mut runs = Runs::new("test-width", types).expect("a temporary file");
        let chunk = Chunk::new(vec![ints(&[1]), ints(&[2])]).expect("one row");
        let why = runs.write(&chunk).expect_err("two columns into a file of one");
        assert!(why.to_string().contains("was given a chunk of 2"), "{why}");
    }

    /// The same for a column whose type is not the one its position was opened with.
    #[test]
    fn a_column_of_the_wrong_type_is_refused() {
        let types = vec![LogicalType::Varchar];
        let mut runs = Runs::new("test-type", types).expect("a temporary file");
        let chunk = Chunk::new(vec![ints(&[1])]).expect("one row");
        let why = runs.write(&chunk).expect_err("an integer into a varchar column");
        assert!(why.to_string().contains("was given INTEGER"), "{why}");
    }

    /// Nothing written is not an error, it is a file with no chunks in it.
    #[test]
    fn a_file_nobody_wrote_to_reads_back_as_nothing() {
        let mut runs = Runs::new("test-nothing", vec![LogicalType::Integer]).expect("a file");
        assert!(runs.next_chunk().expect("readable").is_none());
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
