//! `read_text(path)` and `read_blob(path)`, one row per file with the whole of it in a column, and
//! `glob(path)`, one row per file with only its name.
//!
//! The binder expanded the patterns, so the arguments are one constant per file in the order the
//! rows go out. The files are read while the operator is built, like the other metadata tables
//! here, and a query that does not ask for `content` never reads one: its size and its time come
//! from the file's metadata, which is also the only way a file that is not text can be counted by
//! `read_text` without an error, the pin's answer too.

use std::time::UNIX_EPOCH;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_functions::{TableFunction, content_fields};
use rudb_plan::{Expr, Plan, Slice};

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

/// The file names the binder folded the arguments into, in the order the rows go out.
fn paths(plan: &Plan, args: Slice) -> Result<Vec<String>> {
    plan.expr_list(args)
        .iter()
        .map(|argument| match plan.expr(*argument) {
            Expr::Constant(reference) => match plan.value(*reference) {
                Value::Varchar(path) => Ok(path.clone()),
                other => Err(Error::internal(format!("a file name arrived as {other}"))),
            },
            _ => Err(Error::internal("a file name that the binder did not fold")),
        })
        .collect()
}
