//! The physical plan the compiled engine runs, per `spec/compiler/04-the-physical-plan.md`.
//!
//! [`lower`] reads an optimized [`Plan`] and either returns the same query as a [`Rel`] tree or
//! says why it will not, as a [`Refusal`] the router logs before it hands the query to the first
//! engine. The tree is smaller than the plan on purpose. A column is a position in the input's
//! output rather than a binding, a constant is a [`Value`] rather than a reference into a pool, and
//! everything the generator cannot translate yet is refused here, so that the crates above never
//! meet a node or a type they would have to refuse halfway through writing code for it.
//!
//! What C1 accepts is what ClickBench needs: scans, filters, projections, grouped and ungrouped
//! aggregates, sorts, top N and limits, literal rows, and the late materialisation fetch. Every
//! other node is a refusal that names it, which is how the refusal log says what C2 has to add.

use std::fmt;

use rudb_common::{LogicalType, PhysicalType, Value};
use rudb_plan::{
    Bound, ColumnBinding, CompareOp, ConjunctionOp, Expr as PlanExpr, ExprRef, Node, NodeRef, Plan,
};

/// Why the compiled engine will not run a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// What was refused, for grouping the log: a node kind, a function name or a type.
    pub what: String,
    /// The sentence the log prints.
    pub why: String,
}

impl Refusal {
    /// A refusal of `what` because of `why`.
    pub fn new(what: impl Into<String>, why: impl Into<String>) -> Refusal {
        Refusal { what: what.into(), why: why.into() }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.what, self.why)
    }
}

/// A result of lowering.
pub type Result<T> = std::result::Result<T, Refusal>;

/// One output column of a [`Rel`].
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// The name a result set gives it.
    pub name: String,
    /// Its type.
    pub ty: LogicalType,
}

/// An expression over the columns of one input.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    /// What it computes.
    pub kind: Kind,
    /// The type of the result.
    pub ty: LogicalType,
}

/// The shapes of [`Expr`].
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Column `i` of the input.
    Column(usize),
    /// A constant, possibly null.
    Constant(Value),
    /// A cast to the expression's type.
    Cast {
        /// What is cast.
        input: Box<Expr>,
        /// `TRY_CAST`, which gives null rather than failing.
        try_cast: bool,
    },
    /// A comparison.
    Compare {
        /// Which one.
        op: CompareOp,
        /// The left side.
        left: Box<Expr>,
        /// The right side.
        right: Box<Expr>,
    },
    /// `AND` of the children, with SQL's three valued logic.
    And(Vec<Expr>),
    /// `OR` of the children, with SQL's three valued logic.
    Or(Vec<Expr>),
    /// A scalar function by the name the binder gave it.
    Function {
        /// The name, such as `+`, `~~` or `regexp_replace`.
        name: String,
        /// The arguments.
        args: Vec<Expr>,
    },
    /// `CASE WHEN ... THEN ... ELSE ... END`.
    Case {
        /// The arms in order, each a condition and a result.
        arms: Vec<(Expr, Expr)>,
        /// The `ELSE`, null when there is none.
        otherwise: Option<Box<Expr>>,
    },
}

impl Expr {
    /// Column `i` of type `ty`.
    #[must_use]
    pub fn column(i: usize, ty: LogicalType) -> Expr {
        Expr { kind: Kind::Column(i), ty }
    }

    /// Calls `f` on every direct child.
    pub fn children(&self, mut f: impl FnMut(&Expr)) {
        match &self.kind {
            Kind::Column(_) | Kind::Constant(_) => {}
            Kind::Cast { input, .. } => f(input),
            Kind::Compare { left, right, .. } => {
                f(left);
                f(right);
            }
            Kind::And(c) | Kind::Or(c) | Kind::Function { args: c, .. } => c.iter().for_each(f),
            Kind::Case { arms, otherwise } => {
                for (w, t) in arms {
                    f(w);
                    f(t);
                }
                if let Some(o) = otherwise {
                    f(o);
                }
            }
        }
    }

    /// The same expression with every column `i` replaced by `with[i]`.
    #[must_use]
    pub fn substitute(&self, with: &[Expr]) -> Expr {
        let sub = |e: &Expr| Box::new(e.substitute(with));
        let kind = match &self.kind {
            Kind::Column(i) => return with[*i].clone(),
            Kind::Constant(v) => Kind::Constant(v.clone()),
            Kind::Cast { input, try_cast } => Kind::Cast { input: sub(input), try_cast: *try_cast },
            Kind::Compare { op, left, right } => {
                Kind::Compare { op: *op, left: sub(left), right: sub(right) }
            }
            Kind::And(c) => Kind::And(c.iter().map(|e| e.substitute(with)).collect()),
            Kind::Or(c) => Kind::Or(c.iter().map(|e| e.substitute(with)).collect()),
            Kind::Function { name, args } => Kind::Function {
                name: name.clone(),
                args: args.iter().map(|e| e.substitute(with)).collect(),
            },
            Kind::Case { arms, otherwise } => Kind::Case {
                arms: arms.iter().map(|(w, t)| (w.substitute(with), t.substitute(with))).collect(),
                otherwise: otherwise.as_ref().map(|o| sub(o)),
            },
        };
        Expr { kind, ty: self.ty.clone() }
    }

    /// The columns the expression reads, each once, in the order first read.
    #[must_use]
    pub fn columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<usize>) {
        if let Kind::Column(i) = self.kind {
            if !out.contains(&i) {
                out.push(i);
            }
        }
        self.children(|c| c.collect(out));
    }
}

/// One aggregate call.
#[derive(Clone, Debug, PartialEq)]
pub struct Aggregate {
    /// The function, such as `count_star`, `sum` or `min`.
    pub name: String,
    /// The arguments.
    pub args: Vec<Expr>,
    /// `DISTINCT`.
    pub distinct: bool,
    /// `FILTER (WHERE ...)`.
    pub filter: Option<Expr>,
    /// The result type.
    pub ty: LogicalType,
}

/// One ordering key.
#[derive(Clone, Debug, PartialEq)]
pub struct Key {
    /// What is ordered by.
    pub expr: Expr,
    /// `DESC`.
    pub descending: bool,
    /// `NULLS FIRST`.
    pub nulls_first: bool,
}

/// A relational operator of the physical plan. Every operator's output is a list of columns, and
/// an expression above it names them by position.
#[derive(Clone, Debug, PartialEq)]
pub enum Rel {
    /// A base table, read by the node of the original plan so the driver can ask the storage layer
    /// for its chunks.
    Scan {
        /// The `Get` node in the plan.
        node: NodeRef,
        /// The table, for messages.
        table: String,
        /// The columns the scan produces.
        columns: Vec<Column>,
    },
    /// Literal rows.
    Values {
        /// The rows.
        rows: Vec<Vec<Value>>,
        /// The columns.
        columns: Vec<Column>,
    },
    /// Keeps the rows where the predicate is true.
    Filter {
        /// The input.
        input: Box<Rel>,
        /// The predicate.
        predicate: Expr,
    },
    /// New columns computed from the input's.
    Project {
        /// The input.
        input: Box<Rel>,
        /// The expressions.
        exprs: Vec<Expr>,
        /// The output columns.
        columns: Vec<Column>,
    },
    /// Grouping. The output is the groups and then the aggregates.
    Aggregate {
        /// The input.
        input: Box<Rel>,
        /// The group expressions.
        groups: Vec<Expr>,
        /// The aggregate calls.
        aggregates: Vec<Aggregate>,
        /// The output columns.
        columns: Vec<Column>,
    },
    /// All the rows in order.
    Sort {
        /// The input.
        input: Box<Rel>,
        /// The keys, first key first.
        keys: Vec<Key>,
    },
    /// The first `count` rows in order after skipping `offset`.
    TopN {
        /// The input.
        input: Box<Rel>,
        /// The keys, first key first.
        keys: Vec<Key>,
        /// How many rows.
        count: u64,
        /// How many to skip.
        offset: u64,
    },
    /// Some of the rows, in the order they arrive.
    Limit {
        /// The input.
        input: Box<Rel>,
        /// How many rows, all of them when `None`.
        count: Option<u64>,
        /// How many to skip.
        offset: u64,
    },
    /// Whole rows of a table read back by ordinal, the top half of late materialisation.
    Fetch {
        /// The input, which carries the ordinals.
        input: Box<Rel>,
        /// The `TableFetch` node in the plan.
        node: NodeRef,
        /// The ordinal, over the input.
        row: Expr,
        /// The output columns.
        columns: Vec<Column>,
    },
}

impl Rel {
    /// The output columns.
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        match self {
            Rel::Scan { columns, .. }
            | Rel::Values { columns, .. }
            | Rel::Project { columns, .. }
            | Rel::Aggregate { columns, .. }
            | Rel::Fetch { columns, .. } => columns,
            Rel::Filter { input, .. }
            | Rel::Sort { input, .. }
            | Rel::TopN { input, .. }
            | Rel::Limit { input, .. } => input.columns(),
        }
    }

    /// The operator's name, as `EXPLAIN` prints it.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Rel::Scan { .. } => "Scan",
            Rel::Values { .. } => "Values",
            Rel::Filter { .. } => "Filter",
            Rel::Project { .. } => "Project",
            Rel::Aggregate { .. } => "Aggregate",
            Rel::Sort { .. } => "Sort",
            Rel::TopN { .. } => "TopN",
            Rel::Limit { .. } => "Limit",
            Rel::Fetch { .. } => "Fetch",
        }
    }
}

/// Whether the compiled engine has a representation for values of `ty`.
///
/// A fixed width number, a boolean, a date or time kept as a number, or a string. An enum is a
/// number in storage and a label in every comparison, and an interval and a UUID have orderings of
/// their own, so those are refused until the generator knows them.
#[must_use]
pub fn supported(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Enum(_)
        | LogicalType::Uuid
        | LogicalType::TimeTz
        | LogicalType::Bit
        | LogicalType::Interval => false,
        other => matches!(
            other.physical(),
            PhysicalType::Bool
                | PhysicalType::Int8
                | PhysicalType::Int16
                | PhysicalType::Int32
                | PhysicalType::Int64
                | PhysicalType::Int128
                | PhysicalType::UInt8
                | PhysicalType::UInt16
                | PhysicalType::UInt32
                | PhysicalType::UInt64
                | PhysicalType::UInt128
                | PhysicalType::Float32
                | PhysicalType::Float64
                | PhysicalType::Varlen
        ),
    }
}

/// The aggregates the generator has an accumulator for.
pub const AGGREGATES: &[&str] = &["count_star", "count", "sum", "avg", "min", "max", "any_value"];

/// Lowers the optimized plan, or says why not.
///
/// # Errors
///
/// A [`Refusal`] naming the first thing the compiled engine does not run.
pub fn lower(plan: &Plan) -> Result<Rel> {
    let mut l = Lower { plan };
    let (rel, _) = l.node(plan.root())?;
    Ok(rel)
}

struct Lower<'a> {
    plan: &'a Plan,
}

/// The bindings of a lowered operator's output, parallel to its columns.
type Bindings = Vec<ColumnBinding>;

fn numbered(index: u32, n: usize) -> Bindings {
    (0..n).map(|i| ColumnBinding::new(index, i as u32)).collect()
}

impl Lower<'_> {
    fn fields(&self, slice: rudb_plan::Slice) -> Result<Vec<Column>> {
        let mut out = Vec::new();
        for f in self.plan.field_list(slice) {
            check_type(&f.ty)?;
            out.push(Column { name: f.name.clone(), ty: f.ty.clone() });
        }
        Ok(out)
    }

    fn node(&mut self, at: NodeRef) -> Result<(Rel, Bindings)> {
        let plan = self.plan;
        match plan.node(at).clone() {
            Node::Get { catalog, schema, table, index, columns, .. } => {
                let columns = self.fields(columns)?;
                let bindings = numbered(index, columns.len());
                let table = format!(
                    "{}.{}.{}",
                    plan.string(catalog),
                    plan.string(schema),
                    plan.string(table)
                );
                Ok((Rel::Scan { node: at, table, columns }, bindings))
            }
            Node::Dummy => {
                Ok((Rel::Values { rows: vec![Vec::new()], columns: Vec::new() }, Vec::new()))
            }
            Node::Values { index, columns, rows } => {
                let columns = self.fields(columns)?;
                let mut out = Vec::new();
                for row in plan.row_list(rows) {
                    let mut values = Vec::new();
                    for e in plan.expr_list(*row) {
                        match plan.expr(*e) {
                            PlanExpr::Constant(v) => values.push(plan.value(*v).clone()),
                            _ => {
                                return Err(Refusal::new(
                                    "Values",
                                    "a row holds an expression rather than a constant",
                                ));
                            }
                        }
                    }
                    out.push(values);
                }
                let bindings = numbered(index, columns.len());
                Ok((Rel::Values { rows: out, columns }, bindings))
            }
            Node::Filter { input, predicate } => {
                let (input, bindings) = self.node(input)?;
                let predicate = self.expr(predicate, &bindings)?;
                Ok((Rel::Filter { input: Box::new(input), predicate }, bindings))
            }
            Node::Project { input, index, exprs, names } => {
                let (input, bindings) = self.node(input)?;
                let mut out = Vec::new();
                let mut columns = Vec::new();
                for (e, name) in plan.expr_list(exprs).iter().zip(plan.name_list(names)) {
                    let e = self.expr(*e, &bindings)?;
                    columns.push(Column { name: plan.string(*name).to_owned(), ty: e.ty.clone() });
                    out.push(e);
                }
                let bindings = numbered(index, out.len());
                Ok((Rel::Project { input: Box::new(input), exprs: out, columns }, bindings))
            }
            Node::Aggregate { input, index, groups, aggregates } => {
                let (input, bindings) = self.node(input)?;
                let mut columns = Vec::new();
                let mut gs = Vec::new();
                for (i, g) in plan.expr_list(groups).iter().enumerate() {
                    let g = self.expr(*g, &bindings)?;
                    let name = match g.kind {
                        Kind::Column(c) => input.columns()[c].name.clone(),
                        _ => format!("group{i}"),
                    };
                    columns.push(Column { name, ty: g.ty.clone() });
                    gs.push(g);
                }
                let mut aggs = Vec::new();
                for a in plan.expr_list(aggregates) {
                    let agg = self.aggregate(*a, &bindings)?;
                    columns.push(Column { name: format!("{}()", agg.name), ty: agg.ty.clone() });
                    aggs.push(agg);
                }
                let bindings = numbered(index, columns.len());
                let rel = Rel::Aggregate {
                    input: Box::new(input),
                    groups: gs,
                    aggregates: aggs,
                    columns,
                };
                Ok((rel, bindings))
            }
            Node::Sort { input, keys } => {
                let (input, bindings) = self.node(input)?;
                let keys = self.keys(keys, &bindings)?;
                Ok((Rel::Sort { input: Box::new(input), keys }, bindings))
            }
            Node::TopN { input, keys, count, offset } => {
                let (input, bindings) = self.node(input)?;
                let keys = self.keys(keys, &bindings)?;
                Ok((Rel::TopN { input: Box::new(input), keys, count, offset }, bindings))
            }
            Node::Limit { input, count, offset } => {
                let count = match count {
                    Bound::All => None,
                    Bound::Rows(n) => Some(n),
                    Bound::Read(_) => {
                        return Err(Refusal::new("Limit", "the count is read from a subquery"));
                    }
                };
                let offset = match offset {
                    Bound::All => return Err(Refusal::new("Limit", "an offset of ALL")),
                    Bound::Rows(n) => n,
                    Bound::Read(_) => {
                        return Err(Refusal::new("Limit", "the offset is read from a subquery"));
                    }
                };
                let (input, bindings) = self.node(input)?;
                Ok((Rel::Limit { input: Box::new(input), count, offset }, bindings))
            }
            Node::TableFetch { input, index, columns, row, .. } => {
                let columns = self.fields(columns)?;
                let (input, bindings) = self.node(input)?;
                let row = self.expr(row, &bindings)?;
                let bindings = numbered(index, columns.len());
                Ok((Rel::Fetch { input: Box::new(input), node: at, row, columns }, bindings))
            }
            other => Err(Refusal::new(
                node_name(&other),
                "the compiled engine has no translator for this operator yet",
            )),
        }
    }

    fn keys(&self, keys: rudb_plan::Slice, bindings: &[ColumnBinding]) -> Result<Vec<Key>> {
        let mut out = Vec::new();
        for k in self.plan.sort_key_list(keys) {
            out.push(Key {
                expr: self.expr(k.expr, bindings)?,
                descending: k.descending,
                nulls_first: k.nulls_first,
            });
        }
        Ok(out)
    }

    fn aggregate(&self, at: ExprRef, bindings: &[ColumnBinding]) -> Result<Aggregate> {
        let plan = self.plan;
        let PlanExpr::Aggregate { name, args, distinct, filter } = plan.expr(at).clone() else {
            return Err(Refusal::new(
                "Aggregate",
                "an aggregate list holds something that is not an aggregate",
            ));
        };
        let name = plan.string(name).to_owned();
        let ty = plan.expr_type(at).clone();
        check_type(&ty)?;
        if !AGGREGATES.contains(&name.as_str()) {
            return Err(Refusal::new(
                format!("aggregate {name}"),
                "the generator has no accumulator for it",
            ));
        }
        let args = plan
            .expr_list(args)
            .iter()
            .map(|a| self.expr(*a, bindings))
            .collect::<Result<Vec<_>>>()?;
        if distinct && name != "count" {
            return Err(Refusal::new(
                format!("aggregate {name}"),
                "DISTINCT is only compiled for count",
            ));
        }
        if matches!(name.as_str(), "sum" | "avg")
            && args.first().is_some_and(|a| a.ty.physical() == PhysicalType::Varlen)
        {
            return Err(Refusal::new(format!("aggregate {name}"), "over a string"));
        }
        let filter = filter.map(|f| self.expr(f, bindings)).transpose()?;
        Ok(Aggregate { name, args, distinct, filter, ty })
    }

    fn expr(&self, at: ExprRef, bindings: &[ColumnBinding]) -> Result<Expr> {
        let plan = self.plan;
        let ty = plan.expr_type(at).clone();
        check_type(&ty)?;
        let list = |s: rudb_plan::Slice| {
            plan.expr_list(s).iter().map(|e| self.expr(*e, bindings)).collect::<Result<Vec<_>>>()
        };
        let kind = match plan.expr(at).clone() {
            PlanExpr::Column(b) => match bindings.iter().position(|x| *x == b) {
                Some(i) => Kind::Column(i),
                None => {
                    return Err(Refusal::new(
                        "binding",
                        format!("#{}.{} is not an output of the input", b.table, b.column),
                    ));
                }
            },
            PlanExpr::Constant(v) => Kind::Constant(plan.value(v).clone()),
            PlanExpr::Cast { input, try_cast } => {
                Kind::Cast { input: Box::new(self.expr(input, bindings)?), try_cast }
            }
            PlanExpr::Compare { op, left, right } => Kind::Compare {
                op,
                left: Box::new(self.expr(left, bindings)?),
                right: Box::new(self.expr(right, bindings)?),
            },
            PlanExpr::Conjunction { op: ConjunctionOp::And, children } => {
                Kind::And(list(children)?)
            }
            PlanExpr::Conjunction { op: ConjunctionOp::Or, children } => Kind::Or(list(children)?),
            PlanExpr::Function { name, args } => {
                Kind::Function { name: plan.string(name).to_owned(), args: list(args)? }
            }
            PlanExpr::Case { arms, otherwise } => {
                let mut out = Vec::new();
                for arm in plan.arm_list(arms) {
                    out.push((self.expr(arm.when, bindings)?, self.expr(arm.then, bindings)?));
                }
                let otherwise =
                    otherwise.map(|o| self.expr(o, bindings)).transpose()?.map(Box::new);
                Kind::Case { arms: out, otherwise }
            }
            PlanExpr::Aggregate { .. } => {
                return Err(Refusal::new("expression", "an aggregate outside an aggregate list"));
            }
            PlanExpr::Window { .. } => return Err(Refusal::new("expression", "a window function")),
            PlanExpr::Lambda { .. } | PlanExpr::LambdaParam(_) => {
                return Err(Refusal::new("expression", "a lambda"));
            }
            #[allow(unreachable_patterns)]
            _ => {
                return Err(Refusal::new(
                    "expression",
                    "a kind of expression the generator does not know",
                ));
            }
        };
        Ok(Expr { kind, ty })
    }
}

fn check_type(ty: &LogicalType) -> Result<()> {
    if supported(ty) || *ty == LogicalType::Null {
        Ok(())
    } else {
        Err(Refusal::new(
            format!("type {ty}"),
            "the compiled engine has no representation for it yet",
        ))
    }
}

fn node_name(node: &Node) -> String {
    let text = format!("{node:?}");
    text.split([' ', '{', '(']).next().unwrap_or("node").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lowered(text: &str) -> Result<Rel> {
        let plan = Plan::parse(text).expect("the test plan parses");
        lower(&plan)
    }

    #[test]
    fn a_grouped_count_with_a_filter_and_a_top_n_lowers_to_positions() {
        let rel = lowered(concat!(
            "TopN 10 offset 0 [#2.1::BIGINT DESC NULLS LAST]\n",
            "  Project #2 [#1.0::VARCHAR AS SearchPhrase, #1.1::BIGINT AS c]\n",
            "    Aggregate #1 groups=[#0.0::VARCHAR] aggregates=[count_star()::BIGINT]\n",
            "      Filter (#0.0::VARCHAR <> ''::VARCHAR)::BOOLEAN\n",
            "        Get memory.main.hits AS hits #0 [SearchPhrase::VARCHAR]\n",
        ))
        .unwrap();
        let Rel::TopN { input, keys, count: 10, offset: 0 } = &rel else { panic!("{rel:?}") };
        assert_eq!(keys[0].expr.kind, Kind::Column(1));
        assert!(keys[0].descending);
        let names: Vec<_> = rel.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["SearchPhrase", "c"]);
        let Rel::Project { input, exprs, .. } = &**input else { panic!() };
        assert_eq!(exprs[1].kind, Kind::Column(1));
        let Rel::Aggregate { input, groups, aggregates, .. } = &**input else { panic!() };
        assert_eq!(groups[0].kind, Kind::Column(0));
        assert_eq!(aggregates[0].name, "count_star");
        assert!(matches!(&**input, Rel::Filter { .. }));
    }

    #[test]
    fn an_operator_without_a_translator_is_refused_by_name() {
        let refused =
            lowered(concat!("Distinct on=[]\n", "  Get memory.main.a AS a #0 [x::INTEGER]\n",));
        match refused {
            Err(r) => assert_eq!(r.what, "Distinct"),
            Ok(rel) => panic!("{rel:?} was accepted"),
        }
    }

    #[test]
    fn substitution_replaces_columns_with_expressions() {
        let x = Expr::column(0, LogicalType::Integer);
        let plus = Expr {
            kind: Kind::Function { name: "+".into(), args: vec![x.clone(), x] },
            ty: LogicalType::Integer,
        };
        let with = [Expr::column(3, LogicalType::Integer)];
        let out = plus.substitute(&with);
        assert_eq!(out.columns(), [3]);
    }
}
