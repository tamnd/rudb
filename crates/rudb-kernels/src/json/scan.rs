//! `read_json` and its family: reading files of JSON documents as rows.
//!
//! This follows the pin's JSON reader step by step. A file is cut into units the way the pin cuts
//! one, a line at a time when it is newline delimited and a value at a time otherwise, and each
//! unit is read with the reader in the parent module. The columns are worked out the way the pin
//! works them out, by folding a sample of the units into a tree of what was seen where, narrowing
//! the strings in it to the dates, timestamps and UUIDs they all turned out to be, and turning the
//! tree into a type. Reading the rows then takes the units apart into columns the way the pin's
//! transform does, a column at a time, with the same refusals in the same words and the same line
//! numbers.
//!
//! Nothing here touches a file. The binder hands over the text of each file it wants detected and
//! the executor the text of each file it reads, so this module is the same code for both.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_vector::{Chunk, Vector};

use super::{Document, Node, read, read_into, strict_text, type_name};
use crate::cast::{
    cast_value, cast_value_in_time_zone, strict_date, strict_time, timestamp_offset,
};
use crate::strftime::Format as Pattern;
use crate::strptime::{try_date, try_timestamp};

/// The hidden setting the binder writes into the plan to carry a read's [`Settled`] to the
/// executor.
///
/// It is not one of the functions' parameters, so a call cannot write it.
pub const SETTLED: &str = "json_settled";

/// How many units the pin reads into one chunk.
const CHUNK: usize = 2048;

/// The hint the pin adds to a transform error when the columns were detected.
const DETECTED_HINT: &str = "\nTry increasing 'sample_size', reducing 'maximum_depth', specifying \
                             'columns', 'format' or 'records' manually, setting 'ignore_errors' to \
                             true, or setting 'union_by_name' to true when reading multiple files \
                             with a different structure.";

/// The hint the pin adds to a transform error when the columns were given.
const GIVEN_HINT: &str = "\nTry setting 'auto_detect' to true, specifying 'format' or 'records' \
                          manually, or setting 'ignore_errors' to true.";

/// The date formats auto-detection tries, which the pin tries from the back.
const DATE_TEMPLATES: [&str; 6] =
    ["%m-%d-%Y", "%m-%d-%y", "%d-%m-%Y", "%d-%m-%y", "%Y-%m-%d", "%y-%m-%d"];

/// The timestamp formats auto-detection tries, which the pin tries from the back.
const TIMESTAMP_TEMPLATES: [&str; 12] = [
    "%Y-%m-%d %H:%M:%S.%f",
    "%m-%d-%Y %I:%M:%S %p",
    "%m-%d-%y %I:%M:%S %p",
    "%d-%m-%Y %H:%M:%S",
    "%d-%m-%y %H:%M:%S",
    "%Y-%m-%d %H:%M:%S",
    "%y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%dT%H:%M:%SZ",
    "%Y-%m-%dT%H:%M:%S.%fZ",
    "%Y-%m-%dT%H:%M:%S%z",
    "%Y-%m-%dT%H:%M:%S.%f%z",
];

/// Which of the family a call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    /// `read_json` and `read_json_auto`.
    Json,
    /// `read_ndjson` and `read_ndjson_auto`.
    Ndjson,
    /// `read_json_objects` and `read_json_objects_auto`.
    Objects,
    /// `read_ndjson_objects`.
    NdjsonObjects,
    /// `read_single_json_file`, which is `read_json` over exactly one file, without the options
    /// that belong to reading several.
    Single,
}

impl Function {
    /// Whether the call answers each unit as text rather than as columns.
    #[must_use]
    pub const fn objects(self) -> bool {
        matches!(self, Self::Objects | Self::NdjsonObjects)
    }
}

/// How the documents of a file are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Worked out from the start of each file.
    Auto,
    /// Values one after another with anything between them.
    Unstructured,
    /// One value a line.
    Newline,
    /// One array whose elements are the values.
    Array,
}

/// Whether each value is a row of columns or one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Records {
    /// Worked out from what the values turn out to be.
    Auto,
    /// Each value is an object whose keys are the columns.
    Records,
    /// Each value is the one column.
    Values,
}

/// How the bytes of a file are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Worked out from the file's extension, `.gz` and `.zst` being the two the pin knows.
    Auto,
    /// Not compressed, whatever the file is called.
    None,
    /// Gzip.
    Gzip,
    /// Zstandard.
    Zstd,
}

impl Compression {
    /// What `path` is compressed with, given what the call said.
    #[must_use]
    pub fn of(self, path: &str) -> Self {
        match self {
            Self::Auto if path.ends_with(".gz") => Self::Gzip,
            Self::Auto if path.ends_with(".zst") => Self::Zstd,
            Self::Auto => Self::None,
            other => other,
        }
    }
}

/// Whether a call's `allow_empty` is true, read before its files are looked for.
///
/// The pin casts every named parameter before it expands a pattern and checks what the values mean
/// after, so `allow_empty='x'` on a missing file is the cast error and `format='nope'` on one is
/// the missing file. This is the cast, for the one parameter whose value decides the expansion.
///
/// # Errors
///
/// The pin's refusal of a value that does not cast to a boolean.
pub fn allows_empty(written: &[(&str, Value)]) -> Result<bool> {
    let Some((_, value)) =
        written.iter().rev().find(|(name, _)| name.eq_ignore_ascii_case("allow_empty"))
    else {
        return Ok(false);
    };
    if value.is_null() {
        return Ok(false);
    }
    let cast = cast_value(value, &LogicalType::Boolean, false).map_err(|error| {
        Error::invalid_input(format!("Failed to cast value: {}", error.message()))
    })?;
    Ok(matches!(cast, Value::Boolean(true)))
}

/// What a call's named parameters say.
#[derive(Debug, Clone)]
pub struct Options {
    pub function: Function,
    pub format: Format,
    format_specified: bool,
    pub records: Records,
    pub auto_detect: bool,
    auto_detect_specified: bool,
    pub ignore_errors: bool,
    pub maximum_object_size: u64,
    pub compression: Compression,
    /// The name of the column that answers each row's file, when the call asked for one.
    pub filename: Option<String>,
    sample_size: u64,
    maximum_depth: u64,
    field_appearance_threshold: f64,
    map_inference_threshold: u64,
    maximum_sample_files: u64,
    /// Whether every file is sampled and the columns of all of them merged, rather than the first
    /// `maximum_sample_files`.
    union_by_name: bool,
    convert_strings_to_integers: bool,
    date_format: Option<String>,
    timestamp_format: Option<String>,
    columns: Vec<Field>,
}

/// The parameters each function takes in the order the pin's catalog lists them, which is the order
/// of its hash map rather than any order anybody chose. `duckdb_functions()` and the candidate list
/// of a call that matched no overload both print them this way.
#[must_use]
pub const fn listed(function: Function) -> &'static [&'static str] {
    const JSON: &[&str] = &[
        "timestampformat",
        "field_appearance_threshold",
        "timestamp_format",
        "dateformat",
        "records",
        "geojson",
        "sample_size",
        "columns",
        "maximum_depth",
        "compression",
        "filename",
        "union_by_name",
        "auto_detect",
        "hive_partitioning",
        "date_format",
        "map_inference_threshold",
        "hive_types",
        "hive_types_autocast",
        "allow_empty",
        "convert_strings_to_integers",
        "maximum_sample_files",
        "maximum_object_size",
        "ignore_errors",
        "format",
        "array",
    ];
    const OBJECTS: &[&str] = &[
        "maximum_object_size",
        "format",
        "ignore_errors",
        "allow_empty",
        "hive_types_autocast",
        "hive_types",
        "hive_partitioning",
        "compression",
        "union_by_name",
        "filename",
    ];
    const SINGLE: &[&str] = &[
        "convert_strings_to_integers",
        "maximum_sample_files",
        "maximum_object_size",
        "array",
        "format",
        "ignore_errors",
        "map_inference_threshold",
        "date_format",
        "compression",
        "maximum_depth",
        "columns",
        "sample_size",
        "auto_detect",
        "geojson",
        "records",
        "dateformat",
        "timestamp_format",
        "field_appearance_threshold",
        "timestampformat",
    ];
    match function {
        Function::Objects | Function::NdjsonObjects => OBJECTS,
        Function::Single => SINGLE,
        Function::Json | Function::Ndjson => JSON,
    }
}

/// The parameters each function takes, and the type each is read as.
#[must_use]
pub fn parameters(function: Function) -> &'static [(&'static str, LogicalType)] {
    const JSON: &[(&str, LogicalType)] = &[
        ("allow_empty", LogicalType::Boolean),
        ("array", LogicalType::Boolean),
        ("auto_detect", LogicalType::Boolean),
        ("columns", LogicalType::Null),
        ("compression", LogicalType::Varchar),
        ("convert_strings_to_integers", LogicalType::Boolean),
        ("date_format", LogicalType::Varchar),
        ("dateformat", LogicalType::Varchar),
        ("field_appearance_threshold", LogicalType::Double),
        ("filename", LogicalType::Null),
        ("format", LogicalType::Varchar),
        ("geojson", LogicalType::Boolean),
        ("hive_partitioning", LogicalType::Boolean),
        ("hive_types", LogicalType::Null),
        ("hive_types_autocast", LogicalType::Boolean),
        ("ignore_errors", LogicalType::Boolean),
        ("map_inference_threshold", LogicalType::BigInt),
        ("maximum_depth", LogicalType::BigInt),
        ("maximum_object_size", LogicalType::UInteger),
        ("maximum_sample_files", LogicalType::BigInt),
        ("records", LogicalType::Varchar),
        ("sample_size", LogicalType::BigInt),
        ("timestamp_format", LogicalType::Varchar),
        ("timestampformat", LogicalType::Varchar),
        ("union_by_name", LogicalType::Boolean),
    ];
    const OBJECTS: &[(&str, LogicalType)] = &[
        ("allow_empty", LogicalType::Boolean),
        ("compression", LogicalType::Varchar),
        ("filename", LogicalType::Null),
        ("format", LogicalType::Varchar),
        ("hive_partitioning", LogicalType::Boolean),
        ("hive_types", LogicalType::Null),
        ("hive_types_autocast", LogicalType::Boolean),
        ("ignore_errors", LogicalType::Boolean),
        ("maximum_object_size", LogicalType::UInteger),
        ("union_by_name", LogicalType::Boolean),
    ];
    // The options of one file, which leaves out the six that say how several files come together.
    const SINGLE: &[(&str, LogicalType)] = &[
        ("array", LogicalType::Boolean),
        ("auto_detect", LogicalType::Boolean),
        ("columns", LogicalType::Null),
        ("compression", LogicalType::Varchar),
        ("convert_strings_to_integers", LogicalType::Boolean),
        ("date_format", LogicalType::Varchar),
        ("dateformat", LogicalType::Varchar),
        ("field_appearance_threshold", LogicalType::Double),
        ("format", LogicalType::Varchar),
        ("geojson", LogicalType::Boolean),
        ("ignore_errors", LogicalType::Boolean),
        ("map_inference_threshold", LogicalType::BigInt),
        ("maximum_depth", LogicalType::BigInt),
        ("maximum_object_size", LogicalType::UInteger),
        ("maximum_sample_files", LogicalType::BigInt),
        ("records", LogicalType::Varchar),
        ("sample_size", LogicalType::BigInt),
        ("timestamp_format", LogicalType::Varchar),
        ("timestampformat", LogicalType::Varchar),
    ];
    match function {
        Function::Objects | Function::NdjsonObjects => OBJECTS,
        Function::Single => SINGLE,
        Function::Json | Function::Ndjson => JSON,
    }
}

/// The name the pin's candidate list gives a parameter's type, where `ANY` is written as null.
fn parameter_type_name(ty: &LogicalType) -> String {
    if *ty == LogicalType::Null { "ANY".to_string() } else { ty.to_string() }
}

/// The pin's refusal of a named parameter a function does not take.
fn unknown_parameter(function: Function, name: &str) -> Error {
    let called = match function {
        Function::Objects | Function::NdjsonObjects => "read_json_objects",
        Function::Single => "read_single_json_file",
        Function::Json | Function::Ndjson => "read_json",
    };
    let candidates: Vec<String> = parameters(function)
        .iter()
        .map(|(name, ty)| format!("    {name} {}", parameter_type_name(ty)))
        .collect();
    Error::binder(format!(
        "Invalid named parameter \"{name}\" for function {called}\nCandidates:\n{}",
        candidates.join("\n")
    ))
}

/// A type name a `columns` struct gives, read the way the binder reads one.
pub type Resolve<'r> = &'r mut dyn FnMut(&str) -> Result<LogicalType>;

impl Options {
    /// The options a call starts from before its named parameters.
    #[must_use]
    pub fn new(function: Function) -> Self {
        let objects = function.objects();
        Self {
            function,
            format: match function {
                Function::Ndjson | Function::NdjsonObjects => Format::Newline,
                Function::Json | Function::Objects | Function::Single => Format::Auto,
            },
            format_specified: false,
            records: if objects { Records::Records } else { Records::Auto },
            auto_detect: !objects,
            auto_detect_specified: false,
            ignore_errors: false,
            maximum_object_size: 16_777_216,
            compression: Compression::Auto,
            filename: None,
            sample_size: 20_480,
            maximum_depth: u64::MAX,
            field_appearance_threshold: 0.1,
            map_inference_threshold: 200,
            maximum_sample_files: 32,
            union_by_name: false,
            convert_strings_to_integers: false,
            date_format: None,
            timestamp_format: None,
            columns: if objects { vec![Field::new("json", LogicalType::Json)] } else { Vec::new() },
        }
    }

    /// The options a call's named parameters say, read in the order they were written.
    ///
    /// # Errors
    ///
    /// The pin's refusals of a parameter the function does not take, a null, a value that does not
    /// cast to the parameter's type, and a value out of the parameter's range.
    pub fn parse(
        function: Function,
        written: &[(&str, Value)],
        resolve: Option<Resolve<'_>>,
    ) -> Result<Self> {
        let mut options = Self::new(function);
        let mut resolve = resolve;
        for (name, value) in written {
            if *name == SETTLED {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            let Some((parameter, ty)) =
                parameters(function).iter().find(|(parameter, _)| *parameter == lower)
            else {
                return Err(unknown_parameter(function, name));
            };
            if value.is_null() && *parameter == "filename" {
                return Err(Error::invalid_input("Cannot use NULL as argument for \"filename\""));
            }
            if value.is_null() {
                return Err(Error::binder(format!(
                    "Cannot use NULL as argument to key \"{parameter}\""
                )));
            }
            let value = if *ty == LogicalType::Null {
                value.clone()
            } else {
                cast_value(value, ty, false).map_err(|error| {
                    Error::invalid_input(format!("Failed to cast value: {}", error.message()))
                })?
            };
            options.set(parameter, &value, &mut resolve)?;
        }
        if options.auto_detect_specified && !options.auto_detect && !options.format_specified {
            options.format = Format::Newline;
        }
        Ok(options)
    }

    #[allow(clippy::too_many_lines)]
    fn set(&mut self, name: &str, value: &Value, resolve: &mut Option<Resolve<'_>>) -> Result<()> {
        let text = || match value {
            Value::Varchar(text) => text.clone(),
            other => other.to_string(),
        };
        let flag = || matches!(value, Value::Boolean(true));
        let number = || match value {
            Value::BigInt(number) => *number,
            Value::UInteger(number) => i64::from(*number),
            _ => 0,
        };
        match name {
            "format" => {
                let format = text().to_ascii_lowercase();
                self.format = match format.as_str() {
                    "auto" => Format::Auto,
                    "unstructured" => Format::Unstructured,
                    "newline_delimited" | "nd" => Format::Newline,
                    "array" => Format::Array,
                    _ => {
                        return Err(Error::binder(format!(
                            "format must be one of ['nd', 'array', 'newline_delimited', \
                             'unstructured', 'auto'], not '{}'",
                            text()
                        )));
                    }
                };
                self.format_specified = true;
            }
            "array" => {
                self.format = if flag() { Format::Array } else { Format::Newline };
                self.format_specified = true;
            }
            "ignore_errors" => self.ignore_errors = flag(),
            "maximum_object_size" => {
                self.maximum_object_size = u64::try_from(number()).unwrap_or_default();
            }
            "auto_detect" => {
                self.auto_detect = flag();
                self.auto_detect_specified = true;
            }
            "sample_size" => {
                let size = number();
                self.sample_size = match size {
                    -1 => u64::MAX,
                    size if size > 0 => size.unsigned_abs(),
                    _ => {
                        return Err(Error::binder(
                            "read_json \"sample_size\" parameter must be positive, or -1 to sample \
                             all input files entirely, up to \"maximum_sample_files\" files.",
                        ));
                    }
                };
            }
            "maximum_depth" => {
                let depth = number();
                self.maximum_depth = if depth < 0 { u64::MAX } else { depth.unsigned_abs() };
            }
            "field_appearance_threshold" => {
                let Value::Double(threshold) = value else { return Ok(()) };
                if !(0.0..=1.0).contains(threshold) {
                    return Err(Error::binder(
                        "read_json_auto \"field_appearance_threshold\" parameter must be between 0 \
                         and 1",
                    ));
                }
                self.field_appearance_threshold = *threshold;
            }
            "map_inference_threshold" => {
                let threshold = number();
                self.map_inference_threshold = match threshold {
                    -1 => u64::MAX,
                    threshold if threshold >= 0 => threshold.unsigned_abs(),
                    _ => {
                        return Err(Error::binder(
                            "read_json_auto \"map_inference_threshold\" parameter must be 0 or \
                             positive, or -1 to disable map inference for consistent objects.",
                        ));
                    }
                };
            }
            "maximum_sample_files" => {
                let files = number();
                self.maximum_sample_files = match files {
                    -1 => u64::MAX,
                    files if files > 0 => files.unsigned_abs(),
                    _ => {
                        return Err(Error::binder(
                            "\"maximum_sample_files\" parameter must be positive, or -1 to remove \
                             the limit on the number of files used to determine the schema.",
                        ));
                    }
                };
            }
            "convert_strings_to_integers" => self.convert_strings_to_integers = flag(),
            "dateformat" | "date_format" => {
                let format = text();
                let format = if format.eq_ignore_ascii_case("iso") {
                    "%Y-%m-%d".to_string()
                } else {
                    format
                };
                if let Err(why) = Pattern::parsed(&format) {
                    return Err(Error::binder(format!(
                        "read_json could not parse \"dateformat\": '{why}'."
                    )));
                }
                self.date_format = Some(format);
            }
            "timestampformat" | "timestamp_format" => {
                let format = text();
                let format = if format.eq_ignore_ascii_case("iso") {
                    "%Y-%m-%dT%H:%M:%S.%fZ".to_string()
                } else {
                    format
                };
                if let Err(why) = Pattern::parsed(&format) {
                    return Err(Error::binder(format!(
                        "read_json could not parse \"timestampformat\": '{why}'."
                    )));
                }
                self.timestamp_format = Some(format);
            }
            "records" => {
                self.records = match text().as_str() {
                    "auto" => Records::Auto,
                    "true" => Records::Records,
                    "false" => Records::Values,
                    _ => {
                        return Err(Error::binder(
                            "read_json requires \"records\" to be one of ['auto', 'true', 'false'].",
                        ));
                    }
                };
            }
            "columns" => self.columns = columns(value, resolve)?,
            "compression" => {
                let given = text();
                self.compression = match given.to_ascii_lowercase().as_str() {
                    "auto" | "auto_detect" | "infer" => Compression::Auto,
                    "none" | "uncompressed" | "" => Compression::None,
                    "gzip" => Compression::Gzip,
                    "zstd" => Compression::Zstd,
                    _ => {
                        return Err(Error::not_implemented(format!(
                            "Attempting to open a compressed file, but the compression type is \
                             not supported (compression type \"{given}\")"
                        )));
                    }
                };
            }
            "filename" => {
                // A string names the column and anything else is read as whether to add it, with
                // a value that does not cast to a boolean read as no.
                self.filename = match value {
                    Value::Varchar(name) => Some(name.clone()),
                    other => match cast_value(other, &LogicalType::Boolean, false) {
                        Ok(Value::Boolean(true)) => Some("filename".to_string()),
                        _ => None,
                    },
                };
            }
            "union_by_name" => self.union_by_name = flag(),
            // Read by the binder through [`allows_empty`] before it expands the patterns, which is
            // the only place it changes anything.
            "allow_empty" => {}
            "geojson" | "hive_partitioning" | "hive_types" | "hive_types_autocast" => {
                return Err(Error::not_implemented(format!(
                    "read_json does not take \"{name}\" yet"
                )));
            }
            _ => return Err(unknown_parameter(self.function, name)),
        }
        Ok(())
    }

    /// The date formats and timestamp formats the call reads with, and whether auto-detection's
    /// templates are among them.
    fn formats(&self, templates: bool) -> Formats {
        let read = |given: &Option<String>, defaults: &[&str]| -> Vec<Pattern> {
            match given {
                Some(format) => Pattern::parsed(format).into_iter().collect(),
                None if templates => {
                    defaults.iter().filter_map(|format| Pattern::parsed(format).ok()).collect()
                }
                None => Vec::new(),
            }
        };
        Formats {
            dates: read(&self.date_format, &DATE_TEMPLATES),
            stamps: read(&self.timestamp_format, &TIMESTAMP_TEMPLATES),
        }
    }
}

/// The columns a `columns` struct names, in order.
fn columns(value: &Value, resolve: &mut Option<Resolve<'_>>) -> Result<Vec<Field>> {
    let Value::Struct(children) = value else {
        return Err(Error::binder("read_json \"columns\" parameter requires a struct as input."));
    };
    let mut fields = Vec::with_capacity(children.len());
    for (name, child) in children {
        let written = match child {
            Value::Null => {
                return Err(Error::binder(
                    "read_json \"columns\" parameter type specification cannot be NULL.",
                ));
            }
            Value::Varchar(written) => written,
            _ => {
                return Err(Error::binder(
                    "read_json \"columns\" parameter type specification must be VARCHAR.",
                ));
            }
        };
        let ty = match resolve {
            Some(resolve) => resolve(written)?,
            None => LogicalType::Json,
        };
        fields.push(Field::new(name.clone(), ty));
    }
    if fields.is_empty() {
        return Err(Error::binder("read_json \"columns\" parameter needs at least one column."));
    }
    Ok(fields)
}

/// The formats dates and timestamps are read with, each list tried from the back.
#[derive(Clone, Debug, Default)]
struct Formats {
    dates: Vec<Pattern>,
    stamps: Vec<Pattern>,
}

impl Formats {
    fn of(&self, ty: &LogicalType) -> &[Pattern] {
        match ty {
            LogicalType::Date => &self.dates,
            LogicalType::Timestamp => &self.stamps,
            _ => &[],
        }
    }
}

/// One format reading one string as a date or a timestamp.
fn try_format(format: &Pattern, ty: &LogicalType, text: &str) -> Option<Value> {
    match ty {
        LogicalType::Date => try_date(format, text).map(Value::Date),
        _ => try_timestamp(format, text).map(Value::Timestamp),
    }
}

/// The six characters the pin counts as space between units.
const fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn skip(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && space(bytes[at]) {
        at += 1;
    }
    at
}

/// The pin's `NextJSON`: where the value starting at `start` ends, or `None` when it runs to the
/// end of the text. A container or a string ends past its closing character, counting brackets
/// and stepping over strings, and anything else ends at the next comma or closing bracket.
fn next_json(bytes: &[u8], start: usize) -> Option<usize> {
    let end = bytes.len();
    let mut at = start;
    match bytes[start] {
        b'{' | b'[' | b'"' => {
            let mut parents: i64 = 0;
            while at != end {
                let byte = bytes[at];
                at += 1;
                match byte {
                    b'{' | b'[' => {
                        parents += 1;
                        continue;
                    }
                    b'}' | b']' => parents -= 1,
                    b'"' => {
                        while at != end {
                            let inner = bytes[at];
                            at += 1;
                            if inner == b'"' {
                                break;
                            }
                            if inner == b'\\' && at != end {
                                at += 1;
                            }
                        }
                    }
                    _ => continue,
                }
                if parents == 0 {
                    break;
                }
            }
        }
        _ => {
            while at != end {
                if matches!(bytes[at], b',' | b']') {
                    break;
                }
                at += 1;
            }
        }
    }
    (at != end).then_some(at)
}

/// The pin's guess at a file's format and record type from its start.
fn detect_format(text: &str) -> (Format, Records) {
    let bytes = text.as_bytes();
    let first = |nodes: &[Node]| match nodes.first() {
        Some(Node::Array(children)) => {
            children.first().is_none_or(|child| matches!(nodes[*child], Node::Object(_)))
        }
        _ => false,
    };
    if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
        let line = skip(bytes, newline);
        let mut nodes = Vec::new();
        if read_into(&text[..line], &mut nodes, false).is_ok() {
            return match nodes[0] {
                Node::Array(_) if line == bytes.len() => {
                    (Format::Array, if first(&nodes) { Records::Records } else { Records::Values })
                }
                Node::Object(_) => (Format::Newline, Records::Records),
                _ => (Format::Newline, Records::Values),
            };
        }
    }
    let mut at = skip(bytes, 0);
    if at == bytes.len() || bytes[at] == b'{' {
        return (Format::Unstructured, Records::Records);
    }
    if bytes[at] != b'[' {
        return (Format::Unstructured, Records::Values);
    }
    let mut nodes = Vec::new();
    if let Ok((_, size)) = read_into(&text[at..], &mut nodes, true) {
        at = skip(bytes, at + size);
        if at != bytes.len() {
            return (Format::Unstructured, Records::Values);
        }
        return (Format::Array, if first(&nodes) { Records::Records } else { Records::Values });
    }
    at = skip(bytes, at + 1);
    if at == bytes.len() || bytes[at] == b'{' {
        return (Format::Array, Records::Records);
    }
    (Format::Array, Records::Values)
}

/// One file being cut into units.
#[derive(Debug)]
pub struct Units {
    file: String,
    text: Arc<str>,
    objects: bool,
    format: Format,
    ignore: bool,
    maximum_object_size: u64,
    at: usize,
    lines: usize,
    begun: bool,
}

/// Up to [`CHUNK`] units read out of a file.
#[derive(Debug)]
pub struct Batch {
    document: Document,
    /// The root of each unit, or `None` for one whose error was ignored.
    roots: Vec<Option<usize>>,
    /// The trimmed text of each unit, by byte range, for the functions that answer it.
    units: Vec<(usize, usize)>,
    /// How many units of the file came before this batch.
    first: usize,
}

impl Batch {
    /// How many units the batch holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    /// Whether the batch holds no units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

impl Units {
    /// Starts on a file, working out its format first when the call left it to be worked out.
    #[must_use]
    pub fn new(file: &str, text: Arc<str>, options: &Options) -> Self {
        let format = match options.format {
            Format::Auto if text.is_empty() => Format::Newline,
            Format::Auto => detect_format(&text).0,
            format => format,
        };
        Self {
            file: file.to_string(),
            text,
            objects: options.function.objects(),
            format,
            ignore: options.ignore_errors,
            maximum_object_size: options.maximum_object_size,
            at: 0,
            lines: 0,
            begun: false,
        }
    }

    /// The word the pin's errors use for a unit of this file.
    fn unit(&self) -> &'static str {
        if self.format == Format::Newline { "line" } else { "record/value" }
    }

    fn parse_error(&self, at: usize, message: &str, extra: &str) -> Error {
        Error::invalid_input(format!(
            "Malformed JSON in file \"{}\", at byte {} in {} {}: {message}. {extra}",
            self.file,
            at + 1,
            self.unit(),
            self.lines + 2
        ))
    }

    fn skip_array_start(&mut self) -> Result<()> {
        let text = Arc::clone(&self.text);
        let bytes = text.as_bytes();
        self.at = skip(bytes, self.at);
        if self.at == bytes.len() {
            return Ok(());
        }
        if bytes[self.at] != b'[' {
            let first = self.text[self.at..].chars().next().unwrap_or_default();
            return Err(Error::invalid_input(format!(
                "Expected top-level JSON array with format='array', but first character is '{first}' \
                 in file \"{}\".\n Try setting format='auto' or format='newline_delimited'.",
                self.file
            )));
        }
        self.at = skip(bytes, self.at + 1);
        if self.at >= bytes.len() {
            return Err(Error::invalid_input(format!(
                "Missing closing brace ']' in JSON array with format='array' in file \"{}\"",
                self.file
            )));
        }
        if bytes[self.at] == b']' {
            self.at = skip(bytes, self.at + 1);
            if self.at != bytes.len() {
                return Err(Error::invalid_input(format!(
                    "Empty array with trailing data when parsing JSON array with format='array' in \
                     file \"{}\"",
                    self.file
                )));
            }
        }
        Ok(())
    }

    /// The next batch of units, empty at the end of the file.
    ///
    /// # Errors
    ///
    /// The pin's refusals of a unit that is not a document, of one too big, and of an array that
    /// is not written as one.
    pub fn next_batch(&mut self) -> Result<Batch> {
        if !self.begun {
            self.begun = true;
            if self.format == Format::Array {
                self.skip_array_start()?;
            }
        }
        let text = Arc::clone(&self.text);
        let bytes = text.as_bytes();
        let mut batch = Batch {
            document: Document { nodes: Vec::new() },
            roots: Vec::new(),
            units: Vec::new(),
            first: self.lines,
        };
        while batch.roots.len() < CHUNK {
            self.at = skip(bytes, self.at);
            let start = self.at;
            let remaining = bytes.len() - start;
            if remaining == 0 {
                break;
            }
            let end = if self.format == Format::Newline {
                bytes[start..].iter().position(|byte| *byte == b'\n').map(|at| start + at)
            } else {
                next_json(bytes, start)
            };
            let end = match end {
                Some(end) => end,
                None => {
                    if remaining as u64 > self.maximum_object_size {
                        return Err(Error::invalid_input(format!(
                            "\"maximum_object_size\" of {} bytes exceeded while reading file \"{}\" \
                             (>{remaining} bytes).\n Try increasing \"maximum_object_size\".",
                            self.maximum_object_size, self.file
                        )));
                    }
                    bytes.len()
                }
            };
            let size = end - start;
            let root = self.parse(&mut batch.document.nodes, start, size)?;
            batch.roots.push(root);
            if self.objects {
                let mut from = start;
                let mut to = end;
                while from < to && space(bytes[from]) {
                    from += 1;
                }
                while to > from && space(bytes[to - 1]) {
                    to -= 1;
                }
                batch.units.push((from, to));
            }
            self.at = end;
            if self.format == Format::Array {
                self.at = skip(bytes, self.at);
                match bytes.get(self.at) {
                    Some(b',' | b']') => self.at += 1,
                    _ => return Err(self.parse_error(size, "unexpected character", "")),
                }
            }
            self.at = skip(bytes, self.at);
        }
        Ok(batch)
    }

    /// The pin's `ParseJSON` for one unit, which is the root it read or `None` for an error that
    /// was ignored.
    fn parse(&mut self, nodes: &mut Vec<Node>, start: usize, size: usize) -> Result<Option<usize>> {
        let input =
            if self.objects { &self.text[start..start + size] } else { &self.text[start..] };
        let base = nodes.len();
        let mut root = None;
        let mut read = 0;
        match read_into(input, nodes, true) {
            Ok((at, taken)) => {
                root = Some(at);
                read = taken;
            }
            Err(malformed) => {
                if !(self.ignore && self.format == Format::Newline) {
                    let extra = if self.ignore && self.format != Format::Newline {
                        "Parse errors cannot be ignored for JSON formats other than \
                         'newline_delimited'"
                    } else {
                        ""
                    };
                    return Err(self.parse_error(malformed.at, malformed.message, extra));
                }
            }
        }
        if read > size {
            return Err(self.parse_error(
                size,
                "unexpected end of data",
                "Try auto-detecting the JSON format",
            ));
        }
        if read < size {
            let bytes = self.text.as_bytes();
            let after = skip(&bytes[..start + size], start + read);
            if after != start + size {
                if !self.ignore {
                    return Err(self.parse_error(
                        read,
                        "unexpected content after document",
                        "Try auto-detecting the JSON format",
                    ));
                }
                nodes.truncate(base);
                root = None;
            }
        }
        self.lines += 1;
        Ok(root)
    }
}

/// The type of a value as the structure tree records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Null,
    Boolean,
    BigInt,
    UBigInt,
    HugeInt,
    Double,
    Varchar,
    List,
    Struct,
}

impl Kind {
    const fn numeric(self) -> bool {
        matches!(self, Self::Double | Self::UBigInt | Self::BigInt | Self::HugeInt)
    }

    /// The pin's `MaxNumericType`, for two numeric kinds that differ.
    const fn widest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Double, _) | (_, Self::Double) => Self::Double,
            (Self::HugeInt, _) | (_, Self::HugeInt) => Self::HugeInt,
            (Self::BigInt, Self::UBigInt) | (Self::UBigInt, Self::BigInt) => Self::HugeInt,
            _ => Self::BigInt,
        }
    }
}

/// A type a string may turn out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Candidate {
    Uuid,
    BigInt,
    Timestamp,
    TimestampTz,
    Date,
    Time,
}

impl Candidate {
    fn ty(self) -> LogicalType {
        match self {
            Self::Uuid => LogicalType::Uuid,
            Self::BigInt => LogicalType::BigInt,
            Self::Timestamp => LogicalType::Timestamp,
            Self::TimestampTz => LogicalType::TimestampTz,
            Self::Date => LogicalType::Date,
            Self::Time => LogicalType::Time,
        }
    }
}

/// Whether the timestamps among a column's strings carry an offset, which the pin keeps across
/// chunks and across files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Offsets {
    #[default]
    Unknown,
    With,
    Without,
    Mixed,
}

impl Offsets {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unknown, other) => other,
            (this, Self::Unknown) => this,
            (this, other) if this != other => Self::Mixed,
            (this, _) => this,
        }
    }
}

/// The pin's `JSONStructureNode`: everything seen at one place in the documents.
#[derive(Debug, Clone, Default)]
struct Tree {
    key: String,
    initialized: bool,
    descriptions: Vec<Description>,
    count: u64,
    nulls: u64,
}

/// The pin's `JSONStructureDescription`: one type seen at a place.
#[derive(Debug, Clone)]
struct Description {
    kind: Kind,
    keys: HashMap<String, usize>,
    children: Vec<Tree>,
    candidates: Vec<Candidate>,
    large: bool,
    offsets: Offsets,
}

impl Description {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            keys: HashMap::new(),
            children: Vec::new(),
            candidates: Vec::new(),
            large: false,
            offsets: Offsets::Unknown,
        }
    }

    fn element(&mut self) -> &mut Tree {
        if self.children.is_empty() {
            self.children.push(Tree::default());
        }
        &mut self.children[0]
    }

    fn child(&mut self, key: &str) -> &mut Tree {
        let at = match self.keys.get(key) {
            Some(at) => *at,
            None => {
                self.children.push(Tree { key: key.to_string(), ..Tree::default() });
                self.keys.insert(key.to_string(), self.children.len() - 1);
                self.children.len() - 1
            }
        };
        &mut self.children[at]
    }
}

/// What the structure is turned into a type with.
#[derive(Clone, Copy)]
struct Limits {
    depth: u64,
    threshold: f64,
    maps: u64,
}

impl Tree {
    fn description(&mut self, kind: Kind) -> &mut Description {
        if self.descriptions.is_empty() {
            self.descriptions.push(Description::new(kind));
            return &mut self.descriptions[0];
        }
        if self.descriptions.len() == 1 && self.descriptions[0].kind == Kind::Null {
            self.descriptions[0].kind = kind;
            return &mut self.descriptions[0];
        }
        if kind == Kind::Null {
            return self.descriptions.last_mut().expect("not empty");
        }
        let numeric = kind.numeric();
        let found = self.descriptions.iter().position(|description| {
            description.kind == kind || (numeric && description.kind.numeric())
        });
        if let Some(at) = found {
            let description = &mut self.descriptions[at];
            if description.kind != kind {
                description.kind = kind.widest(description.kind);
            }
            return description;
        }
        self.descriptions.push(Description::new(kind));
        self.descriptions.last_mut().expect("just pushed")
    }

    /// The pin's `ExtractStructure` with errors ignored, which is how detection calls it.
    fn extract(&mut self, document: &Document, at: usize) {
        self.count += 1;
        match &document.nodes[at] {
            Node::Null => {
                self.nulls += 1;
                self.description(Kind::Null);
            }
            Node::Array(children) => {
                let child = self.description(Kind::List).element();
                for element in children {
                    child.extract(document, *element);
                }
            }
            Node::Object(children) => {
                let description = self.description(Kind::Struct);
                for (key, value) in children {
                    description.child(key).extract(document, *value);
                }
            }
            Node::Bool(_) => {
                self.description(Kind::Boolean);
            }
            Node::Unsigned(number) => {
                let description = self.description(Kind::UBigInt);
                if *number > i64::MAX as u64 {
                    description.large = true;
                }
            }
            Node::Signed(_) => {
                self.description(Kind::BigInt);
            }
            Node::Real(..) | Node::Raw(_) => {
                self.description(Kind::Double);
            }
            Node::Str(_) => {
                self.description(Kind::Varchar);
            }
        }
    }

    fn contains_varchar(&self) -> bool {
        let [description] = &self.descriptions[..] else { return false };
        description.kind == Kind::Varchar || description.children.iter().any(Self::contains_varchar)
    }

    fn initialize_candidates(&mut self, limit: u64, integers: bool, given: bool, depth: u64) {
        if depth >= limit || self.descriptions.len() != 1 {
            return;
        }
        let initialized = self.initialized;
        let description = &mut self.descriptions[0];
        if description.kind == Kind::Varchar && !initialized {
            use Candidate::{BigInt, Date, Time, Timestamp, TimestampTz, Uuid};
            description.candidates = match (integers, given) {
                (true, true) => vec![Uuid, BigInt, Timestamp, Date, Time],
                (true, false) => vec![Uuid, BigInt, Timestamp, TimestampTz, Date, Time],
                (false, true) => vec![Uuid, Timestamp, Date, Time],
                (false, false) => vec![Uuid, Timestamp, TimestampTz, Date, Time],
            };
            self.initialized = true;
        } else {
            for child in &mut description.children {
                child.initialize_candidates(limit, integers, given, depth + 1);
            }
        }
    }

    fn refine(
        &mut self,
        document: &Document,
        values: &[Option<usize>],
        formats: &Formats,
        given: bool,
    ) {
        if self.descriptions.len() != 1 || !self.contains_varchar() {
            return;
        }
        let description = &mut self.descriptions[0];
        match description.kind {
            Kind::List => {
                let mut elements = Vec::new();
                for value in values.iter().flatten() {
                    if let Node::Array(children) = &document.nodes[*value] {
                        elements.extend(children.iter().map(|child| Some(*child)));
                    }
                }
                description.element().refine(document, &elements, formats, given);
            }
            Kind::Struct => {
                let count = description.children.len();
                let mut columns = vec![vec![None; values.len()]; count];
                for (row, value) in values.iter().enumerate() {
                    let Some(value) = value else { continue };
                    let Node::Object(children) = &document.nodes[*value] else { continue };
                    for (key, child) in children {
                        if let Some(at) = description.keys.get(key) {
                            columns[*at][row] = Some(*child);
                        }
                    }
                }
                for (child, column) in description.children.iter_mut().zip(&columns) {
                    child.refine(document, column, formats, given);
                }
            }
            Kind::Varchar => {
                if description.candidates.is_empty() {
                    return;
                }
                let strings: Vec<Option<&str>> = values
                    .iter()
                    .map(|value| match value.map(|at| &document.nodes[at]) {
                        Some(Node::Str(text)) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                description.eliminate(&strings, formats, given);
            }
            _ => {}
        }
    }

    /// The pin's `MergeNodes`, folding `node` into `self`.
    fn merge(&mut self, node: &Self) {
        self.count += node.count;
        self.nulls += node.nulls;
        for description in &node.descriptions {
            match description.kind {
                Kind::List => {
                    let merged = self.description(Kind::List).element();
                    for child in &description.children {
                        merged.merge(child);
                    }
                }
                Kind::Struct => {
                    let merged = self.description(Kind::Struct);
                    for child in &description.children {
                        merged.child(&child.key).merge(child);
                    }
                }
                kind => {
                    let single = {
                        let merged = self.description(kind);
                        if description.large {
                            merged.large = true;
                        }
                        merged.kind == Kind::Varchar
                    };
                    if !single || !node.initialized || self.descriptions.len() != 1 {
                        continue;
                    }
                    let initialized = self.initialized;
                    let merged = &mut self.descriptions[0];
                    merged.offsets = merged.offsets.merge(description.offsets);
                    if merged.offsets == Offsets::Mixed {
                        merged.candidates.clear();
                        self.initialized = true;
                        continue;
                    }
                    if !initialized {
                        merged.candidates.clone_from(&description.candidates);
                    } else if merged.candidates.is_empty() != description.candidates.is_empty()
                        || (!merged.candidates.is_empty()
                            && merged.candidates.last() != description.candidates.last())
                    {
                        merged.candidates.clear();
                    }
                    self.initialized = true;
                }
            }
        }
    }

    /// The pin's `StructureToType`.
    fn to_type(&self, limits: Limits, depth: u64) -> LogicalType {
        if depth >= limits.depth {
            return LogicalType::Json;
        }
        let [description] = &self.descriptions[..] else { return LogicalType::Json };
        match description.kind {
            Kind::List => LogicalType::list(description.children[0].to_type(limits, depth + 1)),
            Kind::Struct => self.object_type(description, limits, depth),
            Kind::Varchar => {
                description.candidates.last().map_or(LogicalType::Varchar, |last| last.ty())
            }
            Kind::UBigInt if description.large => LogicalType::HugeInt,
            Kind::UBigInt | Kind::BigInt => LogicalType::BigInt,
            Kind::HugeInt => LogicalType::HugeInt,
            Kind::Double => LogicalType::Double,
            Kind::Boolean => LogicalType::Boolean,
            Kind::Null => LogicalType::Null,
        }
    }

    fn merged_type(&self, description: &Description, limits: Limits, depth: u64) -> LogicalType {
        let mut merged = Self::default();
        for child in &description.children {
            merged.merge(child);
        }
        merged.to_type(limits, depth + 1)
    }

    fn object_type(&self, description: &Description, limits: Limits, depth: u64) -> LogicalType {
        let maps = limits.maps != u64::MAX;
        if description.children.is_empty() {
            return if maps {
                LogicalType::Map(Box::new(LogicalType::Varchar), Box::new(LogicalType::Json))
            } else {
                LogicalType::Json
            };
        }
        if maps {
            #[allow(clippy::cast_precision_loss)]
            let seen = (self.count - self.nulls) as f64;
            #[allow(clippy::cast_precision_loss)]
            let total: f64 =
                description.children.iter().map(|child| child.count as f64 / seen).sum();
            #[allow(clippy::cast_precision_loss)]
            let average = total / description.children.len() as f64;
            if average < limits.threshold {
                let value = self.merged_type(description, limits, depth + 1);
                return LogicalType::Map(Box::new(LogicalType::Varchar), Box::new(value));
            }
        }
        let fields: Vec<Field> = description
            .children
            .iter()
            .map(|child| Field::new(child.key.clone(), child.to_type(limits, depth + 1)))
            .collect();
        if description.children.len() as u64 >= limits.maps {
            let value = self.merged_type(description, limits, depth + 1);
            let mut total = 0.0;
            for field in &fields {
                let similarity = similarity(&value, &field.ty, limits.depth, depth + 1);
                if similarity < 0.0 {
                    total = similarity;
                    break;
                }
                total += similarity;
            }
            #[allow(clippy::cast_precision_loss)]
            if total / fields.len() as f64 >= 0.8 {
                return LogicalType::Map(Box::new(LogicalType::Varchar), Box::new(value));
            }
        }
        LogicalType::Struct(fields)
    }
}

impl Description {
    /// The pin's `EliminateCandidateTypes` over one chunk's strings at a place.
    fn eliminate(&mut self, strings: &[Option<&str>], formats: &Formats, given: bool) {
        if !given {
            for text in strings.iter().flatten() {
                let Some(offset) = timestamp_offset(text) else { continue };
                if offset && !utc_offset(text) {
                    continue;
                }
                self.offsets =
                    self.offsets.merge(if offset { Offsets::With } else { Offsets::Without });
                if self.offsets == Offsets::Mixed {
                    break;
                }
            }
            if self.offsets == Offsets::Mixed {
                self.candidates.clear();
                return;
            }
        }
        while let Some(candidate) = self.candidates.last().copied() {
            let ty = candidate.ty();
            let fits = if candidate == Candidate::TimestampTz {
                !given
                    && self.offsets == Offsets::With
                    && strings.iter().flatten().all(|text| {
                        cast_value_in_time_zone(
                            &Value::Varchar((*text).to_string()),
                            &ty,
                            true,
                            None,
                        )
                        .is_ok_and(|value| !value.is_null())
                    })
            } else if !formats.of(&ty).is_empty() {
                formats.of(&ty).iter().rev().any(|format| {
                    strings.iter().flatten().all(|text| try_format(format, &ty, text).is_some())
                })
            } else {
                strings.iter().flatten().all(|text| strict_cast(text, &ty).is_some())
            };
            if fits {
                return;
            }
            self.candidates.pop();
        }
    }
}

/// A strict cast of a string, which is what the pin's `DefaultTryCast` with `strict` is.
fn strict_cast(text: &str, ty: &LogicalType) -> Option<Value> {
    match ty {
        LogicalType::Date => return strict_date(text).map(Value::Date),
        LogicalType::Time => return strict_time(text).map(Value::Time),
        _ => {}
    }
    if ty.is_numeric() && !strict_text(text, ty) {
        return None;
    }
    cast_value(&Value::Varchar(text.to_string()), ty, true).ok().filter(|value| !value.is_null())
}

/// The pin's `TimestampStringHasUtcOffset`: whether a timestamp's text really ends in an offset,
/// as opposed to a word such as `epoch` that the parser also reports as having one.
fn utc_offset(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(split) = bytes.iter().position(|byte| matches!(byte, b'T' | b' ')) else {
        return false;
    };
    let start = split + 1;
    if start >= bytes.len() {
        return false;
    }
    for at in start..bytes.len() {
        match bytes[at] {
            b'Z' => return true,
            b'+' | b'-' => {
                return bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
                    && bytes.get(at + 2).is_some_and(u8::is_ascii_digit);
            }
            _ => {}
        }
    }
    false
}

/// The pin's `CalculateTypeSimilarity`.
fn similarity(merged: &LogicalType, ty: &LogicalType, limit: u64, depth: u64) -> f64 {
    if depth >= limit || *merged == LogicalType::Null || *ty == LogicalType::Null {
        return 1.0;
    }
    if *merged == LogicalType::Json {
        return -1.0;
    }
    if *ty == LogicalType::Json || merged == ty {
        return 1.0;
    }
    match merged {
        LogicalType::Struct(merged_fields) => match ty {
            LogicalType::Map(_, value) => map_and_struct(value, merged_fields, true, limit, depth),
            LogicalType::Struct(fields) => {
                let mut total = 0.0;
                for field in fields {
                    let Some(found) = merged_fields
                        .iter()
                        .find(|merged| merged.name.eq_ignore_ascii_case(&field.name))
                    else {
                        return -1.0;
                    };
                    let similarity = similarity(&found.ty, &field.ty, limit, depth + 1);
                    if similarity < 0.0 {
                        return similarity;
                    }
                    total += similarity;
                }
                #[allow(clippy::cast_precision_loss)]
                let average = total / merged_fields.len() as f64;
                average
            }
            _ => -1.0,
        },
        LogicalType::Map(_, merged_value) => match ty {
            LogicalType::Map(_, value) => similarity(merged_value, value, limit, depth + 1),
            LogicalType::Struct(fields) => {
                map_and_struct(merged_value, fields, false, limit, depth)
            }
            _ => -1.0,
        },
        LogicalType::List(merged_element) => match ty {
            LogicalType::List(element) => similarity(merged_element, element, limit, depth + 1),
            _ => -1.0,
        },
        _ => 1.0,
    }
}

fn map_and_struct(
    value: &LogicalType,
    fields: &[Field],
    swapped: bool,
    limit: u64,
    depth: u64,
) -> f64 {
    let mut total = 0.0;
    for field in fields {
        let similarity = if swapped {
            similarity(&field.ty, value, limit, depth + 1)
        } else {
            similarity(value, &field.ty, limit, depth + 1)
        };
        if similarity < 0.0 {
            return similarity;
        }
        total += similarity;
    }
    #[allow(clippy::cast_precision_loss)]
    let average = total / fields.len() as f64;
    average
}

/// The pin's `RemoveDuplicateStructKeys`.
fn without_duplicates(ty: &LogicalType, ignore: bool) -> Result<LogicalType> {
    Ok(match ty {
        LogicalType::Struct(fields) => {
            let mut seen = HashSet::new();
            let mut kept = Vec::with_capacity(fields.len());
            for field in fields {
                if !seen.insert(field.name.to_lowercase()) {
                    if ignore {
                        continue;
                    }
                    return Err(Error::not_implemented(format!(
                        "Duplicate name \"{}\" in struct auto-detected in JSON, try \
                         ignore_errors=true",
                        field.name
                    )));
                }
                kept.push(Field::new(field.name.clone(), without_duplicates(&field.ty, ignore)?));
            }
            LogicalType::Struct(kept)
        }
        LogicalType::Map(key, value) => LogicalType::Map(
            Box::new(without_duplicates(key, ignore)?),
            Box::new(without_duplicates(value, ignore)?),
        ),
        LogicalType::List(element) => LogicalType::list(without_duplicates(element, ignore)?),
        other => other.clone(),
    })
}

/// The pin's `DeduplicateColumnNames`, which tells names apart without regard to case.
fn deduplicate(names: &mut [String]) {
    let mut counts: HashMap<String, u64> = HashMap::new();
    for name in names.iter_mut() {
        while let Some(count) = counts.get_mut(&name.to_lowercase()) {
            *count += 1;
            let next = format!("{name}_{count}");
            *name = next;
        }
        counts.insert(name.to_lowercase(), 0);
    }
}

/// What reading a file gives the binder: the file's text by its name.
pub type Load<'l> = &'l mut dyn FnMut(&str) -> Result<String>;

/// How a read was settled at bind time, which is everything the executor needs besides the call's
/// own options and the columns it was planned with.
#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    /// Whether the values are rows of columns or one column.
    pub records: bool,
    /// Whether the columns were detected, which decides the hint and whether an unknown key is an
    /// error.
    pub detected: bool,
    /// Whether auto-detection's date and timestamp templates are read with.
    pub templates: bool,
    /// The key each column is read from.
    pub keys: Vec<String>,
    /// The name each column is answered as.
    pub names: Vec<String>,
}

impl Settled {
    /// The settlement written as text, for the plan to carry.
    #[must_use]
    pub fn written(&self) -> String {
        let mut out = String::from("{\"r\":");
        out.push_str(if self.records { "true" } else { "false" });
        out.push_str(",\"a\":");
        out.push_str(if self.detected { "true" } else { "false" });
        out.push_str(",\"t\":");
        out.push_str(if self.templates { "true" } else { "false" });
        for (key, list) in [("k", &self.keys), ("n", &self.names)] {
            out.push_str(",\"");
            out.push_str(key);
            out.push_str("\":[");
            for (at, item) in list.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                super::string_text(item, &mut out);
            }
            out.push(']');
        }
        out.push('}');
        out
    }

    /// A settlement read back from [`Self::written`].
    ///
    /// # Errors
    ///
    /// An internal error for text that is not one, which is a plan built wrong.
    pub fn from_written(text: &str) -> Result<Self> {
        let broken = || Error::internal(format!("a read_json plan carries {text:?}"));
        let document = read(text).map_err(|_| broken())?;
        let Some(Node::Object(members)) = document.nodes.first() else { return Err(broken()) };
        let member = |name: &str| {
            members.iter().find(|(key, _)| key == name).map(|(_, at)| &document.nodes[*at])
        };
        let flag = |name: &str| matches!(member(name), Some(Node::Bool(true)));
        let list = |name: &str| -> Result<Vec<String>> {
            let Some(Node::Array(items)) = member(name) else { return Err(broken()) };
            items
                .iter()
                .map(|at| match &document.nodes[*at] {
                    Node::Str(text) => Ok(text.clone()),
                    _ => Err(broken()),
                })
                .collect()
        };
        Ok(Self {
            records: flag("r"),
            detected: flag("a"),
            templates: flag("t"),
            keys: list("k")?,
            names: list("n")?,
        })
    }
}

/// What binding one file gave.
struct Bound {
    names: Vec<String>,
    types: Vec<LogicalType>,
    records: Records,
    records_detected: bool,
    detected: bool,
    templates: bool,
    keys: Vec<String>,
    tree: Option<Tree>,
}

/// The pin's `DetectStructure` over one file.
fn detect(options: &Options, file: &str, text: Arc<str>, formats: &Formats) -> Result<Tree> {
    let mut tree = Tree::default();
    let mut units = Units::new(file, text, options);
    let mut remaining = options.sample_size;
    let given = options.timestamp_format.is_some();
    while remaining != 0 {
        let batch = units.next_batch()?;
        if batch.is_empty() {
            break;
        }
        let next = batch.len().min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let values = &batch.roots[..next];
        for root in values.iter().flatten() {
            tree.extract(&batch.document, *root);
        }
        remaining -= next as u64;
        if !tree.contains_varchar() {
            continue;
        }
        tree.initialize_candidates(
            options.maximum_depth,
            options.convert_strings_to_integers,
            given,
            0,
        );
        tree.refine(&batch.document, values, formats, given);
    }
    let mut merged = Tree::default();
    merged.merge(&tree);
    Ok(merged)
}

/// The pin's `StructureToColumns`, which fills in the record type and, when the call gave no
/// columns, the columns.
fn structure_columns(
    options: &Options,
    tree: &Tree,
    records: &mut Records,
    names: &mut Vec<String>,
    types: &mut Vec<LogicalType>,
) -> Result<()> {
    let limits = Limits {
        depth: options.maximum_depth,
        threshold: options.field_appearance_threshold,
        maps: options.map_inference_threshold,
    };
    let ty = tree.to_type(limits, 0);
    if *records == Records::Auto {
        *records =
            if matches!(ty, LogicalType::Struct(_)) { Records::Records } else { Records::Values };
    }
    if !names.is_empty() {
        return Ok(());
    }
    if *records == Records::Records {
        let LogicalType::Struct(fields) = ty else {
            return Err(Error::binder(
                "json_read expected records, but got non-record JSON instead.\n Try setting \
                 records='auto' or records='false'.",
            ));
        };
        for field in fields {
            types.push(without_duplicates(&field.ty, options.ignore_errors)?);
            names.push(field.name);
        }
    } else {
        types.push(without_duplicates(&ty, options.ignore_errors)?);
        names.push("json".to_string());
    }
    Ok(())
}

/// The pin's `BindSchema` and `FinalizeBind` for one file.
fn bind_one(options: &Options, file: &str, text: Arc<str>) -> Result<Bound> {
    let mut names: Vec<String> = options.columns.iter().map(|field| field.name.clone()).collect();
    let mut types: Vec<LogicalType> =
        options.columns.iter().map(|field| field.ty.clone()).collect();
    let mut records = options.records;
    if records == Records::Auto && types.len() > 1 {
        records = Records::Records;
    }
    let mut detected = options.auto_detect && types.is_empty();
    if !detected {
        if types.is_empty() {
            return Err(Error::binder(
                "When auto_detect=false, read_json requires columns to be specified through the \
                 \"columns\" parameter.",
            ));
        }
        if records == Records::Values && types.len() != 1 {
            return Err(Error::binder(
                "read_json requires a single column to be specified through the \"columns\" \
                 parameter when \"records\" is set to 'false'.",
            ));
        }
    }
    let templates = detected;
    let records_detected = records == Records::Auto;
    let keep = names.is_empty();
    let mut tree = None;
    if detected || records == Records::Auto {
        let formats = options.formats(templates);
        let found = detect(options, file, text, &formats)?;
        structure_columns(options, &found, &mut records, &mut names, &mut types)?;
        if keep {
            tree = Some(found);
        }
    }
    let keys = names.clone();
    if detected {
        deduplicate(&mut names);
    } else {
        detected = false;
    }
    Ok(Bound { names, types, records, records_detected, detected, templates, keys, tree })
}

/// Works out the columns of a read and how it is settled, the way the pin's bind does: one file
/// alone, and several by binding each of the first `maximum_sample_files` alone and merging what
/// was detected in them. With `union_by_name` every file is bound, which is all the option does
/// here: the pin merges what each file's sample found the same way either way, so a key one file
/// lacks is a null in that file's rows and a key two files disagree on gets the merged type.
///
/// # Errors
///
/// Whatever reading a file reports, the refusals of detection, and every error the pin raises
/// while it samples, which include a malformed unit early in a file.
pub fn bind(options: &Options, files: &[String], load: Load<'_>) -> Result<(Vec<Field>, Settled)> {
    if files.is_empty() {
        // Only `allow_empty` gets here. The pin answers one BOOLEAN column named `empty` with no
        // rows, whatever columns the call gave and without the one `filename` would add.
        let name = "empty".to_string();
        let settled = Settled {
            records: false,
            detected: false,
            templates: false,
            keys: vec![name.clone()],
            names: vec![name.clone()],
        };
        return Ok((vec![Field::new(name, LogicalType::Boolean)], settled));
    }
    let (mut fields, settled) = bind_columns(options, files, load)?;
    if let Some(name) = &options.filename {
        if fields.iter().any(|field| field.name.eq_ignore_ascii_case(name)) {
            return Err(Error::binder(format!(
                "Option filename adds column \"{name}\", but a column with this name is also in \
                 the file. Try setting a different name: filename='<filename column name>'"
            )));
        }
        fields.push(Field::new(name.clone(), LogicalType::Varchar));
    }
    Ok((fields, settled))
}

/// The columns the files give, before the one `filename` adds.
fn bind_columns(
    options: &Options,
    files: &[String],
    load: Load<'_>,
) -> Result<(Vec<Field>, Settled)> {
    let sampled = if options.union_by_name {
        files.len()
    } else {
        files.len().min(usize::try_from(options.maximum_sample_files).unwrap_or(usize::MAX))
    };
    let mut bounds = Vec::with_capacity(sampled);
    for file in &files[..sampled.max(1).min(files.len())] {
        let text = Arc::from(load(file)?);
        bounds.push(bind_one(options, file, text)?);
    }
    let Some(first) = bounds.first() else {
        return Err(Error::internal("read_json bound with no files"));
    };
    let fields = |names: &[String], types: &[LogicalType]| -> Vec<Field> {
        names.iter().zip(types).map(|(name, ty)| Field::new(name.clone(), ty.clone())).collect()
    };
    if files.len() == 1 || bounds.iter().any(|bound| bound.tree.is_none()) {
        let settled = Settled {
            records: first.records == Records::Records,
            detected: first.detected && files.len() == 1,
            templates: first.templates,
            keys: first.keys.clone(),
            names: first.names.clone(),
        };
        return Ok((fields(&first.names, &first.types), settled));
    }
    let mut merged = Tree::default();
    for bound in &bounds {
        if let Some(tree) = &bound.tree {
            merged.merge(tree);
        }
    }
    let mut records = if first.records_detected { Records::Auto } else { first.records };
    let mut names = Vec::new();
    let mut types = Vec::new();
    structure_columns(options, &merged, &mut records, &mut names, &mut types)?;
    let keys = names.clone();
    deduplicate(&mut names);
    let settled = Settled {
        records: records == Records::Records,
        detected: false,
        templates: first.templates,
        keys,
        names: names.clone(),
    };
    Ok((fields(&names, &types), settled))
}

/// One file being read into chunks.
#[derive(Debug)]
pub struct Reading {
    units: Units,
    objects: bool,
    records: bool,
    hint: &'static str,
    strict: bool,
    unknown: bool,
    formats: Formats,
    text: Arc<str>,
    /// The keys and types of the columns read, in the order they are answered.
    columns: Vec<(String, LogicalType)>,
    /// Where among the columns answered the file's name goes, when it is one of them.
    filename: Option<usize>,
}

impl Reading {
    /// Starts reading a file, answering the `columns` given by name.
    ///
    /// # Errors
    ///
    /// An internal error for a column the settlement does not know, which is a plan built wrong.
    pub fn new(
        file: &str,
        text: Arc<str>,
        options: &Options,
        settled: &Settled,
        columns: &[Field],
    ) -> Result<Self> {
        let mut read = Vec::with_capacity(columns.len());
        let mut filename = None;
        for (index, column) in columns.iter().enumerate() {
            if options.filename.as_ref() == Some(&column.name) {
                filename = Some(index);
                continue;
            }
            let at =
                settled.names.iter().position(|name| *name == column.name).ok_or_else(|| {
                    Error::internal(format!("read_json has no column named \"{}\"", column.name))
                })?;
            read.push((settled.keys[at].clone(), column.ty.clone()));
        }
        let all = read.len() == settled.names.len();
        Ok(Self {
            units: Units::new(file, Arc::clone(&text), options),
            objects: options.function.objects(),
            records: settled.records,
            hint: if settled.detected { DETECTED_HINT } else { GIVEN_HINT },
            strict: !options.ignore_errors,
            unknown: settled.detected && !options.ignore_errors && all,
            formats: options.formats(settled.templates),
            text,
            columns: read,
            filename,
        })
    }

    /// The next chunk of rows, or `None` at the end of the file.
    ///
    /// # Errors
    ///
    /// The pin's refusals of a unit that is not a document and of a value that does not read as
    /// its column's type.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        let Some(chunk) = self.next_columns()? else { return Ok(None) };
        let Some(at) = self.filename else { return Ok(Some(chunk)) };
        let rows = chunk.len();
        let mut vectors = chunk.into_columns();
        let file = Value::Varchar(self.units.file.to_string());
        vectors.insert(at, Vector::constant(LogicalType::Varchar, file, rows));
        Chunk::with_rows(vectors, rows).map(Some)
    }

    /// The next chunk of the columns read from the documents, without the file's name.
    fn next_columns(&mut self) -> Result<Option<Chunk>> {
        let batch = self.units.next_batch()?;
        if batch.is_empty() {
            return Ok(None);
        }
        let rows = batch.len();
        if self.columns.is_empty() {
            return Chunk::with_rows(Vec::new(), rows).map(Some);
        }
        if self.objects {
            let values: Vec<Value> = batch
                .roots
                .iter()
                .zip(&batch.units)
                .map(|(root, (from, to))| match root {
                    Some(_) => Value::Varchar(self.text[*from..*to].to_string()),
                    None => Value::Null,
                })
                .collect();
            let ty = self.columns[0].1.clone();
            return Chunk::with_rows(vec![Vector::from_values(ty, &values)?], rows).map(Some);
        }
        let first = batch.first;
        let roots = batch.roots;
        let mut transform = Transform {
            document: batch.document,
            strict: self.strict,
            duplicate: self.strict,
            unknown: self.unknown,
            formats: &self.formats,
            message: String::new(),
            index: 0,
        };
        let (columns, success) = if self.records {
            let keys: Vec<String> = self.columns.iter().map(|(key, _)| key.clone()).collect();
            let types: Vec<LogicalType> = self.columns.iter().map(|(_, ty)| ty.clone()).collect();
            transform.object(&roots, &keys, &types, true)?
        } else {
            let (values, success) = transform.column(&roots, &self.columns[0].1)?;
            (vec![values], success)
        };
        if !success {
            return Err(Error::invalid_input(format!(
                "JSON transform error in file \"{}\", in {} {}: {}{}",
                self.units.file,
                self.units.unit(),
                first + transform.index + 1,
                transform.message,
                self.hint
            )));
        }
        let vectors = columns
            .iter()
            .zip(&self.columns)
            .map(|(values, (_, ty))| Vector::from_values(ty.clone(), values))
            .collect::<Result<Vec<_>>>()?;
        Chunk::with_rows(vectors, rows).map(Some)
    }
}

/// The pin's `JSONTransform` over one chunk, column at a time.
struct Transform<'f> {
    document: Document,
    strict: bool,
    duplicate: bool,
    unknown: bool,
    formats: &'f Formats,
    message: String,
    index: usize,
}

type Column = (Vec<Value>, bool);

impl Transform<'_> {
    /// A value written out and cut to fifty bytes, as the pin quotes one in an error.
    fn quoted(&self, at: usize) -> String {
        let mut written = self.document.written(at);
        if written.len() > 50 {
            let mut end = 50;
            while !written.is_char_boundary(end) {
                end -= 1;
            }
            written.truncate(end);
            written.push_str("...");
        }
        written
    }

    fn node(&self, item: Option<usize>) -> Option<&Node> {
        item.map(|at| &self.document.nodes[at]).filter(|node| !matches!(node, Node::Null))
    }

    fn column(&mut self, items: &[Option<usize>], ty: &LogicalType) -> Result<Column> {
        if matches!(ty, LogicalType::Date | LogicalType::Timestamp)
            && !self.formats.of(ty).is_empty()
        {
            return Ok(self.with_format(items, ty));
        }
        match ty {
            LogicalType::Json => Ok((
                items
                    .iter()
                    .map(|item| match self.node(*item) {
                        Some(_) => Value::Varchar(self.document.written(item.unwrap_or_default())),
                        None => Value::Null,
                    })
                    .collect(),
                true,
            )),
            LogicalType::Null => Ok((vec![Value::Null; items.len()], true)),
            LogicalType::Varchar | LogicalType::Blob => Ok(self.strings(items, ty)),
            LogicalType::Struct(fields) => {
                let keys: Vec<String> = fields.iter().map(|field| field.name.clone()).collect();
                let types: Vec<LogicalType> = fields.iter().map(|field| field.ty.clone()).collect();
                let (columns, success) = self.object(items, &keys, &types, true)?;
                let values = (0..items.len())
                    .map(|row| {
                        if self.node(items[row]).is_none() {
                            return Value::Null;
                        }
                        Value::Struct(
                            keys.iter()
                                .zip(&columns)
                                .map(|(key, column)| (key.clone(), column[row].clone()))
                                .collect(),
                        )
                    })
                    .collect();
                Ok((values, success))
            }
            LogicalType::List(element) => self.list(items, element, None),
            LogicalType::Array(element, size) => self.list(items, element, Some(*size as usize)),
            LogicalType::Map(key, value) => self.map(items, key, value),
            LogicalType::Enum(..)
            | LogicalType::Date
            | LogicalType::Interval
            | LogicalType::Time
            | LogicalType::TimeTz
            | LogicalType::Timestamp
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs
            | LogicalType::TimestampTz
            | LogicalType::Uuid => Ok(self.string_column(items, ty)),
            _ if ty.is_numeric() || *ty == LogicalType::Boolean => Ok(self.numerical(items, ty)),
            _ => Err(Error::not_implemented(format!(
                "Cannot read a value of type {ty} from a json file"
            ))),
        }
    }

    fn numerical(&mut self, items: &[Option<usize>], ty: &LogicalType) -> Column {
        let kind = if matches!(ty, LogicalType::Decimal { .. }) { "decimal" } else { "numerical" };
        let mut success = true;
        let mut values = Vec::with_capacity(items.len());
        // row at a time: each row is a parsed JSON node whose text is cast on its own, which costs
        // more than building the value it casts to.
        for (row, item) in items.iter().enumerate() {
            let Some(node) = self.node(*item) else {
                values.push(Value::Null);
                continue;
            };
            let read = match node {
                Node::Str(text) | Node::Raw(text) => {
                    if self.strict && !strict_text(text, ty) {
                        None
                    } else {
                        cast_value(&Value::Varchar(text.clone()), ty, true).ok()
                    }
                }
                Node::Array(_) | Node::Object(_) => None,
                other => self
                    .document
                    .scalar_value(other)
                    .and_then(|scalar| cast_value(&scalar, ty, true).ok()),
            };
            match read.filter(|value| !value.is_null()) {
                Some(value) => values.push(value),
                None => {
                    values.push(Value::Null);
                    if self.strict {
                        let at = item.unwrap_or_default();
                        self.message =
                            format!("Failed to cast value to {kind}: {}", self.quoted(at));
                        if success {
                            self.index = row;
                            success = false;
                        }
                    }
                }
            }
        }
        (values, success)
    }

    /// The pin's `GetStringVector`: the strings among the items, with what is not one a null and,
    /// when strict, the first such an error.
    fn string_items(
        &mut self,
        items: &[Option<usize>],
        ty: &LogicalType,
        success: &mut bool,
    ) -> Vec<Option<String>> {
        let mut strings = Vec::with_capacity(items.len());
        for (row, item) in items.iter().enumerate() {
            match self.node(*item) {
                None => strings.push(None),
                Some(Node::Str(text)) => strings.push(Some(text.clone())),
                Some(_) => {
                    strings.push(None);
                    if *success && self.strict {
                        let name = match ty {
                            LogicalType::TimestampTz => "TIMESTAMP WITH TIME ZONE".to_string(),
                            LogicalType::TimeTz => "TIME WITH TIME ZONE".to_string(),
                            other => other.to_string(),
                        };
                        self.message = format!(
                            "Unable to cast '{}' to {name}",
                            self.quoted(item.unwrap_or_default())
                        );
                        self.index = row;
                        *success = false;
                    }
                }
            }
        }
        strings
    }

    fn string_column(&mut self, items: &[Option<usize>], ty: &LogicalType) -> Column {
        let mut success = true;
        let strings = self.string_items(items, ty, &mut success);
        let mut failed = false;
        let values = strings
            .into_iter()
            .map(|text| {
                let Some(text) = text else { return Value::Null };
                match cast_value_in_time_zone(&Value::Varchar(text), ty, false, None) {
                    Ok(value) => value,
                    Err(error) => {
                        if self.message.is_empty() {
                            self.message = error.message().to_string();
                        }
                        failed = true;
                        Value::Null
                    }
                }
            })
            .collect();
        if failed && self.strict {
            self.index = 0;
            self.message.push_str(
                "\n If this error occurred during read_json, line/object number information is \
                 approximate",
            );
            success = false;
        }
        (values, success)
    }

    fn with_format(&mut self, items: &[Option<usize>], ty: &LogicalType) -> Column {
        let mut success = true;
        let strings = self.string_items(items, ty, &mut success);
        let formats = self.formats.of(ty);
        let matched = strings.iter().flatten().find_map(|text| {
            (0..formats.len()).rev().find(|at| try_format(&formats[*at], ty, text).is_some())
        });
        let limit = matched.map_or(formats.len(), |at| at + 1);
        let mut values = Vec::with_capacity(strings.len());
        for (row, text) in strings.iter().enumerate() {
            let Some(text) = text else {
                values.push(Value::Null);
                continue;
            };
            match (0..limit).rev().find_map(|at| try_format(&formats[at], ty, text)) {
                Some(value) => values.push(value),
                None => {
                    values.push(Value::Null);
                    if success && self.strict {
                        self.index = row;
                        success = false;
                    }
                }
            }
        }
        (values, success)
    }

    fn strings(&self, items: &[Option<usize>], ty: &LogicalType) -> Column {
        let values = items
            .iter()
            .map(|item| {
                let Some(node) = self.node(*item) else { return Value::Null };
                let text = match node {
                    Node::Str(text) => text.clone(),
                    Node::Array(_) | Node::Object(_) => {
                        self.document.written(item.unwrap_or_default())
                    }
                    other => match self.document.scalar_value(other) {
                        Some(Value::Varchar(text)) => text,
                        Some(scalar) => cast_value(&scalar, &LogicalType::Varchar, false)
                            .map_or_else(
                                |_| scalar.to_string(),
                                |value| match value {
                                    Value::Varchar(text) => text,
                                    other => other.to_string(),
                                },
                            ),
                        None => String::new(),
                    },
                };
                if *ty == LogicalType::Blob {
                    Value::Blob(text.into_bytes())
                } else {
                    Value::Varchar(text)
                }
            })
            .collect();
        (values, true)
    }

    /// The pin's `TransformObject`: one column per key, from the object in each item.
    fn object(
        &mut self,
        items: &[Option<usize>],
        keys: &[String],
        types: &[LogicalType],
        unknown: bool,
    ) -> Result<(Vec<Vec<Value>>, bool)> {
        let mut map: HashMap<&str, usize> = HashMap::with_capacity(keys.len());
        for (at, key) in keys.iter().enumerate() {
            map.entry(key.as_str()).or_insert(at);
        }
        let mut nested = vec![vec![None; items.len()]; keys.len()];
        let mut success = true;
        for (row, item) in items.iter().enumerate() {
            let Some(at) = *item else { continue };
            let children = match &self.document.nodes[at] {
                Node::Null => continue,
                Node::Object(children) => children.clone(),
                other => {
                    if success && self.strict {
                        self.message = format!(
                            "Expected OBJECT, but got {}: {}",
                            type_name(other),
                            self.quoted(at)
                        );
                        self.index = row;
                        success = false;
                    }
                    continue;
                }
            };
            for (key, child) in &children {
                match map.get(key.as_str()) {
                    Some(column) => {
                        if nested[*column][row].is_some() {
                            if success && self.duplicate {
                                self.message = format!(
                                    "Object {} has duplicate key \"{key}\"",
                                    self.quoted(at)
                                );
                                self.index = row;
                                success = false;
                            }
                        } else {
                            nested[*column][row] = Some(*child);
                        }
                    }
                    None => {
                        if success && unknown && self.unknown {
                            self.message =
                                format!("Object {} has unknown key \"{key}\"", self.quoted(at));
                            self.index = row;
                            success = false;
                        }
                    }
                }
            }
        }
        let mut columns = Vec::with_capacity(keys.len());
        for (items, ty) in nested.iter().zip(types) {
            let (values, fine) = self.column(items, ty)?;
            if !fine {
                success = false;
            }
            columns.push(values);
        }
        Ok((columns, success))
    }

    /// The pin's `TransformArrayToList`, and with a size its `TransformArrayToArray`.
    fn list(
        &mut self,
        items: &[Option<usize>],
        element: &LogicalType,
        size: Option<usize>,
    ) -> Result<Column> {
        let mut success = true;
        let mut ranges: Vec<Option<(usize, usize)>> = Vec::with_capacity(items.len());
        let mut elements = Vec::new();
        for (row, item) in items.iter().enumerate() {
            let Some(at) = *item else {
                ranges.push(None);
                if let Some(size) = size {
                    elements.extend(std::iter::repeat_n(None, size));
                }
                continue;
            };
            let children = match &self.document.nodes[at] {
                Node::Array(children) => Some(children.clone()),
                _ => None,
            };
            let refused = match (&self.document.nodes[at], &children) {
                (Node::Null, _) => None,
                (other, None) => Some(format!(
                    "Expected ARRAY, but got {}: {}",
                    type_name(other),
                    self.quoted(at)
                )),
                (_, Some(children)) => match size {
                    Some(size) if children.len() != size => Some(format!(
                        "Expected array of size {size}, but got '{}' with size {}",
                        self.quoted(at),
                        children.len()
                    )),
                    _ => {
                        let offset = elements.len();
                        elements.extend(children.iter().map(|child| Some(*child)));
                        ranges.push(Some((offset, children.len())));
                        continue;
                    }
                },
            };
            ranges.push(None);
            if let Some(size) = size {
                elements.extend(std::iter::repeat_n(None, size));
            }
            if let Some(message) = refused
                && success
                && self.strict
            {
                self.message = message;
                self.index = row;
                success = false;
            }
        }
        if !success {
            for (row, range) in ranges.iter().enumerate() {
                let Some((offset, length)) = range else { continue };
                if self.index >= *offset && self.index < offset + length {
                    self.index = row;
                }
            }
        }
        let (values, fine) = self.column(&elements, element)?;
        if !fine {
            success = false;
        }
        let rows = ranges
            .iter()
            .map(|range| match range {
                Some((offset, length)) => Value::List {
                    element: element.clone(),
                    values: values[*offset..offset + length].to_vec(),
                },
                None => Value::Null,
            })
            .collect();
        Ok((rows, success))
    }

    /// The pin's `TransformObjectToMap`.
    fn map(
        &mut self,
        items: &[Option<usize>],
        key: &LogicalType,
        value: &LogicalType,
    ) -> Result<Column> {
        let mut success = true;
        let mut ranges = Vec::with_capacity(items.len());
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for (row, item) in items.iter().enumerate() {
            let Some(at) = *item else {
                ranges.push(None);
                continue;
            };
            let children = match &self.document.nodes[at] {
                Node::Null => {
                    ranges.push(None);
                    continue;
                }
                Node::Object(children) => children.clone(),
                other => {
                    ranges.push(None);
                    if success && self.strict {
                        self.message = format!(
                            "Expected OBJECT, but got {}: {}",
                            type_name(other),
                            self.quoted(at)
                        );
                        self.index = row;
                        success = false;
                    }
                    continue;
                }
            };
            ranges.push(Some((keys.len(), children.len())));
            for (name, child) in children {
                self.document.nodes.push(Node::Str(name));
                keys.push(Some(self.document.nodes.len() - 1));
                values.push(Some(child));
            }
        }
        let (keys, fine) = self.column(&keys, key)?;
        if !fine {
            return Err(Error::conversion(format!("{}{}", self.message, super::NULL_KEY)));
        }
        let (values, fine) = self.column(&values, value)?;
        if !fine {
            success = false;
        }
        let rows = ranges
            .iter()
            .map(|range| match range {
                Some((offset, length)) => Value::map(
                    key.clone(),
                    value.clone(),
                    keys[*offset..offset + length]
                        .iter()
                        .cloned()
                        .zip(values[*offset..offset + length].iter().cloned())
                        .collect(),
                ),
                None => Value::Null,
            })
            .collect();
        Ok((rows, success))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(text: &str) -> Vec<(String, String)> {
        let options = Options::new(Function::Json);
        let (fields, _) =
            bind(&options, &["f.json".to_string()], &mut |_| Ok(text.to_string())).unwrap();
        fields.into_iter().map(|field| (field.name, field.ty.to_string())).collect()
    }

    #[test]
    fn newline_delimited_records_are_detected() {
        let text = "{\"a\":1,\"b\":\"x\",\"c\":[1,2]}\n{\"a\":2,\"b\":\"y\",\"c\":[3]}\n";
        assert_eq!(
            types(text),
            vec![
                ("a".to_string(), "BIGINT".to_string()),
                ("b".to_string(), "VARCHAR".to_string()),
                ("c".to_string(), "BIGINT[]".to_string()),
            ]
        );
    }

    #[test]
    fn the_format_is_detected_from_the_start_of_a_file() {
        assert_eq!(detect_format("{\"a\":1}\n{\"a\":2}\n"), (Format::Newline, Records::Records));
        assert_eq!(detect_format("[{\"a\":1},{\"a\":2.5}]"), (Format::Array, Records::Records));
        assert_eq!(detect_format("[1, 2]"), (Format::Array, Records::Values));
        assert_eq!(detect_format("{\"a\":\n1}"), (Format::Unstructured, Records::Records));
    }

    #[test]
    fn names_that_differ_only_in_case_are_told_apart() {
        let mut names = vec!["A".to_string(), "a".to_string(), "a".to_string()];
        deduplicate(&mut names);
        assert_eq!(names, vec!["A", "a_1", "a_2"]);
    }

    #[test]
    fn a_settlement_reads_back_as_written() {
        let settled = Settled {
            records: true,
            detected: false,
            templates: true,
            keys: vec!["a\"b".to_string()],
            names: vec!["x".to_string()],
        };
        assert_eq!(Settled::from_written(&settled.written()).unwrap(), settled);
    }
}
