//! A function in `FROM` that is not a table function, which PostgreSQL allows.
//!
//! `SELECT * FROM upper('abc')` is a relation of one row with one column, named for the function
//! or for the alias of the source. A function that gives a row, such as `pg_input_error_info`,
//! has a column for each field. The call is bound as it is in a select list, so it resolves the
//! same way, and the relation is a projection over one row.

use rudb_common::{Error, Field, FromFunctions, LogicalType, Result, SqlState, Value};
use rudb_functions::TableFunction;
use rudb_parse::{Ast, NONE, ast};
use rudb_plan::{ColumnBinding, Expr, Node, NodeRef};

use crate::binder::Binder;
use crate::scope::{Scope, Visible};
use crate::structs::STRUCT_EXTRACT;

impl Binder<'_> {
    /// The call of a function source that is bound as a value, or `None` when the source is a
    /// table function, a table macro or a session that keeps the rules of DuckDB.
    pub(crate) fn value_call(
        &self,
        ast: &Ast,
        source: ast::SourceRef,
        name: ast::Slice,
    ) -> Option<ast::ExprRef> {
        if self.semantics.from_functions() != FromFunctions::Postgres {
            return None;
        }
        let call = ast.source_call(source)?;
        let parts: Vec<&str> = ast.name(name).collect();
        let called = parts.last()?;
        let table = ["query", "query_table", "test_all_types", "test_vector_types"]
            .iter()
            .any(|held| called.eq_ignore_ascii_case(held));
        if table
            || TableFunction::lookup(called).is_some()
            || self.catalog().resolve_macro(&parts, Some(true)).is_some()
        {
            return None;
        }
        Some(call)
    }

    /// The relation of one row that a call in `FROM` gives.
    pub(crate) fn bind_value_source(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let written = match ast.expr(call) {
            ast::Expr::Function { name, .. } => {
                ast.name(name).last().unwrap_or_default().to_owned()
            }
            _ => return Err(Error::internal("a function source that is not a call")),
        };
        // The arguments cannot read a column of the sources on the left, as for a table function.
        // A call that returns a set, such as `regexp_split_to_table`, is an unnest, which gives
        // the rows here as it does in a select list.
        let waiting = self.scalar_subqueries.len();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let outer_unnests = std::mem::take(&mut self.unnests);
        let outer_index = self.unnest_index.take();
        let outer_here = std::mem::replace(&mut self.unnest_here, true);
        let value = self.bind_expr(ast, call, &Scope::empty());
        self.clause = previous;
        self.unnest_here = outer_here;
        let unnests = std::mem::replace(&mut self.unnests, outer_unnests);
        let index = std::mem::replace(&mut self.unnest_index, outer_index);
        let value = value?;
        let mut input = self.add_node(Node::Dummy);
        for pending in self.scalar_subqueries.split_off(waiting) {
            input = self.attach_subquery(input, pending);
        }
        if let Some(index) = index {
            input = self.plan_unnests(input, index, &unnests)?;
        }
        let label = if alias == NONE { written.clone() } else { ast.string(alias).to_owned() };
        let ty = self.plan().expr_type(value).clone();
        // A row is computed once and its fields are read from the column that holds it.
        let (node, mut scope) = match &ty {
            LogicalType::Struct(fields) if Field::unnamed(fields) => {
                return Err(Error::binder(
                    "a column definition list is required for functions returning \"record\"",
                )
                .state(SqlState::SYNTAX_ERROR));
            }
            LogicalType::Struct(fields) => {
                let (row, held) = self.project(input, &[(value, written)]);
                let column = self.plan_mut().add_expr(Expr::Column(held[0]), ty.clone());
                let mut exprs = Vec::with_capacity(fields.len());
                for (at, field) in fields.iter().enumerate() {
                    let key = self.add_constant(Value::BigInt(at as i64 + 1));
                    let args = self.plan_mut().add_expr_list(&[column, key]);
                    let name = self.plan_mut().intern(STRUCT_EXTRACT);
                    let expr = self.add_expr(Expr::Function { name, args }, field.ty.clone());
                    exprs.push((expr, field.name.clone()));
                }
                self.scoped(row, &exprs, &label)
            }
            _ => {
                let name = if alias == NONE { written } else { label.clone() };
                self.scoped(input, &[(value, name)], &label)
            }
        };
        scope.relabel(&label);
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok((node, scope))
    }

    /// A projection of `exprs` over `input`, with the binding of each column.
    fn project(
        &mut self,
        input: NodeRef,
        exprs: &[(rudb_plan::ExprRef, String)],
    ) -> (NodeRef, Vec<ColumnBinding>) {
        let index = self.fresh_index();
        let names: Vec<_> = exprs.iter().map(|(_, name)| self.plan_mut().intern(name)).collect();
        let values: Vec<_> = exprs.iter().map(|&(expr, _)| expr).collect();
        let values = self.plan_mut().add_expr_list(&values);
        let names = self.plan_mut().add_name_list(&names);
        let node = self.add_node(Node::Project { input, index, exprs: values, names });
        let bindings = (0..exprs.len()).map(|at| ColumnBinding::new(index, at as u32)).collect();
        (node, bindings)
    }

    /// A projection of `exprs` over `input` and the scope of its columns.
    fn scoped(
        &mut self,
        input: NodeRef,
        exprs: &[(rudb_plan::ExprRef, String)],
        label: &str,
    ) -> (NodeRef, Scope) {
        let (node, bindings) = self.project(input, exprs);
        let mut scope = Scope::empty();
        for ((expr, name), binding) in exprs.iter().zip(bindings) {
            scope.push(Visible {
                table: label.to_owned(),
                name: name.clone(),
                binding,
                ty: self.plan().expr_type(*expr).clone(),
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
        (node, scope)
    }
}
