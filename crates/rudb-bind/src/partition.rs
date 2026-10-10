//! The options of a `COPY ... TO` that say how the rows are split over files, read before the
//! format sees the rest.
//!
//! `PARTITION_BY` names the columns whose values pick a directory, one level a column, the way
//! the pin's hive writer lays them out. The options around it say what to do with what is
//! already there, what the files are called and whether the partition columns are written too.
//! The checks are made in the pin's order: a column the query does not have and a column named
//! twice as the options are read, then the options that cannot go together, then a partition
//! that leaves nothing to write.

use rudb_common::{Error, Result};
use rudb_parse::ast;

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
const READ_HERE: [&str; 9] = [
    "partition_by",
    "write_partition_columns",
    "overwrite",
    "overwrite_or_ignore",
    "append",
    "filename_pattern",
    "file_extension",
    "hive_file_pattern",
    "use_tmp_file",
];

/// Reads the partitioning options of `copy` against the columns the query has, and answers
/// them with the copy left holding only the options the format reads.
///
/// Without `PARTITION_BY`, the options about the files of a directory have nothing to act on
/// and are taken and dropped, as the pin drops them for one file.
pub(crate) fn partitioning(
    copy: &ast::CopyTo,
    names: &[String],
    format: &str,
) -> Result<(Partitioned, ast::CopyTo)> {
    let partitioned = copy.options.iter().any(|(name, _)| name == "partition_by");
    let mut out = Partitioned { extension: format.to_string(), ..Partitioned::default() };
    let mut rest = copy.clone();
    rest.options.clear();
    rest.values.clear();
    let (mut overwrite, mut ignore, mut append) = (false, false, false);
    let (mut pattern, mut tmp, mut per_thread) = (None, false, false);
    for (index, (name, value)) in copy.options.iter().enumerate() {
        let here = READ_HERE.contains(&name.as_str()) || partitioned && name == "per_thread_output";
        if !here {
            rest.options.push((name.clone(), value.clone()));
            if let Some(&expr) = copy.values.get(index) {
                rest.values.push(expr);
            }
            continue;
        }
        match name.as_str() {
            "partition_by" => out.columns = columns(value.as_deref(), names)?,
            "write_partition_columns" => out.write_columns = boolean(name, value.as_deref())?,
            "overwrite" => overwrite = boolean(name, value.as_deref())?,
            "overwrite_or_ignore" => ignore = boolean(name, value.as_deref())?,
            "append" => append = boolean(name, value.as_deref())?,
            "filename_pattern" => pattern = Some(value.clone().unwrap_or_default()),
            "file_extension" => out.extension = value.clone().unwrap_or_default(),
            "hive_file_pattern" => out.flat = !boolean(name, value.as_deref())?,
            "use_tmp_file" => tmp = boolean(name, value.as_deref())?,
            _ => per_thread = boolean(name, value.as_deref())?,
        }
    }
    if [overwrite, ignore, append].iter().filter(|&&set| set).count() > 1 {
        return Err(Error::binder("Can only set one of OVERWRITE_OR_IGNORE, OVERWRITE or APPEND"));
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
    if !partitioned {
        return Ok((out, rest));
    }
    if tmp {
        return Err(Error::not_implemented(
            "Can't combine USE_TMP_FILE and PARTITIONED BY for COPY",
        ));
    }
    if per_thread {
        return Err(Error::not_implemented(
            "Can't combine PER_THREAD_OUTPUT and PARTITIONED BY for COPY",
        ));
    }
    if !out.write_columns && out.columns.len() == names.len() {
        return Err(Error::not_implemented(
            "No column to write as all columns are specified as partition columns. \
             WRITE_PARTITION_COLUMNS option can be used to write partition columns.",
        ));
    }
    Ok((out, rest))
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
