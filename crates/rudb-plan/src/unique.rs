//! Which columns of a node's output are enough to tell its rows apart, and which are the same for
//! every row it produces.
//!
//! Four of the rewrites P1 asks for delete a node rather than adding one, and every one of them
//! needs the same sentence to be true before it is allowed to fire. A group by whose key already
//! tells the rows apart is a projection. A `DISTINCT` over rows that are already distinct is a
//! copy. A join to a table that contributes no column and matches exactly one row is nothing at
//! all. None of those are decisions about cost, so none of them may be made from an estimate: a
//! rewrite that is licensed by a guess does not produce a slow query, it produces a wrong answer.
//! That is what `spec/stats/05-every-query.md` section 5.1.1 means by an enabling read, and this
//! module is where the evidence for one comes from.
//!
//! # Why it is here and not in the optimizer
//!
//! The rule that matters is that adding a node forces the question. A `match` over [`Node`] in this
//! crate does not compile when a variant appears, so whoever adds an operator has to say what it
//! does to uniqueness at the moment they add it. The same analysis written in `rudb-opt` would be a
//! lookup with a default arm, and the default arm would be whichever answer was convenient, which
//! for this analysis is the answer that silently licenses a rewrite over an operator nobody thought
//! about.
//!
//! # What a key is here
//!
//! A [`Keys`] is what is known about one node's output. It holds column sets, each of which is
//! enough on its own to tell any two rows apart, and it holds the columns that are the same in
//! every row. The empty set is a legal key and means the node produces at most one row, which is
//! what an ungrouped aggregate produces and is the strongest thing this can say.
//!
//! A key is the only functional dependency any of the four rewrites reads, because a key determines
//! every other column by definition, and a general dependency lattice would be a larger thing to
//! maintain for a caller that does not exist yet. The one dependency that pays for itself on its
//! own is the constant: `WHERE a = 1 GROUP BY a, b` groups by `b`, and knowing that `a` is fixed is
//! what says so. Constants are here for that reason and the lattice is not here for the opposite
//! one.
//!
//! # What it refuses to say
//!
//! Every answer here is one a rewrite may act on, so every case that is not obviously sound says
//! nothing instead of guessing. A scan has no keys, because this engine does not store a primary
//! key yet: `PRIMARY KEY` parses and the catalog does not keep it, so there is nothing to read and
//! a scan that claimed a key would be claiming one from the spelling of the DDL. A projection drops
//! a whole row key rather than trying to prove the projection is a permutation. An outer join keeps
//! nothing from the side that gets padded with nulls. Those are the places to come back to, and the
//! order to come back to them in is written on P1.

use std::collections::BTreeSet;

use crate::expr::{ColumnBinding, CompareOp, ConjunctionOp, Expr};
use crate::node::{Bound, JoinKind, Node};
use crate::plan::Plan;
use crate::{ExprRef, NodeRef, Slice};

/// What is known about telling one node's rows apart.
///
/// Empty means nothing is known, which is the answer for most leaves and is not the same as knowing
/// there are duplicates. Every field is a claim that a rewrite may act on, so absence is always the
/// safe direction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keys {
    /// Column sets, each of which tells any two rows of this node apart on its own.
    ///
    /// Sorted and deduplicated, and no set in here contains another, so a caller that wants the
    /// cheapest key can take the shortest and a caller that wants to know whether its own columns
    /// cover one can test each in turn.
    sets: Vec<Vec<ColumnBinding>>,
    /// Whether the whole output row, whatever its columns are, tells the rows apart.
    ///
    /// A `DISTINCT` and a `UNION` say this and neither of them can say which columns those are
    /// without the caller working out the node's output, which is a walk this analysis does not
    /// need for anything else.
    row: bool,
    /// Columns that hold the same value in every row this node produces.
    ///
    /// A key that contains one of these is a key without it, which is the whole reason to carry
    /// them.
    constants: BTreeSet<ColumnBinding>,
}

impl Keys {
    /// Nothing known.
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// At most one row, which is the empty key.
    #[must_use]
    pub fn single() -> Self {
        Self { sets: vec![Vec::new()], row: true, constants: BTreeSet::new() }
    }

    /// One key, from the columns given.
    #[must_use]
    pub fn of(columns: impl IntoIterator<Item = ColumnBinding>) -> Self {
        let mut keys = Self::default();
        keys.add(columns.into_iter().collect());
        keys
    }

    /// The whole row and nothing more particular than that.
    #[must_use]
    pub fn whole_row() -> Self {
        Self { sets: Vec::new(), row: true, constants: BTreeSet::new() }
    }

    /// The key sets, shortest first.
    #[must_use]
    pub fn sets(&self) -> &[Vec<ColumnBinding>] {
        &self.sets
    }

    /// Whether the whole output row is known to be a key.
    #[must_use]
    pub const fn row(&self) -> bool {
        self.row
    }

    /// The columns that are the same in every row.
    pub fn constants(&self) -> impl Iterator<Item = ColumnBinding> + '_ {
        self.constants.iter().copied()
    }

    /// Whether anything at all is known.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sets.is_empty() && !self.row && self.constants.is_empty()
    }

    /// Whether this node produces at most one row.
    ///
    /// The empty set being a key is exactly that statement, since two rows would have to differ
    /// somewhere in it and there is nowhere.
    #[must_use]
    pub fn at_most_one_row(&self) -> bool {
        self.sets.first().is_some_and(Vec::is_empty)
    }

    /// Whether the given columns contain a key, which is what licenses dropping a duplicate
    /// eliminating node above them.
    ///
    /// The constants are taken off both sides first, because a caller grouping by a column that is
    /// fixed is grouping by one column fewer and a key that mentions a fixed column needs one
    /// column fewer to be covered.
    #[must_use]
    pub fn covers(&self, columns: &[ColumnBinding]) -> bool {
        let held: BTreeSet<ColumnBinding> =
            columns.iter().copied().filter(|column| !self.constants.contains(column)).collect();
        self.sets.iter().any(|set| set.iter().all(|column| held.contains(column)))
    }

    /// Record a key, keeping the list minimal.
    ///
    /// A set that contains a key already here says nothing new, and a set that is contained by one
    /// already here replaces it. Without that the product rule below would grow the list by a
    /// factor on every join and the shortest key would stop being the first one.
    fn add(&mut self, mut columns: Vec<ColumnBinding>) {
        columns.sort_unstable();
        columns.dedup();
        columns.retain(|column| !self.constants.contains(column));
        if self.sets.iter().any(|set| set.iter().all(|column| columns.contains(column))) {
            return;
        }
        self.sets.retain(|set| !columns.iter().all(|column| set.contains(column)));
        self.sets.push(columns);
        self.sets.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    }

    /// Record a column that holds the same value in every row.
    fn fix(&mut self, column: ColumnBinding) {
        if !self.constants.insert(column) {
            return;
        }
        let sets = std::mem::take(&mut self.sets);
        for set in sets {
            self.add(set);
        }
    }

    /// The keys of a row made by putting a row of this beside a row of that.
    ///
    /// One key from each side, since two output rows that agree on both halves came from the same
    /// pair. This is the only rule that multiplies, which is why [`Keys::add`] keeps the list
    /// minimal rather than letting a five way join carry thirty two of them.
    fn product(&self, other: &Self) -> Self {
        let mut keys = Self::default();
        for column in self.constants.iter().chain(other.constants.iter()) {
            keys.constants.insert(*column);
        }
        for left in &self.sets {
            for right in &other.sets {
                let mut both = left.clone();
                both.extend(right.iter().copied());
                keys.add(both);
            }
        }
        keys
    }
}

/// What is known about every node of a plan, indexed by [`NodeRef`].
///
/// One walk rather than a call per node, because every rule reads its inputs and a caller asking
/// about the root would otherwise walk the plan once per question.
#[must_use]
pub fn keys_of(plan: &Plan) -> Vec<Keys> {
    let mut known = vec![Keys::unknown(); plan.node_count()];
    let mut order = Vec::with_capacity(plan.node_count());
    postorder(plan, plan.root(), &mut order);
    for node in order {
        known[node as usize] = compute(plan, node, &known);
    }
    known
}

/// The nodes under a root, children before parents, so one pass can read what it depends on.
fn postorder(plan: &Plan, at: NodeRef, out: &mut Vec<NodeRef>) {
    for child in plan.node(at).children().into_iter().flatten() {
        postorder(plan, child, out);
    }
    out.push(at);
}

/// What one node's output is known to be keyed by, given what its inputs are.
fn compute(plan: &Plan, at: NodeRef, known: &[Keys]) -> Keys {
    let below = |node: NodeRef| known[node as usize].clone();
    match *plan.node(at) {
        // One row and no columns, which is the empty key and the reason the empty key exists.
        Node::Dummy => Keys::single(),

        // The group expressions tell the groups apart, because that is what grouping is. An
        // ungrouped aggregate has none of them and produces exactly one row, so the empty key falls
        // out of the same rule rather than needing its own.
        //
        // A group expression that reads a column the input holds constant is a column with one
        // value, so it comes out constant too and drops out of the key. That is what makes
        // `WHERE a = 1 GROUP BY a, b` a group by `b`, and with every group column fixed it is the
        // empty key and one row.
        Node::Aggregate { input, index, groups, .. } => {
            let source = below(input);
            let mut keys = Keys::default();
            for (position, expr) in plan.expr_list(groups).iter().enumerate() {
                let Expr::Column(binding) = *plan.expr(*expr) else { continue };
                if source.constants.contains(&binding) {
                    keys.constants.insert(ColumnBinding::new(index, at_most_u32(position)));
                }
            }
            keys.add(
                (0..len(plan, groups))
                    .map(|position| ColumnBinding::new(index, position))
                    .collect(),
            );
            keys
        }

        // Plain `DISTINCT` keys the whole row. `DISTINCT ON` keys the expressions it was written
        // with, when they are columns: one row survives per value of them, which is the definition.
        // An expression that is not a column is one this cannot name in a key, and a key that
        // mentions a column nobody can read is worse than no key.
        Node::Distinct { input, on } => {
            let mut keys = below(input);
            if on.len == 0 {
                keys.row = true;
                return keys;
            }
            let exprs = plan.expr_list(on);
            let mut columns = Vec::with_capacity(exprs.len());
            for expr in exprs {
                let Expr::Column(binding) = *plan.expr(*expr) else { return keys };
                columns.push(binding);
            }
            keys.add(columns);
            keys
        }

        // A set operation that is not `ALL` eliminates duplicates over its whole output, and one
        // that is `ALL` is a concatenation and keeps nothing: the same row may come from both sides.
        Node::SetOp { all, .. } => {
            if all {
                Keys::unknown()
            } else {
                Keys::whole_row()
            }
        }

        // A filter drops rows and never adds one, so every key survives. What it adds is the
        // constants: a column the predicate holds equal to a literal has one value in everything
        // that gets through, and that is the dependency `WHERE a = 1 GROUP BY a, b` needs.
        Node::Filter { input, predicate } => {
            let mut keys = below(input);
            for column in fixed(plan, predicate) {
                keys.fix(column);
            }
            keys
        }

        // Reordering and appending change nothing about which rows there are.
        Node::Sort { input, .. } | Node::Window { input, .. } => below(input),

        // Taking a prefix keeps every key, and a prefix of one row is the empty key whatever was
        // underneath. A share of the input is a prefix whose length nothing here knows, so it keeps
        // the keys and claims nothing about how many rows there are: a percentage that works out at
        // one row is still one row of something this cannot count.
        // A count that is read off the rows while the query runs is a number nobody has here, so
        // it falls in with every other prefix this cannot measure.
        Node::Limit { input, count, .. } => match count {
            Bound::Rows(0 | 1) => Keys::single(),
            Bound::All | Bound::Rows(_) | Bound::Read(_) => below(input),
        },
        Node::LimitPercent { input, .. } => below(input),
        Node::TopN { input, count, .. } => {
            if count <= 1 {
                Keys::single()
            } else {
                below(input)
            }
        }

        // A projection produces its own columns, so a key survives only if every column in it is
        // passed through, and it comes out renamed to where it was passed through to. The whole row
        // claim does not survive at all, because proving a projection is a permutation of its input
        // needs the input's width and this is the one caller that would ever ask for it. A
        // `SELECT DISTINCT a, b` that projects to `a` is the case that loses, and it loses by
        // saying nothing rather than by saying something that is false one shape later.
        Node::Project { input, index, exprs, .. } => {
            let mut moved = Vec::new();
            for (position, expr) in plan.expr_list(exprs).iter().enumerate() {
                if let Expr::Column(binding) = *plan.expr(*expr) {
                    moved.push((binding, ColumnBinding::new(index, at_most_u32(position))));
                }
            }
            let source = below(input);
            let mut keys = Keys::default();
            for (from, to) in &moved {
                if source.constants.contains(from) {
                    keys.constants.insert(*to);
                }
            }
            for set in &source.sets {
                let mut mapped = Vec::with_capacity(set.len());
                for column in set {
                    let Some((_, to)) = moved.iter().find(|(from, _)| from == column) else {
                        mapped.clear();
                        break;
                    };
                    mapped.push(*to);
                }
                if mapped.len() == set.len() {
                    keys.add(mapped);
                }
            }
            keys
        }

        // A row of the product is a row of each side, so a key of each side together is a key of
        // it. An inner join with an equality onto a key of one side keeps the other side's keys on
        // their own, because each row of that side meets at most one row of this one.
        Node::Join { left, right, kind, conditions, build: _ } => match kind {
            JoinKind::Inner | JoinKind::Positional => {
                let (left, right) = (below(left), below(right));
                let mut keys = left.product(&right);
                for (from, onto) in [(&left, &right), (&right, &left)] {
                    if onto.covers(&equated(plan, conditions)) {
                        for set in &from.sets {
                            keys.add(set.clone());
                        }
                    }
                }
                keys
            }
            // These produce the rows of their left side and nothing else, at most once each.
            JoinKind::Semi | JoinKind::Anti | JoinKind::Mark | JoinKind::Single => below(left),
            // A padded row holds nulls where the other side's key was, and two padded rows agree
            // there. Saying nothing is the only sound answer without a null aware key.
            JoinKind::Left | JoinKind::Right | JoinKind::Full => Keys::unknown(),
        },
        // The same rules as the join above it, over the same two sides, which it has to be: the
        // rewrite that produced this node is only allowed to produce it where a hash join over
        // these two inputs would have answered the same rows. A link join's kinds are the four
        // section 5.2 names, and `Left` is the only one of them that pads.
        Node::LinkJoin { child, parent, kind, conditions, rid: _ } => match kind {
            JoinKind::Inner => {
                let (child, parent) = (below(child), below(parent));
                let mut keys = child.product(&parent);
                for (from, onto) in [(&child, &parent), (&parent, &child)] {
                    if onto.covers(&equated(plan, conditions)) {
                        for set in &from.sets {
                            keys.add(set.clone());
                        }
                    }
                }
                keys
            }
            JoinKind::Semi | JoinKind::Anti => below(child),
            JoinKind::Left => Keys::unknown(),
            // Refused by `Plan::check`, so this is unreachable rather than a decision. Saying
            // nothing is the sound answer for a plan that should not exist.
            JoinKind::Right
            | JoinKind::Full
            | JoinKind::Mark
            | JoinKind::Single
            | JoinKind::Positional => Keys::unknown(),
        },
        Node::CrossProduct { left, right } => below(left).product(&below(right)),

        // A materialisation produces what the query reading it produces.
        Node::MaterializedCte { body, .. } => below(body),

        // Nothing is stored that would let a leaf claim a key. `PRIMARY KEY` parses and the catalog
        // does not keep it, so a scan claiming one would be claiming it from the spelling of the
        // DDL rather than from anything enforced. This is the first thing P1 should change and the
        // second checklist item there is what changes it.
        Node::Get { .. }
        | Node::Values { .. }
        | Node::TableFunction { .. }
        | Node::LateralFunction { .. }
        | Node::Fetch { .. }
        | Node::TableFetch { .. }
        | Node::CteScan { .. }
        | Node::Consistent { .. }
        | Node::DependentJoin { .. } => Keys::unknown(),
    }
}

/// The columns an equality condition holds against the other side.
///
/// For the question of whether one side of a join is met at most once, what matters is which of
/// that side's columns the condition pins down. Both sides' columns come back in one list, which is
/// what [`Keys::covers`] wants since it is asking whether a key is contained rather than which side
/// each column came from. A condition that is not an equality between two
/// columns pins nothing, and a list that is missing one of the columns a key needs is a list that
/// fails [`Keys::covers`], which is the answer wanted.
fn equated(plan: &Plan, conditions: Slice) -> Vec<ColumnBinding> {
    let mut found = Vec::new();
    for condition in plan.expr_list(conditions) {
        let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(*condition) else {
            continue;
        };
        for side in [left, right] {
            if let Expr::Column(binding) = *plan.expr(side) {
                found.push(binding);
            }
        }
    }
    found
}

/// The columns a predicate holds equal to something that is the same in every row.
///
/// Only through `AND`, since a column is fixed by one arm of an `OR` and free in the other. Only
/// against a constant, since a column equal to another column is two columns that vary together
/// rather than one that does not vary.
fn fixed(plan: &Plan, predicate: ExprRef) -> Vec<ColumnBinding> {
    let mut found = Vec::new();
    collect_fixed(plan, predicate, &mut found);
    found
}

/// The walk behind [`fixed`].
fn collect_fixed(plan: &Plan, predicate: ExprRef, found: &mut Vec<ColumnBinding>) {
    match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => {
            for child in plan.expr_list(children) {
                collect_fixed(plan, *child, found);
            }
        }
        Expr::Compare { op: CompareOp::Equal | CompareOp::NotDistinctFrom, left, right } => {
            for (column, other) in [(left, right), (right, left)] {
                if let (Expr::Column(binding), Expr::Constant(_)) =
                    (plan.expr(column), plan.expr(other))
                {
                    found.push(*binding);
                }
            }
        }
        _ => {}
    }
}

/// How many expressions a slice holds.
fn len(plan: &Plan, slice: Slice) -> u32 {
    at_most_u32(plan.expr_list(slice).len())
}

/// A position as the width a binding wants, clamped rather than wrapped.
fn at_most_u32(position: usize) -> u32 {
    u32::try_from(position).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use rudb_common::{Field, LogicalType, Value};

    use super::{Keys, keys_of};
    use crate::expr::{ColumnBinding, CompareOp, Expr};
    use crate::node::{Bound, JoinKind, Node};
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

    /// What the root of a plan is keyed by.
    fn root(plan: &Plan) -> Keys {
        keys_of(plan)[plan.root() as usize].clone()
    }

    #[test]
    fn a_group_by_is_keyed_by_what_it_grouped_on() {
        // This is the one that pays for the module. A group by produces one row per value of its
        // key, which is the definition of grouping and not an estimate of anything.
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let group = column(&mut plan, 0, 0);
        let groups = plan.add_expr_list(&[group]);
        let empty = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::Aggregate { input, index: 1, groups, aggregates: empty });
        plan.set_root(node);
        assert_eq!(root(&plan).sets(), [vec![ColumnBinding::new(1, 0)]]);
    }

    #[test]
    fn an_ungrouped_aggregate_produces_one_row_and_says_so_with_the_empty_key() {
        // Not a special case in the code. An ungrouped aggregate groups by nothing, the key is the
        // empty set, and two rows that agree everywhere in the empty set are the same row.
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let empty = plan.add_expr_list(&[]);
        let node =
            plan.add_node(Node::Aggregate { input, index: 1, groups: empty, aggregates: empty });
        plan.set_root(node);
        assert!(root(&plan).at_most_one_row());
    }

    #[test]
    fn a_scan_claims_nothing_because_nothing_stores_a_primary_key() {
        let mut plan = Plan::new();
        let node = scan(&mut plan, 0);
        plan.set_root(node);
        assert!(root(&plan).is_empty());
    }

    #[test]
    fn a_column_held_equal_to_a_literal_comes_out_of_the_key_it_was_in() {
        // Group by `a, b` and the pair is the key. Filter to `a = 1` and `b` alone is the key,
        // because there is one value of `a` left and a pair that differs only in a column with one
        // value is not a pair. Filter both and there is at most one row. This is the functional
        // dependency the four rewrites actually read, and it is the reason constants are here.
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let a = column(&mut plan, 0, 0);
        let b = column(&mut plan, 0, 1);
        let groups = plan.add_expr_list(&[a, b]);
        let empty = plan.add_expr_list(&[]);
        let agg = plan.add_node(Node::Aggregate { input, index: 1, groups, aggregates: empty });
        let left = column(&mut plan, 1, 0);
        let one = plan.add_constant(Value::Integer(1));
        let predicate = plan.add_expr(
            Expr::Compare { op: CompareOp::Equal, left, right: one },
            LogicalType::Boolean,
        );
        let filter = plan.add_node(Node::Filter { input: agg, predicate });
        plan.set_root(filter);
        let all = keys_of(&plan);
        assert_eq!(
            all[agg as usize].sets(),
            [vec![ColumnBinding::new(1, 0), ColumnBinding::new(1, 1)]]
        );
        let keys = all[filter as usize].clone();
        assert_eq!(keys.constants().collect::<Vec<_>>(), [ColumnBinding::new(1, 0)]);
        assert_eq!(keys.sets(), [vec![ColumnBinding::new(1, 1)]]);
        assert!(keys.covers(&[ColumnBinding::new(1, 0), ColumnBinding::new(1, 1)]));
        assert!(!keys.at_most_one_row());
    }

    #[test]
    fn a_group_by_on_a_column_the_filter_under_it_fixed_produces_one_row() {
        // `WHERE a = 1 GROUP BY a` is one group, and it is one group for a reason the analysis can
        // see rather than for a reason somebody has to spot. This is the constant crossing a node
        // boundary, which is the half of it that is easy to forget to write.
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let left = column(&mut plan, 0, 0);
        let one = plan.add_constant(Value::Integer(1));
        let predicate = plan.add_expr(
            Expr::Compare { op: CompareOp::Equal, left, right: one },
            LogicalType::Boolean,
        );
        let filter = plan.add_node(Node::Filter { input, predicate });
        let group = column(&mut plan, 0, 0);
        let groups = plan.add_expr_list(&[group]);
        let empty = plan.add_expr_list(&[]);
        let node =
            plan.add_node(Node::Aggregate { input: filter, index: 1, groups, aggregates: empty });
        plan.set_root(node);
        assert!(root(&plan).at_most_one_row());
    }

    #[test]
    fn a_predicate_that_fixes_every_column_of_a_key_leaves_at_most_one_row() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let a = column(&mut plan, 0, 0);
        let groups = plan.add_expr_list(&[a]);
        let empty = plan.add_expr_list(&[]);
        let agg = plan.add_node(Node::Aggregate { input, index: 1, groups, aggregates: empty });
        let left = column(&mut plan, 1, 0);
        let one = plan.add_constant(Value::Integer(1));
        let predicate = plan.add_expr(
            Expr::Compare { op: CompareOp::Equal, left, right: one },
            LogicalType::Boolean,
        );
        let filter = plan.add_node(Node::Filter { input: agg, predicate });
        plan.set_root(filter);
        assert!(root(&plan).at_most_one_row());
    }

    #[test]
    fn an_or_fixes_nothing_because_a_column_is_pinned_by_one_arm_and_free_in_the_other() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let a = column(&mut plan, 0, 0);
        let groups = plan.add_expr_list(&[a]);
        let empty = plan.add_expr_list(&[]);
        let agg = plan.add_node(Node::Aggregate { input, index: 1, groups, aggregates: empty });
        let mut arms = Vec::new();
        for value in [1i32, 2] {
            let left = column(&mut plan, 1, 0);
            let literal = plan.add_constant(Value::Integer(value));
            arms.push(plan.add_expr(
                Expr::Compare { op: CompareOp::Equal, left, right: literal },
                LogicalType::Boolean,
            ));
        }
        let children = plan.add_expr_list(&arms);
        let predicate = plan.add_expr(
            Expr::Conjunction { op: crate::expr::ConjunctionOp::Or, children },
            LogicalType::Boolean,
        );
        let filter = plan.add_node(Node::Filter { input: agg, predicate });
        plan.set_root(filter);
        let keys = root(&plan);
        assert_eq!(keys.constants().count(), 0);
        assert!(!keys.at_most_one_row());
    }

    #[test]
    fn a_projection_carries_a_key_through_and_drops_one_it_did_not_pass_on() {
        // The key of the group by is column zero of index one. A projection that passes it through
        // renames it and keeps it. A projection that keeps only the aggregate loses it, and losing
        // it is the right answer rather than a gap.
        for (kept, expected) in [(0u32, 1usize), (1, 0)] {
            let mut plan = Plan::new();
            let input = scan(&mut plan, 0);
            let group = column(&mut plan, 0, 0);
            let groups = plan.add_expr_list(&[group]);
            let empty = plan.add_expr_list(&[]);
            let agg = plan.add_node(Node::Aggregate { input, index: 1, groups, aggregates: empty });
            let passed = column(&mut plan, 1, kept);
            let exprs = plan.add_expr_list(&[passed]);
            let name = plan.intern("x");
            let names = plan.add_name_list(&[name]);
            let node = plan.add_node(Node::Project { input: agg, index: 2, exprs, names });
            plan.set_root(node);
            assert_eq!(root(&plan).sets().len(), expected, "keeping column {kept}");
        }
    }

    #[test]
    fn a_join_onto_a_key_keeps_the_other_sides_keys_on_their_own() {
        // The right side is grouped by its column zero and the condition is an equality onto it, so
        // each left row meets at most one right row and a key of the left is still a key of the
        // join. Without that rule the only key would be both sides' together, which is what
        // licenses nothing.
        let mut plan = Plan::new();
        let left_scan = scan(&mut plan, 0);
        let left_group = column(&mut plan, 0, 0);
        let left_groups = plan.add_expr_list(&[left_group]);
        let empty = plan.add_expr_list(&[]);
        let left = plan.add_node(Node::Aggregate {
            input: left_scan,
            index: 1,
            groups: left_groups,
            aggregates: empty,
        });
        let right_scan = scan(&mut plan, 2);
        let right_group = column(&mut plan, 2, 0);
        let right_groups = plan.add_expr_list(&[right_group]);
        let right = plan.add_node(Node::Aggregate {
            input: right_scan,
            index: 3,
            groups: right_groups,
            aggregates: empty,
        });
        let on_left = column(&mut plan, 1, 0);
        let on_right = column(&mut plan, 3, 0);
        let condition = plan.add_expr(
            Expr::Compare { op: CompareOp::Equal, left: on_left, right: on_right },
            LogicalType::Boolean,
        );
        let conditions = plan.add_expr_list(&[condition]);
        let node = plan.add_node(Node::Join {
            left,
            right,
            kind: JoinKind::Inner,
            conditions,
            build: crate::node::BuildSide::Right,
        });
        plan.set_root(node);
        let keys = root(&plan);
        assert!(keys.covers(&[ColumnBinding::new(1, 0)]), "{keys:?}");
        assert!(keys.covers(&[ColumnBinding::new(3, 0)]), "{keys:?}");
    }

    #[test]
    fn an_outer_join_keeps_nothing_because_two_padded_rows_agree_where_the_key_was() {
        let mut plan = Plan::new();
        let left_scan = scan(&mut plan, 0);
        let group = column(&mut plan, 0, 0);
        let groups = plan.add_expr_list(&[group]);
        let empty = plan.add_expr_list(&[]);
        let left = plan.add_node(Node::Aggregate {
            input: left_scan,
            index: 1,
            groups,
            aggregates: empty,
        });
        let right = scan(&mut plan, 2);
        let node = plan.add_node(Node::Join {
            left,
            right,
            kind: JoinKind::Left,
            conditions: empty,
            build: crate::node::BuildSide::Right,
        });
        plan.set_root(node);
        assert!(root(&plan).is_empty());
    }

    #[test]
    fn a_distinct_keys_the_whole_row_and_a_union_all_keys_nothing() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let empty = plan.add_expr_list(&[]);
        let node = plan.add_node(Node::Distinct { input, on: empty });
        plan.set_root(node);
        assert!(root(&plan).row());

        let mut plan = Plan::new();
        let left = scan(&mut plan, 0);
        let right = scan(&mut plan, 1);
        let node = plan.add_node(Node::SetOp {
            left,
            right,
            kind: crate::node::SetOpKind::Union,
            all: true,
            index: 2,
        });
        plan.set_root(node);
        assert!(root(&plan).is_empty());
    }

    #[test]
    fn a_limit_of_one_row_is_one_row_whatever_was_underneath_it() {
        let mut plan = Plan::new();
        let input = scan(&mut plan, 0);
        let node =
            plan.add_node(Node::Limit { input, count: Bound::Rows(1), offset: Bound::Rows(0) });
        plan.set_root(node);
        assert!(root(&plan).at_most_one_row());
    }
}
