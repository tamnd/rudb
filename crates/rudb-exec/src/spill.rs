//! Rows written to a file so the memory holding them can be given back.
//!
//! This is the thing #220 is missing. A `GROUP BY UserID` over the ClickBench file has about
//! seventeen million groups, the hash table holding them has nowhere to go, and the query dies at
//! the allocator rather than getting slower. `spec/04-architecture.md` is explicit that exceeding
//! the memory limit spills rather than aborts, and an operator cannot spill until there is
//! somewhere to spill to.
//!
//! What goes out is rows and not aggregate state. That is a choice and it is worth saying why,
//! because writing partial aggregates out is the other obvious design. An `Accumulator` has no
//! serialize, which `spec/engine/07-aggregate.md` records as debt in section 7.8, and adding one
//! means a format per aggregate and a merge per aggregate, both of which have to be right before
//! anything spills at all. Rows need neither. The operator that reads them back builds an ordinary
//! table over them and every aggregate works the way it already works, because nothing about the
//! aggregate changed. The cost is that a row is written whole rather than folded first, which is
//! the wrong trade for a group by with ten groups and the right one for a group by with seventeen
//! million, and seventeen million is the case that does not run today.
//!
//! # The encoding
//!
//! One tag byte per value, then the payload, little endian. The tag is zero for a null and
//! otherwise says which arm of [`Value`] this is, which makes the file self checking against the
//! types it was opened with: a column that says `BIGINT` and a value that arrives as an `INTEGER`
//! is a bug somewhere above here, and it is a bug that would otherwise be read back as a different
//! number rather than as an error.
//!
//! What the tag does not carry is the element type of a list or the field names of a struct. Those
//! come off the schema the reader is given, which is the same schema the writer was given, so they
//! are written once for the file rather than once for every row. A list of a hundred million
//! strings would otherwise spend more of the file saying `VARCHAR` than saying anything.
//!
//! The file is not durable and is not meant to be. It lives under the system temporary directory,
//! it is removed when the [`Spill`] is dropped, including on the error path, and nothing outside
//! this process ever reads it. A `temp_directory` setting is the obvious next thing and belongs
//! with the operator that spills rather than here, because the question it answers is which disk a
//! query should use and this file has no opinion about that.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use rudb_common::{Error, LogicalType, Result, Value};

/// A file of rows, written once and then read back.
///
/// Writing and reading are separate phases and the type does not enforce that, because the operator
/// that uses it has the phases the other way around from what a builder would express: it writes
/// for as long as the input lasts and reads once, and a `Spill` that consumed itself on the way to
/// a reader would have to be moved out of the struct holding it.
#[derive(Debug)]
pub(crate) struct Spill {
    path: PathBuf,
    types: Vec<LogicalType>,
    writer: Option<BufWriter<File>>,
    reader: Option<BufReader<File>>,
    rows: u64,
    bytes: u64,
}

impl Spill {
    /// A fresh file under the system temporary directory, named after `tag`.
    ///
    /// # Errors
    ///
    /// If the file cannot be created, which on a machine with no room left on the temporary
    /// filesystem is the ordinary way this fails.
    pub(crate) fn new(tag: &str, types: Vec<LogicalType>) -> Result<Self> {
        // The process id and the clock together, because two queries in one process spill at once
        // and two processes share the directory. Neither alone is enough and both together cost a
        // syscall once per file.
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let path = std::env::temp_dir().join(format!("rudb-{tag}-{}-{unique}", std::process::id()));
        // Opened for reading as well as writing, because the same descriptor is what the reader
        // rewinds and reads back. `File::create` asks for write only and the read at the other end
        // of it comes back as a bad file descriptor, which is a confusing way to find that out.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|e| {
                Error::io(format!("could not open a spill file at {}: {e}", path.display()))
            })?;
        Ok(Self {
            path,
            types,
            writer: Some(BufWriter::with_capacity(1 << 20, file)),
            reader: None,
            rows: 0,
            bytes: 0,
        })
    }

    /// How many rows have gone out.
    pub(crate) fn rows(&self) -> u64 {
        self.rows
    }

    /// How many bytes have gone out, which is what the file costs on disk rather than in memory.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Writes one row.
    ///
    /// # Errors
    ///
    /// If the row is not the width the file was opened at, if a value is not the type its column
    /// says it is, or if the write fails.
    pub(crate) fn write(&mut self, row: &[Value]) -> Result<()> {
        if row.len() != self.types.len() {
            return Err(Error::internal(format!(
                "a spill file of {} columns was given a row of {}",
                self.types.len(),
                row.len()
            )));
        }
        let Some(writer) = self.writer.as_mut() else {
            return Err(Error::internal("a spill file was written to after it was read"));
        };
        let mut out = Sink { writer, written: 0 };
        for (value, ty) in row.iter().zip(&self.types) {
            put(&mut out, value, ty)?;
        }
        self.bytes += out.written;
        self.rows += 1;
        Ok(())
    }

    /// Finishes writing and hands back a reader over everything written.
    ///
    /// # Errors
    ///
    /// If the buffered writes cannot be flushed or the file cannot be read back.
    pub(crate) fn read(&mut self) -> Result<Reader<'_>> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush().map_err(|e| Error::io(format!("could not finish a spill file: {e}")))?;
            let mut file = writer
                .into_inner()
                .map_err(|e| Error::io(format!("could not finish a spill file: {e}")))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|e| Error::io(format!("could not rewind a spill file: {e}")))?;
            self.reader = Some(BufReader::with_capacity(1 << 20, file));
        }
        let left = self.rows;
        Ok(Reader { reader: self.reader.as_mut(), types: &self.types, left })
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        // Best effort on purpose. A temporary file that outlives the process is untidy and a query
        // that fails because it could not delete one is worse, and the operating system cleans the
        // directory anyway.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A pass over everything a [`Spill`] holds.
pub(crate) struct Reader<'a> {
    reader: Option<&'a mut BufReader<File>>,
    types: &'a [LogicalType],
    left: u64,
}

impl Reader<'_> {
    /// Reads the next row into `row`, reusing the buffers already there, and says whether there was
    /// one.
    ///
    /// Filling rather than returning is for the strings, and it is the same reason the aggregate
    /// fills a key rather than collecting one. A `Value::Varchar` owns its bytes, so a reader that
    /// hands back a fresh row takes a buffer from the allocator for every string of every row and
    /// gives it back on the next one, and a spilled group by over `URL` would do that as many times
    /// as it has rows.
    ///
    /// # Errors
    ///
    /// If the file ends in the middle of a row, if a tag is not one this wrote, or if the read
    /// fails.
    pub(crate) fn next_into(&mut self, row: &mut Vec<Value>) -> Result<bool> {
        if self.left == 0 {
            return Ok(false);
        }
        let Some(reader) = self.reader.as_mut() else {
            return Err(Error::internal("a spill file was read before it was written"));
        };
        row.truncate(self.types.len());
        for (at, ty) in self.types.iter().enumerate() {
            let value = match row.get_mut(at) {
                Some(slot) => get(reader, ty, Some(std::mem::replace(slot, Value::Null)))?,
                None => get(reader, ty, None)?,
            };
            match row.get_mut(at) {
                Some(slot) => *slot = value,
                None => row.push(value),
            }
        }
        self.left -= 1;
        Ok(true)
    }
}

/// A writer that counts what went through it, so the caller learns the cost without a second pass.
struct Sink<'a> {
    writer: &'a mut BufWriter<File>,
    written: u64,
}

impl Sink<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .map_err(|e| Error::io(format!("could not write to a spill file: {e}")))?;
        self.written += bytes.len() as u64;
        Ok(())
    }
}

/// The tag byte for each arm, zero being a null.
///
/// Written out rather than derived so that a rearrangement of [`Value`] cannot silently change what
/// a file means. Nothing outside this process reads these, so the numbers are free to move, but
/// they have to move on purpose.
mod tag {
    pub(super) const NULL: u8 = 0;
    pub(super) const BOOLEAN: u8 = 1;
    pub(super) const TINYINT: u8 = 2;
    pub(super) const SMALLINT: u8 = 3;
    pub(super) const INTEGER: u8 = 4;
    pub(super) const BIGINT: u8 = 5;
    pub(super) const HUGEINT: u8 = 6;
    pub(super) const UTINYINT: u8 = 7;
    pub(super) const USMALLINT: u8 = 8;
    pub(super) const UINTEGER: u8 = 9;
    pub(super) const UBIGINT: u8 = 10;
    pub(super) const UHUGEINT: u8 = 11;
    pub(super) const FLOAT: u8 = 12;
    pub(super) const DOUBLE: u8 = 13;
    pub(super) const DECIMAL: u8 = 14;
    pub(super) const VARCHAR: u8 = 15;
    pub(super) const BLOB: u8 = 16;
    pub(super) const DATE: u8 = 17;
    pub(super) const TIME: u8 = 18;
    pub(super) const TIMESTAMP: u8 = 19;
    pub(super) const INTERVAL: u8 = 20;
    pub(super) const LIST: u8 = 21;
    pub(super) const STRUCT: u8 = 22;
}

/// Writes one value.
fn put(out: &mut Sink<'_>, value: &Value, ty: &LogicalType) -> Result<()> {
    match value {
        Value::Null => out.put(&[tag::NULL]),
        Value::Boolean(held) => out.put(&[tag::BOOLEAN, u8::from(*held)]),
        Value::TinyInt(held) => out.put(&[tag::TINYINT, *held as u8]),
        Value::SmallInt(held) => fixed(out, tag::SMALLINT, &held.to_le_bytes()),
        Value::Integer(held) => fixed(out, tag::INTEGER, &held.to_le_bytes()),
        Value::BigInt(held) => fixed(out, tag::BIGINT, &held.to_le_bytes()),
        Value::HugeInt(held) => fixed(out, tag::HUGEINT, &held.to_le_bytes()),
        Value::UTinyInt(held) => out.put(&[tag::UTINYINT, *held]),
        Value::USmallInt(held) => fixed(out, tag::USMALLINT, &held.to_le_bytes()),
        Value::UInteger(held) => fixed(out, tag::UINTEGER, &held.to_le_bytes()),
        Value::UBigInt(held) => fixed(out, tag::UBIGINT, &held.to_le_bytes()),
        Value::UHugeInt(held) => fixed(out, tag::UHUGEINT, &held.to_le_bytes()),
        Value::Float(held) => fixed(out, tag::FLOAT, &held.to_le_bytes()),
        Value::Double(held) => fixed(out, tag::DOUBLE, &held.to_le_bytes()),
        Value::Decimal { unscaled, width, scale } => {
            // The width and the scale go out per value rather than being read off the type. They
            // are two bytes, and a decimal whose scale disagreed with its column would otherwise
            // read back as a number a hundred times too large with nothing saying so.
            fixed(out, tag::DECIMAL, &unscaled.to_le_bytes())?;
            out.put(&[*width, *scale])
        }
        Value::Varchar(text) => bytes(out, tag::VARCHAR, text.as_bytes()),
        Value::Blob(held) => bytes(out, tag::BLOB, held),
        Value::Date(held) => fixed(out, tag::DATE, &held.to_le_bytes()),
        Value::Time(held) => fixed(out, tag::TIME, &held.to_le_bytes()),
        Value::Timestamp(held) => fixed(out, tag::TIMESTAMP, &held.to_le_bytes()),
        Value::Interval { months, days, micros } => {
            out.put(&[tag::INTERVAL])?;
            out.put(&months.to_le_bytes())?;
            out.put(&days.to_le_bytes())?;
            out.put(&micros.to_le_bytes())
        }
        Value::List { values, .. } => {
            let LogicalType::List(element) = ty else {
                return Err(mismatch(value, ty));
            };
            out.put(&[tag::LIST])?;
            out.put(&count_of(values.len())?.to_le_bytes())?;
            for held in values {
                put(out, held, element)?;
            }
            Ok(())
        }
        Value::Struct(held) => {
            let LogicalType::Struct(fields) = ty else {
                return Err(mismatch(value, ty));
            };
            if held.len() != fields.len() {
                return Err(mismatch(value, ty));
            }
            out.put(&[tag::STRUCT])?;
            for ((_, inner), field) in held.iter().zip(fields) {
                put(out, inner, &field.ty)?;
            }
            Ok(())
        }
        other => Err(Error::not_implemented(format!("spilling a {other:?}"))),
    }
}

/// A tag and a fixed width payload.
fn fixed(out: &mut Sink<'_>, tag: u8, payload: &[u8]) -> Result<()> {
    out.put(&[tag])?;
    out.put(payload)
}

/// A tag, a length and that many bytes.
fn bytes(out: &mut Sink<'_>, tag: u8, payload: &[u8]) -> Result<()> {
    out.put(&[tag])?;
    out.put(&count_of(payload.len())?.to_le_bytes())?;
    out.put(payload)
}

/// A length as the four bytes it goes out as.
///
/// Four and not eight because a single string longer than four gigabytes is not a thing this engine
/// can hold anyway, and the length is paid on every string of every row.
fn count_of(length: usize) -> Result<u32> {
    u32::try_from(length)
        .map_err(|_| Error::internal(format!("a spilled value of {length} bytes is too long")))
}

/// Reads one value, putting it back into `reuse` when that buffer can be kept.
fn get(reader: &mut BufReader<File>, ty: &LogicalType, reuse: Option<Value>) -> Result<Value> {
    let tag = one(reader)?;
    Ok(match tag {
        tag::NULL => Value::Null,
        tag::BOOLEAN => Value::Boolean(one(reader)? != 0),
        tag::TINYINT => Value::TinyInt(one(reader)? as i8),
        tag::SMALLINT => Value::SmallInt(i16::from_le_bytes(take(reader)?)),
        tag::INTEGER => Value::Integer(i32::from_le_bytes(take(reader)?)),
        tag::BIGINT => Value::BigInt(i64::from_le_bytes(take(reader)?)),
        tag::HUGEINT => Value::HugeInt(i128::from_le_bytes(take(reader)?)),
        tag::UTINYINT => Value::UTinyInt(one(reader)?),
        tag::USMALLINT => Value::USmallInt(u16::from_le_bytes(take(reader)?)),
        tag::UINTEGER => Value::UInteger(u32::from_le_bytes(take(reader)?)),
        tag::UBIGINT => Value::UBigInt(u64::from_le_bytes(take(reader)?)),
        tag::UHUGEINT => Value::UHugeInt(u128::from_le_bytes(take(reader)?)),
        tag::FLOAT => Value::Float(f32::from_le_bytes(take(reader)?)),
        tag::DOUBLE => Value::Double(f64::from_le_bytes(take(reader)?)),
        tag::DECIMAL => {
            let unscaled = i128::from_le_bytes(take(reader)?);
            Value::Decimal { unscaled, width: one(reader)?, scale: one(reader)? }
        }
        tag::VARCHAR => {
            // The one place the reuse matters, and the reason this function takes it at all. The
            // buffer already in the row is emptied and filled rather than dropped and replaced.
            let mut text = match reuse {
                Some(Value::Varchar(held)) => held,
                _ => String::new(),
            };
            text.clear();
            let length = length(reader)?;
            let mut raw = std::mem::take(&mut text).into_bytes();
            raw.resize(length, 0);
            fill(reader, &mut raw)?;
            Value::Varchar(String::from_utf8(raw).map_err(|_| {
                Error::internal("a spilled string came back as bytes that are not UTF-8")
            })?)
        }
        tag::BLOB => {
            let mut held = match reuse {
                Some(Value::Blob(buffer)) => buffer,
                _ => Vec::new(),
            };
            held.clear();
            held.resize(length(reader)?, 0);
            fill(reader, &mut held)?;
            Value::Blob(held)
        }
        tag::DATE => Value::Date(i32::from_le_bytes(take(reader)?)),
        tag::TIME => Value::Time(i64::from_le_bytes(take(reader)?)),
        tag::TIMESTAMP => Value::Timestamp(i64::from_le_bytes(take(reader)?)),
        tag::INTERVAL => Value::Interval {
            months: i32::from_le_bytes(take(reader)?),
            days: i32::from_le_bytes(take(reader)?),
            micros: i64::from_le_bytes(take(reader)?),
        },
        tag::LIST => {
            let LogicalType::List(element) = ty else {
                return Err(unexpected(tag, ty));
            };
            let count = length(reader)?;
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(get(reader, element, None)?);
            }
            Value::List { element: (**element).clone(), values }
        }
        tag::STRUCT => {
            let LogicalType::Struct(fields) = ty else {
                return Err(unexpected(tag, ty));
            };
            let mut held = Vec::with_capacity(fields.len());
            for field in fields {
                held.push((field.name.clone(), get(reader, &field.ty, None)?));
            }
            Value::Struct(held)
        }
        other => {
            return Err(Error::internal(format!("a spill file holds an unknown tag {other}")));
        }
    })
}

/// One byte, or an error saying the file ended where a row did not.
fn one(reader: &mut BufReader<File>) -> Result<u8> {
    let mut byte = [0u8; 1];
    fill(reader, &mut byte)?;
    Ok(byte[0])
}

/// A fixed width payload.
fn take<const N: usize>(reader: &mut BufReader<File>) -> Result<[u8; N]> {
    let mut buffer = [0u8; N];
    fill(reader, &mut buffer)?;
    Ok(buffer)
}

/// A four byte length as the size it stands for.
fn length(reader: &mut BufReader<File>) -> Result<usize> {
    Ok(u32::from_le_bytes(take(reader)?) as usize)
}

/// Exactly enough bytes to fill `into`.
fn fill(reader: &mut BufReader<File>, into: &mut [u8]) -> Result<()> {
    reader.read_exact(into).map_err(|e| Error::io(format!("could not read a spill file back: {e}")))
}

/// The error for a value that is not the shape its column says it is.
fn mismatch(value: &Value, ty: &LogicalType) -> Error {
    Error::internal(format!("a {value:?} was spilled into a column of {ty}"))
}

/// The error for a tag that does not go with the type the reader was given.
fn unexpected(tag: u8, ty: &LogicalType) -> Error {
    Error::internal(format!("a spill file holds tag {tag} where the column says {ty}"))
}

#[cfg(test)]
mod tests {
    use rudb_common::Field;

    use super::*;

    /// One value of every arm, and the types they go with.
    fn everything() -> Vec<(LogicalType, Vec<Value>)> {
        vec![
            (LogicalType::Boolean, vec![Value::Boolean(true), Value::Boolean(false), Value::Null]),
            (LogicalType::TinyInt, vec![Value::TinyInt(i8::MIN), Value::TinyInt(i8::MAX)]),
            (LogicalType::SmallInt, vec![Value::SmallInt(i16::MIN), Value::SmallInt(0)]),
            (LogicalType::Integer, vec![Value::Integer(i32::MIN), Value::Integer(7)]),
            (LogicalType::BigInt, vec![Value::BigInt(i64::MIN), Value::BigInt(i64::MAX)]),
            (LogicalType::HugeInt, vec![Value::HugeInt(i128::MIN), Value::HugeInt(0)]),
            (LogicalType::UTinyInt, vec![Value::UTinyInt(u8::MAX)]),
            (LogicalType::USmallInt, vec![Value::USmallInt(u16::MAX)]),
            (LogicalType::UInteger, vec![Value::UInteger(u32::MAX)]),
            (LogicalType::UBigInt, vec![Value::UBigInt(u64::MAX)]),
            (LogicalType::UHugeInt, vec![Value::UHugeInt(u128::MAX)]),
            // A nan and both infinities, because the bits are what goes out and a comparison is not
            // what comes back. A nan is never equal to itself, so the assertion on these is on the
            // bits and the round trip has to keep them exactly.
            (
                LogicalType::Float,
                vec![Value::Float(f32::NAN), Value::Float(f32::NEG_INFINITY), Value::Float(-0.0)],
            ),
            (
                LogicalType::Double,
                vec![Value::Double(f64::NAN), Value::Double(f64::INFINITY), Value::Double(-0.0)],
            ),
            (
                LogicalType::Decimal { width: 38, scale: 10 },
                vec![Value::Decimal { unscaled: i128::MIN, width: 38, scale: 10 }],
            ),
            (
                LogicalType::Varchar,
                vec![
                    Value::Varchar(String::new()),
                    Value::Varchar("π is two bytes".into()),
                    Value::Varchar("a".repeat(70_000)),
                ],
            ),
            (LogicalType::Blob, vec![Value::Blob(Vec::new()), Value::Blob(vec![0, 255, 128])]),
            (LogicalType::Date, vec![Value::Date(i32::MIN), Value::Date(19_000)]),
            (LogicalType::Time, vec![Value::Time(86_399_999_999)]),
            (LogicalType::Timestamp, vec![Value::Timestamp(i64::MIN)]),
            (
                LogicalType::Interval,
                vec![Value::Interval { months: -1, days: 2, micros: i64::MIN }],
            ),
            (
                LogicalType::List(Box::new(LogicalType::Integer)),
                vec![
                    Value::List { element: LogicalType::Integer, values: Vec::new() },
                    Value::List {
                        element: LogicalType::Integer,
                        values: vec![Value::Integer(1), Value::Null, Value::Integer(-1)],
                    },
                ],
            ),
            (
                LogicalType::Struct(vec![
                    Field::new("a", LogicalType::Integer),
                    Field::new("b", LogicalType::Varchar),
                ]),
                vec![Value::Struct(vec![
                    ("a".to_owned(), Value::Integer(3)),
                    ("b".to_owned(), Value::Varchar("x".into())),
                ])],
            ),
        ]
    }

    /// Every value this can hold comes back as the value that went in.
    ///
    /// One file per type rather than one wide file, so that a failure names the type rather than a
    /// column number, and every list has a null in it because a null inside a value is a different
    /// path from a null instead of one.
    #[test]
    fn every_value_comes_back_as_itself() {
        for (ty, values) in everything() {
            let mut spill = Spill::new("round-trip", vec![ty.clone()]).unwrap();
            for value in &values {
                spill.write(std::slice::from_ref(value)).unwrap();
            }
            spill.write(&[Value::Null]).unwrap();

            let mut reader = spill.read().unwrap();
            let mut row = Vec::new();
            for value in &values {
                assert!(reader.next_into(&mut row).unwrap(), "{ty} ran out early");
                assert_eq!(same(&row[0]), same(value), "{ty}");
            }
            assert!(reader.next_into(&mut row).unwrap(), "{ty} lost its null");
            assert_eq!(row[0], Value::Null, "{ty}");
            assert!(!reader.next_into(&mut row).unwrap(), "{ty} had more than it was given");
        }
    }

    /// A value as something that compares equal to itself even when it is a nan.
    fn same(value: &Value) -> String {
        format!("{value:?}")
    }

    /// The buffers in the row are reused rather than replaced, which is the point of `next_into`.
    ///
    /// Asserted on the pointer rather than on the contents, because the contents being right is
    /// what the test above says and this one is about not going to the allocator. A string that
    /// grows is allowed to move, so the strings here are the same length.
    #[test]
    fn a_string_is_read_into_the_buffer_that_is_already_there() {
        let mut spill = Spill::new("reuse", vec![LogicalType::Varchar]).unwrap();
        for text in ["aaaa", "bbbb", "cccc"] {
            spill.write(&[Value::Varchar(text.to_owned())]).unwrap();
        }
        let mut reader = spill.read().unwrap();
        let mut row = Vec::new();
        assert!(reader.next_into(&mut row).unwrap());
        let Value::Varchar(first) = &row[0] else { panic!("not a string") };
        let at = first.as_ptr();
        assert!(reader.next_into(&mut row).unwrap());
        let Value::Varchar(second) = &row[0] else { panic!("not a string") };
        assert_eq!(second.as_ptr(), at, "the second row took a new buffer");
        assert_eq!(second, "bbbb");
    }

    /// A wide row of mixed types keeps its columns in order.
    ///
    /// The round trip test above puts one column in a file, so it cannot catch a reader that reads
    /// the right values in the wrong order, and that is the failure a spilled group by would show
    /// as every group being attributed to the wrong key.
    #[test]
    fn a_row_of_several_types_keeps_its_order() {
        let types = vec![
            LogicalType::Varchar,
            LogicalType::Integer,
            LogicalType::Varchar,
            LogicalType::Double,
        ];
        let mut spill = Spill::new("wide", types).unwrap();
        for at in 0..100i32 {
            spill
                .write(&[
                    Value::Varchar(format!("left {at}")),
                    Value::Integer(at),
                    Value::Varchar(format!("right {at}")),
                    Value::Double(f64::from(at) / 2.0),
                ])
                .unwrap();
        }
        let mut reader = spill.read().unwrap();
        let mut row = Vec::new();
        for at in 0..100i32 {
            assert!(reader.next_into(&mut row).unwrap());
            assert_eq!(row[0], Value::Varchar(format!("left {at}")));
            assert_eq!(row[1], Value::Integer(at));
            assert_eq!(row[2], Value::Varchar(format!("right {at}")));
            assert_eq!(row[3], Value::Double(f64::from(at) / 2.0));
        }
        assert!(!reader.next_into(&mut row).unwrap());
    }

    /// An empty file reads as no rows rather than as an error.
    #[test]
    fn a_file_nothing_was_written_to_has_no_rows() {
        let mut spill = Spill::new("empty", vec![LogicalType::Integer]).unwrap();
        assert_eq!(spill.rows(), 0);
        assert_eq!(spill.bytes(), 0);
        let mut reader = spill.read().unwrap();
        assert!(!reader.next_into(&mut Vec::new()).unwrap());
    }

    /// A row of the wrong width is an error and not a file that reads back shifted by a column.
    #[test]
    fn a_row_of_the_wrong_width_is_refused() {
        let mut spill =
            Spill::new("width", vec![LogicalType::Integer, LogicalType::Integer]).unwrap();
        let held = spill.write(&[Value::Integer(1)]).unwrap_err();
        assert!(held.to_string().contains("was given a row of 1"), "{held}");
    }

    /// The file is gone when the spill is.
    #[test]
    fn the_file_goes_when_the_spill_does() {
        let mut spill = Spill::new("cleanup", vec![LogicalType::Integer]).unwrap();
        spill.write(&[Value::Integer(1)]).unwrap();
        let path = spill.path.clone();
        assert!(path.is_file());
        drop(spill);
        assert!(!path.exists(), "{} is still there", path.display());
    }
}
