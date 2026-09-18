# The optimizer

`crates/rudb-opt` is 4,606 lines now, `filter.rs`, `columns.rs`, `fold.rs`, `limit.rs`, `nulls.rs`, `topn.rs`, `transitive.rs`, `empty.rs`, `tables.rs`, `walk.rs`, so the nine-line stub v1 described has become a pass library. What is not there is a cost model, a join enumerator, and subquery unnesting.

## 1. Shape

A list of named passes over a logical plan, run in a fixed order, each individually switchable.

```rust pub trait Pass: Strategy {
    fn apply(&self, plan: Plan, stats: &Statistics, ctx: &Context) -> Result<Plan>;
}
```

Every pass is registered, named, switchable by `SET optimizer.passes = '...'`, timed in the metrics document, and printed by `EXPLAIN (OPTIMIZER)` with the plan before and after. A researcher adding a rewrite from a paper adds a `Pass`, and the same sweep machinery from [`04-modularity.md`](04-modularity.md) section 7 gives them a per-query ablation across the suite.

Not Cascades. v1 rejected it and the reasoning holds: the per-query planning floor is one of the four axes, the available wins here are rewrite wins rather than search wins, and a memo-based search framework is a large amount of machinery whose main benefit, extensibility of the search space, is delivered here by the pass registry at a fraction of the cost. The one place a search is genuinely needed, join enumeration, is a self-contained DP inside one pass.

## 2. Order, by value

**Filter pushdown**, including through joins and aggregates, and including derived predicates from transitivity. Already partly built.

**Projection pushdown**, which on a 105-column table is the difference between reading three columns and reading 105. Already built as `columns.rs`.

**Subquery unnesting.** Neumann and Kemper's arbitrary unnesting as the general case, plus the five special shapes, uncorrelated scalar, `IN`, `EXISTS`, `ANY`/`ALL`, lateral, that cover almost everything real and unnest to something much better than the general case does. This is the single largest missing pass and it is what makes TPC-H Q2, Q17, Q20 and Q21 tractable.

**Join ordering.** Section 4.

**Partial aggregate pushdown below joins**, which DuckDB v2.0 added and which is worth a great deal on star schemas.

**Folding, simplification, common subexpression elimination.** Constant folding, `NULL` propagation, redundant cast removal, comparison normalisation. `fold.rs` and `nulls.rs` exist.

**Predicate transfer.** Section 5.

**Physical planning.** Distribution and exchange placement per [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md) section 5, form negotiation per [`05-data-model.md`](05-data-model.md) section 8, and strategy selection at every seam per the `Policy`.

## 3. Statistics

The asset is already in the tree: `rudb-encoding/src/sketch.rs`, 508 lines of KMV sketch with `union`, `jaccard` and `dependence`, computed per column and per block.

That is an unusual thing to have and it is worth being precise about why it matters. Cardinality estimation for joins is hard because it needs set intersection, and a histogram cannot do set intersection. A KMV sketch can: `jaccard` between two join keys gives an overlap estimate directly, and `dependence` gives the correlation term that makes multi-predicate selectivity estimates stop being the product of independent selectivities. Most engines estimate joins by assuming containment and most of their bad plans come from that assumption.

What the estimator uses, in order: exact counts where they exist, block min/max for range selectivity, KMV for distinct counts and for join overlap, `dependence` for the correlation term on conjunctions, and a fixed guess as the floor.

The seam is `opt.cardinality` with `fixed-guess` as the reference, `sketch-kmv` as the default, and `sketch-with-correlation` as the one under test. Quality is measured as q-error over CEB and JOB, which are in `rudb-bench`'s suite list, and is reported as a distribution rather than a mean because the tail is what produces bad plans.

## 4. Join ordering

DPccp for queries that fit, DPhyp for hypergraph predicates, greedy above a budget. Cross products considered under a size bound. A planning-time budget in microseconds, because the per-query floor is an axis.

The honest uncertainty here is large and is worth stating. "Debunking the Myth of Join Ordering" argues that with robust predicate transfer applied, the choice of join order matters far less than the literature assumes. If that is true on our workload, then the expensive part of this document is the cheap part, and the effort belongs in section 5 instead.

That is a two-line experiment once both are registered: run JOB and CEB with `opt.join-order=as-written` and `join.filter=predicate-transfer`, against `opt.join-order=dphyp` and `join.filter=none`, and against both. F8's gate requires that experiment to be run and its result recorded, whichever way it comes out.

## 5. Predicate transfer

Yu et al., CIDR 2024, generalised as sideways information passing: build Bloom filters on join keys and push them to every scan they can reach, before the joins run.

Three things in the tree:

`bloom-probe-side`, which is the basic form and belongs to the join operator, build side to probe side, at F6, and [`11-operators.md`](11-operators.md) section 5 calls it the largest single win in that operator.

`predicate-transfer`, the full graph-shaped version, at F8, which propagates filters across the whole join graph in a forward and a backward pass.

`parachute`, which precomputes reachability filters at load time. VLDB 2025 reports 1.54x on JOB against DuckDB v1.2 for about fifteen per cent extra space, filters capped at 8 KiB with m = 2^16 and k = 2. This is a storage decision as much as an optimizer one and it lands in [`06-storage.md`](06-storage.md) section 4 as an optional per-column artefact.

And the 2025 result that pushing Bloom filters *into* bottom-up join enumeration, rather than applying them after the order is fixed, is worth a further 32.8% on 100 GB TPC-H. That is a change to the enumerator's cost function rather than a new pass, and it is the reason sections 4 and 5 have to be designed together rather than sequentially.

## 6. What `EXPLAIN` has to show

The optimizer is the part of an engine that is hardest to debug and easiest to instrument, and the engine's own honesty rules make this mandatory rather than optional.

`EXPLAIN` shows the physical plan with estimated cardinality per node, the strategy chosen at every seam with its provenance, every `Decode` node the form negotiation inserted and who forced it, every `Exchange` and its distribution, and a marker on every node running a reference implementation.

`EXPLAIN ANALYZE` adds actual cardinality beside estimated, per-node and per-pipeline time and CPU time, bytes read, bytes decoded, bytes spilled, and the adaptive decisions taken with the point at which they were taken.

`EXPLAIN (OPTIMIZER)` shows the plan after each pass and the time each took.

The estimated-versus-actual column is the one that pays for itself fastest. A q-error of a thousand on one node explains a bad plan in one glance, and no current output shows it.

## 7. Gate

F8 is done when total CPU seconds across TPC-H SF100 is below DuckDB's, when the optimizer-on and optimizer-off answers are identical across the whole corpus and also per pass, when q-error distributions on JOB and CEB are recorded, and when the join-ordering-versus-predicate-transfer experiment from section 4 has been run and its answer written into this document.
