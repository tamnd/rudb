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

use rudb_common::{Collations, Error, LogicalType, Result, Session};
use rudb_parse::ast::{self, BinaryOp, LiteralKind};
use rudb_parse::{Ast, NONE};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, SetOpKind,
};

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
    /// The groups of a grouping that sorts under a collation, each with the column of the `first`
    /// aggregate that gives the value as it was rather than as the collation made it, and the
    /// index of the grouping.
    grouped: Vec<(u32, ExprRef, ExprRef)>,
    /// The session's `default_collation`, which a comparison or a sort of strings that have no
    /// collation of their own goes by, or none when it is not set.
    default: Option<String>,
}

impl PinCollated {
    /// The collations of a statement bound in `session`, which has met none yet.
    pub(crate) fn for_session(session: &Session) -> PinCollated {
        let default = match session.semantics().collations() {
            Collations::Pin => session.get("default_collation").filter(|name| !name.is_empty()),
            Collations::Postgres => None,
        };
        PinCollated { default: default.map(str::to_string), ..PinCollated::default() }
    }

    /// Whether the statement has met no collation at all, which is nearly always.
    fn is_empty(&self) -> bool {
        self.written.is_empty() && self.columns.is_empty()
    }

    /// Whether a comparison or a sort of strings can need a collation, which it cannot when the
    /// statement has met none and the session has no default.
    fn active(&self) -> bool {
        !self.is_empty() || self.default.is_some()
    }
}

/// Refuses a name that is not a collation, for `SET default_collation`.
pub fn check_collation(name: &str) -> Result<()> {
    collation_functions(name).map(drop)
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

/// Whether a value of `ty` can be compared under a collation, which a string can and so can a list
/// of strings, whose elements the pin puts through the collation one at a time.
pub(crate) fn collatable(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Varchar => true,
        LogicalType::List(element) => collatable(element),
        _ => false,
    }
}

/// Whether `name` makes a list of its arguments, which is the one way a list takes the collation
/// of the strings in it, because the type of the list is made from theirs.
fn makes_a_list(name: &str) -> bool {
    matches!(name, "list_value" | "list_pack")
}

/// The error of two different collations meeting in one comparison or one function.
fn mixed() -> Error {
    Error::binder("Cannot combine types with different collation!")
}

/// Whether the pin puts the collation of one argument of `name` on all of them, which it does for
/// the functions that look for one string in another and for the searches of a list.
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
        "list_contains",
        "list_has",
        "array_contains",
        "array_has",
        "list_position",
        "list_indexof",
        "array_position",
        "array_indexof",
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
        if !self.pin_collated.active() {
            return Ok((left, right));
        }
        let name = match (self.collation_of(left)?, self.collation_of(right)?) {
            (Some(first), Some(second)) if first != second => return Err(mixed()),
            (Some(name), _) | (None, Some(name)) => name,
            (None, None) => match &self.pin_collated.default {
                Some(name) => name.clone(),
                None => return Ok((left, right)),
            },
        };
        Ok((self.apply_collation(left, &name)?, self.apply_collation(right, &name)?))
    }

    /// Puts each group of a grouping through its collation, so that `GROUP BY s` over a `nocase`
    /// column makes one group of `a` and `A`. The groups as they were come back with where they
    /// sit, for [`Binder::first_of_groups`].
    pub(crate) fn collate_groups(
        &mut self,
        groups: &mut [ExprRef],
    ) -> Result<Vec<(usize, ExprRef)>> {
        let mut uncollated = Vec::new();
        if !self.pin_collated.active() {
            return Ok(uncollated);
        }
        for (at, group) in groups.iter_mut().enumerate() {
            let keyed = self.collate_key(*group, *group)?;
            if keyed != *group {
                uncollated.push((at, *group));
                *group = keyed;
            }
        }
        Ok(uncollated)
    }

    /// The `first` aggregate the pin adds for each group that sorts under a collation, which is
    /// what a read of the group in the select list gets: the first value of the group as it was
    /// written, `A` rather than `a`.
    pub(crate) fn first_of_groups(&mut self, uncollated: Vec<(usize, ExprRef)>) -> Result<()> {
        let Some(index) = self.aggregation.as_ref().map(|aggregation| aggregation.index) else {
            return Ok(());
        };
        for (_, group) in uncollated {
            let ty = self.plan().expr_type(group).clone();
            let first = self.aggregate_call("first", &[group], false, None, ty)?;
            self.pin_collated.grouped.push((index, group, first));
        }
        Ok(())
    }

    /// The `first` aggregate of the group `expr` is, when it is a group of the grouping bound now
    /// that sorts under a collation.
    pub(crate) fn collated_group(&self, expr: ExprRef) -> Option<ExprRef> {
        let index = self.aggregation.as_ref()?.index;
        self.pin_collated
            .grouped
            .iter()
            .find(|(at, group, _)| *at == index && self.same_expr(expr, *group))
            .map(|&(_, _, first)| first)
    }

    /// The keys of a `DISTINCT`, each put through its collation. A plain `DISTINCT` over a column
    /// with one becomes a `DISTINCT ON` every column, so that one row of `a` and `A` is kept as it
    /// was written.
    pub(crate) fn collate_distinct(
        &mut self,
        on: Vec<ExprRef>,
        columns: &[crate::scope::Visible],
    ) -> Result<Vec<ExprRef>> {
        if !self.pin_collated.active() {
            return Ok(on);
        }
        let whole = on.is_empty();
        let keys: Vec<ExprRef> = match whole {
            true => columns
                .iter()
                .map(|column| {
                    let (binding, ty) = (column.binding, column.ty.clone());
                    self.plan_mut().add_expr(Expr::Column(binding), ty)
                })
                .collect(),
            false => on,
        };
        let mut collated = Vec::with_capacity(keys.len());
        let mut changed = false;
        for key in keys {
            let keyed = self.collate_key(key, key)?;
            changed |= keyed != key;
            collated.push(keyed);
        }
        if whole && !changed {
            return Ok(Vec::new());
        }
        Ok(collated)
    }

    /// A set operation that removes duplicates under the collations of its columns, or `None` when
    /// it has none and the plain operator does.
    ///
    /// A column of the operation takes the collation of the right side when it has one and of the
    /// left side when not, which is how the pin combines two collated types, and that is the
    /// collation the duplicates are removed under. `EXCEPT` and `INTERSECT` match a row of the
    /// left side with one of the right side under the left side's collation, since the pin joins
    /// the two by the types of the left side. So `UNION` is a `UNION ALL` under a `DISTINCT ON`
    /// the collated columns, and the other two are an anti or a semi join under one. The forms
    /// with `ALL` are left to the plain operator.
    pub(crate) fn collated_set_op(
        &mut self,
        op: SetOpKind,
        all: bool,
        index: u32,
        sides: [(NodeRef, Vec<ColumnBinding>); 2],
        columns: &[(String, LogicalType)],
    ) -> Result<Option<NodeRef>> {
        if all || !self.pin_collated.active() {
            return Ok(None);
        }
        let [(left, left_columns), (right, right_columns)] = sides;
        let mut made = Vec::with_capacity(columns.len());
        let mut matched = Vec::with_capacity(columns.len());
        for at in 0..columns.len() {
            let held = self.column_collation(left_columns[at])?;
            let other = self.column_collation(right_columns[at])?;
            made.push(other.or_else(|| held.clone()));
            matched.push(held);
        }
        let default = self.pin_collated.default.clone();
        let strings: Vec<bool> = columns.iter().map(|(_, ty)| collatable(ty)).collect();
        let under = |names: &[Option<String>]| -> Vec<Option<String>> {
            let fallback = |name: &Option<String>| name.clone().or_else(|| default.clone());
            names
                .iter()
                .zip(&strings)
                .map(|(name, &string)| fallback(name).filter(|_| string))
                .collect()
        };
        let (keyed, matched) = (under(&made), under(&matched));
        let input = match op {
            SetOpKind::Union => {
                if keyed.iter().all(Option::is_none) {
                    return Ok(None);
                }
                self.add_node(Node::SetOp { left, right, kind: op, all: true, index })
            }
            SetOpKind::Except | SetOpKind::Intersect => {
                if matched.iter().all(Option::is_none) {
                    return Ok(None);
                }
                let mut conditions = Vec::with_capacity(columns.len());
                for (at, name) in matched.iter().enumerate() {
                    let ty = &columns[at].1;
                    let mut one =
                        self.plan_mut().add_expr(Expr::Column(left_columns[at]), ty.clone());
                    let mut other =
                        self.plan_mut().add_expr(Expr::Column(right_columns[at]), ty.clone());
                    if let Some(name) = name {
                        one = self.apply_collation(one, name)?;
                        other = self.apply_collation(other, name)?;
                    }
                    let compare =
                        Expr::Compare { op: CompareOp::NotDistinctFrom, left: one, right: other };
                    conditions.push(self.plan_mut().add_expr(compare, LogicalType::Boolean));
                }
                let conditions = self.plan_mut().add_expr_list(&conditions);
                let kind = if op == SetOpKind::Except { JoinKind::Anti } else { JoinKind::Semi };
                let build = BuildSide::default();
                let join = self.add_node(Node::Join { left, right, kind, conditions, build });
                let mut exprs = Vec::with_capacity(columns.len());
                let mut names = Vec::with_capacity(columns.len());
                for (at, (name, ty)) in columns.iter().enumerate() {
                    exprs
                        .push(self.plan_mut().add_expr(Expr::Column(left_columns[at]), ty.clone()));
                    names.push(self.plan_mut().intern(name));
                }
                let exprs = self.plan_mut().add_expr_list(&exprs);
                let names = self.plan_mut().add_name_list(&names);
                self.add_node(Node::Project { input: join, index, exprs, names })
            }
        };
        let mut keys = Vec::with_capacity(columns.len());
        for (at, name) in keyed.iter().enumerate() {
            let binding = ColumnBinding::new(index, at as u32);
            if let Some(name) = &made[at] {
                self.pin_collated.columns.insert(binding, name.clone());
            }
            let key = self.plan_mut().add_expr(Expr::Column(binding), columns[at].1.clone());
            keys.push(match name {
                Some(name) => self.apply_collation(key, name)?,
                None => key,
            });
        }
        let on = self.plan_mut().add_expr_list(&keys);
        Ok(Some(self.add_node(Node::Distinct { input, on })))
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
            if collatable(self.plan().expr_type(*arg)) {
                *arg = self.apply_collation(*arg, &name)?;
            }
        }
        Ok(args)
    }

    /// An `ORDER BY 1 COLLATE nocase`, which the pin reads as the first column sorted under the
    /// collation, as the item with the `COLLATE` taken off and the collation. A `#1` and the name
    /// of a column of `output` are read the same way, so an alias wins over a column of the same
    /// name in the `FROM` clause, as it does in the pin's `OrderBinder`.
    pub(crate) fn ordinal_collation(
        &self,
        ast: &Ast,
        item: ast::OrderItem,
        output: &Scope,
    ) -> (ast::OrderItem, Option<String>) {
        if self.semantics.collations() == Collations::Pin
            && let ast::Expr::Binary { op: BinaryOp::Collate, left, right } = ast.expr(item.expr)
            && self.names_output(ast, left, output)
            && let Some(name) = written_collation(ast, right)
        {
            return (ast::OrderItem { expr: left, ..item }, Some(name));
        }
        (item, None)
    }

    /// A sort key `expr` under the collation an `ORDER BY 1 COLLATE nocase` named, or else under
    /// the collation of `from`, the expression it sorts by.
    /// Whether a sort term is a position or the name of a column of `output`.
    fn names_output(&self, ast: &Ast, term: ast::ExprRef, output: &Scope) -> bool {
        match ast.expr(term) {
            ast::Expr::Literal { kind: LiteralKind::Number, .. } | ast::Expr::Positional { .. } => {
                true
            }
            ast::Expr::Column { name } => {
                let parts: Vec<&str> = ast.name(name).collect();
                let compare = self.semantics.identifier_compare();
                let [written] = parts.as_slice() else { return false };
                output.position_of(compare, None, written).is_some()
            }
            _ => false,
        }
    }

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
        if !self.pin_collated.active() || !collatable(self.plan().expr_type(expr)) {
            return Ok(expr);
        }
        match self.collation_of(from)? {
            Some(name) => self.apply_collation(expr, &name),
            None => match self.pin_collated.default.clone() {
                Some(name) => self.apply_collation(expr, &name),
                None => Ok(expr),
            },
        }
    }

    /// The collation `expr` has, which is none without a look when the statement has met none.
    fn collation_of(&mut self, expr: ExprRef) -> Result<Option<String>> {
        if self.pin_collated.is_empty() {
            return Ok(None);
        }
        self.derive_pin(expr)
    }

    /// `expr` through the functions of the collation `name`.
    fn apply_collation(&mut self, mut expr: ExprRef, name: &str) -> Result<ExprRef> {
        // A list goes through `list_transform` with a lambda that puts each element through the
        // collation, as the pin's `PushNestedCollation` does.
        if let LogicalType::List(element) = self.plan().expr_type(expr).clone() {
            let table = self.fresh_index();
            let parameter = self.plan_mut().intern("x");
            let params = self.plan_mut().add_name_list(&[parameter]);
            let value = self.add_expr(Expr::LambdaParam(ColumnBinding::new(table, 0)), *element);
            let body = self.apply_collation(value, name)?;
            let ty = self.plan().expr_type(body).clone();
            let lambda = self.add_expr(Expr::Lambda { table, params, body }, ty.clone());
            let args = self.plan_mut().add_expr_list(&[expr, lambda]);
            let transform = self.plan_mut().intern(crate::lambda::TRANSFORM);
            let function = Expr::Function { name: transform, args };
            return Ok(self.add_expr(function, LogicalType::List(Box::new(ty))));
        }
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
        let ty = plan.expr_type(expr);
        if !collatable(ty) {
            return Ok(None);
        }
        let string = *ty == LogicalType::Varchar;
        let inputs: Vec<ExprRef> = match plan.expr(expr) {
            Expr::Column(binding) => {
                let binding = *binding;
                let found = self.pin_produced(binding)?;
                self.pin_collated.derived.insert(expr, found.clone());
                return Ok(found);
            }
            Expr::Cast { input, .. } => vec![*input],
            Expr::Function { name, args } if string || makes_a_list(plan.string(*name)) => {
                plan.expr_list(*args).to_vec()
            }
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
            // A set operation takes the collation of the right side, or of the left side when the
            // right side has none.
            Node::SetOp { left, right, .. } => {
                for side in [right, left] {
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
