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
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_parse::ast::{self, Ast};
use rudb_parse::{NONE, parse_ast};
use rudb_plan::{Expr, ExprRef, Node, Plan};

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
    /// `INSERT INTO`.
    Insert(Insert),
    /// `SET name = value`, or `RESET name`, which is the same thing with no value.
    Setting(Setting),
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
    /// The column names the statement gave, which rename a prefix of what the body produces.
    pub aliases: Vec<String>,
    /// Whether an existing entry of that name is left alone rather than being an error.
    pub if_not_exists: bool,
    /// Whether an existing entry of that name is dropped first.
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
}

/// A bound `INSERT`.
#[derive(Debug)]
pub struct Insert {
    /// The table to append to.
    pub name: QualifiedName,
    /// The rows to append. The output is the table's columns, in the table's order, with the
    /// table's types, so nothing between here and the append has a decision left to make.
    pub source: Plan,
}

/// Binds one parsed statement against a catalog.
///
/// # Errors
///
/// If the script does not hold exactly one statement, if a name does not resolve, if a type does
/// not work out, or if the statement uses something that is not bound yet.
pub fn bind_statement(ast: &Ast, catalog: &Catalog) -> Result<Bound> {
    bind_statement_with(ast, catalog, &Parameters::new())
}

/// Binds one parsed statement against a catalog, with values for its parameters.
///
/// This is the prepared statement path. The statement is parsed once and bound once per set of
/// values, so a parameter is a constant by the time the plan exists and everything after the binder
/// sees an ordinary query. That is why there is no parameter in `rudb_plan::Expr`.
///
/// # Errors
///
/// Everything [`bind_statement`] reports, plus an error for a parameter that was given no value.
pub fn bind_statement_with(ast: &Ast, catalog: &Catalog, parameters: &Parameters) -> Result<Bound> {
    let statement = match ast.statements.as_slice() {
        [statement] => *statement,
        [] => return Err(Error::binder("no statement to bind")),
        _ => return Err(Error::not_implemented("a script of more than one statement")),
    };
    match statement {
        ast::Statement::Query(query) => {
            let mut binder = Binder::with(catalog, parameters);
            let (root, _) = binder.bind_query(ast, query)?;
            Ok(Bound::Query(finish(binder, root)?))
        }
        ast::Statement::CreateTable(index) => create_table(ast, catalog, parameters, index),
        ast::Statement::CreateView(index) => create_view(ast, catalog, parameters, index),
        ast::Statement::DropTable(index) => drop_table(ast, catalog, index),
        ast::Statement::Insert(index) => insert(ast, catalog, parameters, index),
        ast::Statement::Set(index) | ast::Statement::Reset(index) => {
            setting(ast, catalog, parameters, index)
        }
    }
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
    index: ast::CreateTableRef,
) -> Result<Bound> {
    let written = ast.create_table(index);
    if written.temporary {
        // A temporary table lives in the `temp` catalog and is dropped when the connection goes,
        // and there is neither a `temp` catalog nor a connection yet. Making one in `memory` that
        // never goes away would answer a later `SELECT` with rows DuckDB would not have.
        return Err(Error::not_implemented("CREATE TEMPORARY TABLE"));
    }
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = catalog.resolve_for_create(&parts)?;
    let defs = ast.column_defs(written.columns);
    let (columns, source) = if written.query == NONE {
        let mut columns = Vec::with_capacity(defs.len());
        for def in defs {
            let text = ast.string(def.ty);
            if text.is_empty() {
                return Err(Error::binder(format!(
                    "Column \"{}\" was declared without a type",
                    ast.string(def.name)
                )));
            }
            let ty = LogicalType::parse(text)?;
            let column = ast.string(def.name);
            columns.push(if def.not_null {
                Field::required(column, ty)
            } else {
                Field::new(column, ty)
            });
        }
        (columns, None)
    } else {
        let mut binder = Binder::with(catalog, parameters);
        let (root, scope) = binder.bind_query(ast, written.query)?;
        if defs.len() > scope.len() {
            // DuckDB's sentence, typo and all. A column list shorter than the query is fine and
            // renames a prefix, so only this direction is an error.
            return Err(Error::binder("Target table has more colum names than query result."));
        }
        let mut columns = Vec::with_capacity(scope.len());
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
    Ok(Bound::CreateTable(CreateTable {
        name,
        columns,
        source,
        if_not_exists: written.if_not_exists,
        or_replace: written.or_replace,
    }))
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
    index: ast::CreateViewRef,
) -> Result<Bound> {
    let written = ast.create_view(index);
    if written.temporary {
        // Same reason as a temporary table: there is no `temp` catalog and no connection for one to
        // belong to, and a view in `memory` that never goes away is not the thing that was asked
        // for.
        return Err(Error::not_implemented("CREATE TEMPORARY VIEW"));
    }
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = catalog.resolve_for_create(&parts)?;
    let aliases: Vec<String> = ast.name(written.columns).map(str::to_string).collect();

    let mut binder = Binder::with(catalog, parameters);
    let (_, scope) = binder.bind_query(ast, written.query)?;
    if aliases.len() > scope.len() {
        return Err(Error::binder("More VIEW aliases than columns in query result"));
    }

    Ok(Bound::CreateView(CreateView {
        name,
        sql: ast.string(written.sql).to_string(),
        aliases,
        if_not_exists: written.if_not_exists,
        or_replace: written.or_replace,
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
    index: ast::SettingRef,
) -> Result<Bound> {
    let written = ast.setting(index);
    let name = ast.string(written.name).to_string();
    let value = if written.value == NONE {
        None
    } else {
        let mut binder = Binder::with(catalog, parameters);
        let bound = binder.bind_setting_value(ast, written.value)?;
        let Expr::Constant(value) = *binder.plan().expr(bound) else {
            return Err(Error::not_implemented(format!(
                "a value for {name} that is not a constant"
            )));
        };
        Some(binder.plan().value(value).clone())
    };
    Ok(Bound::Setting(Setting { name, scope: written.scope, value }))
}

fn insert(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
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
    let fields: Vec<Field> = catalog.table(&name)?.columns().to_vec();

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

    let mut binder = Binder::with(catalog, parameters);
    let (root, scope) = binder.bind_query(ast, written.source)?;
    if scope.len() != targets.len() {
        return Err(Error::binder(format!(
            "Table \"{}\" has {} columns but {} values were supplied",
            name.table,
            targets.len(),
            scope.len()
        )));
    }

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
                binder.cast_to(expr, &field.ty)
            }
            None => {
                // A typed null rather than `add_constant`, which would give it the null type and
                // make the column's type depend on whether a row happened to be inserted into it.
                let value = binder.plan_mut().add_value(Value::Null);
                binder.plan_mut().add_expr(Expr::Constant(value), field.ty.clone())
            }
        };
        exprs.push(expr);
        let interned = binder.plan_mut().intern(&field.name);
        names.push(interned);
    }
    let exprs = binder.plan_mut().add_expr_list(&exprs);
    let names = binder.plan_mut().add_name_list(&names);
    let index = binder.fresh_index();
    let root = binder.plan_mut().add_node(Node::Project { input: root, index, exprs, names });
    Ok(Bound::Insert(Insert { name, source: finish(binder, root)? }))
}
