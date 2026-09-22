//! Building a table's statistics sections from the table's own columns.
//!
//! The same meeting place `graph` is, for the other document. `rudb-stats` at rank 5 knows what a
//! column summary says and knows nothing about a file; the rest of this crate knows how to put an
//! opaque payload in a file and nothing about what one means. Building a summary for a real table
//! means reading the column back, so it happens here, in the crate allowed to see both.
//!
//! Everything here obeys `spec/stats/03-the-file-format.md` section 3.1, which is the graph
//! document's section 3.1 applied to a second kind of payload: delete every statistics section and
//! no query changes its answer, only the time. That is why [`summary`] and [`sketches`] answer with
//! an [`Option`] and not a [`Result`]. There is no failure they could report that is not answered
//! by planning the query the way it was planned before the section existed.
//!
//! # The invariant has teeth here that it does not have in the graph layer
//!
//! A key map can only make a join faster. A summary can answer a query: a `COUNT(DISTINCT c)` comes
//! out of one without the column being touched. So the thing that has to survive is not only *is
//! the section there* but *is the number in it exact*, and [`Summary::distinct_class`] is where that
//! lives. This module's job is to never write [`Class::Exact`] onto a number that is not, which in
//! practice means one rule: the sketch says whether it overflowed, and everything else follows from
//! that answer rather than from what the writer hoped.
//!
//! # One pass, and what that costs
//!
//! Section 3.7 gives the statistics build ten percent of the native write time, and the way to stay
//! inside it is not to be clever but to read the column once. [`build_summary`] takes one scan and
//! computes every field of the summary and the sketch from it, so the cost of statistics on a write
//! is the cost of one more read of each column asked for, and no column is read twice.
//!
//! # What is deliberately not here yet
//!
//! Per stripe sketches. Section 3.8's rule is that per stripe structures are written only for the
//! columns that get read, and which columns those are comes out of document 06's observation log at
//! the next checkpoint. So every sketch this builds is a merged one and [`Sketches::stripes`] is
//! empty, which is not a degraded state but the state the rule says most columns are in. The
//! promotion path is the next piece of work and it does not change any byte written here.

use std::cmp::Ordering;
use std::path::Path;
use std::time::{Duration, Instant};

use rudb_common::bounds::{self, Bound};
use rudb_common::stat::Class;
use rudb_common::{LogicalType, Result, Value};
use rudb_encoding::sketch::Sketch;
use rudb_stats::{Order, Sketches, Summary, sketches::HEADER_BYTES as SKETCH_HEADER};
use rudb_storage::count::{Counts, countable};
use rudb_vector::Vector;

use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid};

/// The share of a table's stored column bytes its statistics sections are allowed to cost together.
///
/// Two percent, per section 3.8, and kept apart from the graph layer's ten percent rather than
/// pooled with it. Two budgets that share a pot are two budgets where the one that runs first wins,
/// and a table whose key maps happened to be built before its summaries would then have no
/// summaries for a reason that has nothing to do with summaries. They are counted separately for the
/// same reason they are two documents.
pub const BUDGET_SHARE: u64 = 2;

/// The size below which a table's statistics sections always fit, whatever the share works out to.
///
/// The same floor and the same argument as `graph::BUDGET_FLOOR`. A summary is a few hundred bytes
/// on a table of any size and two percent of a small, well compressed column is less than that, so
/// the pure rule would throw away the cheapest structure in the system for being expensive.
pub const BUDGET_FLOOR: u64 = 64 * 1024;

/// What one column's statistics cost and what they say.
#[derive(Debug, Clone)]
pub struct Built {
    /// Which column was summarized.
    pub column: usize,
    /// Rows in the column, nulls included.
    pub rows: u64,
    /// Distinct non-null values, as the summary reports them.
    pub distinct: u64,
    /// Whether that distinct count is exact rather than a sketch estimate.
    pub exact: bool,
    /// Which way the values run.
    pub order: Order,
    /// What the summary section takes in the file.
    pub summary_bytes: usize,
    /// What the sketches section takes in the file.
    pub sketch_bytes: usize,
    /// What the column takes in the file, which is what the budget is a share of.
    pub column_bytes: u64,
    /// Whether the sections were kept. False means they were built, measured, and found to cost more
    /// than section 3.8 allows, so the file does not have them and every query plans as though
    /// statistics had never been implemented.
    pub built: bool,
    /// How long the build took, the reading of the column included.
    pub build: Duration,
}

impl Built {
    /// Both sections together, which is what the budget spends.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.summary_bytes + self.sketch_bytes
    }
}

/// A column's summary and its sketches, which are built together because they are one pass.
#[derive(Debug, Clone)]
pub struct Stats {
    /// What the column says about itself.
    pub summary: Summary,
    /// The sketch the distinct count came out of.
    pub sketches: Sketches,
}

/// Builds the summary and the sketches for one column of a committed table.
///
/// # Errors
///
/// If the column cannot be read, is past the end of the table, or is of a type with no hash rule.
/// The last one is refused by name rather than approximated: the types without a rule are the
/// interval and the nested ones, a summary of one would carry a distinct count of zero that nothing
/// could tell from a column of nulls, and none of TPC-H or ClickBench has one.
pub fn build_summary(reader: &Reader, column: usize) -> Result<Stats> {
    let fields = reader.table().fields();
    let Some(field) = fields.get(column) else {
        return Err(invalid(&format!(
            "column {column} is past the {} of table {}",
            fields.len(),
            reader.table().name()
        )));
    };
    if !countable(&field.ty) {
        return Err(invalid(&format!(
            "a summary of {} needs a hash rule, and {} has none",
            field.name, field.ty
        )));
    }

    let mut counts = Counts::new(1);
    let mut pass = Pass::new(&field.ty, reader.table().generation());
    for stripe in reader.stripe_parts() {
        pass.open_stripe();
        for part in stripe {
            let chunk = reader.read(part, &[column])?;
            counts.add(&chunk);
            pass.scan(chunk.column(0)?);
        }
        pass.close_stripe();
    }
    let Some(sketch) = counts.sketch(0) else {
        // A blind column: a form `rudb_storage::count` has no arm for turned up, so its sketch is
        // missing rows and says nothing about which. A distinct count that is too low is the one
        // error an estimator has no defence against, so the column gets no summary at all rather
        // than a summary with a number in it nothing can check.
        return Err(invalid(&format!(
            "column {} of {} holds a form with no hash rule, so it has no sketch",
            field.name,
            reader.table().name()
        )));
    };
    Ok(pass.finish(sketch))
}

/// One scan of one column, in `rid` order, for everything the sketch does not answer.
///
/// In `rid` order because the order fields depend on it. A pass that read the parts in any other
/// order would report a column as unordered that is sorted, which costs a plan and not an answer,
/// and would report the run count of a shuffle, which is worse because it is a number rather than a
/// flag and looks like it was measured.
///
/// The distinct count is not here. That is `rudb_storage::count::Counts`, which walks a vector by
/// its form rather than a row at a time and which a column of a million runs costs one hash. Doing
/// it twice would double the expensive half of the build and the budget is ten percent of the write.
struct Pass {
    rows: u64,
    nulls: u64,
    low: Option<Bound>,
    high: Option<Bound>,
    /// False once a non-null value turned up that has no ordered bound, which makes both ends
    /// unusable rather than merely absent.
    bounded: bool,
    ascending: bool,
    descending: bool,
    runs: u64,
    previous: Option<Bound>,
    bytes: u64,
    widest: u64,
    generation: u64,
    stripe: Option<(Bound, Bound)>,
    stripes: Vec<(Bound, Bound)>,
    /// What one value of this column takes, when every value takes the same.
    ///
    /// Read off the type once rather than off each value, because for every fixed width column it is
    /// a constant and asking a value for it is a branch a hundred million times to hear the same
    /// number. `None` is a variable width type and those are measured per value.
    fixed: Option<u64>,
    /// The scale of a decimal column, so that an integer read out of a vector becomes the bound the
    /// column's other writers would have written for the same value.
    scale: Option<u8>,
}

impl Pass {
    fn new(ty: &LogicalType, generation: u64) -> Self {
        Self {
            rows: 0,
            nulls: 0,
            low: None,
            high: None,
            bounded: true,
            ascending: true,
            descending: true,
            runs: 0,
            previous: None,
            bytes: 0,
            widest: 0,
            generation,
            stripe: None,
            stripes: Vec::new(),
            fixed: fixed_width(ty),
            scale: bounds::scale_of(ty),
        }
    }

    /// One vector of the column, typed rather than a value at a time where the type allows it.
    ///
    /// `signed_at` and `bytes_at` between them cover every integer, date, timestamp, decimal, string
    /// and blob column, which is all sixteen of TPC-H `lineitem` and all but a handful of
    /// ClickBench. Both are a load against a slice. The fall back below builds a `Value`, and it is
    /// there for the float columns and for the forms the two fast paths cannot read, not as the
    /// ordinary path.
    fn scan(&mut self, vector: &Vector) {
        // row at a time: the run count and the order flags are a sequential dependency. Whether this
        // value is below the one before it is a question about a pair of adjacent rows, so there is
        // no shape of this loop that answers it a vector at a time, and the two typed accessors
        // below are loads against a slice rather than value construction. What the checker is
        // looking for is the third arm, which does build a `Value`, and that one runs for a float
        // column and for a form the first two cannot read and for nothing else.
        for row in 0..vector.len() {
            self.rows += 1;
            if vector.is_null_at(row) {
                self.nulls += 1;
                continue;
            }
            if let Some(signed) = vector.signed_at(row) {
                let bound = match self.scale {
                    Some(scale) => Bound::Scaled { unscaled: signed, scale },
                    None => Bound::Int(signed),
                };
                self.value(bound, self.fixed.unwrap_or(8));
                continue;
            }
            if let Some(bytes) = vector.bytes_at(row) {
                let width = bytes.len() as u64;
                self.value(Bound::Bytes(bytes.to_vec()), width);
                continue;
            }
            // row at a time: a float and a form neither typed accessor above can read have no slice
            // to walk, so the value is built for this row and for no other.
            let value = vector.value_at(row);
            let width = self.fixed.unwrap_or_else(|| width(&value));
            match Bound::of_value(&value) {
                Some(bound) => self.value(bound, width),
                None => {
                    // A non-null value with no ordered bound. Both ends go rather than the value
                    // being skipped, because an end computed from only the values that had bounds is
                    // an end that answers a MIN with a value the column does not hold.
                    self.bytes = self.bytes.saturating_add(width);
                    self.widest = self.widest.max(width);
                    self.bounded = false;
                    self.ascending = false;
                    self.descending = false;
                }
            }
        }
    }

    /// One non-null value, as its bound and its width.
    fn value(&mut self, bound: Bound, width: u64) {
        self.bytes = self.bytes.saturating_add(width);
        self.widest = self.widest.max(width);
        match &self.previous {
            None => self.runs = 1,
            Some(previous) => match previous.order(&bound) {
                Some(Ordering::Less) => self.descending = false,
                Some(Ordering::Greater) => {
                    self.ascending = false;
                    self.runs += 1;
                }
                Some(Ordering::Equal) => {}
                // Two bounds of different domains in one column. It should be unreachable, since a
                // column has one type, and it costs an order claim rather than being assumed away.
                None => {
                    self.ascending = false;
                    self.descending = false;
                }
            },
        }
        self.low = Some(match self.low.take() {
            Some(held) => held.smaller(bound.clone()),
            None => bound.clone(),
        });
        self.high = Some(match self.high.take() {
            Some(held) => held.larger(bound.clone()),
            None => bound.clone(),
        });
        self.stripe = Some(match self.stripe.take() {
            Some((low, high)) => (low.smaller(bound.clone()), high.larger(bound.clone())),
            None => (bound.clone(), bound.clone()),
        });
        self.previous = Some(bound);
    }

    fn open_stripe(&mut self) {
        self.stripe = None;
    }

    fn close_stripe(&mut self) {
        if let Some(range) = self.stripe.take() {
            self.stripes.push(range);
        }
    }

    fn finish(self, sketch: Sketch) -> Stats {
        let present = self.rows - self.nulls;
        // The one rule the module doc names. An exact distinct count is one the sketch never had to
        // throw a value away to keep, and everything downstream of the count follows from this
        // answer rather than from what the writer hoped.
        let exact = sketch.is_exact();
        let distinct = if exact {
            sketch.len() as u64
        } else {
            // Rounded rather than truncated, and clamped under the rows it cannot exceed. An
            // estimate above the row count is arithmetically possible and is always wrong, and a
            // planner that sees one concludes a column has more distinct values than rows.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let estimate = sketch.distinct().round().max(0.0) as u64;
            estimate.min(present)
        };
        let summary = Summary {
            rows: self.rows,
            nulls: self.nulls,
            low: if self.bounded { self.low } else { None },
            high: if self.bounded { self.high } else { None },
            // Every end here came from a value the column holds, because this pass read them all.
            // That is the whole difference between a summary and a zone map, which is allowed to be
            // wider than its column and so can only skip and never answer.
            ends_exact: self.bounded,
            distinct,
            // Exact or estimated, and never certified. A KMV sketch's relative error is about one
            // over the square root of k, which is a standard error and not a bound, and Certified
            // in this codebase means a bound that holds. Calling a one and a half percent standard
            // error a guarantee is how an estimate gets treated as an answer.
            distinct_class: if exact { Class::Exact } else { Class::Estimated },
            // Only from an exact count. A sketch that overflowed cannot tell a column of a million
            // unique values from one where two of them repeat, and uniqueness is the claim a key
            // map is built on.
            unique: exact && distinct == present,
            order: if present == 0 {
                Order::Neither
            } else if self.ascending {
                Order::Ascending
            } else if self.descending {
                Order::Descending
            } else {
                Order::Neither
            },
            runs: self.runs,
            overlapping: overlapping(&self.stripes),
            bytes: self.bytes,
            widest: self.widest,
            newest: self.generation,
        };
        Stats { summary, sketches: Sketches::merged(sketch) }
    }
}

/// Whether any two of these stripe ranges overlap.
///
/// Sorted by low end and then walked, so this is one sort rather than the square. A pair this cannot
/// order counts as overlapping, which is the answer that costs a skipped stripe rather than a wrong
/// one.
fn overlapping(stripes: &[(Bound, Bound)]) -> bool {
    let mut order = (0..stripes.len()).collect::<Vec<_>>();
    order
        .sort_by(|&one, &other| stripes[one].0.order(&stripes[other].0).unwrap_or(Ordering::Equal));
    order.windows(2).any(|pair| {
        let before = &stripes[pair[0]].1;
        let after = &stripes[pair[1]].0;
        before.order(after) != Some(Ordering::Less)
    })
}

/// What every value of this type takes, when they all take the same.
///
/// `None` for the variable width types, which is the two string ones and nothing else. Read off the
/// type once by `Pass::new` rather than off each value.
fn fixed_width(ty: &LogicalType) -> Option<u64> {
    Some(match ty {
        LogicalType::Boolean | LogicalType::TinyInt | LogicalType::UTinyInt => 1,
        LogicalType::SmallInt | LogicalType::USmallInt => 2,
        LogicalType::Integer | LogicalType::UInteger | LogicalType::Float | LogicalType::Date => 4,
        LogicalType::HugeInt | LogicalType::UHugeInt | LogicalType::Decimal { .. } => 16,
        LogicalType::Varchar | LogicalType::Blob => return None,
        // The eight byte types: the two big integers, the double, and the four time ones. Anything
        // else that reaches here is refused a summary by `countable` long before this.
        _ => 8,
    })
}

/// What one value takes, for the byte total and the widest value.
///
/// The logical width and not the stored one. The stored width is what the column's encoding chose
/// and is already in the layout; this is what the value costs a plan that has to materialize it,
/// which is the number a hash table sizing decision wants.
fn width(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Boolean(_) | Value::TinyInt(_) | Value::UTinyInt(_) => 1,
        Value::SmallInt(_) | Value::USmallInt(_) => 2,
        Value::Integer(_) | Value::UInteger(_) | Value::Float(_) | Value::Date(_) => 4,
        Value::HugeInt(_) | Value::UHugeInt(_) | Value::Decimal { .. } => 16,
        Value::Varchar(text) => text.len() as u64,
        Value::Blob(bytes) => bytes.len() as u64,
        // The eight byte types and anything else, which is every remaining scalar. A nested value
        // reaching here would be counted at eight and is refused a summary long before this by
        // `countable`.
        _ => 8,
    }
}

/// Builds the statistics for each of these columns and attaches them all in one commit.
///
/// One commit and not one each, for the reason `graph::build_key_maps` gives: a checkpoint that
/// published one generation per column would be one chance per column of being interrupted halfway.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be summarized, or the attach fails.
pub fn build_stats(path: &Path, table: &str, columns: &[usize]) -> Result<Vec<Built>> {
    build_stats_within(path, table, columns, BUDGET_SHARE)
}

/// The same, against a budget of `share` percent of the table's stored column bytes.
///
/// The budget is over the table rather than over a column, and when it binds the cheapest columns
/// are admitted first. That is the same degenerate case section 3.7's expected value ordering has
/// for a key map with no relationship over it: nothing has said which column a plan will ask about,
/// so no summary is worth more than another and the ordering falls back to the denominator. Cheapest
/// first is also the order that fits the most summaries in the room there is.
///
/// A column is all or nothing. Its summary and its sketches are admitted together or neither is,
/// because a summary whose distinct count came from a sketch that was then dropped is a number with
/// nothing behind it to check it against.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be summarized, or the attach fails.
pub fn build_stats_within(
    path: &Path,
    table: &str,
    columns: &[usize],
    share: u64,
) -> Result<Vec<Built>> {
    let reader = Catalog::open(path)?.table(table)?;
    let column_bytes = reader.layout().columns_total();
    let allowance = (column_bytes.saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let mut spent = held_bytes(&reader, columns)?;
    let mut report = Vec::with_capacity(columns.len());
    let mut payloads = Vec::with_capacity(columns.len());
    for &column in columns {
        let start = Instant::now();
        let stats = build_summary(&reader, column)?;
        let mut summary = Vec::new();
        stats.summary.encode(&mut summary)?;
        let mut sketches = Vec::new();
        stats.sketches.encode(&mut sketches)?;
        report.push(Built {
            column,
            rows: stats.summary.rows,
            distinct: stats.summary.distinct,
            exact: stats.summary.distinct_class == Class::Exact,
            order: stats.summary.order,
            summary_bytes: summary.len(),
            sketch_bytes: sketches.len(),
            column_bytes,
            built: false,
            build: start.elapsed(),
        });
        payloads.push((column, summary, sketches));
    }
    let mut order = (0..payloads.len()).collect::<Vec<_>>();
    order.sort_by_key(|&at| report[at].bytes());
    let mut keep = vec![false; payloads.len()];
    for at in order {
        let cost = report[at].bytes() as u64;
        if spent.saturating_add(cost) <= allowance {
            spent += cost;
            keep[at] = true;
            report[at].built = true;
        }
    }
    // The reader holds the file open and the attach opens it again to write, so it is dropped first
    // for the reason `graph` drops it: the moment the file is written is a moment nothing else in
    // this function is reading it.
    drop(reader);
    let mut attachments = Vec::with_capacity(payloads.len() * 2);
    for ((column, summary, sketches), _) in payloads.iter().zip(&keep).filter(|&(_, &keep)| keep) {
        let id = u64::try_from(*column).map_err(|_| invalid("column index overflow"))?;
        attachments.push(Attachment {
            kind: *section::SUMMARY,
            id,
            flags: 0,
            // A summary is a header the whole way down. There is nothing behind it that a reader
            // could decide not to read, which is the shape section 3.2's field is for and not a
            // misuse of it: the answer to "how much do I read to know what this says" is all of it.
            header_bytes: u32::try_from(summary.len())
                .map_err(|_| invalid("a summary longer than a u32 can count"))?,
            bytes: summary,
        });
        attachments.push(Attachment {
            kind: *section::SKETCHES,
            id,
            flags: 0,
            header_bytes: SKETCH_HEADER,
            bytes: sketches,
        });
    }
    crate::attach(path, table, &attachments)?;
    Ok(report)
}

/// What the table's existing sections cost, leaving out the statistics this build is replacing.
///
/// Every section counts, the graph ones included, because the file is one file. The two budgets are
/// separate shares of the same column bytes and each is checked against what is already spent, which
/// is how one layer overrunning is visible to the other rather than silently doubling the total.
fn held_bytes(reader: &Reader, replacing: &[usize]) -> Result<u64> {
    let mut total = 0;
    for held in reader.table().sections() {
        let mine = held.kind == *section::SUMMARY || held.kind == *section::SKETCHES;
        let replaced = mine && replacing.iter().any(|&column| u64::try_from(column) == Ok(held.id));
        if replaced || !held.usable(reader.table().generation()) {
            continue;
        }
        let Ok(extents) = reader.extents(held) else { continue };
        total += extents.iter().map(|extent| u64::from(extent.length)).sum::<u64>();
    }
    Ok(total)
}

/// The summary this table carries for a column, when it carries one this build can use.
///
/// `None` covers every reason there is not one and covering them all is the point. Section 3.1 says
/// deleting every statistics section changes no answer, so there is no reason to distinguish *no
/// summary was built* from *the summary is stale*, *the payload does not checksum*, or *the layout
/// is one a later build invented*. The answer to all four is to plan the query the way it was
/// planned before summaries existed.
#[must_use]
pub fn summary(reader: &Reader, column: usize) -> Option<Summary> {
    let bytes = payload(reader, column, section::SUMMARY)?;
    Summary::decode(&bytes).ok()
}

/// The sketches this table carries for a column, same.
///
/// One more reason for `None` here than above: a sketch built by a hash this build does not use is
/// declined by [`Sketches::decode`] rather than merged into anything, which costs a rebuild where
/// merging would cost an answer.
#[must_use]
pub fn sketches(reader: &Reader, column: usize) -> Option<Sketches> {
    let bytes = payload(reader, column, section::SKETCHES)?;
    Sketches::decode(&bytes).ok()
}

fn payload(reader: &Reader, column: usize, kind: &[u8; 8]) -> Option<Vec<u8>> {
    let table = reader.table();
    let id = u64::try_from(column).ok()?;
    let held = table.sections().iter().find(|section| section.kind == *kind && section.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    reader.payload(held).ok()
}

/// Whether a type can be summarized at all, which is whether it has a hash rule.
#[must_use]
pub fn summarizable(ty: &LogicalType) -> bool {
    countable(ty)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::Field;
    use rudb_encoding::sketch::hash64;
    use rudb_storage::count::hash_value;
    use rudb_vector::{Chunk, Vector};

    use super::*;
    use crate::Writer;

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-stats-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    /// A one column table of these values, written a thousand rows to a part.
    fn table_of(label: &str, values: &[Option<i64>]) -> PathBuf {
        let path = path(label);
        let mut writer =
            Writer::create(&path, "t", vec![Field::new("v", LogicalType::BigInt)]).expect("new");
        for part in values.chunks(1000) {
            let held =
                part.iter().map(|v| v.map_or(Value::Null, Value::BigInt)).collect::<Vec<_>>();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &held).expect("values")])
                    .expect("one column");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        path
    }

    fn reopen(path: &PathBuf) -> Reader {
        Catalog::open(path).expect("reopen").table("t").expect("the table")
    }

    #[test]
    fn a_summary_built_over_a_file_says_what_the_column_holds() {
        // End to end: the column goes to disk, comes back through the reader, and every field of
        // the summary is the truth about it. Three thousand rows so the scan crosses parts, because
        // a pass that read them in the wrong order would be right about one part and wrong about
        // the order fields for the rest.
        let values = (1..=3000_i64).map(Some).collect::<Vec<_>>();
        let path = table_of("sorted", &values);
        let built = build_stats(&path, "t", &[0]).expect("build");
        assert_eq!(built.len(), 1);
        assert!(built[0].built, "a one column table is nowhere near the budget");
        assert_eq!(built[0].rows, 3000);
        assert_eq!(built[0].distinct, 3000);
        assert!(built[0].exact, "three thousand values is under the default k");
        assert_eq!(built[0].order, Order::Ascending);

        let reader = reopen(&path);
        let summary = summary(&reader, 0).expect("the summary is in the file");
        assert_eq!(summary.rows, 3000);
        assert_eq!(summary.nulls, 0);
        assert_eq!(summary.low, Some(Bound::Int(1)));
        assert_eq!(summary.high, Some(Bound::Int(3000)));
        assert!(summary.ends_exact);
        assert!(summary.unique, "a sorted run of distinct values is a key candidate");
        assert_eq!(summary.runs, 1, "one ascending run");
        assert_eq!(summary.distinct_class, Class::Exact);
        assert_eq!(summary.newest, reader.table().generation());

        let sketches = sketches(&reader, 0).expect("the sketches are in the file");
        assert!(sketches.merged.is_exact());
        assert!(sketches.stripes.is_empty(), "the per stripe rule gives this column none");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn nulls_are_counted_and_do_not_reach_the_ends_or_the_sketch() {
        // The distinction that costs an answer if it is got wrong. A null is a row and is not a
        // value, so it moves `rows` and `nulls` and moves nothing else.
        let values: Vec<Option<i64>> =
            (0..2000).map(|at| if at % 3 == 0 { None } else { Some(at) }).collect();
        let path = table_of("nulls", &values);
        build_stats(&path, "t", &[0]).expect("build");

        let reader = reopen(&path);
        let summary = summary(&reader, 0).expect("the summary");
        let nulls = values.iter().filter(|v| v.is_none()).count() as u64;
        assert_eq!(summary.rows, 2000);
        assert_eq!(summary.nulls, nulls);
        assert_eq!(summary.present(), 2000 - nulls);
        assert_eq!(summary.distinct, 2000 - nulls, "a null is not a distinct value");
        assert_eq!(summary.low, Some(Bound::Int(1)), "zero is null here");
        assert!(summary.unique);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_that_repeats_is_not_reported_unique_and_a_descending_one_is_seen() {
        let values = (0..2000_i64).map(|at| Some(-(at / 2))).collect::<Vec<_>>();
        let path = table_of("repeats", &values);
        build_stats(&path, "t", &[0]).expect("build");

        let reader = reopen(&path);
        let summary = summary(&reader, 0).expect("the summary");
        assert_eq!(summary.distinct, 1000);
        assert!(!summary.unique, "every value appears twice");
        assert_eq!(summary.order, Order::Descending);
        assert_eq!(summary.runs, 1000, "a descending column is a run per distinct value");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_past_the_default_k_is_estimated_and_says_so() {
        // The rule the module doc names, at the point where it bites. Past k the sketch threw values
        // away, so the count is an estimate, and the class has to say so or a COUNT(DISTINCT) is
        // answered out of metadata with a number that is close and wrong.
        let values = (0..20_000_i64).map(Some).collect::<Vec<_>>();
        let path = table_of("estimated", &values);
        let built = build_stats(&path, "t", &[0]).expect("build");
        assert!(!built[0].exact, "twenty thousand values is past the default k");

        let reader = reopen(&path);
        let summary = summary(&reader, 0).expect("the summary");
        assert_eq!(summary.distinct_class, Class::Estimated);
        assert!(!summary.unique, "uniqueness is never claimed off an estimate");
        assert!(summary.distinct > 17_000 && summary.distinct <= 20_000, "{}", summary.distinct);
        assert!(summary.distinct <= summary.present(), "more distinct values than rows");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_shuffled_column_is_neither_ordered_nor_one_run() {
        let values = (0..2000_i64).map(|at| Some((at * 7919) % 2000)).collect::<Vec<_>>();
        let path = table_of("shuffled", &values);
        build_stats(&path, "t", &[0]).expect("build");

        let reader = reopen(&path);
        let summary = summary(&reader, 0).expect("the summary");
        assert_eq!(summary.order, Order::Neither);
        assert!(summary.runs > 100, "a shuffle is many runs, not one: {}", summary.runs);
        assert_eq!(summary.low, Some(Bound::Int(0)));
        assert_eq!(summary.high, Some(Bound::Int(1999)));

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn deleting_the_sections_changes_nothing_but_whether_they_are_there() {
        // Section 3.1, as close to directly as a test can put it. The same file, read once with the
        // sections and once with the generation moved past them, and the reader opens and scans the
        // same either way.
        let values = (1..=1500_i64).map(Some).collect::<Vec<_>>();
        let path = table_of("invariant", &values);
        build_stats(&path, "t", &[0]).expect("build");

        let reader = reopen(&path);
        assert!(summary(&reader, 0).is_some());
        let generation = reader.table().generation();
        let held: Vec<_> = reader
            .table()
            .sections()
            .iter()
            .filter(|s| s.kind == *section::SUMMARY || s.kind == *section::SKETCHES)
            .copied()
            .collect();
        assert_eq!(held.len(), 2, "a summary and a sketch section");
        for section in &held {
            assert!(section.usable(generation));
            assert!(!section.usable(generation + 1), "a rewrite invalidates rather than corrupts");
        }
        let rows: usize =
            (0..reader.parts()).map(|part| reader.read(part, &[0]).expect("a part").len()).sum();
        assert_eq!(rows, 1500, "the scan is the scan whether the sections are read or not");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_string_column_is_read_through_the_typed_path_and_measured_by_its_bytes() {
        // The other fast path. A varchar has no fixed width, so the byte total and the widest value
        // are measured per value, and the ends are the string ends rather than the hash ends.
        let path = path("strings");
        let mut writer =
            Writer::create(&path, "t", vec![Field::new("v", LogicalType::Varchar)]).expect("new");
        let words = ["alpha", "bravo", "charlie", "delta", "alpha"];
        let held = words.iter().map(|w| Value::Varchar((*w).into())).collect::<Vec<_>>();
        let chunk =
            Chunk::new(vec![Vector::from_values(LogicalType::Varchar, &held).expect("words")])
                .expect("one column");
        writer.append(&chunk).expect("a part");
        writer.finish().expect("commit");
        build_stats(&path, "t", &[0]).expect("build");

        let reader = reopen(&path);
        let summary = summary(&reader, 0).expect("the summary");
        assert_eq!(summary.rows, 5);
        assert_eq!(summary.distinct, 4, "alpha twice");
        assert!(!summary.unique);
        assert_eq!(summary.bytes, words.iter().map(|w| w.len() as u64).sum::<u64>());
        assert_eq!(summary.widest, 7, "charlie");
        assert_eq!(summary.low, Some(Bound::Bytes(b"alpha".to_vec())));
        assert_eq!(summary.high, Some(Bound::Bytes(b"delta".to_vec())));

        drop(reader);
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_type_with_no_hash_rule_is_refused_by_name_rather_than_summarized_as_empty() {
        let path = table_of("refused", &[Some(1)]);
        let reader = reopen(&path);
        assert!(summarizable(&LogicalType::BigInt));
        assert!(!summarizable(&LogicalType::Interval));
        assert!(build_summary(&reader, 1).is_err(), "a column past the end");
        drop(reader);
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_stored_sketch_depends_on_the_value_rule_and_not_only_on_the_hash() {
        // HASH_IDENTITY pins `hash64`, which is half of what a stored sketch depends on. The other
        // half is the rule that turns a value into the bytes `hash64` sees, and that rule lives in
        // `rudb_storage::count`. Changing it without bumping HASH_IDENTITY would leave every stored
        // sketch readable, accepted, and built over a different universe than the one a new sketch
        // is built over, which is exactly the merge the identity exists to prevent.
        //
        // So the rule is pinned here. If this fails because `hash_value` changed on purpose, the fix
        // is to bump HASH_IDENTITY and then update these numbers, in that order.
        assert_eq!(hash_value(&Value::BigInt(1)), Some(hash64(&1_u128.to_le_bytes())));
        assert_eq!(hash_value(&Value::Integer(1)), hash_value(&Value::BigInt(1)));
        assert_eq!(hash_value(&Value::Varchar("a".into())), Some(hash64(b"a")));
        assert_eq!(hash_value(&Value::Null), None);
    }
}
