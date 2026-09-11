//! The regular expression functions, over a pattern that is the same on every row.
//!
//! `regexp_replace`, `regexp_matches`, `regexp_full_match` and `regexp_extract`, with the engine in
//! `rudb-regex` under them. What this file is for is the thing that separates a usable
//! implementation from one that is technically correct: the pattern is compiled once per vector.
//! ClickBench query 29 runs one pattern over a hundred million rows, and compiling it per row would
//! cost more than matching it.
//!
//! Everything past the first argument has to be constant for the loop here to run, which is what
//! every query in the wild looks like, since a pattern that varies per row is a pattern the planner
//! could not have hoisted anyway. A call where it does vary falls through to the row at a time path
//! in `scalar`, which is correct and counts itself in the kernel table.
//!
//! The text side reads a flat column or a dictionary. A dictionary still runs the machine once per
//! row rather than once per distinct value, which is the obvious next thing to do here and is worth
//! a number before it is worth writing: the column this is measured on, `Referer`, has enough
//! distinct values that the dictionary form may never appear on it.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_regex::{Options, Regex};
use rudb_vector::{Data, StringColumn, Vector};

use crate::number::integral;
use crate::scalar::{each_string, finish, over_valid};
use crate::shape::nulls_of;

/// Whether a name is one of the functions here.
pub(crate) fn is_regexp(name: &str) -> bool {
    matches!(name, "regexp_replace" | "regexp_matches" | "regexp_full_match" | "regexp_extract")
}

/// The vectorized path, or `None` when this call is not one it has a loop for.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    args: &[V],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let Some(text) = args.first().map(AsRef::as_ref) else {
        return Ok(None);
    };
    let Some(source) = Source::of(text) else {
        return Ok(None);
    };
    let mut constants: Vec<&Value> = Vec::new();
    for arg in args.iter().skip(1) {
        let Some(value) = arg.as_ref().constant_value() else {
            return Ok(None);
        };
        constants.push(value);
    }
    let Some(call) = Call::read(name, &constants)? else {
        return Ok(None);
    };
    let base = nulls_of(text);
    match (name, returns) {
        ("regexp_replace", LogicalType::Varchar) => {
            let out = each_string(rows, &base, |index, into| {
                into.push(&call.regex.replace(source.get(index), &call.rewrite, call.global));
            });
            finish(returns, Data::Varlen(out), base.normalize(rows))
        }
        ("regexp_extract", LogicalType::Varchar) => {
            let out = each_string(rows, &base, |index, into| {
                into.push(call.regex.extract(source.get(index), call.group).unwrap_or_default());
            });
            finish(returns, Data::Varlen(out), base.normalize(rows))
        }
        ("regexp_matches" | "regexp_full_match", LogicalType::Boolean) => {
            let whole = name == "regexp_full_match";
            let mut out = vec![false; rows];
            let validity = over_valid(rows, base, |index| {
                let text = source.get(index);
                out[index] =
                    if whole { call.regex.is_full_match(text) } else { call.regex.is_match(text) };
                Ok(true)
            })?;
            finish(returns, Data::Bool(out.into()), validity)
        }
        _ => Ok(None),
    }
}

/// The row at a time path, which compiles the pattern for the one row it is given.
pub(crate) fn value(name: &str, args: &[Value]) -> Result<Value> {
    let Some(Value::Varchar(text)) = args.first() else {
        return Err(Error::internal(format!("{name} of something that is not a string")));
    };
    let constants: Vec<&Value> = args.iter().skip(1).collect();
    let Some(call) = Call::read(name, &constants)? else {
        return Err(Error::internal(format!("{name} with arguments it does not have")));
    };
    Ok(match name {
        "regexp_replace" => Value::Varchar(call.regex.replace(text, &call.rewrite, call.global)),
        "regexp_extract" => {
            Value::Varchar(call.regex.extract(text, call.group).unwrap_or_default().to_string())
        }
        "regexp_full_match" => Value::Boolean(call.regex.is_full_match(text)),
        _ => Value::Boolean(call.regex.is_match(text)),
    })
}

/// Everything a call needs that does not change from row to row.
struct Call {
    regex: Regex,
    /// The replacement, for `regexp_replace` and empty for the rest.
    rewrite: String,
    /// Whether the replacement replaces every match, which is the `g` option.
    global: bool,
    /// Which group `regexp_extract` wants, where zero is the whole match.
    group: usize,
}

impl Call {
    /// Reads the arguments after the text, or `None` when they are not the shape this file handles.
    ///
    /// # Errors
    ///
    /// On a pattern that does not compile and on an option letter that is not one, which are the
    /// two things a user can get wrong and are reported with DuckDB's own words.
    fn read(name: &str, constants: &[&Value]) -> Result<Option<Self>> {
        let Some(Value::Varchar(pattern)) = constants.first().copied() else {
            return Ok(None);
        };
        let mut rewrite = String::new();
        let mut rest = &constants[1..];
        if name == "regexp_replace" {
            let Some(Value::Varchar(held)) = rest.first().copied() else {
                return Ok(None);
            };
            rewrite = held.clone();
            rest = &rest[1..];
        }
        // What is left is the group index, the option string, both or neither, and which is which
        // is decided by the type rather than by the position, since the two are never the same
        // type and `regexp_extract` is the only one that takes both.
        let mut group = 0;
        let mut spelling = "";
        for value in rest {
            match value {
                Value::Varchar(held) => spelling = held,
                // A group index that is not a number the machine can hold is one no pattern has, so
                // it reads as the empty string rather than as an error, which is what DuckDB
                // answers for a group index past the end of the pattern.
                other => {
                    group = integral(other)
                        .and_then(|held| usize::try_from(held).ok())
                        .unwrap_or(usize::MAX);
                }
            }
        }
        let options = Options::parse(spelling)?;
        let regex = Regex::with_options(pattern, options)?;
        Ok(Some(Self { regex, rewrite, global: options.global, group }))
    }
}

/// The text side of a call, which is a flat column or a dictionary over one.
enum Source<'a> {
    Flat(&'a StringColumn),
    Dictionary(&'a [u32], &'a StringColumn),
}

impl<'a> Source<'a> {
    fn of(vector: &'a Vector) -> Option<Self> {
        if *vector.logical_type() != LogicalType::Varchar {
            return None;
        }
        if let Some(Data::Varlen(column)) = vector.data() {
            return Some(Self::Flat(column));
        }
        let (codes, values) = vector.dictionary_parts()?;
        match values.data() {
            Some(Data::Varlen(column)) => Some(Self::Dictionary(codes, column)),
            _ => None,
        }
    }

    /// Row `index`, or the empty string where the row is null and the value under it is whatever
    /// the column happens to hold. A null row is never read, since the loops above skip them.
    fn get(&self, index: usize) -> &'a str {
        match self {
            Self::Flat(column) => column.get(index).unwrap_or_default(),
            Self::Dictionary(codes, values) => {
                codes.get(index).and_then(|&code| values.get(code as usize)).unwrap_or_default()
            }
        }
    }
}
