//! A statement parsed once and run many times, with values for its parameters.

use std::sync::{Arc, Mutex, PoisonError};

use rudb_bind::Parameters;
use rudb_catalog::QualifiedName;
use rudb_common::{
    DeclaredType, Error, Field, IdentifierCompare, LogicalType, Origin, Result, Value,
};
use rudb_parse::ast::{self, Ast};

use crate::connection::single;
use crate::database::Shared;
use crate::result::QueryResult;

/// What [`Prepared::describe`] found.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    /// The type of each parameter, in the order of [`Prepared::parameters`], or `None` where
    /// nothing in the statement settles it.
    pub parameters: Vec<Option<LogicalType>>,
    /// The PostgreSQL type of each parameter, in the same order, where a cast or the declaration
    /// of a column wrote one, such as `varchar(10)`.
    pub written: Vec<Option<DeclaredType>>,
    /// The columns the statement answers, or `None` for a statement that answers no rows.
    pub fields: Option<Vec<Field>>,
    /// The table column that each of `fields` reads with no change, where there is one.
    pub origins: Vec<Option<Origin>>,
    /// The error that planning a query of a PostgreSQL session with no parameters raises, from
    /// the folding of its constant parts. PostgreSQL sends it at `Bind`.
    pub planning: Option<Error>,
}

/// A prepared statement.
///
/// The statement is parsed here and bound at each execution, with the values in hand. That is the
/// opposite of the usual arrangement, where a prepared statement is planned once and the values are
/// pushed into the plan, and it is on purpose for now: an analytical query is planned against the
/// data it reads, so a plan built without knowing that `$1` is `1` or `1000000` is a plan built
/// blind. Parsing is the part that is pure overhead, and that happens once.
///
/// What a parameter can be written as is DuckDB's list: `?` numbered by where it is, `?1` and `$1`
/// numbered by hand, and `$name`. A parameter used twice is one parameter, because it is one value
/// to provide.
///
/// ```
/// use rudb::Database;
/// use rudb_common::Value;
///
/// let db = Database::new();
/// db.execute("CREATE TABLE t (a INTEGER)")?;
/// db.execute("INSERT INTO t VALUES (1), (2), (3)")?;
///
/// let counted = db.prepare("SELECT count(*) FROM t WHERE a > ?")?;
/// assert_eq!(counted.value(&[Value::Integer(1)])?, Value::BigInt(2));
/// assert_eq!(counted.value(&[Value::Integer(2)])?, Value::BigInt(1));
/// # Ok::<(), rudb_common::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct Prepared {
    shared: Shared,
    sql: String,
    ast: Ast,
    names: Vec<String>,
    short: Short,
    /// Whether the parameters are `1` to `n` and nothing else, so that `n` values by position are
    /// exactly the values the statement wants, with nothing missing and nothing left over.
    numbered: bool,
    /// The last description, with the stamp and the declared types it was found with. A client of
    /// the extended protocol asks for one before each execution.
    described: Arc<Mutex<Option<Described>>>,
    /// The PostgreSQL type that the client declared for each parameter, in the order of `names`.
    types: Vec<Option<DeclaredType>>,
}

/// A description that [`Prepared::describe`] keeps.
#[derive(Debug)]
struct Described {
    stamp: (u64, u64, rudb_common::Session),
    declared: Vec<Option<LogicalType>>,
    description: Description,
}

/// An `INSERT INTO t [(columns)] VALUES (row), ...` whose items are parameters or `NULL`, with
/// no `ON CONFLICT` after the rows and a `RETURNING` of columns at most.
///
/// This is the trickle insert, one row or a few per statement, and binding it builds a plan of a
/// projection over a `VALUES` only for the executor to walk it back down to the rows. So the shape is
/// read once here, and an execution that finds a plain table under the name puts the row straight
/// in. A column the statement leaves out gets its default here when the default is a constant, a
/// `nextval` or the time of the transaction. Anything else the shape does not settle by itself, a
/// constraint, another default or a value that needs more than a widening to fit its column, goes
/// the long way, so the errors and the answers are the ones the plan gives. A `RETURNING` of the
/// table's columns gives them from the rows as they went in.
#[derive(Debug, Clone)]
pub(crate) struct Direct {
    /// The table's name, as it was written.
    pub(crate) name: Vec<String>,
    /// The column list, empty when the statement did not write one.
    pub(crate) columns: Vec<String>,
    /// The rows, each one item for each column the statement names.
    pub(crate) rows: Vec<Vec<Item>>,
    /// The `RETURNING` list, read as a [`Lookup`] that sets nothing equal.
    pub(crate) returning: Option<Lookup>,
    /// How the session that read the statement compares a column name with a name of the table.
    pub(crate) compare: IdentifierCompare,
    /// What the last execution worked out about the table, kept while the catalog stays as it was.
    pub(crate) found: Found,
}

/// The table a [`Direct`] insert found and where its items land, with the catalog generation it
/// was found at.
///
/// Resolving the name and matching the column list cost more than putting the row in, and neither
/// changes until the catalog does. The insert itself moves the generation on, so it is the
/// generation after the insert that is kept.
#[derive(Debug, Default)]
pub(crate) struct Found(Mutex<Option<(u64, QualifiedName, Targets)>>);

impl Found {
    /// The name and targets found at `generation`, taken out so the caller can hand them back.
    pub(crate) fn take(&self, generation: u64) -> Option<(QualifiedName, Targets)> {
        let mut found = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match found.take() {
            Some((at, name, targets)) if at == generation => Some((name, targets)),
            _ => None,
        }
    }

    /// Keeps `name` and `targets` as what the catalog holds at `generation`.
    pub(crate) fn keep(&self, generation: u64, name: QualifiedName, targets: Targets) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((generation, name, targets));
    }
}

/// Where the items of a [`Direct`] row land, and what goes into the columns the row leaves out.
#[derive(Debug, Clone)]
pub(crate) struct Targets {
    /// The column of each item, in the order of the items.
    pub(crate) at: Vec<usize>,
    /// Each column the row leaves out that has a default, with the default.
    pub(crate) fills: Vec<(usize, Fill)>,
    /// The columns the `RETURNING` list gives, with their names and types.
    pub(crate) returning: Option<Arc<Target>>,
}

/// A column default that a [`Direct`] insert can supply with no plan.
#[derive(Debug, Clone)]
pub(crate) enum Fill {
    /// A constant, already of the column's type.
    Value(Value),
    /// `nextval('name')`, by the number of the sequence's counter.
    Next(u64),
    /// `now()` or `current_timestamp`, the start of the transaction.
    Now,
}

impl Clone for Found {
    /// A copy starts over, since nothing it would keep is worth sharing.
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// A `SELECT` of columns of one table whose `WHERE` sets every column of one of its keys equal to
/// a parameter, and nothing else: no join, no grouping, no order, no limit.
///
/// That is the point read of `13-the-point-path.md`, and the plan for it binds the statement,
/// optimizes it and builds a pipeline sized for thousands of rows to read one. So the shape is
/// read once here, and an execution whose names still resolve the way they did finds the row by
/// its key. What the shape cannot settle by itself goes the long way: a name that is not a table
/// of the table's columns, columns that are not a key, or a value that is not already its
/// column's type.
#[derive(Debug, Clone)]
pub(crate) struct Lookup {
    /// The table's name, as it was written.
    pub(crate) name: Vec<String>,
    /// The alias the table was given, if any, which is then the only qualifier a column takes.
    pub(crate) alias: Option<String>,
    /// What the select list asks for, in order.
    pub(crate) picks: Vec<Pick>,
    /// Each column the `WHERE` names, as written, with the item it is set equal to.
    pub(crate) equal: Vec<(Vec<String>, Item)>,
    /// How the session that read the statement compares a column name with a name of the table.
    pub(crate) compare: IdentifierCompare,
    /// What the last execution worked out from the names, kept while they resolve the same.
    pub(crate) found: Resolved,
}

/// The equalities of a `WHERE` that is `column = parameter` joined by `AND` and nothing else, as
/// the column written and the parameter's item.
fn equalities(ast: &Ast, filter: ast::ExprRef) -> Option<Vec<(Vec<String>, Item)>> {
    let words = |slice| ast.name(slice).map(str::to_owned).collect::<Vec<_>>();
    let mut equal = Vec::new();
    let mut pending = vec![filter];
    while let Some(expr) = pending.pop() {
        let ast::Expr::Binary { op, left, right } = ast.expr(expr) else { return None };
        match op {
            ast::BinaryOp::And => pending.extend([right, left]),
            ast::BinaryOp::Eq => {
                let (column, other) = match (ast.expr(left), ast.expr(right)) {
                    (ast::Expr::Column { name }, _) => (name, right),
                    (_, ast::Expr::Column { name }) => (name, left),
                    _ => return None,
                };
                // A key is never equal to a `NULL`, which the plan answers with no rows.
                match item(ast, other)? {
                    Item::Null => return None,
                    found => equal.push((words(column), found)),
                }
            }
            _ => return None,
        }
    }
    Some(equal)
}

/// An `UPDATE` of one table whose `WHERE` is a [`Lookup`]'s, and whose `SET` gives each column
/// a parameter, a `NULL` or the column itself plus or minus a parameter, with no `FROM` and no
/// `RETURNING`.
///
/// That is the write by key of `13-the-point-path.md` section 13.4, and the plan for it reads every
/// row of the table to change one. An execution that finds a plain table and the row by its key
/// writes the row where it is. Anything the shape cannot settle by itself, a column in a key, a
/// constraint to check or a value that is not already its column's type, goes the long way.
///
/// A `DELETE` with the same `WHERE` is one too, with nothing to set, and takes the row out where it
/// is, see `Table::remove_rows`, unless a foreign key points into the table.
#[derive(Debug, Clone)]
pub(crate) struct PointWrite {
    /// The table and the key, as a [`Lookup`] of every column.
    pub(crate) lookup: Lookup,
    /// Each column the `SET` names, as written, with what it is set to.
    pub(crate) sets: Vec<(String, Set)>,
    /// Whether this is a `DELETE`, which sets nothing.
    pub(crate) delete: bool,
}

/// What one column of a [`PointWrite`] is set to.
#[derive(Debug, Clone)]
pub(crate) enum Set {
    /// An item as it is.
    To(Item),
    /// The column as it was plus the item, or minus it when this says so.
    Add(Item, bool),
}

/// An `INSERT` of one row of a [`Direct`]'s shape that says what becomes of a row whose key the
/// table holds: `ON CONFLICT [(key)] DO NOTHING`, `ON CONFLICT [(key)] DO UPDATE SET` with no
/// `WHERE`, `INSERT OR IGNORE` or `INSERT OR REPLACE`.
///
/// That is the upsert of `13-the-point-path.md` section 13.4, and the plan for it reads every row
/// of the table to find the one the key names and writes the whole table back. An execution that
/// finds a plain table with the one key looks the key up: a row that is not there goes in as a
/// [`Direct`] row does, and one that is takes its new values where it is, as a [`PointWrite`] row
/// does. Anything else goes the long way.
#[derive(Debug, Clone)]
pub(crate) struct Upsert {
    /// The row, as an insert of its own.
    pub(crate) insert: Direct,
    /// The columns of the key the statement named, empty when it named none.
    pub(crate) key: Vec<String>,
    /// What becomes of a row whose key the table holds.
    pub(crate) action: Action,
}

/// What an [`Upsert`] does with a row whose key the table holds.
#[derive(Debug, Clone)]
pub(crate) enum Action {
    /// Nothing: the row is dropped.
    Nothing,
    /// The held row takes the new row's values in the columns the statement wrote.
    Replace,
    /// The held row takes these values, each column as written with what it is set to.
    Update(Vec<(String, Change)>),
}

/// What one column of an [`Action::Update`] is set to.
#[derive(Debug, Clone)]
pub(crate) enum Change {
    /// A value as it is.
    To(Source),
    /// The column as it was plus the value, or minus it when this says so.
    Add(Source, bool),
}

/// Where a value of an [`Action::Update`] comes from.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// A parameter or a `NULL`.
    Given(Item),
    /// `excluded.column`, the new row's value, by the column as written.
    Excluded(String),
}

/// One entry of a [`Lookup`] select list.
#[derive(Debug, Clone)]
pub(crate) enum Pick {
    /// `*`, or `t.*` with the qualifier.
    All(Vec<String>),
    /// A column as written, with the alias it was given.
    Column(Vec<String>, Option<String>),
}

/// What a [`Lookup`] resolves to: the table, the key's columns in the order of the `WHERE`, the
/// columns to read, and the names and types of the result.
#[derive(Debug)]
pub(crate) struct Target {
    pub(crate) name: QualifiedName,
    pub(crate) key: Vec<usize>,
    pub(crate) columns: Vec<usize>,
    pub(crate) names: Arc<[String]>,
    pub(crate) types: Arc<[LogicalType]>,
    /// The table column of each column of the result, as the binder gives it to a client.
    pub(crate) origins: Arc<[Option<Origin>]>,
    /// Whether a column may hold a `TIMESTAMPTZ`, which is the one kind of value a result needs
    /// the session to write.
    pub(crate) zoned: bool,
    /// The columns a [`PointWrite`] sets, in the order of its `SET`, and nothing for a lookup.
    pub(crate) sets: Vec<usize>,
}

/// The [`Target`] a [`Lookup`] resolved to, with the catalog's [`naming`] it was resolved at.
///
/// [`naming`]: rudb_catalog::Catalog::naming
#[derive(Debug, Default)]
pub(crate) struct Resolved(Mutex<Option<(u64, Arc<Target>)>>);

impl Resolved {
    /// The target resolved at `naming`, if that is what is held.
    pub(crate) fn get(&self, naming: u64) -> Option<Arc<Target>> {
        match &*self.0.lock().unwrap_or_else(PoisonError::into_inner) {
            Some((at, target)) if *at == naming => Some(Arc::clone(target)),
            _ => None,
        }
    }

    /// Keeps `target` as what the names resolve to at `naming`.
    pub(crate) fn keep(&self, naming: u64, target: Arc<Target>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((naming, target));
    }
}

impl Clone for Resolved {
    /// A copy starts over, like [`Found`].
    fn clone(&self) -> Self {
        Self::default()
    }
}

/// One item of a [`Direct`] row.
#[derive(Debug, Clone)]
pub(crate) enum Item {
    /// A parameter, by its identifier, and by its place among values given by position when the
    /// identifier is a number.
    Parameter(String, Option<usize>),
    /// A `NULL` written into the statement.
    Null,
    /// A value written into the statement: an integer, a string or a boolean. A statement given as
    /// text has these where a prepared one has parameters.
    Value(Value),
}

/// The item that `expr` is, when it is a parameter or a value written as it is.
///
/// A number is only an integer of up to 64 bits, with a minus sign or not, which is the value the
/// binder reads the same text as. A number with a point or an exponent is the plan's to type.
fn item(ast: &Ast, expr: ast::ExprRef) -> Option<Item> {
    let integer = |text: &str, negative: bool| {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let written = if negative { format!("-{text}") } else { text.to_owned() };
        match written.parse::<i32>() {
            Ok(value) => Some(Item::Value(Value::Integer(value))),
            Err(_) => written.parse::<i64>().ok().map(|value| Item::Value(Value::BigInt(value))),
        }
    };
    match ast.expr(expr) {
        ast::Expr::Parameter { name } => {
            let name = ast.string(name);
            Some(Item::Parameter(name.to_owned(), numbered(name)))
        }
        ast::Expr::Literal { kind, text } => match kind {
            ast::LiteralKind::Null => Some(Item::Null),
            ast::LiteralKind::True => Some(Item::Value(Value::Boolean(true))),
            ast::LiteralKind::False => Some(Item::Value(Value::Boolean(false))),
            ast::LiteralKind::String => {
                Some(Item::Value(Value::Varchar(ast.string(text).to_owned())))
            }
            ast::LiteralKind::Number => integer(ast.string(text), false),
            ast::LiteralKind::Blob => None,
        },
        ast::Expr::Unary { op: ast::UnaryOp::Negate, operand } => match ast.expr(operand) {
            ast::Expr::Literal { kind: ast::LiteralKind::Number, text } => {
                integer(ast.string(text), true)
            }
            _ => None,
        },
        _ => None,
    }
}

/// The values an execution was given, as a [`Direct`] insert reads them.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Given<'a> {
    /// Checked against the statement, by name.
    Named(&'a Parameters),
    /// By position, one for each of the parameters `1` to `n`.
    Positional(&'a [Value]),
}

impl Given<'_> {
    /// The value `item` stands for, or `None` for a parameter with no value.
    pub(crate) fn value(self, item: &Item) -> Option<Value> {
        match (item, self) {
            (Item::Null, _) => Some(Value::Null),
            (Item::Value(value), _) => Some(value.clone()),
            (Item::Parameter(name, _), Given::Named(parameters)) => parameters.get(name).cloned(),
            (Item::Parameter(_, at), Given::Positional(values)) => values.get((*at)?).cloned(),
        }
    }
}

/// The place of a parameter named by a number among values given by position, counting from zero.
///
/// Only a number written the way [`Parameters::positional`] writes one, so that `$01` is left to
/// the lookup by name, which does not take it for `1`.
fn numbered(name: &str) -> Option<usize> {
    let number: usize = name.parse().ok()?;
    (number >= 1 && number.to_string() == name).then(|| number - 1)
}

/// Whether `names` are `1` to `n` in some order, each once.
fn numbered_one_to_n(names: &[String]) -> bool {
    let mut seen = vec![false; names.len()];
    names.iter().all(|name| match numbered(name) {
        Some(at) if at < seen.len() && !seen[at] => {
            seen[at] = true;
            true
        }
        _ => false,
    })
}

impl Direct {
    /// The shape of `ast`, if it is one statement of it.
    fn of(ast: &Ast, compare: IdentifierCompare) -> Option<Self> {
        let [ast::Statement::Insert(at)] = ast.statements.as_slice() else { return None };
        let insert = ast.insert(*at);
        if insert.conflict.is_some() {
            return None;
        }
        Self::reading(ast, insert, compare)
    }

    /// The rows of `insert` with what it says about a key it finds held left aside.
    fn reading(ast: &Ast, insert: ast::Insert, compare: IdentifierCompare) -> Option<Self> {
        if insert.copy
            || insert.source == rudb_parse::NONE
            || insert.overriding != ast::Overriding::None
        {
            return None;
        }
        let returning = match insert.returning {
            Some(at) => Some(Self::returning(ast, at, compare)?),
            None => None,
        };
        let query = ast.query(insert.source);
        let ast::QueryBody::Values(rows) = query.body else { return None };
        if query != ast::Query::bare(query.body) {
            return None;
        }
        let row = |row| {
            ast.expr_list(row).iter().map(|&expr| item(ast, expr)).collect::<Option<Vec<_>>>()
        };
        let rows = ast.rows(rows).iter().map(|&at| row(at)).collect::<Option<Vec<_>>>()?;
        // Rows of different widths are the plan's to refuse.
        let width = rows.first()?.len();
        if rows.iter().any(|row| row.len() != width) {
            return None;
        }
        Some(Self {
            name: ast.name(insert.name).map(str::to_owned).collect(),
            columns: ast.name(insert.columns).map(str::to_owned).collect(),
            rows,
            returning,
            compare,
            found: Found::default(),
        })
    }

    /// The `RETURNING` list held at `at`, if it is columns and stars of the table.
    fn returning(ast: &Ast, at: ast::QueryRef, compare: IdentifierCompare) -> Option<Lookup> {
        let query = ast.query(at);
        let ast::QueryBody::Select(select) = query.body else { return None };
        if query != ast::Query::bare(query.body) {
            return None;
        }
        let select = ast.select(select);
        if select.filter != rudb_parse::NONE {
            return None;
        }
        Lookup::listing(ast, select, compare)
    }
}

impl Upsert {
    /// The shape of `ast`, if it is one statement of it.
    fn of(ast: &Ast, compare: IdentifierCompare) -> Option<Self> {
        let [ast::Statement::Insert(at)] = ast.statements.as_slice() else { return None };
        let insert = ast.insert(*at);
        let conflict = insert.conflict?;
        if insert.returning.is_some() {
            return None;
        }
        let direct = Direct::reading(ast, insert, compare)?;
        if direct.rows.len() != 1 {
            return None;
        }
        let key = ast.name(conflict.target).map(str::to_owned).collect();
        let action = match conflict.action {
            ast::ConflictAction::Nothing => Action::Nothing,
            ast::ConflictAction::Replace => Action::Replace,
            ast::ConflictAction::Update { columns, query } => {
                Action::Update(Self::changes(ast, columns, query, compare)?)
            }
        };
        Some(Self { insert: direct, key, action })
    }

    /// The `SET` of a `DO UPDATE`, held as `SELECT values..., condition FROM table [AS alias]
    /// POSITIONAL JOIN table AS excluded`, when there is no `WHERE` and each value is a parameter,
    /// a `NULL`, `excluded.column`, or the column itself plus or minus one of those.
    fn changes(
        ast: &Ast,
        columns: ast::Slice,
        query: ast::QueryRef,
        compare: IdentifierCompare,
    ) -> Option<Vec<(String, Change)>> {
        let query = ast.query(query);
        let ast::QueryBody::Select(select) = query.body else { return None };
        let select = ast.select(select);
        let [source] = ast.source_list(select.from) else { return None };
        let ast::Source::Join { left, .. } = ast.source(*source) else { return None };
        let ast::Source::Table { name, alias, .. } = ast.source(left) else { return None };
        // What the held row's columns are qualified by.
        let label = if alias == rudb_parse::NONE {
            ast.name(name).last()?.to_owned()
        } else {
            ast.string(alias).to_owned()
        };
        if label.eq_ignore_ascii_case("excluded") {
            return None;
        }
        let [values @ .., condition] = ast.target_list(select.targets) else { return None };
        if !matches!(
            ast.expr(condition.expr),
            ast::Expr::Literal { kind: ast::LiteralKind::True, .. }
        ) {
            return None;
        }
        let columns: Vec<String> = ast.name(columns).map(str::to_owned).collect();
        if columns.is_empty() || columns.len() != values.len() {
            return None;
        }
        let source = |expr| match ast.expr(expr) {
            ast::Expr::Column { name } => match ast.name(name).collect::<Vec<_>>().as_slice() {
                [table, column] if table.eq_ignore_ascii_case("excluded") => {
                    Some(Source::Excluded((*column).to_owned()))
                }
                _ => None,
            },
            _ => item(ast, expr).map(Source::Given),
        };
        let mut changes = Vec::with_capacity(columns.len());
        for (column, value) in columns.into_iter().zip(values) {
            if changes.iter().any(|(held, _): &(String, Change)| compare.same(held, &column)) {
                return None;
            }
            // The held row's column itself, bare or by the table's label.
            let itself = |expr| match ast.expr(expr) {
                ast::Expr::Column { name } => match ast.name(name).collect::<Vec<_>>().as_slice() {
                    [bare] => compare.same(bare, &column),
                    [table, bare] => compare.same(table, &label) && compare.same(bare, &column),
                    _ => false,
                },
                _ => false,
            };
            let change = match ast.expr(value.expr) {
                ast::Expr::Binary { op: ast::BinaryOp::Add, left, right } if itself(left) => {
                    Change::Add(source(right)?, false)
                }
                ast::Expr::Binary { op: ast::BinaryOp::Add, left, right } if itself(right) => {
                    Change::Add(source(left)?, false)
                }
                ast::Expr::Binary { op: ast::BinaryOp::Subtract, left, right } if itself(left) => {
                    Change::Add(source(right)?, true)
                }
                _ => Change::To(source(value.expr)?),
            };
            changes.push((column, change));
        }
        Some(changes)
    }
}

impl Lookup {
    /// The shape of `ast`, if it is one statement of it.
    fn of(ast: &Ast, compare: IdentifierCompare) -> Option<Self> {
        let [ast::Statement::Query(at)] = ast.statements.as_slice() else { return None };
        let query = ast.query(*at);
        let ast::QueryBody::Select(select) = query.body else { return None };
        if query != ast::Query::bare(query.body) {
            return None;
        }
        let select = ast.select(select);
        let mut lookup = Self::reading(ast, select, compare)?;
        lookup.equal = equalities(ast, select.filter)?;
        Some(lookup)
    }

    /// The table and the select list of `select`, with no columns set equal yet, if it reads one
    /// table by name with a `WHERE` and nothing else.
    fn reading(ast: &Ast, select: ast::Select, compare: IdentifierCompare) -> Option<Self> {
        if select.filter == rudb_parse::NONE {
            return None;
        }
        Self::listing(ast, select, compare)
    }

    /// The table and the select list of `select`, if it reads one table by name and the list is
    /// columns and stars. A `WHERE` is the caller's to read.
    fn listing(ast: &Ast, select: ast::Select, compare: IdentifierCompare) -> Option<Self> {
        if select.distinct != ast::Distinct::No
            || select.group_by.len != 0
            || select.group_by_all
            || select.having != rudb_parse::NONE
            || select.qualify != rudb_parse::NONE
        {
            return None;
        }
        let [source] = ast.source_list(select.from) else { return None };
        let ast::Source::Table { name, alias, columns } = ast.source(*source) else { return None };
        if columns.len != 0 {
            return None;
        }
        let words = |slice| ast.name(slice).map(str::to_owned).collect::<Vec<_>>();
        let alias = (alias != rudb_parse::NONE).then(|| ast.string(alias).to_owned());
        let mut picks = Vec::new();
        for target in ast.target_list(select.targets) {
            let named = (target.alias != rudb_parse::NONE).then(|| ast.string(target.alias));
            picks.push(match ast.expr(target.expr) {
                ast::Expr::Star { qualifier, replacements }
                    if replacements.len == 0
                        && named.is_none()
                        && ast.star_lists(target.expr) == ast::StarLists::default() =>
                {
                    Pick::All(words(qualifier))
                }
                ast::Expr::Column { name } => Pick::Column(words(name), named.map(str::to_owned)),
                _ => return None,
            });
        }
        Some(Self {
            name: words(name),
            alias,
            picks,
            equal: Vec::new(),
            compare,
            found: Resolved::default(),
        })
    }
}

/// A `SELECT ... FROM t WHERE key >= ? ORDER BY key LIMIT n`, or with `>`, `<=` or `<`, either
/// order and the limit a parameter, where the select list is a [`Lookup`]'s.
///
/// That is the short range read of YCSB workload E, and the plan for it reads every row of the
/// table and sorts the ones that pass to keep a few. An execution that finds the key's column to be
/// an integer key of the table reads the rows in key order from where the key is held.
#[derive(Debug, Clone)]
pub(crate) struct RangeRead {
    /// The table and the select list, with the key's column set equal to the bound.
    pub(crate) lookup: Lookup,
    /// The side of the bound the rows' keys are on.
    pub(crate) reach: rudb_catalog::Reach,
    /// The order the `ORDER BY` wrote, and `None` when it wrote none and the session's applies.
    pub(crate) descending: Option<bool>,
    /// How many rows at most.
    pub(crate) limit: Limit,
}

/// One of the shapes a prepared statement can take the short way with, for
/// [`Prepared::explain`].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Shape<'a> {
    Insert(&'a Direct),
    Lookup(&'a Lookup),
    Write(&'a PointWrite),
    Range(&'a RangeRead),
    Upsert(&'a Upsert),
}

/// The `LIMIT` of a [`RangeRead`].
#[derive(Debug, Clone)]
pub(crate) enum Limit {
    /// A number written into the statement.
    Rows(usize),
    /// A parameter.
    Given(Item),
}

/// The most rows a [`RangeRead`] reads one at a time. Past this the plan's scan is the better way.
pub(crate) const RANGE_ROWS: usize = 1_000;

impl RangeRead {
    /// The shape of `ast`, if it is one statement of it.
    fn of(ast: &Ast, compare: IdentifierCompare) -> Option<Self> {
        use rudb_catalog::Reach;
        let [ast::Statement::Query(at)] = ast.statements.as_slice() else { return None };
        let query = ast.query(*at);
        let ast::QueryBody::Select(select) = query.body else { return None };
        let [order] = ast.order_list(query.order_by) else { return None };
        if query.ctes.len != 0
            || query.order_by_all
            || query.limit == rudb_parse::NONE
            || query.limit_percent
            || query.offset != rudb_parse::NONE
        {
            return None;
        }
        let select = ast.select(select);
        let mut lookup = Lookup::reading(ast, select, compare)?;
        let words = |slice| ast.name(slice).map(str::to_owned).collect::<Vec<_>>();
        let ast::Expr::Binary { op, left, right } = ast.expr(select.filter) else { return None };
        let (column, bound, flipped) = match (ast.expr(left), ast.expr(right)) {
            (ast::Expr::Column { name }, _) => (words(name), item(ast, right)?, false),
            (_, ast::Expr::Column { name }) => (words(name), item(ast, left)?, true),
            _ => return None,
        };
        if matches!(bound, Item::Null) {
            return None;
        }
        let reach = match (op, flipped) {
            (ast::BinaryOp::GtEq, false) | (ast::BinaryOp::LtEq, true) => Reach::AtLeast,
            (ast::BinaryOp::Gt, false) | (ast::BinaryOp::Lt, true) => Reach::Above,
            (ast::BinaryOp::LtEq, false) | (ast::BinaryOp::GtEq, true) => Reach::AtMost,
            (ast::BinaryOp::Lt, false) | (ast::BinaryOp::Gt, true) => Reach::Below,
            _ => return None,
        };
        // The order is by the same column written the same way, and by nothing the select list
        // names, since an `ORDER BY` takes an output name before a column of the table.
        let ast::Expr::Column { name } = ast.expr(order.expr) else { return None };
        let ordered = words(name);
        let last = ordered.last()?;
        let renamed = lookup.picks.iter().any(|pick| match pick {
            Pick::Column(_, Some(alias)) => compare.same(alias, last),
            _ => false,
        });
        let same = ordered.len() == column.len()
            && ordered.iter().zip(&column).all(|(a, b)| compare.same(a, b));
        if renamed || !same {
            return None;
        }
        let descending = match order.order {
            ast::Order::Unstated => None,
            ast::Order::Ascending => Some(false),
            ast::Order::Descending => Some(true),
        };
        let limit = match ast.expr(query.limit) {
            ast::Expr::Literal { kind: ast::LiteralKind::Number, text } => {
                Limit::Rows(ast.string(text).parse().ok().filter(|&rows| rows <= RANGE_ROWS)?)
            }
            ast::Expr::Parameter { name } => {
                let name = ast.string(name);
                Limit::Given(Item::Parameter(name.to_owned(), numbered(name)))
            }
            _ => return None,
        };
        lookup.equal = vec![(column, bound)];
        Some(Self { lookup, reach, descending, limit })
    }
}

impl PointWrite {
    /// The shape of `ast`, if it is one statement of it.
    ///
    /// The statement is held as `SELECT *, hit, values... FROM table`, see the parser's
    /// `changed_rows`, so that is the query looked for here.
    fn of(ast: &Ast, compare: IdentifierCompare) -> Option<Self> {
        let (at, delete) = match ast.statements.as_slice() {
            [ast::Statement::Update(at)] => (at, false),
            [ast::Statement::Delete(at)] => (at, true),
            _ => return None,
        };
        let update = ast.insert(*at);
        if update.returning.is_some()
            || update.conflict.is_some()
            || update.copy
            || update.source == rudb_parse::NONE
        {
            return None;
        }
        let query = ast.query(update.source);
        let ast::QueryBody::Select(select) = query.body else { return None };
        if query != ast::Query::bare(query.body) {
            return None;
        }
        let select = ast.select(select);
        if select.distinct != ast::Distinct::No
            || select.group_by.len != 0
            || select.group_by_all
            || select.having != rudb_parse::NONE
            || select.qualify != rudb_parse::NONE
            || select.filter != rudb_parse::NONE
        {
            return None;
        }
        let [source] = ast.source_list(select.from) else { return None };
        let ast::Source::Table { name, alias, columns } = ast.source(*source) else { return None };
        if columns.len != 0 {
            return None;
        }
        let [star, hit, values @ ..] = ast.target_list(select.targets) else { return None };
        let bare_star = matches!(
            ast.expr(star.expr),
            ast::Expr::Star { qualifier, replacements }
                if qualifier.len == 0 && replacements.len == 0
        ) && ast.star_lists(star.expr) == ast::StarLists::default();
        if !bare_star || star.alias != rudb_parse::NONE || hit.alias != rudb_parse::NONE {
            return None;
        }
        let equal = equalities(ast, hit.expr)?;
        let columns: Vec<String> = ast.name(update.columns).map(str::to_owned).collect();
        if columns.is_empty() != delete || columns.len() != values.len() {
            return None;
        }
        let parameter = |expr| item(ast, expr).filter(|item| !matches!(item, Item::Null));
        let mut sets = Vec::with_capacity(columns.len());
        for (column, value) in columns.into_iter().zip(values) {
            if sets.iter().any(|(held, _): &(String, Set)| compare.same(held, &column)) {
                return None;
            }
            // The column itself, unqualified, which is the only way the shape reads it.
            let itself = |expr| match ast.expr(expr) {
                ast::Expr::Column { name } => {
                    let mut words = ast.name(name);
                    words.next().is_some_and(|word| compare.same(word, &column))
                        && words.next().is_none()
                }
                _ => false,
            };
            let set = match ast.expr(value.expr) {
                ast::Expr::Binary { op: ast::BinaryOp::Add, left, right } if itself(left) => {
                    Set::Add(parameter(right)?, false)
                }
                ast::Expr::Binary { op: ast::BinaryOp::Add, left, right } if itself(right) => {
                    Set::Add(parameter(left)?, false)
                }
                ast::Expr::Binary { op: ast::BinaryOp::Subtract, left, right } if itself(left) => {
                    Set::Add(parameter(right)?, true)
                }
                _ => Set::To(item(ast, value.expr)?),
            };
            sets.push((column, set));
        }
        let words = |slice| ast.name(slice).map(str::to_owned).collect::<Vec<_>>();
        let alias = (alias != rudb_parse::NONE).then(|| ast.string(alias).to_owned());
        let lookup = Lookup {
            name: words(name),
            alias,
            picks: vec![Pick::All(Vec::new())],
            equal,
            compare,
            found: Resolved::default(),
        };
        Some(Self { lookup, sets, delete })
    }
}

/// The shapes of a statement that run without binding and planning, see [`Prepared::explain`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Short {
    direct: Option<Direct>,
    lookup: Option<Lookup>,
    write: Option<PointWrite>,
    range: Option<RangeRead>,
    upsert: Option<Upsert>,
}

impl Short {
    /// The shapes `ast` has, with the names compared as `compare` says, which is the rule of the
    /// session that read the statement.
    pub(crate) fn of(ast: &Ast, compare: IdentifierCompare) -> Self {
        Self {
            direct: Direct::of(ast, compare),
            lookup: Lookup::of(ast, compare),
            write: PointWrite::of(ast, compare),
            range: RangeRead::of(ast, compare),
            upsert: Upsert::of(ast, compare),
        }
    }

    /// The shapes of `ast` when it has its values written in. An insert of more than one row is a
    /// load, which the plan streams into the file of a database, so only an insert of one row
    /// takes the short way.
    pub(crate) fn written(ast: &Ast, compare: IdentifierCompare) -> Self {
        let mut short = Self::of(ast, compare);
        short.direct = short.direct.filter(|direct| direct.rows.len() == 1);
        short
    }

    /// Runs the statement `sql` one of the short ways, or gives `None` when none of them takes it
    /// and the statement goes the long way.
    pub(crate) fn run(
        &self,
        shared: &Shared,
        given: Given<'_>,
        sql: &str,
    ) -> Option<Result<QueryResult>> {
        if let Some(lookup) = &self.lookup
            && let Some(done) = shared.lookup(lookup, given, sql)
        {
            return Some(done);
        }
        if let Some(range) = &self.range
            && let Some(done) = shared.range_read(range, given, sql)
        {
            return Some(done);
        }
        // The short ways do not fire triggers, so a write goes the long way once there is one.
        if shared.triggered() {
            return None;
        }
        if let Some(direct) = &self.direct
            && let Some(done) = shared.insert_direct(direct, given, sql)
        {
            return Some(done);
        }
        if let Some(write) = &self.write
            && let Some(done) = if write.delete {
                shared.delete_point(write, given, sql)
            } else {
                shared.write_point(write, given, sql)
            }
        {
            return Some(done);
        }
        if let Some(upsert) = &self.upsert
            && let Some(done) = shared.upsert_point(upsert, given, sql)
        {
            return Some(done);
        }
        None
    }
}

impl Prepared {
    /// Parses `sql` and reads the parameters out of it.
    pub(crate) fn new(shared: Shared, sql: &str) -> Result<Self> {
        let session = shared.session();
        let ast = crate::database::parse(&session, sql)?;
        let names: Vec<String> = ast.parameters().into_iter().map(str::to_string).collect();
        let short = Short::of(&ast, session.semantics().identifier_compare());
        let numbered = numbered_one_to_n(&names);
        let sql = sql.to_string();
        let described = Arc::default();
        Ok(Self { shared, sql, ast, names, short, numbered, described, types: Vec::new() })
    }

    /// Gives the parameters the PostgreSQL types that the client declared, in the order of
    /// [`Prepared::parameters`], with `None` for no type. A parameter alone in the select list is
    /// a column of its declared type, so `SELECT $1` of a `name` is a `name` and not a `text`.
    pub fn declare(&mut self, types: Vec<Option<DeclaredType>>) {
        self.types = types;
        *self.described.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// `parameters` with the declared types.
    fn typed(&self, mut parameters: Parameters) -> Parameters {
        for (name, ty) in self.names.iter().zip(&self.types) {
            if let Some(ty) = ty {
                parameters.declare(name.clone(), *ty);
            }
        }
        parameters
    }

    /// The statement as it was written.
    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// Whether the statement is a query, an `INSERT`, an `UPDATE` or a `DELETE`. PostgreSQL binds
    /// these when it parses them, and binds every other statement only when it runs.
    #[must_use]
    pub fn binds_at_parse(&self) -> bool {
        matches!(
            self.ast.statements.as_slice(),
            [ast::Statement::Query(_)
                | ast::Statement::Insert(_)
                | ast::Statement::Update(_)
                | ast::Statement::Delete(_)]
        )
    }

    /// The parameters the statement uses, once each, in the order they were written.
    ///
    /// The identifier of a positional parameter is its number as a string, so a statement written
    /// with `?` twice has parameters `1` and `2`.
    #[must_use]
    pub fn parameters(&self) -> &[String] {
        &self.names
    }

    /// What the statement takes and gives, found without running it: the type of each parameter,
    /// in the order of [`Prepared::parameters`], and the columns it answers.
    ///
    /// `declared` has the types the caller knows, in the same order, with `None` for one it does
    /// not know. A parameter of no known type takes the type of the first cast the binder puts on
    /// it, which is the type of what it is compared with, what it is passed to or the column it is
    /// written to. One that no cast reaches stays `None`, and a PostgreSQL client calls that `text`.
    ///
    /// # Errors
    ///
    /// Anything binding the statement reports, such as a table or a column that is not there.
    pub fn describe(&self, declared: &[Option<LogicalType>]) -> Result<Description> {
        let stamp = self.shared.stamp();
        let mut kept = self.described.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = kept.as_ref()
            && held.stamp == stamp
            && held.declared == declared
        {
            return Ok(held.description.clone());
        }
        let described = self.shared.describe(&self.ast, &self.names, declared, &self.types)?;
        let parameters = self
            .names
            .iter()
            .map(|name| {
                described
                    .resolved
                    .iter()
                    .find(|(held, _)| held.eq_ignore_ascii_case(name))
                    .map(|(_, ty)| ty.clone())
            })
            .collect();
        let written = self
            .names
            .iter()
            .map(|name| {
                described
                    .written
                    .iter()
                    .find(|(held, _)| held.eq_ignore_ascii_case(name))
                    .map(|(_, ty)| *ty)
            })
            .collect();
        let description = Description {
            parameters,
            written,
            fields: described.fields,
            origins: described.origins,
            planning: described.planning,
        };
        let declared = declared.to_vec();
        *kept = Some(Described { stamp, declared, description: description.clone() });
        Ok(description)
    }

    /// What the statement runs as, as it stands against the database now: a point plan for one of
    /// the shapes that skip binding and planning, named the way `engine-v4/13-the-point-path.md`
    /// section 13.4 does, and `PIPELINE` for anything bound, planned and run as a pipeline each
    /// time.
    ///
    /// The point plans are `InsertOne t`, `InsertRows t`, `POINT Lookup t(key)`,
    /// `UpdateOne t(key) SET column`, `DeltaOne t(key) SET column`, `DeleteOne t(key)`,
    /// `Range t(key)` and `Upsert t(key)`. They hold for values of the key's and the columns' types. Inside a
    /// transaction all of them take the short way too, until the transaction aborts.
    /// A benchmark checks this before it measures, so a statement that would fall back to the
    /// pipeline is found out by name rather than by a slow number.
    #[must_use]
    pub fn explain(&self) -> String {
        let short = &self.short;
        let shapes = [
            short.direct.as_ref().map(Shape::Insert),
            short.lookup.as_ref().map(Shape::Lookup),
            short.write.as_ref().map(Shape::Write),
            short.range.as_ref().map(Shape::Range),
            short.upsert.as_ref().map(Shape::Upsert),
        ];
        let plan = shapes.into_iter().flatten().find_map(|shape| self.shared.point_plan(shape));
        plan.unwrap_or_else(|| "PIPELINE".to_owned())
    }

    /// Runs the statement with values by position, numbered from one.
    ///
    /// # Errors
    ///
    /// If a parameter was given no value, if a value was given for a parameter the statement does
    /// not use, or anything binding and running the statement reports.
    pub fn execute(&self, values: &[Value]) -> Result<QueryResult> {
        let result = self.shared.in_transaction(&self.sql, || {
            // The short ways, which have the values they want and nothing else, read them where
            // they are rather than copying them into parameters to look them up by name again.
            if self.numbered
                && values.len() == self.names.len()
                && let Some(done) = self.short(Given::Positional(values))
            {
                return done;
            }
            self.run(&self.typed(Parameters::positional(values.to_vec())))
        });
        result.map_err(|error| self.shared.process_error(error))
    }

    /// Runs the statement with values by name, which is what `$name` wants.
    ///
    /// A name is matched without regard to case, which is what DuckDB does for this and for nothing
    /// else. Positional parameters can be given this way too, under the identifiers `1`, `2` and so
    /// on, since a position is only a name that happens to be a number.
    ///
    /// # Errors
    ///
    /// The same as [`Prepared::execute`].
    pub fn execute_named(&self, values: &[(&str, Value)]) -> Result<QueryResult> {
        let mut parameters = Parameters::new();
        for (name, value) in values {
            parameters.set(*name, value.clone());
        }
        let parameters = self.typed(parameters);
        let result = self.shared.in_transaction(&self.sql, || self.run(&parameters));
        result.map_err(|error| self.shared.process_error(error))
    }

    /// Runs the statement with values by position and returns the single value it produced.
    ///
    /// # Errors
    ///
    /// Everything [`Prepared::execute`] can raise, plus an error if the result is not one row of one
    /// column.
    pub fn value(&self, values: &[Value]) -> Result<Value> {
        single(&self.execute(values)?)
    }

    /// The statement run one of the ways that skip binding and planning, where one of them takes
    /// it: an insert of a row or a few, a read by key, a write by key, a short range or an upsert.
    fn short(&self, given: Given<'_>) -> Option<Result<QueryResult>> {
        self.short.run(&self.shared, given, &self.sql)
    }

    /// Checks the values against the statement and runs it.
    fn run(&self, parameters: &Parameters) -> Result<QueryResult> {
        self.check(parameters)?;
        if let Some(done) = self.short(Given::Named(parameters)) {
            return done;
        }
        // Zero for the parse, because this statement was parsed once at `PREPARE` and the whole
        // point of it is that this execution did not parse anything.
        self.shared.execute_ast(&self.ast, &self.sql, parameters, &self.shared.token(), 0)
    }

    /// Both halves of the mismatch, in DuckDB's words.
    ///
    /// Missing first, because that is the order duckdb v1.4.1 reports them in when both are true:
    /// running a statement written with `$a` and `$b` by position says the values for `a` and `b`
    /// were not provided rather than that the values for `1` and `2` were not wanted.
    fn check(&self, parameters: &Parameters) -> Result<()> {
        let missing: Vec<&str> = self
            .names
            .iter()
            .filter(|name| parameters.get(name).is_none())
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(Error::invalid_input(format!(
                "Values were not provided for the following prepared statement parameters: {}",
                missing.join(", ")
            )));
        }
        let excess: Vec<&str> = parameters
            .names()
            .filter(|name| !self.names.iter().any(|held| held.eq_ignore_ascii_case(name)))
            .collect();
        if !excess.is_empty() {
            return Err(Error::invalid_input(format!(
                "Parameter argument/count mismatch, identifiers of the excess parameters: {}",
                excess.join(", ")
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rudb_bind::Parameters;
    use rudb_common::Value;

    use super::{Direct, Given};
    use crate::Database;

    /// The tests in `tests/direct_insert.rs` check that the short way lands what the plan lands,
    /// which they would also do if the short way were never taken. This checks that it is.
    #[test]
    fn a_plain_table_takes_the_short_way() {
        let db = Database::new();
        db.execute("CREATE TABLE t (id BIGINT, name VARCHAR, price DOUBLE, qty INTEGER)")
            .expect("creates");
        let prepared = db.prepare("INSERT INTO t VALUES (?, ?, ?, NULL)").expect("prepares");
        let direct = prepared.short.direct.as_ref().expect("the shape is recognised");
        let values = vec![Value::BigInt(1), Value::Varchar("a".into()), Value::Integer(2)];
        let taken =
            prepared.shared.insert_direct(direct, Given::Positional(&values), prepared.sql());
        assert_eq!(taken.expect("taken").expect("runs").value_at(0, 0), Value::BigInt(1));
        let named = Parameters::positional(values);
        let taken = prepared.shared.insert_direct(direct, Given::Named(&named), prepared.sql());
        assert_eq!(taken.expect("taken").expect("runs").value_at(0, 0), Value::BigInt(1));
        assert_eq!(db.table_len("t").expect("counts"), 2);

        for sql in [
            "INSERT INTO t VALUES (?, ?, ?, ?) RETURNING id + 1",
            "INSERT INTO t VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING RETURNING id",
            "INSERT INTO t SELECT ?, ?, ?, ?",
            "INSERT INTO t VALUES (?, ?, ?, 1 + ?)",
        ] {
            assert!(db.prepare(sql).expect("prepares").short.direct.is_none(), "{sql}");
        }
        assert!(
            Direct::of(
                &rudb_parse::parse_ast("SELECT ?").expect("parses"),
                rudb_common::IdentifierCompare::default()
            )
            .is_none()
        );
    }

    #[test]
    fn a_read_by_key_takes_the_short_way() {
        let db = Database::new();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR)").expect("creates");
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").expect("inserts");
        let prepared = db.prepare("SELECT name FROM t WHERE id = ?").expect("prepares");
        let lookup = prepared.short.lookup.as_ref().expect("the shape is recognised");
        let values = vec![Value::BigInt(2)];
        let taken = prepared.shared.lookup(lookup, Given::Positional(&values), prepared.sql());
        let result = taken.expect("taken").expect("runs");
        assert_eq!(result.value_at(0, 0), Value::Varchar("b".into()));
        let values = vec![Value::Varchar("2".into())];
        let taken = prepared.shared.lookup(lookup, Given::Positional(&values), prepared.sql());
        assert!(taken.is_none(), "a value the plan would cast goes to the plan");
        let prepared = db.prepare("SELECT name FROM t WHERE name = ?").expect("prepares");
        let lookup = prepared.short.lookup.as_ref().expect("the shape is recognised");
        let values = vec![Value::Varchar("b".into())];
        let taken = prepared.shared.lookup(lookup, Given::Positional(&values), prepared.sql());
        assert!(taken.is_none(), "name is no key");

        for sql in [
            "SELECT name FROM t WHERE id = ? LIMIT 1",
            "SELECT name FROM t WHERE id = ? OR id = ?",
            "SELECT name FROM t WHERE id = 1 + ?",
            "SELECT DISTINCT name FROM t WHERE id = ?",
            "SELECT upper(name) FROM t WHERE id = ?",
            "SELECT t.name FROM t, t AS u WHERE t.id = ?",
            "SELECT name FROM t",
        ] {
            assert!(db.prepare(sql).expect("prepares").short.lookup.is_none(), "{sql}");
        }
    }

    #[test]
    fn a_short_range_by_key_takes_the_short_way() {
        let db = Database::new();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR)").expect("creates");
        db.execute("INSERT INTO t VALUES (3, 'c'), (1, 'a'), (2, 'b')").expect("inserts");
        let prepared =
            db.prepare("SELECT name FROM t WHERE id >= ? ORDER BY id LIMIT ?").expect("prepares");
        let range = prepared.short.range.as_ref().expect("the shape is recognised");
        let values = vec![Value::BigInt(2), Value::BigInt(5)];
        let taken = prepared.shared.range_read(range, Given::Positional(&values), prepared.sql());
        let result = taken.expect("taken").expect("runs");
        assert_eq!(result.len(), 2);
        assert_eq!(result.value_at(0, 0), Value::Varchar("b".into()));
        assert_eq!(result.value_at(1, 0), Value::Varchar("c".into()));
        let values = vec![Value::BigInt(2), Value::BigInt(5000)];
        let taken = prepared.shared.range_read(range, Given::Positional(&values), prepared.sql());
        assert!(taken.is_none(), "a long range goes to the plan");

        for sql in [
            "SELECT name FROM t WHERE id >= ? ORDER BY name LIMIT 1",
            "SELECT name FROM t WHERE id >= ? ORDER BY id",
            "SELECT name FROM t WHERE id >= ? ORDER BY id LIMIT 1 OFFSET 1",
            "SELECT name FROM t WHERE id >= ? ORDER BY id, name LIMIT 1",
            "SELECT name FROM t WHERE id >= ? AND id < ? ORDER BY id LIMIT 1",
            "SELECT name AS id FROM t WHERE id >= ? ORDER BY id LIMIT 1",
            "SELECT name FROM t WHERE id >= ? ORDER BY 1 LIMIT 1",
            "SELECT name FROM t WHERE id <> ? ORDER BY id LIMIT 1",
            "SELECT name FROM t WHERE id >= ? ORDER BY id LIMIT 5000",
        ] {
            assert!(db.prepare(sql).expect("prepares").short.range.is_none(), "{sql}");
        }
    }

    #[test]
    fn a_write_by_key_takes_the_short_way() {
        let db = Database::new();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR, n BIGINT)")
            .expect("creates");
        db.execute("INSERT INTO t SELECT i, 'v' || i, i FROM range(3000) r(i)").expect("inserts");
        let prepared =
            db.prepare("UPDATE t SET name = ?, n = n + ? WHERE id = ?").expect("prepares");
        let write = prepared.short.write.as_ref().expect("the shape is recognised");
        let values = vec![Value::Varchar("b".into()), Value::BigInt(5), Value::BigInt(2)];
        let taken = prepared.shared.write_point(write, Given::Positional(&values), prepared.sql());
        assert_eq!(taken.expect("taken").expect("runs").value_at(0, 0), Value::BigInt(1));
        let values = vec![Value::Varchar("c".into()), Value::BigInt(5), Value::BigInt(-2)];
        let taken = prepared.shared.write_point(write, Given::Positional(&values), prepared.sql());
        assert_eq!(taken.expect("taken").expect("runs").value_at(0, 0), Value::BigInt(0));
        let read = db.execute("SELECT name, n FROM t WHERE id = 2").expect("reads");
        assert_eq!(read.value_at(0, 0), Value::Varchar("b".into()));
        assert_eq!(read.value_at(0, 1), Value::BigInt(7));
        let values = vec![Value::Varchar("c".into()), Value::BigInt(i64::MAX), Value::BigInt(2)];
        let taken = prepared.shared.write_point(write, Given::Positional(&values), prepared.sql());
        assert!(taken.is_none(), "a sum that overflows goes to the plan");
        let prepared = db.prepare("UPDATE t SET id = ? WHERE id = ?").expect("prepares");
        let write = prepared.short.write.as_ref().expect("the shape is recognised");
        let values = vec![Value::BigInt(-1), Value::BigInt(2)];
        let taken = prepared.shared.write_point(write, Given::Positional(&values), prepared.sql());
        assert!(taken.is_none(), "a key written goes to the plan");

        for sql in [
            "UPDATE t SET name = ? WHERE id = ? RETURNING id",
            "UPDATE t SET name = ? WHERE id = ? OR id = ?",
            "UPDATE t SET name = upper(?) WHERE id = ?",
            "UPDATE t SET n = n * ? WHERE id = ?",
            "UPDATE t SET n = id + ? WHERE id = ?",
            "UPDATE t SET name = ? FROM t AS u WHERE t.id = ?",
            "UPDATE t SET name = ?",
        ] {
            assert!(db.prepare(sql).expect("prepares").short.write.is_none(), "{sql}");
        }
        let delete = db.prepare("DELETE FROM t WHERE id = ?").expect("prepares");
        assert!(delete.short.write.as_ref().is_some_and(|write| write.delete));
    }
}
