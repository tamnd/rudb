//! The columns a file read adds that are in no file: the file's own name, and the `key=value`
//! directories of a Hive partitioned layout.
//!
//! A directory written by `COPY ... (PARTITION_BY (k))` holds `k=1/data.parquet` and
//! `k=2/data.parquet`, and the files have no `k` column, because every row of a file has the same
//! value and the directory already says what it is. Reading the directory back gives `k` back as a
//! column, typed from the values the directory names hold. The `filename` option is the same idea
//! with the whole path as the value.
//!
//! Both are settled in the binder, which knows the file names, and the executor works the values
//! out again per file from the plan's file list. The binder writes what it decided into the plan's
//! options as `hive_partitioning=true` and `filename='<column>'`, so the executor reads a decision
//! rather than repeating the guess.
//!
//! Everything here follows the pin's `HivePartitioning` and `MultiFileOptions`, checked on
//! v2.0.0-dev84237 for the cases the tests name.

use std::collections::BTreeMap;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_kernels::cast_value;

/// What the directories of a Hive partitioned file say a partition value is when it is null, which
/// is what the pin writes for a null key.
const DEFAULT_PARTITION: &str = "__HIVE_DEFAULT_PARTITION__";

/// The `key=value` directories in `path`, by key.
///
/// The pin's parse, step for step. A separator ends a segment, and the segment is a partition when
/// it holds an `=` and only one and no `?` or newline. The last segment is the file and is never
/// one, because nothing after it ends it. A key that is in the path twice keeps the first value,
/// which is what the pin's insert into a map does.
#[must_use]
pub fn partitions(path: &str) -> BTreeMap<String, String> {
    let bytes = path.as_bytes();
    let mut found = BTreeMap::new();
    let mut start = 0;
    let mut equals = 0;
    let mut candidate = true;
    for (at, &byte) in bytes.iter().enumerate() {
        if byte == b'?' || byte == b'\n' {
            candidate = false;
        }
        if byte == b'/' || byte == b'\\' {
            if candidate && equals > start {
                let key = &path[start..equals];
                let value = &path[equals + 1..at];
                found.entry(key.to_string()).or_insert_with(|| value.to_string());
            }
            start = at + 1;
            candidate = true;
        } else if byte == b'=' {
            if equals > start {
                candidate = false;
            }
            equals = at;
        }
    }
    found
}

/// The columns a read adds after the file's own, worked out from its options and its file names.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Extras {
    /// The name of the column holding the path, when `filename` asks for one.
    pub filename: Option<String>,
    /// Each partition key with its type, in key order, when the read is partitioned.
    pub hive: Vec<(String, LogicalType)>,
    /// Whether the read is partitioned, which it can be with no keys at all when every file was
    /// asked for with `hive_partitioning=true` and none of them is under a `key=value` directory.
    pub partitioned: bool,
}

impl Extras {
    /// Whether the read adds no column.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.filename.is_none() && self.hive.is_empty()
    }
}

/// What the `filename` option asks for: a string names the column, and anything else is read as
/// whether to add one called `filename`, with a value that does not cast to a boolean read as no.
#[must_use]
pub fn filename_column(value: &Value) -> Option<String> {
    match value {
        Value::Varchar(name) => Some(name.clone()),
        other => match cast_value(other, &LogicalType::Boolean, true) {
            Ok(Value::Boolean(true)) => Some("filename".to_string()),
            _ => None,
        },
    }
}

/// The extra columns of a read of `paths` with these options.
///
/// `types` is `hive_types` read into types already, since reading a type name needs the catalog.
///
/// # Errors
///
/// The pin's refusals: `hive_types` with `hive_partitioning=false`, files that do not agree on
/// their keys when partitioning was asked for, and a `hive_types` key that is not a partition.
pub fn extras(
    options: &[(&str, Value)],
    paths: &[String],
    types: &[(String, LogicalType)],
) -> Result<Extras> {
    let mut filename = None;
    let mut asked = None;
    let mut autocast = true;
    for (name, value) in options {
        match (*name, value) {
            ("filename", value) => filename = filename_column(value),
            ("hive_partitioning", Value::Boolean(on)) => asked = Some(*on),
            ("hive_types_autocast", Value::Boolean(on)) => autocast = *on,
            _ => {}
        }
    }
    if asked == Some(false) && !types.is_empty() {
        return Err(Error::invalid_input(
            "cannot disable hive_partitioning when hive_types is enabled",
        ));
    }
    let Some(first) = paths.first() else {
        return Ok(Extras { filename, ..Extras::default() });
    };
    let keys = partitions(first);
    let partitioned = match asked {
        Some(on) => on,
        None if !types.is_empty() => true,
        None => {
            !keys.is_empty()
                && paths.iter().all(|path| {
                    let other = partitions(path);
                    other.len() == keys.len() && other.keys().all(|key| keys.contains_key(key))
                })
        }
    };
    if !partitioned {
        return Ok(Extras { filename, ..Extras::default() });
    }
    for path in paths {
        let other = partitions(path);
        if let Some(key) = keys.keys().find(|key| !other.contains_key(*key)) {
            return Err(Error::binder(format!(
                "Hive partition mismatch between file \"{first}\" and \"{path}\": key \"{key}\" \
                 not found"
            )));
        }
        if other.len() != keys.len() {
            return Err(Error::binder(format!(
                "Hive partition mismatch between file \"{first}\" and \"{path}\""
            )));
        }
    }
    if let Some((name, _)) = types.iter().find(|(name, _)| !keys.contains_key(name)) {
        return Err(Error::invalid_input(format!(
            "Unknown hive_type: \"{name}\" does not appear to be a partition"
        )));
    }
    let detected = if autocast { detect(paths, types) } else { BTreeMap::new() };
    let hive = keys
        .into_keys()
        .map(|key| {
            let ty = types
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, ty)| ty.clone())
                .or_else(|| detected.get(&key).cloned())
                .unwrap_or(LogicalType::Varchar);
            (key, ty)
        })
        .collect();
    Ok(Extras { filename, hive, partitioned })
}

/// The type of each partition key that `types` does not set, from the values the paths hold.
///
/// A value is a `DATE` if it reads as one exactly, then a `TIMESTAMP`, then a `BIGINT`, and a
/// `VARCHAR` otherwise. Files that disagree make the key a `VARCHAR`, and a null value has no say.
/// A path with no partitions at all leaves every key a `VARCHAR`, which is the pin giving up on the
/// guess the moment it meets one.
fn detect(paths: &[String], types: &[(String, LogicalType)]) -> BTreeMap<String, LogicalType> {
    let mut detected: BTreeMap<String, LogicalType> = BTreeMap::new();
    for path in paths {
        let found = partitions(path);
        if found.is_empty() {
            return BTreeMap::new();
        }
        for (key, value) in found {
            if types.iter().any(|(name, _)| *name == key) || is_null(&value) {
                continue;
            }
            let ty = guess(&value);
            detected
                .entry(key)
                .and_modify(|seen| {
                    if *seen != ty {
                        *seen = LogicalType::Varchar;
                    }
                })
                .or_insert(ty);
        }
    }
    detected
}

/// Whether a partition value stands for a null when its type is guessed.
fn is_null(value: &str) -> bool {
    value.eq_ignore_ascii_case("NULL") || value == DEFAULT_PARTITION
}

/// The type one partition value reads as, by the pin's strict casts.
///
/// The strictness is spelled out here rather than left to the cast, because the cast is the lenient
/// one SQL gets: it reads `2020-01-01 10:00:00` as a date and `1.5` as an integer, and the pin's
/// guess calls the first a timestamp and the second text.
fn guess(value: &str) -> LogicalType {
    let text = Value::Varchar(value.to_string());
    let casts = |ty: &LogicalType| cast_value(&text, ty, true).is_ok_and(|cast| !cast.is_null());
    let trimmed = value.trim();
    if !trimmed.contains([' ', 'T', 't']) && casts(&LogicalType::Date) {
        return LogicalType::Date;
    }
    if casts(&LogicalType::Timestamp) {
        return LogicalType::Timestamp;
    }
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && casts(&LogicalType::BigInt)
    {
        return LogicalType::BigInt;
    }
    LogicalType::Varchar
}

/// The value of partition `key` written as `text` in a path, in the column's type.
///
/// The pin's rules: the default partition name is a null in any type, a text column takes the value
/// URL decoded, and any other type reads `NULL` and an empty value as a null and casts the rest.
///
/// # Errors
///
/// A value that does not cast, in the pin's words, which name the key in upper case.
pub fn hive_value(key: &str, text: &str, ty: &LogicalType) -> Result<Value> {
    if text == DEFAULT_PARTITION {
        return Ok(Value::Null);
    }
    if *ty == LogicalType::Varchar {
        return Ok(Value::Varchar(url_decode(text)));
    }
    if text.eq_ignore_ascii_case("NULL") || text.is_empty() {
        return Ok(Value::Null);
    }
    let decoded = url_decode(text);
    match cast_value(&Value::Varchar(decoded.clone()), ty, true) {
        Ok(value) if !value.is_null() => Ok(value),
        _ => Err(Error::invalid_input(format!(
            "Unable to cast '{decoded}' (from hive partition column '{}') to: '{ty}'",
            key.to_uppercase()
        ))),
    }
}

/// `text` with each `%` and two hex digits turned into the byte they spell, which is how a value
/// with a slash or an equals sign in it is written into a directory name.
fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |byte: u8| char::from(byte).to_digit(16).and_then(|d| u8::try_from(d).ok());
        if bytes[at] == b'%'
            && at + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2]))
        {
            out.push((high << 4) | low);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Puts the extra columns on the end of a read's `fields`.
///
/// The filename column goes first, then the partition keys in key order. A key that is already a
/// column of the file takes that column over, keeping its place and taking the partition's type,
/// because every row of a file under `k=5/` has a `k` of five whatever the file says.
///
/// Hands back the names of the file's columns a key took over, whose statistics are the file's and
/// no longer say anything about the column.
///
/// # Errors
///
/// A filename column whose name is already a column of the file or a partition key, in the pin's
/// words.
pub fn add_extras(fields: &mut Vec<Field>, extras: &Extras) -> Result<Vec<String>> {
    let mut named = None;
    if let Some(name) = &extras.filename {
        if fields.iter().any(|field| field.name.eq_ignore_ascii_case(name)) {
            return Err(Error::binder(format!(
                "Option filename adds column \"{name}\", but a column with this name is also in \
                 the file. Try setting a different name: filename='<filename column name>'"
            )));
        }
        named = Some(fields.len());
        fields.push(Field::new(name.clone(), LogicalType::Varchar));
    }
    let mut taken = Vec::new();
    for (key, ty) in &extras.hive {
        match fields.iter().position(|field| field.name.eq_ignore_ascii_case(key)) {
            Some(at) if Some(at) == named => {
                return Err(Error::binder(format!(
                    "Option filename adds column \"{key}\", but a hive partition column with this \
                     name also exists. Try setting a different name: filename='<filename column \
                     name>'"
                )));
            }
            Some(at) => {
                fields[at].ty = ty.clone();
                taken.push(fields[at].name.clone());
            }
            None => fields.push(Field::new(key.clone(), ty.clone())),
        }
    }
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partition_is_a_directory_with_one_equals_sign() {
        let found = partitions("/data/k=1/d=2020-01-02/a=b=c/x?y=1/f=g.csv");
        let keys: Vec<(&str, &str)> =
            found.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
        assert_eq!(keys, [("d", "2020-01-02"), ("k", "1")]);
        assert!(partitions("k=1").is_empty());
        assert_eq!(partitions("c:\\k=v\\f.csv").get("k").map(String::as_str), Some("v"));
    }

    #[test]
    fn the_guess_is_a_date_then_a_timestamp_then_an_integer() {
        assert_eq!(guess("2020-01-02"), LogicalType::Date);
        assert_eq!(guess("2020-01-01 10:00:00"), LogicalType::Timestamp);
        assert_eq!(guess("12"), LogicalType::BigInt);
        assert_eq!(guess("1.5"), LogicalType::Varchar);
        assert_eq!(guess("x"), LogicalType::Varchar);
    }

    #[test]
    fn values_follow_the_null_markers_and_decode() {
        let int = LogicalType::Integer;
        assert_eq!(
            hive_value("k", "%41b", &LogicalType::Varchar).unwrap(),
            Value::Varchar("Ab".into())
        );
        assert_eq!(
            hive_value("k", "", &LogicalType::Varchar).unwrap(),
            Value::Varchar(String::new())
        );
        assert_eq!(hive_value("k", "NULL", &int).unwrap(), Value::Null);
        assert_eq!(hive_value("k", DEFAULT_PARTITION, &LogicalType::Varchar).unwrap(), Value::Null);
        let error = hive_value("k", "x", &int).unwrap_err();
        assert!(error.message().contains("hive partition column 'K') to: 'INTEGER'"), "{error}");
    }
}
