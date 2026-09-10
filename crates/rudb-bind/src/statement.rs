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

use rudb_catalog::{Catalog, QualifiedName, same_name};
use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_parse::ast::{self, Ast};
use rudb_parse::{NONE, parse_ast};
use rudb_plan::{Expr, ExprRef, Node, Plan};

use crate::binder::Binder;

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
    /// `DROP TABLE`.
    DropTable(DropTable),
    /// `INSERT INTO`.
    Insert(Insert),
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

/// A bound `DROP TABLE`.
#[derive(Debug)]
pub struct DropTable {
    /// The tables to drop, already resolved. With `IF EXISTS` a name that does not resolve is not
    /// in here at all, which is what makes running this a sequence of drops that cannot fail.
    pub names: Vec<QualifiedName>,
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
    let statement = match ast.statements.as_slice() {
        [statement] => *statement,
        [] => return Err(Error::binder("no statement to bind")),
        _ => return Err(Error::not_implemented("a script of more than one statement")),
    };
    match statement {
        ast::Statement::Query(query) => {
            let mut binder = Binder::new(catalog);
            let (root, _) = binder.bind_query(ast, query)?;
            Ok(Bound::Query(finish(binder, root)?))
        }
        ast::Statement::CreateTable(index) => create_table(ast, catalog, index),
        ast::Statement::DropTable(index) => drop_table(ast, catalog, index),
        ast::Statement::Insert(index) => insert(ast, catalog, index),
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

fn create_table(ast: &Ast, catalog: &Catalog, index: ast::CreateTableRef) -> Result<Bound> {
    let written = ast.create_table(index);
    if written.temporary {
        // A temporary table lives in the `temp` catalog and is dropped when the connection goes,
        // and there is neither a `temp` catalog nor a connection yet. Making one in `memory` that
        // never goes away would answer a later `SELECT` with rows DuckDB would not have.
        return Err(Error::not_implemented("CREATE TEMPORARY TABLE"));
    }
    if written.if_not_exists && written.or_replace {
        return Err(Error::binder("OR REPLACE cannot be used together with IF NOT EXISTS"));
    }
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = catalog.resolve_for_create(&parts)?;
    let defs = ast.column_defs(written.columns);
    for def in defs {
        if def.not_null {
            // Nothing carries a nullability yet, so accepting this would mean an insert of a null
            // succeeding where DuckDB raises. `rudb_common::Field` is where it goes when it lands.
            return Err(Error::not_implemented("a NOT NULL column constraint"));
        }
    }
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
            columns.push(Field::new(ast.string(def.name), LogicalType::parse(text)?));
        }
        (columns, None)
    } else {
        let mut binder = Binder::new(catalog);
        let (root, scope) = binder.bind_query(ast, written.query)?;
        if !defs.is_empty() && defs.len() != scope.len() {
            return Err(Error::binder(format!(
                "Table \"{}\" has {} columns but the query produces {}",
                name.table,
                defs.len(),
                scope.len()
            )));
        }
        let mut columns = Vec::with_capacity(scope.len());
        for (at, column) in scope.columns.iter().enumerate() {
            let named = match defs.get(at) {
                Some(def) => ast.string(def.name).to_string(),
                None => column.name.clone(),
            };
            columns.push(Field::new(named, column.ty.clone()));
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

/// The same check the catalog makes, made here so the message arrives before anything is created.
fn duplicate_check(columns: &[Field]) -> Result<()> {
    for (at, column) in columns.iter().enumerate() {
        if columns[..at].iter().any(|held| same_name(&held.name, &column.name)) {
            return Err(Error::binder(format!(
                "Duplicate column name \"{}\" in a table definition",
                column.name
            )));
        }
    }
    Ok(())
}

fn drop_table(ast: &Ast, catalog: &Catalog, index: ast::DropTableRef) -> Result<Bound> {
    let written = ast.drop_table(index);
    let mut names = Vec::new();
    for &name in ast.name_list(written.names) {
        let parts: Vec<&str> = ast.name(name).collect();
        match catalog.resolve(&parts) {
            Ok(resolved) => names.push(resolved),
            Err(error) if written.if_exists => drop(error),
            Err(error) => return Err(error),
        }
    }
    Ok(Bound::DropTable(DropTable { names }))
}

fn insert(ast: &Ast, catalog: &Catalog, index: ast::InsertRef) -> Result<Bound> {
    let written = ast.insert(index);
    let parts: Vec<&str> = ast.name(written.name).collect();
    let name = catalog.resolve(&parts)?;
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

    let mut binder = Binder::new(catalog);
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
