# Statistics, metadata and feedback

Written 18 September 2026, against rudb 0.3.33.

`../graph/` makes joins fast by storing the join. This directory makes **every** query fast by storing what is true about the data, and by keeping a record of what execution observed, so that the second time the engine is asked a question it does not start from nothing.

It covers three layers and they are deliberately not one thing:

1. **What is written.** Statistics and metadata persisted in the single native file, as sections in the version 11 section table of `../graph/03-the-file-format.md`. Built once by the writer, which already has every value in its hands, rather than recomputed by every query that wants them.
2. **What is resident.** The in-memory statistics service: what is loaded, when, who pays for it, and the rule that no query ever waits on a statistic.
3. **What is observed.** The feedback, the reward, that execution produces: actual cardinalities, actual selectivities, actual degrees, actual memory. What is written back, what is allowed to influence, and what is forbidden from influencing because it would make a run depend on history.

## The thesis

**An estimate is a statistic that lost its proof.**

Most engines compute statistics in order to estimate, and estimation is where optimizers go wrong by orders of magnitude, Leis's result, restated in the ten-year retrospective, is that join estimates of three or more relations are routinely wrong by orders of magnitude in every system tested, and that the cost model's own error is small beside it.

rudb is in an unusual position to do better, and not because it is clever. It is because of three things that already exist in the tree:

- The writer sees every value. Stripe minimums and maximums, null counts and dictionaries are already exact, and `Reader::null_count` is exact per stripe by construction, which is why `COUNT(column)` over a whole table is already free.
- `../storage-v3/11-certified-frequency-synopses.md` established the pattern that matters most in this directory: a synopsis that carries **a certificate**, the exact leading counts plus `omitted_max`, an upper bound on everything not stored, can *answer* a query rather than estimate one, and falls back to a scan when the proof does not hold.
- `crates/rudb-encoding/src/sketch.rs` is 508 lines of k-minimum-values sketch with `jaccard` and `dependence`, built by the encoder for a different purpose, and it is exactly what a join estimator and a correlation term want. Today it is not persisted anywhere: the only mention of a sketch in `rudb-native` is a comment explaining why one would not do.

So the ladder this directory builds is **exact, then certified, then estimated, and never silently**. Every number handed to the planner carries which of the three it is, and `EXPLAIN` prints it, because a bad plan built on a guess and a bad plan built on an exact count are two different bugs.

## The scope rule: every query, not just the joins

The statistics are paid for by every table that gets written. So every query shape has to get something back, and document 05 is the accounting: which statistic changes which decision in which operator, for scans, filters, aggregates, distincts, sorts, top-n, string search, memory reservation and parallelism, not only for joins.

That rule has a sharp edge and it is the one worth stating first: **a statistic nobody's plan consults is a statistic that is not written.** Every entry in document 02's catalogue names the decision it exists for. If the decision goes away, so does the statistic.

## What this is not

**Not a second catalogue.** The catalogue is `crates/rudb-catalog`, the numbers live in the file, and `Rows` already exposes four of them, `top_frequencies`, `distinct_values`, `null_count`, `text_extremes`, with `None` for an in-memory table. This directory widens that interface; it does not build a rival to it.

**Not a learned optimizer.** `../planner/06-cardinality-and-cost.md` section 06.6 rules one out for a product reason, an embedded database has to be reasonable on the first query it ever sees, with no training phase and no model file, and this directory agrees and does not reopen it. What document 06 here adds is narrower and is bounded by a rule that a learned model cannot satisfy.

**Not a licence to make the same query run differently the second time.** `../planner/09-runtime-filters-and-adaptivity.md` section 09.5 explicitly forbids "a feedback loop that writes observed cardinalities back into a catalog for the next query". That prohibition is live, this directory's third layer appears to contradict it, and document 06 exists to resolve that head-on rather than to quietly overrule it. The short version: what may be written back is a **verified fact about the data**, which changes the estimate into a measurement and keeps the plan a function of the data; what may not be written back is a **preference learned from timings**, which makes the plan a function of history. The first is in. The second is opt-in, off by default, and carries a replay guarantee.

## The documents

| | |
| --- | --- |
| [01-research.md](01-research.md) | What has been tried: LEO, cardinality feedback, memory-grant feedback, sketches, and what rudb takes from each |
| [02-the-catalogue.md](02-the-catalogue.md) | Every statistic, its exactness class, its size, its build cost and the decision it exists for |
| [03-the-file-format.md](03-the-file-format.md) | How they persist: the new section kinds, the certificates, the budget and the generation rules |
| [04-in-memory.md](04-in-memory.md) | The resident service: lazy loading, memory charging, the snapshot key, and never blocking a query |
| [05-every-query.md](05-every-query.md) | Operator by operator, which statistic changes which decision, the answer to "not just graph data" |
| [06-the-reward.md](06-the-reward.md) | The feedback loop, the determinism rule it obeys, and the prohibition it does and does not break |
| [07-graph-statistics.md](07-graph-statistics.md) | Degrees, fanout, validity certificates and reachability: what `../graph/` needs and what it gives back |
| [08-maintenance.md](08-maintenance.md) | Staleness, incremental update, `ANALYZE`, and what DuckDB compatibility requires here |
| [09-measurement.md](09-measurement.md) | The claims, q-error reporting, the statistics-off ablation, and the space budget |
| [10-milestones.md](10-milestones.md) | Seven, each with one exit measurement |
| [11-open-questions.md](11-open-questions.md) | Seven things this specification does not settle |

`../planner/06-cardinality-and-cost.md` owns the estimator and the cost model and is not restated here. This directory owns the **supply**: what exists, where it lives, what it costs, how stale it is, and how honest it is about itself.
