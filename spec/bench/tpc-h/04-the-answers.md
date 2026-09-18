# 4. Correctness

A timing of a wrong answer is not a number. This is more than a slogan on TPC-H, because most of the ways to make a join fast are also ways to make it wrong, and because two of the engine's open wrong-answer issues are in `DECIMAL` arithmetic that every money column in this suite goes through.

## 4.1 Three references, used for different things

**The specification's validation output at SF1.** TPC-H defines a qualification database at scale factor one with fixed substitution parameters and publishes the expected answer for each of the twenty two queries. It is the only reference in this list that does not come from another database engine, which makes it the only one that can catch an error rudb and DuckDB make in the same way. It is checked once per corpus regeneration and on every release, not on every commit, because it needs SF1.

**DuckDB, on the same corpus.** The continuous reference, run at SF0.01 on every commit and at whatever scale a run uses. This is what `rudb-compat` already does for the SQL logic corpus and it is the same discipline: the comparison is against an engine's output on the same bytes, so a disagreement is a bug in one of the two and not an ambiguity about what the question meant.

**rudb against itself, with `graph_sections` off.** The differential of `../../graph/09-measurement.md` section 9.2. It catches the failure mode that neither of the other two will, because a row-identity bug produces a plan-shape-dependent wrong answer that DuckDB cannot see and the specification's fixed answers only catch if the plan happens to be chosen at SF1.

A fourth would be useful and is not required: ClickHouse and DataFusion both answer all twenty two, so a three-way disagreement is available cheaply when a two-way one is ambiguous.

## 4.2 Comparing results

The output of a TPC-H query is a small table, the largest is Q10's twenty rows and Q1's four, so comparison can afford to be thorough.

Values are compared by type, not by their printed form. `src/answer.rs` already exists because three engines print an `avg` over a `DECIMAL(15,2)` three different ways, and the resolution is to parse rather than to `diff`. Integers, dates and strings must match exactly. `DECIMAL` must match exactly after the scales are aligned, because decimal arithmetic is exact and an inexact decimal is a bug rather than a rounding difference. `DOUBLE` is compared with a relative tolerance, which TPC-H needs in exactly one place, Q19's and Q14's ratios, where the division makes the result a float, and the tolerance is `1e-9` relative, which is far tighter than the specification's own rounding allowance and is justified because both engines are evaluating the same expression over the same exact decimals.

A `NULL` is a value and matches only a `NULL`. An empty result matches only an empty result. Column count and column order must match; column *names* are compared and a mismatch is a warning rather than a failure, because issue #507 says rudb names an unaliased expression differently from DuckDB and that is a real compatibility bug with its own ticket rather than a reason to fail a benchmark.

## 4.3 Row order

Every TPC-H query has an `ORDER BY`, so row order is part of the answer and is compared. That is the strict rule and it is the right one, with the exception in section 4.4.

## 4.4 The tie hazard, which is the one that will waste a day

Q2, Q3, Q10, Q18 and Q21 have an `ORDER BY` that does not totally order the rows, plus a `LIMIT`. When the value at the cut is tied, two correct engines return different *sets* of rows, not merely different orders. Q2 is the worst: it orders by four columns, limits to a hundred, and at SF1 there are more than a hundred candidate suppliers with the same account balance in play.

The rule:

1. Compare the full result as an ordered list. If it matches, done, which is the overwhelmingly common case.
2. If it does not, re-sort both results by every column, in order, and compare as multisets. A match here means the engines agree on the rows and disagree on a tie order, which is not a failure and is recorded as `tied`.
3. If the multiset comparison also fails, check the boundary: take the last row's `ORDER BY` key from each result and count how many rows in each engine's *unlimited* result share it. If the counts exceed the room left under the `LIMIT`, the difference is a boundary tie, and the comparison falls back to checking the deterministic prefix, the rows whose sort key is strictly before the boundary, plus the total cardinality.
4. Anything else is a failure.

Step three needs the unlimited result, which means running the query again without its `LIMIT`. That is expensive at SF100 and it only happens when steps one and two have already failed, which should be never, so it is the right place to be expensive.

This is also why a comparison must never be a text `diff` of two output files, and why the harness must not sort results before comparing them by default: sorting first would hide a genuine ordering bug, which is a real class of bug in a `TopN` operator and one rudb has an operator for.

## 4.5 Decimals, specifically

TPC-H is a money benchmark and this is where rudb is currently known to be wrong.

Issue #504: `trunc` over a `DECIMAL` keeps the scale instead of dropping it. Issue #503: `FLOAT` divided by `FLOAT` comes back as a `DOUBLE`. Neither is hit by the twenty two queries as written, and both are in the neighbourhood, which is the point: the suite's arithmetic is `l_extendedprice * (1 - l_discount) * (1 + l_tax)` summed over six hundred million rows, and every intermediate scale decision in that expression has to match DuckDB's or the sums differ in the last place and the comparison in section 4.2 fails.

The rule that follows: the scale of every intermediate in every TPC-H expression is a compatibility surface with a test, and the test lives in `rudb-compat` against DuckDB rather than here. What lives here is the failure: a TPC-H answer mismatch attributable to decimal scale is reported as a compatibility bug with the expression that produced it, not as a benchmark note.

## 4.6 What a mismatch does to a run

It does not stop the run and it does not produce a time. The query's row in the table says `wrong` with a link to the diff, the suite total is reported as incomplete with the count of correct queries beside it, and no aggregate ratio is published from a run with a wrong answer in it.

That last clause is the one that matters. `../../15-rudb-bench.md`'s rules exist so that a number cannot be published without its caveats, and the caveat "three of the twenty two were wrong" invalidates the total rather than annotating it. A suite total over nineteen correct queries is not comparable to a suite total over twenty two and must not appear in the same column as one.

## 4.7 The cheap continuous check

SF0.01 is about eleven megabytes of Parquet and all twenty two queries against it take under a second in any engine. It runs on every commit against DuckDB's output, both with and without `graph_sections`, which makes three executions of twenty two queries and is affordable.

That check catches the overwhelming majority of correctness regressions in this area, and it catches them at the commit rather than at the next SF100 run, which is the difference between a bisect over one commit and a bisect over a week.
