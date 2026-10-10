//! The options of a `COPY ... TO` that say how the rows are split over files, read before the
//! format sees the rest.
//!
//! `PARTITION_BY` names the columns whose values pick a directory, one level a column, the way
//! the pin's hive writer lays them out. The options around it say what to do with what is
//! already there, what the files are called and whether the partition columns are written too.
//! The checks are made in the pin's order: a column the query does not have and a column named
//! twice as the options are read, then the options that cannot go together, then a partition
//! that leaves nothing to write.
//!
//! `FILE_SIZE_BYTES`, `ROW_GROUPS_PER_FILE` and `PER_THREAD_OUTPUT` are read here too, since they
//! also turn the path into a directory of numbered files, which is what the pin calls rotating.
//! So are `RETURN_FILES` and `RETURN_STATS`, which say what the statement answers with.

use rudb_common::{Error, LogicalType, Result, Value, parse_size};
use rudb_parse::ast;

use crate::statement::Written;

/// How a `COPY ... TO` splits its rows over files. With no columns it writes the one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partitioned {
    /// The columns of the query that pick the directory, in the order of the levels.
    pub columns: Vec<usize>,
    /// Whether the partition columns are written into the files as well.
    pub write_columns: bool,
    /// Whether the files go side by side in the one directory, numbered across all of them,
    /// rather than one directory a partition, which is `HIVE_FILE_PATTERN false`.
    pub flat: bool,
    /// What is done about files already in the directory.
    pub existing: Existing,
    /// What a file is called, before its extension.
    pub pattern: Vec<NamePiece>,
    /// The extension of a file, after the dot.
    pub extension: String,
    /// The size in bytes at which a file is closed and the next one begun, which is
    /// `FILE_SIZE_BYTES`.
    pub file_size: Option<u64>,
    /// How many batches of rows a file holds before the next one is begun, which is
    /// `ROW_GROUPS_PER_FILE` or `BATCHES_PER_FILE`.
    pub batches: Option<u64>,
    /// Whether the rows go to a directory of files even when one file holds them all, which is
    /// `PER_THREAD_OUTPUT`.
    pub per_thread: bool,
    /// Whether no file at all is written for no rows, which is `WRITE_EMPTY_FILE false`.
    pub skip_empty: bool,
    /// What the statement answers with.
    pub returns: Returns,
}

impl Partitioned {
    /// Whether a file is closed once it holds enough, and the rest go to the next one.
    #[must_use]
    pub fn rotates(&self) -> bool {
        self.file_size.is_some() || self.batches.is_some()
    }

    /// Whether the path names a directory the files are written into rather than the one file.
    #[must_use]
    pub fn directory(&self) -> bool {
        !self.columns.is_empty() || self.rotates() || self.per_thread
    }
}

/// What a partitioned `COPY ... TO` does about the files a directory already holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Existing {
    /// Refuses a directory that holds any file.
    #[default]
    Refuse,
    /// Removes every file under the directory first, which is `OVERWRITE`.
    Overwrite,
    /// Writes over a file of the same name and leaves the others, which is
    /// `OVERWRITE_OR_IGNORE`.
    Ignore,
    /// Adds files of new names, which is `APPEND`.
    Append,
}

/// What a `COPY ... TO` answers with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Returns {
    /// How many rows it wrote.
    #[default]
    Count,
    /// How many rows it wrote and the list of the files, which is `RETURN_FILES`.
    Files,
    /// A row for each file it wrote, with the statistics of its columns, which is
    /// `RETURN_STATS`.
    Stats,
}

/// One piece of a `FILENAME_PATTERN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamePiece {
    /// Text written as it is.
    Text(String),
    /// The number of the file, which is `{i}`.
    Offset,
    /// A random UUID, which is `{uuid}` or `{uuidv4}`.
    Uuid,
    /// A time ordered UUID, which is `{uuidv7}`.
    Uuid7,
}

/// The options read here, which the format never sees.
const READ_HERE: [&str; 14] = [
    "partition_by",
    "write_partition_columns",
    "overwrite",
    "overwrite_or_ignore",
    "append",
    "filename_pattern",
    "file_extension",
    "hive_file_pattern",
    "use_tmp_file",
    "per_thread_output",
    "file_size_bytes",
    "write_empty_file",
    "return_files",
    "return_stats",
];

/// The options that count the batches of a file, which JSON does not take.
const BATCHES: [&str; 2] = ["row_groups_per_file", "batches_per_file"];

/// Reads the partitioning options of `copy` against the columns the query has, and answers
/// them with the copy left holding only the options the format reads.
///
/// Without `PARTITION_BY` or one of the options that rotate, the options about the files of a
/// directory have nothing to act on and are taken and dropped, as the pin drops them for one file.
pub(crate) fn partitioning(
    copy: &ast::CopyTo,
    typed: &[Option<Written>],
    names: &[String],
    format: &str,
) -> Result<(Partitioned, ast::CopyTo)> {
    let partitioned = copy.options.iter().any(|(name, _)| name == "partition_by");
    let mut out = Partitioned { extension: format.to_string(), ..Partitioned::default() };
    let mut rest = copy.clone();
    rest.options.clear();
    rest.values.clear();
    let (mut overwrite, mut ignore, mut append) = (false, false, false);
    let (mut pattern, mut tmp, mut chosen) = (None, false, false);
    for (index, (name, value)) in copy.options.iter().enumerate() {
        let name = name.as_str();
        let here = READ_HERE.contains(&name) || format != "json" && BATCHES.contains(&name);
        if !here {
            rest.options.push((name.to_string(), value.clone()));
            if let Some(&expr) = copy.values.get(index) {
                rest.values.push(expr);
            }
            continue;
        }
        let written = typed.get(index).and_then(Option::as_ref);
        match name {
            "partition_by" => out.columns = columns(value.as_deref(), names)?,
            "write_partition_columns" => out.write_columns = boolean(name, value.as_deref())?,
            // The pin takes one of the three, whether it is on or off.
            "overwrite" | "overwrite_or_ignore" | "append" => {
                if chosen {
                    return Err(Error::binder(
                        "Can only set one of OVERWRITE_OR_IGNORE, OVERWRITE or APPEND",
                    ));
                }
                chosen = true;
                let on = boolean(name, value.as_deref())?;
                match name {
                    "overwrite" => overwrite = on,
                    "overwrite_or_ignore" => ignore = on,
                    _ => append = on,
                }
            }
            "filename_pattern" => pattern = Some(value.clone().unwrap_or_default()),
            "file_extension" => out.extension = value.clone().unwrap_or_default(),
            "hive_file_pattern" => out.flat = !boolean(name, value.as_deref())?,
            // Written at all, even off, is what the pin checks against the options below.
            "use_tmp_file" => {
                boolean(name, value.as_deref())?;
                tmp = true;
            }
            "per_thread_output" => out.per_thread = boolean(name, value.as_deref())?,
            "file_size_bytes" => out.file_size = Some(bytes(value.as_deref(), written)?),
            "write_empty_file" => out.skip_empty = !boolean(name, value.as_deref())?,
            // Only an option that is on counts as set.
            "return_files" | "return_stats" => {
                if flag(name, value.as_deref(), written)? {
                    if out.returns != Returns::Count {
                        return Err(Error::binder(
                            "Can only set one of RETURN_FILES or RETURN_STATS for COPY",
                        ));
                    }
                    out.returns =
                        if name == "return_files" { Returns::Files } else { Returns::Stats };
                }
            }
            _ => out.batches = Some(unsigned(name, value.as_deref(), written)?),
        }
    }
    out.existing = if overwrite {
        Existing::Overwrite
    } else if ignore {
        Existing::Ignore
    } else if append {
        Existing::Append
    } else {
        Existing::Refuse
    };
    out.pattern = match pattern {
        Some(pattern) => pieces(&pattern),
        None if append => vec![NamePiece::Uuid],
        None => vec![NamePiece::Text("data_".to_string()), NamePiece::Offset],
    };
    if append
        && !out.pattern.iter().any(|piece| matches!(piece, NamePiece::Uuid | NamePiece::Uuid7))
    {
        return Err(Error::binder("APPEND mode requires a {uuid} label in filename_pattern"));
    }
    refuse_together(&out, tmp, partitioned)?;
    // The pin writes JSON through its CSV writer, which is the name it gives.
    if out.returns == Returns::Stats && format != "parquet" {
        return Err(Error::not_implemented(
            "RETURN_STATS is not supported for the \"csv\" copy format",
        ));
    }
    if out.returns == Returns::Stats {
        return Err(Error::not_implemented(
            "COPY TO with the option return_stats is not supported yet",
        ));
    }
    if partitioned && !out.write_columns && out.columns.len() == names.len() {
        return Err(Error::not_implemented(
            "No column to write as all columns are specified as partition columns. \
             WRITE_PARTITION_COLUMNS option can be used to write partition columns.",
        ));
    }
    Ok((out, rest))
}

/// Refuses the options that cannot go together, in the pin's order.
fn refuse_together(out: &Partitioned, tmp: bool, partitioned: bool) -> Result<()> {
    let refused = [
        (tmp && out.per_thread, "Can't combine USE_TMP_FILE and PER_THREAD_OUTPUT for COPY"),
        (
            tmp && out.rotates(),
            "Can't combine USE_TMP_FILE and FILE_SIZE_BYTES/BATCHES_PER_FILE for COPY",
        ),
        (tmp && partitioned, "Can't combine USE_TMP_FILE and PARTITIONED BY for COPY"),
        (
            out.per_thread && partitioned,
            "Can't combine PER_THREAD_OUTPUT and PARTITIONED BY for COPY",
        ),
        (
            out.skip_empty && out.per_thread,
            "Can't combine WRITE_EMPTY_FILE false with PER_THREAD_OUTPUT",
        ),
        (out.skip_empty && partitioned, "Can't combine WRITE_EMPTY_FILE false with PARTITIONED BY"),
    ];
    match refused.iter().find(|(refused, _)| *refused) {
        Some((_, message)) => Err(Error::not_implemented(*message)),
        None => Ok(()),
    }
}

/// The size a `FILE_SIZE_BYTES` asks for. Text is a size the way `memory_limit` reads one, and
/// anything else is cast to an unsigned number, which is how the pin reads it.
fn bytes(text: Option<&str>, written: Option<&Written>) -> Result<u64> {
    let Some(text) = text else {
        return Err(Error::binder("FILE_SIZE_BYTES cannot be empty"));
    };
    match written.and_then(|written| written.value.as_ref().map(|value| (&written.ty, value))) {
        Some((ty, value)) if *ty != LogicalType::Varchar => match cast_unsigned(value) {
            Some(size) => Ok(size),
            None => Err(Error::binder(format!(
                "Unable to parse bytes from \"{value}\" for copy option \"FILE_SIZE_BYTES\" "
            ))),
        },
        _ => parse_size(text.trim_matches('\'')),
    }
}

/// The unsigned number an option of that type is written as, in the pin's words when it is not
/// one.
fn unsigned(name: &str, text: Option<&str>, written: Option<&Written>) -> Result<u64> {
    let Some(text) = text else {
        return Err(Error::invalid_input(format!(
            "Copy option \"{name}\" requires an argument of type UBIGINT"
        )));
    };
    let (ty, number, shown) = match written
        .and_then(|written| written.value.as_ref().map(|value| (&written.ty, value)))
    {
        Some((ty, value)) if *ty != LogicalType::Varchar => {
            (ty.clone(), cast_unsigned(value), value.to_string())
        }
        _ => {
            let text = text.trim_matches('\'');
            (LogicalType::Varchar, text.trim().parse().ok(), text.to_string())
        }
    };
    number.ok_or_else(|| {
        Error::invalid_input(format!(
            "Copy option \"{name}\" expected an argument of type UBIGINT - the argument \
             \"{shown}\" of type {ty} could not be cast as this type"
        ))
    })
}

/// An option that is on or off the way the pin reads a `BOOLEAN` copy option, where an option with
/// no value is on and anything else is cast, in the pin's words when it cannot be.
fn flag(name: &str, text: Option<&str>, written: Option<&Written>) -> Result<bool> {
    let Some(text) = text else { return Ok(true) };
    let (ty, value) = match written
        .and_then(|written| written.value.as_ref().map(|value| (&written.ty, value)))
    {
        Some((ty, value)) if *ty != LogicalType::Varchar => (ty.clone(), value.clone()),
        _ => (LogicalType::Varchar, Value::Varchar(text.trim_matches('\'').into())),
    };
    match rudb_kernels::cast_value(&value, &LogicalType::Boolean, true) {
        Ok(Value::Boolean(on)) => Ok(on),
        _ => Err(Error::invalid_input(format!(
            "Copy option \"{name}\" expected an argument of type BOOLEAN - the argument \
             \"{value}\" of type {ty} could not be cast as this type"
        ))),
    }
}

/// A value cast to an unsigned 64 bit number, if it is one.
fn cast_unsigned(value: &Value) -> Option<u64> {
    match rudb_kernels::cast_value(value, &LogicalType::UBigInt, true) {
        Ok(Value::UBigInt(number)) => Some(number),
        _ => None,
    }
}

/// The columns a `PARTITION_BY` names, as a list in parentheses or a single name, each bare or
/// quoted either way, matched without regard to case, where `*` is every column.
fn columns(written: Option<&str>, names: &[String]) -> Result<Vec<usize>> {
    let written = written.unwrap_or_default().trim();
    let list = written.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')).unwrap_or(written);
    let mut out: Vec<usize> = Vec::new();
    for column in split(list) {
        let column = column.trim();
        if column == "*" {
            return Ok((0..names.len()).collect());
        }
        let unquoted = ['"', '\'']
            .iter()
            .find_map(|&quote| column.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)))
            .unwrap_or(column);
        let Some(index) = names.iter().position(|name| name.eq_ignore_ascii_case(unquoted)) else {
            return Err(Error::binder(format!(
                "\"partition_by\" expected to find {unquoted}, but it was not found in the table"
            )));
        };
        if out.contains(&index) {
            return Err(Error::binder(format!(
                "\"partition_by\" does now allow duplicate columns (found: {unquoted})"
            )));
        }
        out.push(index);
    }
    Ok(out)
}

/// Splits a list on the commas that are not inside quotes.
fn split(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quote) = (0, None);
    for (at, character) in list.char_indices() {
        match (character, quote) {
            ('"' | '\'', None) => quote = Some(character),
            (_, Some(open)) if character == open => quote = None,
            (',', None) => {
                out.push(&list[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    out.push(&list[start..]);
    out
}

/// An option that is on or off, where an option with no value is on.
fn boolean(name: &str, written: Option<&str>) -> Result<bool> {
    let Some(written) = written else { return Ok(true) };
    match written.trim_matches('\'').to_ascii_lowercase().as_str() {
        "true" | "t" | "1" | "on" | "y" | "yes" => Ok(true),
        "false" | "f" | "0" | "off" | "n" | "no" => Ok(false),
        _ => Err(Error::binder(format!("\"{name}\" expects a boolean value, not {written}"))),
    }
}

/// The pieces of a `FILENAME_PATTERN`, where a pattern of text alone has the number of the file
/// put after it.
fn pieces(pattern: &str) -> Vec<NamePiece> {
    const LABELS: [(&str, NamePiece); 4] = [
        ("{i}", NamePiece::Offset),
        ("{uuid}", NamePiece::Uuid),
        ("{uuidv4}", NamePiece::Uuid),
        ("{uuidv7}", NamePiece::Uuid7),
    ];
    let mut out = Vec::new();
    let (mut text, mut rest) = (String::new(), pattern);
    while let Some(character) = rest.chars().next() {
        if let Some((label, piece)) = LABELS.iter().find(|(label, _)| rest.starts_with(label)) {
            if !text.is_empty() {
                out.push(NamePiece::Text(std::mem::take(&mut text)));
            }
            out.push(piece.clone());
            rest = &rest[label.len()..];
        } else {
            text.push(character);
            rest = &rest[character.len_utf8()..];
        }
    }
    if !text.is_empty() {
        out.push(NamePiece::Text(text));
    }
    if matches!(out.as_slice(), [NamePiece::Text(_)]) {
        out.push(NamePiece::Offset);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_of_text_alone_is_numbered_and_the_labels_are_read() {
        assert_eq!(pieces("foo"), [NamePiece::Text("foo".into()), NamePiece::Offset]);
        assert_eq!(
            pieces("a{uuid}_{i}{x}"),
            [
                NamePiece::Text("a".into()),
                NamePiece::Uuid,
                NamePiece::Text("_".into()),
                NamePiece::Offset,
                NamePiece::Text("{x}".into()),
            ]
        );
        assert_eq!(pieces("{uuidv7}"), [NamePiece::Uuid7]);
    }

    #[test]
    fn the_columns_are_found_in_any_case_and_any_quotes() {
        let names = ["id".to_string(), "K".to_string(), "a b".to_string()];
        assert_eq!(columns(Some("(k, \"a b\")"), &names).unwrap(), [1, 2]);
        assert_eq!(columns(Some("('id','k')"), &names).unwrap(), [0, 1]);
        assert_eq!(columns(Some("*"), &names).unwrap(), [0, 1, 2]);
        let missing = columns(Some("zz"), &names).unwrap_err().to_string();
        assert!(missing.contains("expected to find zz"), "{missing}");
        let twice = columns(Some("(k, K)"), &names).unwrap_err().to_string();
        assert!(twice.contains("(found: K)"), "{twice}");
    }
}
