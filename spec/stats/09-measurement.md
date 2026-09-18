# 9. Measurement

## 9.1 The claims

**S1. It fits.** Total statistics section bytes are under two percent of a native file's column bytes, on TPC-H at SF100 and on ClickBench at a hundred million rows. Killed by measuring the file.

**S2. It is cheap to build.** Statistics construction adds under ten percent to native write time and under ten percent to load peak RSS, measured against the same commit with the sections off. Killed by the load column, which `../15-rudb-bench.md` already requires be reported beside every runtime result.

**S3. It changes no answer.** The whole ablation, every suite, every commit. Not a performance claim, the correctness claim the rest of the directory rests on.

**S4. It makes the estimates better.** The q-error distribution on CEB and JOB, and the fraction of plan decisions made on `Exact` or `Certified` rather than `Estimated` or `Unknown`. Killed by the distribution not moving.

**S5. It makes queries fast that have no joins in them.** ClickBench, forty three queries, one table, zero joins. This is the claim that answers the question this directory was asked, and it is the one that fails most visibly if the work goes only into the join. Killed by the ClickBench total not moving.

**S6. Feedback changes nothing it is not allowed to change.** A read-only workload run twice produces byte-identical plans with identical provenance. Killed by a diff.

## 9.2 A rule that shapes the implementation

**Every rule in document 05 is behind its own setting.**

Not one master switch, one per rule. Aggregate presizing, direct-addressed grouping, validity-free kernels, filter ordering, top-n seeding, narrowed arithmetic, join elimination, each independently disable-able.

The reason is the accounting: a directory that ships twenty rules and reports one total cannot say which of them earned anything, and the twenty-first gets added on the strength of a number that the third one produced. A rule that cannot be turned off cannot be measured, and a rule that has not been measured individually is not known to be worth its complexity. The per-rule setting is also what makes a bisect possible when one of them turns out to be wrong.

`rudb-bench`'s `sweep` command already runs one suite once per registered implementation of one seam with everything else held fixed, which is exactly this apparatus.

## 9.3 The ablation that runs on every commit

`statistics = off`, every consumer gets `Unknown`, every operator takes the path it takes today, against the engine as it comes, on the SQL logic corpus and on the SF0.01 TPC-H corpus and on a reduced ClickBench.

Answers must match exactly. Not similar: identical, modulo the tie rules `../bench/tpc-h/04-the-answers.md` section 4.4 already defines for `LIMIT` with a non-total order.

This is the same mechanism as `../graph/09-measurement.md` section 9.2 and `../bench/tpc-h/07-the-harness.md` step seven, and it should be one implementation of one idea with three switches, not three implementations.

## 9.4 The per-rule table

The deliverable of every milestone in document 10 is a table with one row per rule: the suite, the queries it fired on, the median delta on those queries, the delta on everything else, and the space and build cost attributable to the statistic it consumes.

A rule whose "everything else" column is negative is a rule that made the engine slower where it did not help, which happens, filter reordering on a query with two cheap conjuncts costs the reordering and buys nothing, and the response is a threshold, not a shrug.

## 9.5 Estimate quality, reported as a distribution

q-error, the maximum of estimate over actual and actual over estimate, as a distribution over a benchmark and never as a mean, because the tail is the thing that causes the catastrophe. `../planner/06-cardinality-and-cost.md` section 06.5 already requires this on CEB; this directory is what supplies the numbers that move it.

Reported alongside it, and new here: **the class histogram.** For a whole suite, what fraction of the cardinality decisions the planner made were `Exact`, `Certified`, `Estimated`, `Unknown`. That number is the direct measurement of whether this directory is doing its job, it is cheap to collect, and it is more diagnostic than q-error for the first several milestones, because early on the estimates will be bad for the boring reason that there are none.

`EXPLAIN ANALYZE` prints estimate, actual and ratio per operator, which document 06 section 6.8 requires and which is how a single bad plan gets diagnosed rather than a suite.

## 9.6 The two costs that are easy to miss

**Cold open.** Opening a file must not get slower, because document 04 section 4.2 says nothing is read at open, and that is a claim with a number attached. Measured as process start to first row on a trivial query over a large file, before and after.

**Planning time.** Document 04 section 4.3 permits synchronous statistics I/O in the planner, and that decision is defensible and could be wrong. The measurement that catches it is the SF0.01 TPC-H suite, where twenty two queries over eleven megabytes is almost entirely planning, plus a ClickBench run with `--rows` small. If plan time per query grows materially, section 11.2 is the fallback.

These two are the reason this directory reports latency as well as throughput. Every other number here is about large queries; these two are about whether the small ones got worse, and an embedded database runs an enormous number of small ones.

## 9.7 What a statistics report contains

Machine, commit, corpus manifest, and the settings state for every rule in document 05.

Then: the space table per suite per table, the load table, the per-rule delta table, the q-error distribution, the class histogram, cold open and plan time, and the ablation's answer-comparison result, which is pass or fail and is not a number.

Plus, for any run with feedback on: the statistics generation, the number of observations committed, and the list of keys disabled by oscillation. Per document 06 section 6.7, no published number comes from a run with tier 2 on unless the header says so and the pinned run is reported beside it.

## 9.8 What would falsify the directory

Stated plainly, because a specification that cannot be wrong is not a specification.

If S1 and S2 hold but S4 and S5 do not, the statistics are cheap, accurate and consulted, and nothing gets faster, then the engine's costs are elsewhere and this work should stop at the point where the numbers are collected and reported, which is still worth having for `EXPLAIN` and for the join.

If S3 ever fails, everything stops until it passes, because a wrong answer produced quickly is the one outcome this project cannot ship.

If S6 fails, tier 1 is reduced to tier 0, observe and report, write nothing back, which costs the directory its third layer and keeps its first two intact.
