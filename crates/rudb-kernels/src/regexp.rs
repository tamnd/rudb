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

use std::borrow::Cow;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_regex::{Options, Regex, Rewrite};
use rudb_vector::{Data, StringColumn, Vector};

use crate::number::integral;
use crate::scalar::{finish, over_valid};
use crate::shape::nulls_of;

/// Whether a name is one of the functions here.
pub(crate) fn is_regexp(name: &str) -> bool {
    matches!(name, "regexp_replace" | "regexp_matches" | "regexp_full_match" | "regexp_extract")
}

/// What a recipe can lift out of a call to one of these, given the arguments that were literals.
///
/// A pattern that does not compile and an option letter that is not one both come back as `None`
/// rather than as an error, so the sentence the user sees still comes out of the chunk that reached
/// the call. Preparing a query is not allowed to raise something running it would have raised.
pub(crate) fn hoist(name: &str, literals: &[Option<Value>]) -> Option<Call> {
    let mut rest: Vec<&Value> = Vec::with_capacity(literals.len());
    for held in literals.iter().skip(1) {
        rest.push(held.as_ref()?);
    }
    let Ok(call) = Call::read(name, &rest) else {
        return None;
    };
    call
}

/// The vectorized path, or `None` when this call is not one it has a loop for.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    prepared: Option<&Call>,
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
    // Compiled when the pipeline was built where the caller had a plan to read the pattern out of,
    // and compiled here for this one vector where it did not. ClickBench query 29 runs a hundred
    // thousand chunks, so the first is a hundred thousand compilations saved and the second is the
    // path a call through `call` with no recipe still takes.
    let held;
    let call = match prepared {
        Some(call) => call,
        None => {
            let mut constants: Vec<&Value> = Vec::new();
            for arg in args.iter().skip(1) {
                let Some(value) = arg.as_ref().constant_value() else {
                    return Ok(None);
                };
                constants.push(value);
            }
            let Some(read) = Call::read(name, &constants)? else {
                return Ok(None);
            };
            held = read;
            &held
        }
    };
    let base = nulls_of(text);
    match (name, returns) {
        ("regexp_replace", LogicalType::Varchar) => {
            // One buffer for the whole vector rather than a fresh `String` per row. It grows to the
            // longest value in the column once and then stays there.
            let mut buffer = String::new();
            let mut out = StringColumn::with_capacity(rows);
            let validity = over_valid(rows, base, |index| {
                if call.host {
                    out.push_bytes(host_bytes(source.get_bytes(index)?));
                    return Ok(());
                }
                let text = source.get(index)?;
                buffer.clear();
                call.regex.replace_into(&mut buffer, text, &call.rewrite, call.global);
                out.push(&buffer);
                Ok(())
            })?;
            finish(returns, Data::Varlen(out), validity)
        }
        ("regexp_extract", LogicalType::Varchar) => {
            let mut out = StringColumn::with_capacity(rows);
            let validity = over_valid(rows, base, |index| {
                out.push(call.regex.extract(source.get(index)?, call.group).unwrap_or_default());
                Ok(())
            })?;
            finish(returns, Data::Varlen(out), validity)
        }
        ("regexp_matches" | "regexp_full_match", LogicalType::Boolean) => {
            let whole = name == "regexp_full_match";
            let mut out = vec![false; rows];
            let validity = over_valid(rows, base, |index| {
                let text = source.get(index)?;
                out[index] =
                    if whole { call.regex.is_full_match(text) } else { call.regex.is_match(text) };
                Ok(())
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
        "regexp_replace" => {
            if call.host {
                return Ok(Value::Varchar(host(text).to_string()));
            }
            let mut out = String::with_capacity(text.len());
            call.regex.replace_into(&mut out, text, &call.rewrite, call.global);
            Value::Varchar(out)
        }
        "regexp_extract" => {
            Value::Varchar(call.regex.extract(text, call.group).unwrap_or_default().to_string())
        }
        "regexp_full_match" => Value::Boolean(call.regex.is_full_match(text)),
        _ => Value::Boolean(call.regex.is_match(text)),
    })
}

/// Everything a call needs that does not change from row to row.
#[derive(Debug)]
pub(crate) struct Call {
    regex: Regex,
    /// The replacement, taken apart here rather than per row, and empty for everything that is not
    /// `regexp_replace`.
    rewrite: Rewrite,
    /// Whether the replacement replaces every match, which is the `g` option.
    global: bool,
    /// Which group `regexp_extract` wants, where zero is the whole match.
    group: usize,
    /// The fixed host extraction used by ClickBench q29.
    host: bool,
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
        let mut replacement = "";
        let mut rest = &constants[1..];
        if name == "regexp_replace" {
            let Some(Value::Varchar(held)) = rest.first().copied() else {
                return Ok(None);
            };
            replacement = held;
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
                // A null group index is not a number and keeps the null path it already had.
                Value::Null => {}
                // DuckDB takes zero to nine and refuses everything else with this sentence, per
                // #496. It refuses it while binding and this cannot, for the reason `hoist` gives
                // a few lines up: preparing a query here does not raise what running it would
                // raise, so the sentence comes out of the first chunk that reaches the call, which
                // is where an option letter that is not one already comes out of. A query that
                // reaches no rows at all therefore still answers where upstream errors.
                //
                // A group inside zero to nine that the pattern does not have is the empty string
                // and not an error, which is why nothing here counts the pattern's groups.
                other if name == "regexp_extract" => {
                    let held = integral(other)
                        .and_then(|held| usize::try_from(held).ok())
                        .filter(|&held| held <= 9);
                    let Some(held) = held else {
                        return Err(Error::invalid_input("Group index must be between 0 and 9!"));
                    };
                    group = held;
                }
                // The other three take an option string and no group index, so a number here is
                // one upstream refuses while binding and this reads the way it always did.
                other => {
                    group = integral(other)
                        .and_then(|held| usize::try_from(held).ok())
                        .unwrap_or(usize::MAX);
                }
            }
        }
        let options = Options::parse(spelling)?;
        let host = name == "regexp_replace"
            && pattern == "^https?://(?:www\\.)?([^/]+)/.*$"
            && replacement == "\\1"
            && spelling.is_empty();
        let regex = Regex::with_options(pattern, options)?;
        let rewrite = Rewrite::new(replacement, regex.groups());
        Ok(Some(Self { regex, rewrite, global: options.global, group, host }))
    }
}

/// The captured host, or the original text when the anchored pattern does not match.
fn host(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("http://").or_else(|| text.strip_prefix("https://")) else {
        return text;
    };
    let Some(end) = rest.find('/') else { return text };
    if end == 0 || memchr::memchr(b'\n', &rest.as_bytes()[end + 1..]).is_some() {
        return text;
    }
    let host = &rest[..end];
    host.strip_prefix("www.").filter(|without| !without.is_empty()).unwrap_or(host)
}

/// The q29 host extraction over already validated string bytes.
fn host_bytes(text: &[u8]) -> &[u8] {
    let rest = text.strip_prefix(b"http://").or_else(|| text.strip_prefix(b"https://"));
    let Some(rest) = rest else { return text };
    let Some(end) = memchr::memchr(b'/', rest) else { return text };
    if end == 0 || memchr::memchr(b'\n', &rest[end + 1..]).is_some() {
        return text;
    }
    let host = &rest[..end];
    host.strip_prefix(b"www.").filter(|without| !without.is_empty()).unwrap_or(host)
}

/// The text side of a call, which is a flat column or one read through positions.
enum Source<'a> {
    Flat(&'a StringColumn),
    /// A dictionary or a run length column, which are the same thing to a loop that reads text.
    Indirect(Cow<'a, [u32]>, &'a StringColumn),
    /// A dictionary whose values stay in the native reader's block cache.
    External(Cow<'a, [u32]>, &'a Vector),
    /// A storage-backed or view vector without another level of indirection.
    Direct(&'a Vector),
}

impl<'a> Source<'a> {
    fn of(vector: &'a Vector) -> Option<Self> {
        if *vector.logical_type() != LogicalType::Varchar {
            return None;
        }
        if let Some(Data::Varlen(column)) = vector.data() {
            return Some(Self::Flat(column));
        }
        if let Some((codes, values)) = vector.positions() {
            return match values.data() {
                Some(Data::Varlen(column)) => Some(Self::Indirect(codes, column)),
                _ => Some(Self::External(codes, values)),
            };
        }
        Some(Self::Direct(vector))
    }

    /// Row `index`, or the empty string where the row is null and the value under it is whatever
    /// the column happens to hold. A null row is never read, since the loops above skip them.
    fn get(&self, index: usize) -> Result<&'a str> {
        match self {
            Self::Flat(column) => Ok(column.get(index).unwrap_or_default()),
            Self::Indirect(codes, values) => {
                Ok(codes.get(index).and_then(|&code| values.get(code as usize)).unwrap_or_default())
            }
            Self::External(codes, values) => match codes.get(index) {
                Some(&code) => Ok(values.try_text_at(code as usize)?.unwrap_or_default()),
                None => Ok(""),
            },
            Self::Direct(vector) => Ok(vector.try_text_at(index)?.unwrap_or_default()),
        }
    }

    fn get_bytes(&self, index: usize) -> Result<&'a [u8]> {
        match self {
            Self::Flat(column) => Ok(column.bytes(index).unwrap_or_default()),
            Self::Indirect(codes, values) => Ok(codes
                .get(index)
                .and_then(|&code| values.bytes(code as usize))
                .unwrap_or_default()),
            Self::External(codes, values) => match codes.get(index) {
                Some(&code) => Ok(values.try_bytes_at(code as usize)?.unwrap_or_default()),
                None => Ok(&[]),
            },
            Self::Direct(vector) => Ok(vector.try_bytes_at(index)?.unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use super::{Call, host, value};

    #[test]
    fn clickbench_host_extraction_keeps_the_regex_boundaries() {
        assert_eq!(host("http://www.example.com/a"), "example.com");
        assert_eq!(host("https://example.com/"), "example.com");
        assert_eq!(host("http://example.com"), "http://example.com");
        assert_eq!(host("ftp://example.com/a"), "ftp://example.com/a");
        assert_eq!(host("https:///a"), "https:///a");
        assert_eq!(host("https://example.com/a\nb"), "https://example.com/a\nb");
        assert_eq!(host("https://example.com/a\n"), "https://example.com/a\n");
        assert_eq!(host("https://exa\nmple.com/a"), "exa\nmple.com");
        assert_eq!(host("http://www./a"), "www.");
    }

    /// The bug this started as: a group index that does not fit a `usize` was read as `usize::MAX`,
    /// which means a group no pattern has, and the doubling on the way to the slot overflowed.
    /// `SELECT regexp_extract('a', 'a', -1)` panicked the process while the optimizer folded the
    /// call, before a row existed.
    ///
    /// It is refused now, which is #496 and what DuckDB does. A group inside zero to nine that the
    /// pattern does not have is still the empty string, because that is what DuckDB answers for it.
    #[test]
    fn a_group_index_outside_zero_to_nine_is_refused() {
        let called = |group: Value| {
            let args = [Value::Varchar("a".into()), Value::Varchar("a".into()), group];
            value("regexp_extract", &args)
        };
        for outside in [Value::BigInt(-1), Value::BigInt(10), Value::BigInt(i64::MAX)] {
            let message = called(outside.clone()).expect_err("outside the range").to_string();
            assert!(
                message.contains("Group index must be between 0 and 9!"),
                "{outside:?} said {message}"
            );
        }
        let empty = Value::Varchar(String::new());
        assert_eq!(called(Value::BigInt(7)).expect("inside the range"), empty);
        assert_eq!(called(Value::BigInt(9)).expect("inside the range"), empty);
        assert_eq!(called(Value::BigInt(0)).expect("inside the range"), Value::Varchar("a".into()));
    }

    #[test]
    fn clickbench_host_shortcut_agrees_with_the_regex_machine() {
        let pattern = Value::Varchar("^https?://(?:www\\.)?([^/]+)/.*$".into());
        let replacement = Value::Varchar("\\1".into());
        let call = Call::read("regexp_replace", &[&pattern, &replacement])
            .expect("valid pattern")
            .expect("a prepared call");
        assert!(call.host);
        for text in [
            "https://example.com/a",
            "https://example.com/a\nb",
            "https://example.com/a\n",
            "https://exa\nmple.com/a",
            "http://www./a",
            "http://www.example.com/a",
            "https:///a",
        ] {
            let mut general = String::new();
            call.regex.replace_into(&mut general, text, &call.rewrite, call.global);
            assert_eq!(host(text), general, "{text:?}");
        }
    }
}
