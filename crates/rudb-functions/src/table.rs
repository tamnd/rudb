//! What a table function call resolves to.
//!
//! A table function is a function written where a table goes, so `FROM range(10)` produces ten rows
//! of one column the same way `FROM t` produces whatever is in `t`. That makes it a different
//! resolution problem from [`crate::signature`]: the answer is not a return type, it is a list of
//! columns, because the caller can alias them and select from them and join against them.
//!
//! Two of them are `range` and `generate_series`, which between them account for two thousand
//! records in DuckDB's `sqllogictest` corpus. They exist because a test that needs a thousand rows
//! should not have to write a thousand rows, and the corpus uses them the way a person uses a for
//! loop.
//!
//! The difference between those two is one row. `range` stops before the end and `generate_series`
//! stops on it, which is the difference between a half open interval and a closed one, and it is
//! the only difference. Nothing else about them differs, including the name of the column, which is
//! the function's own name in both cases.
//!
//! The third is `read_parquet`, which is a different kind of thing and is why [`ResolvedTable`] has
//! a column list that is allowed to be empty. A series knows its one column from its name. A file
//! knows its columns from its footer, and this crate is rank 7 and cannot read a footer, so the
//! resolution here stops at "one `VARCHAR` in, columns decided later" and the binder fills the rest
//! in from the file.

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
    /// `read_parquet(path)`, whose columns are whatever the file's footer says they are.
    ReadParquet,
}

impl TableFunction {
    /// The name the plan records and an error message says.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Range => "range",
            Self::GenerateSeries => "generate_series",
            Self::ReadParquet => "read_parquet",
        }
    }

    /// Whether the last value is produced.
    #[must_use]
    pub const fn inclusive(self) -> bool {
        matches!(self, Self::GenerateSeries)
    }

    /// Whether the call names a file, so that its columns come from the file and not from here.
    #[must_use]
    pub const fn reads_a_file(self) -> bool {
        matches!(self, Self::ReadParquet)
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
        if name.eq_ignore_ascii_case("read_parquet") {
            return Some(Self::ReadParquet);
        }
        None
    }
}

/// A resolved table function call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTable {
    /// Which function.
    pub function: TableFunction,
    /// What each argument has to be cast to, the same length as what was passed in.
    pub arguments: Vec<LogicalType>,
    /// The columns the call produces, with the names an unaliased call gives them.
    ///
    /// Empty when [`TableFunction::reads_a_file`] is true, because the file decides and the binder
    /// is the one holding a reader. An empty list here is not a call that produces no columns.
    pub columns: Vec<Field>,
}

/// Resolve a table function call by name and argument count.
///
/// The types are not consulted, only the count. Both of these take integers in every position and
/// the binder casts to that, so there is nothing here for a type to choose between. DuckDB also has
/// a timestamp and interval form of both, which is a second set of columns rather than a second
/// overload of the same ones, and adding it means adding it rather than widening this.
///
/// # Errors
///
/// When no table function has that name, or when it has that name and not that many arguments.
pub fn resolve_table(name: &str, arity: usize) -> Result<ResolvedTable> {
    let Some(function) = TableFunction::lookup(name) else {
        return Err(Error::catalog(format!("Table Function with name {name} does not exist!")));
    };
    if function.reads_a_file() {
        // The arity is not checked here. DuckDB reports a wrong argument count and a wrong argument
        // type as the same error, and that error prints the types that were passed, which are not
        // known until the binder has bound them. So this hands back a signature that mirrors what
        // was written and [`check_table_arguments`] is where both halves are decided at once.
        return Ok(ResolvedTable {
            function,
            arguments: vec![LogicalType::Varchar; arity],
            columns: Vec::new(),
        });
    }
    if !(1..=3).contains(&arity) {
        return Err(Error::binder(format!(
            "Table function {}() takes between 1 and 3 arguments, {arity} were given",
            function.name()
        )));
    }
    Ok(ResolvedTable {
        function,
        arguments: vec![LogicalType::BigInt; arity],
        columns: vec![Field::new(function.name(), LogicalType::BigInt)],
    })
}

/// Check the arguments of a resolved call, which the name and the count on their own do not settle.
///
/// `range` casts whatever it was given to `BIGINT` and that is the whole of its type rule, so this
/// is about `read_parquet`, which does not cast. `read_parquet(42)` is an error in DuckDB rather
/// than a file called `42`, and it has to be, because the alternative is that a typo in a path
/// argument becomes a file lookup for whatever the typo stringifies to.
///
/// # Errors
///
/// When there is not exactly one argument, or when one is a type the function does not take, with
/// DuckDB's wording for each case.
pub fn check_table_arguments(function: TableFunction, given: &[LogicalType]) -> Result<()> {
    if !function.reads_a_file() {
        return Ok(());
    }
    // One path and nothing else. DuckDB's real signature has fifteen named parameters after it and
    // a second overload taking a list of paths, and neither is here.
    if given.len() != 1 {
        return Err(mismatch(function.name(), given));
    }
    for ty in given {
        // DuckDB reports this one from the parser, which is where it decides whether the argument
        // is a path or a list of paths, and a null is neither. The message is theirs.
        if *ty == LogicalType::Null {
            return Err(Error::parser(format!(
                "{} cannot take NULL list as parameter",
                function.name()
            )));
        }
        if *ty != LogicalType::Varchar {
            return Err(mismatch(function.name(), given));
        }
    }
    Ok(())
}

/// The "no function matches" error, with the types that were actually passed.
///
/// DuckDB follows this with every candidate signature it has, which for `read_parquet` is two lines
/// of fifteen named parameters that nothing here implements. Printing them would be claiming to
/// take arguments that would be ignored, so the candidate list says what this build accepts.
fn mismatch(name: &str, given: &[LogicalType]) -> Error {
    let given = given.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    Error::binder(format!(
        "No function matches the given name and argument types '{name}({given})'. You might need to add explicit type casts.\n\tCandidate functions:\n\t{name}(VARCHAR)\n"
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

    #[test]
    fn a_name_that_is_not_a_table_function_says_so_rather_than_binding() {
        let error = resolve_table("read_csv", 1).unwrap_err();
        assert!(error.to_string().contains("read_csv"), "{error}");
    }

    #[test]
    fn read_parquet_resolves_to_one_path_and_no_columns_of_its_own() {
        let resolved = resolve_table("read_parquet", 1).unwrap();
        assert_eq!(resolved.function, TableFunction::ReadParquet);
        assert_eq!(resolved.arguments, vec![LogicalType::Varchar]);
        assert!(resolved.columns.is_empty(), "the footer decides, not this crate");
        assert!(check_table_arguments(resolved.function, &[LogicalType::Varchar]).is_ok());
    }

    #[test]
    fn read_parquet_refuses_a_count_or_a_type_it_does_not_take() {
        // DuckDB reports all three of these as the same error, and it names the types it was
        // handed, which is why the count is checked here and not where the name is resolved.
        let none = check_table_arguments(TableFunction::ReadParquet, &[]).unwrap_err();
        assert!(none.to_string().contains("'read_parquet()'"), "{none}");
        let two = check_table_arguments(
            TableFunction::ReadParquet,
            &[LogicalType::Varchar, LogicalType::Varchar],
        )
        .unwrap_err();
        assert!(two.to_string().contains("'read_parquet(VARCHAR, VARCHAR)'"), "{two}");
        let number =
            check_table_arguments(TableFunction::ReadParquet, &[LogicalType::Integer]).unwrap_err();
        assert!(number.to_string().contains("'read_parquet(INTEGER)'"), "{number}");
    }

    #[test]
    fn a_null_path_is_the_one_that_comes_back_from_the_parser() {
        // Odd but theirs. DuckDB decides whether the argument is a path or a list of paths before
        // it binds anything, and a null is neither, so the error carries the parser's name.
        let error =
            check_table_arguments(TableFunction::ReadParquet, &[LogicalType::Null]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Parser Error: read_parquet cannot take NULL list as parameter"
        );
    }

    #[test]
    fn a_series_takes_whatever_it_was_given_and_casts_it() {
        // The check is about the file readers. Nothing about `range` is decided by argument type,
        // so this has to stay out of the way of it.
        assert!(check_table_arguments(TableFunction::Range, &[LogicalType::Varchar]).is_ok());
        assert!(check_table_arguments(TableFunction::GenerateSeries, &[]).is_ok());
    }

    #[test]
    fn both_names_resolve_and_each_one_names_its_own_column() {
        let range = resolve_table("range", 1).unwrap();
        assert_eq!(range.columns[0].name, "range");
        let series = resolve_table("GENERATE_SERIES", 3).unwrap();
        assert_eq!(series.columns[0].name, "generate_series");
        assert_eq!(series.arguments.len(), 3);
    }

    #[test]
    fn no_arguments_and_four_arguments_are_both_the_arity_error() {
        assert!(resolve_table("range", 0).is_err());
        assert!(resolve_table("range", 4).is_err());
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
