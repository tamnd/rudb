//! `COPY ... TO` a CSV file.
//!
//! Every value is written as its cast to VARCHAR, under the session's time zone, which is what the
//! pin writes: a list is `[1, 2]`, a double is `1e+20`, a blob is `\x00\xFF`. A value is quoted
//! when it holds the delimiter, the quote or a line break, or when it reads the same as a null
//! would, so an empty string beside an empty null is `""` and a null is nothing at all. An escape
//! character alone does not quote a value. Spaces at either end are left bare, the way the pin
//! leaves them.

use std::fs::File;
use std::io::{BufWriter, Write};

use rudb_bind::CopyTo;
use rudb_common::{Error, LogicalType, Result, SessionTimeZone, Value};
use rudb_kernels::cast::cast_in_time_zone;

use crate::QueryResult;

/// Writes the rows of `result` to the file `copy` names and answers how many there were.
pub(crate) fn write_csv(
    copy: &CopyTo,
    result: &QueryResult,
    zone: SessionTimeZone,
) -> Result<usize> {
    let file = File::create(&copy.path)
        .map_err(|error| Error::io(format!("Cannot open file \"{}\": {error}", copy.path)))?;
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
    let written = |error: std::io::Error| {
        Error::io(format!("Could not write file \"{}\": {error}", copy.path))
    };
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
