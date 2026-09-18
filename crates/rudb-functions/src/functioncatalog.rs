//! What `duckdb_functions()` says about each function this engine knows.
//!
//! Twenty one columns and one row per name per argument count, over the scalar and aggregate names
//! in [`crate::signature`] and the table functions in [`crate::table`]. It is the same kind of table
//! as `duckdb_types()`: a client reads it to find out what the engine supports, so it lists what the
//! engine has rather than what the pinned binary has.
//!
//! # An overload here is a name and a count, not a name and a pair of types
//!
//! This is the one place where rudb's table and upstream's are shaped differently rather than just
//! different lengths, and it follows from a decision [`crate::signature`] made first. That table
//! resolves by shape: `+` is one entry saying both arguments promote and the result is what they
//! promote to. Upstream carries an entry per pair of argument types, because it carries an
//! implementation per pair, so it reports 44 rows for `+` naming concrete types where this reports
//! two, one per arity.
//!
//! So the types in this table are declared types. `T` is the type variable and means the call
//! decides, and every argument spelled `T` in one row is the same type as the others. `ANY` is the
//! weaker one and means the argument is not tied to the others, which is what `count(x)` takes. Both
//! spellings are upstream's own, which uses `T` for `list_extract` and `lag` and `ANY` for `least`,
//! so a client that already reads this table does not have to learn a third vocabulary. A return of
//! `ANY` means the arguments decide it in a way no name can say, which is where `sum` is, since it
//! promotes and then widens an integer to the accumulator.
//!
//! # Builtins are in `system.main` and rudb has no catalog called that
//!
//! Said plainly because it is the one column here that names something that does not exist yet.
//! Upstream puts every builtin in `system.main` and `system.pg_catalog` and puts nothing in
//! `memory`, which is the opposite of `duckdb_types()`, where the types are repeated once per
//! catalog. Reporting `memory` here would break every client query that filters on the schema and
//! would say the functions belong to a database, which is not true of a builtin. So this says
//! `system.main`. The catalog tables have landed since and the catalog still has no `system` entry,
//! so `duckdb_schemas()` cannot return the name this column reports. That is #607 rather than a
//! thing this file can fix, because the entry has to come from the catalog and not from here.
//!
//! `function_oid` is null for the same reason `database_oid` is null in `duckdb_types()`: upstream's
//! is a counter its catalog handed out at startup and rudb has no oid space. `description`,
//! `comment`, `examples` and `categories` are null because rudb has no documentation strings
//! attached to its functions, and inventing a sentence per name here would put the documentation
//! somewhere nobody maintains it.
//!
//! # What the table does not have yet
//!
//! No window functions, because rudb has none. No macros, no pragma functions and no table macros,
//! for the same reason. `has_side_effects` is false and `stability` is `CONSISTENT` on every scalar
//! and aggregate row, because every function rudb has is a pure function of its arguments: there is
//! no `random`, no `nextval` and no `now` in the table yet. A volatile one arrives with a row that
//! says so rather than with this column quietly staying wrong, which is why it is derived from
//! nothing and asserted in a test.

use rudb_common::{Field, LogicalType};

use crate::signature::{FunctionKind, FunctionRow, function_rows};
use crate::table::TableFunction;

/// What one row of `duckdb_functions()` says, before it is turned into values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionEntry {
    /// The name as it is written in a query.
    pub name: &'static str,
    /// `scalar`, `aggregate` or `table`, which are the three kinds rudb has.
    pub function_type: &'static str,
    /// The name this one resolves to, and `None` for a name that is its own.
    pub alias_of: Option<&'static str>,
    /// What the call produces, and `None` for a table function, whose columns are not one type.
    pub return_type: Option<&'static str>,
    /// One name per argument, in order.
    pub parameters: Vec<String>,
    /// One type per argument, in order, the same length as `parameters`.
    pub parameter_types: Vec<String>,
    /// The type of the trailing variadic argument, for the names that take one.
    pub varargs: Option<&'static str>,
    /// Whether calling it twice can give two answers, and `None` for a table function, which is
    /// where upstream leaves this column null.
    pub has_side_effects: Option<bool>,
    /// How much the engine may reuse a result, and `None` for a table function.
    pub stability: Option<&'static str>,
}

/// The catalog builtins are reported as belonging to, which is upstream's name for it.
pub const FUNCTION_CATALOG: &str = "system";

/// The schema builtins are reported as belonging to.
pub const FUNCTION_SCHEMA: &str = "main";

/// The stability every function in this engine has, since none of them is volatile yet.
pub const CONSISTENT: &str = "CONSISTENT";

/// The columns `duckdb_functions()` produces, which is DuckDB's twenty one.
#[must_use]
pub fn function_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        // A varchar and not a bigint, which looks like a mistake upstream and is reproduced because
        // a client that reads the column reads whatever type it was handed. It is null on every row
        // there, so nothing has ever had to parse it.
        Field::new("database_oid", LogicalType::Varchar),
        Field::new("schema_name", LogicalType::Varchar),
        Field::new("function_name", LogicalType::Varchar),
        Field::new("alias_of", LogicalType::Varchar),
        Field::new("function_type", LogicalType::Varchar),
        Field::new("description", LogicalType::Varchar),
        Field::new("comment", LogicalType::Varchar),
        Field::new("tags", LogicalType::map(LogicalType::Varchar, LogicalType::Varchar)),
        Field::new("return_type", LogicalType::Varchar),
        Field::new("parameters", LogicalType::list(LogicalType::Varchar)),
        Field::new("parameter_types", LogicalType::list(LogicalType::Varchar)),
        Field::new("varargs", LogicalType::Varchar),
        Field::new("macro_definition", LogicalType::Varchar),
        Field::new("has_side_effects", LogicalType::Boolean),
        Field::new("internal", LogicalType::Boolean),
        Field::new("extension_name", LogicalType::Varchar),
        Field::new("function_oid", LogicalType::BigInt),
        Field::new("examples", LogicalType::list(LogicalType::Varchar)),
        Field::new("stability", LogicalType::Varchar),
        Field::new("categories", LogicalType::list(LogicalType::Varchar)),
    ]
}

/// Every function this engine has, sorted by name and then by how many arguments it takes.
///
/// Sorted rather than left in whatever order the two tables behind it are written in, because a
/// client reading this table is looking a name up and an unsorted catalog makes that a scan of the
/// whole thing. Upstream's own order is not reproduced: it is the order its catalog registered the
/// functions in, the two corpus records that read the table both say `order by`, and there is
/// nothing in it that is a fact about the language.
#[must_use]
pub fn function_entries() -> Vec<FunctionEntry> {
    let mut entries: Vec<FunctionEntry> = function_rows().into_iter().map(scalar).collect();
    entries.extend(tables());
    entries.sort_by(|left, right| {
        left.name.cmp(right.name).then(left.parameters.len().cmp(&right.parameters.len()))
    });
    entries
}

/// One scalar or aggregate overload, as a row.
fn scalar(row: FunctionRow) -> FunctionEntry {
    FunctionEntry {
        name: row.name,
        function_type: match row.kind {
            FunctionKind::Scalar => "scalar",
            FunctionKind::Aggregate => "aggregate",
            FunctionKind::Window => "window",
        },
        alias_of: row.alias_of,
        return_type: Some(row.returns),
        parameters: named(row.alias_of.unwrap_or(row.name), row.types.len()),
        parameter_types: row.types.iter().map(|name| (*name).to_string()).collect(),
        varargs: row.varargs,
        has_side_effects: Some(false),
        stability: Some(CONSISTENT),
    }
}

/// Every table function, one row per argument count it takes.
///
/// A table function's named parameters go on the end of `parameters` after its positional ones,
/// which is upstream's shape: its `read_csv` row is `col0` followed by forty seven option names. So
/// rudb's is `col0` followed by the six options it acts on, and that list grows as they land rather
/// than being padded out to upstream's length with names nothing reads.
fn tables() -> Vec<FunctionEntry> {
    let mut entries = Vec::new();
    for function in TABLE_FUNCTIONS {
        for count in positional_counts(*function) {
            let mut parameters = positional(count);
            let mut parameter_types = vec![positional_type(*function).to_string(); count];
            for (name, ty) in function.parameters() {
                parameters.push((*name).to_string());
                parameter_types.push(ty.to_string());
            }
            entries.push(FunctionEntry {
                name: function.name(),
                function_type: "table",
                alias_of: None,
                // Null, and upstream's is null too. A table function produces columns rather than a
                // value, so there is no one type to name, and the columns are in `duckdb_columns()`
                // for a table and in the file for a file reader.
                return_type: None,
                parameters,
                parameter_types,
                varargs: None,
                has_side_effects: None,
                stability: None,
            });
        }
    }
    for (alias, function) in TABLE_ALIASES {
        let rows: Vec<FunctionEntry> = entries
            .iter()
            .filter(|entry| entry.name == function.name())
            // `alias_of` stays null, which is upstream's answer for these two rather than an
            // omission here: `parquet_scan` and `read_csv_auto` are separate registrations there and
            // report as their own functions, where `len` and `mean` report as aliases.
            .map(|entry| FunctionEntry { name: alias, ..entry.clone() })
            .collect();
        entries.extend(rows);
    }
    entries
}

/// The table functions, in no particular order, since [`function_entries`] sorts.
const TABLE_FUNCTIONS: &[TableFunction] = &[
    TableFunction::Range,
    TableFunction::GenerateSeries,
    TableFunction::ReadParquet,
    TableFunction::ReadCsv,
    TableFunction::RudbStrategies,
    TableFunction::DuckdbKeywords,
    TableFunction::DuckdbTypes,
    TableFunction::DuckdbFunctions,
    TableFunction::DuckdbSettings,
    TableFunction::DuckdbDatabases,
    TableFunction::DuckdbSchemas,
    TableFunction::DuckdbTables,
    TableFunction::DuckdbViews,
    TableFunction::DuckdbColumns,
    TableFunction::DuckdbExtensions,
    TableFunction::DuckdbOptimizers,
    TableFunction::DuckdbDialects,
    TableFunction::DuckdbGrammarExtensions,
    TableFunction::PragmaTableInfo,
    TableFunction::PragmaShow,
    TableFunction::PragmaVersion,
    TableFunction::PragmaPlatform,
    TableFunction::PragmaUserAgent,
    TableFunction::PragmaDatabaseSize,
];

/// The second name each of the two file readers answers to.
const TABLE_ALIASES: &[(&str, TableFunction)] =
    &[("parquet_scan", TableFunction::ReadParquet), ("read_csv_auto", TableFunction::ReadCsv)];

/// How many positional arguments a table function takes, one count per row it produces.
fn positional_counts(function: TableFunction) -> Vec<usize> {
    match function {
        TableFunction::Range | TableFunction::GenerateSeries => vec![1, 2, 3],
        TableFunction::ReadParquet
        | TableFunction::ReadCsv
        | TableFunction::PragmaTableInfo
        | TableFunction::PragmaShow => vec![1],
        TableFunction::RudbStrategies
        | TableFunction::DuckdbKeywords
        | TableFunction::DuckdbTypes
        | TableFunction::DuckdbFunctions
        | TableFunction::DuckdbSettings
        | TableFunction::DuckdbDatabases
        | TableFunction::DuckdbSchemas
        | TableFunction::DuckdbTables
        | TableFunction::DuckdbViews
        | TableFunction::DuckdbColumns
        | TableFunction::DuckdbExtensions
        | TableFunction::DuckdbOptimizers
        | TableFunction::DuckdbDialects
        | TableFunction::DuckdbGrammarExtensions
        | TableFunction::PragmaVersion
        | TableFunction::PragmaPlatform
        | TableFunction::PragmaUserAgent
        | TableFunction::PragmaDatabaseSize
        | TableFunction::PragmaShowTables
        | TableFunction::PragmaShowDatabases
        | TableFunction::PragmaShowTablesExpanded => vec![0],
    }
}

/// The type a table function's positional arguments take.
const fn positional_type(function: TableFunction) -> &'static str {
    match function {
        TableFunction::ReadParquet
        | TableFunction::ReadCsv
        | TableFunction::PragmaTableInfo
        | TableFunction::PragmaShow => "VARCHAR",
        _ => "BIGINT",
    }
}

/// The names the first `count` arguments go by, which are upstream's for a function whose
/// parameters have no names of their own.
fn positional(count: usize) -> Vec<String> {
    (0..count).map(|at| format!("col{at}")).collect()
}

/// The names one function's arguments go by, which is [`positional`] unless the function is in
/// [`PARAMETER_NAMES`].
fn named(name: &str, count: usize) -> Vec<String> {
    match PARAMETER_NAMES.iter().find(|(entry, _)| *entry == name) {
        Some((_, names)) if names.len() == count => {
            names.iter().map(|name| (*name).to_string()).collect()
        }
        _ => positional(count),
    }
}

/// The scalar functions whose arguments upstream gives real names rather than `col0`.
///
/// Short on purpose. Upstream names the arguments of a few dozen functions and leaves the rest as
/// `col0`, and the ones it names are the ones where the name carries information a type does not:
/// `regexp_replace(string, regex, replacement)` is three VARCHARs and the order is not guessable
/// from that. `current_setting` is here because its error message names the parameter, so a row
/// saying `col0` next to a message saying `setting_name` would be this table disagreeing with the
/// binder about the same argument.
///
/// A row only applies at the argument count it has names for, so a function with two arities keeps
/// `col0` at the arity this list does not cover rather than being given the wrong names.
const PARAMETER_NAMES: &[(&str, &[&str])] = &[("current_setting", &["setting_name"])];

#[cfg(test)]
mod tests {
    use super::{CONSISTENT, function_entries, function_fields};

    #[test]
    fn the_table_is_the_shape_the_pin_returns() {
        assert_eq!(function_fields().len(), 21);
        let entries = function_entries();
        assert!(!entries.is_empty());
        // Every row has as many parameter names as it has parameter types, which is the one thing a
        // client reading the two columns together relies on and the one thing a table built out of
        // two lists can get wrong.
        for entry in &entries {
            assert_eq!(
                entry.parameters.len(),
                entry.parameter_types.len(),
                "{} takes {} names and {} types",
                entry.name,
                entry.parameters.len(),
                entry.parameter_types.len()
            );
        }
    }

    #[test]
    fn a_name_with_two_arities_is_two_rows_and_a_name_with_one_is_one() {
        let entries = function_entries();
        let rows = |name: &str| entries.iter().filter(|entry| entry.name == name).count();
        // `+` is the unary and the binary form, so it is two rows here and 44 on the pin, which is
        // the difference between resolving by shape and carrying an implementation per type pair.
        assert_eq!(rows("+"), 2);
        assert_eq!(rows("*"), 1);
        // `substring` takes two arguments or three and not one, so the hole in the range is a hole
        // in the table rather than a row that binds a call the engine refuses.
        assert_eq!(rows("substring"), 2);
        let substring: Vec<usize> = entries
            .iter()
            .filter(|entry| entry.name == "substring")
            .map(|entry| entry.parameters.len())
            .collect();
        assert_eq!(substring, [2, 3]);
    }

    #[test]
    fn an_alias_is_a_row_of_its_own_that_says_what_it_resolves_to() {
        let entries = function_entries();
        let len: Vec<&super::FunctionEntry> =
            entries.iter().filter(|entry| entry.name == "len").collect();
        assert_eq!(len.len(), 1);
        assert_eq!(len[0].alias_of, Some("length"));
        assert_eq!(len[0].return_type, Some("BIGINT"));
        // The two file reader aliases report as their own functions and not as aliases, which is
        // what the pin does with them.
        let scan: Vec<&super::FunctionEntry> =
            entries.iter().filter(|entry| entry.name == "parquet_scan").collect();
        assert_eq!(scan.len(), 1);
        assert_eq!(scan[0].alias_of, None);
        assert_eq!(scan[0].function_type, "table");
    }

    #[test]
    fn a_shape_that_promotes_is_declared_with_the_type_variable() {
        let entries = function_entries();
        let row = |name: &str, count: usize| {
            entries
                .iter()
                .find(|entry| entry.name == name && entry.parameters.len() == count)
                .unwrap_or_else(|| panic!("{name} of {count}"))
        };
        // Both arguments meet at one type and the result is that type.
        assert_eq!(row("%", 2).parameter_types, ["T", "T"]);
        assert_eq!(row("%", 2).return_type, Some("T"));
        // Both arguments meet at one type and the result moves off it, because a decimal sum gains
        // a carry digit, so the result is the weaker spelling.
        assert_eq!(row("+", 2).parameter_types, ["T", "T"]);
        assert_eq!(row("+", 2).return_type, Some("ANY"));
        // The argument is not constrained at all and the result is fixed.
        assert_eq!(row("count", 1).parameter_types, ["ANY"]);
        assert_eq!(row("count", 1).return_type, Some("BIGINT"));
        // A string function names the type it needs, because it refuses anything else rather than
        // casting to it.
        assert_eq!(row("lower", 1).parameter_types, ["VARCHAR"]);
        assert_eq!(row("lower", 1).return_type, Some("VARCHAR"));
        // A string and then a whole number that is not cast to one.
        assert_eq!(row("substring", 3).parameter_types, ["VARCHAR", "BIGINT", "BIGINT"]);
    }

    #[test]
    fn a_table_function_has_no_return_type_and_no_stability() {
        let entries = function_entries();
        let range: Vec<&super::FunctionEntry> =
            entries.iter().filter(|entry| entry.name == "range").collect();
        // One, two or three arguments, which is what the function takes and what the pin reports.
        assert_eq!(range.len(), 3);
        for entry in &range {
            assert_eq!(entry.function_type, "table");
            assert_eq!(entry.return_type, None);
            assert_eq!(entry.stability, None);
            assert_eq!(entry.has_side_effects, None);
        }
        // A file reader's named options go on the end of its positional argument, which is the
        // shape the pin has and the reason `parameters` is longer than the call is.
        let csv = entries
            .iter()
            .find(|entry| entry.name == "read_csv")
            .expect("the csv reader is a table function");
        assert_eq!(csv.parameters[0], "col0");
        assert_eq!(csv.parameter_types[0], "VARCHAR");
        assert_eq!(
            csv.parameters[1..],
            ["all_varchar", "delim", "escape", "header", "quote", "sep"]
        );
    }

    /// The one scalar whose argument has a name, and the reason it needs one.
    #[test]
    fn a_setting_is_read_by_an_argument_the_table_names() {
        let entry = function_entries()
            .into_iter()
            .find(|entry| entry.name == "current_setting")
            .expect("a row for it");
        assert_eq!(entry.function_type, "scalar");
        assert_eq!(entry.parameters, ["setting_name"]);
        assert_eq!(entry.parameter_types, ["VARCHAR"]);
        assert_eq!(entry.return_type, Some("ANY"));
        // Every other scalar keeps `col0`, so the list is an exception and not a new convention.
        let lower = function_entries()
            .into_iter()
            .find(|entry| entry.name == "lower")
            .expect("a row for it");
        assert_eq!(lower.parameters, ["col0"]);
    }

    #[test]
    fn nothing_in_this_engine_is_volatile_yet_and_the_table_says_so() {
        // There is no `random`, no `nextval` and no `now` in the function table, so every scalar and
        // aggregate row is consistent and has no side effects. The day one of those lands this test
        // fails, which is the point: the column has to be given a real answer rather than inheriting
        // one nobody looked at.
        for entry in function_entries().iter().filter(|entry| entry.function_type != "table") {
            assert_eq!(entry.stability, Some(CONSISTENT), "{}", entry.name);
            assert_eq!(entry.has_side_effects, Some(false), "{}", entry.name);
        }
    }
}
