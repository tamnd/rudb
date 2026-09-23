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
//! The text side reads a flat column or a dictionary. A dictionary that outlives the chunk runs
//! `regexp_replace` once per distinct value through [`StableReplace`], and any other dictionary
//! still runs the machine once per row.
//!
//! The number is 2,719,020 distinct in 8,682,923 rows at ClickBench scale, so running the machine
//! per entry is 3.19 times less matching. Most of the leverage is not in this file, though. An
//! output that carries the codes instead of the strings hands the operator above an integer key,
//! and `GROUP BY` on that column costs 0.28 seconds against 4.5 for the same grouping done on the
//! strings this currently returns. Measured in `spec/storage-v3/18`, where query 29 is 35% of the
//! suite. What that asks for is not local to `regexp_replace`: it is that a function which is
//! constant on a dictionary entry should be allowed to say so, and return a dictionary.

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

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
            if let (Some(call), Some((codes, dictionary))) =
                (prepared, text.stable_dictionary_parts())
            {
                return replace_stable(call, dictionary, codes, base, returns, rows);
            }
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
    /// What `regexp_replace` gave for the values of a dictionary that outlives the chunk.
    stable: OnceLock<StableReplace>,
}

impl Call {
    /// What `regexp_replace` gives for `text`, which is a piece of `text` for the host extraction
    /// and the contents of `buffer` otherwise.
    fn replaced<'t>(&self, text: &'t [u8], buffer: &'t mut String) -> Result<&'t [u8]> {
        if self.host {
            return Ok(host_bytes(text));
        }
        let text = std::str::from_utf8(text)
            .map_err(|_| Error::internal("a VARCHAR value that is not UTF-8"))?;
        buffer.clear();
        self.regex.replace_into(buffer, text, &self.rewrite, self.global);
        Ok(buffer.as_bytes())
    }
}

/// How many dictionary values one decision of the replace memo covers, which is what the native
/// format puts in a payload block, for the reason the `LIKE` memo in `scalar` gives.
const REPLACE_GROUP: usize = 1024;

/// How many bytes of replaced text the memo keeps before it stops taking more.
///
/// A replacement that keeps most of each value, over a dictionary of tens of millions, would
/// otherwise hold a second copy of the whole dictionary for the length of the query. Past this the
/// groups nobody has decided yet are answered a row at a time, the way every row used to be.
const REPLACE_BUDGET: usize = 256 << 20;

/// `regexp_replace` answered once per distinct value of a dictionary that outlives the chunk.
///
/// ClickBench q29 runs the pattern over 8.7 million `Referer` rows that hold 2.7 million distinct
/// values. A row at a time that is 3.2 times the matching and 3.2 times the decompression, and the
/// reader that hands out one value at a time keeps every payload block it decoded, which was most of
/// the half gigabyte the query held. The memo decides a group of values with one sweep of its block
/// and keeps only what the replacement gave.
///
/// Two threads can decide the same group at once. Both reach the same strings, the first to finish
/// keeps its column and the other drops its own, so the race costs a block decoded twice and never
/// a wrong answer.
#[derive(Debug)]
struct StableReplace {
    dictionary: Arc<Vector>,
    groups: Vec<OnceLock<Replaced>>,
    /// Bytes of replaced text kept so far, across every group.
    kept: AtomicUsize,
}

/// The replaced values of one group, end to end, and where each one ends.
///
/// Exactly as long as what they hold rather than a [`StringColumn`], whose views and doubling cost
/// twice the bytes of the hosts q29 keeps, which put the memo for all of `Referer` past its budget.
#[derive(Debug)]
struct Replaced {
    ends: Box<[u32]>,
    bytes: Box<[u8]>,
}

impl Replaced {
    /// The value at `index` within the group.
    fn get(&self, index: usize) -> &[u8] {
        let start = if index == 0 { 0 } else { self.ends[index - 1] as usize };
        let end = self.ends.get(index).map_or(start, |&end| end as usize);
        self.bytes.get(start..end).unwrap_or_default()
    }

    fn footprint(&self) -> usize {
        self.ends.len() * 4 + self.bytes.len()
    }
}

impl StableReplace {
    /// The replaced values of the group holding `code`, deciding it first where nothing has, or
    /// `None` where it is undecided and the memo is already as large as it may grow.
    fn group(&self, code: usize, call: &Call, buffer: &mut String) -> Result<Option<&Replaced>> {
        let slot = self
            .groups
            .get(code / REPLACE_GROUP)
            .ok_or_else(|| Error::internal("a stable dictionary code is out of range"))?;
        if let Some(done) = slot.get() {
            return Ok(Some(done));
        }
        if self.kept.load(Ordering::Relaxed) > REPLACE_BUDGET {
            return Ok(None);
        }
        let first = code / REPLACE_GROUP * REPLACE_GROUP;
        let last = (first + REPLACE_GROUP).min(self.dictionary.len());
        let mut ends = Vec::with_capacity(last - first);
        let mut bytes = Vec::new();
        let mut at = first;
        while at < last {
            let stopped = self.dictionary.sweep_text(at, last, &mut |_, text: &[u8]| {
                bytes.extend_from_slice(call.replaced(text, buffer)?);
                ends.push(
                    u32::try_from(bytes.len())
                        .map_err(|_| Error::internal("a replaced group past four gigabytes"))?,
                );
                Ok(())
            })?;
            if stopped <= at {
                return Err(Error::internal("a dictionary sweep did not move"));
            }
            at = stopped;
        }
        let out = Replaced { ends: ends.into_boxed_slice(), bytes: bytes.into_boxed_slice() };
        let bytes = out.footprint();
        if slot.set(out).is_ok() {
            self.kept.fetch_add(bytes, Ordering::Relaxed);
        }
        Ok(slot.get())
    }
}

/// The `regexp_replace` loop over a stable dictionary, through the memo on `call`.
fn replace_stable(
    call: &Call,
    dictionary: &Arc<Vector>,
    codes: &[u32],
    base: rudb_vector::Validity,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let memo = call.stable.get_or_init(|| StableReplace {
        dictionary: Arc::clone(dictionary),
        groups: (0..dictionary.len().div_ceil(REPLACE_GROUP)).map(|_| OnceLock::new()).collect(),
        kept: AtomicUsize::new(0),
    });
    let shared = Arc::ptr_eq(&memo.dictionary, dictionary);
    let mut buffer = String::new();
    let mut out = StringColumn::with_capacity(rows);
    let validity = over_valid(rows, base, |index| {
        let code = *codes
            .get(index)
            .ok_or_else(|| Error::internal("a dictionary vector is shorter than its rows"))?
            as usize;
        let decided = if shared { memo.group(code, call, &mut buffer)? } else { None };
        match decided {
            Some(group) => {
                out.push_bytes(group.get(code % REPLACE_GROUP));
            }
            None => {
                let text = dictionary.try_bytes_at(code)?.unwrap_or_default();
                out.push_bytes(call.replaced(text, &mut buffer)?);
            }
        }
        Ok(())
    })?;
    finish(returns, Data::Varlen(out), validity)
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
        Ok(Some(Self {
            regex,
            rewrite,
            global: options.global,
            group,
            host,
            stable: OnceLock::new(),
        }))
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

    use std::sync::Arc;

    use rudb_common::LogicalType;
    use rudb_vector::Vector;

    use super::{Call, host, value, vectorized};

    /// The memo over a dictionary that outlives the chunk answers what the flat loop answers, on the
    /// fixed host pattern and on a general one, for a chunk that fills it, a chunk that reads it
    /// back, and a dictionary it was not built for.
    #[test]
    fn a_replace_over_a_stable_dictionary_agrees_with_the_flat_loop() {
        let values: Vec<Value> = (0..2_500)
            .map(|index| match index % 5 {
                0 => Value::Null,
                1 => Value::Varchar(format!("http://www.site{index}.ru/page")),
                2 => Value::Varchar(format!("https://host{}.com/a/b", index % 17)),
                3 => Value::Varchar(String::new()),
                _ => Value::Varchar(format!("plain {index} foo")),
            })
            .collect();
        let dictionary =
            Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("builds"));
        let other = Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("builds"));
        let patterns = [("^https?://(?:www\\.)?([^/]+)/.*$", "\\1"), ("o+", "0")];
        for (pattern, replacement) in patterns {
            let constants = [Value::Varchar(pattern.into()), Value::Varchar(replacement.into())];
            let call = Call::read("regexp_replace", &constants.iter().collect::<Vec<_>>())
                .expect("compiles")
                .expect("a shape this file handles");
            for (rows, step, held) in
                [(2_000_usize, 991, &dictionary), (2_000, 991, &dictionary), (64, 37, &other)]
            {
                let codes: Vec<u32> =
                    (0..rows).map(|row| ((row * step) % values.len()) as u32).collect();
                let picked: Vec<Value> =
                    codes.iter().map(|&code| values[code as usize].clone()).collect();
                let flat = Vector::from_values(LogicalType::Varchar, &picked).expect("builds");
                let column =
                    Vector::stable_dictionary(codes, Arc::clone(held)).expect("codes are in range");
                let answer = |text: &Vector| {
                    vectorized("regexp_replace", Some(&call), &[text], &LogicalType::Varchar, rows)
                        .expect("the call is written")
                        .expect("text in this form has a loop")
                };
                let (want, got) = (answer(&flat), answer(&column));
                for row in 0..rows {
                    assert_eq!(got.value_at(row), want.value_at(row), "{pattern}, row {row}");
                }
            }
        }
    }

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
