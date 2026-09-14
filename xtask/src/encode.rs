//! What the encoder costs, per column and per candidate, on a real file.
//!
//! F2's first exit criterion is a load time: ClickBench `hits` has to come in under 252 seconds,
//! which is twice what DuckDB takes. The encoder is roughly eighty times away from that, and the
//! milestone says in its own words that the first thing to do is profile it rather than redesign
//! it. This is that instrument.
//!
//! # What it is measuring, and why the answer is two tables
//!
//! `rudb_encoding::string::encode` and `rudb_encoding::integer::encode` are exhaustive choosers.
//! They encode every candidate that applies, in full, and keep whichever came out smallest, and the
//! candidates that build a dictionary or a front coded chain go back through the same chooser on
//! their own output. So the cost of encoding a chunk is not the cost of the encoding that wins. It
//! is the sum of the costs of every encoding that was offered, and the one that wins is often not
//! the expensive one.
//!
//! That is a claim and the whole point of this table is to stop it being a claim. The first table
//! is the headline: megabytes a second per column, which is the number criterion one is measured
//! in. The second one splits the same seconds across the candidates that spent them, so the size
//! the chooser buys and the time it costs are next to each other. If the chooser is the cost then
//! a sampled chooser is the fix and the ablation is how much size it gives up, which is what F2
//! already says it wants to build. If the encoders are the cost then sampling saves nothing and
//! the work is somewhere else entirely.
//!
//! # The file
//!
//! The committed `crates/rudb/testdata/hits.parquet` by default, which is ten thousand rows of
//! ClickBench and is real data rather than a generator's idea of it. A path can be given instead,
//! because ten thousand rows of a column with a hundred thousand distinct values is not the same
//! shape as a million, and the candidates that get offered depend on exactly that.
//!
//! Each column is read with its own query, so the file is walked once per column. That is slower
//! than reading it once and holding all of it, and it is what keeps a hundred and five columns of
//! a million row file inside a laptop. The read is not timed either way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use rudb::Database;
use rudb_common::{LogicalType, Value};
use rudb_encoding::chooser::{Chooser, EXHAUSTIVE, Sampled};
use rudb_encoding::{integer, string};

use crate::timing::{build_line, percentile, rebuild};

/// The file this reads when nobody names one.
const FIXTURE: &str = "crates/rudb/testdata/hits.parquet";

/// How many values go into one encoded chunk.
///
/// The row group size the shell prints under `--print-config`, because a chunk is what a writer
/// would hand the encoder and the writer does not exist yet. It matters more here than it looks
/// like it should: every candidate the chooser offers is decided by a property of the chunk, so a
/// chunk ten times bigger has more distinct values, fewer runs and a different candidate list, and
/// the encoders that are quadratic in anything are quadratic in this.
const ROW_GROUP: usize = 122_880;

/// How many times the headline pass is taken, so the throughput is a median and not one sample.
///
/// The same count every other table here uses, and the spread beside it is the same interquartile
/// range, so a reader who has learned to distrust a row with a double figure IQR in `cargo xtask
/// kernels` distrusts the same thing here. What is not shared is [`crate::timing::time`], because
/// that calibrates an inner loop to run for ten milliseconds and one pass over a column already
/// runs for longer than that.
const REPEATS: usize = crate::timing::SAMPLES;

/// How many columns the first table prints without `--all`.
const SHOWN: usize = 20;

/// What one column cost.
struct Column {
    name: String,
    kind: &'static str,
    rows: usize,
    /// The bytes the values occupy before anything is done to them.
    raw: usize,
    /// The bytes every chunk of it encoded to.
    encoded: usize,
    /// Nanoseconds for one pass over the whole column, the median of [`REPEATS`].
    nanos: f64,
    /// The spread of those passes, as a fraction of the median.
    spread: f64,
    /// The shape the first chunk came out as.
    shape: String,
}

/// What one candidate cost across every chunk of every column.
/// One column under both choosers, which is what the ablation is.
struct Pair {
    name: String,
    kind: &'static str,
    raw: usize,
    reference: Side,
    alternative: Side,
}

/// What one chooser did to one column.
struct Side {
    bytes: usize,
    nanos: f64,
    shape: String,
}

#[derive(Default)]
struct Candidate {
    /// How many chunks offered it.
    offered: usize,
    /// How many chunks it came out smallest on at the top level of the chunk.
    ///
    /// Top level only. A candidate that recurses hands its own output back to the chooser, so
    /// `FRONT` can be in the shape of every chunk in a column and be kept zero times here, because
    /// what won at the top was the `DICT` that called it. That is the number to read next to the
    /// seconds: a candidate with seconds and no keeps is one the chooser paid for and threw away.
    won: usize,
    /// Nanoseconds spent encoding it, whether it won or not.
    nanos: f64,
}

/// A column's values, in the form the encoder for its type takes.
enum Values {
    Text(Vec<Vec<u8>>),
    Numbers(Vec<i64>),
}

impl Values {
    fn len(&self) -> usize {
        match self {
            Self::Text(values) => values.len(),
            Self::Numbers(values) => values.len(),
        }
    }

    /// What the values occupy before the encoder sees them.
    ///
    /// The bytes for text, and eight per value for numbers. Eight rather than the width of the
    /// declared type, because that is what the encoder is handed and a ratio against anything else
    /// would be a ratio against a number nobody passed in.
    fn raw(&self) -> usize {
        match self {
            Self::Text(values) => values.iter().map(Vec::len).sum(),
            Self::Numbers(values) => values.len() * 8,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "string",
            Self::Numbers(_) => "integer",
        }
    }
}

/// Runs the tables.
///
/// # Errors
///
/// If the file cannot be read, or if an encoder refuses a chunk of a real column, which would be a
/// bug rather than a condition.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return rebuild(root, "encode", args);
    }

    let all = args.iter().any(|arg| arg == "--all");
    let ablate = args.iter().any(|arg| arg == "--ablate");
    let threads = threads(args)?;
    let path = match given(args) {
        Some(given) => PathBuf::from(given),
        None => root.join(FIXTURE),
    };
    if !path.exists() {
        return Err(format!("{} is not there", path.display()));
    }

    let db = Database::new();
    let source = source(&path);
    let schema = db
        .query(&format!("SELECT * FROM {source} LIMIT 0"))
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let names: Vec<String> = schema.names().to_vec();
    let types: Vec<LogicalType> = schema.types().to_vec();

    // The scaling sweep needs every column at once, because the work it hands to threads is one
    // chunk of one column and there have to be enough of those in hand to keep the threads busy.
    // The per column table does not, so in that mode a column is measured and dropped, which is
    // what keeps a hundred and five columns of a big file inside a laptop.
    let mut held = Vec::new();
    let mut pairs = Vec::new();
    let mut columns = Vec::new();
    let mut candidates: BTreeMap<(&'static str, &'static str), Candidate> = BTreeMap::new();
    let mut skipped = Vec::new();
    for (name, logical) in names.iter().zip(&types) {
        let Some(values) = read(&db, &source, name, logical)? else {
            skipped.push(format!("{name} ({logical})"));
            continue;
        };
        if values.len() == 0 {
            skipped.push(format!("{name} (no rows)"));
            continue;
        }
        if let Some(threads) = threads {
            let _ = threads;
            held.push(values);
            continue;
        }
        if ablate {
            pairs.push(ablate_column(name, &values)?);
            continue;
        }
        columns.push(measure(name, &values)?);
        attribute(&values, &mut candidates)?;
    }

    if let Some(threads) = threads {
        if held.is_empty() {
            return Err(format!("nothing in {} could be encoded", path.display()));
        }
        return scaling(&path, &held, threads);
    }
    if ablate {
        if pairs.is_empty() {
            return Err(format!("nothing in {} could be encoded", path.display()));
        }
        pairs.sort_by(|a, b| b.reference.nanos.total_cmp(&a.reference.nanos));
        ablation(&path, &pairs, &skipped, all);
        return Ok(());
    }
    if columns.is_empty() {
        return Err(format!("nothing in {} could be encoded", path.display()));
    }

    columns.sort_by(|a, b| b.nanos.total_cmp(&a.nanos));
    report(&path, &columns, &candidates, &skipped, all);
    Ok(())
}

/// The file somebody named, which is the one argument here that is not a flag.
///
/// The count after `--threads` is skipped rather than taken as a path, which is the whole reason
/// this is a function and not a `find`.
fn given(args: &[String]) -> Option<&String> {
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--threads" {
            rest.next();
            continue;
        }
        if !arg.starts_with("--") {
            return Some(arg);
        }
    }
    None
}

/// The top of the thread sweep, when `--threads N` asked for one.
fn threads(args: &[String]) -> Result<Option<usize>, String> {
    let Some(at) = args.iter().position(|arg| arg == "--threads") else {
        return Ok(None);
    };
    let count = args
        .get(at + 1)
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|count| *count > 0)
        .ok_or_else(|| "--threads wants a count above zero".to_string())?;
    Ok(Some(count))
}

/// The table expression every query in here reads from.
///
/// `binary_as_string` is what ClickBench's own view uses and what makes the string columns arrive
/// as text rather than as blobs, which is the form the string encoder is written for.
fn source(path: &Path) -> String {
    let quoted = path.display().to_string().replace('\'', "''");
    format!("read_parquet('{quoted}', binary_as_string=True)")
}

/// Pulls one column out, or `None` when its type is not one of the two the encoders take.
fn read(
    db: &Database,
    source: &str,
    name: &str,
    logical: &LogicalType,
) -> Result<Option<Values>, String> {
    if !text(logical) && !number(logical) {
        return Ok(None);
    }
    let quoted = name.replace('"', "\"\"");
    let result = db
        .query(&format!("SELECT \"{quoted}\" FROM {source}"))
        .map_err(|e| format!("could not read {name}: {e}"))?;

    // A null becomes the zero of its type, because validity is a bitmap the column owns rather
    // than something either encoder knows about. See the note at the top of
    // `rudb-encoding/src/string.rs`: a chunk there is N byte strings and an empty one is a value
    // like any other. What that costs here is that a column that is mostly null measures as a
    // column that is mostly the same value, which is a shape the chooser is fast on. The columns
    // that matter in `hits` have no nulls in them at all.
    if text(logical) {
        let mut values = Vec::with_capacity(result.len());
        for value in result.column(0) {
            values.push(match value {
                Value::Varchar(text) => text.into_bytes(),
                Value::Blob(bytes) => bytes,
                _ => Vec::new(),
            });
        }
        return Ok(Some(Values::Text(values)));
    }
    let mut values = Vec::with_capacity(result.len());
    for value in result.column(0) {
        values.push(as_i64(&value));
    }
    Ok(Some(Values::Numbers(values)))
}

fn text(logical: &LogicalType) -> bool {
    matches!(logical, LogicalType::Varchar | LogicalType::Blob)
}

fn number(logical: &LogicalType) -> bool {
    matches!(
        logical,
        LogicalType::Boolean
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::Date
            | LogicalType::Time
            | LogicalType::Timestamp
            | LogicalType::TimestampS
            | LogicalType::TimestampMs
            | LogicalType::TimestampNs
    )
}

/// The integer the encoder is handed for a value of one of the types above.
///
/// `UBigInt` is saturating rather than wrapping, because a value above `i64::MAX` in a column of
/// counters would otherwise arrive as a negative one and turn a column with no deltas worth having
/// into one that looks like it has enormous ones. Nothing in `hits` reaches it.
fn as_i64(value: &Value) -> i64 {
    match value {
        Value::Boolean(flag) => i64::from(*flag),
        Value::TinyInt(number) => i64::from(*number),
        Value::SmallInt(number) => i64::from(*number),
        Value::Integer(number) => i64::from(*number),
        Value::BigInt(number) => *number,
        Value::UTinyInt(number) => i64::from(*number),
        Value::USmallInt(number) => i64::from(*number),
        Value::UInteger(number) => i64::from(*number),
        Value::UBigInt(number) => i64::try_from(*number).unwrap_or(i64::MAX),
        Value::Date(days) => i64::from(*days),
        Value::Time(micros) | Value::Timestamp(micros) => *micros,
        _ => 0,
    }
}

/// How the encode scales across cores, which is the assumption F2's load time rests on.
///
/// The arithmetic behind criterion 1 is that one core encodes `hits` in about 4,500 seconds and a
/// budget of 252 seconds on a 32 thread machine is 8,064 core seconds, so the encoder fits with
/// room for the rest of the loader. Every word of that depends on the encode scaling with cores,
/// and an encoder that allocates as much as this one does is exactly the kind that does not. So the
/// assumption gets measured rather than assumed.
///
/// The unit of work is one chunk of one column, which is the unit F2's own checklist names when it
/// says the write path should be parallel by block and by column. Threads take units off a shared
/// counter, largest first, so the tail is a small unit rather than a whole `URL` column.
fn scaling(path: &Path, held: &[Values], threads: usize) -> Result<(), String> {
    let mut work: Vec<(usize, usize)> = Vec::new();
    for (column, values) in held.iter().enumerate() {
        for chunk in 0..chunks(values.len()) {
            work.push((column, chunk));
        }
    }
    work.sort_by_key(|&(column, chunk)| std::cmp::Reverse(weight(&held[column], chunk)));
    let raw: usize = held.iter().map(Values::raw).sum();

    let mut counts = Vec::new();
    let mut count = 1;
    while count < threads {
        counts.push(count);
        count *= 2;
    }
    counts.push(threads);

    println!();
    println!("how the encode scales, over {}", path.display());
    println!("  {}", build_line());
    println!(
        "  {ROW_GROUP} values a chunk, {} chunks over {} columns, {REPEATS} passes",
        work.len(),
        held.len()
    );
    println!();
    println!(
        "{:>8} {:>11} {:>11} {:>10} {:>12} {:>6}",
        "threads", "wall s", "MiB/s", "speedup", "efficiency", "IQR"
    );
    let mut one = 0.0;
    for &count in &counts {
        let mut passes = Vec::with_capacity(REPEATS);
        for _ in 0..REPEATS {
            passes.push(pass(held, &work, count)?);
        }
        passes.sort_by(f64::total_cmp);
        let median = percentile(&passes, 50);
        let iqr = percentile(&passes, 75) - percentile(&passes, 25);
        if count == 1 {
            one = median;
        }
        let speedup = if median == 0.0 { 0.0 } else { one / median };
        println!(
            "{:>8} {:>11.3} {:>11.1} {:>10.1} {:>11.0}% {:>5.0}%",
            count,
            median / 1e9,
            rate(raw, median),
            speedup,
            speedup / count as f64 * 100.0,
            if median == 0.0 { 0.0 } else { iqr / median * 100.0 }
        );
    }

    println!();
    println!("caveats");
    println!("  rule two: every row is the median of {REPEATS} passes over the whole set.");
    println!("  rule seven: this is one machine, so do not put it next to a number from another.");
    println!("  rule ten: the end to end number this explains is F2's first exit criterion, which");
    println!("    is hits loading in under 252 seconds. What this row says is how much of the");
    println!("    single core time a loader gets to divide, and nothing about reading the Parquet");
    println!("    or writing the blocks, neither of which happens here.");
    println!("  the work is held in memory before the sweep starts and the read is not timed, so");
    println!("    this is the encode on its own and not a load.");
    println!("  efficiency below a hundred at high thread counts on a machine with fewer real");
    println!("    cores than threads is the machine and not the encoder. Check the core count");
    println!("    before reading anything into it.");
    Ok(())
}

/// One pass over every chunk of every column at a given thread count, in nanoseconds.
fn pass(held: &[Values], work: &[(usize, usize)], threads: usize) -> Result<f64, String> {
    let next = AtomicUsize::new(0);
    let start = Instant::now();
    let failure: Result<(), String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| -> Result<(), String> {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(column, chunk)) = work.get(at) else {
                            return Ok(());
                        };
                        let bytes = encode(&held[column], chunk)?;
                        std::hint::black_box(&bytes);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().map_err(|_| "an encode thread panicked".to_string())??;
        }
        Ok(())
    });
    failure?;
    Ok(start.elapsed().as_nanos() as f64)
}

/// How much of a column one chunk holds, which is what the work list is ordered by.
fn weight(values: &Values, chunk: usize) -> usize {
    match values {
        Values::Text(all) => cut(all, chunk).iter().map(Vec::len).sum(),
        Values::Numbers(all) => cut(all, chunk).len() * 8,
    }
}

/// The headline pass: how long the chooser takes over the whole column, [`REPEATS`] times.
fn measure(name: &str, values: &Values) -> Result<Column, String> {
    let mut passes = Vec::with_capacity(REPEATS);
    let mut encoded = 0;
    let mut shape = String::new();
    for pass in 0..REPEATS {
        let start = Instant::now();
        let mut total = 0;
        let mut first = String::new();
        for chunk in 0..chunks(values.len()) {
            let bytes = encode(values, chunk)?;
            if first.is_empty() {
                first = describe(values, &bytes)?;
            }
            total += bytes.len();
        }
        passes.push(start.elapsed().as_nanos() as f64);
        if pass == 0 {
            encoded = total;
            shape = first;
        } else if total != encoded {
            return Err(format!("{name} encoded to {total} bytes and then to {encoded}"));
        }
    }
    passes.sort_by(f64::total_cmp);
    let median = percentile(&passes, 50);
    let iqr = percentile(&passes, 75) - percentile(&passes, 25);
    let spread = if median == 0.0 { 0.0 } else { iqr / median };
    Ok(Column {
        name: name.to_string(),
        kind: values.kind(),
        rows: values.len(),
        raw: values.raw(),
        encoded,
        nanos: median,
        spread,
        shape,
    })
}

/// The attribution pass: the same chunks again, one candidate at a time.
///
/// Once rather than [`REPEATS`] times, because this is a split of seconds across candidates and not
/// a throughput number, and the split does not move between passes the way a total does.
fn attribute(
    values: &Values,
    into: &mut BTreeMap<(&'static str, &'static str), Candidate>,
) -> Result<(), String> {
    for chunk in 0..chunks(values.len()) {
        let offered = offered(values, chunk);
        let mut smallest: Option<(&'static str, usize)> = None;
        let mut spent: Vec<(&'static str, f64)> = Vec::with_capacity(offered.len());
        for name in &offered {
            let start = Instant::now();
            let size = encode_only(values, chunk, name)?;
            spent.push((name, start.elapsed().as_nanos() as f64));
            let Some(size) = size else { continue };
            if smallest.is_none_or(|(_, best)| size < best) {
                smallest = Some((name, size));
            }
        }
        for (name, nanos) in spent {
            let entry = into.entry((values.kind(), name)).or_default();
            entry.offered += 1;
            entry.nanos += nanos;
            if smallest.is_some_and(|(won, _)| won == name) {
                entry.won += 1;
            }
        }
    }
    Ok(())
}

fn chunks(rows: usize) -> usize {
    rows.div_ceil(ROW_GROUP)
}

/// One chunk of a column, through the exhaustive chooser, which is what `encode` has always meant.
fn encode(values: &Values, chunk: usize) -> Result<Vec<u8>, String> {
    encode_using(values, chunk, &EXHAUSTIVE)
}

/// One chunk of a column, through a chooser somebody named.
fn encode_using(values: &Values, chunk: usize, chooser: &dyn Chooser) -> Result<Vec<u8>, String> {
    match values {
        Values::Text(all) => {
            let slice: Vec<&[u8]> = cut(all, chunk).iter().map(Vec::as_slice).collect();
            string::encode_with(&slice, chooser).map_err(|e| e.message().to_string())
        }
        Values::Numbers(all) => {
            integer::encode_with(cut(all, chunk), chooser).map_err(|e| e.message().to_string())
        }
    }
}

/// One column under one chooser: how long it took and how big it came out.
///
/// [`REPEATS`] passes and the median, same as [`measure`], because the whole point of the ablation
/// is a ratio of two times and a ratio of two noisy numbers is noisier than either.
fn side(name: &str, values: &Values, chooser: &dyn Chooser) -> Result<Side, String> {
    let mut passes = Vec::with_capacity(REPEATS);
    let mut bytes = 0;
    let mut shape = String::new();
    for pass in 0..REPEATS {
        let start = Instant::now();
        let mut total = 0;
        let mut first = String::new();
        for chunk in 0..chunks(values.len()) {
            let encoded = encode_using(values, chunk, chooser)?;
            if first.is_empty() {
                first = describe(values, &encoded)?;
            }
            total += encoded.len();
        }
        passes.push(start.elapsed().as_nanos() as f64);
        if pass == 0 {
            bytes = total;
            shape = first;
        } else if total != bytes {
            return Err(format!(
                "{name} under {} encoded to {total} bytes and then to {bytes}",
                chooser.name()
            ));
        }
    }
    passes.sort_by(f64::total_cmp);
    Ok(Side { bytes, nanos: percentile(&passes, 50), shape })
}

/// One column under both choosers, which is the whole of what the ablation measures.
///
/// The sampled side runs first. If it ran second it would find the column in cache with the
/// exhaustive side's work still warm, and the thing being measured is which one is cheaper.
fn ablate_column(name: &str, values: &Values) -> Result<Pair, String> {
    let sampled = Sampled::new();
    let alternative = side(name, values, &sampled)?;
    let reference = side(name, values, &EXHAUSTIVE)?;
    Ok(Pair {
        name: name.to_string(),
        kind: values.kind(),
        raw: values.raw(),
        reference,
        alternative,
    })
}

/// One chunk of a column, through one candidate. `None` when that candidate does not apply.
fn encode_only(values: &Values, chunk: usize, name: &str) -> Result<Option<usize>, String> {
    let bytes = match values {
        Values::Text(all) => {
            let slice: Vec<&[u8]> = cut(all, chunk).iter().map(Vec::as_slice).collect();
            let kind = string::offered(&slice)
                .into_iter()
                .find(|kind| kind.name() == name)
                .ok_or_else(|| format!("{name} was offered and then was not"))?;
            string::encode_only(kind, &slice)
        }
        Values::Numbers(all) => {
            let slice = cut(all, chunk);
            let kind = integer::offered(slice)
                .into_iter()
                .find(|kind| kind.name() == name)
                .ok_or_else(|| format!("{name} was offered and then was not"))?;
            integer::encode_only(kind, slice)
        }
    };
    bytes.map(|bytes| bytes.map(|bytes| bytes.len())).map_err(|e| e.message().to_string())
}

/// The candidates the chooser will try on one chunk, by name.
fn offered(values: &Values, chunk: usize) -> Vec<&'static str> {
    match values {
        Values::Text(all) => {
            let slice: Vec<&[u8]> = cut(all, chunk).iter().map(Vec::as_slice).collect();
            string::offered(&slice).into_iter().map(string::Kind::name).collect()
        }
        Values::Numbers(all) => {
            integer::offered(cut(all, chunk)).into_iter().map(integer::Kind::name).collect()
        }
    }
}

fn describe(values: &Values, bytes: &[u8]) -> Result<String, String> {
    let text = match values {
        Values::Text(_) => string::describe(bytes),
        Values::Numbers(_) => integer::describe(bytes),
    };
    text.map_err(|e| e.message().to_string())
}

fn cut<T>(values: &[T], chunk: usize) -> &[T] {
    let from = chunk * ROW_GROUP;
    &values[from..(from + ROW_GROUP).min(values.len())]
}

/// Both tables and the caveats under them.
fn report(
    path: &Path,
    columns: &[Column],
    candidates: &BTreeMap<(&'static str, &'static str), Candidate>,
    skipped: &[String],
    all: bool,
) {
    let raw: usize = columns.iter().map(|column| column.raw).sum();
    let encoded: usize = columns.iter().map(|column| column.encoded).sum();
    let nanos: f64 = columns.iter().map(|column| column.nanos).sum();

    println!();
    println!("what the encoder costs, over {}", path.display());
    println!("  {}", build_line());
    println!("  {ROW_GROUP} values a chunk, {REPEATS} passes, one thread");
    println!();
    println!(
        "{:<24} {:>8} {:>9} {:>10} {:>8} {:>9} {:>6}  shape",
        "column", "kind", "rows", "raw MiB", "ratio", "MiB/s", "IQR"
    );
    for column in columns.iter().take(if all { columns.len() } else { SHOWN }) {
        println!(
            "{:<24} {:>8} {:>9} {:>10.2} {:>8.1} {:>9.1} {:>5.0}%  {}",
            cap(&column.name, 24),
            column.kind,
            column.rows,
            mib(column.raw),
            ratio(column.raw, column.encoded),
            rate(column.raw, column.nanos),
            column.spread * 100.0,
            cap(&column.shape, 44)
        );
    }
    if !all && columns.len() > SHOWN {
        println!("{:<24} {} more, --all prints them", "...", columns.len() - SHOWN);
    }
    println!(
        "{:<24} {:>8} {:>9} {:>10.2} {:>8.1} {:>9.1}",
        "the whole file",
        "",
        columns.iter().map(|column| column.rows).max().unwrap_or(0),
        mib(raw),
        ratio(raw, encoded),
        rate(raw, nanos)
    );
    if !skipped.is_empty() {
        println!();
        println!("not encoded, because neither encoder takes the type: {}", skipped.join(", "));
    }

    println!();
    println!("where the chooser's time went");
    println!();
    println!(
        "{:<10} {:<14} {:>9} {:>7} {:>11} {:>9}",
        "encoder", "candidate", "offered", "kept", "seconds", "share"
    );
    let total: f64 = candidates.values().map(|candidate| candidate.nanos).sum();
    let mut rows: Vec<_> = candidates.iter().collect();
    rows.sort_by(|a, b| b.1.nanos.total_cmp(&a.1.nanos));
    for ((encoder, name), candidate) in rows {
        println!(
            "{:<10} {:<14} {:>9} {:>7} {:>11.3} {:>8.1}%",
            encoder,
            name,
            candidate.offered,
            candidate.won,
            candidate.nanos / 1e9,
            if total == 0.0 { 0.0 } else { candidate.nanos / total * 100.0 }
        );
    }

    println!();
    println!("caveats");
    println!(
        "  rule two: the MiB/s column is the median of {REPEATS} passes and the spread beside"
    );
    println!("    it is the range over that median. The second table is one pass, because it is a");
    println!("    split of seconds across candidates rather than a throughput number.");
    println!("  rule seven: this is one machine and one file, so do not put it next to a number");
    println!("    from another. The candidate list depends on the data, so a bigger file does not");
    println!("    just scale this, it changes which rows exist.");
    println!("  rule ten: the end to end number this explains is F2's first exit criterion, which");
    println!("    is ClickBench hits loading in under 252 seconds. That is 14.8 GB of Parquet, so");
    println!("    the MiB/s on the whole file line is the number to multiply out.");
    println!("  the two tables do not add up to the same seconds on purpose. The first one times");
    println!("    the chooser, which encodes every candidate and keeps one. The second one times");
    println!("    the candidates one at a time, so a candidate that recurses is counted once here");
    println!("    and once inside whatever called it.");
    println!(
        "  kept counts the top level of a chunk only. A candidate that recurses hands its own"
    );
    println!("    output back to the chooser, so FRONT can be in the shape of every chunk and be");
    println!("    kept zero times, because what won at the top was the DICT that called it. The");
    println!(
        "    row to look at is one with seconds and no keeps: that is search the chooser paid"
    );
    println!("    for and threw away.");
    println!("  raw bytes for a string column are the value bytes and for an integer column are");
    println!("    eight a value, which is what the encoder is handed rather than what the Parquet");
    println!("    file holds. The ratio is against that and not against the file on disk.");
}

/// What the sampled chooser saves and what it gives up, per column and over the file.
fn ablation(path: &Path, pairs: &[Pair], skipped: &[String], all: bool) {
    let raw: usize = pairs.iter().map(|pair| pair.raw).sum();
    let slow: f64 = pairs.iter().map(|pair| pair.reference.nanos).sum();
    let fast: f64 = pairs.iter().map(|pair| pair.alternative.nanos).sum();
    let big: usize = pairs.iter().map(|pair| pair.reference.bytes).sum();
    let small: usize = pairs.iter().map(|pair| pair.alternative.bytes).sum();

    println!();
    println!("what sampling the chooser buys, over {}", path.display());
    println!("  {}", build_line());
    println!("  {ROW_GROUP} values a chunk, {REPEATS} passes each side, one thread");
    println!("  the sample is {} values, in windows of {}", Sampled::new().size(), 1024);
    println!();
    println!(
        "{:<24} {:>8} {:>10} {:>10} {:>8} {:>11} {:>11} {:>7}",
        "column", "kind", "slow MiB/s", "fast MiB/s", "faster", "slow bytes", "fast bytes", "cost"
    );
    let mut changed = 0;
    for pair in pairs.iter().take(if all { pairs.len() } else { SHOWN }) {
        if pair.reference.shape != pair.alternative.shape {
            changed += 1;
        }
        println!(
            "{:<24} {:>8} {:>10.1} {:>10.1} {:>7.2}x {:>11} {:>11} {:>6.2}%",
            cap(&pair.name, 24),
            pair.kind,
            rate(pair.raw, pair.reference.nanos),
            rate(pair.raw, pair.alternative.nanos),
            speedup(pair.reference.nanos, pair.alternative.nanos),
            pair.reference.bytes,
            pair.alternative.bytes,
            cost(pair.reference.bytes, pair.alternative.bytes)
        );
    }
    if !all && pairs.len() > SHOWN {
        println!("{:<24} {} more, --all prints them", "...", pairs.len() - SHOWN);
    }
    println!(
        "{:<24} {:>8} {:>10.1} {:>10.1} {:>7.2}x {:>11} {:>11} {:>6.2}%",
        "the whole file",
        "",
        rate(raw, slow),
        rate(raw, fast),
        speedup(slow, fast),
        big,
        small,
        cost(big, small)
    );
    println!();
    println!(
        "  ratio is {:.2} to one exhaustive and {:.2} to one sampled, over {:.2} raw MiB",
        ratio(raw, big),
        ratio(raw, small),
        mib(raw)
    );
    println!(
        "  the two choosers picked a different top level shape on {changed} of the {} columns \
         printed",
        pairs.len().min(if all { pairs.len() } else { SHOWN })
    );
    if !skipped.is_empty() {
        println!();
        println!("not encoded, because neither encoder takes the type: {}", skipped.join(", "));
    }

    println!();
    println!("caveats");
    println!("  rule two: every MiB/s here is the median of {REPEATS} passes. The two sides are");
    println!(
        "    measured in the same process over the same values in memory, so the ratio between"
    );
    println!("    them is the number to trust and the absolute rates are the ones to compare only");
    println!("    against each other.");
    println!("  rule seven: one machine and one file. Which candidates apply depends on the data,");
    println!(
        "    so a file with different columns in it gives a different answer and not a scaled"
    );
    println!("    version of this one.");
    println!("  rule ten: the end to end number is F2's first exit criterion, ClickBench hits in");
    println!("    under 252 seconds. The faster column is what the encoder's share of that budget");
    println!("    divides by and the cost column is what it is paid for in file size.");
    println!(
        "  cost is how much bigger the sampled output is, so zero means the sample agreed with"
    );
    println!("    the full search on every chunk and a negative number means it did better, which");
    println!("    happens because the exhaustive chooser is greedy per level and not globally");
    println!("    optimal over the cascade.");
    println!(
        "  the sampled side runs first on each column so the exhaustive side cannot be the one"
    );
    println!("    that pays for the cache misses.");
}

/// How many times faster the second is than the first.
fn speedup(slow: f64, fast: f64) -> f64 {
    if fast == 0.0 { 0.0 } else { slow / fast }
}

/// How much bigger the sampled output is, as a percentage of the exhaustive one.
fn cost(reference: usize, alternative: usize) -> f64 {
    if reference == 0 {
        return 0.0;
    }
    (alternative as f64 - reference as f64) / reference as f64 * 100.0
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

fn rate(bytes: usize, nanos: f64) -> f64 {
    if nanos == 0.0 { 0.0 } else { mib(bytes) / (nanos / 1e9) }
}

fn ratio(raw: usize, encoded: usize) -> f64 {
    if encoded == 0 { 0.0 } else { raw as f64 / encoded as f64 }
}

fn cap(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars().take(width - 1).collect::<String>() + "~"
}

#[cfg(test)]
mod tests {
    use super::{Values, as_i64, cap, chunks, cut, ratio};
    use rudb_common::Value;

    fn args(given: &[&str]) -> Vec<String> {
        given.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn the_count_after_threads_is_not_mistaken_for_the_file() {
        // The one argument here that is not a flag is the path, and `--threads 32` puts a bare 32
        // in the middle of the list, which a plain search for the first non flag would pick up and
        // then fail to open.
        assert_eq!(super::given(&args(&["--threads", "32"])), None);
        assert_eq!(
            super::given(&args(&["--threads", "32", "hits.parquet"])),
            Some(&"hits.parquet".to_string())
        );
        assert_eq!(
            super::given(&args(&["hits.parquet", "--threads", "32"])),
            Some(&"hits.parquet".to_string())
        );
    }

    #[test]
    fn a_thread_count_that_is_not_a_count_is_refused_rather_than_ignored() {
        assert_eq!(super::threads(&args(&["--all"])), Ok(None));
        assert_eq!(super::threads(&args(&["--threads", "4"])), Ok(Some(4)));
        assert!(super::threads(&args(&["--threads"])).is_err());
        assert!(super::threads(&args(&["--threads", "0"])).is_err());
        assert!(super::threads(&args(&["--threads", "lots"])).is_err());
    }

    #[test]
    fn a_column_shorter_than_a_chunk_is_still_one_chunk() {
        assert_eq!(chunks(0), 0);
        assert_eq!(chunks(1), 1);
        assert_eq!(chunks(super::ROW_GROUP), 1);
        assert_eq!(chunks(super::ROW_GROUP + 1), 2);
    }

    #[test]
    fn the_last_chunk_stops_at_the_end_of_the_column() {
        let values: Vec<i64> = (0..super::ROW_GROUP as i64 + 7).collect();
        assert_eq!(cut(&values, 0).len(), super::ROW_GROUP);
        assert_eq!(cut(&values, 1).len(), 7);
    }

    #[test]
    fn raw_bytes_are_the_value_bytes_for_text_and_eight_a_value_for_numbers() {
        assert_eq!(Values::Text(vec![b"ab".to_vec(), b"cde".to_vec()]).raw(), 5);
        assert_eq!(Values::Numbers(vec![1, 2, 3]).raw(), 24);
    }

    #[test]
    fn a_count_above_what_an_i64_holds_saturates_rather_than_going_negative() {
        // Otherwise a column of large unsigned counters would arrive looking like it swings across
        // zero, which changes which candidates get offered and so changes the table.
        assert_eq!(as_i64(&Value::UBigInt(u64::MAX)), i64::MAX);
        assert_eq!(as_i64(&Value::UBigInt(7)), 7);
    }

    #[test]
    fn a_ratio_against_nothing_is_not_an_infinity_in_the_table() {
        assert!((ratio(100, 0)).abs() < f64::EPSILON);
    }

    #[test]
    fn a_long_column_name_is_cut_rather_than_pushing_the_table_apart() {
        assert_eq!(cap("short", 8), "short");
        assert_eq!(cap("a name that is far too long", 8), "a name ~");
    }
}
