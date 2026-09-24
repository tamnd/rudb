//! The handle everything else hangs off.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Instant;

use rudb_bind::{Bound, Parameters, Write};
use rudb_catalog::{Catalog, Entry, QualifiedName, View};
use rudb_common::stat::Provenance;
use rudb_common::{
    Cancel, Clustering, Error, Field, LogicalType, Memory, Result, Rule, Session, Value,
};
use rudb_io::{Filesystem, RealFilesystem};
use rudb_metrics::{Document, LoadProfile, Report, Span, Stage};
use rudb_native::graph::Edge;
use rudb_parse::ast::{self, Ast};
use rudb_pipeline::{Lease, Morsel, Pool, Progress, Sink, keep_pages};
use rudb_plan::{Expr, Node, Plan};
use rudb_vector::{Chunk, Data, Form, Selection, Vector};

use crate::config::Config;
use crate::connection::{Connection, single};
use crate::prepared::Prepared;
use crate::result::QueryResult;
use crate::settings::Settings;
use crate::{foreign, upsert};

/// The name that means no file, which is DuckDB's spelling and SQLite's before it.
const MEMORY: &str = ":memory:";

/// Exact native bounds for a one-statement result, before CSV formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeExtremaValues {
    /// Bounds for an integer column.
    Integer { low: i128, high: i128 },
    /// Day counts for a DATE column.
    Date { low: i32, high: i32 },
}

fn native_simple_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// A bare name in a simple SQL shape cannot be one of the grammar's reserved words. The native
/// catalog also contains quoted names, so merely finding the name there is not syntax validation.
fn native_simple_unquoted_identifier(text: &str) -> bool {
    native_simple_identifier(text)
        && rudb_parse::classes(rudb_parse::lookup(text)) & rudb_parse::RESERVED == 0
}

/// A deliberately small recognizer for a single unquoted AVG(column) statement. Anything with
/// another clause, expression, or quoting rule goes through the SQL parser instead.
fn native_simple_average_statement(sql: &str) -> Option<(&str, &str)> {
    let statement = sql.trim();
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    let mut words = statement.split_ascii_whitespace();
    let select = words.next()?;
    let aggregate = words.next()?;
    let from = words.next()?;
    let table = words.next()?;
    if words.next().is_some()
        || !select.eq_ignore_ascii_case("select")
        || !from.eq_ignore_ascii_case("from")
    {
        return None;
    }
    let prefix = aggregate.get(..4)?;
    if !prefix.eq_ignore_ascii_case("avg(") || !aggregate.ends_with(')') {
        return None;
    }
    let column = &aggregate[4..aggregate.len() - 1];
    (native_simple_identifier(column) && native_simple_identifier(table)).then_some((table, column))
}

/// Recognizes an unquoted count filtered by a nonzero integer column. Other SQL syntax goes
/// through the regular parser and executor.
fn native_simple_nonzero_statement(sql: &str) -> Option<(&str, &str)> {
    let statement = sql.trim();
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    let mut words = statement.split_ascii_whitespace();
    let select = words.next()?;
    let count = words.next()?;
    let from = words.next()?;
    let table = words.next()?;
    let where_keyword = words.next()?;
    let column = words.next()?;
    let comparison = words.next()?;
    let zero = words.next()?;
    (words.next().is_none()
        && select.eq_ignore_ascii_case("select")
        && count.eq_ignore_ascii_case("count(*)")
        && from.eq_ignore_ascii_case("from")
        && where_keyword.eq_ignore_ascii_case("where")
        && comparison == "<>"
        && zero == "0"
        && native_simple_identifier(table)
        && native_simple_identifier(column))
    .then_some((table, column))
}

/// Recognizes a single unquoted COUNT(DISTINCT column) without parsing a general SQL result.
fn native_simple_distinct_statement(sql: &str) -> Option<(&str, &str)> {
    let statement = sql.trim();
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    let mut words = statement.split_ascii_whitespace();
    let select = words.next()?;
    let aggregate = words.next()?;
    let column = words.next()?.strip_suffix(')')?;
    let from = words.next()?;
    let table = words.next()?;
    if words.next().is_some()
        || !select.eq_ignore_ascii_case("select")
        || !aggregate.eq_ignore_ascii_case("count(distinct")
        || !from.eq_ignore_ascii_case("from")
        || !native_simple_identifier(column)
        || !native_simple_identifier(table)
    {
        return None;
    }
    Some((table, column))
}

/// Recognizes one unquoted MIN and MAX of the same column, with no other SQL clauses.
fn native_simple_extrema_statement(sql: &str) -> Option<(&str, &str)> {
    let statement = sql.trim();
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    let mut words = statement.split_ascii_whitespace();
    let select = words.next()?;
    let minimum = words.next()?;
    let maximum = words.next()?;
    let from = words.next()?;
    let table = words.next()?;
    if words.next().is_some()
        || !select.eq_ignore_ascii_case("select")
        || !from.eq_ignore_ascii_case("from")
        || !native_simple_identifier(table)
    {
        return None;
    }
    let min_prefix = minimum.get(..4)?;
    let max_prefix = maximum.get(..4)?;
    if !min_prefix.eq_ignore_ascii_case("min(")
        || !max_prefix.eq_ignore_ascii_case("max(")
        || !minimum.ends_with("),")
        || !maximum.ends_with(')')
    {
        return None;
    }
    let column = &minimum[4..minimum.len() - 2];
    let other = &maximum[4..maximum.len() - 1];
    (native_simple_identifier(column) && column.eq_ignore_ascii_case(other))
        .then_some((table, column))
}

/// Recognizes an unquoted sum, count, and average of requested columns. The full SQL parser
/// handles forms with aliases, expressions, or clauses this small path does not understand.
fn native_simple_three_statement(sql: &str) -> Option<(&str, &str, &str)> {
    let statement = sql.trim();
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    let mut words = statement.split_ascii_whitespace();
    let select = words.next()?;
    let sum = words.next()?;
    let count = words.next()?;
    let average = words.next()?;
    let from = words.next()?;
    let table = words.next()?;
    if words.next().is_some()
        || !select.eq_ignore_ascii_case("select")
        || !sum.get(..4)?.eq_ignore_ascii_case("sum(")
        || !count.eq_ignore_ascii_case("count(*),")
        || !average.get(..4)?.eq_ignore_ascii_case("avg(")
        || !from.eq_ignore_ascii_case("from")
        || !native_simple_unquoted_identifier(table)
    {
        return None;
    }
    let sum_column = sum.strip_suffix("),")?.get(4..)?;
    let average_column = average.strip_suffix(')')?.get(4..)?;
    (native_simple_unquoted_identifier(sum_column)
        && native_simple_unquoted_identifier(average_column))
    .then_some((table, sum_column, average_column))
}

/// Recognizes a single numeric key grouped by its nonzero values. This only selects a runtime
/// column scan; the result is never read from stored frequency statistics.
fn native_simple_group_count_statement(sql: &str) -> Option<(&str, &str)> {
    let statement = sql.trim();
    let statement = statement.strip_suffix(';').unwrap_or(statement).trim_end();
    let mut words = statement.split_ascii_whitespace();
    let select = words.next()?;
    let key = words.next()?.strip_suffix(',')?;
    let count = words.next()?;
    let from = words.next()?;
    let table = words.next()?;
    let where_keyword = words.next()?;
    let filtered = words.next()?;
    let comparison = words.next()?;
    let zero = words.next()?;
    let group = words.next()?;
    let group_by = words.next()?;
    let grouped = words.next()?;
    let order = words.next()?;
    let order_by = words.next()?;
    let order_count = words.next()?;
    let descending = words.next()?;
    (words.next().is_none()
        && select.eq_ignore_ascii_case("select")
        && count.eq_ignore_ascii_case("count(*)")
        && from.eq_ignore_ascii_case("from")
        && where_keyword.eq_ignore_ascii_case("where")
        && comparison == "<>"
        && zero == "0"
        && group.eq_ignore_ascii_case("group")
        && group_by.eq_ignore_ascii_case("by")
        && order.eq_ignore_ascii_case("order")
        && order_by.eq_ignore_ascii_case("by")
        && order_count.eq_ignore_ascii_case("count(*)")
        && descending.eq_ignore_ascii_case("desc")
        && native_simple_unquoted_identifier(table)
        && native_simple_unquoted_identifier(key)
        && filtered.eq_ignore_ascii_case(key)
        && grouped.eq_ignore_ascii_case(key))
    .then_some((table, key))
}

/// Recognizes the one aggregate whose exact answer is certified by a native frequency synopsis.
/// Keep this check strict: every clause it does not understand belongs to the regular binder.
fn native_nonzero_shape(ast: &Ast) -> Option<(&str, &str, &str)> {
    use ast::{BinaryOp, Distinct, Expr, LiteralKind, QueryBody, Source, Statement};
    use rudb_parse::NONE;

    let [Statement::Query(query_ref)] = ast.statements.as_slice() else {
        return None;
    };
    let query = ast.query(*query_ref);
    if query.ctes.len != 0
        || query.order_by.len != 0
        || query.order_by_all
        || query.limit != NONE
        || query.offset != NONE
        || query.limit_percent
    {
        return None;
    }
    let QueryBody::Select(select_ref) = query.body else {
        return None;
    };
    let select = ast.select(select_ref);
    if select.distinct != Distinct::No
        || select.group_by.len != 0
        || select.group_by_all
        || select.having != NONE
    {
        return None;
    }
    let [target] = ast.target_list(select.targets) else {
        return None;
    };
    let Expr::Function { name, args, distinct: false, filter: NONE } = ast.expr(target.expr) else {
        return None;
    };
    if name.len != 1 {
        return None;
    }
    let function = ast.name(name).next()?;
    if !function.eq_ignore_ascii_case("count") {
        return None;
    }
    let [arg] = ast.expr_list(args) else {
        return None;
    };
    if !matches!(ast.expr(*arg), Expr::Star { qualifier, replacements } if qualifier.len == 0 && replacements.len == 0)
    {
        return None;
    }
    let [source] = ast.source_list(select.from) else {
        return None;
    };
    let Source::Table { name, alias: NONE, columns } = ast.source(*source) else {
        return None;
    };
    if columns.len != 0 {
        return None;
    }
    if name.len != 1 {
        return None;
    }
    let table = ast.name(name).next()?;
    let Expr::Binary { op: BinaryOp::NotEq, left, right } = ast.expr(select.filter) else {
        return None;
    };
    let (column, zero) = match (ast.expr(left), ast.expr(right)) {
        (Expr::Column { name }, Expr::Literal { kind: LiteralKind::Number, text }) => (name, text),
        (Expr::Literal { kind: LiteralKind::Number, text }, Expr::Column { name }) => (name, text),
        _ => return None,
    };
    if ast.string(zero) != "0" {
        return None;
    }
    if column.len != 1 {
        return None;
    }
    let column = ast.name(column).next()?;
    Some((
        table,
        column,
        if target.alias == NONE { "count_star()" } else { ast.string(target.alias) },
    ))
}

fn native_column_aggregate<'a>(
    ast: &'a Ast,
    expr: ast::ExprRef,
    function: &str,
) -> Option<&'a str> {
    use rudb_parse::NONE;
    let ast::Expr::Function { name, args, distinct: false, filter: NONE } = ast.expr(expr) else {
        return None;
    };
    if name.len != 1 || !ast.name(name).next()?.eq_ignore_ascii_case(function) {
        return None;
    }
    let [argument] = ast.expr_list(args) else { return None };
    let ast::Expr::Column { name } = ast.expr(*argument) else { return None };
    (name.len == 1).then(|| ast.name(name).next()).flatten()
}

fn native_three_aggregate_shape(ast: &Ast) -> Option<(&str, &str, &str, [String; 3])> {
    use ast::{Distinct, Expr, QueryBody, Source, Statement};
    use rudb_parse::NONE;

    let [Statement::Query(query_ref)] = ast.statements.as_slice() else { return None };
    let query = ast.query(*query_ref);
    if query.ctes.len != 0
        || query.order_by.len != 0
        || query.order_by_all
        || query.limit != NONE
        || query.offset != NONE
        || query.limit_percent
    {
        return None;
    }
    let QueryBody::Select(select_ref) = query.body else { return None };
    let select = ast.select(select_ref);
    if select.distinct != Distinct::No
        || select.filter != NONE
        || select.group_by.len != 0
        || select.group_by_all
        || select.having != NONE
    {
        return None;
    }
    let [sum, count, average] = ast.target_list(select.targets) else { return None };
    let sum_column = native_column_aggregate(ast, sum.expr, "sum")?;
    let average_column = native_column_aggregate(ast, average.expr, "avg")?;
    let Expr::Function { name, args, distinct: false, filter: NONE } = ast.expr(count.expr) else {
        return None;
    };
    if name.len != 1 || !ast.name(name).next()?.eq_ignore_ascii_case("count") {
        return None;
    }
    let [argument] = ast.expr_list(args) else { return None };
    if !matches!(ast.expr(*argument), Expr::Star { qualifier, replacements } if qualifier.len == 0 && replacements.len == 0)
    {
        return None;
    }
    let [source] = ast.source_list(select.from) else { return None };
    let Source::Table { name, alias: NONE, columns } = ast.source(*source) else { return None };
    if name.len != 1 || columns.len != 0 {
        return None;
    }
    let table = ast.name(name).next()?;
    let names = [
        if sum.alias == NONE { format!("sum({sum_column})") } else { ast.string(sum.alias).into() },
        if count.alias == NONE { "count_star()".into() } else { ast.string(count.alias).into() },
        if average.alias == NONE {
            format!("avg({average_column})")
        } else {
            ast.string(average.alias).into()
        },
    ];
    Some((table, sum_column, average_column, names))
}

fn native_single_average_shape(ast: &Ast) -> Option<(&str, &str, String)> {
    use ast::{Distinct, QueryBody, Source, Statement};
    use rudb_parse::NONE;

    let [Statement::Query(query_ref)] = ast.statements.as_slice() else { return None };
    let query = ast.query(*query_ref);
    if query.ctes.len != 0
        || query.order_by.len != 0
        || query.order_by_all
        || query.limit != NONE
        || query.offset != NONE
        || query.limit_percent
    {
        return None;
    }
    let QueryBody::Select(select_ref) = query.body else { return None };
    let select = ast.select(select_ref);
    if select.distinct != Distinct::No
        || select.filter != NONE
        || select.group_by.len != 0
        || select.group_by_all
        || select.having != NONE
    {
        return None;
    }
    let [target] = ast.target_list(select.targets) else { return None };
    let column = native_column_aggregate(ast, target.expr, "avg")?;
    let [source] = ast.source_list(select.from) else { return None };
    let Source::Table { name, alias: NONE, columns } = ast.source(*source) else { return None };
    if name.len != 1 || columns.len != 0 {
        return None;
    }
    let table = ast.name(name).next()?;
    let name = if target.alias == NONE {
        format!("avg({column})")
    } else {
        ast.string(target.alias).into()
    };
    Some((table, column, name))
}

fn native_single_distinct_shape(ast: &Ast) -> Option<(&str, &str, String)> {
    use ast::{Distinct, Expr, QueryBody, Source, Statement};
    use rudb_parse::NONE;

    let [Statement::Query(query_ref)] = ast.statements.as_slice() else { return None };
    let query = ast.query(*query_ref);
    if query.ctes.len != 0
        || query.order_by.len != 0
        || query.order_by_all
        || query.limit != NONE
        || query.offset != NONE
        || query.limit_percent
    {
        return None;
    }
    let QueryBody::Select(select_ref) = query.body else { return None };
    let select = ast.select(select_ref);
    if select.distinct != Distinct::No
        || select.filter != NONE
        || select.group_by.len != 0
        || select.group_by_all
        || select.having != NONE
    {
        return None;
    }
    let [target] = ast.target_list(select.targets) else { return None };
    let Expr::Function { name, args, distinct: true, filter: NONE } = ast.expr(target.expr) else {
        return None;
    };
    if name.len != 1 || !ast.name(name).next()?.eq_ignore_ascii_case("count") {
        return None;
    }
    let [argument] = ast.expr_list(args) else { return None };
    let Expr::Column { name } = ast.expr(*argument) else { return None };
    if name.len != 1 {
        return None;
    }
    let column = ast.name(name).next()?;
    let [source] = ast.source_list(select.from) else { return None };
    let Source::Table { name, alias: NONE, columns } = ast.source(*source) else { return None };
    if name.len != 1 || columns.len != 0 {
        return None;
    }
    let table = ast.name(name).next()?;
    let name = if target.alias == NONE {
        format!("count(DISTINCT {column})")
    } else {
        ast.string(target.alias).into()
    };
    Some((table, column, name))
}

fn native_extrema_shape(ast: &Ast) -> Option<(&str, &str, [String; 2])> {
    use ast::{Distinct, QueryBody, Source, Statement};
    use rudb_parse::NONE;

    let [Statement::Query(query_ref)] = ast.statements.as_slice() else { return None };
    let query = ast.query(*query_ref);
    if query.ctes.len != 0
        || query.order_by.len != 0
        || query.order_by_all
        || query.limit != NONE
        || query.offset != NONE
        || query.limit_percent
    {
        return None;
    }
    let QueryBody::Select(select_ref) = query.body else { return None };
    let select = ast.select(select_ref);
    if select.distinct != Distinct::No
        || select.filter != NONE
        || select.group_by.len != 0
        || select.group_by_all
        || select.having != NONE
    {
        return None;
    }
    let [minimum, maximum] = ast.target_list(select.targets) else { return None };
    let column = native_column_aggregate(ast, minimum.expr, "min")?;
    let other = native_column_aggregate(ast, maximum.expr, "max")?;
    if !column.eq_ignore_ascii_case(other) {
        return None;
    }
    let [source] = ast.source_list(select.from) else { return None };
    let Source::Table { name, alias: NONE, columns } = ast.source(*source) else { return None };
    if name.len != 1 || columns.len != 0 {
        return None;
    }
    let table = ast.name(name).next()?;
    let names = [
        if minimum.alias == NONE {
            format!("min({column})")
        } else {
            ast.string(minimum.alias).into()
        },
        if maximum.alias == NONE {
            format!("max({column})")
        } else {
            ast.string(maximum.alias).into()
        },
    ];
    Some((table, column, names))
}

fn native_integer_value(ty: &LogicalType, value: i128) -> Option<Value> {
    Some(match ty {
        LogicalType::TinyInt => Value::TinyInt(i8::try_from(value).ok()?),
        LogicalType::SmallInt => Value::SmallInt(i16::try_from(value).ok()?),
        LogicalType::Integer => Value::Integer(i32::try_from(value).ok()?),
        LogicalType::BigInt => Value::BigInt(i64::try_from(value).ok()?),
        LogicalType::UTinyInt => Value::UTinyInt(u8::try_from(value).ok()?),
        LogicalType::USmallInt => Value::USmallInt(u16::try_from(value).ok()?),
        LogicalType::UInteger => Value::UInteger(u32::try_from(value).ok()?),
        LogicalType::UBigInt => Value::UBigInt(u64::try_from(value).ok()?),
        LogicalType::Date => Value::Date(i32::try_from(value).ok()?),
        _ => return None,
    })
}

/// An in process database.
///
/// One catalog, held in memory, with no file behind it. `ATTACH` and the storage format are E2, and
/// the shape of this type does not change when they arrive: a database with a file behind it is a
/// catalog whose tables read from a block manager rather than from a `Vec` of chunks, which is a
/// change under [`rudb_catalog::Table`] and not a change here.
///
/// A handle rather than the thing itself. Cloning one is cheap and gives another handle on the same
/// database, and [`Database::connect`] gives a [`Connection`], which is the same sharing with a
/// name that says what it is for. The catalog is behind a lock, so every method here takes `&self`
/// and a write from one thread is serialized against a read from another rather than refused by the
/// compiler. That is what an embedded database has to do, because the program embedding it is the
/// one that decided how many threads it has.
#[derive(Debug, Clone)]
pub struct Database {
    shared: Shared,
}

/// The state one database is, however many handles are on it.
#[derive(Debug, Clone)]
pub(crate) struct Shared {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    catalog: RwLock<Catalog>,
    /// Held by whatever might write the file or change the catalog, for as long as it does, and
    /// always taken before the catalog lock.
    ///
    /// The catalog lock used to be this as well, which meant a load held it for writing from its
    /// first row to its last and no query could start on the same database until it was done. A
    /// load into a new table only has to change the catalog at the end, once its rows are in the
    /// file, so it takes this for the whole load and the catalog lock for writing only for the two
    /// ends, and reads can run while the rows go in. Every other writer takes this too, which is
    /// what stops a checkpoint or a second load from writing the file under the first one.
    writer: Mutex<()>,
    /// The transaction a `BEGIN` opened, until a `COMMIT` or a `ROLLBACK` closes it.
    open: Mutex<Option<Open>>,
    /// Told when a load has let go of the catalog, so a test can run a query in the middle of one.
    #[cfg(test)]
    loading: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    path: Option<PathBuf>,
    /// Whether this database may write its file, which is [`Config::read_only`] turned around.
    ///
    /// Read in the two places the file is written, which is `CHECKPOINT` and the write on the way
    /// out below. Kept here rather than read back off the settings because the write on the way out
    /// happens while the database is being dropped, and a setting is a `SET` away from being
    /// something else by then.
    writable: bool,
    settings: Settings,
    memory: Memory,
    pool: Pool,
    /// The pages every table of the file keeps between queries. One per database, and handed to
    /// the catalog each checkpoint opens, so a checkpoint does not start a second budget.
    pages: rudb_native::PagePool,
    /// The counts the last statement planned from, kept for the next one.
    ///
    /// What is stored carries the catalog generation it was read at, so the next statement can tell
    /// whether it is still current by comparing two numbers rather than by walking every table and
    /// every column again. A set that is out of date is replaced, and one that is not is handed on
    /// as it is, which is safe to do because nothing writes to a set after it is built.
    ///
    /// The lock is its own rather than the catalog's because this is not part of the catalog, and it
    /// is never contended: every caller is already holding the catalog lock when it gets here.
    facts: Mutex<Arc<rudb_opt::estimate::Facts>>,
    /// The relationships the last statement planned from, kept for the next one.
    ///
    /// Cached the same way and for the same reason, and on one more key: what is declared comes
    /// from a setting, so a `SET graph_links` between two statements has to be seen even when the
    /// catalog has not moved. The pair that is stored is the generation and the setting text the
    /// list was read at, and either one changing rereads it.
    relationships: Mutex<(u64, String, Arc<Vec<rudb_opt::link::Linked>>)>,
    /// The Parquet files this database tried to mirror and could not, with the options they were
    /// read under, so that a file whose load fails pays for the failure once and not on every
    /// query. See [`crate::mirror`].
    declined: Mutex<BTreeSet<(String, bool)>>,
    /// A successful setting statement invalidates the one cached native aggregate plan.
    settings_revision: AtomicU64,
    native_aggregate_plan: Mutex<Option<CachedNativeAggregate>>,
}

#[derive(Debug)]
struct CachedNativeAggregate {
    sql: String,
    catalog_generation: u64,
    settings_revision: u64,
    plan: Arc<Plan>,
}

/// The file is written when the last handle on the database goes away.
///
/// A table created in one run is in the file for the next one without anybody having to say
/// `CHECKPOINT`, which is what DuckDB does and what a program embedding a database expects of it.
/// This runs once however many handles and connections there were, because the state is behind an
/// `Arc` and this is the drop of the thing inside it.
///
/// The error is swallowed, because a `Drop` has nowhere to put one. [`Database::close`] is the same
/// write with the error handed back, for a program that wants to know. Nothing is written for an in
/// memory database, which has no file, or for a read only one, which was asked not to.
/// A transaction `BEGIN` opened and nothing has closed yet.
///
/// The catalog as it was at the `BEGIN` is kept whole, and a `ROLLBACK` puts it back. A table's
/// rows are chunks that are shared rather than copied when the catalog is cloned, so holding the
/// old one costs the rows that changed and not the database. One transaction for the database
/// rather than one per connection, which is as much as a database that runs its statements one at
/// a time can tell apart.
#[derive(Debug)]
struct Open {
    /// What the catalog was when the transaction began.
    before: Catalog,
    /// Whether a statement failed inside it, after which only `COMMIT` and `ROLLBACK` run and both
    /// of them roll back, which is what the pin does.
    aborted: bool,
    /// Whether it was begun `READ ONLY`.
    read_only: bool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let Some(path) = self.path.as_ref().filter(|_| self.writable) else {
            return;
        };
        let catalog = self.catalog.get_mut().unwrap_or_else(PoisonError::into_inner);
        // A transaction still open when the database goes away never committed, so what it changed
        // is not what the file gets.
        if let Some(open) = self.open.get_mut().unwrap_or_else(PoisonError::into_inner).take() {
            catalog.restore(open.before);
        }
        let _ = persist(path, catalog, &self.pages);
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

/// The two things a database sets up before it will run anything, neither of which is per query.
///
/// The threads are the obvious one. The other is the system allocator, which by default hands every
/// large block back to the kernel the moment a query is done with it and then faults the same pages
/// in again on the next one, and which is asked here to stop. Both of them are process wide or
/// database wide rather than query wide, both of them are cheap to set and expensive to find out
/// about later, and opening a database is the one place that knows a query is coming.
/// How many bytes of pages a database keeps between queries, which is half of its memory limit.
///
/// Half, because the other half is what a query's hash tables and sorts get, and those have to
/// spill when they run out while a page that is let go can always be read again. No limit keeps
/// every page, which is what DuckDB does too.
fn page_budget(limit: Option<u64>) -> usize {
    limit.map_or(usize::MAX, |limit| usize::try_from(limit / 2).unwrap_or(usize::MAX))
}

fn runtime(config: &Config) -> Pool {
    keep_pages();
    Pool::new(config.threads())
}

impl Database {
    /// Returns a filtered count for the narrow read-only CSV path without building a query
    /// result. The count is derived from a leading zero frequency, the row count, and
    /// null statistics when the statement runs.
    pub fn query_native_nonzero_value_once(path: &str, sql: &str) -> Result<Option<i64>> {
        let Some((table, column)) = native_simple_nonzero_statement(sql) else { return Ok(None) };
        let native = rudb_native::Catalog::open(path)?;
        let Some(table) = native.names().find(|name| name.eq_ignore_ascii_case(table)) else {
            return Ok(None);
        };
        let Some(fields) = native.table_fields(table) else { return Ok(None) };
        let Some(index) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
        else {
            return Ok(None);
        };
        if !matches!(
            fields[index].ty,
            LogicalType::TinyInt
                | LogicalType::SmallInt
                | LogicalType::Integer
                | LogicalType::BigInt
                | LogicalType::UTinyInt
                | LogicalType::USmallInt
                | LogicalType::UInteger
                | LogicalType::UBigInt
        ) {
            return Ok(None);
        }
        Ok(native.nonzero_count(table, index)?.and_then(|count| i64::try_from(count).ok()))
    }

    /// Scans one native integer column and counts its nonzero values while this SQL runs.
    /// The file's range statistic selects a small dense counter when possible; no frequency
    /// statistic supplies a group or a count.
    pub fn query_native_group_count_once(
        path: &str,
        sql: &str,
    ) -> Result<Option<Vec<(i128, i64)>>> {
        let Some((table, column)) = native_simple_group_count_statement(sql) else {
            return Ok(None);
        };
        let catalog = rudb_native::Catalog::open(path)?;
        let Some(name) = catalog.names().find(|name| name.eq_ignore_ascii_case(table)) else {
            return Ok(None);
        };
        let Some(fields) = catalog.table_fields(name) else { return Ok(None) };
        let Some(index) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
        else {
            return Ok(None);
        };
        if !matches!(
            fields[index].ty,
            LogicalType::TinyInt
                | LogicalType::SmallInt
                | LogicalType::Integer
                | LogicalType::BigInt
                | LogicalType::UTinyInt
                | LogicalType::USmallInt
                | LogicalType::UInteger
                | LogicalType::UBigInt
        ) {
            return Ok(None);
        }
        // A one-column counter is useful for a small domain. Leave high-cardinality grouping to
        // the general executor instead of growing a tree for millions of distinct keys here.
        let distinct = catalog.distinct_count(name, index)?;
        if distinct.is_some_and(|count| count > 4096) {
            return Ok(None);
        }
        let mut dense = match catalog.integer_extremes(name, index)? {
            Some(rudb_native::IntegerExtremes::Values { low, high })
                if (0..=4096).contains(&(high - low)) =>
            {
                Some((low, vec![0_u64; usize::try_from(high - low + 1).unwrap_or(0)]))
            }
            _ => None,
        };
        if distinct.is_none() && dense.is_none() {
            return Ok(None);
        }
        let mut sparse = BTreeMap::<i128, u64>::new();
        let folded = catalog.integer_fold(name, index, |value, count| {
            if value == 0 {
                return Ok(());
            }
            let value = i128::from(value);
            if let Some((low, dense_counts)) = &mut dense {
                let at = usize::try_from(value - *low).unwrap_or(usize::MAX);
                if let Some(held) = dense_counts.get_mut(at) {
                    *held += count;
                    return Ok(());
                }
            }
            *sparse.entry(value).or_default() += count;
            Ok(())
        })?;
        if folded.is_none() {
            let reader = catalog.table(name)?;
            for part in 0..reader.parts() {
                if let Some(counts) = reader.integer_tally(part, index)? {
                    for (value, count) in counts {
                        if value == 0 {
                            continue;
                        }
                        let value = i128::from(value);
                        if let Some((low, dense_counts)) = &mut dense {
                            let at = usize::try_from(value - *low).unwrap_or(usize::MAX);
                            if let Some(held) = dense_counts.get_mut(at) {
                                *held += count;
                                continue;
                            }
                        }
                        *sparse.entry(value).or_default() += count;
                    }
                    continue;
                }
                let chunk = reader.read(part, &[index])?;
                let column = chunk
                    .into_columns()
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::internal("native column scan returned no column"))?
                    .into_flat()?;
                let validity = column.validity();
                macro_rules! count_values {
                    ($values:expr) => {
                        if let Some((low, counts)) = &mut dense {
                            if column.none_null() {
                                for &value in $values.as_slice() {
                                    let value = i128::from(value);
                                    if value != 0 {
                                        let at =
                                            usize::try_from(value - *low).unwrap_or(usize::MAX);
                                        if let Some(held) = counts.get_mut(at) {
                                            *held += 1;
                                        } else {
                                            *sparse.entry(value).or_default() += 1;
                                        }
                                    }
                                }
                            } else {
                                for (row, &value) in $values.as_slice().iter().enumerate() {
                                    let value = i128::from(value);
                                    if value != 0 && validity.is_valid(row) {
                                        let at =
                                            usize::try_from(value - *low).unwrap_or(usize::MAX);
                                        if let Some(held) = counts.get_mut(at) {
                                            *held += 1;
                                        } else {
                                            *sparse.entry(value).or_default() += 1;
                                        }
                                    }
                                }
                            }
                        } else {
                            for (row, &value) in $values.as_slice().iter().enumerate() {
                                let value = i128::from(value);
                                if value != 0 && validity.is_valid(row) {
                                    *sparse.entry(value).or_default() += 1;
                                }
                            }
                        }
                    };
                }
                match column.data() {
                    Some(Data::Int8(values)) => count_values!(values),
                    Some(Data::Int16(values)) => count_values!(values),
                    Some(Data::Int32(values)) => count_values!(values),
                    Some(Data::Int64(values)) => count_values!(values),
                    Some(Data::UInt8(values)) => count_values!(values),
                    Some(Data::UInt16(values)) => count_values!(values),
                    Some(Data::UInt32(values)) => count_values!(values),
                    Some(Data::UInt64(values)) => count_values!(values),
                    _ => return Ok(None),
                }
            }
        }
        if let Some((low, counts)) = dense {
            for (at, count) in counts.into_iter().enumerate() {
                if count != 0 {
                    sparse.insert(low + at as i128, count);
                }
            }
        }
        let mut groups = sparse
            .into_iter()
            .map(|(value, count)| i64::try_from(count).map(|count| (value, count)))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| Error::internal("grouped count exceeds BIGINT"))?;
        groups.sort_unstable_by(|(left_value, left_count), (right_value, right_count)| {
            right_count.cmp(left_count).then_with(|| left_value.cmp(right_value))
        });
        Ok(Some(groups))
    }

    /// Returns a sum, row count and average from certified native column sums.
    /// The shell can format these on its single-statement, read-only path.
    pub fn query_native_three_values_once(
        path: &str,
        sql: &str,
    ) -> Result<Option<(i128, i64, f64)>> {
        let Some((table_name, sum_column, avg_column)) = native_simple_three_statement(sql) else {
            return Ok(None);
        };
        let catalog = rudb_native::Catalog::open(path)?;
        let Some(name) = catalog.names().find(|name| name.eq_ignore_ascii_case(table_name)) else {
            return Ok(None);
        };
        let Some(fields) = catalog.table_fields(name) else { return Ok(None) };
        let Some(sum_column) =
            fields.iter().position(|field| field.name.eq_ignore_ascii_case(sum_column))
        else {
            return Ok(None);
        };
        let Some(avg_column) =
            fields.iter().position(|field| field.name.eq_ignore_ascii_case(avg_column))
        else {
            return Ok(None);
        };
        let Some(sums) = catalog.aggregate_sums(name, &[sum_column, avg_column])? else {
            return Ok(None);
        };
        let Ok(rows) = i64::try_from(sums.rows) else { return Ok(None) };
        let (sum, sum_count) = sums.columns[0];
        let (avg_sum, avg_count) = sums.columns[1];
        if sum_count == 0 || avg_count == 0 {
            return Ok(None);
        }
        let average = avg_sum as f64 / avg_count as f64;
        Ok(Some((sum, rows, average)))
    }

    /// Reads a certified integer average for a simple one-statement CSV invocation without
    /// allocating a parsed query or result vectors. Null and unsupported cases use the normal path.
    pub fn query_native_average_value_once(path: &str, sql: &str) -> Result<Option<f64>> {
        let Some((table, column)) = native_simple_average_statement(sql) else {
            return Ok(None);
        };
        let catalog = rudb_native::Catalog::open(path)?;
        let Some(name) = catalog.names().find(|name| name.eq_ignore_ascii_case(table)) else {
            return Ok(None);
        };
        let Some(fields) = catalog.table_fields(name) else { return Ok(None) };
        let Some(index) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
        else {
            return Ok(None);
        };
        let Some(sums) = catalog.aggregate_sums(name, &[index])? else {
            return Ok(None);
        };
        let (sum, count) = sums.columns[0];
        if count == 0 {
            return Ok(None);
        }
        Ok(Some(sum as f64 / count as f64))
    }

    /// Reads a certified distinct count for a simple read-only CSV invocation without building
    /// a parsed query or result vectors. Missing certificates use regular execution.
    pub fn query_native_distinct_value_once(path: &str, sql: &str) -> Result<Option<i64>> {
        let Some((table, column)) = native_simple_distinct_statement(sql) else {
            return Ok(None);
        };
        let catalog = rudb_native::Catalog::open(path)?;
        let Some(name) = catalog.names().find(|name| name.eq_ignore_ascii_case(table)) else {
            return Ok(None);
        };
        let Some(fields) = catalog.table_fields(name) else { return Ok(None) };
        let Some(index) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
        else {
            return Ok(None);
        };
        let Some(count) = catalog.distinct_count(name, index)? else {
            return Ok(None);
        };
        Ok(i64::try_from(count).ok())
    }

    /// Reads certified bounds for a simple one-statement CSV invocation without constructing a
    /// parsed query or result vectors. Null and unsupported bounds use regular execution.
    pub fn query_native_extrema_values_once(
        path: &str,
        sql: &str,
    ) -> Result<Option<NativeExtremaValues>> {
        let Some((table, column)) = native_simple_extrema_statement(sql) else {
            return Ok(None);
        };
        let catalog = rudb_native::Catalog::open(path)?;
        let Some(name) = catalog.names().find(|name| name.eq_ignore_ascii_case(table)) else {
            return Ok(None);
        };
        let Some(fields) = catalog.table_fields(name) else { return Ok(None) };
        let Some(index) = fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
        else {
            return Ok(None);
        };
        let Some(rudb_native::IntegerExtremes::Values { low, high }) =
            catalog.integer_extremes(name, index)?
        else {
            return Ok(None);
        };
        let ty = &fields[index].ty;
        if native_integer_value(ty, low).is_none() || native_integer_value(ty, high).is_none() {
            return Ok(None);
        }
        Ok(Some(match ty {
            LogicalType::Date => {
                let (Ok(low), Ok(high)) = (i32::try_from(low), i32::try_from(high)) else {
                    return Ok(None);
                };
                NativeExtremaValues::Date { low, high }
            }
            _ => NativeExtremaValues::Integer { low, high },
        }))
    }

    /// Answers supported read-only aggregates directly from certified native synopses.
    /// Other statements return `None` so the caller can use a regular database connection.
    pub fn query_native_once(path: &str, sql: &str) -> Result<Option<QueryResult>> {
        let ast = rudb_parse::parse_ast(sql)?;
        let nonzero = native_nonzero_shape(&ast);
        let three = native_three_aggregate_shape(&ast);
        let average = native_single_average_shape(&ast);
        let distinct = native_single_distinct_shape(&ast);
        let extrema = native_extrema_shape(&ast);
        let Some(table) = nonzero
            .map(|(table, _, _)| table)
            .or_else(|| three.as_ref().map(|(table, _, _, _)| *table))
            .or_else(|| average.as_ref().map(|(table, _, _)| *table))
            .or_else(|| distinct.as_ref().map(|(table, _, _)| *table))
            .or_else(|| extrema.as_ref().map(|(table, _, _)| *table))
        else {
            return Ok(None);
        };
        let native = rudb_native::Catalog::open(path)?;
        let Some((stored_name, fields)) = native
            .names()
            .find(|stored| stored.eq_ignore_ascii_case(table))
            .and_then(|stored| native.table_fields(stored).map(|fields| (stored, fields)))
        else {
            return Ok(None);
        };
        if let Some((_, column, name)) = nonzero {
            let Some(index) =
                fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
            else {
                return Ok(None);
            };
            let Some(count) = native.nonzero_count(stored_name, index)? else {
                return Ok(None);
            };
            let Ok(count) = i64::try_from(count) else {
                return Ok(None);
            };
            let vector = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(count)])?;
            let chunk = Chunk::new(vec![vector])?;
            return Ok(Some(QueryResult::new(
                vec![name.to_string()],
                vec![LogicalType::BigInt],
                vec![chunk],
                Memory::unlimited().reservation(),
            )));
        }
        if let Some((_, column, name)) = average {
            let Some(index) =
                fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
            else {
                return Ok(None);
            };
            let Some(sums) = native.aggregate_sums(stored_name, &[index])? else {
                return Ok(None);
            };
            let (sum, count) = sums.columns[0];
            let value =
                if count == 0 { Value::Null } else { Value::Double(sum as f64 / count as f64) };
            let vector = Vector::from_values(LogicalType::Double, &[value])?;
            let chunk = Chunk::new(vec![vector])?;
            return Ok(Some(QueryResult::new(
                vec![name],
                vec![LogicalType::Double],
                vec![chunk],
                Memory::unlimited().reservation(),
            )));
        }
        if let Some((_, column, name)) = distinct {
            let Some(index) =
                fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
            else {
                return Ok(None);
            };
            let Some(count) = native.distinct_count(stored_name, index)? else {
                return Ok(None);
            };
            let Ok(count) = i64::try_from(count) else {
                return Ok(None);
            };
            let vector = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(count)])?;
            let chunk = Chunk::new(vec![vector])?;
            return Ok(Some(QueryResult::new(
                vec![name],
                vec![LogicalType::BigInt],
                vec![chunk],
                Memory::unlimited().reservation(),
            )));
        }
        if let Some((_, column, names)) = extrema {
            let Some(index) =
                fields.iter().position(|field| field.name.eq_ignore_ascii_case(column))
            else {
                return Ok(None);
            };
            let Some(ends) = native.integer_extremes(stored_name, index)? else {
                return Ok(None);
            };
            let ty = fields[index].ty.clone();
            let (low, high) = match ends {
                rudb_native::IntegerExtremes::Null => (Value::Null, Value::Null),
                rudb_native::IntegerExtremes::Values { low, high } => {
                    let Some(low) = native_integer_value(&ty, low) else { return Ok(None) };
                    let Some(high) = native_integer_value(&ty, high) else { return Ok(None) };
                    (low, high)
                }
            };
            let chunk = Chunk::new(vec![
                Vector::from_values(ty.clone(), &[low])?,
                Vector::from_values(ty.clone(), &[high])?,
            ])?;
            return Ok(Some(QueryResult::new(
                names.into(),
                vec![ty.clone(), ty],
                vec![chunk],
                Memory::unlimited().reservation(),
            )));
        }
        let Some((_, sum_column, avg_column, names)) = three else {
            return Ok(None);
        };
        let Some(sum_index) =
            fields.iter().position(|field| field.name.eq_ignore_ascii_case(sum_column))
        else {
            return Ok(None);
        };
        let Some(avg_index) =
            fields.iter().position(|field| field.name.eq_ignore_ascii_case(avg_column))
        else {
            return Ok(None);
        };
        let Some(sums) = native.aggregate_sums(stored_name, &[sum_index, avg_index])? else {
            return Ok(None);
        };
        let Ok(rows) = i64::try_from(sums.rows) else {
            return Ok(None);
        };
        let (sum, sum_count) = sums.columns[0];
        let (avg_sum, avg_count) = sums.columns[1];
        let types = vec![LogicalType::HugeInt, LogicalType::BigInt, LogicalType::Double];
        let values = [
            if sum_count == 0 { Value::Null } else { Value::HugeInt(sum) },
            Value::BigInt(rows),
            if avg_count == 0 {
                Value::Null
            } else {
                Value::Double(avg_sum as f64 / avg_count as f64)
            },
        ];
        let vectors = types
            .iter()
            .cloned()
            .zip(values)
            .map(|(ty, value)| Vector::from_values(ty, &[value]))
            .collect::<Result<Vec<_>>>()?;
        let chunk = Chunk::new(vectors)?;
        Ok(Some(QueryResult::new(
            names.into(),
            types,
            vec![chunk],
            Memory::unlimited().reservation(),
        )))
    }
    /// An empty database with the default catalog and schema, held in memory.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    /// An empty database held in memory, opened with these settings.
    #[must_use]
    pub fn with_config(config: Config) -> Self {
        let memory = Memory::new(config.memory_limit());
        let pool = runtime(&config);
        let writable = !config.read_only();
        let settings = Settings::new(config);
        let inner = Inner {
            catalog: RwLock::new(Catalog::new()),
            writer: Mutex::default(),
            open: Mutex::default(),
            #[cfg(test)]
            loading: Mutex::default(),
            path: None,
            writable,
            settings,
            memory,
            pool,
            pages: rudb_native::PagePool::default(),
            facts: Mutex::default(),
            relationships: Mutex::default(),
            declined: Mutex::default(),
            settings_revision: AtomicU64::new(0),
            native_aggregate_plan: Mutex::default(),
        };
        Self { shared: Shared { inner: Arc::new(inner) } }
    }

    /// What this database is running with now.
    ///
    /// By value rather than by reference, because `SET` changes it while the database is open and a
    /// reference into the settings would be a lock held for as long as the caller kept it. A
    /// `Config` is three numbers, so a copy costs nothing worth avoiding.
    #[must_use]
    pub fn config(&self) -> Config {
        self.shared.inner.settings.config()
    }

    /// What this database was opened with, which is what `RESET` puts a setting back to.
    #[must_use]
    pub fn opened_with(&self) -> Config {
        self.shared.inner.settings.defaults()
    }

    /// One setting, by the name `SET` uses for it, in the spelling DuckDB prints.
    ///
    /// The Rust side of reading a setting back. `SELECT current_setting('threads')` is the SQL side
    /// and it answers with the same text, typed as whatever the setting holds.
    ///
    /// # Errors
    ///
    /// For a name that is not a setting, with the names there are.
    pub fn setting(&self, name: &str) -> Result<String> {
        // The row order declarations are read off the catalog rather than out of the settings,
        // because that is where they live. Handled here rather than a layer down for the plain
        // reason that a `Settings` cannot see a catalog and this can.
        if crate::settings::is_clustering(name) {
            return Ok(self.shared.read().clustering());
        }
        self.shared.inner.settings.value(name)
    }

    /// Which implementation runs at each seam, as this session has left it.
    ///
    /// The session half of the three surfaces. The other two reach the same place: a process flag
    /// is a `SET` the shell runs before anything else, and a per query hint is this with the
    /// query's own pins laid on top, which is [`Database::seams_for`].
    #[must_use]
    pub fn seams(&self) -> rudb_seam::Settings {
        self.shared.inner.settings.seams()
    }

    /// The seam settings one query runs under, which is [`Database::seams`] plus its hints.
    ///
    /// `SELECT /*+ hash.table(unchained) */ ...` pins a seam for one statement and leaves the
    /// session alone, which is what a researcher comparing two implementations of one thing over a
    /// suite needs, because the alternative is a `SET` before every query and a `RESET` after it
    /// that somebody eventually forgets.
    ///
    /// # Errors
    ///
    /// A parse error, and everything a hint naming a seam nobody has raises.
    pub fn seams_for(&self, sql: &str) -> Result<rudb_seam::Settings> {
        self.shared.seams(sql)
    }

    /// The memory budget every query against this database is held to.
    ///
    /// One budget for the database rather than one per query, which is what
    /// [`Config::memory_limit`] means: two queries running at once share the limit rather than
    /// getting one each. Public because [`rudb_common::Memory::used`] is the only way to see what
    /// is being held, and a program that sets a limit wants to know how close it is.
    #[must_use]
    pub fn memory(&self) -> &Memory {
        &self.shared.inner.memory
    }

    /// Opens a database by name.
    ///
    /// `:memory:` and the empty string are an in memory database, which are DuckDB's two spellings
    /// of it. Anything else names a file in rudb's own native format, holding every table of the
    /// database. A name that does not exist yet is a database with nothing in it, and the file is
    /// written by `CHECKPOINT` and again when the last handle on the database goes away.
    ///
    /// # Errors
    ///
    /// When the file exists and is not a native database this build can read.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_with(path, Config::default())
    }

    /// Opens a database by name, with these settings.
    ///
    /// # Errors
    ///
    /// When the file exists and is not a native database this build can read.
    pub fn open_with(path: &str, config: Config) -> Result<Self> {
        if path.is_empty() || path == MEMORY {
            return Ok(Self::with_config(config));
        }
        let path = PathBuf::from(path);
        let mut catalog = Catalog::new();
        let pages = rudb_native::PagePool::new(page_budget(config.memory_limit()));
        if path.exists() {
            // The catalog directory names the tables and the loop below decodes each one's own
            // directory. That is one decode per table rather than one decode of everything, but it
            // still happens at open, because the catalog this builds holds a reader per table and a
            // reader is built from a decoded directory. Deferring the decode to the first query
            // that touches a table is what the two levels are for and is not done here yet.
            let native = rudb_native::Catalog::open_in(&path, &pages)?;
            let names = native.names().map(str::to_string).collect::<Vec<_>>();
            for name in names {
                catalog.create_native_table(native.table(&name)?)?;
            }
            // After the tables, because a view is allowed to stand over one and the name check in
            // the catalog is over both. Nothing binds here, so the order does not matter to the
            // body, but it matters to the sentence a clash produces.
            let views = native.views().cloned().collect::<Vec<_>>();
            for view in &views {
                catalog.create_native_view(view)?;
            }
        }
        let memory = Memory::new(config.memory_limit());
        let pool = runtime(&config);
        let writable = !config.read_only();
        let settings = Settings::new(config);
        let inner = Inner {
            catalog: RwLock::new(catalog),
            writer: Mutex::default(),
            open: Mutex::default(),
            #[cfg(test)]
            loading: Mutex::default(),
            path: Some(path),
            writable,
            settings,
            memory,
            pool,
            pages,
            facts: Mutex::default(),
            relationships: Mutex::default(),
            declined: Mutex::default(),
            settings_revision: AtomicU64::new(0),
            native_aggregate_plan: Mutex::default(),
        };
        Ok(Self { shared: Shared { inner: Arc::new(inner) } })
    }

    /// A connection to this database.
    #[must_use]
    pub fn connect(&self) -> Connection {
        Connection::new(self.shared.clone())
    }

    /// Writes the file and hands back what went wrong, which dropping the database cannot do.
    ///
    /// The same write the last handle does on its way out, said out loud. A program that wants to
    /// know whether its last session reached the disk calls this, and one that does not gets the
    /// write anyway and never hears about a failure, which is the best a `Drop` can manage.
    ///
    /// Safe to call and then drop, because a checkpoint over a file that already holds exactly what
    /// the catalog does returns without writing anything. Safe to call while other handles are
    /// open too: it writes what the catalog says now, and the handle that goes out last writes
    /// whatever has changed since.
    ///
    /// # Errors
    ///
    /// Everything `CHECKPOINT` raises. An in memory database and a read only one both have nothing
    /// to write and are always `Ok`.
    pub fn close(self) -> Result<()> {
        let Some(path) = self.shared.inner.path.as_ref().filter(|_| self.shared.inner.writable)
        else {
            return Ok(());
        };
        let path = path.clone();
        let _writing = self.shared.writing();
        persist(&path, &mut self.shared.write(), &self.shared.inner.pages)
    }

    /// Parses a statement so it can be run more than once, with values for its parameters.
    ///
    /// The same call as [`Connection::prepare`].
    ///
    /// # Errors
    ///
    /// A parse error. A name that does not resolve or a type that does not work out is an error at
    /// execution rather than here, because a parameter has no type until it has a value.
    pub fn prepare(&self, sql: &str) -> Result<Prepared> {
        Prepared::new(self.shared.clone(), sql).map_err(|error| self.shared.process_error(error))
    }

    /// Reads the catalog.
    ///
    /// A closure rather than a returned reference, because the catalog is behind a lock and a
    /// reference out of it would outlive the guard. The lock is held for the call and no longer.
    pub fn with_catalog<T>(&self, read: impl FnOnce(&Catalog) -> T) -> T {
        read(&self.shared.read())
    }

    /// Writes the catalog.
    ///
    /// Public because a program that builds its own catalog rather than parsing SQL to build one is
    /// a real thing an embedded database gets used for.
    pub fn with_catalog_mut<T>(&self, write: impl FnOnce(&mut Catalog) -> T) -> T {
        let _writing = self.shared.writing();
        write(&mut self.shared.write())
    }

    /// Defines a table.
    ///
    /// The name is `table`, `schema.table` or `catalog.schema.table`, and anything unqualified goes
    /// to the default catalog and schema, which is what an unqualified name in a query resolves
    /// against too.
    ///
    /// # Errors
    ///
    /// If the name has more than three parts, if the catalog or the schema does not exist, if the
    /// table already exists, or if two of the columns have the same name.
    pub fn create_table(&self, name: &str, columns: Vec<Field>) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let _writing = self.shared.writing();
        let mut catalog = self.shared.write();
        let resolved = catalog.resolve_for_create(&parts)?;
        catalog.create_table(resolved, columns)
    }

    /// Drops a table.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn drop_table(&self, name: &str) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let _writing = self.shared.writing();
        let mut catalog = self.shared.write();
        let resolved = catalog.resolve(&parts)?;
        catalog.drop_table(&resolved)
    }

    /// Appends rows to a table, each row left to right in the table's column order.
    ///
    /// The row shaped write path, because the caller with rows in hand is the common case and the
    /// caller with columns in hand can reach [`Database::with_catalog_mut`] and append a
    /// [`rudb_vector::Chunk`] directly. Values are converted to the column's type on the way in, so
    /// an `Integer` lands in a `BIGINT` column.
    ///
    /// # Errors
    ///
    /// If the name does not resolve, if a row is not as wide as the table, or if a value cannot be
    /// converted to its column's type.
    pub fn append(&self, name: &str, rows: &[Vec<Value>]) -> Result<()> {
        let parts: Vec<&str> = name.split('.').collect();
        let _writing = self.shared.writing();
        let mut catalog = self.shared.write();
        let resolved = catalog.resolve(&parts)?;
        catalog.table_mut(&resolved)?.append_rows(rows)
    }

    /// How many rows a table holds.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn table_len(&self, name: &str) -> Result<usize> {
        let parts: Vec<&str> = name.split('.').collect();
        let catalog = self.shared.read();
        let resolved = catalog.resolve(&parts)?;
        Ok(catalog.table(&resolved)?.rows().len())
    }

    /// Every table in the database, unqualified, in creation order.
    ///
    /// Unqualified because that is what a person typing `.tables` wants to read and what they would
    /// then type into a query. Two tables of the same name in different schemas both appear, which
    /// is the same thing DuckDB's `.tables` does.
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        self.shared.read().tables().map(|table| table.name().table.clone()).collect()
    }

    /// The `CREATE TABLE` that would define a table as it stands.
    ///
    /// Built from the catalog rather than remembered from the statement that made it, so a table
    /// defined by [`Database::create_table`] describes itself as well as one defined by SQL. It
    /// carries the column names, the types and `NOT NULL`, and nothing else, because nothing else
    /// is in the catalog yet. Defaults, primary keys and check constraints appear here the day the
    /// catalog holds them.
    ///
    /// # Errors
    ///
    /// If the name does not resolve or the table does not exist.
    pub fn table_sql(&self, name: &str) -> Result<String> {
        let parts: Vec<&str> = name.split('.').collect();
        let catalog = self.shared.read();
        let resolved = catalog.resolve(&parts)?;
        let table = catalog.table(&resolved)?;
        let columns: Vec<String> = table
            .columns()
            .iter()
            .map(|field| {
                let null = if field.not_null { " NOT NULL" } else { "" };
                format!("{} {}{null}", field.name, field.ty)
            })
            .collect();
        Ok(format!("CREATE TABLE {}({});", resolved.table, columns.join(", ")))
    }

    /// Runs one query and returns every row it produced.
    ///
    /// The same call as [`Connection::query`], for a program that has one database and no reason to
    /// name a connection.
    ///
    /// The query timeout in [`Database::config`] applies, and nothing can interrupt it, because an
    /// interrupt needs somebody holding the other end of a token and a bare database hands out no
    /// token. [`Connection::interrupt`] is that other end.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, or anything the operators raise while running, which is
    /// mostly cast failures and arithmetic that leaves the range of its type.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.shared
            .query(sql, &self.shared.token())
            .map_err(|error| self.shared.process_error(error))
    }

    /// Runs one statement, which may change the database.
    ///
    /// # Errors
    ///
    /// A parse error, a binder error, a catalog error, or anything the operators raise.
    pub fn execute(&self, sql: &str) -> Result<QueryResult> {
        self.shared
            .execute(sql, &self.shared.token())
            .map_err(|error| self.shared.process_error(error))
    }

    /// The plan for a query, in the textual form `spec/07-execution.md` describes, without running
    /// it.
    ///
    /// The same plan `EXPLAIN` prints, without the estimates and as a `String` rather than a result
    /// set, which is what the plan tests and the optimizer work read. The text round trips:
    /// `rudb_plan::Plan::parse` of this string gives back the plan it was printed from, and that is
    /// why the estimates are not on it.
    ///
    /// # Errors
    ///
    /// A parse error or a binder error.
    pub fn plan(&self, sql: &str) -> Result<String> {
        self.shared.plan(sql).map_err(|error| self.shared.process_error(error))
    }

    /// Runs a query and returns the single value it produced.
    ///
    /// A convenience for `SELECT count(*) FROM t` and the rest of the one cell queries, which are
    /// most of what a program embedded in something else asks.
    ///
    /// # Errors
    ///
    /// Everything [`Database::query`] can raise, plus an error if the result is not one row of one
    /// column.
    pub fn value(&self, sql: &str) -> Result<Value> {
        single(&self.query(sql)?)
    }
}

/// Writes the whole catalog as a complete native snapshot and publishes it by rename.
///
/// Every table goes into one file under one generation, so the rename that publishes it publishes
/// all of them at once and a reader never sees half a checkpoint.
///
/// A table already backed by the file is carried forward by its directory pointer rather than read
/// back out and written again, which is what the two header slots are for. The tables that changed
/// go on the end of the file in a new generation, and the slot write that publishes them is what
/// makes the whole of it atomic. A checkpoint over a catalog where nothing changed still does
/// nothing.
///
/// The rewrite below it is still there for the cases the append cannot answer. Carrying a table
/// forward means carrying forward what the committed generation says, so a generation that names a
/// table the catalog no longer has, or does not name one the catalog says is native, is one where
/// the file and the catalog disagree about which tables exist, and the honest answer is to write
/// all of them again.
///
/// The tables are rebound afterwards. Without that the catalog would go on reading the generation
/// before this one, which still answers correctly because its bytes are unchanged, but which would
/// be kept alive by every checkpoint for as long as the database is open.
fn persist(path: &Path, catalog: &mut Catalog, pages: &rudb_native::PagePool) -> Result<()> {
    let names = catalog.stored_tables().map(|table| table.name().clone()).collect::<Vec<_>>();
    let views = views(catalog);
    // Nothing to write is every table already in the file and the file holding no other. The second
    // half is what a drop leaves behind: every table that is left is still native, and without
    // asking the file which tables it names the checkpoint would decide there was nothing to do and
    // the dropped one would still be there on the next open.
    // Every table already in the file, and every declaration in the file already the one the
    // catalog holds. The second half is what stops a `CLUSTER BY` on a table that was checkpointed
    // before the declaration existed from being decided as nothing to do and quietly lost.
    let clean = catalog
        .stored_tables()
        .all(|table| table.rows().is_native() && table.clustering_is_stored());
    let held = committed(path)?;
    // The views are compared by what they are rather than by their whole record, because the column
    // list on a record is a cache the binder writes over every time somebody selects from the view.
    // Comparing that too would make a plain `SELECT` from a view leave the file looking out of date,
    // and the next checkpoint would write the whole database again to store a list that answers the
    // same questions it already answered.
    if clean
        && held
            .as_ref()
            .is_some_and(|held| held.tables == wanted(&names) && same_views(&held.views, &views))
    {
        return Ok(());
    }
    // A database with no table in it is still a database, and the file has to say so or the tables
    // that were dropped out of it are all still there the next time it is opened. There is nothing
    // to append in that case and nothing to carry forward either, so it goes straight to the
    // rewrite below it, written as an empty file and renamed over whatever was there.
    if names.is_empty() {
        let temporary = scratch(path)?;
        rudb_native::Writer::empty(&temporary, &views)?;
        return rename(&temporary, path);
    }
    // Only the views moved, so nothing has to be written again. Everything the file holds is still
    // the right bytes in the right place and the commit is a new catalog naming the same pages.
    if clean && held.is_some_and(|held| held.tables == wanted(&names)) {
        rudb_native::Writer::restate(path, &views)?;
        return rebind(path, catalog, &names, pages);
    }
    if appended(path, catalog, &names, &views)? {
        return rebind(path, catalog, &names, pages);
    }
    let temporary = scratch(path)?;
    let mut writer: Option<rudb_native::Writer> = None;
    for name in &names {
        let table = catalog.table(name)?;
        let fields = table.columns().to_vec();
        let columns = (0..fields.len()).collect::<Vec<_>>();
        let mut open = match writer.take() {
            None => rudb_native::Writer::create(&temporary, name.table.clone(), fields)?,
            Some(writer) => writer.next(name.table.clone(), fields)?,
        };
        if let Some(clustering) = table.clustering() {
            open = open.declare(clustering.clone())?;
        }
        for at in 0..table.rows().chunk_count() {
            open.append(&table.rows().read(at, &columns)?)?;
        }
        writer = Some(open);
    }
    let writer = writer.ok_or_else(|| Error::internal("a catalog with tables wrote none"))?;
    writer.with_views(views).finish()?;
    rename(&temporary, path)?;
    rebind(path, catalog, &names, pages)
}

/// A path beside the database for the file being built, with anything left there removed first.
///
/// Named after the process, so two processes checkpointing the same database do not write over one
/// another's half finished file. What is left there by a process that died is this process's to
/// remove, because the name says it was this process's, and a build that refused would be a build
/// that never checkpoints again after one crash.
fn scratch(path: &Path) -> Result<PathBuf> {
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    if temporary.exists() {
        std::fs::remove_file(&temporary).map_err(|error| Error::io(error.to_string()))?;
    }
    Ok(temporary)
}

/// Publishes the file that was built beside the database, which is what makes a rewrite atomic.
fn rename(temporary: &Path, path: &Path) -> Result<()> {
    publish(&RealFilesystem::new(), temporary, path)
}

/// Renames a finished file over the one it replaces, then syncs the directory the name is in.
///
/// The rename is atomic, which says that a reader sees the old file or the new one and never half
/// of each. It says nothing about the new name having reached the disk. That is the directory's
/// own write and it is only durable once the directory is synced, so without the second call a
/// power loss after the rename can bring the machine back with the directory still naming the old
/// file. The data and the slot were synced before the rename, so the new file is intact on disk
/// with nothing pointing at it, and a load that was acknowledged is gone.
pub(crate) fn publish(fs: &dyn Filesystem, temporary: &Path, path: &Path) -> Result<()> {
    fs.rename(temporary, path)?;
    fs.sync_dir(directory_of(path))
}

/// The directory a path's name lives in. A bare file name has an empty parent rather than none,
/// and the directory it means is the current one.
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// What the committed file holds, or `None` for a path nothing has been written to yet.
///
/// A path that is there but is not a native file this build can read is an error rather than a
/// `None`, because the caller's other answer is to write the whole file, and writing over something
/// that might be somebody else's is worse than refusing.
fn committed(path: &Path) -> Result<Option<Held>> {
    if !path.exists() {
        return Ok(None);
    }
    let held = rudb_native::Catalog::open(path)?;
    Ok(Some(Held {
        tables: held.names().map(str::to_string).collect(),
        views: held.views().cloned().collect(),
    }))
}

/// What the committed file says it holds, without reading a table directory for any of it.
struct Held {
    /// The names of its tables, which is all a checkpoint has to know about them.
    tables: BTreeSet<String>,
    /// Its views, whole, since a view is entirely in the catalog level.
    views: Vec<rudb_native::ViewEntry>,
}

/// The tables with the row count the committed catalog records for each of them.
///
/// Only one caller needs the counts and it needs them for one question, which is whether a table
/// already in the file would be in the way of a load streaming into it. See [`appendable`].
fn held_rows(path: &Path) -> Result<Option<BTreeMap<String, usize>>> {
    if !path.exists() {
        return Ok(None);
    }
    let held = rudb_native::Catalog::open(path)?;
    Ok(Some(held.rows().map(|(name, rows)| (name.to_string(), rows)).collect()))
}

/// The same set of names, taken from the catalog instead.
fn wanted(names: &[QualifiedName]) -> BTreeSet<String> {
    names.iter().map(|name| name.table.clone()).collect()
}

/// The views a file written from this catalog now would hold.
///
/// The column list goes in as it stands, cache and all. See
/// `rudb_catalog::Catalog::create_native_view` for why a file carries a cache at all.
fn views(catalog: &Catalog) -> Vec<rudb_native::ViewEntry> {
    catalog
        .stored_views()
        .map(|view| rudb_native::ViewEntry {
            name: view.name().table.clone(),
            sql: view.sql().to_string(),
            statement: view.statement().to_string(),
            aliases: view.aliases().to_vec(),
            columns: view.columns(),
        })
        .collect()
}

/// Whether two lists of views are the same views, ignoring the column cache on each.
fn same_views(held: &[rudb_native::ViewEntry], wanted: &[rudb_native::ViewEntry]) -> bool {
    held.len() == wanted.len()
        && held.iter().zip(wanted).all(|(held, wanted)| {
            held.name == wanted.name
                && held.sql == wanted.sql
                && held.statement == wanted.statement
                && held.aliases == wanted.aliases
        })
}

/// Builds a key map over the parent column of every declared relationship.
///
/// A second pass, after the tables are in the file and not before, because a key map is derived
/// from a column and the column has to be readable to be derived from. Section 3.8 of
/// spec/graph/03-the-file-format.md is where that is a rule rather than a convenience: a load that
/// inserts parent and child in one statement cannot know the parent's row ids while it is still
/// writing them.
///
/// Nothing here refuses anything. Section 3.1 says deleting every graph section changes no answer,
/// which cuts both ways: a relationship naming a table that is not here, or a column no form can
/// map, is passed over rather than made into an error, because the query it was declared for will
/// run either way and only the time is different. What did and did not get built is `rudb_links()`.
fn index(
    path: &Path,
    catalog: &mut Catalog,
    links: &str,
    pages: &rudb_native::PagePool,
) -> Result<()> {
    let declared = rudb_graph::parse_links(links).unwrap_or_default();
    if declared.is_empty() {
        return Ok(());
    }
    // A list and not a map, because a qualified name does not order and the count here is the
    // count of declared relationships. One entry per parent table, because section 3.7's budget is
    // a table's budget and a table whose maps are built one column at a time would spend it twice.
    let mut wanted: Vec<(QualifiedName, Vec<usize>)> = Vec::new();
    for link in &declared {
        let [column] = &link.parent.columns[..] else { continue };
        let Some(table) = catalog
            .tables()
            .find(|table| table.name().table.eq_ignore_ascii_case(&link.parent.table))
        else {
            continue;
        };
        let Some(at) = table.column_index(column) else { continue };
        let name = table.name().clone();
        match wanted.iter_mut().find(|(held, _)| held == &name) {
            Some((_, columns)) if columns.contains(&at) => {}
            Some((_, columns)) => columns.push(at),
            None => wanted.push((name, vec![at])),
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }
    for (name, columns) in &wanted {
        rudb_native::graph::build_key_maps(path, &name.table, columns)?;
    }
    // The second pass section 3.8 asks for, and it is a second pass over the file and not only over
    // the declarations: a link is built by looking a child's keys up in the parent's key map, and
    // the loop above is what put that map in the file. Doing both in one walk would mean building
    // a link against a map that is still in memory, which works until the relationship's two tables
    // arrive in the other order.
    let edges = edges_of(catalog, &declared);
    let mut names = wanted.into_iter().map(|(name, _)| name).collect::<Vec<_>>();
    if !edges.is_empty() {
        rudb_native::graph::build_links(path, &edges)?;
        for edge in &edges {
            let Some(name) = catalog
                .tables()
                .find(|table| table.name().table == edge.child)
                .map(|table| table.name().clone())
            else {
                continue;
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    rebind(path, catalog, &names, pages)
}

/// The declared relationships whose four names all resolve, as the link builder wants them.
///
/// A declaration that names a table or a column that is not there is dropped rather than reported.
/// `rudb_links()` is where a user finds out, because it reads the declaration and the file side by
/// side and can say which half is missing; a checkpoint can only refuse to index, and by section
/// 3.1 refusing to index is not an error.
fn edges_of(catalog: &Catalog, declared: &[rudb_graph::Relationship]) -> Vec<Edge> {
    let mut edges = Vec::new();
    for link in declared {
        let ([child], [parent]) = (&link.child.columns[..], &link.parent.columns[..]) else {
            continue;
        };
        let find = |name: &str| {
            catalog.tables().find(|table| table.name().table.eq_ignore_ascii_case(name))
        };
        let (Some(child_table), Some(parent_table)) =
            (find(&link.child.table), find(&link.parent.table))
        else {
            continue;
        };
        let (Some(child_column), Some(parent_column)) =
            (child_table.column_index(child), parent_table.column_index(parent))
        else {
            continue;
        };
        edges.push(Edge {
            child: child_table.name().table.clone(),
            child_column,
            parent: parent_table.name().table.clone(),
            parent_column,
        });
    }
    edges
}

/// Points every table at the generation the file now holds.
fn rebind(
    path: &Path,
    catalog: &mut Catalog,
    names: &[QualifiedName],
    pages: &rudb_native::PagePool,
) -> Result<()> {
    let native = rudb_native::Catalog::open_in(path, pages)?;
    for name in names {
        let reader = native.table(&name.table)?;
        catalog.table_mut(name)?.rebind_native(reader)?;
    }
    Ok(())
}

/// Writes the tables that changed into a new generation over the committed file, if it can.
///
/// It can when the file exists and names exactly the tables the catalog says are already native.
/// Then the ones it names are carried forward untouched and the rest are appended, and the cost of
/// the checkpoint is the size of what changed rather than the size of the database. That is the
/// difference between loading eight TPC-H tables one statement at a time and loading one of them
/// eight times over.
///
/// It cannot when the file is not there, when nothing in the catalog is native yet, or when the two
/// disagree about which tables exist, and the caller writes the whole file instead. A file that is
/// there but is not a native file this build can read is an error either way, so the error from
/// reading it is returned rather than swallowed into a rewrite that would overwrite it.
fn appended(
    path: &Path,
    catalog: &mut Catalog,
    names: &[QualifiedName],
    views: &[rudb_native::ViewEntry],
) -> Result<bool> {
    let Some(held) = committed(path)? else { return Ok(false) };
    if held.tables.is_empty() {
        return Ok(false);
    }
    let native = names
        .iter()
        .filter(|name| catalog.table(name).is_ok_and(|table| table.rows().is_native()))
        .map(|name| name.table.clone())
        .collect::<BTreeSet<_>>();
    if held.tables != native {
        return Ok(false);
    }
    let dirty =
        names.iter().filter(|name| !native.contains(&name.table)).cloned().collect::<Vec<_>>();
    let mut writer: Option<rudb_native::Writer> = None;
    for name in &dirty {
        let table = catalog.table(name)?;
        let fields = table.columns().to_vec();
        let columns = (0..fields.len()).collect::<Vec<_>>();
        let mut open = match writer.take() {
            None => rudb_native::Writer::open(path, name.table.clone(), fields)?,
            Some(writer) => writer.next(name.table.clone(), fields)?,
        };
        // The tables already in the file keep theirs, because they are carried forward by
        // directory pointer and their bytes are not rewritten. Only the ones being written here
        // need it said again.
        if let Some(clustering) = table.clustering() {
            open = open.declare(clustering.clone())?;
        }
        for at in 0..table.rows().chunk_count() {
            open.append(&table.rows().read(at, &columns)?)?;
        }
        writer = Some(open);
    }
    let Some(writer) = writer else { return Ok(false) };
    // Told rather than carried forward, because the caller's list is the catalog's and the writer's
    // is whatever the committed generation had. A view that was dropped since then is only missing
    // from the first of those.
    writer.with_views(views.to_vec()).finish()?;
    Ok(true)
}

/// The rows an `UPDATE` or a `DELETE` source produced, split by the flag column after the table's.
///
/// The first half is what the table holds afterwards, which is every row for an `UPDATE` and the
/// unflagged ones for a `DELETE`. The second is the flagged rows, the ones the statement changed or
/// took out, and it is only built when a `RETURNING` list wants them. The count is how many were
/// flagged either way. The flag column is taken off both.
fn split(
    chunks: Vec<Chunk>,
    delete: bool,
    wanted: bool,
) -> Result<(Vec<Chunk>, Vec<Chunk>, usize)> {
    let mut kept = Vec::with_capacity(chunks.len());
    let mut changed = Vec::new();
    let mut count = 0;
    for chunk in chunks {
        let width = chunk.width().saturating_sub(1);
        let hit = Selection::from_predicate(chunk.len(), |row| {
            chunk.value_at(row, width) == Value::Boolean(true)
        });
        count += hit.len();
        let columns: Vec<usize> = (0..width).collect();
        let chunk = chunk.project(&columns)?;
        if wanted && !hit.is_empty() {
            changed.push(chunk.clone().select(&hit)?);
        }
        if delete {
            let rest = hit.complement(chunk.len());
            if !rest.is_empty() {
                kept.push(chunk.select(&rest)?);
            }
        } else {
            kept.push(chunk);
        }
    }
    Ok((kept, changed, count))
}

/// Whether a table can be written straight into the file as its own generation.
///
/// The writer carries forward the tables the file names and nothing else, so the file and the
/// catalog have to already agree about which those are. A table the file names and the catalog has
/// dropped would come back, and a table the catalog holds that the file does not name would be left
/// out of the generation this commits and would only reach the file at the next rewrite, which is a
/// rewrite this was meant to avoid.
///
/// The target is counted out of both sides rather than assumed to be in the catalog, because a
/// `CREATE TABLE AS SELECT` asks this before it has made the entry.
fn appendable(path: &Path, catalog: &Catalog, target: &QualifiedName) -> Result<bool> {
    let Some(held) = held_rows(path)? else { return Ok(false) };
    // A committed table of this name with rows in it is one the writer would collide with, because
    // carrying those rows forward means reading and rewriting its pages. One with no rows is not:
    // the generation being written takes its place. That is what lets a schema created and
    // committed by an earlier statement still be loaded by a stream instead of through memory.
    if held.get(&target.table).is_some_and(|rows| *rows > 0) {
        return Ok(false);
    }
    // Everything the new generation carries forward, which is the file's tables without the one
    // being written. It has to match the catalog's other tables exactly for the same reason it
    // always did: a table in one and not the other is rows this generation would not carry.
    let carried =
        held.keys().filter(|name| *name != &target.table).cloned().collect::<BTreeSet<_>>();
    let others = catalog.stored_tables().filter(|table| table.name() != target).count();
    let native = catalog
        .stored_tables()
        .filter(|table| table.name() != target && table.rows().is_native())
        .map(|table| table.name().table.clone())
        .collect::<BTreeSet<_>>();
    // The count as well as the set, because a table that is in neither is a table with rows in
    // memory that this generation would not carry.
    // Views are not asked about, because the writer that follows this carries forward whatever the
    // committed generation says about them and does not have to be told. A view made or dropped
    // since then is settled by the checkpoint after it, which sees the two lists differ.
    Ok(carried == native && others == native.len())
}

/// One pipeline instance's place in the source, and the run of chunks it is holding.
///
/// The run is what makes the sink safe to instance. A stripe has to be a contiguous run of the
/// source in order, and the writer cannot work out which of several interleaved callers a chunk
/// belongs to, so each instance groups its own and hands over whole stripes.
///
/// It is also where one instance's share of the load profile's convert stage is counted. Convert is
/// everything upstream of the writer, the scan and the operators between it and here, and nothing
/// times that directly. So the instance times itself from the first thing it is handed to
/// [`Sink::combine`], takes off the time it spent inside the writer, which the writer charges to
/// its own stages, and charges what is left once, at the end. Rows and bytes are plain integers
/// here and go to the profile in the same one call.
#[derive(Debug, Default)]
struct NativePlace {
    morsel: u64,
    chunk: u64,
    held: Vec<((u64, u64), Chunk)>,
    /// What `held` was charged to the load profile, given back once its stripe is written.
    holding: u64,
    started: Option<Span>,
    inside_wall: u64,
    inside_cpu: u64,
    rows: u64,
    bytes: u64,
}

impl NativePlace {
    /// Starts the instance's clock the first time it is called, on the thread that fills it.
    fn start(&mut self) {
        if self.started.is_none() {
            self.started = Some(Span::start());
        }
    }
}

/// A writer that has been told what order the rows it is about to take are meant to be in.
///
/// The declaration has to go down with the table the sink writes, and not only at a checkpoint that
/// rewrites the file. A streaming load is the path a table of any size takes, the rows arriving at
/// it are already sorted because the binder put the sort there, and without this the file would hold
/// the right rows in the right order with nothing saying so. The table is then rebound to what the
/// file says, so the declaration would be gone from the catalog as well, on the statement that
/// honoured it.
fn declared(
    writer: rudb_native::Writer,
    clustering: Option<Clustering>,
) -> Result<rudb_native::Writer> {
    match clustering {
        None => Ok(writer),
        Some(clustering) => writer.declare(clustering),
    }
}

/// How many rows a load asks the scan under it to gather small row groups into, as one morsel.
///
/// The sink never lets a stripe span two morsels, so a Parquet file of eight thousand row groups
/// loaded a group at a time is eight thousand row stripes, written one at a time behind the
/// writer's lock. Loading the ClickBench ten million row sample (1,203 groups) on the 32 thread
/// gamingpc took 76.4 s that way. Gathered, it took 17.0 s at 32,768 rows, 20.8 s at 131,072 and
/// 19.9 s at 524,288, and the smallest target wrote a 2.31 GiB file against 1.55 GiB for the other
/// two, because short stripes compress worse. The largest peaked at 6.46 GiB of memory against 3.37
/// GiB, so this is the middle one: the file size and query speed of a full stripe at half its peak.
const GATHER_ROWS: usize = 131_072;

/// The root of a file-backed initial insert.
#[derive(Debug)]
struct NativeSink {
    writer: Mutex<Option<rudb_native::Writer>>,
    /// Encodes a stripe as far as it can be without the writer, so that the lock is held for the
    /// dictionary merge and the write rather than the whole encode.
    preparer: rudb_native::Preparer,
    /// Merges a prepared stripe into the table's dictionaries one column lock at a time, so the
    /// writer's own lock is held only for the write.
    merger: rudb_native::Merger,
    /// The file being written, when it is not the database itself, and what gets renamed over the
    /// database at the end. `None` for an append, which writes the database in place and has
    /// nothing to rename: the bytes go past the catalog the committed generation points at, and
    /// the file still reads as that generation until the last write lands in the header.
    temporary: Option<PathBuf>,
    target: PathBuf,
    table: String,
    fields: Vec<Field>,
    profile: Arc<LoadProfile>,
}

impl NativeSink {
    fn create(
        target: &Path,
        name: String,
        fields: Vec<Field>,
        clustering: Option<Clustering>,
    ) -> Result<Self> {
        let temporary = target.with_extension(format!("{}.tmp", std::process::id()));
        if temporary.exists() {
            std::fs::remove_file(&temporary).map_err(|error| Error::io(error.to_string()))?;
        }
        let profile = LoadProfile::begin(name.clone());
        let writer = rudb_native::Writer::create(&temporary, name.clone(), fields.clone())?
            .with_profile(Arc::clone(&profile));
        let mut writer = declared(writer, clustering)?;
        Ok(Self {
            preparer: writer.preparer(),
            merger: writer.merger()?,
            writer: Mutex::new(Some(writer)),
            temporary: Some(temporary),
            target: target.to_path_buf(),
            table: name,
            fields,
            profile,
        })
    }

    /// The same sink against a file that is already a database, as the generation after the one it
    /// holds.
    fn open(
        target: &Path,
        name: String,
        fields: Vec<Field>,
        clustering: Option<Clustering>,
    ) -> Result<Self> {
        let profile = LoadProfile::begin(name.clone());
        let writer = rudb_native::Writer::open(target, name.clone(), fields.clone())?
            .with_profile(Arc::clone(&profile));
        let mut writer = declared(writer, clustering)?;
        Ok(Self {
            preparer: writer.preparer(),
            merger: writer.merger()?,
            writer: Mutex::new(Some(writer)),
            temporary: None,
            target: target.to_path_buf(),
            table: name,
            fields,
            profile,
        })
    }

    /// Runs `work` on the writer under its lock, charging the wait for the lock as a write wait.
    fn locked<T>(&self, work: impl FnOnce(&mut rudb_native::Writer) -> Result<T>) -> Result<T> {
        let waiting = Instant::now();
        let mut writer =
            self.writer.lock().map_err(|_| Error::internal("native writer panicked"))?;
        self.profile.waited(Stage::Write, elapsed_ns(waiting));
        work(
            writer
                .as_mut()
                .ok_or_else(|| Error::internal("native writer was already committed"))?,
        )
    }

    /// Hands whatever this instance is holding to the writer as one stripe.
    fn hand_over(&self, place: &mut NativePlace) -> Result<()> {
        if place.held.is_empty() {
            return Ok(());
        }
        let parts = std::mem::take(&mut place.held);
        let holding = std::mem::take(&mut place.holding);
        // Once a stripe, so both clocks. The waits for the lock are write waits: they are the time
        // one instance spent while another was writing its stripe, which is the cost of the writer
        // being one file behind one lock.
        //
        // The lock is taken once, to write. The encode before it needs nothing of the writer, and
        // the merge takes the lock of each column it merges into instead, so two stripes merge at
        // once unless they want the same column at the same moment. That is what lets thirty two
        // instances keep going when one of them is merging `URL`.
        let inside = Span::start();
        let appended = self.preparer.prepare(parts).and_then(|prepared| {
            let merged = self.merger.merge(prepared)?;
            let mut paged = merged.pages()?;
            self.merger.give_back(&mut paged)?;
            self.locked(|writer| writer.write(paged))
        });
        // The rows are charged until their stripe is in the file, since the encode keeps them
        // until every column is done and the pages it built are bytes on top of that.
        self.profile.release(holding);
        let (wall, cpu) = inside.stop();
        place.inside_wall = place.inside_wall.saturating_add(wall);
        place.inside_cpu = place.inside_cpu.saturating_add(cpu);
        appended
    }
}

impl Sink for NativeSink {
    type Local = NativePlace;

    fn parallel(&self) -> bool {
        // The writer is one file behind one lock, but a stripe is encoded before the lock is taken
        // and its pages are built between the two times it is, so what one instance holds the
        // lock for is the dictionary merge and the write. More than one instance buys the read,
        // which is decoding a row group of the Parquet source, and the encode, which is most of
        // what a load costs.
        true
    }

    fn local(&self) -> Self::Local {
        NativePlace::default()
    }

    fn gather(&self) -> usize {
        GATHER_ROWS
    }

    fn at(&self, morsel: &Morsel, place: &mut Self::Local) -> Result<()> {
        place.start();
        // A stripe never spans two morsels, so that its parts are a run of the source with nothing
        // from another instance in the middle of them. The cost is a short stripe at the end of
        // each morsel, and a morsel on ClickBench is a whole row group of about a million rows, or
        // a run of small groups gathered up to GATHER_ROWS.
        self.hand_over(place)?;
        place.morsel = morsel.index();
        place.chunk = 0;
        Ok(())
    }

    fn sink(&self, chunk: &Chunk, place: &mut Self::Local) -> Result<Progress> {
        for (at, field) in self.fields.iter().enumerate().filter(|(_, field)| field.not_null) {
            let vector = chunk.column(at)?;
            let null = match vector.form() {
                Form::Dictionary | Form::Rle => (0..vector.len()).any(|row| vector.is_null_at(row)),
                _ => vector.validity().has_nulls(vector.len()),
            };
            if null {
                return Err(Error::constraint(format!(
                    "NOT NULL constraint failed: {}.{}",
                    self.table, field.name
                )));
            }
        }
        place.start();
        place.rows = place.rows.saturating_add(chunk.len() as u64);
        let footprint = chunk.footprint() as u64;
        place.bytes = place.bytes.saturating_add(footprint);
        place.holding = place.holding.saturating_add(footprint);
        self.profile.hold(footprint);
        place.held.push(((place.morsel, place.chunk), chunk.clone()));
        place.chunk = place.chunk.saturating_add(1);
        if place.held.len() == rudb_native::STRIPE_PARTS {
            self.hand_over(place)?;
        }
        Ok(Progress::More)
    }

    fn combine(&self, mut local: Self::Local) -> Result<()> {
        let handed = self.hand_over(&mut local);
        if let Some(started) = local.started.take() {
            let (wall, cpu) = started.stop();
            self.profile.charge(
                Stage::Convert,
                wall.saturating_sub(local.inside_wall),
                cpu.saturating_sub(local.inside_cpu),
            );
            self.profile.moved(Stage::Convert, 0, local.bytes, local.rows);
        }
        handed
    }

    fn finalize(&self, _threads: &Lease<'_>) -> Result<()> {
        let writer = self
            .writer
            .lock()
            .map_err(|_| Error::internal("native writer panicked"))?
            .take()
            .ok_or_else(|| Error::internal("native writer was already committed"))?;
        writer.finish()?;
        let Some(temporary) = &self.temporary else {
            self.profile.finish();
            return Ok(());
        };
        let renamed = {
            let _timing = self.profile.span(Stage::Publish);
            publish(&RealFilesystem::new(), temporary, &self.target)
        };
        self.profile.finish();
        renamed
    }
}

impl Drop for NativeSink {
    /// A load that failed or was cancelled still ends, and its profile says how long it ran before
    /// it did. A profile that was finished already keeps the time it had.
    fn drop(&mut self) {
        self.profile.finish();
    }
}

/// Nanoseconds since `since`, as the profile counts them.
fn elapsed_ns(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Shared {
    /// Applies the session's public error rendering mode at the API boundary.
    pub(crate) fn process_error(&self, error: Error) -> Error {
        if self.session().semantics().errors_as_json() { error.into_json() } else { error }
    }

    /// The catalog, for reading.
    ///
    /// A poisoned lock is taken rather than reported. Poisoning says some thread panicked while it
    /// held the lock, and the catalog is a `Vec` of chunks rather than an invariant somebody was
    /// halfway through breaking, so refusing every later query would turn one panicked query into a
    /// dead database.
    fn read(&self) -> RwLockReadGuard<'_, Catalog> {
        self.inner.catalog.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The catalog, for writing.
    fn write(&self) -> RwLockWriteGuard<'_, Catalog> {
        self.inner.catalog.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The right to write the file and change the catalog, taken before the catalog lock. See
    /// `Inner::writer`. Poisoning is ignored for the reason [`Shared::read`] gives.
    fn writing(&self) -> MutexGuard<'_, ()> {
        self.inner.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The memory and the threads this database will lend a query.
    fn budget(&self) -> Budget<'_> {
        Budget { memory: &self.inner.memory, pool: &self.inner.pool }
    }

    /// What the settings are now, read once and handed to both the binder and the executor.
    ///
    /// It used to be read only for a plan that turned out to mention `duckdb_settings()`, on the
    /// argument that no other query looks at a setting. `current_setting()` is the other one that
    /// does and the binder folds it, so the values have to be in hand before there is a plan to
    /// look at, and the check that would say whether a statement needs them is a walk of the parse
    /// tree that costs about what reading them costs. So it is read once per statement and the
    /// special case is gone. [`crate::settings::Settings::session`] takes two locks for it.
    pub(crate) fn session(&self) -> Session {
        self.inner.settings.session()
    }

    /// Runs one query and returns every row it produced.
    ///
    /// `EXPLAIN` comes through here as well as through [`Shared::execute`], because it answers with
    /// rows and this is the path that reads rows back. It takes the read lock like any other query,
    /// since printing a plan changes nothing. A statement that writes is refused here rather than
    /// run under a read lock.
    pub(crate) fn query(&self, sql: &str, cancel: &Cancel) -> Result<QueryResult> {
        self.in_transaction(sql, || {
            if let Some(answer) = self.cached_native_aggregate(sql, cancel)? {
                return Ok(answer);
            }
            self.query_mirrored(sql, cancel, true)
        })
    }

    /// Runs a statement under whatever transaction is open.
    ///
    /// A transaction a statement failed in is aborted, and until it is closed every statement but
    /// the one closing it is refused with the pin's sentence. A statement that did not parse leaves
    /// the transaction as it was, because on the pin it never reached one.
    fn in_transaction(
        &self,
        sql: &str,
        run: impl FnOnce() -> Result<QueryResult>,
    ) -> Result<QueryResult> {
        let aborted = self.open().as_ref().is_some_and(|open| open.aborted);
        if aborted && crate::syntax::statement_kind(sql) != Some("TransactionStatement") {
            return Err(Error::transaction("Current transaction is aborted (please ROLLBACK)"));
        }
        let result = run();
        // A statement rudb does not run yet leaves the transaction open. The pin would have run it,
        // so aborting here would turn one gap into a refusal of everything after it.
        let aborts = result.as_ref().err().is_some_and(|error| {
            !matches!(
                error.code(),
                rudb_common::ErrorCode::Parser | rudb_common::ErrorCode::NotImplemented
            )
        });
        if aborts {
            if let Some(open) = self.open().as_mut() {
                open.aborted = true;
            }
        }
        result
    }

    /// The open transaction, if there is one.
    fn open(&self) -> MutexGuard<'_, Option<Open>> {
        self.inner.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `BEGIN`, `COMMIT` or `ROLLBACK`, with the pin's refusals for the ones that do not fit.
    fn transaction(&self, kind: ast::Transaction, catalog: &mut Catalog) -> Result<QueryResult> {
        let mut open = self.open();
        match kind {
            ast::Transaction::Begin { read_only } => {
                if open.is_some() {
                    return Err(Error::transaction(
                        "cannot start a transaction within a transaction",
                    ));
                }
                *open = Some(Open { before: catalog.clone(), aborted: false, read_only });
            }
            ast::Transaction::Commit => {
                let Some(closed) = open.take() else {
                    return Err(Error::transaction("cannot commit - no transaction is active"));
                };
                if closed.aborted {
                    catalog.restore(closed.before);
                }
            }
            ast::Transaction::Rollback => {
                let Some(closed) = open.take() else {
                    return Err(Error::transaction("cannot rollback - no transaction is active"));
                };
                catalog.restore(closed.before);
            }
        }
        Ok(QueryResult::empty())
    }

    /// Whether a transaction is open, which is what keeps a load from writing the file directly,
    /// since a file that was written cannot be rolled back.
    fn transacting(&self) -> bool {
        self.open().is_some()
    }

    /// Reuse a simple native aggregate plan while the table and settings are unchanged.
    /// Execution still runs for every call, producing a fresh answer and metrics document.
    fn cached_native_aggregate(&self, sql: &str, cancel: &Cancel) -> Result<Option<QueryResult>> {
        let catalog = self.read();
        let revision = self.inner.settings_revision.load(Ordering::Relaxed);
        let cached = self
            .inner
            .native_aggregate_plan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|cached| {
                cached.sql == sql
                    && cached.catalog_generation == catalog.generation()
                    && cached.settings_revision == revision
            })
            .map(|cached| Arc::clone(&cached.plan));
        let Some(plan) = cached else { return Ok(None) };
        let seams = self.seams(sql)?;
        let context = self.optimizer(&catalog)?;
        let session = self.session();
        let under = Under::new(self.budget(), context.facts(), &seams, &session, Rows::ForACaller);
        run(sql, &plan, &catalog, cancel, under).map(Some)
    }

    fn remember_native_aggregate(&self, sql: &str, ast: &Ast, plan: &Plan, catalog: &Catalog) {
        if !is_native_summary_aggregate(ast, plan, catalog) {
            return;
        }
        *self.inner.native_aggregate_plan.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(CachedNativeAggregate {
                sql: sql.to_string(),
                catalog_generation: catalog.generation(),
                settings_revision: self.inner.settings_revision.load(Ordering::Relaxed),
                plan: Arc::new(plan.clone()),
            });
    }

    /// [`Shared::query`], asking for the Parquet mirrors the statement wants when `mirror` is set.
    ///
    /// A statement that wanted one is bound again once the mirrors are in, and not a third time,
    /// so a mirror that cannot be had costs one extra bind and leaves the statement reading the
    /// file.
    fn query_mirrored(&self, sql: &str, cancel: &Cancel, mirror: bool) -> Result<QueryResult> {
        let catalog = self.read();
        let seams = self.seams(sql)?;
        let context = self.optimizer(&catalog)?;
        let session = self.session();
        let (ast, parse_ns) =
            timed(|| rudb_parse::parse_ast_with_case(sql, session.semantics().identifier_case()))?;
        let outlined = mirror && self.inner.settings.config().parquet_mirror();
        let (bound, bind_ns) = timed(|| {
            if outlined {
                rudb_bind::bind_statement_outlined(&ast, &catalog, &Parameters::new(), &session)
            } else {
                rudb_bind::bind_statement_with(&ast, &catalog, &Parameters::new(), &session)
            }
        })?;
        if mirror {
            let wanted = self.wanted_mirrors(&bound);
            // An outlined plan that asked for a mirror is bound again whether or not it gets one,
            // because the reads that asked were bound without their bounds.
            if !wanted.is_empty() || (outlined && asked_for_mirrors(&bound)) {
                // The plan goes first. It holds the bounds of the footer it was bound against, which
                // on a file of eighty row groups is two megabytes the rebound plan does not use.
                drop(bound);
                drop(catalog);
                self.mirror(&wanted);
                return self.query_mirrored(sql, cancel, false);
            }
        }
        match bound {
            Bound::Query(mut plan) => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                self.remember_native_aggregate(sql, &ast, &plan, &catalog);
                let budget = self.budget();
                let under = Under::new(budget, context.facts(), &seams, &session, Rows::ForACaller)
                    .after(Planning { parse_ns, bind_ns, optimize_ns });
                run(sql, &plan, &catalog, cancel, under)
            }
            Bound::Explain { mut plan, analyze, statistics } => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                let seams = rudb_opt::explain::Seams::new(&seams, rudb_exec::registries());
                explaining(
                    &plan,
                    &catalog,
                    cancel,
                    self.budget(),
                    &context,
                    seams,
                    &session,
                    Asked { analyze, statistics },
                    sql,
                    Planning { parse_ns, bind_ns, optimize_ns },
                )
            }
            _ => Err(Error::not_implemented("a statement that is not a query, on the query path")),
        }
    }

    /// The Parquet files a bound query read directly and would rather have read through a native
    /// mirror, less the ones this database already failed to mirror.
    ///
    /// Only a query asks. A load reads the file once into a table of its own, which is the one
    /// thing a mirror would duplicate, and an `EXPLAIN` should not start one.
    fn wanted_mirrors(&self, bound: &Bound) -> Vec<(String, bool)> {
        let Bound::Query(plan) = bound else { return Vec::new() };
        if plan.wanted_mirrors().is_empty() {
            return Vec::new();
        }
        let config = self.inner.settings.config();
        if !config.parquet_mirror() {
            return Vec::new();
        }
        let declined = self.inner.declined.lock().unwrap_or_else(PoisonError::into_inner);
        plan.wanted_mirrors()
            .iter()
            .filter(|(_, _, rows)| *rows >= config.mirror_rows())
            .map(|(path, binary_as_string, _)| (path.clone(), *binary_as_string))
            .filter(|wanted| !declined.contains(wanted))
            .collect()
    }

    /// Finds or builds a mirror of each file and puts it in the catalog.
    ///
    /// Nothing here fails the statement. A file that cannot be mirrored is read as it always was,
    /// and it is remembered so that the next statement does not try again.
    fn mirror(&self, wanted: &[(String, bool)]) {
        let config = self.inner.settings.config();
        for (path, binary_as_string) in wanted {
            let added = crate::mirror::ensure(path, *binary_as_string, config).and_then(|found| {
                let Some((stamp, reader)) = found else { return Ok(false) };
                self.write().add_mirror(path, *binary_as_string, stamp, reader)?;
                Ok(true)
            });
            if !matches!(added, Ok(true)) {
                self.inner
                    .declined
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert((path.clone(), *binary_as_string));
            }
        }
    }

    /// The seam settings a statement runs under, which is the session's with its hints on top.
    ///
    /// What the settings choose goes nowhere yet, because no seam has a second implementation to
    /// choose between until F1 and every one of the twenty seven is unregistered. What they do
    /// today is fail a statement whose hint names a seam nobody has, and feed the seam section of
    /// `EXPLAIN`, which is the half of the behaviour worth having before the other half arrives: a
    /// hint that is quietly ignored is a measurement of the wrong thing.
    pub(crate) fn seams(&self, sql: &str) -> Result<rudb_seam::Settings> {
        let mut seams = self.inner.settings.seams();
        for hint in rudb_parse::hints(sql)? {
            seams.hint(hint)?;
        }
        Ok(seams)
    }

    /// The passes this database's queries run, as `SET disabled_optimizers` has left them, with
    /// the row counts the catalog holds.
    ///
    /// The passes are rebuilt for each statement, because the statement before this one may have
    /// been the `SET`. It cannot fail: the names were checked when they were set, and the `?` is
    /// here because nothing stops a later version from having a pass that goes away.
    ///
    /// The counts are not rebuilt for each statement. They were, and on a catalog of two hundred
    /// tables of twenty columns that was about forty five microseconds a statement spent walking every
    /// table and asking every column for a distinct count, whether or not the query named any of
    /// them. What replaces it is the catalog's own generation: a set of counts carries the version
    /// it was read at, and a statement that finds that version still current plans from the set the
    /// last one built. Anything that can change what a query would read moves the generation on, so
    /// a stale set is replaced rather than used, and a plan is a function of exactly one generation.
    ///
    /// The catalog comes in as an argument rather than being read from the lock here, because
    /// every caller is already holding that lock and one of them is holding it for writing. This
    /// is also the seam that stops the optimizer from reaching the catalog on its own: what it
    /// gets is a set of counts, which is the whole of what estimation reads today.
    fn optimizer(&self, catalog: &Catalog) -> Result<rudb_opt::pass::Context> {
        let mut context =
            rudb_opt::pass::Context::without(&self.inner.settings.disabled_optimizers())?;
        context.measure(self.estimates(catalog));
        context.relate(self.relationships(catalog));
        context.size(self.inner.settings.sizes());
        context.govern(self.inner.settings.rules());
        Ok(context)
    }

    /// The relationships this catalog's files hold a readable forward link for.
    ///
    /// The seam the graph layer reaches the optimizer through, and the place the difference between
    /// a declaration and a measurement is enforced. What is declared is a setting, which is what
    /// somebody believes. What goes in here is the subset of it the child's file actually holds a
    /// link for, and a link is only written once the build has read the parent side and found it
    /// unique, so a plan that reads one is relying on something that was checked rather than on
    /// something that was said.
    ///
    /// A poisoned lock is a cache that is not there, for the reason [`Self::facts`] says.
    ///
    /// `graph.sections` is read here and nowhere else, because this is the only place a stored
    /// section reaches a plan. `spec/graph/09-measurement.md` section 9.2 asks for the whole layer
    /// to be turnable off from a setting so that the suite can be run both ways and the two runs
    /// compared byte for byte, and a switch that reached half the layer would make that comparison
    /// say nothing. Returning nothing here is the whole of off: the optimizer is handed no
    /// relationship, so no rewrite has anything to fire on, so no plan reads a link, so the
    /// executor never opens one. The sections stay in the file and the checkpoint still writes
    /// them, which is what section 3.1 says they are for, being ignorable rather than absent.
    fn relationships(&self, catalog: &Catalog) -> Arc<Vec<rudb_opt::link::Linked>> {
        if !self.inner.settings.rules().enabled(Rule::GraphSections) {
            return Arc::default();
        }
        let declared = self.inner.settings.links();
        // The common case by a long way, and the one worth not taking a lock for: no relationship
        // is declared, so there is nothing to look for and nothing to cache.
        if declared.is_empty() {
            return Arc::default();
        }
        let generation = catalog.generation();
        let mut held = match self.inner.relationships.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        if held.0 != generation || held.1 != declared {
            let found = Arc::new(Self::related(catalog, &declared));
            *held = (generation, declared, found);
        }
        Arc::clone(&held.2)
    }

    /// Every declared relationship, with whether the files answer for it, read once.
    ///
    /// The ones no file answers for are kept rather than dropped. Only a built one licenses a
    /// rewrite, and the planner is careful about that, but a declaration whose link was never built
    /// is exactly the case somebody wants explained: it looks from the outside like a relationship
    /// nobody declared, and `spec/graph/06-the-optimizer.md` section 6.7 asks for the two to be told
    /// apart in the plan output.
    ///
    /// A declaration this cannot parse contributes nothing, which is the same silence
    /// `rudb_links()` gives it: a setting the parser refused is one no build acted on either, so
    /// there is no link to find for it whatever this did.
    fn related(catalog: &Catalog, declared: &str) -> Vec<rudb_opt::link::Linked> {
        let mut found = Vec::new();
        for link in rudb_graph::parse_links(declared).unwrap_or_default() {
            // One column each. A composite relationship is two key maps over a folded key and
            // nothing folds one yet, so there is no link stored for one and nothing to plan over.
            let ([child_key], [parent_key]) = (&link.child.columns[..], &link.parent.columns[..])
            else {
                continue;
            };
            let built = stored_link(
                catalog,
                (&link.child.table, child_key),
                (&link.parent.table, parent_key),
            );
            let sides = (&link.child.table, child_key, &link.parent.table, parent_key);
            found.push(match built {
                Some(true) => rudb_opt::link::Linked::verified(sides.0, sides.1, sides.2, sides.3),
                Some(false) => rudb_opt::link::Linked::built(sides.0, sides.1, sides.2, sides.3),
                None => rudb_opt::link::Linked::declared(sides.0, sides.1, sides.2, sides.3),
            });
        }
        found
    }

    /// The counts for this catalog, built if the ones in hand are out of date and reused if not.
    ///
    /// A poisoned lock is treated as a cache that is not there rather than as a failure, because
    /// what is behind it is a set of numbers that can always be built again and a statement that
    /// refused to run over it would be refusing for no reason.
    fn facts(&self, catalog: &Catalog) -> Arc<rudb_opt::estimate::Facts> {
        let generation = catalog.generation();
        let mut held = match self.inner.facts.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        if held.generation() != generation {
            *held = Arc::new(Self::measured(catalog, generation));
        }
        Arc::clone(&held)
    }

    /// Everything the optimizer is told about that catalog, read once.
    /// The counts the optimizer is allowed to plan from, which is all of them unless `statistics` is
    /// off.
    ///
    /// The same shape [`Self::relationships`] has and for the same reason. A switch that reached half
    /// the layer would make the comparison the ablation runs say nothing, and the honest way to turn a
    /// statistic off is to not hand it over rather than to ask every reader to remember to check.
    ///
    /// Off leaves the row counts, because a row count is the length of the table rather than something
    /// a store measured. `rudb_opt::estimate::Facts::without_distincts` says why at more length.
    fn estimates(&self, catalog: &Catalog) -> Arc<rudb_opt::estimate::Facts> {
        let held = self.facts(catalog);
        if self.inner.settings.rules().enabled(Rule::StatsAll) {
            return held;
        }
        Arc::new(held.without_distincts())
    }

    fn measured(catalog: &Catalog, generation: u64) -> rudb_opt::estimate::Facts {
        let mut facts = rudb_opt::estimate::Facts::at(generation);
        for table in catalog.tables().chain(catalog.mirrored_tables()) {
            let name = table.name();
            let rows = u64::try_from(table.rows().len()).unwrap_or(u64::MAX);
            facts.record(&name.catalog, &name.schema, &name.table, rows);
            // Where a distinct count comes from here, which is a property of the table and not of
            // the column. A file counts the entries of a dictionary, or for an integer column the set
            // its writer kept on the frequency pass, and a table in memory reads the sketch
            // `rudb-storage`'s `count.rs` built as the rows arrived.
            // Named the long way round because this file has a `Rows` of its own, which is the
            // shape a result set comes back in and has nothing to do with where a table keeps its
            // rows.
            let provenance = match table.rows() {
                rudb_catalog::table::Rows::Memory(_) => Provenance::Sketch,
                rudb_catalog::table::Rows::Native(_) => Provenance::Dictionary,
                // A table with rows in memory beside the file answers no distinct count at all, so
                // the loop below leaves every one of its columns out and this is never read. It is
                // the sketch rather than the dictionary because the rows nobody has counted are the
                // ones in memory.
                rudb_catalog::table::Rows::Grown(_, _) => Provenance::Sketch,
            };
            for (at, column) in table.columns().iter().enumerate() {
                // A table that cannot answer leaves the column out, which is a column of a native
                // table that has no dictionary and a column of a memory table with more distinct
                // values in it than its sketch holds. The estimate falls back to the shape it used
                // before there were any of these. Only an exact count is asked for, because this
                // goes into `Facts` and everything in there is read back as exact. The estimate a
                // wider column does have reaches the optimizer the other way, through the plan,
                // where it carries the class that says what it is.
                let Ok(Some(distinct)) = table.rows().distinct_values(at) else {
                    continue;
                };
                facts.record_distinct(
                    &name.catalog,
                    &name.schema,
                    &name.table,
                    &column.name,
                    distinct,
                    provenance,
                );
            }
        }
        facts
    }

    /// The query timeout this database was opened with.
    pub(crate) fn timeout(&self) -> Option<std::time::Duration> {
        self.inner.settings.config().query_timeout()
    }

    /// The token a statement of this database's runs under, when nobody holds one of their own.
    ///
    /// It carries the configured query timeout and nothing can interrupt it, because there is
    /// nobody holding the other half. [`Connection`] is where the other half lives.
    pub(crate) fn token(&self) -> Cancel {
        match self.inner.settings.config().query_timeout() {
            Some(timeout) => Cancel::after(timeout),
            None => Cancel::new(),
        }
    }

    /// The plan a query runs.
    pub(crate) fn plan(&self, sql: &str) -> Result<String> {
        let catalog = self.read();
        let context = self.optimizer(&catalog)?;
        Ok(planned(sql, &catalog, &context, &self.session())?.to_string())
    }

    /// Runs one statement, which may change the database.
    ///
    /// The write lock is taken for the whole statement rather than for the part that writes,
    /// because the part that writes is decided by what the part that reads produced. `INSERT INTO t
    /// SELECT * FROM t` would otherwise read the table under a read lock, let go, and append to
    /// whatever the table had become in between.
    pub(crate) fn execute(&self, sql: &str, cancel: &Cancel) -> Result<QueryResult> {
        self.in_transaction(sql, || {
            if let Some(answer) = self.cached_native_aggregate(sql, cancel)? {
                return Ok(answer);
            }
            let session = self.session();
            let (ast, parse_ns) = timed(|| {
                rudb_parse::parse_ast_with_case(sql, session.semantics().identifier_case())
            })?;
            self.execute_ast(&ast, sql, &Parameters::new(), cancel, parse_ns)
        })
    }

    /// Runs one parsed statement, with values for its parameters.
    ///
    /// The prepared statement path, and the path an ordinary statement takes once it is parsed, so
    /// that there is one description of what running a statement does.
    ///
    /// `parse_ns` is how long the caller spent getting the AST, because this function is below the
    /// parse and the metrics document is below this. A prepared statement passes zero, which is not
    /// a missing measurement: the parse happened once at `PREPARE` and charging it again to every
    /// execution would make a statement prepared once and run a thousand times report the same
    /// parse a thousand times.
    pub(crate) fn execute_ast(
        &self,
        ast: &Ast,
        sql: &str,
        parameters: &Parameters,
        cancel: &Cancel,
        parse_ns: u64,
    ) -> Result<QueryResult> {
        let _writing = self.writing();
        self.execute_mirrored(ast, sql, parameters, cancel, parse_ns, true)
    }

    /// [`Shared::execute_ast`], asking for mirrors the way [`Shared::query_mirrored`] does.
    fn execute_mirrored(
        &self,
        ast: &Ast,
        sql: &str,
        parameters: &Parameters,
        cancel: &Cancel,
        parse_ns: u64,
        mirror: bool,
    ) -> Result<QueryResult> {
        let seams = self.seams(sql)?;
        let mut catalog = self.write();
        let context = self.optimizer(&catalog)?;
        let session = self.session();
        let outlined = mirror && self.inner.settings.config().parquet_mirror();
        let (bound, bind_ns) = timed(|| {
            if outlined {
                rudb_bind::bind_statement_outlined(ast, &catalog, parameters, &session)
            } else {
                rudb_bind::bind_statement_with(ast, &catalog, parameters, &session)
            }
        })?;
        if mirror {
            let wanted = self.wanted_mirrors(&bound);
            if !wanted.is_empty() || (outlined && asked_for_mirrors(&bound)) {
                // See `query_mirrored`: the plan bound against the file holds its footer.
                drop(bound);
                drop(catalog);
                self.mirror(&wanted);
                return self.execute_mirrored(ast, sql, parameters, cancel, parse_ns, false);
            }
        }
        let writes = matches!(
            bound,
            Bound::CreateTable(_) | Bound::CreateView(_) | Bound::DropTable(_) | Bound::Insert(_)
        );
        if writes && self.open().as_ref().is_some_and(|open| open.read_only) {
            return Err(Error::transaction(format!(
                "Cannot write to database \"\"{}\"\" - transaction is launched in read-only mode",
                catalog.default_catalog()
            )));
        }
        match bound {
            Bound::Query(mut plan) => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                if parameters.is_empty() {
                    self.remember_native_aggregate(sql, ast, &plan, &catalog);
                }
                let budget = self.budget();
                let under = Under::new(budget, context.facts(), &seams, &session, Rows::ForACaller)
                    .after(Planning { parse_ns, bind_ns, optimize_ns });
                run(sql, &plan, &catalog, cancel, under)
            }
            Bound::Explain { mut plan, analyze, statistics } => {
                let ((), optimize_ns) = timed(|| rudb_opt::optimize_with(&mut plan, &context))?;
                let seams = rudb_opt::explain::Seams::new(&seams, rudb_exec::registries());
                explaining(
                    &plan,
                    &catalog,
                    cancel,
                    self.budget(),
                    &context,
                    seams,
                    &session,
                    Asked { analyze, statistics },
                    sql,
                    Planning { parse_ns, bind_ns, optimize_ns },
                )
            }
            Bound::Setting(setting) if setting.pragma => {
                self.inner.settings.toggle(&setting.name)?;
                self.inner.settings_revision.fetch_add(1, Ordering::Relaxed);
                Ok(QueryResult::empty())
            }
            Bound::Setting(setting) => {
                let value = setting.value.as_ref();
                self.inner.settings.apply(
                    &self.inner.memory,
                    &self.inner.pool,
                    &mut catalog,
                    &setting.name,
                    setting.scope,
                    value,
                )?;
                self.inner.settings_revision.fetch_add(1, Ordering::Relaxed);
                Ok(QueryResult::empty())
            }
            Bound::Transaction(kind) => self.transaction(kind, &mut catalog),
            Bound::Checkpoint => {
                // A read only database answers this the way the pinned DuckDB does, which is by
                // succeeding and writing nothing. It is not an error there and it is not one here.
                if let Some(path) = self.inner.path.as_ref().filter(|_| self.inner.writable) {
                    persist(path, &mut catalog, &self.inner.pages)?;
                    index(path, &mut catalog, &self.inner.settings.links(), &self.inner.pages)?;
                }
                Ok(QueryResult::empty())
            }
            Bound::CreateTable(mut create) => {
                // A `CREATE TABLE AS SELECT` into a file-backed database is the same write as an
                // `INSERT` into a table that was just created, so it takes the same sink and the
                // rows reach the file without the whole table being held in memory first. Without
                // this the statement builds the answer twice over, once in the root queue and once
                // as the table, and neither copy is charged against the memory limit.
                //
                // Only for a name the catalog does not have. `OR REPLACE` and `IF NOT EXISTS` both
                // have to decide what happens to the old table, and `CREATE OR REPLACE TABLE t AS
                // SELECT * FROM t` reads the table it is about to replace, so neither can have the
                // entry made before the query runs. Making it afterwards is also what leaves no
                // table behind when the query fails.
                // A temporary table never reaches the file, so it never takes this path however
                // well it fits the shape otherwise, and neither does anything at all on a read only
                // database, which is the one other way a statement writes the file without being a
                // checkpoint.
                let writable =
                    self.inner.writable && !create.name.temporary() && !self.transacting();
                if let Some(path) = self.inner.path.as_ref().filter(|_| writable) {
                    let fresh = create.source.is_some() && catalog.table(&create.name).is_err();
                    let alone = fresh && !path.exists() && catalog.stored_tables().count() == 0;
                    if fresh && (alone || appendable(path, &catalog, &create.name)?) {
                        let plan = create.source.as_mut().expect("a source, asked for above");
                        rudb_opt::optimize_with(plan, &context)?;
                        let table = create.name.table.clone();
                        let fields = create.columns.clone();
                        // No declaration to carry. The table is being created by this statement, so
                        // there is nowhere a declaration could have come from yet.
                        let sink = Arc::new(if alone {
                            NativeSink::create(path, table.clone(), fields, None)?
                        } else {
                            NativeSink::open(path, table.clone(), fields, None)?
                        });
                        // The rows go in under a read lock, so queries run while they do. Nothing
                        // else can change the catalog in the gap between the two locks, because
                        // every writer holds `writer` first and this statement is holding it now.
                        drop(catalog);
                        let reading = self.read();
                        #[cfg(test)]
                        if let Some(told) = self.inner.loading.lock().unwrap().take() {
                            let _ = told.send(());
                        }
                        let query = rudb_exec::build_measured_into(
                            plan,
                            &reading,
                            cancel,
                            &self.inner.memory,
                            &seams,
                            &session,
                            sink,
                        )?;
                        query.run(cancel, &self.inner.pool)?;
                        drop(query);
                        drop(reading);
                        let reader = rudb_native::Catalog::open(path)?.table(&table)?;
                        let mut catalog = self.write();
                        catalog.create_table(create.name.clone(), create.columns)?;
                        catalog.table_mut(&create.name)?.commit_native(reader)?;
                        return Ok(QueryResult::empty());
                    }
                }
                create_table(
                    sql,
                    create,
                    &mut catalog,
                    cancel,
                    self.budget(),
                    &context,
                    &seams,
                    &session,
                )?;
                Ok(QueryResult::empty())
            }
            Bound::CreateView(create) => {
                create_view(create, &mut catalog)?;
                Ok(QueryResult::empty())
            }
            Bound::DropTable(drop) => {
                for name in &drop.names {
                    match drop.kind {
                        Entry::Table => catalog.drop_table(name)?,
                        Entry::View => catalog.drop_view(name)?,
                    }
                }
                Ok(QueryResult::empty())
            }
            Bound::Insert(mut insert) => {
                // The source runs to completion before anything is appended, which is not an
                // implementation detail. `INSERT INTO t SELECT * FROM t` reads the table it writes,
                // and a version of this that appended chunk by chunk would either read its own
                // output forever or depend on how the scan holds its chunks.
                let ((), optimize_ns) =
                    timed(|| rudb_opt::optimize_with(&mut insert.source, &context))?;
                // Same as the create above: rows going into a temporary table are rows the file
                // never sees, and a read only database writes no file at all.
                let writable =
                    self.inner.writable && !insert.name.temporary() && !self.transacting();
                if let Some(path) = self.inner.path.as_ref().filter(|_| {
                    // A table with a key checks every row against the ones it holds, which the sink
                    // does not, so it takes the path through the table.
                    writable
                        && insert.write == Write::Append
                        && insert.returning.is_none()
                        && insert.checks.is_none()
                        && catalog.table(&insert.name).is_ok_and(|table| {
                            table.keys().is_empty() && table.foreign().is_empty()
                        })
                }) {
                    let target = catalog.table(&insert.name)?;
                    // Rows go from the source to the file without the table being held in memory on
                    // the way, which is the difference between loading a table and having to fit
                    // one in RAM. Either the sink writes a fresh file and renames it over the
                    // database, which is only safe while there is nothing in the database to lose,
                    // or it appends a generation to the file that is already there.
                    //
                    // A load that meets neither takes the in-memory path and reaches the file at
                    // the next checkpoint.
                    //
                    // Asked in this order because the second question opens the file's catalog and
                    // the first two are a `stat` and a count.
                    let alone = !path.exists() && catalog.stored_tables().count() == 1;
                    let empty = target.rows().is_empty();
                    if empty && (alone || appendable(path, &catalog, &insert.name)?) {
                        let table = target.name().table.clone();
                        let fields = target.columns().to_vec();
                        // The declaration the target already carries, which is the one the binder
                        // put the sort in for. The rows reaching the sink are in this order and the
                        // file has to say so, because the table is rebound to what the file says as
                        // soon as this returns.
                        let clustering = target.clustering().cloned();
                        let sink = Arc::new(if alone {
                            NativeSink::create(path, table.clone(), fields, clustering)?
                        } else {
                            NativeSink::open(path, table.clone(), fields, clustering)?
                        });
                        let query = rudb_exec::build_measured_into(
                            &insert.source,
                            &catalog,
                            cancel,
                            &self.inner.memory,
                            &seams,
                            &session,
                            sink,
                        )?;
                        query.run(cancel, &self.inner.pool)?;
                        drop(query);
                        // By name out of the file's catalog rather than as the one table in the
                        // file, because after an append it is not the one table in the file.
                        let reader = rudb_native::Catalog::open(path)?.table(&table)?;
                        let added = reader.table().rows();
                        catalog.table_mut(&insert.name)?.commit_native(reader)?;
                        return QueryResult::changed(added);
                    }
                }
                let facts = context.facts();
                let under = Under::new(self.budget(), facts, &seams, &session, Rows::ForATable)
                    .after(Planning { parse_ns, bind_ns, optimize_ns });
                let result = run(sql, &insert.source, &catalog, cancel, under)?;
                let workers = self.inner.pool.threads();
                let chunks = result.into_chunks();
                let wanted = insert.returning.is_some();
                let place = (cancel, &seams, &session);
                let mut checks = insert.checks.take();
                let (count, written) = match insert.write {
                    Write::Append if insert.conflict.is_some() => {
                        let conflict = insert.conflict.take().expect("asked just above");
                        let name = insert.name.clone();
                        let upsert = (conflict, checks.as_mut());
                        self.upsert(sql, &mut catalog, place, &name, upsert, chunks)?
                    }
                    Write::Append => {
                        if let Some(checks) = checks.as_mut() {
                            self.check(sql, &mut catalog, place, &insert.name, checks, &chunks)?;
                        }
                        foreign::missing(&catalog, &insert.name, &chunks)?;
                        let added = chunks.iter().map(Chunk::len).sum();
                        let written = if wanted { chunks.clone() } else { Vec::new() };
                        catalog.table_mut(&insert.name)?.append_all(chunks, workers)?;
                        (added, written)
                    }
                    Write::Update | Write::Delete => {
                        let delete = insert.write == Write::Delete;
                        let plain = catalog.table(&insert.name)?.foreign().is_empty();
                        let (kept, changed, count) =
                            split(chunks, delete, wanted || checks.is_some() || !plain)?;
                        if let Some(checks) = checks.as_mut() {
                            self.check(sql, &mut catalog, place, &insert.name, checks, &changed)?;
                        }
                        if !delete {
                            foreign::missing(&catalog, &insert.name, &changed)?;
                        }
                        foreign::lost(&catalog, &insert.name, &kept)?;
                        catalog.table_mut(&insert.name)?.replace_all(kept, workers)?;
                        (count, changed)
                    }
                };
                let Some(mut returning) = insert.returning else {
                    return QueryResult::changed(count);
                };
                // The list is a query over the table, so for the length of it the table holds the
                // rows the statement wrote and nothing else, and then gets its own back whether
                // the query ran or not.
                let held = catalog.table_mut(&insert.name)?.stand_in(written, workers)?;
                let answer = (|| {
                    let context = self.optimizer(&catalog)?;
                    rudb_opt::optimize_with(&mut returning, &context)?;
                    let under = Under::new(
                        self.budget(),
                        context.facts(),
                        &seams,
                        &session,
                        Rows::ForACaller,
                    );
                    run(sql, &returning, &catalog, cancel, under)
                })();
                catalog.table_mut(&insert.name)?.put_back(held);
                answer
            }
        }
    }
}

impl Shared {
    /// Refuses rows a write is about to keep when one of them fails a `CHECK` of the table, with
    /// the pin's message for the first constraint, in the order written, that a row fails.
    ///
    /// For the length of the query the table holds just these rows, and it gets its own back
    /// whether the query ran or not.
    fn check(
        &self,
        sql: &str,
        catalog: &mut Catalog,
        (cancel, seams, session): (&Cancel, &rudb_seam::Settings, &Session),
        name: &QualifiedName,
        checks: &mut rudb_bind::Checks,
        rows: &[Chunk],
    ) -> Result<()> {
        if rows.iter().all(|chunk| chunk.is_empty()) {
            return Ok(());
        }
        let workers = self.inner.pool.threads();
        let before = catalog.table_mut(name)?.stand_in(rows.to_vec(), workers)?;
        let answer = (|| {
            let context = self.optimizer(catalog)?;
            rudb_opt::optimize_with(&mut checks.plan, &context)?;
            let under =
                Under::new(self.budget(), context.facts(), seams, session, Rows::ForACaller);
            run(sql, &checks.plan, catalog, cancel, under)
        })();
        catalog.table_mut(name)?.put_back(before);
        let chunks = answer?.into_chunks();
        for (at, message) in checks.messages.iter().enumerate() {
            let failed = chunks.iter().any(|chunk| {
                (0..chunk.len()).any(|row| chunk.value_at(row, at) == Value::Boolean(true))
            });
            if failed {
                return Err(Error::constraint(message.clone()));
            }
        }
        Ok(())
    }

    /// Writes the rows of an `INSERT` that says what to do with a key the table already holds, and
    /// answers how many rows it inserted or updated and which, the updated ones first, which is the
    /// count and the order the pin gives.
    ///
    /// A `DO UPDATE` works its values out with a plan that reads the table and `excluded`. For the
    /// length of that plan the table holds just the rows that clash and a temporary table holds the
    /// new rows they clash with, one of each per clash and in the same order, so the plan pairs them
    /// by place. Both go back to how they were whether the plan ran or not.
    fn upsert(
        &self,
        sql: &str,
        catalog: &mut Catalog,
        (cancel, seams, session): (&Cancel, &rudb_seam::Settings, &Session),
        name: &QualifiedName,
        (conflict, checks): (rudb_bind::Conflict, Option<&mut rudb_bind::Checks>),
        chunks: Vec<Chunk>,
    ) -> Result<(usize, Vec<Chunk>)> {
        let workers = self.inner.pool.threads();
        let table = catalog.table(name)?;
        let types = table.types();
        let fields = table.columns().to_vec();
        let keys = table.keys().to_vec();
        let all: Vec<usize> = (0..fields.len()).collect();
        let mut stored = Vec::with_capacity(table.rows().chunk_count());
        for at in 0..table.rows().chunk_count() {
            stored.push(table.rows().read(at, &all)?);
        }
        let mut held = upsert::rows_of(&stored);
        let new = upsert::rows_of(&chunks);
        let arrivals = upsert::arrivals(&keys, conflict.key, &held, &new);
        let mut added = Vec::new();
        let mut clashes = Vec::new();
        for (row, arrival) in new.into_iter().zip(&arrivals) {
            match arrival {
                upsert::Arrival::New => added.push(row),
                upsert::Arrival::Held(at) => clashes.push((*at, row)),
                upsert::Arrival::Dropped => {}
            }
        }
        let mut updated = Vec::new();
        match conflict.action {
            rudb_bind::ConflictAction::Nothing => {}
            rudb_bind::ConflictAction::Replace(columns) => {
                for (at, row) in clashes {
                    for &column in &columns {
                        held[at][column] = row[column].clone();
                    }
                    updated.push(at);
                }
            }
            rudb_bind::ConflictAction::Update { columns, mut plan } if !clashes.is_empty() => {
                let matched: Vec<Vec<Value>> =
                    clashes.iter().map(|(at, _)| held[*at].clone()).collect();
                let incoming: Vec<Vec<Value>> =
                    clashes.iter().map(|(_, row)| row.clone()).collect();
                let excluded = QualifiedName::excluded();
                let loose =
                    fields.iter().map(|field| Field { not_null: false, ..field.clone() }).collect();
                catalog.create_table(excluded.clone(), loose)?;
                let answer = (|| {
                    let rows = upsert::chunks_of(&types, &incoming)?;
                    catalog.table_mut(&excluded)?.append_all(rows, workers)?;
                    let rows = upsert::chunks_of(&types, &matched)?;
                    let before = catalog.table_mut(name)?.stand_in(rows, workers)?;
                    let answer = (|| {
                        let context = self.optimizer(catalog)?;
                        rudb_opt::optimize_with(&mut plan, &context)?;
                        let under = Under::new(
                            self.budget(),
                            context.facts(),
                            seams,
                            session,
                            Rows::ForACaller,
                        );
                        run(sql, &plan, catalog, cancel, under)
                    })();
                    catalog.table_mut(name)?.put_back(before);
                    answer
                })();
                catalog.drop_table(&excluded)?;
                let values = upsert::rows_of(&answer?.into_chunks());
                for ((at, _), row) in clashes.iter().zip(&values) {
                    if row.last() != Some(&Value::Boolean(true)) {
                        continue;
                    }
                    for (&column, value) in columns.iter().zip(row) {
                        held[*at][column] = value.clone();
                    }
                    updated.push(*at);
                }
            }
            rudb_bind::ConflictAction::Update { .. } => {}
        }
        let count = updated.len() + added.len();
        let mut written: Vec<Vec<Value>> = updated.iter().map(|&at| held[at].clone()).collect();
        written.extend(added.iter().cloned());
        let rows = upsert::chunks_of(&types, &written)?;
        if let Some(checks) = checks {
            self.check(sql, catalog, (cancel, seams, session), name, checks, &rows)?;
        }
        foreign::missing(catalog, name, &rows)?;
        let table = catalog.table_mut(name)?;
        if updated.is_empty() {
            table.append_all(upsert::chunks_of(&types, &added)?, workers)?;
        } else {
            held.extend(added);
            table.replace_all(upsert::chunks_of(&types, &held)?, workers)?;
        }
        Ok((count, upsert::chunks_of(&types, &written)?))
    }
}

/// A query bound and then optimized, which is the plan that runs.
///
/// What [`Database::plan`] dumps, and it optimizes rather than stopping at the bound plan because a
/// dump of the bound plan next to a run of the optimized one would make the dump a description of
/// something nobody executes, which is the one thing a plan dump must not be. `EXPLAIN` and the
/// query path do the same two steps in that order for the same reason.
fn planned(
    sql: &str,
    catalog: &Catalog,
    context: &rudb_opt::pass::Context,
    session: &Session,
) -> Result<Plan> {
    let mut plan = rudb_bind::bind_sql_with(sql, catalog, session)?;
    rudb_opt::optimize_with(&mut plan, context)?;
    Ok(plan)
}

/// Whether the child's file holds a forward link for that relationship, built against that parent,
/// and whether every child row found a parent through it.
///
/// The same question `rudb_links()` answers in its stored columns, asked here for one relationship
/// at a time. `None` is no link, `Some(false)` is a link some of whose children matched nothing, and
/// `Some(true)` is both certificates of `spec/stats/07-graph-statistics.md` section 7.3. Totality is
/// read off the link rather than out of the degree section beside it, because the link counted the
/// children on its way to being written and a file from before that section existed still answers.
///
/// Both tables have to be in the same file, because a row id is a position in a table and a link
/// that named a parent in another file would only be resolvable by a reader that had both open and
/// had checked that neither had moved. The binding check inside `stored_link` is what catches a
/// parent that was rewritten since the link was built.
fn stored_link(catalog: &Catalog, child: (&str, &str), parent: (&str, &str)) -> Option<bool> {
    let child_table = table_named(catalog, child.0)?;
    let parent_table = table_named(catalog, parent.0)?;
    let (
        rudb_catalog::table::Rows::Native(child_rows),
        rudb_catalog::table::Rows::Native(parent_rows),
    ) = (child_table.rows(), parent_table.rows())
    else {
        return None;
    };
    let (Some(child_column), Some(parent_column)) =
        (child_table.column_index(child.1), parent_table.column_index(parent.1))
    else {
        return None;
    };
    let edge = Edge {
        child: child_table.name().table.clone(),
        child_column,
        parent: parent_table.name().table.clone(),
        parent_column,
    };
    let held = rudb_native::graph::stored_link(child_rows, parent_rows, &edge)?;
    Some(held.linked() == held.children())
}

/// The first table of that name in any schema of any database.
///
/// A relationship names a table and not a qualified name, because the `graph_links` grammar has no
/// dot in it and the tables of one benchmark are in one schema. A name that is ambiguous across two
/// databases resolves to the first, which is the order `duckdb_tables()` lists them in.
fn table_named<'a>(catalog: &'a Catalog, name: &str) -> Option<&'a rudb_catalog::Table> {
    catalog.tables().into_iter().find(|table| table.name().table.eq_ignore_ascii_case(name))
}

/// Builds and drains one plan, stopping if the token says to or if it runs out of memory.
///
/// The result is materialized, so it is charged, and the charge is handed to the result and
/// released when the result is dropped. That is what makes a program holding ten results at once
/// count as holding ten results: the limit is on the database and a result outlives the query.
///
/// This is also the one place a metrics document is made. Everything in it below the top level
/// comes out of the report the builder filled, and the two spans here are the two things only this
/// function knows: how long the tree took to build and how long it took to drain. Parsing, binding
/// and optimizing happened before this was called, so they are measured up there and arrive in
/// [`Under::planning`], and a caller that does not know says zero rather than guessing.
///
/// A query that fails part way through has a document too, and it is thrown away here, because an
/// error is a [`rudb_common::Error`] and that type is two ranks below the one the document lives
/// in. Carrying it out of a failure is worth doing and it is a change to how an error is reported
/// rather than a change to this function.
/// The two budgets a query draws on, which belong to the database rather than to the query.
///
/// They travel together because they are the same kind of thing. Memory is how much a query may
/// hold and the pool is how many threads it may run on, both are shared with whatever else the
/// database is doing at the same time, and neither is a property of the plan. Passing them as one
/// also keeps the argument lists of the functions below from growing a slot every time a new
/// resource turns up.
#[derive(Clone, Copy)]
struct Budget<'a> {
    memory: &'a Memory,
    pool: &'a Pool,
}

/// Who the rows a query produced are for, which is what decides whether they are flattened.
///
/// A caller outside the engine reads a value at a time and has never heard of a dictionary vector,
/// so a result going to one has every column copied into flat form first. A table has heard of it,
/// because storage holds the same vector forms execution does, so a result going into one keeps
/// whatever form the scan handed up.
///
/// The difference is not small. `CREATE TABLE t AS SELECT * FROM 'hits.parquet'` over a hundred and
/// five columns does its reading on every thread in the pool, and the string columns of that file
/// are dictionary encoded, so flattening them is a copy per row per column. Measured against duckdb
/// on a nine row group file, keeping the forms is the difference between getting 1.8 times out of
/// thirty two threads and getting 5.4.
///
/// Where the flattening happens matters as much as whether it happens. It is asked for once, before
/// the query runs, and the root sink does it as it queues each chunk, which is the worker thread
/// that produced it. It used to be done by the loop that drains the finished queue instead, and that
/// loop is one thread with every worker already joined: `SELECT` of four columns of six million rows
/// spent 103ms running and then 225ms flattening, and a single string column spent 68ms running and
/// then 2452ms in the drain. Per #1124.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rows {
    /// Going out of the engine, so every column is flattened on the way.
    ForACaller,
    /// Going into a table, so nothing is copied.
    ForATable,
}

/// What a statement spent before there was a plan to build, in nanoseconds.
///
/// Parsing, binding and optimizing all happen on the statement path, above the function that makes
/// the metrics document, so until this existed those three fields were zero in every document ever
/// written and `total_ns` was the physical build plus the run. Planning was the one cost of a query
/// that nothing could see.
///
/// That is the case milestone E1 asks for a column and an assertion about, and it states it as
/// plainly as it can be stated: a query that plans for four hundred milliseconds and runs for two
/// hundred is a query the optimizer made slower, and without a number nobody finds out, because
/// nobody profiles the planner. An optimizer only ever gets added to, every pass costs something to
/// run, and the pass that pays for itself on a scan of ten million rows does not pay for itself on
/// a point lookup.
///
/// Wall clock and not CPU. All three phases are single threaded, so the two are the same number up
/// to scheduling noise, and the wall clock is the one a caller waited.
#[derive(Clone, Copy, Default, Debug)]
struct Planning {
    parse_ns: u64,
    bind_ns: u64,
    optimize_ns: u64,
}

impl Planning {
    /// Everything before the physical build, which is what a budget is asserted against.
    fn total_ns(self) -> u64 {
        self.parse_ns.saturating_add(self.bind_ns).saturating_add(self.optimize_ns)
    }
}

/// Whether a bound query read any Parquet file that could have gone through a native mirror,
/// whatever became of the ask.
fn asked_for_mirrors(bound: &Bound) -> bool {
    matches!(bound, Bound::Query(plan) if !plan.wanted_mirrors().is_empty())
}

/// Run something and say how long it took, in wall nanoseconds.
///
/// Here so that the three phases are timed the same way rather than three ways, and so that adding
/// a span around a call that already existed does not also re-indent it. A failure is not timed,
/// because there is no document to put the number in and a partial phase is not a phase.
fn timed<T>(what: impl FnOnce() -> Result<T>) -> Result<(T, u64)> {
    let span = Span::start();
    let out = what()?;
    Ok((out, span.stop().0))
}

/// Everything a query runs under that is not the plan, the catalog or the cancel flag.
///
/// The same reasoning as [`Budget`], one level out. These travel together because every caller of
/// `run` has to say all of them and none is a property of the plan, and passing them as one keeps
/// the argument list from growing a slot every time something new turns out to be true of a running
/// query rather than of the query itself.
#[derive(Clone, Copy)]
struct Under<'a> {
    budget: Budget<'a>,
    facts: &'a rudb_opt::estimate::Facts,
    seams: &'a rudb_seam::Settings,
    session: &'a Session,
    going: Rows,
    planning: Planning,
}

impl<'a> Under<'a> {
    fn new(
        budget: Budget<'a>,
        facts: &'a rudb_opt::estimate::Facts,
        seams: &'a rudb_seam::Settings,
        session: &'a Session,
        going: Rows,
    ) -> Self {
        Self { budget, facts, seams, session, going, planning: Planning::default() }
    }

    /// What the statement path spent getting to this plan.
    ///
    /// Separate from [`Under::new`] and defaulting to zero, because not every path that runs a plan
    /// knows. A prepared statement parsed at `PREPARE` time and an `EXPLAIN ANALYZE` that is handed
    /// a plan somebody else optimized both run a query whose planning happened somewhere this
    /// cannot see, and a zero there says so. The alternative is a number one of those paths made up
    /// out of the part it did measure, which is the kind of thing a budget is later asserted
    /// against and nobody remembers is partly invented.
    fn after(mut self, planning: Planning) -> Self {
        self.planning = planning;
        self
    }
}

/// Direct counts, one average, and the three Q3 aggregates over an immutable native table can use
/// the cache. A filter is accepted only when its written form has no changing calls.
fn is_native_summary_aggregate(ast: &Ast, plan: &Plan, catalog: &Catalog) -> bool {
    let Node::Project { input, exprs, .. } = *plan.node(plan.root()) else { return false };
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(input) else {
        return false;
    };
    let projected = plan.expr_list(exprs);
    let aggregates = plan.expr_list(aggregates);
    if projected.is_empty()
        || projected.len() != aggregates.len()
        || !plan.expr_list(groups).is_empty()
        || !projected.iter().enumerate().all(|(position, expr)| {
            matches!(plan.expr(*expr), Expr::Column(column)
                if column.table == index && column.column as usize == position)
        })
    {
        return false;
    }
    let filtered = matches!(plan.node(input), Node::Filter { .. });
    let input = match *plan.node(input) {
        Node::Filter { input, .. } if simple_literal_filter(ast) => input,
        Node::Filter { .. } => return false,
        _ => input,
    };
    let Node::Get { catalog: source_catalog, schema, table, index: source_index, columns, .. } =
        *plan.node(input)
    else {
        return false;
    };
    let direct_aggregate = |expr, expected, arguments| {
        let Expr::Aggregate { name, args, distinct: false, filter: None } = plan.expr(expr) else {
            return false;
        };
        if plan.string(*name) != expected {
            return false;
        }
        let args = plan.expr_list(*args);
        args.len() == arguments
            && args.iter().all(|arg| {
                matches!(plan.expr(*arg), Expr::Column(column) if column.table == source_index)
            })
    };
    let supported = match aggregates {
        [aggregate] => {
            (direct_aggregate(*aggregate, "count_star", 0)
                && (filtered || plan.field_list(columns).is_empty()))
                || (!filtered && direct_aggregate(*aggregate, "avg", 1))
        }
        [sum, count, avg] if !filtered => {
            direct_aggregate(*sum, "sum", 1)
                && direct_aggregate(*count, "count_star", 0)
                && direct_aggregate(*avg, "avg", 1)
        }
        _ => false,
    };
    if !supported {
        return false;
    }
    let source =
        QualifiedName::new(plan.string(source_catalog), plan.string(schema), plan.string(table));
    catalog.table(&source).is_ok_and(|table| table.rows().is_native())
}

/// The filtered count case is intentionally narrower than all deterministic predicates. Checking
/// the written expression keeps a binder-folded `now()` or `random()` out of a reused plan.
fn simple_literal_filter(ast: &Ast) -> bool {
    let [ast::Statement::Query(reference)] = ast.statements.as_slice() else { return false };
    let query = ast.query(*reference);
    if !query.ctes.is_empty()
        || !query.order_by.is_empty()
        || query.order_by_all
        || query.limit != rudb_parse::NONE
        || query.offset != rudb_parse::NONE
    {
        return false;
    }
    let ast::QueryBody::Select(reference) = query.body else { return false };
    let select = ast.select(reference);
    if select.distinct != ast::Distinct::No
        || !select.group_by.is_empty()
        || select.group_by_all
        || select.filter == rudb_parse::NONE
        || select.having != rudb_parse::NONE
    {
        return false;
    }
    let [source] = ast.source_list(select.from) else { return false };
    if !matches!(ast.source(*source), ast::Source::Table { .. }) {
        return false;
    }
    let ast::Expr::Binary { op: ast::BinaryOp::NotEq, left, right } = ast.expr(select.filter)
    else {
        return false;
    };
    matches!(ast.expr(left), ast::Expr::Column { .. })
        && matches!(ast.expr(right), ast::Expr::Literal { kind: ast::LiteralKind::Number, .. })
}

fn run(
    sql: &str,
    plan: &Plan,
    catalog: &Catalog,
    cancel: &Cancel,
    under: Under<'_>,
) -> Result<QueryResult> {
    let Under { budget: Budget { memory, pool }, facts, seams, session, going, planning } = under;
    // The budget is shared by the database and its high-water mark survives a query. Reset it to
    // what is live now before measuring this execution, otherwise a metrics document either says
    // zero forever (when nobody copies the mark) or inherits the largest earlier query. A caller
    // with concurrent statements cannot attribute the shared budget to one query; this field is a
    // database-level peak in that case. The CLI benchmark path has one statement in flight.
    memory.forget_peak();
    let report = Report::new();
    let building = Span::start();
    let query = rudb_exec::build_measured(plan, catalog, cancel, memory, seams, session, &report)?;
    let (built_wall, built_cpu) = building.stop();
    if going == Rows::ForACaller {
        query.for_a_caller();
    }
    let names = query.schema().names();
    let types = query.schema().types();
    let mut held = memory.reservation();
    let mut chunks = Vec::new();
    // Every pipeline the query runs is timed against its own driver inside `run`, so what is left
    // for this span to say is how long the whole of the execution took, which is what `execute_ns`
    // is. The loop after it is the one that turns the queued chunks into a result set, and it is
    // inside the span because a caller waiting for rows is waiting for that too.
    let driving = Span::start();
    query.run(cancel, pool)?;
    while let Some(chunk) = query.next_chunk()? {
        if chunk.is_empty() {
            continue;
        }
        let chunk = match going {
            // flatten: this is the top of the query and the chunk is about to become a result set
            // that somebody outside the engine reads. A caller holding a `Result` gets a value at a
            // time, so a dictionary or a constant here would be a form every one of them has to
            // understand to read a row. The decode stops at this line and nothing below it sees a
            // flat column. The other arm is a chunk going into a table, where there is nobody
            // outside the engine to protect: storage holds the same forms execution does.
            //
            // The chunk is flat already, because `for_a_caller` above asked the root to flatten as
            // it queued and that happened on the worker that produced it. This line is what makes
            // that an optimisation rather than a promise kept in two places: a chunk that arrives
            // flat is moved through and a chunk that somehow does not is flattened here, the way
            // every chunk used to be. It used to be the whole cost of a large result, because this
            // loop is the one part of a parallel query that runs on a single thread, so the copy
            // made here was a copy the rest of the pool sat idle through.
            Rows::ForACaller => chunk.into_flat()?,
            Rows::ForATable => chunk,
        };
        held.grow(u64::try_from(chunk.footprint()).unwrap_or(u64::MAX))?;
        chunks.push(chunk);
    }
    let (ran_wall, ran_cpu) = driving.stop();
    let mut metrics = Document::new(sql);
    metrics.settings.memory_limit = memory.limit();
    metrics.settings.threads = u32::try_from(pool.threads()).unwrap_or(u32::MAX);
    metrics.timing.parse_ns = planning.parse_ns;
    metrics.timing.bind_ns = planning.bind_ns;
    metrics.timing.optimize_ns = planning.optimize_ns;
    metrics.timing.physical_ns = built_wall;
    metrics.timing.execute_ns = ran_wall;
    // Every phase and not the two this function timed itself. A total that left the planner out was
    // the reason planning time could grow without anything going up, and the harness reads this
    // field as the cost of the statement.
    metrics.timing.total_ns =
        planning.total_ns().saturating_add(built_wall).saturating_add(ran_wall);
    // The span above reads this thread's CPU clock, which is the only clock that says which thread
    // did the work and therefore the one clock that cannot see the workers. The query counted what
    // they burned as they finished, so it goes on here rather than going missing.
    let ran_cpu = ran_cpu.saturating_add(query.worker_cpu_ns());
    metrics.resource.cpu_ns = built_cpu.saturating_add(ran_cpu);
    metrics.resource.build_cpu_ns = built_cpu;
    metrics.resource.peak_bytes = memory.peak();
    report.fill(&mut metrics);
    rudb_opt::explain::record_estimates(plan, facts, &mut metrics);
    Ok(QueryResult::new(names, types, chunks, held).in_session(session.clone()).measured(metrics))
}

/// The plan `EXPLAIN` prints, run first if `ANALYZE` was asked for.
///
/// `ANALYZE` runs the query and throws the rows away. That is the whole difference between the two,
/// and it is deliberately the only difference: the plan that is printed is the plan that was built
/// and drained, so a number on a line came from the operator on that line rather than from an
/// operator something else would have built.
///
/// The rows are dropped rather than returned because the result set of `EXPLAIN ANALYZE` is the
/// plan. DuckDB does the same and calls the row `analyzed_plan`, and a client that gets a query's
/// rows back from an `EXPLAIN` has no way to tell which it asked for.
#[allow(clippy::too_many_arguments)]
fn explaining(
    plan: &Plan,
    catalog: &Catalog,
    cancel: &Cancel,
    budget: Budget<'_>,
    context: &rudb_opt::pass::Context,
    seams: rudb_opt::explain::Seams<'_>,
    session: &Session,
    asked: Asked,
    sql: &str,
    planning: Planning,
) -> Result<QueryResult> {
    let facts = context.facts();
    let statistics = asked.statistics();
    if !asked.analyze {
        let text = rudb_opt::explain::explain_with(plan, context, seams, statistics);
        return explained("logical_plan", &text);
    }
    // `EXPLAIN ANALYZE` is the one place a person reads these numbers with their own eyes rather
    // than through the harness, so the planning that produced the plan being printed has to reach
    // the document. It is the planning of the inner query and not of the `EXPLAIN`: the bind above
    // is what turned the statement into this plan and the optimize above is what ran on it.
    // Somebody is about to read a CPU number per operator with their own eyes, so this run is the
    // one that pays for them. The shim only reads the thread clock when the statement asked, and
    // asking is this setting, so the run is given a session that has it on. The copy goes away with
    // the rows, which means a connection that never turned profiling on does not find it turned on
    // after an `EXPLAIN ANALYZE`. The word is the same one `PRAGMA enable_profiling` writes.
    let mut profiled = session.clone();
    profiled.set("enable_profiling", "query_tree");
    let under =
        Under::new(budget, facts, seams.settings(), &profiled, Rows::ForACaller).after(planning);
    let result = run(sql, plan, catalog, cancel, under)?;
    let measured = result.metrics().expect("a query that ran reports what it did");
    let text = rudb_opt::explain::analyzed(plan, context, seams, measured, statistics);
    explained("analyzed_plan", &text)
}

/// The two flags an `EXPLAIN` can carry, which are the whole of what the options change.
///
/// One argument rather than two, because they arrive together, they are both booleans, and a pair
/// of bare booleans in a row at a call site is a pair somebody eventually swaps.
#[derive(Debug, Clone, Copy)]
struct Asked {
    /// Run the query as well, and print what happened next to what was expected.
    analyze: bool,
    /// Print what the planner knew: the use and the class behind every number in the plan.
    statistics: bool,
}

impl Asked {
    /// The statistics flag, in the words the printer takes it in.
    fn statistics(self) -> rudb_opt::explain::Statistics {
        if self.statistics {
            rudb_opt::explain::Statistics::Asked
        } else {
            rudb_opt::explain::Statistics::NotAsked
        }
    }
}

/// One row of two strings, which is the result set `EXPLAIN` hands back.
///
/// The column names and the shape are DuckDB's, `explain_key` and `explain_value`, because a
/// client reading a result set has to cope with whatever comes out and there is no reason to make
/// it cope with something new. The text in the second column is ours, since
/// `spec/12-duckdb-compat.md` section 12.5 excludes explain output from the guarantee.
///
/// One row rather than one per operator. DuckDB puts its whole tree in a single value and every
/// shell prints it as a block, and splitting it into rows would mean a shell's column width
/// deciding where a plan wraps.
fn explained(key: &str, text: &str) -> Result<QueryResult> {
    let key = Vector::from_values(LogicalType::Varchar, &[Value::Varchar(key.to_owned())])?;
    let value = Vector::from_values(LogicalType::Varchar, &[Value::Varchar(text.to_owned())])?;
    Ok(QueryResult::new(
        vec!["explain_key".to_owned(), "explain_value".to_owned()],
        vec![LogicalType::Varchar, LogicalType::Varchar],
        vec![Chunk::new(vec![key, value])?],
        Memory::unlimited().reservation(),
    ))
}

/// The `CREATE VIEW` half of a statement.
///
/// There is nothing to run. The body was bound by the binder to check that it can be, and what is
/// kept is the text, so this is the two modifiers and a catalog call.
fn create_view(create: rudb_bind::CreateView, catalog: &mut Catalog) -> Result<()> {
    if create.if_not_exists && catalog.entry(&create.name).is_ok() {
        return Ok(());
    }
    if create.or_replace && catalog.view(&create.name).is_ok() {
        catalog.drop_view(&create.name)?;
    }
    catalog.create_view(View::new(
        create.name,
        create.sql,
        create.statement,
        create.aliases,
        create.columns,
    ))
}

/// The `CREATE TABLE` half of a statement.
#[allow(clippy::too_many_arguments)]
fn create_table(
    sql: &str,
    mut create: rudb_bind::CreateTable,
    catalog: &mut Catalog,
    cancel: &Cancel,
    budget: Budget<'_>,
    context: &rudb_opt::pass::Context,
    seams: &rudb_seam::Settings,
    session: &Session,
) -> Result<()> {
    if create.if_not_exists && catalog.table(&create.name).is_ok() {
        return Ok(());
    }
    // The query runs before the old table is dropped, so `CREATE OR REPLACE TABLE t AS SELECT * FROM
    // t` reads the table it is about to replace rather than the empty new one.
    let rows = match &mut create.source {
        Some(plan) => {
            rudb_opt::optimize_with(plan, context)?;
            let under = Under::new(budget, context.facts(), seams, session, Rows::ForATable);
            Some(run(sql, plan, catalog, cancel, under)?)
        }
        None => None,
    };
    if create.or_replace && catalog.table(&create.name).is_ok() {
        catalog.drop_table(&create.name)?;
    }
    catalog.create_table(create.name.clone(), create.columns)?;
    if !create.keys.is_empty() {
        catalog.table_mut(&create.name)?.set_keys(create.keys)?;
    }
    if create.defaults.iter().any(Option::is_some) {
        catalog.table_mut(&create.name)?.set_defaults(create.defaults);
    }
    if !create.checks.is_empty() {
        catalog.table_mut(&create.name)?.set_checks(create.checks);
    }
    if !create.foreign.is_empty() {
        catalog.table_mut(&create.name)?.set_foreign(create.foreign);
    }
    if let Some(rows) = rows {
        catalog.table_mut(&create.name)?.append_all(rows.into_chunks(), budget.pool.threads())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::Duration;

    use rudb_common::Value;

    use rudb_io::{Filesystem, Op, OpenMode, SimFilesystem};

    use super::{
        Database, NativeExtremaValues, native_extrema_shape, native_nonzero_shape,
        native_simple_average_statement, native_simple_distinct_statement,
        native_simple_extrema_statement, native_simple_three_statement,
        native_single_average_shape, native_single_distinct_shape, native_three_aggregate_shape,
        publish,
    };

    #[test]
    fn grouped_counts_read_rows_instead_of_returning_a_saved_frequency_list() {
        let path =
            std::env::temp_dir().join(format!("rudb-grouped-rows-{}.rdb", std::process::id()));
        let name = path.to_str().unwrap();
        let database = Database::open(name).unwrap();
        database.execute("CREATE TABLE events (source_id SMALLINT)").unwrap();
        database.execute("INSERT INTO events VALUES (2), (2), (3), (0), (NULL)").unwrap();
        database.execute("CREATE TABLE wide_events (source_id BIGINT)").unwrap();
        database
            .execute("INSERT INTO wide_events VALUES (-10000), (-10000), (10000), (0), (NULL)")
            .unwrap();
        drop(database);

        let sql = "SELECT source_id, COUNT(*) FROM events WHERE source_id <> 0 GROUP BY source_id ORDER BY COUNT(*) DESC";
        assert!(Database::query_native_once(name, sql).unwrap().is_none());
        assert_eq!(
            Database::query_native_group_count_once(name, sql).unwrap(),
            Some(vec![(2, 2), (3, 1)])
        );
        assert_eq!(
            Database::query_native_group_count_once(
                name,
                "SELECT source_id, COUNT(*) FROM wide_events WHERE source_id <> 0 GROUP BY source_id ORDER BY COUNT(*) DESC"
            )
            .unwrap(),
            Some(vec![(-10000, 2), (10000, 1)])
        );
        assert!(
            Database::query_native_group_count_once(name, "SELECT COUNT(*) FROM events")
                .unwrap()
                .is_none()
        );
        for changed_sql in [
            "SELECT source_id, COUNT(*) FROM events WHERE source_id <> 1 GROUP BY source_id ORDER BY COUNT(*) DESC",
            "SELECT source_id, COUNT(*) FROM events WHERE source_id <> 0 GROUP BY source_id ORDER BY source_id DESC",
            "SELECT source_id, COUNT(*) FROM events WHERE source_id <> 0 GROUP BY source_id ORDER BY COUNT(*) DESC LIMIT 1",
        ] {
            assert!(Database::query_native_group_count_once(name, changed_sql).unwrap().is_none());
        }
        let database = Database::open(name).unwrap();
        let rows = database.query(sql).unwrap().rows().collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                vec![Value::SmallInt(2), Value::BigInt(2)],
                vec![Value::SmallInt(3), Value::BigInt(1)]
            ]
        );
        drop(database);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cold_extrema_shape_accepts_only_direct_bounds() {
        let parsed =
            rudb_parse::parse_ast("SELECT MIN(EventDate), MAX(EventDate) FROM hits").unwrap();
        assert_eq!(
            native_extrema_shape(&parsed),
            Some(("hits", "EventDate", ["min(EventDate)".into(), "max(EventDate)".into()]))
        );
        for sql in [
            "SELECT MIN(EventDate), MAX(EventDate) FROM hits WHERE EventDate > 0",
            "SELECT MIN(EventDate), MAX(EventDate) FROM hits LIMIT 1",
            "SELECT MIN(EventDate + 1), MAX(EventDate) FROM hits",
            "SELECT MIN(EventDate), MAX(OtherDate) FROM hits",
            "SELECT MIN(DISTINCT EventDate), MAX(EventDate) FROM hits",
        ] {
            let parsed = rudb_parse::parse_ast(sql).unwrap();
            assert_eq!(native_extrema_shape(&parsed), None, "{sql}");
        }
    }

    #[test]
    fn simple_extrema_statement_rejects_other_sql() {
        assert_eq!(
            native_simple_extrema_statement(" SELECT MIN(EventDate), MAX(EventDate) FROM hits; "),
            Some(("hits", "EventDate"))
        );
        for sql in [
            "SELECT MIN(EventDate), MAX(EventDate) FROM hits WHERE EventDate > 0",
            "SELECT MIN(EventDate), MAX(OtherDate) FROM hits",
            "SELECT MIN(EventDate + 1), MAX(EventDate) FROM hits",
            "SELECT MIN(EventDate), MAX(EventDate) FROM hits; SELECT 1",
            "SELECT MIN(EventDate), MAX(EventDate) FROM hits GROUP BY RegionID",
        ] {
            assert_eq!(native_simple_extrema_statement(sql), None, "{sql}");
        }
    }

    #[test]
    fn cold_extrema_matches_regular_execution_for_dates_and_nulls() {
        let path = std::env::temp_dir().join(format!("rudb-q7-{}.rdb", std::process::id()));
        let name = path.to_str().unwrap();
        let database = Database::open(name).unwrap();
        database.execute("CREATE TABLE hits (EventDate DATE)").unwrap();
        database
            .execute("INSERT INTO hits VALUES (DATE '2013-07-31'), (NULL), (DATE '2013-07-02')")
            .unwrap();
        database.execute("CREATE TABLE small_hits (EventDate USMALLINT)").unwrap();
        database.execute("INSERT INTO small_hits VALUES (15917), (15888), (NULL)").unwrap();
        database.execute("CREATE TABLE empty_hits (EventDate DATE)").unwrap();
        database.execute("CREATE TABLE null_hits (EventDate DATE)").unwrap();
        database.execute("INSERT INTO null_hits VALUES (NULL)").unwrap();
        let cases = [
            "SELECT MIN(EventDate), MAX(EventDate) FROM hits",
            "SELECT MIN(EventDate), MAX(EventDate) FROM small_hits",
            "SELECT MIN(EventDate), MAX(EventDate) FROM empty_hits",
            "SELECT MIN(EventDate), MAX(EventDate) FROM null_hits",
        ];
        let expected = cases.map(|sql| database.query(sql).unwrap().rows().collect::<Vec<_>>());
        drop(database);
        for (sql, expected) in cases.into_iter().zip(expected) {
            let actual = Database::query_native_once(name, sql).unwrap().unwrap();
            assert_eq!(actual.rows().collect::<Vec<_>>(), expected, "{sql}");
        }
        assert_eq!(
            Database::query_native_extrema_values_once(
                name,
                "SELECT MIN(EventDate), MAX(EventDate) FROM hits"
            )
            .unwrap(),
            Some(NativeExtremaValues::Date { low: 15888, high: 15917 })
        );
        assert_eq!(
            Database::query_native_extrema_values_once(
                name,
                "SELECT MIN(EventDate), MAX(EventDate) FROM small_hits"
            )
            .unwrap(),
            Some(NativeExtremaValues::Integer { low: 15888, high: 15917 })
        );
        for table in ["empty_hits", "null_hits"] {
            let sql = format!("SELECT MIN(EventDate), MAX(EventDate) FROM {table}");
            assert_eq!(Database::query_native_extrema_values_once(name, &sql).unwrap(), None);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cold_distinct_shape_accepts_only_a_direct_count() {
        let parsed = rudb_parse::parse_ast("SELECT COUNT(DISTINCT UserID) FROM hits").unwrap();
        assert_eq!(
            native_single_distinct_shape(&parsed),
            Some(("hits", "UserID", "count(DISTINCT UserID)".into()))
        );
        for sql in [
            "SELECT COUNT(DISTINCT UserID) FROM hits WHERE UserID > 0",
            "SELECT COUNT(DISTINCT UserID + 1) FROM hits",
            "SELECT COUNT(UserID) FROM hits",
            "SELECT COUNT(DISTINCT UserID) FROM hits LIMIT 1",
            "SELECT COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID",
        ] {
            let parsed = rudb_parse::parse_ast(sql).unwrap();
            assert_eq!(native_single_distinct_shape(&parsed), None, "{sql}");
        }
    }

    #[test]
    fn simple_distinct_statement_rejects_other_sql() {
        assert_eq!(
            native_simple_distinct_statement(" SELECT COUNT(DISTINCT UserID) FROM hits; "),
            Some(("hits", "UserID"))
        );
        for sql in [
            "SELECT COUNT(DISTINCT UserID) FROM hits WHERE UserID > 0",
            "SELECT COUNT(DISTINCT UserID + 1) FROM hits",
            "SELECT COUNT(UserID) FROM hits",
            "SELECT COUNT(DISTINCT UserID) FROM hits; SELECT 1",
            "SELECT COUNT(DISTINCT UserID) FROM hits GROUP BY RegionID",
            "SELECT COUNT(DISTINCT UserID) FROM hits; ;",
        ] {
            assert_eq!(native_simple_distinct_statement(sql), None, "{sql}");
        }
    }

    #[test]
    fn cold_distinct_matches_regular_execution_with_nulls() {
        let path = std::env::temp_dir().join(format!("rudb-q5-{}.rdb", std::process::id()));
        let name = path.to_str().unwrap();
        let database = Database::open(name).unwrap();
        database.execute("CREATE TABLE hits (UserID BIGINT)").unwrap();
        database.execute("INSERT INTO hits VALUES (1), (2), (1), (NULL)").unwrap();
        database.execute("CREATE TABLE empty_hits (UserID BIGINT)").unwrap();
        database.execute("CREATE TABLE null_hits (UserID BIGINT)").unwrap();
        database.execute("INSERT INTO null_hits VALUES (NULL)").unwrap();
        let cases = [
            "SELECT COUNT(DISTINCT UserID) FROM hits",
            "SELECT COUNT(DISTINCT UserID) FROM empty_hits",
            "SELECT COUNT(DISTINCT UserID) FROM null_hits",
        ];
        let expected = cases.map(|sql| database.query(sql).unwrap().rows().collect::<Vec<_>>());
        drop(database);
        for (sql, expected) in cases.into_iter().zip(expected) {
            let actual = Database::query_native_once(name, sql).unwrap().unwrap();
            assert_eq!(actual.rows().collect::<Vec<_>>(), expected, "{sql}");
        }
        assert_eq!(
            Database::query_native_distinct_value_once(
                name,
                "SELECT COUNT(DISTINCT UserID) FROM hits"
            )
            .unwrap(),
            Some(2)
        );
        for sql in [
            "SELECT COUNT(DISTINCT UserID) FROM empty_hits",
            "SELECT COUNT(DISTINCT UserID) FROM null_hits",
        ] {
            assert_eq!(Database::query_native_distinct_value_once(name, sql).unwrap(), Some(0));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cold_average_shape_accepts_only_a_direct_aggregate() {
        let parsed = rudb_parse::parse_ast("SELECT AVG(UserID) FROM hits").unwrap();
        assert_eq!(
            native_single_average_shape(&parsed),
            Some(("hits", "UserID", "avg(UserID)".into()))
        );
        let aliased = rudb_parse::parse_ast("SELECT AVG(UserID) AS mean_user FROM hits").unwrap();
        assert_eq!(
            native_single_average_shape(&aliased),
            Some(("hits", "UserID", "mean_user".into()))
        );
        for sql in [
            "SELECT AVG(UserID) FROM hits WHERE UserID > 0",
            "SELECT AVG(DISTINCT UserID) FROM hits",
            "SELECT AVG(UserID + 1) FROM hits",
            "SELECT AVG(UserID) FROM hits LIMIT 1",
            "SELECT AVG(UserID) FROM hits GROUP BY RegionID",
        ] {
            let parsed = rudb_parse::parse_ast(sql).unwrap();
            assert_eq!(native_single_average_shape(&parsed), None, "{sql}");
        }
    }

    #[test]
    fn simple_average_statement_rejects_other_sql() {
        assert_eq!(
            native_simple_average_statement(" SELECT AVG(UserID) FROM hits; "),
            Some(("hits", "UserID"))
        );
        for sql in [
            "SELECT AVG(UserID) FROM hits WHERE UserID > 0",
            "SELECT AVG(DISTINCT UserID) FROM hits",
            "SELECT AVG(UserID + 1) FROM hits",
            "SELECT AVG(UserID) FROM hits; SELECT 1",
            "SELECT AVG(UserID) FROM hits GROUP BY RegionID",
            "SELECT AVG(UserID) FROM hits -- comment",
            "SELECT AVG(UserID) FROM hits; ;",
        ] {
            assert_eq!(native_simple_average_statement(sql), None, "{sql}");
        }
    }

    #[test]
    fn cold_average_matches_regular_execution_for_bigints_and_nulls() {
        let path = std::env::temp_dir().join(format!("rudb-q4-{}.rdb", std::process::id()));
        let name = path.to_str().unwrap();
        let database = Database::open(name).unwrap();
        database.execute("CREATE TABLE hits (UserID BIGINT)").unwrap();
        database
            .execute("INSERT INTO hits VALUES (2414420660257356000), (2534231104689841000), (NULL)")
            .unwrap();
        database.execute("CREATE TABLE empty_hits (UserID BIGINT)").unwrap();
        database.execute("CREATE TABLE null_hits (UserID BIGINT)").unwrap();
        database.execute("INSERT INTO null_hits VALUES (NULL)").unwrap();
        let cases = [
            "SELECT AVG(UserID) FROM hits",
            "SELECT AVG(UserID) FROM empty_hits",
            "SELECT AVG(UserID) FROM null_hits",
        ];
        let expected = cases.map(|sql| database.query(sql).unwrap().rows().collect::<Vec<_>>());
        drop(database);
        for (sql, expected) in cases.into_iter().zip(expected) {
            let actual = Database::query_native_once(name, sql).unwrap().unwrap();
            assert_eq!(actual.rows().collect::<Vec<_>>(), expected, "{sql}");
        }
        assert_eq!(
            Database::query_native_average_value_once(name, "SELECT AVG(UserID) FROM hits")
                .unwrap(),
            Some((2414420660257356000_i128 + 2534231104689841000_i128) as f64 / 2.0)
        );
        for sql in ["SELECT AVG(UserID) FROM empty_hits", "SELECT AVG(UserID) FROM null_hits"] {
            assert_eq!(Database::query_native_average_value_once(name, sql).unwrap(), None);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cold_three_aggregate_shape_accepts_only_the_certified_query() {
        let parsed = rudb_parse::parse_ast(
            "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits",
        )
        .unwrap();
        assert_eq!(
            native_three_aggregate_shape(&parsed),
            Some((
                "hits",
                "AdvEngineID",
                "ResolutionWidth",
                ["sum(AdvEngineID)".into(), "count_star()".into(), "avg(ResolutionWidth)".into()]
            ))
        );
        for sql in [
            "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits WHERE AdvEngineID > 0",
            "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits LIMIT 1",
            "SELECT SUM(DISTINCT AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits",
            "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits GROUP BY RegionID",
        ] {
            let parsed = rudb_parse::parse_ast(sql).unwrap();
            assert_eq!(native_three_aggregate_shape(&parsed), None, "{sql}");
        }
    }

    #[test]
    fn simple_three_statement_uses_requested_columns_and_rejects_other_clauses() {
        assert_eq!(
            native_simple_three_statement(
                " SELECT SUM(Points), COUNT(*), AVG(Width) FROM measurements; "
            ),
            Some(("measurements", "Points", "Width"))
        );
        for sql in [
            "SELECT SUM(Points), COUNT(*), AVG(Width) FROM measurements WHERE Points > 0",
            "SELECT SUM(DISTINCT Points), COUNT(*), AVG(Width) FROM measurements",
            "SELECT SUM(Points), COUNT(*), AVG(Width) FROM measurements GROUP BY Width",
            "SELECT SUM(Points), COUNT(*), AVG(Width) FROM measurements LIMIT 1",
            "SELECT SUM(Points), COUNT(*), AVG(Width) FROM measurements; SELECT 1",
            "SELECT SUM(Points), COUNT(*), AVG(Width + 1) FROM measurements",
            "SELECT SUM(select), COUNT(*), AVG(Width) FROM measurements",
            "SELECT SUM(Points), COUNT(*), AVG(Width) FROM from",
        ] {
            assert_eq!(native_simple_three_statement(sql), None, "{sql}");
        }
    }

    #[test]
    fn cold_three_aggregate_answers_from_native_catalog() {
        let path = std::env::temp_dir().join(format!("rudb-q3-{}.rdb", std::process::id()));
        let name = path.to_str().unwrap();
        let database = Database::open(name).unwrap();
        database
            .execute("CREATE TABLE hits (AdvEngineID SMALLINT, ResolutionWidth SMALLINT)")
            .unwrap();
        database.execute("INSERT INTO hits VALUES (1, 100), (NULL, 200), (3, NULL)").unwrap();
        database
            .execute("CREATE TABLE empty_hits (AdvEngineID SMALLINT, ResolutionWidth SMALLINT)")
            .unwrap();
        database
            .execute("CREATE TABLE null_hits (AdvEngineID SMALLINT, ResolutionWidth SMALLINT)")
            .unwrap();
        database.execute("INSERT INTO null_hits VALUES (NULL, NULL)").unwrap();
        database.execute("CREATE TABLE measurements (Points SMALLINT, Width SMALLINT)").unwrap();
        database.execute("INSERT INTO measurements VALUES (2, 10), (5, 20)").unwrap();
        drop(database);
        let result = Database::query_native_once(
            name,
            "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            result.rows().collect::<Vec<_>>(),
            vec![vec![Value::HugeInt(4), Value::BigInt(3), Value::Double(150.0)]]
        );
        assert_eq!(
            Database::query_native_three_values_once(
                name,
                "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits",
            )
            .unwrap(),
            Some((4, 3, 150.0))
        );
        assert_eq!(
            Database::query_native_three_values_once(
                name,
                "SELECT SUM(Points), COUNT(*), AVG(Width) FROM measurements",
            )
            .unwrap(),
            Some((7, 2, 15.0))
        );
        assert_eq!(
            Database::query_native_three_values_once(
                name,
                "SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM hits; SELECT 1",
            )
            .unwrap(),
            None
        );
        for (table, count) in [("empty_hits", 0), ("null_hits", 1)] {
            let sql =
                format!("SELECT SUM(AdvEngineID), COUNT(*), AVG(ResolutionWidth) FROM {table}");
            let result = Database::query_native_once(name, &sql).unwrap().unwrap();
            assert_eq!(
                result.rows().collect::<Vec<_>>(),
                vec![vec![Value::Null, Value::BigInt(count), Value::Null]]
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn cold_count_shape_accepts_only_the_certified_query() {
        let parsed = rudb_parse::parse_ast("SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0")
            .expect("query parses");
        assert_eq!(native_nonzero_shape(&parsed), Some(("hits", "AdvEngineID", "count_star()")));
        for sql in [
            "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 1",
            "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0 LIMIT 1",
            "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0 AND RegionID = 1",
            "SELECT COUNT(DISTINCT AdvEngineID) FROM hits WHERE AdvEngineID <> 0",
        ] {
            let parsed = rudb_parse::parse_ast(sql).expect("query parses");
            assert_eq!(native_nonzero_shape(&parsed), None, "{sql}");
        }
    }

    #[test]
    fn cold_nonzero_csv_count_uses_column_statistics() {
        let path = std::env::temp_dir().join(format!("rudb-q2-csv-{}.rdb", std::process::id()));
        let name = path.to_str().unwrap();
        let database = Database::open(name).unwrap();
        database.execute("CREATE TABLE hits (AdvEngineID SMALLINT)").unwrap();
        database.execute("INSERT INTO hits VALUES (0), (0), (NULL), (2), (-3)").unwrap();
        database.execute("CREATE TABLE events (engine INTEGER)").unwrap();
        database.execute("INSERT INTO events VALUES (0), (8), (NULL)").unwrap();
        drop(database);
        let sql = "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 0";
        assert_eq!(Database::query_native_nonzero_value_once(name, sql).unwrap(), Some(2));
        assert_eq!(
            Database::query_native_nonzero_value_once(
                name,
                "select count(*) from events where engine <> 0;"
            )
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            Database::query_native_nonzero_value_once(
                name,
                "SELECT COUNT(*) FROM hits WHERE AdvEngineID <> 1"
            )
            .unwrap(),
            None
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn publishing_syncs_the_directory_after_the_rename() {
        let fs = SimFilesystem::new();
        fs.create_dir_all(Path::new("/data")).unwrap();
        let file = fs.open(Path::new("/data/db.7.tmp"), OpenMode::CreateNew).unwrap();
        file.write_at(0, b"new").unwrap();
        file.sync().unwrap();
        fs.clear_log();

        publish(&fs, Path::new("/data/db.7.tmp"), Path::new("/data/db")).unwrap();

        let ops = fs.ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert!(matches!(ops[0], Op::Rename { .. }), "{ops:?}");
        assert_eq!(ops[1], Op::SyncDir { path: "/data".into() });
        assert_eq!(fs.contents(Path::new("/data/db")).unwrap(), b"new".to_vec());
    }

    #[test]
    fn a_query_runs_while_a_load_into_a_new_table_does() {
        let path = std::env::temp_dir().join(format!(
            "rudb-load-lock-{}-{}.rdb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock advances")
                .as_nanos()
        ));
        let database = Database::open(path.to_str().expect("a UTF-8 temporary path")).unwrap();
        database.execute("CREATE TABLE small AS SELECT 7 AS a").unwrap();
        let (told, loading) = mpsc::channel();
        *database.shared.inner.loading.lock().unwrap() = Some(told);

        // An aggregate over a range that does not end in any time a test has, so the load is still
        // running when the query goes in, and it hands the sink nothing until it is stopped.
        let connection = database.connect();
        let stopper = connection.clone();
        let load = std::thread::spawn(move || {
            connection.execute("CREATE TABLE big AS SELECT count(*) AS n FROM range(100000000000)")
        });
        loading.recv_timeout(Duration::from_secs(60)).expect("the load let go of the catalog");

        let reader = database.clone();
        let (answered, answer) = mpsc::channel();
        std::thread::spawn(move || {
            let rows = reader.query("SELECT a FROM small").map(|result| result.rows().collect());
            let _ = answered.send(rows);
        });
        let got = answer.recv_timeout(Duration::from_secs(20));
        stopper.interrupt();
        let loaded = load.join().expect("the load thread ran");

        let rows: Vec<Vec<Value>> =
            got.expect("the query finished while the load ran").expect("the query succeeded");
        assert_eq!(rows, vec![vec![Value::Integer(7)]]);
        assert!(loaded.is_err(), "the load was interrupted");
        assert!(
            database.query("SELECT * FROM big").is_err(),
            "an interrupted load leaves no table"
        );
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_bare_file_name_lives_in_the_current_directory() {
        assert_eq!(super::directory_of(Path::new("db")), Path::new("."));
        assert_eq!(super::directory_of(Path::new("/data/db")), Path::new("/data"));
        assert_eq!(super::directory_of(Path::new("a/db")), Path::new("a"));
    }

    #[test]
    fn two_statements_over_one_catalog_plan_from_the_same_counts() {
        // The counts used to be rebuilt for every statement, which on a catalog of two hundred
        // tables of twenty columns was about forty five microseconds a statement spent walking tables
        // the query never named. What stops that is the catalog's generation: the second statement
        // finds the version it read at still current and plans from the set the first one built.
        let database = Database::new();
        database.execute("CREATE TABLE t (a INTEGER)").expect("a table");
        database.execute("INSERT INTO t VALUES (1), (2), (3)").expect("three rows");
        let shared = &database.shared;

        let first = shared.facts(&shared.read());
        let again = shared.facts(&shared.read());
        assert!(std::sync::Arc::ptr_eq(&first, &again), "nothing changed, so nothing was rebuilt");

        // And a write moves the generation on, so the next statement does not plan against the
        // size the table used to be. Serving one number for two different catalogs is the failure
        // this has to avoid, and it is the reason the count moves for anything that took the
        // catalog by mutable reference rather than for the ones that really wrote something.
        database.execute("INSERT INTO t VALUES (4)").expect("a fourth row");
        let after = shared.facts(&shared.read());
        assert!(!std::sync::Arc::ptr_eq(&first, &after), "the catalog changed under it");
        assert!(after.generation() > first.generation(), "and it says which version it is");

        let rows = |facts: &rudb_opt::estimate::Facts| {
            facts.get(&rudb_opt::estimate::Key::Rows {
                catalog: "memory",
                schema: "main",
                table: "t",
            })
        };
        assert_eq!(rows(&first), rudb_common::Stat::exact(3, rudb_common::Provenance::RowCount));
        assert_eq!(rows(&after), rudb_common::Stat::exact(4, rudb_common::Provenance::RowCount));
    }

    /// Every column of a result set is flat, whatever form the operators that made it produced.
    ///
    /// The promise a caller outside the engine reads a value at a time relies on. It is kept in the
    /// root sink now rather than in the loop that drains it, and the reason it is worth a test of
    /// its own is that the place it is kept moved: a string column of a stored table comes out of
    /// the scan as views over a shared arena, and a caller that was handed one would be reading a
    /// form nothing outside this workspace knows about.
    #[test]
    fn every_column_of_a_result_reaches_the_caller_flat() {
        use rudb_vector::Form;

        let database = Database::new();
        database.execute("CREATE TABLE t (a INTEGER, s VARCHAR)").expect("a table");
        database
            .execute("INSERT INTO t VALUES (1, 'a long string that will not fit inline'), (2, 'b')")
            .expect("two rows");
        let result = database.query("SELECT a, s, s || 'x' AS j FROM t WHERE a > 0").expect("runs");
        assert_eq!(result.len(), 2);
        for chunk in result.chunk_iter() {
            for (at, column) in chunk.columns().iter().enumerate() {
                assert_eq!(column.form(), Form::Flat, "column {at} came out encoded");
            }
        }
    }

    /// The two certificates of `spec/stats/07-graph-statistics.md` section 7.3, as the optimizer
    /// receives them.
    ///
    /// `rudb_links()` already shows both in its own words, and this asks the same file the same
    /// question through the path a rewrite reads. The reason it is worth a second test is that the
    /// two paths get their answer from different places: the table reads the degree section, and
    /// this reads the link, so a file written before that section existed answers here and not
    /// there. Reading them off different things is the point, not an oversight, because the thing a
    /// rewrite relies on is the structure it is about to use rather than a summary beside it.
    #[test]
    fn a_relationship_carries_both_certificates_only_when_every_child_row_found_a_parent() {
        use super::Shared;

        let path = std::env::temp_dir().join(format!(
            "rudb-certificates-{}-{}.rdb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock advances")
                .as_nanos()
        ));
        let database =
            Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("a file");
        database.execute("CREATE TABLE customer (c_custkey INTEGER)").unwrap();
        database.execute("CREATE TABLE orders (o_custkey INTEGER)").unwrap();
        database.execute("CREATE TABLE returns (r_custkey INTEGER)").unwrap();
        database.execute("CREATE TABLE zones (z_key INTEGER)").unwrap();
        database.execute("CREATE TABLE visits (v_zone INTEGER)").unwrap();
        database.execute("INSERT INTO customer SELECT i FROM range(1, 4001) AS r(i)").unwrap();
        // Three orders per customer, every one of them a customer that exists.
        database
            .execute("INSERT INTO orders SELECT 1 + (i - 1) / 3 FROM range(1, 10001) AS r(i)")
            .unwrap();
        // The same, plus one row whose key is past the last customer, which is all it takes.
        database
            .execute("INSERT INTO returns SELECT 1 + (i - 1) / 3 FROM range(1, 10001) AS r(i)")
            .unwrap();
        database.execute("INSERT INTO returns VALUES (9999)").unwrap();
        // A parent key that repeats, which is not a key, so the build writes no link at all and
        // the declaration arrives with neither certificate.
        database
            .execute("INSERT INTO zones SELECT 1 + i % 500 FROM range(0, 1000) AS r(i)")
            .unwrap();
        database
            .execute("INSERT INTO visits SELECT 1 + i % 500 FROM range(0, 2000) AS r(i)")
            .unwrap();
        let declared = "orders(o_custkey) -> customer(c_custkey), \
                        returns(r_custkey) -> customer(c_custkey), \
                        visits(v_zone) -> zones(z_key)";
        database.execute(&format!("SET graph_links = '{declared}'")).unwrap();
        database.execute("CHECKPOINT").unwrap();

        let shared = &database.shared;
        let found = Shared::related(&shared.read(), declared);
        let certificates: Vec<(&str, bool, bool)> =
            found.iter().map(|link| (link.child.as_str(), link.built, link.total)).collect();
        assert_eq!(
            certificates,
            vec![("orders", true, true), ("returns", true, false), ("visits", false, false)],
            "one unmatched child row costs the relationship its totality and not its link"
        );

        drop(database);
        std::fs::remove_file(&path).ok();
    }
}
