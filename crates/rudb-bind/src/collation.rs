//! The collations of DuckDB.
//!
//! The pin keeps a collation in the `VARCHAR` type, so a column declared `COLLATE nocase` and an
//! expression written `x COLLATE nocase` are of a type that compares by the collation, and a
//! function that returns a `VARCHAR` hands on the collation of its `VARCHAR` arguments. rudb keeps
//! the collations beside the plan instead: the expressions `COLLATE` was written on, the table
//! columns declared with one, and what each expression took from its inputs, found the first time
//! a comparison or a sort asks. Where the pin compares or sorts by a collation, both sides go
//! through the functions the collation names, which is what its `PushCollation` does: `nocase` is
//! `lower`, `noaccent` is `strip_accents` and `nfc` is `nfc_normalize`. A statement that meets no
//! collation pays for none of it.

use std::collections::HashMap;

use rudb_common::{Collations, Error, LogicalType, Result};
use rudb_parse::ast::{self, BinaryOp, LiteralKind};
use rudb_parse::{Ast, NONE};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node};

use crate::binder::Binder;
use crate::scope::Scope;

/// The collations a statement met and the ones the expressions above them took.
#[derive(Debug, Default)]
pub(crate) struct PinCollated {
    /// The bound expressions a `COLLATE` was written on, with the collation as written.
    written: HashMap<ExprRef, String>,
    /// The columns of the tables read whose declaration named a collation.
    columns: HashMap<ColumnBinding, String>,
    /// The collation each expression took from its inputs, kept so that each one is found once.
    derived: HashMap<ExprRef, Option<String>>,
}

impl PinCollated {
    /// Whether the statement has met no collation at all, which is nearly always.
    fn is_empty(&self) -> bool {
        self.written.is_empty() && self.columns.is_empty()
    }
}

/// The functions a collation puts a string through before it is compared, in the order they
/// apply, as `GetVarcharCollationFunctions` finds them.
///
/// The name is read without regard to case. `binary`, `c` and `posix`, alone, compare the bytes.
/// Otherwise each part between dots names a collation, a part named twice counts once, and a later
/// part applies before an earlier one, so `nocase.noaccent` strips the accents and then folds the
/// case.
pub(crate) fn collation_functions(name: &str) -> Result<Vec<&'static str>> {
    let lowered = name.to_lowercase();
    if matches!(lowered.as_str(), "" | "binary" | "c" | "posix") {
        return Ok(Vec::new());
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut functions = Vec::new();
    for part in lowered.split('.') {
        if seen.contains(&part) {
            continue;
        }
        seen.push(part);
        let function = match part {
            "nocase" => "lower",
            "noaccent" => "strip_accents",
            "nfc" => "nfc_normalize",
            _ => {
                return Err(Error::catalog(format!("Collation with name {part} does not exist!")));
            }
        };
        functions.insert(0, function);
    }
    Ok(functions)
}

/// The name written after `COLLATE`, which is an identifier, a dotted name whose parts the pin
/// joins with dots, or a string.
pub(crate) fn written_collation(ast: &Ast, right: ast::ExprRef) -> Option<String> {
    match ast.expr(right) {
        ast::Expr::Column { name } => Some(ast.name(name).collect::<Vec<_>>().join(".")),
        ast::Expr::Literal { kind: LiteralKind::String, text } if text != NONE => {
            Some(ast.string(text).to_string())
        }
        _ => None,
    }
}

/// The error of two different collations meeting in one comparison or one function.
fn mixed() -> Error {
    Error::binder("Cannot combine types with different collation!")
}

/// Whether the pin puts the collation of one argument of `name` on all of them, which it does for
/// the functions that look for one string in another. The list searches do too, and are left out
/// here until a collation can be put on the strings in a list.
fn pushes_collations(name: &str) -> bool {
    [
        "~~",
        "!~~",
        "~~~",
        "~~*",
        "!~~*",
        "like",
        "not_like",
        "glob",
        "ilike",
        "not_ilike",
        "contains",
        "starts_with",
        "^@",
        "instr",
        "strpos",
        "position",
    ]
    .iter()
    .any(|held| rudb_catalog::same_name(name, held))
}

/// Whether an aggregate or a window function returns its first argument, collation and all.
fn returns_its_argument(name: &str) -> bool {
    matches!(
        name,
        "min"
            | "max"
            | "first"
            | "last"
            | "any_value"
            | "arbitrary"
            | "mode"
            | "first_value"
            | "last_value"
            | "nth_value"
            | "lead"
            | "lag"
    )
}

impl Binder<'_> {
    /// Binds `left COLLATE right` as the pin does, where `right` names the collation.
    ///
    /// The collation goes on a copy of what it is written on, so a select list item that another
    /// item reads keeps its own collation.
    pub(crate) fn bind_pin_collate(
        &mut self,
        ast: &Ast,
        left: ast::ExprRef,
        right: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let bound = self.bind_expr(ast, left, scope)?;
        if *self.plan().expr_type(bound) != LogicalType::Varchar {
            return Err(Error::binder("collations are only supported for type varchar")
                .with_span(ast.leftmost_span(left)));
        }
        let Some(name) = written_collation(ast, right) else {
            return Err(Error::parser("COLLATE expects a collation name"));
        };
        collation_functions(&name)?;
        let copy = self.plan().expr(bound).clone();
        let span = self.plan().expr_span(bound);
        let collated = self.plan_mut().add_expr_at(copy, LogicalType::Varchar, span);
        self.pin_collated.written.insert(collated, name);
        Ok(collated)
    }

    /// Whether a `COLLATE` was written on neither of two expressions or the same one on both, so
    /// that `x COLLATE nocase` in an `ORDER BY` is not taken for an `x` of the select list.
    pub(crate) fn same_written_collation(&self, left: ExprRef, right: ExprRef) -> bool {
        self.pin_collated.is_empty()
            || self.pin_collated.written.get(&left) == self.pin_collated.written.get(&right)
    }

    /// Records the collations of the columns of a table read at `index`.
    pub(crate) fn collated_columns(&mut self, index: u32, collations: Vec<(u32, String)>) {
        for (column, name) in collations {
            self.pin_collated.columns.insert(ColumnBinding::new(index, column), name);
        }
    }

    /// The collation of the column `binding`, for a table made from a query.
    pub(crate) fn column_collation(&mut self, binding: ColumnBinding) -> Result<Option<String>> {
        if self.pin_collated.is_empty() {
            return Ok(None);
        }
        self.pin_produced(binding)
    }

    /// The two sides of a comparison of strings, each put through the collation of the
    /// comparison, which is the one either side has. Two different ones are an error.
    pub(crate) fn collate_sides(
        &mut self,
        left: ExprRef,
        right: ExprRef,
    ) -> Result<(ExprRef, ExprRef)> {
        if self.pin_collated.is_empty() {
            return Ok((left, right));
        }
        let name = match (self.derive_pin(left)?, self.derive_pin(right)?) {
            (Some(first), Some(second)) if first != second => return Err(mixed()),
            (Some(name), _) | (None, Some(name)) => name,
            (None, None) => return Ok((left, right)),
        };
        Ok((self.apply_collation(left, &name)?, self.apply_collation(right, &name)?))
    }

    /// Gives every string of `values` the collation one of them has, for `IN` and `BETWEEN`, which
    /// the pin compares under the collation of all their operands together. Two different ones are
    /// an error.
    pub(crate) fn share_collation(&mut self, values: &mut [ExprRef]) -> Result<()> {
        if self.pin_collated.is_empty() {
            return Ok(());
        }
        let mut found: Option<String> = None;
        let mut plain = Vec::new();
        for (at, &value) in values.iter().enumerate() {
            match self.derive_pin(value)? {
                Some(name) => match &found {
                    Some(held) if *held != name => return Err(mixed()),
                    Some(_) => {}
                    None => found = Some(name),
                },
                None if *self.plan().expr_type(value) == LogicalType::Varchar => plain.push(at),
                None => {}
            }
        }
        let Some(name) = found else { return Ok(()) };
        for at in plain {
            let copy = self.plan().expr(values[at]).clone();
            let span = self.plan().expr_span(values[at]);
            let collated = self.plan_mut().add_expr_at(copy, LogicalType::Varchar, span);
            self.pin_collated.written.insert(collated, name.clone());
            values[at] = collated;
        }
        Ok(())
    }

    /// The arguments of a call to `name` with the collation one of them has put on every string
    /// among them, for the functions the pin binds that way, which are the ones that match one
    /// string against another. `contains(s COLLATE nocase, 'O')` is
    /// `contains(lower(s), lower('O'))`.
    pub(crate) fn push_collations(
        &mut self,
        name: &str,
        mut args: Vec<ExprRef>,
    ) -> Result<Vec<ExprRef>> {
        if self.pin_collated.is_empty() || !pushes_collations(name) {
            return Ok(args);
        }
        let mut found: Option<String> = None;
        for &arg in &args {
            if let Some(name) = self.derive_pin(arg)? {
                match &found {
                    Some(held) if *held != name => return Err(mixed()),
                    Some(_) => {}
                    None => found = Some(name),
                }
            }
        }
        let Some(name) = found else { return Ok(args) };
        for arg in &mut args {
            if *self.plan().expr_type(*arg) == LogicalType::Varchar {
                *arg = self.apply_collation(*arg, &name)?;
            }
        }
        Ok(args)
    }

    /// An `ORDER BY 1 COLLATE nocase`, which the pin reads as the first column sorted under the
    /// collation, as the item with the `COLLATE` taken off and the collation.
    pub(crate) fn ordinal_collation(
        &self,
        ast: &Ast,
        item: ast::OrderItem,
    ) -> (ast::OrderItem, Option<String>) {
        if self.semantics.collations() == Collations::Pin
            && let ast::Expr::Binary { op: BinaryOp::Collate, left, right } = ast.expr(item.expr)
            && matches!(ast.expr(left), ast::Expr::Literal { kind: LiteralKind::Number, .. })
            && let Some(name) = written_collation(ast, right)
        {
            return (ast::OrderItem { expr: left, ..item }, Some(name));
        }
        (item, None)
    }

    /// A sort key `expr` under the collation an `ORDER BY 1 COLLATE nocase` named, or else under
    /// the collation of `from`, the expression it sorts by.
    pub(crate) fn order_key(
        &mut self,
        expr: ExprRef,
        from: ExprRef,
        ordinal: Option<&str>,
    ) -> Result<ExprRef> {
        let Some(name) = ordinal else { return self.collate_key(expr, from) };
        if *self.plan().expr_type(expr) != LogicalType::Varchar {
            return Err(Error::binder("COLLATE can only be applied to varchar columns"));
        }
        self.apply_collation(expr, name)
    }

    /// A sort key `expr` put through the collation of `from`, the expression it sorts by.
    pub(crate) fn collate_key(&mut self, expr: ExprRef, from: ExprRef) -> Result<ExprRef> {
        if self.pin_collated.is_empty() || *self.plan().expr_type(expr) != LogicalType::Varchar {
            return Ok(expr);
        }
        match self.derive_pin(from)? {
            Some(name) => self.apply_collation(expr, &name),
            None => Ok(expr),
        }
    }

    /// `expr` through the functions of the collation `name`.
    fn apply_collation(&mut self, mut expr: ExprRef, name: &str) -> Result<ExprRef> {
        for function in collation_functions(name)? {
            expr = self.call(function, vec![expr])?;
        }
        Ok(expr)
    }

    fn derive_pin(&mut self, expr: ExprRef) -> Result<Option<String>> {
        if let Some(name) = self.pin_collated.written.get(&expr) {
            return Ok(Some(name.clone()));
        }
        if let Some(found) = self.pin_collated.derived.get(&expr) {
            return Ok(found.clone());
        }
        let plan = self.plan();
        if *plan.expr_type(expr) != LogicalType::Varchar {
            return Ok(None);
        }
        let inputs: Vec<ExprRef> = match plan.expr(expr) {
            Expr::Column(binding) => {
                let binding = *binding;
                let found = self.pin_produced(binding)?;
                self.pin_collated.derived.insert(expr, found.clone());
                return Ok(found);
            }
            Expr::Cast { input, .. } => vec![*input],
            Expr::Function { args, .. } => plan.expr_list(*args).to_vec(),
            Expr::Aggregate { name, args, .. } | Expr::Window { name, args, .. } => {
                if returns_its_argument(plan.string(*name)) {
                    plan.expr_list(*args).iter().take(1).copied().collect()
                } else {
                    Vec::new()
                }
            }
            Expr::Case { arms, otherwise } => {
                let arms = plan.arm_list(*arms);
                arms.iter().map(|arm| arm.then).chain(*otherwise).collect()
            }
            _ => Vec::new(),
        };
        let mut found: Option<String> = None;
        for input in inputs {
            if let Some(name) = self.derive_pin(input)? {
                match &found {
                    Some(held) if *held != name => return Err(mixed()),
                    Some(_) => {}
                    None => found = Some(name),
                }
            }
        }
        self.pin_collated.derived.insert(expr, found.clone());
        Ok(found)
    }

    /// The collation of the expression that makes the column `binding`.
    fn pin_produced(&mut self, binding: ColumnBinding) -> Result<Option<String>> {
        if let Some(name) = self.pin_collated.columns.get(&binding) {
            return Ok(Some(name.clone()));
        }
        if let Some(&output) = self.collated.reads.get(&binding.table) {
            return self.pin_produced(ColumnBinding::new(output, binding.column));
        }
        let column = binding.column as usize;
        // The grouping of the block that is bound now, whose node is added after its select list.
        if let Some(aggregation) = &self.aggregation
            && aggregation.index == binding.table
        {
            let made =
                aggregation.groups.iter().chain(&aggregation.aggregates).nth(column).copied();
            return made.map_or(Ok(None), |made| self.derive_pin(made));
        }
        let plan = self.plan();
        let node = (0..plan.node_count())
            .map(|node| node as u32)
            .find(|&node| plan.node(node).table_index() == Some(binding.table));
        let Some(node) = node else { return Ok(None) };
        let made: Vec<ExprRef> = match *plan.node(node) {
            Node::Project { exprs, .. } => {
                plan.expr_list(exprs).get(column).copied().into_iter().collect()
            }
            Node::Aggregate { groups, aggregates, .. } => plan
                .expr_list(groups)
                .iter()
                .chain(plan.expr_list(aggregates))
                .nth(column)
                .copied()
                .into_iter()
                .collect(),
            Node::Window { expressions, .. } => {
                plan.expr_list(expressions).get(column).copied().into_iter().collect()
            }
            Node::Values { rows, .. } => plan
                .row_list(rows)
                .iter()
                .filter_map(|&row| plan.expr_list(row).get(column).copied())
                .collect(),
            // A set operation takes the collation either side has.
            Node::SetOp { left, right, .. } => {
                for side in [left, right] {
                    if let Some(output) = self.output_index(side)
                        && let Some(name) =
                            self.pin_produced(ColumnBinding::new(output, binding.column))?
                    {
                        return Ok(Some(name));
                    }
                }
                return Ok(None);
            }
            Node::RecursiveCte { anchor, .. } => {
                return match self.output_index(anchor) {
                    Some(output) => self.pin_produced(ColumnBinding::new(output, binding.column)),
                    None => Ok(None),
                };
            }
            _ => Vec::new(),
        };
        for made in made {
            if let Some(name) = self.derive_pin(made)? {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }
}
