//! Choosing which order a run of inner joins runs in.
//!
//! Until this, the order was the order of the `FROM` list. Filter pushdown walks down the cross
//! products the binder made and turns a predicate into a join condition at the first level where
//! both of the columns it reads are available, which is a rule about where a predicate may go and
//! not a rule about which join is worth doing first. It produces a correct plan in whatever order
//! the query was typed in.
//!
//! On TPC-H q9 that costs a cross product. The `FROM` list is part, supplier, lineitem, partsupp,
//! orders, nation, and no condition in the query reads part and supplier and nothing else, because
//! the two of them are joined through lineitem. So the first two entries of the list become a
//! genuine cross product, ten thousand suppliers against every part whose name has green in it,
//! which is a hundred million rows built to answer a query whose answer has 175 rows in it.
//!
//! # What it does
//!
//! A run of inner joins and cross products with nothing else between them is one region, and the
//! things it joins are its leaves. Every condition of every join in the region is a condition of the
//! region, because an inner join is associative and commutative and its conditions are anded, so any
//! of them may be tested at any join that has the columns it reads. That is the whole licence this
//! pass needs and it is why nothing here rewrites an expression: a column is bound to the operator
//! that produces it rather than to a position in a row, so moving a join does not move a column.
//!
//! The order is chosen greedily. Repeatedly take the pair of parts with a condition between them
//! that produces the fewest rows, join them, and put the result back, until one part is left. A pair
//! with a condition between them is scored with [`crate::estimate::matched_sides`], which is the
//! larger of the two sides and the keyspace reading of the conditions that become testable at that
//! pair, whichever is bigger, cut down by the fraction the filters under each side kept. A pair with
//! no condition between them is taken only when no pair in the region has one, which is the case
//! where a cross product is the only thing left to build, and `cheapest` is where that rule is and
//! why it is not a tie break.
//!
//! Every part carries two numbers for that, the rows it produces and the rows it would produce with
//! the filters under it taken out, and a pair is scored from both. Containment is a statement about
//! two whole tables, so scoring a filtered side by containment alone says a join to a fiftieth of
//! part is the whole of lineitem, and then nothing in the region reduces and the tie breaks on the
//! smaller input. That is how q9 came to join supplier first, which removes no rows at all, before
//! the part join, which removes nineteen rows in twenty. The filter is charged once, at the join
//! that first saw the side it sits under, because what carries up out of a pair is the unfiltered
//! join and not the answer.
//!
//! The order that comes out replaces the one that was there when the sum of the rows its joins
//! produce is smaller, and never when it builds more cross products than the region already had.
//! Both orders are scored the same way and by the same function the greedy step minimises one pair
//! at a time.
//!
//! A region of up to fourteen leaves is searched exhaustively instead, over the connected subgraphs
//! and their complements, which is DPccp and is described at [`searched`]. Greedy is what a larger
//! region gets, and what a region gets whose graph has a shape that would make the search score too
//! many pairs. Both are scored by the same measure, so the search only ever finds an order greedy
//! would have scored the same way or a cheaper one.
//!
//! The search is given the edges an equality between columns implies as well as the ones written.
//! Columns that a chain of equalities joins are one equivalence class, and two columns of a class in
//! different leaves are an edge whether or not a condition compares them directly. Joining two sets
//! that each hold a column of the class then tests one of the class's conditions and not all of
//! them, since inside each set the class's columns are already equal. [`classed`] is where that is
//! and why it is the same query.
//!
//! # What it refuses
//!
//! A region where any leaf has no row estimate. Two sides cannot be compared when one of them is
//! unknown, and picking an order from a number that was made up to fill the gap is how a plan gets
//! worse rather than better.
//!
//! A condition that reads something no leaf of the region produces, which is a correlated reference
//! the unnesting pass has not taken out yet, and a condition that reads only one leaf, which is a
//! filter written as a join condition. Neither of those is a thing to reason about here, and a
//! region with one in it is left exactly as it was.
//!
//! An order that does not cost less than the order the query was written in. The cost is the sum of
//! the rows the joins produce, which is the measure the greedy step is minimising one pair at a
//! time, and the plan that was already there is scored the same way and kept when it wins. That
//! keeps this pass off every query whose `FROM` list was already in a sensible order, which is most
//! of them, and it means a plan can only be replaced by one this pass believes is better rather than
//! by one it merely built later.
//!
//! An order that builds more cross products than the region already had. A cross product is worse
//! than a join with a condition on it whatever the two sides are, and that is true without knowing a
//! single distinct count, so it is refused whatever the rest of the sum says.
//!
//! This pass was held to removing cross products and nothing else until #917, because the only thing
//! the cost function could say about a join with a condition on it was the containment assumption,
//! and on q5 that is wrong by two orders: it put customer joined to supplier on `nationkey` at a
//! hundred and fifty thousand rows, the size of the larger side, when there are twenty five nations
//! in the table and the answer is sixty million. Greedy believed it, built that join first and made
//! q5 seventy times slower. The distinct counts are what took the refusal out, and the same join now
//! scores at what it produces, so greedy leaves it until the sides have been cut down.

use std::collections::HashMap;

use rudb_common::{LogicalType, Result};
use rudb_plan::{
    BuildSide, ColumnBinding, CompareOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
};

use crate::estimate::{self, Facts, Side};
use crate::pass::{Context, Pass};
use crate::tables::{TableSet, Tables, produced};
use crate::walk;

/// Reorders each run of inner joins by how many rows the orders are estimated to produce.
#[derive(Debug, Clone, Copy)]
pub struct JoinOrder;

impl Pass for JoinOrder {
    /// DuckDB's name for the same job, which is one of the forty four `duckdb_optimizers()` lists.
    fn name(&self) -> &'static str {
        "join_order"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        reorder(plan, context.facts());
        Ok(())
    }
}

/// Reorders every run of inner joins in `plan`.
pub fn reorder(plan: &mut Plan, stats: &Facts) {
    let mut tables = Tables::new();
    let root = rebuild(plan, plan.root(), &mut tables, stats);
    plan.set_root(root);
}

/// One part of a region as the search has it so far, which starts as a leaf and ends as the region.
struct Part {
    /// Which entry of the build list produces its rows.
    build: usize,
    /// Which table indices those rows carry, which is what says whether a condition can be tested.
    tables: TableSet,
    /// How many rows it is estimated to produce, and how many it would produce unfiltered.
    ///
    /// Both numbers, because the second is what the next pair is scored from. A filter is charged
    /// once, at the join that first saw the side it sits under, and the unfiltered size is what
    /// carries up so that no join above charges it again.
    side: Side,
    /// The node, while the part is still one leaf, for [`estimate::named`] to read its filters.
    leaf: Option<NodeRef>,
}

/// One node of the order the search chose, before any of it is put in the arena.
///
/// The search is scored against the order that was already there and loses most of the time, so it
/// writes down what it would build and builds it only if it wins. A pass that added nodes while
/// searching would leave the arena holding a plan nobody runs every time it decided not to.
enum Build {
    /// Something the region joins, which is already a node.
    Leaf(NodeRef),
    /// A join of two entries, or a cross product where there are no conditions between them.
    Pair { left: usize, right: usize, conditions: Vec<ExprRef> },
}

/// Rewrites the plan under `at`, returning `at` itself where nothing under it changed.
fn rebuild(plan: &mut Plan, at: NodeRef, tables: &mut Tables, stats: &Facts) -> NodeRef {
    if joining(plan, at) {
        let mut leaves = Vec::new();
        let mut conditions = Vec::new();
        gather(plan, at, &mut leaves, &mut conditions);
        let rebuilt: Vec<NodeRef> =
            leaves.iter().map(|&leaf| rebuild(plan, leaf, tables, stats)).collect();
        // Two leaves is one join and there is nothing to choose. The region is still rebuilt where a
        // leaf changed under it, which the walk below does.
        let chosen = match leaves.len() {
            0..=2 => None,
            _ => order(plan, at, &rebuilt, &conditions, tables, stats),
        };
        if let Some(chosen) = chosen {
            return chosen;
        }
        if rebuilt == leaves {
            return at;
        }
        return restack(plan, at, &leaves, &rebuilt);
    }
    let children: Vec<NodeRef> = plan.node(at).children().into_iter().flatten().collect();
    let rebuilt: Vec<NodeRef> =
        children.iter().map(|&child| rebuild(plan, child, tables, stats)).collect();
    if rebuilt == children {
        return at;
    }
    let mut node = plan.node(at).clone();
    walk::replace_children(&mut node, &rebuilt);
    plan.add_node(node)
}

/// Whether this node is part of a region, which is an inner join or a cross product and nothing else.
fn joining(plan: &Plan, at: NodeRef) -> bool {
    matches!(*plan.node(at), Node::CrossProduct { .. } | Node::Join { kind: JoinKind::Inner, .. })
}

/// The leaves and the conditions of the region rooted at `at`, in the order the region holds them.
fn gather(plan: &Plan, at: NodeRef, leaves: &mut Vec<NodeRef>, conditions: &mut Vec<ExprRef>) {
    match *plan.node(at) {
        Node::CrossProduct { left, right } => {
            gather(plan, left, leaves, conditions);
            gather(plan, right, leaves, conditions);
        }
        Node::Join { left, right, kind: JoinKind::Inner, conditions: list, .. } => {
            conditions.extend_from_slice(plan.expr_list(list));
            gather(plan, left, leaves, conditions);
            gather(plan, right, leaves, conditions);
        }
        _ => leaves.push(at),
    }
}

/// Rebuilds the region rooted at `at` with each leaf replaced by what it rebuilt to.
///
/// The shape is the shape that was already there. This is the path for a region the search declined
/// or did not improve on, where something underneath a leaf changed anyway.
fn restack(plan: &mut Plan, at: NodeRef, leaves: &[NodeRef], rebuilt: &[NodeRef]) -> NodeRef {
    if let Some(found) = leaves.iter().position(|&leaf| leaf == at) {
        return rebuilt[found];
    }
    let children: Vec<NodeRef> = plan.node(at).children().into_iter().flatten().collect();
    let children: Vec<NodeRef> =
        children.into_iter().map(|child| restack(plan, child, leaves, rebuilt)).collect();
    let mut node = plan.node(at).clone();
    walk::replace_children(&mut node, &children);
    plan.add_node(node)
}

/// The region built in the order the search chose, or `None` where it will not choose one.
fn order(
    plan: &mut Plan,
    at: NodeRef,
    leaves: &[NodeRef],
    conditions: &[ExprRef],
    tables: &mut Tables,
    stats: &Facts,
) -> Option<NodeRef> {
    let mut builds: Vec<Build> = leaves.iter().map(|&leaf| Build::Leaf(leaf)).collect();
    let mut parts = Vec::with_capacity(leaves.len());
    for (build, &leaf) in leaves.iter().enumerate() {
        parts.push(Part {
            build,
            tables: produced(plan, leaf),
            side: estimate::side(plan, leaf, stats)?,
            leaf: Some(leaf),
        });
    }
    let mut whole = TableSet::new();
    for part in &parts {
        whole.extend(&part.tables);
    }
    let mut pending: Vec<(ExprRef, TableSet)> = Vec::with_capacity(conditions.len());
    for &condition in conditions {
        let reads = tables.of(plan, condition);
        // A condition that reaches outside the region belongs to a scope this pass is not reasoning
        // about, and one that reads a single leaf is a filter rather than an edge. Either way the
        // region is left as it was rather than rebuilt around a condition nobody can place.
        if !reads.is_subset_of(&whole) || parts.iter().any(|part| reads.is_subset_of(&part.tables))
        {
            return None;
        }
        pending.push((condition, reads));
    }
    let (_, before, was) = cost(plan, at, stats)?;
    let (searchable, classes) = classed(plan, &parts, &pending);
    if let Some((top, after)) = searched(plan, &parts, &searchable, &classes, stats, &mut builds) {
        // The search builds no cross product, so it wins outright over an order that builds one,
        // and otherwise the sum decides. Greedy has nothing to add either way: the order it would
        // find is one of the orders the search already scored.
        return (was > 0 || after < before).then(|| put(plan, &builds, top));
    }
    let mut after = 0u64;
    let mut built = 0usize;
    while parts.len() > 1 {
        let (left, right, side) = cheapest(plan, &parts, &pending, stats);
        let mut union = parts[left].tables.clone();
        union.extend(&parts[right].tables);
        let conditions: Vec<ExprRef> = pending
            .iter()
            .filter(|(_, reads)| reads.is_subset_of(&union))
            .map(|(condition, _)| *condition)
            .collect();
        pending.retain(|(_, reads)| !reads.is_subset_of(&union));
        // The larger index comes out first, so the smaller one is still where it was.
        let right = parts.remove(right);
        let left = parts.remove(left);
        built += usize::from(conditions.is_empty());
        builds.push(Build::Pair { left: left.build, right: right.build, conditions });
        after = after.saturating_add(side.rows);
        parts.push(Part { build: builds.len() - 1, tables: union, side, leaf: None });
    }
    // An order that builds more cross products than the region already had is refused whatever the
    // sum says, because a cross product is worse than a join with a condition on it whatever the two
    // sides are and no estimate is needed to know that. Otherwise the sum decides, which it can now
    // that the rows it adds up come from [`estimate::matched`] rather than from the containment
    // assumption alone. That is the change #917 made: a join on a low cardinality key is scored at
    // what it produces, so the order that puts one first no longer looks like the cheap one.
    //
    // An order that builds fewer of them is taken whatever the sum says, for the same reason. The
    // sum undercounts a cross product of one row dimensions, because each of them is charged once
    // at the join that first reads it and a cross product reads none of them. JOB 13d with the
    // consistent rule off kept its `FROM` list, five dimensions crossed and then joined to
    // `movie_companies` on two keys, once the joins to the dimensions were priced at what they
    // keep, and ran in two seconds where the joined order runs in two hundred milliseconds.
    if built > was || (built == was && after >= before) {
        return None;
    }
    Some(put(plan, &builds, parts[0].build))
}

/// The conditions [`searched`] is given, and the equivalence class of columns each one belongs to.
///
/// The region's own conditions come first and in their order. After them comes one condition for
/// every two columns that a chain of equalities says are equal and no condition compares directly.
/// TPC-H q05 is why. It writes `c_nationkey = s_nationkey` and `s_nationkey = n_nationkey`, which
/// say that customer's nation is nation's key as well, and without that edge the search could not
/// join customer to the five nations of Asia before supplier was in. It never saw the thirty
/// thousand customers that join keeps, chose orders, lineitem and supplier first, and built all
/// 150,000 customers into the last join.
///
/// The class is what keeps an edge from being tested twice. Joining two sets that each hold a
/// column of a class needs one of the class's conditions between them and not all of them, provided
/// the class's columns inside each set are already equal, so [`Search::crossing`] takes the first
/// condition of each class and leaves the rest. Inside a set they are equal because every edge of the
/// class is there: any two sets holding a column of it each have a condition of it between them, so
/// every join that brought two of its columns together tested one. That fails where a leaf holds two
/// columns of one class, since nothing inside the leaf compared them, and where the edges would not
/// fit in the sixty four conditions the search counts in, and then no condition is given a class and
/// none is added.
///
/// Only `=` between two bare columns of the same type makes a class. A null matches nothing under
/// it, so a row whose column is null is dropped by the written conditions and by the implied one
/// alike, and the region is inner joins, so a row dropped earlier is a row that would have been
/// dropped later.
fn classed(
    plan: &mut Plan,
    parts: &[Part],
    pending: &[(ExprRef, TableSet)],
) -> (Vec<(ExprRef, TableSet)>, Vec<Option<usize>>) {
    // Each column the equalities read, the expression that reads it, and its parent in the union.
    let mut columns: Vec<(ColumnBinding, ExprRef)> = Vec::new();
    let mut parent: Vec<usize> = Vec::new();
    let mut edges: Vec<Option<(usize, usize)>> = Vec::with_capacity(pending.len());
    for &(condition, _) in pending {
        let Some(pair) = equated(plan, condition) else {
            edges.push(None);
            continue;
        };
        let one = numbered(&mut columns, &mut parent, pair.0);
        let other = numbered(&mut columns, &mut parent, pair.1);
        let (top, under) = (root(&mut parent, one), root(&mut parent, other));
        parent[top] = under;
        edges.push(Some((one, other)));
    }
    let leaf = |at: usize| parts.iter().position(|part| part.tables.contains(columns[at].0.table));
    // The classes by their root, each with its columns, and whether a leaf holds two of them.
    let mut members: Vec<(usize, Vec<usize>)> = Vec::new();
    for at in 0..columns.len() {
        let top = root(&mut parent, at);
        match members.iter_mut().find(|(held, _)| *held == top) {
            Some((_, list)) => list.push(at),
            None => members.push((top, vec![at])),
        }
    }
    let mut usable: Vec<usize> = Vec::new();
    let mut added: Vec<(ExprRef, ExprRef, TableSet, usize)> = Vec::new();
    for (top, list) in &members {
        let leaves: Vec<Option<usize>> = list.iter().map(|&at| leaf(at)).collect();
        let shared = leaves
            .iter()
            .enumerate()
            .any(|(at, one)| one.is_none() || leaves[at + 1..].iter().any(|other| other == one));
        if shared {
            continue;
        }
        let class = usable.len();
        usable.push(*top);
        for (place, &one) in list.iter().enumerate() {
            for &other in &list[place + 1..] {
                let direct = edges
                    .iter()
                    .flatten()
                    .any(|&(a, b)| (a, b) == (one, other) || (a, b) == (other, one));
                if !direct {
                    let mut reads = TableSet::of(columns[one].0.table);
                    reads.insert(columns[other].0.table);
                    added.push((columns[one].1, columns[other].1, reads, class));
                }
            }
        }
    }
    if pending.len() + added.len() > 64 {
        return (pending.to_vec(), vec![None; pending.len()]);
    }
    let mut classes: Vec<Option<usize>> = edges
        .iter()
        .map(|edge| {
            let (one, _) = (*edge)?;
            let top = root(&mut parent, one);
            usable.iter().position(|&held| held == top)
        })
        .collect();
    let mut searchable = pending.to_vec();
    for (left, right, reads, class) in added {
        let condition = plan
            .add_expr(Expr::Compare { op: CompareOp::Equal, left, right }, LogicalType::Boolean);
        searchable.push((condition, reads));
        classes.push(Some(class));
    }
    (searchable, classes)
}

/// The two columns a condition says are equal, where it is `=` between two bare columns of one type.
fn equated(
    plan: &Plan,
    condition: ExprRef,
) -> Option<((ColumnBinding, ExprRef), (ColumnBinding, ExprRef))> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(condition) else {
        return None;
    };
    let (&Expr::Column(one), &Expr::Column(other)) = (plan.expr(left), plan.expr(right)) else {
        return None;
    };
    (plan.expr_type(left) == plan.expr_type(right)).then_some(((one, left), (other, right)))
}

/// Where a column is in the list of columns the equalities read, adding it as its own class if it
/// is not there yet.
fn numbered(
    columns: &mut Vec<(ColumnBinding, ExprRef)>,
    parent: &mut Vec<usize>,
    (binding, expr): (ColumnBinding, ExprRef),
) -> usize {
    if let Some(found) = columns.iter().position(|&(held, _)| held == binding) {
        return found;
    }
    columns.push((binding, expr));
    parent.push(parent.len());
    columns.len() - 1
}

/// The root of a column's class in the union, halving the path on the way.
fn root(parent: &mut [usize], mut at: usize) -> usize {
    while parent[at] != at {
        parent[at] = parent[parent[at]];
        at = parent[at];
    }
    at
}

/// The most leaves a region may have for [`searched`] to look at every order of it.
///
/// Fourteen, which covers all but the four largest JOB queries. The pairs the search scores grow
/// with the shape of the graph rather than with the leaves alone, from a few hundred for a chain of
/// fourteen to fifty thousand for a star of fourteen, and [`PAIRS`] is what bounds a shape this
/// does not expect.
const SEARCHED: usize = 14;

/// The most pairs [`searched`] scores before it gives the region to the greedy search instead.
const PAIRS: usize = 100_000;

/// The cheapest order of the region with no cross product in it, appended to `builds`, with where
/// its top is in the list and what it costs. `None`, with `builds` as it was, where the region has
/// more leaves than [`SEARCHED`], a condition the search cannot place, no order without a cross
/// product, or more pairs than [`PAIRS`].
///
/// This is the search over connected subgraphs and their complements of Moerkotte and Neumann,
/// DPccp. It takes every pair of disjoint connected sets of leaves with a condition between them
/// exactly once, and takes them in an order where both halves of a pair have already been solved,
/// so the best tree of each set is the best of its pairs. Nothing it looks at is a cross product,
/// and nothing a cross product would have led to is looked at. The measure is the one [`cost`] and
/// the greedy step use: the sum of the rows every join produces, with each pair scored by
/// [`estimate::matched_shares`] and each leaf by [`named`] where its filters name its key values.
///
/// Greedy gets the order wrong where the cheapest join to take first is not part of the cheapest
/// tree, which is what a join that is small on its own and large once the rest is joined to it
/// looks like.
fn searched(
    plan: &Plan,
    parts: &[Part],
    pending: &[(ExprRef, TableSet)],
    classes: &[Option<usize>],
    stats: &Facts,
    builds: &mut Vec<Build>,
) -> Option<(usize, u64)> {
    let count = parts.len();
    if count > SEARCHED || pending.len() > 64 {
        return None;
    }
    let reads: Vec<u32> = pending
        .iter()
        .map(|(_, read)| {
            (0..count).filter(|&leaf| read.meets(&parts[leaf].tables)).fold(0, |m, l| m | 1 << l)
        })
        .collect();
    let mut neighbours = vec![0u32; count];
    for &read in &reads {
        // A condition over three leaves or more is placed where all of them are joined, but it does
        // not make any two of them neighbours, since neither pair can test it on its own.
        if read.count_ones() == 2 {
            for (leaf, near) in neighbours.iter_mut().enumerate() {
                if read & 1 << leaf != 0 {
                    *near |= read & !(1 << leaf);
                }
            }
        }
    }
    let mut search = Search {
        plan,
        stats,
        parts,
        pending,
        classes,
        reads,
        neighbours,
        best: vec![None; 1 << count],
        keys: HashMap::new(),
        named: HashMap::new(),
        pairs: 0,
    };
    for (leaf, part) in parts.iter().enumerate() {
        search.best[1 << leaf] = Some(Tree { left: 0, right: 0, side: part.side, cost: 0 });
    }
    for leaf in (0..count).rev() {
        let start = 1u32 << leaf;
        if !search.complements(start) || !search.grow(start, below(leaf)) {
            return None;
        }
    }
    let whole = (1u32 << count) - 1;
    let tree = search.best[whole as usize]?;
    let top = search.place(whole, builds);
    Some((top, tree.cost))
}

/// The leaves numbered `leaf` and below, which is `B_i` in the paper.
fn below(leaf: usize) -> u32 {
    (1u32 << (leaf + 1)) - 1
}

/// The non empty subsets of `set`, smallest first.
fn subsets(set: u32) -> impl Iterator<Item = u32> {
    let mut held = 0u32;
    std::iter::from_fn(move || {
        held = held.wrapping_sub(set) & set;
        (held != 0).then_some(held)
    })
}

/// The best tree [`searched`] has found for a set of leaves so far.
#[derive(Clone, Copy)]
struct Tree {
    /// The two sets it joins, both zero for a leaf.
    left: u32,
    right: u32,
    /// What it produces.
    side: Side,
    /// The sum of the rows its joins produce.
    cost: u64,
}

/// What [`searched`] carries through the enumeration.
struct Search<'a> {
    plan: &'a Plan,
    stats: &'a Facts,
    parts: &'a [Part],
    pending: &'a [(ExprRef, TableSet)],
    /// The equivalence class of columns each condition is an edge of, from [`classed`].
    classes: &'a [Option<usize>],
    /// The leaves each condition reads, as a set.
    reads: Vec<u32>,
    /// The leaves a condition joins each leaf to.
    neighbours: Vec<u32>,
    /// The best tree of each set of leaves, by the set.
    best: Vec<Option<Tree>>,
    /// The key space of each set of conditions, which many pairs share.
    keys: HashMap<u64, Option<u64>>,
    /// The share [`named`] gives a leaf against a set of conditions.
    named: HashMap<(u32, u64), f64>,
    /// How many pairs have been scored.
    pairs: usize,
}

impl Search<'_> {
    /// The leaves a condition joins to `set` that are not in it.
    fn around(&self, set: u32) -> u32 {
        let mut found = 0;
        for (leaf, &next) in self.neighbours.iter().enumerate() {
            if set & 1 << leaf != 0 {
                found |= next;
            }
        }
        found & !set
    }

    /// `EnumerateCsgRec`: every connected set that grows out of `set` without touching `out`.
    fn grow(&mut self, set: u32, out: u32) -> bool {
        let next = self.around(set) & !out;
        if next == 0 {
            return true;
        }
        for more in subsets(next) {
            if !self.complements(set | more) {
                return false;
            }
        }
        subsets(next).all(|more| self.grow(set | more, out | next))
    }

    /// `EmitCsg`: every connected complement of `set` above its lowest leaf, paired with it.
    fn complements(&mut self, set: u32) -> bool {
        let out = set | below(set.trailing_zeros() as usize);
        let next = self.around(set) & !out;
        for leaf in (0..self.parts.len()).rev() {
            if next & 1 << leaf == 0 {
                continue;
            }
            if !self.pair(set, 1 << leaf)
                || !self.extend(set, 1 << leaf, out | (below(leaf) & next))
            {
                return false;
            }
        }
        true
    }

    /// `EnumerateCmpRec`: the complement `other` grown without touching `out`, each one paired.
    fn extend(&mut self, set: u32, other: u32, out: u32) -> bool {
        let next = self.around(other) & !out;
        if next == 0 {
            return true;
        }
        for more in subsets(next) {
            if !self.pair(set, other | more) {
                return false;
            }
        }
        subsets(next).all(|more| self.extend(set, other | more, out | next))
    }

    /// The conditions that become testable when `left` and `right` are joined, with one condition
    /// of each equivalence class rather than all of them, for the reason at [`classed`].
    fn crossing(&self, left: u32, right: u32) -> u64 {
        let both = left | right;
        let mut found = 0u64;
        let mut taken = 0u64;
        for (at, &read) in self.reads.iter().enumerate() {
            if read & !both == 0 && read & !left != 0 && read & !right != 0 {
                if let Some(class) = self.classes[at] {
                    if taken & 1 << class != 0 {
                        continue;
                    }
                    taken |= 1 << class;
                }
                found |= 1 << at;
            }
        }
        found
    }

    /// The conditions in a set, in the order the region held them.
    fn listed(&self, conditions: u64) -> Vec<ExprRef> {
        let held = self.pending.iter().enumerate();
        held.filter(|&(at, _)| conditions & 1 << at != 0).map(|(_, (expr, _))| *expr).collect()
    }

    /// The share of the join one side keeps, as [`named`] gives it for a leaf and as the side's own
    /// share otherwise.
    fn share(&mut self, set: u32, side: Side, conditions: u64) -> f64 {
        if set.count_ones() != 1 {
            return side.share();
        }
        let leaf = set.trailing_zeros();
        if let Some(&held) = self.named.get(&(leaf, conditions)) {
            return held;
        }
        let testable = self.listed(conditions);
        let found = named(self.plan, self.parts[leaf as usize].leaf, side, &testable);
        self.named.insert((leaf, conditions), found);
        found
    }

    /// `EmitCsgCmp`: scores `left` joined to `right` and keeps it where it beats the best tree of
    /// the two together so far. False once the search has scored more than [`PAIRS`].
    fn pair(&mut self, left: u32, right: u32) -> bool {
        self.pairs += 1;
        if self.pairs > PAIRS {
            return false;
        }
        let (Some(one), Some(two)) = (self.best[left as usize], self.best[right as usize]) else {
            return true;
        };
        let conditions = self.crossing(left, right);
        if conditions == 0 {
            return true;
        }
        let keys = match self.keys.get(&conditions) {
            Some(&held) => held,
            None => {
                let found = estimate::keyspace_of(self.plan, &self.listed(conditions), self.stats);
                self.keys.insert(conditions, found);
                found
            }
        };
        let shares =
            (self.share(left, one.side, conditions), self.share(right, two.side, conditions));
        let side = estimate::matched_shares(one.side, two.side, keys, shares);
        let cost = one.cost.saturating_add(two.cost).saturating_add(side.rows);
        let slot = &mut self.best[(left | right) as usize];
        if slot.is_none_or(|held| cost < held.cost) {
            *slot = Some(Tree { left, right, side, cost });
        }
        true
    }

    /// Appends the best tree of `set` to the build list, returning where its top went. A leaf is
    /// already in the list, at its own position.
    fn place(&self, set: u32, builds: &mut Vec<Build>) -> usize {
        if set.count_ones() == 1 {
            return self.parts[set.trailing_zeros() as usize].build;
        }
        let tree = self.best[set as usize].expect("a set the best tree is made of was solved");
        let left = self.place(tree.left, builds);
        let right = self.place(tree.right, builds);
        let conditions = self.listed(self.crossing(tree.left, tree.right));
        builds.push(Build::Pair { left, right, conditions });
        builds.len() - 1
    }
}

/// Puts one entry of the build list and everything under it into the arena.
///
/// Depth first, so both inputs of a join are in the arena before the join is, which is the arena's
/// rule that a node may only point backwards.
fn put(plan: &mut Plan, builds: &[Build], at: usize) -> NodeRef {
    match &builds[at] {
        Build::Leaf(node) => *node,
        Build::Pair { left, right, conditions } => {
            let conditions = conditions.clone();
            let left = put(plan, builds, *left);
            let right = put(plan, builds, *right);
            if conditions.is_empty() {
                return plan.add_node(Node::CrossProduct { left, right });
            }
            let conditions = plan.add_expr_list(&conditions);
            // The build side the pass that chooses one will read as it ends up, which is
            // [`crate::sides`] and runs after this.
            plan.add_node(Node::Join {
                left,
                right,
                kind: JoinKind::Inner,
                conditions,
                build: BuildSide::default(),
            })
        }
    }
}

/// The pair of parts to join next, and how many rows it is estimated to produce.
///
/// A pair with a condition between them beats a pair without one however few rows the second would
/// produce, which is not a tie break but the first thing asked. TPC-H q7 is why. It reads nation
/// twice, once for the supplier's nation and once for the customer's, and the two copies have no
/// condition between them, so the cheapest pair in the region by any row count is those two: the
/// product of twenty five rows with twenty five is six hundred and twenty five, which is fewer than
/// any real join in the query produces. Taking it puts both copies of nation on one side, and the
/// joins after it are then joins to a side that carries a column the condition does not mention, so
/// each of them multiplies by twenty five rather than matching. The query ran out of memory. A cross
/// product is a thing to build when the region leaves no choice, and never because it looked cheap.
///
/// After that, the fewest rows, then the pair whose two inputs are smallest between them, then the
/// pair that came first, which is what makes the choice the same on every run.
fn cheapest(
    plan: &Plan,
    parts: &[Part],
    pending: &[(ExprRef, TableSet)],
    stats: &Facts,
) -> (usize, usize, Side) {
    let mut best: Option<Pick> = None;
    for left in 0..parts.len() {
        for right in left + 1..parts.len() {
            let mut union = parts[left].tables.clone();
            union.extend(&parts[right].tables);
            let testable: Vec<ExprRef> = pending
                .iter()
                .filter(|(_, reads)| reads.is_subset_of(&union))
                .map(|(condition, _)| *condition)
                .collect();
            let linked = !testable.is_empty();
            let (this, that) = (parts[left].side, parts[right].side);
            let side = if linked {
                let keys = estimate::keyspace_of(plan, &testable, stats);
                let shares = (
                    named(plan, parts[left].leaf, this, &testable),
                    named(plan, parts[right].leaf, that, &testable),
                );
                estimate::matched_shares(this, that, keys, shares)
            } else {
                Side {
                    rows: this.rows.saturating_mul(that.rows),
                    base: this.base.saturating_mul(that.base),
                }
            };
            let order = (!linked, side.rows, this.rows.saturating_add(that.rows));
            if best.is_none_or(|held| order < held.order) {
                best = Some(Pick { left, right, side, order });
            }
        }
    }
    let best = best.expect("a region has at least two parts");
    (best.left, best.right, best.side)
}

/// The share of the join a part keeps: what [`estimate::named`] counted on the other side where the
/// part is one leaf whose filters name its key values, and the share of its own rows otherwise.
fn named(plan: &Plan, leaf: Option<NodeRef>, side: Side, testable: &[ExprRef]) -> f64 {
    leaf.and_then(|leaf| estimate::named(plan, leaf, testable))
        .map_or(side.share(), |(share, _)| share)
}

/// One pair [`cheapest`] is considering, with what it would cost and where that puts it.
#[derive(Clone, Copy)]
struct Pick {
    /// The part on the left, by position.
    left: usize,
    /// The part on the right, by position.
    right: usize,
    /// What joining the two is estimated to produce, filtered and unfiltered.
    side: Side,
    /// What the pairs are sorted by: unconnected last, then the rows, then the two inputs together.
    order: (bool, u64, u64),
}

/// What the region already there produces and what it costs, by the measure the search minimises.
///
/// The rows, the sum of the rows of every join in it, and how many of those joins have no condition
/// between their two sides. Scored with the same two rules the search uses rather than with
/// [`crate::estimate`], because the two have to be the same measure for the comparison to mean
/// anything, and because this is the measure the greedy step is minimising one pair at a time.
fn cost(plan: &Plan, at: NodeRef, stats: &Facts) -> Option<(Side, u64, usize)> {
    let (left, right, testable) = match *plan.node(at) {
        Node::CrossProduct { left, right } => (left, right, Vec::new()),
        Node::Join { left, right, kind: JoinKind::Inner, conditions, .. } => {
            (left, right, plan.expr_list(conditions).to_vec())
        }
        _ => return Some((estimate::side(plan, at, stats)?, 0, 0)),
    };
    let linked = !testable.is_empty();
    let leaves = ((!joining(plan, left)).then_some(left), (!joining(plan, right)).then_some(right));
    let (left, under_left, crossed_left) = cost(plan, left, stats)?;
    let (right, under_right, crossed_right) = cost(plan, right, stats)?;
    let side = if linked {
        let keys = estimate::keyspace_of(plan, &testable, stats);
        let shares =
            (named(plan, leaves.0, left, &testable), named(plan, leaves.1, right, &testable));
        estimate::matched_shares(left, right, keys, shares)
    } else {
        Side {
            rows: left.rows.saturating_mul(right.rows),
            base: left.base.saturating_mul(right.base),
        }
    };
    Some((
        side,
        under_left.saturating_add(under_right).saturating_add(side.rows),
        crossed_left + crossed_right + usize::from(!linked),
    ))
}

#[cfg(test)]
mod tests {
    use rudb_common::stat::Provenance;
    use rudb_plan::Plan;

    use crate::estimate::Facts;

    use super::reorder;

    /// What the plan a text prints looks like once the pass has run over it.
    ///
    /// The tables are counted here rather than in each test, because the pass refuses a region with
    /// an uncounted leaf in it and a test that forgot to count one would pass by being refused.
    fn ordered(text: &str) -> String {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 1000), ("u", 10), ("v", 100), ("w", 100_000)] {
            counts.record("memory", "main", table, rows);
        }
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        reorder(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// The same with distinct counts handed in as well, the counts named table then column.
    fn counted(text: &str, columns: &[(&str, &str, u64)]) -> String {
        let mut counts = Facts::new();
        for (table, rows) in [("t", 1000), ("u", 10), ("v", 100), ("w", 100_000)] {
            counts.record("memory", "main", table, rows);
        }
        for (table, column, distinct) in columns {
            counts.record_distinct(
                "memory",
                "main",
                table,
                column,
                *distinct,
                Provenance::Dictionary,
            );
        }
        let mut plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        reorder(&mut plan, &counts);
        plan.validate().unwrap_or_else(|error| panic!("{text} did not stay valid: {error}"));
        plan.to_string()
    }

    /// TPC-H q9's shape. `t` and `v` are the two entries of the `FROM` list that have no condition
    /// between them, and filter pushdown makes them a cross product because they are written next to
    /// each other. Both of them have a condition to `u`, so the cross product is avoidable.
    #[test]
    fn a_cross_product_the_conditions_can_avoid_is_not_built() {
        assert_eq!(
            ordered(concat!(
                "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
                "(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  CrossProduct\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.t AS t #0 [a::BIGINT]\n",
                "  Join INNER on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn an_order_the_search_does_not_improve_on_is_left_exactly_as_it_was() {
        // The two smallest joined first and the largest last, which is what the search would pick,
        // so the plan is kept rather than rebuilt into the same shape with different node numbers.
        let text = concat!(
            "Join INNER on=[(#3.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "  Get memory.main.w AS w #3 [d::BIGINT]\n",
            "  Join INNER on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "    Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn a_region_with_a_leaf_nobody_counted_is_left_alone() {
        // `x` is in no table this test counted, so its side is unknown and there is nothing to
        // compare the cross product against. The cross product stays.
        let text = concat!(
            "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
            "(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  CrossProduct\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.x AS x #2 [c::BIGINT]\n",
            "  Get memory.main.u AS u #1 [b::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn a_condition_that_reads_one_leaf_stops_the_search() {
        // A join condition over a single side is a filter that ended up written as a condition, and
        // placing it is a question about where a filter goes rather than about join order.
        let text = concat!(
            "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
            "(#2.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  CrossProduct\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.v AS v #2 [c::BIGINT]\n",
            "  Get memory.main.u AS u #1 [b::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn an_outer_join_is_not_part_of_a_region() {
        // A left join is neither associative nor commutative with an inner join in the general case,
        // so the region stops at it and the cross product above it has two leaves and nothing to
        // reorder.
        let text = concat!(
            "Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
            "  Join LEFT on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.v AS v #2 [c::BIGINT]\n",
            "  Get memory.main.u AS u #1 [b::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn two_small_parts_with_no_condition_between_them_are_not_joined_to_each_other() {
        // TPC-H q7 and q8 read nation twice and have no condition between the two copies, so the
        // product of the two is the cheapest pair in the region by row count and is the wrong pair
        // by a long way. Here `u` and `v` are the two copies and `t` is what both of them join to.
        // The cross product of `w` and `u` is what lets the search act at all, and the order it
        // builds has to take that one out without putting `u` and `v` together instead. Joining
        // `t` to `v` first and joining it to `u` first score the same, and the search keeps the
        // first of the two it reaches.
        assert_eq!(
            ordered(concat!(
                "Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#3.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
                "(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    CrossProduct\n",
                "      Get memory.main.w AS w #3 [d::BIGINT]\n",
                "      Get memory.main.u AS u #1 [b::BIGINT]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "  Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#3.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.w AS w #3 [d::BIGINT]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
                "    Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.t AS t #0 [a::BIGINT]\n",
                "      Get memory.main.v AS v #2 [c::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_region_with_no_path_between_any_of_it_still_builds_the_smallest_middle() {
        // Nothing here has a condition to anything, so the answer is the product either way and
        // every order builds two cross products. What differs is what is held in between: crossing
        // the hundred thousand with the hundred first makes ten million rows to cross again, and
        // taking the two small ones first makes a thousand. The sum is the measure and it says so.
        assert_eq!(
            ordered(concat!(
                "CrossProduct\n",
                "  CrossProduct\n",
                "    Get memory.main.w AS w #3 [d::BIGINT]\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
            )),
            concat!(
                "CrossProduct\n",
                "  Get memory.main.w AS w #3 [d::BIGINT]\n",
                "  CrossProduct\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn an_order_the_search_reaches_and_does_not_beat_leaves_the_region_alone() {
        // `v` joins to nothing, so one cross product is built however the region is ordered. Greedy
        // takes the linked pair before any unlinked one, so it builds the same shape that is there
        // and scores it the same, and an order that only ties is not an order worth rebuilding for.
        let text = concat!(
            "CrossProduct\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }

    #[test]
    fn a_join_on_a_column_with_two_values_in_it_is_left_until_the_sides_have_been_cut_down() {
        // The q5 shape. `u` joins to `w` on a column with two values in it and `t` joins to `w` on
        // a key. Both pairs read the same under the containment assumption, which puts each of them
        // at the size of `w`, and the tie goes to the pair whose inputs are smaller between them,
        // which is `u` and `w`. That is the wrong one: ten rows against a hundred thousand on two
        // values is half a million rows and not a hundred thousand.
        let text = concat!(
            "Join INNER on=[(#0.0::BIGINT = #3.1::BIGINT)::BOOLEAN]\n",
            "  Join INNER on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "    Get memory.main.w AS w #3 [d::BIGINT, e::BIGINT]\n",
            "  Get memory.main.t AS t #0 [a::BIGINT]\n",
        );
        // Nobody counted anything, so the pass has nothing to say and the region stays as written.
        assert_eq!(ordered(text), text);
        // With the counts the low cardinality pair is scored at what it produces, so greedy joins
        // `t` to `w` on the key first and leaves `u` for last.
        assert_eq!(
            counted(text, &[("u", "b", 2), ("w", "d", 2), ("t", "a", 1000), ("w", "e", 100_000)]),
            concat!(
                "Join INNER on=[(#1.0::BIGINT = #3.0::BIGINT)::BOOLEAN]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
                "  Join INNER on=[(#0.0::BIGINT = #3.1::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.w AS w #3 [d::BIGINT, e::BIGINT]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
            )
        );
    }

    #[test]
    fn a_region_under_something_else_is_reordered_and_what_is_above_it_is_rebuilt() {
        assert_eq!(
            ordered(concat!(
                "Project #4 [#0.0::BIGINT AS a]\n",
                "  Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN, ",
                "(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    CrossProduct\n",
                "      Get memory.main.t AS t #0 [a::BIGINT]\n",
                "      Get memory.main.v AS v #2 [c::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            )),
            concat!(
                "Project #4 [#0.0::BIGINT AS a]\n",
                "  Join INNER on=[(#1.0::BIGINT = #0.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.t AS t #0 [a::BIGINT]\n",
                "    Join INNER on=[(#1.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "      Get memory.main.v AS v #2 [c::BIGINT]\n",
                "      Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn the_join_that_removes_rows_runs_before_the_join_that_removes_none() {
        // TPC-H q9 again, this time about which join goes first rather than about the cross
        // product. `u` is the supplier side, which every row of `w` matches, and the filtered `v`
        // is the part side, which a fifth of them match. Reading the filter through the join is
        // what tells the two apart, because containment alone calls both of them a hundred
        // thousand rows and then the tie breaks on the smaller input, which is `u`.
        assert_eq!(
            ordered(concat!(
                "Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.w AS w #0 [d::BIGINT]\n",
                "    Get memory.main.u AS u #1 [b::BIGINT]\n",
                "  Filter (#2.0::BIGINT = 3::BIGINT)::BOOLEAN\n",
                "    Get memory.main.v AS v #2 [c::BIGINT]\n",
            )),
            concat!(
                "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
                "  Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
                "    Get memory.main.w AS w #0 [d::BIGINT]\n",
                "    Filter (#2.0::BIGINT = 3::BIGINT)::BOOLEAN\n",
                "      Get memory.main.v AS v #2 [c::BIGINT]\n",
                "  Get memory.main.u AS u #1 [b::BIGINT]\n",
            )
        );
    }

    #[test]
    fn with_nothing_filtering_either_side_the_same_region_is_left_as_it_was() {
        // The same three tables with the filter taken off. Neither join removes anything now, so
        // there is nothing to prefer and the order the query was written in stands. This is the
        // half of the previous test that says the new reading is the filter and not the shape.
        let text = concat!(
            "Join INNER on=[(#0.0::BIGINT = #2.0::BIGINT)::BOOLEAN]\n",
            "  Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n",
            "    Get memory.main.w AS w #0 [d::BIGINT]\n",
            "    Get memory.main.u AS u #1 [b::BIGINT]\n",
            "  Get memory.main.v AS v #2 [c::BIGINT]\n",
        );
        assert_eq!(ordered(text), text);
    }
}
