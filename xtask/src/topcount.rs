//! Measures the bounded top count of `spec/storage-v3/26` against the table it would replace.
//!
//! Document 26 proposes answering `GROUP BY k ORDER BY COUNT(*) DESC LIMIT n` from hash bucket
//! counts rather than from a table of every key, and projects what it would cost from document 25's
//! scan floor. A projection is what document 20 and document 23 each got wrong, so this task exists
//! to replace the projection with a number before anybody builds the operator.
//!
//! It is a harness and not the engine. It drives rudb's Parquet reader, which is the expensive half
//! of the query and is the same code the engine runs, and then does the grouping itself in one
//! thread with a plain hash table. So the absolute figures are nothing like a query's and the
//! comparison between the two modes is the whole point: both modes read the same file through the
//! same decoder, hash with the same function, and differ only in what they remember.
//!
//! `exact` holds every key, which is what the aggregate does today. `buckets` reads the file twice,
//! counting into a fixed array the first time and counting the keys of the heaviest buckets the
//! second. Peak resident comes from outside, because a process cannot watch its own high water mark
//! portably and the point of the exercise is the high water mark.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::path::Path;
use std::time::Instant;

use rudb_io::{File, Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::Reader;

/// How many counters the first pass keeps, unless `--buckets` says otherwise.
const BUCKETS: usize = 1 << 24;

/// How many of the heaviest buckets the second pass counts keys in, unless `--budget` says so.
const BUDGET: usize = 1_000;

/// How many rows the answer holds, unless `--k` says otherwise.
const ROWS: usize = 10;

/// Runs one mode over one column of one Parquet file and prints what it found and what it took.
///
/// # Errors
///
/// If the arguments do not name a file, a column and a mode this task knows, or if reading the file
/// fails anywhere the reader reports.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let settings = Settings::parse(args)?;
    let column = locate(&settings.path, &settings.column)?;
    let started = Instant::now();
    let (answer, note) = match settings.mode {
        Mode::Exact => exact(&settings, column)?,
        Mode::Buckets => buckets(&settings, column)?,
    };
    let took = started.elapsed();
    println!("{} over {} of {}", settings.mode.name(), settings.column, settings.path);
    println!("{note}");
    for (key, count) in &answer {
        let text = String::from_utf8_lossy(key);
        let shown = text.chars().take(60).collect::<String>();
        println!("{count:>12}  {shown}");
    }
    println!("took {:.2} s", took.as_secs_f64());
    Ok(())
}

/// What the command line asked for.
struct Settings {
    path: String,
    column: String,
    mode: Mode,
    buckets: usize,
    budget: usize,
    rows: usize,
}

/// Which of the two structures to measure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A table holding every key, which is what the aggregate does today.
    Exact,
    /// Bucket counts, then the keys of the heaviest buckets, then a certification.
    Buckets,
}

impl Mode {
    /// What to print for this mode.
    fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Buckets => "buckets",
        }
    }
}

impl Settings {
    /// The arguments, or a usage line saying what was wanted.
    fn parse(args: &[String]) -> Result<Self, String> {
        let usage = "usage: topcount <file.parquet> <column> \
                     [--mode exact|buckets] [--buckets N] [--budget N] [--k N]";
        let mut path = None;
        let mut column = None;
        let mut mode = Mode::Buckets;
        let mut buckets = BUCKETS;
        let mut budget = BUDGET;
        let mut rows = ROWS;
        let mut at = 0;
        while at < args.len() {
            match args[at].as_str() {
                "--mode" => {
                    mode = match value(args, &mut at, usage)?.as_str() {
                        "exact" => Mode::Exact,
                        "buckets" => Mode::Buckets,
                        other => return Err(format!("unknown mode {other}, {usage}")),
                    };
                }
                "--buckets" => buckets = number(&value(args, &mut at, usage)?, usage)?,
                "--budget" => budget = number(&value(args, &mut at, usage)?, usage)?,
                "--k" => rows = number(&value(args, &mut at, usage)?, usage)?,
                other if path.is_none() => path = Some(other.to_string()),
                other if column.is_none() => column = Some(other.to_string()),
                other => return Err(format!("unexpected argument {other}, {usage}")),
            }
            at += 1;
        }
        let (Some(path), Some(column)) = (path, column) else {
            return Err(usage.to_string());
        };
        if buckets == 0 || budget == 0 || rows == 0 {
            return Err(format!("every count has to be positive, {usage}"));
        }
        Ok(Self { path, column, mode, buckets, budget, rows })
    }
}

/// The argument after a flag.
fn value(args: &[String], at: &mut usize, usage: &str) -> Result<String, String> {
    *at += 1;
    args.get(*at).cloned().ok_or_else(|| format!("{} wants a value, {usage}", args[*at - 1]))
}

/// A positive count out of the command line.
fn number(text: &str, usage: &str) -> Result<usize, String> {
    text.parse().map_err(|_| format!("{text} is not a count, {usage}"))
}

/// The index of the named column in the file's schema.
fn locate(path: &str, column: &str) -> Result<usize, String> {
    let reader = open(path)?;
    let fields = reader.fields();
    fields
        .iter()
        .position(|field| field.name == column)
        .ok_or_else(|| format!("{path} has no column named {column}"))
}

/// A reader over the file, positioned before its first row group.
fn open(path: &str) -> Result<Reader, String> {
    let filesystem = RealFilesystem::new();
    let at = Path::new(path);
    if !filesystem.exists(at) {
        return Err(format!("no file at {path}"));
    }
    let file: Box<dyn File> =
        filesystem.open(at, OpenMode::Read).map_err(|error| error.to_string())?;
    Reader::open(file).map_err(|error| error.to_string())
}

/// Every non empty value of one column, in file order, handed to `body` as bytes.
///
/// Empty is the filter every one of the queries in question carries, and skipping it here is the
/// `WHERE Referer <> ''` that document 23 measured with.
fn scan(path: &str, column: usize, mut body: impl FnMut(&[u8])) -> Result<u64, String> {
    let mut reader = open(path)?;
    reader.project(&[column]).map_err(|error| error.to_string())?;
    reader.as_string(&[true]);
    let mut rows = 0;
    while let Some(chunk) = reader.next_chunk().map_err(|error| error.to_string())? {
        let vector = chunk.column(0).map_err(|error| error.to_string())?;
        // row at a time: the whole measurement is what one row costs to remember, and a column at a
        // time version of this harness would be measuring a kernel that the operator does not have.
        for row in 0..chunk.len() {
            if let Some(bytes) = vector.bytes_at(row) {
                if !bytes.is_empty() {
                    body(bytes);
                    rows += 1;
                }
            }
        }
    }
    Ok(rows)
}

/// The hash both modes use, so that neither is flattered by the other's.
///
/// A word at a time multiply and rotate, which is what `rudb-exec`'s table does and is not the
/// interesting variable here. What matters is that `exact` and `buckets` pay the same for it.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let mut hash = self.0 ^ (bytes.len() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut rest = bytes;
        while let Some((word, tail)) = rest.split_first_chunk::<8>() {
            hash = (hash.rotate_left(5) ^ u64::from_le_bytes(*word))
                .wrapping_mul(0x517C_C1B7_2722_0A95);
            rest = tail;
        }
        for &byte in rest {
            hash = (hash.rotate_left(5) ^ u64::from(byte)).wrapping_mul(0x517C_C1B7_2722_0A95);
        }
        self.0 = hash;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// One value's hash, standing on its own so both passes agree on it.
fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = Fx::default();
    hasher.write(bytes);
    hasher.finish()
}

/// A table keyed by bytes and hashed the same way both modes are.
type Counts = HashMap<Box<[u8]>, u64, BuildHasherDefault<Fx>>;

/// The answer's rows and a line saying how the mode arrived at them.
type Found = (Vec<(Box<[u8]>, u64)>, String);

/// The top rows, largest count first, out of a table of every key.
fn exact(settings: &Settings, column: usize) -> Result<Found, String> {
    let mut counts: Counts = Counts::default();
    let rows = scan(&settings.path, column, |bytes| {
        if let Some(count) = counts.get_mut(bytes) {
            *count += 1;
        } else {
            counts.insert(bytes.into(), 1);
        }
    })?;
    let groups = counts.len();
    let answer = best(counts.into_iter().collect(), settings.rows);
    Ok((answer, format!("{rows} rows, {groups} groups, one pass")))
}

/// The top rows out of bucket counts, an exact pass over the heaviest, and a certification.
fn buckets(settings: &Settings, column: usize) -> Result<Found, String> {
    let mask = settings.buckets - 1;
    let power = settings.buckets.is_power_of_two();
    let mut tally = vec![0_u32; settings.buckets];
    let rows = scan(&settings.path, column, |bytes| {
        let hash = hash_of(bytes) as usize;
        let at = if power { hash & mask } else { hash % settings.buckets };
        tally[at] = tally[at].saturating_add(1);
    })?;
    let threshold = nth_largest(&tally, settings.budget);
    let mut counts: Counts = Counts::default();
    scan(&settings.path, column, |bytes| {
        let hash = hash_of(bytes) as usize;
        let at = if power { hash & mask } else { hash % settings.buckets };
        if tally[at] >= threshold {
            if let Some(count) = counts.get_mut(bytes) {
                *count += 1;
            } else {
                counts.insert(bytes.into(), 1);
            }
        }
    })?;
    let candidates = counts.len();
    let used = tally.iter().filter(|&&count| count > 0).count();
    let answer = best(counts.into_iter().collect(), settings.rows);
    // The proof in document 26. A key the second pass skipped is in a bucket under the threshold,
    // counts are never negative, so that key's own count is under the threshold too. If the answer's
    // last row is at or above the threshold, nothing skipped can displace it and the answer is
    // exact. If it is not, the operator would raise the budget and go round again, and this harness
    // says so rather than printing a number it cannot stand behind.
    let last = answer.last().map_or(0, |&(_, count)| count);
    let certified = u64::from(threshold) <= last;
    let verdict = if certified { "certified exact" } else { "NOT CERTIFIED, raise the budget" };
    Ok((
        answer,
        format!(
            "{rows} rows, {used} buckets used, threshold {threshold}, \
             {candidates} candidate keys, two passes, {verdict}"
        ),
    ))
}

/// The `budget`th largest counter, or one when there are fewer counters than that.
///
/// A partial sort of the counts above a floor rather than a sort of sixteen million, because the
/// counters are overwhelmingly small and the interesting ones are a rounding error of them.
fn nth_largest(tally: &[u32], budget: usize) -> u32 {
    let mut heavy: Vec<u32> = Vec::new();
    let mut floor = 1_u32;
    for &count in tally {
        if count >= floor {
            heavy.push(count);
            if heavy.len() >= budget.saturating_mul(4) && heavy.len() > 64 {
                heavy.sort_unstable_by(|left, right| right.cmp(left));
                heavy.truncate(budget);
                floor = heavy.last().copied().unwrap_or(1).max(1);
            }
        }
    }
    heavy.sort_unstable_by(|left, right| right.cmp(left));
    heavy.get(budget - 1).copied().unwrap_or(1).max(1)
}

/// The `rows` largest counts, largest first, settling ties by the key so a run is repeatable.
fn best(mut counts: Vec<(Box<[u8]>, u64)>, rows: usize) -> Vec<(Box<[u8]>, u64)> {
    counts.sort_unstable_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    counts.truncate(rows);
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tally where one bucket is heavy and the rest are not.
    fn tally(counts: &[u32]) -> Vec<u32> {
        counts.to_vec()
    }

    #[test]
    fn the_threshold_is_the_budgets_place_in_the_order() {
        let counts = tally(&[5, 1, 9, 3, 7]);
        assert_eq!(nth_largest(&counts, 1), 9);
        assert_eq!(nth_largest(&counts, 2), 7);
        assert_eq!(nth_largest(&counts, 3), 5);
    }

    #[test]
    fn a_budget_past_the_end_still_names_a_floor() {
        let counts = tally(&[4, 0, 0]);
        assert_eq!(nth_largest(&counts, 9), 1);
    }

    #[test]
    fn the_partial_sort_agrees_with_a_whole_one() {
        let counts: Vec<u32> = (0..10_000).map(|at| (at * 7919 % 1237) as u32).collect();
        let mut whole = counts.clone();
        whole.sort_unstable_by(|left, right| right.cmp(left));
        for budget in [1, 2, 17, 64, 500] {
            assert_eq!(nth_largest(&counts, budget), whole[budget - 1].max(1), "budget {budget}");
        }
    }

    #[test]
    fn the_same_bytes_hash_the_same_way_twice() {
        assert_eq!(hash_of(b"the same"), hash_of(b"the same"));
        assert_ne!(hash_of(b"the same"), hash_of(b"not the same"));
    }

    #[test]
    fn the_answer_is_largest_first_and_settles_ties_by_the_key() {
        let counts = vec![
            (Box::from(b"b".as_slice()), 2),
            (Box::from(b"a".as_slice()), 2),
            (Box::from(b"c".as_slice()), 9),
        ];
        let answer = best(counts, 3);
        assert_eq!(answer[0].1, 9);
        assert_eq!(&*answer[1].0, b"a");
        assert_eq!(&*answer[2].0, b"b");
    }

    #[test]
    fn a_mode_that_is_not_one_of_the_two_is_refused() {
        let args = ["file".to_string(), "column".to_string(), "--mode".into(), "guess".into()];
        assert!(Settings::parse(&args).is_err());
    }

    #[test]
    fn the_defaults_are_the_ones_the_document_measured() {
        let args = ["file".to_string(), "column".to_string()];
        let settings = Settings::parse(&args).expect("a path and a column are enough");
        assert_eq!(settings.buckets, 1 << 24);
        assert_eq!(settings.budget, 1_000);
        assert_eq!(settings.rows, 10);
    }
}
