//! `count(t.*)`, which counts the rows of `t` and not the rows an outer join made up for it.
//!
//! A row that is null in every column is still a row of `t`, so the count cannot read the columns.
//! What it reads instead is a value that every row of `t` has and that a made up row does not: the
//! row number of a table, or for any other relation a constant put over it in a projection. The
//! constant is only put there when the statement says `count(t.*)` somewhere, since every relation
//! of every other query would otherwise carry a column nothing reads.

use rudb_catalog::same_name;
use rudb_common::{Error, LogicalType, Result, SqlState, Value};
use rudb_parse::ast::{self, Ast};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef};

use crate::binder::{Binder, ROWID};
use crate::scope::{Scope, Visible};

impl Binder<'_> {
    /// Whether the statement `ast` holds says `count(t.*)` anywhere.
    pub(crate) fn counts_relations(&mut self, ast: &Ast) -> bool {
        let key = (std::ptr::from_ref(ast) as usize, ast.exprs.len());
        if let Some((held, counts)) = self.counted
            && held == key
        {
            return counts;
        }
        let counts = ast.exprs.iter().any(|expr| match *expr {
            ast::Expr::Function { name, args, .. } => {
                let args = ast.expr_list(args);
                ast.name(name).last().is_some_and(|name| same_name(name, "count"))
                    && args.len() == 1
                    && counted_star(ast, args[0]).is_some()
            }
            _ => false,
        });
        self.counted = Some((key, counts));
        counts
    }

    /// A projection over `node` handing back the columns `scope` names and a constant after them,
    /// which `count(t.*)` reads to tell the rows of the relation from the rows a join made up.
    pub(crate) fn mark_rows(&mut self, node: NodeRef, scope: &mut Scope) -> NodeRef {
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.columns.len() + 1);
        let mut names = Vec::with_capacity(scope.columns.len() + 1);
        for column in &scope.columns {
            exprs.push(self.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan_mut().intern(&column.name));
        }
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
        }
        let marker = ColumnBinding::new(index, exprs.len() as u32);
        exprs.push(self.add_constant(Value::BigInt(1)));
        names.push(self.plan_mut().intern(ROWID));
        let exprs = self.plan_mut().add_expr_list(&exprs);
        let names = self.plan_mut().add_name_list(&names);
        let node = self.add_node(Node::Project { input: node, index, exprs, names });
        let table = scope.columns.first().map(|column| column.table.clone()).unwrap_or_default();
        let column = Visible {
            table,
            name: ROWID.to_string(),
            binding: marker,
            ty: LogicalType::BigInt,
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: true,
            using: None,
        };
        scope.add_marker(column, node, 0);
        node
    }

    /// The argument `arg` of the aggregate `name` called with `args`, bound against `scope`.
    ///
    /// Only `count(t.*)` is any different from binding the argument. It reads the row number or
    /// the marker of the relation `t`, and when `t` is no relation but a struct column it counts
    /// the rows where the struct is not null, which is what upstream does with it too.
    pub(crate) fn bind_counted(
        &mut self,
        ast: &Ast,
        name: &str,
        args: &[ast::ExprRef],
        arg: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let qualifier = counted_star(ast, arg).filter(|_| args.len() == 1);
        let Some(qualifier) = qualifier.filter(|_| same_name(name, "count")) else {
            return self.bind_expr(ast, arg, scope);
        };
        let table = ast.name(qualifier).last().unwrap_or_default().to_string();
        let compare = self.semantics.identifier_compare();
        match scope.markers_of(compare, &table)[..] {
            [(binding, scan)] => {
                self.number_scan(scan);
                return Ok(self.add_expr(Expr::Column(binding), LogicalType::BigInt));
            }
            [] => {}
            _ => {
                return Err(Error::binder(format!(
                    "Ambiguous reference to table \"{table}\" (duplicate alias \"{table}\", \
                     explicitly alias one of the tables using \"AS my_alias\")"
                )));
            }
        }
        let structs: Vec<&Visible> = scope
            .columns
            .iter()
            .filter(|column| {
                !column.hidden
                    && compare.same(&column.name, &table)
                    && matches!(column.ty, LogicalType::Struct(_))
            })
            .collect();
        if let [column] = structs[..] {
            return Ok(self.add_expr(Expr::Column(column.binding), column.ty.clone()));
        }
        // A relation whose marker a join could not carry, which is a full join the binder put
        // together out of other plans. Its rows are counted as rows, which only differs from the
        // right answer on the rows that join made up.
        if scope.columns.iter().any(|column| compare.same(&column.table, &table)) {
            return Ok(self.add_constant(Value::BigInt(1)));
        }
        Err(Error::binder(format!("Referenced table \"{table}\" not found in FROM clause!"))
            .state(SqlState::UNDEFINED_TABLE))
    }
}

/// Refuses a star in the arguments `args` of `name` that is neither `count(*)` nor `count(t.*)`.
///
/// That is a qualified star given to anything but a plain `count`, and any star with a list after
/// it other than the replace list, which the arm that binds a bare star already refuses.
pub(crate) fn refuse_star(
    ast: &Ast,
    name: &str,
    distinct: bool,
    args: &[ast::ExprRef],
) -> Result<()> {
    for &arg in args {
        let ast::Expr::Star { qualifier, replacements } = ast.expr(arg) else { continue };
        let lists = ast.star_lists(arg);
        let listed = !lists.exclude.is_empty() || !lists.renames.is_empty();
        let counted = same_name(name, "count") && !distinct && args.len() == 1;
        if listed || (!qualifier.is_empty() && (!counted || !replacements.is_empty())) {
            return Err(Error::binder(
                "STAR expression is only allowed as the root element of an expression. Use \
                 COLUMNS(*) instead.",
            ));
        }
    }
    Ok(())
}

/// The qualifier of `expr` when it is `t.*` with nothing replaced.
fn counted_star(ast: &Ast, expr: ast::ExprRef) -> Option<ast::Slice> {
    match ast.expr(expr) {
        ast::Expr::Star { qualifier, replacements }
            if !qualifier.is_empty() && replacements.is_empty() =>
        {
            Some(qualifier)
        }
        _ => None,
    }
}
