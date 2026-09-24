# Observability

How a person finds out where a compiled query spends its time, why it is slow, and why it crashed or returned the wrong answer. Generated code starts out invisible to every tool. A profiler sees addresses with no symbols. A debugger sees a frame with no function. `EXPLAIN` sees a plan that no longer exists as code. This document makes each of those visible, at a cost small enough to leave on.

Photon's authors chose not to compile, and in their words "a majority of the work… was around adding tooling and observability" (https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf). The systems that did compile and stayed debuggable all built the same thing: a map from each machine instruction back to the operator that produced it. Umbra built it and published how (https://vldb.org/pvldb/vol14/p3207-neumann.pdf). Tailored Profiling (Beischl et al., EuroSys 2021, https://db.in.tum.de/~beischl/papers/Profiling_Dataflow_Systems_on_Multiple_Abstraction_Levels.pdf) measured what the map costs. UmbraPerf, built on that work, won the VLDB 2025 Best Demo. LingoDB-CT tracks locations through every lowering stage `[snippet]`. We build the map from the first commit, because it cannot be retrofitted into a code generator that did not carry it.

## 16.1 Design rules

1. **Every QIR instruction carries an origin.** Design rule E26 comes first because everything else here depends on it.
2. **Every backend emits a PC-to-origin table.** This includes `interp`, whose "PC" is a bytecode offset.
3. **Every compiled function has a stable, readable name**, registered with the platform's profilers when it is installed.
4. **All generated code keeps a frame pointer**, so any sampling profiler can walk through it without unwind tables.
5. **Every compile phase is timed on every query**, always, and the times are visible in `EXPLAIN ANALYZE`.
6. **Every failure can produce a reproducer** that runs one morsel of one pipeline on any tier with no database.

Rule 4 costs one register on x86-64. AArch64 already reserves `x29` by convention. The alternative is to emit and register DWARF unwind information for every function. That costs compile time on every query, which G1 cannot afford, so frame pointers are mandatory. `direct` always emits the frame-pointer prologue, and `clif` sets Cranelift's option to preserve frame pointers `[GK]`.

## 16.2 Origins: the map from machine code to operator

An origin names the plan node that caused an instruction and the line of the generator that emitted it:

```rust
pub struct Origin {
    pub node: PlanNodeId,                         // A5 physical operator
    pub pipeline: PipelineId,                     // A6 pipeline
    pub role: Role,                               // Build, Probe, Filter, AggUpdate, KeyEncode, ...
    pub site: &'static core::panic::Location<'static>, // generator call site
}
pub struct OriginId(u32); // interned per query in rudb-qc-ir
```

**The builder records the origin without the translator writer doing anything.** The QIR builder's emit methods are `#[track_caller]`, and each translator sets `node` and `role` on entry through a scope guard. Every instruction gets `(current scope, Location::caller())` for free, interned to an `OriginId`. That is one hash lookup per new pair, and consecutive instructions from the same site reuse the previous id.

**Each lowering step keeps a tagging dictionary**, which is Tailored Profiling's term for it:

| from | to | table |
|---|---|---|
| A4 logical node | A5 physical node(s) | `PlanNodeId -> LogicalNodeId` |
| A5 node | A6 pipeline steps | in `Origin` |
| A7 QIR instruction | `OriginId` | inline in the instruction |
| A8 machine code | QIR instruction range | `Vec<(pc_start: u32, pc_end: u32, OriginId)>` |

**The A8 table is written at emit time, one entry per change of origin.** `direct` pushes an entry when the current instruction's origin differs from the previous one. Cranelift's `MachSrcLoc` side table carries the id through (`[GK]`: Cranelift's source locations are a 32-bit field attached to instructions). `llvm` uses debug locations. The table is sorted by construction, so a lookup is a binary search. Its measured cost must stay under 3% of backend time, and document 17 tracks it as part of compile time.

Tailored Profiling's own implementation was 44 + 6 + 6 lines across its components. Ours is larger only because it covers four backends.

## 16.3 Symbols for external profilers

**Every installed function is named `qc:<plan-hash>:p<pipeline>:<tier>:<fn>`**, for example `qc:9f3a1c07:p2:direct:step`. The name alone tells the reader which query, which pipeline and which tier they are looking at. The name is also a lookup key into `rudb_qc_functions()` (section 16.10), which returns the plan and the QIR.

**Linux, perf map.** When `SET qc_perf_map = on` is set, or `RUDB_QC_PERF_MAP=1` is in the environment, the runtime appends one line per installed function to `/tmp/perf-<pid>.map`:

```
7f3a2c001000 3a4 qc:9f3a1c07:p2:direct:step
```

The format is `START SIZE name`, in hexadecimal. It is cheap enough to leave on in every benchmark run, and `perf report` then shows compiled functions by name.

**Linux, jitdump.** When `SET qc_jitdump = on` is set, the runtime writes `jit-<pid>.dump`. It maps the file with `PROT_EXEC` so that `perf record -k 1` notices it, then emits records:

- `JIT_CODE_LOAD` per function,
- `JIT_CODE_DEBUG_INFO` mapping PC ranges to lines of the function's printed QIR,
- `JIT_CODE_MOVE` if the code cache relocates, and
- `JIT_CODE_CLOSE` at exit.

After `perf inject --jit`, perf annotates the generated machine code line by line against the QIR text. The QIR text is written next to the dump as `qc-<plan-hash>-p<pipeline>.qir`, so the debug info can refer to real files. Wasmtime's `crates/jit-debug/src/perf_jitdump.rs` is the reference implementation of the format. This is heavier than the perf map, and it is for investigation, not for benchmarks.

**macOS on the M4.** There is no jitdump. What works:

- **samply** reads perf-map-style symbol files `[GK]`, so the same `/tmp/perf-<pid>.map` lines give named frames in the Firefox profiler on the M4. This is the day-to-day profiler on the development machine.
- **Instruments** walks frames through generated code because of the frame pointers, but shows no names for them. Our own sampler (section 16.4) fills that gap and is the one that works identically everywhere.

**Windows is out of scope**, as it is for the rest of rudb's benchmarking.

## 16.4 Operator attribution

External profilers name functions. The question a person actually asks is which operator the time went to. One pipeline function contains a scan, three probes and an aggregation fused together, so the function name alone cannot answer it. **The built-in sampler maps samples to operators using the origin tables.**

**How it samples.**

- On Linux, a perf-event sampling counter per worker thread, cycles at a fixed period, delivered as a signal.
- On macOS, a timer-driven `SIGPROF` per worker thread.

The handler reads the PC from the signal context and appends `(pc, tag, thread)` to a per-thread ring buffer. The ring is resolved to origins after the query, never in the handler.

**Register tagging for shared code.** Samples that land in `rudb-qc-rt`, such as a string kernel, a `vcall` into a first-engine kernel, or a hash-table resize, are in code shared by every operator. Tailored Profiling solved this by keeping the current operator's tag in a register. We use a thread-local slot instead of a reserved register, which would cost `direct`'s register allocator a register everywhere. Before each runtime call, generated code stores the caller's `OriginId` into `PipelineState.tag`. That is one store per call, and it is emitted only when profiling is compiled in (section 16.5). The sampler reads the slot along with the PC.

**The overhead numbers we design against.** They come from Tailored Profiling on TPC-H:

| configuration | overhead |
|---|---|
| PEBS sample every 5,000 cycles | 35% |
| same, plus register sampling | 38% |
| call-stack sampling | 529% |
| overall at normal sampling rates | 2.8% |

The last row is the target: **operator attribution at under 3% overhead, cheap enough to run in the nightly benchmark.** It is too expensive for timed runs. The 529% row is why the sampler never walks the stack for attribution. The origin table replaces the call stack.

## 16.5 Counters in QIR

Sampling says where time went. Counters say what happened: how many tuples passed a filter, how long probe chains were, how many morsels were deoptimized. **QIR has one instruction for this:**

```
count.add  %ctr.<id>, <i64 value>   ; origin = the operator the counter belongs to
```

Counters live in a per-thread array in `PipelineState`. The adds are plain non-atomic adds, and the arrays are summed when the pipeline finishes. Each operator translator declares its counters in document 07's translator interface. The standard set:

| operator | counters |
|---|---|
| scan | rows read, rows after pushed filters, morsels, morsels skipped by zone map |
| filter | rows in, rows out |
| hash build | rows, distinct keys, max chain length, resizes |
| hash probe | probes, Bloom rejects, matches, chain steps |
| aggregate | rows in, groups, spills |
| guard | checks, failures (per fact) |
| runtime call | calls per `vcall` target |

**Counters are compiled in only when asked for.** A counted function is different code, so it gets a different code-cache key (document 09). Plain `EXPLAIN ANALYZE` compiles with counters. Benchmark runs never do, and `rudb-bench` checks this through the `instrumented` flag in the per-query record. Instrumentation that changes the code under test is honest only if it is labeled.

The count *instructions* are kept out of hot loops wherever the value can be derived. Rows out of a filter equals the selection count at the end of the batch, so one add per batch replaces one add per row. The counter overhead on JOB must stay under 5% in `EXPLAIN ANALYZE`. That makes the counted run a usable picture of the uncounted run.

## 16.6 `EXPLAIN (CODEGEN)` and `EXPLAIN ANALYZE`

**`EXPLAIN (CODEGEN)` shows what would be compiled, without running the query.** Two narrower forms print one artifact each: `EXPLAIN (CODEGEN, PHYSICAL)` the A5 physical plan of document 04, and `EXPLAIN (CODEGEN, PIPELINES)` the A6 pipeline graph of document 05. It prints the A5 tree annotated with pipelines, the facts each pipeline is specialized on, the guards, the tier the policy would start with, and optionally the QIR. For JOB 1a:

```
EXPLAIN (CODEGEN) SELECT MIN(mc.note), MIN(t.title), MIN(t.production_year) FROM ... ;

pipeline p0  build  ht0 <- company_type ct  WHERE ct.kind = 'production companies'
  facts: ct.kind dict(4) ; ct.id dense[1..4]            -> ht0 = direct array[4]
  tier: interp (est. 4 rows)
pipeline p1  build  ht1 <- info_type it     WHERE it.info = 'top 250 rank'
  facts: it.id dense[1..113]                            -> ht1 = direct array[113]
  tier: interp (est. 1 row)
pipeline p2  build  ht2 <- movie_companies mc  ⋉ ht0  WHERE mc.note NOT LIKE '%(as Metro-Goldwyn-Mayer Pictures)%' AND (...)
  facts: mc.note nullable, dict none, avg 14.1 B        -> like: 2 x memmem(SIMD), prefix fast path
  guards: mc.company_type_id in [1..4]
  tier: direct
pipeline p3  probe  movie_info_idx mi_idx ⋉ ht1 ⋈ ht2 ⋈ title t -> agg MIN x3 (registers)
  facts: mi_idx.info_type_id dense ; t.id dense[1..2528312]
  staged probe: batch 16, prefetch ht2, t
  tier: direct
cache: miss  plan-hash 9f3a1c07  functions 4  est. compile 0.21 ms
```

**`EXPLAIN ANALYZE` runs the query and adds what happened**: per pipeline tier history, compile times, morsel counts, per-operator time from the sampler, and counters.

```
EXPLAIN ANALYZE ...

query 9f3a1c07  engine=compiled  total 11.84 ms  first morsel 0.46 ms  cache miss
  frontend 0.19 ms | physical 0.06 | pipelines 0.02 | qir 0.07 | backend 0.12 | install 0.01

pipeline p2  tiers: direct(0.05 ms compile) ; morsels 162 ; 3.10 ms
  scan movie_companies        rows 2,609,129 -> 1,337,088   41%  (sampled)
  like mc.note NOT LIKE ...   in 1,337,088 -> out 1,120,410  33%
  semi-join ht0               probes 1,120,410 hits 1,120,410  9%
  build ht2                   rows 1,120,410 keys 1,087,236   17%
pipeline p3  tiers: direct(0.04 ms) -> clif(1.9 ms, background, from morsel 41) ; morsels 88 ; 8.52 ms
  scan movie_info_idx         rows 1,380,035 -> 250            3%
  probe ht1                   probes 1,380,035 bloom-rejects 1,379,785   7%
  probe ht2 ; probe t         matches 147 ; chain steps 1.02 avg    61%
  agg MIN x3                  rows 142                            2%
  runtime vcall str_min       calls 142                           27%
guards: 3 checked, 0 failed ; deopt morsels 0 ; refusals none
```

The numbers above are illustrative. They show the format, and none of them is a measurement. Two lines in that output are the ones people will read first. `first morsel` is the G1 quantity. The `tiers:` line shows whether background compilation paid for itself on this query.

**The same content is available as a table** through `SET qc_profile_output = 'json'`, which is what `rudb-bench` consumes (document 17, section 17.2).

## 16.7 IR dumps and the round trip

`SET qc_dump = 'a5,a6,a7,asm'` writes each requested artifact for every compiled query to `$RUDB_QC_DUMP_DIR/<plan-hash>/`: the physical plan as text, the pipeline graph, the QIR module, and the disassembly annotated with origins. The disassembly uses the dev-dependency disassemblers from document 15 when they are present. Otherwise it is written as raw bytes with the PC table, for offline decoding.

**QIR's text form round-trips exactly.** `print(parse(print(m))) == print(m)`, and the parsed module executes identically on every tier. Document 15 fuzzes this property. The round trip is what makes QIR text a unit of work in its own right, one that can be run without a database:

```
rudb-qc run   pipeline.qir --state state.bin --morsel morsel.bin --tier direct
rudb-qc diff  pipeline.qir --state state.bin --morsel morsel.bin --tiers interp,direct,clif
rudb-qc time  pipeline.qir --tier direct          # backend compile time only, 1000 reps
```

`rudb-qc` is a developer binary in `rudb-qc`'s `src/bin`. It links the backends and the runtime, and no storage.

## 16.8 Bisecting a miscompile

When the tier matrix in document 15 finds a difference, the job is to name the instruction. **`rudb-qc bisect` does it in four steps, automatically, and each step halves the search.**

1. **By tier.** It reruns the query with `qc_tier` pinned to each backend in turn. The backends that disagree with `interp` are the suspects. If `interp` itself disagrees with the first engine, it stops here: the bug is in a translator, and the next step is the QIR dump, not the backend.
2. **By pipeline.** It runs each pipeline on the suspect tier with every other pipeline on `interp`. Tiers are chosen per pipeline anyway, so this uses the production mechanism. The first pipeline whose output state differs is the suspect.
3. **By pass.** QIR passes (document 06) and backend passes (for example Cranelift's egraph optimizations) can be disabled one at a time with `SET qc_passes = '-licm,-gvn'`. It binary searches over the pass list for the smallest set of disabled passes that makes the difference go away.
4. **By instruction, through function splitting.** It splits the suspect function at a block boundary into two functions. The live values at the cut become the second function's arguments, which the runtime passes through a spill area. It runs the prefix on the suspect tier and the suffix on `interp`, and compares, then binary searches over cut points. This converges to a single block, and within the block the same cut applies at instruction granularity. The result is a QIR function of a few instructions with an input, which is a filed bug.

Steps 1 to 3 use settings that exist in release builds. Step 4 needs `rudb-qc-ir`'s outliner, which is also what the QIR reducer uses. **The reducer is delta debugging over QIR**: remove instructions, replace values with constants, and shrink morsels, while keeping the divergence. It is the QIR analogue of `rudb-compat`'s `src/reduce.rs` for SQL.

## 16.9 When generated code crashes

Generated code never unwinds, and it signals errors through status codes (document 13). A crash therefore means a bug: a bad address, an illegal instruction, or a misaligned access.

**The runtime installs a handler for `SIGSEGV`, `SIGBUS`, `SIGILL` and `SIGFPE`** that checks whether the faulting PC is inside a JIT code region. If it is not, it chains to the previous handler. If it is, it collects:

- the function name and the origin at that PC,
- all general-purpose and vector registers from the signal context,
- the faulting address, and which state region or column buffer it falls in or is nearest to,
- the pipeline id, the morsel id and the batch offset, which generated code keeps in `PipelineState` for exactly this purpose.

It writes these to stderr, then writes a **reproducer bundle** and aborts:

```
qc-repro-<plan-hash>-<unix-time>/
  query.sql           settings.txt         rudb.version
  plan.a5             pipelines.a6         facts.json
  p3.qir              state-before.bin     morsel-41.bin
  crash.txt           # registers, pc, origin, fault address
```

`rudb-qc run p3.qir --state state-before.bin --morsel morsel-41.bin --tier interp` then reproduces the crash, or the non-crash, without the database. `state-before.bin` exists because the runtime snapshots the pipeline state before each morsel whenever `SET qc_repro = on`. That setting is on in the nightly corpus runs, and off by default because the snapshot is a copy.

**Debuggers.** Debug builds implement the GDB JIT interface `[GK]`. On install, each function is wrapped in a small in-memory object file with a symbol and line table pointing at the QIR text. The runtime links it into `__jit_debug_descriptor` and calls `__jit_debug_register_code()`. GDB on Linux and LLDB on macOS, which supports the same interface when its JIT loader plugin is enabled `[GK]`, then show `qc:` frames with QIR line numbers. Wasmtime's `gdb_jit_int.rs` is the reference. Release builds do not do this, because building an object file per function is compile time that G1 cannot afford.

**The time-travel option we are not building.** Umbra built a time-travel debugger for generated code (the VLDB 2021 paper above). The reproducer plus `interp` covers most of what it offered, because `interp` can single-step QIR with full state inspection. We revisit this if the reproducer proves insufficient on real bugs.

## 16.10 Where the millisecond goes

G1 is the first constraint (document 02, section 2.8), so compile time is observed as carefully as execution time. **Every query records a phase breakdown, always.** Each timestamp is one monotonic counter read, `cntvct_el0` on AArch64 and `rdtsc` on x86-64, so the cost is tens of nanoseconds per query.

| phase | budget (median, from document 02) | recorded as |
|---|---|---|
| frontend: parse, bind, rewrite to A4 | 0.3 ms | `frontend` |
| physical plan and pipeline graph, A5 and A6 | 0.1 ms | `physical`, `pipelines` |
| QIR generation and passes, A7 | 0.1 ms | `qir` |
| backend to A8, `direct` | 0.5 ms | `backend`, per function |
| install: map, protect, flush icache, register symbols | inside backend | `install` |
| **time to first morsel** | **1.0 ms median, 5 ms max** | `first_morsel` |

**The backend time is further broken down per function and per backend phase**: instruction selection, register allocation and encoding for `direct`, and Cranelift's own timing passes for `clif`. `rudb-qc time` reproduces it on a single QIR file. That makes a compile-time regression bisectable without a database, the same way a miscompile is.

**`rudb_qc_stats()`** is a table function over the process's history since start:

- compile time percentiles per backend,
- functions compiled,
- code-cache hits, misses and evictions,
- tier switches and deoptimized morsels,
- refusals by reason, and
- the G1 quantity as a histogram.

**`rudb_qc_functions()`** lists the live functions with their names, sizes, tiers, plan hashes and QIR. Both are ordinary tables so that `rudb-bench` and people can query them with SQL.

## What we should take from this document

Everything here depends on one decision made at the first commit: every QIR instruction carries an origin, meaning the operator and the generator call site, and every backend emits a PC-to-origin table. That table is what gives profiles, crashes and miscompiles an operator name instead of an address. A code generator that did not carry origins from the start cannot be made observable later.

External profilers get named functions: perf map always on Linux, jitdump for line-level work, samply's perf-map support on the M4. Operator-level time comes from our own sampler plus origin tables, following Tailored Profiling's 2.8% result, with a tag slot for shared runtime code. Generated code always keeps a frame pointer so that any profiler can walk through it.

`EXPLAIN (CODEGEN)` shows facts, guards, pipelines and the planned tier without running. `EXPLAIN ANALYZE` adds tier history, per-phase compile times, time to first morsel, counters and sampled per-operator time. Counted code is different code, it is cached separately, and benchmark runs never use it.

Miscompiles are bisected mechanically by tier, pipeline, pass and finally instruction, using function splitting against `interp`. Crashes produce a reproducer bundle that runs one morsel on any tier without a database. QIR text round-trips exactly, which is what makes both of those possible.

The 1 ms budget is observed per query, per phase, always, and is reproducible per function with `rudb-qc time`. If G1 regresses, the phase table says which layer spent the time before anyone opens a profiler.
