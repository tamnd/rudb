# 6. The baselines

## 6.1 There is no board to copy

ClickBench has one. A public repository of results, one machine class, one protocol, a hundred-odd systems, and a number you can put your own next to. That is why `rudb-bench` could produce a ClickBench column that meant something on the first day.

TPC-H has nothing equivalent. What exists is two things, neither of which is what is needed:

**Audited TPC-H results on tpc.org.** These are real, they are rigorous, and they are irrelevant here. They are full-disclosure runs of clustered systems with priced hardware, they include the load test and the throughput test with the refresh functions, and the smallest published scale factor is far above anything an embedded engine on a laptop is doing. Quoting one next to a single-process SF100 power-test-only run would be a category error, and the TPC's rules on what may be called a TPC-H result exist precisely to prevent it.

**Blog posts and papers.** Every analytical engine has published a TPC-H chart. They disagree about scale factor, about whether the data was in the engine's own format or in Parquet, about thread count, about whether the load was timed, about whether all twenty two queries ran, and about the machine. Between two such charts there is no arithmetic that produces a comparison.

The conclusion is not that TPC-H numbers are meaningless. It is that **the only TPC-H numbers this project can use are ones it measured itself**, on one machine, on one corpus, under one protocol, on the same afternoon. That is an inconvenience for marketing and it is a clarification for engineering: the baseline is DuckDB, run here, by us, on the same bytes.

## 6.2 The baseline that counts

**DuckDB, same machine, same corpus, same protocol, both forms.** It is the compatibility target, the goal document's number is stated against it, and it is a genuinely strong TPC-H implementation, a cost-based optimizer with join reordering, hash joins with Bloom-filter pushdown, parallel execution and out-of-core spilling. Beating it on TPC-H is a real result and there is no risk of it being a weak opponent.

The version is pinned per report and upgraded deliberately, not silently, because DuckDB's TPC-H performance changes materially between releases and a ratio that moved because the baseline moved is not a result about rudb.

## 6.3 The other three

`src/engine.rs` already has ClickHouse, DataFusion and Polars, and all three are run because they are nearly free once the harness runs anything.

**DataFusion** is the closest comparison for a Rust engine with a vectorized executor over Arrow, and it answers all twenty two. It is the answer to "is this a Rust problem or a rudb problem", which is a question that will come up.

**ClickHouse** answers all twenty two and is the best in the world at exactly the part of TPC-H that is not the join, Q1 and Q6, which makes it a calibration device. If rudb is behind ClickHouse on Q1, that is scan and aggregate work and belongs in `../../perf/`, not here.

**Polars** answers nineteen. The three absences are already declared in `src/suite.rs` with their reasons, and the rule from `../../15-rudb-bench.md` applies: a nineteen-query total does not go in the same column as a twenty-two-query total. Polars appears per query and not in the suite total.

None of these three is a goal. The goal is stated against DuckDB and the others are diagnosis.

## 6.4 rudb against rudb

The most useful baseline in the whole directory, and the one no other engine can provide.

**The dated series.** Every SF10 and SF100 run is kept in `reports/` with its date, its commit and its manifest hash. The first one has twenty timeouts in it (document 01 section 1.4) and that is the point, it is the fixed reference the second one is measured against. `rudb-bench`'s README already makes this argument about the recorded board; TPC-H is where it earns its keep, because every change in `../../graph/10-milestones.md` is a claim about a number that has to move.

**The configuration ladder.** One machine, one corpus, one afternoon, four rudb columns:

1. Parquet, no graph sections, what the engine does with nothing.
2. Native, no graph sections, what the format is worth, which is `../../storage-v3/03-benchmark-contract.md`'s question.
3. Native with links, reduction off, what the link join alone is worth.
4. Native with links and reduction on, the full design.

The difference between 3 and 2 is the claim of `../../graph/05-execution.md` section 5.2. The difference between 4 and 3 is the claim of section 5.4. Those two differences are the entire empirical content of the graph directory, and this ladder is where they are either observed or not. A milestone that moves the total but not its own difference has not demonstrated what it claims.

**The hash-join control.** Milestone G0 is an ordinary hash join, and every later measurement is reported against it and not against the nested loop. Beating a nested loop is not an achievement, and a design that beats a nested loop by 40x but a hash join by 1.1x is a design that should not be built. This is the floor `../../graph/06-the-optimizer.md` calls the never-slower rule, measured.

## 6.5 The result to beat that is not an engine

GRainDB put predefined joins into DuckDB and reported a large win on LDBC SNB and a small one on TPC-H, with its authors saying in print that TPC-H lacks the selective many-to-many joins the technique rewards. That is the honest prior for what `../../graph/` should expect on this suite specifically, and the exact figures are read out of the paper and quoted in the report rather than remembered here.

It is recorded as a *target for the graph layer's contribution*, not for the total: if the difference between ladder rung 4 and ladder rung 3 across the twenty two queries is comfortably larger than GRainDB's TPC-H figure, the design did something GRainDB did not, which is the claim `../../graph/09-measurement.md` section 9.6 exists to test. If it merely matches it, the design reproduced a published result on a workload that does not favour it. If it is 1.0x, the design bought nothing on TPC-H and its justification has to come from the workloads in `../../graph/09-measurement.md` section 9.7. Either finding is publishable internally; only one of them is a reason to keep the sections on by default.

The 10x in the goal document is against DuckDB on the total, and it is not the graph layer's number alone. Most of it has to come from the join, the optimizer, the scan and the format together.

## 6.6 The machine

One machine class per report, named, with core count, memory, storage type and whether it was a cloud instance with noisy neighbours. Every engine in a report ran on the same one, and cross-machine comparison is not done, not adjusted for, and not published.

SF1000 will not fit on the development machine and that is not a reason to skip it; it is a reason for the SF1000 report to name a different machine and to stand alone, since it is measuring spilling and the SF100 report is not.

## 6.7 What is never published

A rudb TPC-H number without a same-day DuckDB number beside it from the same corpus. A total from a run with a wrong answer or a timeout in it (documents 04 and 05). A geometric mean without the maximum ratio next to it. Any number labelled as a TPC-H result in the audited sense. And any comparison against a figure taken from somebody else's chart, which after section 6.1 should not need saying but is the single easiest rule in this document to break by accident.
