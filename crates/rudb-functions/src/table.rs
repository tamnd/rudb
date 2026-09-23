//! What a table function call resolves to.
//!
//! A table function is a function written where a table goes, so `FROM range(10)` produces ten rows
//! of one column the same way `FROM t` produces whatever is in `t`. That makes it a different
//! resolution problem from [`crate::signature`]: the answer is not a return type, it is a list of
//! columns, because the caller can alias them and select from them and join against them.
//!
//! Twenty two of them are here. `range` and `generate_series` between them account for two thousand
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
//!
//! The next ten are the third kind, a table whose rows are a fact about the engine rather than data
//! somebody stored. All ten take no arguments and all ten know their own columns, so resolving one
//! is the simplest case in this file and they share an arm. `rudb_strategies()` is not
//! a DuckDB function at all: it lists every seam in the engine and every implementation registered
//! against it, which is how a reader finds out what this engine will let them swap and what it lets
//! them swap today. `duckdb_keywords()` is every word the grammar knows about, which this crate can
//! answer because the grammar is vendored. `duckdb_types()` is every type name the engine has and
//! `duckdb_functions()` is every function, and both of their lists are in this crate because which
//! names exist is a fact about the type system and the function library rather than about the
//! executor. `duckdb_settings()` is every setting `SET` will take, and it is the one of the five
//! whose rows are not all known here: the names and the descriptions are, and the values come from
//! the session the query is running in. `duckdb_databases()`, `duckdb_schemas()`, `duckdb_tables()`
//! `duckdb_views()` and `duckdb_columns()` are the last five and they are further from this crate
//! again, because their rows are whatever somebody created, so only their columns are here and
//! [`crate::entrycatalog`] says why.
//!
//! `duckdb_extensions()` and `duckdb_optimizers()` are two more of that third kind and they are the
//! two where rudb has to answer about itself rather than reproduce a list. `duckdb_optimizers()` is
//! every name `SET disabled_optimizers` takes, which is DuckDB's forty four, because rudb takes all
//! forty four and turning off a pass that was never written is a request that has already been
//! granted. `duckdb_extensions()` is the same names DuckDB's default build advertises with rudb's own
//! answer in the two boolean columns, and `rudb_exec` says which two are true and why.
//!
//! `pragma_version()`, `pragma_platform()`, `pragma_user_agent()` and `pragma_database_size()` are
//! four more of that third kind and they are the four where the fact is about this build and this
//! process rather than about the language. The first three are a constant worked out from the crate
//! version and the target, and the fourth reads the catalog and the memory budget, so like
//! `duckdb_settings()` its rows are not all known here. `rudb_exec::enginenames` decides all four
//! values and argues there for why they describe rudb rather than reporting DuckDB's answers.
//!
//! D2 adds a few more of that third kind. Each one is a column list here and a list of rows in
//! `rudb_exec::metadata`, and nothing else.
//!
//! `rudb_device_card(path)` is the one of that kind that takes an argument, because the fact it
//! reports is about a directory rather than the process: what a sync costs on the device under it,
//! measured the way `engine-v4/16-measurement.md` section 16.3 says. It resolves in its own arm
//! because its argument is a path and an optional iteration count, which is neither a table name
//! nor nothing.
//!
//! `pragma_table_info()` and `pragma_show()` are a fourth kind and the first two of the pragma
//! family. They take one table name and describe whatever it names, so their columns are fixed and
//! their rows are not a fact about the engine at all, they are a fact about one entry in a catalog.
//! That makes them the first table functions here whose answer the binder settles on its own: it
//! binds the name the way `DESCRIBE` binds one and hands back the rows, which is why nothing in
//! `rudb_exec` knows either name.

use rudb_common::{Error, Field, LogicalType, Result};

use crate::entrycatalog::{
    column_fields, database_fields, schema_fields, show_database_fields, show_expanded_fields,
    show_table_fields, table_fields, view_fields,
};
use crate::functioncatalog::function_fields;
use crate::settingcatalog::setting_fields;
use crate::typecatalog::type_fields;

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
    /// `rudb_strategies()`, every seam and every implementation registered against it.
    RudbStrategies,
    /// `rudb_links()`, every relationship declared and what is stored for it.
    RudbLinks,
    /// `rudb_device_card(path)`, what a sync costs on the device a directory is on.
    RudbDeviceCard,
    /// `duckdb_keywords()`, every word the grammar knows and which class each one is in.
    DuckdbKeywords,
    /// `duckdb_types()`, every type name the engine knows and what each one stands for.
    DuckdbTypes,
    /// `duckdb_functions()`, every function the engine knows and what each one takes.
    DuckdbFunctions,
    /// `duckdb_settings()`, every setting `SET` will take and what each one is now.
    DuckdbSettings,
    /// `duckdb_databases()`, every database attached to this session.
    DuckdbDatabases,
    /// `duckdb_schemas()`, every schema in every one of them.
    DuckdbSchemas,
    /// `duckdb_tables()`, every base table somebody created.
    DuckdbTables,
    /// `duckdb_views()`, every view somebody created.
    DuckdbViews,
    /// `duckdb_columns()`, every column of every one of those.
    DuckdbColumns,
    /// `duckdb_extensions()`, every extension DuckDB names and whether this engine has it.
    DuckdbExtensions,
    /// `duckdb_optimizers()`, every name `SET disabled_optimizers` takes.
    DuckdbOptimizers,
    /// `duckdb_dialects()`, every installed SQL parser dialect.
    DuckdbDialects,
    /// `duckdb_grammar_extensions()`, every installed grammar extension.
    DuckdbGrammarExtensions,
    /// `pragma_table_info(name)`, the columns of one table or view, in SQLite's six columns.
    PragmaTableInfo,
    /// `pragma_show(name)`, the same columns again in the six `DESCRIBE` answers with.
    PragmaShow,
    /// `pragma_storage_info(name)`, what every stored part of every column of one table is.
    PragmaStorageInfo,
    /// `pragma_version()`, the version of the engine answering, in three columns.
    PragmaVersion,
    /// `pragma_platform()`, the operating system and processor this build was made for.
    PragmaPlatform,
    /// `pragma_user_agent()`, the one line a client sends when it says who it is.
    PragmaUserAgent,
    /// `pragma_database_size()`, what each attached database costs on disk and in memory.
    PragmaDatabaseSize,
    /// `PRAGMA show_tables`, the name of everything an unqualified name can reach.
    PragmaShowTables,
    /// `PRAGMA show_databases`, the name of everything that is attached.
    PragmaShowDatabases,
    /// `PRAGMA show_tables_expanded`, every table and view anywhere with its columns beside it.
    PragmaShowTablesExpanded,
}

/// The name of the column `file_row_number=True` adds.
///
/// Here rather than in the binder because the executor is the half that fills it in and the two
/// have to agree on the spelling. It is DuckDB's name for it, and the column is a row's ordinal
/// inside its own file rather than inside the read, so a glob of three files counts from zero three
/// times.
pub const FILE_ROW_NUMBER: &str = "file_row_number";

impl TableFunction {
    /// The name the plan records and an error message says.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Range => "range",
            Self::GenerateSeries => "generate_series",
            Self::ReadParquet => "read_parquet",
            Self::ReadCsv => "read_csv",
            Self::RudbStrategies => "rudb_strategies",
            Self::RudbLinks => "rudb_links",
            Self::RudbDeviceCard => "rudb_device_card",
            Self::DuckdbKeywords => "duckdb_keywords",
            Self::DuckdbTypes => "duckdb_types",
            Self::DuckdbFunctions => "duckdb_functions",
            Self::DuckdbSettings => "duckdb_settings",
            Self::DuckdbDatabases => "duckdb_databases",
            Self::DuckdbSchemas => "duckdb_schemas",
            Self::DuckdbTables => "duckdb_tables",
            Self::DuckdbViews => "duckdb_views",
            Self::DuckdbColumns => "duckdb_columns",
            Self::DuckdbExtensions => "duckdb_extensions",
            Self::DuckdbOptimizers => "duckdb_optimizers",
            Self::DuckdbDialects => "duckdb_dialects",
            Self::DuckdbGrammarExtensions => "duckdb_grammar_extensions",
            Self::PragmaTableInfo => "pragma_table_info",
            Self::PragmaShow => "pragma_show",
            Self::PragmaStorageInfo => "pragma_storage_info",
            Self::PragmaVersion => "pragma_version",
            Self::PragmaPlatform => "pragma_platform",
            Self::PragmaUserAgent => "pragma_user_agent",
            Self::PragmaDatabaseSize => "pragma_database_size",
            Self::PragmaShowTables => "pragma_show_tables",
            Self::PragmaShowDatabases => "pragma_show_databases",
            Self::PragmaShowTablesExpanded => "pragma_show_tables_expanded",
        }
    }

    /// Whether the name can be written where a table goes, rather than only after the word `PRAGMA`.
    ///
    /// Nine of the pin's nineteen query pragmas answer to `pragma_name()` in a `FROM` clause and ten
    /// do not, and which is which was measured rather than guessed. `SELECT * FROM
    /// pragma_show_tables()` on the pin is `Catalog Error: Table Function with name
    /// pragma_show_tables does not exist!` while `PRAGMA show_tables` returns rows, so the two
    /// namespaces really are separate and a name in one is not a name in the other. These three are
    /// the ones rudb has from the pragma only half of that, and the rest of that half are `ATTACH`
    /// and `COPY` in disguise or want something rudb has not written.
    #[must_use]
    pub const fn reachable_as_a_function(self) -> bool {
        !matches!(
            self,
            Self::PragmaShowTables | Self::PragmaShowDatabases | Self::PragmaShowTablesExpanded
        )
    }

    /// Whether the call takes one table name and answers about whatever that names.
    ///
    /// The two pragmas are the only ones, and they are a family rather than a pair because the rest
    /// of the `pragma_*` functions that take a name are the storage ones, which land here the day
    /// rudb has storage to describe.
    #[must_use]
    pub const fn takes_a_name(self) -> bool {
        matches!(self, Self::PragmaTableInfo | Self::PragmaShow | Self::PragmaStorageInfo)
    }

    /// Whether the answer is settled while the call is bound rather than while the query runs.
    ///
    /// The two column describing pragmas are, because the columns of a table are known by the time
    /// its name has resolved, so the rows are constants from there on and the call comes out as a
    /// `VALUES`. `pragma_storage_info` is not, because its rows are read off the file and there are
    /// as many of them as the table has parts times columns, which at SF1 is six thousand for
    /// lineitem alone. Folding that into the plan would put six thousand rows of constants through
    /// every pass the optimizer has, to produce a table the executor can hand back a chunk at a
    /// time.
    #[must_use]
    pub const fn answered_when_bound(self) -> bool {
        matches!(self, Self::PragmaTableInfo | Self::PragmaShow)
    }

    /// Whether the last value is produced.
    ///
    /// Only the two series functions differ here. The file readers answer false and nothing asks
    /// them.
    #[must_use]
    pub const fn inclusive(self) -> bool {
        matches!(self, Self::GenerateSeries)
    }

    /// The named parameters the call takes, and the type each one wants.
    ///
    /// This is the list rudb acts on and not the list DuckDB prints, and the difference is worth
    /// being plain about. `read_parquet` there takes seventeen named parameters and `read_csv`
    /// takes around thirty. One of the Parquet ones is on the critical path, since the ClickBench
    /// entry reads its file with `binary_as_string=True` and without it every string column in
    /// `hits.parquet` comes back as `BLOB`, and the other sixteen have no caller here yet. A
    /// parameter that is listed is one that does something, so this list grows as they land rather
    /// than accepting names and ignoring them, which is the failure mode that makes an option look
    /// supported when it is not.
    ///
    /// The CSV ones here are the ones that say how the file is written, which are the ones where
    /// guessing wrong changes the answer rather than the speed. `sep` is DuckDB's other name for
    /// `delim` and is a separate row rather than an alias, because the list is also what the
    /// candidates on a misspelling are read out of and the binary prints both of them.
    #[must_use]
    pub fn parameters(self) -> &'static [(&'static str, LogicalType)] {
        static READ_PARQUET: &[(&str, LogicalType)] = &[
            ("binary_as_string", LogicalType::Boolean),
            ("file_row_number", LogicalType::Boolean),
        ];
        static READ_CSV: &[(&str, LogicalType)] = &[
            ("all_varchar", LogicalType::Boolean),
            ("delim", LogicalType::Varchar),
            ("escape", LogicalType::Varchar),
            ("header", LogicalType::Boolean),
            ("quote", LogicalType::Varchar),
            ("sep", LogicalType::Varchar),
        ];
        match self {
            Self::ReadParquet => READ_PARQUET,
            Self::ReadCsv => READ_CSV,
            _ => &[],
        }
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
        if name.eq_ignore_ascii_case("rudb_strategies") {
            return Some(Self::RudbStrategies);
        }
        if name.eq_ignore_ascii_case("rudb_links") {
            return Some(Self::RudbLinks);
        }
        if name.eq_ignore_ascii_case("rudb_device_card") {
            return Some(Self::RudbDeviceCard);
        }
        if name.eq_ignore_ascii_case("duckdb_keywords") {
            return Some(Self::DuckdbKeywords);
        }
        if name.eq_ignore_ascii_case("duckdb_types") {
            return Some(Self::DuckdbTypes);
        }
        if name.eq_ignore_ascii_case("duckdb_functions") {
            return Some(Self::DuckdbFunctions);
        }
        if name.eq_ignore_ascii_case("duckdb_settings") {
            return Some(Self::DuckdbSettings);
        }
        if name.eq_ignore_ascii_case("duckdb_databases") {
            return Some(Self::DuckdbDatabases);
        }
        if name.eq_ignore_ascii_case("duckdb_schemas") {
            return Some(Self::DuckdbSchemas);
        }
        if name.eq_ignore_ascii_case("duckdb_tables") {
            return Some(Self::DuckdbTables);
        }
        if name.eq_ignore_ascii_case("duckdb_views") {
            return Some(Self::DuckdbViews);
        }
        if name.eq_ignore_ascii_case("duckdb_columns") {
            return Some(Self::DuckdbColumns);
        }
        if name.eq_ignore_ascii_case("duckdb_extensions") {
            return Some(Self::DuckdbExtensions);
        }
        if name.eq_ignore_ascii_case("duckdb_optimizers") {
            return Some(Self::DuckdbOptimizers);
        }
        if name.eq_ignore_ascii_case("duckdb_dialects") {
            return Some(Self::DuckdbDialects);
        }
        if name.eq_ignore_ascii_case("duckdb_grammar_extensions") {
            return Some(Self::DuckdbGrammarExtensions);
        }
        if name.eq_ignore_ascii_case("pragma_table_info") {
            return Some(Self::PragmaTableInfo);
        }
        if name.eq_ignore_ascii_case("pragma_show") {
            return Some(Self::PragmaShow);
        }
        if name.eq_ignore_ascii_case("pragma_storage_info") {
            return Some(Self::PragmaStorageInfo);
        }
        if name.eq_ignore_ascii_case("pragma_version") {
            return Some(Self::PragmaVersion);
        }
        if name.eq_ignore_ascii_case("pragma_platform") {
            return Some(Self::PragmaPlatform);
        }
        if name.eq_ignore_ascii_case("pragma_user_agent") {
            return Some(Self::PragmaUserAgent);
        }
        if name.eq_ignore_ascii_case("pragma_database_size") {
            return Some(Self::PragmaDatabaseSize);
        }
        if name.eq_ignore_ascii_case("pragma_show_tables") {
            return Some(Self::PragmaShowTables);
        }
        if name.eq_ignore_ascii_case("pragma_show_databases") {
            return Some(Self::PragmaShowDatabases);
        }
        if name.eq_ignore_ascii_case("pragma_show_tables_expanded") {
            return Some(Self::PragmaShowTablesExpanded);
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
    let function = match TableFunction::lookup(name) {
        // A pragma only name written in a `FROM` clause is a name that does not exist there, which
        // is the pin's answer and not a shortcut: the two namespaces are separate and this is the
        // side of the fence the caller is standing on.
        Some(function) if function.reachable_as_a_function() => function,
        _ => {
            return Err(Error::catalog(format!("Table Function with name {name} does not exist!")));
        }
    };
    resolve_found(function, arguments)
}

/// The same resolution once the name has been settled, which is where the two spellings meet.
///
/// Split out of [`resolve_table`] because a pragma only name has to get here without going past the
/// check that turns it down in a `FROM` clause.
fn resolve_found(function: TableFunction, arguments: &[LogicalType]) -> Result<ResolvedTable> {
    if let Some(columns) = file_columns(function) {
        // Two overloads, one path and a list of them, which is DuckDB's pair. The list is where
        // `read_parquet(['a.parquet', 'b.parquet'])` binds. An empty list is a list of the untyped
        // null and it binds here too, because a list with nothing in it is a fine list and the
        // objection to it is that it names no file, which is what the reader says about it rather
        // than what this table says.
        let single = arguments.len() == 1 && arguments[0] == LogicalType::Varchar;
        let many = arguments.len() == 1
            && matches!(&arguments[0], LogicalType::List(element)
                if **element == LogicalType::Varchar || **element == LogicalType::Null);
        // A bare null matches, and is a sentence about nulls rather than about overloads, which is
        // what DuckDB answers `read_parquet(NULL)` with. It is left as a null rather than cast to a
        // path so that the binder still has a null to recognise when it goes looking for the name.
        let nothing = arguments.len() == 1 && arguments[0] == LogicalType::Null;
        if !single && !many && !nothing {
            return Err(no_overload(function, arguments));
        }
        // A list keeps the type it arrived with rather than being cast to a list of strings, because
        // the two that reach here are already one of those and a cast between two list types is
        // machinery this does not need. The reader reads the values and not the declaration.
        let wanted = if many {
            arguments[0].clone()
        } else if nothing {
            LogicalType::Null
        } else {
            LogicalType::Varchar
        };
        return Ok(ResolvedTable { function, arguments: vec![wanted], columns });
    }
    if function.takes_a_name() {
        // One name, and a null is one of them. `pragma_table_info(NULL)` is a catalog error about a
        // table called NULL on the pin rather than a complaint about the argument, because the
        // pragma turns whatever it was given into text before it goes looking, so the null is left
        // as a null here and the binder does the same thing with it.
        let single = arguments.len() == 1
            && matches!(arguments[0], LogicalType::Varchar | LogicalType::Null);
        if !single {
            return Err(one_name(function, arguments));
        }
        return Ok(ResolvedTable {
            function,
            arguments: vec![arguments[0].clone()],
            columns: Columns::Fixed(name_columns(function)),
        });
    }
    if function == TableFunction::RudbDeviceCard {
        return device_card(arguments);
    }
    let arity = arguments.len();
    // The metadata tables take nothing and their columns are fixed, which makes them the simplest
    // case here. They are one arm rather than one each because the only thing that differs is the
    // column list, and a name that is added to this list and not to `lookup` cannot be reached.
    if let Some(columns) = fixed_columns(function) {
        if arity != 0 {
            return Err(nothing_at_all(function, arguments));
        }
        return Ok(ResolvedTable {
            function,
            arguments: Vec::new(),
            columns: Columns::Fixed(columns),
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
        columns: Columns::Fixed(vec![Field::new(function.name(), LogicalType::BigInt)]),
    })
}

/// The same resolution for a call the user wrote as `PRAGMA name`, whose messages spell it so.
///
/// Every pragma is an ordinary table function under a longer name, so the resolution is
/// [`resolve_table`] and nothing else. What changes is what a bad call says. Upstream writes both
/// halves of that message in the form the user used, so `PRAGMA table_info('a', 'b')` is
/// `'table_info(VARCHAR, VARCHAR)'` with a candidate line reading `PRAGMA "table_info"(VARCHAR)`.
/// Handing back a complaint about a `pragma_table_info` nobody typed would be handing the user the
/// rewrite to debug rather than their own statement.
///
/// Only two shapes can reach this. A pragma never reads a file and is never `range`, so the
/// overload it has is either one name or nothing at all, and the candidate line says which.
///
/// # Errors
///
/// When the function has that name and not those arguments, and otherwise whatever
/// [`resolve_table`] says.
pub fn resolve_pragma(name: &str, arguments: &[LogicalType]) -> Result<ResolvedTable> {
    let Some(function) = TableFunction::lookup(name) else {
        return Err(Error::catalog(format!("Table Function with name {name} does not exist!")));
    };
    // Through [`resolve_found`] rather than [`resolve_table`], because three of these names only
    // exist after the word `PRAGMA` and the other spelling is where they are turned down.
    if let Ok(resolved) = resolve_found(function, arguments) {
        return Ok(resolved);
    }
    let spelled = name.strip_prefix("pragma_").unwrap_or(name);
    // A pragma that takes nothing prints no parentheses at all on the candidate line, where the
    // function spelling of the same complaint prints an empty pair. Measured on the pin, which
    // answers `PRAGMA version(1)` with a candidate reading `PRAGMA "version"` and stopping there.
    let takes = if function.takes_a_name() { "(VARCHAR)" } else { "" };
    let written: Vec<String> = arguments.iter().map(ToString::to_string).collect();
    Err(Error::binder(format!(
        "No function matches the given name and argument types '{spelled}({})'. You might need to \
         add explicit type casts.\n\tCandidate functions:\n\tPRAGMA \"{spelled}\"{takes}\n",
        written.join(", ")
    )))
}

/// Where a file reading table function's columns come from, and `None` for one that does not read
/// a file.
fn file_columns(function: TableFunction) -> Option<Columns> {
    match function {
        TableFunction::ReadParquet => Some(Columns::Parquet),
        TableFunction::ReadCsv => Some(Columns::Csv),
        TableFunction::Range
        | TableFunction::GenerateSeries
        | TableFunction::RudbStrategies
        | TableFunction::RudbLinks
        | TableFunction::RudbDeviceCard
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
        | TableFunction::PragmaTableInfo
        | TableFunction::PragmaShow
        | TableFunction::PragmaStorageInfo
        | TableFunction::PragmaVersion
        | TableFunction::PragmaPlatform
        | TableFunction::PragmaUserAgent
        | TableFunction::PragmaDatabaseSize
        | TableFunction::PragmaShowTables
        | TableFunction::PragmaShowDatabases
        | TableFunction::PragmaShowTablesExpanded => None,
    }
}

/// The columns of a table function that takes no arguments and knows its own, and `None` for one
/// that has to look at what it was called with.
fn fixed_columns(function: TableFunction) -> Option<Vec<Field>> {
    match function {
        TableFunction::RudbStrategies => Some(strategy_fields()),
        TableFunction::RudbLinks => Some(link_fields()),
        TableFunction::DuckdbKeywords => Some(keyword_fields()),
        TableFunction::DuckdbTypes => Some(type_fields()),
        TableFunction::DuckdbFunctions => Some(function_fields()),
        TableFunction::DuckdbSettings => Some(setting_fields()),
        TableFunction::DuckdbDatabases => Some(database_fields()),
        TableFunction::DuckdbSchemas => Some(schema_fields()),
        TableFunction::DuckdbTables => Some(table_fields()),
        TableFunction::DuckdbViews => Some(view_fields()),
        TableFunction::DuckdbColumns => Some(column_fields()),
        TableFunction::DuckdbExtensions => Some(extension_fields()),
        TableFunction::DuckdbOptimizers => Some(optimizer_fields()),
        TableFunction::DuckdbDialects => Some(dialect_fields()),
        TableFunction::DuckdbGrammarExtensions => Some(grammar_extension_fields()),
        TableFunction::PragmaVersion => Some(version_fields()),
        TableFunction::PragmaPlatform => Some(platform_fields()),
        TableFunction::PragmaUserAgent => Some(user_agent_fields()),
        TableFunction::PragmaDatabaseSize => Some(database_size_fields()),
        TableFunction::PragmaShowTables => Some(show_table_fields()),
        TableFunction::PragmaShowDatabases => Some(show_database_fields()),
        TableFunction::PragmaShowTablesExpanded => Some(show_expanded_fields()),
        TableFunction::Range
        | TableFunction::GenerateSeries
        | TableFunction::ReadParquet
        | TableFunction::ReadCsv
        | TableFunction::RudbDeviceCard
        | TableFunction::PragmaTableInfo
        | TableFunction::PragmaShow
        | TableFunction::PragmaStorageInfo => None,
    }
}

/// `rudb_device_card(path)` and `rudb_device_card(path, iterations)`.
///
/// The path is a directory and the card is about the device under it. The second argument is how
/// many timed iterations each sync probe runs, and giving it at all means measuring again rather
/// than reading the card this process already has for that device, which is what somebody passing
/// `2000` to get the spec's precision wants. A null path is left a null so the executor can say
/// what is wrong with it in its own words, the same as the file readers do.
fn device_card(arguments: &[LogicalType]) -> Result<ResolvedTable> {
    let function = TableFunction::RudbDeviceCard;
    let path = matches!(arguments.first(), Some(LogicalType::Varchar | LogicalType::Null));
    let count = arguments.get(1).is_none_or(LogicalType::is_integer);
    if !path || !count || arguments.len() > 2 {
        return Err(Error::binder(format!(
            "No function matches the given name and argument types '{}({})'. You might need to \
             add explicit type casts.\n\tCandidate functions:\n\t{0}(VARCHAR)\n\t{0}(VARCHAR, \
             BIGINT)\n",
            function.name(),
            arguments.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        )));
    }
    let mut wanted = vec![arguments[0].clone()];
    if arguments.len() == 2 {
        wanted.push(LogicalType::BigInt);
    }
    Ok(ResolvedTable { function, arguments: wanted, columns: Columns::Fixed(device_card_fields()) })
}

/// The columns `rudb_device_card()` produces, one row per sync call the platform has.
///
/// The first row is the call `commit_sync = full` uses, and the columns after `plausible` are about
/// the device rather than the call, so they repeat on every row. One row per call rather than one
/// row with a column per call, because which calls there are depends on the platform and a column
/// that is null on every Linux machine is a column nobody can write a query against. The latencies
/// are microseconds as doubles, because that is the unit the spec states them in and a p50 of 24 µs
/// and one of 3,347 µs should both read as what they are.
#[must_use]
pub fn device_card_fields() -> Vec<Field> {
    vec![
        Field::new("path", LogicalType::Varchar),
        Field::new("device", LogicalType::Varchar),
        Field::new("filesystem", LogicalType::Varchar),
        Field::new("sync_call", LogicalType::Varchar),
        Field::new("chosen", LogicalType::Boolean),
        Field::new("p50_4k_us", LogicalType::Double),
        Field::new("p99_4k_us", LogicalType::Double),
        Field::new("p50_64k_us", LogicalType::Double),
        Field::new("p99_64k_us", LogicalType::Double),
        Field::new("plausible", LogicalType::Boolean),
        Field::new("write_mib_s", LogicalType::Double),
        Field::new("syncs_1", LogicalType::BigInt),
        Field::new("syncs_2", LogicalType::BigInt),
        Field::new("syncs_4", LogicalType::BigInt),
        Field::new("syncs_8", LogicalType::BigInt),
        Field::new("scaling", LogicalType::Double),
        Field::new("plp", LogicalType::Varchar),
        Field::new("memory_backed", LogicalType::Boolean),
        Field::new("lanes", LogicalType::Integer),
        Field::new("iterations", LogicalType::Integer),
    ]
}

/// The columns one of the two name taking pragmas produces.
fn name_columns(function: TableFunction) -> Vec<Field> {
    match function {
        TableFunction::PragmaShow => describe_fields(),
        TableFunction::PragmaStorageInfo => storage_info_fields(),
        _ => table_info_fields(),
    }
}

/// The columns `pragma_table_info()` produces, which is SQLite's six.
///
/// DuckDB answers to this because SQLite did, and the six are SQLite's names, its order and its
/// types right down to `cid` being a 32 bit integer where everything else in these tables is a
/// bigint. The one departure from SQLite is that `notnull` and `pk` are booleans rather than the
/// zero or one SQLite prints, which was measured rather than assumed.
///
/// `dflt_value` and `pk` are null and false on everything rudb can declare, because `DEFAULT`,
/// `PRIMARY KEY` and `UNIQUE` are all refused by `CREATE TABLE` today. They are here rather than
/// left out because the width of a result is part of the result. `DESCRIBE` says the same three
/// nothings in its own three columns and for the same reason.
#[must_use]
pub fn table_info_fields() -> Vec<Field> {
    vec![
        Field::new("cid", LogicalType::Integer),
        Field::new("name", LogicalType::Varchar),
        Field::new("type", LogicalType::Varchar),
        Field::new("notnull", LogicalType::Boolean),
        Field::new("dflt_value", LogicalType::Varchar),
        Field::new("pk", LogicalType::Boolean),
    ]
}

/// The columns `DESCRIBE` answers with, which is what `pragma_show()` produces too.
///
/// One list rather than two because the two really are the same six columns: `pragma_show('t')` and
/// `DESCRIBE t` return the same rows on the pin, which is what you would expect of a pragma that
/// exists so a client can write the describe as a function call and select from it.
#[must_use]
pub fn describe_fields() -> Vec<Field> {
    ["column_name", "column_type", "null", "key", "default", "extra"]
        .iter()
        .map(|name| Field::new(*name, LogicalType::Varchar))
        .collect()
}

/// The columns `pragma_storage_info()` produces, which is DuckDB's sixteen.
///
/// The names, the order and the types are the pin's, measured against 1.5.5 rather than read off
/// the documentation, down to `additional_block_ids` being a list of bigints on a table that has
/// nothing to put in it.
///
/// What each one means here is the interesting part, because the words are DuckDB's and the
/// storage is ours. A row group is a stripe and a segment is a part, which is the same split under
/// both names: the unit a file is written in and the unit a scan reads. `compression` is what the
/// encoder chose for that part, spelled the way `rudb-encoding` spells a cascade, so it reads
/// `DICT(PACKED, PACKED)` rather than one of DuckDB's single words. `block_id` is where in the file
/// the column page holding the part starts, because a page is what a read actually moves, and
/// `block_offset` is where the part sits inside it. `segment_info` carries the stored size of the
/// part, which is the one number in the row nothing else says.
///
/// `has_updates` is false and `persistent` is true on everything, and both will mean something the
/// day a native table has a delta region to report. They are here rather than left out because the
/// width of a result is part of the result.
#[must_use]
pub fn storage_info_fields() -> Vec<Field> {
    vec![
        Field::new("row_group_id", LogicalType::BigInt),
        Field::new("column_name", LogicalType::Varchar),
        Field::new("column_id", LogicalType::BigInt),
        Field::new("column_path", LogicalType::Varchar),
        Field::new("segment_id", LogicalType::BigInt),
        Field::new("segment_type", LogicalType::Varchar),
        Field::new("start", LogicalType::BigInt),
        Field::new("count", LogicalType::BigInt),
        Field::new("compression", LogicalType::Varchar),
        Field::new("stats", LogicalType::Varchar),
        Field::new("has_updates", LogicalType::Boolean),
        Field::new("persistent", LogicalType::Boolean),
        Field::new("block_id", LogicalType::BigInt),
        Field::new("block_offset", LogicalType::BigInt),
        Field::new("segment_info", LogicalType::Varchar),
        Field::new("additional_block_ids", LogicalType::list(LogicalType::BigInt)),
    ]
}

/// The columns `pragma_version()` produces.
///
/// Three columns rather than one, because a build has three things worth asking about: which release
/// it is, which source it was made from and what that release is called. rudb answers all three
/// about itself rather than reporting a DuckDB version, for the reason `crate` level compatibility
/// does not extend to lying about which engine is running. `crates/rudb-exec/src/enginenames.rs` is
/// where the three values are decided and it argues the case there.
#[must_use]
pub fn version_fields() -> Vec<Field> {
    ["library_version", "source_id", "codename"]
        .iter()
        .map(|name| Field::new(*name, LogicalType::Varchar))
        .collect()
}

/// The column `pragma_platform()` produces, which is the name a build is published under.
#[must_use]
pub fn platform_fields() -> Vec<Field> {
    vec![Field::new("platform", LogicalType::Varchar)]
}

/// The column `pragma_user_agent()` produces, which is the line a client sends to say who it is.
#[must_use]
pub fn user_agent_fields() -> Vec<Field> {
    vec![Field::new("user_agent", LogicalType::Varchar)]
}

/// The columns `pragma_database_size()` produces, one row per attached database.
///
/// Three of the nine are a size written for a person to read rather than a number, which is DuckDB's
/// choice and not a helpful one for a client doing arithmetic, but the width and the types of a
/// result are part of the result. The four block columns are the ones that mean something only once
/// there is a file underneath, so they are the ones rudb answers zero to and says why.
#[must_use]
pub fn database_size_fields() -> Vec<Field> {
    vec![
        Field::new("database_name", LogicalType::Varchar),
        Field::new("database_size", LogicalType::Varchar),
        Field::new("block_size", LogicalType::BigInt),
        Field::new("total_blocks", LogicalType::BigInt),
        Field::new("used_blocks", LogicalType::BigInt),
        Field::new("free_blocks", LogicalType::BigInt),
        Field::new("wal_size", LogicalType::Varchar),
        Field::new("memory_usage", LogicalType::Varchar),
        Field::new("memory_limit", LogicalType::Varchar),
    ]
}

/// The columns `rudb_strategies()` produces.
///
/// Named here rather than in the executor because the binder resolves the call and the executor
/// fills it, and a table whose two halves disagree about its own columns is a bug that shows up as
/// a wrong answer rather than as a compile error.
///
/// Nine columns and every one of them earns its place at a seam that has no implementations yet,
/// which is twenty six of the twenty seven today. `seam`, `milestone` and `seam_description` say
/// what the seam is and which milestone owes it its first two implementations, and they are filled
/// whether or not anything is registered. The other six describe an implementation and are null
/// when there is none, which is how the table says that a seam is planned rather than built without
/// anybody having to read a design document to find out.
#[must_use]
pub fn strategy_fields() -> Vec<Field> {
    vec![
        Field::new("seam", LogicalType::Varchar),
        Field::new("milestone", LogicalType::Varchar),
        Field::new("seam_description", LogicalType::Varchar),
        Field::new("implementation", LogicalType::Varchar),
        Field::new("implementation_description", LogicalType::Varchar),
        Field::new("provenance", LogicalType::Varchar),
        Field::new("determinism", LogicalType::Varchar),
        Field::new("is_reference", LogicalType::Boolean),
        Field::new("is_default", LogicalType::Boolean),
    ]
}

/// The columns `rudb_links()` produces.
///
/// Section 2.6 of spec/graph/02-the-data-model.md asks this table for what was declared, what was
/// verified, and what is physically there, and the three are separate columns because they are
/// separate claims. `cardinality` is what the build observed and not what a declaration asserted:
/// section 2.3 says a declared relationship whose parent side turns out not to be unique is
/// reported `unverified` and gets no structure, so a reader who sees `unverified` here is being told
/// why the join they expected to be fast is not.
///
/// `key_map_bytes` is filled whether or not the map was kept, which is the whole point of section
/// 3.7's budget record: a relationship that did not fit is a number rather than a silence, so
/// raising `graph_budget` is a decision somebody can make from what this says.
///
/// The link columns are what milestone G2 fills. They are here and null rather than absent for the
/// reason `rudb_strategies()` lists a seam with no implementations: a structure that is planned and
/// not built is a commitment, and a table that showed only what exists would make the layer look
/// finished.
///
/// The five degree columns are section 7.2 and 7.4 of spec/stats/07-graph-statistics.md, and they
/// are five rather than a histogram because a histogram in a cell is something nobody reads. They
/// are the numbers a reader acts on: the mean says how far a traversal expands, the maximum and the
/// ninety ninth percentile together say whether that expansion is even, and `gather_locality` says
/// whether following the link touches cache or memory. `degree_p99` is a bucket's upper bound
/// rather than an exact percentile, which is what a log bucketed histogram holds.
///
/// `parent_unique` and `child_total` are section 7.3's two certificates, which are what license
/// join elimination, outer to inner and semi join removal. They are null rather than false when
/// nothing was measured, because an unproven certificate and a disproven one lead a planner to the
/// same place by different roads and only one of them is a fact about the data.
#[must_use]
pub fn link_fields() -> Vec<Field> {
    vec![
        Field::new("name", LogicalType::Varchar),
        Field::new("child_table", LogicalType::Varchar),
        Field::new("child_key", LogicalType::Varchar),
        Field::new("parent_table", LogicalType::Varchar),
        Field::new("parent_key", LogicalType::Varchar),
        Field::new("cardinality", LogicalType::Varchar),
        Field::new("key_map", LogicalType::Varchar),
        Field::new("key_map_bytes", LogicalType::BigInt),
        Field::new("link", LogicalType::Varchar),
        Field::new("link_bytes", LogicalType::BigInt),
        Field::new("degree_mean", LogicalType::Double),
        Field::new("degree_max", LogicalType::BigInt),
        Field::new("degree_p99", LogicalType::BigInt),
        Field::new("gather_locality", LogicalType::Double),
        Field::new("parent_unique", LogicalType::Boolean),
        Field::new("child_total", LogicalType::Boolean),
        Field::new("note", LogicalType::Varchar),
    ]
}

/// The columns `duckdb_keywords()` produces, which is DuckDB's two.
#[must_use]
pub fn keyword_fields() -> Vec<Field> {
    vec![
        Field::new("keyword_name", LogicalType::Varchar),
        Field::new("keyword_category", LogicalType::Varchar),
    ]
}

/// The columns `duckdb_extensions()` produces, which is DuckDB's ten in its order.
///
/// `aliases` is the one list column in any of these tables. It is the other names an extension
/// answers to, so `httpfs` carries `[http, https, s3]` and most of them carry an empty list, and an
/// empty list is not a null: the pin returns `[]` on every row that has no alias.
#[must_use]
pub fn extension_fields() -> Vec<Field> {
    vec![
        Field::new("extension_name", LogicalType::Varchar),
        Field::new("loaded", LogicalType::Boolean),
        Field::new("installed", LogicalType::Boolean),
        Field::new("install_path", LogicalType::Varchar),
        Field::new("description", LogicalType::Varchar),
        Field::new("aliases", LogicalType::list(LogicalType::Varchar)),
        Field::new("extension_version", LogicalType::Varchar),
        Field::new("install_mode", LogicalType::Varchar),
        Field::new("installed_from", LogicalType::Varchar),
        Field::new("signature_key_fingerprint", LogicalType::Varchar),
    ]
}

/// The columns `duckdb_optimizers()` produces, which is DuckDB's one.
#[must_use]
pub fn optimizer_fields() -> Vec<Field> {
    vec![Field::new("name", LogicalType::Varchar)]
}

/// The column `duckdb_dialects()` produces.
#[must_use]
pub fn dialect_fields() -> Vec<Field> {
    vec![Field::new("dialect_name", LogicalType::Varchar)]
}

/// The columns `duckdb_grammar_extensions()` produces.
#[must_use]
pub fn grammar_extension_fields() -> Vec<Field> {
    vec![Field::new("name", LogicalType::Varchar), Field::new("description", LogicalType::Varchar)]
}

/// The four categories DuckDB sorts a keyword into.
///
/// The vendored grammar does not carry these. It carries five keyword rules, `reserved_keyword`,
/// `unreserved_keyword`, `column_name_keyword`, `func_name_keyword` and `type_name_keyword`, and
/// `rudb_parse::KEYWORDS` is a mask over those five because they are not disjoint. DuckDB's table
/// reports PostgreSQL's four categories instead, where `type_function` is the one category that the
/// grammar spells as two rules, because a word usable as a type name is usable as a function name.
///
/// So a word can produce two rows, and six of them do: `columns`, `generated`, `map`, `struct`,
/// `try_cast` and `tuple` are each in the column name class and in the type function class. That is
/// why the pinned binary returns 505 rows over 499 distinct words, and a table that deduplicated
/// them would be 499 rows and wrong.
///
/// A word whose mask is zero is in no class at all. The grammar spells fifteen words directly in
/// some rule, `ascending` and `variant` among them, which makes them matchable as literals and
/// keywords nowhere, and the pinned binary leaves all fifteen out of this table.
#[must_use]
pub fn keyword_categories(classes: u8) -> Vec<&'static str> {
    use rudb_parse::{COLUMN_NAME, FUNC_NAME, RESERVED, TYPE_NAME, UNRESERVED};
    let mut out = Vec::new();
    if classes & RESERVED != 0 {
        out.push("reserved");
    }
    if classes & UNRESERVED != 0 {
        out.push("unreserved");
    }
    if classes & COLUMN_NAME != 0 {
        out.push("column_name");
    }
    if classes & (FUNC_NAME | TYPE_NAME) != 0 {
        out.push("type_function");
    }
    out
}

/// DuckDB's message for a call that matched a name and no overload of it.
///
/// The candidate list it prints carries fifteen named parameters that none of them accept here, so
/// what is listed is the two overloads that exist. The first line is the one a test in the wild
/// asserts on and it is reproduced exactly.
fn no_overload(function: TableFunction, arguments: &[LogicalType]) -> Error {
    let written: Vec<String> = arguments.iter().map(ToString::to_string).collect();
    let name = function.name();
    Error::binder(format!(
        "No function matches the given name and argument types '{name}({})'. You might need to \
         add explicit type casts.\n\tCandidate functions:\n\t{name}(VARCHAR)\n\t{name}(VARCHAR[])\n",
        written.join(", ")
    ))
}

/// The same message for a pragma, which has one overload and prints its own name quoted.
///
/// The quoting is upstream's and is not a mistake being copied for its own sake. A pragma is
/// registered under a name the parser also spells as a statement, so the binary writes the
/// candidate through its identifier rule and gets `"pragma_table_info"(VARCHAR)` where
/// `read_parquet` gets no quotes. A client that matches on the line has to see the quotes.
fn one_name(function: TableFunction, arguments: &[LogicalType]) -> Error {
    let written: Vec<String> = arguments.iter().map(ToString::to_string).collect();
    let name = function.name();
    Error::binder(format!(
        "No function matches the given name and argument types '{name}({})'. You might need to \
         add explicit type casts.\n\tCandidate functions:\n\t\"{name}\"(VARCHAR)\n",
        written.join(", ")
    ))
}

/// The same message again for a table function whose one overload takes nothing at all.
///
/// Every metadata table is one of these and upstream quotes all of their names, not only the ones
/// the parser also spells as a statement, so `"duckdb_extensions"()` reads the same way
/// `"pragma_version"()` does. Saying how many arguments were given instead would be a shorter
/// sentence and a worse one, because a client that reads the candidate line to find out what it may
/// call learns nothing from a count.
fn nothing_at_all(function: TableFunction, arguments: &[LogicalType]) -> Error {
    let written: Vec<String> = arguments.iter().map(ToString::to_string).collect();
    let name = function.name();
    Error::binder(format!(
        "No function matches the given name and argument types '{name}({})'. You might need to \
         add explicit type casts.\n\tCandidate functions:\n\t\"{name}\"()\n",
        written.join(", ")
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
    fn the_four_categories_come_out_of_the_grammars_five_rules() {
        use rudb_parse::{COLUMN_NAME, FUNC_NAME, RESERVED, TYPE_NAME, UNRESERVED};
        assert_eq!(keyword_categories(RESERVED), ["reserved"]);
        assert_eq!(keyword_categories(UNRESERVED), ["unreserved"]);
        assert_eq!(keyword_categories(COLUMN_NAME), ["column_name"]);
        // The two rules that are one category. A word usable as a type name is usable as a function
        // name, which is why the grammar has two rules where PostgreSQL has one category, and either
        // rule on its own is still that one category rather than half of it.
        assert_eq!(keyword_categories(FUNC_NAME | TYPE_NAME), ["type_function"]);
        assert_eq!(keyword_categories(TYPE_NAME), ["type_function"]);
        assert_eq!(keyword_categories(FUNC_NAME), ["type_function"]);
        // Both, which is the case that makes one word two rows.
        assert_eq!(keyword_categories(COLUMN_NAME | FUNC_NAME), ["column_name", "type_function"]);
        // A word the grammar spells directly in a rule is in no class, and the pinned binary leaves
        // all fifteen of those out of the table rather than giving them a category of their own.
        assert!(keyword_categories(0).is_empty());
    }

    #[test]
    fn a_metadata_table_given_an_argument_says_it_takes_none() {
        for name in [
            "rudb_strategies",
            "duckdb_keywords",
            "duckdb_types",
            "duckdb_functions",
            "duckdb_settings",
            "duckdb_databases",
            "duckdb_schemas",
            "duckdb_tables",
            "duckdb_columns",
        ] {
            let function = TableFunction::lookup(name).expect("a known function");
            let error = resolve_table(name, &[LogicalType::BigInt]).expect_err("takes none");
            assert!(error.to_string().contains(&format!("\"{}\"()", function.name())), "{error}");
            let resolved = resolve_table(name, &[]).expect("takes none, and none were given");
            assert_eq!(resolved.function, function);
            assert!(matches!(resolved.columns, Columns::Fixed(_)));
        }
    }

    #[test]
    fn duckdb_keywords_has_duckdbs_two_columns_under_that_name() {
        let resolved = resolve_table("DuckDB_Keywords", &[]).expect("a case insensitive name");
        assert_eq!(resolved.function, TableFunction::DuckdbKeywords);
        let Columns::Fixed(fields) = resolved.columns else { panic!("fixed columns") };
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["keyword_name", "keyword_category"]);
        assert!(fields.iter().all(|field| field.ty == LogicalType::Varchar));
    }

    #[test]
    fn duckdb_types_has_duckdbs_seventeen_columns_under_that_name() {
        let resolved = resolve_table("DuckDB_Types", &[]).expect("a case insensitive name");
        assert_eq!(resolved.function, TableFunction::DuckdbTypes);
        let Columns::Fixed(fields) = resolved.columns else { panic!("fixed columns") };
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names.len(), 17);
        assert_eq!(names[0], "database_name");
        assert_eq!(names[16], "varargs");
        // The one column that is not a varchar, a bigint or a boolean, and the reason this table
        // waited on the map vector.
        let tags = fields.iter().find(|field| field.name == "tags").expect("a tags column");
        assert_eq!(tags.ty, LogicalType::map(LogicalType::Varchar, LogicalType::Varchar));
    }

    #[test]
    fn duckdb_settings_has_duckdbs_seven_columns_under_that_name() {
        let resolved = resolve_table("DuckDB_Settings", &[]).expect("a case insensitive name");
        assert_eq!(resolved.function, TableFunction::DuckdbSettings);
        let Columns::Fixed(fields) = resolved.columns else { panic!("fixed columns") };
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        assert_eq!(
            names,
            ["name", "value", "description", "input_type", "scope", "aliases", "typed_value"]
        );
        // The last one is a VARIANT in the pin and rudb has no such type, so it is text here.
        assert_eq!(fields[6].ty, LogicalType::Varchar);
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

    #[test]
    fn rudb_strategies_takes_no_arguments_and_produces_a_fixed_table() {
        let resolved = resolve_table("rudb_strategies", &[]).unwrap();
        assert_eq!(resolved.function, TableFunction::RudbStrategies);
        assert!(resolved.arguments.is_empty());
        assert_eq!(fixed(&resolved), strategy_fields());
    }

    #[test]
    fn rudb_strategies_with_an_argument_says_it_takes_none() {
        let error = resolve_table("rudb_strategies", &[LogicalType::BigInt]).unwrap_err();
        assert!(error.to_string().contains("\"rudb_strategies\"()"), "{error}");
        assert!(error.to_string().contains("'rudb_strategies(BIGINT)'"), "{error}");
    }

    #[test]
    fn the_two_pragmas_take_a_name_and_nothing_else_does() {
        assert!(TableFunction::PragmaTableInfo.takes_a_name());
        assert!(TableFunction::PragmaShow.takes_a_name());
        for other in [TableFunction::Range, TableFunction::DuckdbTables, TableFunction::ReadParquet]
        {
            assert!(!other.takes_a_name(), "{}", other.name());
        }
    }

    #[test]
    fn pragma_table_info_answers_in_sqlites_six_columns() {
        let resolved = resolve_table("PRAGMA_Table_Info", &[LogicalType::Varchar])
            .expect("a case insensitive name");
        assert_eq!(resolved.function, TableFunction::PragmaTableInfo);
        assert_eq!(resolved.arguments, vec![LogicalType::Varchar]);
        let names: Vec<&str> = fixed(&resolved).iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["cid", "name", "type", "notnull", "dflt_value", "pk"]);
    }

    #[test]
    fn pragma_show_answers_in_the_six_columns_describe_answers_in() {
        let resolved =
            resolve_table("pragma_show", &[LogicalType::Varchar]).expect("one name, one overload");
        assert_eq!(resolved.function, TableFunction::PragmaShow);
        let names: Vec<&str> = fixed(&resolved).iter().map(|field| field.name.as_str()).collect();
        assert_eq!(names, ["column_name", "column_type", "null", "key", "default", "extra"]);
        assert!(fixed(&resolved).iter().all(|field| field.ty == LogicalType::Varchar));
    }

    #[test]
    fn a_null_name_resolves_because_the_catalog_is_what_turns_it_down() {
        let resolved = resolve_table("pragma_table_info", &[LogicalType::Null]).expect("a null");
        assert_eq!(resolved.arguments, vec![LogicalType::Null]);
    }

    #[test]
    fn a_pragma_given_the_wrong_arguments_lists_its_one_overload() {
        for count in [0, 2] {
            let error = resolve_table("pragma_table_info", &integers(count)).expect_err("one name");
            assert!(
                error.message().starts_with(
                    "No function matches the given name and argument types 'pragma_table_info("
                ),
                "{error}"
            );
            assert!(error.message().contains("\"pragma_table_info\"(VARCHAR)"), "{error}");
        }
        // A single argument of the wrong type is the same message, because the pin does not cast
        // an integer to a name any more than it casts one to a path.
        let error = resolve_table("pragma_show", &[LogicalType::Integer]).expect_err("a name");
        assert!(error.message().contains("'pragma_show(INTEGER)'"), "{error}");
    }

    /// The same call written as a statement gets the same complaint spelled the way it was written.
    #[test]
    fn a_pragma_written_as_a_statement_is_complained_about_as_one() {
        let error = resolve_pragma("pragma_table_info", &integers(2)).expect_err("one name");
        assert!(
            error.message().starts_with(
                "No function matches the given name and argument types 'table_info(BIGINT, \
                 BIGINT)'"
            ),
            "{error}"
        );
        assert!(error.message().contains("\tPRAGMA \"table_info\"(VARCHAR)\n"), "{error}");
        // A pragma that takes nothing prints no parentheses on the candidate line at all, which is
        // the pin's spelling and is not the same as the empty pair the function form prints.
        let error = resolve_pragma("pragma_version", &integers(1)).expect_err("nothing");
        assert!(error.message().contains("'version(BIGINT)'"), "{error}");
        assert!(error.message().ends_with("\tPRAGMA \"version\"\n"), "{error}");
    }

    /// A call that resolves comes back the same either way, because it is the same function.
    #[test]
    fn a_pragma_that_resolves_resolves_to_what_the_function_spelling_does() {
        let name = [LogicalType::Varchar];
        let written = resolve_pragma("pragma_table_info", &name).expect("one name");
        let called = resolve_table("pragma_table_info", &name).expect("one name");
        assert_eq!(written.function, called.function);
        assert_eq!(written.arguments, called.arguments);
        let written = resolve_pragma("pragma_version", &[]).expect("nothing");
        assert_eq!(written.function, TableFunction::PragmaVersion);
    }

    #[test]
    fn the_four_pragmas_about_the_build_take_nothing_and_name_their_own_columns() {
        let wanted: [(&str, TableFunction, &[&str]); 4] = [
            (
                "PRAGMA_Version",
                TableFunction::PragmaVersion,
                &["library_version", "source_id", "codename"],
            ),
            ("pragma_platform", TableFunction::PragmaPlatform, &["platform"]),
            ("pragma_user_agent", TableFunction::PragmaUserAgent, &["user_agent"]),
            (
                "pragma_database_size",
                TableFunction::PragmaDatabaseSize,
                &[
                    "database_name",
                    "database_size",
                    "block_size",
                    "total_blocks",
                    "used_blocks",
                    "free_blocks",
                    "wal_size",
                    "memory_usage",
                    "memory_limit",
                ],
            ),
        ];
        for (name, function, columns) in wanted {
            let resolved = resolve_table(name, &[]).expect("takes none, and none were given");
            assert_eq!(resolved.function, function);
            assert!(resolved.arguments.is_empty());
            assert!(!function.takes_a_name(), "{name}");
            let written: Vec<&str> =
                fixed(&resolved).iter().map(|field| field.name.as_str()).collect();
            assert_eq!(written, columns);
            let error = resolve_table(name, &[LogicalType::Varchar]).expect_err("takes none");
            assert!(error.to_string().contains(&format!("\"{}\"()", function.name())), "{error}");
        }
    }

    #[test]
    fn the_four_block_columns_are_the_only_numbers_pragma_database_size_reports() {
        // The pin writes three of the nine as text a person reads rather than as a number, which is
        // worth a test because a client doing arithmetic on `database_size` gets a cast error on
        // both engines and that is the compatible answer rather than a bug in either.
        let fields = database_size_fields();
        let numbers: Vec<&str> = fields
            .iter()
            .filter(|field| field.ty == LogicalType::BigInt)
            .map(|field| field.name.as_str())
            .collect();
        assert_eq!(numbers, ["block_size", "total_blocks", "used_blocks", "free_blocks"]);
        assert!(fields.iter().filter(|field| field.ty == LogicalType::Varchar).count() == 5);
    }

    #[test]
    fn the_device_card_takes_a_path_and_maybe_a_count() {
        let found = resolve_table("rudb_device_card", &[LogicalType::Varchar]).unwrap();
        assert_eq!(found.function, TableFunction::RudbDeviceCard);
        assert_eq!(found.columns, Columns::Fixed(device_card_fields()));
        let counted =
            resolve_table("RUDB_DEVICE_CARD", &[LogicalType::Varchar, LogicalType::Integer])
                .unwrap();
        assert_eq!(counted.arguments, [LogicalType::Varchar, LogicalType::BigInt]);
        for wrong in [
            &[][..],
            &[LogicalType::Integer][..],
            &[LogicalType::Varchar, LogicalType::Varchar][..],
            &[LogicalType::Varchar, LogicalType::BigInt, LogicalType::BigInt][..],
        ] {
            let message = resolve_table("rudb_device_card", wrong).unwrap_err().to_string();
            assert!(message.contains("rudb_device_card(VARCHAR, BIGINT)"), "{message}");
        }
    }
}
