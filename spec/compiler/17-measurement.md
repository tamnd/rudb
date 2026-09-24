# Measurement

How the compiled engine's performance is measured, on which machines, against which rival builds, and what may be reported. Document 02 set the targets. This document makes them checkable, one query at a time. It also checks the prediction in document 02 section 2.9 about where the 10x comes from, instead of assuming it.

Everything here lives in `rudb-bench`, which already enforces most of the rules in its types. `report::publishable` returns the list of reasons a result may not be published. Median and best-of-three are different constructors. A distribution of fewer than five runs is not publishable. Reporting rule seven forbids comparing across machines. `regress.rs` refuses to gate on noise. The compiled engine adds four things the harness does not have yet:

- a JOB suite that actually runs (`suite.rs` has an entry with `Size::Unsettled` and no table counts),
- an engine and tier dimension on every record,
- compile time as its own measured quantity, and
- ablation switches.

Everything in this document marked **new** is one of those four.

## 17.1 The rules, restated for a compiler

The rules from document 02 section 2.1 stand: end to end, per query as well as total, the rival's version and the machine named, and gated on instructions as well as time. Three additions are specific to a compiling engine.

**Compile time is inside every number, and the code cache is off for headline runs.** A hot run in the usual benchmark sense repeats a query in the same process. For a compiling engine, that means the second run is served from the code cache and compile time disappears from it. Umbra's JOB numbers show what that hides: 0.928 s of execution next to 7.592 s of compilation at 32 threads, so compile is 89% of end to end (`research-notes/D-benchmarks.md` section 4). **For every suite, the headline hot runs use `SET qc_code_cache = off`**, so each run compiles from scratch. A cache-on column is also reported, clearly labeled, because a real deployment with repeated dashboards gets that number too. TPC-C is the exception by design: it is measured with prepared statements, and document 14 says why.

**The rival's version and methodology are fixed per claim, and stated.** DuckDB halved its JOB time between 0.10.1 and 1.3.2, so a stale baseline flatters us. ClickBench changed its methodology three times in a year:

- the combined metric became the default in September 2025,
- cold runs required a restart from May 2026, and
- a storage-type filter was added on 2026-09-18.

QuestDB showed that keeping the process alive is worth 1.2x to 2.2x on its own. "Survivorship Bias" (CIDR 2026) is the general warning about comparisons that quietly drop the unfavorable cases. Every claim therefore records the DuckDB version, the ClickBench methodology date where it applies, and whether the process was fresh.

**The refusal rate is part of every number.** A compiled-engine result for a suite is reported together with the fraction of queries the router sent to the first engine and the reasons it gave. Under `engine = 'auto'` those queries still count in the total, at the first engine's time. That is correct, because it is what a user gets. A claim about the compiled engine's own speed is made under `engine = 'compiled'` on the accepted subset and says "on the accepted subset" in its first sentence.

**No performance number without a correctness claim on the same commit** (document 15, section 15.15).

## 17.2 The JOB harness (new)

**Data.** The IMDB snapshot distributed with the original JOB queries: 21 tables `[GK]`, about 3.6 GB of CSV (document 02, section 2.2). The procedure:

1. The harness downloads it once and records its SHA-256.
2. It loads the same CSV into DuckDB and into rudb, each into its own database file, using the loading path `suite::loading` already provides.
3. It fills `suite.rs`'s `tables` with exact row counts read from DuckDB after the load.

Row counts are checked on every run before any timing. A load that disagrees is a harness failure, not a slow query.

**Queries.** The 113 files `1a.sql` through `33c.sql`, unmodified, in `rudb-bench/fixtures/job/`. Answers come from the pinned DuckDB release (document 15, section 15.11). Every timed run checks its answer first.

**Runs per query per engine.**

| run kind | process | code cache | OS page cache | repetitions | reported as |
|---|---|---|---|---|---|
| cold | fresh process | empty | warm | 1 | `cold` |
| hot, compile included | same process, after cold | off | warm | 5 | median, and the distribution |
| hot, cache on | same process | on | warm | 5 | `hot-cached` (secondary) |
| instructions | fresh process per repetition | off | warm | 3 | median `perf stat -e instructions` minus the `SELECT 1` baseline |

"Cold" here means a fresh process with warm files. Dropping the OS page cache needs root and measures the disk, and the disk is not what this suite is about. The instruction method is the one `reports/2026-09-24/tpch-instructions-retired.md` established and #221 folded into the harness. The two engines alternate in running first, and the startup instructions measured with `SELECT 1` are subtracted. Resource counters (peak RSS, faults, context switches) come from `scripts/measure-child.c` around the same fresh processes.

**Threads.** Every run happens twice: at `threads = 1`, and at all hardware threads. The headline JOB target (document 02: 10x DuckDB, which is 5.5 s against DuckDB 1.3.2's 55.3 s on a Xeon E-2236, about 49 ms per query) is single-threaded. The rival's number is re-measured on our machine and never taken from the paper.

**The per-phase breakdown (new).** Each engine run is followed by one extra `EXPLAIN ANALYZE` run with `SET qc_profile_output = 'json'` (document 16, section 16.6). From it the harness records per query:

```
frontend physical pipelines qir backend install first_morsel execute result
tiers_used deopt_morsels guard_failures refusal_reason cache
```

The timed runs never have counters compiled in. The harness checks the `instrumented` flag on every timed record and refuses to publish a record where it is set.

**One command.** `rudb-bench run job --engine compiled --threads 1,all` produces the table in section 17.9. `--engine` is the new dimension, with values `duckdb`, `rudb-vectorized`, `rudb-compiled` and `rudb-auto`.

## 17.3 JOB-light and CEB

**JOB-light** is the 70-query subset used by the learned-cardinality literature `[GK]`. It runs in the nightly job only as a fast signal. Its queries are simpler than JOB's, so it cannot stand in for JOB.

**CEB** is 13,644 queries over the same data (document 02, section 2.3). The headline statistic is not the total but **the worst per-query ratio against DuckDB**, because the failure mode being measured is one catastrophic plan and the mean is exactly the statistic that hides one. That is `suite.rs`'s note on JOB, and it applies to CEB with more force.

- **Nightly:** a fixed 1,000-query sample, 64 per template, with a seed recorded in the suite definition.
- **Weekly:** all 13,644 queries, one hot run each with compile included, plus the fresh-process instruction count on the 100 worst.
- **Published orientation only:** CEB at SF5, multi-threaded, Bespoke 0.6 s, Umbra 1.1 s, DuckDB 14.7 s `[snippet]`. On Umbra's raw CEB-style data, DuckDB took 6,721 s against Umbra's 175 s plus 142 s of compile.

On CEB, the compile column matters more than on JOB. There are thousands of distinct plans, so the code cache cannot help, and compile time is spent query after query. The report puts `sum(first_morsel)` beside `sum(total)` for that reason.

## 17.4 TPC-H

- **SF1, instructions.** This already exists: 22 queries, fresh process, `perf stat`, median of 3, startup subtracted. The baseline is rudb at 27.27G against DuckDB at 17.90G, 1.51x. The worst ratios are q01 at 2.34x and q12 at 1.96x. The target is 1.79G (document 02, section 2.5). This is the compiled engine's daily number, because it holds on a busy box: two runs at load averages of 4.2 and 10.1 agreed to within 0.03x on every query.
- **SF10 and SF100, wall time.** At `threads = 1` and all threads, on a quiet machine only. `rudb-bench`'s drift gate refuses a run whose machine load exceeds its threshold.
- **Compile share.** At SF1, Umbra spent 1.10 s compiling against 0.107 s of execution. The report shows our compile share at SF1 and SF100 so that the small-scale-factor failure mode is visible.

## 17.5 ClickBench

The board's own method on `c6a.4xlarge`: 43 queries, three runs each. The first run follows a restart, as the board has required since May 2026. Hot is the best of the other two. The score is the board's combined metric, with the storage-type filter set to match the configuration we submit. The report also gives the plain hot sum, because document 02's bar is stated that way: Umbra 7.41 s, DuckDB 26.25 s, and 10x DuckDB is 2.63 s.

Two columns are specific to the compiler:

- **compile per query**, against the 0.3 ms median budget, and
- **refused or interpreted per query**, because under the policy in document 09 a query estimated under 1 ms starts on `interp` and never compiles. That is correct behavior and it must be visible.

Q29 is reported with a footnote every time. Document 02 says 10x on the total needs Q29 solved outside the compiler, and the report must not let a Q29 win or loss be read as a compiler result.

## 17.6 TPC-DS

Run at SF1 nightly for coverage, and at SF10 and SF100 weekly for time. For each query the report records whether it ran on the compiled engine, how many pipelines it had, and whether any pipeline was refused. The C10 gate is at least 90 of 99 compiled, and the headline counts compiled queries, not executed ones. Q67 gets its own line at SF100, because DuckDB spent 51 of 79 minutes on it at SF300, and a total that hides it says nothing.

## 17.7 TPC-C (new driver)

`rudb-bench` has no TPC-C driver. **The compiled engine needs one to check document 02's statement-overhead target: under 20,000 instructions of statement overhead per transaction.** The driver:

- **In process, no network.** It links rudb as a library and runs terminals as threads, so that what it measures is the engine and not a wire protocol.
- **Prepared statements for all five transactions**, at the standard mix (45% NewOrder, 43% Payment, 4% each of OrderStatus, Delivery and StockLevel), without keying or think time. That is a throughput measurement, not a compliant TPC-C result, and the report says so.
- **Warehouses:** 1 for the overhead measurement, and one per worker thread for throughput.
- **Overhead measurement.** Instructions per transaction via `perf stat` over 100,000 transactions after warmup. The share attributed to storage comes from the operator sampler (document 16, section 16.4), where storage calls carry their own origin. **Statement overhead is everything else**: dispatch, cache lookup, state setup, generated code outside storage calls, and result handoff. The measurement is only as good as the attribution, and the report shows the sampled split next to the total.

References, for orientation only, because none of them was measured under our configuration: HyPer 126,576 tps, Umbra 27k TX/s, LeanStore 41k, PostgreSQL 2.6k. The TPC-C column in document 02 section 2.9 assumes `../engine-v4/` storage. Until that storage exists, the driver reports overhead per transaction and not a throughput claim.

## 17.8 Machines

| machine | ISA | role | instructions | notes |
|---|---|---|---|---|
| MacBook Air M4 | AArch64 | development; G1 gate for `direct`/AArch64 (C3); wall time single-thread | no (no `perf`) | samply profiles; wall time only on a quiet, plugged-in machine |
| `c6a.4xlarge` | x86-64 | G1 gate for `direct`/x86-64 (C4); ClickBench; published x86 numbers | yes | the ClickBench reference machine; rented per run |
| Graviton, `c8g.4xlarge` (ours to choose) | AArch64 | AArch64 instructions retired; nightly encoder and tier tests on Linux/AArch64 | yes | the only place AArch64 instruction counts come from |
| `server2`, `server3` | x86-64 | daily instructions (TPC-H SF1, JOB), commit gate, drift gate | yes | shared and busy; instructions only, never wall-time claims |
| `gamingpc-wsl` | x86-64, 32 threads | nightly corpus and fuzzing; all-threads runs | yes | WSL2, noted on every record |

**Rule seven is unchanged: no comparison across machines.** Every ratio is two engines on one machine in one session. A number from the M4 and a number from `c6a.4xlarge` are never divided by each other, and G1 is gated separately on each.

## 17.9 What a report contains

For each suite, engine and thread count, the harness produces one table with a row per query and a summary block. The JOB report looks like this; the numbers are placeholders, not measurements:

```
JOB  threads=1  machine=c6a.4xlarge  duckdb=v1.x.y  rudb=0.5.0@abcd123  qc_code_cache=off
data sha256=…  answers: 113/113 correct  correctness claim: 15.15 ok (divergences: none)

query  duckdb_ms  rudb_ms  ratio  compile_ms  first_morsel_ms  tiers         instr_ratio
1a        212.1     18.4  11.5x      0.41        0.52          direct        0.09
…
33c       903.7    121.0   7.5x      0.88        1.04          direct>clif   0.14

total        55,310 ms   5,420 ms   10.2x
geomean                              11.0x
worst                               33c 7.5x     (floor: no query < 1.0x)
compile share        sum(compile)/sum(total) = 1.9%
G1                   first_morsel median 0.61 ms  max 3.8 ms  (budget 1 / 5)
                     compile_total median 0.74 ms  max 4.6 ms  (budget 1 / 5)
refusals             0/113
```

**Required, and enforced by `publishable`:**

- every query, including losses;
- total, geometric mean and **worst ratio**, with the worst named;
- compile share and G1 median and maximum;
- the refusal rate with reasons;
- instruction ratio where the machine has counters;
- the versions, the machine, the cache setting and the thread count.

The per-query floor from document 02 is a line of its own: any ratio below 1.0x makes the suite's claim unpublishable, whatever the total says.

## 17.10 Checking the decomposition

Document 02 section 2.9 predicts that JOB's 10x is 1.5x (plan) × 1.5x (layout facts) × 1.5x (fusion) × 2x (staged probes) × 1.5x (strings). That table is a prediction, and **each factor gets a switch that turns off the technique behind it**:

| §2.9 row | switch (new, all builds) | off means |
|---|---|---|
| plan: reduction, order, unnesting | `qc_reduction = off` | no semi-join reduction or transferred filters; the join order stays as planned without them |
| layout and encoded facts | `qc_facts = off` | no fact specialization: generic decode, no dictionary-code predicates, no dense-key arrays |
| fused pipelines, register state | `qc_fusion = off` | a materialization point after every operator; pipelines become one operator each |
| staged probes, prefetch, hash table | `qc_staged_probe = off` | tuple-at-a-time probes, no prefetch, generic chained table |
| compiled strings and patterns | `qc_string_kernels = off` | LIKE and string compares go through `vcall` to first-engine kernels |
| statement overhead and code reuse | `qc_code_cache = off`, `qc_prepared = off` | recompile every execution |

**Each factor is measured two ways, because the factors are not independent** (document 02, section 2.9, first caution):

- **Leave one out.** Everything is on except one switch, and the factor is `instr(one off) / instr(all on)`. This is the marginal value of the technique in the finished engine.
- **Add one in.** Everything is off except one switch, and the factor is `instr(all off) / instr(one on)`. This is the technique's value on its own.

The product of the add-one-in factors is compared with document 02's product. The product of the leave-one-out factors is compared with the measured total. The gap between the two products is the interaction, and the report shows it.

The metric is instructions retired on `server3` at `threads = 1`, with wall time on `c6a.4xlarge` alongside. Instructions are the stable one. **A factor measured below 0.7 of its prediction is flagged in the report and opens a revision of document 02's table and document 18's plan.** An underdelivering technique must be noticed, not absorbed into a total that happens to come out fine. The same switches exist for TPC-H and ClickBench, whose columns are checked the same way from C8 and C9.

## 17.11 Regression gates

| gate | runs on | when | fails on |
|---|---|---|---|
| G1 compile latency | M4 and `server3` (and `c6a.4xlarge` at C4 and later) | every commit | JOB `first_morsel` or `compile_total` median over 1 ms or any query over 5 ms, measured over 5 runs with the code cache off |
| instructions | `server3` | every commit | any JOB or TPC-H SF1 query up more than 3% in instructions against the committed record, confirmed by a second run |
| refusal rate | any | every commit | the refusal count on JOB, TPC-H or ClickBench goes up |
| tier-diff and answers | per document 15 | every commit | any difference |
| drift | `server3`, quiet window | nightly | `regress.rs`'s existing rule, non-overlapping distributions and median past `DRIFT` |
| decomposition | `server3` | weekly | a factor below 0.7 of prediction (a flag, not a build failure) |
| cross-machine | never | never | rule seven |

Instruction counts gate at 3% because they repeat to within that on a busy box, as the two TPC-H SF1 runs show. Wall time gates only where `regress.rs` already allows it. The committed records are text, one query per line, so a baseline update is reviewed as a diff, as it is today.

## 17.12 The record format

Every timed run writes one record per query into the ledger `rudb-bench` already keeps (`src/ledger.rs`). The compiled engine adds these fields, all new:

```rust
pub struct QcRecord {
    pub engine: Engine,                 // Duckdb | Vectorized | Compiled | Auto
    pub tier_policy: TierPolicy,        // Auto | Forced(Tier)
    pub code_cache: bool,
    pub instrumented: bool,             // counters or sampler compiled in: never publishable
    pub phases: PhaseTimes,             // frontend .. result, section 17.2
    pub first_morsel: Duration,         // G1, first half
    pub compile_total: Duration,        // G1, second half: generation + backend over all pipelines
    pub tiers_used: SmallVec<[(PipelineId, Tier, u32 /* morsels */); 8]>,
    pub refusal: Option<RefusalReason>,
    pub ablation: AblationSet,          // which qc_* switches were off; empty for headline runs
    pub correctness: CorrectnessRef,    // the 15.15 claim this run relies on
}
```

`publishable` gains four reasons to refuse:

- `instrumented`,
- a non-empty `ablation` on a headline table,
- a `correctness` claim from a different commit, and
- `code_cache = true` on a headline hot run.

Each refusal is enforced in the type, not in a checklist, which is how the existing rules already work.

## 17.13 Honest limits

**The M4 has no instruction counter we can use**, so AArch64 instruction numbers come only from Graviton. Graviton is a different microarchitecture from the M4, and `direct`'s AArch64 code is tuned on the M4. An AArch64 instruction ratio is a statement about the code, not about the laptop.

**Instructions are not time.** q18 in the SF1 report retires 1.95x DuckDB's instructions and runs in 0.73x to 0.78x of its time. A compiled engine that removes instructions but adds cache misses would show a win here and a loss on the clock. Every instruction claim is paired with a wall-time run on a quiet machine before it is published. The instruction gate is for catching regressions, not for making claims.

**"Cold" is process-cold, not disk-cold.** Nothing in this document measures storage from a cold disk. ClickBench's cold column, where the board requires a restart, is the only place where we follow someone else's cold definition, and it is reported under that definition's name.

**Ablation switches measure our implementation of a technique, not the technique.** A factor that comes in low may mean the technique is worth less than document 01 found, or that our version of it is weak. The report cannot tell which, and the revision it triggers has to find out.

## What we should take from this document

Compile time is inside every headline number. Hot runs are taken with the code cache off, so the number JOB is judged on includes the compile that Umbra's published number leaves out. The cache-on number is reported beside it and labeled.

JOB gets a real harness in `rudb-bench`: checksummed IMDB data, row counts checked before timing, answers from a pinned DuckDB, one and all threads, fresh-process instruction counts, and a per-phase breakdown from the engine's own profile output. The rival is re-measured on our machine, and its version is part of the claim.

Every report carries total, geometric mean, the worst query by name, compile share, G1, the refusal rate and the instruction ratio. A single query below 1.0x makes a suite's claim unpublishable. CEB's headline is its worst ratio, not its total.

Document 02's decomposition is checked, not believed. Each factor has a switch, is measured both leave-one-out and add-one-in in instructions, and a factor that comes in under 0.7 of prediction reopens the plan.

The gates run on instructions because those survive a busy machine. G1, refusals, tier-diff and answers gate every commit, drift gates nightly on a quiet box, and no number is ever divided by a number from another machine.
