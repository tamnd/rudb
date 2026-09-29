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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rudb_common::bounds::{Bound, Reach, Zones};
use rudb_common::rules::Rule;
use rudb_common::{Field, LogicalType, Result};
use rudb_plan::{
    ColumnBinding, CompareOp, ConjunctionOp, Edge, Expr, ExprRef, Extreme, JoinKind, Key, Leaf,
    Node, NodeRef, Plan, Reducer,
};

use crate::estimate::{self, Facts};
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
        crate::eliminate::inner_joins(plan, context);
        let mut changed = false;
        let root = plan.root();
        let mut next = walk::fresh_index(plan);
        let rewritten = walk::restack(plan, root, &mut changed, &mut |plan, at| match rewrite(
            plan,
            at,
            &mut next,
            context.facts(),
            context.allows(Rule::GraphReduction),
            context.allows(Rule::Skew),
            context.allows(Rule::JoinElimination),
        ) {
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
    /// The top of the filters over the scan, or the scan when there are none.
    top: NodeRef,
}

/// A column of one relation, as a position in the list of relations and a position in its scan.
type Place = (usize, u32);

/// The node that stands in for the aggregate at `at`, or why there is none.
fn rewrite(
    plan: &mut Plan,
    at: NodeRef,
    next: &mut u32,
    facts: &Facts,
    gathering: bool,
    skewing: bool,
    dropping: bool,
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

    // What is read of each relation, which is its keys, the columns its extremes are read from and
    // the columns its own filters read.
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

    // A relation that only joins, with no filter and nothing read of it, and that every other
    // relation of its class finds exactly once, keeps every row of the rest. So it goes.
    let (relations, class_of, edges, read, extremes) = if dropping {
        without_parents(plan, relations, class_of, edges, read, extremes)
    } else {
        (relations, class_of, edges, read, extremes)
    };

    // The key values a relation of one class names by its filters, see [`picked`].
    let picks: Vec<Option<Picked>> =
        (0..relations.len()).map(|at| picked(plan, &relations, &class_of, &edges, at)).collect();

    // The tree and the order the relations are scanned in, cheapest first. See [`gyo`].
    let weights: Vec<Weight> = (0..relations.len())
        .map(|at| {
            let relation = &relations[at];
            let rows = estimate::rows(plan, relation.get, facts).unwrap_or(UNMEASURED);
            let width = read[at]
                .iter()
                .filter(|&&column| !class_of.contains_key(&(at, column)))
                .map(|&column| if keyed(&field(plan, &relations, (at, column)).ty) { 1 } else { 4 })
                .sum();
            let kept = if relation.local.is_empty() {
                1.0
            } else {
                let after = estimate::rows(plan, relation.top, facts).unwrap_or(rows);
                (after as f64 / rows.max(1) as f64).clamp(0.0, 0.999)
            };
            let kept = match &picks[at] {
                Some(picked) if picked.whole => {
                    (picked.rows as f64 / rows.max(1) as f64).clamp(0.0, 0.999)
                }
                _ => kept,
            };
            let zones = plan.zones(relation.index);
            let columns = |zones: &dyn Zones| {
                class_of
                    .iter()
                    .filter(|&(&(held, _), _)| held == at)
                    .filter_map(|(&place, &class)| {
                        Some((class, zones.column(&field(plan, &relations, place).name)?))
                    })
                    .collect::<Vec<_>>()
            };
            let reach = zones
                .map(|zones| {
                    columns(zones.as_ref())
                        .into_iter()
                        .filter_map(|(class, column)| Some((class, zones.reach(column)?)))
                        .collect()
                })
                .unwrap_or_default();
            let gathered: Vec<u32> = zones
                .filter(|_| gathering)
                .map(|zones| {
                    columns(zones.as_ref())
                        .into_iter()
                        .filter(|&(_, column)| zones.gathers(column))
                        .map(|(class, _)| class)
                        .collect()
                })
                .unwrap_or_default();
            let skew: Vec<(u32, f64, u64)> = plan
                .frequencies(relation.index)
                .filter(|_| skewing)
                .map(|frequencies| {
                    class_of
                        .iter()
                        .filter(|&(&(held, _), _)| held == at)
                        .filter_map(|(&place, &class)| {
                            let column =
                                frequencies.column(&field(plan, &relations, place).name)?;
                            let (skew, values) = frequencies.skew(column)?;
                            Some((class, skew, values))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let named = plan
                .frequencies(relation.index)
                .map(|frequencies| {
                    class_of
                        .iter()
                        .filter(|&(&(held, _), _)| held == at)
                        .filter_map(|(&place, &class)| {
                            let picked = picks.iter().enumerate().find_map(|(other, picked)| {
                                picked
                                    .as_ref()
                                    .filter(|picked| other != at && picked.class == class)
                            })?;
                            let column =
                                frequencies.column(&field(plan, &relations, place).name)?;
                            let counted: Option<u64> = picked
                                .values
                                .iter()
                                .map(|value| {
                                    frequencies.rows_with(column, value).exact_value().copied()
                                })
                                .sum();
                            let stored = zones.and_then(|zones| {
                                Some((zones, zones.column(&field(plan, &relations, place).name)?))
                            });
                            let spans = stored
                                .and_then(|(zones, column)| zones.spans(column, &picked.values));
                            let share = match (counted, spans) {
                                (Some(held), _) => held as f64 / frequencies.rows().max(1) as f64,
                                // Not all of them listed. Where the table is laid out by the column
                                // the ends of its parts say where the values are, and where it is
                                // not a sample says how many rows they hold.
                                (None, Some((parts, rows))) if parts < 0.5 => rows,
                                (None, _) => {
                                    let (zones, column) = stored?;
                                    zones.holding(column, &picked.values)?
                                }
                            };
                            let values = u64::try_from(picked.values.len()).ok()?;
                            let parts = spans.map(|(parts, _)| parts);
                            Some((class, values, share.min(1.0), parts))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let placed = zones
                .map(|zones| {
                    columns(zones.as_ref())
                        .into_iter()
                        .filter(|(class, _)| gathered.contains(class))
                        .filter_map(|(class, column)| {
                            let &(_, _, values) = skew.iter().find(|(held, ..)| *held == class)?;
                            Some((class, zones.placed(column)? / values.max(1) as f64))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Weight { rows, width, kept, reach, gathered, skew, domain: Vec::new(), named, placed }
        })
        .collect();
    let weights = with_domains(weights);
    let order = match gyo(&edges, &weights) {
        Ok(order) => trail(&edges, &weights, order),
        Err(reason) => return because(reason),
    };

    // Each relation as a fresh scan of what is read of it, with its own filters over it.
    let mut position = vec![0usize; relations.len()];
    for (at, &(relation, _)) in order.iter().enumerate() {
        position[relation] = at;
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

/// The relations left once every relation the others only look up is taken out.
///
/// A relation goes when it has no filter, nothing is read of it past its key, it holds one class,
/// and every other relation in that class holds a column the store linked to its key and found a
/// parent for in every row. Then each of their rows has exactly one partner in it, so the join to it
/// neither drops a row nor repeats one. The classes are already built, so an equality that ran
/// through it still joins the others.
///
/// This matters for the tree more than for the scan it saves. In JOB 26b `name` is joined on
/// `cast_info.person_id` alone, and while it is in the graph `cast_info` has three neighbours and
/// is not an ear until `char_name` is gone, so `char_name` was read whole with its `LIKE` and only
/// then `cast_info` at the six movies left. Without `name`, `cast_info` goes first and `char_name`
/// is read at the roles it kept.
///
/// One at a time, so two relations that are each other's parent cannot both go.
#[expect(clippy::type_complexity, reason = "the five parallel lists the rewrite keeps")]
fn without_parents(
    plan: &Plan,
    relations: Vec<Relation>,
    class_of: BTreeMap<Place, u32>,
    edges: Vec<BTreeSet<u32>>,
    read: Vec<BTreeSet<u32>>,
    extremes: Vec<(Place, bool, String)>,
) -> (
    Vec<Relation>,
    BTreeMap<Place, u32>,
    Vec<BTreeSet<u32>>,
    Vec<BTreeSet<u32>>,
    Vec<(Place, bool, String)>,
) {
    let mut gone = vec![false; relations.len()];
    for at in 0..relations.len() {
        let relation = &relations[at];
        let [class] = edges[at].iter().copied().collect::<Vec<_>>()[..] else { continue };
        if !relation.local.is_empty()
            || read[at].len() != 1
            || extremes.iter().any(|&((held, _), ..)| held == at)
        {
            continue;
        }
        let Node::Get { table, .. } = *plan.node(relation.get) else { continue };
        let name = plan.string(table);
        let Some(zones) = plan.zones(relation.index) else { continue };
        let Some(generation) = zones.generation() else { continue };
        let Some(&key) = read[at].first() else { continue };
        let Some(key) = zones.column(&field(plan, &relations, (at, key)).name) else { continue };
        let children: Vec<Place> = class_of
            .iter()
            .filter(|&(&(held, _), &of)| of == class && held != at && !gone[held])
            .map(|(&place, _)| place)
            .collect();
        let found = |&(child, column): &Place| {
            let zones = plan.zones(relations[child].index)?;
            let column = zones.column(&field(plan, &relations, (child, column)).name)?;
            let (parent, parent_column, stamp) = zones.total_link(column)?;
            Some(parent.eq_ignore_ascii_case(name) && parent_column == key && stamp == generation)
        };
        if !children.is_empty() && children.iter().all(|child| found(child) == Some(true)) {
            gone[at] = true;
        }
    }
    if !gone.contains(&true) {
        return (relations, class_of, edges, read, extremes);
    }
    let mut moved = vec![usize::MAX; relations.len()];
    let mut next = 0;
    for (at, &went) in gone.iter().enumerate() {
        if !went {
            moved[at] = next;
            next += 1;
        }
    }
    let keep = |list: Vec<BTreeSet<u32>>| {
        list.into_iter().enumerate().filter(|&(at, _)| !gone[at]).map(|(_, set)| set).collect()
    };
    let class_of = class_of
        .into_iter()
        .filter(|&((held, _), _)| !gone[held])
        .map(|((held, column), class)| ((moved[held], column), class))
        .collect();
    let extremes = extremes
        .into_iter()
        .map(|((held, column), max, called)| ((moved[held], column), max, called))
        .collect();
    let relations = relations
        .into_iter()
        .enumerate()
        .filter(|&(at, _)| !gone[at])
        .map(|(_, one)| one)
        .collect();
    (relations, class_of, keep(edges), keep(read), extremes)
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
fn relation(plan: &Plan, top: NodeRef) -> std::result::Result<Relation, Declined> {
    let mut local = Vec::new();
    let mut at = top;
    loop {
        match *plan.node(at) {
            Node::Filter { input, predicate } => {
                conjuncts(plan, predicate, &mut local);
                at = input;
            }
            Node::Get { index, .. } => return Ok(Relation { get: at, index, local, top }),
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
    fn numbered(mut self) -> (BTreeMap<Place, u32>, u32) {
        let mut class_of = BTreeMap::new();
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

/// How many rows a relation with no estimate is taken to have, which is more than any JOB table so
/// that a relation nothing is known about is scanned after the ones that are known.
const UNMEASURED: u64 = 1 << 40;

/// The values of its one key a relation's filters keep, as [`picked`] finds them.
#[derive(Debug)]
struct Picked {
    /// The class of the key.
    class: u32,
    /// The distinct values of it in the rows kept.
    values: Vec<Bound>,
    /// The rows kept, which is the values with repeats.
    rows: usize,
    /// Whether every filter of the relation was run, so that `rows` is what they keep together
    /// rather than what one of them keeps.
    whole: bool,
}

/// The values of its key the filters of the relation `at` keep, where it joins on one class and the
/// store could run a filter of it over the whole table and it keeps no more than `NAMED` rows.
///
/// A dimension table a query names a value of. The share of it the filter keeps says how many of its
/// values are kept and nothing about how many rows of a fact table they are, and that is the number
/// the order turns on. `k.keyword = 'character-name-in-title'` in JOB 17c keeps one keyword in
/// 134,170, the share and the skew of `movie_keyword` made it 8,600 rows of `movie_keyword`, and it
/// is 41,840, because it is the most common keyword there is. With the value in hand the fact
/// table's frequency synopsis counts them, see [`Weight::named`]. Where several filters could run,
/// the values are those of the rows every one of them keeps.
fn picked(
    plan: &Plan,
    relations: &[Relation],
    class_of: &BTreeMap<Place, u32>,
    edges: &[BTreeSet<u32>],
    at: usize,
) -> Option<Picked> {
    let relation = &relations[at];
    if edges[at].len() != 1 || relation.local.is_empty() {
        return None;
    }
    let (&place, &class) = class_of.iter().find(|&(&(held, _), _)| held == at)?;
    let key = field(plan, relations, place).name.clone();
    let most = NAMED as usize;
    let mut kept: Option<Vec<Bound>> = None;
    let mut whole = true;
    for &predicate in &relation.local {
        let Some(values) = estimate::picked(plan, relation.get, predicate, &key, most) else {
            whole = false;
            continue;
        };
        kept = Some(match kept {
            // Two conditions keep the rows both keep, and a value is only one of them where both
            // kept a row of it. As values rather than rows this can keep a value twice over two rows
            // that each passed only one condition, which is the side a ceiling errs on.
            Some(held) => held.into_iter().filter(|value| values.contains(value)).collect(),
            None => values,
        });
    }
    let kept = kept?;
    let rows = kept.len();
    let mut values: Vec<Bound> = Vec::with_capacity(rows);
    for value in kept {
        if !values.contains(&value) {
            values.push(value);
        }
    }
    // Where more than one filter ran, the rows are only a ceiling on the rows kept together.
    let whole = whole && relation.local.len() == 1;
    Some(Picked { class, values, rows, whole })
}

/// What [`gyo`] knows of one relation to order it by.
#[derive(Debug, Clone)]
struct Weight {
    /// The rows of the whole table.
    rows: u64,
    /// What reading a row of it costs past its keys, a unit an integer column and four a string.
    width: u64,
    /// The share of its rows its own filters are estimated to keep, one when it has none.
    kept: f64,
    /// How the values of each of its key columns sit across its parts, by class, where the store
    /// could say.
    reach: Vec<(u32, Reach)>,
    /// The classes whose column the store can find a set of values' rows of without reading the
    /// rest, see [`Zones::gathers`]. Empty unless the graph reduction rule is on.
    gathered: Vec<u32>,
    /// How many times more rows a kept value of each key column reaches than the average value, and
    /// how many values the column has, by class, where the store counted. See
    /// [`Frequencies::skew`].
    ///
    /// [`Frequencies::skew`]: rudb_common::bounds::Frequencies::skew
    skew: Vec<(u32, f64, u64)>,
    /// How many values each of its classes has, the most any relation that holds it counted, where
    /// the store could say. See [`Weight::local`].
    domain: Vec<(u32, u64)>,
    /// The share of its rows the values of each class a dimension table's filters name reach, with
    /// how many values they are, counted by the store where it could, and the share of its parts
    /// whose ends hold one of them. See [`picked`].
    named: Vec<(u32, u64, f64, Option<f64>)>,
    /// How many parts one value's rows are in, by class, for the classes it is gathered at where
    /// the store measured. See [`Zones::placed`].
    ///
    /// [`Zones::placed`]: rudb_common::bounds::Zones::placed
    placed: Vec<(u32, f64)>,
}

/// The weights with each one's `domain` filled in from what every relation counted.
fn with_domains(mut weights: Vec<Weight>) -> Vec<Weight> {
    let mut domain: BTreeMap<u32, u64> = BTreeMap::new();
    for weight in &weights {
        let counted = weight.reach.iter().map(|(class, reach)| (*class, reach.values));
        let counted = counted.chain(weight.skew.iter().map(|&(class, _, values)| (class, values)));
        for (class, values) in counted {
            let most = domain.entry(class).or_insert(0);
            *most = (*most).max(values);
        }
    }
    let domain: Vec<(u32, u64)> = domain.into_iter().filter(|&(_, values)| values > 0).collect();
    for weight in &mut weights {
        weight.domain.clone_from(&domain);
    }
    weights
}

impl Weight {
    /// What the scan costs once the relations before it left `standing` of each class's values.
    ///
    /// The key column is read in every part the handed keys do not rule out, and the rest at the
    /// rows they keep. See `Scan::read_deferring` in `rudb-exec`. The parts left are the fewest any
    /// one class leaves, since a part is skipped when any set of keys rules it out: in JOB 6a the
    /// eleven movies `title` keeps fall inside a few hundred of the 4,425 parts of `cast_info` and
    /// the people `name` keeps fall inside nearly all of them.
    ///
    /// A table of one chunk or less is read whole whatever keys it is handed, so where it goes in
    /// the order does not change what it costs. Priced by the keys, `role_type` in JOB 29a cost a
    /// few units less read last, the order put it there, and `cast_info` was read at 1,687 roles
    /// where `role_type` read first leaves 11.
    #[expect(clippy::cast_precision_loss, reason = "a count of rows is a weight here")]
    fn cost(&self, standing: &Standing, classes: &BTreeSet<u32>) -> f64 {
        if self.rows <= CHUNK {
            return self.rows as f64 * (1 + self.width) as f64;
        }
        let fed = self.fed(standing, classes);
        let touched = self
            .reach
            .iter()
            .filter_map(|(class, reach)| {
                let share = *standing.get(class)?;
                // The named values' own parts, where the ends of the parts said, rather than those
                // of the average value.
                if let Some(spanned) = self.spanned(*class, share) {
                    return Some(spanned);
                }
                let share = self.local(*class, share);
                Some(reach.touched(share * reach.values as f64))
            })
            .fold(1.0, f64::min);
        let scanned = self.rows as f64 * touched * (1.0 + self.width as f64 * fed);
        // Read at the rows the narrowed classes' values are in, which the executor does when each
        // is under one row in `GATHERED`, see `listed_keys` in `rudb-exec`, and at the rows all of
        // them hand down together. A row found that way is found in its part, and the part is
        // decoded to read it, so what a gather costs is the parts its rows fall in, each at the
        // share of a scan of it that [`DECODE`] says, and a little per row on top. In JOB 17c the 41,840 movies of one keyword were 975,229
        // rows of `cast_info` in every one of its 4,425 parts, and gathering them cost more than
        // reading the table. Priced by the rows alone it was a sixth of that, and it put the one
        // read of `name` that narrows `cast_info` to 250 rows after it.
        let shares = self
            .gathered
            .iter()
            .filter_map(|class| Some((*class, *standing.get(class)?)))
            .filter(|&(_, share)| share * GATHERED < 1.0)
            .map(|(class, share)| (self.reached(class, share), class));
        let Some((_, narrowest)) = shares.clone().min_by(|one, other| one.0.total_cmp(&other.0))
        else {
            return scanned;
        };
        let rows = self.rows as f64;
        let found = rows * backoff(shares.map(|(share, _)| share));
        let parts = (rows / PART).ceil().max(1.0);
        // No more parts than the kept values' own reach puts them in, where the table is laid out
        // by the key and the rows of a value sit together.
        let decoded = (1.0 - (1.0 - 1.0 / parts).powf(found))
            .min(self.grouped(narrowest, found, touched, parts))
            .min(touched);
        scanned.min(rows * decoded * DECODE * (1.0 + self.width as f64) + found * GATHER)
    }

    /// The share of its parts the rows `found` at the values of `class` are in, taking the rows of
    /// one value to sit together in as many parts as the store measured, and only in the `touched`
    /// share of the parts the other classes leave. One where the store did not measure.
    ///
    /// A gather at rows spread over the table at random decodes a part a row. In JOB 14b the 41
    /// rows of `movie_info` the order reads last are the countries of a few movies, and a movie's
    /// rows of one type sit together, so they were in 6 of its 1,812 parts where a part a row made
    /// it 125, and the order read `movie_info` whole first instead.
    #[expect(clippy::cast_precision_loss, reason = "counts of rows and values are weights here")]
    fn grouped(&self, class: u32, found: f64, touched: f64, parts: f64) -> f64 {
        let Some(&(_, spread)) = self.placed.iter().find(|(held, _)| *held == class) else {
            return 1.0;
        };
        let Some(&(_, _, values)) = self.skew.iter().find(|(held, ..)| *held == class) else {
            return 1.0;
        };
        let groups = found * values as f64 / self.rows.max(1) as f64;
        (groups * (spread * touched).max(1.0) / parts).min(1.0)
    }

    /// The share of its rows the relations before it leave.
    ///
    /// The fewest any one class leaves, times the square root of the next fewest, times the fourth
    /// root of the one after, and so on. The fewest alone takes the classes to keep the same rows,
    /// and a relation that narrows a second class then narrows nothing: in JOB 21a `movie_keyword`
    /// leaves ten thousand movies, `movie_link` two thousand, together they leave 116, and with the
    /// fewest alone the order saw no reason to read `movie_link` before `movie_info`. The product
    /// takes the classes to be unrelated, which a fact table's keys seldom are. The roots are the
    /// usual way between the two.
    fn fed(&self, standing: &Standing, classes: &BTreeSet<u32>) -> f64 {
        backoff(
            classes.iter().filter_map(|&class| Some(self.reached(class, *standing.get(&class)?))),
        )
    }

    /// The share of its rows `share` of the values of `class` reach.
    ///
    /// Not the same share. The values a filter keeps are the ones people ask about and those hold
    /// more rows than the average one, so in JOB 6d the eight keywords reach 35,548 rows of
    /// `movie_keyword` and not the 270 their share of the keywords would give. Taking that for a
    /// few hundred sent the order through `cast_info` at movies it expected to be a handful and
    /// were eleven thousand.
    ///
    /// The ratio holds for the few values a query names. A filter that keeps thousands keeps them
    /// by something other than their size, as the country of a company in JOB 13a, and those hold
    /// about their share. So the ratio counts in full up to `NAMED` values and fades past it.
    #[expect(clippy::cast_precision_loss, reason = "a count of values is a weight here")]
    fn reached(&self, class: u32, share: f64) -> f64 {
        if let Some(reached) = self.counted(class, share) {
            return reached;
        }
        let share = self.local(class, share);
        let Some(&(_, skew, values)) = self.skew.iter().find(|(held, ..)| *held == class) else {
            return share.min(1.0);
        };
        let kept = share * values as f64;
        let named = if kept > NAMED { NAMED / kept } else { 1.0 };
        (share * (1.0 + (skew - 1.0) * named)).min(1.0)
    }

    /// The share of its rows `share` of the values of `class` reach, where they are the values a
    /// dimension table named and the store counted them. See [`Weight::named`].
    ///
    /// Fewer values than the named ones, where a relation read since has thrown some away, reach
    /// their share of the counted rows.
    #[expect(clippy::cast_precision_loss, reason = "a count of values is a weight here")]
    fn counted(&self, class: u32, share: f64) -> Option<f64> {
        let &(_, values, reached, _) = self.named.iter().find(|(held, ..)| *held == class)?;
        let &(_, domain) = self.domain.iter().find(|(held, _)| *held == class)?;
        let named = values as f64 / domain.max(1) as f64;
        if named <= 0.0 {
            return None;
        }
        Some((reached * (share / named).min(1.0)).min(1.0))
    }

    /// The share of its parts `share` of the values of `class` are in, where they are the values a
    /// dimension table named and the ends of the parts said where those are. See [`Zones::spans`].
    ///
    /// [`Zones::spans`]: rudb_common::bounds::Zones::spans
    #[expect(clippy::cast_precision_loss, reason = "a count of values is a weight here")]
    fn spanned(&self, class: u32, share: f64) -> Option<f64> {
        let &(_, values, _, parts) = self.named.iter().find(|(held, ..)| *held == class)?;
        let parts = parts?;
        let &(_, domain) = self.domain.iter().find(|(held, _)| *held == class)?;
        let named = values as f64 / domain.max(1) as f64;
        (named > 0.0).then(|| parts * (share / named).min(1.0))
    }

    /// The share of its own values of `class` that `share` of the class's values keeps.
    ///
    /// The same share when the values kept are spread over the class. Not when a query names them:
    /// a value a query names is in the table it filters, and a fact table holds only some of the
    /// values of a small dimension. In JOB 13a `info_type` keeps `rating`, one type of 113, and
    /// `movie_info_idx` holds five types, so the one kept is a fifth of its types and a third of
    /// its rows, not one percent of them. Taken as one percent, `movie_info_idx` read first left a
    /// hundredth of the movies, the German companies could not narrow them further, and the order
    /// read them last. As with the skew ratio this counts in full up to `NAMED` kept values and
    /// fades past it, where the values kept are the ones some other relation left and are spread.
    #[expect(clippy::cast_precision_loss, reason = "a count of values is a weight here")]
    fn local(&self, class: u32, share: f64) -> f64 {
        let Some(&(_, domain)) = self.domain.iter().find(|(held, _)| *held == class) else {
            return share;
        };
        let counted =
            self.skew.iter().find(|(held, ..)| *held == class).map(|&(.., values)| values);
        let reach = self.reach.iter().find(|(held, _)| *held == class);
        let own = counted.or_else(|| reach.map(|(_, reach)| reach.values));
        let Some(own) = own.filter(|&own| own > 0 && own < domain) else {
            return share;
        };
        let kept = share * domain as f64;
        let named = if kept > NAMED { NAMED / kept } else { 1.0 };
        (share * (1.0 + (domain as f64 / own as f64 - 1.0) * named)).min(1.0)
    }
}

/// The fewest of `shares` times the square root of the next fewest, times the fourth root of the
/// one after, and so on. See [`Weight::fed`].
///
/// Only the fewest [`COUNTED`] count, and the roots are square roots taken again, since the cost
/// model runs this for every step of every order it weighs and a `powf` and a vector each time
/// were a fifth of planning JOB 29a.
fn backoff(shares: impl IntoIterator<Item = f64>) -> f64 {
    let mut fewest = [1.0_f64; COUNTED];
    let mut held = 0;
    for share in shares {
        if held < COUNTED {
            fewest[held] = share;
            held += 1;
        } else if let Some(most) = fewest.iter_mut().max_by(|one, other| one.total_cmp(other))
            && share < *most
        {
            *most = share;
        }
    }
    let fewest = &mut fewest[..held];
    fewest.sort_unstable_by(f64::total_cmp);
    let mut kept = 1.0;
    for (at, &share) in fewest.iter().enumerate() {
        let mut root = share;
        for _ in 0..at {
            root = root.sqrt();
        }
        kept *= root;
    }
    kept
}

/// How many of a relation's classes count in [`backoff`]. The root the next would be taken to is
/// within a hundredth of a percent of one for any share over a billionth.
const COUNTED: usize = 16;

/// What is left standing of each class, as the share of its values.
type Standing = BTreeMap<u32, f64>;

/// What decoding a part for a gather costs, as a share of scanning it. A gather decodes the columns
/// it reads of the part and tests no filter over it, and on the IMDb load a part of `cast_info` took
/// about 18 us to decode for a gather against about 200 us to scan. Priced at the whole part, the
/// gather of 6b at the 23 movies of `title` cost as much as reading `cast_info` whole, and the plan
/// read `name` with its pattern first. Across the JOB queries whose plans this moves, 0.3 ran faster
/// than both 1 and 0.1, where 0.1 made gathers so cheap that 17c took `cast_info` early again.
const DECODE: f64 = 0.3;

/// The rows of one part of a native table, which a row gathered out of it decodes.
const PART: f64 = 8192.0;

/// The rows of one chunk, under which a table is read in one piece.
const CHUNK: u64 = 2048;

/// How many kept values a query can be taken to have named, past which the values a filter keeps
/// are taken to be as big as any others. JOB names at most a few dozen: the keywords of 6d are
/// eight and the longest `IN` list is a few more.
const NAMED: f64 = 64.0;

/// The share of a table's rows under which the executor reads a relation at the rows its kept keys
/// reach rather than testing every row. The same number as `GATHERED` in `rudb-exec`.
const GATHERED: f64 = 64.0;

/// What gathering one row costs past decoding the part it is in, in the units of [`Weight::cost`]
/// where reading an integer column of a row is one. The gather runs on one thread at about 30 ns a
/// row on the IMDb load, and a scan reads an integer column of a row in about two across the
/// workers.
const GATHER: f64 = 16.0;

/// The join tree, by ear removal, as the relations in the order they are scanned, each with its
/// parent.
///
/// GYO reduction over the hypergraph whose edges are the relations and whose vertices are the
/// classes. A relation is an ear when every class it shares with a relation still in the graph is
/// held by one single other relation, which becomes its parent, and a relation that shares nothing
/// with any relation left is the root of a tree of its own. The graph is acyclic exactly when this
/// removes every relation, and it does whichever ear is taken at each step.
///
/// So the ears are taken in the order that costs least in all, see [`search`], and the order the
/// ears come off in is the order the executor scans the relations in, children before parents. A
/// join too wide to search takes at each step the ear that costs least with the rest of the order
/// taken greedily after it, for the few cheapest ears, see [`LOOKAHEAD`]. Every relation that
/// finishes hands the keys it kept to the next relation in each of its classes, whose scan tests
/// its key column against them and reads the rest only at the rows that pass. So what an ear costs
/// depends on what was taken before it: the share of each class's values still standing is carried
/// along as relations are taken, and an ear is charged its key column in full and the rest of its
/// row at the smallest share among its classes. In JOB 24b `title` is filtered to four movies, so
/// `cast_info` is charged little more than its movie column and goes before `name`, which is then
/// read at the three people left rather than whole.
///
/// An ear with no filter of its own and no class a relation already taken holds waits until no
/// other ear is left, whatever it costs, because read then it is read whole and narrows nothing.
/// In JOB 24b that is `char_name`, read at the one role left rather than all three million.
///
/// A parent is the cheapest relation left that holds what the ear shares, by the cost of reading it
/// whole. Any holder gives a tree that answers the same.
///
/// Each relation has to share exactly one class with its parent. Two would be a composite key, and
/// the executor keys a set by one integer.
fn gyo(
    edges: &[BTreeSet<u32>],
    weights: &[Weight],
) -> std::result::Result<Vec<(usize, Option<usize>)>, String> {
    let mut order = Vec::with_capacity(edges.len());
    let mut left: Vec<usize> = (0..edges.len()).collect();
    let whole = |relation: usize| weights[relation].cost(&Standing::new(), &edges[relation]);
    left.sort_by(|&one, &other| whole(one).total_cmp(&whole(other)).then(one.cmp(&other)));
    if edges.len() > 64 {
        return Err("joins more than 64 relations".to_owned());
    }
    // For each class of each relation, the relations that hold it, as bits.
    let holders: Vec<Vec<u64>> = edges
        .iter()
        .map(|classes| {
            classes
                .iter()
                .map(|class| {
                    edges
                        .iter()
                        .enumerate()
                        .filter(|(_, held)| held.contains(class))
                        .fold(0, |mask, (relation, _)| mask | 1 << relation)
                })
                .collect()
        })
        .collect();
    let searched = search(edges, weights, &holders, &left);
    if let Some((_, found, false)) = searched {
        return Ok(found);
    }
    // The share of each class's values the relations taken so far have left standing.
    let mut standing: Standing = Standing::new();
    while !left.is_empty() {
        let (mut ears, waiting) = ears(edges, weights, &holders, &left, &standing)?;
        // The cheapest few ears, each priced with the rest of the order taken greedily after it.
        // One step alone does not see what an ear leaves the others: in JOB 13a `company_name`
        // costs more to read than the movie side, and each time the order took the movie side
        // first, until the German companies came last and narrowed nothing.
        if left.len() > 2 && ears.len() > 1 {
            ears.sort_by(|one, other| one.0.total_cmp(&other.0).then(one.1.cmp(&other.1)));
            ears.truncate(LOOKAHEAD);
            for ear in &mut ears {
                let mut rest = left.clone();
                let taken = rest.remove(ear.1);
                let mut after = standing.clone();
                take(edges, weights, &mut after, taken);
                ear.0 += finish(edges, weights, &holders, rest, after);
            }
        }
        let best = ears
            .iter()
            .copied()
            .reduce(|best, ear| if ear.0 < best.0 { ear } else { best })
            .map(|(_, slot, parent)| (slot, parent));
        let Some((slot, parent)) = best.or(waiting) else {
            return Err("is over a join with a cycle in it".to_owned());
        };
        let ear = left.remove(slot);
        take(edges, weights, &mut standing, ear);
        order.push((ear, parent));
    }
    if let Some((cost, found, _)) = searched
        && cost < priced(edges, weights, &order)
    {
        return Ok(found);
    }
    Ok(order)
}

/// The cheapest order of ears, by the cheapest way to have taken each set of relations.
///
/// What an ear costs depends on what was taken before it only through what is left standing of
/// each class, so the orders that reach one set are compared by what they cost, and only the
/// cheapest goes on. That is not exact, since a dearer way to a set can leave less standing, but it
/// sees a step that pays for itself two steps later, which the greedy order does not. In JOB 13a
/// that is `company_name` and then `movie_companies`: dear to read, and together they leave two
/// percent of the movies for `movie_info`, `title` and `movie_info_idx`.
///
/// A layer that grows past [`STATES`] keeps the cheapest of its sets, and the order that comes out
/// says so, since it is then only a good order and [`gyo`] weighs it against the greedy one. JOB 29a
/// joins seventeen relations and every subset of its dimension tables is a set, and searching them
/// all took a hundred milliseconds to plan a query that runs in sixty. `None` when no order takes
/// every relation, where the greedy order says why.
fn search(
    edges: &[BTreeSet<u32>],
    weights: &[Weight],
    holders: &[Vec<u64>],
    ranked: &[usize],
) -> Option<(f64, Vec<(usize, Option<usize>)>, bool)> {
    type Reached = (f64, Standing, Vec<(usize, Option<usize>)>);
    let mut place = vec![0; edges.len()];
    for (at, &relation) in ranked.iter().enumerate() {
        place[relation] = at;
    }
    let rank = |relation: usize| place[relation];
    let mut layer: BTreeMap<u64, Reached> =
        BTreeMap::from([(0, (0.0, Standing::new(), Vec::new()))]);
    let mut beamed = false;
    for _ in 0..edges.len() {
        // The cheapest way to each set one more ear reaches, as the set it grew from and the ear.
        // What is left standing and the order are only built for the sets the beam keeps: in JOB
        // 29a building them for every set reached took most of its planning.
        let mut next: BTreeMap<u64, (f64, u64, usize, Option<usize>)> = BTreeMap::new();
        for (&taken, (cost, standing, order)) in &layer {
            let (found, waiting) = steps(ranked, weights, holders, taken)?;
            let found = if found.is_empty() { waiting.into_iter().collect() } else { found };
            for (ear, parent) in found {
                let total = cost + weights[ear].cost(standing, &edges[ear]);
                let key = taken | 1 << ear;
                // Orders that cost the same, as a sum taken in another order can differ in its
                // last bits, go to the one that takes the cheaper relations first, which is what
                // the greedy order does.
                if next.get(&key).is_some_and(|&(least, from, held, _)| {
                    let tie = (total - least).abs() <= least.abs() * 1e-9;
                    let kept = layer[&from].2.iter().map(|&(one, _)| one).chain([held]);
                    let longer = order.iter().map(|&(one, _)| one).chain([ear]);
                    (total > least && !tie) || (tie && kept.map(rank).le(longer.map(rank)))
                }) {
                    continue;
                }
                next.insert(key, (total, taken, ear, parent));
            }
        }
        // Too many sets to carry them all, so only the cheapest go on. That is no longer the
        // cheapest order for certain, and [`gyo`] weighs it against the greedy one.
        if next.len() > STATES {
            beamed = true;
            let mut costs: Vec<f64> = next.values().map(|(cost, ..)| *cost).collect();
            costs.sort_by(f64::total_cmp);
            let most = costs[STATES - 1];
            let mut kept = 0;
            next.retain(|_, (cost, ..)| {
                kept += 1;
                *cost < most || (*cost == most && kept <= STATES)
            });
        }
        layer = next
            .into_iter()
            .map(|(key, (total, from, ear, parent))| {
                let (_, standing, order) = &layer[&from];
                let mut longer = order.clone();
                longer.push((ear, parent));
                let mut after = standing.clone();
                take(edges, weights, &mut after, ear);
                (key, (total, after, longer))
            })
            .collect();
    }
    layer.into_values().next().map(|(cost, _, order)| (cost, order, beamed))
}

/// The ears left once the relations in `taken` are, with a set of relations written as bits, and
/// the first ear that waits. `None` for a relation that shares two classes with the rest.
///
/// Planning JOB 29a spent most of its time building the sets [`ears`] builds for each set of
/// relations the search reaches, and each one follows from which relations hold each class.
fn steps(
    ranked: &[usize],
    weights: &[Weight],
    holders: &[Vec<u64>],
    taken: u64,
) -> Option<(Vec<(usize, Option<usize>)>, Option<(usize, Option<usize>)>)> {
    let mut found = Vec::new();
    let mut waiting = None;
    let left = ranked.iter().fold(0, |mask, &relation| mask | 1 << relation) & !taken;
    for &ear in ranked {
        let bit = 1 << ear;
        if left & bit == 0 {
            continue;
        }
        let others = left & !bit;
        let (mut shared, mut common) = (0, others);
        let mut narrowed = weights[ear].kept < 1.0;
        for &holding in &holders[ear] {
            if holding & others != 0 {
                shared += 1;
                common &= holding;
            }
            narrowed |= holding & taken != 0;
        }
        let parent = if shared == 0 {
            None
        } else {
            match ranked.iter().copied().find(|&other| common & 1 << other != 0) {
                Some(holder) => Some(holder),
                None => continue,
            }
        };
        if shared > 1 {
            return None;
        }
        if narrowed {
            found.push((ear, parent));
        } else {
            waiting = waiting.or(Some((ear, parent)));
        }
    }
    Some((found, waiting))
}

/// The order with the leaves of its root read after the root, where that costs less.
///
/// Ears come off children first, so the relations around the dearest one, which the order leaves
/// for last, are all read before it but one. In JOB 26a that is `cast_info`, which the movies
/// narrow to 37,085 rows, with `name` and `char_name` hanging off it. `name` has no filter and is
/// the root, and `char_name` is read first with its pattern tested on all three million names to
/// keep a sixth of the roles, where read after `cast_info` it would be looked up at a few thousand.
/// A relation with no children whose parent is a root can trail the root in the executor, see
/// `Reducer`, so this tries each such leaf after the root, and hands the root on first where the
/// root is itself a leaf of one other relation, as `name` is of `cast_info`.
///
/// The root then holds the rows it keeps until the second sweep, which is charged at [`HOLD`] a
/// row.
#[expect(clippy::cast_precision_loss, reason = "a count of rows is a weight here")]
fn trail(
    edges: &[BTreeSet<u32>],
    weights: &[Weight],
    order: Vec<(usize, Option<usize>)>,
) -> Vec<(usize, Option<usize>)> {
    let Some(&(last, None)) = order.last() else { return order };
    let children = |order: &[(usize, Option<usize>)], of: usize| {
        order.iter().filter(|&&(_, parent)| parent == Some(of)).count()
    };
    let mut tried = order.clone();
    let mut root = last;
    if edges[last].len() == 1
        && let [only] = order
            .iter()
            .filter(|&&(_, parent)| parent == Some(last))
            .map(|&(child, _)| child)
            .collect::<Vec<_>>()[..]
    {
        for (relation, parent) in &mut tried {
            if *relation == only {
                *parent = None;
            } else if *relation == last {
                *parent = Some(only);
            }
        }
        root = only;
    }
    // What an order costs with the rows its root holds, when relations trail it.
    let cost = |order: &[(usize, Option<usize>)]| {
        let mut standing = Standing::new();
        let mut total = 0.0;
        for &(ear, _) in order {
            total += weights[ear].cost(&standing, &edges[ear]);
            if ear == root {
                let kept = weights[ear].fed(&standing, &edges[ear]) * weights[ear].kept;
                total += kept * weights[ear].rows as f64 * HOLD;
            }
            take(edges, weights, &mut standing, ear);
        }
        total
    };
    let mut best = priced(edges, weights, &order);
    let mut moved = false;
    loop {
        let at = tried.iter().position(|&(relation, _)| relation == root).unwrap_or_default();
        let mut cheapest: Option<(f64, Vec<(usize, Option<usize>)>)> = None;
        for (slot, &(leaf, parent)) in tried[..at].iter().enumerate() {
            if parent != Some(root) || edges[leaf].len() != 1 || children(&tried, leaf) > 0 {
                continue;
            }
            let mut after = tried.clone();
            let entry = after.remove(slot);
            after.push(entry);
            let total = cost(&after);
            if total < best && cheapest.as_ref().is_none_or(|(least, _)| total < *least) {
                cheapest = Some((total, after));
            }
        }
        let Some((total, after)) = cheapest else { break };
        best = total;
        tried = after;
        moved = true;
    }
    if moved { tried } else { order }
}

/// What holding a row of a root that relations trail costs, in the units of [`Weight::cost`]. A
/// row is copied out of its chunk once and read once more in the second sweep.
const HOLD: f64 = 2.0;

/// What an order of ears costs, each priced with what the ones before it left standing.
fn priced(edges: &[BTreeSet<u32>], weights: &[Weight], order: &[(usize, Option<usize>)]) -> f64 {
    let mut standing = Standing::new();
    let mut total = 0.0;
    for &(ear, _) in order {
        total += weights[ear].cost(&standing, &edges[ear]);
        take(edges, weights, &mut standing, ear);
    }
    total
}

/// How many sets of relations [`search`] carries from one depth to the next, the cheapest ones.
/// Every join of JOB up to about ten relations fits whole, and the widest, seventeen relations with
/// most of them one filtered table hanging off a fact table, is cut to this at its middle depths.
const STATES: usize = 256;

/// How many of the cheapest ears [`gyo`] prices with the rest of the order after them. Each one
/// costs a greedy order of the rest, so all of them would be a power of the relations more.
const LOOKAHEAD: usize = 4;

/// An ear, with what it costs, its place in `left` and its parent.
type Ear = (f64, usize, Option<usize>);

/// The ears of what is `left`, each priced by the one step, and the first ear that waits.
///
/// An ear with no filter of its own and no class a relation already taken holds waits until no
/// other ear is left, and is only handed back as the second half.
fn ears(
    edges: &[BTreeSet<u32>],
    weights: &[Weight],
    holders: &[Vec<u64>],
    left: &[usize],
    standing: &Standing,
) -> std::result::Result<(Vec<Ear>, Option<(usize, Option<usize>)>), String> {
    let slot = |ear: usize| left.iter().position(|&relation| relation == ear).unwrap_or_default();
    let taken = !left.iter().fold(0, |mask, &relation| mask | 1 << relation);
    let Some((steps, waiting)) = steps(left, weights, holders, taken) else {
        return Err("joins two relations on more than one column".to_owned());
    };
    let mut found = Vec::with_capacity(steps.len());
    for (ear, parent) in steps {
        let mut cost = weights[ear].cost(standing, &edges[ear]);
        // With two relations left the one taken second is the root, and what it costs follows
        // from the first: read `cast_info` first and `name` is gathered at the people it kept.
        // So the pair is charged in total, which the cheaper single step does not see.
        if let [one, other] = left[..] {
            let last = if ear == one { other } else { one };
            let mut after = standing.clone();
            take(edges, weights, &mut after, ear);
            cost += weights[last].cost(&after, &edges[last]);
        }
        found.push((cost, slot(ear), parent));
    }
    Ok((found, waiting.map(|(ear, parent)| (slot(ear), parent))))
}

/// What is left standing of each of its classes once `ear` is taken.
///
/// The share of its rows it keeps, and no more than the rows it keeps can hold of the class's
/// values. In JOB 21a `link_type` keeps two kinds of link out of eighteen, and the two are the
/// common ones, so `movie_link` keeps more than half its rows. Those are 16,000 rows, which name at
/// most 16,000 of the 2.5 million movies. Taken as half the movies, reading `movie_link` early
/// narrowed nothing, the order put it after `movie_info`, and 21a went from 23 ms to 330.
#[expect(clippy::cast_precision_loss, reason = "counts of rows and values are weights here")]
fn take(edges: &[BTreeSet<u32>], weights: &[Weight], standing: &mut Standing, ear: usize) {
    let share = weights[ear].fed(standing, &edges[ear]) * weights[ear].kept;
    let rows = share * weights[ear].rows as f64;
    for &class in &edges[ear] {
        let values =
            weights[ear].domain.iter().find(|(held, _)| *held == class).map(|&(_, values)| values);
        let named = values.map_or(1.0, |values| (rows / values as f64).min(1.0));
        let held = standing.entry(class).or_insert(1.0);
        *held = held.min(share).min(named);
    }
}

/// What the rest of an order costs taken one cheapest step at a time, and infinity for a rest that
/// has no order.
fn finish(
    edges: &[BTreeSet<u32>],
    weights: &[Weight],
    holders: &[Vec<u64>],
    mut left: Vec<usize>,
    mut standing: Standing,
) -> f64 {
    let mut total = 0.0;
    while !left.is_empty() {
        let Ok((ears, waiting)) = ears(edges, weights, holders, &left, &standing) else {
            return f64::INFINITY;
        };
        let best = ears.iter().copied().reduce(|best, ear| if ear.0 < best.0 { ear } else { best });
        let slot = match (best, waiting) {
            (Some((cost, slot, _)), _) => {
                total += cost;
                // Two left were priced as the pair.
                if left.len() == 2 {
                    return total;
                }
                slot
            }
            (None, Some((slot, _))) => {
                total += weights[left[slot]].cost(&standing, &edges[left[slot]]);
                slot
            }
            (None, None) => return f64::INFINITY,
        };
        let ear = left.remove(slot);
        take(edges, weights, &mut standing, ear);
    }
    total
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
        let (_, column) =
            find.place(plan, binding).expect("the predicate was checked to read this");
        let ty = plan.expr_type(expr).clone();
        let span = plan.expr_span(expr);
        return plan.add_expr_at(Expr::Column(ColumnBinding::new(fresh, moved[&column])), ty, span);
    }
    walk::rebuild(plan, expr, &mut |plan, child| rebound(plan, child, fresh, moved, find))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{ConsistentExtremes, DECODE, GATHER, Reach, Standing, Weight, gyo, take, trail, widens};
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
        let plan =
            Plan::parse(text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"));
        let (text, declined) = rewritten(plan);
        assert!(!text.contains("Consistent"), "{text}");
        assert!(declined.iter().any(|written| written.contains("cycle")), "{declined:?}");
    }

    fn edges(list: &[&[u32]]) -> Vec<BTreeSet<u32>> {
        list.iter().map(|classes| classes.iter().copied().collect()).collect()
    }

    /// Each relation's parent, off the order [`gyo`] gives, after checking that every relation is
    /// in it once and comes before its parent.
    fn parents(order: &[(usize, Option<usize>)]) -> Vec<Option<usize>> {
        let mut parents = vec![None; order.len()];
        let mut seen = vec![false; order.len()];
        for &(relation, parent) in order {
            assert!(!seen[relation], "{order:?}");
            seen[relation] = true;
            if let Some(parent) = parent {
                assert!(!seen[parent], "a parent comes after its children {order:?}");
            }
            parents[relation] = parent;
        }
        parents
    }

    /// A relation of `rows` rows with `width` read past its keys and `kept` of it left by its filter.
    fn weight(rows: u64, width: u64, kept: f64) -> Weight {
        Weight {
            rows,
            width,
            kept,
            reach: Vec::new(),
            gathered: Vec::new(),
            skew: Vec::new(),
            domain: Vec::new(),
            named: Vec::new(),
            placed: Vec::new(),
        }
    }

    /// The same weight for every relation, with a filter on each so that none waits.
    fn even(count: usize) -> Vec<Weight> {
        vec![weight(10, 1, 0.5); count]
    }

    #[test]
    fn a_star_is_one_tree_with_the_points_under_the_middle() {
        let order = gyo(&edges(&[&[0], &[0, 1, 2], &[1], &[2]]), &even(4)).expect("acyclic");
        // The middle is an ear of the last point once the others are gone, which is as good a
        // tree as the other way round, so what matters is one root and the points on the middle.
        let parents = parents(&order);
        assert_eq!(parents.iter().filter(|parent| parent.is_none()).count(), 1, "{order:?}");
        assert_eq!((parents[0], parents[2]), (Some(1), Some(1)), "{order:?}");
    }

    #[test]
    fn a_triangle_is_a_cycle() {
        let found = gyo(&edges(&[&[0, 1], &[1, 2], &[2, 0]]), &even(3));
        assert!(found.is_err_and(|reason| reason.contains("cycle")));
    }

    #[test]
    fn one_class_shared_by_many_is_a_line_from_the_cheapest_to_the_dearest() {
        let weights =
            [weight(30, 0, 0.5), weight(10, 0, 0.5), weight(40, 0, 0.5), weight(20, 0, 0.5)];
        let order = gyo(&edges(&[&[0], &[0], &[0], &[0]]), &weights).expect("acyclic");
        assert_eq!(order, [(1, Some(3)), (3, Some(0)), (0, Some(2)), (2, None)]);
    }

    /// JOB 26b's shape: `title` keeps a few movies, `cast_info` is written in movie order and holds
    /// both classes, and `char_name` has a filter of its own and is a tenth the size. Priced by rows
    /// alone `char_name` goes first and is read whole. Priced by the parts the movies left fall in,
    /// `cast_info` is nearly free after `title`, and `char_name` is then read at the roles left.
    #[test]
    fn a_relation_written_in_the_order_of_a_narrowed_class_goes_before_a_smaller_one() {
        let mut clustered = weight(36_000_000, 0, 1.0);
        clustered.reach = vec![(0, Reach { parts: 4_425, values: 2_500_000, per_value: 1.0 })];
        let weights = [weight(2_500_000, 0, 0.0001), clustered, weight(3_100_000, 4, 0.06)];
        let order = gyo(&edges(&[&[0], &[0, 1], &[1]]), &weights).expect("acyclic");
        let scanned: Vec<usize> = order.iter().map(|&(relation, _)| relation).collect();
        assert_eq!(scanned, [0, 1, 2]);

        // The same relation written in no order is the whole table again. It still goes second,
        // because the two left are priced as a pair and `char_name` read after it is read at the
        // roles left, where read first it is read whole and `cast_info` is still read whole after.
        let mut spread = weight(36_000_000, 0, 1.0);
        spread.reach = vec![(0, Reach { parts: 4_425, values: 2_500_000, per_value: 4_425.0 })];
        let weights = [weight(2_500_000, 0, 0.0001), spread, weight(3_100_000, 4, 0.06)];
        let order = gyo(&edges(&[&[0], &[0, 1], &[1]]), &weights).expect("acyclic");
        let scanned: Vec<usize> = order.iter().map(|&(relation, _)| relation).collect();
        assert_eq!(scanned, [0, 1, 2]);
    }

    /// JOB 9a's shape: `title` keeps a few movies, `cast_info` is written in no order that helps,
    /// and `name` has a filter of its own and is a ninth the size. One step at a time `name` is the
    /// cheaper read and went first, read whole. Taken as a pair, reading `cast_info` first leaves so
    /// few people that `name` is read at their rows, and the pair costs less that way round.
    #[test]
    fn the_last_two_are_ordered_by_what_the_pair_costs() {
        let mut spread = weight(36_000_000, 0, 1.0);
        spread.reach = vec![(0, Reach { parts: 4_425, values: 2_500_000, per_value: 4_425.0 })];
        let weights = [weight(2_500_000, 0, 0.0001), spread, weight(4_000_000, 4, 0.4)];
        let order = gyo(&edges(&[&[0], &[0, 1], &[1]]), &weights).expect("acyclic");
        let scanned: Vec<usize> = order.iter().map(|&(relation, _)| relation).collect();
        assert_eq!(scanned, [0, 1, 2]);
    }

    /// A relation whose key column can be gathered costs the parts the rows the standing keys reach
    /// fall in and no scan of the key column, once they are few enough. Past that it is scanned like
    /// any other.
    #[test]
    fn a_gathered_class_costs_the_parts_its_rows_fall_in() {
        let mut name = weight(4_000_000, 4, 1.0);
        name.gathered = vec![1];
        let classes = BTreeSet::from([1]);
        let few = Standing::from([(1, 0.00001)]);
        let cost = name.cost(&few, &classes);
        let parts = (4_000_000.0_f64 / 8192.0).ceil();
        let decoded = 1.0 - (1.0 - 1.0 / parts).powf(40.0);
        let gathered = 4_000_000.0 * decoded * DECODE * 5.0 + 40.0 * GATHER;
        assert!((cost - gathered).abs() < 1.0, "{cost} {gathered}");
        assert!(cost < 2_000_000.0, "{cost}");
        // Four thousand rows fall in nearly every part, and the scan is cheaper.
        let spread = Standing::from([(1, 0.001)]);
        assert!((name.cost(&spread, &classes) - 4_000_000.0 * 1.004).abs() < 1.0);
        let many = Standing::from([(1, 0.5)]);
        assert!((name.cost(&many, &classes) - 12_000_000.0).abs() < 1.0);
    }

    /// JOB 6d's `movie_keyword`: eight keywords of 134,170 reach 35,548 rows, which the average
    /// keyword puts at 270. The skew of the key column brings the price up to the rows it reaches,
    /// and rows that fall in every part cost the scan.
    #[test]
    fn a_skewed_key_column_costs_the_rows_its_kept_values_reach() {
        let mut movie_keyword = weight(4_523_930, 0, 1.0);
        movie_keyword.gathered = vec![0];
        let classes = BTreeSet::from([0]);
        let eight = Standing::from([(0, 8.0 / 134_170.0)]);
        let even = movie_keyword.cost(&eight, &classes);
        movie_keyword.skew = vec![(0, 133.0, 134_170)];
        let skewed = movie_keyword.cost(&eight, &classes);
        // The even guess finds 270 rows in 553 parts and decodes a third of them. The skewed one
        // finds 35,548, which fall in every part, and so it decodes the whole table.
        let found = 4_523_930.0 * 8.0 * 133.0 / 134_170.0;
        let whole = 4_523_930.0 * DECODE + found * GATHER;
        assert!(even < 600_000.0, "{even}");
        assert!((skewed - whole).abs() < 1_000.0, "{skewed} {whole}");
        assert!((movie_keyword.fed(&eight, &classes) - 8.0 * 133.0 / 134_170.0).abs() < 1e-9);
    }

    /// JOB 13a's `movie_companies`: the German companies are ten thousand of 235,000, kept for
    /// their country and not their size, and they hold 5.7 percent of the rows. The ratio of 400
    /// the column has would say all of them, and past the few values a query names it fades.
    #[test]
    fn a_skew_counts_for_the_values_named_and_fades_for_a_filter_that_keeps_thousands() {
        let mut movie_companies = weight(2_609_129, 0, 1.0);
        movie_companies.skew = vec![(0, 400.0, 234_997)];
        let classes = BTreeSet::from([0]);
        let german = 10_000.0 / 234_997.0;
        let fed = movie_companies.fed(&Standing::from([(0, german)]), &classes);
        let expected = german * (1.0 + 399.0 * 64.0 / 10_000.0);
        assert!((fed - expected).abs() < 1e-9, "{fed}");
        assert!(fed < 0.2, "{fed}");
        let four = 4.0 / 234_997.0;
        let named = movie_companies.fed(&Standing::from([(0, four)]), &classes);
        assert!((named - four * 400.0).abs() < 1e-9, "{named}");
    }

    /// JOB 13a in small: a dimension that is dear to read and keeps one percent, a hub it narrows,
    /// and two cheaper relations on the hub's other class whose rows are read at the keys the hub
    /// leaves. Each step alone takes the cheap ones first and the dimension last, where it narrows
    /// nothing.
    #[test]
    fn a_dear_filter_goes_first_when_the_hub_it_narrows_pays_for_it() {
        let mut weights = vec![weight(1_000_000, 4, 0.01), weight(1_000_000, 0, 1.0)];
        for _ in 0..2 {
            weights.push(weight(900_000, 4, 0.5));
        }
        let edges = edges(&[&[1], &[0, 1], &[0], &[0]]);
        let order = gyo(&edges, &weights).expect("acyclic");
        parents(&order);
        let taken: Vec<usize> = order.iter().map(|&(relation, _)| relation).collect();
        assert_eq!(taken[..2], [0, 1], "{order:?}");
    }

    /// JOB 26a in small: `title` narrows `cast_info` to a sliver by the movies, `name` has no
    /// filter and `char_name` has a pattern that keeps six percent of three million names. Read
    /// first, `char_name` tests all of them, and read after `cast_info` it tests the few left.
    #[test]
    fn a_leaf_with_a_loose_filter_trails_the_root_that_narrows_it() {
        let weights = vec![
            weight(2_500_000, 1, 0.0005),
            weight(36_000_000, 0, 1.0),
            weight(4_000_000, 4, 1.0),
            weight(3_000_000, 4, 0.06),
        ];
        let edges = edges(&[&[0], &[0, 1, 2], &[1], &[2]]);
        let order = trail(&edges, &weights, gyo(&edges, &weights).expect("acyclic"));
        let taken: Vec<usize> = order.iter().map(|&(relation, _)| relation).collect();
        let root = order.iter().position(|&(_, parent)| parent.is_none()).expect("a root");
        assert_eq!(order[root].0, 1, "{order:?}");
        assert!(taken[root + 1..].contains(&3), "{order:?}");
        assert!(order[root + 1..].iter().all(|&(_, parent)| parent == Some(1)), "{order:?}");
        // A leaf small enough to read whole costs less than holding the rows the root keeps.
        let mut weights = weights;
        weights[3] = weight(100, 4, 0.06);
        let order = trail(&edges, &weights, gyo(&edges, &weights).expect("acyclic"));
        let taken: Vec<usize> = order.iter().map(|&(relation, _)| relation).collect();
        let root = order.iter().position(|&(_, parent)| parent.is_none()).expect("a root");
        assert!(taken[..root].contains(&3), "{order:?}");
    }

    /// JOB 21a in small: `link_type` keeps two common kinds of eighteen, so `movie_link` keeps half
    /// its rows, and those rows name a sliver of the movies and not half of them.
    #[test]
    fn a_relation_leaves_no_more_values_standing_than_the_rows_it_keeps() {
        let mut movie_link = weight(30_000, 0, 1.0);
        movie_link.skew = vec![(1, 5.0, 18)];
        movie_link.reach = vec![(0, Reach { parts: 4, values: 2_500_000, per_value: 1.0 })];
        movie_link.domain = vec![(0, 2_500_000)];
        let edges = edges(&[&[0, 1]]);
        let mut standing = Standing::from([(1, 2.0 / 18.0)]);
        take(&edges, &[movie_link], &mut standing, 0);
        let movies = standing[&0];
        assert!((movies - 30_000.0 * 10.0 / 18.0 / 2_500_000.0).abs() < 1e-12, "{movies}");
    }

    /// JOB 13a: `info_type` keeps one type of 113, and `movie_info_idx` holds five of them. The one
    /// a query names is one of the five, so it reaches a fifth of them and not one percent.
    #[test]
    fn a_named_value_is_one_of_the_values_the_table_holds() {
        let mut movie_info_idx = weight(1_380_035, 4, 1.0);
        movie_info_idx.skew = vec![(0, 1.0, 5)];
        movie_info_idx.domain = vec![(0, 113)];
        assert!((movie_info_idx.local(0, 1.0 / 113.0) - 0.2).abs() < 1e-12);
        // Kept by another relation and in the thousands, the values are spread over the class.
        movie_info_idx.domain = vec![(0, 2_500_000)];
        movie_info_idx.skew = vec![(0, 1.0, 460_000)];
        let spread = movie_info_idx.local(0, 0.01);
        assert!(spread < 0.012, "{spread}");
    }

    #[test]
    fn a_value_the_store_counted_reaches_the_rows_it_counted() {
        // JOB 17c: one keyword of 134,170, which is 41,840 rows of movie_keyword and not the few
        // thousand the skew of the column gives an average named keyword.
        let mut movie_keyword = weight(4_523_930, 0, 1.0);
        movie_keyword.skew = vec![(0, 127.8, 134_170)];
        movie_keyword.domain = vec![(0, 134_170)];
        movie_keyword.named = vec![(0, 1, 41_840.0 / 4_523_930.0, None)];
        let one = 1.0 / 134_170.0;
        let rows = movie_keyword.reached(0, one) * 4_523_930.0;
        assert!((rows - 41_840.0).abs() < 1.0, "{rows}");
        // Half the named values left, half the rows.
        let rows = movie_keyword.reached(0, one / 2.0) * 4_523_930.0;
        assert!((rows - 20_920.0).abs() < 1.0, "{rows}");
    }

    /// JOB 14b: `movie_info` is laid out by its type of information, and the countries are in 390
    /// of its 1,812 parts, where the average type the reach of the column gives is in 94.
    #[test]
    fn a_named_value_costs_the_parts_its_ends_hold_it_in() {
        let mut movie_info = weight(14_835_720, 4, 0.1);
        movie_info.reach = vec![(0, Reach { parts: 1_812, values: 110, per_value: 94.0 })];
        movie_info.domain = vec![(0, 113)];
        let standing = Standing::from([(0, 1.0 / 113.0)]);
        let classes = BTreeSet::from([0]);
        let average = movie_info.cost(&standing, &classes);
        movie_info.named = vec![(0, 1, 0.085, Some(390.0 / 1_812.0))];
        let counted = movie_info.cost(&standing, &classes);
        let parts = 390.0 / 1_812.0 * 14_835_720.0;
        assert!((counted - parts * (1.0 + 4.0 * 0.085)).abs() < 1.0, "{counted}");
        assert!(counted > average * 4.0, "{average} {counted}");
    }

    /// JOB 14b: eight movies of `movie_info` gathered late, whose rows sit together in a table laid
    /// out by movie, are in eight parts and not a part a row.
    #[test]
    fn a_gather_at_values_whose_rows_sit_together_costs_a_part_a_value() {
        let mut movie_info = weight(14_835_720, 4, 0.1);
        movie_info.gathered = vec![0];
        movie_info.skew = vec![(0, 1.0, 2_468_825)];
        let standing = Standing::from([(0, 8.0 / 2_468_825.0)]);
        let classes = BTreeSet::from([0]);
        let scattered = movie_info.cost(&standing, &classes);
        movie_info.placed = vec![(0, 1.0)];
        let together = movie_info.cost(&standing, &classes);
        let parts = (14_835_720.0_f64 / 8192.0).ceil();
        let found = 14_835_720.0 * 8.0 / 2_468_825.0;
        let expected = 14_835_720.0 * (8.0 / parts) * DECODE * 5.0 + found * GATHER;
        assert!((together - expected).abs() < 1.0, "{together} {expected}");
        assert!(together * 4.0 < scattered, "{scattered} {together}");
    }

    #[test]
    fn two_relations_sharing_two_classes_are_a_composite_key() {
        let found = gyo(&edges(&[&[0, 1], &[0, 1]]), &even(2));
        assert!(found.is_err_and(|reason| reason.contains("more than one")));
    }

    #[test]
    fn relations_that_share_nothing_are_each_a_tree() {
        let order = gyo(&edges(&[&[0], &[1], &[]]), &even(3)).expect("a forest is acyclic");
        assert_eq!(parents(&order), [None, None, None]);
    }

    #[test]
    fn the_dearest_relation_is_scanned_last_and_the_rest_cheapest_first() {
        // Keyword, movie_keyword, title and movie_info, as JOB 3c joins them, with movie_info the
        // dearest by far. It has to come last so that it is read only at the movies left.
        let weights = [
            weight(134_000, 4, 0.001),
            weight(4_500_000, 0, 1.0),
            weight(2_500_000, 1, 0.7),
            weight(15_000_000, 4, 0.1),
        ];
        let order = gyo(&edges(&[&[0], &[0, 1], &[1], &[1]]), &weights).expect("acyclic");
        assert_eq!(order, [(0, Some(1)), (1, Some(2)), (2, Some(3)), (3, None)]);
    }

    #[test]
    fn a_relation_nothing_narrows_waits_for_one_that_does() {
        // Role names, cast and a filtered title, as JOB 24b joins them. The role names are the
        // cheapest but have no filter and share nothing with the title, so the title goes first and
        // the role names last, read only at the roles the cast kept.
        let weights =
            [weight(3_000_000, 4, 1.0), weight(36_000_000, 0, 1.0), weight(2_500_000, 1, 0.01)];
        let order = gyo(&edges(&[&[0], &[0, 1], &[1]]), &weights).expect("acyclic");
        assert_eq!(order, [(2, Some(1)), (1, Some(0)), (0, None)]);
    }

    #[test]
    fn a_dear_relation_the_keys_narrow_goes_before_a_cheap_one_they_do_not() {
        // Title filtered to a handful, then cast and name as JOB 24b has them. Name has a filter and
        // is the cheaper to read whole, but cast is read at a handful of movies past its key column
        // and hands name a handful of people.
        let weights = [
            weight(2_500_000, 1, 0.000_002),
            weight(36_000_000, 0, 1.0),
            weight(4_200_000, 8, 0.05),
        ];
        let order = gyo(&edges(&[&[0], &[0, 1], &[1]]), &weights).expect("acyclic");
        assert_eq!(order, [(0, Some(1)), (1, Some(2)), (2, None)]);
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
