//! `COPY ... TO` a CSV, a JSON or a Parquet file.
//!
//! In CSV, every value is written as its cast to VARCHAR, under the session's time zone, which is
//! what the pin writes: a list is `[1, 2]`, a double is `1e+20`, a blob is `\x00\xFF`. A value
//! is quoted when it holds the delimiter, the quote or a line break, or when it reads the same as a
//! null would, so an empty string beside an empty null is `""` and a null is nothing at all. An
//! escape character alone does not quote a value. Spaces at either end are left bare, the way the
//! pin leaves them.
//!
//! In JSON, a row is one object keyed by the column names, with no spaces, which is what the pin's
//! `to_json` of the row writes. A number or a bool is bare, a decimal is written as the double it
//! casts to, a list is an array, a struct and a map are objects, and anything else is the string its
//! cast to VARCHAR reads. The rows go one to a line, or with `ARRAY` into one array with a row a
//! line after a tab, where no rows at all is an array holding one empty line.
//!
//! A Parquet file is written by `rudb-parquet`, a row group at a time, with the columns cast first
//! to the types that crate stores: an enum as its text, a coarse timestamp in microseconds.
//!
//! With `PARTITION_BY`, the rows are split by the values of the partition columns into a directory
//! a value, laid out the way Hive lays them out, with a file of the chosen format in each.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use rudb_bind::{CopyTo, Existing, NamePiece};
use rudb_common::{Error, Field, LogicalType, Memory, Result, SessionTimeZone, Value};
use rudb_compress::Codec;
use rudb_kernels::cast::{cast_in_time_zone, cast_value};
use rudb_kernels::compare::order_with_nulls;
use rudb_kernels::strftime::Format;
use rudb_vector::{Chunk, Vector};

use crate::QueryResult;

/// Writes the rows of `result` the way `copy` asks, to the one file or split over a directory,
/// and answers how many there were.
pub(crate) fn write(copy: &CopyTo, result: &QueryResult, zone: SessionTimeZone) -> Result<usize> {
    if copy.partitioned.columns.is_empty() {
        write_file(copy, &copy.path, result, zone)
    } else {
        write_partitioned(copy, result, zone)
    }
}

/// Writes the rows of `result` to the one file at `path`, in the format `copy` names.
fn write_file(
    copy: &CopyTo,
    path: &str,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    if copy.parquet {
        write_parquet(copy, path, result)
    } else if copy.json {
        write_json(copy, path, result, zone)
    } else {
        write_csv(copy, path, result, zone)
    }
}

/// What a directory is called for a NULL partition value, which is the name Hive gave it.
const NULL_PARTITION: &str = "__HIVE_DEFAULT_PARTITION__";

/// Writes the rows of `result` split by the values of the partition columns, a directory a
/// column named `column=value`, both percent encoded, with a file in each of the deepest ones.
///
/// The rows of a partition keep their order. A partition's file is numbered 0 in its own
/// directory, or, with the files side by side, by the order of the partition values, a column at
/// a time with a NULL last, which is how the pin numbers them.
fn write_partitioned(copy: &CopyTo, result: &QueryResult, zone: SessionTimeZone) -> Result<usize> {
    let split = &copy.partitioned;
    let root = Path::new(&copy.path);
    match split.existing {
        Existing::Refuse if holds_files(root) => {
            return Err(Error::io(format!(
                "Directory \"{}\" is not empty! Enable OVERWRITE option to overwrite files",
                copy.path
            )));
        }
        Existing::Overwrite => remove_files(root)?,
        _ => {}
    }
    let made = |path: &Path| {
        std::fs::create_dir_all(path).map_err(|error| {
            Error::io(format!("Failed to create directory \"{}\": {error}", path.display()))
        })
    };
    made(root)?;
    let names = result.names();
    let chunks = result.chunks();
    let mut found: HashMap<String, usize> = HashMap::new();
    let mut partitions: Vec<(String, Vec<Value>, Vec<Vec<u32>>)> = Vec::new();
    for (at, chunk) in chunks.iter().enumerate() {
        let mut keys = Vec::with_capacity(split.columns.len());
        for &column in &split.columns {
            let vector = chunk.column(column)?;
            keys.push(cast_in_time_zone(vector, &LogicalType::Varchar, false, Some(zone))?);
        }
        for row in 0..chunk.len() {
            let mut directory = String::new();
            for (key, &column) in keys.iter().zip(&split.columns) {
                if !directory.is_empty() {
                    directory.push('/');
                }
                encode(&mut directory, &names[column]);
                directory.push('=');
                match key.try_value_at(row)? {
                    Value::Null => directory.push_str(NULL_PARTITION),
                    Value::Varchar(text) => encode_value(&mut directory, &text),
                    other => encode_value(&mut directory, &other.to_string()),
                }
            }
            let partition = match found.get(&directory) {
                Some(&partition) => partition,
                None => {
                    let values = split
                        .columns
                        .iter()
                        .map(|&column| chunk.column(column)?.try_value_at(row))
                        .collect::<Result<Vec<_>>>()?;
                    found.insert(directory.clone(), partitions.len());
                    partitions.push((directory, values, vec![Vec::new(); chunks.len()]));
                    partitions.len() - 1
                }
            };
            partitions[partition].2[at].push(u32::try_from(row).unwrap_or(u32::MAX));
        }
    }
    let kept = (0..names.len())
        .filter(|column| split.write_columns || !split.columns.contains(column))
        .collect::<Vec<_>>();
    let kept_names = kept.iter().map(|&column| names[column].clone()).collect::<Vec<_>>();
    let kept_types = kept.iter().map(|&column| result.types()[column].clone()).collect::<Vec<_>>();
    if split.flat {
        partitions.sort_by(|(_, left, _), (_, right, _)| {
            left.iter()
                .zip(right)
                .map(|(left, right)| {
                    order_with_nulls(left, right, false).unwrap_or(Ordering::Equal)
                })
                .find(|&order| order != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        });
    }
    let mut rows = 0;
    for (number, (directory, _, picked)) in partitions.iter().enumerate() {
        let mut parts = Vec::new();
        for (chunk, rows) in chunks.iter().zip(picked) {
            if rows.is_empty() {
                continue;
            }
            let columns = kept
                .iter()
                .map(|&column| chunk.column(column)?.gather(rows))
                .collect::<Result<Vec<_>>>()?;
            parts.push(Chunk::new(columns)?);
        }
        let part = QueryResult::new(
            kept_names.clone(),
            kept_types.clone(),
            parts,
            Memory::unlimited().reservation(),
        );
        let (place, number) =
            if split.flat { (root.to_path_buf(), number) } else { (root.join(directory), 0) };
        made(&place)?;
        let name = format!("{}.{}", file_name(&split.pattern, number, zone)?, split.extension);
        rows += write_file(copy, &place.join(name).to_string_lossy(), &part, zone)?;
    }
    Ok(rows)
}

/// Whether a directory holds a file at any depth.
fn holds_files(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else { return false };
    entries.flatten().any(|entry| {
        entry.file_type().is_ok_and(|kind| !kind.is_dir()) || holds_files(&entry.path())
    })
}

/// Removes every file under a directory and leaves the directories, which is what the pin's
/// `OVERWRITE` does.
fn remove_files(directory: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(directory) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_files(&path)?;
        } else {
            std::fs::remove_file(&path).map_err(|error| {
                Error::io(format!("Could not remove file \"{}\": {error}", path.display()))
            })?;
        }
    }
    Ok(())
}

/// Appends `text` percent encoded, where everything but a letter, a digit and `_-~.` is a `%`
/// and two upper case hex digits a byte.
fn encode(out: &mut String, text: &str) {
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'~' | b'.') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
}

/// Appends a partition value percent encoded, where a value that would read as the NULL
/// directory has its first character encoded as well so that it gets a directory of its own.
fn encode_value(out: &mut String, text: &str) {
    let start = out.len();
    encode(out, text);
    if out[start..].eq_ignore_ascii_case(NULL_PARTITION) {
        let first = out.as_bytes()[start];
        out.replace_range(start..=start, &format!("%{first:02X}"));
    }
}

/// The name of a file before its extension, from the pieces of a `FILENAME_PATTERN`.
fn file_name(pattern: &[NamePiece], number: usize, zone: SessionTimeZone) -> Result<String> {
    let mut out = String::new();
    for piece in pattern {
        match piece {
            NamePiece::Text(text) => out.push_str(text),
            NamePiece::Offset => out.push_str(&number.to_string()),
            NamePiece::Uuid | NamePiece::Uuid7 => {
                let kind = if *piece == NamePiece::Uuid { "uuidv4" } else { "uuidv7" };
                let drawn = rudb_kernels::drawn(kind, 1)?;
                let value = drawn.try_value_at(0)?;
                out.push_str(&text(&value, &LogicalType::Uuid, zone)?.unwrap_or_default());
            }
        }
    }
    Ok(out)
}

/// Writes the rows of `result` to the CSV file at `path` and answers how many there were.
fn write_csv(
    copy: &CopyTo,
    path: &str,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    let file = File::create(path)
        .map_err(|error| Error::io(format!("Cannot open file \"{path}\": {error}")))?;
    let mut out = BufWriter::new(file);
    let names = result.names();
    let forced = names
        .iter()
        .map(|name| {
            copy.force_quote_all
                || copy.force_quote.iter().any(|column| column.eq_ignore_ascii_case(name))
        })
        .collect::<Vec<_>>();
    for column in &copy.force_quote {
        if !names.iter().any(|name| name.eq_ignore_ascii_case(column)) {
            return Err(Error::binder(format!(
                "\"force_quote\" expected to find {column}, but it was not found in the table"
            )));
        }
    }
    let written =
        |error: std::io::Error| Error::io(format!("Could not write file \"{path}\": {error}"));
    let mut line = String::new();
    if copy.header {
        for (column, name) in names.iter().enumerate() {
            if column > 0 {
                line.push_str(&copy.delimiter);
            }
            field(&mut line, name, false, copy);
        }
        line.push('\n');
        out.write_all(line.as_bytes()).map_err(written)?;
    }
    let mut rows = 0;
    for chunk in result.chunks() {
        let mut columns = Vec::with_capacity(chunk.width());
        for column in 0..chunk.width() {
            columns.push(cast_in_time_zone(
                chunk.column(column)?,
                &LogicalType::Varchar,
                false,
                Some(zone),
            )?);
        }
        // row at a time: a CSV file is written a line per row, and every column is already text.
        for row in 0..chunk.len() {
            line.clear();
            for (column, vector) in columns.iter().enumerate() {
                if column > 0 {
                    line.push_str(&copy.delimiter);
                }
                match vector.try_value_at(row)? {
                    Value::Null => line.push_str(&copy.null),
                    Value::Varchar(text) => field(&mut line, &text, forced[column], copy),
                    other => field(&mut line, &other.to_string(), forced[column], copy),
                }
            }
            line.push('\n');
            out.write_all(line.as_bytes()).map_err(written)?;
        }
        rows += chunk.len();
    }
    out.flush().map_err(written)?;
    Ok(rows)
}

/// Appends one value, quoted if it has to be or was asked to be.
fn field(line: &mut String, text: &str, forced: bool, copy: &CopyTo) {
    // An empty quote writes every value as it is, with nothing escaped either, whatever the escape
    // is. The pin does, and the corpus writes JSON lines that way through `quote ''`.
    if copy.quote.is_empty() {
        line.push_str(text);
        return;
    }
    let quoted = forced
        || text == copy.null
        || text.contains(copy.delimiter.as_str())
        || (!copy.quote.is_empty() && text.contains(copy.quote.as_str()))
        || text.contains(['\n', '\r']);
    if !quoted {
        line.push_str(text);
        return;
    }
    line.push_str(&copy.quote);
    let mut rest = text;
    while !rest.is_empty() {
        let special = [copy.quote.as_str(), copy.escape.as_str()]
            .into_iter()
            .filter(|mark| !mark.is_empty())
            .find(|mark| rest.starts_with(mark));
        if let Some(mark) = special {
            line.push_str(&copy.escape);
            line.push_str(mark);
            rest = &rest[mark.len()..];
        } else {
            let next = rest.chars().next().map_or(1, char::len_utf8);
            line.push_str(&rest[..next]);
            rest = &rest[next..];
        }
    }
    line.push_str(&copy.quote);
}

/// Writes the rows of `result` to the JSON file at `path` and answers how many there were.
fn write_json(
    copy: &CopyTo,
    path: &str,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    let file = File::create(path)
        .map_err(|error| Error::io(format!("Cannot open file \"{path}\": {error}")))?;
    let mut out = BufWriter::new(file);
    let written =
        |error: std::io::Error| Error::io(format!("Could not write file \"{path}\": {error}"));
    let moments = Moments {
        zone,
        date: copy.date_format.as_deref().map(Format::parse).transpose()?,
        timestamp: copy.timestamp_format.as_deref().map(Format::parse).transpose()?,
    };
    let names = result.names();
    let types = result.types();
    if copy.array {
        out.write_all(b"[\n").map_err(written)?;
    }
    let mut line = String::new();
    let mut rows = 0;
    for chunk in result.chunks() {
        let mut columns = Vec::with_capacity(chunk.width());
        for (column, ty) in types.iter().enumerate() {
            let vector = chunk.column(column)?;
            columns.push(if quoted(ty) && !moments.formats(ty) {
                cast_in_time_zone(vector, &LogicalType::Varchar, false, Some(zone))?
            } else {
                vector.clone()
            });
        }
        for row in 0..chunk.len() {
            line.clear();
            if copy.array {
                line.push_str(if rows == 0 { "\t" } else { ",\n\t" });
            }
            line.push('{');
            for (column, vector) in columns.iter().enumerate() {
                if column > 0 {
                    line.push(',');
                }
                string(&mut line, &names[column]);
                line.push(':');
                json(&mut line, &vector.try_value_at(row)?, &types[column], &moments)?;
            }
            line.push('}');
            if !copy.array {
                line.push('\n');
            }
            out.write_all(line.as_bytes()).map_err(written)?;
            rows += 1;
        }
    }
    if copy.array {
        out.write_all(if rows == 0 { b"\t\n]\n" } else { b"\n]\n" }).map_err(written)?;
    }
    out.flush().map_err(written)?;
    Ok(rows)
}

/// Writes a result as a Parquet file, a row group every `ROW_GROUP_SIZE` rows or so, since a
/// group ends at the end of the chunk that fills it.
fn write_parquet(copy: &CopyTo, path: &str, result: &QueryResult) -> Result<usize> {
    let file = File::create(path)
        .map_err(|error| Error::io(format!("Cannot open file \"{path}\": {error}")))?;
    let types = result.types();
    let mut stored = Vec::with_capacity(types.len());
    let mut fields = Vec::with_capacity(types.len());
    for (name, ty) in result.names().iter().zip(types) {
        let ty = rudb_parquet::storage(ty)?;
        fields.push(Field::new(name.clone(), ty.clone()));
        stored.push(ty);
    }
    let codec = if copy.compression == "snappy" { Codec::Snappy } else { Codec::Uncompressed };
    let mut writer = rudb_parquet::Writer::new(BufWriter::new(file), &fields, codec, CREATED_BY)?;
    let mut group = Vec::new();
    let (mut waiting, mut rows) = (0, 0);
    for chunk in result.chunks() {
        let mut columns = Vec::with_capacity(chunk.width());
        for (column, ty) in types.iter().enumerate() {
            let vector = chunk.column(column)?;
            columns.push(if *ty == stored[column] {
                vector.clone()
            } else {
                cast_in_time_zone(vector, &stored[column], false, None)?
            });
        }
        group.push(Chunk::new(columns)?);
        waiting += chunk.len();
        rows += chunk.len();
        if waiting as u64 >= copy.row_group_size {
            writer.write_group(&group)?;
            group.clear();
            waiting = 0;
        }
    }
    writer.write_group(&group)?;
    writer.finish()?;
    Ok(rows)
}

/// What a Parquet file written here says wrote it.
const CREATED_BY: &str = concat!("rudb version ", env!("CARGO_PKG_VERSION"));

/// Whether a value of `ty` is written as a JSON string, the text of its cast to VARCHAR.
fn quoted(ty: &LogicalType) -> bool {
    !matches!(
        ty,
        LogicalType::Null
            | LogicalType::Boolean
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
            | LogicalType::Float
            | LogicalType::Double
            | LogicalType::Decimal { .. }
            | LogicalType::List(_)
            | LogicalType::Array(..)
            | LogicalType::Struct(_)
            | LogicalType::Map(..)
            | LogicalType::Union(_)
    )
}

/// Appends one value of type `ty`. A whole column of a quoted type arrives already cast to VARCHAR,
/// and what is nested in a list, a struct or a map is cast here, one value at a time.
fn json(line: &mut String, value: &Value, ty: &LogicalType, moments: &Moments) -> Result<()> {
    match value {
        Value::Null => line.push_str("null"),
        Value::Boolean(value) => line.push_str(if *value { "true" } else { "false" }),
        Value::TinyInt(value) => line.push_str(&value.to_string()),
        Value::SmallInt(value) => line.push_str(&value.to_string()),
        Value::Integer(value) => line.push_str(&value.to_string()),
        Value::BigInt(value) if !quoted(ty) => line.push_str(&value.to_string()),
        Value::HugeInt(value) if !quoted(ty) => line.push_str(&value.to_string()),
        Value::UTinyInt(value) => line.push_str(&value.to_string()),
        Value::USmallInt(value) => line.push_str(&value.to_string()),
        Value::UInteger(value) => line.push_str(&value.to_string()),
        Value::UBigInt(value) => line.push_str(&value.to_string()),
        Value::UHugeInt(value) => line.push_str(&value.to_string()),
        Value::Float(value) => double(line, f64::from(*value)),
        Value::Double(value) => double(line, *value),
        Value::Decimal { .. } => match cast_value(value, &LogicalType::Double, false)? {
            Value::Double(value) => double(line, value),
            other => line.push_str(&other.to_string()),
        },
        Value::Varchar(text) => string(line, text),
        Value::List { values, .. } => {
            let element = match ty {
                LogicalType::List(element) | LogicalType::Array(element, _) => element.as_ref(),
                _ => &LogicalType::Null,
            };
            line.push('[');
            for (at, value) in values.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                json(line, value, element, moments)?;
            }
            line.push(']');
        }
        Value::Struct(fields) => {
            let types = match ty {
                LogicalType::Struct(types) => types.as_slice(),
                _ => &[],
            };
            line.push('{');
            for (at, (name, value)) in fields.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                string(line, name);
                line.push(':');
                let ty = types.get(at).map_or(&LogicalType::Null, |field| &field.ty);
                json(line, value, ty, moments)?;
            }
            line.push('}');
        }
        Value::Map { key, value: value_type, entries } => {
            line.push('{');
            for (at, (name, value)) in entries.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                match moments.text(name, key)? {
                    Some(name) => string(line, &name),
                    None => line.push_str("null"),
                }
                line.push(':');
                json(line, value, value_type, moments)?;
            }
            line.push('}');
        }
        // A union is an object of the one member it holds, under the member's name.
        Value::Union { members, tag, value } => {
            let Some(member) = members.get(usize::from(*tag)) else {
                return Err(Error::internal("a union tag names no member"));
            };
            line.push('{');
            string(line, &member.name);
            line.push(':');
            json(line, value, &member.ty, moments)?;
            line.push('}');
        }
        other => match moments.text(other, ty)? {
            Some(text) => string(line, &text),
            None => line.push_str("null"),
        },
    }
    Ok(())
}

/// What the JSON writer needs to write a moment: the session's time zone, and the `dateformat`
/// and the `timestampformat` the `COPY` was given, if it was.
struct Moments {
    zone: SessionTimeZone,
    date: Option<Format>,
    timestamp: Option<Format>,
}

impl Moments {
    /// The text a value that is not a JSON number, a list or an object is written as: a moment
    /// through its format, and anything else as its cast to VARCHAR under the session's zone.
    fn text(&self, value: &Value, ty: &LogicalType) -> Result<Option<String>> {
        match self.formatted(value)? {
            Some(text) => Ok(Some(text)),
            None => text(value, ty, self.zone),
        }
    }

    /// Whether a value of `ty` is written through one of the formats rather than its cast.
    fn formats(&self, ty: &LogicalType) -> bool {
        match ty {
            LogicalType::Date => self.date.is_some(),
            LogicalType::Timestamp
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs
            | LogicalType::TimestampTz => self.timestamp.is_some(),
            _ => false,
        }
    }

    /// The text of a date or a timestamp written through its format, or `None` for any other
    /// value and for a moment there is no format for. A timestamp in seconds or milliseconds is
    /// written as a timestamp in microseconds, and one with a time zone as the wall clock it shows
    /// in the session's zone, which are the pin's answers.
    fn formatted(&self, value: &Value) -> Result<Option<String>> {
        let written = match (value, &self.date, &self.timestamp) {
            (Value::Date(_), Some(format), _) => format.write(value)?,
            (Value::Timestamp(_) | Value::TimestampNs(_), _, Some(format)) => {
                format.write(value)?
            }
            (Value::TimestampS(_) | Value::TimestampMs(_), _, Some(format)) => {
                format.write(&cast_value(value, &LogicalType::Timestamp, false)?)?
            }
            (Value::TimestampTz(micros), _, Some(format)) => {
                format.write_zoned(*micros, self.zone)?
            }
            _ => return Ok(None),
        };
        Ok(match written {
            Value::Null => None,
            Value::Varchar(text) => Some(text),
            other => Some(other.to_string()),
        })
    }
}

/// A value's cast to VARCHAR under the session's time zone, or `None` for a null.
fn text(value: &Value, ty: &LogicalType, zone: SessionTimeZone) -> Result<Option<String>> {
    let ty = if matches!(ty, LogicalType::Null) { value.logical_type() } else { ty.clone() };
    let vector = Vector::from_values(ty, std::slice::from_ref(value))?;
    let cast = cast_in_time_zone(&vector, &LogicalType::Varchar, false, Some(zone))?;
    Ok(match cast.try_value_at(0)? {
        Value::Null => None,
        Value::Varchar(text) => Some(text),
        other => Some(other.to_string()),
    })
}

/// Appends `text` as a JSON string, escaped the way the pin's writer escapes it.
fn string(line: &mut String, text: &str) {
    line.push('"');
    for character in text.chars() {
        match character {
            '"' => line.push_str("\\\""),
            '\\' => line.push_str("\\\\"),
            '\n' => line.push_str("\\n"),
            '\t' => line.push_str("\\t"),
            '\r' => line.push_str("\\r"),
            '\u{8}' => line.push_str("\\b"),
            '\u{c}' => line.push_str("\\f"),
            control if u32::from(control) < 0x20 => {
                line.push_str(&format!("\\u{:04X}", u32::from(control)));
            }
            other => line.push(other),
        }
    }
    line.push('"');
}

/// Appends a double the way the pin's JSON writer does: the shortest digits that read back the
/// same, laid out plainly with a `.0` on a whole number when the exponent is from -6 to 20, and as
/// `1.5e-7` or `1e21` outside that, with `Infinity`, `-Infinity` and `NaN` bare.
fn double(line: &mut String, value: f64) {
    if value.is_nan() {
        line.push_str("NaN");
        return;
    }
    if value.is_infinite() {
        line.push_str(if value > 0.0 { "Infinity" } else { "-Infinity" });
        return;
    }
    if value == 0.0 {
        line.push_str(if value.is_sign_negative() { "-0.0" } else { "0.0" });
        return;
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits = mantissa.replace('.', "");
    if value < 0.0 {
        line.push('-');
    }
    if !(-6..=20).contains(&exponent) {
        line.push_str(mantissa);
        line.push('e');
        line.push_str(&exponent.to_string());
        return;
    }
    if exponent < 0 {
        line.push_str("0.");
        for _ in 0..(-exponent - 1) {
            line.push('0');
        }
        line.push_str(&digits);
        return;
    }
    let whole = usize::try_from(exponent).unwrap_or(0) + 1;
    if digits.len() <= whole {
        line.push_str(&digits);
        for _ in digits.len()..whole {
            line.push('0');
        }
        line.push_str(".0");
    } else {
        line.push_str(&digits[..whole]);
        line.push('.');
        line.push_str(&digits[whole..]);
    }
}
