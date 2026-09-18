# 1. Where we are

Measured by reading the tree, 18 September 2026, `rudb` at 0.3.33 and `rudb-bench` at the same date. Nothing in this document is an estimate.

## 1.1 What the harness already has

More than one would guess from the fact that no TPC-H number exists.

`src/suite.rs` declares the suite: name `tpch`, twenty two queries, the eight tables by name, a data directory of `tpch100`, `comparable: true`, and a note saying the three scale factors are three different measurements, SF10 fits in cache on a large machine and measures the engine, SF100 is the standard comparison point, SF1000 exceeds memory on most machines and measures spilling.

All twenty two query texts are there, as one line each with the newlines and indentation removed, character for character from DuckDB's `extension/tpch/dbgen/queries`. The module comment explains why there is one text rather than one per engine: TPC-H is one specification with one meaning per business question, and a per-engine rewrite measures the rewrite. Every engine was asked whether it accepts that text against a real SF 0.01 corpus before the constant was written, and DuckDB, ClickHouse and DataFusion each answered all twenty two; Polars answers nineteen and the three absences are declared with their reasons. There are tests asserting that the text is one line, that every query has a shape description, and that only Polars is short of the set.

`src/data.rs` knows the suite has eight tables and refuses `--rows` against it, with the best paragraph in the file behind the refusal: keeping every hundredth `lineitem` row and every hundredth `orders` row keeps roughly none of the pairs that join, so every join query would come back nearly empty and the column would read as an engine that got a hundred times faster. Scaling TPC-H is what `dbgen -s` is for.

`src/answer.rs` already handles the decimal case TPC-H produces, an `avg` over a `DECIMAL(15,2)` where three engines print three different renderings, and carries the three real q01 answers at SF100 as fixtures.

`src/engine.rs` has all the engines and the ability plumbing.

## 1.2 What is missing

**The data.** `prepare` builds a path of `$RUDB_BENCH_DATA/tpch100/{table}.parquet` and fails with a message if it is not there. Nothing generates it. `needs` says "dbgen at SF10, SF100 and SF1000" and the suite's `directory` field is the single string `tpch100`, so the harness as written can only address one scale factor and it is the one whose name is baked into a constant. Document 02 and document 07 section 7.2 are the fix.

**The rudb column.** `Rudb::can_run` refuses the whole suite. Document 07 section 7.3 is the fix, and it is the most consequential change in this directory because it is what turns TPC-H from a thing that happens after the join work into the instrument the join work is done against.

**Per-query timeouts.** The refusal exists because a nested loop join on TPC-H does not finish, and the harness has no bounded way to record that. A suite where one query can hang the run is a suite nobody runs overnight. Document 05 section 5.6.

**The join attribution.** `../../perf/` built the per-operator time accounting that made ClickBench legible: at ten million rows, FileScan 28.198s and 54.8 percent, Aggregate 13.427s and 26.1 percent, and so on. The equivalent for TPC-H needs operator counters a scan-and-aggregate workload never needed, build rows, probe rows, output rows per join, and the ratio between the intermediate and the final cardinality, which is the number that says whether a plan was good. Document 05 section 5.4.

## 1.3 What the engine can and cannot do, as of 0.3.33

The relevant facts, from the tree rather than from the issue titles.

`crates/rudb-exec/src/join.rs` is 695 lines of nested loop. Eight join kinds are implemented and their unmatched-row rules are correct: inner, left, right, full, semi, anti, single and positional. The right side is fully materialized and the condition is evaluated per left row against a whole chunk of the right side, which keeps the evaluator on its batch interface. It is a careful implementation of an algorithm that is quadratic. On TPC-H Q5 at SF1 that is a hundred and fifty thousand orders against six hundred thousand line items, which is ninety billion condition evaluations for one of five joins.

`crates/rudb-opt/src/lib.rs` is 461 lines with filter pushdown, constant folding, limit and topn rules, transitive predicates and subquery unnesting. There is no cost model, no table statistics and no join reordering. TPC-H queries are written in a join order that is deliberately not the good one, and the optimizer that is supposed to fix that does not exist. This matters more than the join algorithm for several queries and it is easy to miss.

`crates/rudb-catalog/src` has no `PRIMARY KEY` or `FOREIGN KEY` representation at all, which `../../graph/02-the-data-model.md` section 2.5 needs and which is D5 work independent of any of this.

Subquery unnesting exists, `crates/rudb-opt/src/unnest.rs` is 1240 lines, which matters because eight of the twenty two queries are subqueries and four of them are correlated.

Windows landed in 0.3.32 and 0.3.33, so Q with a window is not a gap; TPC-H does not use one anyway.

## 1.4 What a first run would actually produce

Worth stating precisely, because the first honest number is a deliverable in document 08 and somebody has to know what to expect.

At SF1 with a per-query timeout, the queries that touch no join, Q1 and Q6, run and should be within the ClickBench-shaped ratio the engine already has, which is currently behind DuckDB rather than ahead. The two-table joins with a small side, Q12 and Q14, may finish. Everything with three or more tables at SF1 is a nested loop over hundreds of millions of pairs and will hit the timeout. That is not a failure of the run, it is the run: twenty of twenty two timing out at SF1 in September 2026, recorded, is the baseline that every later number is measured against, and it is worth more than the absence of a number.

## 1.5 The thing to avoid

The temptation is to wait: build the hash join, build the links, and then turn on TPC-H so the first published table is a good one. That ordering produces a table nobody can check against anything, because there is no prior measurement of the same apparatus on the same machine. `rudb-bench`'s README makes the argument in general, the recorded board was recorded before there was anything to flatter, and TPC-H is the case where it is about to be tested.

So: the refusal is replaced before the join is built, not after, and the first TPC-H report in `reports/` has twenty timeouts in it.
