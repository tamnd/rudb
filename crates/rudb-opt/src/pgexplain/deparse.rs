//! Expressions as PostgreSQL writes them in `EXPLAIN`.
//!
//! A port of the part of `ruleutils.c` that `explain.c` calls, which is `deparse_expression` over
//! the expressions of a plan node. A column is written `refname.column` when the reader needs to
//! know which relation it came from and `column` otherwise, the way `explain.c` decides it per
//! property. An operator is in parentheses with its operands, a cast is `(x)::type` and a constant
//! carries its type unless the type is the one a bare literal would get.
//!
//! The names come from [`Names`], which walks the plan once and records what every table index
//! means: a relation with a reference name and column names, the expressions an operator computes,
//! or the left input a set operation renames.

use std::collections::{HashMap, HashSet};

use rudb_common::{LogicalType, Value};
use rudb_pgtypes::keywords::quote_identifier;
use rudb_pgtypes::{OutputSettings, format_type_with_typmod, oid, pg_type, text_values};
use rudb_plan::{ColumnBinding, CompareOp, Expr, ExprRef, Node, NodeRef, Plan};
use rudb_vector::Vector;

/// The kernels the binder writes for a PostgreSQL operator whose rule differs from DuckDB's, and
/// the operator each one is.
const OPERATOR_KERNELS: [(&str, &str); 5] = [
    ("__rudb_pg_float_add", "+"),
    ("__rudb_pg_float_subtract", "-"),
    ("__rudb_pg_float_multiply", "*"),
    ("__rudb_pg_float_divide", "/"),
    ("__rudb_pg_regex_match", "~"),
];

/// The kernels the binder writes for a PostgreSQL cast, which `EXPLAIN` writes as the cast.
const CAST_KERNELS: [&str; 7] = [
    "__rudb_pg_bpchar",
    "__rudb_pg_bpchar_cut",
    "__rudb_pg_varchar",
    "__rudb_pg_number",
    "__rudb_pg_name",
    "__rudb_pg_input",
    "__rudb_pg_output",
];

/// The prefix of every kernel the binder writes for a PostgreSQL function.
const KERNEL_PREFIX: &str = "__rudb_pg_";

/// The functions `ruleutils.c` writes as SQL syntax in capitals rather than as a call.
const SQL_FUNCTIONS: [(&str, &str); 4] =
    [("coalesce", "COALESCE"), ("greatest", "GREATEST"), ("least", "LEAST"), ("nullif", "NULLIF")];

/// What one table index means.
enum Source {
    /// A relation in the range table: a table, a `VALUES` list, a function or a CTE.
    Relation { refname: String, columns: Vec<String> },
    /// The expressions an operator computes, one per column.
    Computed(Vec<ExprRef>),
    /// A set operation, whose columns are its left input's.
    Renamed(Vec<ColumnBinding>),
}

/// What every table index of a plan means, and the name of every window.
pub(super) struct Names {
    sources: HashMap<u32, Source>,
    windows: HashMap<ExprRef, String>,
    window_nodes: HashMap<NodeRef, String>,
    relations: usize,
}

impl Names {
    /// Walks the plan and names everything in it.
    ///
    /// A reference name is the alias, or the table name when there is no alias. Two relations with
    /// the same name get `_1`, `_2` and so on after the second and later, which is what
    /// `set_rtable_names` does. Windows are numbered from the bottom of the plan up, `w1` first.
    pub(super) fn of(plan: &Plan) -> Self {
        let mut names = Self {
            sources: HashMap::new(),
            windows: HashMap::new(),
            window_nodes: HashMap::new(),
            relations: 0,
        };
        let mut used = HashSet::new();
        names.walk(plan, plan.root(), &mut used);
        names
    }

    /// How many relations the plan reads, which is `rtable_size` in `explain.c`.
    pub(super) fn relations(&self) -> usize {
        self.relations
    }

    /// The reference name of the relation behind a table index.
    pub(super) fn refname(&self, index: u32) -> Option<&str> {
        match self.sources.get(&index) {
            Some(Source::Relation { refname, .. }) => Some(refname),
            _ => None,
        }
    }

    /// The name of the window a window node computes.
    pub(super) fn window(&self, node: NodeRef) -> &str {
        self.window_nodes.get(&node).map_or("w1", String::as_str)
    }

    /// A relation is named before what is under it and after what is before it, which is the
    /// order of the range table: the outer query before a `WITH` it reads, and the left side of a
    /// join before the right.
    fn walk(&mut self, plan: &Plan, at: NodeRef, used: &mut HashSet<String>) {
        let column_names = |columns| -> Vec<String> {
            plan.field_list(columns).iter().map(|field| field.name.clone()).collect()
        };
        match *plan.node(at) {
            Node::Get { table, alias, index, columns, .. } => {
                let name = if plan.string(alias).is_empty() { table } else { alias };
                let refname = unique(plan.string(name), used);
                self.relation(index, refname, column_names(columns));
            }
            Node::Values { index, columns, .. } => {
                let width = plan.field_list(columns).len();
                let refname = unique("*VALUES*", used);
                self.relation(index, refname, (1..=width).map(|n| format!("column{n}")).collect());
            }
            Node::TableFunction { index, function, columns, .. } => {
                let refname = unique(function_name(plan.string(function)), used);
                self.relation(index, refname, column_names(columns));
            }
            Node::CteScan { index, name, columns, .. }
            | Node::RecursiveCte { index, name, columns, .. } => {
                let refname = unique(plan.string(name), used);
                self.relation(index, refname, column_names(columns));
            }
            Node::Consistent { index, columns, .. } => {
                let refname = unique("consistent", used);
                self.relation(index, refname, column_names(columns));
            }
            _ => {}
        }
        match *plan.node(at) {
            Node::MaterializedCte { definition, body, .. } => {
                self.walk(plan, body, used);
                match *plan.node(definition) {
                    // A recursive definition is written as its union, which is not a relation
                    // and takes no name from the scans.
                    Node::RecursiveCte { index, name, columns, .. } => {
                        self.relation(index, plan.string(name).to_owned(), column_names(columns));
                        for child in super::inputs(plan, definition) {
                            self.walk(plan, child, used);
                        }
                    }
                    _ => self.walk(plan, definition, used),
                }
            }
            _ => {
                for child in super::inputs(plan, at) {
                    self.walk(plan, child, used);
                }
            }
        }
        match *plan.node(at) {
            Node::TableFetch { table, index, columns, .. } => {
                let refname = unique(plan.string(table), used);
                self.relation(index, refname, column_names(columns));
            }
            Node::LateralFunction { index, function, columns, .. } => {
                let refname = unique(function_name(plan.string(function)), used);
                self.relation(index, refname, column_names(columns));
            }
            Node::Fetch { index, columns, .. } => {
                let refname = unique("fetch", used);
                self.relation(index, refname, column_names(columns));
            }
            Node::Project { index, exprs, .. } => {
                self.sources.insert(index, Source::Computed(plan.expr_list(exprs).to_vec()));
            }
            Node::Aggregate { index, groups, aggregates, .. } => {
                let mut computed = plan.expr_list(groups).to_vec();
                computed.extend_from_slice(plan.expr_list(aggregates));
                self.sources.insert(index, Source::Computed(computed));
            }
            Node::Window { index, expressions, .. } => {
                let name = format!("w{}", self.window_nodes.len() + 1);
                for &expr in plan.expr_list(expressions) {
                    self.windows.insert(expr, name.clone());
                }
                self.window_nodes.insert(at, name);
                self.sources.insert(index, Source::Computed(plan.expr_list(expressions).to_vec()));
            }
            Node::SetOp { left, index, .. } => {
                let renamed = super::outputs(plan, left).into_iter().map(|(b, _)| b).collect();
                self.sources.insert(index, Source::Renamed(renamed));
            }
            _ => {}
        }
    }

    fn relation(&mut self, index: u32, refname: String, columns: Vec<String>) {
        self.relations += 1;
        self.sources.insert(index, Source::Relation { refname, columns });
    }
}

/// A reference name no relation has yet.
fn unique(name: &str, used: &mut HashSet<String>) -> String {
    let mut candidate = name.to_owned();
    let mut n = 0;
    while used.contains(&candidate) {
        n += 1;
        candidate = format!("{name}_{n}");
    }
    used.insert(candidate.clone());
    candidate
}

/// The name of a function as PostgreSQL knows it, without the prefix of a PostgreSQL kernel.
pub(super) fn function_name(name: &str) -> &str {
    name.strip_prefix(KERNEL_PREFIX).unwrap_or(name)
}

/// Writes expressions of one plan node.
pub(super) struct Deparse<'a> {
    pub(super) plan: &'a Plan,
    pub(super) names: &'a Names,
    pub(super) settings: &'a OutputSettings<'a>,
    /// Whether a column says which relation it is from.
    pub(super) prefix: bool,
    /// The table index of the node whose own output is being written. A reference to one of its
    /// columns is the expression that computes it. A reference to any other computed column is
    /// the expression in parentheses, because `ruleutils.c` writes a reference to a subplan's
    /// output that is not a plain column that way.
    pub(super) own: Option<u32>,
}

impl Deparse<'_> {
    /// One expression.
    pub(super) fn expr(&self, at: ExprRef) -> String {
        let plan = self.plan;
        match plan.expr(at) {
            Expr::Column(binding) | Expr::LambdaParam(binding) => self.column(*binding),
            Expr::Constant(value) => self.constant(plan.value(*value), plan.expr_type(at)),
            Expr::Cast { input, .. } => self.cast(*input, plan.expr_type(at)),
            Expr::Compare { op, left, right } => self.compare(*op, *left, *right),
            Expr::Conjunction { op, children } => {
                let parts: Vec<String> =
                    plan.expr_list(*children).iter().map(|&child| self.expr(child)).collect();
                format!("({})", parts.join(&format!(" {} ", op.keyword())))
            }
            Expr::Function { name, args } => self.function(at, plan.string(*name), *args),
            Expr::Aggregate { name, args, distinct, filter } => {
                self.aggregate(plan.string(*name), *args, *distinct, *filter)
            }
            Expr::Window { name, args, distinct, filter, .. } => {
                let call = self.aggregate(plan.string(*name), *args, *distinct, *filter);
                let window = self.names.windows.get(&at).map_or("w1", String::as_str);
                format!("{call} OVER {window}")
            }
            Expr::Case { arms, otherwise } => {
                let mut out = "CASE".to_owned();
                for arm in plan.arm_list(*arms) {
                    out.push_str(&format!(
                        " WHEN {} THEN {}",
                        self.expr(arm.when),
                        self.expr(arm.then)
                    ));
                }
                let otherwise = match otherwise {
                    Some(otherwise) => self.expr(*otherwise),
                    None => self.constant(&Value::Null, plan.expr_type(at)),
                };
                format!("{out} ELSE {otherwise} END")
            }
            Expr::Lambda { body, .. } => self.expr(*body),
        }
    }

    /// A list of expressions, each on its own, as `Output`, `Sort Key` and `Group Key` hold them.
    pub(super) fn exprs(&self, list: &[ExprRef]) -> Vec<String> {
        list.iter().map(|&expr| self.expr(expr)).collect()
    }

    /// A column, which is a relation's column or a reference to what an operator computed.
    pub(super) fn column(&self, binding: ColumnBinding) -> String {
        match self.names.sources.get(&binding.table) {
            Some(Source::Relation { refname, columns }) => {
                let column =
                    columns.get(binding.column as usize).map_or("?column?", String::as_str);
                match self.prefix {
                    true => format!("{}.{}", quote_identifier(refname), quote_identifier(column)),
                    false => quote_identifier(column).into_owned(),
                }
            }
            Some(Source::Computed(exprs)) => {
                let Some(&expr) = exprs.get(binding.column as usize) else {
                    return "?column?".to_owned();
                };
                let text = self.expr(expr);
                let plain = matches!(self.plan.expr(expr), Expr::Column(_));
                match plain || self.own == Some(binding.table) {
                    true => text,
                    false => format!("({text})"),
                }
            }
            Some(Source::Renamed(columns)) => match columns.get(binding.column as usize) {
                Some(&renamed) => self.column(renamed),
                None => "?column?".to_owned(),
            },
            None => "?column?".to_owned(),
        }
    }

    /// A comparison, with the forms that `IS NULL` and `IS TRUE` bind to written as those.
    fn compare(&self, op: CompareOp, left: ExprRef, right: ExprRef) -> String {
        let operand = self.expr(left);
        if matches!(op, CompareOp::DistinctFrom | CompareOp::NotDistinctFrom) {
            let not = if op == CompareOp::DistinctFrom { "NOT " } else { "" };
            if let Expr::Constant(value) = self.plan.expr(right) {
                match self.plan.value(*value) {
                    Value::Null => return format!("({operand} IS {not}NULL)"),
                    Value::Boolean(true) => return format!("({operand} IS {not}TRUE)"),
                    Value::Boolean(false) => return format!("({operand} IS {not}FALSE)"),
                    _ => {}
                }
            }
            let other = self.expr(right);
            return match op {
                CompareOp::DistinctFrom => format!("({operand} IS DISTINCT FROM {other})"),
                _ => format!("(NOT ({operand} IS DISTINCT FROM {other}))"),
            };
        }
        format!("({operand} {} {})", op.symbol(), self.expr(right))
    }

    /// A function call, an operator or a cast the binder wrote as a function.
    fn function(&self, at: ExprRef, name: &str, args: rudb_plan::Slice) -> String {
        let list = self.plan.expr_list(args);
        let symbol = OPERATOR_KERNELS
            .iter()
            .find(|(kernel, _)| *kernel == name)
            .map_or(name, |(_, symbol)| symbol);
        let is_operator =
            !symbol.is_empty() && symbol.bytes().all(|b| b"+-*/<>=~!@#%^&|`?".contains(&b));
        match (symbol, list) {
            ("not", [operand]) => return format!("(NOT {})", self.expr(*operand)),
            (_, [left, right]) if is_operator => {
                return format!("({} {symbol} {})", self.expr(*left), self.expr(*right));
            }
            (_, [operand]) if is_operator => return format!("({symbol} {})", self.expr(*operand)),
            _ => {}
        }
        if let (true, Some(&first)) = (CAST_KERNELS.contains(&name), list.first()) {
            return self.cast(first, self.plan.expr_type(at));
        }
        let parts = self.exprs(list).join(", ");
        match SQL_FUNCTIONS.iter().find(|(lower, _)| *lower == name) {
            Some((_, upper)) => format!("{upper}({parts})"),
            None => format!("{}({parts})", quote_identifier(function_name(name))),
        }
    }

    /// An aggregate call, which is the call of a window function too.
    fn aggregate(
        &self,
        name: &str,
        args: rudb_plan::Slice,
        distinct: bool,
        filter: Option<ExprRef>,
    ) -> String {
        let (name, star) = match name {
            "count_star" => ("count", true),
            other => (function_name(other), false),
        };
        let arguments = match star {
            true => "*".to_owned(),
            false => self.exprs(self.plan.expr_list(args)).join(", "),
        };
        let distinct = if distinct { "DISTINCT " } else { "" };
        let filter = match filter {
            Some(filter) => format!(" FILTER (WHERE {})", self.expr(filter)),
            None => String::new(),
        };
        format!("{}({distinct}{arguments}){filter}", quote_identifier(name))
    }

    /// A cast. A constant cast to a type is that type's constant, the way the planner folds it
    /// before `EXPLAIN` sees it.
    fn cast(&self, input: ExprRef, to: &LogicalType) -> String {
        if let Expr::Constant(value) = self.plan.expr(input) {
            let value = self.plan.value(*value);
            if value.is_null() {
                return self.constant(value, to);
            }
            if let Some(text) = self.text(value, self.plan.expr_type(input)) {
                return labelled(&text, to);
            }
        }
        let typed = pg_type(to);
        format!("({})::{}", self.expr(input), format_type_with_typmod(typed.oid, typed.typmod))
    }

    /// A constant, the way `get_const_expr` writes it.
    fn constant(&self, value: &Value, ty: &LogicalType) -> String {
        if value.is_null() {
            let typed = pg_type(ty);
            return format!("NULL::{}", format_type_with_typmod(typed.oid, typed.typmod));
        }
        match self.text(value, ty) {
            Some(text) => labelled(&text, ty),
            None => "NULL".to_owned(),
        }
    }

    /// The text of a value as its type's output function writes it.
    fn text(&self, value: &Value, ty: &LogicalType) -> Option<String> {
        let vector = Vector::from_values(ty.clone(), std::slice::from_ref(value)).ok()?;
        let texts = text_values(&vector, pg_type(ty).oid, self.settings).ok()?;
        // flatten: one value whose text is `None` when it is `NULL`, not a column.
        texts.into_iter().next().flatten()
    }
}

/// The text of a constant with the label `get_const_expr` puts on it.
///
/// An `integer` that is not negative and a `numeric` that reads as a number with a point are what
/// a bare literal would be, so they have no label. A `boolean` is `true` or `false`. Everything
/// else is a quoted string with its type.
fn labelled(text: &str, ty: &LogicalType) -> String {
    let typed = pg_type(ty);
    match typed.oid {
        oid::BOOL => return if text.starts_with('t') { "true" } else { "false" }.to_owned(),
        oid::INT4 if text.bytes().all(|b| b.is_ascii_digit()) => return text.to_owned(),
        oid::NUMERIC
            if text.starts_with(|c: char| c.is_ascii_digit())
                && text.contains(['.', 'e', 'E'])
                && text.bytes().all(|b| b.is_ascii_digit() || b".eE+-".contains(&b)) =>
        {
            return text.to_owned();
        }
        _ => {}
    }
    // A numeric constant's precision is the literal's own, so its label never carries one.
    let typmod = if typed.oid == oid::NUMERIC { -1 } else { typed.typmod };
    format!("'{}'::{}", text.replace('\'', "''"), format_type_with_typmod(typed.oid, typmod))
}
