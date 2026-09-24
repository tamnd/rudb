# The research, as of September 2026

Written 24 September 2026. This document lays out what the published record says about compiling queries, and what each result forces on the compiled engine. It is organized by mechanism, not by system. Each section ends with what the engine takes from it. The sources are the five files in `research-notes/`, and every number below carries the URL recorded there. Markers carry over unchanged:

- `[snippet]`: seen only in a search summary.
- `[derived]`: computed by us from primary numbers.
- `[fig]`: read off a figure.
- `[GK]`: general knowledge with no number claimed.
- `[gap]`: nothing published was found.

No number here was produced by us except where marked `[derived]`.

Two warnings apply to the whole document.

First, almost all of the strongest evidence comes from one group, TUM (HyPer, Umbra, CedarDB, TPDE), measuring its own systems on its own hardware. The results are consistent with each other and with the few outside measurements. But a TUM number is a TUM number, and where we lean on one we say so.

Second, the rival moves. DuckDB halved its JOB time between 0.10.1 and 1.3.2, and version 1.5 ships join Bloom filters. A number measured against DuckDB 0.9 is not a number against the DuckDB we will be compared to.

## 1.1 Compile latency is the constraint

**On short queries over large plans, compile time decides who wins, and every published compiler that used LLVM on the query path lost that race at some point.** This holds across fifteen years and every system that published the split. The rest of this document is written under that constraint.

The evidence starts where query compilation did.

**HyPer (2011).** Neumann's paper compiled TPC-CH queries with LLVM in 16-41 ms, against 1556-2592 ms for generated C++ through gcc (https://www.vldb.org/pvldb/vol4/p539-neumann.pdf). That made compilation viable at all, for OLAP queries measured in hundreds of milliseconds.

**Adaptive execution (ICDE 2018).** Seven years later Kohn, Leis and Neumann showed the problem had not gone away. A catalog query executed in under 1 ms after 54 ms of LLVM compilation (https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf). TPC-H Q1 took 59 ms to compile, the largest TPC-H query 146 ms, and the largest TPC-DS query 911 ms. Compile time was near-linear in plan size, which ran from 300 to 19,000 LLVM instructions.

**Umbra (CIDR 2020).** The paper reports that HyPer spent up to 29x more time compiling than executing on cheap queries (https://www.cidrdb.org/cidr2020/papers/p29-neumann-cidr20.pdf).

**Umbra on JOB (VLDB 2024 raw data).** This is the clearest single number for our workload. Umbra's published data for the Diamond paper shows 0.928 s of execution for the 113 JOB queries at 32 threads, and 7.592 s of compilation in its optimized mode (umbra-db/diamond-vldb2024, `[computed]` in `D-benchmarks.md` section 4). That is 67 ms of compile per query `[derived]`. A variant with lookup and filter operators compiled for about 88 s. On TPC-H SF1 the same data shows 1.10 s of compile against 0.107 s of execution. Umbra's adaptive mode normally hides this, which is the point: without a fast tier, compilation dominates.

**The industry saw the same thing.**

- At HYTRADBOI 2025, Neumann and Leis reported machine-generated SQL of up to 10 MB. They also reported that about half of Redshift's end-to-end latency is compilation, despite cache hit rates above 99% (https://www.hytradboi.com/2025/slides/leis-neumann-compilation.pdf).
- Redshift's cache-miss P50 compile was 4.3 s in 2026 `[snippet]`. Redshift Observatory measured an ETL job where 48 of 100 runs recompiled after a version patch.
- SpeQL (arXiv 2503.00714) found that on TPC-DS 10 GB, compilation in the system it studied took "significantly more time than planning or execution". It could run up to 10 s `[snippet]`.
- PostgreSQL 19 ships with `jit` off by default (https://www.postgresql.org/docs/19/runtime-config-query.html). That ends a decade of cost-threshold JIT that flipped on small statistics changes.

**The other side of the ledger is how cheap the front half has become.** Tidy Tuples measured plan-plus-prepare time at TPC-H SF0.01 as 0.66 ms for Umbra, 0.47 ms for DuckDB, 0.53 ms for MonetDB and 1.33 ms for HyPer (https://zenodo.org/records/5770190). A compiled engine can match an interpreter on latency only if code generation and the backend together fit in about the time the interpreter spends planning.

The reference points, in one place:

| Source | Backend | Compile time | What it measured |
|---|---|---|---|
| HyPer 2011 | C++ / gcc | 1.6-2.6 s per query | TPC-CH |
| HyPer 2011 | LLVM | 16-41 ms per query | TPC-CH |
| Kohn 2018 | LLVM optimized | 42-149 ms (TPC-H), 911 ms (largest TPC-DS) | per query |
| Kohn 2018 | HyPer bytecode | 0.4-1.2 ms | per query |
| LB2 2018 | LMS + gcc | 59-736 ms + 175-664 ms | TPC-H; codegen alone 299 ms geomean |
| LingoDB 2022 | MLIR + LLVM | 13 + 68 ms | TPC-H Q2 (https://vldb.org/pvldb/vol15/p2389-jungmair.pdf) |
| Umbra 2024 data | LLVM optimized | 67 ms per query [derived] | JOB, 113 queries |
| Flounder 2021 | Flounder IR + asmjit | 0.21-1.71 ms per query | TPC-H (https://vldb.org/pvldb/vol14/p2691-funke.pdf) |
| CGO 2024 | DirectEmit | about 10 µs per function | 6,678 TPC-DS functions |
| mutable 2023 | V8 Liftoff | under 1 ms | complex queries [snippet] |

The rule of thumb that falls out is this:

| Tier | Typical compile time |
|---|---|
| LLVM optimized | 50-150 ms for an average analytic query, up to about 1 s for the largest |
| LLVM -O0 and Cranelift | 5-25 ms |
| Single-pass emitters | 0.1-2 ms |
| Bytecode translation | under 1 ms |

JOB on DuckDB 1.3.2 single-threaded averages about 0.49 s per query (55.3 s for 113 on a Xeon E-2236, arXiv 2511.16455) `[fig]`. Ten times that is 49 ms per query, all in. LLVM-optimized compile alone exceeds that budget on its own. A single-pass emitter uses about 2% of it.

**What the compiled engine takes.**

- Compile latency is a gate, not a tuning target. G1 is at most 1 ms at the median and 5 ms at the maximum to the first morsel, which is 67x under Umbra's per-query LLVM cost on JOB.
- Code generation must be linear in plan size and allocation-light. The front half (bind, rewrite, physical plan, translate) has to stay near DuckDB's 0.47 ms, or the backend's speed is irrelevant.
- The existence of 10 MB SQL and 10,000-join queries means "linear" has to hold for real, not just for benchmark-sized plans.

## 1.2 The IR question

**Every system that solved compile latency owns a compact, database-specific IR, and lowers to general compiler IRs only as a backend step.**

**Umbra IR is the reference design** (Tidy Tuples, https://zenodo.org/records/5770190; CGO 2024, https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf).

- Instructions are variable-length and stored in one buffer. Values are 4-byte offsets. There are no use-lists.
- Constants are folded and deduplicated at append.
- The only transform is dead-code elimination, which removes about 4% of code.
- Database operations are single instructions. Checked addition with an overflow successor (`checkedsadd`), a trapping subtraction that calls `throwOverflow()`, `crc32`, `isnull`, 128-bit arithmetic and inlined address computation are all first-class.

**Tidy Tuples puts five layers above the IR:** operator translators, data structures, tuples, SQL values, and a typed codegen API. Only the SQL-value layer knows about NULL, overflow and casts. The structured `If`, `Loop` and `Function` builders emit SSA and phi nodes directly, so no mem2reg pass exists. The generator is more than 1000x faster than LB2's, which needed 299 ms geomean just to produce code.

**Other systems chose differently, and paid for it.**

| System | IR choice | Result |
|---|---|---|
| LB2 (https://www.cs.purdue.edu/homes/rompf/papers/tahboub-sigmod18.pdf) | Staged Scala interpreter (Futamura projection) | Code quality matched HyPer, but generation alone cost 299 ms geomean |
| LingoDB (https://www.vldb.org/pvldb/vol16/p3461-jungmair.pdf) | MLIR dialects with progressive lowering | Very compact code: aggregation in 384 lines against 1,358 and parallelization in 347 lines; 4.8x DuckDB on TPC-H SF10 and 3.9x on TPC-DS. But the LLVM path cost 13 + 68 ms on Q2, and the fast BASELINE mode goes through TPDE and is Linux-only |
| Nautilus (https://nebula.stream/paper/grulich_sigmod2024.pdf) | Traces C++ operator code written over `val<T>` into its own IR | Each control-flow split costs another trace pass, O(2^n) in the worst case. Its MLIR backend takes tens of ms, and Umbra beats it on complex queries. Headline numbers are a [gap] in our notes |
| mutable (https://openproceedings.org/2023/conf/edbt/paper-156.pdf) | Emits WebAssembly and lets V8 tier it | Liftoff compiles complex queries in under 1 ms [snippet]; TurboFan is up to 6.6x faster than HyPer's optimized mode. Costs: a C++ V8 embedding, Wasm's memory model, no SIMD beyond 128 bits |
| Flounder (https://vldb.org/pvldb/vol14/p2691-funke.pdf) | Thin, x86-shaped IR with virtual registers, through asmjit | 0.21-1.71 ms per TPC-H query, 70.1x shorter than HyPer on average. x86 only |

**Cranelift IR and LLVM IR are poor design surfaces for us.** CGO 2024 had to add CRC32, overflow-checked arithmetic and wide multiply to Cranelift before Umbra could use it. CRC32 alone improved the TPC-DS mean by 19%. CLIF also has no pointer or aggregate types. On the LLVM side, 3,876 FastISel fallbacks to SelectionDAG cost 36% of instruction-selection time, and all of them came from i128 and intrinsics. Those are exactly the operations a query IR uses most.

TPDE's Wasm experiment makes the related point from the other direction: 41% of TPDE-CLIF compile time was Wasm-to-CLIF translation, not code generation. Building a second IR is itself a cost.

**What the compiled engine takes.**

- We own the IR (QIR). It is flat and typed SSA, with 4-byte value references and constant folding on append.
- Checked arithmetic, `crc32`, `isnull`, 128-bit integers, German-string operations and hash-table probes are first-class instructions that each backend lowers in its own best way.
- The generator is a Tidy-Tuples-style layered builder API in Rust, with no tracing and no staging framework on the query path.
- CLIF and LLVM IR are produced from QIR by the `clif` and `llvm` backends. They are never the design surface.

## 1.3 Backends

**A single-pass emitter compiles about 16x faster than Cranelift and 100x faster than optimized LLVM, and gives up 10-20% of execution speed in aggregate.** That trade is the right one for almost every query we will see.

CGO 2024 (Engelke and Schwarz) is the only rigorous comparison of backends on database code (https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf). It compiled all 6,678 TPC-DS functions and ran the queries at SF10. Times are totals in seconds, compile / execution:

| Backend | x86-64 (Xeon Gold 6338) | AArch64 (M1, Asahi) |
|---|---|---|
| Umbra interpreter | 0.03 / 15.40 | 0.02 / 64.55 |
| DirectEmit (single pass) | 0.06 / 4.83 | not merged in 2023 |
| Cranelift | 1.07 / 4.62 | 0.61 / 16.37 |
| LLVM cheap (-O0, FastISel) | 1.63 / 5.23 | 0.74 / 19.45 |
| LLVM optimized | 11.36 / 4.12 | 5.86 / 12.88 |
| GCC via C | 48.88 / 4.28 | 41.64 / 13.99 |

Three findings from the paper:

1. **Cranelift compiles only 20-35% faster than cheap LLVM.** Its code is slightly better than cheap LLVM's.
2. **DirectEmit compiles in 64 ms total, about 10 µs per function.** About 75% of its analysis time is liveness, and register allocation is about 30% of its code-generation phase.
3. **LLVM optimized is 12-15% faster in aggregate than DirectEmit or Cranelift, and up to 38% faster on single queries** (TPC-DS Q17, 0.93 s against 1.29 s). End to end, DirectEmit was almost always the best choice at TPC-H SF10. LLVM optimized paid off on several queries only at SF100.

**Flying Start**, DirectEmit's original form in Tidy Tuples, is the same trade viewed from the other end: 108x faster to compile than LLVM -O3, and 1.2x slower code. HyPer's bytecode VM compiled 91x faster but ran 4.1x slower. LLVM -O0 compiled only 6x faster and ran 1.3x slower. On a 2,000-join query of 108,000 IR instructions, the three backends took 150 s (LLVM optimized), 4 s (FastISel) and under 0.04 s (Flying Start).

Two more details matter for our design:

- **Register allocation is worth 32% of execution time** against an all-stack version.
- **Linear scan was tried and rejected.** It gave 1% faster code for 14% more compile time.
- **The code-quality gap is instruction count, not memory behavior.** Flying Start executes 2.3x the instructions of optimized LLVM at 1.4x higher IPC, with equal branch and LLC misses. On memory-bound analytic work that gap shrinks.

**TPDE (CGO 2026) is the current state of the art for single-pass backends** (https://arxiv.org/pdf/2505.22610; final numbers at https://home.cit.tum.de/~engelke/pubs/2602-cgo1.pdf).

Design:

- It is a framework for any SSA IR. One analysis pass does loop detection, block layout and Kohn-style liveness.
- One fused pass does instruction selection, greedy register allocation and encoding.
- Instruction patterns are written as small C functions that LLVM compiles into encoder routines ("snippet encoders").

Results:

- TPDE-LLVM compiles SPEC -O0 IR 13.88x (x86-64) and 18.29x (AArch64) faster than LLVM -O0, with code within ±9%.
- On Umbra IR at TPC-DS SF1, TPDE matched DirectEmit, 0.087 s against 0.11 s to compile on x86-64, with equal execution time.
- The Umbra adapter is 3.3 kLOC with 1.4k target-specific. The arXiv v1 figures differ slightly. DirectEmit is about 11 kLOC for both architectures.

Limits: TPDE is C++, emits ELF only, has no Mach-O support and no Rust port, and does not support vector operations at -O2.

**Cranelift in 2026** (cranelift-jit 0.136.0, released 21 September 2026) is pure Rust, mature on AArch64 including macOS, and 128-bit-SIMD only.

- Its aegraph mid-end buys about 2% faster code for 7-8% more compile time (https://cfallin.org/blog/2026/04/09/aegraph/).
- The single-pass `fastalloc` register allocator compiles 1.07-5x faster and produces 1.06-7.5x slower code. It was disabled and re-enabled after correctness bugs through late 2025.
- TPDE's CLIF backend compiled 4.94x faster than Cranelift with its default allocator and 3.10x faster than Cranelift with `fastalloc`. So Cranelift's fixed per-function pipeline stays several times above a single-pass design no matter which allocator it uses.
- CGO 2024 also found no GOT/PLT handling in `cranelift-jit` (far calls crash) and no unwind info. We do not need unwinding (section 1.4 and document 13), but far calls must be handled.

**Copy-and-patch is the fastest to compile and the weakest in code.**

- The OOPSLA 2021 paper (https://arxiv.org/abs/2011.13127) compiled a high-level language up to 276x faster than LLVM -O0. Its code was 14% faster than -O0 and 22-25% slower than -O1 through -O3.
- It needed 98,831 stencils (17.5 MB) to cover the type and operand space.
- In TPDE's own comparison, a copy-and-patch compiler produced code 2.32x slower and 4.27x larger than TPDE's, at similar compile speed.
- CPython's production copy-and-patch JIT gains only 5-12% over its interpreter in 3.15 (https://blog.python.org/2026/03/jit-on-track/). Stencils without cross-stencil register allocation do not beat a good interpreter by much.

**LLVM remains the ceiling on code quality, and it is not cheap even at -O0.**

- There is about 1 ms of fixed startup per compilation.
- GlobalISel on AArch64 is 47% slower than FastISel (EuroLLVM 2025, https://llvm.org/devmtg/2025-04/slides/technical_talk/engelke_faster.pdf).
- On the dev box, ORC's JITLink supports Mach-O `[GK]`.

**Winch** reached AArch64 parity in Wasmtime 35 in August 2025, after about 1.5 years (https://bytecodealliance.org/articles/winch-aarch64-support). It is a Rust existence proof that a small single-pass compiler for both ISAs is maintainable. It is also a warning that the second ISA is not a weekend.

**Platform requirements, before any of this runs** (`B-backends.md` section 12, `[GK]` where marked):

- macOS arm64 needs `MAP_JIT`, the per-thread `pthread_jit_write_protect_np` toggle and `sys_icache_invalidate`.
- Linux AArch64 needs explicit icache maintenance. x86 does not, which is the classic "works on x86, crashes on Graviton" bug.
- AArch64 `BL` reaches only ±128 MB, so calls into the runtime library use absolute indirect calls or code allocated near the binary.

**What the compiled engine takes.**

- `direct`, a TPDE/DirectEmit-design single-pass emitter written in Rust for AArch64 and x86-64, is the default backend. Its budget is about 10 µs per function, with code within about 1.2x of optimized LLVM.
- It uses greedy register allocation, not linear scan.
- `clif` is the bring-up backend (weeks to a working engine on both ISAs and macOS) and then the optimizing tier for long pipelines. It is not the latency tier.
- `llvm` is a feature, off by default, evaluated at C12 against the 12-15% it can buy on large scale factors.
- `interp` exists for correctness and for statements too short to compile. Its 3.2-3.9x gap to compiled code (CGO 2024) rules it out as a default.
- Copy-and-patch and TPDE-by-FFI are refused (section 1.11).
- Both ISAs merge together. The W^X, icache and far-call layer is written before the first emitter.

## 1.4 Tiering and adaptivity

**Switching tiers at a morsel boundary needs no on-stack replacement, and the policy that decides it should be a cost extrapolation from measured progress, not a counter or an optimizer estimate.**

**Kohn, Leis and Neumann's rule** (ICDE 2018 best paper, https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf) is still the proven policy.

Each pipeline starts in HyPer's bytecode VM, which is about 800 lines. LLVM's own interpreter was more than 800x slower. After 1 ms, and then after every morsel, one thread extrapolates the remaining time three ways: keep interpreting, compile unoptimized, or compile optimized.

- Each estimate charges the compile time.
- Each estimate credits the w-1 workers that keep going while one thread compiles.
- The cheapest option wins.

The results:

- Geometric-mean execution at SF1, one thread: 232 ms in bytecode, 60 ms unoptimized, 46 ms optimized.
- Compile cost: bytecode 0.4-1.2 ms, unoptimized 6-23 ms, optimized 42-149 ms.
- On TPC-H Q11 the adaptive mode beat the three fixed choices by 10%, 40% and 80%.

Because pipeline state lives in memory, the next morsel just calls a different function pointer.

**Umbra later simplified this.** CGO 2024 describes starting every pipeline in DirectEmit and moving to LLVM after a few executions if a code-size heuristic says so. The authors state that morsel-driven execution keeps each call short enough that "advanced mechanisms for switching functions are not necessary".

CedarDB exposes the same idea as a `compilationmode` setting with adaptive, interpreted, direct, cheap and optimized values (https://cedardb.com/blog/compilation/). Its ASM backend delivers "nearly 90% of the throughput in 1% of the compile time".

**Language VMs teach the counters, not the mechanism.**

| VM | Tiers | Tier-up thresholds |
|---|---|---|
| JavaScriptCore | LLInt → Baseline → DFG → FTL | 500 / 1000 / 100,000 points; a call is worth 15, a loop iteration 1; exponential backoff after deoptimization |
| V8 | Ignition → Sparkplug → Maglev → TurboFan | about 500 and 6,000 invocations [snippet] |

JSC's DFG costs about 4x Baseline to compile, and FTL about 6x DFG (https://webkit.org/blog/10308/speculation-in-javascriptcore/).

Their tier-up needs frame mapping and deoptimization metadata because hot loops are unbounded and state lives in frames. A morsel engine has neither problem. What carries over is background compilation (workers never wait for the compiler) and backoff (do not recompile a pipeline shape that recently lost).

**Production engines that compile on a threshold have bugs and cliffs to show for it.**

- ClickHouse JITs an expression after 3 executions into a 1 GB cache and gains 1.5-3x on expressions and 1.15-2x on aggregation (https://clickhouse.com/blog/clickhouse-just-in-time-compiler-jit). Issue #118334 reports Int128 wrong results from the fourth run, which is exactly when the compiled code takes over.
- Spark's whole-stage codegen falls back to interpretation above 8,000 bytes of bytecode and fails above the JVM's 64 KB method limit.
- SAP HANA's HEX runs its own L interpreter while LLVM compiles in the background, and routes unsupported queries to the old engines.
- SingleStore went MPL → MBC bytecode → LLVM and interprets until compilation finishes.

**Adaptivity inside a tier is the second half of the story.**

- **Permutable Compiled Queries** (https://www.vldb.org/pvldb/vol14/p101-menon.pdf) compile each conjunct as its own function and reorder the array by sampled selectivity. That stays within 10% of optimal where a static order can be 4.4x off.
- **CAKE** (arXiv 2602.04181, February 2026, a Rust prototype) picks the kernel variant per morsel with a contextual bandit compiled into "regret trees". A decision takes under 20 ns, and the gain is up to 2x lower latency.
- **Excalibur** (https://www.vldb.org/pvldb/vol16/p829-boncz.pdf) measured the Amdahl limit. A 10x speedup on 40% of a query, from the start, gives at most 1.5x. It also found a fragment code cache is essential: 64 cached fragments made Q1 about 26x faster.

**What the compiled engine takes.**

- Tier choice is per pipeline, at runtime, by Kohn-style extrapolation over measured morsel rate, remaining tuples, worker count and a calibrated compile-time model (linear in QIR size, per backend and ISA).
- It is never driven by the optimizer's cost estimate.
- Compilation happens on a background thread, and the switch is a function-pointer swap between morsels. Every tier shares one pipeline-state ABI: `extern "C" fn(state: *mut PipelineState, morsel: *const Morsel) -> Status`.
- Speculation on data facts (no NULLs, short strings, no overflow, dictionary input) uses guards that return a DEOPT status before the first side effect. The morsel then reruns on the generic variant.
- Variant choice within a tier (PCQ permutation, CAKE-style bandits) is spent only on the pipelines that dominate runtime, because Excalibur's Amdahl bound caps late or partial wins.

## 1.5 Compiled vs vectorized, and the hybrid

**The debate is settled in practice. Fused compiled loops win on computation, vectorized code wins on memory stalls, and the systems that are fastest in 2026 compile the pipeline body and keep precompiled, vectorized kernels at its edges.**

**Kersten et al. (VLDB 2018)** built both paradigms with identical algorithms (https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf).

- Typer (compiled) was 74% faster on TPC-H Q1. Tectorwise (vectorized) was 32% faster on Q9 and 4% faster on Q3.
- Per tuple on Q1: Typer took 34 cycles and 68 instructions; Tectorwise took 59 cycles and 162 instructions.
- Vectorized execution ran up to 2.4x more instructions and up to 3.3x more L1 misses.
- It won on joins because independent loads in a vector overlap their cache misses.
- SIMD gave 8.4x in microbenchmarks and 1.4x on Q6.
- Interpretation overhead in the vectorized engine was under 1.5%.

**The follow-ups closed the gap from both sides.**

- **Relaxed Operator Fusion** (https://www.vldb.org/pvldb/vol11/p1-menon.pdf) puts a small buffer of tuple IDs before a probe into a cache-exceeding hash table and group-prefetches it. The best group size was 16. That is up to 2.2x over pure fusion and 1.8x over HyPer and Vectorwise.
- **Lang et al.** refill SIMD lanes with AVX-512 compress/expand when fewer than about 75% of lanes are active. That gives up to 34% on a scan and 25% on a join probe (https://db.in.tum.de/~lang/papers/simd_divergence.pdf).
- **Data chunk compaction** does the vectorized-engine version of the same fix. It is worth up to 63% in DuckDB on JOB, TPC-H and TPC-DS `[snippet]`.
- **The filter representation should follow selectivity.** Ngom et al. put the crossover between selection vectors and bitmaps at about 0.15 (https://db.cs.cmu.edu/papers/2021/ngom-damon2021.pdf).
- **HyPer's Data Blocks** never generated scan code. A precompiled SIMD scan over compressed blocks hands matching tuples to the compiled pipeline, because the product of formats and predicates would explode code size.

**The single-backend hybrids are the most direct evidence.**

- **InkFuse** (https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/inkfuse.pdf) generates one IR for both a vectorized interpreter and a compiler. It runs the first morsels interpreted, with a split of 5% / 5% / 90%. At SF0.1 every query finished under 20 ms without JIT, and interpretation beat compilation by up to 10x there.
- **VOILA** (http://vldb.org/pvldb/vol14/p1067-gubner.pdf) synthesizes both flavors from one DSL. It was 30% to 17.5x faster than DuckDB and LegoBase `[snippet]`, and the best flavor was about 3x better than the average one.

**Production chose the opposite end and explained why.**

- **Photon** (https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf) is vectorized and specialized. It averages 4x over Databricks Runtime, with a maximum of 23x on Q1. The team says "a majority of the work" on codegen was tooling and observability, and aggregation took two months with codegen against a couple of weeks vectorized.
- Every vectorized cloud engine surveyed (Photon, Velox, Snowflake, Databend, Firebolt, DuckDB) either never compiled or limited JIT to expressions.
- Velox's expression codegen took up to 10 s (https://www.vldb.org/pvldb/vol15/p3372-pedreira.pdf).
- DataFusion's JIT was removed in 23.0.0 (2023).
- The engines that compile whole queries in production are HyPer/Tableau, Umbra/CedarDB, SingleStore, Redshift, HANA HEX, Spark WSCG and Hekaton.

**Umbra's own lead does not isolate the paradigm.** Umbra is 3.0x HyPer on JOB and 1.8x on TPC-H SF10 (CIDR 2020). The paper credits storage, buffer management and compilation together, and never splits them.

**What the compiled engine takes.**

- Scans and format decoders are precompiled, vectorized and SIMD, and they hand a selection vector of up to 1,024 rows to the generated body.
- Morsels are about 16,384 tuples, aligned to row groups. The Morsel paper found overhead negligible above 10,000 (https://db.in.tum.de/~leis/papers/morsels.pdf).
- The body is fused tuple-at-a-time in registers. A `vcall` instruction lets it call a first-engine vectorized kernel where one already exists and is better.
- Probes into tables larger than the cache get a staged, prefetching loop. The choice between fused and staged is made at probe-pipeline start from the table's actual size.
- Fused kernels are not a tier. They are what `vcall` calls.
- Photon's warning is taken literally: the operator map, `EXPLAIN (CODEGEN)` and perf registration are built before the first optimization. Tailored Profiling cost Umbra 44+6+6 lines for 2.8% overhead (https://db.in.tum.de/~beischl/papers/Profiling_Dataflow_Systems_on_Multiple_Abstraction_Levels.pdf).

## 1.6 Joins for JOB

**JOB speedups compose from three independent sources: plan robustness through semi-join reduction, hash-table and probe efficiency, and string predicates.** No paper combines all three and reports a JOB total. Our 10x has to be assembled from parts.

**The workload** (https://www.vldb.org/pvldb/vol9/p204-leis.pdf):

- 113 queries from 33 templates.
- 3-16 joins each, average 8.
- Every query is alpha-acyclic.
- Every projection is wrapped in MIN, so the output is one row and the cost is all join pipeline and scan.
- cast_info has about 36M rows.
- Predicates are heavy on LIKE, IN-lists and disjunctions over `movie_info.info` and `movie_companies.note`.

**Absolute totals are scarce and version-sensitive.**

| System | JOB, 113 queries | Setup | Source |
|---|---|---|---|
| DuckDB 1.3.2 | 55.3 s [fig] | 1 thread, Xeon E-2236 | arXiv 2511.16455 |
| DuckDB 0.10.1 | 123.9 s [fig] | same | same |
| DuckDB 0.9 | 18.210 s | threads not recorded | Umbra raw data [computed] |
| Umbra | 7.756 s | 1 thread; compile not included | same |
| Umbra | 0.928 s execution + 7.592 s compile | 32 threads | same |

No Umbra, CedarDB or Hyper whole-benchmark JOB total appears in any paper.

**Semi-join reduction is the biggest plan-level lever, and it is being absorbed into DuckDB.**

| Technique | Gain on JOB over DuckDB | Robustness and regressions |
|---|---|---|
| Robust Predicate Transfer, SIGMOD 2025 (https://arxiv.org/abs/2502.15181) | 1.46x | Worst-to-best join-order ratio drops from 30.4 to 1.2 (average) and from 371 to 1.6 (maximum). Bloom work is 12% of JOB time. Bushy plans add only 11% over left-deep once it is on. Below 0.9x on at least 28% of SQLStorm queries |
| RPT+, PVLDB 2026 (https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt+-vldb26.pdf) | 1.47x | Below 0.9x on 2.1% of SQLStorm. Uses asymmetric transfer, a min-max → Bloom → exact cascade, and a cache-sectorized Bloom filter at 20 bits/key, k=7, 64-byte blocks, 2.48 cycles/tuple. Filters kept only if sampled selectivity is under 0.35. Regressions on templates 8, 10 and 24 came from breaking zone-map skipping |
| Parachute, PVLDB 2025 (https://arxiv.org/abs/2506.13670) | 1.54x (1 thread) | Costs 14.35% extra space and 3.9x load time |
| Yannakakis+ (https://arxiv.org/abs/2504.03279) | mean 1.42x, maximum 14.84x | |
| Shredded Yannakakis (https://arxiv.org/abs/2411.04042) | maximum 6.4x | Maximum slowdown 1.3x |
| TreeTracker (https://arxiv.org/abs/2403.01631) | average 1.11x | Shrinks cast_info in 6a from 36M to 486 rows |

SQL Server 2025 already ships bitmap cascades costed by its optimizer as a Yannakakis bottom-up pass, up to 3.47x on TPC-H SF100 (https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf). DuckDB 1.2 transfers min-max and small IN-lists, and 1.5 pushes join Bloom filters into probe-side scans `[snippet]`. Expect DuckDB to absorb 1.2-1.5x of these gains.

**Hash-table design is the biggest engine-level lever.** Birler et al.'s unchained table (DaMoN 2024, https://db.in.tum.de/~birler/papers/hashtable.pdf) works like this:

- Each directory word holds a 48-bit pointer and a 16-bit Bloom tag, with 4 bits set per key from a 2,048-entry table. The false-positive rate is about 1/169 at load factor 0.65.
- The build is partitioned and bump-allocated into a dense array, so each slot's matches are contiguous.
- Hashing is CRC32 plus a multiply.
- The probe relies on out-of-order execution rather than explicit prefetch.

It measured about 2x over Robin Hood open addressing across 10,312 queries including JOB. It was up to 30% slower on tiny queries.

Related findings:

- Bandle et al. (SIGMOD 2021) found radix-partitioned joins rarely pay for their complexity in a compiling engine (https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf).
- Group prefetching gave 2.7-3.7x on probe-like lookups, ahead of coroutines and AMAC (https://doi.org/10.14778/3149193.3149202).

**Structure beats cleverness on acyclic workloads.**

- **Diamond-hardened joins** split a probe into Lookup and Expand, so n:m fan-out can be deferred (https://db.in.tum.de/people/sites/birler/papers/diamond.pdf). On JOB only the contiguous-match-range hash table helped. Pure worst-case-optimal joins were about 25x slower on JOB in Umbra.
- **Freitag's WCOJ hybrid** had 5 false negatives out of 923 joins and no JOB slowdown (https://doi.org/10.14778/3407790.3407797).
- **Free Join** was 2.94x DuckDB geomean on JOB, against a 2023 DuckDB (https://arxiv.org/abs/2301.10841).
- **Factorized execution (FFX, arXiv 2609.09002)** reached 102-105x on many-to-many analytics. JOB's MIN-only outputs are duplicate-insensitive, so fan-out need never be enumerated. No paper isolates that on JOB `[gap]`.

**What the compiled engine takes.**

- Semi-join reduction is a physical-plan stage. The compiler inlines each transferred filter into the scan loop as a few instructions per tuple, cheapest first (min-max, then Bloom, then exact), and keeps or drops it by sampled selectivity. It must never disable zone-map skipping.
- The join hash table is unchained, with in-word tags and contiguous match ranges.
- Lookup and Expand are separate QIR operations, so Expand can be deferred and MIN pushed below fan-out.
- A tiny-build path (direct-mapped or perfect hash) covers JOB's dimension tables.
- Staged group-prefetch probes are emitted alongside fused ones.
- Worst-case-optimal joins are not on JOB's path.
- The 10x target is measured against current DuckDB, which already has half the reduction story.

## 1.7 Strings and patterns

**On JOB and ClickBench, string predicates are where compiled code has shown its largest single-operator wins, because a pattern known when the query starts can be compiled into a matcher specialized to it.**

**Compiled LIKE.**

- Riedl et al. (ADMS 2023, https://db.in.tum.de/~riedl/papers/like-codegen.pdf) generated pattern-specific search code in Umbra, up to 2.5x faster than interpreted LIKE.
- "Teach Your DBMS to LIKE Strings" (arXiv 2608.23307, PVLDB 20(1), 2026; authors Nguyen, Ginter, Duc-Tam Nguyen, Neumann, Leis) goes further. It splits the pattern on `%` and matches each segment with SIMD primitives (short), Boyer-Moore (medium) or Two-Way (long), with skip tables built at compile time. Underscores become fixed skips, and prefix patterns are checked on the German string's inline prefix without a dereference. The compiled filter was 13.3x faster than DuckDB and 14.3x faster than baseline Umbra on a filter stress query.
- For column-LIKE-pattern-column joins, the same paper builds an Aho-Corasick automaton over the pattern literals. That wildcard join ran up to 30.6x faster than DuckDB v1.4.4 and 114.75x faster than baseline Umbra. One join query was 81.3x faster than DuckDB, where baseline Umbra timed out beyond 180 s.
- One co-author, Duc-Tam Nguyen, appears to be this project's author. We cite the numbers as published in a peer-reviewed venue and have not reproduced them in rudb.
- The paper also found that better LIKE selectivity estimates changed join orders. That is a planner effect, and it cuts both ways.

**Compressed and dictionary domains.**

- **FSST** (https://www.vldb.org/pvldb/vol13/p2649-boncz.pdf) supports equality on compressed strings when both sides share a symbol table. Its TPC-H overhead was at most 3%.
- **FSST-domain LIKE** (Pop, Riedl, Neumann, DaMoN 2026) compiles patterns into automata over symbol codes. It is 2.5-17x faster than decompress-then-match `[snippet]`.
- **Dictionary evaluation** runs the predicate once per distinct entry. It suits JOB's low-cardinality columns (keyword, kind_type, info_type) and not high-cardinality `movie_info.info`. No 2025-2026 paper gives JOB numbers `[gap]`.

**ClickBench makes the same point at a larger scale** (c6a.4xlarge, hot, `[computed]` from the result JSON):

| Query | DuckDB | Umbra | Gap | Share of DuckDB's 26.25 s |
|---|---|---|---|---|
| Q29, `REGEXP_REPLACE` on Referer | 6.478 s | 1.371 s | 4.7x | 25% alone |
| Q23, `Title LIKE '%Google%'` | | | 18x | |
| Q22, `URL LIKE '%google%'` | | | 14x | |

Q29 is mostly a question of evaluating once per distinct value, and a matcher specialized to that one regex shape. rudb's gram sieve cut Q21 instructions by 17.58%, with FSST decompression still about 20% of the query (`rudb-bench/reports/2026-09-23`).

**Layout** is shared across the leaders. Umbra, DuckDB, Velox and Arrow all use a 16-byte German string: 4-byte length, 4-byte prefix, inline up to 12 bytes (https://engineering.fb.com/2024/02/20/developer-tools/velox-apache-arrow-15-composable-data-management/).

**What the compiled engine takes.**

- LIKE and regex patterns are compile-time constants, and their shape is baked into the code: segment split, per-segment SIMD, Boyer-Moore or Two-Way search, and prefix checks on the inline word. The pattern value is not a parameter unless the shape stays the same.
- OR-ed patterns on one column share a pass.
- Pattern lists and wildcard joins use Aho-Corasick.
- Predicates run on dictionary codes or FSST codes when the storage facts say so, and decompression is late.
- String layout is the 16-byte German string, with storage classes promoted at pipeline breakers.
- The 2608.23307 numbers are a target to reproduce in rudb-bench, not a result we claim.

## 1.8 Aggregation and sort

**Aggregation and sort follow the same split as scans: generate the per-row update or key encoding, and keep the algorithms precompiled and chosen at runtime.**

**Aggregation strategy.**

- **Ticketing global table.** "Global Hash Tables Strike Back!" (https://arxiv.org/abs/2505.04153) hands each group a dense ticket from a concurrent linear-probing table and keeps aggregate state in arrays. It measured 1.78x over partitioned aggregation at low cardinality on 48 threads. It assumes perfect cardinality estimates, which is why the choice must be made at runtime.
- **Groupjoin** fuses a join and the following aggregation on the same key. It occurs in about one in eight TPC-H and TPC-DS queries (https://vldb.org/pvldb/vol14/p2383-fent.pdf).
- **Bespoke OLAP** used fused inline aggregation in 97.4% of queries and dense-key array aggregation in 55%. That is the pattern a compiler reaches when it knows a key's range.
- **ClickBench** Q33-Q35, Q19 and Q17 (high-cardinality GROUP BY on strings and wide keys) are 55% of DuckDB's time together with Q29. Key packing and the probe-insert loop are compiler work. The parallel final merge and memory are not.

**Sort.**

- DuckDB's ICDE 2023 sort (https://duckdb.org/pdf/ICDE2023-kuiper-muehleisen-sorting.pdf) normalizes all keys into one memcmp-comparable string. It found static-size compares 25% faster than dynamic ones below 16 bytes.
- DuckDB 1.4 (https://duckdb.org/2025/09/24/sorting-again) templates the key width at compile time and runs vergesort, then ska sort, then pdqsort, with a k-way merge path. It measured 2.7x on random integers, 10.4x on sorted ones and 3.4x on TPC-H SF100. The SF100 lineitem sort fell from 273.98 s to 80.92 s.

**What the compiled engine takes.**

- Generated code covers the aggregate update body (all aggregates fused, NULL checks specialized away), the key packing and hashing (two CRC32 chains, then rotate, XOR and multiply), and the sort-key normalization into a fixed-width struct.
- Precompiled runtime code covers partitioned or ticketed tables, radix, pdq and merge-path sort monomorphized by key width, and spilling.
- The aggregation strategy is chosen from observed group counts.
- Dense-key arrays and groupjoin are chosen when the data facts allow them.
- DuckDB's sort is already specialized by key width. Our win there comes from fusing the encode into the pipeline, not from the sort algorithm.

## 1.9 OLTP compilation

**Compiled OLTP wins by an order of magnitude only when the code is compiled once and reused, and only on top of a storage engine that updates in place.**

- **Hekaton** (https://15721.courses.cs.cmu.edu/spring2016/papers/freedman-ieee2014.pdf) compiles T-SQL procedures to C at `CREATE PROCEDURE` time. It states the rule we have borrowed: "To go 10X faster, the engine must execute 90% fewer instructions." Compiled lookups took 6-19% of the interpreted instructions. Native procedures ran TPC-C-like work at 36,375 tps against 2,312 interpreted, 15.7x.
- **HyPer** ran TPC-C at 169,491 tps with 0.81 s of total LLVM compile (https://www.vldb.org/pvldb/vol4/p539-neumann.pdf). The ICDE 2011 configuration recorded 126,576 tps on one OLTP thread.
- **Umbra's MVCC paper** (https://www.vldb.org/pvldb/vol15/p2797-freitag.pdf) measured 27,000 TX/s on one thread and 413,300 at 48 threads, against PostgreSQL's 2,600 and 44,700. Its ablation shows removing in-place updates costs 5.13x. That is the storage decision, and no amount of codegen recovers it.
- **Neumann's "Evolution" paper** (https://vldb.org/pvldb/vol14/p3207-neumann.pdf) compiles provably small OLTP plans into a single function, skipping the pipeline state machine.

**DuckDB is not a competitor here.** It publishes no TPC-C, says small concurrent transactions are not a goal, and reportedly rebinds prepared statements on `EXECUTE` (https://github.com/duckdb/duckdb/issues/17237) `[snippet]`.

**Caching works if the compiler is slow and the workload repeats.** Redshift raised cache hits from 99.60% to 99.95% (http://muratbuffalo.blogspot.com/2022/09/amazon-redshift-re-invented.html). Impala measured a codegen cache saving 22% on queries under 2 s `[snippet]`. With a microsecond baseline compiler, the cache matters mostly for the optimizing tier.

**What the compiled engine takes.**

- Prepared statements compile once. Values come from query state. Types, nullability, collation and pattern shape are baked in.
- The cache is keyed by QIR hash, parameter types, schema version, CPU features and compiler version, and invalidated on DDL.
- Provably single-row plans compile to one function with an inline index probe and no morsel machinery. The target is a few hundred instructions per lookup.
- One-shot statements run on `interp` or `direct`, and repeated ones get `clif` after a repeat threshold.
- Document 14 says plainly that TPC-C numbers depend on the storage engine's update path as much as on the compiler.

## 1.10 The specialization ceiling

**Bespoke OLAP and GenDB show what code specialized to one workload's data can do, and the 2x2 ablation says the multiplier is delivered by code that knows the data, which is what a compiler produces.**

**Bespoke OLAP** (arXiv 2603.02001, PVLDB 19(11)) had an LLM agent synthesize a whole C++ engine for a fixed workload, taking minutes to hours at $10-250. On an EPYC 9654P:

| Workload | Bespoke | DuckDB 1.4.1 | Bespoke / DuckDB | Bespoke / Umbra 26.02 |
|---|---|---|---|---|
| TPC-H SF20, 1 thread | 4.4 s | 49.2 s | 11.17x | 7.24x |
| CEB SF2, 1 thread | 0.4 s | 19.5 s | 45.33x | 9.56x |
| TPC-H, multi-thread | | | 7.65x | 6.12x |
| CEB, multi-thread | | | 23.97x | 1.87x |

The techniques the agent reached for were inline fused aggregation (97.4% of queries), bitmap semi-joins (73.7%) and dictionary predicate rewrites (71%). The final TPC-H engine was about 11,600 lines. Ad hoc queries went to a fallback DBMS.

Table 1 is a 2x2 of storage (flat or bespoke) against code (basic or optimized), as speedup over DuckDB:

| Storage | TPC-H: basic → optimized | CEB: basic → optimized |
|---|---|---|
| Flat | 1.26x → 5.18x | 0.57x → 8.09x |
| Bespoke | 2.34x → 12.35x | 2.10x → 51.40x |

`../planner-v2/10-specialization.md` and `../08-codegen.md` read the flat-basic cell (1.26x, and 0.57x on CEB) as the value of code specialization, and ranked compilation third. The table supports a different reading.

- **Holding storage fixed, going from basic to optimized code** multiplies by 4.1x (flat TPC-H), 14.2x (flat CEB), 5.3x (bespoke TPC-H) and 24.5x (bespoke CEB) `[derived]`.
- **Holding code fixed at basic, going from flat to bespoke storage** multiplies by 1.9x (TPC-H) and 3.7x (CEB) `[derived]`.
- The two compound. Neither alone gets near the corner.

Our notes record the table, not the paper's precise definition of "basic" and "optimized" `[gap]`. The optimized step clearly includes algorithm choices (fused aggregation, bitmap semi-joins, dictionary rewrites), not only instruction selection. That is exactly the point.

Each of those choices is a loop specialized to a data fact: this key is dense, this column has 211 dictionary entries, this child is clustered by its parent. A vectorized engine reaches it by writing one kernel per (operation × encoding × type × nullability) point. A compiler reaches it by emitting the one loop the query needs. Doc 00's argument stands: specializing the layout is the storage layer's job, and specializing code to the layout is the compiler's job.

**GenDB** (Lao and Trummer, arXiv 2603.02081; demo arXiv 2607.20630) generates one executable per query with agents built on Claude Code.

- On five TPC-H SF10 queries at 64 threads it ran 214 ms total, against 594 ms for DuckDB and 590 ms for Umbra (2.8x).
- On SEC-EDGAR it was 5.0x DuckDB.
- Synthesis took 91-140 minutes and cost $14-23.
- The `gendb` entry on ClickBench c6a.4xlarge sits at a hot sum of 86.29 s, against DuckDB's 26.25 s `[computed]`. Offline per-query synthesis does not transfer to an unseen workload.

**What the compiled engine takes.**

- Bespoke OLAP is the ceiling for a known workload: about 6-10x over Umbra single-threaded. It is reached through physical and algorithmic specialization expressed as code.
- Our compiler's specialization inputs are storage facts known when the query starts (encoding, dictionary size, value range, nullability, sort order, clustering, stored links). They are not the history of previous queries, and they are guarded so a changed fact falls back rather than returning wrong answers.
- We do not overfit. "Survivorship Bias" (CIDR 2026 best paper, https://www.vldb.org/cidrdb/papers/2026/p22-marcus.pdf) and TPC's rules against benchmark-special code apply to every technique in section 1.6 through 1.8.

## 1.11 What we refuse, and why

**Learned policies on the query path.** CAKE's regret trees decide in under 20 ns, and we allow that class of cheap, bounded, per-morsel choice among precompiled variants. We refuse learned cardinality models, learned plan choice and learned tier policies that need model inference on the path. Kohn's extrapolation is a closed-form formula, it is proven, and when it is wrong it can be debugged.

**MLIR.** LingoDB shows MLIR makes a compiler compact. It also shows the cost: 13 ms of MLIR plus 68 ms of LLVM on TPC-H Q2, with the fast path depending on TPDE, a C++ dependency that runs on Linux only. Nautilus's MLIR backend takes tens of milliseconds. MLIR would put a large C++ toolchain under a Rust engine whose latency budget is 1 ms. The progressive-lowering idea is one we keep, in QIR's own passes.

**Source-to-source compilation** (C, C++ or Rust source through an external compiler). HyPer measured it at 1.6-2.6 s per query in 2011. GCC via C took 48.88 s for CGO 2024's workload. Velox's expression codegen reached 10 s. Hekaton could afford it only because it compiles at `CREATE PROCEDURE` time. We keep a Rust-source debug printer for sanitizer runs, as Umbra keeps a C backend, and never run it on the query path.

**LLM-in-path.** Bespoke OLAP and GenDB take minutes to hours and dollars per workload, and handle unseen queries by falling back. They are offline tools. On an ad hoc query the latency is off by six orders of magnitude.

**GPU.** Sirius reaches a ClickBench hot sum of 1.45 s on a GH200 `[computed]`. That is a different machine class from the c6a.4xlarge and M4 we are measured on, and it gives no JOB evidence. Nothing in QIR prevents a later GPU backend, and nothing in this folder builds one.

**TPDE via FFI.** It is the best single-pass design published. It is also C++ with a template IR adapter, emits ELF only, cannot run in-process on the M4, and does not support vector operations at -O2. We build its design in Rust (`rudb-qc-direct`) and use TPDE as a benchmark reference, not a dependency.

**Also refused, for the reasons given in the sections above:**

- Copy-and-patch as the default tier: code 2.32x slower than TPDE, and the stencil count explodes.
- Wasm as an intermediate: indirection with no benefit for trusted code, and 128-bit SIMD only.
- libgccjit: GPL.
- Worst-case-optimal joins as a JOB default: 25x slower in Umbra.
- Unwinding through generated frames.

## What we should take from this document

The literature agrees on the shape of a fast query compiler more than it agrees on anything else. It has an owned, compact IR with database operations as instructions, a generator that runs in one linear pass, and a single-pass backend at about 10 µs per function. An optimizing backend comes in only after measured progress says a pipeline is long. Pipeline state lives in memory, so tiers switch between morsels without on-stack replacement. Every system that skipped the fast tier lost short queries, and JOB is 113 short queries over large plans.

What the literature does not supply is a single-pass backend in Rust for both AArch64 and x86-64. That is the novel engineering in this folder. TPDE sizes it at about 1.4k target-specific lines per architecture. Winch shows the second ISA took 1.5 years when it was added late. We write both from the start, and use Cranelift to have a working engine while `direct` is built.

The compiled-versus-vectorized question is answered by division of labor. Scans, decoders and complex algorithms stay precompiled and vectorized. The pipeline body is fused and compiled. Probes that miss the cache are staged and prefetched. Photon's lesson about tooling is taken as a requirement, not a reason to avoid compiling.

On JOB, the gains available are roughly 1.5x from semi-join reduction (which DuckDB is absorbing), about 2x from hash-table design, 2.5-13x on string predicates where they dominate, and whatever late materialization and MIN-pushdown yield (unmeasured). None of these is 10x alone, and no paper reports their product. Our JOB target is an assembly of parts, and document 02 budgets it that way.

The ceiling is Bespoke OLAP's 6-10x over Umbra, and its ablation says the multiplier is code that knows the data. The compiler is how rudb captures that without an agent per workload: by specializing to storage facts known when the query starts, behind guards, in microseconds.
