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
    Bound as ColumnBound, Clustering, Error, Field, LogicalType, Result, Session, Stat, Value,
    Width,
};
use rudb_parse::ast::{self, Ast};
use rudb_parse::{NONE, deparse, parse_ast};
use rudb_plan::{Arm, Expr, ExprRef, Node, Plan, SortKey};

use crate::binder::Binder;
use crate::parameters::Parameters;

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
    /// `ALTER TABLE` or `ALTER VIEW`.
    Alter(Alter),
    /// `CREATE INDEX` or `DROP INDEX`.
    Index(IndexChange),
    /// `INSERT INTO`.
    Insert(Insert),
    /// `SET name = value`, or `RESET name`, which is the same thing with no value.
    Setting(Setting),
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
    Explain { plan: Plan, analyze: bool, statistics: bool, codegen: bool },
    /// `COPY ... TO`, a query and how to write what it answers.
    CopyTo(CopyTo),
}

/// A bound `COPY ... TO` a CSV file, with every option read and defaulted.
///
/// The defaults are the pin's: a header line, a comma, a double quote that is its own escape, and
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
    /// The sequences the defaults use, which the table depends on.
    pub sequences: Vec<QualifiedName>,
    /// The SQL of each `CHECK`, in the order written.
    pub checks: Vec<String>,
    /// The foreign keys, in the order written.
    pub foreign: Vec<rudb_catalog::ForeignKey>,
    /// Every constraint, in the order written.
    pub order: Vec<rudb_catalog::Constraint>,
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
}

/// A bound `CREATE TYPE` or `DROP TYPE`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Which of the table's keys a clash is on, or `None` for any of them.
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
fn returning(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
    query: Option<ast::QueryRef>,
) -> Result<Option<Box<Plan>>> {
    let Some(query) = query else { return Ok(None) };
    let mut binder = Binder::with(catalog, parameters, session);
    let (root, _) = binder.bind_query(ast, query)?;
    Ok(Some(Box::new(finish(binder, root)?)))
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

fn bind_one(
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
            let mut binder = Binder::with(catalog, parameters, session);
            binder.outlined = outlined;
            let (root, _) = binder.bind_query(ast, query)?;
            Ok(Bound::Query(finish(binder, root)?))
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
            let name = if written.drop || alter {
                match catalog.resolve_sequence(&parts) {
                    Ok(name) => Some(name),
                    Err(_) if written.quiet => None,
                    Err(error) => return Err(error),
                }
            } else if written.temporary {
                Some(catalog.resolve_for_create_temporary(&parts)?)
            } else {
                Some(catalog.resolve_for_create(&parts)?)
            };
            if alter && name.is_some() {
                let parts: Vec<&str> = ast.name(written.owner).collect();
                owner = Some(catalog.resolve_owner(&parts)?);
            }
            Ok(Bound::Sequence(SequenceChange {
                name,
                drop: written.drop,
                if_not_exists: written.quiet,
                or_replace: written.or_replace,
                cascade: written.cascade,
                options: written.options,
                owner,
            }))
        }
        ast::Statement::Type(index) => {
            let written = ast.type_def(index);
            let parts: Vec<&str> = ast.name(written.name).collect();
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
                let (ty, uses) = written_type(catalog, ast.string(written.ty))?;
                (Some(name), Some(ty), uses)
            };
            Ok(Bound::Type(TypeChange {
                name,
                ty,
                uses,
                if_not_exists: written.quiet,
                or_replace: written.or_replace,
                cascade: written.cascade,
            }))
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
        ast::Statement::Explain { query, analyze, statistics, codegen } => {
            let mut binder = Binder::with(catalog, parameters, session);
            let (root, _) = binder.bind_query(ast, query)?;
            Ok(Bound::Explain { plan: finish(binder, root)?, analyze, statistics, codegen })
        }
        ast::Statement::CopyTo(index) => {
            let copy = &ast.copies[index as usize];
            let mut binder = Binder::with(catalog, parameters, session);
            let (root, _) = binder.bind_query(ast, copy.query)?;
            copy_to(copy, finish(binder, root)?).map(Bound::CopyTo)
        }
    }
}

/// Reads the options of a `COPY ... TO` against the format they are for.
///
/// Only CSV is written. A `.parquet` or `.json` file, or a format named outright, is refused rather
/// than written as CSV under a name that says otherwise. An option the pin takes and this does not
/// is refused by name, and one the pin does not take either gets the first line of its refusal.
fn copy_to(copy: &ast::CopyTo, plan: Plan) -> Result<CopyTo> {
    let lowered = copy.path.to_ascii_lowercase();
    let mut format = if lowered.ends_with(".parquet") {
        "parquet"
    } else if lowered.ends_with(".json") || lowered.ends_with(".ndjson") {
        "json"
    } else {
        "csv"
    }
    .to_string();
    if let Some((_, Some(written))) = copy.options.iter().rev().find(|(name, _)| name == "format") {
        format = written.trim_matches('\'').to_ascii_lowercase();
    }
    if format != "csv" {
        return Err(Error::not_implemented(format!(
            "COPY TO with FORMAT {format} is not supported yet"
        )));
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
    let mut uses = Vec::new();
    let ty = LogicalType::parse_with(text, &mut |parts| {
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let made = catalog.resolve_type(&parts)?;
        uses.push(made.name().clone());
        Some(made.ty().clone())
    })?;
    Ok((ty, uses))
}

/// [`written_type`] for a caller that only wants the type.
pub(crate) fn read_type(catalog: &Catalog, text: &str) -> Result<LogicalType> {
    written_type(catalog, text).map(|(ty, _)| ty)
}

fn finish(binder: Binder<'_>, root: rudb_plan::NodeRef) -> Result<Plan> {
    let mut plan = binder.into_plan();
    plan.set_root(root);
    plan.validate()?;
    Ok(plan)
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
    let (mut columns, source) = if written.query == NONE {
        let mut columns = Vec::with_capacity(defs.len());
        for def in defs {
            let text = ast.string(def.ty);
            if text.is_empty() {
                return Err(Error::binder(format!(
                    "Column \"{}\" was declared without a type",
                    ast.string(def.name)
                )));
            }
            let ty = read_type(catalog, text)?;
            let column = ast.string(def.name);
            columns.push(if def.not_null {
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
        }
        if defs.is_empty() {
            deduplicate(&mut columns);
        }
        (columns, Some(finish(binder, root)?))
    };
    duplicate_check(&columns)?;
    let mut defaults = Vec::with_capacity(defs.len());
    let mut sequences = Vec::new();
    for def in defs {
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
    let mut checks = Vec::new();
    for &expr in ast.expr_list(written.checks) {
        checks.push(check_text(ast, expr, &columns, catalog, parameters, session)?);
    }
    let mut keys = Vec::new();
    for (at, &names) in ast.name_list(written.keys).iter().enumerate() {
        let mut places = Vec::new();
        for wanted in ast.name(names) {
            let Some(place) = columns.iter().position(|field| same_name(&field.name, wanted))
            else {
                return Err(Error::catalog(format!(
                    "table \"{}\" does not have a column named \"{wanted}\"",
                    name.table
                )));
            };
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
        let key = (names.as_slice(), parts.as_slice(), wanted.as_slice());
        foreign.push(foreign_key(catalog, &name, (&columns, &keys), key)?);
    }
    Ok(Bound::CreateTable(CreateTable {
        name,
        columns,
        source,
        if_not_exists: written.if_not_exists,
        or_replace: written.or_replace,
        keys,
        defaults,
        checks,
        foreign,
        sequences,
        order: ast.constraint_list(written.order).iter().map(|&held| constraint(held)).collect(),
    }))
}

/// A constraint the parser kept the place of, as the catalog keeps it.
fn constraint(held: ast::Constraint) -> rudb_catalog::Constraint {
    match held {
        ast::Constraint::Key(at) => rudb_catalog::Constraint::Key(at as usize),
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
    catalog: &Catalog,
    made: &QualifiedName,
    (columns, keys): (&[Field], &[rudb_catalog::Key]),
    (names, parts, wanted): (&[&str], &[&str], &[&str]),
) -> Result<rudb_catalog::ForeignKey> {
    let mut places = Vec::with_capacity(names.len());
    for &wanted in names {
        let Some(place) = columns.iter().position(|field| same_name(&field.name, wanted)) else {
            return Err(Error::binder(format!(
                "Failed to create foreign key: referencing column \"{wanted}\" does not exist"
            )));
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
            let Some(place) = fields.iter().position(|field| same_name(&field.name, column)) else {
                return Err(Error::binder(format!(
                    "Failed to create foreign key: referenced table \"{}\" does not have a column \
                     named \"{column}\"",
                    table.table
                )));
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
            qualified: false,
            also: None,
        });
    }
    match binder.bind_expr(ast, expr, &scope) {
        Err(error) if error.message().starts_with("Referenced column \"") => {
            let column = error.message().split('"').nth(1).unwrap_or_default();
            Err(Error::binder(format!(
                "Table does not contain column \"{column}\" referenced in check constraint!"
            )))
        }
        Err(error) => Err(error),
        Ok(_) if !binder.windows.is_empty() => {
            Err(Error::binder("window functions are not allowed in check constraints"))
        }
        Ok(_) => Ok(deparse::expression(ast, expr)),
    }
}

/// The `CHECK` constraints of a table as the query a write runs over the rows it wrote, or `None`
/// for a table with none.
fn bind_checks(
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
    if kind == Entry::View {
        return Err(Error::catalog("Can only modify view with ALTER VIEW statement"));
    }
    let table = catalog.table(&name)?;
    let fields = table.columns();
    let place = |column: ast::StrRef| {
        fields.iter().position(|field| same_name(&field.name, ast.string(column)))
    };
    let missing = |column: ast::StrRef| {
        let names: Vec<String> = fields.iter().map(|field| format!("\"{}\"", field.name)).collect();
        Error::binder(format!(
            "Table \"{}\" does not have a column with name \"{}\"\n\nDid you mean: {}",
            name.table,
            ast.string(column),
            names.join(", ")
        ))
    };
    let found = |column: ast::StrRef| place(column).ok_or_else(|| missing(column));
    let checks = table.checks();
    let mut rewrite = None;
    let alteration = match written.action {
        ast::AlterAction::Rename { .. } => unreachable!("a rename is handled above"),
        ast::AlterAction::RenameColumn { column, to } => {
            let at = found(column)?;
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
            rudb_catalog::Alteration::RenameColumn { column: at, to: to.to_string(), checks }
        }
        ast::AlterAction::AddColumn { column, quiet } => {
            if quiet && place(column.name).is_some() {
                return nothing(Some(name));
            }
            let ty = read_type(catalog, ast.string(column.ty))?;
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
            rudb_catalog::Alteration::AddColumn { field, default, sequences }
        }
        ast::AlterAction::DropColumn { column, quiet } => {
            let Some(at) = place(column) else {
                return if quiet { nothing(Some(name)) } else { Err(missing(column)) };
            };
            let dropped = fields[at].name.as_str();
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
            rewrite =
                Some(table_rewrite(ast, (catalog, parameters, session), &name, |_, _, out| {
                    out.remove(at);
                    Ok(())
                })?);
            rudb_catalog::Alteration::DropColumn { column: at, checks: kept }
        }
        ast::AlterAction::Default { column, default } => {
            let at = found(column)?;
            let (default, sequences) = if default == NONE {
                (None, Vec::new())
            } else {
                let (text, used) = default_text(ast, default, catalog, parameters, session)?;
                (Some(text), used)
            };
            rudb_catalog::Alteration::Default { column: at, default, sequences }
        }
        ast::AlterAction::NotNull { column, set } => {
            rudb_catalog::Alteration::NotNull { column: found(column)?, set }
        }
        ast::AlterAction::Type { column, ty, using } => {
            let at = found(column)?;
            let changed = fields[at].name.as_str();
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
            let mut target =
                if ty == NONE { None } else { Some(read_type(catalog, ast.string(ty))?) };
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
            let ty = target.ok_or_else(|| Error::internal("an ALTER TYPE that settled no type"))?;
            rudb_catalog::Alteration::Type { column: at, ty }
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
    for &expr in ast.expr_list(written.elements) {
        if let ast::Expr::Column { name: column } = ast.exprs[expr as usize] {
            let column: Vec<&str> = ast.name(column).collect();
            if let [only] = column[..]
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
                )));
            }
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
    let index_name = parts.last().cloned().unwrap_or_default();
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

    Ok(Bound::CreateView(CreateView {
        name,
        sql: ast.string(written.sql).to_string(),
        statement: deparse::create_view(ast, index),
        aliases,
        if_not_exists: written.if_not_exists,
        or_replace: written.or_replace,
        columns: scope.fields(),
    }))
}

fn drop_table(ast: &Ast, catalog: &Catalog, index: ast::DropTableRef) -> Result<Bound> {
    let written = ast.drop_table(index);
    let kind = if written.view { Entry::View } else { Entry::Table };
    let mut names = Vec::new();
    for &name in ast.name_list(written.names) {
        let parts: Vec<&str> = ast.name(name).collect();
        // The statement said which of the two it meant, so a name that is not there is a missing
        // one of those and not a missing table.
        match catalog.resolve_as(&parts, kind) {
            Ok(resolved) => names.push(resolved),
            Err(error) if written.if_exists => drop(error),
            Err(error) => return Err(error),
        }
    }
    Ok(Bound::DropTable(DropTable { names, kind }))
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
    let value = if written.value == NONE {
        None
    } else {
        let mut binder = Binder::with(catalog, parameters, session);
        let bound = binder.bind_setting_value(ast, written.value)?;
        let Expr::Constant(value) = *binder.plan().expr(bound) else {
            return Err(Error::not_implemented(format!(
                "a value for {name} that is not a constant"
            )));
        };
        Some(binder.plan().value(value).clone())
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
        (0..fields.len()).collect()
    } else {
        let mut targets = Vec::new();
        for column in ast.name(written.columns) {
            let at = fields.iter().position(|field| same_name(&field.name, column)).ok_or_else(
                || {
                    Error::binder(format!(
                        "Table \"{}\" does not have a column named \"{column}\"",
                        name.table
                    ))
                },
            )?;
            if targets.contains(&at) {
                return Err(Error::binder(format!(
                    "Column \"{column}\" is named twice in the same INSERT"
                )));
            }
            targets.push(at);
        }
        targets
    };

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
        }
        // A `COPY t FROM 'file'` reads the file as the columns it lands in, the way DuckDB does,
        // so a value that does not fit is the reader's conversion error on its line rather than a
        // cast failing later with no line to point at. The `read_csv` under it takes the columns.
        if written.copy {
            binder.copy_into = Some(targets.iter().map(|&at| fields[at].clone()).collect());
        }
        let bound = binder.bind_query(ast, written.source)?;
        binder.copy_into = None;
        bound
    };
    let targets = if written.source == NONE { Vec::new() } else { targets };
    if scope.len() != targets.len() {
        return Err(Error::binder(format!(
            "Table \"{}\" has {} columns but {} values were supplied",
            name.table,
            targets.len(),
            scope.len()
        )));
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
        let expr = match targets.iter().position(|&target| target == at) {
            Some(from) => {
                let column = &scope.columns[from];
                let expr =
                    binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
                binder.checked_cast_to(expr, &field.ty, false)?
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
    let source = finish(binder, root)?;
    let returning = returning(ast, catalog, parameters, session, written.returning)?;
    let conflict = match written.conflict {
        Some(conflict) => {
            Some(bind_conflict(ast, catalog, parameters, session, &name, &targets, conflict)?)
        }
        None => None,
    };
    let checks = bind_checks(catalog, parameters, session, &name)?;
    Ok(Bound::Insert(Insert { name, source, write: Write::Append, returning, conflict, checks }))
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
    let keys = table.keys();
    let key = if conflict.target.is_empty() {
        if keys.is_empty() {
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
        let mut wanted = Vec::new();
        for column in ast.name(conflict.target) {
            let Some(at) = fields.iter().position(|field| same_name(&field.name, column)) else {
                return Err(Error::binder(format!(
                    "Table \"{}\" does not have a column with name \"{column}\"",
                    name.table
                )));
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
        let Some(found) = found else {
            return Err(Error::binder(
                "The specified columns as conflict target are not referenced by a UNIQUE/PRIMARY \
                 KEY CONSTRAINT or INDEX",
            ));
        };
        Some(found)
    };
    let action = match conflict.action {
        ast::ConflictAction::Nothing => ConflictAction::Nothing,
        ast::ConflictAction::Replace => ConflictAction::Replace(targets.to_vec()),
        ast::ConflictAction::Update { columns: written, query } => {
            let mut columns = Vec::new();
            for column in ast.name(written) {
                let Some(at) = fields.iter().position(|field| same_name(&field.name, column))
                else {
                    return Err(Error::binder(format!(
                        "Referenced update column {column} not found in table!"
                    )));
                };
                if columns.contains(&at) {
                    return Err(Error::binder(format!(
                        "Multiple assignments to same column \"\"{column}\"\""
                    )));
                }
                columns.push(at);
            }
            let mut binder = Binder::with(catalog, parameters, session);
            binder.upsert = true;
            let (root, scope) = binder.bind_query(ast, query)?;
            // Each value is cast to its column's type here, so the write only has to place it,
            // and the condition is cast to a boolean, so the write only has to test it.
            let mut exprs = Vec::with_capacity(scope.columns.len());
            let mut names = Vec::with_capacity(scope.columns.len());
            for (at, column) in scope.columns.iter().enumerate() {
                let expr =
                    binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
                let ty = columns.get(at).map_or(LogicalType::Boolean, |&to| fields[to].ty.clone());
                exprs.push(binder.checked_cast_to(expr, &ty, false)?);
                names.push(binder.plan_mut().intern(&column.name));
            }
            let exprs = binder.plan_mut().add_expr_list(&exprs);
            let names = binder.plan_mut().add_name_list(&names);
            let index = binder.fresh_index();
            let root =
                binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
            ConflictAction::Update { columns, plan: Box::new(finish(binder, root)?) }
        }
    };
    Ok(Conflict { key, action })
}

/// An `UPDATE` or a `DELETE`, bound to the query that produces every row the table has afterwards.
///
/// The source the transform built is `SELECT *, condition, values... FROM table`. A row the
/// condition holds for gets the new values in the named columns for an `UPDATE`, and every other
/// row comes through as it was. A null condition is a row that did not match, which is what a
/// searched `CASE` does with one, so the one expression covers both. After the table's columns
/// comes the flag saying which rows matched, which are the rows an `UPDATE` changed and the rows a
/// `DELETE` takes out.
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
    let mut targets: Vec<usize> = Vec::new();
    for column in ast.name(written.columns) {
        let at =
            fields.iter().position(|field| same_name(&field.name, column)).ok_or_else(|| {
                Error::binder(format!("Referenced update column {column} not found in table!"))
            })?;
        if targets.contains(&at) {
            return Err(Error::binder(format!(
                "Multiple assignments to same column \"\"{column}\"\""
            )));
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
    let mut binder = Binder::with(catalog, parameters, session);
    binder.default_as_null = defaulted.contains(&true);
    let (root, scope) = binder.bind_query(ast, written.source)?;
    binder.default_as_null = false;
    let width = fields.len();
    if scope.len() != width + 1 + targets.len() {
        return Err(Error::internal(format!(
            "an UPDATE source of {} columns over a table of {width}",
            scope.len()
        )));
    }
    let column = |binder: &mut Binder<'_>, at: usize| {
        let column = &scope.columns[at];
        binder.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone())
    };
    let hit = column(&mut binder, width);
    let hit = binder.checked_cast_to(hit, &LogicalType::Boolean, false)?;
    let mut exprs = Vec::with_capacity(width);
    let mut names = Vec::with_capacity(width);
    for (at, field) in fields.iter().enumerate() {
        let old = column(&mut binder, at);
        let expr = match targets.iter().position(|&target| target == at) {
            Some(from) => {
                let then = if defaulted[from] {
                    binder.bind_default(table.default(at), &field.ty)?
                } else {
                    let new = column(&mut binder, width + 1 + from);
                    binder.checked_cast_to(new, &field.ty, false)?
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
    let source = finish(binder, root)?;
    let returning = returning(ast, catalog, parameters, session, written.returning)?;
    let write = if delete { Write::Delete } else { Write::Update };
    let checks = if delete { None } else { bind_checks(catalog, parameters, session, &name)? };
    Ok(Bound::Insert(Insert { name, source, write, returning, conflict: None, checks }))
}
