# A. Query-compilation systems survey (for rudb compiling engine #2)

Research date: 2026-09-24. Scope: HyPer to Umbra/CedarDB, the staging line, research engines, industry, 2024-2026 work, and the compiled-vs-vectorized debate.

Conventions:
- Numbers come from the primary PDF or page unless marked **[snippet]**. That mark means the number was seen only in a search-engine or secondary summary and still needs checking against the source.
- **[gap]** means I looked and could not get the data.
- "CT" = compile time. "ET" = execution time. "SF" = TPC scale factor. "geo" = geometric mean. "1T" = single-threaded.

## 0. Cross-system quick table

| System | Compiled unit | IR levels | Backend(s) | Compile latency (reported) | Short-query answer |
|---|---|---|---|---|---|
| HyPer 2011 | pipelines (data-centric) | C++ plan → LLVM IR + precompiled C++ "cogwheels" | LLVM | 16-41 ms (TPC-CH Q1-5) | none in 2011; bytecode interpreter in 2018 |
| HyPer adaptive (ICDE'18) | per-pipeline functions | LLVM IR → own register bytecode | bytecode VM / LLVM -O0 / LLVM opt | IR gen <2 ms; bytecode 0.4-1.2 ms; unopt 6-23 ms; opt 42-149 ms | per-pipeline switch, driven by morsel progress |
| Umbra | pipeline *steps* (state machine) | Umbra IR (LLVM-like subset) | DirectEmit (default) / LLVM / (TPDE) / C / Cranelift / interpreter | TPC-DS SF1 total: DirectEmit 0.11 s vs LLVM-opt 16.2 s | Flying Start/DirectEmit first, then LLVM for long pipelines |
| CedarDB | same as Umbra | custom IR | ASM (DirectEmit) / cheap LLVM / opt LLVM / interpreter | ASM gives ~90% of throughput at ~1% of CT | `compilationmode=a` (adaptive) by default |
| Tableau Hyper | HyPer lineage | LLVM IR | bytecode / unopt / opt LLVM | "multiple seconds" on some Tableau queries [snippet] | adaptive (ICDE'18) |
| LB2 / LegoBase | whole query | Scala staged (LMS) → C | GCC -O3 | codegen 59-736 ms + GCC 175-664 ms | none |
| LingoDB | whole query | MLIR: relalg → subop → db/dsa/util → scf/arith/LLVM | LLVM (SPEED), C, BASELINE (TPDE), GPU | Q2: MLIR 13 ms + LLVM 68 ms (2022) | BASELINE mode added (TPDE) |
| InkFuse | sub-operators | sub-op IR → C | clang-14 (JIT) + pre-generated vectorized primitives | JIT >40 ms at SF0.1 | hybrid: interpreter first, JIT in background |
| mutable | whole query | WebAssembly | V8 Liftoff → TurboFan | TurboFan up to 6.6x faster than HyPer's opt LLVM | V8 tiering "for free" |
| Nautilus (NebulaStream) | traced C++ operators | trace → Nautilus IR | MLIR / C++ / bytecode / asmjit / copy-and-patch | [gap] | multi-backend |
| Spark WSCG | whole stage | Java source | Janino → JVM bytecode → HotSpot JIT | [gap]; 64KB / 8000-byte limits | falls back to Volcano |
| Photon | nothing (vectorized C++) | n/a | n/a | 0 | n/a |
| Impala | inner-loop functions | Clang-precompiled IR + LLVM | LLVM | 100-250 ms common [snippet] | codegen cache (2024) |
| SingleStore | whole plan | MPL → MBC bytecode → LLVM | interpreter + LLVM | "noticeable" first run | interpret until compile is done; on-disk plan cache |
| Redshift | query segments | C++ source | GCC on a separate compile fleet | ~half of end-to-end latency (Redset) | 2-level cache, 99.95% hits |
| Hekaton | stored procedures | MAT → PIT → C | MSVC | [gap] | compile once, execute many |
| PostgreSQL | expressions + tuple deforming | LLVM IR (+ bitcode inlining) | LLVM | cost-threshold driven | JIT off by default in PG 19 |
| ClickHouse | expressions, aggregates, sort comparators | LLVM IR | LLVM | 5-15 ms per function [snippet] | compile after 3 executions; 1 GB cache |
| Velox | expressions (experimental) | C++ source | gcc/clang → .so | "up to 10s" | not for short queries |
| DataFusion | row conversion / comparisons (removed) | Cranelift IR | Cranelift | [gap] | crate abandoned (last 23.0.0, 2023) |
| SAP HANA HEX | pipelines | "L" language → LLVM | L interpreter + LLVM (async) | [gap] | interpret first, compile if hot |
| Bespoke OLAP | whole engine per workload | C++ (LLM-written) | clang/gcc | minutes to hours of synthesis | fallback DBMS for ad hoc queries |
| GenDB | one executable per query | C++ (LLM-written) | compiler | 91-140 min of synthesis | n/a |

## 1. TUM line

### 1.1 HyPer: Neumann, "Efficiently Compiling Efficient Query Plans for Modern Hardware", VLDB 2011
URL: https://www.vldb.org/pvldb/vol4/p539-neumann.pdf

**What is compiled**
- Whole pipelines in the data-centric *produce/consume* model: tuples stay in registers between pipeline breakers.
- Each pipeline becomes one tight loop.

**IR and backend**
- A C++ code generator emits LLVM IR.
- Complex logic (hash-table resizing, spilling, etc.) stays in precompiled C++ "cogwheels" that the generated LLVM code calls. The hot path must be pure LLVM.
- An earlier backend emitted C++ and compiled it with gcc. That backend was abandoned.

**Compile latency**
- TPC-C: C++ backend 161,794 tps with 16.53 s total compile; LLVM backend 169,491 tps with 0.81 s total compile.
- TPC-CH Q1-Q5 per query:
  - C++ compile: 1556 / 2367 / 1976 / 2214 / 2592 ms.
  - LLVM compile: 16 / 41 / 30 / 16 / 34 ms.

**Execution (TPC-CH Q1-Q5, ms)**

| Engine | Q1 | Q2 | Q3 | Q4 | Q5 |
|---|---|---|---|---|---|
| HyPer+LLVM | 35 | 125 | 80 | 117 | 1105 |
| HyPer+C++ | 142 | 374 | 141 | 203 | 1416 |
| VectorWise | 98 | n/a | 257 | 436 | 1107 |
| MonetDB | 72 | 218 | 112 | 8168 | 12028 |

- Branches on Q1: LLVM 19.8M vs MonetDB 144.6M.

**Lessons**
- Generating LLVM IR directly beat generating C++ by roughly 100x in CT, and the code also ran faster.
- The "cogwheel" split (generated hot loop plus precompiled runtime) is the template every later system reuses.

### 1.2 HyPer adaptive execution: Kohn, Leis, Neumann, ICDE 2018
URL: https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf

**Problem**
- A catalog query executes in under 1 ms but takes 54 ms to compile with LLVM (50x the execution time; 98% of total time wasted).
- TPC-H Q1 compiles in 59 ms. The largest TPC-H query takes 146 ms and the largest TPC-DS query 911 ms (about 1 s).

**Q1 pipeline breakdown**
- plan 0.2 ms
- codegen (LLVM IR) 0.7 ms
- bytecode translation 0.4 ms
- unoptimized LLVM 6 ms
- optimized LLVM: 25 ms of passes + 17 ms of machine-code generation

**Table I (TPC-H Q1-5 and the largest query)**
- LLVM IR generation always under 2 ms.
- Bytecode 0.4-1.2 ms.
- Unoptimized 6-23 ms.
- Optimized 42-149 ms.

**Table II (execution geo, TPC-H SF1, ms)**

| Mode | 1 thread | 8 threads |
|---|---|---|
| bytecode | 232 | 45 |
| unoptimized | 60 | 15 |
| optimized | 46 | 12 |
| PostgreSQL | 497 | n/a |
| MonetDB | 57 | n/a |

- The bytecode VM is 3.6x slower than unoptimized and 5.0x slower than optimized code, but 2.1x faster than PostgreSQL.
- LLVM's own IR interpreter is over 800x slower, so it is unusable.

**Mechanism**
- The unit of decision is the pipeline function.
- Morsel-driven workers report progress. The system extrapolates remaining time from each thread's rate and compares three options: stay in bytecode, compile unoptimized, or compile optimized.
- Compilation runs on one thread while the other threads keep executing in bytecode.
- Bytecode design:
  - register-based, translated from LLVM IR
  - linear-time liveness analysis and register allocation
  - macro-ops fuse common instruction pairs (overflow-checked arithmetic, GEP+load)

**Results**
- TPC-H Q11: the adaptive run was 10%, 40% and 80% faster than the fixed alternatives. Optimized compilation alone took 103 ms.
- For very large queries, optimized LLVM is "not viable".
- The paper cites MemSQL's MPL → MBC → LLVM path as related work.

**Lessons**
- Adaptivity must work at the pipeline level and inside a running query, not only per query.
- The interpreter tier must be a purpose-built bytecode, not LLVM's interpreter.

### 1.3 Umbra: Neumann & Freitag, CIDR 2020
URL: https://www.cidrdb.org/cidr2020/papers/p29-neumann-cidr20.pdf

**Design**
- Pipelines are split into *steps*. Each step is one function, and a step boundary is a point where the pipeline can suspend and resume.
- Execution is a state machine. This allows suspension, cooperative scheduling and morsel restarts.

**IR and backends**
- A custom "Umbra IR" that looks like a subset of LLVM IR, tuned for fast construction: compact, with no per-instruction malloc.
- Adaptive: start fast (bytecode interpreter at first, later replaced by Flying Start/DirectEmit), then move to LLVM for expensive pipelines.

**Speed**
- Geo speedup over HyPer: 3.0x on JOB and 1.8x on TPC-H SF10.
- On cheap queries, HyPer spent up to 29x more time compiling than executing.
- Geo over MonetDB: 4.6x on JOB and 2.3x on TPC-H.

### 1.4 Tidy Tuples & Flying Start: Kersten, Leis, Neumann, VLDB Journal 2021
URL: https://zenodo.org/records/5770190 (DOI 10.1007/s00778-020-00643-4)

**Tidy Tuples: code-generation architecture in 5 layers**
1. Operator translators (produce/consume)
2. Data-structure generators (hash tables and similar)
3. Tuples
4. SQL values (null, overflow and collation semantics)
5. Codegen API ("typed builder" over Umbra IR)

**Code-generation speed**
- LB2 codegen takes 299 ms geo. Tidy Tuples is more than 1000x faster.
- End-to-end "preparation" time (parse + plan + codegen), average: Umbra 0.66 ms, DuckDB 0.47, MonetDB 0.53, HyPer 1.33.
- So a compiling engine can have front-end overhead on par with DuckDB.

**Flying Start: single-pass x86 backend from Umbra IR (Table 3)**

| Backend | Compile speed vs LLVM O3 | Execution vs LLVM O3 |
|---|---|---|
| Flying Start | 108x faster | 1.2x slower |
| HyPer bytecode interpreter | 91x faster | 4.1x slower |
| LLVM O0 | 6x faster | 1.3x slower |

- Stress test: a 2000-join query (108,000 IR instructions). LLVM 150 s; LLVM with fast instruction selection 4 s; Flying Start under 0.04 s.
- The four optimizations Flying Start keeps: register allocation (execution time -32%), address-calculation folding, comparison-branch fusion, and one more.
- Linear-scan register allocation would have given +1% speed for 14% more CT, so it was not adopted.

**Lesson**
- Once the IR is good, a dumb-but-careful single-pass backend is within 1.2x of LLVM O3. That makes a separate interpreter tier optional.

### 1.5 TPDE: Schwarz, Kamm, Engelke, arXiv 2505.22610 (CGO 2026)
URL: https://arxiv.org/pdf/2505.22610

**What it is**
- A framework for writing single-pass compiler backends.
- TPDE-LLVM (consumes LLVM IR) compiles 8-24x faster than LLVM -O0 on SPEC, with similar code quality.

**Umbra, TPC-DS SF1, all queries accumulated**

| Target | Backend | Compile (s) | Run (s) |
|---|---|---|---|
| x86 | LLVM-Opt | 16.193 | 0.615 |
| x86 | LLVM-O0 | 2.504 | 0.650 |
| x86 | TPDE-LLVM | 0.29 | 0.651 |
| x86 | TPDE (Umbra IR) | 0.087 | 0.652 |
| x86 | DirectEmit | 0.11 | 0.644 |
| AArch64 | LLVM-Opt | 7.341 | 0.936 |
| AArch64 | TPDE | 0.067 | 1.055 |
| AArch64 | DirectEmit | 0.069 | 1.024 |

**Engineering cost**
- DirectEmit (Umbra's default backend, two-pass): ~11k lines of code, platform dependent.
- The TPDE Umbra backend is 3.6k lines, of which 1.6k are target-specific.
- LingoDB adopted TPDE as its BASELINE mode (see 3.2).

**Lesson**
- Single-pass codegen is now a reusable library pattern, not a heroic one-off.
- For TPC-DS, LLVM-Opt gains 5% execution for 186x the CT on x86.

### 1.6 Engelke & Schwarz, "Compile-Time Analysis of Compiler Frameworks for Query Compilation", CGO 2024
URL: https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf

**Setup**
- Umbra with every backend.
- Totals are for compiling all TPC-DS queries (6678 functions).

**Table III, TPC-DS SF10 (x86 machine 32 cores; AArch64 machine 4 cores)**

| Backend | x86 compile | x86 exec | AArch64 compile | AArch64 exec |
|---|---|---|---|---|
| Interpreter | 0.03 s | 15.40 s | 0.02 s | 64.55 s |
| DirectEmit | 0.06 s | 4.83 s | n/a | n/a |
| Cranelift | 1.07 s | 4.62 s | 0.61 s | 16.37 s |
| LLVM-cheap | 1.63 s | 5.23 s | 0.74 s | 19.45 s |
| LLVM-opt | 11.36 s | 4.12 s | 5.86 s | 12.88 s |
| GCC | 48.88 s | 4.28 s | 41.64 s | 13.99 s |

**Findings**
- Cranelift ≈ unoptimized LLVM in execution speed, but compiles only 20-35% faster. The single-pass compiler compiles 16x faster than Cranelift at similar execution speed.
- GCC total 46.34 s, of which parsing 6.45 s. In Umbra the GCC backend is "in practice only used for debugging".
- LLVM-opt matters for some queries: on TPC-DS Q17 its code is 38% faster than DirectEmit's (0.93 s vs 1.29 s).
- LLVM tuning tips from the paper:
  - use FastISel for cheap mode
  - GlobalISel is 1.4x faster for optimized compiles
  - avoid i128 in IR
  - use ORC JIT carefully

**Direct relevance to rudb (Rust)**
- Cranelift is the natural Rust JIT, but it is *not* a fast tier; it sits in the LLVM-cheap class.
- A fast tier needs a single-pass emitter (own or TPDE-like), or a copy-and-patch approach.

### 1.7 Neumann & Leis, HYTRADBOI 2025 talk "Query compilation" (slides)
URL: https://www.hytradboi.com/2025/slides/leis-neumann-compilation.pdf

**Timeline**
- HyPer LLVM 2011-2017
- bytecode interpreter 2018-2020
- direct machine code since 2020 (x86-64 and ARM64)

**Points**
- Machine-generated queries can reach 10 MB of SQL, so CT must scale linearly.
- Redshift still compiles to C++ with a multi-tenant code cache and 99%+ hit rates, yet "about half the end-to-end latency appears to be in compilation" (citing the Redset dataset).
- Advice: invest in tooling (debuggers, profilers mapping back to operators).

### 1.8 CedarDB (commercial Umbra)
- Blog "Compilation" (2025-04-02): https://cedardb.com/blog/compilation/
  - Compares C, LLVM, own assembler ("ASM") and an interpreter.
  - ASM gets "about 90%" of the throughput at "about 1%" of the compile time.
  - Custom IR "similar to LLVM IR but tailored".
  - Toy example: interpreter 10 GB/s vs generated code 65 GB/s; JIT about 1 ms of a 64 ms total.
  - Adaptive: start with ASM, switch hot pipelines to LLVM.
- Config docs: https://cedardb.com/docs/references/configuration/
  - `compilationmode` = `a` (adaptive, default) | `i` (interpreted) | `d` (DirectEmit) | `c` (cheap LLVM) | `o` (optimized LLVM).
  - Morsel-driven parallelism.
- Other CedarDB blog posts:
  - "Amplify": MemSQL's C++ compilation took up to 10 s on a cold start. **[snippet]**
  - "Ode to Postgres": DuckDB executes ~5x more branches. **[snippet]**
- Published compile latencies or ClickBench deltas specific to CedarDB: **[gap]**.

### 1.9 Tableau Hyper
- Product lineage: https://tableau.github.io/hyper-db/journey/
  - Shipped in Tableau 10.5 (January 2018) after 18 months of integration.
  - Uses the ICDE'18 adaptive execution (bytecode / unopt / opt LLVM).
  - LLVM IR generation is under 2 ms for all 22 TPC-H queries. **[snippet via search summary]**
- Some Tableau queries "still take multiple seconds just in compilation step". **[snippet]** (attributed to researchers; primary source not located)
- LingoDB 2022 reports Tableau Hyper is 1.3x faster than LingoDB on average (1T, SF1).
- LingoDB 2023 reports LingoDB is 10% faster than Hyper at SF10.

## 2. Staging / generative-programming line

### 2.1 LegoBase (Klonatos, Koch, Rompf, Chafi, VLDB 2014) and DBLab
URL: https://arxiv.org/abs/1612.05566 (TODS version)
- A Scala query engine specialized by source-to-source compilation into C.
- The whole engine, including data structures, is specialized per query.
- Numbers not re-extracted: **[gap]**.
- LingoDB 2022 reports that DBLab compiled TPC-H Q2 in more than 900 ms of generation plus 300 ms of Clang.

### 2.2 LB2: Tahboub, Essertel, Rompf, "How to Architect a Query Compiler, Revisited", SIGMOD 2018
URL: https://www.cs.purdue.edu/homes/rompf/papers/tahboub-sigmod18.pdf

**Idea**
- First Futamura projection: write an ordinary *interpreter* in Scala and stage it with LMS (Lightweight Modular Staging). Specializing the interpreter to a plan yields a compiler.
- It emits C, compiled with GCC -O3.
- One code base serves as both interpreter and compiler. Parallelism and indexes are added as staged library code.

**Compile latency**
- LMS codegen ~59-736 ms per query, plus GCC ~175-664 ms.
- Tidy Tuples later measured LB2 codegen at 299 ms geo.

**Speed vs HyPer (SF10)**
- LB2 is 2-3x faster on Q3, Q11, Q16, Q18.
- HyPer is 2-3x faster on Q2 and Q17 (GroupJoin, indexes).
- Overall competitive.

**Lesson**
- "Interpreter + staging = compiler" is elegant, but host-language staging plus a C compiler gives ~0.2-1.4 s CT. It is unusable for interactive work without tiering.

### 2.3 Flare (Essertel et al., OSDI 2018)
URL: https://www.usenix.org/conference/osdi18/presentation/essertel ; https://arxiv.org/abs/1703.08219
- Compiles whole Spark SQL plans (Catalyst output) to native code through LMS, bypassing the JVM runtime. Also compiles UDFs.
- Speedups:
  - "10x to 100x speedups on standard analytical benchmarks such as TPC-H" **[snippet: DOE report]**
  - "order of magnitude" (arXiv abstract) **[snippet]**
- Scale-up, single machine, medium-size data.

## 3. Research engines

### 3.1 Peloton relaxed operator fusion (ROF): Menon, Mowry, Pavlo, VLDB 2017; NoisePage
URL: https://www.vldb.org/pvldb/vol11/p1-menon.pdf

**Design**
- Stages inside a pipeline are separated by small *stage vectors* (buffers of tuple ids).
- Fusion stays within a stage; buffering sits between stages, where it enables SIMD and software (group) prefetching for hash probes.
- LLVM backend.

**Speed**
- Up to 2.2x faster than pure data-centric compilation.
- Up to 1.8x vs HyPer/Actian Vector.
- Tuple-at-a-time processing with prefetching gains up to 1.2x.

**NoisePage**
- Moved to a bytecode VM plus LLVM with adaptive switching (the TPL language).
- Project is dormant.
- Published numbers: **[gap]**.

**Lesson**
- The pure fused loop loses on cache-miss-bound probes.
- A compiling engine should be able to insert a buffering or prefetch boundary inside a pipeline. The Kersten 2018 analysis confirms this.

### 3.2 LingoDB (Jungmair, Giceva et al., TUM)

**VLDB 2022 "Designing an Open Framework for Query Optimization and Compilation"**
URL: https://vldb.org/pvldb/vol15/p2389-jungmair.pdf
- MLIR dialects:
  - `relalg`: relational algebra; the optimizer works here
  - `db`: SQL types, nulls
  - `dsa`: data structures
  - `util`
  - lowering continues to standard MLIR (scf/arith) and then LLVM
- Query optimization (join ordering and more) is done as MLIR passes.
- Speed (1T, SF1): 3.5x faster than DuckDB. Tableau Hyper is on average 1.3x faster than LingoDB.
- Compile: TPC-H Q2 takes 13 ms of MLIR lowering plus 68 ms of LLVM. DBLab takes more than 900 ms plus 300 ms of Clang.
- Less code than DuckDB or NoisePage.

**VLDB 2023 "Declarative Sub-Operators for Universal Data Processing"**
URL: https://www.vldb.org/pvldb/vol16/p3461-jungmair.pdf
- A sub-operator layer (`subop`) sits between relational algebra and imperative code.
- Aggregation takes 384 lines vs 1358 in DuckDB.
- Auto-parallelization pass: 347 lines. It "fully parallelize[s] every TPC-H and TPC-DS query". Scaling is roughly linear until memory bandwidth or SMT becomes the limit.
- SF10: 4.8x faster than DuckDB on TPC-H and 3.9x on TPC-DS; 10% faster than Hyper; Umbra is faster than LingoDB.
- Pass effects on TPC-DS:
  - GlobalSharing: up to 2x; also consistently reduces CT
  - DeferLoading: up to 3x
  - entry compression: up to 25%
- Compilation latency rose slightly vs the 2022 version.

**LingoDB-CT (SIGMOD 2025 demo)**
- Lightweight tracing and MLIR snapshots after each pass link runtime events back to IR, then to operators, then to SQL.
- Answers the "compiled engines are hard to debug and profile" critique that Photon made.

**Jungmair & Giceva, "Towards Designing Future-Proof Data Processing Systems", PVLDB 18(11) 2025**
- Vision paper: declarative sub-operators plus layered compilation on a modular IR (MLIR).
- WebAssembly is suggested for isolating user code.
- No performance numbers.

**Repository state (checked 2026-05-15 commit, `include/lingodb/execution/Execution.h`)**
- `ExecutionMode` = SPEED, DEFAULT, PERF, DEBUGGING, CHEAP, C, GPU, NONE, BASELINE ("baseline compilation mode, similar to LLVM -O0, uses TPDE"), BASELINE_SPEED.
- There is a C backend (`CBackend.cpp`). The baseline backend is Linux-only.
- The TUM blog puts LingoDB's JIT time at ~90 ms. **[snippet]**

**Lesson**
- MLIR buys composability and a small codebase, but the default LLVM path costs 50-100 ms per query.
- LingoDB also had to add a TPDE-based baseline tier. MLIR is a good *mid-level* IR, not a latency solution.

### 3.3 InkFuse / Incremental Fusion: Wagner, Kohn, Boncz, Leis, ICDE 2024
URL: https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/inkfuse.pdf

**Design**
- One sub-operator IR serves both backends. The *vectorized interpreter's primitives are generated ahead of time from the same IR*, and the vectorized backend itself is under 50 lines of C++.
- JIT backend: generates C and compiles it with clang-14 on one background thread per pipeline.
- Hybrid mode:
  - 5% of morsels go to the interpreter and 5% to JIT code
  - the remaining 90% go to whichever backend is faster
  - throughput is tracked as an exponentially decaying average

**Results**
- SF0.1: every query runs in under 20 ms without ever using the JIT. Compiled InkFuse needs more than 40 ms and Umbra's LLVM backend about 20 ms. The interpreter beats compilation by up to 10x.
- SF10: Q1, Q4, Q14 and Q19 beat DuckDB 0.9.1.
- SF100: the winning mode depends on the query.
- Cites SAP HEX and Redshift as industrial hybrids.

**Lesson**
- Derive the interpreter from the compiler IR so the two can never diverge semantically.
- This is the cheapest way to get a correct fast-start tier plus a JIT tier.

### 3.4 mutable (Haffner & Dittrich, Saarland)
- "Fast Compilation and Execution of SQL Queries with WebAssembly", arXiv 2104.15098: https://arxiv.org/abs/2104.15098
- "A Simplified Architecture for Fast, Adaptive Compilation and Execution of SQL Queries", EDBT 2023: https://openproceedings.org/2023/conf/edbt/paper-156.pdf
- CIDR 2023 system paper: https://www.cidrdb.org/cidr2023/papers/p41-haffner.pdf

**Design**
- Emits WebAssembly and hands it to V8, so tiering comes for free: Liftoff (baseline, like Flying Start), then TurboFan (optimizing).

**Compile latency (EDBT 2023, TPC-H SF1)**
- TurboFan compilation is up to 6.6x faster than HyPer's optimizing LLVM (Q1).
- Liftoff is up to 7.4x faster than HyPer's non-optimizing compilation (Q12).
- mutable's CT includes generating hash tables and other library code.
- arXiv abstract: "compile even complex queries in less than a millisecond". **[snippet]**

**Execution**
- Competitive with HyPer except Q14, where 1T HyPer wins.
- For Q1 and Q6 mutable beats the other systems; for Q3, Q12 and Q14 HyPer is ~2x faster (foreign-key joins). **[snippet]**

**Lesson**
- Reusing a production JIT (V8/Wasm) gives two tiers with little engineering.
- Costs: the Wasm sandbox (bounds checks, a 32-bit address space in wasm32) and FFI crossings to the runtime.
- The PostgreSQL-vs-mutable analysis (arXiv 2311.04692) notes mutable is easier to debug via Chrome DevTools. It is otherwise qualitative.

### 3.5 Nautilus: Grulich et al., "Query Compilation Without Regrets", SIGMOD 2024
DOI 10.1145/3654968. Code: https://github.com/nebulastream/nautilus

**Design**
- Operators are written as ordinary imperative C++ over `val<T>` types.
- A *tracing* JIT records the operator into Nautilus IR, which is then sent to a backend. Backends in the current repo: interpreter, MLIR, bc, tbc, tbc-jit, asmjit, and C++. The bytecode backend can use copy-and-patch (README).
- There is a query-compilation cache PR in NebulaStream: a cache hit skips tracing entirely.

**Numbers**
- The paper's CT and ET numbers could not be fetched (the ACM PDF returned HTML): **[gap]**.

**Lesson**
- The "write it like an interpreter, get a compiler" approach (a runtime Futamura projection via tracing) is viable in C++. A Rust analogue is possible with operator-overloaded builder types.

### 3.6 VOILA / Excalibur (Gubner & Boncz, CWI)
- VOILA, PVLDB 14(6) 2021: http://vldb.org/pvldb/vol14/p1067-gubner.pdf
  - A DSL for operators. The FUJI code generator lowers it to "CLite" as data-centric, vectorized, or mixed flavors.
  - Dimensions varied: selective processing, prefetching (AMAC/IMV), buffering, adaptivity.
  - Generated queries are 30% to 17.5x faster than DuckDB and LegoBase (1T), and up to 35.5x faster multi-threaded. **[snippet]**
- ADMS 2021: TPC-H Q1, Q3, Q6, Q9 at SF10 with 50 sampled flavors per query. Runtimes vary about 10x across machines, and ARM Graviton 2 was fastest. **[snippet]**
- Excalibur, PVLDB 16(4) 2022: https://www.vldb.org/pvldb/vol16/p829-boncz.pdf
  - A VM that JIT-compiles VOILA fragments and re-optimizes during the query, reusing already compiled fragments.
  - Picks among candidate flavors with a multi-armed bandit (heuristics or MCTS generate the candidates).
  - Numbers: **[gap]**.
- Follow-up: "Piece of CAKE" (Zhao & Marcus, arXiv 2602.04181, Feb 2026): https://arxiv.org/abs/2602.04181
  - Chooses per-morsel kernels with a microsecond-scale contextual bandit compiled into "regret trees".
  - Up to 2x lower end-to-end latency than static heuristics.

**Lesson**
- The best flavor (fused vs vectorized vs prefetching) differs per query and per machine.
- A compiler IR that can *emit several flavors* plus runtime selection beats committing to one paradigm.

### 3.7 Weld (Palkar et al., CIDR 2017; VLDB 2018)
URL: https://people.eecs.berkeley.edu/~matei/papers/2018/vldb_weld.pdf
- A functional loop/"builder" IR for cross-library fusion (NumPy, Pandas, Spark SQL), with an LLVM backend.
- Up to 23x faster on 1 thread and 80x on 8 threads.
- Adaptive optimizations (predication, choice of data structure via sampling) give up to 3.75x over rule-based optimization.
- Optimize + compile + sample is "sub-second". **[snippet]**
- Photon's codegen prototype started from Weld (see 4.2).

### 3.8 Voodoo (Pirk, Moll, Zaharia, Madden, VLDB 2016)
URL: https://www.vldb.org/pvldb/vol9/p1707-pirk.pdf
- A vector algebra (Scatter, FoldSum, ...) with "control vectors" that describe parallelism abstractly.
- Compiles to OpenCL for CPU and GPU. Built as a MonetDB backend.
- Matches HyPer and Ocelot on a TPC-H subset. **[snippet]**
- Per-query numbers: **[gap]**.

### 3.9 Proteus / ViDa (Karpathiotakis, Ailamaki, EPFL)
- VLDB 2016: https://proteusdb.com/publications/VLDB2016-rawjit/
- An LLVM engine generated per query *and per data format* (CSV, JSON, binary), with a single plan traversal.
- Later work: HetExchange (VLDB 2019) for JIT-compiled CPU+GPU parallelism.
- Numbers: **[gap]**.

### 3.10 Copy-and-patch (Xu & Kjolstad, OOPSLA 2021)
URL: https://arxiv.org/abs/2011.13127
- A baseline compiler that stitches precompiled binary "stencils" (generated by Clang from C++ templates) and patches holes for constants and jump targets.
- The authors built a SQL query compiler on it, "the first database query compiler equipped with a dedicated baseline compiler". **[snippet]**
- TPDE paper: TPDE is "not as fast as a copy-and-patch-based compiler" but within the same order of magnitude, with code on par with LLVM -O0.
- Now used by CPython 3.13 and Nautilus.

## 4. Industry

### 4.1 Apache Spark whole-stage codegen (WSCG, Tungsten, Spark 2.0, 2016)
- Blog: https://www.databricks.com/blog/2016/05/23/apache-spark-as-a-compiler-joining-a-billion-rows-per-second-on-a-laptop.html
  - Fuses each stage into one Java method, generated as Java source and compiled by Janino to bytecode; HotSpot then JITs it.
  - Hash join over 1B rows in under 1 s on 3 Haswell cores.
  - Most core operators are "an order of magnitude faster". Sort-merge join is barely improved.
  - Per-operator ns/row table: numbers not verified against the post, **[gap]**.
- Limits (Spark source and docs):
  - The JVM caps a method at 64KB of bytecode; exceeding it makes compilation fail and triggers a fallback when `spark.sql.codegen.fallback=true`.
  - HotSpot will not JIT methods over 8000 bytes of bytecode. `spark.sql.codegen.hugeMethodLimit` defaults to 65535, with 8000 recommended. Above the limit, WSCG is disabled for the entire subtree and Spark runs Volcano iterators.
  - Size is only known after Janino compiles, so splitting uses a source-length proxy (`methodSplitThreshold`, 1024 chars). **[snippet]**
  - Sources: https://github.com/apache/spark/blob/master/sql/catalyst/src/main/scala/org/apache/spark/sql/catalyst/expressions/codegen/CodeGenerator.scala ; https://github.com/apache/spark/pull/19440
- Lesson: generating a *source language* for another compiler leaves the engine blind to code size and CT. Cliffs such as the 64KB and 8000-byte limits turn into all-or-nothing performance drops.

### 4.2 Databricks Photon (SIGMOD 2022): why they did *not* compile
URL: https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf

**Process**
- They prototyped both approaches; the codegen prototype started from Weld.

**Reasons for choosing a vectorized interpreter**
1. Easier to build and debug: the interpreted engine is "just C++", while codegen made debuggers and stack traces hard to use. "a majority of the work in using a code generating runtime… was around adding tooling and observability rather than building the compiler."
2. Aggregation took 2 months to prototype with codegen vs "a couple weeks" vectorized.
3. Per-operator metrics are hard to report once operators are fused.
4. Adaptivity (e.g. ASCII-only or no-null fast paths chosen per batch) would require compiling "a prohibitive number of branches… or re-compile".
5. They note that "even HyPer… includes an interpreter".

**How they close the gap without codegen**
- Specialization via fused operators (e.g. a fused BETWEEN) and C++ templates with RESTRICT.
- Batch-level adaptivity (ASCII, no-nulls). This closes the gap "in many cases".

**Results vs Databricks Runtime (DBR, Spark WSCG)**
- TPC-H SF3000: average 4x per query, max 23x (Q1). The abstract says average 3x and max over 10x.
- Joins 3.5x (Fig. 4). SIMD ASCII upper: 3x (Fig. 6).

**Lesson**
- The costs were tooling, observability and adaptivity, not raw speed.
- A new compiling engine must budget for profiling and debug tooling up front (see LingoDB-CT and the Neumann/Leis advice).

### 4.3 Impala (Cloudera, 2014+)
- Wanderman-Milne & Li, IEEE Data Eng. Bull. 2014: http://sites.computer.org/debull/A14mar/p31.pdf
- **What is compiled:** "inner-loop" functions (tuple materialization, expression evaluation, hash functions), not whole fused pipelines.
- **How:** C++ runtime functions are precompiled to LLVM IR with Clang. At runtime Impala substitutes constants and types, inlines virtual calls, and optimizes.
- **Speed (Table 3, codegen off vs on)**

| Query | Off | On | Speedup |
|---|---|---|---|
| `count(*)` | 3.554 s | 2.976 s | 1.19x |
| `count(l_orderkey)` | 6.582 s | 3.522 s | 1.87x |
| TPC-H Q1 | 37.852 s | 6.644 s | 5.70x |

- TPC-H Q1 instruction counts: 72.9B → 19.4B (4.29x); branches: 14.45B → 3.32B (3.76x).
- **Compile cost:** 100-250 ms of codegen is common, significant for queries of about 2 s. **[snippet]**
  - IMPALA-2651 records excessive codegen time on huge BI-tool expressions. https://issues.apache.org/jira/browse/IMPALA-2651
- **Codegen cache (2024 blog):** on TPC-DS 1 TB, geo improves 4.8% over the whole suite and 22% for queries under 2 s. **[snippet]**
- Kersten 2018 calls Impala a hybrid: templates with LLVM-replaced functions and no fusion.

### 4.4 SingleStore (MemSQL)
- Docs: https://docs.singlestore.com/db/v8.9/query-data/advanced-query-topics/code-generation/
- **Pipeline:** SQL → AST → parameterized plan in MPL (MemSQL Plan Language, a high-level imperative DSL) → flattened into MBC (MemSQL ByteCode) → either interpreted or lowered to LLVM bitcode, then machine code.
- **Tiering:**
  - Interpret mode has "no additional latency" on the first run. Compiled code is "more than twice as fast" on later runs.
  - Hybrid mode (since MemSQL 6): the query is interpreted until compilation of its shape finishes.
  - Compiled plans are cached on disk per parameterized query shape (`plancache/`).
- Columnstore execution is vectorized; rowstore is tuple-at-a-time.
- Cold-start compiles of up to 10 s with the old C++ backend (pre-MemSQL 5) **[snippet: CedarDB blog]**.
- SIGMOD 2022 "Cloud-native transactions and analytics in SingleStore" was not accessible (paywall): **[gap]**.

### 4.5 Amazon Redshift
- "Amazon Redshift Re-invented", SIGMOD 2022: https://www.amazon.science/publications/amazon-redshift-re-invented (direct PDF download returned empty; figures below via secondary sources)
- **What is compiled:** plan *segments*, generated as C++ and compiled with GCC.
  - Generated code includes software prefetch instructions.
  - Hand-tuned pre-compiled primitives are injected to cut CT.
  - Source: CMU 15-721 notes https://15721.courses.cs.cmu.edu/spring2024/notes/22-redshift.pdf
- **Compilation-as-a-Service**
  - A separate compile fleet with a per-cluster local cache and a fleet-wide global cache.
  - Background workers recompile popular segments when a new Redshift version ships.
  - Cache hits rose from 99.60% to 99.95%: 87% of local misses were found in the external cache.
  - Source: http://muratbuffalo.blogspot.com/2022/09/amazon-redshift-re-invented.html (quoting the paper).
- **Still costly:** even so, "about half the end-to-end latency appears to be in compilation" (Neumann/Leis HYTRADBOI 2025, citing Redset).
- **Lesson:** caching alone does not solve CT for ad hoc and BI workloads; long-tail queries dominate the latency. Parameterized, segment-level caches are nonetheless very effective and cheap to add.

### 4.6 Microsoft SQL Server Hekaton (In-Memory OLTP)
- Freedman, Ismert, Larson, IEEE Data Eng. Bull. 2014: https://15721.courses.cs.cmu.edu/spring2016/papers/freedman-ieee2014.pdf
- **What is compiled:** natively compiled stored procedures and table definitions.
- **Path:** T-SQL → MAT (mixed abstract tree) → PIT (pure imperative tree) → C source → MSVC → DLL loaded in-process.
- **Philosophy:** "compile once, execute many". To go 10x faster you must execute 90% fewer instructions.
- **Results:**
  - Predicate evaluation: compiled code executes up to 10x fewer instructions than interpreted.
  - Relative instruction cost for a lookup query (interpreted on a regular table = 100%): interop 66%, compiled 19% (output to client); interpreted 94%, interop 61%, compiled 6% (output to variable). That is up to 15x fewer instructions.
  - End-to-end engine vs regular SQL Server (Diaconu et al., SIGMOD 2013): 10.8x for lookups, about 20-30x for updates **[snippet]**.
- **Lesson:** for OLTP (TPC-C in scope for rudb), compiling transactions ahead of time with a C compiler is fine, because CT is paid at DDL time.

### 4.7 PostgreSQL LLVM JIT (PG 11+)
- Docs: https://www.postgresql.org/docs/current/jit-decision.html ; https://www.postgresql.org/docs/19/runtime-config-query.html
- **What is compiled:** expression evaluation (the expression "step" programs) and tuple deforming. The rest of the executor stays Volcano.
- **Inlining:** operators and functions from PostgreSQL's own LLVM bitcode.
- **Decision:** based on planner cost.
  - `jit_above_cost` default 100000
  - `jit_inline_above_cost` 500000
  - `jit_optimize_above_cost` 500000
- **History:**
  - Shipped off by default in PG 11, turned on by default in PG 12.
  - **PG 19 turns it off again (`jit` default `off`).** Proposed by Jelte Fennema-Nio on 2026-01-30; the rationale is cost-threshold cliffs. Small statistics changes flip plans across the threshold, so a fast query suddenly pays hundreds of ms of compilation. Hyperscalers already disabled it. **[snippet for proposal details; the default of `off` is confirmed in the PG 19 docs]**
- **Alternatives:** pg_jitter adds sljit, AsmJit and MIR backends for PG 14-18 to cut CT (beta). https://github.com/vladich/pg_jitter
- **Lessons:**
  - Guarding compilation with *optimizer cost estimates* is fragile. HyPer and Umbra use observed runtime progress instead (per-morsel extrapolation).
  - Compiling only expressions inside a Volcano executor limits the upside.

### 4.8 ClickHouse JIT
- Blog: https://clickhouse.com/blog/clickhouse-just-in-time-compiler-jit ; talk: https://maksimkita.com/presentations/highload2022/jit_compilation_in_clickhouse/index.html
- **What is compiled:** fused expression DAGs, aggregate-function update/merge loops, and ORDER BY comparators. The engine stays vectorized; JIT sits *inside* vectorized operators.
- **Settings:**
  - `compile_expressions` and `compile_aggregate_expressions` are on by default.
  - `min_count_to_compile_expression`, `min_count_to_compile_aggregate_expression` and `min_count_to_compile_sort_description` all default to 3 (compile after the third sighting).
  - `compiled_expression_cache_size` is 1 GB, with LRU eviction.
- **Gains:** expression step 1.5-3x (sometimes over 20x); aggregation 1.15-2x.
  - Examples: +33% on a complex WHERE on hits, +34% on multi-`sum` GROUP BY, +71% with `minIf`.
  - PR #70598: 0.607 s → 0.281 s on 100M rows. **[snippet]**
- **Cost:** 5-15 ms per compiled function, linear in code size; about 2 pages (8 KB) per function. **[snippet]**
- **Correctness risk:** open issue #118334 (2026). JIT computes `least`/`greatest`/`midpoint`/`bitShiftRight` on Int128 with unsigned semantics, so results are wrong *from the 4th execution* once the expression is compiled. The wrong results are then served from the cache. Another fix stopped JIT-compiling range-checked numeric conversions (they wrapped instead of erroring). **[snippet]** https://github.com/ClickHouse/ClickHouse/issues/118334
- **Lesson:**
  - The "compile on the Nth execution" heuristic plus a cache works for dashboards (ClickBench-like repeated queries).
  - JIT/interpreter semantic divergence is a real production bug class, and the JIT path needs differential testing against the interpreter.

### 4.9 Velox (Meta, VLDB 2022)
URL: https://www.vldb.org/pvldb/vol15/p3372-pedreira.pdf
- **Default:** vectorized. "Expression compilation" means tree rewriting only (constant folding, CSE, flattening AND/OR).
- **Codegen (sec. 4.3.3):** experimental. The expression tree is rewritten as C++ source, compiled by gcc/clang into a shared library and dlopen-ed. "compilation times are usually high (up to 10s in some cases), and are not meant to be used in short lived queries or interactive workloads". Targets long ETL jobs and fixed expression trees (feature engineering).
- Runtime switching between vectorized and codegen paths is listed as future work.

### 4.10 Apache DataFusion
- `datafusion-jit` crate, 2022: Cranelift-based JIT for row-oriented work (row↔batch conversion, tuple comparisons, hash-agg updates). Issues: https://github.com/apache/datafusion/issues/1850 , https://github.com/apache/arrow-datafusion/issues/2703
- Advertised in DataFusion 8.0.0 (May 2022).
- The last crate release is 23.0.0 (April 2023). The crate is gone from current DataFusion: **[snippet]**.
- Reason (not verified): it was never enabled in any operator by default; arrow-rs's `RowConverter` replaced the need for it. **[gap: exact removal PR]**
- **Lesson for a Rust engine:**
  - The only Rust-ecosystem query JIT attempt was narrow (row format) and was abandoned.
  - Cranelift's CT is only ~20-35% better than LLVM-O0 (CGO'24).

### 4.11 SAP HANA HEX
- CIDR 2023 sponsor-talk slides: https://www.cidrdb.org/cidr2023/slides/sponsor-talk-sap-slides.pdf
- CMU 15-721 guest lecture 2019: https://15721.courses.cs.cmu.edu/spring2019/slides/26-saphana.pdf
- **Design:** data-centric code generation (citing Neumann 2011) in "L", SAP's LLVM-based language also used for stored procedures. Morsel-style pipelined push execution.
- **Tiering:** code first runs in the L interpreter ("heavy performance penalty, but execution can start immediately"). Hot code is compiled asynchronously with LLVM and swapped in. SAP admitted the L interpreter "is not optimal yet".
- **Claims:** more CPU-efficient; "performance same or slightly better"; less memory from fewer materializations.
- **Rollout:** gradual replacement of the older join and OLAP engines. Unsupported queries route to the old engines (a hint can force either path).
- Numbers: **[gap]**.

### 4.12 Oracle, SQL Server (non-Hekaton), Snowflake, Apple, Firebolt, Databend
- **SQL Server batch mode:** vectorized, no JIT, apart from Hekaton procedures. **[gap: no primary source fetched]**
- **Oracle:** no public query-JIT design found beyond PL/SQL native compilation. **[gap]**
- **Snowflake:** no public paper on codegen. Snowflake's SIGMOD 2016 paper describes a columnar, vectorized, push-based engine (from memory, not re-fetched). **[gap on any JIT]**
- **Apple:** nothing public found. **[gap]**
- **Firebolt:**
  - ClickHouse-derived, vectorized, batch-at-a-time.
  - A 2020 blog lists "JIT compilation" among features, but current product pages mention only vectorization. https://www.firebolt.io/blog/a-comparison-of-data-warehouse-and-query-engines-on-amazon-web-services-aws **[snippet]**
- **Databend (Rust):** vectorized, Arrow-based, uses `std::simd` / auto-vectorization. No JIT found. **[snippet]**
- **Pattern:** every vectorized cloud engine found (Photon, Velox, Snowflake, Databend, Firebolt, DuckDB) either never compiled or limited JIT to expressions.
  - Whole-query compilation in production: HyPer/Tableau, Umbra/CedarDB, SingleStore, Redshift, HANA HEX, Spark WSCG, Hekaton.

## 5. 2024-2026 frontier

### 5.1 Bespoke OLAP: arXiv 2603.02001v2 (PVLDB 19(11), VLDB 2026)
URL: https://arxiv.org/abs/2603.02001

**Idea**
- An LLM agent (Claude Sonnet 4.7; GPT 5.4-Codex was comparable) *synthesizes an entire engine* for a fixed workload: storage layout, operators and query code, in C++.
- Iterated with correctness tests.
- Synthesis takes minutes to hours and costs ≤$10-250.
- Hardware: AMD EPYC 9654P.

**Single-threaded results**
- TPC-H SF20: 4.4 s vs DuckDB 49.2 s (11.17x); 7.24x over Umbra.
- CEB SF2: 0.4 s vs 19.5 s (45.33x); 9.56x over Umbra.
- Per query: 2.83-102x (TPC-H) and 11.6-1500x (CEB).

**Multi-threaded results**
- TPC-H: 7.65x over DuckDB, 6.12x over Umbra.
- CEB: 23.97x over DuckDB, 1.87x over Umbra.

**Ablation (Table 1, speedup over DuckDB)**

| Storage | TPC-H: basic → optimized | CEB: basic → optimized |
|---|---|---|
| Flat | 1.26x → 5.18x | 0.57x → 8.09x |
| Bespoke | 2.34x → 12.35x | 2.10x → 51.40x |

- Storage specialization accounts for a large share of the gain.

**Details**
- The final TPC-H engine is ~11,600 lines of C++.
- Most-used techniques: inline fused aggregation (97.4% of queries), bitmap semi-join (73.7%), dictionary predicate rewrite (71%).
- Ad hoc queries go to a fallback DBMS.

**Relevance**
- This is the "10x DuckDB" ceiling for a *known* workload.
- The gap between Umbra (a general compiler) and bespoke code is 6-10x single-threaded. That gap mostly comes from physical-design and algorithm specialization, not from instruction selection.

### 5.2 GenDB: Lao & Trummer, arXiv 2603.02081; demo arXiv 2607.20630
- Five agents built on Claude Code generate **one executable per query**.
- Baselines: DuckDB 1.4.4, ClickHouse 26.2.1, Umbra (Feb 2026), PostgreSQL 18.2. TPC-H SF10, 64 threads.
- Main paper:
  - 5 TPC-H queries in 214 ms total vs DuckDB 594 ms and Umbra 590 ms (2.8x); 11.2x vs ClickHouse.
  - SEC-EDGAR 328 ms (5.0x DuckDB, 3.9x Umbra).
  - Q9 38 ms (6.1x).
- Demo: 249 ms total; 2.4x DuckDB and 2.7x Umbra.
- Synthesis cost: $14 and 91 min (TPC-H); $23 and 140 min (SEC-EDGAR).

### 5.3 Other 2025-2026 items
- **TPDE** (CGO 2026): see 1.5.
- **Piece of CAKE** (arXiv 2602.04181): per-morsel kernel selection by bandit, up to 2x. See 3.6.
- **SpeQL, "Speculative Ad-hoc Querying"** (arXiv 2503.00714, 2025):
  - On TPC-DS 10GB, "query compilation requires significantly more time than planning or execution" in the system studied. Precompiling speculative queries cuts compilation latency "from up to 10 seconds to a couple of milliseconds". **[snippet]**
  - Up to 289x lower user-perceived latency at $4/hour (abstract).
- **AI Query Compilation** (arXiv 2608.10139, Chung et al., Aug 2026): compiles AI/semantic queries for TPUs; up to 5.3x latency and 9.8x throughput on SemBench. **[snippet]**
- **"From Interpretation to Compilation"** (arXiv 2607.13407 vision; 2608.06677 system): query compilation for LLM semantic operators. Not relevant to SQL execution. **[snippet]**
- **arXiv 2311.04692** (PostgreSQL vs mutable JIT): qualitative only; mutable beat PostgreSQL.

## 6. The debate: compiled vs vectorized

### 6.1 Kersten, Leis, Kemper, Neumann, Pavlo, Boncz, "Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask", VLDB 2018
URL: https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf

**Setup**
- Two engines with the same algorithms and data structures: Typer (HyPer-style compiled) and Tectorwise (VectorWise-style).
- TPC-H SF1, 1 thread.

**Relative results**
- Typer 74% faster on Q1.
- Tectorwise 32% faster on Q9 and 4% faster on Q3.

**Table 1 (per tuple)**

| Query | Engine | Cycles | IPC | Instructions | L1 misses | LLC misses | Branch misses |
|---|---|---|---|---|---|---|---|
| Q1 | Typer | 34 | 2.0 | 68 | 0.6 | 0.57 | 0.01 |
| Q1 | Tectorwise | 59 | 2.8 | 162 | 2.0 | 0.57 | 0.03 |
| Q6 | Typer | 11 | 1.8 | 20 | n/a | n/a | n/a |
| Q6 | Tectorwise | 11 | 1.4 | 15 | n/a | n/a | n/a |
| Q3 | Typer | 25 | n/a | n/a | n/a | n/a | n/a |
| Q3 | Tectorwise | 24 | n/a | n/a | n/a | n/a | n/a |
| Q9 | Typer | 74 | n/a | n/a | n/a | n/a | n/a |
| Q9 | Tectorwise | 56 | n/a | n/a | n/a | n/a | n/a |
| Q18 | Typer | 30 | n/a | n/a | n/a | n/a | n/a |
| Q18 | Tectorwise | 48 | n/a | n/a | n/a | n/a | n/a |

- Tectorwise executes up to 2.4x more instructions and has up to 3.3x more L1 misses.

**Table 2 (ms)**

| Query | HyPer | VectorWise | Typer | Tectorwise |
|---|---|---|---|---|
| Q1 | 53 | 71 | 44 | 85 |
| Q6 | 10 | 21 | 15 | 15 |
| Q3 | 48 | 50 | 47 | 44 |
| Q9 | 124 | 154 | 126 | 111 |
| Q18 | 224 | 159 | 90 | 154 |

**SIMD**
- Up to 8.4x in micro-benchmarks, but only 1.4x on Q6.
- Hashing 2.3x, probing 1.4x.

**Multi-core (ms at 1 / 10 / 20 threads)**

| Query | Typer | Tectorwise |
|---|---|---|
| Q1 | 4426 / 496 / 466 | 7871 / 867 / 708 |
| Q6 | 1511 / 243 / 236 | 1443 / 213 / 196 |
| Q3 | 9754 / 1119 / 842 | 7627 / 913 / 743 |
| Q9 | 28086 / 3047 / 2525 | 20371 / 2394 / 2083 |
| Q18 | 13620 / 2099 / 1955 | 18072 / 2432 / 2026 |

- Both paradigms scale with morsels.

**Conclusions**
- Compiled wins on compute-heavy work (aggregation, expressions).
- Vectorized wins where memory stalls dominate (independent loads overlap across a vector, so cache misses are served in parallel).
- SIMD matters little for whole queries.
- Compiled also wins on OLTP and language integration.
- Vectorized wins on CT, profiling and adaptivity.
- The paper describes Impala as a hybrid (templates plus LLVM-replaced functions, no fusion).

### 6.2 Follow-ups that resolve the debate in practice
- **ROF (3.1):** add stage boundaries and prefetch inside compiled pipelines to fix the join-probe weakness.
- **InkFuse (3.3):** one IR, two backends, per-morsel choice.
- **VOILA / Excalibur / CAKE (3.6):** generate many flavors and pick at runtime.
- **Umbra's later numbers (1.3):** 1.8x over HyPer on TPC-H and 3x on JOB, mostly from engineering beyond "compiled vs vectorized" (the CIDR paper credits its storage, buffer and compilation design; the split between them was not isolated).
- **Photon (4.2):** vectorized plus specialization gets 3-4x over WSCG. The reasons for not compiling were engineering-process reasons.
- **Bespoke OLAP (5.1):** the remaining 6-10x over Umbra comes from specialization of storage and algorithms, not from the execution paradigm.

## 7. Compile-latency reference points (one place)

| Source | Workload | Backend | CT |
|---|---|---|---|
| Neumann 2011 | TPC-CH Q1-5 | C++/gcc | 1.6-2.6 s per query |
| Neumann 2011 | TPC-CH Q1-5 | LLVM | 16-41 ms per query |
| Kohn 2018 | TPC-H | bytecode | 0.4-1.2 ms |
| Kohn 2018 | TPC-H | LLVM unopt | 6-23 ms |
| Kohn 2018 | TPC-H | LLVM opt | 42-149 ms |
| Kohn 2018 | TPC-DS largest | LLVM opt | 911 ms |
| Tidy Tuples 2021 | 2000-join query | LLVM / LLVM fast-isel / Flying Start | 150 s / 4 s / <0.04 s |
| TPDE 2025 | TPC-DS SF1, all queries | LLVM-Opt / LLVM-O0 / TPDE / DirectEmit | 16.2 / 2.5 / 0.087 / 0.11 s |
| CGO 2024 | TPC-DS, all queries | GCC / LLVM-opt / LLVM-cheap / Cranelift / DirectEmit / interpreter | 48.9 / 11.4 / 1.63 / 1.07 / 0.06 / 0.03 s |
| LingoDB 2022 | TPC-H Q2 | MLIR + LLVM | 13 + 68 ms |
| LB2 2018 | TPC-H | LMS + GCC | 59-736 + 175-664 ms |
| InkFuse 2024 | TPC-H SF0.1 | C/clang | >40 ms |
| ClickHouse | per expression | LLVM | 5-15 ms [snippet] |
| Impala | typical query | LLVM | 100-250 ms [snippet] |
| Velox | expression | gcc/clang | up to 10 s |
| Redshift | fleet | GCC plus cache | ~50% of end-to-end latency (Redset) |

Rule of thumb from these points:
- LLVM-opt: 50-150 ms per average analytic query and up to ~1 s for the largest.
- LLVM-O0 / Cranelift: 5-25 ms.
- Single-pass: 0.1-2 ms.
- Bytecode: under 1 ms.
- DuckDB-class planning is ~0.5 ms.

## 8. Lessons for a new compiling engine

1. **Never make LLVM (or any optimizing compiler) the first tier.** Short queries lose to interpretation by 10-50x: HyPer catalog query (54 ms compile vs <1 ms run), InkFuse SF0.1 (interpreter up to 10x better), Umbra (HyPer spent up to 29x more time compiling than executing on cheap queries). ClickBench has many sub-100 ms queries.
2. **Tier inside a query, per pipeline, driven by observed morsel progress,** not by optimizer cost estimates. PostgreSQL's cost-threshold JIT caused enough plan-flip regressions that PG 19 turned JIT off by default. HyPer/Umbra/CedarDB extrapolate from per-thread morsel rates.
3. **Build a fast baseline tier that is a real code generator, not an interpreter of LLVM IR.** Single-pass emitters (Flying Start/DirectEmit/TPDE) reach 1.0-1.2x of LLVM-O3 execution at 100-180x lower CT. With such a tier, the interpreter tier becomes optional (Umbra's default is DirectEmit).
4. **Do not bet on Cranelift as the fast tier.** CGO'24: it compiles only 20-35% faster than LLVM-cheap and 16x slower than a single-pass backend, at similar code quality. It fits as a portable mid tier if LLVM is too heavy a dependency for Rust.
5. **Own the IR.** Every successful system has a custom, compact, SSA, LLVM-like IR (Umbra IR, CedarDB, MBC, Nautilus IR). It makes backends pluggable (interpreter, single-pass, LLVM, C for debugging) and keeps codegen under ~1 ms. Tidy Tuples is >1000x faster than LB2's staging.
6. **Derive the interpreter/vectorized tier from the same IR** (InkFuse, SingleStore MBC, HyPer bytecode from LLVM IR). Divergence is a real bug class: ClickHouse returns wrong Int128 results only after the 4th execution, once JIT kicks in. Run differential tests of tier against tier on every expression.
7. **Keep the "cogwheel" split:** generate only the hot per-tuple loop. Keep hash-table growth, spilling, string functions, decimal division and similar in precompiled Rust runtime functions called from generated code. For hot runtime functions, allow inlining via precompiled IR (Impala/PostgreSQL bitcode inlining) or copy-and-patch stencils.
8. **Structure pipelines as resumable step functions** (Umbra state machines). This enables morsel-driven parallelism, cancellation, spilling and tier switches mid-pipeline without recompiling.
9. **Leave room for buffering or prefetch boundaries inside fused pipelines** (ROF: up to 2.2x). Pure fused loops lose to vectorized on hash-probe-heavy queries (Kersten: Q9 32% slower, Q3 4% slower). JOB, CEB and TPC-DS joins are probe-bound.
10. **Expect compiled execution to win on aggregation/expression-heavy queries, not on everything.** Kersten Q1: 74% faster. Impala Q1: 5.7x from codegen. Hekaton: 10-15x fewer instructions. ClickHouse JIT: 1.5-3x on expressions and 1.15-2x on aggregation.
11. **Budget for tooling from day one.** Photon rejected codegen mainly because "a majority of the work… was around adding tooling and observability". Needed:
    - per-operator metrics in fused code (counters by IR region)
    - perf/jitdump symbolization
    - an IR-to-operator-to-SQL trace (LingoDB-CT)
    - a "C backend" or interpreter mode for debugging (Umbra uses GCC only for debugging)
12. **Batch-level adaptivity is where vectorized engines win.** Examples: ASCII-only, no-nulls, dense-key paths. A compiler must match it by generating a few guarded variants per pipeline, or by recompiling with runtime statistics (Excalibur, CAKE: up to 2x from per-morsel kernel choice).
13. **Cache compiled code by parameterized plan shape.** Redshift reaches 99.95% hits with a local plus global cache. SingleStore persists compiled plans per query shape. ClickHouse compiles after 3 sightings with a 1 GB LRU. Impala's codegen cache gives -22% geo on queries under 2 s. Caching is cheap and helps ClickBench-style repeated runs, but does not help first-run latency (Redshift: still ~50% of latency).
14. **Avoid generating source for an external compiler** (C++/Java/C). Examples: gcc 1.6-2.6 s/query (HyPer 2011), 46 s for TPC-DS (CGO'24), Velox up to 10 s, Spark's 64KB/8000-byte cliffs with all-or-nothing fallback. Keep a C emitter only for debugging and differential testing.
15. **Compile time must scale linearly in query size.** Generated BI SQL reaches 10 MB (Neumann/Leis). A 2000-join query took 150 s in LLVM vs <0.04 s single-pass. Cap function sizes; split huge pipelines into multiple functions.
16. **MLIR is a good mid-level representation but not a latency answer.** LingoDB gets small code (subop aggregation 384 lines vs DuckDB's 1358; auto-parallelization in 347 lines) but spends ~80 ms per query in MLIR+LLVM. It later added a TPDE BASELINE mode. For a Rust engine, MLIR's C++ dependency is also heavy.
17. **Precompiled vectorized kernels plus fused compiled loops is the proven hybrid.** HyPer uses vectorized scans; Redshift injects precompiled primitives; ClickHouse uses JIT inside vectorized operators. Reusing rudb's existing vectorized engine as tier 0 and fallback matches SAP HEX, SingleStore and InkFuse.
18. **Route unsupported features to the existing engine instead of blocking on coverage.** HANA HEX routes to the old engines, Bespoke OLAP to a fallback DBMS, Spark WSCG to Volcano. For 100% DuckDB compatibility, the compiler can start with the hot relational core (scan/filter/project/hash join/hash agg/sort/top-k).
19. **The 10x target is set by specialization, not by paradigm.** Umbra vs DuckDB is ~2-5x (LingoDB 3.5-4.8x; Umbra 2.3x over MonetDB on TPC-H). Bespoke engines reach 11x (TPC-H) and 45x (CEB) single-threaded over DuckDB mainly via storage layout, fused aggregation, bitmap semi-joins and dictionary rewrites. The compiler should expose these physical specializations (dictionary-code predicates, bitmap semi-joins, perfect hashing on dense keys, GroupJoin) as first-class codegen choices.
20. **Morsel-driven parallelism is orthogonal to paradigm and mandatory.** Typer and Tectorwise scale similarly. LingoDB's auto-parallelization is a 347-line pass. Design the IR so that parallel pipeline instantiation, thread-local hash tables and merge phases are compiler passes.
21. **OLTP (TPC-C) wants ahead-of-time compiled transactions.** Hekaton compiles stored procedures once (10-15x fewer instructions); HyPer 2011 reached 169k tps with LLVM. Prepared statements should compile once to the top tier, off the critical path.
22. **Keep x86-64 and AArch64 parity in the fast tier.** DirectEmit was x86-only at first, and CGO'24 has no AArch64 DirectEmit numbers. TPDE provides both. ARM (Graviton, Apple silicon) was often the fastest machine in VOILA's study. Choose a backend strategy with a small target-specific part (TPDE Umbra backend: 1.6k of 3.6k lines target-specific).
23. **Use LLM-driven specialization offline, not in the query path.** Bespoke OLAP and GenDB take minutes to hours and $14-250 per workload. They are useful for generating stencils, kernels and templates for known benchmark shapes, but a general engine still needs a millisecond-class online compiler.
