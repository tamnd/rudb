//! The arena a plan lives in, and the invariant that keeps its indices honest.

use rudb_common::{Error, Field, LogicalType, Result, Span, Value};

use crate::expr::{Arm, ColumnBinding, Expr, SortKey};
use crate::node::{JoinKind, Node};
use crate::{ExprRef, NodeRef, Slice, StrRef, ValueRef};

/// A bound logical plan.
///
/// Ten flat pools and a root. Everything refers to everything else by `u32` index, and the one
/// structural rule is that **a reference always points backwards**: a node's children have smaller
/// indices than the node, and an expression's operands have smaller indices than the expression.
/// Building bottom up gives that for free, it makes a cycle impossible rather than merely unlikely,
/// and it means a walk of the whole plan is a loop over a vector in either direction instead of a
/// recursion with a visited set. [`Plan::validate`] checks it.
///
/// A fresh plan is [`Node::Dummy`] at the root, which is one row and no columns. That is a valid
/// plan rather than a placeholder, so there is no state in which a `Plan` exists and cannot be
/// printed.
///
/// There is no `PartialEq`. Two plans that compute the same thing can have different arena layouts
/// after a rewrite reorders pools, so comparing arenas would report differences that are not
/// differences. The textual form is what plans are compared by, and it is canonical because
/// printing walks from the root and never touches an unreachable entry.
#[derive(Debug, Clone)]
pub struct Plan {
    nodes: Vec<Node>,
    /// Source ranges parallel to `nodes`.
    node_spans: Vec<Span>,
    exprs: Vec<Expr>,
    /// Source ranges parallel to `exprs`.
    expr_spans: Vec<Span>,
    /// The type of `exprs[i]`, parallel and always the same length.
    types: Vec<LogicalType>,
    values: Vec<Value>,
    strings: Vec<String>,
    expr_lists: Vec<ExprRef>,
    name_lists: Vec<StrRef>,
    fields: Vec<Field>,
    sort_keys: Vec<SortKey>,
    arms: Vec<Arm>,
    rows: Vec<Slice>,
    root: NodeRef,
}

impl Default for Plan {
    fn default() -> Self {
        Self::new()
    }
}

impl Plan {
    /// An empty plan, which is one row and no columns.
    #[must_use]
    pub fn new() -> Self {
        let mut plan = Self::without_nodes();
        plan.add_node(Node::Dummy);
        plan
    }

    /// A plan with nothing in it at all, which is not a state anybody outside this crate can hold.
    ///
    /// The reader needs it: it builds the root from the text and would otherwise start from a
    /// [`Node::Dummy`] that nothing points at, and an unreachable node in a freshly parsed plan is
    /// a difference between a plan and the same plan printed and read back.
    pub(crate) fn without_nodes() -> Self {
        Self {
            nodes: Vec::new(),
            node_spans: Vec::new(),
            exprs: Vec::new(),
            expr_spans: Vec::new(),
            types: Vec::new(),
            values: Vec::new(),
            strings: Vec::new(),
            expr_lists: Vec::new(),
            name_lists: Vec::new(),
            fields: Vec::new(),
            sort_keys: Vec::new(),
            arms: Vec::new(),
            rows: Vec::new(),
            root: 0,
        }
    }

    /// The node the plan is rooted at.
    #[must_use]
    pub fn root(&self) -> NodeRef {
        self.root
    }

    /// Roots the plan at `node`.
    pub fn set_root(&mut self, node: NodeRef) {
        self.root = node;
    }

    /// How many nodes are in the arena, reachable or not.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// How many expressions are in the arena, reachable or not.
    #[must_use]
    pub fn expr_count(&self) -> usize {
        self.exprs.len()
    }

    // Builders. Each one appends and hands back the index, which is why building bottom up gives
    // the backwards-reference invariant without anybody having to think about it.

    /// Appends a node.
    pub fn add_node(&mut self, node: Node) -> NodeRef {
        let span = node
            .children()
            .into_iter()
            .flatten()
            .map(|child| self.node_span(child))
            .fold(Span::new(0, 0), merge_span);
        self.add_node_at(node, span)
    }

    /// Appends a node with the source range that produced it.
    pub fn add_node_at(&mut self, node: Node, span: Span) -> NodeRef {
        self.node_spans.push(span);
        push(&mut self.nodes, node)
    }

    /// Appends an expression and the type it evaluates to.
    pub fn add_expr(&mut self, expr: Expr, ty: LogicalType) -> ExprRef {
        let span = self.inferred_expr_span(&expr);
        self.add_expr_at(expr, ty, span)
    }

    /// Appends an expression and its type with the source range that produced it.
    pub fn add_expr_at(&mut self, expr: Expr, ty: LogicalType, span: Span) -> ExprRef {
        self.types.push(ty);
        self.expr_spans.push(span);
        push(&mut self.exprs, expr)
    }

    fn inferred_expr_span(&self, expr: &Expr) -> Span {
        let mut span = Span::new(0, 0);
        let mut include = |reference: ExprRef| {
            span = merge_span(span, self.expr_span(reference));
        };
        match *expr {
            Expr::Column(_) | Expr::Constant(_) => {}
            Expr::Cast { input, .. } => include(input),
            Expr::Compare { left, right, .. } => {
                include(left);
                include(right);
            }
            Expr::Conjunction { children, .. } => {
                for &child in self.expr_list(children) {
                    include(child);
                }
            }
            Expr::Function { args, .. } => {
                for &arg in self.expr_list(args) {
                    include(arg);
                }
            }
            Expr::Aggregate { args, filter, .. } | Expr::Window { args, filter, .. } => {
                for &arg in self.expr_list(args) {
                    include(arg);
                }
                if let Some(filter) = filter {
                    include(filter);
                }
            }
            Expr::Case { arms, otherwise } => {
                for arm in self.arm_list(arms) {
                    include(arm.when);
                    include(arm.then);
                }
                if let Some(otherwise) = otherwise {
                    include(otherwise);
                }
            }
        }
        span
    }

    /// Appends a constant.
    pub fn add_value(&mut self, value: Value) -> ValueRef {
        push(&mut self.values, value)
    }

    /// Appends a constant expression, taking its type from the value.
    ///
    /// The shorthand for the common case. A typed null needs [`Plan::add_expr`] with
    /// [`Expr::Constant`] instead, since a `NULL` literal knows its type from context and not from
    /// itself.
    pub fn add_constant(&mut self, value: Value) -> ExprRef {
        let ty = value.logical_type();
        let reference = self.add_value(value);
        self.add_expr(Expr::Constant(reference), ty)
    }

    /// Interns a string, returning an existing entry if there is one.
    ///
    /// A linear scan, because a plan's string table is table names, column names and function
    /// names and runs to tens of entries. A hash map here would be a second copy of every string
    /// to save a scan nobody can measure.
    ///
    /// # Panics
    ///
    /// If the string table has more than `u32::MAX` entries. Every pool in the arena is indexed by
    /// a `u32` and the reference type says so, so a plan that large is not a plan this type can
    /// hold and there is nothing sensible to return instead.
    pub fn intern(&mut self, text: &str) -> StrRef {
        if let Some(found) = self.strings.iter().position(|held| held == text) {
            return u32::try_from(found).expect("a string table this large cannot be built");
        }
        push(&mut self.strings, text.to_string())
    }

    /// Appends a run to the expression list pool.
    pub fn add_expr_list(&mut self, exprs: &[ExprRef]) -> Slice {
        extend(&mut self.expr_lists, exprs.iter().copied())
    }

    /// Appends a run to the name list pool.
    pub fn add_name_list(&mut self, names: &[StrRef]) -> Slice {
        extend(&mut self.name_lists, names.iter().copied())
    }

    /// Appends a run to the field pool, which is what a scan's or a `VALUES`' output schema is.
    pub fn add_fields(&mut self, fields: &[Field]) -> Slice {
        extend(&mut self.fields, fields.iter().cloned())
    }

    /// Appends a run to the sort key pool.
    pub fn add_sort_keys(&mut self, keys: &[SortKey]) -> Slice {
        extend(&mut self.sort_keys, keys.iter().copied())
    }

    /// Appends a run to the `CASE` arm pool.
    pub fn add_arms(&mut self, arms: &[Arm]) -> Slice {
        extend(&mut self.arms, arms.iter().copied())
    }

    /// Appends a run to the row pool, each element itself a run of the expression list pool.
    pub fn add_rows(&mut self, rows: &[Slice]) -> Slice {
        extend(&mut self.rows, rows.iter().copied())
    }

    // Accessors. Every one panics on an out of range index rather than returning an option,
    // because a reference that does not resolve is a bug in whoever built the plan and the useful
    // thing to do with it is to stop at the place that would otherwise silently do nothing.

    /// The node at `reference`.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena.
    #[must_use]
    pub fn node(&self, reference: NodeRef) -> &Node {
        &self.nodes[reference as usize]
    }

    /// The expression at `reference`.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena.
    #[must_use]
    pub fn expr(&self, reference: ExprRef) -> &Expr {
        &self.exprs[reference as usize]
    }

    /// The type the expression at `reference` evaluates to.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena.
    #[must_use]
    pub fn expr_type(&self, reference: ExprRef) -> &LogicalType {
        &self.types[reference as usize]
    }

    /// The source range carried by a node.
    #[must_use]
    pub fn node_span(&self, reference: NodeRef) -> Span {
        self.node_spans[reference as usize]
    }

    /// The source range carried by an expression.
    #[must_use]
    pub fn expr_span(&self, reference: ExprRef) -> Span {
        self.expr_spans[reference as usize]
    }

    /// The constant at `reference`.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena.
    #[must_use]
    pub fn value(&self, reference: ValueRef) -> &Value {
        &self.values[reference as usize]
    }

    /// The string at `reference`.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena.
    #[must_use]
    pub fn string(&self, reference: StrRef) -> &str {
        &self.strings[reference as usize]
    }

    /// The expression run at `slice`.
    ///
    /// # Panics
    ///
    /// If the run is not in the pool.
    #[must_use]
    pub fn expr_list(&self, slice: Slice) -> &[ExprRef] {
        &self.expr_lists[slice.range()]
    }

    /// The name run at `slice`.
    ///
    /// # Panics
    ///
    /// If the run is not in the pool.
    #[must_use]
    pub fn name_list(&self, slice: Slice) -> &[StrRef] {
        &self.name_lists[slice.range()]
    }

    /// The field run at `slice`.
    ///
    /// # Panics
    ///
    /// If the run is not in the pool.
    #[must_use]
    pub fn field_list(&self, slice: Slice) -> &[Field] {
        &self.fields[slice.range()]
    }

    /// The sort key run at `slice`.
    ///
    /// # Panics
    ///
    /// If the run is not in the pool.
    #[must_use]
    pub fn sort_key_list(&self, slice: Slice) -> &[SortKey] {
        &self.sort_keys[slice.range()]
    }

    /// The `CASE` arm run at `slice`.
    ///
    /// # Panics
    ///
    /// If the run is not in the pool.
    #[must_use]
    pub fn arm_list(&self, slice: Slice) -> &[Arm] {
        &self.arms[slice.range()]
    }

    /// The row run at `slice`.
    ///
    /// # Panics
    ///
    /// If the run is not in the pool.
    #[must_use]
    pub fn row_list(&self, slice: Slice) -> &[Slice] {
        &self.rows[slice.range()]
    }

    // Rewriters. Two of them, both narrow on purpose. A pass that wants to change what an
    // expression computes adds a new expression and points at it, because the type of an
    // expression is stored beside it and a general `expr_mut` is a way to change one without the
    // other. These two cannot: a binding does not carry a type and a node does not have one.

    /// Points a column reference at a different column.
    ///
    /// What column pruning does after it narrows a scan, since dropping a column moves every column
    /// after it up. The type does not change, because it is the same column of the same operator
    /// read from a different position.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena, or if it is not a column reference, both of which are
    /// bugs in the pass rather than anything a plan can be.
    pub fn rebind(&mut self, reference: ExprRef, binding: ColumnBinding) {
        match &mut self.exprs[reference as usize] {
            Expr::Column(held) => *held = binding,
            other => panic!("expression {reference} is {other:?}, not a column"),
        }
    }

    /// The node at `reference`, to be rewritten in place.
    ///
    /// # Panics
    ///
    /// If the reference is not in the arena.
    pub fn node_mut(&mut self, reference: NodeRef) -> &mut Node {
        &mut self.nodes[reference as usize]
    }

    /// Checks the plan invariant.
    ///
    /// `spec/09-optimizer.md` section 9.1 says every pass preserves an invariant that is checked in
    /// debug builds, and this is that check. It is not a type checker and it does not know what
    /// any function returns. What it knows is what this crate can get wrong on its own: an index
    /// that points at nothing, an index that points forwards and could therefore be a cycle, a
    /// projection with more expressions than names, a ragged `VALUES`, a filter on something that
    /// is not boolean, a one-armed conjunction, and an aggregate somewhere an aggregate cannot be.
    ///
    /// Every one of those is a bug that produces a wrong answer or a hang rather than an error, and
    /// `spec/16-testing.md` section 16.9 is specifically about not shipping the first kind.
    ///
    /// # Errors
    ///
    /// With a message naming the node or expression index that broke the rule, because the useful
    /// question about a malformed plan is always which part of it.
    ///
    /// # Panics
    ///
    /// If a pool has more than `u32::MAX` entries, which is the same bound every reference in the
    /// arena already carries.
    pub fn validate(&self) -> Result<()> {
        if self.nodes.len() != self.node_spans.len() {
            return Err(Error::internal(format!(
                "the plan has {} nodes and {} node spans",
                self.nodes.len(),
                self.node_spans.len()
            )));
        }
        if self.exprs.len() != self.expr_spans.len() {
            return Err(Error::internal(format!(
                "the plan has {} expressions and {} expression spans",
                self.exprs.len(),
                self.expr_spans.len()
            )));
        }
        if self.exprs.len() != self.types.len() {
            return Err(Error::internal(format!(
                "the plan has {} expressions and {} types",
                self.exprs.len(),
                self.types.len()
            )));
        }
        if self.root as usize >= self.nodes.len() {
            return Err(Error::internal(format!(
                "the plan is rooted at node {} and has {} nodes",
                self.root,
                self.nodes.len()
            )));
        }
        for index in 0..self.exprs.len() {
            self.validate_expr(u32::try_from(index).expect("index came from a length"))?;
        }
        for index in 0..self.nodes.len() {
            self.validate_node(u32::try_from(index).expect("index came from a length"))?;
        }
        Ok(())
    }

    fn validate_expr(&self, reference: ExprRef) -> Result<()> {
        let fail = |what: &str| Err(Error::internal(format!("expression {reference} {what}")));
        let backwards = |operand: ExprRef| -> Result<()> {
            if operand < reference {
                Ok(())
            } else {
                Err(Error::internal(format!(
                    "expression {reference} refers to expression {operand}, which is not behind it"
                )))
            }
        };
        match *self.expr(reference) {
            Expr::Column(_) => {}
            Expr::Constant(value) => {
                if value as usize >= self.values.len() {
                    return fail("names a constant that is not in the value table");
                }
                // A null literal takes its type from context, so it is the one case where the
                // stored type is allowed to disagree with the value.
                let held = self.value(value);
                if !held.is_null() && held.logical_type() != *self.expr_type(reference) {
                    return fail("is a constant whose type disagrees with the value it holds");
                }
            }
            Expr::Cast { input, .. } => backwards(input)?,
            Expr::Compare { left, right, .. } => {
                backwards(left)?;
                backwards(right)?;
                if *self.expr_type(reference) != LogicalType::Boolean {
                    return fail("is a comparison that does not produce BOOLEAN");
                }
            }
            Expr::Conjunction { children, .. } => {
                if children.len < 2 {
                    return fail("is a conjunction with fewer than two operands");
                }
                for &child in self.checked_expr_list(children, reference)? {
                    backwards(child)?;
                }
                if *self.expr_type(reference) != LogicalType::Boolean {
                    return fail("is a conjunction that does not produce BOOLEAN");
                }
            }
            Expr::Function { name, args }
            | Expr::Aggregate { name, args, .. }
            | Expr::Window { name, args, .. } => {
                if name as usize >= self.strings.len() {
                    return fail("names a function that is not in the string table");
                }
                for &arg in self.checked_expr_list(args, reference)? {
                    backwards(arg)?;
                }
                if let Expr::Aggregate { filter: Some(filter), .. }
                | Expr::Window { filter: Some(filter), .. } = *self.expr(reference)
                {
                    backwards(filter)?;
                    if *self.expr_type(filter) != LogicalType::Boolean {
                        return fail("has a FILTER that is not BOOLEAN");
                    }
                }
            }
            Expr::Case { arms, otherwise } => {
                if arms.is_empty() {
                    return fail("is a CASE with no arms");
                }
                let end = arms.start as usize + arms.len as usize;
                if end > self.arms.len() {
                    return fail("names an arm run that is not in the pool");
                }
                for arm in self.arm_list(arms) {
                    backwards(arm.when)?;
                    backwards(arm.then)?;
                    if *self.expr_type(arm.when) != LogicalType::Boolean {
                        return fail("has a WHEN that is not BOOLEAN");
                    }
                }
                if let Some(otherwise) = otherwise {
                    backwards(otherwise)?;
                }
            }
        }
        Ok(())
    }

    fn validate_node(&self, reference: NodeRef) -> Result<()> {
        let node = self.node(reference);
        let fail = |what: &str| {
            Err(Error::internal(format!("node {reference}, which is a {}, {what}", node.keyword())))
        };
        for child in node.children().into_iter().flatten() {
            if child >= reference {
                return Err(Error::internal(format!(
                    "node {reference} has child {child}, which is not behind it"
                )));
            }
        }
        match *node {
            Node::Dummy | Node::CrossProduct { .. } => {}
            Node::Get { catalog, schema, table, alias, columns, .. } => {
                for name in [catalog, schema, table, alias] {
                    if name as usize >= self.strings.len() {
                        return fail("names a string that is not in the table");
                    }
                }
                self.checked_field_list(columns, reference)?;
            }
            Node::Values { columns, rows, .. } => {
                let width = self.checked_field_list(columns, reference)?.len();
                let end = rows.start as usize + rows.len as usize;
                if end > self.rows.len() {
                    return fail("names a row run that is not in the pool");
                }
                for row in self.row_list(rows) {
                    if self.checked_expr_list(*row, reference)?.len() != width {
                        return fail("has a row whose length is not the number of columns");
                    }
                }
            }
            Node::TableFunction { function, args, options, settings, columns, .. } => {
                if function as usize >= self.strings.len() {
                    return fail("names a string that is not in the table");
                }
                self.checked_field_list(columns, reference)?;
                self.checked_expr_list(args, reference)?;
                if self.checked_expr_list(settings, reference)?.len() != options.len as usize {
                    return fail("has a named parameter with no value or a value with no name");
                }
                let end = options.start as usize + options.len as usize;
                if end > self.name_lists.len() {
                    return fail("names a name run that is not in the pool");
                }
                for &name in self.name_list(options) {
                    if name as usize >= self.strings.len() {
                        return fail("names a parameter that is not in the string table");
                    }
                }
            }
            Node::Fetch { args, columns, row, .. } => {
                self.checked_field_list(columns, reference)?;
                if self.checked_expr_list(args, reference)?.len() != 1 {
                    return fail("reads other than exactly one file, which no ordinal identifies");
                }
                self.checked_expr(row, reference)?;
                if *self.expr_type(row) != LogicalType::BigInt {
                    return fail("takes its ordinals from an expression that is not BIGINT");
                }
            }
            Node::TableFetch { catalog, schema, table, columns, row, .. } => {
                for name in [catalog, schema, table] {
                    if name as usize >= self.strings.len() {
                        return fail("names a string that is not in the table");
                    }
                }
                self.checked_field_list(columns, reference)?;
                self.checked_expr(row, reference)?;
                if *self.expr_type(row) != LogicalType::BigInt {
                    return fail("takes its ordinals from an expression that is not BIGINT");
                }
            }
            Node::Filter { predicate, .. } => {
                self.checked_expr(predicate, reference)?;
                if *self.expr_type(predicate) != LogicalType::Boolean {
                    return fail("filters on an expression that is not BOOLEAN");
                }
            }
            Node::Project { exprs, names, .. } => {
                let count = self.checked_expr_list(exprs, reference)?.len();
                let end = names.start as usize + names.len as usize;
                if end > self.name_lists.len() {
                    return fail("names a name run that is not in the pool");
                }
                if self.name_list(names).len() != count {
                    return fail("has a different number of names and expressions");
                }
                for &name in self.name_list(names) {
                    if name as usize >= self.strings.len() {
                        return fail("names an output name that is not in the string table");
                    }
                }
            }
            Node::Aggregate { groups, aggregates, .. } => {
                for &group in self.checked_expr_list(groups, reference)? {
                    if matches!(self.expr(group), Expr::Aggregate { .. }) {
                        return fail("groups by an aggregate");
                    }
                }
                for &aggregate in self.checked_expr_list(aggregates, reference)? {
                    if !matches!(self.expr(aggregate), Expr::Aggregate { .. }) {
                        return fail(
                            "has something in its aggregate list that is not an aggregate",
                        );
                    }
                }
            }
            Node::Window { partition, order, frame, expressions, .. } => {
                self.checked_expr_list(partition, reference)?;
                let end = order.start as usize + order.len as usize;
                if end > self.sort_keys.len() {
                    return fail("names a window order run that is not in the pool");
                }
                for key in self.sort_key_list(order) {
                    self.checked_expr(key.expr, reference)?;
                }
                for bound in [frame.start, frame.end] {
                    match bound {
                        crate::WindowBound::Preceding(offset)
                        | crate::WindowBound::Following(offset) => {
                            self.checked_expr(offset, reference)?;
                        }
                        crate::WindowBound::UnboundedPreceding
                        | crate::WindowBound::CurrentRow
                        | crate::WindowBound::UnboundedFollowing => {}
                    }
                }
                if matches!(frame.start, crate::WindowBound::UnboundedFollowing)
                    || matches!(frame.end, crate::WindowBound::UnboundedPreceding)
                {
                    return fail("has an impossible frame boundary");
                }
                let expressions = self.checked_expr_list(expressions, reference)?;
                if expressions.is_empty() {
                    return fail("has no window expressions");
                }
                for &expression in expressions {
                    if !matches!(self.expr(expression), Expr::Window { .. }) {
                        return fail(
                            "has something in its expression list that is not a window function",
                        );
                    }
                }
            }
            Node::Sort { keys, .. } | Node::TopN { keys, .. } => {
                let end = keys.start as usize + keys.len as usize;
                if end > self.sort_keys.len() {
                    return fail("names a sort key run that is not in the pool");
                }
                if keys.is_empty() {
                    return fail("sorts on nothing");
                }
                for key in self.sort_key_list(keys) {
                    self.checked_expr(key.expr, reference)?;
                }
            }
            Node::Limit { .. } => {}
            Node::Distinct { on, .. } => {
                self.checked_expr_list(on, reference)?;
            }
            Node::Join { conditions, .. } => {
                for &condition in self.checked_expr_list(conditions, reference)? {
                    if *self.expr_type(condition) != LogicalType::Boolean {
                        return fail("joins on a condition that is not BOOLEAN");
                    }
                }
            }
            Node::DependentJoin { kind, conditions, .. } => {
                if matches!(kind, JoinKind::Right | JoinKind::Full | JoinKind::Positional) {
                    return fail("has a join kind that cannot preserve an outer row dependency");
                }
                for &condition in self.checked_expr_list(conditions, reference)? {
                    if *self.expr_type(condition) != LogicalType::Boolean {
                        return fail("joins on a condition that is not BOOLEAN");
                    }
                }
            }
            Node::SetOp { .. } => {}
        }

        // An aggregate is legal only as a direct element of an Aggregate node's aggregate list,
        // per the note on Expr::Aggregate. Everything else that reaches one is a plan the printer
        // would emit and the reader would misread as a scalar function, which is a wrong answer
        // rather than an error.
        for (expr, aggregate_allowed, window_allowed) in self.top_level_exprs(node) {
            if window_allowed {
                if let Expr::Window { args, filter, .. } = *self.expr(expr) {
                    let nested = self.expr_list(args).iter().chain(filter.iter()).any(|&child| {
                        self.reaches_a_window(child) || self.reaches_an_aggregate(child)
                    });
                    if nested {
                        return fail("has a window or aggregate inside a window function");
                    }
                    continue;
                }
            }
            if self.reaches_a_window(expr) {
                return fail("has a window function outside a window list");
            }
            if aggregate_allowed {
                if let Expr::Aggregate { args, filter, .. } = *self.expr(expr) {
                    let nested = self
                        .expr_list(args)
                        .iter()
                        .chain(filter.iter())
                        .any(|&child| self.reaches_an_aggregate(child));
                    if nested {
                        return fail("has an aggregate inside an aggregate");
                    }
                    continue;
                }
            }
            if self.reaches_an_aggregate(expr) {
                return fail("has an aggregate outside an aggregate list");
            }
        }
        Ok(())
    }

    /// Every expression a node holds directly, paired with whether an aggregate is allowed there.
    ///
    /// One list rather than a rule restated in each arm of `validate_node`, because the rule is
    /// about the whole node set and a rule stated twelve times is a rule that is wrong in one of
    /// them. Runs after the per-operator checks, so every run named here is known to be in range.
    fn top_level_exprs(&self, node: &Node) -> Vec<(ExprRef, bool, bool)> {
        let plain = |list: &[ExprRef]| -> Vec<(ExprRef, bool, bool)> {
            list.iter().map(|&expr| (expr, false, false)).collect()
        };
        match *node {
            Node::Get { .. }
            | Node::Dummy
            | Node::CrossProduct { .. }
            | Node::SetOp { .. }
            | Node::Limit { .. } => Vec::new(),
            Node::Values { rows, .. } => {
                self.row_list(rows).iter().flat_map(|row| plain(self.expr_list(*row))).collect()
            }
            Node::TableFunction { args, .. } => plain(self.expr_list(args)),
            Node::Fetch { args, row, .. } => {
                let mut held = plain(self.expr_list(args));
                held.push((row, false, false));
                held
            }
            Node::TableFetch { row, .. } => vec![(row, false, false)],
            Node::Filter { predicate, .. } => vec![(predicate, false, false)],
            Node::Project { exprs, .. } => plain(self.expr_list(exprs)),
            Node::Aggregate { groups, aggregates, .. } => {
                let mut all = plain(self.expr_list(groups));
                all.extend(self.expr_list(aggregates).iter().map(|&expr| (expr, true, false)));
                all
            }
            Node::Window { partition, order, frame, expressions, .. } => {
                let mut all = plain(self.expr_list(partition));
                all.extend(self.sort_key_list(order).iter().map(|key| (key.expr, false, false)));
                for bound in [frame.start, frame.end] {
                    if let crate::WindowBound::Preceding(offset)
                    | crate::WindowBound::Following(offset) = bound
                    {
                        all.push((offset, false, false));
                    }
                }
                all.extend(self.expr_list(expressions).iter().map(|&expr| (expr, false, true)));
                all
            }
            Node::Sort { keys, .. } | Node::TopN { keys, .. } => {
                self.sort_key_list(keys).iter().map(|key| (key.expr, false, false)).collect()
            }
            Node::Distinct { on, .. } => plain(self.expr_list(on)),
            Node::Join { conditions, .. } | Node::DependentJoin { conditions, .. } => {
                plain(self.expr_list(conditions))
            }
        }
    }

    /// Whether an aggregate is anywhere in this expression, itself included.
    ///
    /// A plain recursion terminates because operands point backwards, which is checked before this
    /// runs.
    fn reaches_an_aggregate(&self, reference: ExprRef) -> bool {
        match *self.expr(reference) {
            Expr::Aggregate { .. } => true,
            Expr::Window { args, filter, .. } => {
                self.expr_list(args).iter().any(|&child| self.reaches_an_aggregate(child))
                    || filter.is_some_and(|child| self.reaches_an_aggregate(child))
            }
            Expr::Column(_) | Expr::Constant(_) => false,
            Expr::Cast { input, .. } => self.reaches_an_aggregate(input),
            Expr::Compare { left, right, .. } => {
                self.reaches_an_aggregate(left) || self.reaches_an_aggregate(right)
            }
            Expr::Conjunction { children: list, .. } | Expr::Function { args: list, .. } => {
                self.expr_list(list).iter().any(|&child| self.reaches_an_aggregate(child))
            }
            Expr::Case { arms, otherwise } => {
                self.arm_list(arms).iter().any(|arm| {
                    self.reaches_an_aggregate(arm.when) || self.reaches_an_aggregate(arm.then)
                }) || otherwise.is_some_and(|child| self.reaches_an_aggregate(child))
            }
        }
    }

    fn reaches_a_window(&self, reference: ExprRef) -> bool {
        match *self.expr(reference) {
            Expr::Window { .. } => true,
            Expr::Column(_) | Expr::Constant(_) => false,
            Expr::Cast { input, .. } => self.reaches_a_window(input),
            Expr::Compare { left, right, .. } => {
                self.reaches_a_window(left) || self.reaches_a_window(right)
            }
            Expr::Conjunction { children, .. }
            | Expr::Function { args: children, .. }
            | Expr::Aggregate { args: children, filter: None, .. } => {
                self.expr_list(children).iter().any(|&child| self.reaches_a_window(child))
            }
            Expr::Aggregate { args, filter: Some(filter), .. } => {
                self.expr_list(args).iter().any(|&child| self.reaches_a_window(child))
                    || self.reaches_a_window(filter)
            }
            Expr::Case { arms, otherwise } => {
                self.arm_list(arms)
                    .iter()
                    .any(|arm| self.reaches_a_window(arm.when) || self.reaches_a_window(arm.then))
                    || otherwise.is_some_and(|child| self.reaches_a_window(child))
            }
        }
    }

    fn checked_expr(&self, reference: ExprRef, node: NodeRef) -> Result<()> {
        if reference as usize >= self.exprs.len() {
            return Err(Error::internal(format!(
                "node {node} names expression {reference}, which is not in the arena"
            )));
        }
        Ok(())
    }

    fn checked_expr_list(&self, slice: Slice, owner: u32) -> Result<&[ExprRef]> {
        let end = slice.start as usize + slice.len as usize;
        if end > self.expr_lists.len() {
            return Err(Error::internal(format!(
                "{owner} names an expression run that is not in the pool"
            )));
        }
        let list = self.expr_list(slice);
        for &reference in list {
            if reference as usize >= self.exprs.len() {
                return Err(Error::internal(format!(
                    "{owner} names expression {reference}, which is not in the arena"
                )));
            }
        }
        Ok(list)
    }

    fn checked_field_list(&self, slice: Slice, owner: u32) -> Result<&[Field]> {
        let end = slice.start as usize + slice.len as usize;
        if end > self.fields.len() {
            return Err(Error::internal(format!(
                "{owner} names a field run that is not in the pool"
            )));
        }
        Ok(self.field_list(slice))
    }
}

/// Appends and hands back the index, which is the only place a pool length becomes a reference.
///
/// # Panics
///
/// If the pool has more than `u32::MAX` entries, which is a plan of four billion nodes and is a
/// bug somewhere upstream rather than a query anybody wrote.
fn merge_span(left: Span, right: Span) -> Span {
    match (left.is_empty(), right.is_empty()) {
        (true, _) => right,
        (_, true) => left,
        (false, false) => Span::new(left.start.min(right.start), left.end.max(right.end)),
    }
}

fn push<T>(pool: &mut Vec<T>, item: T) -> u32 {
    let index = u32::try_from(pool.len()).expect("a plan arena cannot hold four billion entries");
    pool.push(item);
    index
}

/// Appends a run and hands back the slice that names it.
///
/// # Panics
///
/// As [`push`].
fn extend<T>(pool: &mut Vec<T>, items: impl Iterator<Item = T>) -> Slice {
    let start = u32::try_from(pool.len()).expect("a plan arena cannot hold four billion entries");
    pool.extend(items);
    let len =
        u32::try_from(pool.len()).expect("a plan arena cannot hold four billion entries") - start;
    Slice { start, len }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::{ColumnBinding, CompareOp};

    #[test]
    fn a_fresh_plan_is_a_valid_plan() {
        let plan = Plan::new();
        assert_eq!(*plan.node(plan.root()), Node::Dummy);
        plan.validate().expect("an empty plan is one row and no columns, which is legal");
    }

    #[test]
    fn every_node_and_expression_carries_a_span_through_an_in_place_rewrite() {
        let mut plan = Plan::new();
        let expr = plan.add_expr_at(
            Expr::Column(ColumnBinding::new(7, 0)),
            LogicalType::Integer,
            Span::new(7, 12),
        );
        let node = plan
            .add_node_at(Node::Filter { input: plan.root(), predicate: expr }, Span::new(0, 18));
        let root = plan.root();
        let exprs = plan.add_expr_list(&[expr]);
        *plan.node_mut(node) = Node::Project { input: root, index: 9, exprs, names: Slice::EMPTY };
        assert_eq!(plan.expr_span(expr), Span::new(7, 12));
        assert_eq!(plan.node_span(node), Span::new(0, 18));
        assert_eq!(plan.node_spans.len(), plan.nodes.len());
        assert_eq!(plan.expr_spans.len(), plan.exprs.len());
    }

    #[test]
    fn interning_the_same_string_twice_gives_the_same_reference() {
        let mut plan = Plan::new();
        let first = plan.intern("hits");
        let second = plan.intern("hits");
        let other = plan.intern("visits");
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(plan.string(first), "hits");
    }

    #[test]
    fn a_constant_takes_its_type_from_its_value() {
        let mut plan = Plan::new();
        let one = plan.add_constant(Value::Integer(1));
        assert_eq!(*plan.expr_type(one), LogicalType::Integer);
        plan.validate().expect("a constant that agrees with itself is valid");
    }

    /// The one case where a constant's stored type is allowed to differ from the value's, because
    /// `NULL::VARCHAR` is a varchar expression holding an untyped null.
    #[test]
    fn a_typed_null_is_allowed_to_disagree_with_its_value() {
        let mut plan = Plan::new();
        let null = plan.add_value(Value::Null);
        plan.add_expr(Expr::Constant(null), LogicalType::Varchar);
        plan.validate().expect("a typed null is the point of carrying types separately");
    }

    #[test]
    fn a_constant_that_disagrees_with_its_value_is_caught() {
        let mut plan = Plan::new();
        let value = plan.add_value(Value::Integer(1));
        plan.add_expr(Expr::Constant(value), LogicalType::Varchar);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("disagrees"), "unhelpful message: {message}");
    }

    #[test]
    fn a_filter_on_something_that_is_not_boolean_is_caught() {
        let mut plan = Plan::new();
        let one = plan.add_constant(Value::Integer(1));
        let filter = plan.add_node(Node::Filter { input: 0, predicate: one });
        plan.set_root(filter);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("BOOLEAN"), "unhelpful message: {message}");
    }

    #[test]
    fn a_projection_with_more_expressions_than_names_is_caught() {
        let mut plan = Plan::new();
        let one = plan.add_constant(Value::Integer(1));
        let two = plan.add_constant(Value::Integer(2));
        let exprs = plan.add_expr_list(&[one, two]);
        let name = plan.intern("a");
        let names = plan.add_name_list(&[name]);
        let project = plan.add_node(Node::Project { input: 0, index: 1, exprs, names });
        plan.set_root(project);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("names and expressions"), "unhelpful message: {message}");
    }

    #[test]
    fn a_ragged_values_is_caught() {
        let mut plan = Plan::new();
        let one = plan.add_constant(Value::Integer(1));
        let two = plan.add_constant(Value::Integer(2));
        let wide = plan.add_expr_list(&[one, two]);
        let narrow = plan.add_expr_list(&[one]);
        let rows = plan.add_rows(&[wide, narrow]);
        let columns = plan.add_fields(&[
            Field::new("a", LogicalType::Integer),
            Field::new("b", LogicalType::Integer),
        ]);
        let values = plan.add_node(Node::Values { index: 0, columns, rows });
        plan.set_root(values);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("number of columns"), "unhelpful message: {message}");
    }

    #[test]
    fn an_aggregate_outside_an_aggregate_list_is_caught() {
        let mut plan = Plan::new();
        let name = plan.intern("count_star");
        let count = plan.add_expr(
            Expr::Aggregate { name, args: Slice::EMPTY, distinct: false, filter: None },
            LogicalType::BigInt,
        );
        let zero = plan.add_constant(Value::BigInt(0));
        let compare = plan.add_expr(
            Expr::Compare { op: CompareOp::Greater, left: count, right: zero },
            LogicalType::Boolean,
        );
        let filter = plan.add_node(Node::Filter { input: 0, predicate: compare });
        plan.set_root(filter);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("aggregate outside"), "unhelpful message: {message}");
    }

    #[test]
    fn an_aggregate_inside_an_aggregate_list_is_fine() {
        let mut plan = Plan::new();
        let name = plan.intern("count_star");
        let count = plan.add_expr(
            Expr::Aggregate { name, args: Slice::EMPTY, distinct: false, filter: None },
            LogicalType::BigInt,
        );
        let aggregates = plan.add_expr_list(&[count]);
        let aggregate =
            plan.add_node(Node::Aggregate { input: 0, index: 1, groups: Slice::EMPTY, aggregates });
        plan.set_root(aggregate);
        plan.validate().expect("this is the one place an aggregate belongs");
    }

    #[test]
    fn a_window_function_outside_a_window_list_is_caught() {
        let mut plan = Plan::new();
        let name = plan.intern("row_number");
        let call = plan.add_expr(
            Expr::Window {
                name,
                args: Slice::EMPTY,
                distinct: false,
                filter: None,
                ignore_nulls: false,
            },
            LogicalType::BigInt,
        );
        let exprs = plan.add_expr_list(&[call]);
        let label = plan.intern("n");
        let names = plan.add_name_list(&[label]);
        let project = plan.add_node(Node::Project { input: 0, index: 1, exprs, names });
        plan.set_root(project);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("window function outside"), "unhelpful message: {message}");
    }

    /// The backwards-reference rule is what makes a cycle impossible, so the check for it has to
    /// actually fire rather than being a comment about how nobody would do that.
    #[test]
    fn a_node_that_refers_to_itself_is_caught() {
        let mut plan = Plan::new();
        let filter = plan.add_node(Node::Filter { input: 0, predicate: 0 });
        let one = plan.add_constant(Value::Boolean(true));
        plan.nodes[filter as usize] = Node::Filter { input: filter, predicate: one };
        plan.set_root(filter);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("not behind it"), "unhelpful message: {message}");
    }

    #[test]
    fn an_expression_that_refers_forwards_is_caught() {
        let mut plan = Plan::new();
        let left = plan.add_constant(Value::Integer(1));
        let compare = plan.add_expr(
            Expr::Compare { op: CompareOp::Equal, left, right: left },
            LogicalType::Boolean,
        );
        plan.exprs[compare as usize] =
            Expr::Compare { op: CompareOp::Equal, left, right: compare + 1 };
        plan.add_constant(Value::Integer(2));
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("not behind it"), "unhelpful message: {message}");
    }

    #[test]
    fn a_root_that_is_not_in_the_arena_is_caught() {
        let mut plan = Plan::new();
        plan.set_root(17);
        let message = plan.validate().unwrap_err().to_string();
        assert!(message.contains("rooted at node 17"), "unhelpful message: {message}");
    }

    #[test]
    fn a_column_binding_is_two_numbers_and_nothing_else() {
        let binding = ColumnBinding::new(3, 7);
        assert_eq!(binding.table, 3);
        assert_eq!(binding.column, 7);
        assert_eq!(size_of::<ColumnBinding>(), 8);
    }
}
