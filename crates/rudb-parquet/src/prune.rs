//! Deciding that a row group holds no row a filter wants, without reading it.
//!
//! A Parquet writer records the smallest and the largest value of every column of every row group in
//! the footer. A filter that is a comparison against a constant can be answered against those two
//! numbers instead of against the column: if every value in the group is below the constant and the
//! filter wants values above it, nothing in the group passes and the group does not have to be
//! opened at all. That skips the read, the decompression and the decode together, which is why this
//! is worth more than any of the three individually.
//!
//! The reader has parsed these bounds into [`Stats`] since M1 and nothing read them. What that costs
//! is five queries of ClickBench. q39 through q43 filter on `EventDate` over a file written in
//! `EventDate` order, which is the shape this was invented for, and on a ten million row sample DuckDB
//! answers q40 in 43 ms holding 103 MiB where rudb takes 281 ms holding 1.11 GiB. Six times the time
//! and eleven times the memory, all of it pages neither engine needed and only one of them read.
//!
//! # What is deliberately not here
//!
//! The older `min` and `max` fields of the statistics structure are not read, by [`read_stats`], and
//! this inherits that. Writers disagreed about how they ordered strings, which is why `min_value` and
//! `max_value` were added, and a skip decided on a bound whose ordering is in doubt is a wrong answer
//! rather than a slow one.
//!
//! Nulls are not reasoned about beyond the one case that is free. A comparison is false for a null
//! operand, so a group whose values all fall outside the bound can still be skipped whatever its null
//! count is: the nulls would not have passed either. What is not done is the other direction, where a
//! group that is all nulls could be skipped for any comparison at all, because `nulls` is optional in
//! the format and the saving is a group that is cheap to read anyway.
//!
//! Page level bounds are not here, and the reason is that they are not in the files. The column
//! index and the offset index are a separate structure that the footer only points at, and
//! [`ColumnChunk::column_index`] reads the pointer. On the `hits.parquet` that ClickBench
//! distributes, written by parquet-cpp 1.5.1, there is no pointer to read, and DuckDB v1.4.1 writes
//! none either, so on every file this engine is measured against the bounds a page level skip would
//! be decided on were never written. `tests/pageindex.rs` is that finding as a test.
//!
//! It would not have helped on the query that costs the most even if they were there. DuckDB writes
//! `URL` as one plain page per row group, ten and a half megabytes of it, so the page and the row
//! group are the same unit on the one column worth skipping. Finer bounds need a file with finer
//! pages in it, which is a writer decision rather than a reader one.
//!
//! [`ColumnChunk::column_index`]: crate::ColumnChunk::column_index
//!
//! [`read_stats`]: crate::metadata::read_stats
//! [`Stats`]: crate::metadata::Stats

use std::sync::Arc;

use rudb_common::bounds::{End, Spread, Zones, kept, scale_of};
use rudb_common::stat::Provenance;
use rudb_common::{LogicalType, Stat};

use crate::metadata::{ColumnChunk, Metadata, Physical, RowGroup, SchemaColumn};

/// The bounds vocabulary, which is shared rather than restated.
///
/// [`Op`], [`Bound`] and [`Test`] live in `rudb-common` because a Parquet row group is one of three
/// places this engine keeps a minimum and a maximum, and the reasoning about what those two numbers
/// rule out is the same wherever they came from. What is Parquet's own is everything below:
/// decoding a writer's bytes into a bound, and knowing which chunk of which row group to look in.
pub use rudb_common::bounds::{Bound, Op, Test};

/// The footer's zone maps, asked what the planner asks rather than what a scan asks.
///
/// A scan walks the groups and skips the ones it can. The planner wants the same walk summed rather
/// than iterated: how many rows are left once the skippable groups are gone. That is the same
/// [`skips`] over the same footer, which is why this is here and not somewhere that would have to
/// parse it again.
///
/// It holds the metadata the reader already parsed, shared rather than copied, so building one
/// costs a refcount. That matters: the file it is built over on ClickBench has a hundred and five
/// columns in eight thousand row groups, and copying the bounds out would cost more than the answer
/// is worth.
#[derive(Debug)]
pub struct Footer {
    /// What the reader parsed out of the end of the file.
    metadata: Arc<Metadata>,
}

impl Footer {
    /// The zone maps of a file whose footer has been read.
    #[must_use]
    pub fn new(metadata: Arc<Metadata>) -> Self {
        Self { metadata }
    }
}

impl Zones for Footer {
    fn column(&self, name: &str) -> Option<usize> {
        self.metadata.schema.iter().position(|column| column.name == name)
    }

    fn surviving(&self, tests: &[Test]) -> Option<u64> {
        let mut total: u64 = 0;
        for group in &self.metadata.row_groups {
            if skips(tests, group, &self.metadata.schema) {
                continue;
            }
            // A group whose count does not read as a row count gives up the whole answer rather
            // than being left out of the sum. Leaving it out would turn a ceiling into a number
            // below the truth, which is the one way this can be wrong that costs rows.
            total = total.checked_add(u64::try_from(group.rows).ok()?)?;
        }
        Some(total)
    }

    fn spread(&self, tests: &[Test]) -> Option<Spread> {
        let mut passing = 0.0_f64;
        let mut whole = 0.0_f64;
        let mut read = 0;
        for group in &self.metadata.row_groups {
            let rows = rows(group.rows);
            let spread = fraction(tests, group, &self.metadata.schema);
            whole += rows;
            passing += rows * spread.fraction;
            // The most any one group could read, rather than a sum or a count of the groups. A
            // writer is free to state statistics for a column in one group and not in the next, and
            // the caller is charging its constant for the tests nobody answered, so the question is
            // whether anybody answered this one anywhere.
            read = read.max(spread.read);
        }
        (read > 0 && whole > 0.0)
            .then(|| Spread { fraction: (passing / whole).clamp(0.0, 1.0), read })
    }

    fn extreme(&self, column: usize, end: End) -> Stat<Bound> {
        let Some(schema) = self.metadata.schema.get(column) else { return Stat::Unknown };
        let mut folded: Option<Bound> = None;
        for group in &self.metadata.row_groups {
            let Some(chunk) = group.columns.iter().find(|chunk| chunk.column == column) else {
                return Stat::Unknown;
            };
            let Some(stats) = chunk.stats.as_ref() else { return Stat::Unknown };
            let Some(bytes) = stats.bound(end) else {
                // No bound, which is what a chunk of nothing but nulls looks like. `MIN` and `MAX`
                // skip nulls, so a chunk holding only them has nothing to contribute and the rest
                // of the file still answers. A chunk that did not say why it has no bound could be
                // holding anything, so it gives up the answer.
                if stats.nulls == Some(chunk.values) {
                    continue;
                }
                return Stat::Unknown;
            };
            if !stats.exact(end, chunk.physical) {
                return Stat::Unknown;
            }
            let Some(bound) = read_bound(bytes, schema) else { return Stat::Unknown };
            // A float bound is not read as an answer even when the writer called it exact. The
            // format keeps NaN out of the bounds and this engine sorts NaN above every number, so
            // the largest value of a column holding one is a NaN the footer never mentions.
            if matches!(bound, Bound::Real(_)) {
                return Stat::Unknown;
            }
            folded = match folded {
                None => Some(bound),
                Some(so_far) => match end.further(&so_far, &bound) {
                    Some(further) => Some(further),
                    // Two bounds of one column that do not order against each other, which is a
                    // decimal too wide to restate at the other's scale. Nothing to fold them with.
                    None => return Stat::Unknown,
                },
            };
        }
        folded.map_or(Stat::Unknown, |value| Stat::exact(value, Provenance::ZoneMap))
    }
}

/// The fraction of one group these tests are expected to keep, and how many of them said so.
///
/// Tests on one column are intersected and not multiplied, which [`kept`] does and this feeds. Tests
/// on different columns are multiplied, which assumes the columns are independent of each other and
/// is the same assumption the estimator makes everywhere else.
///
/// A group this cannot read at all keeps a fraction of one rather than being left out of the sum.
/// Leaving it out would divide by the groups that were read and report their fraction as the whole
/// file's, which is a number about part of a file wearing the name of all of it.
fn fraction(tests: &[Test], group: &RowGroup, schema: &[SchemaColumn]) -> Spread {
    let mut spread = Spread { fraction: 1.0, read: 0 };
    for (position, test) in tests.iter().enumerate() {
        // Once per column and not once per test, because `kept` is given every test on the column
        // and answers for all of them together. The first mention of a column is the one that asks.
        if tests[..position].iter().any(|earlier| earlier.column == test.column) {
            continue;
        }
        let Some(column) = schema.get(test.column) else { continue };
        let Some(chunk) = group.columns.iter().find(|chunk| chunk.column == test.column) else {
            continue;
        };
        let Some(stats) = chunk.stats.as_ref() else { continue };
        let Some(low) = stats.min.as_deref().and_then(|bytes| read_bound(bytes, column)) else {
            continue;
        };
        let Some(high) = stats.max.as_deref().and_then(|bytes| read_bound(bytes, column)) else {
            continue;
        };
        let Some(kept) = kept(tests, test.column, &low, &high) else { continue };
        spread.fraction *= kept.fraction;
        spread.read += kept.read;
    }
    spread
}

/// A group's row count as a weight, floored at zero for a count that does not read as one.
#[expect(clippy::cast_precision_loss, reason = "a row count is a weight here and not an identity")]
fn rows(count: i64) -> f64 {
    if count > 0 { count as f64 } else { 0.0 }
}

/// Whether the bounds say no row of this group can satisfy every one of these tests.
///
/// The tests are the conjuncts of one filter, so one of them ruling the group out rules it out. A
/// test this cannot decide, for any of the reasons in [`Bound`], says nothing and the next one is
/// tried. With no tests at all, or no bounds written, nothing is skipped, which is exactly the
/// behaviour every caller had before this existed.
#[must_use]
pub fn skips(tests: &[Test], group: &RowGroup, schema: &[SchemaColumn]) -> bool {
    tests.iter().any(|test| {
        let Some(column) = schema.get(test.column) else { return false };
        let Some(chunk) = group.columns.iter().find(|chunk| chunk.column == test.column) else {
            return false;
        };
        rules_out(test, chunk, column)
    })
}

/// Whether one test rules out one column chunk.
fn rules_out(test: &Test, chunk: &ColumnChunk, column: &SchemaColumn) -> bool {
    let Some(stats) = chunk.stats.as_ref() else { return false };
    let low = stats.min.as_deref().and_then(|bytes| read_bound(bytes, column));
    let high = stats.max.as_deref().and_then(|bytes| read_bound(bytes, column));
    rudb_common::bounds::excluded(test.op, &test.value, low.as_ref(), high.as_ref())
}

/// Reads a bound out of the bytes a writer put in the footer, in the column's own domain.
///
/// The format says a bound is the value in its plain encoding, which for the fixed width types is
/// the little endian word and for a byte array is the bytes with no length in front of them. A width
/// that does not match the physical type is a writer this reader does not understand, and answering
/// `None` for it keeps the row group rather than guessing at what it meant.
///
/// The annotation is consulted and not only the physical type, because Parquet stores an unsigned
/// integer in a signed physical one and says the bound is ordered as unsigned. Reading a `UINTEGER`
/// above two billion as an `i32` gives a negative number, and a skip decided on that is a row group
/// dropped from an answer, so the unsigned types read through the unsigned word of the same width.
fn read_bound(bytes: &[u8], column: &SchemaColumn) -> Option<Bound> {
    let unsigned = matches!(
        column.ty,
        LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
    );
    if let Some(scale) = scale_of(&column.ty) {
        let unscaled = match column.physical {
            Physical::Int32 => i128::from(i32::from_le_bytes(bytes.try_into().ok()?)),
            Physical::Int64 => i128::from(i64::from_le_bytes(bytes.try_into().ok()?)),
            // A decimal too wide for a word is two's complement big endian and of whatever length
            // the writer needed, which is the one place in this format the bytes run the other way.
            Physical::FixedLenByteArray | Physical::ByteArray => big_endian(bytes)?,
            Physical::Boolean | Physical::Float | Physical::Double | Physical::Int96 => {
                return None;
            }
        };
        return Some(Bound::Scaled { unscaled, scale });
    }
    match column.physical {
        Physical::Boolean => bytes.first().map(|byte| Bound::Int(i128::from(*byte != 0))),
        Physical::Int32 if unsigned => {
            Some(Bound::Int(i128::from(u32::from_le_bytes(bytes.try_into().ok()?))))
        }
        Physical::Int64 if unsigned => {
            Some(Bound::Int(i128::from(u64::from_le_bytes(bytes.try_into().ok()?))))
        }
        Physical::Int32 => Some(Bound::Int(i128::from(i32::from_le_bytes(bytes.try_into().ok()?)))),
        Physical::Int64 => Some(Bound::Int(i128::from(i64::from_le_bytes(bytes.try_into().ok()?)))),
        Physical::Float => Some(Bound::Real(f64::from(f32::from_le_bytes(bytes.try_into().ok()?)))),
        Physical::Double => Some(Bound::Real(f64::from_le_bytes(bytes.try_into().ok()?))),
        Physical::ByteArray => Some(Bound::Bytes(bytes.to_vec())),
        // An INT96 has no ordering this reader agrees with anybody about, and a fixed length byte
        // array that is not a decimal is a UUID, which is not read yet.
        Physical::Int96 | Physical::FixedLenByteArray => None,
    }
}

/// A two's complement big endian integer of up to sixteen bytes, which is how a wide decimal is
/// written.
///
/// Longer than sixteen is a decimal this reader cannot hold, and answering `None` for it keeps the
/// row group. Shorter is sign extended, which is what makes a negative one byte bound negative
/// rather than a number over a hundred.
fn big_endian(bytes: &[u8]) -> Option<i128> {
    if bytes.is_empty() || bytes.len() > 16 {
        return None;
    }
    let sign = if bytes[0] & 0x80 == 0 { 0 } else { u8::MAX };
    let mut whole = [sign; 16];
    whole[16 - bytes.len()..].copy_from_slice(bytes);
    Some(i128::from_be_bytes(whole))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::LogicalType;
    use rudb_common::bounds::{MICROS, Zones};

    use super::{Bound, Footer, Op, Test, read_bound, skips};
    use crate::metadata::{
        ColumnChunk, Encoding, Metadata, Physical, RowGroup, SchemaColumn, Stats,
    };

    /// A schema of one `INTEGER` column called `d`, which every test here filters on.
    fn schema() -> Vec<SchemaColumn> {
        vec![SchemaColumn {
            name: "d".to_string(),
            physical: Physical::Int32,
            ty: LogicalType::Integer,
            optional: false,
            width: 0,
        }]
    }

    /// One row group whose only column holds values between `low` and `high`.
    fn group(low: Option<i32>, high: Option<i32>) -> RowGroup {
        RowGroup {
            columns: vec![ColumnChunk {
                column: 0,
                physical: Physical::Int32,
                compression: rudb_compress::Codec::Uncompressed,
                encodings: vec![Encoding::Plain],
                values: 100,
                compressed_size: 400,
                uncompressed_size: 400,
                data_page_offset: 4,
                dictionary_page_offset: None,
                stats: Some(Stats {
                    nulls: Some(0),
                    distinct: None,
                    min: low.map(|number| number.to_le_bytes().to_vec()),
                    max: high.map(|number| number.to_le_bytes().to_vec()),
                    min_exact: None,
                    max_exact: None,
                }),
                column_index: None,
                offset_index: None,
            }],
            rows: 100,
            bytes: 400,
        }
    }

    /// One test against column zero.
    fn test(op: Op, number: i32) -> Vec<Test> {
        vec![Test { column: 0, op, value: Bound::Int(i128::from(number)) }]
    }

    #[test]
    fn a_group_whose_values_are_all_below_the_bound_is_skipped() {
        let group = group(Some(1), Some(10));
        assert!(skips(&test(Op::Greater, 10), &group, &schema()));
        assert!(skips(&test(Op::GreaterOrEqual, 11), &group, &schema()));
        assert!(!skips(&test(Op::Greater, 9), &group, &schema()));
        assert!(!skips(&test(Op::GreaterOrEqual, 10), &group, &schema()));
    }

    #[test]
    fn a_group_whose_values_are_all_above_the_bound_is_skipped() {
        let group = group(Some(20), Some(30));
        assert!(skips(&test(Op::Less, 20), &group, &schema()));
        assert!(skips(&test(Op::LessOrEqual, 19), &group, &schema()));
        assert!(!skips(&test(Op::Less, 21), &group, &schema()));
        assert!(!skips(&test(Op::LessOrEqual, 20), &group, &schema()));
    }

    #[test]
    fn equality_skips_a_group_the_constant_falls_outside() {
        let group = group(Some(5), Some(15));
        assert!(skips(&test(Op::Equal, 4), &group, &schema()));
        assert!(skips(&test(Op::Equal, 16), &group, &schema()));
        assert!(!skips(&test(Op::Equal, 5), &group, &schema()));
        assert!(!skips(&test(Op::Equal, 15), &group, &schema()));
        assert!(!skips(&test(Op::Equal, 9), &group, &schema()));
    }

    #[test]
    fn one_conjunct_ruling_the_group_out_rules_it_out() {
        let group = group(Some(5), Some(15));
        let mut both = test(Op::GreaterOrEqual, 5);
        both.extend(test(Op::Greater, 100));
        assert!(skips(&both, &group, &schema()), "the second says no row can pass");
        let mut neither = test(Op::GreaterOrEqual, 5);
        neither.extend(test(Op::LessOrEqual, 15));
        assert!(!skips(&neither, &group, &schema()));
    }

    #[test]
    fn a_group_with_no_statistics_is_never_skipped() {
        let mut group = group(Some(5), Some(15));
        group.columns[0].stats = None;
        assert!(!skips(&test(Op::Equal, 999), &group, &schema()));
    }

    #[test]
    fn a_bound_the_writer_left_out_is_never_decided() {
        assert!(!skips(&test(Op::Greater, 999), &group(Some(1), None), &schema()));
        assert!(!skips(&test(Op::Less, 0), &group(None, Some(10)), &schema()));
        assert!(
            skips(&test(Op::Less, 0), &group(Some(1), None), &schema()),
            "the low end is there"
        );
    }

    #[test]
    fn a_constant_of_another_domain_decides_nothing() {
        let group = group(Some(5), Some(15));
        let mismatched =
            vec![Test { column: 0, op: Op::Equal, value: Bound::Bytes(b"nine".to_vec()) }];
        assert!(!skips(&mismatched, &group, &schema()));
    }

    #[test]
    fn no_tests_skip_nothing() {
        assert!(!skips(&[], &group(Some(5), Some(15)), &schema()));
    }

    /// The bug this was written to stop. Parquet keeps a `UINTEGER` in a physical `INT32` and orders
    /// its statistics as unsigned, so a group running from three billion to four billion has a
    /// minimum whose bytes read as a negative number if the annotation is ignored. Read that way the
    /// group looks like it holds small values and a filter for large ones throws it away.
    #[test]
    fn an_unsigned_column_is_read_as_unsigned() {
        let mut schema = schema();
        schema[0].ty = LogicalType::UInteger;
        let bytes = |number: u32| i32::from_le_bytes(number.to_le_bytes());
        let group = group(Some(bytes(3_000_000_000)), Some(bytes(4_000_000_000)));
        let test = vec![Test { column: 0, op: Op::Greater, value: Bound::Int(2_000_000_000) }];
        assert!(!skips(&test, &group, &schema), "the group is entirely above two billion");
    }

    /// A footer over the given groups, with the one column schema every test here uses.
    fn footer(groups: Vec<RowGroup>) -> Footer {
        let rows = groups.iter().map(|group| group.rows).sum();
        Footer::new(Arc::new(Metadata {
            version: 2,
            rows,
            schema: schema(),
            row_groups: groups,
            created_by: None,
        }))
    }

    #[test]
    fn a_column_is_found_by_the_name_the_file_wrote_and_not_by_any_other() {
        let footer = footer(vec![group(Some(1), Some(10))]);
        assert_eq!(footer.column("d"), Some(0));
        // A column the query computed rather than read looks like this from here, and the caller
        // has to give up on the whole estimate rather than test some other column by accident.
        assert_eq!(footer.column("nothing_of_the_sort"), None);
    }

    #[test]
    fn the_surviving_rows_are_the_rows_of_the_groups_that_were_not_ruled_out() {
        let footer = footer(vec![
            group(Some(1), Some(10)),
            group(Some(20), Some(30)),
            group(Some(40), Some(50)),
        ]);
        assert_eq!(footer.surviving(&[]), Some(300), "no test rules anything out");
        assert_eq!(footer.surviving(&test(Op::Greater, 35)), Some(100), "only the last one");
        assert_eq!(footer.surviving(&test(Op::Less, 15)), Some(100), "only the first one");
        assert_eq!(footer.surviving(&test(Op::Greater, 15)), Some(200), "the last two");
    }

    #[test]
    fn a_constant_outside_every_group_leaves_no_rows_at_all() {
        // The answer the estimator turns into an exact zero, which is the one case where bounds
        // say something certain rather than something smaller than the guess.
        let footer = footer(vec![group(Some(1), Some(10)), group(Some(20), Some(30))]);
        assert_eq!(footer.surviving(&test(Op::Greater, 1000)), Some(0));
        assert_eq!(footer.surviving(&test(Op::Equal, 15)), Some(0));
    }

    #[test]
    fn a_group_nothing_can_decide_is_counted_in_full() {
        // Which is the conservative direction. A group kept that could have gone costs an estimate
        // above the truth, and an estimate above the truth is a slower plan and not a wrong answer.
        let mut unwritten = group(Some(1), Some(10));
        unwritten.columns[0].stats = None;
        let footer = footer(vec![group(Some(20), Some(30)), unwritten]);
        assert_eq!(footer.surviving(&test(Op::Greater, 1000)), Some(100));
    }

    #[test]
    fn a_file_of_no_row_groups_answers_zero_rather_than_nothing() {
        // Zero rows is a fact about the file and the estimator is right to take it as one. The
        // `None` this returns is reserved for a count that did not read as a row count.
        assert_eq!(footer(Vec::new()).surviving(&test(Op::Equal, 1)), Some(0));
    }

    /// The fraction `spread` answers for these tests, which is what every test below asks about.
    fn spread(footer: &Footer, tests: &[Test]) -> Option<f64> {
        footer.spread(tests).map(|spread| spread.fraction)
    }

    #[test]
    fn the_spread_is_interpolated_inside_every_group_and_weighted_by_its_rows() {
        // Three groups of a hundred rows each running 1 to 10, 20 to 30 and 40 to 50. `x < 25`
        // keeps all of the first, the five values 20 to 24 out of the eleven in the second, and
        // none of the third, which is a hundred plus forty five point four rows out of three
        // hundred. The ceiling for the same filter is two hundred, because a group that holds
        // anything at all survives whole, and that gap is the whole point of having both.
        let footer = footer(vec![
            group(Some(1), Some(10)),
            group(Some(20), Some(30)),
            group(Some(40), Some(50)),
        ]);
        let fraction = spread(&footer, &test(Op::Less, 25)).expect("every group was read");
        assert!((fraction - (100.0 + 500.0 / 11.0) / 300.0).abs() < 1e-9, "{fraction}");
        assert_eq!(footer.surviving(&test(Op::Less, 25)), Some(200));
    }

    #[test]
    fn a_group_the_bounds_rule_out_contributes_nothing_to_the_spread() {
        let footer = footer(vec![group(Some(1), Some(10)), group(Some(20), Some(30))]);
        assert_eq!(spread(&footer, &test(Op::Greater, 1000)), Some(0.0));
        assert_eq!(spread(&footer, &test(Op::Less, 1000)), Some(1.0));
    }

    #[test]
    fn a_group_that_cannot_be_interpolated_is_carried_whole_rather_than_left_out() {
        // Leaving it out would divide by the groups that were read and report their fraction as
        // the file's, which is a number about part of the file wearing the name of the whole of it.
        // Here the second group is half the rows and says nothing, so the answer cannot go below a
        // half however narrow the filter on the first group is.
        let mut unwritten = group(Some(1), Some(10));
        unwritten.columns[0].stats = None;
        let footer = footer(vec![group(Some(20), Some(30)), unwritten]);
        assert_eq!(spread(&footer, &test(Op::Greater, 1000)), Some(0.5));
        // And the test still counts as read, because one group answered it. Charging the caller's
        // constant for a condition that was answered would apply two guesses to one condition.
        assert_eq!(footer.spread(&test(Op::Greater, 1000)).map(|spread| spread.read), Some(1));
    }

    #[test]
    fn a_test_no_group_can_interpolate_is_nothing_rather_than_the_whole_file() {
        // The caller's signal to fall back to its constant. Answering one here would say the filter
        // keeps everything, which is a claim about the data made out of having read none of it.
        assert_eq!(spread(&footer(Vec::new()), &test(Op::Less, 5)), None, "no group to read");
        let read = footer(vec![group(Some(1), Some(10)), group(Some(20), Some(30))]);
        assert_eq!(spread(&read, &test(Op::Equal, 5)), None, "a range cannot divide by a value");
        assert_eq!(spread(&read, &[]), None);
        assert!(spread(&read, &test(Op::Less, 5)).is_some(), "a range it can read comes back");
    }

    #[test]
    fn two_tests_on_one_column_name_the_interval_between_them() {
        // `x >= 22 AND x < 25` names the three values 22, 23 and 24 of the eleven in the group.
        // Multiplying two fractions would give nine elevenths times five elevenths, which is a
        // wider interval than the one asked for and is the error this shape used to have.
        let footer = footer(vec![group(Some(20), Some(30))]);
        let mut both = test(Op::GreaterOrEqual, 22);
        both.extend(test(Op::Less, 25));
        let fraction = spread(&footer, &both).expect("both were read");
        assert!((fraction - 3.0 / 11.0).abs() < 1e-9, "{fraction}");
        assert_eq!(footer.spread(&both).map(|spread| spread.read), Some(2));
    }

    /// One `DECIMAL(15, 2)` column stored the way DuckDB stores `l_discount`, as an `INT64` of
    /// hundredths, with a group running from `low` to `high` in those hundredths.
    fn decimal_group(low: i64, high: i64) -> (Vec<SchemaColumn>, RowGroup) {
        let schema = vec![SchemaColumn {
            name: "d".to_string(),
            physical: Physical::Int64,
            ty: LogicalType::Decimal { width: 15, scale: 2 },
            optional: false,
            width: 0,
        }];
        let mut group = group(None, None);
        group.columns[0].physical = Physical::Int64;
        group.columns[0].stats = Some(Stats {
            nulls: Some(0),
            distinct: None,
            min: Some(low.to_le_bytes().to_vec()),
            max: Some(high.to_le_bytes().to_vec()),
            min_exact: None,
            max_exact: None,
        });
        (schema, group)
    }

    #[test]
    fn a_decimal_columns_bounds_are_read_as_the_scale_the_schema_states() {
        let (schema, group) = decimal_group(0, 10);
        let bytes = 7_i64.to_le_bytes();
        assert_eq!(
            read_bound(&bytes, &schema[0]),
            Some(Bound::Scaled { unscaled: 7, scale: 2 }),
            "the footer's 7 is 0.07 and not 7"
        );
        // And the whole path: `d > 0.10` rules the group out and `d > 0.07` does not, where reading
        // the bound as a plain integer would have made both of them 7 and 10 against a 0.
        let above = vec![Test {
            column: 0,
            op: Op::Greater,
            value: Bound::Scaled { unscaled: 10, scale: 2 },
        }];
        assert!(skips(&above, &group, &schema));
        let inside = vec![Test {
            column: 0,
            op: Op::Greater,
            value: Bound::Scaled { unscaled: 7, scale: 2 },
        }];
        assert!(!skips(&inside, &group, &schema));
    }

    #[test]
    fn a_decimal_group_is_interpolated_over_the_hundredths_it_holds() {
        // The constant arrives at scale 3 and the column is at scale 2, so the counting is over the
        // finer of the two: 0.000 to 0.100 is 101 thousandths and `d <= 0.070` keeps 71 of them.
        // Over the column's own hundredths it would be 8 of 11, which is the same fraction to
        // within one step, and taking the finer grid is what keeps a constant between two of the
        // column's values from being rounded onto one of them.
        let (schema, group) = decimal_group(0, 10);
        let footer = Footer::new(Arc::new(Metadata {
            version: 2,
            rows: group.rows,
            schema,
            row_groups: vec![group],
            created_by: None,
        }));
        let tests = vec![Test {
            column: 0,
            op: Op::LessOrEqual,
            value: Bound::Scaled { unscaled: 70, scale: 3 },
        }];
        let fraction = spread(&footer, &tests).expect("a decimal group interpolates");
        assert!((fraction - 71.0 / 101.0).abs() < 1e-9, "{fraction}");
    }

    #[test]
    fn a_wide_decimal_is_read_from_its_bytes_the_way_it_was_written() {
        // A `DECIMAL(38, 4)` goes in a fixed length byte array, two's complement and big endian,
        // which is the one place in this format the bytes run the other way round.
        let column = SchemaColumn {
            name: "d".to_string(),
            physical: Physical::FixedLenByteArray,
            ty: LogicalType::Decimal { width: 38, scale: 4 },
            optional: false,
            width: 16,
        };
        let positive = 123_456_i128.to_be_bytes();
        assert_eq!(
            read_bound(&positive, &column),
            Some(Bound::Scaled { unscaled: 123_456, scale: 4 })
        );
        // Shorter than sixteen bytes is sign extended, so a one byte negative stays negative rather
        // than reading as a number over a hundred.
        assert_eq!(read_bound(&[0xff], &column), Some(Bound::Scaled { unscaled: -1, scale: 4 }));
        assert_eq!(read_bound(&[], &column), None, "no bytes is no bound");
        assert_eq!(read_bound(&[0; 17], &column), None, "wider than this reader holds");
    }

    #[test]
    fn a_timestamp_columns_bounds_are_read_at_the_files_own_unit() {
        // The same instant in the three units a file may choose, each of which has to come back as
        // the same quantity so that the microseconds every constant arrives as compares with it.
        for (ty, unscaled, scale) in [
            (LogicalType::TimestampMs, 1_700_000_000_000_i64, 3),
            (LogicalType::Timestamp, 1_700_000_000_000_000, MICROS),
            (LogicalType::TimestampNs, 1_700_000_000_000_000_000, 9),
        ] {
            let column = SchemaColumn {
                name: "t".to_string(),
                physical: Physical::Int64,
                ty,
                optional: false,
                width: 0,
            };
            let bytes = unscaled.to_le_bytes();
            let read = read_bound(&bytes, &column).expect("a timestamp bound is read");
            assert_eq!(read, Bound::Scaled { unscaled: i128::from(unscaled), scale });
            let micros = Bound::Scaled { unscaled: 1_700_000_000_000_000, scale: MICROS };
            assert_eq!(read.order(&micros), Some(std::cmp::Ordering::Equal), "{read:?}");
        }
    }
}
