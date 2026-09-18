# 1. What has been tried

Statistics-and-feedback is one of the oldest lines in database research and almost none of it is new. The value of reading it is not novelty; it is that the failure modes are documented, and every one of them has been paid for by somebody else already.

Citations here are by title and venue. Where this project has not fetched a public copy, no URL is given rather than a guessed one.

## 1.1 The feedback systems that shipped

**LEO, DB2's LEarning Optimizer**. Stillger, Lohman, Markl and Kandil, *LEO: DB2's LEarning Optimizer*, VLDB 2001. The canonical design: instrument the plan, compare actual cardinalities against estimated ones at each operator, and store **adjustment factors** that correct the estimate for the next compilation of a similar predicate. The insight rudb takes is the one about where the correction is attached: not to a query, but to a *predicate on a column*, which is a fact about the data and generalises to other queries touching the same column.

The failure mode LEO's line documented is oscillation, a correction that makes the next plan worse, which produces a new correction that makes the one after that worse again, and the fix everywhere it has been deployed is damping and hysteresis rather than faster learning.

**Oracle's cardinality feedback and SQL plan directives.** Cardinality feedback re-optimizes on the second execution when the first execution's actuals differed materially from the estimates; plan directives persist the observation as "use dynamic sampling for this column group", which is again an instruction about *how to estimate a data property*, not a memorised plan. The recorded operational complaint about this family of features is plan instability: the same query behaving differently across executions is hard to support, which is exactly the objection in `../planner/09-runtime-filters-and-adaptivity.md` section 09.5.

**SQL Server's memory grant feedback**, shipped in 2017 and extended since. It is the most conservative and most successful member of the family, and it is the closest model for what document 06 specifies. Its properties are the design: it corrects **one scalar** (the memory grant) rather than a plan; it corrects in a direction with a known cost asymmetry (too little spills, too much starves concurrency); it **clips** the correction; and it **disables itself** when it detects oscillation between executions. A narrow feedback loop with a stated objective, a bound, and an automatic off switch.

**Adaptive query processing in general**. Deshpande, Ives and Raman's survey, and Avnur and Hellerstein's *Eddies* (SIGMOD 2000) as the extreme case where routing is per tuple. `../engine/12-adaptivity.md` already decided how much of this rudb builds, which is: one mechanism, bounded, within a query. Nothing here reopens it.

## 1.2 The synopses

**Misra-Gries**, *Finding Repeated Elements*. Already in the tree's design via `../storage-v3/11-certified-frequency-synopses.md`, and the reason that document matters more than its size suggests is the certificate: a value absent from the final candidate table occurs no more often than the number of decrement rounds, which converts a heavy-hitter sketch into something that can **prove** a top-k boundary and therefore answer a query instead of estimating one.

**KMV**. Beyer, Haas, Reinwald, Sismanis and Gemulla, *On Synopses for Distinct-Value Estimation Under Multiset Operations*, SIGMOD 2007. The family `crates/rudb-encoding/src/sketch.rs` implements. Its advantage over HyperLogLog for this project is the one its own module doc gives: the multi-column encoding chooser needed to know **which values two columns share**, not only how many distinct values each has, and KMV supports intersection and Jaccard while HLL supports cardinality and union. A join estimator wants the intersection.

Its second advantage is under-advertised and is load-bearing here: `Sketch::is_exact` is true when fewer than `k` distinct values were ever offered to it. Below that threshold the sketch is not a sketch, it is the complete set, which means a low-cardinality column gets an *exact* distinct count and an *exact* value set out of the same structure that gives a high-cardinality column an estimate. That is the exact-then-certified-then-estimated ladder falling out of a data structure rather than being imposed on one.

**HyperLogLog**. Flajolet, Fusy, Gandouet and Meunier, 2007, is not used, for the reason above, and is named so the decision is visible rather than accidental.

## 1.3 The estimation evidence

`../planner/06-cardinality-and-cost.md` section 06.1 covers this properly and is not repeated. Three points from it govern this directory:

- The error is unbounded in the estimate and roughly bounded in the cost model, so effort spent making numbers exact beats effort spent making the cost function sophisticated.
- The error is worst on joins of three or more relations, which is exactly where `../graph/`'s stored links make the number exact instead, a link join's output cardinality is the child row count minus the unmatched count, both recorded at build time.
- Underestimation is the dangerous direction, because the plans it produces are the ones that fall over rather than the ones that are merely slow.

## 1.4 The graph side

Kùzu's storage work and the columnar-storage-for-graph-DBMS line (PVLDB 14) both keep **degree information** as a first-class persisted structure rather than a derived one, because a graph engine's central plan decision, which direction to traverse, is a question about degrees and nothing else. Neo4j keeps a degree store for the same reason.

The transfer to rudb is direct and is document 07: a declared relationship's degree distribution is cheap to compute while the link is being built, it is exact, it is tiny, and it answers the two questions `../graph/06-the-optimizer.md` has to answer at plan time, how much does this join expand its input, and is the expansion uniform or skewed.

The second transfer is the **validity certificate**. A foreign key that has been *verified* rather than *declared* turns a join into a lookup that cannot fail, a semi-join into a no-op, and an outer join into an inner one. That is a metadata fact with an enormous execution consequence, and it is the graph layer's cardinality verification (`../graph/02-the-data-model.md` section 2.3) written down as a statistic other operators can read.

## 1.5 The bandit line, and where it already lives

**Piece of CAKE: Adaptive Execution Engines via Microsecond-Scale Learning**, 2026, already appears in `../engine/01-survey.md`, and `../engine-v2/` schedules it at F10 as `Policy::Adaptive`, a contextual bandit over the registered implementations at each seam, with a replay guarantee: an adaptive run records its choices and re-running with them pinned reproduces it.

That is important for this directory because it means **the project has already decided how a learned choice may behave**: at a seam, over alternatives that are all correct, with its choices recorded and replayable. Document 06 does not invent a policy mechanism; it supplies the reward signal to the one that is already specified, and it obeys the replay rule that mechanism already carries.

## 1.6 What rudb takes, and what it declines

**Takes: the certificate pattern.** From `../storage-v3/11`. Every statistic in document 02 is classified exact, certified or estimated, and a certified one names the proof obligation that lets an operator use it as an answer.

**Takes: correction attached to a data property, not to a query.** From LEO. Document 06 writes back observed selectivities and observed join cardinalities keyed by column and predicate class, not by query text, and a query-text cache is explicitly excluded.

**Takes: one scalar, clipped, with an off switch.** From memory grant feedback. It is the shape of every feedback rule in document 06, including the memory one, which is literally the same problem and where issue #735 says rudb currently mis-accounts by a factor of about three.

**Takes: degrees as first-class persisted metadata.** From the graph systems.

**Declines: learned cost and cardinality models.** For the product reason in `../planner/06-cardinality-and-cost.md` section 06.6, unchanged.

**Declines: mid-query re-planning.** `../planner/09` section 09.5 and `../engine/12-adaptivity.md`, unchanged.

**Declines: a plan cache keyed by query text that changes behaviour across runs.** Named here because it is the most tempting way to make a benchmark's second run fast, and the second run is not what a user experiences.
