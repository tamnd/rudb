//! What a table function call resolves to.
//!
//! A table function is a function written where a table goes, so `FROM range(10)` produces ten rows
//! of one column the same way `FROM t` produces whatever is in `t`. That makes it a different
//! resolution problem from [`crate::signature`]: the answer is not a return type, it is a list of
//! columns, because the caller can alias them and select from them and join against them.
//!
//! Four of them are here. `range` and `generate_series` between them account for two thousand
//! records in DuckDB's `sqllogictest` corpus, because a test that needs a thousand rows should not
//! have to write a thousand rows, and the corpus uses them the way a person uses a for loop. The
//! difference between those two is one row: `range` stops before the end and `generate_series`
//! stops on it, which is the difference between a half open interval and a closed one, and it is
//! the only difference. Nothing else about them differs, including the name of the column, which is
//! the function's own name in both cases.
//!
//! `read_parquet` and `read_csv` are the other two and they are a different kind of thing, because
//! their columns are in the file rather than in this table. That is what [`Columns`] exists to say.
//! A caller that resolves one of those has to open the file to finish resolving it, and
//! [`crate::file`] is where that happens. For CSV there is nothing in the file that states the
//! columns either, so opening it means sniffing it.

use rudb_common::{Error, Field, LogicalType, Result};

/// Which table function a call resolved to.
///
/// An enum rather than a name, because the executor dispatches on this and a string comparison per
/// operator build is a string comparison that can be spelled wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFunction {
    /// `range(stop)`, `range(start, stop)`, `range(start, stop, step)`, stopping before the end.
    Range,
    /// The same three, stopping on the end.
    GenerateSeries,
    /// `read_parquet(path)`, the rows of a Parquet file.
    ReadParquet,
    /// `read_csv(path)`, the rows of a CSV file, with everything about how it is written sniffed.
    ReadCsv,
}

impl TableFunction {
    /// The name the plan records and an error message says.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Range => "range",
            Self::GenerateSeries => "generate_series",
            Self::ReadParquet => "read_parquet",
            Self::ReadCsv => "read_csv",
        }
    }

    /// Whether the last value is produced.
    ///
    /// Only the two series functions differ here. The file readers answer false and nothing asks
    /// them.
    #[must_use]
    pub const fn inclusive(self) -> bool {
        matches!(self, Self::GenerateSeries)
    }

    /// The function of that name, if there is one.
    #[must_use]
    pub fn lookup(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("range") {
            return Some(Self::Range);
        }
        if name.eq_ignore_ascii_case("generate_series") {
            return Some(Self::GenerateSeries);
        }
        if name.eq_ignore_ascii_case("read_parquet") || name.eq_ignore_ascii_case("parquet_scan") {
            return Some(Self::ReadParquet);
        }
        // `read_csv_auto` is the older spelling and DuckDB still answers to it. It meant sniffing
        // back when `read_csv` did not sniff unless it was told to, and today they are the same
        // function, which is why they are the same variant here.
        if name.eq_ignore_ascii_case("read_csv") || name.eq_ignore_ascii_case("read_csv_auto") {
            return Some(Self::ReadCsv);
        }
        None
    }
}

/// Where a call's columns come from.
///
/// A table function that produces a fixed set of columns is resolved by this crate and nothing
/// else has to be consulted. One that reads a file is not, because the columns are in the file, so
/// the answer here is which file to open rather than what is in it. An enum rather than an empty
/// column list, because an empty list is what `read_parquet` of a file with no columns would also
/// give and a caller that forgot to handle the case would get an empty table instead of an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Columns {
    /// The columns this call produces, with the names an unaliased call gives them.
    Fixed(Vec<Field>),
    /// The columns of the Parquet file the first argument names.
    Parquet,
    /// The columns of the CSV file the first argument names, which are sniffed out of its front.
    Csv,
}

/// A resolved table function call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTable {
    /// Which function.
    pub function: TableFunction,
    /// What each argument has to be cast to, the same length as what was passed in.
    pub arguments: Vec<LogicalType>,
    /// Where the columns the call produces come from.
    pub columns: Columns,
}

/// Resolve a table function call by name and the types of its arguments.
///
/// The series pair does not consult the types, only the count, because it takes integers in every
/// position and the binder casts to that, so there is nothing there for a type to choose between.
/// DuckDB also has a timestamp and interval form of both, which is a second set of columns rather
/// than a second overload of the same ones, and adding it means adding it rather than widening this.
///
/// The file readers do consult them, because DuckDB does. `read_parquet(3)` and `read_csv(3)` are
/// binder errors there rather than reads of a file called `3`, which was measured against the binary
/// rather than assumed, and it is the right answer: a path that arrived as a number is a query that
/// meant something else.
///
/// # Errors
///
/// When no table function has that name, or when it has that name and not those arguments.
pub fn resolve_table(name: &str, arguments: &[LogicalType]) -> Result<ResolvedTable> {
    let Some(function) = TableFunction::lookup(name) else {
        return Err(Error::catalog(format!("Table Function with name {name} does not exist!")));
    };
    if let Some(columns) = file_columns(function) {
        if arguments.len() != 1 || arguments[0] != LogicalType::Varchar {
            return Err(no_overload(function, arguments));
        }
        return Ok(ResolvedTable { function, arguments: vec![LogicalType::Varchar], columns });
    }
    let arity = arguments.len();
    if !(1..=3).contains(&arity) {
        return Err(Error::binder(format!(
            "Table function {}() takes between 1 and 3 arguments, {arity} were given",
            function.name()
        )));
    }
    Ok(ResolvedTable {
        function,
        arguments: vec![LogicalType::BigInt; arity],
        columns: Columns::Fixed(vec![Field::new(function.name(), LogicalType::BigInt)]),
    })
}

/// Where a file reading table function's columns come from, and `None` for one that does not read
/// a file.
fn file_columns(function: TableFunction) -> Option<Columns> {
    match function {
        TableFunction::ReadParquet => Some(Columns::Parquet),
        TableFunction::ReadCsv => Some(Columns::Csv),
        TableFunction::Range | TableFunction::GenerateSeries => None,
    }
}

/// DuckDB's message for a call that matched a name and no overload of it.
///
/// The candidate list it prints carries fifteen named parameters that none of them accept here, so
/// what is listed is the one overload that exists. The first line is the one a test in the wild
/// asserts on and it is reproduced exactly.
fn no_overload(function: TableFunction, arguments: &[LogicalType]) -> Error {
    let written: Vec<String> = arguments.iter().map(ToString::to_string).collect();
    Error::binder(format!(
        "No function matches the given name and argument types '{}({})'. You might need to add \
         explicit type casts.\n\tCandidate functions:\n\t{}(VARCHAR)\n",
        function.name(),
        written.join(", "),
        function.name()
    ))
}

/// The values `start`, `stop` and `step` produce, in order.
///
/// Whole rather than an iterator because the caller wants them in a vector to build a vector out
/// of, and because the count is known up front, which is what keeps a three million row `range`
/// from growing a `Vec` twenty times on the way there.
///
/// A step of zero is an error and is the one case that is not simply an empty result. Everything
/// else that produces nothing produces nothing: a start past a stop with a positive step, a start
/// before a stop with a negative one, and the two of them equal under `range`.
///
/// # Errors
///
/// When the step is zero, with DuckDB's own wording.
pub fn series(function: TableFunction, start: i64, stop: i64, step: i64) -> Result<Vec<i64>> {
    let count = series_length(function, start, stop, step)?;
    let mut out = Vec::with_capacity(count);
    let mut at = start;
    for _ in 0..count {
        out.push(at);
        // The count was worked out from the same three numbers, so this cannot pass the stop, and
        // a saturating add is what keeps a step near the end of the range from wrapping into a
        // value on the wrong side of it rather than stopping.
        at = at.saturating_add(step);
    }
    Ok(out)
}

/// How many values the series has, without producing any of them.
///
/// The executor wants this and not the values. `range(100000000)` is a hundred row chunks a
/// hundred thousand times over, and building the whole run first to find out how long it is would
/// be eight hundred megabytes for a query whose answer is one number.
///
/// This is also where the step is checked, so the check happens once rather than in each of the
/// two callers.
///
/// # Errors
///
/// When the step is zero, with DuckDB's own wording.
pub fn series_length(function: TableFunction, start: i64, stop: i64, step: i64) -> Result<usize> {
    if step == 0 {
        return Err(Error::binder("interval cannot be 0!"));
    }
    Ok(length(function, start, stop, step))
}

/// How many values the series has.
///
/// In `i128` because `range(-9223372036854775808, 9223372036854775807)` is a legal call whose
/// length does not fit in an `i64`, and a length that overflows into a negative is a `Vec` capacity
/// that panics rather than a query that fails.
fn length(function: TableFunction, start: i64, stop: i64, step: i64) -> usize {
    let start = i128::from(start);
    let stop = i128::from(stop);
    let step = i128::from(step);
    let span = if function.inclusive() {
        if step > 0 { stop - start + 1 } else { stop - start - 1 }
    } else {
        stop - start
    };
    if (span > 0) != (step > 0) {
        return 0;
    }
    // Rounding away from zero, since a span of five over a step of two is three values and not two.
    let count = (span + step - step.signum()) / step;
    usize::try_from(count).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed columns of a resolved call, which every function that does not read a file has.
    fn fixed(resolved: &ResolvedTable) -> &[Field] {
        match &resolved.columns {
            Columns::Fixed(fields) => fields,
            Columns::Parquet | Columns::Csv => {
                panic!("{} resolves to a file", resolved.function.name())
            }
        }
    }

    /// A call of `count` integer arguments, which is what every series call looks like.
    fn integers(count: usize) -> Vec<LogicalType> {
        vec![LogicalType::BigInt; count]
    }

    #[test]
    fn a_name_that_is_not_a_table_function_says_so_rather_than_binding() {
        let error = resolve_table("read_csv", &integers(1)).unwrap_err();
        assert!(error.to_string().contains("read_csv"), "{error}");
    }

    #[test]
    fn both_names_resolve_and_each_one_names_its_own_column() {
        let range = resolve_table("range", &integers(1)).unwrap();
        assert_eq!(fixed(&range)[0].name, "range");
        let series = resolve_table("GENERATE_SERIES", &integers(3)).unwrap();
        assert_eq!(fixed(&series)[0].name, "generate_series");
        assert_eq!(series.arguments.len(), 3);
    }

    #[test]
    fn no_arguments_and_four_arguments_are_both_the_arity_error() {
        assert!(resolve_table("range", &integers(0)).is_err());
        assert!(resolve_table("range", &integers(4)).is_err());
    }

    #[test]
    fn a_series_call_ignores_the_types_it_was_given_and_casts_them_all_to_bigint() {
        let resolved =
            resolve_table("range", &[LogicalType::Varchar, LogicalType::Double]).unwrap();
        assert_eq!(resolved.arguments, integers(2));
    }

    #[test]
    fn read_parquet_takes_one_string_and_says_its_columns_are_in_the_file() {
        let resolved = resolve_table("read_parquet", &[LogicalType::Varchar]).unwrap();
        assert_eq!(resolved.function, TableFunction::ReadParquet);
        assert_eq!(resolved.arguments, vec![LogicalType::Varchar]);
        assert_eq!(resolved.columns, Columns::Parquet);
    }

    #[test]
    fn parquet_scan_is_the_same_function_under_duckdbs_other_name_for_it() {
        assert_eq!(TableFunction::lookup("parquet_scan"), Some(TableFunction::ReadParquet));
        // And it records itself under the one name, so a plan does not have two spellings in it.
        let resolved = resolve_table("parquet_scan", &[LogicalType::Varchar]).unwrap();
        assert_eq!(resolved.function.name(), "read_parquet");
    }

    #[test]
    fn a_path_that_is_not_a_string_is_the_message_duckdb_gives_for_it() {
        // Measured against v1.4.1 on server3: `read_parquet(3)` does not cast, it fails to match.
        let error = resolve_table("read_parquet", &[LogicalType::Integer]).unwrap_err();
        assert!(
            error.message().starts_with(
                "No function matches the given name and argument types 'read_parquet(INTEGER)'."
            ),
            "{error}"
        );
        assert!(error.message().contains("read_parquet(VARCHAR)"), "{error}");
    }

    #[test]
    fn read_parquet_of_no_arguments_or_two_is_the_same_no_overload_message() {
        let two = resolve_table("read_parquet", &[LogicalType::Varchar, LogicalType::Varchar]);
        assert!(two.unwrap_err().message().contains("read_parquet(VARCHAR, VARCHAR)"));
        let none = resolve_table("read_parquet", &[]);
        assert!(none.unwrap_err().message().contains("read_parquet()"));
    }

    #[test]
    fn range_stops_before_the_end_and_generate_series_stops_on_it() {
        assert_eq!(series(TableFunction::Range, 0, 3, 1).unwrap(), vec![0, 1, 2]);
        assert_eq!(series(TableFunction::GenerateSeries, 0, 3, 1).unwrap(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_step_that_does_not_divide_the_span_stops_before_the_end_of_it() {
        // DuckDB gives 2, 4, 6 for both of these. The seven is not reached by either, which is
        // where the two functions stop being different.
        assert_eq!(series(TableFunction::Range, 2, 7, 2).unwrap(), vec![2, 4, 6]);
        assert_eq!(series(TableFunction::GenerateSeries, 2, 7, 2).unwrap(), vec![2, 4, 6]);
    }

    #[test]
    fn a_negative_step_counts_down_and_stops_on_the_same_rule() {
        assert_eq!(series(TableFunction::Range, 5, 1, -2).unwrap(), vec![5, 3]);
        assert_eq!(series(TableFunction::GenerateSeries, 5, 1, -2).unwrap(), vec![5, 3, 1]);
    }

    #[test]
    fn a_step_going_the_wrong_way_produces_nothing_rather_than_running_forever() {
        assert!(series(TableFunction::Range, 0, 10, -1).unwrap().is_empty());
        assert!(series(TableFunction::Range, 10, 0, 1).unwrap().is_empty());
    }

    #[test]
    fn an_empty_range_and_a_single_value_series_are_the_boundary_between_the_two() {
        assert!(series(TableFunction::Range, 4, 4, 1).unwrap().is_empty());
        assert_eq!(series(TableFunction::GenerateSeries, 4, 4, 1).unwrap(), vec![4]);
    }

    #[test]
    fn a_step_of_zero_is_the_one_case_that_is_an_error_rather_than_nothing() {
        let error = series(TableFunction::Range, 1, 5, 0).unwrap_err();
        assert!(error.to_string().contains("interval cannot be 0"), "{error}");
    }

    #[test]
    fn a_span_that_does_not_fit_in_an_i64_does_not_overflow_the_length() {
        // Not run, only counted. The point is that the count is worked out in i128, so this comes
        // out as a huge number rather than as a negative one that becomes a capacity panic.
        assert_eq!(length(TableFunction::Range, i64::MIN, i64::MAX, 1), usize::MAX);
    }
}
