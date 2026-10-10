//! The `SEARCH` and `CYCLE` clauses of a recursive definition, which PostgreSQL has and DuckDB
//! does not.
//!
//! PostgreSQL checks the clauses in `analyzeCTE` in `parse_cte.c` and then rewrites the definition
//! in `rewriteSearchCycle.c`. The rewrite adds columns after the columns of the definition, and
//! this binds the same columns into the plan of the definition.
//!
//! - `SEARCH DEPTH FIRST BY a, b SET seq` adds `seq`, the array of the rows `(a, b)` on the path to
//!   a row. The left side starts it as `ARRAY[ROW(a, b)]`, and the right side adds the row it
//!   makes to the array of the row it read. Sorting on it walks the graph depth first.
//! - `SEARCH BREADTH FIRST BY a, b SET seq` adds `seq` as `ROW(depth, a, b)`. The depth is 0 on the
//!   left side and one more than the depth of the row read on the right side.
//! - `CYCLE a, b SET mark TO v DEFAULT d USING path` adds `mark` and `path`. The path is the array
//!   of the rows `(a, b)`, as for a depth first search. The mark is `v` on a row whose `(a, b)` is
//!   already in the path of the row it came from and `d` on the others. The right side reads no
//!   row with the mark `v`, so a path stops at the first row that it repeats.
//!
//! The right side reads the added columns of the row it came from. The read of the definition
//! there has them after its own columns, where a star does not reach them, and the block of the
//! right side passes them through after its own targets. For this the read has to be in the `FROM`
//! of that block, which is the rule of PostgreSQL.

use rudb_common::{Error, Field, LogicalType, Result, SqlState, Value};
use rudb_parse::ast::{self, Ast};
use rudb_plan::{Arm, ColumnBinding, CompareOp, Expr, ExprRef, Node, NodeRef};

use crate::binder::Binder;
use crate::fold;
use crate::scope::{Scope, Visible};

/// The name of the depth in the sequence of a breadth first search, as PostgreSQL names it.
const DEPTH: &str = "*DEPTH*";

/// The two values of the mark of a `CYCLE` clause, at their common type.
#[derive(Debug)]
pub(crate) struct Mark {
    value: Value,
    default: Value,
    ty: LogicalType,
}

/// The columns that the `SEARCH` and `CYCLE` clauses of a recursive definition add, checked
/// against the columns of its left side.
#[derive(Debug)]
pub(crate) struct Added {
    /// The added columns in order: the sequence of the search, then the mark and the path of the
    /// cycle.
    pub(crate) fields: Vec<Field>,
    /// Whether the search is breadth first, and the positions of the columns it goes by.
    search: Option<(bool, Vec<usize>)>,
    /// The positions of the columns of the cycle, and the values of its mark.
    cycle: Option<(Vec<usize>, Mark)>,
}

/// The block of the right side of a recursive definition with added columns, which passes them
/// through from its read of the definition.
#[derive(Debug)]
pub(crate) struct Passing {
    /// The written definition, as an index into `Ast::ctes`.
    pub(crate) written: u32,
    /// The name of the definition.
    name: String,
    /// The block of the right side.
    pub(crate) select: ast::SelectRef,
    /// The position of the first added column in a read of the definition.
    pub(crate) first: usize,
    /// The names of the added columns.
    names: Vec<String>,
    /// The table index of each read of the definition in the right side.
    pub(crate) reads: Vec<u32>,
}

impl Passing {
    /// How many columns the block passes through.
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }
}

impl Binder<'_> {
    /// The values of the mark of a `CYCLE` clause, with the checks that `analyzeCTE` makes on them
    /// before it reads the query of the definition.
    pub(crate) fn cycle_mark(&mut self, ast: &Ast, cycle: &ast::Cycle) -> Result<Mark> {
        let empty = Scope::empty();
        let written = [cycle.value, cycle.default];
        let mut bound = [
            self.bind_expr(ast, cycle.value, &empty)?,
            self.bind_expr(ast, cycle.default, &empty)?,
        ];
        self.common_type(ast, &written, &mut bound, Some("CYCLE"))?;
        let ty = self.plan().expr_type(bound[0]).clone();
        bound[1] = self.checked_cast_to(bound[1], &ty, false)?;
        self.sort_group_operators(&ty, false, cycle.span).map_err(Error::unplaced)?;
        let mut values = Vec::with_capacity(2);
        for expr in bound {
            match fold::value_of(self.plan(), expr)? {
                Some(value) => values.push(value),
                None => return Err(Error::not_implemented("a CYCLE mark that is not a constant")),
            }
        }
        let default = values.pop().unwrap_or(Value::Null);
        let value = values.pop().unwrap_or(Value::Null);
        Ok(Mark { value, default, ty })
    }

    /// The columns that the clauses of `held` add to a definition with the columns `fields`, after
    /// the checks of `analyzeCTE`, in its order and with its messages.
    pub(crate) fn search_cycle(
        &mut self,
        ast: &Ast,
        held: &ast::Cte,
        fields: &[Field],
        mark: Option<Mark>,
    ) -> Result<Option<Added>> {
        if held.search.is_none() && held.cycle.is_none() {
            return Ok(None);
        }
        let names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
        let row = |positions: &[usize]| -> LogicalType {
            LogicalType::Struct(positions.iter().map(|&at| fields[at].clone()).collect())
        };
        let mut added = Added { fields: Vec::new(), search: None, cycle: None };
        if let Some(search) = held.search {
            let positions = positions(ast, search.columns, &names, "search", search.span)?;
            let sequence = ast.string(search.sequence);
            used(&names, sequence, "search sequence", search.span)?;
            let ty = if search.breadth_first {
                let mut depth = vec![Field::new(DEPTH, LogicalType::BigInt)];
                depth.extend(positions.iter().map(|&at| fields[at].clone()));
                LogicalType::Struct(depth)
            } else {
                LogicalType::List(Box::new(row(&positions)))
            };
            added.fields.push(Field::new(sequence, ty));
            added.search = Some((search.breadth_first, positions));
        }
        if let (Some(cycle), Some(mark)) = (held.cycle, mark) {
            let positions = positions(ast, cycle.columns, &names, "cycle", cycle.span)?;
            let (marked, path) = (ast.string(cycle.mark), ast.string(cycle.path));
            used(&names, marked, "cycle mark", cycle.span)?;
            used(&names, path, "cycle path", cycle.span)?;
            if marked == path {
                return Err(same("cycle mark column name and cycle path column name", cycle.span));
            }
            added.fields.push(Field::new(marked, mark.ty.clone()));
            added.fields.push(Field::new(path, LogicalType::List(Box::new(row(&positions)))));
            added.cycle = Some((positions, mark));
        }
        if let (Some(search), Some(cycle)) = (held.search, held.cycle) {
            let sequence = ast.string(search.sequence);
            if sequence == ast.string(cycle.mark) {
                return Err(same(
                    "search sequence column name and cycle mark column name",
                    search.span,
                ));
            }
            if sequence == ast.string(cycle.path) {
                return Err(same(
                    "search sequence column name and cycle path column name",
                    search.span,
                ));
            }
        }
        Ok(Some(added))
    }

    /// Marks the block of the right side of the definition `written`, whose reads have the added
    /// columns from `first` on, as the block that passes them through, after the checks that
    /// `rewriteSearchAndCycle` makes on the form of the definition.
    pub(crate) fn passing(
        &self,
        ast: &Ast,
        written: u32,
        (left, right): (ast::QueryRef, ast::QueryRef),
        first: usize,
        added: &Added,
    ) -> Result<Passing> {
        if !matches!(ast.query(left).body, ast::QueryBody::Select(_)) {
            return Err(Error::binder(
                "with a SEARCH or CYCLE clause, the left side of the UNION must be a SELECT",
            )
            .state(SqlState::FEATURE_NOT_SUPPORTED)
            .unplaced());
        }
        let ast::QueryBody::Select(select) = ast.query(right).body else {
            return Err(Error::binder(
                "with a SEARCH or CYCLE clause, the right side of the UNION must be a SELECT",
            )
            .state(SqlState::SYNTAX_ERROR)
            .unplaced());
        };
        Ok(Passing {
            written,
            name: ast.string(ast.cte(written).name).to_string(),
            select,
            first,
            names: added.fields.iter().map(|field| field.name.clone()).collect(),
            reads: Vec::new(),
        })
    }

    /// Whether `column` is an added column of a read of the definition on its right side, which a
    /// star does not reach and a name does.
    pub(crate) fn unstarred(&self, column: &Visible) -> bool {
        self.passing.as_ref().is_some_and(|passing| {
            passing.reads.contains(&column.binding.table)
                && column.binding.column as usize >= passing.first
        })
    }

    /// Adds to the targets of the block of the right side the added columns of its read of the
    /// definition, which has to be in its `FROM`.
    pub(crate) fn pass_added(
        &mut self,
        input: &Scope,
        exprs: &mut Vec<ExprRef>,
        names: &mut Vec<String>,
    ) -> Result<()> {
        let Some(passing) = &self.passing else { return Ok(()) };
        let mut found = Vec::with_capacity(passing.names.len());
        for at in 0..passing.names.len() {
            let column = (passing.first + at) as u32;
            let read = input.columns.iter().find(|held| {
                passing.reads.contains(&held.binding.table) && held.binding.column == column
            });
            let Some(read) = read else {
                return Err(Error::binder(format!(
                    "with a SEARCH or CYCLE clause, the recursive reference to WITH query \"{}\" must be at the top level of its right-hand SELECT",
                    passing.name
                ))
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .unplaced());
            };
            found.push((read.binding, read.ty.clone(), passing.names[at].clone()));
        }
        for (binding, ty, name) in found {
            exprs.push(self.add_expr(Expr::Column(binding), ty));
            names.push(name);
        }
        Ok(())
    }

    /// A side of the definition with the added columns after the columns of the definition.
    ///
    /// `over` holds the first `width` columns of the definition, and on the right side the added
    /// columns of the row that was read after them. The right side reads no row that ends a cycle.
    pub(crate) fn with_added(
        &mut self,
        node: NodeRef,
        over: &Scope,
        width: usize,
        added: &Added,
        name: &str,
    ) -> Result<(NodeRef, Scope)> {
        let column = |this: &mut Self, at: usize| {
            let held = &over.columns[at];
            this.add_expr(Expr::Column(held.binding), held.ty.clone())
        };
        let read: Option<Vec<ExprRef>> =
            (over.len() > width).then(|| (width..over.len()).map(|at| column(self, at)).collect());
        let mut node = node;
        let mut values = Vec::with_capacity(added.fields.len());
        if let Some((breadth_first, positions)) = &added.search {
            let sequence = if *breadth_first {
                let depth = match &read {
                    None => self.add_constant(Value::BigInt(0)),
                    Some(read) => {
                        let LogicalType::Struct(fields) = self.plan().expr_type(read[0]).clone()
                        else {
                            unreachable!("the sequence of a breadth first search is a row")
                        };
                        let depth = self.struct_fields(read[0], &fields)[0].0;
                        let one = self.add_constant(Value::BigInt(1));
                        self.call("+", vec![depth, one])?
                    }
                };
                let mut names = vec![DEPTH.to_string()];
                let mut row = vec![depth];
                for &at in positions {
                    names.push(over.columns[at].name.clone());
                    row.push(column(self, at));
                }
                self.pack_row(&names, &row)
            } else {
                let path = self.path_of(over, positions)?;
                match &read {
                    None => path,
                    Some(read) => self.call("list_concat", vec![read[0], path])?,
                }
            };
            values.push(sequence);
        }
        if let Some((positions, mark)) = &added.cycle {
            let at = usize::from(added.search.is_some());
            let constant = |this: &mut Self, value: &Value| {
                let value = this.add_constant(value.clone());
                this.cast_to(value, &mark.ty)
            };
            let path = self.path_of(over, positions)?;
            match &read {
                None => {
                    values.push(constant(self, &mark.default));
                    values.push(path);
                }
                Some(read) => {
                    let value = constant(self, &mark.value);
                    let predicate = self.compare(CompareOp::NotEqual, read[at], value)?;
                    node = self.add_node(Node::Filter { input: node, predicate });
                    let row = self.row_of(over, positions);
                    let element = self.plan().expr_type(row).clone();
                    let when =
                        self.quantified_list(row, read[at + 1], element, CompareOp::Equal, false);
                    let then = constant(self, &mark.value);
                    let arms = self.plan_mut().add_arms(&[Arm { when, then }]);
                    let otherwise = Some(constant(self, &mark.default));
                    values.push(self.add_expr(Expr::Case { arms, otherwise }, mark.ty.clone()));
                    values.push(self.call("list_concat", vec![read[at + 1], path])?);
                }
            }
        }

        let table = self.fresh_index();
        let mut exprs = Vec::with_capacity(width + values.len());
        let mut names = Vec::with_capacity(width + values.len());
        let mut out = Scope::empty();
        let fields = over.columns[..width]
            .iter()
            .map(|held| Field::new(held.name.clone(), held.ty.clone()))
            .chain(added.fields.iter().cloned());
        for (at, field) in fields.enumerate() {
            let expr = match at.checked_sub(width) {
                None => column(self, at),
                Some(at) => self.cast_to(values[at], &field.ty),
            };
            exprs.push(expr);
            names.push(self.plan_mut().intern(&field.name));
            out.push(Visible {
                table: name.to_string(),
                name: field.name,
                binding: ColumnBinding::new(table, at as u32),
                ty: field.ty,
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
        let exprs = self.plan_mut().add_expr_list(&exprs);
        let names = self.plan_mut().add_name_list(&names);
        let node = self.add_node(Node::Project { input: node, index: table, exprs, names });
        Ok((node, out))
    }

    /// `ROW(a, b)` of the columns at `positions`.
    fn row_of(&mut self, over: &Scope, positions: &[usize]) -> ExprRef {
        let mut names = Vec::with_capacity(positions.len());
        let mut values = Vec::with_capacity(positions.len());
        for &at in positions {
            let held = &over.columns[at];
            names.push(held.name.clone());
            values.push(self.add_expr(Expr::Column(held.binding), held.ty.clone()));
        }
        self.pack_row(&names, &values)
    }

    /// `ARRAY[ROW(a, b)]` of the columns at `positions`.
    fn path_of(&mut self, over: &Scope, positions: &[usize]) -> Result<ExprRef> {
        let row = self.row_of(over, positions);
        self.call("list_value", vec![row])
    }
}

/// The positions of the columns that a clause names, which have to be columns of the definition,
/// each named once.
fn positions(
    ast: &Ast,
    columns: ast::Slice,
    names: &[&str],
    clause: &str,
    span: rudb_common::Span,
) -> Result<Vec<usize>> {
    let mut positions = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for column in ast.name(columns) {
        let Some(at) = names.iter().position(|&name| name == column) else {
            return Err(Error::binder(format!(
                "{clause} column \"{column}\" not in WITH query column list"
            ))
            .state(SqlState::SYNTAX_ERROR)
            .with_span(span));
        };
        if seen.contains(&column) {
            return Err(Error::binder(format!(
                "{clause} column \"{column}\" specified more than once"
            ))
            .state(SqlState::DUPLICATE_COLUMN)
            .with_span(span));
        }
        seen.push(column);
        positions.push(at);
    }
    Ok(positions)
}

/// Refuses a name for an added column that is a column of the definition.
fn used(names: &[&str], name: &str, what: &str, span: rudb_common::Span) -> Result<()> {
    if names.contains(&name) {
        return Err(Error::binder(format!(
            "{what} column name \"{name}\" already used in WITH query column list"
        ))
        .state(SqlState::SYNTAX_ERROR)
        .with_span(span));
    }
    Ok(())
}

/// Refuses two added columns with one name.
fn same(what: &str, span: rudb_common::Span) -> Error {
    Error::binder(format!("{what} are the same")).state(SqlState::SYNTAX_ERROR).with_span(span)
}
