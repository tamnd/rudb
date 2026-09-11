//! One schema out of several files.
//!
//! `read_csv('data/*.csv')` is one stream of rows and a stream has one schema, so the files a
//! pattern matched have to agree on one. A Parquet file states its schema and the first file's is
//! taken as the answer, but a CSV file states nothing, so every file is sniffed and the answers are
//! combined. That is not an optimisation choice, it is a correctness one: a directory of daily
//! exports where one day's file happens to hold whole numbers in a column that is otherwise decimal
//! would otherwise come out BIGINT or DOUBLE depending on which day sorted first.
//!
//! Both rules here were measured against duckdb v1.4.1 rather than reasoned about.
//!
//! The types combine by [`widen`]. Two integers stay an integer, an integer and a double become a
//! double, and everything else becomes text. Notably a date and a timestamp become text rather than
//! a timestamp, which is not what a type lattice would say and is what the binary does.
//!
//! The names have to match, and a file that is missing a column is [`mismatch`], which is a
//! different sentence from the one the Parquet reader gives for the same situation because the two
//! readers in DuckDB are two pieces of code that each wrote their own.

use rudb_common::{Error, Field, LogicalType, Result};

/// The type a column has to be for values from both files to fit in it.
///
/// Measured: BIGINT with DOUBLE is DOUBLE, BIGINT with VARCHAR is VARCHAR, BOOLEAN with BIGINT is
/// VARCHAR, DATE with BIGINT is VARCHAR, DATE with TIMESTAMP is VARCHAR. So the only pair that
/// widens to anything but text is the numeric one, and everything else falls back to the type that
/// holds whatever was written.
#[must_use]
pub fn widen(one: &LogicalType, other: &LogicalType) -> LogicalType {
    if one == other {
        return one.clone();
    }
    let numeric = |ty: &LogicalType| matches!(ty, LogicalType::BigInt | LogicalType::Double);
    if numeric(one) && numeric(other) {
        return LogicalType::Double;
    }
    LogicalType::Varchar
}

/// The columns of a read that covers several files, given each file's own sniffed columns.
///
/// The first file fixes the names and their order. Every file after it has to have all of them, by
/// name, and contributes its types.
///
/// # Errors
///
/// When a file is missing a column the first one has, with DuckDB's own wording.
pub fn across(sniffed: &[(String, Vec<Field>)]) -> Result<Vec<Field>> {
    let Some((main, first)) = sniffed.first() else { return Ok(Vec::new()) };
    let mut fields = first.clone();
    for (path, held) in &sniffed[1..] {
        for field in &mut fields {
            let found = held
                .iter()
                .find(|column| column.name == field.name)
                .ok_or_else(|| mismatch(main, path, &field.name))?;
            field.ty = widen(&field.ty, &found.ty);
        }
    }
    Ok(fields)
}

/// DuckDB's message for a globbed file that does not have a column the first file has.
///
/// The trailing space after `Potential Fixes` is the binary's and is kept, because a compatibility
/// test that compares output compares all of it and a difference that is invisible on a terminal is
/// the worst kind to go looking for later.
#[must_use]
pub fn mismatch(main: &str, current: &str, missing: &str) -> Error {
    Error::invalid_input(format!(
        "Schema mismatch between globbed files.\nMain file schema: {main}\nCurrent file: \
         {current}\nColumn with name: \"{missing}\" is missing\nPotential Fixes \n* Consider \
         setting union_by_name=true.\n* Consider setting files_to_sniff to a higher value (e.g., \
         files_to_sniff = -1)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, ty: LogicalType) -> Field {
        Field::new(name, ty)
    }

    #[test]
    fn the_only_pair_that_widens_to_anything_but_text_is_the_numeric_one() {
        assert_eq!(widen(&LogicalType::BigInt, &LogicalType::BigInt), LogicalType::BigInt);
        assert_eq!(widen(&LogicalType::BigInt, &LogicalType::Double), LogicalType::Double);
        assert_eq!(widen(&LogicalType::Double, &LogicalType::BigInt), LogicalType::Double);
        assert_eq!(widen(&LogicalType::Boolean, &LogicalType::BigInt), LogicalType::Varchar);
        assert_eq!(widen(&LogicalType::Date, &LogicalType::BigInt), LogicalType::Varchar);
        // Not TIMESTAMP, which is what a lattice would say and is not what the binary does.
        assert_eq!(widen(&LogicalType::Date, &LogicalType::Timestamp), LogicalType::Varchar);
    }

    #[test]
    fn the_first_file_fixes_the_names_and_every_file_contributes_a_type() {
        let sniffed = vec![
            (
                "one.csv".to_string(),
                vec![named("a", LogicalType::BigInt), named("b", LogicalType::BigInt)],
            ),
            (
                "two.csv".to_string(),
                vec![named("a", LogicalType::Double), named("b", LogicalType::BigInt)],
            ),
        ];
        let fields = across(&sniffed).expect("agrees");
        assert_eq!(fields[0].ty, LogicalType::Double);
        assert_eq!(fields[1].ty, LogicalType::BigInt);
        assert_eq!(fields[0].name, "a");
    }

    #[test]
    fn a_column_that_is_not_in_a_later_file_is_duckdbs_own_complaint() {
        let sniffed = vec![
            ("one.csv".to_string(), vec![named("a", LogicalType::BigInt)]),
            ("two.csv".to_string(), vec![named("z", LogicalType::BigInt)]),
        ];
        let error = across(&sniffed).unwrap_err();
        assert!(error.message().starts_with("Schema mismatch between globbed files."), "{error}");
        assert!(error.message().contains("Main file schema: one.csv"), "{error}");
        assert!(error.message().contains("Column with name: \"a\" is missing"), "{error}");
        assert!(error.message().contains("union_by_name=true"), "{error}");
    }

    #[test]
    fn a_column_order_that_differs_between_files_is_matched_by_name_and_not_by_position() {
        let sniffed = vec![
            (
                "one.csv".to_string(),
                vec![named("a", LogicalType::BigInt), named("b", LogicalType::Varchar)],
            ),
            (
                "two.csv".to_string(),
                vec![named("b", LogicalType::Varchar), named("a", LogicalType::Double)],
            ),
        ];
        let fields = across(&sniffed).expect("agrees");
        assert_eq!(fields[0].name, "a");
        assert_eq!(fields[0].ty, LogicalType::Double);
    }
}
