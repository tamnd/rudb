//! Answering a MIN or MAX over an acyclic join without running the join.
//!
//! A query that asks only for the smallest and largest values of some columns across the rows of a
//! join does not need the rows of the join. It needs to know which rows of each relation take part
//! in at least one of them, and then the smallest and largest value of each column among those.
//! MIN and MAX are the two aggregates for which that is enough, because neither cares how many
//! times a value turns up: a value that is in the join once and a value that is in it a million
//! times, once per partner it has in some other relation, are the same value to both of them. A
//! COUNT or a SUM cares exactly about that, which is why neither is rewritten here.
//!
//! Which rows take part is a question a full reducer answers exactly when the join is acyclic, and
//! that is the Yannakakis result this pass leans on. The relations are laid out as a tree in which
//! every class of columns equal to each other is carried along a connected path, and two sweeps of
//! semijoins over the tree, one from the leaves up and one from the root back down, leave each
//! relation holding exactly its rows that have a partner in every other relation. The work is
//! proportional to the relations rather than to the join, and on the Join Order Benchmark the join
//! is what costs: a query whose answer is a handful of MINs reads a few million rows and builds tens
//! of millions of intermediate ones to get there.
//!
//! This pass decides whether an aggregate is one it can answer, and if so writes the tree down as a
//! [`Node::Consistent`] in the aggregate's place. The executor runs the sweeps.
//!
//! # What it takes
//!
//! An ungrouped aggregate whose every output is a MIN or a MAX of a plain column of one relation,
//! over a region of inner joins and cross products whose relations are each a scan with filters on
//! it. Filters and projections of plain columns may sit between the aggregate and the region. Every
//! predicate in the region that reads two relations has to be an equality between an integer
//! column of each, possibly widened by a cast, and every predicate that reads one relation stays on
//! that relation and runs while it is scanned. The join has to be acyclic once equal columns are
//! put into classes, each pair of relations next to each other in the tree has to share exactly one
//! class, and no relation may have two of its own columns in one class.
//!
//! Each of those is a real limit rather than caution for its own sake. The executor keeps a set of
//! integer keys per class, so a key that is a string or a pair of columns is a key it has nowhere
//! to put. A predicate between two relations that is not an equality is not a semijoin, and the
//! sweeps can only apply semijoins. A cycle has no tree, and a relation that joins two of its own
//! columns to each other needs both of them to hold one value on the same row, which a set per
//! class does not remember.
//!
//! # What it declines, and where it says so
//!
//! Anything else, and the aggregate is left exactly as it was. The reason is written down with
//! [`Plan::note_declined`] when the aggregate sat over a join, which is when somebody reading the
//! plan might have expected the rewrite, and `EXPLAIN` prints it under the tree. An aggregate over a
//! single table says nothing, since there is no join to save and a note there would be on every
//! plan with a `count(*)` in it.
//!
//! # Why the relations are narrowed
//!
//! Each relation is written as a fresh scan of just the columns this reads, which are its keys, the
//! columns an extreme is taken from and the columns its filters read, with the filters copied onto
//! it. The rewrite runs before column pruning, so the scans in the region still read every column of
//! their tables, and the relations here are held by the node rather than being its children, so
//! pruning would never reach them. Doing it here is doing it once, in the one place that knows which
//! columns are read.

use std::collections::{BTreeSet, HashMap};

use rudb_common::rules::Rule;
use rudb_common::{Field, LogicalType, Result};
use rudb_plan::{
    ColumnBinding, CompareOp, ConjunctionOp, Edge, Expr, ExprRef, Extreme, JoinKind, Key, Leaf,
    Node, NodeRef, Plan, Reducer,
};

use crate::pass::{Context, Pass};
use crate::walk;

/// Rewrites an ungrouped MIN and MAX over an acyclic equi-join into a [`Node::Consistent`].
#[derive(Debug, Clone, Copy)]
pub struct ConsistentExtremes;

impl Pass for ConsistentExtremes {
    fn name(&self) -> &'static str {
        "consistent_extremes"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if !context.allows(Rule::Consistent) {
            return Ok(());
        }
        let mut changed = false;
        let root = plan.root();
        let mut next = walk::fresh_index(plan);
        let rewritten = walk::restack(plan, root, &mut changed, &mut |plan, at| {
            match rewrite(plan, at, &mut next) {
                Ok(done) => Some(done),
                Err(Declined::Silently) => None,
                Err(Declined::Because(reason)) => {
                    let index = match *plan.node(at) {
                        Node::Aggregate { index, .. } => index,
                        _ => 0,
                    };
                    plan.note_declined(format!("aggregate #{index} {reason}"));
                    None
                }
            }
        });
        if changed {
            plan.set_root(rewritten);
        }
        Ok(())
    }
}

/// Why an aggregate was left alone.
#[derive(Debug)]
enum Declined {
    /// It is not over a join, so there was nothing to decline and nothing is written down.
    Silently,
    /// It is over a join and this is what stopped the rewrite.
    Because(String),
}

/// Shorthand for a reason worth writing down.
fn because<T>(reason: impl Into<String>) -> std::result::Result<T, Declined> {
    Err(Declined::Because(reason.into()))
}

/// One relation of the region while the tree is being worked out.
struct Relation {
    /// The scan at the bottom of it.
    get: NodeRef,
    /// The table index of that scan, which is what the columns of the relation are bound to.
    index: u32,
    /// Every predicate that reads only this relation, as the plan holds it.
    local: Vec<ExprRef>,
}

/// A column of one relation, as a position in the list of relations and a position in its scan.
type Place = (usize, u32);

/// The node that stands in for the aggregate at `at`, or why there is none.
fn rewrite(
    plan: &mut Plan,
    at: NodeRef,
    next: &mut u32,
) -> std::result::Result<NodeRef, Declined> {
    let Node::Aggregate { input, index, groups, aggregates } = *plan.node(at) else {
        return Err(Declined::Silently);
    };

    // Down through the filters and projections to the region. Everything above it is carried
    // along, the filters as predicates and the projections as a map from what they produce to what
    // they read, since both have to be rewritten in terms of the relations in the end.
    let mut above: Vec<ExprRef> = Vec::new();
    let mut projections: HashMap<u32, NodeRef> = HashMap::new();
    let mut region = input;
    loop {
        match *plan.node(region) {
            Node::Filter { input, predicate } => {
                conjuncts(plan, predicate, &mut above);
                region = input;
            }
            Node::Project { input, index, .. } => {
                projections.insert(index, region);
                region = input;
            }
            _ => break,
        }
    }
    if !is_join(plan.node(region)) {
        return Err(Declined::Silently);
    }
    if !groups.is_empty() {
        return because("groups by a key, so its answer is per group rather than one extreme");
    }

    // The relations and the predicates of the region.
    let mut bottoms = Vec::new();
    let mut conditions = above;
    gather(plan, region, &mut bottoms, &mut conditions)?;
    let mut relations = Vec::with_capacity(bottoms.len());
    for &bottom in &bottoms {
        relations.push(relation(plan, bottom)?);
    }
    let by_index: HashMap<u32, usize> =
        relations.iter().enumerate().map(|(at, relation)| (relation.index, at)).collect();
    let find = Resolver { projections: &projections, by_index: &by_index };

    // The extremes, each a plain column of one relation with the type the aggregate returns.
    let mut extremes = Vec::new();
    for &aggregate in plan.expr_list(aggregates) {
        let Expr::Aggregate { name, args, filter, .. } = *plan.expr(aggregate) else {
            return because("holds something that is not an aggregate call");
        };
        let called = plan.string(name).to_ascii_lowercase();
        let max = match called.as_str() {
            "min" => false,
            "max" => true,
            _ => return because(format!("computes {called}, which is not a MIN or a MAX")),
        };
        if filter.is_some() {
            return because(format!("has a FILTER on a {called}"));
        }
        let [argument] = plan.expr_list(args) else {
            return because(format!("calls {called} with other than one argument"));
        };
        let Some(place) = find.plain(plan, *argument) else {
            return because(format!("takes a {called} of something other than a column"));
        };
        if field(plan, &relations, place).ty != *plan.expr_type(aggregate) {
            return because(format!("takes a {called} whose type is not its column's"));
        }
        extremes.push((place, max, called));
    }

    // Every predicate on one relation or joining two. Anything else is refused.
    let mut equalities: Vec<(Place, Place)> = Vec::new();
    for &condition in &conditions {
        let mut read: BTreeSet<usize> = BTreeSet::new();
        let mut outside = false;
        walk::columns(plan, condition, &mut |binding| match find.place(plan, binding) {
            Some((relation, _)) => {
                read.insert(relation);
            }
            None => outside = true,
        });
        if outside {
            return because("has a predicate that reads a column from outside the join");
        }
        match read.len() {
            // A predicate that reads nothing is a constant, and a constant filter on the whole
            // join is the same filter on any one relation of it, since the join is empty exactly
            // when one relation is.
            0 => relations[0].local.push(condition),
            1 => relations[*read.first().unwrap_or(&0)].local.push(condition),
            _ => {
                let Some(pair) = equality(plan, &find, &relations, condition) else {
                    return because(
                        "has a predicate between relations that is not an equality of integer columns",
                    );
                };
                equalities.push(pair);
            }
        }
    }

    // The classes of equal columns. Only the columns an equality names are in one.
    let mut classes = Classes::default();
    for &(one, other) in &equalities {
        classes.union(one, other);
    }
    let (class_of, count) = classes.numbered();
    let mut edges: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); relations.len()];
    for (&(relation, _), &class) in &class_of {
        if !edges[relation].insert(class) {
            return because("joins two columns of one relation to each other");
        }
    }

    let parents = match gyo(&edges) {
        Ok(parents) => parents,
        Err(reason) => return because(reason),
    };

    // A root per tree, the relation most of the extremes are read from, since a root is the one
    // relation whose rows the second sweep never has to go back over.
    let mut wanted = vec![0usize; relations.len()];
    for &((relation, _), _, _) in &extremes {
        wanted[relation] += 1;
    }
    let order = rooted(&parents, &wanted);

    // Each relation as a fresh scan of what is read of it, with its own filters over it.
    let mut position = vec![0usize; relations.len()];
    for (at, &(relation, _)) in order.iter().enumerate() {
        position[relation] = at;
    }
    let mut read: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); relations.len()];
    for &(relation, column) in class_of.keys() {
        read[relation].insert(column);
    }
    for &((relation, column), _, _) in &extremes {
        read[relation].insert(column);
    }
    for (at, relation) in relations.iter().enumerate() {
        for &predicate in &relation.local {
            walk::columns(plan, predicate, &mut |binding| {
                if let Some((_, column)) = find.place(plan, binding) {
                    read[at].insert(column);
                }
            });
        }
    }
    let mut narrowed: Vec<(u32, HashMap<u32, u32>)> = Vec::with_capacity(relations.len());
    let mut inputs = Vec::with_capacity(relations.len());
    for (at, relation) in relations.iter().enumerate() {
        let fresh = *next;
        *next += 1;
        let moved: HashMap<u32, u32> =
            read[at].iter().enumerate().map(|(new, &old)| (old, count_u32(new))).collect();
        let input = narrow(plan, relation, fresh, &read[at], &moved, &find);
        narrowed.push((fresh, moved));
        inputs.push(input);
    }

    let mut leaves = Vec::with_capacity(relations.len());
    for &(relation, parent) in &order {
        let moved = &narrowed[relation].1;
        let mut keys: Vec<Key> = class_of
            .iter()
            .filter(|((held, _), _)| *held == relation)
            .map(|(&(_, column), &class)| Key { class, column: moved[&column] })
            .collect();
        keys.sort_by_key(|key| key.class);
        let parent = parent.map(|parent| {
            let shared = edges[relation]
                .intersection(&edges[parent])
                .next()
                .copied()
                .expect("the tree only joins relations that share a class");
            Edge { leaf: count_u32(position[parent]), class: shared }
        });
        leaves.push(Leaf { input: inputs[relation], keys, parent });
    }
    let produced: Vec<Extreme> = extremes
        .iter()
        .map(|&((relation, column), max, _)| Extreme {
            leaf: count_u32(position[relation]),
            column: narrowed[relation].1[&column],
            max,
        })
        .collect();
    let fields: Vec<Field> = extremes
        .iter()
        .zip(plan.expr_list(aggregates).to_vec())
        .map(|((_, _, called), aggregate)| Field {
            name: called.clone(),
            ty: plan.expr_type(aggregate).clone(),
            not_null: false,
        })
        .collect();
    let reducer = Reducer { leaves, classes: count, extremes: produced };
    if reducer.validate().is_err() {
        return because("built a join tree that does not hold together, which is a bug");
    }
    let reducer = plan.add_reducer(reducer);
    let columns = plan.add_fields(&fields);
    let span = plan.node_span(at);
    Ok(plan.add_node_at(Node::Consistent { index, columns, reducer }, span))
}

/// Whether a node is a join of any kind, which is what makes an aggregate over it a candidate.
fn is_join(node: &Node) -> bool {
    matches!(
        node,
        Node::Join { .. }
            | Node::CrossProduct { .. }
            | Node::LinkJoin { .. }
            | Node::DependentJoin { .. }
    )
}

/// The relations and the predicates of a region of inner joins and cross products.
///
/// A join of any other kind inside it is refused rather than treated as a relation, because an
/// outer join keeps rows that match nothing and a semi or anti join is a filter the sweeps would
/// have to know about, and neither is a scan.
fn gather(
    plan: &Plan,
    at: NodeRef,
    bottoms: &mut Vec<NodeRef>,
    conditions: &mut Vec<ExprRef>,
) -> std::result::Result<(), Declined> {
    match *plan.node(at) {
        Node::CrossProduct { left, right } => {
            gather(plan, left, bottoms, conditions)?;
            gather(plan, right, bottoms, conditions)
        }
        Node::Join { left, right, kind: JoinKind::Inner, conditions: list, .. } => {
            for &condition in plan.expr_list(list) {
                conjuncts(plan, condition, conditions);
            }
            gather(plan, left, bottoms, conditions)?;
            gather(plan, right, bottoms, conditions)
        }
        Node::Join { .. } | Node::DependentJoin { .. } | Node::LinkJoin { .. } => {
            because("is over a join that is not an inner join")
        }
        _ => {
            bottoms.push(at);
            Ok(())
        }
    }
}

/// Splits a predicate into the conjuncts of its top level `AND`.
fn conjuncts(plan: &Plan, predicate: ExprRef, into: &mut Vec<ExprRef>) {
    match *plan.expr(predicate) {
        Expr::Conjunction { op: ConjunctionOp::And, children } => {
            for &child in plan.expr_list(children) {
                conjuncts(plan, child, into);
            }
        }
        _ => into.push(predicate),
    }
}

/// A relation of the region, which has to be a scan of a table under nothing but filters.
fn relation(plan: &Plan, bottom: NodeRef) -> std::result::Result<Relation, Declined> {
    let mut local = Vec::new();
    let mut at = bottom;
    loop {
        match *plan.node(at) {
            Node::Filter { input, predicate } => {
                conjuncts(plan, predicate, &mut local);
                at = input;
            }
            Node::Get { index, .. } => return Ok(Relation { get: at, index, local }),
            ref other => {
                return because(format!(
                    "joins a relation that is a {} rather than a table scan",
                    other.keyword()
                ));
            }
        }
    }
}

/// The field of the scan a column of a relation is read from.
fn field<'a>(plan: &'a Plan, relations: &[Relation], (relation, column): Place) -> &'a Field {
    let Node::Get { columns, .. } = *plan.node(relations[relation].get) else {
        unreachable!("a relation is a scan, which `relation` checked")
    };
    &plan.field_list(columns)[column as usize]
}

/// Where the columns above the region come from.
struct Resolver<'a> {
    /// The projections between the aggregate and the region, by the index they bind to.
    projections: &'a HashMap<u32, NodeRef>,
    /// The relations, by the index of their scans.
    by_index: &'a HashMap<u32, usize>,
}

impl Resolver<'_> {
    /// The column of a relation a binding reads, through the projections of plain columns.
    fn place(&self, plan: &Plan, binding: ColumnBinding) -> Option<Place> {
        if let Some(&relation) = self.by_index.get(&binding.table) {
            return Some((relation, binding.column));
        }
        let &projection = self.projections.get(&binding.table)?;
        let Node::Project { exprs, .. } = *plan.node(projection) else { return None };
        let &expr = plan.expr_list(exprs).get(binding.column as usize)?;
        self.plain(plan, expr)
    }

    /// The column an expression is, if it is nothing but a column.
    fn plain(&self, plan: &Plan, expr: ExprRef) -> Option<Place> {
        match *plan.expr(expr) {
            Expr::Column(binding) => self.place(plan, binding),
            _ => None,
        }
    }
}

/// The two columns an equality joins, when it is one this can use.
///
/// Both sides have to be a column of a different relation, each possibly under a cast that widens
/// one integer type into another, and both columns have to be signed integers, which fit in the
/// signed 64 bit value the executor keys its sets by. `IS NOT DISTINCT FROM` is refused, because
/// it matches a null to a null and the sweeps drop every null key.
fn equality(
    plan: &Plan,
    find: &Resolver<'_>,
    relations: &[Relation],
    condition: ExprRef,
) -> Option<(Place, Place)> {
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(condition) else {
        return None;
    };
    let one = key(plan, find, relations, left)?;
    let other = key(plan, find, relations, right)?;
    (one.0 != other.0).then_some((one, other))
}

/// One side of a join equality, as the column it reads.
fn key(plan: &Plan, find: &Resolver<'_>, relations: &[Relation], side: ExprRef) -> Option<Place> {
    let place = match *plan.expr(side) {
        Expr::Cast { input, try_cast: false } => {
            let place = find.plain(plan, input)?;
            if !widens(&field(plan, relations, place).ty, plan.expr_type(side)) {
                return None;
            }
            place
        }
        _ => find.plain(plan, side)?,
    };
    keyed(&field(plan, relations, place).ty).then_some(place)
}

/// Whether a column of this type can be a key of the executor's sets.
///
/// Only the signed integers are, because the executor reads keys through the readers that widen a
/// signed column into an `i64`, and those do not take an unsigned one. An unsigned key is rare
/// enough in a join that declining it costs nothing worth the extra reader.
fn keyed(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer | LogicalType::BigInt
    )
}

/// Whether every value of `from` is a value of `to`, so that a cast between them changes nothing
/// an equality could see.
fn widens(from: &LogicalType, to: &LogicalType) -> bool {
    /// The range of an integer type, as the lowest and highest value it holds.
    fn range(ty: &LogicalType) -> Option<(i128, i128)> {
        Some(match ty {
            LogicalType::TinyInt => (i128::from(i8::MIN), i128::from(i8::MAX)),
            LogicalType::SmallInt => (i128::from(i16::MIN), i128::from(i16::MAX)),
            LogicalType::Integer => (i128::from(i32::MIN), i128::from(i32::MAX)),
            LogicalType::BigInt => (i128::from(i64::MIN), i128::from(i64::MAX)),
            LogicalType::UTinyInt => (0, i128::from(u8::MAX)),
            LogicalType::USmallInt => (0, i128::from(u16::MAX)),
            LogicalType::UInteger => (0, i128::from(u32::MAX)),
            LogicalType::UBigInt => (0, i128::from(u64::MAX)),
            _ => return None,
        })
    }
    match (range(from), range(to)) {
        (Some((low, high)), Some((floor, ceiling))) => floor <= low && high <= ceiling,
        _ => false,
    }
}

/// A position in a list that came from a plan, which has fewer than `u32::MAX` of anything.
fn count_u32(at: usize) -> u32 {
    u32::try_from(at).expect("a plan has fewer than u32::MAX of anything")
}

/// Columns put into classes by the equalities between them, as a union find.
#[derive(Default)]
struct Classes {
    /// The number each column has in `parent`.
    numbers: HashMap<Place, usize>,
    /// Each column's parent, a column that is its own being the representative of its class.
    parent: Vec<usize>,
    /// The columns in the order they were first seen, so the class numbers do not depend on the
    /// order a hash map walks in.
    seen: Vec<Place>,
}

impl Classes {
    /// The number of a column, giving it one if it has none.
    fn number(&mut self, place: Place) -> usize {
        if let Some(&number) = self.numbers.get(&place) {
            return number;
        }
        let number = self.parent.len();
        self.parent.push(number);
        self.numbers.insert(place, number);
        self.seen.push(place);
        number
    }

    /// The representative of a column's class.
    fn root(&mut self, mut number: usize) -> usize {
        while self.parent[number] != number {
            self.parent[number] = self.parent[self.parent[number]];
            number = self.parent[number];
        }
        number
    }

    /// Puts two columns in one class.
    fn union(&mut self, one: Place, other: Place) {
        let one = self.number(one);
        let other = self.number(other);
        let (one, other) = (self.root(one), self.root(other));
        if one != other {
            self.parent[one.max(other)] = one.min(other);
        }
    }

    /// Each column's class, numbered from zero in the order the classes were first seen, and how
    /// many classes there are.
    fn numbered(mut self) -> (std::collections::BTreeMap<Place, u32>, u32) {
        let mut class_of = std::collections::BTreeMap::new();
        let mut numbers: HashMap<usize, u32> = HashMap::new();
        for place in self.seen.clone() {
            let number = self.numbers[&place];
            let root = self.root(number);
            let fresh = count_u32(numbers.len());
            let class = *numbers.entry(root).or_insert(fresh);
            class_of.insert(place, class);
        }
        (class_of, count_u32(numbers.len()))
    }
}

/// The join tree, by ear removal, as each relation's neighbour on the way to a root.
///
/// GYO reduction over the hypergraph whose edges are the relations and whose vertices are the
/// classes. A relation is an ear when every class it shares with a relation still in the graph is
/// held by one single other relation, which becomes its parent, and a relation that shares nothing
/// with any relation left is the root of a tree of its own. The graph is acyclic exactly when this
/// removes every relation.
///
/// Each relation has to share exactly one class with its parent. Two would be a composite key, and
/// the executor keys a set by one integer.
fn gyo(edges: &[BTreeSet<u32>]) -> std::result::Result<Vec<Option<usize>>, String> {
    let mut parents: Vec<Option<usize>> = vec![None; edges.len()];
    let mut left: Vec<usize> = (0..edges.len()).collect();
    while !left.is_empty() {
        let mut removed = None;
        for (slot, &ear) in left.iter().enumerate() {
            let shared: BTreeSet<u32> = edges[ear]
                .iter()
                .copied()
                .filter(|class| left.iter().any(|&other| other != ear && edges[other].contains(class)))
                .collect();
            if shared.is_empty() {
                removed = Some((slot, None));
                break;
            }
            let holder = left
                .iter()
                .copied()
                .find(|&other| other != ear && shared.is_subset(&edges[other]));
            if let Some(holder) = holder {
                if shared.len() > 1 {
                    return Err("joins two relations on more than one column".to_owned());
                }
                removed = Some((slot, Some(holder)));
                break;
            }
        }
        let Some((slot, parent)) = removed else {
            return Err("is over a join with a cycle in it".to_owned());
        };
        let ear = left.remove(slot);
        parents[ear] = parent;
    }
    Ok(parents)
}

/// The relations in the order the executor scans them, each with its parent, after rooting every
/// tree at the relation the most extremes are read from.
///
/// The tree ear removal finds is rooted wherever the last relation happened to be, and a tree has
/// no direction as far as the sweeps are concerned, so it is turned around to hang from the root
/// that is worth the most. The order is children before parents, which is the promise
/// [`Reducer::leaves`] makes.
fn rooted(parents: &[Option<usize>], wanted: &[usize]) -> Vec<(usize, Option<usize>)> {
    let count = parents.len();
    let mut next_to: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (child, parent) in parents.iter().enumerate() {
        if let Some(parent) = *parent {
            next_to[child].push(parent);
            next_to[parent].push(child);
        }
    }
    let mut placed = vec![false; count];
    let mut order = Vec::with_capacity(count);
    for start in 0..count {
        if placed[start] {
            continue;
        }
        // Every relation of this tree, then the one to root it at.
        let mut tree = vec![start];
        placed[start] = true;
        let mut at = 0;
        while at < tree.len() {
            for &other in &next_to[tree[at]] {
                if !placed[other] {
                    placed[other] = true;
                    tree.push(other);
                }
            }
            at += 1;
        }
        let root = tree
            .iter()
            .copied()
            .max_by_key(|&relation| (wanted[relation], std::cmp::Reverse(relation)))
            .unwrap_or(start);
        // Depth first from the root, writing a relation down after everything under it.
        let mut stack: Vec<(usize, Option<usize>, bool)> = vec![(root, None, false)];
        while let Some((relation, parent, expanded)) = stack.pop() {
            if expanded {
                order.push((relation, parent));
                continue;
            }
            stack.push((relation, parent, true));
            for &other in next_to[relation].iter().rev() {
                if Some(other) != parent {
                    stack.push((other, Some(relation), false));
                }
            }
        }
    }
    order
}

/// A relation written as a fresh scan of the columns in `read`, with its filters over it.
fn narrow(
    plan: &mut Plan,
    relation: &Relation,
    fresh: u32,
    read: &BTreeSet<u32>,
    moved: &HashMap<u32, u32>,
    find: &Resolver<'_>,
) -> NodeRef {
    let Node::Get { catalog, schema, table, alias, columns, .. } = *plan.node(relation.get) else {
        unreachable!("a relation is a scan, which `relation` checked")
    };
    let kept: Vec<Field> =
        read.iter().map(|&column| plan.field_list(columns)[column as usize].clone()).collect();
    let kept = plan.add_fields(&kept);
    let span = plan.node_span(relation.get);
    let scan = plan.add_node_at(
        Node::Get { catalog, schema, table, alias, index: fresh, columns: kept },
        span,
    );
    if relation.local.is_empty() {
        return scan;
    }
    let predicates: Vec<ExprRef> = relation
        .local
        .iter()
        .map(|&predicate| rebound(plan, predicate, fresh, moved, find))
        .collect();
    let predicate = match predicates.as_slice() {
        [one] => *one,
        _ => {
            let span = plan.expr_span(relation.local[0]);
            let children = plan.add_expr_list(&predicates);
            plan.add_expr_at(
                Expr::Conjunction { op: ConjunctionOp::And, children },
                LogicalType::Boolean,
                span,
            )
        }
    };
    plan.add_node(Node::Filter { input: scan, predicate })
}

/// A copy of a predicate that reads the narrowed scan instead of whatever it read before.
fn rebound(
    plan: &mut Plan,
    expr: ExprRef,
    fresh: u32,
    moved: &HashMap<u32, u32>,
    find: &Resolver<'_>,
) -> ExprRef {
    if let Expr::Column(binding) = *plan.expr(expr) {
        let (_, column) = find.place(plan, binding).expect("the predicate was checked to read this");
        let ty = plan.expr_type(expr).clone();
        let span = plan.expr_span(expr);
        return plan.add_expr_at(
            Expr::Column(ColumnBinding::new(fresh, moved[&column])),
            ty,
            span,
        );
    }
    walk::rebuild(plan, expr, &mut |plan, child| rebound(plan, child, fresh, moved, find))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{ConsistentExtremes, gyo, rooted, widens};
    use crate::pass::{Context, Pass};
    use rudb_common::LogicalType;
    use rudb_common::rules::{Rule, Rules};
    use rudb_plan::Plan;

    /// Two tables joined on one integer column each, with an aggregate of `aggregates` over them.
    fn joined(aggregates: &str, groups: &str, join: &str) -> Plan {
        let text = format!(
            "Aggregate #3 groups=[{groups}] aggregates=[{aggregates}]\n  \
             Join {join}\n    \
             Get memory.main.t AS t #0 [id::INTEGER, name::VARCHAR, n::INTEGER]\n    \
             Get memory.main.u AS u #1 [t_id::INTEGER, note::VARCHAR]\n"
        );
        Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    const EQUAL: &str = "INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]";

    /// The plan after the pass with the rules as a fresh database has them, and the reasons it
    /// wrote down.
    fn rewritten(mut plan: Plan) -> (String, Vec<String>) {
        ConsistentExtremes.run(&mut plan, &Context::new()).expect("the pass does not fail");
        (plan.to_string(), plan.declined().to_vec())
    }

    #[test]
    fn a_min_and_a_max_over_an_equality_join_become_one_node() {
        let plan = joined("min(#0.1::VARCHAR)::VARCHAR, max(#1.1::VARCHAR)::VARCHAR", "", EQUAL);
        let (text, declined) = rewritten(plan);
        assert!(text.starts_with("Consistent #3"), "{text}");
        assert!(!text.contains("Join"), "{text}");
        assert!(declined.is_empty(), "{declined:?}");
    }

    #[test]
    fn the_switch_keeps_the_join() {
        let mut plan = joined("min(#0.1::VARCHAR)::VARCHAR", "", EQUAL);
        let mut rules = Rules::new();
        rules.set(Rule::Consistent, false);
        let mut context = Context::new();
        context.govern(rules);
        ConsistentExtremes.run(&mut plan, &context).expect("the pass does not fail");
        assert!(plan.to_string().contains("Join INNER"), "{plan}");
    }

    #[test]
    fn every_other_shape_is_declined_with_its_reason() {
        let cases = [
            ("count_star()::BIGINT", "", EQUAL, "not a MIN or a MAX"),
            ("sum(#0.2::INTEGER)::HUGEINT", "", EQUAL, "not a MIN or a MAX"),
            ("min(#0.2::INTEGER)::INTEGER, count_star()::BIGINT", "", EQUAL, "not a MIN or a MAX"),
            ("string_agg(#0.1::VARCHAR, ','::VARCHAR)::VARCHAR", "", EQUAL, "not a MIN or a MAX"),
            ("min(#0.2::INTEGER)::INTEGER", "#1.1::VARCHAR", EQUAL, "groups by a key"),
            (
                "min(#0.2::INTEGER)::INTEGER",
                "",
                "LEFT on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]",
                "not an inner join",
            ),
            (
                "min(#0.2::INTEGER)::INTEGER",
                "",
                "INNER on=[(#0.0::INTEGER < #1.0::INTEGER)::BOOLEAN]",
                "not an equality",
            ),
            (
                "min(#0.2::INTEGER)::INTEGER",
                "",
                "INNER on=[(#0.1::VARCHAR = #1.1::VARCHAR)::BOOLEAN]",
                "not an equality",
            ),
        ];
        for (aggregates, groups, join, reason) in cases {
            let (text, declined) = rewritten(joined(aggregates, groups, join));
            assert!(!text.contains("Consistent"), "{aggregates} {groups} {join}: {text}");
            assert!(
                declined.iter().any(|written| written.contains(reason)),
                "{aggregates} {groups} {join}: {declined:?}"
            );
        }
    }

    #[test]
    fn a_cyclic_join_is_declined() {
        let text = "Aggregate #3 groups=[] aggregates=[min(#0.1::INTEGER)::INTEGER]\n  \
             Filter (((#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN AND (#1.1::INTEGER = #2.0::INTEGER)::BOOLEAN)::BOOLEAN AND (#2.1::INTEGER = #0.1::INTEGER)::BOOLEAN)::BOOLEAN\n    \
             CrossProduct\n      \
             CrossProduct\n        \
             Get memory.main.a AS a #0 [x::INTEGER, y::INTEGER]\n        \
             Get memory.main.b AS b #1 [x::INTEGER, y::INTEGER]\n      \
             Get memory.main.c AS c #2 [x::INTEGER, y::INTEGER]\n";
        let plan = Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let (text, declined) = rewritten(plan);
        assert!(!text.contains("Consistent"), "{text}");
        assert!(declined.iter().any(|written| written.contains("cycle")), "{declined:?}");
    }

    fn edges(list: &[&[u32]]) -> Vec<BTreeSet<u32>> {
        list.iter().map(|classes| classes.iter().copied().collect()).collect()
    }

    #[test]
    fn a_star_is_one_tree_with_the_points_under_the_middle() {
        // Which relation ends up the root is whichever is left last, and the middle can be an ear
        // of the last point as well as the other way round. Either is a join tree, and the root is
        // chosen again afterwards anyway, so what matters is one root and the points on the middle.
        let parents = gyo(&edges(&[&[0], &[0, 1, 2], &[1], &[2]])).expect("a star is acyclic");
        assert_eq!(parents.iter().filter(|parent| parent.is_none()).count(), 1, "{parents:?}");
        assert_eq!(parents[0], Some(1), "{parents:?}");
        assert_eq!(parents[2], Some(1), "{parents:?}");
    }

    #[test]
    fn a_triangle_is_a_cycle() {
        let found = gyo(&edges(&[&[0, 1], &[1, 2], &[2, 0]]));
        assert!(found.is_err_and(|reason| reason.contains("cycle")));
    }

    #[test]
    fn one_class_shared_by_many_is_not_a_cycle() {
        assert!(gyo(&edges(&[&[0], &[0], &[0], &[0]])).is_ok());
    }

    #[test]
    fn two_relations_sharing_two_classes_are_a_composite_key() {
        let found = gyo(&edges(&[&[0, 1], &[0, 1]]));
        assert!(found.is_err_and(|reason| reason.contains("more than one")));
    }

    #[test]
    fn relations_that_share_nothing_are_each_a_tree() {
        let parents = gyo(&edges(&[&[0], &[1], &[]])).expect("a forest is acyclic");
        assert_eq!(parents, [None, None, None]);
    }

    #[test]
    fn the_root_is_the_relation_most_extremes_are_read_from_and_comes_last() {
        let parents = gyo(&edges(&[&[0], &[0, 1], &[1]])).expect("a line is acyclic");
        let order = rooted(&parents, &[0, 0, 2]);
        assert_eq!(order.last(), Some(&(2, None)));
        let position = |relation: usize| order.iter().position(|&(at, _)| at == relation);
        for &(relation, parent) in &order {
            if let Some(parent) = parent {
                assert!(position(relation) < position(parent), "{order:?}");
            }
        }
    }

    #[test]
    fn only_a_cast_that_keeps_every_value_is_a_key() {
        assert!(widens(&LogicalType::Integer, &LogicalType::BigInt));
        assert!(widens(&LogicalType::UInteger, &LogicalType::BigInt));
        assert!(!widens(&LogicalType::BigInt, &LogicalType::Integer));
        assert!(!widens(&LogicalType::Integer, &LogicalType::UInteger));
        assert!(!widens(&LogicalType::Integer, &LogicalType::Double));
    }
}
