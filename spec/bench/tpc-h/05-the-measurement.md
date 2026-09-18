# 5. The measurement

The protocol in `../../15-rudb-bench.md` governs and is not restated: a fresh process per run, one cold run then five hot, the median and the interquartile range, per-child CPU and peak resident set, raw results retained, and a list of reasons a number may not be published printed underneath the table. What is here is what TPC-H adds.

## 5.1 The three scales are three results

They are not averaged, not combined and not reported in one column. SF10, SF100 and SF1000 answer different questions and an engine can be good at one and bad at another, which is the entire reason `src/suite.rs` lists three.

The one number the goal document commits to is SF100 total runtime against DuckDB. Everything else is diagnosis.

## 5.2 The total, and what it hides

Report both the sum of the twenty two medians and the geometric mean of the per-query ratios, and lead with the sum because it is the one the goal is stated in.

Report the *maximum* per-query ratio prominently next to both. `src/suite.rs` already says this for JOB, the failure mode being measured is one catastrophic plan and a mean is exactly the statistic that hides one, and it is true for TPC-H too, more so while there is no join reordering. A suite where twenty one queries are 12x and one is 0.3x has a good sum and a broken engine.

## 5.3 Per-operator attribution

The ClickBench work made the runtime legible by attributing it: at ten million rows, FileScan 28.198s and 54.8 percent, Aggregate 13.427s and 26.1 percent, Filter 5.069s and 9.9 percent, TopN 4.525s and 8.8 percent, Project 228ms and 0.4 percent. Every operator writes what it cost.

TPC-H inherits that and adds Join as a first-class row, which on this suite should dominate the way FileScan dominates ClickBench. The per-operator table is reported per query, not only per suite, because on TPC-H the interesting fact is usually about one query.

## 5.4 The join counters

The per-operator time is not enough here, because two joins with the same wall time can be doing wildly different amounts of work. Every join operator records:

- **Build rows and build bytes**, or zero for a link join, which is how a link join is recognised in the output without trusting the plan label.
- **Probe rows**, meaning rows entering from the probe side.
- **Output rows.**
- **The selectivity**, output over probe, which is the number that says whether the join was worth doing where it was done.
- **Which algorithm**, and for the ones not chosen, the reason, per `../../graph/06-the-optimizer.md` section 6.7.

And per query, one derived number that is worth more than any of them: **the sum of all intermediate cardinalities divided by the result cardinality.** That is the quantity Yannakakis' bound is about, it is what a reduction is trying to lower, and it is comparable across engines and across scale factors in a way that wall time is not. A plan whose intermediates total a thousand times its output is a bad plan regardless of how fast the hardware ran it, and this is the column that says so.

## 5.5 The reduction counters

When the graph layer is on, per reduction edge: rows in, rows out, the removal fraction, wall time, and whether the runtime gate of `../../graph/06-the-optimizer.md` section 6.5 stopped it early. Per scan: parts skipped by the link's zone map, and rows rejected by the bitmap before decoding.

Last-level cache misses during the reduction phase, where the platform provides them, because `../../graph/11-open-questions.md` section 11.1 says that is the number the design's main risk turns on and a design risk without a counter behind it is a hope.

## 5.6 Timeouts

Every query gets a wall-clock timeout, defaulting to five minutes at SF10 and scaling with the scale factor, settable per run. A query that exceeds it is killed, its row says `timeout` with the limit, and the run continues.

This is the change that replaces the whole-suite refusal. A suite where one query can hang the process is a suite nobody runs unattended, and the refusal in `src/engine.rs` exists precisely because there was no bounded way to record "this does not finish". With a timeout, "this does not finish" is a result: it is recorded, it is dated, it is attributed to a query, and the day it becomes a number instead, the improvement is measurable against something.

A timeout invalidates the suite total the same way a wrong answer does, per document 04 section 4.6. The table shows two of twenty two completing and no total.

## 5.7 Memory

Peak resident set per query, per child process, as the ClickBench audit already measures it.

TPC-H needs one addition that ClickBench did not: a **memory limit** as part of the configuration, set and reported. An engine that answers Q9 in four seconds using sixty gigabytes and an engine that answers it in six using four are not comparable, and the only way to make them comparable is to run both under the same limit and record which ones failed. The limit for a published SF100 run is stated in the report header; unlimited is a valid setting and is also a statement.

Issue #735 says rudb currently holds about three times the memory it charges against its own limit, which means rudb's memory limit is not currently a limit. Until that is fixed, the reported number is the measured peak RSS of the child, which is a true measurement of a thing regardless of what the engine believes about itself.

## 5.8 Threads

Reported as cores used, hot, per the ClickBench precedent, where the table showing DuckDB at 9.01 and rudb at 0.99 at ten million rows was the single most informative line in that whole report.

TPC-H at SF100 on a many-core machine is a parallelism benchmark whether or not anyone intends it to be, and a single-threaded engine against a thirty-two-thread DuckDB is measuring the scheduler rather than the join. Runs are therefore reported at both the full thread count and at one thread, and the one-thread column is the one that isolates the algorithmic work this specification is about. Issues #512 and #510 say the threading has known costs; the one-thread column is how progress on the join is visible through them.

## 5.9 What a TPC-H report contains

Header: machine, engine versions, scale factor, data provenance and manifest hash per document 02 section 2.2, corpus form per section 2.5, thread count, memory limit, timeout, and for rudb the list of graph sections that existed.

Then: load time, load CPU, peak load RSS and on-disk bytes per engine per form. The twenty two query rows with cold, hot median, IQR, peak RSS, answer status, and the intermediate-over-output ratio. The per-operator breakdown. The join counters. The reduction counters where applicable. The totals, with the maximum ratio next to them. And the reasons list, which document 04 and section 5.6 both feed and which is not expected to be empty for a long time.

## 5.10 What is not measured

Load throughput as a headline. TPC-H load is measured because it has to be reported, and an engine that loads slowly and queries quickly is a different product from the reverse, but the goal document's number is a query number and the load column is context.

Compilation, planning and binding time separately from execution, for now. It should be separated eventually, because a suite of twenty two queries at SF0.01 is almost entirely planning time and that is a useful measurement of the optimizer, but it is a different measurement from this one and mixing them produces a column nobody can interpret.

Anything resembling an audited TPC-H result. This is a TPC-H-derived workload run for engineering purposes, the refresh functions are not run, and every report says so, because the TPC's rules about what may be called a TPC-H result exist and this is not one.
