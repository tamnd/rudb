# Spec 2140 / compiler: the second engine

Written 24 September 2026, against rudb 0.4.12 at `97a146a2`. This folder specifies a second execution engine for rudb that compiles every query to machine code. It shares the parser, binder and logical plan with the first engine and nothing below them. It is written from first principles on purpose. It does not start from `crates/rudb-exec`, `crates/rudb-pipeline` or the four-tier design in `../08-codegen.md`, and where it disagrees with those it says so and says why.

The research behind it is in `research-notes/`: five source files, 3,658 lines, every number linked to where it came from. Numbers in this folder that come from a search summary rather than the source text are marked `[snippet]`, as they are there.

`research-notes/F-fact-classes.md` is a note of a different kind. It lists every fact class document 04 uses and whether rudb records it today, which answers Q8 of document 20.

## Why a second engine, and why now

`../08-codegen.md` and `../planner-v2/10-specialization.md` treat compilation as a tier inside the vectorized engine. The argument cited the Bespoke OLAP ablation: basic generated code over a flat layout bought 1.26x on TPC-H and 0.57x on CEB, while the fully specialized engines bought 12.35x and 51.40x. On that reading compilation came third, behind encoded execution and fusion.

That reading compared the table's corners. The table is a 2x2 of storage (flat or bespoke) against code (basic or optimized), and its sides say the opposite (document 01 section 1.10). Holding storage fixed, going from basic to optimized code multiplies by 4.1x to 24.5x. Holding code at basic, going from flat to bespoke storage multiplies by 1.9x to 3.7x `[derived]`. Three findings from the research this folder rests on turn that into a design.

**The win is delivered by code that knows the data, and that code has to be generated.** Bespoke OLAP's generated engines got their 12x from 97% fused inline aggregation, 74% bitmap semi-joins and 71% dictionary predicate rewrites (`research-notes/A-systems.md` section on Bespoke OLAP). Every one of those is a loop specialized to a fact about the data: this column is dictionary coded with 211 entries, this join key is dense, this child is clustered by its parent. A vectorized engine gets there by writing one kernel per (operation, encoding, type, nullability) point, which is the matrix `group.rs` has been growing into at 5,249 lines. A compiler gets there by emitting the one loop the query needs. Specializing the layout is the storage layer's job. Specializing the code to the layout is the compiler's job, and without a compiler it becomes a combinatorial kernel library. The "optimized" side of the ablation is that code: 5.18x over DuckDB on TPC-H with a flat layout, before storage specialization adds its factor.

**The first engine is at instruction parity with nothing to spare.** On TPC-H SF1 single-threaded rudb matches DuckDB's time while retiring 1.51x its instructions, 27.27G against 17.90G (`rudb-bench/reports/2026-09-24`, `research-notes/D-benchmarks.md` section 5). It wins on instructions per cycle, not on work done. Ten times DuckDB on that workload is 1.79G instructions, a 15x cut. Hekaton's paper states the rule plainly: to go 10x faster the engine must execute 90% fewer instructions. Vectorized interpretation cannot go below the cost of writing every intermediate to a vector and reading it back. Fused compiled loops keep intermediates in registers, which is the only known way under that floor.

**On the benchmark we are focused on, compile time decides who wins, and the published compilers lose it.** On JOB, Umbra executes the 113 queries in 0.928 s at 32 threads and spends 7.592 s compiling them (`research-notes/D-benchmarks.md` section 4, from Umbra's published raw data). End to end that is about 2.1x DuckDB v0.9, not the 20x its execution time suggests. A variant with lookup and filter operators compiles for about 88 s. JOB is 113 short queries over large plans, 3 to 16 joins each, and it is the worst case for a compiler. Any design that treats compile latency as a tuning problem loses JOB. This folder treats it as the first constraint.

Taken together: the compiler is not a tier bolted onto the vectorized engine to win the last 20%. It is the mechanism that turns data facts into code, and it has to be designed around compile latency the way a storage engine is designed around I/O.

## The thesis

**Compile everything, compile it in microseconds, and specialize it to the data.**

Stated so it can be falsified:

1. Every query that the compiled engine accepts runs as generated machine code. There is no per-operator interpretation on the hot path. The interpreter exists as a reference and as a fallback for statements too short to compile, not as a default.
2. Code generation plus backend time is at most **1 ms at the median and 5 ms at the maximum per JOB query**, measured on the M4 and on `c6a.4xlarge`. That is about 67x under Umbra's per-query LLVM cost on the same workload. Document 02 derives the number from the per-query budget.
3. The generated code is specialized to storage facts known when the query starts: encoding, dictionary size, value range, nullability, sort order, clustering, stored links. It is not specialized to the history of previous queries.
4. The compiled engine returns exactly what the first engine and DuckDB return: same rows, same types, same errors, same NULL behavior. `rudb-compat` checks this differentially. It is not assumed.

## What we build, in one paragraph

A bound, rewritten logical plan comes in from `rudb-plan`. A physical planner owned by this engine chooses algorithms and a reduction schedule and records which data facts each choice relies on (document 04). The physical plan is split into pipelines, each one a resumable step function over morsels with explicitly declared state (document 05). A translator in one linear pass turns the pipelines into QIR, a flat, typed SSA IR with database-specific instructions: checked arithmetic, CRC32 hashing, 128-bit integers, German strings, hash table probes (documents 06 and 07). QIR then goes to one of four backends: an interpreter; `direct`, a single-pass emitter we write for AArch64 and x86-64; `clif`, Cranelift; and optionally LLVM (document 08). A policy picks the backend per pipeline from observed morsel progress, compiles in the background, and swaps the function pointer at a morsel boundary with no on-stack replacement (document 09). Generated code calls a precompiled Rust runtime library through a narrow `extern "C"` ABI for everything that is not the hot loop: hash table growth, spilling, string functions, decimal division, regex (document 13).

## Settled decisions

**The unit of compilation is the pipeline, and a query compiles to one function per pipeline.** Not an expression, and not the whole query. A pipeline is a morsel-driven step function with its state declared outside it. That lets tiers switch, lets the scheduler run it on any thread, and lets cancellation and error checks happen at a boundary the generated code does not have to know about. Umbra's state machines are the prior art. Document 05.

**We own the IR.** Every compiler in production that has solved compile latency has its own compact IR: Umbra IR, CedarDB's, SingleStore MBC, Nautilus IR. Cranelift IR and LLVM IR are backend inputs, lowered to from ours. They are not the design surface. QIR uses 4-byte value references and folds constants as it appends, and it has first-class instructions for the operations a query engine does most. Document 06.

**The default backend is a single-pass emitter we write in Rust, and Cranelift is the stepping stone, not the destination.** The CGO 2024 study measured Cranelift at 1.07 s to compile 6,678 TPC-DS functions against 0.06 s for Umbra's single-pass DirectEmit, 16x slower, with code of similar quality (4.62 s vs 4.83 s to run). TPDE (CGO 2026) shows the single-pass design takes about 1.4k lines of target-specific code per architecture and runs within about 9% of LLVM -O0 code, at 14 to 18x its compile speed. TPDE is C++, only emits ELF, and cannot run on the M4 we develop on. So we build its design, not its code. Cranelift gets a working compiled engine on both architectures in weeks and stays as the optimizing tier for long pipelines. Because the workspace does not depend on outside crates, `clif` sits behind the cargo feature `qc-clif`, which is on in CI and benchmark builds. The default release build ships `interp` and `direct` only, and document 20 question Q4 asks whether that costs enough to change. Documents 08 and 19.

**AArch64 is a first-class target from the first line of emitter code.** The development machine is an Apple M4 and Graviton is a deployment target. DirectEmit started x86-only and needed a second implementation later. Winch took from April 2024 to August 2025 to reach AArch64 parity. We write both encoders behind one instruction selection layer, and a pull request that adds a lowering on one architecture without the other does not merge.

**Tier choice is made per pipeline, at runtime, from measured morsel progress, never from the optimizer's cost estimate.** PostgreSQL's cost-threshold JIT flipped plans across the threshold on small statistics changes, and PostgreSQL 19 turned JIT off by default as a result (`research-notes/A-systems.md`). Kohn, Leis and Neumann's extrapolation rule from ICDE 2018 is the policy. Document 09.

**Scans and format decoders are precompiled, vectorized and SIMD, and the generated code starts where they hand off a selection.** Umbra's Data Blocks work showed that generating per-format scan code is a loss. We generate the tuple-at-a-time body. We do not generate decoders. The storage layer's encodings are visible to the compiler as facts, so the body can consume codes rather than decoded values. Documents 07 and 12.

**Hash probes that miss the cache get a staged, prefetching loop, not a fused one.** Kersten et al. measured the compiled engine 32% slower than the vectorized one on TPC-H Q9 because a fused loop cannot hide the memory latency of a probe. Relaxed Operator Fusion's fix, a buffer boundary with group prefetch of about 16, is worth up to 2.2x. JOB is probe-bound, so this is not an optional optimization here. The translator emits both variants, and the program picks one at probe-pipeline start from the actual size of the built table. Document 10.

**Semi-join reduction is a plan stage and its filters are compiled into the scans.** Robust Predicate Transfer and its successors measure about 1.46 to 1.54x on JOB, and `../graph/` makes the reduction exact where a stored link exists. The compiler's part is to make each transferred filter cost one to three instructions per tuple, inlined into the scan loop, and ordered by measured selectivity. Document 10.

**The first engine is the reference and the fallback, not a competitor.** The compiled engine accepts a query only if every operator in it has a translator. Otherwise the query runs on the first engine, unchanged. Coverage grows by adding translators, and the router's refusal rate is a published metric. Differential testing runs every accepted query on both engines and on DuckDB. Documents 03 and 15.

**Generated code never unwinds.** Runtime functions return status codes. Errors go into a per-thread slot and are checked where a status comes back. Panics are caught at the runtime-library boundary and turned into errors. Rust unwinding through JIT frames is never on the normal path. Document 13.

**Prepared statements compile once and run as parameterized code.** Constants that change between executions are loaded from query state. Constants that decide the shape of the code, such as types, nullability, collation and the structure of a LIKE pattern, are baked in. DuckDB reportedly rebinds and replans on `EXECUTE` (issue #17237). Reusing compiled code across executions is the whole TPC-C argument. Document 14.

**Observability is built into the code generation API, not added after.** Every QIR instruction carries the plan node that produced it. Generated functions are registered with `perf` on Linux and with a symbol map on macOS. `EXPLAIN (CODEGEN)` prints QIR and the assembly it became. Photon's team said most of the work of a codegen engine was tooling and observability, and chose not to compile partly for that reason. We take the warning and build the tooling first. Document 16.

## The documents

| | | |
|---|---|---|
| 00 | this file | thesis, settled decisions, reading order |
| 01 | `01-research-2026.md` | the landscape as of September 2026, and what each result forces |
| 02 | `02-the-targets.md` | what winning each benchmark means, the per-query budgets, compile latency as a first-class number |
| 03 | `03-architecture.md` | the two engines, the artifacts, the arrows between them, the routing rule |
| 04 | `04-the-physical-plan.md` | the compiler's physical planner: algorithm choice, reduction schedule, facts and guards |
| 05 | `05-pipelines-and-state.md` | pipeline decomposition, the step function, morsels, the state ABI, parallel instantiation |
| 06 | `06-qir.md` | the IR: types, instructions, NULL model, textual form, verifier, passes |
| 07 | `07-code-generation.md` | the translator layers, operator translators, specialization, the SIMD and scalar split |
| 08 | `08-backends.md` | interpreter, `direct`, `clif`, `llvm`; register allocation, encoders, calls, platform |
| 09 | `09-tiering-and-caching.md` | the switching policy, background compilation, deoptimization, the code cache |
| 10 | `10-joins.md` | JOB first: reduction, hash tables, staged probes, late materialization, compiled LIKE |
| 11 | `11-aggregation-sort-window.md` | aggregation strategies, sort keys, top-N, windows |
| 12 | `12-expressions-and-semantics.md` | strings, decimals, overflow, casts, NULLs, and matching DuckDB bit for bit |
| 13 | `13-runtime.md` | the runtime library ABI, memory, errors, cancellation, spilling, scheduling |
| 14 | `14-oltp-and-short-queries.md` | TPC-C, prepared statements, the point path, time to first tuple |
| 15 | `15-correctness.md` | the differential harness, tier-against-tier testing, fuzzing, what counts as proof |
| 16 | `16-observability.md` | profiling generated code, `EXPLAIN (CODEGEN)`, debugging, the operator map |
| 17 | `17-measurement.md` | the JOB harness in `rudb-bench`, the metrics, and the rules for reporting |
| 18 | `18-milestones.md` | C0 through C12, with a gate measurement on each |
| 19 | `19-crate-layout.md` | the `rudb-qc-*` crates and their dependency rules |
| 20 | `20-open-questions.md` | what this folder does not settle, ranked |

## How to read this if you are short of time

Read 02, then 03, then 10.

Document 02 is where the 10x is either justified or not, benchmark by benchmark, and it is honest about the two benchmarks where the compiler is not the lever. Document 03 is the shape. Document 10 is the JOB answer, which is the current focus and the place where compile latency, reduction and probe latency all meet.

## What this folder does not change

The storage format, the compression layer and `../graph/` link structures are inputs to this engine, not parts of it. The compiler reads facts from them and emits code against their decoders. Where a specialization needs a storage fact that does not exist yet, document 04 names it and it becomes a request to `../storage-v3/` or `../stats/`, not something the compiler works around.

The DuckDB compatibility surface is unchanged. SQL dialect, types, function semantics and error messages are what DuckDB does. Document 12 is the list of places where a compiler is especially likely to get that wrong.

## Relationship to the earlier codegen documents

`../08-codegen.md` specified four tiers inside one engine: interpreted vectorized, fused kernels, Cranelift, and a speculative single-pass emitter. `../planner-v2/10-specialization.md` kept that and moved compilation to third in the order of work.

This folder replaces both for the compiled engine and leaves them in force for the first engine. The three disagreements, stated here so nobody has to find them:

1. **The single-pass emitter is not speculative. It is the default backend.** The condition in `../planner-v2/10-specialization.md` section 10.7 was to build it only if a workload we care about spends a stated fraction of its time waiting for tier 2. The JOB measurement in document 02 meets that condition before a line of tier 2 exists: at Umbra's LLVM compile cost, compilation is 89% of the end-to-end time on the benchmark we are focused on.
2. **Cranelift is not the tier-2 answer for latency.** It stays as the optimizing tier and as the bring-up backend. The CGO 2024 measurement puts it in the "cheap LLVM" band, not the single-pass band.
3. **Fused kernels are not a tier.** In a compiled engine every pipeline is fused by construction. The precompiled vectorized kernels survive as the scan and decode layer below generated code and as the runtime library it calls. They are not a way to run a pipeline.

The first engine keeps its order of work. Encoded execution first is still right for it, and everything it learns about encodings becomes a fact the compiler can specialize on.
