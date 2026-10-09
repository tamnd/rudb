//! `VACUUM` and `ANALYZE`, checked the way the session reads them.
//!
//! rudb keeps the statistics of a table up to date as rows are written, and a table has no dead
//! rows to reclaim, so neither statement has work to do on the data. What is left is what the
//! statement says about itself: the options, the tables and the columns, with the errors of the
//! pin or of PostgreSQL as [`Maintenance`] chooses. A PostgreSQL `VACUUM` does not run in a
//! transaction block, and the layer that knows the transaction refuses it there.
//!
//! The PostgreSQL checks are the ones of `ExecVacuum` in `vacuum.c`, in its order, and of
//! `validate_va_cols_list` in `analyze.c`.

use rudb_catalog::{Catalog, Entry, QualifiedName};
use rudb_common::{Error, Maintenance, Result, Session, SqlState};
use rudb_parse::Ast;
use rudb_parse::ast::{self, OptionArg, UtilityOption};

use crate::explain::{boolean, string};

/// A `VACUUM` or an `ANALYZE` that was checked.
#[derive(Debug)]
pub struct Vacuum {
    /// `VACUUM` rather than `ANALYZE`.
    pub vacuum: bool,
    /// Whether the statement is refused in a transaction block, which a PostgreSQL `VACUUM` is.
    pub outside_transaction: bool,
    /// The tables it names, none when it means every table.
    pub tables: Vec<QualifiedName>,
    /// The warnings PostgreSQL gives for a relation it skips, which is a view.
    pub warnings: Vec<String>,
}

/// The largest `PARALLEL`, which is `MAX_PARALLEL_WORKER_LIMIT`.
const MAX_PARALLEL_WORKERS: i64 = 1024;

/// The smallest and the largest `BUFFER_USAGE_LIMIT` other than 0, in kB, which are
/// `MIN_BAS_VAC_RING_SIZE_KB` and `MAX_BAS_VAC_RING_SIZE_KB`.
const RING_KB: (i64, i64) = (128, 16 * 1024 * 1024);

/// The options of a PostgreSQL `VACUUM` or `ANALYZE` that a later check reads.
#[derive(Debug, Default)]
struct Options {
    analyze: bool,
    full: bool,
    disable_page_skipping: bool,
    process_toast: bool,
    only_database_stats: bool,
    /// Any option that is not `VACUUM`, `VERBOSE`, `PROCESS_MAIN`, `PROCESS_TOAST` or
    /// `ONLY_DATABASE_STATS`, which `ONLY_DATABASE_STATS` refuses.
    other: bool,
    workers: i64,
    ring: bool,
}

/// Checks one `VACUUM` or `ANALYZE`.
pub(crate) fn vacuum(
    ast: &Ast,
    written: &ast::Vacuum,
    catalog: &Catalog,
    session: &Session,
) -> Result<Vacuum> {
    let maintenance = session.semantics().maintenance();
    let options = &ast.utility_options[written.options.range()];
    let analyze = match maintenance {
        Maintenance::Pin => pin_options(ast, options)?,
        Maintenance::Postgres => postgres_options(ast, written, options)?,
    };
    let mut tables = Vec::with_capacity(written.targets.len());
    for target in &written.targets {
        let parts: Vec<&str> = ast.name(target.name).collect();
        let name = catalog.resolve(&parts).map_err(|missing| {
            missing
                .state(SqlState::UNDEFINED_TABLE)
                .pg(format!("relation \"{}\" does not exist", parts.join(".")))
                .unplaced()
        })?;
        tables.push(name);
    }
    let mut warnings = Vec::new();
    let mut kept = Vec::with_capacity(tables.len());
    for (target, name) in written.targets.iter().zip(tables) {
        let view = catalog.entry(&name)? == Entry::View;
        if view && maintenance == Maintenance::Pin {
            return Err(Error::binder("Can only vacuum or analyze base tables"));
        }
        if view && written.vacuum {
            warnings.push(skipping(&name, "vacuum"));
        }
        if analyze && !target.columns.is_empty() {
            check_columns(ast, target.columns, catalog, &name, session)?;
        }
        if view && !written.vacuum {
            warnings.push(skipping(&name, "analyze"));
        }
        if !view {
            kept.push(name);
        }
    }
    let outside_transaction = written.vacuum && maintenance == Maintenance::Postgres;
    Ok(Vacuum { vacuum: written.vacuum, outside_transaction, tables: kept, warnings })
}

/// The options as the pin reads them, which are the four its grammar takes. Its transform turns
/// three of them down whatever order they were written in, and an option it does not know never
/// gets this far. Whether the statement analyzes is the answer.
fn pin_options(ast: &Ast, options: &[UtilityOption]) -> Result<bool> {
    let named = |name: &str| options.iter().any(|option| ast.string(option.name) == name);
    for (name, refused) in [("verbose", "Verbose"), ("freeze", "Freeze"), ("full", "Full")] {
        if named(name) {
            return Err(Error::not_implemented(format!("{refused} vacuum option")));
        }
    }
    // The pin analyzes the columns of a `VACUUM` with a column list whether or not it says
    // `ANALYZE`, so they are checked either way.
    Ok(true)
}

/// The options as `ExecVacuum` reads them, and the checks it makes on them before it looks at a
/// table. Whether the statement analyzes is the answer.
fn postgres_options(ast: &Ast, written: &ast::Vacuum, list: &[UtilityOption]) -> Result<bool> {
    let command = if written.vacuum { "VACUUM" } else { "ANALYZE" };
    let mut read = Options { analyze: !written.vacuum, process_toast: true, ..Options::default() };
    for option in list {
        let name = ast.string(option.name);
        match name {
            "verbose" => {
                boolean(ast, option)?;
            }
            "skip_locked" => read.other |= boolean(ast, option)?,
            "buffer_usage_limit" => {
                ring_size(&string(ast, option)?)?;
                read.ring = true;
            }
            _ if !written.vacuum => return Err(unrecognized(command, name, option)),
            "analyze" => read.analyze = boolean(ast, option)?,
            "freeze" => read.other |= boolean(ast, option)?,
            "full" => read.full = boolean(ast, option)?,
            "disable_page_skipping" => read.disable_page_skipping = boolean(ast, option)?,
            "index_cleanup" => {
                if !matches!(option.arg, OptionArg::None)
                    && !string(ast, option)?.eq_ignore_ascii_case("auto")
                {
                    boolean(ast, option)?;
                }
            }
            "process_main" => {
                boolean(ast, option)?;
            }
            "process_toast" => read.process_toast = boolean(ast, option)?,
            "truncate" => {
                boolean(ast, option)?;
            }
            "parallel" => {
                let OptionArg::Integer(workers) = option.arg else {
                    return Err(Error::binder(format!("{name} requires an integer value"))
                        .state(SqlState::SYNTAX_ERROR)
                        .unplaced());
                };
                if !(0..=MAX_PARALLEL_WORKERS).contains(&workers) {
                    return Err(Error::binder(format!(
                        "PARALLEL option must be between 0 and {MAX_PARALLEL_WORKERS}"
                    ))
                    .state(SqlState::SYNTAX_ERROR)
                    .with_span(option.span));
                }
                read.workers = workers;
            }
            "skip_database_stats" => read.other |= boolean(ast, option)?,
            "only_database_stats" => read.only_database_stats = boolean(ast, option)?,
            _ => return Err(unrecognized(command, name, option)),
        }
    }
    read.other |= read.analyze || read.full || read.disable_page_skipping;
    let refused = |message: &str| {
        Err(Error::binder(message).state(SqlState::FEATURE_NOT_SUPPORTED).unplaced())
    };
    if read.full && read.workers > 0 {
        return refused("VACUUM FULL cannot be performed in parallel");
    }
    if read.ring && read.full && !read.analyze {
        return refused("BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL");
    }
    if !read.analyze && written.targets.iter().any(|target| !target.columns.is_empty()) {
        return refused("ANALYZE option must be specified when a column list is provided");
    }
    if read.full && read.disable_page_skipping {
        return refused("VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL");
    }
    if read.full && !read.process_toast {
        return refused("PROCESS_TOAST required with VACUUM FULL");
    }
    if read.only_database_stats {
        if !written.targets.is_empty() {
            return refused("ONLY_DATABASE_STATS cannot be specified with a list of tables");
        }
        if read.other {
            return refused("ONLY_DATABASE_STATS cannot be specified with other VACUUM options");
        }
    }
    Ok(read.analyze)
}

/// `BUFFER_USAGE_LIMIT` as `parse_int` reads a value in kB: a whole number with an optional unit
/// of memory, and 0 or a size between the two limits.
fn ring_size(text: &str) -> Result<()> {
    let trimmed = text.trim();
    let digits = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(digits);
    let scale = match unit.trim_start() {
        "" | "kB" => Some(1.0),
        "B" => Some(1.0 / 1024.0),
        "MB" => Some(1024.0),
        "GB" => Some(1024.0 * 1024.0),
        "TB" => Some(1024.0 * 1024.0 * 1024.0),
        _ => None,
    };
    let kb = number.parse::<f64>().ok().zip(scale).map(|(number, scale)| (number * scale).round());
    let fits =
        kb.is_some_and(|kb| kb == 0.0 || (RING_KB.0 as f64..=RING_KB.1 as f64).contains(&kb));
    if fits {
        return Ok(());
    }
    let mut error = Error::binder(format!(
        "BUFFER_USAGE_LIMIT option must be 0 or between {} kB and {} kB",
        RING_KB.0, RING_KB.1
    ))
    .state(SqlState::INVALID_PARAMETER_VALUE)
    .unplaced();
    if scale.is_none() {
        error = error
            .hint("Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\".");
    }
    Err(error)
}

/// The error for an option the statement does not take, at the option.
fn unrecognized(command: &str, name: &str, option: &UtilityOption) -> Error {
    Error::binder(format!("unrecognized {command} option \"{name}\""))
        .state(SqlState::SYNTAX_ERROR)
        .with_span(option.span)
}

/// The warning for a relation the statement skips, as `vacuum_rel` and `analyze_rel` give it.
fn skipping(name: &QualifiedName, action: &str) -> String {
    format!("skipping \"{}\" --- cannot {action} non-tables or special system tables", name.table)
}

/// The columns written after a table, which have to be columns of it, each named once.
fn check_columns(
    ast: &Ast,
    columns: ast::Slice,
    catalog: &Catalog,
    name: &QualifiedName,
    session: &Session,
) -> Result<()> {
    let fields = match catalog.entry(name)? {
        Entry::Table => catalog.table(name)?.columns().to_vec(),
        Entry::View => catalog.view(name)?.columns(),
    };
    let compare = session.semantics().identifier_compare();
    let mut seen = Vec::with_capacity(fields.len());
    for column in ast.name(columns) {
        let Some(at) = fields.iter().position(|field| compare.same(&field.name, column)) else {
            return Err(Error::binder(format!("Column with name \"{column}\" does not exist"))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!("column \"{column}\" of relation \"{}\" does not exist", name.table))
                .unplaced());
        };
        if seen.contains(&at) {
            return Err(Error::binder(
                "cannot vacuum or analyze the same column twice, i.e., there is a duplicate \
                 entry in the list of column names",
            )
            .state(SqlState::DUPLICATE_COLUMN)
            .pg(format!(
                "column \"{column}\" of relation \"{}\" appears more than once",
                name.table
            ))
            .unplaced());
        }
        seen.push(at);
    }
    Ok(())
}
