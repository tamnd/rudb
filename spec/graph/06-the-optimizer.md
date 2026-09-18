# 6. The optimizer

`crates/rudb-opt/src/lib.rs` is 461 lines and there is no cost model, no table statistics and no join reordering in it. That is the constraint this document is written under, and it is a more important constraint than any preference about how the decision *should* be made. A design that needs a good cost model to be fast is a design that is not fast yet.

## 6.1 The decision, in the order it is made

Four passes, each of which can decline and leave the plan as it was.

**Pass one: annotate.** Walk the plan and mark, for every output of every node, whether it still carries a base table `rid`, per document 05 section 5.1. This pass is mechanical, has no cost model in it, and is the precondition for everything after it. It belongs in `crates/rudb-plan` beside the other plan properties rather than in the optimizer, so that it is maintained by whoever adds a node rather than by whoever remembers.

**Pass two: match relationships.** For each equi-join, look up whether a verified relationship exists between the two columns and whether its link is built. Record the match on the join node with the direction and the cardinality. Still no cost model: this pass only discovers what is available.

**Pass three: reduce.** Build the join graph, choose a reduction schedule, and insert the reduction operators. Section 6.5.

**Pass four: rewrite joins.** Turn the joins whose links survived the reduction into link joins, and choose the build side for the rest. Section 6.4.

The ordering matters. Reduction comes before join rewriting because a reduction changes the cardinalities the join decision is made against, and because a reduction that ran is often enough that the join after it does not need to be a link join at all.

## 6.2 The statistics this needs, and where they come from

Exactly four numbers per relationship, all of them recorded at build time in the section header per document 03 section 3.3 and therefore exact rather than estimated: the child row count, the parent row count, the number of child rows with no parent, and the maximum degree. From those, the join's output cardinality for an inner link join is exact, it is the child row count minus the unmatched count, which is a strictly better position than any cost model this project will have for a long while, and it is worth noticing that this is a side effect of storing the join rather than an additional feature.

`../stats/07-graph-statistics.md` adds five more facts of the same kind and the same cost, computed inside the same build: the degree distribution, which is what decides whether factorization pays and whether a backward traversal parallelises by parent or by edge; the measured gather locality, which replaces section 6.4's plan-time approximation of the link-versus-hash crossover; and the uniqueness and totality certificates, which license join elimination.

All four cross the interface as `Fact` values with class `Exact` and provenance `LinkHeader`, per `../stats/02-the-catalogue.md` section 2.1.1, and so does the output cardinality derived from them. That is worth being fussy about at the boundary. An exact join cardinality is the single most valuable number in the whole catalogue, because join cardinality is where every cost model in the literature goes wrong by orders of magnitude, and handing it to the planner as a bare number would let it be mistaken for an estimate two layers later. It has to arrive carrying its class, and the physical planner's cost comparison has to be able to see that one side of the comparison is not a guess.

What is still estimated is the selectivity of a predicate on the parent table, which decides whether a reduction pays. Three sources, in order of quality. The frequency synopses of `../storage-v3/11-certified-frequency-synopses.md`, which are exact for leading values and carry a certified bound for the rest. The zone maps, which bound a range predicate per part and so bound a selectivity by counting the parts that survive. And a default, which is a guess, and which is recorded as a guess in `EXPLAIN` so a bad plan is attributable.

## 6.3 The floor

`../02-the-goal.md` states that no query may be slower than DuckDB, ever, on any suite. Applied here that becomes a rule with teeth: **the graph path is taken only where the plan without it is still available and the fallback is cheap.**

That rules out one class of decision entirely, which is a reduction whose cost cannot be bounded before it runs. It permits the rest because of three properties. A link join's cost is bounded by the child row count, which is known exactly. A reduction's cost is bounded by one pass per edge, which is known exactly. And both degrade at runtime: a reduction that has processed a third of its input and removed nothing can stop, keep the `Full` bitmap of section 4.3, and cost only the third it did. That last one is adaptivity in the sense of `../engine/12-adaptivity.md` and it is the cheapest possible version of it, because the thing being adapted is a filter that is allowed to be incomplete.

## 6.4 Choosing between a link join and a hash join

The link join wins when the parent's projected columns are few and the gather is local. The hash join wins when the parent is small enough that its hash table is cache resident and the link join's gathers are random over a parent that is not. The crossover is a memory hierarchy question, and the plan-time approximation is:

- Parent fits in the last level cache after projection: hash join. A dimension table of twenty five nations is not worth a link.
- Parent does not fit, child is clustered by the parent `rid`, so the gathers are ascending: link join.
- Parent does not fit, child is not clustered: measure. This is the case the default has to be chosen for and the default is the link join when the projected parent width is under thirty two bytes and the hash join otherwise, with both numbers settings and both defaults owed a measurement in document 09 section 9.3.

A semi or anti join over a verified relationship is always the link join, because it never touches the parent at all.

## 6.5 Scheduling the reduction

Given the join graph after pass two, decide which edges to reduce and in what order.

The full algorithm is the Yannakakis shape: build a join tree, pass semi-joins bottom up, pass them top down, then join. Robust Predicate Transfer's contribution is that the tree must be chosen to guarantee full reduction, which its LargestRoot does with a maximum spanning tree over the weighted join graph, and that a join order has to be checked for safety when the query is not γ-acyclic. rudb takes LargestRoot's tree construction directly, and it gets the full reduction guarantee more easily than RPT did because the reduction is exact rather than Bloom-approximate, per document 05 section 5.4.

What rudb adds is a gate, because a full reduction on a query where everything joins is pure overhead. Three tiers:

**Always reduce an edge whose parent side has a predicate on it.** This is nearly free and it is where the wins are. TPC-H has a filtered small table on almost every query that joins.

**Reduce an edge with no predicate only if an adjacent edge's reduction removed more than a threshold fraction**, default half. This is the propagation that turns a filter on `region` into a filter on `lineitem`, and gating it on the previous edge's observed effect means the chain stops as soon as it stops paying. The threshold is observed at runtime, not estimated at plan time, which means this decision is made by the executor with the optimizer having only laid out the option.

**Never reduce an edge whose child is smaller than a threshold**, default one hundred thousand rows, because the pass costs more than the join.

The learned gating in the selective-Yannakakis work reports 4.4x end-to-end on DuckDB and is the obvious next step; it is not scheduled, because a learned model is a dependency and a component with no measurements behind it yet, and the runtime-observed threshold above gets a large fraction of the same effect with none of the machinery. Document 11 keeps it open.

## 6.6 Join order

Reduction makes join order matter less, which is the argument of both *Debunking the Myth of Join Ordering* and *One Join Order Does Not Fit All*: after every table has been reduced to its contributing rows, the intermediate sizes are bounded by the output and the orderings differ by much less. That is convenient, because rudb has no join reordering at all.

The position this directory takes is therefore: build the reduction first and the reordering second, and measure the reordering's remaining value on the reduced plans rather than on the unreduced ones. It is entirely possible that the measured value of a cost-based join enumerator, after full reduction, is small enough on TPC-H to move it behind other work. It will not be small on JOB. `../planner/07-join-ordering.md` remains the specification for when it is built; this document only claims the order to build things in.

## 6.7 What `EXPLAIN` has to show

Per join: whether a relationship was found, whether its link was built, whether the `rid` survived, which algorithm was chosen, and for the ones not chosen, the reason. Per reduction: the edge, the estimated and the observed removal fraction, and whether it stopped early. Per scan: how many parts the reduction's zone map skipped.

Every one of those is a number the operator already has, and the reason to insist on all of them is that this layer's failure mode is silence. A hash join that is slow is visibly a hash join. A link that was not used because a projection dropped the `rid` two nodes up looks exactly like a link that does not exist, and without the reason in the plan output nobody finds it.
