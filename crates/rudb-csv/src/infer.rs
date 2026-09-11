//! What type a column of text is.
//!
//! A CSV file has no types in it, so a reader that wants to hand a `BIGINT` to an aggregate has to
//! decide, and a wrong decision is a wrong answer rather than a slow query. The rule is DuckDB's
//! and it is a ladder: every value in the column is tried against each type in turn and the first
//! type every value fits is the column's type, with `VARCHAR` at the bottom because everything fits
//! it. A column of nothing but nulls is `VARCHAR` too, which is the bottom of the same ladder.
//!
//! The order was read off duckdb v1.4.1 rather than chosen. `BOOLEAN` before `BIGINT` matters,
//! because a column of `true` and `false` is a boolean and not a pair of words. `BIGINT` before
//! `DOUBLE` matters, because a column of whole numbers should be whole. `DATE` before `TIMESTAMP`
//! is the order the binary uses and nothing in it overlaps, so the order is only visible here.
//!
//! `TIME` is a rung DuckDB has and this does not, and the gap is deliberate rather than forgotten.
//! `rudb-kernels` has no cast from `VARCHAR` to `TIME`, so a column sniffed as one would be a column
//! declared with a type its own values cannot be converted to, which is a worse answer than the
//! `VARCHAR` it falls through to. The rung goes in when the cast does, and the test below says so in
//! both directions so that adding the cast without adding the rung fails.
//!
//! Two of the tests are the sniffer's rather than the cast's, and both were measured. `007` is a
//! `VARCHAR` here although `CAST('007' AS BIGINT)` is 7, because a column of zero padded numbers is
//! a column of codes and adding them up is not what anybody meant. `+1` is a `VARCHAR` for the same
//! sort of reason. Everything else defers to the cast, which is the point: a string the sniffer
//! calls a `BIGINT` is a string the reader then casts to `BIGINT`, so a test that disagreed with the
//! cast would produce a column whose declared type its own values do not fit.

use rudb_common::{LogicalType, Value};
use rudb_kernels::cast_value;

/// The types tried, in the order they are tried. `VARCHAR` is the bottom and is not in here because
/// it never fails.
pub const LADDER: [LogicalType; 5] = [
    LogicalType::Boolean,
    LogicalType::BigInt,
    LogicalType::Double,
    LogicalType::Date,
    LogicalType::Timestamp,
];

/// How many rows the sniffer looks at.
///
/// DuckDB's `sample_size` default, which it prints in the block under a conversion error. A value
/// past this that does not fit the type the sample chose is an error at read time rather than a
/// wider type, because widening would mean going back and rewriting the chunks already handed out.
pub const SAMPLE: usize = 20480;

/// The type of a column, given every value the sample had for it.
///
/// A `None` is a null and is skipped, because a null fits every type and a column that is all nulls
/// has nothing to go on.
#[must_use]
pub fn column(values: &[Option<&str>]) -> LogicalType {
    if values.iter().all(Option::is_none) {
        // Otherwise every rung is satisfied vacuously and the column comes back as the first one.
        return LogicalType::Varchar;
    }
    for candidate in LADDER {
        if values.iter().flatten().all(|text| fits(text, &candidate)) {
            return candidate;
        }
    }
    LogicalType::Varchar
}

/// Whether one value would read as `candidate`.
#[must_use]
pub fn fits(text: &str, candidate: &LogicalType) -> bool {
    match candidate {
        LogicalType::Boolean => is_boolean(text),
        LogicalType::BigInt if !numeric(text) => false,
        LogicalType::Double if !numeric(text) => false,
        _ => {
            let value = Value::Varchar(text.to_string());
            matches!(cast_value(&value, candidate, true), Ok(converted) if !converted.is_null())
        }
    }
}

/// The spellings DuckDB's sniffer reads as a boolean.
///
/// Not `1` and `0`, although the cast takes both, because a column of ones and zeroes is a column of
/// numbers far more often than it is a column of flags and the binary agrees. Not `y` and `n`
/// either, and not `on` and `off`, both of which were tried against it.
fn is_boolean(text: &str) -> bool {
    ["true", "false", "t", "f", "yes", "no"]
        .iter()
        .any(|spelling| text.trim().eq_ignore_ascii_case(spelling))
}

/// Whether a number written like this is a number to the sniffer.
///
/// The two rules that are not the cast's. A leading `+` is refused, and so is a leading zero with
/// another digit behind it, which is how a column of `007` stays a column of `007` rather than
/// becoming a column of sevens. The sign is looked past for neither of them, because the binary does
/// not look past it either: `-007` really is a `BIGINT` there and this reproduces that rather than
/// tidying it up.
fn numeric(text: &str) -> bool {
    let text = text.trim();
    let mut bytes = text.bytes();
    match bytes.next() {
        Some(b'+') => false,
        Some(b'0') => !matches!(bytes.next(), Some(byte) if byte.is_ascii_digit()),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(values: &[&str]) -> LogicalType {
        let values: Vec<Option<&str>> = values.iter().map(|text| Some(*text)).collect();
        column(&values)
    }

    #[test]
    fn each_rung_of_the_ladder_is_the_type_duckdb_sniffs_for_it() {
        assert_eq!(of(&["true", "false"]), LogicalType::Boolean);
        assert_eq!(of(&["1", "2"]), LogicalType::BigInt);
        assert_eq!(of(&["1.5", "2"]), LogicalType::Double);
        assert_eq!(of(&["2020-01-02", "2021-03-04"]), LogicalType::Date);
        assert_eq!(of(&["2020-01-02 03:04:05"]), LogicalType::Timestamp);
        assert_eq!(of(&["1", "x"]), LogicalType::Varchar);
    }

    #[test]
    fn a_column_of_times_is_a_varchar_here_and_a_time_in_duckdb() {
        // The one rung this ladder is missing, and the test is here so that the gap is a thing
        // somebody deleted rather than a thing somebody never noticed. It goes away when
        // `rudb-kernels` can cast a VARCHAR to a TIME, and the second assertion is what says so.
        assert_eq!(of(&["03:04:05"]), LogicalType::Varchar);
        let value = Value::Varchar("03:04:05".into());
        assert!(
            cast_value(&value, &LogicalType::Time, true).is_err(),
            "the cast exists now, so TIME belongs back in the ladder"
        );
    }

    #[test]
    fn a_column_of_nothing_but_nulls_is_a_varchar() {
        assert_eq!(column(&[None, None]), LogicalType::Varchar);
        assert_eq!(column(&[]), LogicalType::Varchar);
    }

    #[test]
    fn a_null_in_a_column_does_not_change_what_the_rest_of_it_is() {
        assert_eq!(column(&[Some("1"), None, Some("2")]), LogicalType::BigInt);
    }

    #[test]
    fn ones_and_zeroes_are_numbers_rather_than_flags() {
        // Measured. `CAST('1' AS BOOLEAN)` is true, so a ladder that asked the cast would call this
        // column a boolean, and duckdb v1.4.1 calls it a BIGINT.
        assert_eq!(of(&["0", "1"]), LogicalType::BigInt);
    }

    #[test]
    fn the_boolean_spellings_are_the_six_the_binary_takes_and_no_more() {
        for yes in ["true", "TRUE", "True", "t", "T", "yes", "Yes"] {
            assert_eq!(of(&[yes]), LogicalType::Boolean, "{yes}");
        }
        for no in ["on", "off", "y", "n"] {
            assert_eq!(of(&[no]), LogicalType::Varchar, "{no}");
        }
    }

    #[test]
    fn a_zero_padded_number_stays_the_text_it_was_written_as() {
        assert_eq!(of(&["007", "008"]), LogicalType::Varchar);
        assert_eq!(of(&["00"]), LogicalType::Varchar);
        // And the two that go the other way, both measured against the binary.
        assert_eq!(of(&["0"]), LogicalType::BigInt);
        assert_eq!(of(&["-007"]), LogicalType::BigInt);
    }

    #[test]
    fn a_leading_plus_is_not_a_number_to_the_sniffer() {
        assert_eq!(of(&["+1"]), LogicalType::Varchar);
    }

    #[test]
    fn space_around_a_number_does_not_stop_it_being_one() {
        assert_eq!(of(&[" 1", "2 "]), LogicalType::BigInt);
    }

    #[test]
    fn a_whole_number_too_big_for_a_bigint_widens_to_a_double() {
        assert_eq!(of(&["99999999999999999999"]), LogicalType::Double);
    }
}
