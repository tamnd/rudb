# 09. Tiering and caching

Which backend runs each pipeline, when that changes, what happens when specialized code meets data it did not expect, and how compiled code is reused across queries.

Document 08 defines the backends. Document 04 (section 4.9) defines the closed list of runtime decision points, and this document implements the "tier" row of that list plus the mechanics the other rows need. Document 13 owns the scheduler that calls into all of this.

Markers: `[snippet]` means the figure came from a search snippet, not the primary text. `[derived]` means we computed it from cited numbers. `[GK]` means general knowledge not re-verified for this spec.

## 9.1 Principles

1. **Tiering is per pipeline, and it is decided from measured progress, never from optimizer estimates.**
   - PostgreSQL gated JIT on planner cost (`jit_above_cost` = 100,000). PostgreSQL 19 turns JIT off by default because small statistics changes flipped queries across the threshold and made them pay hundreds of milliseconds of compile (https://www.postgresql.org/docs/19/runtime-config-query.html; proposal details `[snippet]`).
   - HyPer and Umbra decide from observed morsel throughput instead (Kohn et al., ICDE 2018, https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf).
2. **The swap happens at morsel boundaries, by pointer.** There is no on-stack replacement. Umbra's authors put it this way: "Advanced mechanisms for switching functions are not necessary, as morsel-driven parallelism ensures that the function is called for sufficiently small workloads" (CGO 2024, https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf).
3. **Tier choice is the one sanctioned timing-dependent decision.** It is allowed because it cannot change results: document 08 requires bit-identical backends, and document 15 checks that. Everything else in this document is either a function of the data or a pure performance hint.
4. **The policy degrades cleanly with the build.**
   - A default build has `interp` and `direct` only (document 08).
   - `clif` exists only with the `qc-clif` cargo feature, and `llvm` only with `qc-llvm`.
   - Every rule below that mentions a backend applies only when that backend is compiled in. Without the features, the policy is `interp → direct` and nothing else.

## 9.2 The tiers and what they cost

| Tier | In default build | Compile, per function | Speed relative to `direct` | Source |
|---|---|---|---|---|
| `interp` | yes | about 4.5 µs | 3.2x slower (x86) | CGO 2024 Table III `[derived]` |
| `direct` | yes | about 9 µs | 1.0 | DirectEmit, CGO 2024 `[derived]` |
| `clif` | `qc-clif` | about 160 µs (x86), about 90 µs (M1) | about 1.045x faster on average (x86); 1.39x possible on loop-heavy functions (TPC-DS Q17, LLVM-opt vs DirectEmit) | CGO 2024 `[derived]` |
| `llvm` | `qc-llvm` | about 1.7 ms | 1.17x faster on average | CGO 2024 `[derived]` |

**On average, tier-up above `direct` buys little, and it buys a lot on a few pipelines.** The policy's job is to find those few without paying compile costs on everything else. CedarDB describes its single-pass tier as reaching "nearly 90% of the throughput in 1% of the compile time" (https://cedardb.com/blog/compilation/). The numbers in the table agree.

## 9.3 The initial tier

**Rule I1: a pipeline whose input is provably at most one morsel runs on `interp`.**

- "Provably" means an upper bound from storage metadata, after zone-map pruning: row-group row counts, or the exact size of a materialized input. It never means a cardinality estimate.
- This is how document 02's "queries predicted under 1 ms run on the interpreter" is implemented without using the optimizer's estimates, which would be the PostgreSQL failure mode.
- Catalog queries, point lookups not already on the prepared path (document 14), and tiny dimension-table builds land here.

**Rule I2: every other pipeline compiles on `direct`, synchronously, on the worker that first claims it.** We call this compile-on-claim.

- It does not start in `interp` and tier up, as Kohn's HyPer did. That design made sense when the next tier cost 6 ms or more of LLVM compile time. Ours costs about 10 µs.
- Interpreting first saves about 5 µs of compile per function and costs 2.2x the function's `direct` run time `[derived: 3.2 - 1]`. That trade wins only for pipelines that run for about 2.5 µs `[derived]`, and Rule I1 already covers those.

**Rule I3: compilation is lazy, in dependency order, and in parallel.**

- At query start, only the pipelines with no unfinished dependencies are ready. In JOB, that is the reduction and hash-build pipelines.
- Each is compiled by the first worker to claim a morsel of it, so ready pipelines compile in parallel across workers.
- A pipeline whose runtime decisions resolve at its own start is compiled only then, and only in the chosen variant. These are the build/probe flip and fused-versus-staged probing (document 04, 4.9). No unused variant is ever compiled.
- Decisions that are only data do not produce code variants. The hash table size and mask, keep/drop of a reduction filter, and filter order live in `PipelineState` and are read by the same code (document 07).

**Time to first morsel is gate G1:** at most 1 ms median and 5 ms maximum (document 02, 2.8). Under I2, that time is planning plus the `direct` compile of the first claimed pipeline. The other half of G1 bounds the total compile time summed over all pipelines of the query to the same numbers, so overlapping compile with execution does not hide it.

## 9.4 Tier-up: the decision procedure

**We use Kohn's extrapolation, with exact remaining work.** Every pipeline source is either a scan with a morsel queue or a materialized buffer, so the number of remaining tuples `n` is known exactly, never estimated. The rate is measured. Only the candidate tier's speedup and compile time are modelled.

```rust
/// Runs at a morsel boundary on the worker that just finished a morsel,
/// at most once per MORSEL_DECISION_PERIOD per pipeline (a CAS on a timestamp).
fn tier_up_decision(p: &PipelineProgress, c: &Calibration) -> Option<BackendKind> {
    if p.elapsed < FIRST_DECISION_AFTER || p.morsels_done < p.workers { return None; }
    if p.compile_in_flight || p.tier_up_refused { return None; }
    let n  = p.remaining_tuples as f64;           // exact
    let r0 = p.tuples_per_sec_per_worker;         // measured over completed morsels, current tier
    let w  = p.workers as f64;
    let t_stay = n / r0 / w;
    let mut best: Option<(f64, BackendKind)> = None;
    for cand in c.candidates_above(p.tier) {      // only backends compiled into this build
        if p.qir_insns > cand.max_qir_insns { continue; }          // document 08, 8.10
        let ct = cand.fixed_ns + cand.ns_per_qir_insn * p.qir_insns as f64; // linear in IR size
        let r1 = r0 * cand.speedup(p.class);
        let done_meanwhile = (w - 1.0) * r0 * ct; // other workers keep going
        let t_up = ct + (n - done_meanwhile).max(0.0) / r1 / w;
        let gain = t_stay - t_up;
        if gain >= ct * BENEFIT_OVER_COST && best.map_or(true, |(t, _)| t_up < t.0) {
            best = Some((t_up, cand.kind));
        }
    }
    best.map(|(_, k)| k)
}
```

This is Kohn's formula (`t1 = c1 + max(n − (w−1)·r0·c1, 0)/r1/w`, ICDE 2018), with three changes:

1. **`n` is exact.** HyPer's `n` was also the remaining morsel count. We state it as a requirement: a pipeline source that cannot report its remaining work never tiers up.
2. **Required margin.** Tier-up must save at least `BENEFIT_OVER_COST` (initially 1.0) times the compile time again. This absorbs error in the speedup model. A multiplicative margin on `t_stay` would never fire at a 4.5% average speedup.
3. **Per-class speedup.** `speedup(class)` is calibrated per pipeline class and architecture, not globally. The initial classes are:
   - scan-filter-aggregate;
   - probe-heavy;
   - string-heavy;
   - arithmetic-heavy (decimal and float expression depth ≥ 8).

   A class with no calibration has speedup 1.0, which means it never tiers up.

**Worked example `[derived]`.** Take one worker, `clif` over `direct` at 1.045x, and a 160 µs compile. Tier-up requires `T·(1 − 1/1.045) ≥ 2·160 µs`, so the pipeline needs about 7.4 ms of remaining `direct` time.

- On an arithmetic-heavy pipeline where `clif` reaches 1.3x, the threshold drops to about 1.4 ms.
- **Most JOB pipelines never qualify.** Most TPC-H SF100 aggregation pipelines do. That is the intended split.

**Tier-up is decided at most once per pipeline per tier.** When the chosen tier is compiled, the next decision considers only tiers above it, so there is no ping-pong. If a compile fails with `Unsupported`, the pipeline sets `tier_up_refused` and stays where it is.

## 9.5 Background compilation and the swap

- **A compile task goes onto the shared worker pool at low priority** (document 13). It is not a dedicated thread. Workers take compile tasks only when no morsel of higher priority is available, so the `(w−1)` term in 9.4 is honest.
- **Concurrency cap:** at most `max(1, workers/8)` background compiles run in the process at once. The queue is ordered by expected `gain` from 9.4.
- **The swap.**
  - `PipelineState.entry` is an `AtomicPtr`, and `PipelineState.code_gen` is an `AtomicU32`.
  - The compiler publishes the code (document 08, 8.8), then stores `entry` with Release and increments `code_gen`.
  - A worker loads `entry` with Acquire at each morsel start. When `code_gen` differs from the worker's last seen value, it executes `isb` on AArch64 before calling.
- **No state migration.** Every tier uses the `PipelineState` layout fixed in QIR (document 05). A worker halfway through a morsel on the old code finishes it there. The next morsel runs on the new code. Both write the same thread-local state.
- **Cancellation.**
  - If the pipeline finishes before the compile completes, the task is not cancelled. Its result goes into the code cache (9.8), because the same function may run again.
  - If the query is cancelled, the task is dropped at its next checkpoint, which is between QIR blocks in `clif`.
- **Old code stays alive** until the query ends. Reclamation is by epochs (document 08, 8.8).

## 9.6 Guards and deoptimization

The planner places every guard and names every deopt target (document 04, 4.7). Generated code never invents a speculation. This section is the runtime half of that contract.

**Two kinds of guard.**

| Kind | Checked | On failure | Examples (document 04 table) |
|---|---|---|---|
| **Pre-check** | at morsel start, from morsel metadata (zone map, encoding tag, null count, max string length) | *dispatch* this morsel to the target variant. No rerun and no counter. | no NULLs in column; strings ≤12 bytes; dictionary-coded input |
| **In-flight** | inside the loop, on computed values | return `Status::Deopt`; rerun the morsel on the target variant | a string built from a computation exceeds the inline limit; hash values from a speculated narrow type |
| **Build-time** | at build insert | switch the *probe* pipeline's variant as a whole before it starts | build side unique on key |

**Prefer pre-checks.** A pre-check costs a few instructions per morsel and never throws work away. The planner should turn a guard into a pre-check whenever the metadata can decide it.

**Narrow accumulators become a pre-check.** Document 04's guard table implements narrow SUM as a headroom check at morsel start:

- Each worker keeps a bound `B` on the absolute value of any of its accumulators.
- Before a morsel, the worker checks `B + rows × max_abs(morsel)` against `i64::MAX`, where `max_abs` comes from zone-map min/max propagated through the aggregated expression by interval arithmetic.
- When the check fails, the runtime widens that worker's thread-local table to the i128 variant. Both layouts are declared in the plan. Execution then continues on the wide variant.
- No morsel is ever rerun, and aggregation, the most common side effect, needs no rollback.
- **If no bound is available, the planner does not speculate narrow.** Documents 04 and 11 follow this rule.

**Restartability rules for in-flight guards** (document 03; document 13, section 13.4):

- R1. Every in-flight guard is placed before the first side effect of the morsel on shared or thread-local state. Or:
- R2. Side effects before the guard go only to append-only thread-local buffers. The runtime records each buffer's high-water mark at morsel start and truncates to it on `Deopt`.
- R3. No guard may follow a hash-table insert, an aggregate update, or output to the result sink. The QIR verifier (document 06) rejects a function that violates R1-R3.

**What happens on `Deopt`:**

```
worker: status = entry(state, morsel)
  Deopt(site) → runtime.rollback_thread_local(marks)
              → target = state.variant_for(site)          // compiled on first use: `direct`, synchronously (~10 µs)
              → status = target(state, morsel)            // same morsel, generic code
              → pipeline.deopt_count += 1
```

**Giving up on the speculation.** A pipeline stops calling the specialized variant for the rest of the query once 3 morsels have deopted, or once more than 1/16 of its completed morsels have. After that, every morsel goes straight to the target.

- Both thresholds are initial values under the provenance rule (9.9).
- This matches the "stop speculating after repeated failure" behavior of JIT runtimes such as JavaScriptCore, which uses exponential backoff after deopt (https://webkit.org/blog/10308/speculation-in-javascriptcore/). The scope is narrower: one query only.

**No memory across queries in v1.** A failed speculation is not remembered for the next execution of the same plan. Document 04 forbids the planner from reading history. Remembering it in the code layer instead would be legal, since it changes only performance. We defer that to document 20 until measurement shows repeated deopts on the same cached plan.

## 9.7 Runtime decision points other than tier

These are the other rows of document 04's section 4.9 list, and how each one appears at the code level. **This document adds no decision point to that list.**

| Decision | Representation | When evaluated | Code variants compiled |
|---|---|---|---|
| Keep or drop a reduction filter | a flag in `PipelineState` | at build finalize, and after the first 100K probes | one |
| Filter order on a scan | a permutation in `PipelineState` driving the filter sequence (document 07) | per morsel | one |
| Build/probe flip | code variant | at join pipeline start | only the chosen one (Rule I3) |
| Hash table size | mask and base in `PipelineState` | at build finalize | one |
| Probe staging: fused or group-prefetch | code variant | at probe pipeline start, table size vs LLC | only the chosen one |
| Aggregation: thread-local or partitioned spill | code variant plus runtime migration | when the reservation is exceeded | lazily, on the first exceed |
| Guard targets | code variants | 9.6 | lazily, on the first failure |

**Cost of the decision.** Each per-morsel decision must cost less than 20 ns. CAKE's learned regret trees fit in under 20 ns (arXiv 2602.04181), and our rule-based versions are simpler.

**Allowed inputs.** Only properties of the morsel or of state built so far:

- selection density;
- code width;
- partition size versus cache;
- match density on probe;
- validity density;
- observed pass rates.

Hardware counters are not used (see the White-Box Micro-Adaptivity discussion in research note E, §9.4).

**Out of scope for v1.** Per-morsel switching between code *flavors*, such as branching versus predicated selection or unrolled versus not, which is the Micro Adaptivity and CAKE space. The flavor is chosen once per pipeline by document 07, from facts. Adding a per-morsel flavor switch means adding a row to document 04's list, and it needs a measured JOB or ClickBench gain first. The Excalibur lesson applies: generate alternatives only where the pipeline's share of runtime makes the gain matter (Amdahl).

## 9.8 The code cache

**Two keys, three levels, LRU by bytes, in memory only.**

**Keys.**

- `K_plan` = hash of:
  - the canonical text of the parameterized A5 plan;
  - parameter types;
  - the catalog version of every referenced table;
  - the session settings that affect semantics and are baked into code (collation, time zone handling, and the other settings document 12 marks "shape");
  - the rudb build id;
  - the target feature set.
- `K_fn` = hash of:
  - the canonical text of one QIR function;
  - the backend;
  - the `TargetDesc`;
  - the build id.

**Keys are compared in full, not by hash.** The hash is 128 bits, from rudb's in-tree hashing, so there is no dependency. It selects a bucket. A hit then compares the stored canonical key bytes. A collision can cost a lookup; it can never cost a wrong answer. We impose this because a code cache is the one place where one bug serves wrong results to every later query: ClickHouse issue #118334 kept serving wrong Int128 results from its expression cache (https://github.com/ClickHouse/ClickHouse/issues/118334, `[snippet]`).

**Levels.**

| Level | Holds | Lookup cost | Hit skips |
|---|---|---|---|
| L0: prepared-statement slot | an `Arc` to the plan entry, per shape class (at most 4) | none; a field load | parse, bind, plan, QIR, compile |
| L1: plan cache (`K_plan`) | per-pipeline (variant → best `CodeHandle`) and the `K_fn` list | one hash of the A5 text | QIR build and compile |
| L2: function cache (`K_fn`) | `CodeHandle` of the best tier compiled so far | one hash per function | compile |

- **L2 is shared across plans.** Two JOB queries that build the same filtered `title` hash table share that function.
- **An L2 entry remembers its best tier.** When a background `clif` compile completes, it upgrades the entry. The next query that hits the entry starts on `clif` directly. This is the only way history reaches execution, and it affects only speed.

**Admission and eviction.**

- Every compiled function is admitted on first compile. A `direct` compile costs about 10 µs, so there is no reason to wait for a third sighting the way ClickHouse does (`min_count_to_compile_expression` = 3, https://clickhouse.com/blog/clickhouse-just-in-time-compiler-jit).
- Eviction is LRU weighted by code bytes. The default budget is `qc.code_cache_bytes` = 64 MB, inside the arena limit of 256 MB (document 08, 8.10). For comparison, ClickHouse defaults to 1 GB with LRU eviction.
- Evicted code is freed by epochs (document 08, 8.8). Running queries hold `Arc`s, so eviction never pulls code out from under a query.

**Invalidation.**

- DDL increments the table's catalog version. Old `K_plan` entries become unreachable and age out.
- `DROP TABLE`, `ALTER TABLE` and `DROP FUNCTION` also purge eagerly by table id, to free memory.
- A change to a semantic setting changes the key.
- A build id change means a new process.
- Statistics changes never invalidate code. Plans depend on facts (document 04), and facts that change the plan change the A5 text, which changes `K_plan`.

**No persistent on-disk cache in v1.** The reasons:

1. `direct` makes a miss cost about 0.5 ms per query. Redshift needs its compilation service and fleet-wide cache because a GCC miss costs seconds: its hit rate is 99.60%, rising to 99.95% with the external cache, and a P50 miss costs 4.3 s (`[snippet]`, via http://muratbuffalo.blogspot.com/2022/09/amazon-redshift-re-invented.html).
2. Loading executable bytes from disk is a code-injection surface. It needs signing, and on macOS it interacts with the hardened runtime.
3. Invalidation across rudb versions and catalog versions on disk is a new class of bug.
4. CEB-style workloads, where every plan is new, get nothing from any cache. Compile was 45% of Umbra's end-to-end time there (document 02).

Revisit this only if the TPC-C or ClickBench cold-start numbers show compile above 10% of the total.

**Verification mode.** `qc.cache_verify = <fraction>` reruns that fraction of cache hits on `interp` and compares results. It is on at 1.0 in CI and in document 15's soak tests.

## 9.9 Thresholds and the provenance rule

**Every constant in the tiering and caching code carries a provenance comment.** The comment gives either the paper and table it came from, or the `rudb-bench` run (commit, machine, suite) that calibrated it. Changing a constant requires rerunning that calibration and attaching the new run to the PR. We do not copy counter thresholds from language JITs. JavaScriptCore uses 500/1,000/100,000 and HotSpot tier 4 about 5,000/15,000 `[snippet]`, and both count calls because they cannot see remaining work. We can.

| Constant | Initial value | Provenance | Calibrated at |
|---|---|---|---|
| `FIRST_DECISION_AFTER` | 1 ms of pipeline wall time | Kohn, ICDE 2018 | C5 |
| interp bound (I1) | 1 morsel, 16,384 tuples | ours; morsel size from document 05 | C5 |
| `BENEFIT_OVER_COST` | 1.0 | ours; absorbs speedup-model error | C5, C12 |
| `speedup(class)` for `clif`/`direct`, x86 | 1.045 for all classes | CGO 2024 average `[derived]` | C12, per class |
| `speedup(class)` for `clif`/`direct`, AArch64 | 1.0, so no tier-up | no published DirectEmit M1 data | C5 on M4 |
| `ns_per_qir_insn`, `fixed_ns` | measured | microbenchmark over the JOB, TPC-H and ClickBench QIR corpus | C2 (`clif`), C3/C4 (`direct`) |
| deopt give-up | 3 morsels, or 1/16 of morsels | ours | C6 |
| background compile cap | `max(1, workers/8)` | ours | C5 |
| `qc.code_cache_bytes` | 64 MB | ours; about 8 KB per function `[snippet, ClickHouse]` gives about 8,000 functions | C9 |

## 9.10 How this plays out per benchmark

- **JOB** (113 queries, 5-18 functions each).
  - Rule I2 compiles everything on `direct` within G1. Almost no pipeline reaches the tier-up threshold, so JOB speed is `direct` speed.
  - That is the right bet. Umbra spent 7.592 s compiling against 0.928 s executing on JOB, and 89% of its end-to-end time was compile (document 02).
  - Headline JOB numbers are reported with the cache cleared between queries (document 17). Warm-cache numbers are reported separately and labelled.
- **ClickBench** (43 queries, each run three times `[GK]`).
  - Run 1 pays the `direct` compile, which must fit the 0.3 ms budget.
  - Runs 2 and 3 hit L1 and skip QIR and compile entirely, and they start on the best tier L2 has recorded.
  - Short queries stay on `interp` only when Rule I1 proves them tiny. Most ClickBench scans are not tiny.
- **TPC-H.**
  - At SF1 it behaves like JOB.
  - At SF100, long aggregation and join pipelines tier up to `clif` in `qc-clif` builds. This is where the decimal-arithmetic class earns its calibration.
  - The default build stays on `direct`, and document 08, 8.13 tracks whether that leaves too much behind.
- **TPC-C** (prepared statements).
  - Compile happens once, at the first `EXECUTE`, into parameterized code. Every later `EXECUTE` goes through the L0 slot with no rebind. DuckDB rebinds on EXECUTE (issue #17237), which is part of the overhead we are beating.
  - The L0 hit path, meaning slot load, parameter copy into `PipelineState`, and the entry call, is budgeted at under 500 instructions. That is our target, inside document 02's 20,000-instruction statement budget.
  - Point pipelines are a single morsel, so they never tier up.
  - Document 14 owns the rest.
- **CEB-style ad hoc workloads.** The cache does nothing and G1 does everything. This is why the design puts compile speed first and the cache second.

## 9.11 Controls and observability

| Setting | Values | Default |
|---|---|---|
| `qc.backend` | `auto`, `interp`, `direct`, `clif`, `llvm` | `auto`. Naming a backend not compiled into the build is an error, not a silent fallback. |
| `qc.tier_up` | `on`, `off` | `on` |
| `qc.code_cache_bytes` | bytes | 64 MB |
| `qc.cache_verify` | fraction 0-1 | 0 (1.0 in CI) |
| `qc.chaos` | test builds only | off |

`EXPLAIN ANALYZE` reports the following per pipeline (document 16): the initial tier and why (I1 or I2); compile time per tier; the morsel index at which a swap took effect; deopt counts per guard site; L0/L1/L2 hit or miss; and the tier-up decision inputs (`n`, `r0`, and the predicted `t_stay` and `t_up`).

**Chaos mode is the correctness test for this whole document.** With `qc.chaos` on:

- each morsel runs on a randomly chosen compiled-in tier;
- in-flight guards fail at random;
- pre-checks dispatch at random;
- the cache evicts at random.

Results must be byte-identical to `interp`. It runs over every suite in CI (document 15).

## What we should take from this document

**With a 10 µs baseline compiler, the interesting question moves from "when do we compile" to "when is anything better than `direct` worth it".** Starting in the interpreter and tiering up, as HyPer did, made sense when the next tier cost milliseconds of LLVM time. For us, everything that is not provably one morsel compiles on `direct` right away, on the worker that claims it, in dependency order. The interpreter is for statements whose input is bounded by metadata, never by estimates.

**Tier-up is Kohn's extrapolation, made safer.** Remaining work is exact, so only the speedup is modelled. The speedup is per pipeline class and starts at "no gain" until measured. Tier-up must also save its own compile cost again. At the published average of a 4.5% `clif` gain, that means about 7 ms of remaining work. The policy will therefore almost never fire on JOB and will fire on SF100 arithmetic, which is what the evidence says it should do. In the default build, without `qc-clif`, the policy is simply `interp → direct`.

**Deoptimization should mostly not be deoptimization.** Guards that the planner can check from morsel metadata become dispatch at morsel start. Narrow accumulators become a headroom check. Only true in-flight guards return `Deopt`, and they obey restartability rules that the verifier enforces. A pipeline gives up a speculation after 3 failures. Nothing is remembered across queries in v1.

**The cache is a plan cache plus a function cache, compared by full key, kept in memory.** It makes ClickBench's repeated runs and TPC-C's prepared statements compile-free. It lets a later query start on the best tier an earlier one reached. It does nothing for ad hoc workloads, which is why the cache is not the plan for compile latency. A persistent cache is deferred until measurement asks for it.

**Every threshold has a paper or a benchmark run behind it, and chaos mode proves timing never changes answers.** Language-JIT call counts are not our model, because we can see remaining work directly. Randomizing tiers, guards and eviction against the interpreter is the test that makes a timing-dependent system safe to ship.
