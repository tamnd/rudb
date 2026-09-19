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

use rudb_common::LogicalType;

use crate::metadata::{ColumnChunk, Physical, RowGroup, SchemaColumn};

/// The bounds vocabulary, which is shared rather than restated.
///
/// [`Op`] and [`Bound`] live in `rudb-common` because a Parquet row group is one of three places
/// this engine keeps a minimum and a maximum, and the reasoning about what those two numbers rule
/// out is the same wherever they came from. What is Parquet's own is everything below: decoding a
/// writer's bytes into a bound, and knowing which chunk of which row group to look in.
pub use rudb_common::bounds::{Bound, Op};

/// One comparison against one column, with the bound it is testing for.
#[derive(Debug, Clone)]
pub struct Test {
    /// Which column of the file's flat schema, which is what a row group's chunks are indexed by.
    pub column: usize,
    /// The comparison, written with the column on the left.
    pub op: Op,
    /// The constant the column is compared against.
    pub value: Bound,
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
    let low = stats.min.as_deref().and_then(|bytes| read(bytes, column));
    let high = stats.max.as_deref().and_then(|bytes| read(bytes, column));
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
fn read(bytes: &[u8], column: &SchemaColumn) -> Option<Bound> {
    let unsigned = matches!(
        column.ty,
        LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::UHugeInt
    );
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
        // array is a decimal or a UUID depending on the annotation, neither of which is read yet.
        Physical::Int96 | Physical::FixedLenByteArray => None,
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::LogicalType;

    use super::{Bound, Op, Test, skips};
    use crate::metadata::{ColumnChunk, Encoding, Physical, RowGroup, SchemaColumn, Stats};

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
}
