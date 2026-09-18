# 5. Every query

Document 02's catalogue is indexed by statistic. This one is indexed by operator, and it is the accounting that justifies the bytes: a statistics layer built for joins would be paid for by every table and collected by a fifth of the queries, and the point of this document is that the same numbers make scans, filters, aggregates, sorts and memory reservation better, which is where most of the runtime in this engine actually is.

The ClickBench profile is the reason to believe that: at ten million rows, FileScan was 54.8 percent of the runtime, Aggregate 26.1, Filter 9.9, TopN 8.8 and Project 0.4. A join does not appear on that list at all. An engine that gets statistics only into the join has improved nothing on the workload it has been measuring for a year.

## 5.1 The principle behind the individual rules

**Where a decision's cost is asymmetric, use a bound and not a point estimate.**

Under-reserving memory spills; over-reserving starves. Under-sizing a hash table rehashes; over-sizing wastes a cache. A point estimate says 0.125 and is wrong in an unknown direction. A certified statistic says "between 0.11 and 0.14" and lets the operator pick the end of the range whose failure it can afford. This is what the `Certified { bound }` class in document 04 section 4.1 is *for*, and it is the single most valuable difference between this design and one that stores the same numbers without their proofs.

## 5.2 Scan

The largest consumer, because it is the largest cost.

**Part skipping** already works from zone maps and is measured: ClickBench q37 went from 999,975 rows to 247,265 and from 11.05 ms to 4.36 ms. Nothing here replaces it.

**Stripe skipping on equality**, which zone maps do badly. A predicate `c = 'x'` against a column whose values are not clustered cannot be excluded by a minimum and a maximum, and is excluded exactly by a per-stripe sketch when the sketch is in its `is_exact` regime, a stripe whose complete value set does not contain `'x'` is skipped with certainty. This is the stripe-level equivalent of a Bloom filter and it is already paid for.

**Decode choice.** Whether to decode a dictionary column into values or to run the predicate against codes is a decision about the dictionary's size against the surviving row count, and both numbers are exact.

**Reading nothing at all.** Four query shapes are answerable from metadata and one already is:

- `COUNT(*)` from the row count.
- `COUNT(c)` from the row count minus the null count, which is exact per stripe and is why this is already free.
- `MIN(c)` and `MAX(c)` from the merged zone maps, when the bounds are exact values rather than widened bounds, which the summary's flag says.
- `COUNT(DISTINCT c)` from the dictionary when the class is exact, and **only** then. An estimated distinct count may never answer this query, and the class flag of document 03 section 3.3 exists so that the two cannot be confused by a caller.
- The certified top-k group-by that `../storage-v3/11-certified-frequency-synopses.md` already specifies, with its proof obligation intact.

Every one of these keeps its fallback. That is the invariant of document 03 section 3.1 seen from the consuming end.

## 5.3 Filter

**Ordering the conjuncts** by selectivity divided by evaluation cost. Both halves need numbers: the selectivity from quantiles, sketches or the sample; the cost from the expression's shape, where a `regexp_matches` is not a comparison. Today conjuncts are evaluated in the order written, which is the order a human found readable.

**Proving a predicate empty or total.** A range predicate outside a column's minimum and maximum yields no rows and the scan is replaced by an empty result. `c IS NULL` on a column with a zero null count is `false` and folds away. Both are exact, both are free, and both fire more often than they sound like they would, the second one especially, because analytic schemas are mostly non-null and query generators emit null checks anyway.

**Choosing the output representation.** A filter that keeps 90 percent of its rows should produce a selection vector over the existing vector; one that keeps 2 percent should materialize a compacted one. That is a selectivity decision with a real constant factor behind it and it is currently made by a fixed rule.

## 5.4 Aggregate

Twenty six percent of ClickBench, and the operator with the most to gain.

**Presizing the hash table** from the distinct count of the grouping key. A hash aggregate that grows by rehashing pays for every row it has already inserted, repeatedly. The distinct count is exactly the number that makes this a single allocation, and for a low-cardinality key it is *exact* out of `is_exact` rather than estimated.

**Choosing direct addressing over hashing.** When the grouping key's range is small and dense, which the summary's minimum, maximum and distinct count establish exactly, the aggregate is an array indexed by the key, not a hash table. That is a different algorithm with a different constant factor, it is the same mechanism `../graph/05-execution.md` section 5.6 uses to make a backward traversal a direct-addressed group-by on a row id, and the statistics are what let the planner reach for it on an ordinary `GROUP BY` over an ordinary integer column.

**Partition count** for the partitioned aggregation of `../perf/06-partitioned-aggregation.md`, from the distinct count and the thread count.

**Eliminating the aggregate.** A `GROUP BY` on a column with the distinctness flag set produces one row per input row, so the grouping is a projection. A `DISTINCT` on the same column is a no-op. Both are exact facts and both remove an operator rather than speed it up.

**Skew.** The frequency leaders say whether a handful of groups dominate, which is what decides whether a parallel aggregate's partitions will be balanced. This is the same number `../planner/09` section 09.5 wants for the join's build skew, computed once.

## 5.5 Sort and top-n

Eight point eight percent of ClickBench, and every TPC-H query ends in an `ORDER BY`.

**Not sorting.** Sortedness is exact and is recorded per column. A sort on an already-sorted column is a scan; a merge over sorted stripes is a merge and not a sort. Neither is possible without the fact being persisted, and it is one flag.

**Range partitioning without a sampling pass.** A parallel sort partitions by value range, and picking boundaries normally costs a sampling pass over the input. The quantile summary is those boundaries, already computed, with a stated ε, and the ε is the right input, because an unbalanced partition costs the maximum partition's time rather than the average's.

**Seeding a top-n threshold.** A `LIMIT 10 ORDER BY c DESC` maintains a heap and rejects everything below its current threshold. Starting that threshold at the quantile summary's estimate of the ninetieth-something percentile instead of at negative infinity means the first stripe rejects most of its rows instead of inserting them all and evicting them. The seed must be a *safe* bound, one the certificate says cannot exclude a qualifying row, which is exactly the difference the class flag encodes, and it is why this optimization is available at all rather than being a heuristic that sometimes returns wrong answers.

**Spill sizing** from the exact value widths, which is section 5.7.

## 5.6 Join

Covered by `../planner/06-cardinality-and-cost.md`, `../planner/07-join-ordering.md` and `../graph/06-the-optimizer.md`. Listed here only for the two things this directory supplies that they assume:

The **exact** output cardinality of a link join, from the relationship's recorded counts (document 07), which is a number no cost model produces.

The **class** on every other join's cardinality, so that a join order chosen on an estimate and a join order chosen on an exact count are distinguishable in `EXPLAIN` and in a post-mortem.

## 5.7 Memory reservation and spilling

Rows times width. The row count is an estimate; the width is *exact*, total bytes and maximum bytes per column are in the summary, and multiplying an exact width by a bounded count gives a bounded reservation, which is a strictly better input than the current path has.

This matters more in rudb than it would elsewhere because issue #735 says the engine holds about three times the memory it charges against its own limit. A reservation built from exact widths is not a fix for that, and it is the half of the problem that statistics can fix: knowing what a hash table of *n* rows of this schema costs is arithmetic once the widths are exact.

The asymmetry rule of section 5.1 applies at its sharpest here. Spilling is expensive and starving other operators is expensive, and the two failures are not symmetric, so the operator reserves at the bound whose failure it prefers, and says which in `EXPLAIN`.

## 5.8 Parallelism

How many rows a worker takes at a time is a function of row width and total rows, and both are exact. The current fixed morsel size is right for an average row and wrong for a sixteen-byte one and a two-kilobyte one, in opposite directions.

Whether to parallelise at all is a function of the estimated surviving rows, issues #512 and #510 say the threading has measurable fixed costs, and a query over four thousand surviving rows should not pay them. That decision is being made today without a number.

## 5.9 Strings

**`LIKE 'prefix%'` becomes a range predicate**, which the zone maps and the quantiles can then prune with. The rewrite is a planner rule; what makes it worth doing is that the statistics exist to exploit it.

**`text_extremes` already persists** the smallest and largest string per column, which is what lets the above prune at all.

**Kernel selection** from the exact maximum value width: a column whose longest value is twelve bytes takes a different path from one whose longest is two kilobytes, and the flavour choice `../engine/12-adaptivity.md` section 12.4 describes needs a number to choose on.

## 5.10 Expressions and codegen

**Validity-free kernels.** A column with an exact zero null count needs no validity mask, no null branch, and no combination of masks downstream. This is the single most mechanical win in the document: the null count is already exact and already persisted, and the arithmetic, comparison and aggregate kernels all have a branch that is provably dead. `for_each_layout!` makes adding the arm exhaustive rather than hopeful.

**Narrower arithmetic.** A `DECIMAL(15,2)` column whose exact minimum and maximum fit inside an `i64` can be summed in `i64` and widened at the end, instead of in `i128` throughout. On TPC-H that is `l_extendedprice`, `l_discount`, `l_tax` and every money aggregate in the suite, and it is correctness-preserving only because the bound is exact, which is the third time in this document that the class flag is what makes an optimization legal rather than merely attractive.

**Constant folding with statistics** is where this gets dangerous and needs a hard rule: a fold that depends on a statistic is legal only when the statistic is `Exact`. Folding on a `Certified` bound is legal only when the certificate's proof obligation is discharged in the plan and the fallback survives. Folding on an `Estimated` value is never legal, in any circumstance, for any speedup. This is the rule that keeps the invariant of document 03 section 3.1 true, and it is the one a future contributor is most likely to break in good faith.

## 5.11 What none of this may do

Change an answer. Every rule above either removes work that provably produces nothing, chooses between two algorithms that produce the same rows, or sizes a structure. Document 09 section 9.3's ablation, every statistic returns `Unknown`, run the whole suite, compare, is what keeps that true, and it runs on every commit rather than at a milestone.
