//! Which of a node's output rows still know which base table row they came from.
//!
//! A link maps a row id of a child table to a row id of its parent, so it can only be read where
//! the operator in front of it knows, for every row it holds, which row of the base table that row
//! is. spec/graph/05-execution.md section 5.1 calls this the single most important paragraph in
//! that document, and the reason is blunt: every wrong answer the graph layer can produce is a row
//! id used after the operator that invalidated it. This module is where an operator says whether it
//! invalidated one.
//!
//! # Why it is here and not in the optimizer
//!
//! The same reason [`unique`](crate::unique) is here. A `match` over [`Node`] in this crate does not
//! compile when a variant appears, so whoever adds an operator has to answer this question at the
//! moment they add it. Written in `rudb-opt` it would be a lookup with a default arm, and the
//! default arm would eventually be the answer that was convenient rather than the answer that was
//! true, on an analysis where the convenient answer is a wrong result rather than a slow one.
//!
//! # What a row id is here
//!
//! The table index of a scan, not the name of a table. A query that joins `orders` to itself has
//! two scans and two indexes, and the rows under one of them are not the rows under the other. The
//! index is also what a column binding names, so a rewrite that has found a join condition can ask
//! about exactly the side it is holding.
//!
//! # What it refuses to say
//!
//! Absence is always safe here, so an operator that could preserve a row id with work it does not
//! do yet is written down as not preserving one. There are two of those and they are marked: a sort
//! preserves identity only if it carries the row id through the sort as a column, and a join
//! preserves it for the side it streams rather than for the side it gathers. Both are statements
//! about what the executor does today, and the day one of them changes, this changes with it.

use std::collections::BTreeSet;

use crate::NodeRef;
use crate::node::{BuildSide, JoinKind, Node};
use crate::plan::Plan;

/// The scans whose row ids a node's output still carries.
///
/// Empty is the common answer and the safe one. It does not mean the rows came from nowhere, it
/// means no rewrite may treat a row of this output as a row of a base table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Carried {
    tables: BTreeSet<u32>,
}

impl Carried {
    /// No row id of any table.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The row id of one scan, named by the table index its columns bind against.
    #[must_use]
    pub fn of(table: u32) -> Self {
        Self { tables: BTreeSet::from([table]) }
    }

    /// Whether a row of this output is still a row of that scan.
    #[must_use]
    pub fn has(&self, table: u32) -> bool {
        self.tables.contains(&table)
    }

    /// Whether any row id survives here.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// The scans, by table index, ascending.
    pub fn tables(&self) -> impl Iterator<Item = u32> + '_ {
        self.tables.iter().copied()
    }

    /// Both sets, for an operator that preserves the identity of rows from either side.
    #[must_use]
    fn and(mut self, other: &Self) -> Self {
        self.tables.extend(other.tables.iter().copied());
        self
    }
}

/// What every node of a plan carries, indexed by [`NodeRef`].
///
/// One walk rather than a call per node, for the reason
/// [`keys_of`](crate::unique::keys_of) does the same: every rule reads its inputs.
#[must_use]
pub fn rids_of(plan: &Plan) -> Vec<Carried> {
    let mut known = vec![Carried::none(); plan.node_count()];
    let mut order = Vec::with_capacity(plan.node_count());
    postorder(plan, plan.root(), &mut order);
    for node in order {
        known[node as usize] = compute(plan, node, &known);
    }
    known
}

/// The nodes under a root, children before parents.
fn postorder(plan: &Plan, at: NodeRef, out: &mut Vec<NodeRef>) {
    for child in plan.node(at).children().into_iter().flatten() {
        postorder(plan, child, out);
    }
    out.push(at);
}

/// What one node's output carries, given what its inputs carry.
fn compute(plan: &Plan, at: NodeRef, known: &[Carried]) -> Carried {
    let below = |node: NodeRef| known[node as usize].clone();
    match *plan.node(at) {
        // Where every row id in a plan comes from. A scan emits the rows of the table in stored
        // order, so the nth row it emits is row n, which is the whole of section 2.1's definition.
        Node::Get { index, .. } => Carried::of(index),

        // A row read back by its table-wide ordinal is that row, which is what the node is for.
        Node::TableFetch { index, .. } => Carried::of(index),

        // Not the same claim. This one reads an ordinal inside a file, and a file is not a table
        // in append order: two scans of one Parquet file in one query can disagree about which row
        // is which, and there is no catalog table for a link to be declared against anyway.
        Node::Fetch { .. } => Carried::none(),

        // A selection is a list of positions, so what comes out of a filter is a subsequence of
        // what went in and every row of it is still the row it was.
        Node::Filter { input, .. } => below(input),

        // A projection changes the columns and not the rows. The row id is not one of the columns,
        // so there is nothing for a projection to drop.
        Node::Project { input, .. } => below(input),

        // A window appends columns to rows it does not reorder or remove.
        Node::Window { input, .. } => below(input),

        // A prefix of the rows, in the order they arrived.
        Node::Limit { input, .. } | Node::LimitPercent { input, .. } => below(input),

        // A materialisation produces the rows of its body.
        Node::MaterializedCte { body, .. } => below(body),

        // A sort permutes the rows, and each row that comes out is still exactly one row that went
        // in, so this is a conservative no rather than a true one. Saying yes would require the row
        // id to travel through the sort as a column, which nothing materialises yet, and a row id
        // that is asserted rather than carried is the exact bug section 5.1 is about. A top n is a
        // sort with a limit fused into it and answers the same way.
        Node::Sort { .. } | Node::TopN { .. } => Carried::none(),

        // Which row of a duplicate group survived is not defined, so no row of the output is a
        // particular row of the input.
        Node::Distinct { .. } => Carried::none(),

        // An aggregate's rows are groups. A group is not a row of anything.
        Node::Aggregate { .. } => Carried::none(),

        // Rows that were written down, rows a function made up, and rows read back out of a
        // materialisation, none of which are rows of a base table.
        Node::Dummy
        | Node::Values { .. }
        | Node::TableFunction { .. }
        | Node::LateralFunction { .. }
        | Node::CteScan { .. } => Carried::none(),

        // Set operations renumber everything. Even a `UNION ALL`, which emits the rows of both
        // sides untouched, emits them with two different tables' rows interleaved under one output
        // schema, and a rewrite that took the left side's row ids would be reading them for rows
        // that came from the right.
        Node::SetOp { .. } => Carried::none(),

        // A cross product pairs every row with every row, so one side's rows repeat and the other
        // side's do too. The streaming side's identity does survive, and this says nothing anyway,
        // because a cross product has no side to stream written down the way a join does.
        Node::CrossProduct { .. } => Carried::none(),

        // Has to be unnested before it can run, so nothing downstream of one is real yet.
        Node::DependentJoin { .. } => Carried::none(),

        Node::Join { left, right, kind, build, .. } => {
            joined(&below(left), &below(right), kind, build)
        }

        // The child, and only the child. This is the one join in the engine whose streaming side
        // is not a choice: section 5.2 scans the child and reads the link beside its columns, so
        // the child's rows arrive in order and a selection is all that happens to them, which is
        // the same argument the probe side of a hash join gets. The parent's rows are gathered
        // through the link, and a gathered row carries no row id of its own however the gather
        // was worked out.
        //
        // An inner join drops the child rows whose link is the no parent sentinel and a semi or
        // anti join tests that sentinel, both of which are selections. A left join keeps them and
        // gathers null, which pads the parent side rather than the child side, so the child's
        // row ids survive all four kinds and the padding rule of `joined` never bites.
        Node::LinkJoin { child, .. } => below(child),
    }
}

/// What a join preserves, which is the side it streams and only where that side is not padded.
///
/// Section 5.1 says a join's output does not know for both of its inputs, and the reason is
/// physical. The build side is gathered: a row of the output holds a copy of a build row that was
/// selected by a hash table, and nothing carries where that copy came from. The probe side is
/// streamed, so its rows arrive in order and a selection is all that happens to them.
///
/// The padding is the other half. An outer join emits rows on the padded side that came from no
/// row at all, and a row id for one of those would be a row id invented by the join.
fn joined(left: &Carried, right: &Carried, kind: JoinKind, build: BuildSide) -> Carried {
    // The nth row with the nth row, so neither side is gathered and neither is padded.
    if kind == JoinKind::Positional {
        return left.clone().and(right);
    }
    let probe = match build {
        BuildSide::Left => Side::Right,
        BuildSide::Right => Side::Left,
    };
    let padded = match kind {
        // Every left row survives, so the right side is what gets padded.
        JoinKind::Left | JoinKind::Single | JoinKind::Mark => Some(Side::Right),
        JoinKind::Right => Some(Side::Left),
        JoinKind::Full => return Carried::none(),
        JoinKind::Inner | JoinKind::Semi | JoinKind::Anti => None,
        JoinKind::Positional => unreachable!("answered above"),
    };
    if padded == Some(probe) {
        return Carried::none();
    }
    // A semi or anti join emits left rows and nothing else, so a probing right side has nothing of
    // its own in the output to carry an identity for.
    if matches!(kind, JoinKind::Semi | JoinKind::Anti) && probe == Side::Right {
        return Carried::none();
    }
    match probe {
        Side::Left => left.clone(),
        Side::Right => right.clone(),
    }
}

/// Which input of a join, for the two questions this asks about one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType, Value};

    use super::{Carried, rids_of};
    use crate::expr::{ColumnBinding, Expr};
    use crate::node::{
        Bound, BuildSide, JoinKind, Node, SetOpKind, Share, WindowBound, WindowExclude,
        WindowFrame, WindowUnit,
    };
    use crate::plan::Plan;

    /// A scan of two integer columns bound against `index`.
    fn scan(plan: &mut Plan, index: u32) -> u32 {
        let columns = plan.add_fields(&[
            Field::new("a", LogicalType::Integer),
            Field::new("b", LogicalType::Integer),
        ]);
        let name = plan.intern("t");
        plan.add_node(Node::Get {
            catalog: name,
            schema: name,
            table: name,
            alias: name,
            index,
            columns,
        })
    }

    /// A column reference of integer type.
    fn column(plan: &mut Plan, table: u32, position: u32) -> u32 {
        plan.add_expr(Expr::Column(ColumnBinding::new(table, position)), LogicalType::Integer)
    }

    /// What the root of a plan carries.
    fn root(plan: &Plan) -> Carried {
        rids_of(plan)[plan.root() as usize].clone()
    }

    /// The tables the root carries, as a list, which is what most of these assert on.
    fn carried(plan: &Plan) -> Vec<u32> {
        root(plan).tables().collect()
    }

    #[test]
    fn a_scan_is_where_a_row_id_comes_from() {
        let mut plan = Plan::new();
        let node = scan(&mut plan, 7);
        plan.set_root(node);
        assert_eq!(carried(&plan), vec![7], "the index the scan's columns bind against");
        assert!(root(&plan).has(7));
        assert!(!root(&plan).has(0), "and no other scan's");
    }

    #[test]
    fn a_filter_and_a_projection_and_a_window_keep_the_rows_they_were_given() {
        // The three shapes that sit over a scan in almost every plan. If any of them dropped a row
        // id there would be nowhere for the link rewrite to fire.
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let predicate = column(&mut plan, 0, 0);
        let filter = plan.add_node(Node::Filter { input, predicate });
        let exprs = plan.add_expr_list(&[predicate]);
        let a = plan.intern("a");
        let names = plan.add_name_list(&[a]);
        let project = plan.add_node(Node::Project { input: filter, index: 1, exprs, names });
        let empty = plan.add_expr_list(&[]);
        let order = plan.add_sort_keys(&[]);
        let window = plan.add_node(Node::Window {
            input: project,
            index: 2,
            partition: empty,
            order,
            frame: WindowFrame {
                unit: WindowUnit::Rows,
                start: WindowBound::UnboundedPreceding,
                end: WindowBound::CurrentRow,
                exclude: WindowExclude::NoOthers,
            },
            expressions: empty,
        });
        plan.set_root(window);
        assert_eq!(carried(&plan), vec![0]);
    }

    #[test]
    fn a_limit_keeps_them_and_a_limit_percent_does_too() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let limit =
            plan.add_node(Node::Limit { input, count: Bound::Rows(10), offset: Bound::All });
        let percent = plan.add_node(Node::LimitPercent {
            input: limit,
            percent: Share::Percent(30.0),
            offset: Bound::Rows(0),
        });
        plan.set_root(percent);
        assert_eq!(carried(&plan), vec![0], "a prefix of the rows is still those rows");
    }

    #[test]
    fn a_sort_and_a_top_n_drop_it_until_something_carries_it_through() {
        // Conservative and marked as such in the module: a sort could preserve this by carrying the
        // row id as a column, and nothing materialises one yet.
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let keys = plan.add_sort_keys(&[]);
        let sort = plan.add_node(Node::Sort { input, keys });
        plan.set_root(sort);
        assert!(root(&plan).is_empty());

        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let keys = plan.add_sort_keys(&[]);
        let top = plan.add_node(Node::TopN { input, keys, count: 10, offset: 0 });
        plan.set_root(top);
        assert!(root(&plan).is_empty());
    }

    #[test]
    fn an_aggregate_and_a_distinct_produce_rows_that_are_nobody_in_particular() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let empty = plan.add_expr_list(&[]);
        let group = column(&mut plan, 0, 0);
        let groups = plan.add_expr_list(&[group]);
        let node = plan.add_node(Node::Aggregate { input, index: 1, groups, aggregates: empty });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "a group is not a row of anything");

        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let on = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::Distinct { input, on });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "which row of a duplicate group survived is not defined");
    }

    #[test]
    fn rows_that_were_never_a_table_carry_nothing() {
        let mut plan = Plan::new();
        let node = plan.add_node(Node::Dummy);
        plan.set_root(node);
        assert!(root(&plan).is_empty());

        let mut plan = Plan::new();
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let one = plan.add_constant(Value::Integer(1));
        let row = plan.add_expr_list(&[one]);
        let rows = plan.add_rows(&[row]);
        let node = plan.add_node(Node::Values { index: 0, columns, rows });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "a literal row is not a row of a table");

        let mut plan = Plan::new();
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let name = plan.intern("range");
        let empty = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::TableFunction {
            index: 0,
            function: name,
            args: empty,
            options: empty,
            settings: empty,
            columns,
        });
        plan.set_root(node);
        assert!(root(&plan).is_empty());

        let mut plan = Plan::new();
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let name = plan.intern("x");
        let node = plan.add_node(Node::CteScan { index: 0, cte: 0, name, columns });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "what it reads was computed rather than stored");
    }

    #[test]
    fn a_lateral_function_produces_its_own_rows_and_not_its_inputs() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let name = plan.intern("unnest");
        let empty = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::LateralFunction {
            input,
            index: 1,
            function: name,
            args: empty,
            options: empty,
            settings: empty,
            columns,
        });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "one input row becomes any number of output rows");
    }

    #[test]
    fn a_table_fetch_is_a_row_id_and_a_file_fetch_is_not() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let row = column(&mut plan, 0, 0);
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let name = plan.intern("t");
        let node = plan.add_node(Node::TableFetch {
            input,
            index: 3,
            catalog: name,
            schema: name,
            table: name,
            columns,
            row,
        });
        plan.set_root(node);
        assert_eq!(carried(&plan), vec![3], "a row read back by its ordinal is that row");

        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let row = column(&mut plan, 0, 0);
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let args = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::Fetch { input, index: 3, args, columns, row });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "an ordinal in a file is not a row id of a table");
    }

    #[test]
    fn a_materialisation_carries_what_its_body_carries_and_a_set_operation_carries_nothing() {
        let mut plan = Plan::new();
        let definition = scan(&mut plan, 0);
        let body = scan(&mut plan, 1);
        let name = plan.intern("x");
        let columns = plan.add_fields(&[Field::new("a", LogicalType::Integer)]);
        let node = plan.add_node(Node::MaterializedCte { definition, body, name, cte: 0, columns });
        plan.set_root(node);
        assert_eq!(carried(&plan), vec![1], "the rows are the body's, not the definition's");

        let mut plan = Plan::new();
        let left = scan(&mut plan, 0);
        let right = scan(&mut plan, 1);
        let node =
            plan.add_node(Node::SetOp { left, right, kind: SetOpKind::Union, all: true, index: 2 });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "two tables' rows under one schema are neither table's");
    }

    #[test]
    fn a_cross_product_and_a_dependent_join_carry_nothing() {
        let mut plan = Plan::new();
        let left = scan(&mut plan, 0);
        let right = scan(&mut plan, 1);
        let node = plan.add_node(Node::CrossProduct { left, right });
        plan.set_root(node);
        assert!(root(&plan).is_empty());

        let mut plan = Plan::new();
        let left = scan(&mut plan, 0);
        let right = scan(&mut plan, 1);
        let conditions = plan.add_expr_list(&[]);
        let node =
            plan.add_node(Node::DependentJoin { left, right, kind: JoinKind::Inner, conditions });
        plan.set_root(node);
        assert!(root(&plan).is_empty(), "it has to be unnested before it can run at all");
    }

    /// A join of a scan of 0 to a scan of 1, of that kind, with that side built.
    fn join(kind: JoinKind, build: BuildSide) -> Plan {
        let mut plan = Plan::new();
        let left = scan(&mut plan, 0);
        let right = scan(&mut plan, 1);
        let conditions = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::Join { left, right, kind, conditions, build });
        plan.set_root(node);
        plan
    }

    #[test]
    fn a_join_carries_the_side_it_streams_and_not_the_side_it_gathers() {
        // The paragraph this module exists for. A build row reaches the output as a copy that a
        // hash table selected, and nothing records which row the copy was made from.
        assert_eq!(carried(&join(JoinKind::Inner, BuildSide::Right)), vec![0]);
        assert_eq!(carried(&join(JoinKind::Inner, BuildSide::Left)), vec![1]);
    }

    #[test]
    fn an_outer_join_carries_nothing_for_a_side_it_pads() {
        // A padded row came from no row, so a row id for one would be invented by the join.
        assert!(root(&join(JoinKind::Left, BuildSide::Left)).is_empty());
        assert_eq!(
            carried(&join(JoinKind::Left, BuildSide::Right)),
            vec![0],
            "the left side of a left join is never padded, and here it is also the probe"
        );
        assert!(root(&join(JoinKind::Right, BuildSide::Right)).is_empty());
        assert_eq!(carried(&join(JoinKind::Right, BuildSide::Left)), vec![1]);
        assert!(root(&join(JoinKind::Full, BuildSide::Right)).is_empty());
        assert!(root(&join(JoinKind::Full, BuildSide::Left)).is_empty());
    }

    #[test]
    fn a_semi_join_carries_its_left_side_only_where_the_left_side_probes() {
        // A semi join emits left rows. With the left side built, those rows reach the output as
        // gathered copies, which is the same problem an inner join's build side has.
        assert_eq!(carried(&join(JoinKind::Semi, BuildSide::Right)), vec![0]);
        assert!(root(&join(JoinKind::Semi, BuildSide::Left)).is_empty());
        assert_eq!(carried(&join(JoinKind::Anti, BuildSide::Right)), vec![0]);
        assert!(root(&join(JoinKind::Anti, BuildSide::Left)).is_empty());
    }

    #[test]
    fn a_single_and_a_mark_join_pad_the_right_side() {
        assert_eq!(carried(&join(JoinKind::Single, BuildSide::Right)), vec![0]);
        assert!(root(&join(JoinKind::Single, BuildSide::Left)).is_empty());
        assert_eq!(carried(&join(JoinKind::Mark, BuildSide::Right)), vec![0]);
        assert!(root(&join(JoinKind::Mark, BuildSide::Left)).is_empty());
    }

    #[test]
    fn a_positional_join_carries_both_sides() {
        // The nth row with the nth row. Neither side is gathered and neither is padded, so this is
        // the one join where both row ids survive.
        assert_eq!(carried(&join(JoinKind::Positional, BuildSide::Right)), vec![0, 1]);
        assert_eq!(carried(&join(JoinKind::Positional, BuildSide::Left)), vec![0, 1]);
    }

    #[test]
    fn a_link_join_carries_its_child_and_never_its_parent() {
        // The node this whole module is for, and the one join whose streaming side is not a
        // choice. Section 5.2 scans the child and reads the link beside its columns, so the
        // child's rows arrive in order and a selection is all that happens to them. The parent's
        // rows are gathered through the link, and a gathered row carries no row id of its own.
        //
        // All four kinds the node admits, because the argument is the same for all four and a
        // reader would otherwise have to take the module's word for it. An inner join drops the
        // child rows whose link is the no parent sentinel and a semi or anti join tests it, which
        // are selections; a left join keeps them and gathers null, which pads the parent side.
        for kind in [JoinKind::Inner, JoinKind::Left, JoinKind::Semi, JoinKind::Anti] {
            let mut plan = Plan::new();
            let child = scan(&mut plan, 4);
            let parent = scan(&mut plan, 5);
            let conditions = plan.add_expr_list(&[]);
            let rid = column(&mut plan, 4, 0);
            let node = plan.add_node(Node::LinkJoin { child, parent, kind, conditions, rid });
            plan.set_root(node);
            assert_eq!(carried(&plan), vec![4], "{kind:?} keeps the child and only the child");
            assert!(!root(&plan).has(5), "{kind:?} gathered the parent");
        }
    }

    #[test]
    fn a_row_id_survives_a_stack_of_operators_that_all_preserve_it() {
        // What a TPC-H plan looks like under the join: scan, filter, project. The rewrite of
        // document 06 fires here and nowhere the analysis above says no.
        let mut plan = Plan::new();
        let orders = scan(&mut plan, 0);
        let predicate = column(&mut plan, 0, 0);
        let filter = plan.add_node(Node::Filter { input: orders, predicate });
        let exprs = plan.add_expr_list(&[predicate]);
        let a = plan.intern("a");
        let names = plan.add_name_list(&[a]);
        let project = plan.add_node(Node::Project { input: filter, index: 1, exprs, names });
        let lineitem = scan(&mut plan, 2);
        let conditions = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::Join {
            left: lineitem,
            right: project,
            kind: JoinKind::Inner,
            conditions,
            build: BuildSide::Right,
        });
        plan.set_root(node);
        assert_eq!(carried(&plan), vec![2], "the child side streams, so its row ids are there");
        let all = rids_of(&plan);
        assert_eq!(all[project as usize].tables().collect::<Vec<_>>(), vec![0]);
        assert_eq!(all[filter as usize].tables().collect::<Vec<_>>(), vec![0]);
    }
}
