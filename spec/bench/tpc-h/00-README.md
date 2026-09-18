# TPC-H in rudb-bench

Written 18 September 2026, against rudb 0.3.33 and the `rudb-bench` working tree of the same date.

This directory specifies the second of rudb's seven benchmark suites all the way down: where the data comes from, which queries are run, how an answer is decided to be right, what is measured, what the numbers are compared against, and exactly which files in `rudb-bench` have to change. It is the measurement half of the work whose engine half is [`../../graph/`](../../graph/) and [`../../engine/08-join.md`](../../engine/08-join.md).

It exists because of one line in `crates/rudb-bench/src/engine.rs`:

> every join in rudb is a nested loop and TPC-H is twenty two of them, so this would be a timing of a hang

That refusal is correct today and it is the wrong shape for tomorrow. A whole-suite refusal means the suite produces nothing, which means the join work has no measurement in front of it, which means the first honest TPC-H number this project produces will arrive after the join is built rather than before. Document 01 is about that, document 07 is the fix, and the fix is worth stating up front: **the harness has to be able to measure a suite the engine is bad at, including a suite the engine cannot finish.** A benchmark apparatus whose output is "refused" is an apparatus that only works once the work is done.

## Why TPC-H is the suite that matters next

ClickBench is one table and forty three queries and no joins. It is where rudb's performance work has been, it is where `../../perf/` measured a scan that is over half the runtime, and it is a workload that says nothing at all about whether this engine can execute a join. TPC-H is eight tables, twenty two queries, and a join graph on every one but two. It is the smallest standard workload that exercises the thing the engine most conspicuously cannot do.

It is also the workload the goal document commits to a number on: 10x on TPC-H at SF100 total runtime, from `../../02-the-goal.md`. That number has no measurement behind it in either direction, which is unusual in this specification series and is the gap this directory closes first.

The honest caution goes here rather than in a footnote. TPC-H is not a good join benchmark in the way JOB is. Its schema is a clean snowflake, its joins are almost all primary to foreign key with high match rates, its filters are mostly on the small tables, and the GRainDB authors said in print that a structure very like the one `../../graph/` specifies should not be expected to win much on it. Being fast on TPC-H is necessary and it is not sufficient, and `../../graph/09-measurement.md` section 9.7 is where the suites that test the rest live.

## The documents

| | |
| --- | --- |
| [01-where-we-are.md](01-where-we-are.md) | The refusal, what is actually in the harness today, and what runs and does not |
| [02-the-data.md](02-the-data.md) | dbgen, the three scale factors, provenance, the Parquet and native forms, and why there is no sampling |
| [03-the-queries.md](03-the-queries.md) | All twenty two, classified by join shape, with what each one demands of the engine |
| [04-the-answers.md](04-the-answers.md) | Correctness: the reference, decimals, tie-breaking under `LIMIT`, and what a mismatch does |
| [05-the-measurement.md](05-the-measurement.md) | The protocol, the per-operator attribution, the join counters, timeouts and memory |
| [06-the-baselines.md](06-the-baselines.md) | What we compare against, why there is no public board to copy, and how the board gets built |
| [07-the-harness.md](07-the-harness.md) | The changes to `rudb-bench`, file by file, in the order they can be made |
| [08-the-gate.md](08-the-gate.md) | What each milestone has to show, and the regression gates that keep it |
| [09-open-questions.md](09-open-questions.md) | Six things this specification does not settle |

The reporting rules in `../../15-rudb-bench.md` govern everything here and are not restated. Where this directory adds a rule it says so and says why.
