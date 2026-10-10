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
//!
//! With `FILE_SIZE_BYTES` or `ROW_GROUPS_PER_FILE`, the path is a directory and a file is closed
//! once it is big enough, the next rows going to the next one, numbered on. `PER_THREAD_OUTPUT`
//! makes it a directory too, with the rows shared out over a file for each thread.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::rc::Rc;

use rudb_bind::{CopyTo, Existing, NamePiece};
use rudb_common::{Error, Field, LogicalType, Memory, Result, SessionTimeZone, Value};
use rudb_compress::Codec;
use rudb_kernels::cast::{cast_in_time_zone, cast_value};
use rudb_kernels::compare::order_with_nulls;
use rudb_kernels::strftime::Format;
use rudb_vector::{Chunk, Vector};

use crate::QueryResult;

/// How many rows a batch holds. The pin counts the batches of a file in these and closes a file
/// only between two of them.
const BATCH: usize = 2048;

/// Writes the rows of `result` the way `copy` asks, to the one file or to a directory of them,
/// and answers how many there were.
pub(crate) fn write(
    copy: &CopyTo,
    result: &QueryResult,
    zone: SessionTimeZone,
    threads: usize,
) -> Result<usize> {
    let split = &copy.partitioned;
    if split.skip_empty && result.is_empty() {
        return Ok(0);
    }
    if !split.directory() {
        let mut sink = Sink::open(copy, &copy.path, result, zone)?;
        sink.write(result.chunks())?;
        return sink.finish();
    }
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
    make_directory(root)?;
    if split.columns.is_empty() {
        return Ok(write_files(copy, root, 0, result, zone, threads)?.0);
    }
    write_partitioned(copy, root, result, zone)
}

/// Makes a directory and the ones above it.
fn make_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        Error::io(format!("Failed to create directory \"{}\": {error}", path.display()))
    })
}

/// Writes the rows of `result` into `directory`, the files numbered from `first`, and answers how
/// many rows and how many files there were.
///
/// When the `COPY` rotates, a file is closed before the next batch once it holds one and has
/// reached the size or the count of batches asked for, so a file is never left empty, where the
/// pin goes on opening empty files for good when a size is below that of an empty file. No rows
/// at all still write the one file.
fn write_files(
    copy: &CopyTo,
    directory: &Path,
    first: usize,
    result: &QueryResult,
    zone: SessionTimeZone,
    threads: usize,
) -> Result<(usize, usize)> {
    let split = &copy.partitioned;
    let path = |number: usize| -> Result<String> {
        let name = format!("{}.{}", file_name(&split.pattern, number, zone)?, split.extension);
        Ok(directory.join(name).to_string_lossy().into_owned())
    };
    if split.per_thread && !split.rotates() {
        return write_threads(copy, &path, first, result, zone, threads);
    }
    let mut sink = Sink::open(copy, &path(first)?, result, zone)?;
    if !split.rotates() {
        sink.write(result.chunks())?;
        return Ok((sink.finish()?, 1));
    }
    let least = if copy.parquet { copy.row_group_size } else { 0 };
    let (mut rows, mut files, mut held) = (0, 1, 0);
    for batch in batches(result.chunks(), least)? {
        let full = held > 0
            && (split.file_size.is_some_and(|most| sink.size() >= most)
                || split.batches.is_some_and(|most| held >= most));
        if full {
            rows += sink.finish()?;
            sink = Sink::open(copy, &path(first + files)?, result, zone)?;
            files += 1;
            held = 0;
        }
        sink.write(&batch)?;
        held += 1;
    }
    Ok((rows + sink.finish()?, files))
}

/// Writes the rows of `result` the way `PER_THREAD_OUTPUT` does, a file for each of `threads`
/// that gets rows, each taking the next run of batches, and answers how many rows and files there
/// were.
///
/// The pin writes a file for each thread that ends up with rows, so how many there are depends on
/// how its scan was split, and a source that one thread reads, such as `range`, gives it one file.
/// Here the batches are shared out evenly, which gives the same rows in as many files as threads,
/// at most, and one file for no rows.
fn write_threads(
    copy: &CopyTo,
    path: &dyn Fn(usize) -> Result<String>,
    first: usize,
    result: &QueryResult,
    zone: SessionTimeZone,
    threads: usize,
) -> Result<(usize, usize)> {
    let batches = batches(result.chunks(), 0)?;
    let each = batches.len().div_ceil(threads.max(1)).max(1);
    let mut runs = batches.chunks(each).peekable();
    if runs.peek().is_none() {
        let sink = Sink::open(copy, &path(first)?, result, zone)?;
        return Ok((sink.finish()?, 1));
    }
    let (mut rows, mut files) = (0, 0);
    for run in runs {
        let mut sink = Sink::open(copy, &path(first + files)?, result, zone)?;
        for batch in run {
            sink.write(batch)?;
        }
        rows += sink.finish()?;
        files += 1;
    }
    Ok((rows, files))
}

/// The rows cut into the batches the pin closes a file between: [`BATCH`] rows each, or as many of
/// those as it takes to reach `least` rows, which for Parquet is a row group.
fn batches(chunks: &[Chunk], least: u64) -> Result<Vec<Vec<Chunk>>> {
    let mut out = Vec::new();
    let (mut batch, mut held) = (Vec::new(), 0);
    for chunk in chunks {
        let mut at = 0;
        while at < chunk.len() {
            let take = (BATCH - held % BATCH).min(chunk.len() - at);
            batch.push(if take == chunk.len() {
                chunk.clone()
            } else {
                let columns = chunk.columns().iter().map(|column| column.slice(at, take));
                Chunk::new(columns.collect::<Result<Vec<_>>>()?)?
            });
            at += take;
            held += take;
            if held % BATCH == 0 && held as u64 >= least {
                out.push(std::mem::take(&mut batch));
                held = 0;
            }
        }
    }
    if !batch.is_empty() {
        out.push(batch);
    }
    Ok(out)
}

/// What a directory is called for a NULL partition value, which is the name Hive gave it.
const NULL_PARTITION: &str = "__HIVE_DEFAULT_PARTITION__";

/// Writes the rows of `result` split by the values of the partition columns, a directory a
/// column named `column=value`, both percent encoded, with the files in each of the deepest ones.
///
/// The rows of a partition keep their order. A partition's files are numbered from 0 in its own
/// directory, or, with the files side by side, on from the last partition's, the partitions in the
/// order of their values, a column at a time with a NULL last, which is how the pin numbers them.
fn write_partitioned(
    copy: &CopyTo,
    root: &Path,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    let split = &copy.partitioned;
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
    let (mut rows, mut next) = (0, 0);
    for (directory, _, picked) in &partitions {
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
        let (place, first) =
            if split.flat { (root.to_path_buf(), next) } else { (root.join(directory), 0) };
        make_directory(&place)?;
        let (written, files) = write_files(copy, &place, first, &part, zone, 1)?;
        rows += written;
        next += files;
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

/// One file of the format a `COPY` names, written a batch of rows at a time.
struct Sink<'a> {
    copy: &'a CopyTo,
    path: String,
    zone: SessionTimeZone,
    names: &'a [String],
    types: &'a [LogicalType],
    /// How many bytes have gone to the file so far, buffered or not.
    size: Rc<Cell<u64>>,
    rows: usize,
    body: Body,
}

/// What a [`Sink`] holds for its format.
enum Body {
    Csv {
        out: Counted,
        /// Whether every value of a column is quoted.
        forced: Vec<bool>,
    },
    Json {
        out: Counted,
        moments: Moments,
    },
    Parquet {
        writer: rudb_parquet::Writer<Counted>,
        /// The type each column is cast to before it is stored.
        stored: Vec<LogicalType>,
        /// The chunks of the row group not written yet, and how many rows they hold.
        group: Vec<Chunk>,
        waiting: usize,
    },
}

/// A buffered file that counts the bytes written to it.
struct Counted {
    out: BufWriter<File>,
    size: Rc<Cell<u64>>,
}

impl Write for Counted {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.out.write(bytes)?;
        self.size.set(self.size.get() + written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

impl<'a> Sink<'a> {
    /// Makes the file at `path` for rows of the columns of `result`, and writes what comes before
    /// the first row: the header of a CSV file, or the opening of a JSON array.
    fn open(
        copy: &'a CopyTo,
        path: &str,
        result: &'a QueryResult,
        zone: SessionTimeZone,
    ) -> Result<Self> {
        let file = File::create(path)
            .map_err(|error| Error::io(format!("Cannot open file \"{path}\": {error}")))?;
        let size = Rc::new(Cell::new(0));
        let mut out = Counted { out: BufWriter::new(file), size: Rc::clone(&size) };
        let (names, types) = (result.names(), result.types());
        let written = failed(path);
        let body = if copy.parquet {
            let mut stored = Vec::with_capacity(types.len());
            let mut fields = Vec::with_capacity(types.len());
            for (name, ty) in names.iter().zip(types) {
                let ty = rudb_parquet::storage(ty)?;
                fields.push(Field::new(name.clone(), ty.clone()));
                stored.push(ty);
            }
            let codec =
                if copy.compression == "snappy" { Codec::Snappy } else { Codec::Uncompressed };
            let writer = rudb_parquet::Writer::new(out, &fields, codec, CREATED_BY)?;
            Body::Parquet { writer, stored, group: Vec::new(), waiting: 0 }
        } else if copy.json {
            let moments = Moments {
                zone,
                date: copy.date_format.as_deref().map(Format::parse).transpose()?,
                timestamp: copy.timestamp_format.as_deref().map(Format::parse).transpose()?,
            };
            if copy.array {
                out.write_all(b"[\n").map_err(written)?;
            }
            Body::Json { out, moments }
        } else {
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
                        "\"force_quote\" expected to find {column}, but it was not found in the \
                         table"
                    )));
                }
            }
            if copy.header {
                let mut line = String::new();
                for (column, name) in names.iter().enumerate() {
                    if column > 0 {
                        line.push_str(&copy.delimiter);
                    }
                    field(&mut line, name, false, copy);
                }
                line.push('\n');
                out.write_all(line.as_bytes()).map_err(written)?;
            }
            Body::Csv { out, forced }
        };
        Ok(Self { copy, path: path.to_string(), zone, names, types, size, rows: 0, body })
    }

    /// How many bytes the file holds so far.
    fn size(&self) -> u64 {
        self.size.get()
    }

    /// Writes the rows of `chunks`.
    fn write(&mut self, chunks: &[Chunk]) -> Result<()> {
        let Self { copy, path, zone, names, types, rows, body, .. } = self;
        let (copy, zone) = (*copy, *zone);
        let written = failed(path);
        match body {
            Body::Csv { out, forced } => {
                let mut line = String::new();
                for chunk in chunks {
                    let mut columns = Vec::with_capacity(chunk.width());
                    for column in 0..chunk.width() {
                        columns.push(cast_in_time_zone(
                            chunk.column(column)?,
                            &LogicalType::Varchar,
                            false,
                            Some(zone),
                        )?);
                    }
                    // row at a time: a CSV file is written a line per row, and every column is
                    // already text.
                    for row in 0..chunk.len() {
                        line.clear();
                        for (column, vector) in columns.iter().enumerate() {
                            if column > 0 {
                                line.push_str(&copy.delimiter);
                            }
                            match vector.try_value_at(row)? {
                                Value::Null => line.push_str(&copy.null),
                                Value::Varchar(text) => {
                                    field(&mut line, &text, forced[column], copy);
                                }
                                other => {
                                    field(&mut line, &other.to_string(), forced[column], copy);
                                }
                            }
                        }
                        line.push('\n');
                        out.write_all(line.as_bytes()).map_err(written)?;
                    }
                    *rows += chunk.len();
                }
            }
            Body::Json { out, moments } => {
                let mut line = String::new();
                for chunk in chunks {
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
                            line.push_str(if *rows == 0 { "\t" } else { ",\n\t" });
                        }
                        line.push('{');
                        for (column, vector) in columns.iter().enumerate() {
                            if column > 0 {
                                line.push(',');
                            }
                            string(&mut line, &names[column]);
                            line.push(':');
                            json(&mut line, &vector.try_value_at(row)?, &types[column], moments)?;
                        }
                        line.push('}');
                        if !copy.array {
                            line.push('\n');
                        }
                        out.write_all(line.as_bytes()).map_err(written)?;
                        *rows += 1;
                    }
                }
            }
            Body::Parquet { writer, stored, group, waiting } => {
                for chunk in chunks {
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
                    *waiting += chunk.len();
                    *rows += chunk.len();
                    // A group ends at the end of the chunk that fills it, so it holds
                    // `ROW_GROUP_SIZE` rows or a little more.
                    if *waiting as u64 >= copy.row_group_size {
                        writer.write_group(group)?;
                        group.clear();
                        *waiting = 0;
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes what comes after the last row and closes the file, and answers how many rows it
    /// holds.
    fn finish(self) -> Result<usize> {
        let written = failed(&self.path);
        match self.body {
            Body::Csv { mut out, .. } => out.flush().map_err(written)?,
            Body::Json { mut out, .. } => {
                if self.copy.array {
                    let end: &[u8] = if self.rows == 0 { b"\t\n]\n" } else { b"\n]\n" };
                    out.write_all(end).map_err(written)?;
                }
                out.flush().map_err(written)?;
            }
            Body::Parquet { mut writer, group, .. } => {
                writer.write_group(&group)?;
                writer.finish()?;
            }
        }
        Ok(self.rows)
    }
}

/// The error a failed write to the file at `path` is reported as.
fn failed(path: &str) -> impl Fn(std::io::Error) -> Error + Copy + '_ {
    move |error| Error::io(format!("Could not write file \"{path}\": {error}"))
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
