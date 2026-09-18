# The plan

Ten phases. Each one names what lands, why it is at that point in the order, what it deliberately leaves out, and the measurement that says it is done. The measurement is the important column: a phase whose exit is "it works" is a phase nobody can close.

Two properties hold at every point. The engine is whole, so there is no flag day and no long lived branch. And nothing in a later phase is a precondition for an earlier one, so stopping after any phase leaves a coherent system rather than a half migration.

## P0. Facts reach the planner

**What lands.** The `Fact` type with its four classes and its provenance, in the statistics interface rather than in the planner, per document 04 section 4.6. A `Facts::get` that never blocks. `Context` holding a snapshot key rather than a map of numbers. The first four suppliers wired: row counts, per-part zone maps, dictionary sizes, and the four numbers in a link header from `../graph/`. `estimate.rs` keeps its constants for everything else and they become `Estimated` facts with `Provenance::Default`, which is what they always were.

**Why first.** Every later phase spends facts. A physical planner reading two Selinger constants is a second place to guess.

**Exit.** `EXPLAIN (LOGICAL, STATISTICS)` prints a class and a provenance for every cardinality in the ClickBench and TPC-H plans, and the q-error histogram is published per class. The specific number to look at is the fraction of plan cardinalities that are `Exact` or `Certified` rather than `Estimated`, measured before and after.

**Not in this phase.** No pass changes behaviour. Plans are identical to today's except that they now carry classes.

## P1. The analyses, and the rewrites they enable

**What lands.** The uniqueness and functional dependency analysis and the row identity preservation analysis, both in `rudb-plan` next to the plan, per document 05 section 5.2. Then the four enabling rewrites that consume them: join elimination, group by elimination, `DISTINCT` elimination and sort elimination, as a fact-consuming suffix after annotation, with the monotone loop from document 05 section 5.5.

**Why here.** These are the cheapest real wins in the folder. They need one analysis and `Exact` facts, they delete nodes rather than adding them, and they need nothing from the physical plan.

**Exit.** Every one of the four fires on at least one query in the corpus, with a plan diff checked in. On TPC-H the specific thing to look for is the `DISTINCT` and `GROUP BY` eliminations that a primary key makes valid.

## P2. Answered from statistics

**What lands.** The aggregation strategy that reads a fact and emits a constant: `COUNT(*)`, `COUNT(col)`, `COUNT(DISTINCT s)` on a dictionary column, `MIN` and `MAX` from merged zone maps, and a top-k `GROUP BY` from a certified frequency synopsis where the certificate discharges. Document 11 section 11.3.

**Why here.** It is pure addition, it touches no existing operator, and it is the largest single speedup available for the least code. It needs P0 and nothing else.

**Exit.** The affected ClickBench queries read zero data pages, measured by the page counter rather than by the clock, and the wall time drops to the planning time. Every answered query is also run with the strategy disabled and the rows compared, which is the first use of the differential setting rule from document 13 section 13.3.

## P3. The physical plan, empty

**What lands.** The `rudb-phys` crate. A physical node type, a printer, a parser, round trip tests, `EXPLAIN (PHYSICAL)`, and a lowering that produces exactly what `build.rs` produces today. Every strategy field is present and every one is set to the value that reproduces current behaviour.

**Why here.** It is the structural phase and everything after it is cheaper because of it. It is deliberately boring: nothing gets faster and nothing gets slower.

**Exit.** Two measurements. Every query in the corpus round trips through print and parse of the physical plan. And the benchmark is unchanged within noise, which is the whole point of doing it as an empty shell first.

**Not in this phase.** No decision changes. `sides.rs` and `late.rs` stay in `rudb-opt` where they work.

## P4. Layout requirements and encoded execution

**What lands.** Requirement propagation down and capability propagation up, meeting at the scan, with an inserted and printed decode where they do not meet. Document 07 section 7.5. Then the four encoded operations from document 10 section 10.4, in order: group on codes, compare in code space, aggregate over runs, compare on compressed bytes.

**Why here, and why before compilation.** Document 10 section 10.3. The Bespoke OLAP ablation measured code specialization at 1.26x and 0.57x on a fixed layout, and the same system at 12.35x and 51.40x once the layout moved. rudb already writes these representations and currently decodes all of them before anything interesting happens. This is the phase with the largest expected effect in the whole folder.

**Exit.** On the ClickBench group by queries, the dictionary decode does not appear in the physical plan and does not appear in a profile. The measurement is decoded bytes per query, before and after, published per query rather than in aggregate.

## P5. The strategies leave group.rs

**What lands.** Aggregation strategy selection moves into the physical planner, one strategy at a time: direct addressed, shared concurrent, partitioned, pre-aggregated. Each move deletes the runtime recogniser from the operator. Then the distinct and mixed aggregate paths become plan rewrites and `group_distinct.rs` and `group_mixed.rs` go away. Document 11 section 11.7.

**Why here.** It needs the physical plan from P3 and it benefits from the encoded paths from P4, because grouping on codes is what makes direct addressing apply so widely.

**Exit.** Three numbers. `group.rs` line count, which should fall by more than half. The strategy differential across the corpus, every legal strategy forced, same rows. And the churn metric from document 13 section 13.6 six weeks later, where `group.rs` should no longer be in the top three.

## P6. Reduction

**What lands.** `Node::Reduce` and `Node::LinkJoin`, the four-pass schedule from document 06 section 6.3, the gate from section 6.4, the abort from section 6.5, and a runtime filter emitted unconditionally by every hash join build. The exact bitmap path where `../graph/` has a link, the Bloom path where it does not.

**Why here.** It needs facts from P0 and row identity preservation from P1, and its join rewriting half wants the physical plan from P3. It is late in the order despite being the largest claim in the folder, which is a deliberate choice: it is the phase most likely to take longer than planned, and putting it after the phases that pay off quickly means a slip costs less.

**Exit.** Per edge output showing which mechanism was chosen and the actual reduction achieved, per document 06 section 6.8. On TPC-H the number to publish is intermediate result size against output size, which is the guarantee Robust Predicate Transfer states and the thing an exact reduction is supposed to improve on. On the join benchmarks, wall time per query with reduction on and off.

## P7. The pipeline program

**What lands.** `rudb-ir` grows from a 9 line stub into the program type, with a printer, a parser and `EXPLAIN (PROGRAM)`. Every physical node lowers to a single opaque block wrapping the operator that runs it today. Then the aggregate and the join come apart into blocks, sharing the hash, key, insert, probe and update blocks. Document 08 section 8.9.

**Why here.** It is a refactor with no user visible effect, so it goes after the phases that produce numbers. Doing it earlier would be defensible on architectural grounds and would mean a long stretch with nothing to show.

**Exit.** The block count, which should be at or under thirty. The share of execution time spent in blocks rather than in opaque wrappers, published as the migration progresses. And no regression on any benchmark query, which is the hard part, because a decomposed operator that is five percent slower than the fused one it replaced is a real cost that has to be bought back in P8.

## P8. Fusion and compilation

**What lands.** Tier 1 fusion over runs of blocks rather than over expression shapes, then tier 2 Cranelift over a whole pipeline rather than an expression tree. Document 10 section 10.2. Asynchronous compilation, morsel boundary switching, the compiled-against-interpreted differential.

**Why last.** Two reasons. It needs the program from P7 to be worth doing at pipeline scope. And document 10 section 10.3's evidence says it is worth tens of percent where P4 is worth an order of magnitude, so it goes after the thing that is worth more.

**Exit.** The tier differential across the full corpus, bit for bit, subject to the thread count caveat. Compile latency measured as a fraction of total query time per benchmark query, which is also the measurement that decides whether tier 3 is ever built, per document 10 section 10.7.

## P9. What is left

**What lands.** DPhyp under a budget for join ordering, per document 06 section 6.6. `sides.rs` and `late.rs` finally move into `rudb-phys`. Continuous spill degradation replacing the current transitions. The window and set operation paths get their blocks. Transitive predicate generation becomes a named pass.

**Why last.** Every item here is either a bounded improvement on something that already works or a cleanup that is safe to defer. None of them blocks anything.

**Exit.** Per item, and none of them is a headline number.

## If only some of this happens

The order above is the order to do them in. It is not the order of importance, and if the work gets cut the thing to protect is different from the thing to do first.

**If three phases happen, make them P0, P3 and P5.** Facts, the physical plan, and the strategies leaving `group.rs`. That is the churn fix, which is the problem this folder was written about.

**If one number has to move, it is P4.** Encoded execution is the largest measured effect available and it does not depend on P3 to be worth doing, though it is much cleaner with it.

**If one thing has to be protected from being cut, it is P0.** Every other phase either spends facts or is made safe by them, and a physical planner built on guesses is a worse outcome than no physical planner at all.

## What we should take from this document

Ten phases, each with an exit that is a measurement rather than a judgement, and the engine whole at every point.

The structural phase, P3, deliberately changes nothing about performance, which is what makes it safe to do early and cheap to review.

The order puts encoded execution before compilation because the ablation says representation is worth an order of magnitude and code is worth tens of percent, which reverses the order a codegen-first reading of `../08-codegen.md` would suggest.

Reduction is the largest claim and it is scheduled sixth, because it is the phase most likely to slip and the phases before it pay off on their own.
