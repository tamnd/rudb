//! Showing the planner what a native table already wrote down about itself.
//!
//! Every stripe carries the two ends and the null count of every column, in the directory, in
//! memory from the moment the file is opened. The scan has been reading them since the format
//! existed and the planner has never seen them, so a query over a native table was ordered from the
//! same constants a query over a table nobody had measured would get.
//!
//! Two things come out of that directory here. [`Stripes`] answers how many rows a set of tests
//! keeps, which is what a filter's estimate rests on. [`distincts`] answers how many values a column
//! holds, which is what a join's estimate rests on. They are in one module because they are one
//! idea, and because TPC-H q05 needs both of them and is the reason either exists.
//!
//! [`Common`] came later and is not out of the directory. It answers how many rows hold one
//! particular value, off the frequency synopsis the writer takes per column, and it belongs here
//! because it is the same idea pointed at the same reader: a number the file already holds that the
//! planner was assuming its way past.
//!
//! # What q05 actually needed
//!
//! The filter is the easy half. The `o_orderdate` range over SF1 keeps 227,597 rows of 1,500,000.
//! Through Parquet the footer gives 227,556 and through the native file the estimate was 60,000,
//! which is the constant for a range nobody could read. [`Stripes`] closes that: the same query now
//! estimates 227,556 from the stripe bounds, off the true answer by forty one rows in two hundred
//! thousand.
//!
//! Closing it changed nothing. q05 measured 5,217 ms with the filter estimate fixed against 4,491
//! before, which is the same plan and a loaded laptop. The join order was never reading the filter.
//! It was reading the distinct counts, and the containment assumption in `estimate::matched` only
//! gives way to `left * right / keys` where both key columns have one. DuckDB writes distinct counts
//! into a Parquet footer, so the Parquet plan divides the customer against supplier join by the 25
//! nations and scores it at sixty million, which is enough for the search to put customer against
//! orders first instead. The native file stated no count for an integer column, the divisor fell
//! back to the table's own row count, and the same join scored 150,000. So the search took it first
//! and built the twelve million row intermediate that is the whole of q05's time.
//!
//! # Why the stripe and not the part
//!
//! A stripe's bounds are in the directory and a part's are a page in the file. The planner is
//! deciding what to read and reading a page per column per stripe to decide it would be the scan
//! run twice, so this answers from the stripe alone and never touches the file. The loss is smaller
//! than it sounds: the interpolation below is what the estimate mostly rests on, and interpolating
//! inside sixteen stripes and inside nine hundred parts of the same column give nearly the same
//! fraction when the rows are in no particular order, which is the case this exists for. A part
//! bound is worth reading when the question is which parts to skip, and that question is the scan's
//! and is already answered by [`Reader::skips`].

use std::cmp::Ordering;

use rudb_common::Result;
use rudb_common::Stat;
use rudb_common::bounds::{Bound, End, Frequencies, Spread, Test, Zones, kept};
use rudb_common::stat::{Direction, Provenance};
use rudb_storage::Probe;

use crate::Reader;

/// The bounds of a committed native table, as the planner asks for them.
///
/// Holds the reader rather than a copy of the bounds. A reader is a handful of reference counts and
/// cloning one shares the caches it has already filled, where copying the bounds out would be every
/// stripe of every column of the table per statement bound.
#[derive(Debug, Clone)]
pub struct Stripes {
    reader: Reader,
}

impl Stripes {
    /// The bounds of a table somebody has open.
    #[must_use]
    pub fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl Zones for Stripes {
    fn column(&self, name: &str) -> Option<usize> {
        self.reader.table().fields().iter().position(|field| field.name == name)
    }

    fn surviving(&self, tests: &[Test]) -> Option<u64> {
        let probes = probes(tests);
        let mut total: u64 = 0;
        for (at, stripe) in self.reader.table().stripes().iter().enumerate() {
            if self.reader.stripe_skips(at, &probes) {
                continue;
            }
            total = total.checked_add(u64::try_from(stripe.rows()).ok()?)?;
        }
        Some(total)
    }

    fn spread(&self, tests: &[Test]) -> Option<Spread> {
        let mut passing = 0.0_f64;
        let mut whole = 0.0_f64;
        let mut read = 0;
        for stripe in self.reader.table().stripes() {
            let rows = rows(stripe.rows());
            let spread = fraction(tests, stripe.zone());
            whole += rows;
            passing += rows * spread.fraction;
            // The most any one stripe could read rather than a total of them, the same as the
            // Parquet footer does it and for the same reason: the caller is charging its constant
            // for the tests nobody answered, so the question is whether anybody answered this one.
            read = read.max(spread.read);
        }
        (read > 0 && whole > 0.0)
            .then(|| Spread { fraction: (passing / whole).clamp(0.0, 1.0), read })
    }

    fn nulls(&self, column: usize) -> Stat<u64> {
        // Every stripe of a native file states its null count, so this is exact or the column is
        // not there. A reader that cannot answer its own directory fails the scan a moment later
        // with the same error, and the planner is not the place to raise it.
        self.reader
            .null_count(column)
            .map_or(Stat::Unknown, |nulls| Stat::exact(nulls, Provenance::NullCount))
    }

    fn extreme(&self, column: usize, end: End) -> Stat<Bound> {
        // The reader folds the stripes itself and answers only where every one of them wrote a
        // bound its writer called exact, which is the same promise this has to make. A column whose
        // ends were widened, or whose stripes do not compare against each other, comes back `None`
        // there and unknown here.
        match self.reader.exact_extremes(column) {
            Ok(Some((low, high))) => {
                Stat::exact(if end == End::Low { low } else { high }, Provenance::ZoneMap)
            }
            _ => Stat::Unknown,
        }
    }
}

/// How many distinct values each column of a native table holds, for the columns it can say.
///
/// Two sources, and a column with neither is left out rather than guessed at. An absent column
/// reads back as unknown and the estimator falls back to the table's row count, which is what every
/// column did before this existed.
///
/// A string column has a global dictionary and the directory records how many codes any row of it
/// actually holds, so that count is exact and comes back as such. The dictionary page is not opened
/// to answer, which matters: this runs once per table per statement bound.
///
/// Every other column is answered from its two ends, where they are integers. A column of integers
/// between `low` and `high` cannot hold more than `high - low + 1` distinct values, so the span is a
/// ceiling, and on the columns that decide a join order it is a tight one. TPC-H nationkey runs 0 to
/// 24 and holds 25 values, regionkey 0 to 4 and holds 5. On `l_orderkey` the span is six million
/// against a true one and a half, which is loose and still safe, for the reason below.
///
/// # Why a ceiling is the safe end here
///
/// The two readers of a distinct count both divide by it. A divisor that is too large makes the
/// join look smaller, and `estimate::matched` takes the larger of that and the containment
/// assumption, so too large a span can only fail to raise an estimate and can never lower one below
/// what shape alone already said. Too small a divisor is the dangerous direction and a span cannot
/// be too small: a widened bound is wider than the truth, never narrower, so the span it implies is
/// a ceiling however the bound was written.
///
/// A span at or above the table's row count is dropped rather than recorded. The row count is what
/// the estimator already falls back to for a column nobody counted, so recording it would be an
/// entry that says what its own absence says.
///
/// # Errors
///
/// Never, today. The two reads it makes are indexed by a column this loop produced, so neither can
/// be out of range, and the signature carries the `Result` because both of them do.
pub fn distincts(reader: &Reader) -> Result<Vec<(String, Stat<u64>)>> {
    let table = reader.table();
    let rows = u64::try_from(table.rows()).unwrap_or(u64::MAX);
    let mut counted = Vec::new();
    for (at, field) in table.fields().iter().enumerate() {
        if let Some(exact) = reader.distinct_values(at)? {
            counted.push((field.name.clone(), Stat::exact(exact, Provenance::Dictionary)));
            continue;
        }
        let Some((Bound::Int(low), Bound::Int(high))) = reader.exact_extremes(at)? else {
            continue;
        };
        let Some(span) = high.checked_sub(low).and_then(|span| u64::try_from(span).ok()) else {
            continue;
        };
        let Some(span) = span.checked_add(1).filter(|&span| span < rows) else {
            continue;
        };
        // The weakest certificate there is: certain from above with the relative error unbounded,
        // which is the class a ceiling with nothing under it takes everywhere else in the tree.
        counted.push((
            field.name.clone(),
            Stat::certified(span, 1.0, Direction::AtMost, Provenance::ZoneMap),
        ));
    }
    Ok(counted)
}

/// What a native table's frequency synopsis says about one value, as the planner asks for it.
///
/// Holds the reader for the reason [`Stripes`] does. The synopsis is small where it exists at all,
/// but it exists per column and copying every column's into every plan would be paying for the
/// columns nothing filters on, which is most of them.
#[derive(Debug, Clone)]
pub struct Common {
    reader: Reader,
}

impl Common {
    /// The frequencies of a table somebody has open.
    #[must_use]
    pub fn new(reader: Reader) -> Self {
        Self { reader }
    }
}

impl Frequencies for Common {
    fn column(&self, name: &str) -> Option<usize> {
        self.reader.table().fields().iter().position(|field| field.name == name)
    }

    fn rows(&self) -> u64 {
        u64::try_from(self.reader.table().rows()).unwrap_or(u64::MAX)
    }

    fn rows_with(&self, column: usize, value: &Bound) -> Stat<u64> {
        // The prefix and not only the complete list, because the counts in it are exact either way.
        // The writer recounts the candidates that survive its pass, so what an incomplete synopsis
        // lost is values rather than counts, and a value it kept is one of the leading values of the
        // column, which is the one an equality would otherwise guess worst about.
        let Ok(Some(prefix)) = self.reader.frequency_prefix(column) else {
            return Stat::Unknown;
        };
        let mut comparable = false;
        for (held, count) in prefix.entries {
            // A null entry is the column's nulls, and no equality matches a null. Skipping it is
            // both the right answer and the only one available, since a null has no bound.
            let Some(bound) = Bound::of_value(&held) else {
                continue;
            };
            match bound.order(value) {
                Some(Ordering::Equal) => return Stat::exact(count, Provenance::FrequencySynopsis),
                Some(_) => comparable = true,
                None => {}
            }
        }
        // Nothing in the list was the value. That is a count of zero when the list left nothing out
        // and the constant was in the same domain, because a complete synopsis accounts for every
        // row. Where the list left something out, the value is somewhere between no rows and the
        // bound the writer recorded, and a prefix has nothing to say about which. Where not one
        // entry would even compare, the constant is of another type and the zero would be an
        // artefact of that rather than a fact about the rows.
        if prefix.omitted_max == 0 && comparable {
            Stat::exact(0, Provenance::FrequencySynopsis)
        } else {
            Stat::Unknown
        }
    }
}

/// The tests as the storage layer spells them, which is the same three fields under another name.
fn probes(tests: &[Test]) -> Vec<Probe> {
    tests
        .iter()
        .map(|test| Probe { column: test.column, op: test.op, value: test.value.clone() })
        .collect()
}

/// A stripe's row count as a weight, and zero for one that does not read as a count.
#[expect(clippy::cast_precision_loss, reason = "a row count is a weight here and not an identity")]
fn rows(count: usize) -> f64 {
    count as f64
}

/// The fraction of one stripe these tests are expected to keep, and how many of them said so.
///
/// Tests on one column are intersected by [`kept`] and tests on different columns are multiplied
/// here, which assumes the columns are independent of each other. That is the assumption the
/// estimator makes everywhere else and the one that fails first, and a pair of bounds cannot do
/// anything about it either way.
///
/// A stripe this cannot read keeps a fraction of one rather than dropping out of the total. Leaving
/// it out would report the fraction of the stripes that were read as the fraction of the table.
fn fraction(tests: &[Test], zone: &rudb_storage::Zone) -> Spread {
    let mut spread = Spread { fraction: 1.0, read: 0 };
    for (position, test) in tests.iter().enumerate() {
        // Once per column rather than once per test, because `kept` is handed every test on the
        // column and answers for all of them at once. The first mention of a column is the one that
        // asks and the rest are already in that answer.
        if tests[..position].iter().any(|earlier| earlier.column == test.column) {
            continue;
        }
        let Some(range) = zone.column(test.column) else { continue };
        let (Some(low), Some(high)) = (range.low.as_ref(), range.high.as_ref()) else { continue };
        let Some(kept) = kept(tests, test.column, low, high) else { continue };
        spread.fraction *= kept.fraction;
        spread.read += kept.read;
    }
    spread
}
