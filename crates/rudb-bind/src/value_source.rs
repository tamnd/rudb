//! A function in `FROM` that is not a table function, which PostgreSQL allows.
//!
//! `SELECT * FROM upper('abc')` is a relation of one row with one column, named for the function
//! or for the alias of the source. A function that gives a row, such as `pg_input_error_info`,
//! has a column for each field. The call is bound as it is in a select list, so it resolves the
//! same way, and the relation is a projection over one row.

use rudb_common::{Error, Field, FromFunctions, LogicalType, Result, SqlState, Value};
use rudb_functions::TableFunction;
use rudb_kernels::pgjson::JsonSet;
use rudb_parse::{Ast, NONE, ast};
use rudb_plan::{ColumnBinding, Expr, Node, NodeRef};

use crate::binder::Binder;
use crate::ordinality::ORDINALITY;
use crate::scope::{Scope, Visible};

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
        // The JSON set functions of PostgreSQL have the names of table functions of DuckDB.
        let duckdb = TableFunction::lookup(called).is_some() && JsonSet::of(called).is_none();
        if table || duckdb || self.catalog().resolve_macro(&parts, Some(true)).is_some() {
            return None;
        }
        Some(call)
    }

    /// The relation that the calls of a function source give. One call that does not return a
    /// set gives one row. The calls of `ROWS FROM` give their rows side by side, and a call that
    /// gives fewer rows than the others gives nulls for the rest.
    pub(crate) fn bind_value_source(
        &mut self,
        ast: &Ast,
        calls: &[ast::ExprRef],
        alias: ast::StrRef,
        columns: ast::Slice,
        ordinality: bool,
    ) -> Result<(NodeRef, Scope)> {
        if self.semantics.from_functions() != FromFunctions::Postgres {
            return Err(Error::not_implemented("ROWS FROM"));
        }
        let mut written = Vec::with_capacity(calls.len());
        for &call in calls {
            let ast::Expr::Function { name, .. } = ast.expr(call) else {
                return Err(Error::internal("a function source that is not a call"));
            };
            written.push(ast.name(name).last().unwrap_or_default().to_owned());
        }
        // The arguments cannot read a column of the sources on the left, as for a table function.
        // A call that returns a set, such as `regexp_split_to_table`, is an unnest, which gives
        // the rows here as it does in a select list. Next to other calls, a call that does not
        // return a set is a set of one row, so the unnests put the rows of all of them side by
        // side.
        let waiting = self.scalar_subqueries.len();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let outer_unnests = std::mem::take(&mut self.unnests);
        let outer_index = self.unnest_index.take();
        let outer_here = std::mem::replace(&mut self.unnest_here, true);
        let mut values = Vec::with_capacity(calls.len());
        let mut bound = Ok(());
        for &call in calls {
            let made = self.unnests.len();
            match self.bind_expr(ast, call, &Scope::empty()) {
                Ok(value) if calls.len() > 1 && self.unnests.len() == made => {
                    match self.unnest_one(value) {
                        Ok(value) => values.push(value),
                        Err(error) => bound = Err(error),
                    }
                }
                Ok(value) => values.push(value),
                Err(error) => bound = Err(error),
            }
            if bound.is_err() {
                break;
            }
        }
        self.clause = previous;
        self.unnest_here = outer_here;
        let unnests = std::mem::replace(&mut self.unnests, outer_unnests);
        let index = std::mem::replace(&mut self.unnest_index, outer_index);
        bound?;
        let mut input = self.add_node(Node::Dummy);
        for pending in self.scalar_subqueries.split_off(waiting) {
            input = self.attach_subquery(input, pending);
        }
        if let Some(index) = index {
            input = self.plan_unnests(input, index, &unnests)?;
        }
        // `WITH ORDINALITY` numbers the rows of the unnests, and the one row of any other call
        // is 1. An unnest of more than one level would number only the last level.
        let number = match (ordinality, index) {
            (false, _) => None,
            (true, None) => Some(self.add_constant(Value::BigInt(1))),
            (true, Some(_)) if unnests.iter().all(|call| call.depth == 1) => {
                let binding = self.ordinality_column(input)?;
                Some(self.add_expr(Expr::Column(binding), LogicalType::BigInt))
            }
            (true, Some(_)) => {
                return Err(Error::not_implemented(
                    "WITH ORDINALITY for an unnest of more than one level",
                ));
            }
        };
        let label = if alias == NONE { written[0].clone() } else { ast.string(alias).to_owned() };
        let types: Vec<LogicalType> =
            values.iter().map(|&value| self.plan().expr_type(value).clone()).collect();
        if types
            .iter()
            .any(|ty| matches!(ty, LogicalType::Struct(fields) if Field::unnamed(fields)))
        {
            return Err(Error::binder(
                "a column definition list is required for functions returning \"record\"",
            )
            .state(SqlState::SYNTAX_ERROR));
        }
        // A row is computed once and its fields are read from the column that holds it, so a
        // call that gives a row is projected first, with the other values and the number.
        let mut base = input;
        let mut reads = values.clone();
        let mut number = number;
        if types.iter().any(|ty| matches!(ty, LogicalType::Struct(_))) {
            let mut inner: Vec<_> =
                values.iter().zip(&written).map(|(&value, name)| (value, name.clone())).collect();
            inner.extend(number.map(|expr| (expr, ORDINALITY.to_owned())));
            let (row, held) = self.project(input, &inner);
            base = row;
            reads = held[..values.len()]
                .iter()
                .zip(&types)
                .map(|(&binding, ty)| self.plan_mut().add_expr(Expr::Column(binding), ty.clone()))
                .collect();
            number = number.map(|_| {
                let binding = held[values.len()];
                self.plan_mut().add_expr(Expr::Column(binding), LogicalType::BigInt)
            });
        }
        let mut exprs = Vec::new();
        for ((&read, ty), name) in reads.iter().zip(&types).zip(&written) {
            match ty {
                LogicalType::Struct(fields) => exprs.extend(self.struct_fields(read, fields)),
                // One call that gives a value names its column for the alias of the source.
                // A JSON set function names its column, and the alias does not rename it.
                _ if let Some(column) = JsonSet::of(name).and_then(JsonSet::column) => {
                    exprs.push((read, column.to_owned()));
                }
                _ if calls.len() == 1 && alias != NONE => exprs.push((read, label.clone())),
                _ => exprs.push((read, name.clone())),
            }
        }
        exprs.extend(number.map(|expr| (expr, ORDINALITY.to_owned())));
        let (node, mut scope) = self.scoped(base, &exprs, &label);
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
