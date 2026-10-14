//! `read_text(path)` and `read_blob(path)`, one row per file with the whole of it in a column, and
//! `glob(path)`, one row per file with only its name, and the five that print a Parquet footer,
//! `parquet_metadata`, `parquet_schema`, `parquet_kv_metadata`, `parquet_file_metadata` and
//! `parquet_full_metadata`.
//!
//! The binder expanded the patterns, so the arguments are one constant per file in the order the
//! rows go out. The files are read while the operator is built, like the other metadata tables
//! here, and a query that does not ask for `content` never reads one: its size and its time come
//! from the file's metadata, which is also the only way a file that is not text can be counted by
//! `read_text` without an error, the pin's answer too.

use std::time::UNIX_EPOCH;

use rudb_common::{Error, Field, LogicalType, Result, SessionTimeZone, Value};
use rudb_functions::{TableFunction, content_fields, parquet_bloom_excludes, parquet_footer};
use rudb_kernels::cast_in_time_zone;
use rudb_parquet::{MetadataTable, RawFooter};
use rudb_plan::{Expr, ExprRef, Plan, Slice};
use rudb_vector::Vector;

use crate::metadata::{Metadata, text};

/// The rows of a `read_text` or a `read_blob` over the files its arguments name.
///
/// # Errors
///
/// When a file cannot be read, and for `read_text` when one is not UTF-8.
pub(crate) fn contents(
    function: TableFunction,
    plan: &Plan,
    args: Slice,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let name = function.name();
    if function == TableFunction::Glob {
        let rows = paths(plan, args)?.into_iter().map(|path| vec![text(&path)]).collect::<Vec<_>>();
        let fields = [Field::new("file", LogicalType::Varchar)];
        return Metadata::new(name, &fields, &rows, plan, index, columns);
    }
    let blob = function == TableFunction::ReadBlob;
    let wanted = plan.field_list(columns).iter().any(|field| field.name == "content");
    let mut rows = Vec::new();
    for path in paths(plan, args)? {
        let opened =
            |error: std::io::Error| Error::io(format!("Cannot open file \"{path}\": {error}"));
        let metadata = std::fs::metadata(&path).map_err(opened)?;
        let content = if !wanted {
            Value::Null
        } else if blob {
            Value::Blob(std::fs::read(&path).map_err(opened)?)
        } else {
            let bytes = std::fs::read(&path).map_err(opened)?;
            Value::Varchar(String::from_utf8(bytes).map_err(|_| {
                Error::invalid_input(format!(
                    "{name}: could not read content of file '{path}' as valid UTF-8 encoded \
                     text. You may want to use read_blob instead."
                ))
            })?)
        };
        // Whole seconds, which is what the pin's file system reports a modification time in.
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_secs()).ok())
            .map_or(Value::Null, |seconds| Value::TimestampTz(seconds * 1_000_000));
        let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        rows.push(vec![text(&path), content, Value::BigInt(size), modified]);
    }
    Metadata::new(name, &content_fields(blob), &rows, plan, index, columns)
}

/// The rows of one of the functions that print a Parquet footer, a file after another.
///
/// `parquet_full_metadata` has a row per file, with the rows of each of the other four for that
/// file as a list of structs, the way the pin puts them together.
///
/// # Errors
///
/// When a file is missing or is not Parquet, and what [`MetadataTable::rows`] reports.
pub(crate) fn parquet_tables(
    function: TableFunction,
    table: MetadataTable,
    plan: &Plan,
    args: Slice,
    index: u32,
    columns: Slice,
    time_zone: SessionTimeZone,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for path in paths(plan, args)? {
        let footer = parquet_footer(&path)?;
        if table != MetadataTable::Full {
            rows.extend(printed(table, &footer, &path, time_zone)?);
            continue;
        }
        let mut row = Vec::with_capacity(MetadataTable::PARTS.len());
        for part in MetadataTable::PARTS {
            let fields = part.fields();
            let values = printed(part, &footer, &path, time_zone)?
                .into_iter()
                .map(|values| {
                    let names = fields.iter().map(|field| field.name.clone());
                    Value::Struct(names.zip(values).collect())
                })
                .collect();
            row.push(Value::List { element: LogicalType::Struct(fields), values });
        }
        rows.push(row);
    }
    Metadata::new(function.name(), &table.fields(), &rows, plan, index, columns)
}

/// The rows of `table` for the file at `path`, with every value in a `VARCHAR` column as text.
///
/// A statistic comes back from the footer in its column's type and is turned into text here, in
/// the session's time zone, which is where a `TIMESTAMPTZ` bound gets the offset it prints with.
fn printed(
    table: MetadataTable,
    footer: &RawFooter,
    path: &str,
    time_zone: SessionTimeZone,
) -> Result<Vec<Vec<Value>>> {
    let fields = table.fields();
    let mut rows = table.rows(footer, path)?;
    for row in &mut rows {
        for (value, field) in row.iter_mut().zip(&fields) {
            let typed = !matches!(value, Value::Null | Value::Varchar(_));
            if field.ty == LogicalType::Varchar && typed {
                let held = Vector::from_values(value.logical_type(), std::slice::from_ref(value))?;
                let cast = cast_in_time_zone(&held, &LogicalType::Varchar, false, Some(time_zone))?;
                *value = cast.value_at(0);
            }
        }
    }
    Ok(rows)
}

/// Whether `ty` is a struct, list, array or map, which the pin has no cast to from a value that is
/// neither text nor one of those.
fn nested(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Struct(_)
            | LogicalType::List(_)
            | LogicalType::Array(..)
            | LogicalType::Map(..)
    )
}

/// The rows of `parquet_bloom_probe`, a row per row group of each file saying whether the column's
/// bloom filter rules the value out.
///
/// The binder put the column name and the value ahead of the files. The value is cast to the
/// column's type in each file, in the session's time zone, and a value that does not cast is the
/// pin's error about it, as is a column the file does not have.
///
/// # Errors
///
/// When a file is missing or is not Parquet, when it has no such column, and when the value does
/// not cast to the column's type.
pub(crate) fn bloom_probe(
    plan: &Plan,
    args: Slice,
    index: u32,
    columns: Slice,
    time_zone: SessionTimeZone,
) -> Result<Metadata> {
    let arguments = plan.expr_list(args);
    let [column, probe, files @ ..] = arguments else {
        return Err(Error::internal("parquet_bloom_probe without its column and value"));
    };
    let column = match constant(plan, *column)? {
        Value::Varchar(column) => column.clone(),
        other => return Err(Error::internal(format!("a column name arrived as {other}"))),
    };
    let probe = constant(plan, *probe)?;
    let held = Vector::from_values(probe.logical_type(), std::slice::from_ref(probe))?;
    let mut rows = Vec::new();
    for path in names(plan, files)? {
        let footer = parquet_footer(&path)?;
        let Some((at, ty, group)) = footer.probe_column(&column)? else {
            return Err(Error::invalid_input(format!("Column {column} not found in {path}")));
        };
        if nested(&ty) && !nested(&probe.logical_type()) && !matches!(probe, Value::Varchar(_)) {
            return Err(Error::invalid_input(format!(
                "Failed to cast value: Unimplemented type for cast ({} -> {ty})",
                probe.logical_type()
            )));
        }
        let value = cast_in_time_zone(&held, &ty, false, Some(time_zone))
            .map_err(|error| {
                Error::invalid_input(format!("Failed to cast value: {}", error.message()))
            })?
            .value_at(0);
        let value = if group { Value::Null } else { value };
        let excludes = parquet_bloom_excludes(&path, &footer, at, &value)?;
        for (group, excludes) in excludes.into_iter().enumerate() {
            let group = i64::try_from(group).unwrap_or(i64::MAX);
            rows.push(vec![text(&path), Value::BigInt(group), Value::Boolean(excludes)]);
        }
    }
    let fields = [
        Field::new("file_name", LogicalType::Varchar),
        Field::new("row_group_id", LogicalType::BigInt),
        Field::new("bloom_filter_excludes", LogicalType::Boolean),
    ];
    Metadata::new(TableFunction::ParquetBloomProbe.name(), &fields, &rows, plan, index, columns)
}

/// The file names the binder folded the arguments into, in the order the rows go out.
fn paths(plan: &Plan, args: Slice) -> Result<Vec<String>> {
    names(plan, plan.expr_list(args))
}

/// The file names among some of the arguments.
fn names(plan: &Plan, arguments: &[ExprRef]) -> Result<Vec<String>> {
    arguments
        .iter()
        .map(|argument| match constant(plan, *argument)? {
            Value::Varchar(path) => Ok(path.clone()),
            other => Err(Error::internal(format!("a file name arrived as {other}"))),
        })
        .collect()
}

/// The value of an argument the binder folded into a constant.
fn constant(plan: &Plan, argument: ExprRef) -> Result<&Value> {
    match plan.expr(argument) {
        Expr::Constant(reference) => Ok(plan.value(*reference)),
        _ => Err(Error::internal("a table function argument that the binder did not fold")),
    }
}
