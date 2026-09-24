# 18. Milestones

Thirteen milestones, C0 through C12. Each has one exit measurement. Each can be shipped on its own, meaning the tree is releasable with the compiled engine behind `SET engine = 'compiled'` and the router refusing whatever is not finished. They are ordered by dependency first and measured value second. The first three deliver no speed to a user, and that is stated rather than disguised. They build the thing every later speed claim is measured against.

**The order as filed (tamnd/rudb #1828 to #1840).** ClickBench is the first target, and every machine we benchmark on (server1, server2, server3, gamingpc) is x86-64, so the filed order differs from the sections below in four ways. C1's correctness gate is the 43 ClickBench queries, with JOB and TPC-H refusal rates reported. C3 builds `direct` for x86-64 and measures G1 on server3, and the AArch64 encoder moves to C12. Tiering, parallelism and the code cache become C4. The ClickBench work of section 18.11 becomes C5 with a 10x gate against DuckDB and ClickHouse, and the bridge becomes C6. JOB joins, JOB strings, TPC-H, TPC-DS and OLTP follow as C7 to C11. The scope of each piece of work below is unchanged, and so are the two rules. No benchmark number is taken on a laptop.

Two rules apply to every milestone.

**No execution optimization is attempted until G1 holds.** G1 is the compile budget of document 02 section 2.8: time to first morsel on a cache miss, and total compile time over all of a query's pipelines, are each at most 1 ms at the median and 5 ms at the maximum. Document 02 section 2.2 is the reason. On JOB, a compiler that executes like Umbra and compiles like Umbra loses to DuckDB end to end. So C3 and C4, which establish G1 on both architectures, come before C6, which makes joins fast. A milestone after C4 that breaks G1 has not passed, whatever it bought in execution time.

**Every exit is a number from the harness, not a demo.** The harness is the one in document 17: `rudb-bench` for time and instructions retired, `rudb-compat` for answers. "Correct" means the differential of document 15 is clean against DuckDB, the first engine and the `interp` tier, with the known-divergence registry unchanged. "Faster" means instructions retired and wall time together, as document 02 section 2.1 requires.

## 18.1 The dependencies outside this folder

Four things this folder does not own gate some of its milestones. Each is named here so that a slip in any of them is noticed as a schedule fact, not argued about as an engineering one.

| Needed by | What | Owner | If it is late |
|---|---|---|---|
| C6 | exact row-id bitmaps through stored links (G3, G4 of `../graph/10-milestones.md`) | `../graph/` | C6 uses Bloom filters only for reduction. The JOB reduction factor in document 02 section 2.9 is then measured with Bloom filters and the exact-bitmap gain becomes a later delta |
| C6, C7 | per-column facts: dictionary size, value range, null count, sort and cluster flags, exposed with an exactness class | `../stats/`, `../storage-v3/` | the physical planner of document 04 treats the fact as `estimated`. The generated code is the generic variant with no guard. Correct, slower |
| C7 | dictionary-coded strings visible to the scan kernel as codes plus a dictionary pointer | `../storage-v3/` | compiled LIKE runs on decoded strings. The C7 gate probably fails, and document 20 question Q3 becomes urgent |
| C11 | in-place updates, an OLTP index, a group-commit log | `../engine-v4/` | C11 measures statement overhead only (instructions from cache lookup to first index probe). The throughput target waits |

## 18.2 C0: measure before building

**Scope.** Three measurements and one harness, and no compiler code.

1. **Frontend latency on JOB.** Parse, bind, rewrite and the logical optimizer, per query, over the 113 queries. We need the median and maximum on the M4 and on `c6a.4xlarge`, both single-threaded. Document 02 section 2.2 budgets 0.3 ms median and 2 ms maximum for this phase, taken out of the 1 ms total. If the frontend already costs more than that, G1 is unreachable no matter how fast `direct` is, and the frontend becomes the first piece of work.
2. **The JOB baselines.** DuckDB 1.4.x pinned by exact version, at 1 thread and at all threads, hot, end to end including planning. We record time and instructions retired per query, plus the first engine's numbers on the same harness. This replaces the numbers read off published figures in document 02 section 2.2 (DuckDB 1.3.2 single-threaded at 55.3 s on a different CPU) with our own.
3. **Where the first engine spends JOB.** The operator-level profile of the first engine on the ten slowest JOB queries, split into probe, build, scan-filter, string predicate and materialization. This profile tests the decomposition in document 02 section 2.9 before anything is built to exploit it.

The harness is the JOB runner in `rudb-bench` described in document 17: IMDB load, 113 queries, answer files, per-phase timing, and instructions retired through the existing child-process measurement.

**Exit.** The report `rudb-bench/reports/<date>-job-baseline` with all three measurements. If the frontend median is above 0.3 ms, the report comes with a written plan for cutting it.

**What it does not include.** Any claim about the compiler.

## 18.3 C1: QIR, the interpreter, and a correct compiled engine

**Scope.** The spine, end to end, at the slowest backend:

- `rudb-qc-ir` in full: types, builder, printer, parser, verifier (document 06).
- The physical plan and pipeline graph for the core operators: scan with pushed filters, filter, project, hash join build and probe (fused variant only), hash aggregate with the inline aggregates of document 03 section 3.4, `MIN` and `MAX` on strings, top-N, the result sink (documents 04 and 05).
- The translators for those operators (document 07).
- The `interp` backend (document 08).
- The router with its refusal log, and `SET engine`.
- The differential harness of document 15 wired to `rudb-compat`.

`vcall` works from C1, so every scalar function the first engine has is reachable. Refusal at C1 is therefore about operators, not functions.

**Exit.** All 113 JOB queries are accepted by the router and return correct answers on `interp`, single-threaded. The refusal rate on TPC-H (22 queries) and ClickBench (43 queries) is reported, not gated.

**Why this first.** The interpreter is the reference every later backend is diffed against (document 15). A backend built before its reference exists has no test oracle below the SQL level, and a miscompile found only through SQL answers takes days to localize, not minutes.

## 18.4 C2: `clif`, and the tier-against-tier differential

**Scope.** QIR to Cranelift lowering, behind the cargo feature `qc-clif` that document 19 section 19.4 requires under the zero dependency rule, with the workarounds for operations Cranelift lacks (document 08). Loading goes through our own code arena in `rudb-qc-rt`, and the platform layer is W^X on macOS and Linux. The tier-against-tier differential of document 15 section on forced tiers: every query runs on `interp` and `clif`, including forced switches at random morsel boundaries, and results must be bit-identical.

**Exit.** All 113 JOB queries correct on `clif`, and the tier differential clean over JOB, TPC-H SF1 and the `rudb-compat` corpus that the router accepts. Compile time per query is reported. It is expected to miss G1, since Cranelift's measured cost is about 16x DirectEmit's, and the point of reporting it is to have the number.

**Why before `direct`.** Cranelift gives a compiled engine on both architectures in weeks. It shakes out every QIR design mistake that only shows up under a real register allocator and a real calling convention. It is the optimizing tier later. Building `direct` first would find those same mistakes one encoder bug at a time.

## 18.5 C3: `direct` on AArch64, and G1 on the M4

**Scope.** The single-pass emitter of document 08: the analysis pass, the combined instruction selection, register allocation and encoding pass, the AAPCS64 call lowering with the Apple variant, the AArch64 encoder, far calls to the runtime through the pinned table register, and encoder tests against a disassembler. The x86-64 lowering skeleton goes in at the same time, per document 00: every lowering added here has an x86-64 counterpart stubbed with a failing test, and C4 turns them green.

**Exit.** G1 on the M4. On all 113 JOB queries, with the code cache off, time to first morsel is ≤1 ms median and ≤5 ms max, measured from SQL text in to the first morsel dispatched, and the total generation and backend time summed over all of a query's pipelines meets the same bounds. The tier differential is clean with `direct` added. The per-phase split of the budget is reported against document 02 section 2.2's table: frontend, physical planning, QIR generation, backend.

**What does not count as passing.** Meeting G1 by refusing the expensive queries, or by running them on `interp`. The G1 measurement is over queries executed on `direct`.

## 18.6 C4: `direct` on x86-64, and G1 on `c6a.4xlarge`

**Scope.** The x86-64 encoder and the SysV call lowering, with the stubs from C3 turned green.

**Exit.** G1 on `c6a.4xlarge`, the tier differential clean on x86-64, and the same JOB answers on both architectures. The last one is a check on the determinism rule of document 03 section 3.8, which forbids floating-point reassociation and FMA contraction in every backend.

## 18.7 C5: tiering, parallelism, and the code cache

**Scope.**

- The Kohn extrapolation policy at morsel boundaries, with background `clif` compilation as low-priority tasks on the shared pool and an atomic function-pointer swap (document 09).
- Deoptimization on guard failure (documents 09 and 13).
- Morsel-parallel execution with thread-local state instances and merge steps (document 05).
- The code cache keyed on normalized QIR (document 09).

**Exit.** Three numbers:

1. JOB end to end at 1 thread and at all threads on both machines, against the C0 baselines. This is the first speed number the compiled engine reports. It is expected to be roughly at parity with DuckDB, because no JOB-specific mechanism exists yet.
2. On TPC-H SF10, the share of pipelines that tier up, and the end-to-end time under `auto` against forced `direct` and forced `clif`. `auto` must be no worse than the better of the two on every query, within 5%.
3. The deopt rate on JOB and TPC-H with facts from `../stats/`. It should be zero on read-only data, and anything else is a bug in the facts or in the guard.

## 18.8 C6: JOB joins, and 5x

**Scope.** Document 10 up to and not including strings:

- The unchained hash table with in-pointer tags and specialized entry layout.
- Tiny-build and dense-key fast paths.
- Staged, prefetching probes chosen from the built size.
- The Lookup and Expand split.
- Late materialization carrying row ids.
- `MIN` pushdown below n:m joins.
- The reduction schedule compiled into scans, with the adaptive keep and drop rule.
- Mark, semi and anti joins, including the NULL-aware variant.

Exact bitmaps come in if `../graph/` G4 has landed, and Bloom filters otherwise (section 18.1).

**Exit.** JOB end to end at **5x DuckDB single-threaded** (sum of the 113 query times), with G1 still holding. Also reported: the geomean, the worst per-query ratio, and the ablation of document 17 with each mechanism switched off in turn, set against the rows of document 02 section 2.9. A mechanism that measures below 0.7 of its predicted factor reopens its row in document 02, per document 17, and gets a written explanation before C7 starts.

## 18.9 C7: strings, and 10x on JOB

**Scope.** Document 12's string work:

- Compiled LIKE and ILIKE with per-segment matcher choice.
- Multi-pattern fusion.
- Dictionary-domain predicate evaluation (evaluate once per dictionary entry, test a code in the loop).
- FSST-domain matching where the storage layer exposes it.
- String `MIN` and `MAX` over codes where order-preserving.
- Promotion of transient strings at pipeline breakers.

**Exit.** **JOB at 10x DuckDB single-threaded**, end to end including compile, on the M4, with G1 holding. Reported alongside: the all-threads ratio, the ratio on `c6a.4xlarge`, the CEB and JOB-light numbers (reported, not gated), and the ablation again.

This is the milestone the folder exists for. If it misses, document 20 lists what to look at, in order.

## 18.10 C8: TPC-H, aggregation and sort

**Scope.** Document 11 and the rest of the TPC-H operator surface:

- Aggregation strategy choice: thread-local partitioned against global ticketed, dense array, and perfect-hash on small dictionaries.
- Groupjoin and eager aggregation.
- The normalized-key sort encoder with width-monomorphized runtime kernels.
- Top-N with a compiled comparator.
- Decimal i128 paths.
- The TPC-H-specific decorrelation shapes, if `rudb-opt` does not already emit them.

**Exit.** TPC-H SF1 single-threaded at **≤1/10 of DuckDB's instructions retired** in total, which is 1.79G against 17.90G at the `rudb-bench/reports/2026-09-24` DuckDB baseline, re-measured against the pinned DuckDB version at C0. Wall time is reported at SF1 and SF10 at 1 thread and all threads. No query may be worse than 1/3 of DuckDB's instructions, per the rule in document 17 that means hide catastrophes.

## 18.11 C9: the bridge, and ClickBench

**Scope.** Partial acceptance (document 03 section 3.4). A plan with an operator that has no translator runs its translatable pipelines compiled, and the rest on the first engine. Buffers exchange at pipeline breakers in the first engine's chunk format. The ClickBench-specific work goes in too:

- Wide-scan aggregation with register-resident state.
- `COUNT(DISTINCT)` strategies.
- Top-N over large group counts.
- The regex path through `rudb-regex`, and the Q29 rewrite if document 12's proof obligation for it is met.

**Exit.** ClickBench hot run on `c6a.4xlarge` at the ratio document 02 section 2.4 names, reported with Q29 separately, and a router refusal rate under 1% of statements across `rudb-compat`'s corpus, counting partial acceptance as acceptance.

## 18.12 C10: TPC-DS, windows, grouping sets

**Scope.** Window functions (document 11), `GROUPING SETS`, `ROLLUP` and `CUBE`, set operations, the multi-way star probe chains of document 10, and whatever translators the TPC-DS refusal log shows are missing.

**Exit.** At least 90 of the 99 TPC-DS queries fully compiled (not bridged) at SF10, all 99 correct, and the 5x target of document 02 section 2.6 reported per query.

## 18.13 C11: OLTP

**Scope.** Document 14:

- Prepared statement compilation to parameterized code.
- The statement-shape cache for unprepared repeated SQL.
- The point path: a single function with no morsels and an inline index probe.
- Compiled DML.
- The TPC-C driver in `rudb-bench`.

**Exit.** Under **20,000 instructions of statement overhead per TPC-C transaction**, measured from cache lookup to first index probe and summed over the statements of each of the five transaction types. Throughput at 1 thread and 100 warehouses is reported if `../engine-v4/` has landed, and marked as waiting on it if not.

## 18.14 C12: the optimizing tier, LLVM, release

**Scope.**

- Tuning the `clif` optimizing tier on long pipelines: TPC-H SF100 and TPC-DS SF100.
- The LLVM evaluation behind its cargo feature.
- Making the compiled engine the default under `SET engine = 'auto'`.

**Exit.** Three things:

1. A measurement deciding the LLVM question. If LLVM beats `clif` by more than 10% of execution time on SF100 pipelines long enough to amortize its compile time, it ships behind the feature. Otherwise the crate is deleted, per document 08.
2. `auto` as the default, which requires the C1–C11 gates holding at once on the release candidate.
3. A published report comparing against DuckDB on JOB, TPC-H, ClickBench and TPC-DS, with the refusal rate and compile share shown alongside the ratios.

## 18.15 The order, and what could reorder it

**C1 before C2 before C3.** A reference comes before the backend it checks. A correct slow backend comes before a fast one, because the fast one's bugs are only findable against something correct.

**C3 and C4 before any execution work.** This is G1. The rest of the order depends on it.

**C6 before C7.** The string mechanisms are measured on the plans the join work produces. Measuring them on plans that still expand n:m fan-out would credit strings with work that late materialization removes, which is caution 1 of document 02 section 2.9.

**C8 through C11 are nearly independent of one another** and can run in parallel once C5 is in. Their order is the order of the benchmarks in the goal: TPC-H is the calibration benchmark, ClickBench has the widest audience, TPC-DS is coverage, and TPC-C needs `../engine-v4/`.

**Three outcomes would reorder this.**

- **C0 shows the frontend over budget.** Frontend work becomes C0.5 and blocks C3's exit, because G1 includes it.
- **C3 shows `direct` cannot meet G1** because the per-query QIR is larger than the budget can encode, not because the emitter is slow. The problem is then in documents 06 and 07, and the answer is plan-level sharing of pipelines between queries or lazier generation of rarely taken paths (document 20 question Q1), not a faster backend.
- **C6 shows staged probes buying less than predicted** because reduction already removed the misses. Document 02's caution 1 was right. The factor moves to reduction and nothing else changes. That outcome is a reason to write the prediction down, not a failure.

## 18.16 What is explicitly not scheduled

- Worst-case-optimal joins as a default operator (document 10 gives Umbra's 25x slowdown on JOB).
- An on-disk persistent code cache (document 09).
- Whole-query compilation.
- Source-to-source generation through a C or Rust compiler.
- MLIR.
- Learned tiering or learned kernel choice.
- SIMD in `direct`'s generated code.
- NUMA-aware scheduling.
- GPU backends.

Each has a paragraph in its document, or in document 20, saying what measurement would put it on this list.

## What we should take from this document

G1 is the hinge. Everything before it builds the ability to compile in about a millisecond, and everything after it spends that ability. A schedule that lets execution work start before G1 holds is a schedule that will ship Umbra's JOB result: fast execution, lost benchmark.

The first three milestones make no one faster, and they are the ones that make every later claim believable. The interpreter is the oracle, Cranelift is the proof that QIR survives a real backend, and only then is the emitter written, against both.

The 10x JOB number is C7, not C6. The join mechanisms get to about half of it, and the rest is strings and the plan-level interaction between reduction and late materialization. That split is a prediction from document 02, and C6's ablation is where it is first tested.

Four external dependencies are named with their fallback. None of them blocks correctness, and each one missing costs a stated factor, not an argument.
