# The pass pipeline

A bound plan goes in, a better plan comes out, and in between is a fixed sequence of rewrites and exactly one search. This document is the framework and the order. The individual passes are documents 04 through 08.

## What a pass is

```rust pub trait Pass {
    /// The name, which is what a session setting disables and what the bisector reports.
    fn name(&self) -> &'static str;
    /// A plan in, a plan out. Pure: no catalog writes, no interior mutability, no ordering
    /// dependence on anything but its input.
    fn run(&self, plan: Plan, ctx: &Context) -> Result<Plan>;
}
```

`Context` holds the catalog, the statistics, the session settings and a deadline. That is the whole framework. There is no rule engine, no memo, no pattern-matching DSL, and document 07 section 07.6 gives the argument for why not.

Three properties are imposed on every pass and checked rather than documented.

**It preserves the plan invariant.** `Plan::validate` in debug builds, after every pass, not after the pipeline. A pass that produces a plan whose types are inconsistent or whose column references do not resolve fails on the commit that broke it rather than three passes later in a stack that names the wrong pass.

**It preserves the output schema.** The root's column names, order and types are the same after the pass as before. This is the strongest single check available and it is cheap: a pass that drops a column or widens a type is caught immediately, and almost every pushdown bug has that shape. The one pass allowed to break it is the last one, and no pass in this folder is.

**It is idempotent, or it says it is not.** Running a pass twice should produce the same plan. Where that is not true, and for the fixed-point passes it deliberately is not on the first application, the pass says so and the harness stops asserting it. A pass that oscillates is a pass that will hang the fixed-point loop.

## Naming and toggling

Every pass has a name and `SET disabled_optimizers = 'filter_pushdown,join_order'` turns it off, which is DuckDB's spelling and is therefore what the corpus already uses in the files that set it.

This exists for testing before it exists for users, and document 12 spends most of its length on what it buys. The short version: with *n* named passes, a wrong answer is localized in *n* runs instead of an afternoon of reading, and `rudb-compat` has already committed to doing that automatically.

## The order

Numbered, because the numbers are how the rest of the folder refers to them.

**1. Expression simplification.** Constant folding, comparison normalization so the constant is on the right, connective flattening, boolean identities, null propagation. Applied to a fixed point with a small round bound. Runs first so every later pass sees one shape of every expression. Document 04 section 04.1.

**2. Type coercion made explicit.** Casts become plan nodes rather than implicit kernel promotions. The reason is not tidiness: an implicit promotion inside a kernel is invisible to every later pass, so a pass that wants to know whether a predicate can become a zone-map check on a column cannot tell whether the comparison is against the column's own type or against something widened first. This is also where a cast that is provably lossless on a comparison is removed, which is what turns `CAST(int_col AS BIGINT) = 5` back into a predicate the scan can evaluate.

**3. Subquery unnesting.** Document 05. Third rather than later because everything after it assumes a plan with no dependent joins, and a pass that has to reason about one is a pass with a second code path.

**4. Set operation rewrites.** `UNION ALL` flattening, `INTERSECT` and `EXCEPT` to semi and anti joins where the duplicate semantics permit it. Here because it produces joins that later passes want to see, and because `UNION ALL` flattening turns a left-deep chain of two-input nodes into one *n*-input node that pushdown can push into all of at once.

**5. Filter pushdown.** Document 04 section 04.3. The largest pass in the folder, including the transitive predicates that document 08 later seeds from.

**6. Join reordering preparation.** Flatten the join tree into a hypergraph and mark which joins are reorderable given outer-join semantics. Not the search, just the analysis, and separate from it because the analysis is also what document 08 builds its transfer graph from.

**7. Projection pushdown.** Document 04 section 04.2. After filter pushdown, so that a scan's column list is computed once against a plan whose predicates have already moved, rather than computed and then invalidated.

**8. Common subexpression and common subplan elimination.** After both pushdowns, so that subtrees made identical by pushdown are recognized as identical. This is the pass that needs the multi-consumer node from document 02.

**9. Aggregate and distinct rewrites.** Aggregate pushdown through a join where the functional dependencies permit. Distinct elimination where a key constraint or an upstream aggregate already guarantees distinctness. `COUNT(DISTINCT x)` over a single column becoming a two-phase aggregate. `GROUP BY` on a column functionally determined by another group key being dropped from the key list.

**10. Limit pushdown and top-N.** A limit through a projection, combined with the limit below it, and pushed into a sort to make it a top-N. Pushed into a scan to stop early where no operator between reorders or filters.

**11. Window rewrites.** Sharing a sort between windows with the same partitioning, and turning a filtered ranking window into a top-N per group, which is the shape TPC-DS uses repeatedly.

**12. Empty and constant pruning.** A provably false filter collapses its subtree to an empty node with the right schema. A join against an empty side collapses by the join kind's own rule. `WHERE false` on a table that has not been read is the case that makes this a correctness matter and not only a speed one, because the alternative reads the table.

**13. Join ordering.** Document 07. The only search in the list.

**14. Predicate transfer.** Document 08. After join ordering because it wants the join graph, and after everything else because the filters it transfers are seeded by the predicates pass 5 pushed to the leaves.

**15. Physical planning.** Document 10. Operator selection, build side, layout requirement propagation. Strictly speaking not a rewrite pass and it produces the physical plan rather than a logical one, which is why it is last and separately numbered in document 10.

## Why fixed and not searched

There is no cost model choosing between rewrite orders, in DuckDB, in Polars, or here. Building one would be the second-hardest thing in this folder for a benefit nobody has demonstrated.

The order above is not arbitrary and the argument for each position is in the entry. The two positions most likely to be argued with, stated explicitly:

**Filter pushdown before projection pushdown**, which is the opposite of the order the firepanda folder chose. The reason for the difference is that firepanda is a dataframe library whose biggest single win was narrowing a scan, and rudb's scan is narrowed by the same pass but the join columns are not known until the predicates have moved. Pushing a predicate below a join changes which columns the join needs to carry, so a projection computed first is a projection computed twice. Both orders are defensible and the loop below makes the difference small; the reason to write one down is that a fixed pipeline with an unstated order is a pipeline nobody can reason about.

**Unnesting before pushdown**, which is not negotiable. A dependent join is a correlation, and a pass that pushes a predicate across one has to decide whether the predicate references the correlated column, which is a question with a wrong answer available. Unnest first and the question does not exist.

## The loop, and the budget

Run passes 1 through 12 twice if the second run changes anything, up to two extra rounds. DuckDB applies expression rewriting repeatedly for the same reason and it costs microseconds on a plan of tens of nodes. Passes 13, 14 and 15 run once.

**And there is a deadline on the whole thing.** `Context` carries it, every pass checks it at its own natural granularity, and the pass that exceeds it returns the plan it has rather than continuing.

This is not an optimization, it is the second axis of the parent spec showing up in the optimizer. `spec/02-the-goal.md`'s per-query floor says no query is slower than DuckDB on any suite ever, and a query that plans for four hundred milliseconds and runs for two hundred is a query the optimizer made slower. Without a budget that case is invisible, because nobody profiles the planner.

The budget is a fraction of the estimated execution cost rather than a constant, with a floor and a ceiling. A query estimated to run for a minute can afford ten milliseconds of join ordering; a query estimated to run for a millisecond cannot. Document 07 section 07.5 says what join ordering does when it is cut off, which is the only pass where the answer is interesting.

**And planning time is asserted per query in CI**, against a committed budget, so that the floor cannot regress silently. That assertion is in document 12.

## What the pipeline is not

**Not Cascades.** Not a top-down memo with a rule set and a search over transformations. `spec/engine/11-optimizer.md` section 11.6 rejects it rather than deferring it and the argument is worth keeping: almost every win in the list is a rewrite that is always good and never needs to be costed, and the one place that genuinely needs search is join ordering, which has its own dedicated algorithm with its own dedicated budget. A memo buys the ability to cost a rewrite against its alternative, at the price of unpredictable planning time, which is exactly the axis the project cannot spend.

The cost of that choice is real and should be stated rather than won by assertion: a rewrite whose benefit depends on context cannot be decided by the framework and has to decide for itself, with a local cost comparison. `spec/engine/11-optimizer.md` section 11.1 lists six such decisions deferred from earlier documents and each one gets a local comparison in document 10 rather than a global search.

**Not a rule DSL.** Twelve passes written as twelve Rust functions over an arena is less code than the framework that would let them be written as patterns, and it is much easier to bisect.

## What we should take from this document

A pass is a named pure function from plan to plan, and the name is the thing the bisector reports.

Three checked properties: the plan invariant in debug builds after every pass, the root schema unchanged, and idempotence unless declared otherwise.

Fifteen numbered stages, with unnesting third and both pushdowns before elimination, and the two order decisions most likely to be argued with have their arguments written next to them.

A deadline on the whole pipeline, scaled to the estimated execution cost, because the per-query floor is an optimizer property before it is an execution one and a planner nobody profiles is a planner that regresses invisibly.

Not Cascades, and the cost of not being Cascades is six decisions that have to cost themselves locally.
