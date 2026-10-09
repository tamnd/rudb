//! From an `Ast` to a `Bound`, which is a statement rather than a query.
//!
//! A `SELECT` binds to a [`Plan`] and nothing else, and that is why [`bind`](crate::bind) can hand
//! one back. `CREATE TABLE`, `DROP TABLE` and `INSERT` are not plans and are deliberately not being
//! made into plans. A `Node::CreateTable` would be a node with no columns, no rows, no cost and no
//! reason to be pushed past anything, which is to say a node the optimizer has to be told to leave
//! alone and the executor has to special case at the root. `spec/09-optimizer.md` section 9.1 says
//! every node in a plan produces rows, and a DDL statement does not, so it goes beside the plan and
//! not inside it.
//!
//! What each variant carries is the statement with every name and type already resolved, so the
//! thing that runs it does catalog calls and nothing else. An `INSERT` in particular arrives with
//! a plan whose output is exactly the target's columns in the target's order and the target's
//! types, with the casts and the nulls for unmentioned columns already in it, so appending is a
//! loop over chunks.

use rudb_catalog::{Catalog, Entry, QualifiedName, duplicate_check, same_name};
use rudb_common::bounds::End;
use rudb_common::{
    Bound as ColumnBound, Clustering, ConflictArbiter, DeclaredType, Error, ExplainOutput, Field,
    IdentifierCompare, InsertColumns, LogicalType, PlanErrors, QueryColumns, Result,
    SequenceOwners, Session, Span, SqlState, Stat, TypeNames, UnknownTypes, Value, Width,
};
use rudb_parse::ast::{self, Ast};
use rudb_parse::{NONE, deparse, parse_ast};
use rudb_plan::{Arm, Expr, ExprRef, Node, Plan, SortKey};

use crate::binder::Binder;
use crate::parameters::{Parameters, Placeholders};

/// One statement, bound.
///
/// Not `#[non_exhaustive]`. A new variant here is a new kind of statement, and the compiler
/// pointing at every place that has to decide what to do with it is the whole value of the enum.
#[derive(Debug)]
pub enum Bound {
    /// A query, which is the only one of these that produces rows.
    Query(Plan),
    /// `CREATE TABLE`.
    CreateTable(CreateTable),
    /// `CREATE VIEW`.
    CreateView(CreateView),
    /// `DROP TABLE` or `DROP VIEW`.
    DropTable(DropTable),
    /// `CREATE SCHEMA` or `DROP SCHEMA`.
    Schema(SchemaChange),
    /// `CREATE SEQUENCE` or `DROP SEQUENCE`.
    Sequence(SequenceChange),
    /// `CREATE TYPE` or `DROP TYPE`.
    Type(TypeChange),
    /// `CREATE TRIGGER` or `DROP TRIGGER`.
    Trigger(TriggerChange),
    /// `CREATE MACRO` or `DROP MACRO`.
    Macro(MacroChange),
    /// `ALTER TABLE` or `ALTER VIEW`.
    Alter(Alter),
    /// `CREATE INDEX` or `DROP INDEX`.
    Index(IndexChange),
    /// `INSERT INTO`.
    Insert(Insert),
    /// `SET name = value`, or `RESET name`, which is the same thing with no value.
    Setting(Setting),
    /// `SET VARIABLE name = value`, with the plan that computes the value, or `RESET VARIABLE name`,
    /// with none.
    ///
    /// A plan rather than a value because the pin computes a variable the way it computes a query,
    /// so `SET VARIABLE a = (SELECT 42)` and `SET VARIABLE a = random() < 2` are both taken, where
    /// a setting has to be a constant. The plan answers one row with one column.
    Variable { name: String, value: Option<Plan> },
    /// Flushes a persistent database snapshot, of the named database when one is named.
    Checkpoint(Option<String>),
    /// `ATTACH`.
    Attach(Attach),
    /// `DETACH`, with the name and whether `IF EXISTS` was written.
    Detach { name: String, if_exists: bool },
    /// `BEGIN`, `COMMIT` or `ROLLBACK`, which have nothing to bind and are carried as written.
    Transaction(ast::Transaction),
    /// `EXPLAIN` over a query, holding the plan of the query rather than the query.
    ///
    /// The same `Plan` a [`Bound::Query`] would have carried, bound the same way and by the same
    /// code. What makes it an explain is that the layer above optimizes it and prints it instead
    /// of running it, which is the point: a plan that was built differently because somebody asked
    /// to see it is not the plan that runs.
    ///
    /// With `analyze` set the layer above runs it as well and prints what happened on it. Still the
    /// same plan, for the same reason.
    ///
    /// With `statistics` set it prints what the planner knew as well, which is the use and the class
    /// behind every number in the plan. That one changes nothing about the plan or the run either.
    ///
    /// With `codegen` set the layer above hands the plan to the compiled engine and prints what it
    /// generated instead.
    ///
    /// `postgres` is the options of a PostgreSQL `EXPLAIN`, which is set when the session reads
    /// `EXPLAIN` as PostgreSQL does. The layer above then prints the plan in the format and with the
    /// node names of PostgreSQL, and the three flags are not read.
    Explain {
        plan: Plan,
        analyze: bool,
        statistics: bool,
        codegen: bool,
        postgres: Option<rudb_plan::explain::Options>,
    },
    /// `COPY ... TO`, a query and how to write what it answers.
    CopyTo(CopyTo),
    /// A table function called for what it does rather than for rows, which is `enable_logging`
    /// and `disable_logging`. They answer no rows and change settings, so they are run where a
    /// `SET` is run and not planned.
    Call(Call),
}

/// A call to a table function that changes settings, with its arguments folded to constants.
///
/// The binder only recognises the call and folds what was passed. What the arguments mean, and
/// which of them the function refuses, is decided where the settings are, because that is the
/// layer that knows what a log level or a log storage is.
#[derive(Debug)]
pub struct Call {
    /// The function name, in lower case.
    pub name: String,
    /// The positional arguments, in order.
    pub positional: Vec<Value>,
    /// The named arguments, in order, by the name as written.
    pub named: Vec<(String, Value)>,
}

/// The table functions that are run as a [`Call`] when they are the whole statement.
const CALLS: [&str; 2] = ["enable_logging", "disable_logging"];

/// A bound `COPY ... TO` a CSV or a JSON file, with every option read and defaulted.
///
/// The CSV defaults are the pin's: a header line, a comma, a double quote that is its own escape, and
/// a null written as nothing at all. The escape does not follow the quote, so `QUOTE ''''` alone
/// still escapes with a double quote, which is what the pin writes.
#[derive(Debug)]
pub struct CopyTo {
    /// The query whose rows are written.
    pub plan: Plan,
    /// The file.
    pub path: String,
    /// Whether the first line names the columns.
    pub header: bool,
    /// What goes between two values.
    pub delimiter: String,
    /// What a value is quoted with.
    pub quote: String,
    /// What comes before a quote inside a quoted value.
    pub escape: String,
    /// What a null is written as.
    pub null: String,
    /// The columns every value of which is quoted, by name.
    pub force_quote: Vec<String>,
    /// Whether every column is, which is `FORCE_QUOTE *`.
    pub force_quote_all: bool,
    /// Whether the file is JSON rather than CSV, and the CSV options above are left alone.
    pub json: bool,
    /// For JSON, whether the rows go in one array rather than one object a line.
    pub array: bool,
    /// Whether the file is Parquet, in which case only the two options below apply.
    pub parquet: bool,
    /// For Parquet, the codec the pages are compressed with, in lower case.
    pub compression: String,
    /// For Parquet, how many rows go in a row group.
    pub row_group_size: u64,
    /// For JSON, the `strftime` format a date is written with, where `None` writes its cast to
    /// VARCHAR.
    pub date_format: Option<String>,
    /// For JSON, the `strftime` format a timestamp of any precision or zone is written with.
    pub timestamp_format: Option<String>,
}

/// A bound `SET` or `RESET`.
///
/// The value is a [`Value`] rather than an expression, because every setting there is takes a
/// string or a number and nothing that runs one wants a plan. What a setting does with the value it
/// gets is the setting's own business and is decided a layer up, since the binder has no idea what
/// settings exist.
///
/// The narrow part of that is that the value has to already be a constant. `SET threads = 2 + 2` is
/// four in DuckDB and is refused here, because folding it needs the expression rewriter and the
/// rewriter is two layers above the binder. Nothing writes arithmetic in a `SET` and the refusal
/// says what it is, so this waits for a reason to move.
#[derive(Debug)]
pub struct Setting {
    /// The setting name, as written.
    pub name: String,
    /// The scope word, if one was written.
    pub scope: ast::Scope,
    /// The value, or `None` for a `RESET`.
    pub value: Option<Value>,
    /// Whether the statement was written as a bare `PRAGMA name`, which carries its value in it.
    pub pragma: bool,
}

/// A bound `ATTACH`.
///
/// The path and the option values are constants, for the same reason a setting's value is: nothing
/// that opens a file wants a plan, and the pin folds them to constants before it opens anything.
#[derive(Debug)]
pub struct Attach {
    /// The path, which is `:memory:` or empty for a database with no file behind it.
    pub path: String,
    /// The name after `AS`, or `None` when the name comes from the path.
    pub alias: Option<String>,
    /// Whether `OR REPLACE` was written.
    pub or_replace: bool,
    /// Whether `IF NOT EXISTS` was written.
    pub if_not_exists: bool,
    /// The options, with the name as written and the value when one was.
    pub options: Vec<(String, Option<Value>)>,
}

/// A bound `CREATE TABLE`.
#[derive(Debug)]
pub struct CreateTable {
    /// The full name the table gets.
    pub name: QualifiedName,
    /// The columns, in order, with the types already resolved. For a `CREATE TABLE AS` these are
    /// the query's output types under whatever names the statement or the query gave them.
    pub columns: Vec<Field>,
    /// The query to fill it from, for a `CREATE TABLE AS`.
    pub source: Option<Plan>,
    /// Whether an existing table of that name is left alone rather than being an error.
    pub if_not_exists: bool,
    /// Whether an existing table of that name is dropped first.
    pub or_replace: bool,
    /// The primary key and the unique constraints, over the columns by place.
    pub keys: Vec<rudb_catalog::Key>,
    /// Each column's `DEFAULT` as the SQL of its expression, or `None` for a column with none.
    pub defaults: Vec<Option<String>>,
    /// Each column's PostgreSQL type, where the declaration wrote one that the logical type does
    /// not give.
    pub types: Vec<Option<DeclaredType>>,
    /// Each column's collation as written, or `None` for a column declared with none.
    pub collations: Vec<Option<String>>,
    /// The sequences the defaults use, which the table depends on.
    pub sequences: Vec<QualifiedName>,
    /// The sequences that a `serial` column makes, which the table owns. Each is also in
    /// `sequences`.
    pub serials: Vec<(QualifiedName, rudb_common::sequence::Options)>,
    /// Which columns are identity columns, one per column, or empty when no column is one.
    pub identities: Vec<Option<rudb_catalog::Identity>>,
    /// The SQL of each `CHECK`, in the order written.
    pub checks: Vec<String>,
    /// The expression of each generated column as SQL, one per column, or empty when no column is
    /// one.
    pub generated: Vec<Option<String>>,
    /// The foreign keys, in the order written.
    pub foreign: Vec<rudb_catalog::ForeignKey>,
    /// Every constraint, in the order written.
    pub order: Vec<rudb_catalog::Constraint>,
    /// The keys written as constraints of the table rather than on a column, by place in `keys`.
    pub apart: Vec<usize>,
}

/// A bound `CREATE VIEW`.
///
/// The body is the text that was written rather than the plan it bound to. It was bound once on the
/// way through here, which is what refuses a view over a table that is not there, and the plan that
/// came out of that is then thrown away, because a view follows the tables underneath it and a plan
/// cannot. See [`rudb_catalog::View`].
#[derive(Debug)]
pub struct CreateView {
    /// The full name the view gets.
    pub name: QualifiedName,
    /// The body, as written.
    pub sql: String,
    /// The whole statement written back out, which is what `duckdb_views()` reports as `sql`.
    ///
    /// Written here because this is the last place the tree is in reach. See
    /// [`rudb_catalog::View::statement`] for what the column is and why it is not the text.
    pub statement: String,
    /// The column names the statement gave, which rename a prefix of what the body produces.
    pub aliases: Vec<String>,
    /// Whether an existing entry of that name is left alone rather than being an error.
    pub if_not_exists: bool,
    /// Whether an existing entry of that name is dropped first.
    pub or_replace: bool,
    /// The columns binding the body produced, after the alias list was applied.
    ///
    /// Worked out here because this is where the body is bound, and carried to the catalog because
    /// that is where `duckdb_columns()` and `duckdb_views()` read it from. See the doc on
    /// `rudb_catalog::View` for why the catalog keeps a list it will have to refresh later.
    pub columns: Vec<Field>,
}

/// A bound `CREATE SCHEMA` or `DROP SCHEMA`.
///
/// Only the name is resolved here. Whether the schema is there is a question for the catalog the
/// statement runs against, which is where `IF NOT EXISTS`, `IF EXISTS` and `OR REPLACE` are
/// answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaChange {
    /// The database the schema is in.
    pub catalog: String,
    /// The schema's own name.
    pub name: String,
    /// Whether this is a `DROP` rather than a `CREATE`.
    pub drop: bool,
    /// Whether a create over a schema that is there, or a drop of one that is not, does nothing.
    pub quiet: bool,
    /// Whether a create drops a schema that is there first, which a schema that holds anything
    /// refuses.
    pub or_replace: bool,
    /// Whether a drop takes everything in the schema with it.
    pub cascade: bool,
}

/// A bound `CREATE SEQUENCE` or `DROP SEQUENCE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceChange {
    /// The full name. `None` for a `DROP SEQUENCE IF EXISTS` of one that is not there.
    pub name: Option<QualifiedName>,
    /// Whether this is a `DROP` rather than a `CREATE`.
    pub drop: bool,
    /// Whether a create over a sequence that is there does nothing.
    pub if_not_exists: bool,
    /// Whether a create replaces a sequence that is there.
    pub or_replace: bool,
    /// Whether a drop takes the tables whose defaults use the sequence with it.
    pub cascade: bool,
    /// What a create settled.
    pub options: rudb_common::sequence::Options,
    /// The table or view an `ALTER SEQUENCE ... OWNED BY` gives the sequence to, which makes this
    /// an alter rather than a create.
    pub owner: Option<QualifiedName>,
    /// The name as written of a sequence that a `DROP SEQUENCE IF EXISTS` did not find, for the
    /// notice that says so.
    pub missing: Option<String>,
}

/// A bound `CREATE MACRO` or `DROP MACRO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroChange {
    /// The full name. `None` for a `DROP MACRO IF EXISTS` of one that is not there.
    pub name: Option<QualifiedName>,
    /// Whether it is a table macro, which for a drop is the kind that was found.
    pub table: bool,
    /// What a create makes, and `None` for a drop.
    pub made: Option<rudb_catalog::Macro>,
    /// Whether a create replaces a macro of that name and kind.
    pub or_replace: bool,
    /// Whether a create over a macro of that name and kind does nothing.
    pub if_not_exists: bool,
}

/// A bound `CREATE TYPE` or `DROP TYPE`.
#[derive(Debug, Clone)]
pub struct TypeChange {
    /// The full name. `None` for a `DROP TYPE IF EXISTS` of one that is not there.
    pub name: Option<QualifiedName>,
    /// What a create makes the name stand for, and `None` on a drop.
    pub ty: Option<LogicalType>,
    /// The made types a create read its type through, which it will depend on.
    pub uses: Vec<QualifiedName>,
    /// Whether a create over a type that is there does nothing.
    pub if_not_exists: bool,
    /// Whether a create replaces a type that is there.
    pub or_replace: bool,
    /// Whether a drop takes the types made from this one with it.
    pub cascade: bool,
    /// The query of `ENUM (SELECT ...)`, whose one column read as strings is the list of labels,
    /// in which case `ty` is `None` on a create too.
    pub labels: Option<Plan>,
}

/// A bound `ALTER TABLE` or `ALTER VIEW`.
#[derive(Debug)]
pub struct Alter {
    /// The table or view, or `None` when `IF EXISTS` found nothing to change.
    pub name: Option<QualifiedName>,
    /// The change, or `None` when an `IF EXISTS` or an `IF NOT EXISTS` on a column made it one.
    pub alteration: Option<rudb_catalog::Alteration>,
    /// Every row of the table as it reads after the change, for the changes that move data.
    pub rewrite: Option<Plan>,
}

/// A bound `CREATE INDEX` or `DROP INDEX`.
#[derive(Debug)]
pub struct IndexChange {
    /// The table a create is over, and `None` on a drop.
    pub table: Option<QualifiedName>,
    /// The index a create makes, stamped by the catalog when it goes in, and `None` on a drop.
    pub index: Option<rudb_catalog::Index>,
    /// The name a drop removes, as written, and empty on a create.
    pub name: Vec<String>,
    /// Whether `IF NOT EXISTS` or `IF EXISTS` was written.
    pub quiet: bool,
}

/// A bound `CREATE TRIGGER` or `DROP TRIGGER`.
#[derive(Debug)]
pub struct TriggerChange {
    /// The table the trigger is on, resolved.
    pub table: QualifiedName,
    /// The trigger's name.
    pub name: String,
    /// The trigger a create makes, checked against the catalog, and `None` on a drop.
    pub trigger: Option<rudb_catalog::Trigger>,
    /// Whether `IF NOT EXISTS` or `IF EXISTS` was written.
    pub quiet: bool,
    /// Whether `OR REPLACE` was written.
    pub or_replace: bool,
}

/// A bound `DROP TABLE` or `DROP VIEW`.
#[derive(Debug)]
pub struct DropTable {
    /// The tables or views to drop, already resolved. With `IF EXISTS` a name that does not resolve
    /// is not in here at all, which is what makes running this a sequence of drops that cannot
    /// fail for being missing. Dropping one of these as the wrong type still can, because `DROP
    /// TABLE IF EXISTS v` where `v` is a view is an error in DuckDB and was measured to be one.
    pub names: Vec<QualifiedName>,
    /// Which of the two the statement said it was dropping.
    pub kind: Entry,
    /// Whether `CASCADE` was written, which takes the triggers that read one of them along.
    pub cascade: bool,
    /// The last part of each name that `IF EXISTS` let go, in the order written, for the notice
    /// PostgreSQL gives for each one.
    pub missing: Vec<String>,
}

/// A bound `INSERT`.
#[derive(Debug)]
pub struct Insert {
    /// The table to append to.
    pub name: QualifiedName,
    /// The rows to append. The output is the table's columns, in the table's order, with the
    /// table's types, so nothing between here and the append has a decision left to make.
    pub source: Plan,
    /// Which of the three writes the source is for.
    pub write: Write,
    /// The `RETURNING` list, bound as a query over the table and run over the rows the statement
    /// wrote in place of the table's own.
    pub returning: Option<Box<Plan>>,
    /// What an append does with a row whose key the table already holds.
    pub conflict: Option<Conflict>,
    /// The table's `CHECK` constraints, for the rows an append or an update writes.
    pub checks: Option<Checks>,
    /// For an `UPDATE` that writes its rows beside the file rather than the whole table again,
    /// the columns it sets, see [`rudb_catalog::Table::patch_rows`]. The source then reads the new
    /// value of each of them, null where the row did not match, and the flag, and nothing else.
    pub patched: Option<Vec<usize>>,
}

/// The `CHECK` constraints of a table, bound as one query over it.
///
/// The query answers, for each row the table holds, whether each constraint fails on it. The write
/// runs it with the table standing in for the rows it wrote, so a failed constraint is found before
/// anything the statement wrote is kept.
#[derive(Debug)]
pub struct Checks {
    /// One boolean column per constraint, true where the row fails it. A null is a pass.
    pub plan: Box<Plan>,
    /// The pin's message for each constraint, in the same order as the columns.
    pub messages: Vec<String>,
}

/// A bound `ON CONFLICT`, `INSERT OR REPLACE` or `INSERT OR IGNORE`.
#[derive(Debug)]
pub struct Conflict {
    /// Which of the table's guards a clash is on, its keys and then its unique indexes, or `None`
    /// for any of them.
    pub key: Option<usize>,
    /// What happens to a row that clashes.
    pub action: ConflictAction,
}

/// What happens to a row whose key the table already holds.
#[derive(Debug)]
pub enum ConflictAction {
    /// The row is dropped.
    Nothing,
    /// The held row takes the new row's values in these columns.
    Replace(Vec<usize>),
    /// The held row takes the values the plan works out in these columns. The plan reads the held
    /// rows as the table and the new rows as [`QualifiedName::excluded`], one of each per row it
    /// answers, and after a value for each column answers whether the row is updated at all.
    Update {
        /// The columns that are set, by place in the table.
        columns: Vec<usize>,
        /// The query that works the values out.
        plan: Box<Plan>,
        /// For a table with generated columns, the query that works them out again for the rows
        /// that were updated, which it reads as the table and answers whole and in order.
        generate: Option<Box<Plan>>,
    },
}

/// What an [`Insert`]'s source means for the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    /// The rows are added to the table.
    Append,
    /// The rows are the whole table afterwards, and one more column after the table's says which
    /// of them the statement changed, so it can count them and return them.
    Update,
    /// The rows are the table as it was, and the column after the table's says which of them
    /// the statement deletes. The table keeps the rest.
    Delete,
}

/// The `RETURNING` query of a writing statement, bound over the table it writes.
///
/// Only a `DELETE` reads the `rowid` of that table there. The pin has none for the rows an
/// `INSERT` or an `UPDATE` returns, and refuses the name as a column the table does not have.
fn returning(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    query: Option<ast::QueryRef>,
    rowid: bool,
) -> Result<Option<Box<Plan>>> {
    let Some(query) = query else { return Ok(None) };
    let mut binder = Binder::with(catalog, parameters, session);
    binder.unnumbered = !rowid;
    let (root, scope) = binder.bind_query(ast, query)?;
    if let Some(placeholders) = parameters.placeholders() {
        placeholders.answer(scope.fields());
        placeholders.answer_origins(scope.origins());
    }
    let mut plan = finish(binder, root)?;
    plan.set_origins(scope.origins());
    Ok(Some(Box::new(plan)))
}

/// Binds one parsed statement against a catalog.
///
/// # Errors
///
/// If the script does not hold exactly one statement, if a name does not resolve, if a type does
/// not work out, or if the statement uses something that is not bound yet.
pub fn bind_statement(ast: &Ast, catalog: &Catalog) -> Result<Bound> {
    bind_statement_with(ast, catalog, &Parameters::new(), &Session::new())
}

/// Binds one parsed statement against a catalog, with values for its parameters and its settings.
///
/// This is the prepared statement path. The statement is parsed once and bound once per set of
/// values, so a parameter is a constant by the time the plan exists and everything after the binder
/// sees an ordinary query. That is why there is no parameter in `rudb_plan::Expr`.
///
/// # Errors
///
/// Everything [`bind_statement`] reports, plus an error for a parameter that was given no value.
pub fn bind_statement_with(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Result<Bound> {
    bind_one(ast, catalog, parameters, session, false)
}

/// Binds one statement the way [`bind_statement_with`] does, except that a query reads a Parquet
/// file that could go through a native mirror from its columns and row count alone.
///
/// For the first bind of a query that will be bound again once its mirrors are in. A query that
/// comes back with [`rudb_plan::Plan::wanted_mirrors`] empty was bound in full and can run. One that
/// comes back with any must be bound again with [`bind_statement_with`] before it runs, because the
/// reads that asked for a mirror were bound without the bounds and the distinct counts the
/// optimizer would have used.
///
/// # Errors
///
/// Everything [`bind_statement_with`] reports.
pub fn bind_statement_outlined(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Result<Bound> {
    bind_one(ast, catalog, parameters, session, true)
}

pub(crate) fn bind_one(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    outlined: bool,
) -> Result<Bound> {
    let statement = match ast.statements.as_slice() {
        [statement] => *statement,
        [] => return Err(Error::binder("no statement to bind")),
        _ => return Err(Error::not_implemented("a script of more than one statement")),
    };
    match statement {
        ast::Statement::Query(query) => {
            if let Some(call) = call(ast, catalog, parameters, session, query)? {
                return Ok(Bound::Call(call));
            }
            let mut binder = Binder::with(catalog, parameters, session);
            binder.outlined = outlined;
            let (root, scope) = binder.bind_query(ast, query)?;
            if let Some(placeholders) = parameters.placeholders() {
                placeholders.answer(scope.fields());
                placeholders.answer_origins(scope.origins());
            }
            let mut plan = finish(binder, root)?;
            plan.set_origins(scope.origins());
            if session.semantics().plan_errors() == PlanErrors::Postgres {
                match parameters.placeholders() {
                    // A parameter is a null while a statement is described, and its value can
                    // change what folds.
                    Some(placeholders) if placeholders.none() => {
                        placeholders.plan_error(crate::fold::planned(&plan).err());
                    }
                    Some(_) => {}
                    None => crate::fold::planned(&plan)?,
                }
            }
            Ok(Bound::Query(plan))
        }
        ast::Statement::CreateTable(index) => {
            create_table(ast, catalog, parameters, session, index)
        }
        ast::Statement::CreateView(index) => create_view(ast, catalog, parameters, session, index),
        ast::Statement::DropTable(index) => drop_table(ast, catalog, index),
        ast::Statement::Schema(index) => {
            let written = ast.schema(index);
            if written.temporary {
                return Err(Error::binder("Temporary schemas are not supported"));
            }
            let parts: Vec<&str> = ast.name(written.name).collect();
            let (catalog, name) = catalog.schema_name(&parts)?;
            Ok(Bound::Schema(SchemaChange {
                catalog,
                name,
                drop: written.drop,
                quiet: written.quiet,
                or_replace: written.or_replace,
                cascade: written.cascade,
            }))
        }
        ast::Statement::Sequence(index) => {
            let written = ast.sequence(index);
            let parts: Vec<&str> = ast.name(written.name).collect();
            let alter = !written.owner.is_empty();
            let mut owner = None;
            let mut missing = None;
            let name = if written.drop || alter {
                match catalog.resolve_sequence(&parts) {
                    Ok(name) => Some(name),
                    Err(_) if written.quiet => {
                        missing = written.drop.then(|| parts.join("."));
                        None
                    }
                    Err(error) => return Err(error),
                }
            } else if written.temporary {
                Some(catalog.resolve_for_create_temporary(&parts)?)
            } else {
                Some(catalog.resolve_for_create(&parts)?)
            };
            if alter && name.is_some() {
                let mut parts: Vec<&str> = ast.name(written.owner).collect();
                // PostgreSQL names a column of the owner, and the table owns the sequence here.
                let column = match session.semantics().sequence_owners() {
                    SequenceOwners::Column if parts.len() > 1 => parts.pop(),
                    _ => None,
                };
                let held = catalog.resolve_owner(&parts)?;
                if let Some(column) = column
                    && !catalog.table(&held).is_ok_and(|table| {
                        let compare = session.semantics().identifier_compare();
                        table.columns().iter().any(|field| compare.same(&field.name, column))
                    })
                {
                    return Err(Error::binder(format!(
                        "column \"{column}\" of relation \"{}\" does not exist",
                        held.table
                    ))
                    .state(SqlState::UNDEFINED_COLUMN));
                }
                owner = Some(held);
            }
            Ok(Bound::Sequence(SequenceChange {
                name,
                drop: written.drop,
                if_not_exists: written.quiet,
                or_replace: written.or_replace,
                cascade: written.cascade,
                options: written.options,
                owner,
                missing,
            }))
        }
        ast::Statement::Type(index) => {
            let written = ast.type_def(index);
            let parts: Vec<&str> = ast.name(written.name).collect();
            let mut labels = None;
            let (name, ty, uses) = if written.drop {
                let name = catalog.resolve_type(&parts).map(|made| made.name().clone());
                if name.is_none() && !written.quiet {
                    return Err(Error::catalog(format!(
                        "Type with name {} does not exist!",
                        parts.last().copied().unwrap_or_default()
                    )));
                }
                (name, None, Vec::new())
            } else {
                let name = if written.temporary {
                    catalog.resolve_for_create_temporary(&parts)?
                } else {
                    catalog.resolve_for_create(&parts)?
                };
                if written.query != NONE {
                    let mut binder = Binder::with(catalog, parameters, session);
                    let (root, scope) = binder.bind_query(ast, written.query)?;
                    if scope.columns.len() != 1 {
                        return Err(Error::binder("The query must return a single column"));
                    }
                    labels = Some(finish(binder, root)?);
                    (Some(name), None, Vec::new())
                } else {
                    let (ty, uses) = written_type(catalog, ast.string(written.ty))?;
                    (Some(name), Some(ty), uses)
                }
            };
            Ok(Bound::Type(TypeChange {
                name,
                ty,
                uses,
                if_not_exists: written.quiet,
                or_replace: written.or_replace,
                cascade: written.cascade,
                labels,
            }))
        }
        ast::Statement::Trigger(index) => trigger(ast, catalog, parameters, session, index),
        ast::Statement::Macro(index) => {
            crate::macros::statement(ast, catalog, parameters, session, index)
        }
        ast::Statement::Alter(index) => alter(ast, catalog, parameters, session, index),
        ast::Statement::Index(index) => create_index(ast, catalog, parameters, session, index),
        ast::Statement::Insert(index) => insert(ast, catalog, parameters, session, index),
        ast::Statement::Update(index) => change(ast, catalog, parameters, session, index, false),
        ast::Statement::Delete(index) => change(ast, catalog, parameters, session, index, true),
        ast::Statement::Set(index) | ast::Statement::Reset(index) => {
            setting(ast, catalog, parameters, session, index)
        }
        ast::Statement::Checkpoint(name) => {
            Ok(Bound::Checkpoint((name != NONE).then(|| ast.string(name).to_string())))
        }
        ast::Statement::Attach(index) => attach(ast, catalog, parameters, session, index),
        ast::Statement::Detach { name, if_exists } => {
            Ok(Bound::Detach { name: ast.string(name).to_string(), if_exists })
        }
        ast::Statement::Transaction(kind) => Ok(Bound::Transaction(kind)),
        ast::Statement::Explain { query, analyze, statistics, codegen, options } => {
            let postgres = session.semantics().explain_output() == ExplainOutput::Postgres;
            let described;
            let parameters = if postgres
                && parameters.placeholders().is_none()
                && crate::explain::generic(ast, options)
            {
                let names = ast.parameters().into_iter().map(|name| (name.to_owned(), None));
                described = Parameters::describing(Placeholders::new(names.collect()));
                &described
            } else {
                parameters
            };
            let mut binder = Binder::with(catalog, parameters, session);
            let (root, _) = binder.bind_query(ast, query)?;
            let plan = finish(binder, root)?;
            // PostgreSQL reads the options after the query is bound, so a wrong column is reported
            // before a wrong option.
            let postgres = match session.semantics().explain_output() {
                ExplainOutput::Pin => None,
                ExplainOutput::Postgres => Some(crate::explain::options(ast, options, analyze)?),
            };
            let analyze = postgres.map_or(analyze, |options| options.analyze);
            Ok(Bound::Explain { plan, analyze, statistics, codegen, postgres })
        }
        ast::Statement::CopyTo(index) => {
            let copy = &ast.copies[index as usize];
            let mut binder = Binder::with(catalog, parameters, session);
            let (root, _) = binder.bind_query(ast, copy.query)?;
            let plan = finish(binder, root)?;
            let typed = copy_values(ast, copy, catalog, parameters, session);
            copy_to(copy, &typed, plan).map(Bound::CopyTo)
        }
        // The connection holds prepared statements, so the database runs these three itself and
        // only a caller that hands one straight to the binder gets here.
        ast::Statement::Prepare { .. }
        | ast::Statement::Execute { .. }
        | ast::Statement::Deallocate(_) => {
            Err(Error::not_implemented("PREPARE, EXECUTE and DEALLOCATE outside of a connection"))
        }
    }
}

/// The type and the value one `COPY ... TO` option was written as.
struct Written {
    ty: LogicalType,
    value: Option<Value>,
}

impl Written {
    /// Whether the option was written as a NULL of any type.
    fn null(&self) -> bool {
        self.value.as_ref().is_some_and(Value::is_null)
    }
}

/// What each option of a `COPY ... TO` was written as, for the checks the pin makes of an option's
/// type before it reads the value. One written bare, as a word, or as anything that does not bind
/// over no rows has none and is read from its text.
fn copy_values(
    ast: &Ast,
    copy: &ast::CopyTo,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Vec<Option<Written>> {
    let written = |expr: ast::ExprRef| {
        if expr == NONE {
            return None;
        }
        let mut binder = Binder::with(catalog, parameters, session);
        let bound = binder.bind_expr(ast, expr, &crate::scope::Scope::empty()).ok()?;
        let ty = binder.plan().expr_type(bound).clone();
        let value = crate::fold::value_of(binder.plan(), bound).ok().flatten();
        Some(Written { ty, value })
    };
    copy.values.iter().map(|&expr| written(expr)).collect()
}

/// The options of a JSON `COPY ... TO`, the ones the pin takes whether or not this does.
const JSON_OPTIONS: [&str; 19] = [
    "format",
    "array",
    "dateformat",
    "date_format",
    "timestampformat",
    "timestamp_format",
    "compression",
    "encoding",
    "per_thread_output",
    "file_size_bytes",
    "partition_by",
    "overwrite",
    "overwrite_or_ignore",
    "filename_pattern",
    "file_extension",
    "use_tmp_file",
    "return_files",
    "write_partition_columns",
    "preserve_order",
];

/// Refuses an option written as NULL, or as a value of a type other than the string an option
/// that names something takes, in the pin's words and in the pin's order.
///
/// A JSON `COPY` looks at a bare `NULL` first, for every option it knows, and says so in its own
/// words. A NULL of a written type, and a NULL for the other formats, is refused before the format
/// sees it. A file name pattern and a compression are cast to a string before the format sees them,
/// and the pin calls any other type one that could not be cast.
fn refuse_written(copy: &ast::CopyTo, typed: &[Option<Written>], format: &str) -> Result<()> {
    let options = || copy.options.iter().map(|(name, _)| name.as_str()).zip(typed);
    let json = format == "json";
    let bare = |written: &Written| written.null() && written.ty == LogicalType::Null;
    if json {
        for (name, written) in options() {
            if written.as_ref().is_some_and(bare) && JSON_OPTIONS.contains(&name) {
                return Err(Error::binder(format!(
                    "COPY (FORMAT JSON) parameter \"{name}\" cannot be NULL."
                )));
            }
        }
    }
    for (name, written) in options() {
        let Some(written) = written else { continue };
        // A bare NULL for `HEADER` is refused here for every format, before JSON says it does
        // not know the option.
        if written.null() && (!json || !bare(written) || name == "header") {
            return Err(Error::binder(format!(
                "NULL is not supported as a valid option for COPY option \"{name}\""
            )));
        }
    }
    for (name, written) in options() {
        let Some(written) = written else { continue };
        if !written.null()
            && written.ty != LogicalType::Varchar
            && matches!(name, "filename_pattern" | "compression")
        {
            let value = written.value.as_ref().map(Value::to_string).unwrap_or_default();
            return Err(Error::invalid_input(format!(
                "Copy option \"{name}\" expected an argument of type VARCHAR - the argument \
                 \"{value}\" of type {} could not be cast as this type",
                written.ty
            )));
        }
    }
    if json {
        for (name, written) in options() {
            let Some(written) = written else { continue };
            let named = matches!(
                name,
                "dateformat"
                    | "date_format"
                    | "timestampformat"
                    | "timestamp_format"
                    | "file_extension"
            );
            if named && !written.null() && written.ty != LogicalType::Varchar {
                return Err(Error::binder(format!(
                    "COPY (FORMAT JSON) parameter \"{name}\" expects a VARCHAR argument, but got \
                     {}.",
                    written.ty
                )));
            }
        }
    }
    Ok(())
}

/// Reads the options of a `COPY ... TO` against the format they are for.
///
/// CSV, JSON and Parquet are written, the format picked by the file's extension unless `FORMAT`
/// names one. An option the pin takes and this does not is
/// refused by name, and one the pin does not take either gets the first line of its refusal.
fn copy_to(copy: &ast::CopyTo, typed: &[Option<Written>], plan: Plan) -> Result<CopyTo> {
    let lowered = copy.path.to_ascii_lowercase();
    let mut format = if lowered.ends_with(".parquet") {
        "parquet"
    } else if [".json", ".ndjson", ".jsonl"].iter().any(|end| lowered.ends_with(end)) {
        "json"
    } else {
        "csv"
    }
    .to_string();
    if let Some((_, Some(written))) = copy.options.iter().rev().find(|(name, _)| name == "format") {
        format = written.trim_matches('\'').to_ascii_lowercase();
    }
    if !matches!(format.as_str(), "csv" | "json" | "parquet") {
        return Err(Error::catalog(format!("Copy Function with name {format} does not exist!")));
    }
    refuse_written(copy, typed, &format)?;
    match format.as_str() {
        "json" => return json_to(copy, plan),
        "parquet" => return parquet_to(copy, plan),
        _ => {}
    }
    let mut out = CopyTo {
        plan,
        path: copy.path.clone(),
        header: true,
        delimiter: ",".to_string(),
        quote: "\"".to_string(),
        escape: "\"".to_string(),
        null: String::new(),
        force_quote: Vec::new(),
        force_quote_all: false,
        json: false,
        array: false,
        parquet: false,
        compression: String::new(),
        row_group_size: 0,
        date_format: None,
        timestamp_format: None,
    };
    for (name, value) in &copy.options {
        let text = || {
            value.clone().ok_or_else(|| {
                Error::binder(format!("\"{name}\" expects a single argument as a string value"))
            })
        };
        match name.as_str() {
            "format" => {}
            "header" => {
                out.header = match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
                    None | Some("true" | "1" | "on") => true,
                    Some("false" | "0" | "off") => false,
                    Some(other) => {
                        return Err(Error::binder(format!(
                            "\"header\" expects a boolean value, not {other}"
                        )));
                    }
                };
            }
            // The pin reads `'\t'` as a tab here, and only here.
            "delimiter" | "delim" | "sep" => {
                out.delimiter = text()?.replace("\\t", "\t");
            }
            "quote" => out.quote = text()?,
            "escape" => out.escape = text()?,
            "null" | "nullstr" => out.null = text()?,
            "force_quote" => {
                let written = text()?;
                let written = written.trim();
                let list = written.strip_prefix('(').and_then(|rest| rest.strip_suffix(')'));
                let list = list.unwrap_or(written);
                if list.trim() == "*" {
                    out.force_quote_all = true;
                } else {
                    for column in list.split(',') {
                        let column = column.trim();
                        let unquoted =
                            column.strip_prefix('"').and_then(|rest| rest.strip_suffix('"'));
                        out.force_quote.push(unquoted.unwrap_or(column).to_string());
                    }
                }
            }
            "compression"
            | "dateformat"
            | "date_format"
            | "timestampformat"
            | "timestamp_format"
            | "new_line"
            | "prefix"
            | "suffix"
            | "per_thread_output"
            | "file_size_bytes"
            | "partition_by"
            | "overwrite"
            | "overwrite_or_ignore"
            | "filename_pattern"
            | "file_extension"
            | "use_tmp_file"
            | "return_files"
            | "write_partition_columns"
            | "preserve_order"
            | "force_not_null"
            | "encoding" => {
                return Err(Error::not_implemented(format!(
                    "COPY TO with the option {name} is not supported yet"
                )));
            }
            _ => {
                return Err(Error::not_implemented(format!(
                    "Unrecognized option \"{name}\" for csv"
                )));
            }
        }
    }
    if out.delimiter.is_empty() {
        return Err(Error::binder("The delimiter option cannot be empty"));
    }
    Ok(out)
}

/// Reads the options of a `COPY ... TO` a JSON file, which is one object a line unless `ARRAY`
/// asks for one array of them.
fn json_to(copy: &ast::CopyTo, plan: Plan) -> Result<CopyTo> {
    let mut out = CopyTo {
        plan,
        path: copy.path.clone(),
        header: false,
        delimiter: String::new(),
        quote: String::new(),
        escape: String::new(),
        null: String::new(),
        force_quote: Vec::new(),
        force_quote_all: false,
        json: true,
        array: false,
        parquet: false,
        compression: String::new(),
        row_group_size: 0,
        date_format: None,
        timestamp_format: None,
    };
    for (name, value) in &copy.options {
        match name.as_str() {
            "format" => {}
            "array" => {
                out.array = match value.as_deref().map(str::to_ascii_lowercase).as_deref() {
                    None | Some("true" | "t" | "1" | "on" | "y" | "yes") => true,
                    Some("false" | "f" | "0" | "off" | "n" | "no") => false,
                    Some(_) => {
                        let written = value.as_deref().unwrap_or_default();
                        return Err(Error::invalid_input(format!(
                            "Failed to cast value: Could not convert string '{written}' to BOOL"
                        )));
                    }
                };
            }
            "dateformat" | "date_format" => out.date_format = Some(json_format(name, value)?),
            "timestampformat" | "timestamp_format" => {
                out.timestamp_format = Some(json_format(name, value)?);
            }
            "encoding" => {
                return Err(Error::invalid_input(
                    "Option \"encoding\" is not supported for writing - only for reading",
                ));
            }
            "compression"
            | "per_thread_output"
            | "file_size_bytes"
            | "partition_by"
            | "overwrite"
            | "overwrite_or_ignore"
            | "filename_pattern"
            | "file_extension"
            | "use_tmp_file"
            | "return_files"
            | "write_partition_columns"
            | "preserve_order" => {
                return Err(Error::not_implemented(format!(
                    "COPY TO with the option {name} is not supported yet"
                )));
            }
            // The pin quotes the names of the options that have a spelling of their own in the
            // grammar, and only those.
            "header" | "delimiter" | "quote" | "escape" => {
                return Err(Error::binder(format!(
                    "Unknown option for COPY ... TO ... (FORMAT JSON): \"{name}\"."
                )));
            }
            _ => {
                return Err(Error::binder(format!(
                    "Unknown option for COPY ... TO ... (FORMAT JSON): {name}."
                )));
            }
        }
    }
    Ok(out)
}

/// The format a `DATEFORMAT` or a `TIMESTAMPFORMAT` of a JSON `COPY` names, checked the way the
/// pin checks it before a row is written.
fn json_format(name: &str, value: &Option<String>) -> Result<String> {
    let Some(format) = value else {
        return Err(Error::binder(format!(
            "COPY (FORMAT JSON) parameter \"{name}\" expects a single argument."
        )));
    };
    if format.eq_ignore_ascii_case("null") {
        return Err(Error::binder(format!(
            "COPY (FORMAT JSON) parameter \"{name}\" cannot be NULL."
        )));
    }
    rudb_kernels::strftime::Format::parse(format)?;
    Ok(format.clone())
}

/// Reads the options of a `COPY ... TO` a Parquet file, which are the codec and the row group size.
fn parquet_to(copy: &ast::CopyTo, plan: Plan) -> Result<CopyTo> {
    let mut out = CopyTo {
        plan,
        path: copy.path.clone(),
        header: false,
        delimiter: String::new(),
        quote: String::new(),
        escape: String::new(),
        null: String::new(),
        force_quote: Vec::new(),
        force_quote_all: false,
        json: false,
        array: false,
        parquet: true,
        compression: "snappy".to_string(),
        row_group_size: 122_880,
        date_format: None,
        timestamp_format: None,
    };
    for (name, value) in &copy.options {
        let written = value.as_deref().unwrap_or_default().trim_matches('\'');
        match name.as_str() {
            "format" => {}
            "compression" | "codec" => {
                let codec = written.to_ascii_lowercase();
                match codec.as_str() {
                    "uncompressed" | "snappy" => out.compression = codec,
                    "brotli" | "gzip" | "lz4" | "lz4_raw" | "zstd" => {
                        return Err(Error::not_implemented(format!(
                            "COPY TO a Parquet file with the {codec} codec is not supported yet"
                        )));
                    }
                    _ => {
                        return Err(Error::binder(
                            "Expected \"compression\" argument to be any of [uncompressed, brotli, \
                             gzip, snappy, lz4, lz4_raw or zstd]",
                        ));
                    }
                }
            }
            "row_group_size" => {
                out.row_group_size = written.parse().map_err(|_| {
                    Error::invalid_input(format!(
                        "Copy option \"row_group_size\" expected an argument of type UBIGINT - the \
                         argument \"{written}\" of type VARCHAR could not be cast as this type"
                    ))
                })?;
                out.row_group_size = out.row_group_size.max(1);
            }
            "row_group_size_bytes"
            | "row_groups_per_file"
            | "compression_level"
            | "field_ids"
            | "kv_metadata"
            | "dictionary_size_limit"
            | "string_dictionary_page_size_limit"
            | "write_bloom_filter"
            | "bloom_filter_false_positive_ratio"
            | "parquet_version"
            | "geoparquet_version"
            | "per_thread_output"
            | "file_size_bytes"
            | "partition_by"
            | "overwrite"
            | "overwrite_or_ignore"
            | "filename_pattern"
            | "file_extension"
            | "use_tmp_file"
            | "return_files"
            | "write_partition_columns"
            | "preserve_order" => {
                return Err(Error::not_implemented(format!(
                    "COPY TO with the option {name} is not supported yet"
                )));
            }
            _ => {
                return Err(Error::not_implemented(format!(
                    "Unrecognized option \"{name}\" for parquet"
                )));
            }
        }
    }
    Ok(out)
}

/// Parses and binds one statement, which is the whole front end in one call.
///
/// # Errors
///
/// Anything the parser or the binder reports.
pub fn bind_statement_sql(sql: &str, catalog: &Catalog) -> Result<Bound> {
    let ast = parse_ast(sql)?;
    bind_statement(&ast, catalog)
}

/// Roots a binder's plan and checks it.
/// A written type, with any name `CREATE TYPE` made read through `catalog`, and the made types it
/// was read through, which a type made from this one depends on.
pub(crate) fn written_type(
    catalog: &Catalog,
    text: &str,
) -> Result<(LogicalType, Vec<QualifiedName>)> {
    typed_in(catalog, text, None)
}

/// [`written_type`] for something made in `home`, where a bare name is looked for in `home`'s
/// own schema before anywhere else, as the pin does for the columns of a table in another
/// database.
fn typed_in(
    catalog: &Catalog,
    text: &str,
    home: Option<&QualifiedName>,
) -> Result<(LogicalType, Vec<QualifiedName>)> {
    let mut uses = Vec::new();
    let ty = LogicalType::parse_with(text, &mut |parts| {
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let at_home = match (home, parts.as_slice()) {
            (Some(home), [name]) => {
                catalog.resolve_type(&[home.catalog.as_str(), home.schema.as_str(), name])
            }
            _ => None,
        };
        let made = at_home.or_else(|| catalog.resolve_type(&parts))?;
        uses.push(made.name().clone());
        Some(made.ty().clone())
    })?;
    Ok((ty, uses))
}

/// [`written_type`] for a caller that only wants the type.
pub(crate) fn read_type(catalog: &Catalog, text: &str) -> Result<LogicalType> {
    written_type(catalog, text).map(|(ty, _)| ty)
}

/// [`read_type`] in a session. A PostgreSQL session reads `oid` and `"char"` as PostgreSQL does,
/// and not as DuckDB does.
pub(crate) fn session_type(
    catalog: &Catalog,
    session: &Session,
    text: &str,
) -> Result<LogicalType> {
    column_type(catalog, session, text, None)
}

/// [`session_type`] for a column of table `home`, see [`typed_in`].
fn column_type(
    catalog: &Catalog,
    session: &Session,
    text: &str,
    home: Option<&QualifiedName>,
) -> Result<LogicalType> {
    if session.semantics().type_names() == TypeNames::Postgres
        && let Some(declared) = rudb_pgtypes::declared_type(text)
    {
        if let Some(ty) = rudb_pgtypes::session_type(declared) {
            return Ok(ty);
        }
        // A built-in type is in `pg_catalog`, and the grammar names each type of the SQL syntax
        // there, so `integer` is `pg_catalog.int4`. The name in `pg_type` is also a name of the
        // type here, so the type is the one of that name.
        if let Some(name) = text.strip_prefix("pg_catalog.") {
            return typed_in(catalog, name, home).map(|(ty, _)| ty);
        }
    }
    typed_in(catalog, text, home).map(|(ty, _)| ty)
}

fn finish(binder: Binder<'_>, root: rudb_plan::NodeRef) -> Result<Plan> {
    let mut plan = binder.into_plan();
    plan.set_root(root);
    plan.validate()?;
    Ok(plan)
}

/// The PostgreSQL type a column declaration wrote, kept only when it tells a client more than the
/// logical type does, so that a table of plain types writes no type block into its file.
fn pg_declared(text: &str, ty: &LogicalType) -> Option<DeclaredType> {
    let declared = rudb_pgtypes::declared_type(text)?;
    let plain = rudb_pgtypes::pg_type(ty);
    (declared.oid != plain.oid || declared.typmod != plain.typmod).then_some(declared)
}

fn create_table(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::CreateTableRef,
) -> Result<Bound> {
    let written = ast.create_table(index);
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = if written.temporary {
        catalog.resolve_for_create_temporary(&parts)?
    } else {
        catalog.resolve_for_create(&parts)?
    };
    let defs = ast.column_defs(written.columns);
    let mut types = Vec::with_capacity(defs.len());
    let mut serials = Vec::with_capacity(defs.len());
    let mut collations = Vec::with_capacity(defs.len());
    let (mut columns, source) = if written.query == NONE {
        let mut columns = Vec::with_capacity(defs.len());
        for def in defs {
            let text = ast.string(def.ty);
            if text.is_empty() && def.generated != NONE {
                // The type of the expression, which is settled once every column has a name.
                serials.push(false);
                types.push(None);
                collations.push(None);
                columns.push(Field::new(ast.string(def.name), LogicalType::Null));
                continue;
            }
            if text.is_empty() && session.semantics().type_names() == TypeNames::Pin {
                return Err(Error::parser(format!(
                    "Column {} must have a type or be defined as a GENERATED column.",
                    ast.string(def.name)
                )));
            }
            if text.is_empty() {
                return Err(Error::binder(format!(
                    "Column \"{}\" was declared without a type",
                    ast.string(def.name)
                )));
            }
            let serial = match session.semantics().type_names() {
                TypeNames::Pin => None,
                TypeNames::Postgres => serial_type(text),
            };
            serials.push(serial.is_some() || def.identity.is_some());
            let ty = match serial.clone() {
                Some(ty) => ty,
                None => column_type(catalog, session, text, Some(&name))?,
            };
            if ty == LogicalType::Type {
                return Err(Error::invalid_input("A table cannot be created with a 'TYPE' column"));
            }
            if def.identity.is_some() && serial_options(&ty).max == 0 {
                return Err(Error::binder(
                    "identity column type must be smallint, integer, or bigint",
                )
                .state(SqlState::INVALID_PARAMETER_VALUE));
            }
            types.push(pg_declared(text, &ty));
            collations.push(if def.collation == NONE {
                None
            } else {
                if ty != LogicalType::Varchar {
                    return Err(Error::parser("Only VARCHAR columns can have collations!"));
                }
                let name = ast.string(def.collation);
                crate::collation::collation_functions(name)?;
                Some(name.to_string())
            });
            let column = ast.string(def.name);
            columns.push(if def.not_null || serial.is_some() || def.identity.is_some() {
                Field::required(column, ty)
            } else {
                Field::new(column, ty)
            });
        }
        (columns, None)
    } else {
        let mut binder = Binder::with(catalog, parameters, session);
        let (root, scope) = binder.bind_query(ast, written.query)?;
        if defs.len() > scope.len() {
            // DuckDB's sentence, typo and all. A column list shorter than the query is fine and
            // renames a prefix, so only this direction is an error.
            return Err(Error::binder("Target table has more colum names than query result."));
        }
        let mut columns = Vec::with_capacity(scope.columns.len());
        for (at, column) in scope.columns.iter().enumerate() {
            let named = match defs.get(at) {
                Some(def) => ast.string(def.name).to_string(),
                None => column.name.clone(),
            };
            columns.push(Field::new(named, column.ty.clone()));
            // A column that a table column or a cast gives keeps its type, as in PostgreSQL.
            types.push(column.origin.and_then(|origin| origin.ty));
            collations.push(binder.column_collation(column.binding)?);
        }
        if defs.is_empty() && session.semantics().query_columns() == QueryColumns::Pin {
            deduplicate(&mut columns);
        }
        (columns, Some(finish(binder, root)?))
    };
    duplicate_check(&columns, session.semantics().identifier_compare())?;
    let generated = if defs.iter().any(|def| def.generated != NONE) {
        generated_columns(ast, defs, &mut columns, (catalog, parameters, session))?
    } else {
        Vec::new()
    };
    let is_generated = |at: usize| generated.get(at).is_some_and(Option::is_some);
    let mut defaults = Vec::with_capacity(defs.len());
    let mut sequences = Vec::new();
    let mut made = Vec::new();
    let mut identities = Vec::new();
    for (at, def) in defs.iter().enumerate() {
        if serials.get(at).copied().unwrap_or(false) {
            let column = &columns[at];
            if def.default != NONE {
                let what = if def.identity.is_some() {
                    "both default and identity specified"
                } else {
                    "multiple default values specified"
                };
                return Err(Error::binder(format!(
                    "{what} for column \"{}\" of table \"{}\"",
                    column.name, name.table
                ))
                .state(SqlState::SYNTAX_ERROR));
            }
            let (sequence, options) = match def.identity {
                Some(identity) => {
                    let options = identity_options(identity.options, &column.ty)?;
                    let named: Vec<&str> = ast.name(identity.sequence).collect();
                    let sequence = match named.split_last() {
                        Some((last, [])) => {
                            QualifiedName { table: (*last).to_string(), ..name.clone() }
                        }
                        Some(_) => catalog.resolve_for_create(&named)?,
                        None => serial_sequence(catalog, &name, &column.name, &made),
                    };
                    identities.resize(at + 1, None);
                    identities[at] = Some(if identity.always {
                        rudb_catalog::Identity::Always
                    } else {
                        rudb_catalog::Identity::ByDefault
                    });
                    (sequence, options)
                }
                None => (
                    serial_sequence(catalog, &name, &column.name, &made),
                    serial_options(&column.ty),
                ),
            };
            let text = if sequence.schema.eq_ignore_ascii_case(&name.schema) {
                sequence.table.clone()
            } else {
                format!("{}.{}", sequence.schema, sequence.table)
            };
            defaults.push(Some(format!("nextval('{}')", text.replace('\'', "''"))));
            sequences.push(sequence.clone());
            made.push((sequence, options));
            continue;
        }
        defaults.push(if def.default == NONE {
            None
        } else {
            let (text, used) = default_text(ast, def.default, catalog, parameters, session)?;
            for name in used {
                if !sequences.contains(&name) {
                    sequences.push(name);
                }
            }
            Some(text)
        });
    }
    let compare = session.semantics().identifier_compare();
    let mut checks = Vec::new();
    for &expr in ast.expr_list(written.checks) {
        let text = check_text(ast, expr, &columns, catalog, parameters, session)?;
        if !generated.is_empty() {
            for used in columns_in(&text)? {
                let place = columns.iter().position(|field| compare.same(&field.name, &used));
                if place.is_some_and(is_generated) {
                    return Err(Error::binder(
                        "Constraints on generated columns are not supported yet",
                    ));
                }
            }
        }
        checks.push(text);
    }
    let mut keys = Vec::new();
    for (at, &names) in ast.name_list(written.keys).iter().enumerate() {
        let mut places = Vec::new();
        for wanted in ast.name(names) {
            let Some(place) = columns.iter().position(|field| compare.same(&field.name, wanted))
            else {
                return Err(Error::catalog(format!(
                    "table \"{}\" does not have a column named \"{wanted}\"",
                    name.table
                ))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!("column \"{wanted}\" named in key does not exist"))
                .unplaced());
            };
            if is_generated(place) {
                return Err(Error::binder(
                    "Constraints on generated columns are not supported yet",
                ));
            }
            places.push(place);
        }
        let primary = at as u32 == written.primary;
        if primary {
            for &place in &places {
                columns[place].not_null = true;
            }
        }
        keys.push(rudb_catalog::Key { columns: places, primary });
    }
    let mut foreign = Vec::new();
    let lists = ast.name_list(written.foreign).iter();
    let tables = ast.name_list(written.foreign_tables).iter();
    let referenced = ast.name_list(written.foreign_referenced).iter();
    for ((&names, &table), &wanted) in lists.zip(tables).zip(referenced) {
        let names: Vec<&str> = ast.name(names).collect();
        let parts: Vec<&str> = ast.name(table).collect();
        let wanted: Vec<&str> = ast.name(wanted).collect();
        for column in &names {
            let place = columns.iter().position(|field| compare.same(&field.name, column));
            if place.is_some_and(is_generated) {
                return Err(Error::binder(format!(
                    "Failed to create foreign key: referenced column \"{column}\" is a generated \
                     column"
                )));
            }
        }
        let key = (names.as_slice(), parts.as_slice(), wanted.as_slice());
        foreign.push(foreign_key((catalog, compare), &name, (&columns, &keys), key)?);
    }
    Ok(Bound::CreateTable(CreateTable {
        name,
        columns,
        source,
        if_not_exists: written.if_not_exists,
        or_replace: written.or_replace,
        keys,
        defaults,
        types,
        collations,
        checks,
        generated,
        foreign,
        sequences,
        serials: made,
        identities,
        order: ast.constraint_list(written.order).iter().map(|&held| constraint(held)).collect(),
        apart: ast
            .constraint_list(written.order)
            .iter()
            .filter_map(|&held| match held {
                ast::Constraint::TableKey(at) => Some(at as usize),
                _ => None,
            })
            .collect(),
    }))
}

/// The integer type of a PostgreSQL `serial` column, or `None` for any other type.
fn serial_type(text: &str) -> Option<LogicalType> {
    Some(match text.trim().to_ascii_lowercase().as_str() {
        "smallserial" | "serial2" => LogicalType::SmallInt,
        "serial" | "serial4" => LogicalType::Integer,
        "bigserial" | "serial8" => LogicalType::BigInt,
        _ => return None,
    })
}

/// The options of the sequence behind a `serial` column, which stops at the largest value of the
/// column type, as `CREATE SEQUENCE ... AS` does. The largest value is 0 for a type that is not a
/// `smallint`, an `integer` or a `bigint`.
fn serial_options(ty: &LogicalType) -> rudb_common::sequence::Options {
    let max = match ty {
        LogicalType::SmallInt => i64::from(i16::MAX),
        LogicalType::Integer => i64::from(i32::MAX),
        LogicalType::BigInt => i64::MAX,
        _ => 0,
    };
    rudb_common::sequence::Options { increment: 1, min: 1, max, start: 1, cycle: false }
}

/// The options of the sequence behind an identity column. A bound that the options did not give is
/// the bound of a `bigint`, which comes down to the bound of the column type here, and a bound that
/// they gave has to fit the column type.
fn identity_options(
    mut options: rudb_common::sequence::Options,
    ty: &LogicalType,
) -> Result<rudb_common::sequence::Options> {
    let max = serial_options(ty).max;
    let min = -max - 1;
    let name = match ty {
        LogicalType::SmallInt => "smallint",
        LogicalType::Integer => "integer",
        _ => "bigint",
    };
    if options.max == i64::MAX {
        if options.start == options.max {
            options.start = max;
        }
        options.max = max;
    }
    if options.min == i64::MIN {
        options.min = min;
    }
    let out = |what: &str, value: i64| {
        Error::binder(format!("{what} ({value}) is out of range for sequence data type {name}"))
            .state(SqlState::INVALID_PARAMETER_VALUE)
    };
    if options.max > max || options.max < min {
        return Err(out("MAXVALUE", options.max));
    }
    if options.min > max || options.min < min {
        return Err(out("MINVALUE", options.min));
    }
    if options.start > options.max {
        return Err(Error::binder(format!(
            "START value ({}) cannot be greater than MAXVALUE ({})",
            options.start, options.max
        ))
        .state(SqlState::INVALID_PARAMETER_VALUE));
    }
    Ok(options)
}

/// The name PostgreSQL gives the sequence of a `serial` column: `<table>_<column>_seq`, cut to the
/// 63 bytes of a name, with a number after it when the name is taken.
fn serial_sequence(
    catalog: &Catalog,
    table: &QualifiedName,
    column: &str,
    made: &[(QualifiedName, rudb_common::sequence::Options)],
) -> QualifiedName {
    const NAME: usize = 63;
    let taken = |candidate: &str| {
        let name = QualifiedName { table: candidate.to_string(), ..table.clone() };
        catalog.entry(&name).is_ok()
            || catalog.sequence(&name).is_ok()
            || made.iter().any(|(held, _)| same_name(&held.table, candidate))
    };
    let mut pass = 0usize;
    loop {
        let label = if pass == 0 { "seq".to_string() } else { format!("seq{pass}") };
        let (mut first, mut second) = (table.table.as_str(), column);
        // The longer part loses a byte until both fit, the way `makeObjectName` cuts them.
        while first.len() + second.len() + label.len() + 2 > NAME {
            if first.len() >= second.len() {
                first = cut(first, first.len() - 1);
            } else {
                second = cut(second, second.len() - 1);
            }
        }
        let candidate = format!("{first}_{second}_{label}");
        if !taken(&candidate) {
            return QualifiedName { table: candidate, ..table.clone() };
        }
        pass += 1;
    }
}

/// The longest start of `text` that is `most` bytes or less and ends on a character boundary.
fn cut(text: &str, most: usize) -> &str {
    let mut end = most.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A constraint the parser kept the place of, as the catalog keeps it.
fn constraint(held: ast::Constraint) -> rudb_catalog::Constraint {
    match held {
        ast::Constraint::Key(at) | ast::Constraint::TableKey(at) => {
            rudb_catalog::Constraint::Key(at as usize)
        }
        ast::Constraint::Check(at) => rudb_catalog::Constraint::Check(at as usize),
        ast::Constraint::Foreign(at) => rudb_catalog::Constraint::Foreign(at as usize),
        ast::Constraint::NotNull(at) => rudb_catalog::Constraint::NotNull(at as usize),
    }
}

/// One `FOREIGN KEY` of a table being made, refused the way the pin refuses one that names no key
/// of the referenced table or pairs columns of different types.
///
/// The referenced table is the one being made when the name is its own, and then its columns and
/// keys are the ones this statement declares.
fn foreign_key(
    (catalog, compare): (&Catalog, IdentifierCompare),
    made: &QualifiedName,
    (columns, keys): (&[Field], &[rudb_catalog::Key]),
    (names, parts, wanted): (&[&str], &[&str], &[&str]),
) -> Result<rudb_catalog::ForeignKey> {
    let mut places = Vec::with_capacity(names.len());
    for &wanted in names {
        let Some(place) = columns.iter().position(|field| compare.same(&field.name, wanted)) else {
            return Err(Error::binder(format!(
                "Failed to create foreign key: referencing column \"{wanted}\" does not exist"
            ))
            .state(SqlState::UNDEFINED_COLUMN)
            .pg(format!("column \"{wanted}\" referenced in foreign key constraint does not exist"))
            .unplaced());
        };
        places.push(place);
    }
    let own = parts.last().is_some_and(|last| same_name(last, &made.table))
        && catalog.resolve(parts).map_or(true, |resolved| resolved == *made);
    let (table, fields, held): (QualifiedName, Vec<Field>, Vec<rudb_catalog::Key>) = if own {
        (made.clone(), columns.to_vec(), keys.to_vec())
    } else {
        let resolved = catalog.resolve(parts)?;
        if catalog.view(&resolved).is_ok() {
            return Err(Error::binder("cannot reference a VIEW with a FOREIGN KEY"));
        }
        let table = catalog.table(&resolved)?;
        (resolved, table.columns().to_vec(), table.keys().to_vec())
    };
    let referenced = if wanted.is_empty() {
        let Some(primary) = held.iter().find(|key| key.primary) else {
            return Err(Error::binder(format!(
                "Failed to create foreign key: there is no primary key for referenced table \"{}\"",
                table.table
            )));
        };
        if primary.columns.len() != places.len() {
            return Err(Error::parser(
                "The number of referencing and referenced columns for foreign keys must be the same",
            ));
        }
        primary.columns.clone()
    } else {
        let mut referenced = Vec::with_capacity(wanted.len());
        for &column in wanted {
            let Some(place) = fields.iter().position(|field| compare.same(&field.name, column))
            else {
                return Err(Error::binder(format!(
                    "Failed to create foreign key: referenced table \"{}\" does not have a column \
                     named \"{column}\"",
                    table.table
                ))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!(
                    "column \"{column}\" referenced in foreign key constraint does not exist"
                ))
                .unplaced());
            };
            referenced.push(place);
        }
        let mut sorted = referenced.clone();
        sorted.sort_unstable();
        let matched = held.iter().any(|key| {
            let mut columns = key.columns.clone();
            columns.sort_unstable();
            columns == sorted
        });
        if !matched && held.is_empty() {
            return Err(Error::binder(format!(
                "Failed to create foreign key: there is no primary key or unique constraint for \
                 referenced table \"{}\"",
                table.table
            )));
        }
        if !matched {
            return Err(Error::binder(format!(
                "Failed to create foreign key: referenced table \"{}\" does not have a primary key \
                 or unique constraint on the columns {}",
                table.table,
                wanted.join(", ")
            )));
        }
        referenced
    };
    for (&from, &to) in places.iter().zip(&referenced) {
        if columns[from].ty != fields[to].ty {
            return Err(Error::binder(format!(
                "Failed to create foreign key: incompatible types between column \"{}\" (\"{}\") \
                 and column \"{}\" (\"{}\")",
                fields[to].name, fields[to].ty, columns[from].name, columns[from].ty
            )));
        }
    }
    Ok(rudb_catalog::ForeignKey { columns: places, table, referenced })
}

/// The SQL a `CHECK` is kept as, refused the way the pin refuses one when the table is made.
fn check_text(
    ast: &Ast,
    expr: ast::ExprRef,
    columns: &[Field],
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Result<String> {
    if crate::expr::has_aggregate(ast, expr) {
        return Err(Error::binder("aggregate functions are not allowed in check constraints"));
    }
    let mut binder = Binder::with(catalog, parameters, session);
    let index = binder.fresh_index();
    let mut scope = crate::scope::Scope::empty();
    for (at, field) in columns.iter().enumerate() {
        scope.push(crate::scope::Visible {
            table: String::new(),
            name: field.name.clone(),
            binding: rudb_plan::ColumnBinding::new(index, at as u32),
            ty: field.ty.clone(),
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        });
    }
    match binder.bind_expr(ast, expr, &scope) {
        Err(error) if error.message().starts_with("Referenced column \"") => {
            let column = error.message().split('"').nth(1).unwrap_or_default();
            Err(Error::binder(format!(
                "Table does not contain column \"{column}\" referenced in check constraint!"
            ))
            .state(SqlState::UNDEFINED_COLUMN)
            .pg(format!("column \"{column}\" does not exist"))
            .unplaced())
        }
        Err(error) => Err(error),
        Ok(_) if !binder.windows.is_empty() => {
            Err(Error::binder("window functions are not allowed in check constraints"))
        }
        Ok(_) => Ok(deparse::expression(ast, expr)),
    }
}

/// The expression of each generated column as the SQL it is kept as, refused the way the pin
/// refuses one when the table is made. Each is bound over the columns in the order they depend on
/// each other, and a column written without a type takes the type of its expression.
fn generated_columns(
    ast: &Ast,
    defs: &[ast::ColumnDef],
    columns: &mut [Field],
    (catalog, parameters, session): (&Catalog, &Parameters, &Session),
) -> Result<Vec<Option<String>>> {
    let compare = session.semantics().identifier_compare();
    let mut texts = vec![None; defs.len()];
    let mut uses = vec![Vec::new(); defs.len()];
    for (at, def) in defs.iter().enumerate() {
        if def.generated == NONE {
            continue;
        }
        let text = deparse::expression(ast, def.generated);
        let (parsed, _) = check_ast(&text)?;
        for expr in &parsed.exprs {
            match *expr {
                ast::Expr::Subquery { .. }
                | ast::Expr::Exists { .. }
                | ast::Expr::InSubquery { .. }
                | ast::Expr::QuantifiedSubquery { .. } => {
                    return Err(Error::parser(format!(
                        "Expression of generated column \"{}\" contains a subquery, which isn't \
                         allowed",
                        columns[at].name
                    )));
                }
                ast::Expr::Lambda { .. } => {
                    return Err(Error::not_implemented(
                        "Lambda functions are currently not supported in generated columns.",
                    ));
                }
                ast::Expr::Column { name } if name.len > 1 => {
                    return Err(Error::parser(
                        "Qualified (tbl.name) column references are not allowed inside of \
                         generated column expressions",
                    ));
                }
                _ => {}
            }
        }
        for used in columns_in(&text)? {
            let Some(place) = columns.iter().position(|field| compare.same(&field.name, &used))
            else {
                return Err(Error::binder(format!(
                    "Column \"{used}\" referenced by generated column does not exist"
                ))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!("column \"{used}\" does not exist"))
                .unplaced());
            };
            uses[at].push(place);
        }
        texts[at] = Some(text);
    }
    // After the columns are looked up, since the pin names a missing one first.
    if defs.iter().all(|def| def.generated != NONE) {
        return Err(Error::binder(
            "Creating a table without physical (non-generated) columns is not supported",
        ));
    }
    // Depth first over what each column reads, so a column is bound after every generated column
    // it reads, and a column that comes back to itself is the pin's circular dependency.
    let mut order = Vec::new();
    let mut state = vec![0u8; defs.len()];
    for start in 0..defs.len() {
        if texts[start].is_none() || state[start] == 2 {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        state[start] = 1;
        while let Some(&mut (at, ref mut next)) = stack.last_mut() {
            if let Some(&used) = uses[at].get(*next) {
                *next += 1;
                if texts[used].is_none() || state[used] == 2 {
                    continue;
                }
                if state[used] == 1 {
                    return Err(Error::invalid_input(
                        "Circular dependency encountered when resolving generated column \
                         expressions",
                    ));
                }
                state[used] = 1;
                stack.push((used, 0));
            } else {
                state[at] = 2;
                order.push(at);
                stack.pop();
            }
        }
    }
    for at in order {
        let text = texts[at].as_deref().unwrap_or_default();
        let (parsed, expr) = check_ast(text)?;
        if crate::expr::has_aggregate(&parsed, expr) {
            return Err(Error::binder("Aggregate functions are not supported here"));
        }
        let mut binder = Binder::with(catalog, parameters, session);
        let index = binder.fresh_index();
        let mut scope = crate::scope::Scope::empty();
        for (place, field) in columns.iter().enumerate() {
            scope.push(crate::scope::Visible {
                table: String::new(),
                name: field.name.clone(),
                binding: rudb_plan::ColumnBinding::new(index, place as u32),
                ty: field.ty.clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        let before = binder.plan_mut().node_count();
        let bound = binder.bind_expr(&parsed, expr, &scope)?;
        if !binder.windows.is_empty() {
            return Err(Error::binder("Window functions are not supported here"));
        }
        // A macro can hide a subquery the text did not show, and that binds to a new node.
        if binder.plan_mut().node_count() > before {
            return Err(Error::binder("Failed to bind generated column"));
        }
        if ast.string(defs[at].ty).is_empty() {
            columns[at].ty = binder.plan().expr_type(bound).clone();
        }
    }
    Ok(texts)
}

/// The `CHECK` constraints of a table as the query a write runs over the rows it wrote, or `None`
/// for a table with none.
///
/// # Errors
///
/// If the name does not resolve to a table or a constraint no longer binds against it.
pub fn bind_checks(
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    name: &QualifiedName,
) -> Result<Option<Checks>> {
    let table = catalog.table(name)?;
    if table.checks().is_empty() {
        return Ok(None);
    }
    let failed: Vec<String> =
        table.checks().iter().map(|text| format!("NOT CAST(({text}) AS BOOLEAN)")).collect();
    let ast = parse_ast(&format!("SELECT {}", failed.join(", ")))?;
    let ast::Statement::Query(query) = ast.statements[0] else {
        return Err(Error::internal("a check that is not an expression"));
    };
    let ast::QueryBody::Select(select) = ast.query(query).body else {
        return Err(Error::internal("a check that is not an expression"));
    };
    let mut binder = Binder::with(catalog, parameters, session);
    let (root, scope) =
        binder.bind_catalog_table(&ast, name, name.table.clone(), ast::Slice::default())?;
    let mut exprs = Vec::with_capacity(failed.len());
    let mut names = Vec::with_capacity(failed.len());
    for target in ast.target_list(ast.select(select).targets) {
        exprs.push(binder.bind_expr(&ast, target.expr, &scope)?);
        names.push(binder.plan_mut().intern("failed"));
    }
    let exprs = binder.plan_mut().add_expr_list(&exprs);
    let names = binder.plan_mut().add_name_list(&names);
    let index = binder.fresh_index();
    let root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    let messages = table
        .checks()
        .iter()
        .map(|text| {
            format!(
                "CHECK constraint failed on table \"{}\" with expression CHECK({text})",
                name.table
            )
        })
        .collect();
    Ok(Some(Checks { plan: Box::new(finish(binder, root)?), messages }))
}

/// The SQL a column's `DEFAULT` is kept as, refused the way the pin refuses one when the table is
/// made. The expression is bound once here to find out, and bound again by every insert that needs
/// it, because a default like `random()` is worked out per row.
fn default_text(
    ast: &Ast,
    expr: ast::ExprRef,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Result<(String, Vec<QualifiedName>)> {
    if crate::expr::has_aggregate(ast, expr) {
        return Err(Error::binder("DEFAULT value cannot contain aggregates!"));
    }
    let mut binder = Binder::with(catalog, parameters, session);
    let before = binder.plan_mut().node_count();
    match binder.bind_expr(ast, expr, &crate::scope::Scope::empty()) {
        Err(error) if error.message().starts_with("Referenced ") => {
            Err(Error::binder("DEFAULT value cannot contain column names"))
        }
        Err(error) => Err(error),
        // Nothing but a subquery adds a node to the plan while an expression over no rows binds.
        Ok(_) if binder.plan_mut().node_count() > before => {
            Err(Error::binder("DEFAULT value cannot contain subqueries"))
        }
        Ok(_) if !binder.windows.is_empty() => {
            Err(Error::binder("DEFAULT value cannot contain window functions!"))
        }
        Ok(_) => Ok((deparse::expression(ast, expr), binder.sequences)),
    }
}

/// `ALTER TABLE` and `ALTER VIEW`, refused the way the pin refuses each change before the catalog
/// sees it: a missing column is the binder's sentence, and so is changing the type of a column a
/// constraint is over.
fn alter(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::AlterRef,
) -> Result<Bound> {
    let written = ast.alter(index);
    let nothing = |name| Ok(Bound::Alter(Alter { name, alteration: None, rewrite: None }));
    let parts: Vec<&str> = ast.name(written.name).collect();
    let wanted = if written.view { Entry::View } else { Entry::Table };
    let name = match catalog.resolve_as(&parts, wanted) {
        Ok(name) => name,
        Err(_) if written.quiet => return nothing(None),
        Err(error) => return Err(error),
    };
    let kind = catalog.entry(&name)?;
    if written.view && kind == Entry::Table {
        return Err(Error::catalog("Can only modify table with ALTER TABLE statement"));
    }
    if let ast::AlterAction::Rename { to } = written.action {
        let alteration = rudb_catalog::Alteration::Rename(ast.string(to).to_string());
        return Ok(Bound::Alter(Alter {
            name: Some(name),
            alteration: Some(alteration),
            rewrite: None,
        }));
    }
    if kind == Entry::View && matches!(written.action, ast::AlterAction::AddKey { .. }) {
        return Err(Error::binder(
            "Cannot execute the `ALTER TABLE` statement on `View`, only `Table` entries are \
             accepted.",
        ));
    }
    if kind == Entry::View {
        return Err(Error::catalog("Can only modify view with ALTER VIEW statement"));
    }
    let table = catalog.table(&name)?;
    let fields = table.columns();
    let compare = session.semantics().identifier_compare();
    let place = |column: ast::StrRef| {
        fields.iter().position(|field| compare.same(&field.name, ast.string(column)))
    };
    let missing = |column: ast::StrRef| {
        let names: Vec<String> = fields.iter().map(|field| format!("\"{}\"", field.name)).collect();
        let column = ast.string(column);
        Error::binder(format!(
            "Table \"{}\" does not have a column with name \"{column}\"\n\nDid you mean: {}",
            name.table,
            names.join(", ")
        ))
        .state(SqlState::UNDEFINED_COLUMN)
        .pg(format!("column \"{column}\" of relation \"{}\" does not exist", name.table))
        .unplaced()
    };
    // The catalog takes a name only when no column has the same bytes, so the session's rule is
    // checked here.
    let taken = |column: &str| {
        if fields.iter().any(|field| compare.same(&field.name, column)) {
            return Err(Error::catalog(format!("Column with name \"{column}\" already exists!"))
                .state(SqlState::DUPLICATE_COLUMN)
                .pg(format!("column \"{column}\" of relation \"{}\" already exists", name.table))
                .unplaced());
        }
        Ok(())
    };
    let found = |column: ast::StrRef| place(column).ok_or_else(|| missing(column));
    let checks = table.checks();
    let mut rewrite = None;
    let alteration = match written.action {
        ast::AlterAction::Rename { .. } => unreachable!("a rename is handled above"),
        ast::AlterAction::RenameColumn { column, to } => {
            // PostgreSQL leaves the table out of this one.
            let at = place(column).ok_or_else(|| {
                missing(column).pg(format!("column \"{}\" does not exist", ast.string(column)))
            })?;
            let (old, to) = (fields[at].name.as_str(), ast.string(to));
            if in_foreign_key(catalog, &name, table, at) {
                // The doubled quotes are the pin's, which quotes a name that is already quoted.
                return Err(Error::catalog(format!(
                    "Cannot rename column \"\"{old}\"\" because this is involved in the foreign key \
                     constraint"
                )));
            }
            let checks =
                checks.iter().map(|text| rename_in(text, old, to)).collect::<Result<Vec<_>>>()?;
            // The pin writes a generated column over the renamed one with the new name, too.
            let generated = (0..fields.len())
                .filter(|_| table.has_generated())
                .map(|at| table.generated(at).map(|text| rename_in(text, old, to)).transpose())
                .collect::<Result<Vec<_>>>()?;
            taken(to)?;
            let to = to.to_string();
            rudb_catalog::Alteration::RenameColumn { column: at, to, checks, generated }
        }
        ast::AlterAction::AddColumn { column, quiet } => {
            if quiet && place(column.name).is_some() {
                return nothing(Some(name));
            }
            taken(ast.string(column.name))?;
            let ty = session_type(catalog, session, ast.string(column.ty))?;
            let declared = pg_declared(ast.string(column.ty), &ty);
            let field = Field {
                not_null: column.not_null,
                ..Field::new(ast.string(column.name), ty.clone())
            };
            let (default, sequences) = if column.default == NONE {
                (None, Vec::new())
            } else {
                let (text, used) = default_text(ast, column.default, catalog, parameters, session)?;
                (Some(text), used)
            };
            rewrite = Some(table_rewrite(
                ast,
                (catalog, parameters, session),
                &name,
                |binder, _, out| {
                    let value = if column.default == NONE {
                        let null = binder.add_constant(Value::Null);
                        binder.cast_to(null, &ty)
                    } else {
                        let value =
                            binder.bind_expr(ast, column.default, &crate::scope::Scope::empty())?;
                        binder.checked_cast_to(value, &ty, false)?
                    };
                    out.push((value, field.name.clone()));
                    Ok(())
                },
            )?);
            rudb_catalog::Alteration::AddColumn { field, default, sequences, declared }
        }
        ast::AlterAction::DropColumn { column, quiet, cascade } => {
            let Some(at) = place(column) else {
                return if quiet { nothing(Some(name)) } else { Err(missing(column)) };
            };
            let dropped = fields[at].name.as_str();
            // The generated columns that read the dropped one, and the ones that read those.
            let mut also: Vec<usize> = Vec::new();
            let mut reached = vec![at];
            while let Some(read) = reached.pop() {
                for other in 0..fields.len() {
                    if other == at || also.contains(&other) {
                        continue;
                    }
                    if let Some(text) = table.generated(other)
                        && columns_in(text)?
                            .iter()
                            .any(|used| compare.same(used, &fields[read].name))
                    {
                        also.push(other);
                        reached.push(other);
                    }
                }
            }
            if !also.is_empty() && !cascade {
                return Err(Error::catalog(
                    "Cannot drop column: column is a dependency of 1 or more generated column(s)",
                ));
            }
            let mut kept = Vec::with_capacity(checks.len());
            for text in checks {
                let used = columns_in(text)?;
                if !used.iter().any(|used| same_name(used, dropped)) {
                    kept.push(text.clone());
                } else if used.iter().any(|used| !same_name(used, dropped)) {
                    return Err(Error::catalog(format!(
                        "Cannot drop column \"{dropped}\" because there is a CHECK constraint that \
                         depends on it"
                    )));
                }
            }
            let mut gone = also.clone();
            gone.push(at);
            gone.sort_unstable();
            rewrite =
                Some(table_rewrite(ast, (catalog, parameters, session), &name, |_, _, out| {
                    for &at in gone.iter().rev() {
                        out.remove(at);
                    }
                    Ok(())
                })?);
            rudb_catalog::Alteration::DropColumn { column: at, checks: kept, also }
        }
        ast::AlterAction::Default { column, default } => {
            let at = found(column)?;
            if table.generated(at).is_some() {
                return Err(Error::binder(format!(
                    "Cannot SET DEFAULT for generated column \"{}\"",
                    fields[at].name
                )));
            }
            let (default, sequences) = if default == NONE {
                (None, Vec::new())
            } else {
                let (text, used) = default_text(ast, default, catalog, parameters, session)?;
                (Some(text), used)
            };
            rudb_catalog::Alteration::Default { column: at, default, sequences }
        }
        ast::AlterAction::NotNull { column, set } => {
            let at = found(column)?;
            if set && table.generated(at).is_some() {
                return Err(Error::binder("Unsupported constraint for generated column!"));
            }
            rudb_catalog::Alteration::NotNull { column: at, set }
        }
        ast::AlterAction::AddKey { columns, primary } => {
            let mut places = Vec::new();
            for column in ast.name(columns) {
                let at = fields.iter().position(|field| same_name(&field.name, column));
                let at = at.ok_or_else(|| {
                    Error::catalog(format!(
                        "table \"{}\" does not have a column named \"{column}\"",
                        name.table
                    ))
                })?;
                // The pin says PRIMARY KEY for a UNIQUE as well.
                if table.generated(at).is_some() {
                    return Err(Error::binder(format!(
                        "cannot create a PRIMARY KEY on a generated column: {column}"
                    )));
                }
                places.push(at);
            }
            rudb_catalog::Alteration::AddKey { columns: places, primary }
        }
        ast::AlterAction::Type { column, ty, using } => {
            let at = found(column)?;
            let changed = fields[at].name.as_str();
            if table.generated(at).is_some() {
                return Err(Error::binder(
                    "Using generated columns in alter statement not supported",
                ));
            }
            for (other, field) in fields.iter().enumerate() {
                if let Some(text) = table.generated(other)
                    && columns_in(text)?.iter().any(|used| same_name(used, changed))
                {
                    return Err(Error::binder(format!(
                        "This column is referenced by the generated column \"{}\", so its type \
                         can not be changed",
                        field.name
                    )));
                }
            }
            if table.keys().iter().any(|key| key.columns.contains(&at)) {
                return Err(Error::binder(
                    "Cannot change the type of a column that has a UNIQUE or PRIMARY KEY \
                     constraint specified",
                ));
            }
            for text in checks {
                if columns_in(text)?.iter().any(|used| same_name(used, changed)) {
                    return Err(Error::binder(
                        "Cannot change the type of a column that has a CHECK constraint specified",
                    ));
                }
            }
            if in_foreign_key(catalog, &name, table, at) {
                return Err(Error::binder(
                    "Cannot change the type of a column that has a FOREIGN KEY constraint specified",
                ));
            }
            let mut target = if ty == NONE {
                None
            } else {
                Some(session_type(catalog, session, ast.string(ty))?)
            };
            rewrite = Some(table_rewrite(
                ast,
                (catalog, parameters, session),
                &name,
                |binder, scope, out| {
                    let value = if using == NONE {
                        out[at].0
                    } else {
                        binder.bind_expr(ast, using, scope)?
                    };
                    let ty = target
                        .get_or_insert_with(|| binder.plan_mut().expr_type(value).clone())
                        .clone();
                    out[at].0 = binder.checked_cast_to(value, &ty, false)?;
                    Ok(())
                },
            )?);
            let written = (ty != NONE).then(|| ast.string(ty));
            let ty = target.ok_or_else(|| Error::internal("an ALTER TYPE that settled no type"))?;
            let declared = written.and_then(|text| pg_declared(text, &ty));
            rudb_catalog::Alteration::Type { column: at, ty, declared }
        }
    };
    Ok(Bound::Alter(Alter { name: Some(name), alteration: Some(alteration), rewrite }))
}

/// `CREATE INDEX` and `DROP INDEX`, with every refusal the pin makes before its catalog sees the
/// index. A drop is only its name here, since which index it is depends on the catalog it runs
/// against.
///
/// Each element is bound over the table to find its type and the columns it reads. A bare column
/// is written back by its name and anything else inside parentheses, so `ON t(b, (a+1))` keeps
/// `b` and `((a + 1))`, which is what the pin's `sql` and `expressions` show.
fn create_index(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    at: ast::IndexRef,
) -> Result<Bound> {
    let written = ast.index(at);
    let parts: Vec<String> = ast.name(written.name).map(str::to_string).collect();
    if written.drop {
        return Ok(Bound::Index(IndexChange {
            table: None,
            index: None,
            name: parts,
            quiet: written.quiet,
        }));
    }
    let written_table: Vec<&str> = ast.name(written.table).collect();
    let name = catalog.resolve_as(&written_table, Entry::Table)?;
    if catalog.entry(&name)? == Entry::View {
        return Err(Error::binder("can only create an index on a base table"));
    }
    if written.using != NONE && !same_name(ast.string(written.using), "art") {
        return Err(Error::binder(format!("Unknown index type: {}", ast.string(written.using))));
    }
    let fields = catalog.table(&name)?.columns();
    let mut binder = Binder::with(catalog, parameters, session);
    let (_, scope) =
        binder.bind_catalog_table(ast, &name, name.table.clone(), ast::Slice::default())?;
    let mut columns = Vec::new();
    let mut plain = true;
    let mut texts = Vec::new();
    let mut column_names = Vec::new();
    for &expr in ast.expr_list(written.elements) {
        if let ast::Expr::Column { name: column } = ast.exprs[expr as usize] {
            let path: Vec<&str> = ast.name(column).collect();
            if let [only] = path[..]
                && !fields.iter().any(|field| same_name(&field.name, only))
            {
                let names: Vec<String> =
                    fields.iter().map(|field| format!("\"{}\"", field.name)).collect();
                // The stray colon is the pin's.
                return Err(Error::binder(format!(
                    "Table \"{}\" does not have a column named \"{only}\"\n\nCandidate bindings: \
                         : {}",
                    name.table,
                    names.join(", ")
                ))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!("column \"{only}\" does not exist"))
                .placed_at(ast.part_span(column, 0)));
            }
        }
        if written.name.is_empty() {
            column_names.push(binder.index_column_name(ast, expr, &scope));
        }
        if crate::expr::has_aggregate(ast, expr) {
            return Err(Error::binder("aggregate functions are not allowed in index expressions"));
        }
        let before = binder.plan_mut().node_count();
        let value = binder.bind_expr(ast, expr, &scope)?;
        if binder.plan_mut().node_count() > before {
            return Err(Error::binder("cannot use subquery in index expressions"));
        }
        if !binder.windows.is_empty() {
            return Err(Error::binder("window functions are not allowed in index expressions"));
        }
        let ty = binder.plan_mut().expr_type(value).clone();
        if ty.is_nested() {
            return Err(Error::invalid_type(format!(
                "Invalid Type [{ty}]: Invalid type for index key."
            )));
        }
        let bare = match binder.plan_mut().expr(value) {
            Expr::Column(binding) => scope.columns.iter().position(|held| held.binding == *binding),
            _ => None,
        };
        if let Some(at) = bare {
            columns.push(at);
            texts.push(rudb_parse::quoted(&fields[at].name));
            continue;
        }
        plain = false;
        let text = deparse::expression(ast, expr);
        let used = columns_in(&text)?;
        if used.is_empty() {
            return Err(Error::binder(
                "CREATE INDEX does not refer to any columns in the base table!",
            ));
        }
        for used in used {
            if let Some(at) = fields.iter().position(|field| same_name(&field.name, &used)) {
                columns.push(at);
            }
        }
        texts.push(format!("({text})"));
    }
    if written.unique && !plain {
        return Err(Error::not_implemented("A UNIQUE index over an expression is not supported"));
    }
    let expressions = Value::List {
        element: LogicalType::Varchar,
        values: texts.iter().map(|text| Value::Varchar(text.clone())).collect(),
    };
    let unique = if written.unique { "UNIQUE " } else { "" };
    let table: Vec<String> = written_table.iter().map(|part| rudb_parse::quoted(part)).collect();
    let using = if written.using == NONE {
        String::new()
    } else {
        format!(" USING {} ", ast.string(written.using))
    };
    // Only the PostgreSQL grammar takes an index with no name, and it is named as PostgreSQL
    // names it.
    let index_name = match parts.last() {
        Some(written) => written.clone(),
        None => crate::figure::index_name(&name.table, &column_names, |taken| {
            catalog.relation_named(&name, taken)
        }),
    };
    let sql = format!(
        "CREATE {unique}INDEX {} ON {}{using}({});",
        rudb_parse::quoted(&index_name),
        table.join("."),
        texts.join(", ")
    );
    if !plain {
        columns.sort_unstable();
        columns.dedup();
    }
    let index = rudb_catalog::Index {
        name: index_name,
        unique: written.unique,
        columns,
        plain,
        expressions: expressions.to_string(),
        sql,
        oid: 0,
    };
    Ok(Bound::Index(IndexChange {
        table: Some(name),
        index: Some(index),
        name: Vec::new(),
        quiet: written.quiet,
    }))
}

/// Whether a column is in one of its table's foreign keys, or is a column another table's foreign
/// key points at.
fn in_foreign_key(
    catalog: &Catalog,
    name: &QualifiedName,
    table: &rudb_catalog::Table,
    at: usize,
) -> bool {
    table.foreign().iter().any(|key| key.columns.contains(&at))
        || catalog.tables().any(|held| {
            held.foreign().iter().any(|key| key.table == *name && key.referenced.contains(&at))
        })
}

/// A plan over every row of a table giving each of its columns, as changed by `change`, which gets
/// the column expressions and their names in order and can add, drop or replace any of them.
fn table_rewrite(
    ast: &Ast,
    (catalog, parameters, session): (&Catalog, &Parameters, &Session),
    name: &QualifiedName,
    change: impl FnOnce(
        &mut Binder<'_>,
        &crate::scope::Scope,
        &mut Vec<(ExprRef, String)>,
    ) -> Result<()>,
) -> Result<Plan> {
    let mut binder = Binder::with(catalog, parameters, session);
    let (root, scope) =
        binder.bind_catalog_table(ast, name, name.table.clone(), ast::Slice::default())?;
    let mut out = Vec::with_capacity(scope.columns.len() + 1);
    for column in &scope.columns {
        let expr = binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
        out.push((expr, column.name.clone()));
    }
    change(&mut binder, &scope, &mut out)?;
    let exprs: Vec<ExprRef> = out.iter().map(|(expr, _)| *expr).collect();
    let names: Vec<_> = out.iter().map(|(_, name)| binder.plan_mut().intern(name)).collect();
    let exprs = binder.plan_mut().add_expr_list(&exprs);
    let names = binder.plan_mut().add_name_list(&names);
    let index = binder.fresh_index();
    let root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    finish(binder, root)
}

/// A kept `CHECK`, parsed back into an expression.
fn check_ast(text: &str) -> Result<(Ast, ast::ExprRef)> {
    let ast = parse_ast(&format!("SELECT {text}"))?;
    let found = match ast.statements.first() {
        Some(&ast::Statement::Query(query)) => match ast.query(query).body {
            ast::QueryBody::Select(select) => {
                ast.target_list(ast.select(select).targets).first().map(|target| target.expr)
            }
            _ => None,
        },
        _ => None,
    };
    let expr = found.ok_or_else(|| Error::internal("a check that is not an expression"))?;
    Ok((ast, expr))
}

/// The columns a kept `CHECK` reads, by the last part of each name.
fn columns_in(text: &str) -> Result<Vec<String>> {
    let (ast, _) = check_ast(text)?;
    let mut out = Vec::new();
    for expr in &ast.exprs {
        if let ast::Expr::Column { name } = *expr
            && let Some(last) = ast.name(name).last()
        {
            out.push(last.to_string());
        }
    }
    Ok(out)
}

/// A kept `CHECK` with every column named `old` renamed to `to`, which is what the pin does to one
/// over a column that `RENAME COLUMN` renames.
fn rename_in(text: &str, old: &str, to: &str) -> Result<String> {
    let (mut ast, expr) = check_ast(text)?;
    let mut renamed = false;
    for at in 0..ast.exprs.len() {
        let ast::Expr::Column { name } = ast.exprs[at] else { continue };
        if name.len == 0 {
            continue;
        }
        let last = (name.start + name.len - 1) as usize;
        if same_name(ast.string(ast.parts[last]), old) {
            let index = ast.strings.len() as u32;
            ast.strings.push(to.to_string());
            ast.parts[last] = index;
            renamed = true;
        }
    }
    Ok(if renamed { deparse::expression(&ast, expr) } else { text.to_string() })
}

/// Renames the columns a query repeated, which is what makes `CREATE TABLE t AS SELECT 1 AS a, 2 AS
/// a` a table rather than an error.
///
/// A query is allowed to produce two columns of one name and `SELECT 1 AS a, 2 AS a` prints two
/// columns called `a`, so a statement that turns a query into a table has to decide what to do with
/// that, and DuckDB renames rather than refusing. The suffix is `_1`, then `_2`, counting up until
/// the name is free, so a query that already has an `a_1` in it pushes the renamed column to `a_2`
/// rather than colliding with it.
///
/// This only runs when the statement wrote no column list. With a list, even a short one, duckdb
/// v1.4.1 takes the names as they come and a repeat is an error, so `CREATE TABLE t (z) AS SELECT 1
/// AS a, 2 AS a` is a table of `z` and `a` and adding a third `a` to that query is a refusal.
fn deduplicate(columns: &mut [Field]) {
    for at in 0..columns.len() {
        let taken = |name: &str, upto: usize, columns: &[Field]| {
            columns[..upto].iter().any(|held| same_name(&held.name, name))
        };
        if !taken(&columns[at].name, at, columns) {
            continue;
        }
        let mut suffix = 1;
        let mut candidate = format!("{}_{suffix}", columns[at].name);
        while taken(&candidate, at, columns) {
            suffix += 1;
            candidate = format!("{}_{suffix}", columns[at].name);
        }
        columns[at].name = candidate;
    }
}

/// Binds a `CREATE VIEW`, which means binding the body and then throwing the plan away.
///
/// Throwing it away is the point. The body is bound here so that a view over a table that is not
/// there is refused now rather than at the first select, and so that the column list can be checked
/// against what the body actually produces. What the catalog keeps is the text, because a view
/// follows the tables underneath it and a plan is a photograph of the day it was built.
fn create_view(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::CreateViewRef,
) -> Result<Bound> {
    let written = ast.create_view(index);
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = if written.temporary {
        catalog.resolve_for_create_temporary(&parts)?
    } else {
        catalog.resolve_for_create(&parts)?
    };
    let aliases: Vec<String> = ast.name(written.columns).map(str::to_string).collect();

    let mut binder = Binder::with(catalog, parameters, session);
    // The plan is thrown away and the columns are all that is kept, so a file is read for its
    // columns and nothing else.
    binder.outlined = true;
    let (_, mut scope) = binder.bind_query(ast, written.query)?;
    if aliases.len() > scope.len() {
        return Err(Error::binder("More VIEW aliases than columns in query result"));
    }
    if !aliases.is_empty() {
        let written: Vec<&str> = aliases.iter().map(String::as_str).collect();
        scope.rename(&written, "unnamed_subquery")?;
    }
    let semantics = session.semantics();
    if semantics.query_columns() == QueryColumns::Postgres {
        duplicate_check(&scope.fields(), semantics.identifier_compare())?;
    }

    let statement = deparse::create_view(ast, index, &name.schema);
    Ok(Bound::CreateView(CreateView {
        name,
        sql: ast.string(written.sql).to_string(),
        statement,
        aliases,
        if_not_exists: written.if_not_exists,
        or_replace: written.or_replace,
        columns: scope.fields(),
    }))
}

/// Binds `CREATE TRIGGER`, which makes every check the pin makes before it keeps one, in the order
/// it makes them, and binds the body against the table's columns so a body that would fail when it
/// fires fails now instead.
fn trigger(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::TriggerRef,
) -> Result<Bound> {
    use rudb_catalog::Event;
    let written = ast.trigger(index);
    let parts: Vec<&str> = ast.name(written.table).collect();
    let name = ast.string(written.name).to_string();
    let (quiet, or_replace) = (written.quiet, written.or_replace);
    if written.drop {
        let table = catalog.resolve(&parts)?;
        return Ok(Bound::Trigger(TriggerChange { table, name, trigger: None, quiet, or_replace }));
    }
    let table = catalog.resolve_as(&parts, Entry::Table)?;
    if catalog.entry(&table).is_ok_and(|found| found == Entry::View) {
        return Err(Error::binder("CREATE TRIGGER requires a base table, not a view or subquery"));
    }
    if written.timing == ast::TriggerTiming::InsteadOf {
        return Err(Error::not_implemented("INSTEAD OF triggers are not yet supported"));
    }
    let before = written.timing == ast::TriggerTiming::Before;
    let event = match written.event {
        ast::TriggerEvent::Insert => Event::Insert,
        ast::TriggerEvent::Update => Event::Update,
        ast::TriggerEvent::Delete => Event::Delete,
    };
    let new_table = (written.new_table != NONE).then(|| ast.string(written.new_table).to_string());
    let old_table = (written.old_table != NONE).then(|| ast.string(written.old_table).to_string());
    let transition = new_table.is_some() || old_table.is_some();
    if written.row {
        if transition {
            return Err(Error::binder("REFERENCING is not valid for FOR EACH ROW triggers"));
        }
        if before {
            return Err(Error::not_implemented(
                "BEFORE FOR EACH ROW triggers are not yet supported",
            ));
        }
        if event == Event::Update {
            return Err(Error::not_implemented(
                "UPDATE FOR EACH ROW triggers are not yet supported",
            ));
        }
    }
    if transition {
        if before {
            return Err(Error::binder(
                "Transition tables can only be specified for AFTER triggers",
            ));
        }
        if old_table.is_some() && event == Event::Insert {
            return Err(Error::binder(
                "REFERENCING OLD TABLE AS is not valid for AFTER INSERT triggers",
            ));
        }
        if new_table.is_some() && event == Event::Delete {
            return Err(Error::binder(
                "REFERENCING NEW TABLE AS is not valid for AFTER DELETE triggers",
            ));
        }
        if !written.columns.is_empty() {
            return Err(Error::binder("UPDATE OF is not valid with transition tables"));
        }
    }
    let held = catalog.table(&table)?;
    let fields = held.columns().to_vec();
    let mut columns = Vec::new();
    for column in ast.name(written.columns) {
        let Some(field) = fields.iter().find(|field| same_name(&field.name, column)) else {
            return Err(Error::binder(format!(
                "Column \"\"{column}\"\" does not exist in table \"\"{}\"\"",
                table.table
            )));
        };
        columns.push(field.name.clone());
    }
    // The pin keeps the triggers on one table all of one kind for now, and a replaced trigger is
    // not one of them any more.
    let mixed = catalog
        .triggers_on(&table)
        .any(|held| held.row != written.row && !(or_replace && same_name(&held.name, &name)));
    if mixed {
        return Err(Error::not_implemented(
            "Mixing FOR EACH STATEMENT and FOR EACH ROW triggers on the same table is not yet \
             supported",
        ));
    }
    let fired = ast.string(written.fired).to_string();
    let body = rudb_parse::parse_ast_with_case(&fired, session.semantics().identifier_case())?;
    let (writes, does) = match body.statements.as_slice() {
        [ast::Statement::Insert(at)] => (body.inserts[*at as usize].name, Event::Insert),
        [ast::Statement::Update(at)] => (body.inserts[*at as usize].name, Event::Update),
        [ast::Statement::Delete(at)] => (body.inserts[*at as usize].name, Event::Delete),
        _ => {
            return Err(Error::not_implemented(
                "a trigger body that is not an INSERT, an UPDATE or a DELETE",
            ));
        }
    };
    if written.row && does == Event::Update {
        return Err(Error::not_implemented(
            "UPDATE trigger bodies in FOR EACH ROW triggers are not yet supported",
        ));
    }
    // The body binds against the rows that fire it, which are rows of the table, under the names
    // it reads them by. None of them are there yet, so this checks it and nothing more.
    let rows = crate::Written {
        names: fields.iter().map(|field| field.name.clone()).collect(),
        types: fields.iter().map(|field| field.ty.clone()).collect(),
        rows: Vec::new(),
    };
    let mut given = parameters.uncaught();
    if written.row {
        let names = rows.names.iter().map(|name| rudb_parse::trigger_column(name)).collect();
        given.relate(rudb_parse::TRIGGER_ROWS, crate::Written { names, ..rows.clone() });
    }
    for alias in new_table.iter().chain(&old_table) {
        given.relate(alias.clone(), rows.clone());
    }
    bind_one(&body, catalog, &given, session, false)?;
    if written.row && !written.reads_row {
        return Err(Error::binder(format!(
            "FOR EACH ROW trigger \"{name}\" on table \"{}\" must reference at least one NEW or OLD \
             column in the trigger body (use FOR EACH STATEMENT if row data is not needed)",
            table.table
        )));
    }
    let target: Vec<&str> = body.name(writes).collect();
    let writes = catalog.resolve(&target)?;
    // What it reads is every table and view the body names, which the catalog keeps from being
    // dropped or renamed under it. A name that is not in the catalog is one of the names above.
    let mut reads = vec![writes.clone()];
    for source in &body.sources {
        let ast::Source::Table { name: read, .. } = *source else { continue };
        let read: Vec<&str> = body.name(read).collect();
        if let Ok(read) = catalog
            .resolve_as(&read, Entry::Table)
            .or_else(|_| catalog.resolve_as(&read, Entry::View))
            && !reads.contains(&read)
        {
            reads.push(read);
        }
    }
    if let Some(other) =
        reads.iter().find(|read| !read.catalog.eq_ignore_ascii_case(&table.catalog))
    {
        return Err(Error::binder(format!(
            "Trigger \"\"{name}\"\" cannot reference \"\"{}\"\" from a different catalog \
             (\"\"{}\"\")",
            other.table, other.catalog
        )));
    }
    let trigger = rudb_catalog::Trigger {
        name: name.clone(),
        table: table.clone(),
        written_table: ast
            .name(written.table)
            .map(rudb_parse::quoted)
            .collect::<Vec<_>>()
            .join("."),
        before,
        event,
        columns,
        new_table,
        old_table,
        row: written.row,
        fired,
        written: ast.string(written.written).to_string(),
        reads,
        writes,
        does,
        oid: 0,
    };
    refuse_chains(catalog, &trigger)?;
    Ok(Bound::Trigger(TriggerChange { table, name, trigger: Some(trigger), quiet, or_replace }))
}

/// Refuses a trigger that would set off itself, through any run of the triggers already there,
/// and a row trigger that writes rows a row trigger would then fire on, which the pin does not
/// support yet either, in its words for each.
fn refuse_chains(catalog: &Catalog, made: &rudb_catalog::Trigger) -> Result<()> {
    let others = |held: &&rudb_catalog::Trigger| {
        !(same_name(&held.name, &made.name) && held.table == made.table)
    };
    if made.row {
        let cascades = catalog
            .triggers_on(&made.writes)
            .filter(others)
            .any(|held| held.row && held.event == made.does);
        if cascades {
            return Err(cascading(made));
        }
    }
    // The statement in a body fires the triggers on the table it writes for what it does, and the
    // chain is a loop when it comes back to the table the new one is on, whatever it does there.
    let mut pending = vec![(made.writes.clone(), made.does)];
    let mut seen: Vec<(QualifiedName, rudb_catalog::Event)> = Vec::new();
    while let Some((table, does)) = pending.pop() {
        if table == made.table {
            return Err(Error::not_implemented(format!(
                "Recursive trigger chains are not yet supported (trigger cycle detected through \
                 trigger \"{}\" on table \"{}\")",
                made.name, made.table.table
            )));
        }
        if seen.contains(&(table.clone(), does)) {
            continue;
        }
        for held in catalog.triggers_on(&table).filter(others) {
            if held.event == does {
                pending.push((held.writes.clone(), held.does));
            }
        }
        seen.push((table, does));
    }
    Ok(())
}

/// The pin's refusal of a row trigger whose rows a row trigger on the table it writes would fire on.
#[must_use]
pub fn cascading(trigger: &rudb_catalog::Trigger) -> Error {
    Error::not_implemented(format!(
        "FOR EACH ROW trigger \"\"{}\"\" on table \"\"{}\"\" writes to a table that has its own FOR \
         EACH ROW trigger (cascading row triggers are not yet supported)",
        trigger.name, trigger.table.table
    ))
}

fn drop_table(ast: &Ast, catalog: &Catalog, index: ast::DropTableRef) -> Result<Bound> {
    let written = ast.drop_table(index);
    let kind = if written.view { Entry::View } else { Entry::Table };
    let mut names = Vec::new();
    let mut missing = Vec::new();
    for &name in ast.name_list(written.names) {
        let parts: Vec<&str> = ast.name(name).collect();
        // The statement said which of the two it meant, so a name that is not there is a missing
        // one of those and not a missing table.
        match catalog.resolve_as(&parts, kind) {
            Ok(resolved) => names.push(resolved),
            Err(_) if written.if_exists => {
                missing.push(parts.last().copied().unwrap_or_default().to_owned());
            }
            Err(error) => {
                let what = if written.view { "view" } else { "table" };
                let name = parts.last().copied().unwrap_or_default();
                return Err(error
                    .state(SqlState::UNDEFINED_TABLE)
                    .pg(format!("{what} \"{name}\" does not exist"))
                    .unplaced());
            }
        }
    }
    Ok(Bound::DropTable(DropTable { names, kind, cascade: written.cascade, missing }))
}

/// Recognises `CALL enable_logging(...)` and the other spellings of the same thing, which are a
/// `SELECT *` over the call with nothing else in the query, and folds the arguments.
///
/// A bare word is text here, the way it is in a `SET`, so `storage=file` names the file storage.
/// The pin warns that it took the word that way and takes it, and rudb takes it without the warning.
///
/// # Errors
///
/// For an argument that does not fold to a constant.
fn call(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    query: ast::QueryRef,
) -> Result<Option<Call>> {
    let written = ast.query(query);
    let ast::QueryBody::Select(select) = written.body else { return Ok(None) };
    let select = ast.select(select);
    let bare = written.ctes.len == 0
        && written.order_by.len == 0
        && written.limit == NONE
        && written.offset == NONE
        && select.distinct == ast::Distinct::No
        && select.filter == NONE
        && select.group_by.len == 0
        && select.having == NONE
        && select.qualify == NONE
        && select.from.len == 1;
    let [target] = ast.target_list(select.targets) else { return Ok(None) };
    if !bare || !matches!(ast.expr(target.expr), ast::Expr::Star { .. }) {
        return Ok(None);
    }
    let ast::Source::Function { name, args, .. } = ast.source(ast.source_list(select.from)[0])
    else {
        return Ok(None);
    };
    let parts: Vec<&str> = ast.name(name).collect();
    let [function] = parts.as_slice() else { return Ok(None) };
    let Some(&function) = CALLS.iter().find(|known| known.eq_ignore_ascii_case(function)) else {
        return Ok(None);
    };
    let mut out = Call { name: function.to_string(), positional: Vec::new(), named: Vec::new() };
    for argument in ast.target_list(args) {
        let mut binder = Binder::with(catalog, parameters, session);
        let bound = binder.bind_setting_value(ast, argument.expr)?;
        let value = if let Expr::Constant(value) = *binder.plan().expr(bound) {
            binder.plan().value(value).clone()
        } else if let Some(value) = crate::fold::value_with_lambdas(binder.plan(), bound)? {
            value
        } else {
            return Err(Error::not_implemented(format!(
                "an argument to {function} that is not a constant"
            )));
        };
        if argument.alias == NONE {
            out.positional.push(value);
        } else {
            out.named.push((ast.string(argument.alias).to_string(), value));
        }
    }
    Ok(Some(out))
}

/// Binds a `SET` or a `RESET`, which is resolving its value and nothing else.
///
/// The name is not checked here. The binder knows what tables exist and has no idea what settings
/// exist, since a setting is a knob on the engine rather than an entry in a catalog, and a version
/// of this that held the list would be the binder holding a copy of something it cannot enforce.
fn setting(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::SettingRef,
) -> Result<Bound> {
    let written = ast.setting(index);
    let name = ast.string(written.name).to_string();
    if written.scope == ast::Scope::Variable {
        if written.value == NONE {
            return Ok(Bound::Variable { name, value: None });
        }
        let mut binder = Binder::with(catalog, parameters, session);
        let root = binder.bind_variable_value(ast, written.value)?;
        return Ok(Bound::Variable { name, value: Some(finish(binder, root)?) });
    }
    let value = if written.value == NONE {
        None
    } else {
        let mut binder = Binder::with(catalog, parameters, session);
        let bound = binder.bind_setting_value(ast, written.value)?;
        if !binder.scalar_subqueries.is_empty() {
            return Err(Error::binder("SET value cannot contain subqueries"));
        }
        // The pin evaluates the value when it binds, so `MAP {'operator_casing': 'upper'}` and
        // `1 + 1` are as good as a literal.
        if let Expr::Constant(value) = *binder.plan().expr(bound) {
            Some(binder.plan().value(value).clone())
        } else if let Some(value) = crate::fold::value_with_lambdas(binder.plan(), bound)? {
            Some(value)
        } else {
            return Err(Error::not_implemented(format!(
                "a value for {name} that is not a constant"
            )));
        }
    };
    Ok(Bound::Setting(Setting { name, scope: written.scope, value, pragma: written.pragma }))
}

/// Binds an `ATTACH`, folding the path and every option value to a constant.
fn attach(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::AttachRef,
) -> Result<Bound> {
    let written = ast.attach(index);
    let mut binder = Binder::with(catalog, parameters, session);
    let mut constant = |expr: ast::ExprRef, what: &str| -> Result<Value> {
        let bound = binder.bind_setting_value(ast, expr)?;
        let Expr::Constant(value) = *binder.plan().expr(bound) else {
            return Err(Error::not_implemented(format!("{what} of ATTACH that is not a constant")));
        };
        Ok(binder.plan().value(value).clone())
    };
    let path = match constant(written.path, "a path")? {
        Value::Null => {
            return Err(Error::binder("ATTACH path expression must not evaluate to NULL"));
        }
        value => value.to_string(),
    };
    let names = ast.name(written.names).map(str::to_string).collect::<Vec<_>>();
    let mut options = Vec::with_capacity(names.len());
    for (name, &value) in names.into_iter().zip(ast.expr_list(written.values)) {
        let value = if value == NONE { None } else { Some(constant(value, "an option")?) };
        options.push((name, value));
    }
    let alias = (written.alias != NONE).then(|| ast.string(written.alias).to_string());
    Ok(Bound::Attach(Attach {
        path,
        alias,
        or_replace: written.or_replace,
        if_not_exists: written.if_not_exists,
        options,
    }))
}

/// Sorts an insert's rows into the order the target table declared.
///
/// Returns the input unchanged when the statement supplies none of the declared columns, because
/// every one of them is then a constant null and sorting on a constant is a sort that buys nothing
/// and costs a pass. A statement that supplies some of them sorts on those: the declaration is
/// about the order the rows are written in, and the columns that are there still order them.
///
/// The leading key carries the width. `date_trunc('month', d)` and `d` sort the same rows into the
/// same fragments for any predicate a month wide or wider, and the difference is what happens
/// inside a month: bucketed, the second key orders the whole month, which is the key locality the
/// joins want and the reason the width is part of the declaration at all.
fn clustered(
    binder: &mut Binder<'_>,
    input: rudb_plan::NodeRef,
    scope: &crate::scope::Scope,
    clustering: &Clustering,
    targets: &[usize],
    fields: &[Field],
) -> Result<rudb_plan::NodeRef> {
    let mut keys: Vec<SortKey> = Vec::with_capacity(clustering.columns().len());
    for (at, &column) in clustering.columns().iter().enumerate() {
        let Some(from) = targets.iter().position(|&target| target == column as usize) else {
            continue;
        };
        let source = &scope.columns[from];
        let expr = binder.plan_mut().add_expr(Expr::Column(source.binding), source.ty.clone());
        // Cast to the column's own type before bucketing, since the source of a load is a file
        // whose date column can arrive as a timestamp and `date_trunc` gives back the type it was
        // handed. Sorting on a different type than the column stores would still be an order, but
        // it would not be the order the declaration names.
        let expr = binder.checked_cast_to(expr, &fields[column as usize].ty, false)?;
        let expr =
            if at == 0 { bucketed(binder, expr, clustering.width(), fields, column) } else { expr };
        keys.push(SortKey { expr, descending: false, nulls_first: false });
    }
    if keys.is_empty() {
        return Ok(input);
    }
    let keys = binder.plan_mut().add_sort_keys(&keys);
    Ok(binder.plan_mut().add_node(Node::Sort { input, keys }))
}

/// The declaration with an automatic width turned into the bucket the incoming rows ask for.
///
/// A declaration that named no width says the bucket should come from how many rows a partition
/// would hold, and this is the only place that number is in reach. The rows are the source's, not
/// the target's: a load into an empty table has a target with nothing to count, and the whole case
/// the rule exists for is the first load of a big table. So the count and the range come off the
/// source's own zones, which is the Parquet footer for a file and the directory for a table, and
/// both are already on the plan because the estimator wanted them.
///
/// Everything about this is best effort and that is by design. The three widths hold the same rows
/// and answer the same queries, so guessing wrong costs some pruning or some key locality and
/// cannot cost an answer. A source that is a join, a group by or a values list has no zones to read
/// and gets [`Width::DEFAULT`], which is what the fixed default was before the rule existed.
fn fitted(
    binder: &Binder<'_>,
    scope: &crate::scope::Scope,
    clustering: &Clustering,
    targets: &[usize],
) -> Clustering {
    if clustering.width() != Width::Auto {
        return clustering.clone();
    }
    let Some(from) = targets.iter().position(|&target| target == clustering.partition() as usize)
    else {
        return clustering.fitted(0, 0);
    };
    let source = &scope.columns[from];
    let Some(zones) = binder.plan().sole_zones() else {
        return clustering.fitted(0, 0);
    };
    // By name, and off whichever store the plan reads rather than off the one this column is bound
    // to. The binding points at the projection over the scan, since a load is a projection into the
    // target's types, and following a binding back through a projection is the optimizer's job. A
    // load reads one table or one file, so the store with bounds on it is the store the name is in.
    let Some(at) = zones.column(&source.name) else {
        return clustering.fitted(0, 0);
    };
    let rows = zones.surviving(&[]).unwrap_or(0);
    let days = span(&zones.extreme(at, End::Low), &zones.extreme(at, End::High)).unwrap_or(0);
    clustering.fitted(rows, days)
}

/// How many days a column covers, from the smallest and largest values in it.
///
/// `None` wherever the two do not make a span, which is a column that is entirely null, a store
/// that could not fold its parts into one answer, and a pair of bounds that are not the same shape.
/// All of them mean the same thing here, which is that there is nothing to divide the row count by.
fn span(low: &Stat<ColumnBound>, high: &Stat<ColumnBound>) -> Option<u64> {
    let (Stat::Known { value: low, .. }, Stat::Known { value: high, .. }) = (low, high) else {
        return None;
    };
    let days = match (low, high) {
        // A date is a day count already, which is the common case and the only exact one.
        (ColumnBound::Int(low), ColumnBound::Int(high)) => high.checked_sub(*low)?,
        // A timestamp is a count of seconds at whichever unit the column keeps, so the span is that
        // difference divided by a day's worth of them. A scale wide enough to overflow the divisor
        // is a column no calendar covers and falls out as no span at all.
        (
            ColumnBound::Scaled { unscaled: low, scale: at },
            ColumnBound::Scaled { unscaled: high, scale: to },
        ) if at == to => {
            let day = 86_400_i128.checked_mul(10_i128.checked_pow(u32::from(*at))?)?;
            high.checked_sub(*low)? / day
        }
        _ => return None,
    };
    u64::try_from(days).ok()
}

/// Wraps a sort key in the calendar bucket its declaration asked for.
fn bucketed(
    binder: &mut Binder<'_>,
    expr: ExprRef,
    width: Width,
    fields: &[Field],
    column: u32,
) -> ExprRef {
    if width == Width::Exact {
        return expr;
    }
    let unit = binder.plan_mut().add_value(Value::Varchar(width.to_string().to_lowercase()));
    let unit = binder.plan_mut().add_expr(Expr::Constant(unit), LogicalType::Varchar);
    let args = binder.plan_mut().add_expr_list(&[unit, expr]);
    let name = binder.plan_mut().intern("date_trunc");
    let ty = fields[column as usize].ty.clone();
    binder.plan_mut().add_expr(Expr::Function { name, args }, ty)
}

fn insert(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::InsertRef,
) -> Result<Bound> {
    let written = ast.insert(index);
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = catalog.resolve(&parts)?;
    if catalog.entry(&name)? == Entry::View {
        // The binary's sentence, article and all. A view has no rows of its own to append to, and
        // an updatable view is a rule about rewriting the insert that neither database has.
        return Err(Error::catalog(format!("{} is not an table", name.table)));
    }
    let target = catalog.table(&name)?;
    let fields: Vec<Field> = target.columns().to_vec();
    let clustering = target.clustering().cloned();

    // Which table column each source column lands in. Without a column list that is the first n
    // columns in order, and with one it is whatever the list says, which is also the check that
    // the list names columns the table has and names none of them twice.
    let targets: Vec<usize> = if written.columns.is_empty() {
        (0..fields.len()).filter(|&at| target.generated(at).is_none()).collect()
    } else {
        let compare = session.semantics().identifier_compare();
        let mut targets = Vec::new();
        for (written_at, column) in ast.name(written.columns).enumerate() {
            let span = ast.part_span(written.columns, written_at);
            let at = fields.iter().position(|field| compare.same(&field.name, column)).ok_or_else(
                || {
                    Error::binder(format!(
                        "Table \"{}\" does not have a column with name \"{column}\"",
                        name.table
                    ))
                    .state(SqlState::UNDEFINED_COLUMN)
                    .pg(format!(
                        "column \"{column}\" of relation \"{}\" does not exist",
                        name.table
                    ))
                    .placed_at(span)
                },
            )?;
            if targets.contains(&at) {
                return Err(Error::binder(format!("Duplicate column name \"{column}\" in INSERT"))
                    .state(SqlState::DUPLICATE_COLUMN)
                    .pg(format!("column \"{column}\" specified more than once"))
                    .placed_at(span));
            }
            if target.generated(at).is_some() {
                return Err(Error::binder("Cannot insert into a generated column"));
            }
            targets.push(at);
        }
        targets
    };

    // `OVERRIDING USER VALUE` leaves out what the statement gives an identity column.
    let ignored =
        |at: usize| written.overriding == ast::Overriding::User && target.identity(at).is_some();
    let defaults: Vec<(LogicalType, Option<String>)> = (0..fields.len())
        .map(|at| (fields[at].ty.clone(), target.default(at).map(str::to_owned)))
        .collect();
    let mut binder = Binder::with(catalog, parameters, session);
    let (root, scope) = if written.source == NONE {
        // `DEFAULT VALUES` is one row with nothing in it, and the projection below fills every
        // column with its default.
        (binder.plan_mut().add_node(Node::Dummy), crate::scope::Scope::empty())
    } else {
        // A `DEFAULT` item of a `VALUES` row is the default of the column it lands in, which only
        // this statement knows, so the `VALUES` right under it is told.
        if matches!(ast.query(written.source).body, ast::QueryBody::Values(_)) {
            binder.insert_defaults = Some(targets.iter().map(|&at| defaults[at].clone()).collect());
            if session.semantics().unknown_types() == UnknownTypes::Postgres {
                let oid = |at: usize| {
                    target.declared_type(at).map_or_else(
                        || rudb_pgtypes::pg_type(&fields[at].ty).oid,
                        |declared| declared.oid,
                    )
                };
                binder.insert_inputs = Some(targets.iter().map(|&at| oid(at)).collect());
            }
        }
        // A `COPY t FROM 'file'` reads the file as the columns it lands in, the way DuckDB does,
        // so a value that does not fit is the reader's conversion error on its line rather than a
        // cast failing later with no line to point at. The `read_csv` under it takes the columns.
        if written.copy {
            binder.copy_into = Some(targets.iter().map(|&at| fields[at].clone()).collect());
        }
        binder.unknowns_kept = binder.insert_inputs.is_none();
        let bound = binder.bind_query(ast, written.source)?;
        binder.copy_into = None;
        bound
    };
    let mut targets = if written.source == NONE { Vec::new() } else { targets };
    if scope.len() != targets.len() {
        match session.semantics().insert_columns() {
            // The pin's two sentences, lowercase `table` and all, for without a column list and
            // with one.
            InsertColumns::Exact => {
                return Err(Error::binder(if written.columns.is_empty() {
                    format!(
                        "table \"{}\" has {} columns but {} values were supplied",
                        name.table,
                        targets.len(),
                        scope.len()
                    )
                } else {
                    format!(
                        "Column name/value mismatch for insert on \"{}\": expected {} columns but \
                         {} values were supplied",
                        name.table,
                        targets.len(),
                        scope.len()
                    )
                }));
            }
            // PostgreSQL points at the first value that has no column, or at the first column that
            // has no value.
            InsertColumns::Leading if scope.len() > targets.len() => {
                let error = Error::binder("INSERT has more expressions than target columns")
                    .state(SqlState::SYNTAX_ERROR);
                return Err(match extra_value(ast, written.source, targets.len()) {
                    Some(expr) => error.with_span(ast.expr_span(expr)),
                    None => error.unplaced(),
                });
            }
            InsertColumns::Leading if !written.columns.is_empty() => {
                return Err(Error::binder("INSERT has more target columns than expressions")
                    .state(SqlState::SYNTAX_ERROR)
                    .placed_at(ast.part_span(written.columns, scope.len())));
            }
            // With no column list, the values go to the leading columns and the others take
            // their defaults in the projection below.
            InsertColumns::Leading => targets.truncate(scope.len()),
        }
    }

    // A value for a `GENERATED ALWAYS` identity column is refused unless the statement said
    // `OVERRIDING`, and `DEFAULT` is not a value. `COPY` puts the value in, as in PostgreSQL.
    if !written.copy && written.overriding == ast::Overriding::None {
        for (from, &at) in targets.iter().enumerate() {
            if target.identity(at) == Some(rudb_catalog::Identity::Always)
                && !all_default(ast, written.source, from)
            {
                let column = &fields[at].name;
                return Err(Error::binder(format!(
                    "cannot insert a non-DEFAULT value into column \"{column}\""
                ))
                .state(SqlState::GENERATED_ALWAYS)
                .detail(format!(
                    "Column \"{column}\" is an identity column defined as GENERATED ALWAYS."
                ))
                .hint("Use OVERRIDING SYSTEM VALUE to override.")
                .unplaced());
            }
        }
    }

    // A table that declared what order its rows go in gets the sort here, under the projection
    // rather than over it, because a projection does not reorder rows and the bindings the sort
    // keys need are the ones the query just produced. This is the whole of the loader honouring
    // the declaration: the rows arrive at the writer in order and the per fragment ranges, which
    // are built from whatever order arrives, come out narrow instead of each covering the table.
    let root = match &clustering {
        None => root,
        Some(clustering) => {
            // The width is settled here and not on the table. A declaration that left the bucket to
            // the data is a standing instruction, so it stays on the table as one and every load
            // answers it with the rows that load is carrying. What the sort needs is an answer, and
            // that is what this is.
            let fitted = fitted(&binder, &scope, clustering, &targets);
            clustered(&mut binder, root, &scope, &fitted, &targets, &fields)?
        }
    };

    // The projection that makes the source look exactly like the table. Every column the statement
    // did not name becomes a null of the column's own type, so the append never has to know that a
    // column list was written at all.
    let mut exprs: Vec<ExprRef> = Vec::with_capacity(fields.len());
    let mut names = Vec::with_capacity(fields.len());
    for (at, field) in fields.iter().enumerate() {
        let expr = match targets.iter().position(|&target| target == at).filter(|_| !ignored(at)) {
            Some(from) => {
                let column = &scope.columns[from];
                let expr =
                    binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
                let expr = binder.checked_cast_to(expr, &field.ty, false)?;
                binder.stored(expr, target.declared_type(at))?
            }
            // The column's default, or a null of the column's own type when it has none.
            None => binder.bind_default(defaults[at].1.as_deref(), &field.ty)?,
        };
        exprs.push(expr);
        let interned = binder.plan_mut().intern(&field.name);
        names.push(interned);
    }
    let exprs = binder.plan_mut().add_expr_list(&exprs);
    let names = binder.plan_mut().add_name_list(&names);
    let index = binder.fresh_index();
    let root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    let root = generate(&mut binder, (root, index), &fields, target, 0)?;
    let source = finish(binder, root)?;
    excluded_returning(ast, written.returning)?;
    let returning = returning(ast, catalog, parameters, session, written.returning, false)?;
    let conflict = match written.conflict {
        Some(conflict) => {
            Some(bind_conflict(ast, catalog, parameters, session, &name, &targets, conflict)?)
        }
        None => None,
    };
    let checks = bind_checks(catalog, parameters, session, &name)?;
    Ok(Bound::Insert(Insert {
        name,
        source,
        write: Write::Append,
        returning,
        conflict,
        checks,
        patched: None,
    }))
}

/// Works out the generated columns of rows that have every other column, which is how a generated
/// column is kept: a write stores its value like any other column's.
///
/// The input is `(root, index)`, a projection of the table's `fields` and then `extra` columns the
/// write carries along. Each generated column gets a projection of its own, in the order they
/// depend on each other, so a column that reads another generated column sees its new value.
fn generate(
    binder: &mut Binder<'_>,
    (mut root, mut index): (rudb_plan::NodeRef, u32),
    fields: &[Field],
    table: &rudb_catalog::Table,
    extra: usize,
) -> Result<rudb_plan::NodeRef> {
    if !table.has_generated() {
        return Ok(root);
    }
    let width = fields.len() + extra;
    let types: Vec<LogicalType> = {
        let Node::Project { exprs, .. } = *binder.plan().node(root) else {
            return Err(Error::internal("a write of generated columns that is not a projection"));
        };
        let list = binder.plan().expr_list(exprs);
        list.iter().map(|&expr| binder.plan().expr_type(expr).clone()).collect()
    };
    if types.len() != width {
        return Err(Error::internal("a write of generated columns of the wrong width"));
    }
    for at in generated_order(table)? {
        let text = table.generated(at).unwrap_or_default();
        let (parsed, expr) = check_ast(text)?;
        let mut scope = crate::scope::Scope::empty();
        for (place, field) in fields.iter().enumerate() {
            scope.push(crate::scope::Visible {
                table: String::new(),
                name: field.name.clone(),
                binding: rudb_plan::ColumnBinding::new(index, place as u32),
                ty: types[place].clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        // The pin names the column in any error that working its value out raises, a cast to a
        // type there is no cast to among them, and that is one the binder raises here.
        let field = &fields[at];
        let column = format!(
            "{} {} AS ({})",
            field.name,
            field.ty,
            table.generation(at).unwrap_or_default()
        );
        let value = binder
            .bind_expr(&parsed, expr, &scope)
            .and_then(|value| binder.checked_cast_to(value, &field.ty, false))
            .map_err(|error| rudb_plan::incorrect_generated(&column, error))?;
        let ty = binder.plan().expr_type(value).clone();
        let name = binder.add_constant(Value::Varchar(column));
        let args = binder.plan_mut().add_expr_list(&[value, name]);
        let function = binder.plan_mut().intern(rudb_plan::GENERATED);
        let value = binder.add_expr(Expr::Function { name: function, args }, ty);
        let mut exprs = Vec::with_capacity(width);
        let mut names = Vec::with_capacity(width);
        for (place, ty) in types.iter().enumerate() {
            exprs.push(if place == at {
                value
            } else {
                let binding = rudb_plan::ColumnBinding::new(index, place as u32);
                binder.plan_mut().add_expr(Expr::Column(binding), ty.clone())
            });
            let name = fields.get(place).map_or("changed", |field| field.name.as_str());
            names.push(binder.plan_mut().intern(name));
        }
        let exprs = binder.plan_mut().add_expr_list(&exprs);
        let names = binder.plan_mut().add_name_list(&names);
        index = binder.fresh_index();
        root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    }
    Ok(root)
}

/// The generated columns of a table in an order where each comes after every generated column it
/// reads. The table refused a circle when it was made, so one is an internal error here.
fn generated_order(table: &rudb_catalog::Table) -> Result<Vec<usize>> {
    let fields = table.columns();
    let mut waiting = Vec::new();
    for at in 0..fields.len() {
        if let Some(text) = table.generated(at) {
            let mut reads = Vec::new();
            for used in columns_in(text)? {
                if let Some(place) = fields.iter().position(|field| same_name(&field.name, &used))
                    && place != at
                    && table.generated(place).is_some()
                {
                    reads.push(place);
                }
            }
            waiting.push((at, reads));
        }
    }
    let mut order = Vec::with_capacity(waiting.len());
    while !waiting.is_empty() {
        let Some(ready) =
            waiting.iter().position(|(_, reads)| reads.iter().all(|read| order.contains(read)))
        else {
            return Err(Error::internal("generated columns that read each other in a circle"));
        };
        order.push(waiting.remove(ready).0);
    }
    Ok(order)
}

/// The rows of a table with its generated columns worked out again, for an `ON CONFLICT DO UPDATE`
/// that changed the columns they read.
fn regenerate(
    ast: &Ast,
    (catalog, parameters, session): (&Catalog, &Parameters, &Session),
    name: &QualifiedName,
) -> Result<Plan> {
    let table = catalog.table(name)?;
    let fields = table.columns();
    let mut binder = Binder::with(catalog, parameters, session);
    let (root, scope) =
        binder.bind_catalog_table(ast, name, name.table.clone(), ast::Slice::default())?;
    let mut exprs = Vec::with_capacity(fields.len());
    let mut names = Vec::with_capacity(fields.len());
    for field in fields {
        let column = scope
            .columns
            .iter()
            .find(|column| !column.hidden && same_name(&column.name, &field.name))
            .ok_or_else(|| Error::internal("a table column its own scan does not have"))?;
        exprs.push(binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone()));
        names.push(binder.plan_mut().intern(&field.name));
    }
    let exprs = binder.plan_mut().add_expr_list(&exprs);
    let names = binder.plan_mut().add_name_list(&names);
    let index = binder.fresh_index();
    let root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    let root = generate(&mut binder, (root, index), fields, table, 0)?;
    finish(binder, root)
}

/// The value at `at` in the first row of the source of an `INSERT`, when the AST shows it. A star
/// in the target list hides which value is where, so there is none then.
fn extra_value(ast: &Ast, source: ast::QueryRef, at: usize) -> Option<ast::ExprRef> {
    match ast.query(source).body {
        ast::QueryBody::Values(rows) => {
            let first = *ast.rows(rows).first()?;
            ast.expr_list(first).get(at).copied()
        }
        ast::QueryBody::Select(select) => {
            let targets = ast.target_list(ast.select(select).targets);
            if targets.iter().any(|target| matches!(ast.expr(target.expr), ast::Expr::Star { .. }))
            {
                return None;
            }
            targets.get(at).map(|target| target.expr)
        }
        _ => None,
    }
}

/// Whether every row of an `INSERT ... VALUES` writes `DEFAULT` for the value at `from`.
fn all_default(ast: &Ast, source: ast::QueryRef, from: usize) -> bool {
    if source == NONE {
        return true;
    }
    let ast::QueryBody::Values(rows) = ast.query(source).body else { return false };
    ast.rows(rows).iter().all(|&row| {
        ast.expr_list(row)
            .get(from)
            .is_some_and(|&expr| matches!(ast.expr(expr), ast::Expr::Default))
    })
}

/// Which key an `ON CONFLICT` is about and what it does, refused the way the pin refuses one that
/// names no key or leaves which key open when that matters.
fn bind_conflict(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    name: &QualifiedName,
    targets: &[usize],
    conflict: ast::Conflict,
) -> Result<Conflict> {
    let table = catalog.table(name)?;
    let fields = table.columns();
    // A unique index is a conflict target as a key is, which is how the pin counts them too.
    let keys = table.guards();
    let arbiter = session.semantics().conflict_arbiter();
    // A PostgreSQL session refuses a target that matches no key only after the action is read.
    let mut unmatched = None;
    let key = if conflict.target.is_empty() {
        if keys.is_empty() && arbiter == ConflictArbiter::Pin {
            return Err(Error::binder(
                "There are no UNIQUE/PRIMARY KEY constraints that refer to this table, specify ON \
                 CONFLICT columns manually",
            ));
        }
        match conflict.action {
            ast::ConflictAction::Nothing => None,
            _ if keys.len() > 1 => {
                return Err(Error::binder(
                    "Conflict target has to be provided for a DO UPDATE operation when the table \
                     has multiple UNIQUE/PRIMARY KEY constraints",
                ));
            }
            _ => Some(0),
        }
    } else {
        let compare = session.semantics().identifier_compare();
        let mut wanted = Vec::new();
        for (written_at, column) in ast.name(conflict.target).enumerate() {
            let Some(at) = fields.iter().position(|field| compare.same(&field.name, column)) else {
                return Err(Error::binder(format!(
                    "Table \"{}\" does not have a column with name \"{column}\"",
                    name.table
                ))
                .state(SqlState::UNDEFINED_COLUMN)
                .pg(format!("column \"{column}\" does not exist"))
                .placed_at(ast.part_span(conflict.target, written_at)));
            };
            wanted.push(at);
        }
        wanted.sort_unstable();
        wanted.dedup();
        let found = keys.iter().position(|key| {
            let mut held = key.columns.clone();
            held.sort_unstable();
            held == wanted
        });
        if found.is_none() {
            let error = Error::binder(
                "The specified columns as conflict target are not referenced by a UNIQUE/PRIMARY \
                 KEY CONSTRAINT or INDEX",
            )
            .state(SqlState::INVALID_COLUMN_REFERENCE)
            .pg("there is no unique or exclusion constraint matching the ON CONFLICT specification")
            .unplaced();
            match arbiter {
                ConflictArbiter::Pin => return Err(error),
                ConflictArbiter::Postgres => unmatched = Some(error),
            }
        }
        found
    };
    let action = match conflict.action {
        ast::ConflictAction::Nothing => ConflictAction::Nothing,
        // The row that replaces a held one brings its generated columns, worked out from it.
        ast::ConflictAction::Replace => ConflictAction::Replace(
            targets
                .iter()
                .copied()
                .chain((0..fields.len()).filter(|&at| table.generated(at).is_some()))
                .collect(),
        ),
        ast::ConflictAction::Update { columns: written, query } => {
            let compare = session.semantics().identifier_compare();
            let mut columns = Vec::new();
            for (written_at, column) in ast.name(written).enumerate() {
                let Some(at) = fields.iter().position(|field| compare.same(&field.name, column))
                else {
                    let span = ast.part_span(written, written_at);
                    return Err(missing_update_column(column, &name.table, span));
                };
                if columns.contains(&at) {
                    return Err(repeated_update_column(column));
                }
                if table.generated(at).is_some() {
                    return Err(Error::binder(format!(
                        "Cant update column \"{column}\" because it is a generated column!"
                    )));
                }
                columns.push(at);
            }
            let defaulted = conflict_defaults(ast, query)?;
            let mut binder = Binder::with(catalog, parameters, session);
            binder.upsert = true;
            binder.default_as_null = defaulted.contains(&true);
            let (root, scope) = binder.bind_query(ast, query)?;
            binder.default_as_null = false;
            // Each value is cast to its column's type here, so the write only has to place it,
            // and the condition is cast to a boolean, so the write only has to test it. A value
            // that is `DEFAULT` is its column's default instead.
            let mut exprs = Vec::with_capacity(scope.columns.len());
            let mut names = Vec::with_capacity(scope.columns.len());
            for (at, column) in scope.columns.iter().enumerate() {
                let ty = columns.get(at).map_or(LogicalType::Boolean, |&to| fields[to].ty.clone());
                if defaulted.get(at) == Some(&true) {
                    exprs.push(binder.bind_default(table.default(columns[at]), &ty)?);
                    names.push(binder.plan_mut().intern(&column.name));
                    continue;
                }
                let expr =
                    binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
                exprs.push(binder.checked_cast_to(expr, &ty, false)?);
                names.push(binder.plan_mut().intern(&column.name));
            }
            let exprs = binder.plan_mut().add_expr_list(&exprs);
            let names = binder.plan_mut().add_name_list(&names);
            let index = binder.fresh_index();
            let root =
                binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
            let plan = Box::new(finish(binder, root)?);
            let generate = if table.has_generated() {
                Some(Box::new(regenerate(ast, (catalog, parameters, session), name)?))
            } else {
                None
            };
            ConflictAction::Update { columns, plan, generate }
        }
    };
    if let Some(error) = unmatched {
        return Err(error);
    }
    Ok(Conflict { key, action })
}

/// Refuses what the pin refuses in a `DO UPDATE`, and says which of its values are a bare
/// `DEFAULT`. A subquery is refused anywhere in it, a `DEFAULT` in its `WHERE` or inside a value,
/// and a name qualified with `excluded` when the table is written with that alias as well.
fn conflict_defaults(ast: &Ast, query: ast::QueryRef) -> Result<Vec<bool>> {
    let ast::QueryBody::Select(select) = ast.query(query).body else { return Ok(Vec::new()) };
    let select = ast.select(select);
    let aliased = ast.source_list(select.from).iter().any(|&source| {
        let ast::Source::Join { left, .. } = ast.source(source) else { return false };
        matches!(ast.source(left), ast::Source::Table { alias, .. }
            if alias != NONE && same_name(ast.string(alias), "excluded"))
    });
    let targets = ast.target_list(select.targets);
    let mut defaulted = Vec::with_capacity(targets.len());
    for (at, target) in targets.iter().enumerate() {
        // The condition is the last item, after the values.
        let condition = at + 1 == targets.len();
        let bare = !condition && matches!(ast.expr(target.expr), ast::Expr::Default);
        defaulted.push(bare);
        let mut stack = if bare { Vec::new() } else { vec![target.expr] };
        while let Some(expr) = stack.pop() {
            match ast.expr(expr) {
                ast::Expr::Subquery { .. }
                | ast::Expr::Exists { .. }
                | ast::Expr::InSubquery { .. }
                | ast::Expr::QuantifiedSubquery { .. } => {
                    return Err(Error::binder("DO UPDATE SET clause cannot contain a subquery"));
                }
                ast::Expr::Default if condition => {
                    return Err(Error::binder("WHERE clause cannot contain DEFAULT clause"));
                }
                ast::Expr::Default => {
                    return Err(Error::not_implemented(
                        "Unimplemented expression class in ExpressionBinder::BindExpression: \
                         DEFAULT",
                    ));
                }
                ast::Expr::Column { name }
                    if aliased
                        && ast.name(name).count() > 1
                        && ast
                            .name(name)
                            .next()
                            .is_some_and(|first| same_name(first, "excluded")) =>
                {
                    return Err(Error::binder(
                        "Ambiguous reference to table \"excluded\" (duplicate alias \"excluded\", \
                         explicitly alias one of the tables using \"AS my_alias\")",
                    ));
                }
                _ => {}
            }
            stack.extend(ast.children(expr));
        }
    }
    defaulted.pop();
    Ok(defaulted)
}

/// Refuses a `RETURNING` of an `INSERT` that reads the `excluded` row, the way the pin does. That
/// is a column qualified with `excluded`, and any column at all of a table that is itself called
/// `excluded`, which the pin takes for the same thing.
fn excluded_returning(ast: &Ast, query: Option<ast::QueryRef>) -> Result<()> {
    let Some(query) = query else { return Ok(()) };
    let ast::QueryBody::Select(select) = ast.query(query).body else { return Ok(()) };
    let select = ast.select(select);
    let called = ast.source_list(select.from).iter().any(|&source| match ast.source(source) {
        ast::Source::Table { name, alias, .. } => {
            let name = if alias == NONE {
                ast.name(name).last().unwrap_or_default()
            } else {
                ast.string(alias)
            };
            same_name(name, "excluded")
        }
        _ => false,
    });
    let mut stack: Vec<ast::ExprRef> =
        ast.target_list(select.targets).iter().map(|target| target.expr).collect();
    while let Some(expr) = stack.pop() {
        let reads = match ast.expr(expr) {
            ast::Expr::Column { name } => {
                called
                    || (ast.name(name).count() > 1
                        && ast.name(name).next().is_some_and(|first| same_name(first, "excluded")))
            }
            ast::Expr::Star { .. } => called,
            _ => false,
        };
        if reads {
            return Err(Error::not_implemented(
                "'excluded' qualified columns are not supported in the RETURNING clause yet",
            ));
        }
        stack.extend(ast.children(expr));
    }
    Ok(())
}

/// An `UPDATE` or a `DELETE`, bound to the query that produces every row the table has afterwards.
///
/// The source the transform built is `SELECT *, condition, values... FROM table`. A row the
/// condition holds for gets the new values in the named columns for an `UPDATE`, and every other
/// row comes through as it was. A null condition is a row that did not match, which is what a
/// searched `CASE` does with one, so the one expression covers both. After the table's columns
/// comes the flag saying which rows matched, which are the rows an `UPDATE` changed and the rows a
/// `DELETE` takes out.
/// The error for a `SET` of a column that the table does not have, at the column when the
/// transform kept its place.
fn missing_update_column(column: &str, table: &str, span: Option<Span>) -> Error {
    Error::binder(format!("Referenced update column {column} not found in table!"))
        .state(SqlState::UNDEFINED_COLUMN)
        .pg(format!("column \"{column}\" of relation \"{table}\" does not exist"))
        .placed_at(span)
}

/// The error for a `SET` that names one column two times. The DuckDB text has the doubled quotes.
fn repeated_update_column(column: &str) -> Error {
    Error::binder(format!("Multiple assignments to same column \"\"{column}\"\""))
        .state(SqlState::SYNTAX_ERROR)
        .pg(format!("multiple assignments to same column \"{column}\""))
        .unplaced()
}

fn change(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    index: ast::InsertRef,
    delete: bool,
) -> Result<Bound> {
    let written = ast.insert(index);
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = catalog.resolve(&parts)?;
    if catalog.entry(&name)? == Entry::View {
        return Err(Error::binder(if delete {
            "Can only delete from base table"
        } else {
            "Can only update base table"
        }));
    }
    let fields: Vec<Field> = catalog.table(&name)?.columns().to_vec();
    let compare = session.semantics().identifier_compare();
    let mut targets: Vec<usize> = Vec::new();
    for (written_at, column) in ast.name(written.columns).enumerate() {
        let at =
            fields.iter().position(|field| compare.same(&field.name, column)).ok_or_else(|| {
                missing_update_column(
                    column,
                    &name.table,
                    ast.part_span(written.columns, written_at),
                )
            })?;
        if catalog.table(&name)?.generated(at).is_some() {
            return Err(Error::binder(format!(
                "Cant update column \"{column}\" because it is a generated column!"
            )));
        }
        if targets.contains(&at) {
            return Err(repeated_update_column(column));
        }
        targets.push(at);
    }

    // Which assignments are `SET c = DEFAULT`, which are the last items of the source's list.
    let mut defaulted = vec![false; targets.len()];
    if let ast::QueryBody::Select(select) = ast.query(written.source).body {
        let items = ast.target_list(ast.select(select).targets);
        let first = items.len().saturating_sub(targets.len());
        for (at, item) in items[first..].iter().enumerate() {
            defaulted[at] = matches!(ast.expr(item.expr), ast::Expr::Default);
        }
    }
    let table = catalog.table(&name)?;
    for (from, &at) in targets.iter().enumerate() {
        if table.identity(at) == Some(rudb_catalog::Identity::Always) && !defaulted[from] {
            let column = &fields[at].name;
            return Err(Error::binder(format!(
                "column \"{column}\" can only be updated to DEFAULT"
            ))
            .state(SqlState::GENERATED_ALWAYS)
            .detail(format!(
                "Column \"{column}\" is an identity column defined as GENERATED ALWAYS."
            ))
            .unplaced());
        }
    }
    let mut binder = Binder::with(catalog, parameters, session);
    binder.default_as_null = defaulted.contains(&true);
    binder.unknowns_kept = true;
    let (root, scope) = binder.bind_query(ast, written.source)?;
    binder.default_as_null = false;
    let width = fields.len();
    if scope.len() != width + 1 + targets.len() {
        return Err(Error::internal(format!(
            "an UPDATE source of {} columns over a table of {width}",
            scope.len()
        )));
    }
    picked_first(&mut binder, root, width, scope.len());
    let column = |binder: &mut Binder<'_>, at: usize| {
        let column = &scope.columns[at];
        binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone())
    };
    let hit = column(&mut binder, width);
    let hit = binder.checked_cast_to(hit, &LogicalType::Boolean, false)?;
    let returning = returning(ast, catalog, parameters, session, written.returning, delete)?;
    // A delete that marks its rows gone needs only which rows those are, so the source reads the
    // columns of the condition and not the rest. See [`Catalog::takes_rows`].
    // A trigger reads every column of the rows it changed, so a statement that fires one keeps them.
    // A generated column is worked out again from the whole new row, so an update of a table with
    // one reads every column.
    let narrow = returning.is_none()
        && parameters.capture().is_none()
        && catalog.takes_rows(&name)
        && (delete || !table.has_generated());
    // An update that writes its rows beside the file reads the new values and not the old ones,
    // so a column it does not set is not read at all.
    let patched = (narrow && !delete).then(|| targets.clone());
    let mut exprs = Vec::with_capacity(width);
    let mut names = Vec::with_capacity(width);
    for (from, &at) in targets.iter().enumerate().filter(|_| patched.is_some()) {
        let field = &fields[at];
        let then = if defaulted[from] {
            binder.bind_default(table.default(at), &field.ty)?
        } else {
            let new = column(&mut binder, width + 1 + from);
            let new = binder.checked_cast_to(new, &field.ty, false)?;
            binder.stored(new, table.declared_type(at))?
        };
        let arms = binder.plan_mut().add_arms(&[Arm { when: hit, then }]);
        let null = binder.add_constant(Value::Null);
        let otherwise = Some(binder.checked_cast_to(null, &field.ty, false)?);
        exprs.push(binder.plan_mut().add_expr(Expr::Case { arms, otherwise }, field.ty.clone()));
        let interned = binder.plan_mut().intern(&field.name);
        names.push(interned);
    }
    for (at, field) in fields.iter().enumerate().filter(|_| !narrow) {
        let old = column(&mut binder, at);
        let expr = match targets.iter().position(|&target| target == at) {
            Some(from) => {
                let then = if defaulted[from] {
                    binder.bind_default(table.default(at), &field.ty)?
                } else {
                    let new = column(&mut binder, width + 1 + from);
                    let new = binder.checked_cast_to(new, &field.ty, false)?;
                    binder.stored(new, table.declared_type(at))?
                };
                let arms = binder.plan_mut().add_arms(&[Arm { when: hit, then }]);
                binder
                    .plan_mut()
                    .add_expr(Expr::Case { arms, otherwise: Some(old) }, field.ty.clone())
            }
            None => old,
        };
        exprs.push(expr);
        let interned = binder.plan_mut().intern(&field.name);
        names.push(interned);
    }
    // The flag is true only where the condition is, so a row whose condition is null is left
    // alone the way a `WHERE` leaves it out.
    let yes = binder.add_constant(Value::Boolean(true));
    let arms = binder.plan_mut().add_arms(&[Arm { when: hit, then: yes }]);
    let otherwise = Some(binder.add_constant(Value::Boolean(false)));
    exprs.push(binder.plan_mut().add_expr(Expr::Case { arms, otherwise }, LogicalType::Boolean));
    let interned = binder.plan_mut().intern("changed");
    names.push(interned);
    let exprs = binder.plan_mut().add_expr_list(&exprs);
    let names = binder.plan_mut().add_name_list(&names);
    let index = binder.fresh_index();
    let root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    let root = if delete { root } else { generate(&mut binder, (root, index), &fields, table, 1)? };
    let source = finish(binder, root)?;
    let write = if delete { Write::Delete } else { Write::Update };
    let checks = if delete { None } else { bind_checks(catalog, parameters, session, &name)? };
    Ok(Bound::Insert(Insert { name, source, write, returning, conflict: None, checks, patched }))
}

/// Has each value of an `UPDATE` source worked out only for the rows its condition picks, as
/// `CASE WHEN condition THEN value END`.
///
/// The source is `SELECT *, condition, values... FROM table` over every row, since the rows the
/// condition leaves out are written back as they were, and a value worked out for every row fails
/// on rows the statement never meant to change. `SET i = s::INTEGER WHERE s SIMILAR TO '[0-9]+'`
/// would fail on the first `s` that is not a number, and `SET n = n + 1 WHERE false` on an `n`
/// at the top of its type. The pin works the values out for the rows the `WHERE` keeps and for
/// nothing else. A column, a constant or a parameter cannot fail and is left as it is, and so is
/// every value when the condition is not a plain boolean or calls something volatile, which asked
/// a second time could pick a row the flag did not. `count` is how many columns the source has,
/// and a root that is not the projection of them is left alone.
fn picked_first(binder: &mut Binder<'_>, root: rudb_plan::NodeRef, width: usize, count: usize) {
    let Node::Project { exprs, .. } = *binder.plan().node(root) else { return };
    let mut values = binder.plan().expr_list(exprs).to_vec();
    let Some(&hit) = values.get(width).filter(|_| values.len() == count) else { return };
    let plan = binder.plan();
    if *plan.expr_type(hit) != LogicalType::Boolean || crate::expr::volatile(plan, hit) {
        return;
    }
    let mut changed = false;
    for value in values.iter_mut().skip(width + 1) {
        if matches!(binder.plan().expr(*value), Expr::Column(_) | Expr::Constant(_)) {
            continue;
        }
        let ty = binder.plan().expr_type(*value).clone();
        let arms = binder.plan_mut().add_arms(&[Arm { when: hit, then: *value }]);
        *value = binder.plan_mut().add_expr(Expr::Case { arms, otherwise: None }, ty);
        changed = true;
    }
    if changed {
        let list = binder.plan_mut().add_expr_list(&values);
        if let Node::Project { exprs, .. } = binder.plan_mut().node_mut(root) {
            *exprs = list;
        }
    }
}
