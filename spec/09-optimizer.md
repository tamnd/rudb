# Optimizer

An optimizer's job on this workload is not to find a clever plan, it is to avoid a catastrophic one. On ClickBench the plans are nearly forced and the optimizer contributes little. On TPC-DS, JOB and CEB the difference between a good plan and a bad one is three orders of magnitude, and the reason bad plans happen is almost always cardinality estimation error compounding through a join tree. So this document spends most of its length on being robust to bad estimates rather than on producing better ones.

## 9.1 Shape

A fixed sequence of passes over the bound logical plan, each a pure function from plan to plan, then cost-based join ordering, then physical planning.

Every pass is individually disableable by name from a session setting. This is a testing feature first: the differential harness in document 14 bisects a wrong answer by disabling passes one at a time, and without it a miscompilation in the optimizer is found by reading code.

Every pass preserves a plan invariant checked in debug builds: types are consistent, column references resolve, correlated references are bound, and the output schema is unchanged. A pass that breaks the invariant fails immediately rather than producing a wrong answer three passes later.

## 9.2 Logical rewrites

The standard set, in roughly this order.

Expression simplification and constant folding. Null propagation, meaning an expression provably null in a filter position eliminates the branch. Comparison normalization so that later passes see one shape.

Subquery unnesting, which is the single most valuable rewrite in the set. Uncorrelated scalar subqueries become a single evaluation. Correlated subqueries become joins by the standard dependent-join elimination, which is Neumann and Kemper's algorithm and which DuckDB implements and which is not optional if TPC-DS is a target. `EXISTS` and `IN` become semi joins, `NOT EXISTS` and `NOT IN` become anti joins with the null semantics handled correctly, which is the part everyone gets wrong.

Filter pushdown through joins, aggregates and unions, down to the scan where it becomes a zone-map predicate. Projection pushdown, so that a scan reads only referenced columns, which on a 105-column table is the difference between 20 GB and 200 MB.

Join reordering preparation: flattening join trees into a hypergraph, identifying which joins are reorderable given outer-join semantics.

Common subexpression and common subplan elimination, including turning a repeated scan of the same table with the same filter into a single materialized node with two consumers, which is what makes TPC-DS's repeated date-dimension pattern tolerable.

Aggregate pushdown through joins where the functional dependencies permit it. Distinct elimination where a key constraint or an upstream aggregate already guarantees distinctness.

Limit pushdown, including into sorts to make them top-N and into scans to stop early.

Set operation rewrites, `UNION ALL` flattening, `INTERSECT` and `EXCEPT` to semi and anti joins.

Window function rewrites, including sharing a sort between windows with the same partitioning and turning a filtered ranking window into a top-N per group.

**Unnesting and pushdown are where a compatibility gap will show up first**, because DuckDB's exact behaviour on correlated subqueries with nulls and on `NOT IN` with nulls is subtle, and any difference is a wrong answer rather than a slow query. Document 14's corpus specifically targets this area.

## 9.3 Cardinality estimation

Base table cardinality is exact. Filter selectivity comes from per-column statistics: min, max, null count, distinct count estimate, and for the columns that justify it an equi-depth histogram plus a most-common-values list. These are the same statistics that drive zone-map skipping, per document 5.3, so they are maintained once.

Join cardinality uses the standard containment assumption with distinct counts, corrected by sketch-based estimation where the sketches exist.

**The design position is that these estimates are wrong and the plan must not depend on them being right.** Every serious study of cardinality estimation, from the original Join Order Benchmark paper onward, reports errors of several orders of magnitude on real multi-join queries with correlated predicates. Building a better estimator is a research program. Building a plan that survives a bad estimate is engineering, and it is what sections 9.4 and 9.5 do.

**Sampling for correlated predicates.** For a conjunction of predicates on one table, evaluate them against a stored sample of the table rather than multiplying independent selectivities, which is where the independence assumption does its worst damage. The sample is a fixed-size reservoir maintained per table and it is cheap to evaluate against.

## 9.4 Join ordering

Dynamic programming over connected subgraphs, DPhyp-style so that hypergraph edges from outer joins and complex predicates are handled correctly, up to a subgraph count limit. Past the limit, a greedy heuristic seeded by the DP result on the densest subgraph.

The cost model counts rows produced and bytes moved, weighted by whether a side is likely to fit in cache. It does not attempt to model everything, because a cost model consuming bad cardinalities in more detail is not more accurate, only more confident.

**Bushy plans are allowed.** Restricting to left-deep is a simplification that costs real performance on TPC-DS and JOB and there is no reason to accept it.

## 9.5 Robust Predicate Transfer

This is the mechanism that makes the join workloads good rather than merely acceptable, and it is scheduled in M4.

**The idea.** Yannakakis's algorithm runs semi-join reductions over an acyclic query so that every relation is reduced to exactly its contribution to the result before any join runs, giving a worst-case optimal result for acyclic queries. It was long considered impractical because real queries are not acyclic and because the semi-joins cost more than they save on easy queries. The 2024 Robust Predicate Transfer line of work makes it practical: build a spanning tree of the join graph, do a forward pass and a backward pass of bloom filters along it, and accept that the reduction is approximate because the bloom filters have false positives. The joins then run on reduced relations.

**Why it matters more than a better estimator.** The reduction happens at runtime on real data, so it does not care what the optimizer estimated. A query whose optimizer thought a filter was selective and was wrong still gets its relations reduced correctly, because the reduction is measuring rather than guessing. The published results on JOB and CEB show large improvements and, more importantly, show a large reduction in the variance across queries, which is the thing that actually hurts users.

**The two design choices that matter.** Which relation roots the spanning tree, where LargestRoot is the published heuristic and is what we implement. And which joins are safe to include in the transfer, where SafeSubjoin identifies the subset for which the reduction is guaranteed not to change semantics in the presence of outer joins and aggregates. Getting the second one wrong produces wrong answers, so it is conservative by default.

**When it is skipped.** Single-table queries, two-table joins where the transfer cost exceeds the benefit, and any query whose estimated total work is below a threshold. ClickBench gets nothing from this and should not pay for it.

## 9.6 Physical layout adaptation

This is the pass with no equivalent in any shipping engine and it is where the Bespoke OLAP result enters the design as engineering rather than as inspiration.

**The result being responded to.** Bespoke OLAP generates a whole database specialized to one workload ahead of time and measures 11.17x on TPC-H and 45.33x on CEB against DuckDB. Their ablation attributes essentially all of it to storage layout specialization at 12.35x, with code specialization contributing 1.26x. Their flat-storage variant, which specializes code but not layout, scores 0.57x on CEB, meaning it loses to DuckDB.

**Why we cannot do what they did.** They compile a database for a known workload ahead of time. A general database does not know the workload. Their result is an upper bound on what layout specialization is worth, achieved under an assumption we do not get to make.

**What we do instead.** The physical planner chooses, per scan, per column, which physical representation to produce, based on what the consumer does with the values. The same dictionary-encoded column can be scanned as codes, as decoded strings, or as codes plus a dictionary reference, and the right answer depends entirely on the consumer.

A `GROUP BY` on the column wants codes. A `LIKE` predicate wants FSST-compressed bytes and the compressed needle. An equality predicate against a constant wants a single code. A projection to the output wants decoded strings but only for surviving rows, so it wants codes until the very end. A join against another column sharing the dictionary wants codes. A join against a column with a different dictionary wants a translation table built once rather than a decode per row.

The pass propagates these requirements up from the consumers to the scan, resolves conflicts by cost, and annotates the scan. When two consumers of the same scan want different forms, the scan produces the cheaper one and a conversion node handles the other.

**And then the runtime overrides it.** Adaptive execution per document 7.9 can change the decision after observing real selectivity, because the planner's choice depends on a filter's selectivity estimate and that estimate is unreliable.

**This is the project's second-largest open question**, document 19 open question two: does runtime layout adaptation capture a useful fraction of the offline specialization win. The honest prior is that it captures some and not all, because a workload-specialized database can reorder and co-locate the data itself, which we cannot do at query time. If the answer at M3 is that it captures less than 2x, axis 2's target moves and the specification is amended.

## 9.7 Physical planning

Operator selection: hash join versus merge join versus nested loop; hash aggregate versus streaming aggregate on already-sorted input; sort versus top-N; whether a distinct becomes a group-by.

Parallelism: how many pipelines, how morsels are sized, where the exchange points are.

Physical layout, per 9.6.

**Everything downstream of physical planning is allowed to change its mind at runtime**, and the planner's output is therefore best understood as a starting configuration rather than a decision. This is stated explicitly because it changes how the planner should be written: it should be fast and reasonable rather than slow and exhaustive, since the runtime corrects its worst mistakes.

## 9.8 Explain

`EXPLAIN` shows the logical plan, the physical plan, and estimated cardinalities. `EXPLAIN ANALYZE` shows actual cardinalities, per-operator time, per-operator memory, the encoded-versus-decoded vector counts from document 6.7, which execution tier each pipeline ran at, and every adaptive decision with the observation that triggered it.

**Estimated and actual are printed side by side with the ratio**, because the ratio is the first thing anyone diagnosing a bad plan wants and computing it by hand is tedious.

`EXPLAIN` output is not a stable interface and is explicitly excluded from the compatibility guarantee in document 12.5, because matching DuckDB's explain text would freeze our optimizer to their operator names. It is stable enough for tests within one minor version.
