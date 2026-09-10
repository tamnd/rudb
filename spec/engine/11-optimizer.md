# Layer nine: the optimizer

This is sub-milestone 2k. It is the first layer that does not make any operator faster. It makes the engine run fewer of them, and on a query with six joins that is worth more than every layer below it put together.

It comes ninth rather than first because an optimizer's job is to choose between plans by cost, a cost model is a model of the operators underneath it, and a cost model written against operators that are about to be replaced is a model that has to be rewritten with them. By 2k the operators are built and measured, and the cost model can be fitted to numbers rather than guessed.

## 11.1 What exists today

`crates/rudb-opt/src/lib.rs` is nine lines. It is a crate with a module doc.

There is no optimizer. A bound plan goes to the executor as written. There is no filter pushdown, which is why document 00 could say that the corpus timeouts were joins with no filter pushdown. There is no join reordering, which is why document 08 section 8.10 set the TPC-H target at a factor of two rather than a win. There are no statistics beyond what the catalog knows about row counts.

Every earlier document in this directory has deferred something to here, and this section is the list: build side selection from document 08 section 8.4, which is currently a heuristic on catalog row counts; the compaction decision from document 03 section 3.6; the late materialization decision from document 05 section 5.7; the grouping shape choice from document 07 section 7.4; the top-N threshold from document 09 section 9.5; and predicate transfer from document 08 section 8.6. Each of those is a plan-time decision made today by a rule of thumb, and each becomes a cost decision here.

## 11.2 The order the rewrites are worth doing in

Not alphabetically and not by elegance. By what they are worth on the benchmarks the project is measured against.

**Filter pushdown** first, because it is the largest and because everything else depends on it. A predicate evaluated at the scan reads fewer rows into every operator above it, and combined with the pruning from document 05 section 5.6 it reads fewer bytes off the disk. On TPC-H it is worth orders of magnitude on several queries. It includes pushing through joins, through aggregates where the predicate is on a grouping key, through unions and through subqueries, and each of those has a correctness condition that has to be checked rather than assumed. Pushing a predicate through the null-producing side of an outer join is the classic wrong answer.

**Projection pushdown** second, which document 05 section 5.6 already established as the difference between reading 20 GB and 200 MB on ClickBench. Part of it is in the scan already. The general version prunes columns everywhere, not only at the scan, which shrinks every intermediate.

**Subquery unnesting** third, because an un-unnested correlated subquery is a nested loop over the outer query and is asymptotically wrong in the same way the nested loop join was. Section 11.5 is about this.

**Join ordering** fourth, because it is the largest remaining factor and because it needs the statistics that section 11.3 builds.

**Constant folding, expression simplification and common subexpression elimination** fifth, because they are worth tens of percent rather than multiples, and because folding is required for correctness in a few places anyway, such as a constant `WHERE false` that should not read the table.

**Predicate transfer** last, because it needs the join graph, the statistics and the Bloom filter machinery from document 08 all to exist first.

## 11.3 Statistics, and the asset that already exists

Cardinality estimation is where optimizers are wrong, and the errors compound multiplicatively through a join tree, which is why the join order benchmark exists and why it is hard.

rudb has an unusual asset here. `crates/rudb-encoding/src/sketch.rs` is 508 lines of k-minimum-values sketch, built for the M1 multi-column encoding chooser, and its module doc explains that it was chosen over HyperLogLog specifically because the chooser needed to ask which values two columns share and not only how many distinct values each has. It has `union`, `jaccard`, and a `dependence` function over pair hashes.

That is a set-intersection-capable sketch already computed per column and per block by the encoder. It is exactly what a join cardinality estimator wants, because the size of a join is a function of how many key values the two sides share, which a min-max histogram cannot express and which Jaccard over KMV sketches estimates directly. Most optimizers cannot do this because they only keep histograms and distinct counts, and then they assume independence and are wrong.

The correlation term is the other half. The `dependence` function over pair hashes measures how much two columns in the same table travel together, which is exactly the independence assumption that makes multi-predicate selectivity estimates wrong. A query with `WHERE country = 'DE' AND language = 'de'` has an actual selectivity far higher than the product of the two, every textbook optimizer gets it wrong, and the encoder already computed the number that fixes it.

So the statistics layer is: per-block minimum, maximum and null count, which the scan already keeps for pruning; per-column KMV sketches, which the encoder already builds; and pairwise dependence for column pairs that the chooser already evaluated. What has to be built is the plumbing that makes them reachable from the planner and the estimator that uses them, not the sketches themselves.

The honest caveat: sketches on the base table say nothing about the selectivity of a predicate whose result then feeds a join, and the error still compounds. This does not solve cardinality estimation. It makes the base cases much better and the correlation term much better, which is where a large share of the catastrophic errors come from.

## 11.4 Join ordering

For queries up to about a dozen relations, dynamic programming over connected subgraphs, which is DPccp and then DPhyp from Moerkotte and Neumann when there are hyperedges from non-inner joins and complex predicates. This is exact given the cost model and it is fast enough at that size.

Past that, DP is exponential and something has to give. The standard answer is a greedy or randomized algorithm above a threshold, with the threshold set by planning time rather than by relation count, because a query with twenty relations and a linear join graph is easy and one with twelve and a dense graph is not.

The parent spec's second axis is the per-query floor, and an optimizer is where a floor goes bad. So there is a planning time budget, it is enforced, and exceeding it falls back to the greedy order rather than continuing. A query that takes four hundred milliseconds to plan and two hundred to run is a query the optimizer made worse, and without a budget that case is invisible.

Cross products are considered rather than excluded, because a small filtered dimension table crossed with another small one before joining the fact table is sometimes the right plan and excluding cross products makes it unreachable. They are considered under a size bound so the search does not blow up.

## 11.5 Subquery unnesting

The reference is Neumann and Kemper's unnesting of arbitrary queries. The technique turns a correlated subquery into a dependent join and then pushes the dependent join down until the correlation is resolved, at which point it becomes an ordinary join. It works for arbitrary nesting rather than for a catalogue of recognized patterns, which is what makes it worth implementing properly rather than as a set of special cases.

The special cases are still worth having on top, because they produce better plans than the general algorithm for the shapes that are common. `EXISTS` becomes a semi join, `NOT EXISTS` becomes an anti join, `IN` becomes a semi join, `NOT IN` becomes an anti join with the null rule from document 08 section 8.3, and a scalar subquery becomes a single join with the more-than-one-row check that document 08 requires. All five of those already have join kinds waiting for them, which is why the eight kinds were built before the optimizer that produces them.

`NOT IN` deserves repeating because it is where every database has been wrong at some point, and the correct behaviour depends on nulls in the subquery result in a way that is not obvious. It gets its answer from DuckDB through the corpus and not from a reading of the standard.

## 11.6 The architecture, and what it is not

Not Cascades. Not a general top-down memo-based transformation engine with a rule set and a search.

The reason is the per-query floor again, and the reason the trade is acceptable is that the wins in section 11.2 are almost all rewrite wins rather than search wins. Filter pushdown, projection pushdown and unnesting are transformations that are always good and never need to be costed, and the one place that genuinely needs search is join ordering, which has its own dedicated algorithm.

So the architecture is a fixed sequence of rewrite passes over the plan, each of which is a function from plan to plan, plus a dedicated join ordering phase that does search. That is DuckDB's architecture, it is much simpler, its planning time is predictable, and it is easy to test because each pass is independently checkable.

The cost of that choice is that a rewrite whose benefit depends on context cannot be decided by the framework and has to decide for itself. The deferred decisions listed in section 11.1 are exactly that shape, and each one gets a small local cost comparison rather than being entered into a global search.

Every pass can be turned off individually. That is a debugging feature and a testing feature and it is the mechanism section 11.8 uses for its strongest correctness property.

## 11.7 Predicate transfer as a plan phase

Document 08 section 8.6 specified the mechanism and deferred the policy. The policy is here.

Predicate transfer runs as a phase after join ordering, because it needs the join graph, and it inserts filter-building and filter-applying steps into the pipeline dependency graph from document 10 section 10.3. The forward and backward passes are extra pipeline dependencies, which the scheduler already knows how to express.

Robust Predicate Transfer's contribution is deciding when not to. The decision inputs are the estimated selectivity of each filter, which comes from the sketches in section 11.3, and the cost of building and applying, which is known from the microbenchmarks in document 08 section 8.10. A filter that is estimated to reject less than some fraction is not built, and the fraction is measured rather than chosen.

This is the M4 milestone item in the parent spec and it is the last piece of this directory's plan that has a large expected factor attached to it on TPC-H and on the join order benchmark.

## 11.8 The test gate

**The optimizer must never change an answer.** That is the single strongest property available and it is enforced directly: every corpus query runs with the optimizer fully on and fully off, and the results are compared. With more than four thousand corpus files that is a large amount of coverage for one property, and it catches the outer join pushdown bugs and the unnesting null bugs that are otherwise found by users.

Each pass individually: every corpus query runs with exactly one pass enabled and the result compared to none enabled. That localizes a failure to a pass, which turns a day of bisecting into a minute.

Plan stability: `EXPLAIN` output for a fixed set of queries against a fixed set of statistics is compared against a committed expected plan. This is how a plan regression becomes visible, and it is the only way to notice that a cost model change made query seventeen choose a worse join order while making everything else faster. The committed plans are regenerated deliberately and the diff is reviewed, which makes a plan change a decision rather than an accident.

Planning time is asserted, per query, against a budget, so that the per-query floor cannot regress silently.

Estimation quality is measured rather than tested, because a cardinality estimate is not right or wrong, it is off by a factor. The measurement is the q-error distribution over the join order benchmark, which is the standard way to report this, and it is published alongside the timings because an engine whose estimates are bad but whose execution is fast is a different engine from one where both are good, and the difference shows up on workloads that are not in the benchmark.

## 11.9 The benchmark gate

This is the layer where the three planning benchmarks come in, which document 02 section 2.3 said would happen when the optimizer arrived.

**The join order benchmark**, which is the standard measure of whether an optimizer's cardinality estimates are any good, run on real IMDB data with its skew and correlation intact. This is the benchmark the sketches in section 11.3 are supposed to win, and it is the one where the correlation term should show most clearly.

**CEB**, the cardinality estimation benchmark, which measures the estimates directly rather than through the runtime, and which produces the q-error distribution from section 11.8.

**TPC-DS**, which has ninety-nine queries with much more complex plan shapes than TPC-H, many correlated subqueries and many `GROUPING SETS`, and which is where the unnesting work in section 11.5 is measured. Running all ninety-nine correctly is itself a milestone, and it is likely to find compatibility gaps as well as performance ones.

**TPC-H SF100** again, where the target now becomes the real one. Document 08 set a factor of two behind DuckDB with the explicit reason that there was no optimizer. That reason is now gone, and the target at 2k is that rudb beats DuckDB on total CPU seconds across all twenty-two queries at SF100 on `server1`.

**ClickBench** should barely move, and that is the expected result rather than a disappointment. ClickBench queries are single table with no joins, so the optimizer has almost nothing to do on them, and a large ClickBench movement at this layer would mean something was wrong before.

## 11.10 Exit criterion for 2k

**A fixed sequence of independently toggleable rewrite passes covering filter and projection pushdown, expression simplification, constant folding and common subexpression elimination, plus arbitrary subquery unnesting with the five special-case shapes, plus DP join ordering with a hypergraph formulation and an enforced planning time budget with a greedy fallback, plus predicate transfer with its build-or-not decision, all driven by a cost model fitted to the microbenchmark numbers from layers three through nine and by cardinality estimates from the existing per-column sketches with the correlation term applied, with the six decisions deferred from earlier documents now made by cost and shown in `EXPLAIN`, with optimizer-on and optimizer-off answers identical across the whole corpus and per-pass as well, with committed plan stability baselines, with the q-error distribution published on CEB, with all ninety-nine TPC-DS queries running correctly, and with rudb beating DuckDB on total CPU seconds across TPC-H SF100 on `server1`.**

Named as deferred: a full Cascades search, which is rejected rather than deferred and section 11.6 gives the reason; adaptive re-optimization mid-query, which is document 12; and materialized view matching, which is not something this database has.
