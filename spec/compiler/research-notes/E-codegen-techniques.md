# E. Code-generation techniques for query pipelines (what to generate)

Research notes for the rudb query-compiling engine (Rust, DuckDB-compatible, target ~10x DuckDB on ClickBench / TPC-H / JOB / TPC-C). Compiled 2026-09-24.

This file is about *what code a pipeline compiler should emit*: pipeline shape, tuple layout, the SIMD/vector mix, expression semantics, error paths, latency tiers, testing, OLTP and adaptivity. Backend choice (LLVM, Cranelift, a single-pass backend) is only covered where it changes what the front end should generate.

Conventions:
- Every number carries a source URL.
- "[snippet]" means the fact came only from a search-result snippet or a secondary summary, not from reading the primary text.
- "[background]" means well-known engineering knowledge not re-verified in this session. Check it before relying on it in the spec.
- Primary PDFs were downloaded and grepped with `pdftotext`. Sentence-level quotes are paraphrased unless shown in quotation marks.

## 0. Source index

| Key | Paper / artifact | URL |
|---|---|---|
| NEU11 | Neumann, "Efficiently Compiling Efficient Query Plans for Modern Hardware", PVLDB 4(9) 2011 | https://www.vldb.org/pvldb/vol4/p539-neumann.pdf |
| MORSEL | Leis, Boncz, Kemper, Neumann, "Morsel-Driven Parallelism", SIGMOD 2014 | https://db.in.tum.de/~leis/papers/morsels.pdf |
| DATABLK | Lang et al., "Data Blocks: Hybrid OLTP and OLAP on Compressed Storage using both Vectorization and Compilation", SIGMOD 2016, DOI 10.1145/2882903.2882925 | [snippet] |
| ROF | Menon, Pavlo, Mowry, "Relaxed Operator Fusion for In-Memory Databases", PVLDB 11(1) 2017 | http://www.vldb.org/pvldb/vol11/p1-menon.pdf |
| KER18 | Kersten et al., "Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask", PVLDB 11(13) 2018 | https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf |
| ADAPT | Kohn, Leis, Neumann, "Adaptive Execution of Compiled Queries", ICDE 2018 | https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf |
| LANG | Lang et al., "Make the most out of your SIMD investments: counter control flow divergence in compiled query pipelines", DaMoN 2018 / VLDBJ 2019 | https://db.in.tum.de/~lang/papers/simd_divergence.pdf |
| LB2 | Tahboub, Essertel, Rompf, "How to Architect a Query Compiler, Revisited", SIGMOD 2018, DOI 10.1145/3183713.3196893 | [snippet] |
| UMBRA | Neumann, Freitag, "Umbra: A Disk-Based System with In-Memory Performance", CIDR 2020 | https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf |
| TIDY | Kersten, Leis, Neumann, "Tidy Tuples and Flying Start: Fast Compilation and Fast Execution of Relational Queries in Umbra", VLDBJ 30(5):883-905, 2021 | https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf |
| EVOL | Neumann, "Evolution of a Compiling Query Engine", PVLDB 14(12) 2021 | https://vldb.org/pvldb/vol14/p3207-neumann.pdf |
| PCQ | Menon, Ngom, Ma, Mowry, Pavlo, "Permutable Compiled Queries", PVLDB 14(2) 2020 | https://www.vldb.org/pvldb/vol14/p101-menon.pdf |
| NGOM | Ngom, Menon, Butrovich, Ma, Lim, Mowry, Pavlo, "Filter Representation in Vectorized Query Execution", DaMoN 2021 | https://db.cs.cmu.edu/papers/2021/ngom-damon2021.pdf |
| VOILA | Gubner, Boncz, "Charting the Design Space of Query Execution using VOILA", PVLDB 14(6):1067 | http://vldb.org/pvldb/vol14/p1067-gubner.pdf |
| EXCAL | Gubner, Boncz, "Excalibur: A Virtual Machine for Adaptive Fine-grained JIT-Compiled Query Execution based on VOILA", PVLDB 16(4):829 | https://www.vldb.org/pvldb/vol16/p829-boncz.pdf |
| LINGO | Jungmair, Kohn, Giceva, "Designing an Open Framework for Query Optimization and Compilation" (LingoDB), PVLDB 15(11) 2022 | https://www.vldb.org/pvldb/vol15/p2389-jungmair.pdf |
| NAUT | Grulich et al., "Query Compilation Without Regrets" (Nautilus), SIGMOD 2024 / PACMMOD 2(3) | https://nebula.stream/paper/grulich_sigmod2024.pdf |
| CGO24 | Engelke et al., "Compile-Time Analysis of Compiler Frameworks for Query Compilation", CGO 2024 | https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf |
| GHT | Xue, Marcus, "Global Hash Tables Strike Back! An Analysis of Parallel GROUP BY Aggregation", arXiv 2505.04153 | https://arxiv.org/abs/2505.04153 |
| CAKE | Zhao, Marcus, "Piece of CAKE: Adaptive Execution Engines via Microsecond-Scale Learning", arXiv 2602.04181 (Feb 2026) | https://arxiv.org/abs/2602.04181 |
| MICRO | Raducanu, Boncz, Zukowski, "Micro Adaptivity in Vectorwise", SIGMOD 2013 | http://oai.cwi.nl/oai/asset/21351/21351B.pdf [snippet; PDF download failed] |
| HEK | Freedman, Ismert, Larson, "Compilation in the Microsoft SQL Server Hekaton Engine", IEEE DEB 37(1) 2014 | http://sites.computer.org/debull/A14mar/p22.pdf (read via https://15721.courses.cs.cmu.edu/spring2016/papers/freedman-ieee2014.pdf) |
| DSORT | Kuiper, Mühleisen, "These Rows Are Made for Sorting and That's Just What We'll Do", ICDE 2023 | https://duckdb.org/pdf/ICDE2023-kuiper-muehleisen-sorting.pdf |
| DSORT25 | DuckDB blog, "Sorting Again" (v1.4.0), 2025-09-24 | https://duckdb.org/2025/09/24/sorting-again |
| COMPACT | Qiao, Zhang, "Data Chunk Compaction in Vectorized Execution", SIGMOD 2025, DOI 10.1145/3709676 | https://people.iiis.tsinghua.edu.cn/~huanchen/publications/data-chunk-compaction-sigmod25.pdf [snippet] |
| WBMA | Pearce et al., "White-Box Micro-Adaptive Query Processing", ICDE 2025 | https://ieeexplore.ieee.org/document/11113219/ [snippet] |
| TLP | Rigger, Su, "Finding Bugs in Database Systems via Query Partitioning", OOPSLA 2020 | https://www.manuelrigger.at/preprints/TLP.pdf [snippet] |

## 1. Pipeline structure: produce/consume and what replaced it

### 1.1 The original model (NEU11)
- A **pipeline breaker** is an operator that "takes an incoming tuple out of the CPU registers", i.e. it materializes the tuple (hash-join build, aggregation, sort). Code between breakers is one tight loop that keeps attributes in registers. https://www.vldb.org/pvldb/vol4/p539-neumann.pdf
- Operator interface: `produce()` asks an operator to generate tuples. `consume(attributes, source)` is called by the child to push a tuple into the parent. Both are *compile-time* functions: they emit code, and are not called at runtime.
- Code is data-centric rather than operator-centric. Operator boundaries disappear inside a pipeline, and each pipeline becomes a loop nest over its source.
- Mixed execution: precompiled C++ "cogwheels" (index structures, spilling, complex runtime functions) are joined by an LLVM-generated "chain". The generated code calls into C++ for the cold, complex parts.
- Hash-chain probing loops are shaped so the common case is well predicted. LLVM was chosen over generating C++ partly because LLVM exposes **overflow flags** for checked arithmetic (C/C++ does not portably).
- Compile-time numbers (NEU11, HyPer):
  - TPC-C, C++ backend: 161,794 tps, 16.53 s total compile time.
  - TPC-C, LLVM backend: 169,491 tps, 0.81 s compile.
  - TPC-CH Q1:

    | Engine | Run time | Compile time |
    |---|---|---|
    | C++ backend | 142 ms | 1556 ms |
    | LLVM backend | 35 ms | 16 ms |
    | VectorWise | 98 ms | n/a |
    | MonetDB | 72 ms | n/a |
    | "DB X" | 4221 ms | n/a |

    https://www.vldb.org/pvldb/vol4/p539-neumann.pdf
- Takeaway for rudb: generating a high-level language and invoking a heavyweight compiler (C++, or Rust source + rustc) is ruled out for ad-hoc queries. The compile-time gap was 20x to 100x back in 2011.

### 1.2 Tidy Tuples: layering the generator (TIDY)
Umbra splits the generator into layers, top to bottom (https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf):

1. **Operator translators.** produce/consume per relational operator.
2. **Data structures.** Hash tables, buffers, sort runs; these are code-generating wrappers.
3. **Tuples.** Pack, unpack and hash of a tuple of SQL values into a memory layout.
4. **SQL values.** `SQLValue` = NULL indicator + value + SQL type. Every operation goes through `evaluateBinary`/`evaluateUnary`, which centralize NULL propagation, overflow checks and implicit casts.
5. **Codegen API.** A typed C++ API:
   - Types: `Int8..Int64`, `UInt*`, `Bool`, `Double`, `Data128`, `Ptr<T>`.
   - Operations: `crc32`, `rotate`, `bswap`, atomics.
   - Structured control flow: `If`, `Loop`, `Function` objects emit SSA and phi nodes directly, so no mem2reg pass is needed.

More points from TIDY:
- **Proxy system.** At C++ build time, proxies are generated automatically for precompiled runtime functions, so generated code can call them in a type-safe way. Functions can be marked "inline": the LLVM backend honors the marker, and the single-pass Flying Start backend ignores it.
- **Tuple hashing.**
  - Key values are concatenated into 64-bit words and fed to **two CRC32 chains** with different seeds (6763793487589347598 and 4593845798347983834).
  - Combine: `h = h1 ^ rotr(h2, 32)`, then multiply by 11400714819323198485 (the Fibonacci-hashing constant).
  - CGO24 adds that if hardware CRC32 is missing, Umbra falls back to a 64x64 to 128-bit multiply whose halves are XOR-folded ("long-mul-fold"). https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf
- **Speed of code generation itself.** Umbra's generator is >1000x faster than LB2's. LB2 needs a 299 ms geomean just to *generate* code, which caps it at ~3 queries/s.
- Lesson: the generator must be a fast, allocation-light, single-pass program. Heavy generic staging frameworks can dominate latency before the backend even starts.

### 1.3 Umbra IR (TIDY, CGO24)
- **Storage.**
  - Instructions are variable-length (opcode, type, args) and stored in one dynamic array.
  - Values are referenced by 4-byte offsets.
  - Constants are folded and deduplicated at append time.
  - A separate dead-code-elimination pass runs afterwards.
- **Checked arithmetic is a single instruction with two successors:**
  `%c = checkedsadd i32 %a, %b %continue %overflow`
  There is also a trapping form: `ssubtrap i32 %3, 53`, where "ssubtrap may call throwOverflow()". https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf
- **Domain instructions:**
  - `isnull`, `crc32`, `rotr`, 128-bit ops (`Data128`).
  - Loads and stores with inlined `getelementptr` addressing.
  - Functions tagged `noexcept`.
  - One complex operation is one instruction, "increasing the expressiveness and brevity".
- **Consequences.**
  - The IR is compact, cheap to build and cheap to lower.
  - Any backend (LLVM, C, asmJIT, bytecode) is a translator from this IR.
  - The IR is the single source of truth for semantics; backends only differ in performance.

### 1.4 Pipelines as state machines of steps (UMBRA, EVOL)
- Umbra splits each pipeline into **steps**. Each step is a generated function, and the query is a state machine over the steps. Execution can therefore suspend after any morsel (for I/O, for scheduling, or to switch tiers). https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf
- EVOL: one function per pipeline, driven by the state machine. The exception is provably small OLTP-style queries, which can be one function. https://vldb.org/pvldb/vol14/p3207-neumann.pdf
- Worker function signature:
  - EVOL uses a `pickMorsel` / `executeMorsel` split.
  - ADAPT uses worker functions `(state, morsel)` with morsels of ~10,000 tuples. https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf
- Global query state is read-only during a pipeline. Mutable state is thread-local and merged at the pipeline end (EVOL).
- **Register management (EVOL):**
  - Load attributes as late as possible and keep them in registers.
  - Materialize earlier when many outer joins/attributes would raise register pressure.
  - Umbra uses Ramalingam's loop-identification algorithm in its analyses.

### 1.5 Morsel-driven parallelism and compiled pipelines (MORSEL)
- Morsels are ~100,000 tuples; overhead is negligible above ~10,000. https://db.in.tum.de/~leis/papers/morsels.pdf
- The dispatcher hands (pipeline job, morsel) pairs to workers, with work stealing and NUMA-local storage areas. Average speedup is >30 on 32 cores.
- **Hash join build** is two-phase:
  - Workers first materialize tuples into thread-local storage.
  - The global table is then sized exactly and filled with a lock-free CAS insert.
  - Pointer tagging (spare high bits of the bucket pointer) filters misses instead of a separate Bloom filter.
- **Aggregation:** thread-local pre-aggregation in a small hash table. When it fills, entries spill to hash partitions, and each partition is then aggregated by one thread.
- **Cancellation:** a user abort, a numeric overflow exception or OOM sets a marker that workers check **after each morsel**, so no check is needed inside the tight loop.
- The paper explicitly states JIT-compiled pipelines fit the morsel scheduler: the compiled function just takes a morsel range.
- Implication: morsel boundaries are the natural points for cancellation, tier switching (ADAPT), adaptive kernel choice (CAKE) and deferred error checks.

### 1.6 Relaxed Operator Fusion: staging inside a pipeline (ROF)
Source: http://www.vldb.org/pvldb/vol11/p1-menon.pdf

- **Mechanism.**
  - A pipeline is split into **stages** separated by cache-resident vectors of tuple IDs (TIDs).
  - Each stage fills a complete output vector before handing over, except the last one.
  - A stage boundary is forced at the output of every SIMD operator, and inserted at the input of a random-access operator (hash probe) whose structure exceeds cache. This enables **group prefetching**.
- **Results:**

  | Query | vs Vector(wise) | vs HyPer |
  |---|---|---|
  | Q6 | 5.4x | 2.3x |
  | Q3 | 1.8x | 1.5x |
  | Q4 | 1.8x | 1.2x |
  | Q13 | ~1.4x | n/a |

  - Up to 2.2x over pure fusion (OLAP), and up to 1.8x vs other systems.
- **Prefetch group size:** best at 16, even though the CPU supports only 10 outstanding L1 misses. Vector size mostly does not matter, except Q13 (LIKE).
- **Tuple-at-a-time + prefetch** beat a SIMD hash join by up to 1.2x. SIMD on the Q1 scan would give at most 1.036x.
- HyPer won some queries because it hashes with SSE4 CRC32 while Peloton used MurmurHash3. The hash function matters.
- **Risk:** stage boundaries are placed using statistics, and wrong estimates put boundaries in the wrong place and slow queries down. That argues for making the decision adaptive, per morsel.

### 1.7 Data-centric vs vectorized: the empirical baseline (KER18)
Source: https://www.vldb.org/pvldb/vol11/p2209-kersten.pdf

- **Head to head.**
  - Typer (compiled) is 74% faster on Q1.
  - Tectorwise (vectorized) is 32% faster on Q9 and 4% faster on Q3. Vectorized wins on join-heavy queries because independent loads overlap memory latency.
  - Tectorwise executes up to 2.4x more instructions and has up to 3.3x more L1 misses.
- **Hashing:** Typer uses two CRC32 chains, Tectorwise uses Murmur2.
- **Vector size:** best around 1,000. Q3 is 15% faster at 64K.
- **SIMD:**
  - Selection: 8.4x dense, 2.7x sparse, 1.4x end-to-end on Q6.
  - Gather: 1.1x. Probe: 1.4x.
  - Auto-vectorization is "not fire-and-forget".
- **Other findings:**
  - Interpretation overhead in vectorized engines is below 1.5%.
  - Spark falls back to interpretation for pipelines over 8 KB of bytecode.
  - Compiled engines are harder to profile and harder to make adaptive.
- SF100, 20 threads: Q1 Typer 466 ms vs Tectorwise 708 ms.
- **Lessons:** a compiled engine for joins must recover memory-level parallelism, via ROF-style stages, prefetching, or batched probes. Profiling and adaptivity must be designed in, not added later.

### 1.8 VOILA / Excalibur: synthesize the flavor space
- **VOILA** (http://vldb.org/pvldb/vol14/p1067-gubner.pdf):
  - A DSL from which both data-centric and vectorized "flavors" are synthesized.
  - Generated queries reach hand-optimized performance and are up to 35.5x faster than well-known systems.
  - Single-threaded they are 30% to 17.5x faster than DuckDB and LegoBase. DuckDB was 4.3x to 9.5x slower on Q1/Q6 (the 2021 DuckDB).
  - The best flavors are ~3x better than average. There is a tail of runtimes >4x slower than the best, with outliers ~100x slower.
  - ROF is ~20% faster than VOILA.
  - The paper's rough history of TPC-H Q1 speed:

    | Step | Gain |
    |---|---|
    | Vectorized execution vs tuple-at-a-time | 40x |
    | Data-centric compilation vs vectorized | 2x |
    | BiPie | 3x |
    | Morsel parallelism on 48 cores | 48x |
    | Total | ≈10,000x |

- **Excalibur** (https://www.vldb.org/pvldb/vol16/p829-boncz.pdf):
  - A VM that JIT-compiles fine-grained fragments and explores flavors adaptively.
  - Up to 28x faster than open-source systems, up to 1.8x faster than Umbra, and up to 2x faster than static flavors on specific queries.
  - **A code cache is essential.** Caching 64 fragments makes Q1 ~26x faster than no cache. Q18 needs ~128 fragments for ~30x.
  - Fragment code footprint is ~10 kB, 40x smaller than a naive implementation.
  - **Amdahl limit for adaptivity:** a 10x speedup on 40% of a query, applied from the start, gives at most 1.5x overall. A 100x speedup gives 1.7x. A 4x speedup on 50% of the query gives 1.6x, and less if it is found mid-query (`S = (φ + (1-φ)/y)^-1`, applied to query progress).
  - Lesson: adaptivity pays only on the dominant pipeline and only when it is decided early.

## 2. SIMD inside compiled code; filter representation

### 2.1 Data Blocks (DATABLK) [snippet]
- HyPer's compressed Data Blocks:
  - The scan is an *interpreted, vectorized* (pre-compiled) component that evaluates restrictions on compressed data with SIMD.
  - It uses Positional SMAs (min/max plus lookup tables) to narrow scan ranges.
  - It emits matching tuples into the JIT-compiled pipeline.
  - SIMD is used only for integer comparisons.
- Why this matters: HyPer does not generate the scan per storage format. The number of compressed formats times the number of predicate types would blow up code size and compile time.
- Design point for rudb: **precompiled, vectorized, format-aware scan and filter kernels** (monomorphized Rust with SIMD), feeding a generated pipeline body through a TID/selection vector.

### 2.2 SIMD lane divergence (LANG)
Source: https://db.in.tum.de/~lang/papers/simd_divergence.pdf, code at github.com/harald-lang/simd_divergence

- **Problem:** in a SIMD-compiled pipeline, filters and probes deactivate lanes, and later stages run with poor lane utilization.
- **Fix:** refill lanes with AVX-512 `compress`/`expand` from a small buffer. Two strategies:
  - Buffered: hold partial vectors in registers or a small buffer.
  - Partial consume: consume only some lanes and keep the rest for the next round.
- Refill when utilization drops below ~75%.
- Result: up to 34% faster on a Q1-like scan and up to 25% faster on hash-join probes.
- This is ROF's idea (full vectors at stage boundaries) applied at register granularity.

### 2.3 Selection vectors vs bitmaps (NGOM)
Source: https://db.cs.cmu.edu/papers/2021/ngom-damon2021.pdf

- **Two representations:**
  - Selection vector (SV): a list of qualifying positions.
  - Bitmap (BM): 1 bit per row.
- **Findings:**
  - Bitmaps win for operations that can be SIMD'd over all rows. Selection vectors win otherwise.
  - Selectivity ≤ 0.15: partial SV evaluation (only selected rows) is best.
  - Above 0.15: a hand-written SIMD bitmap kernel that evaluates all rows is 3-11x faster.
  - Without hand-written SIMD, full-bitmap evaluation (BMFull) wins above ~0.35 by 2-7x.
- Proposes a **Mixed** strategy that switches by selectivity, using the cost model `R = N·(I+O)` (rows times input plus output cost).
- **Micro Adaptivity rule baseline** (MICRO): branching evaluation below 10% and above 90% selectivity, branch-free (predicated) evaluation in between. Vectorwise picks flavors with vw-greedy (epsilon-greedy). http://oai.cwi.nl/oai/asset/21351/21351B.pdf [snippet]

### 2.4 Chunk compaction (COMPACT) [snippet]
- After selective joins, vectors become sparse. Many small chunks lose the benefit of vectorized execution.
- **When to compact:** a learned runtime threshold.
- **How:** "logical compaction" for hash joins merges selection vectors instead of copying data.
- Up to 63% speedup in DuckDB on JOB, TPC-H and TPC-DS. https://people.iiis.tsinghua.edu.cn/~huanchen/publications/data-chunk-compaction-sigmod25.pdf
- This is the vectorized-engine version of LANG's lane refill and ROF's full-vector stages. A compiled pipeline that feeds precompiled kernels needs the same thing at kernel boundaries.

### 2.5 Adaptive predicate ordering and flavor switching
- **PCQ** (https://www.vldb.org/pvldb/vol14/p101-menon.pdf):
  - A DSL (TPL) is compiled to bytecode, then optionally to LLVM, with adaptive modes.
  - **Permutable filters:** conjunctive predicates are an array of function pointers, one compiled function per term. The engine reorders them by sampled selectivity/cost with probability p. Code does not need to be regenerated.
  - Also hot-key adaptive aggregation.
  - A static plan can be up to 4.4x slower than optimal. PCQ stays within 10% of optimal.
  - More than 4x over static plans, and ~2x over HyPer/Vectorwise-style baselines.
  - Codegen time grows only ~20% going from 1 to 7 terms.
- **CAKE** (https://arxiv.org/abs/2602.04181), see §9.

## 3. Staging, partial evaluation, multi-stage programming

### 3.1 LMS / LB2 / Futamura [snippet]
- LB2 (DOI 10.1145/3183713.3196893) writes a query *interpreter* in Scala with LMS (Lightweight Modular Staging). Staging the interpreter with respect to a query plan yields a compiler. This is the first Futamura projection.
- It matches HyPer-class code quality with a clean interpreter-style codebase [snippet].
- **Cost:** 299 ms geomean just to generate code (TIDY). That is unacceptable for short queries; Umbra's generator is >1000x faster.

### 3.2 Nautilus: tracing JIT for operator code (NAUT)
Source: https://nebula.stream/paper/grulich_sigmod2024.pdf, github.com/nebulastream/nautilus

- Operators are written in imperative C++ over `val<T>` wrappers. **Symbolic tracing** runs the operator code and records operations into an IR. This is a staging-by-overloading approach, like LMS but in C++.
- Each control-flow split needs another trace pass: worst case O(2^n) passes for n splits.
- **Backends:**

  | Backend | Notes |
  |---|---|
  | MLIR | Default; tens of ms |
  | C++ | Multi-second |
  | Bytecode | Interpreter |
  | Flounder | n/a |
  | MIR | Up to 2x faster than Flounder |

- Umbra outperforms Nautilus on complex queries.
- This is directly relevant to a Rust design. A `Val<T>` type with operator overloading can trace Rust operator implementations into rudb IR. The cost is paid per split at codegen time, so hot generic code (hash tables, sort) should be emitted by hand-written generators, not traced.

### 3.3 Multi-stage programming in Rust (design notes, [background])
Rust has no LMS-style type-directed staging. Practical options:

- **(a) Builder API.** A Tidy-Tuples-style API: typed wrappers (`I64Val`, `PtrVal<T>`, `SqlVal`) that append to a flat IR vec. This is the cheapest and most controllable option.
- **(b) Tracing.** `Val<T>` operator overloading, as in Nautilus.
- **(c) Proc-macros.** A `#[staged]` attribute that rewrites a Rust function body into builder calls. Good for runtime kernels that are shared between an interpreter and codegen.
- **(d) AOT runtime kernels.** Rust generics monomorphized at build time for the type × operator matrix, called from generated code through a proxy table (Umbra's proxies). This is the "cogwheels" of NEU11.

Recommendation: (a) + (d), with (c) optional later.

### 3.4 Backend cost facts that constrain the front end (CGO24, TIDY, ADAPT)
- **Cranelift:** code quality similar to unoptimized LLVM, compiles only 20-35% faster than (tuned) LLVM. Umbra's single-pass backend compiles 16x faster than Cranelift at similar execution performance. https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf
- **Cranelift IR constraints:** a small set of data types, no pointer or aggregate types (emulated with integers), wasm-aligned ops, no intrinsics such as crc32. The front end must lower such operations itself or call runtime helpers.
- **LLVM tuning (CGO24):**
  - FastISel for cheap builds brought >50% compile-time improvement.
  - GlobalISel is 1.4x faster than SelectionDAG when optimizing, but 2.7x slower than FastISel for cheap builds (+52% total).
  - SelectionDAG takes ~30% of optimized compile time.
  - Assembly printing takes 12% in cheap mode.
  - Destroying the LLVM module costs ~1%.
  - Caching the TargetMachine saves ~7%.
- **GCC via C:** an order of magnitude slower. Re-parsing the generated C alone is ~13% of compile time.
- **Flying Start** (TIDY):
  - Each IR instruction maps to one x86 sequence via asmJIT.
  - Liveness in linear time over reverse post-order (Kohn's algorithm).
  - Stack-slot reuse.
  - Register allocation over 11 of 16 x86 GPRs (4 scratch, 1 stack pointer), preferring values local to a block or in the innermost loop.
  - Lazy address calculation. Compare and branch are fused via deferred translation.
  - Register allocation alone cuts execution time 32% on average.
- **Tiers relative to LLVM -O3** (TIDY, as extracted in this session):

  | Tier | Compile time vs LLVM O3 | Run time vs LLVM O3 |
  |---|---|---|
  | Flying Start | 108x faster | 1.2x slower |
  | HyPer bytecode interpreter | 91x faster | 4.1x slower |
  | LLVM O0 | 6x faster | 1.3x slower |

  So Flying Start compiles about as fast as an interpreter while running almost as fast as optimized LLVM.
- **Scaling to huge plans:** a 2000-join query produced 108,000 IR instructions. Compile time was LLVM 150 s, LLVM fast-isel 4 s, Flying Start <0.04 s.
- **Prep time at SF 0.01:** Umbra 0.66 ms, DuckDB 0.47 ms, MonetDB 0.53 ms, HyPer 1.33 ms. Machine: i9-7900X, 10 cores.
- **EVOL:** LLVM compile of TPC-H queries went from 40-90 ms down to 1-2 ms in current Umbra. Umbra runs a 10,000-join query in ~5 s, while LLVM did not finish in 2 hours. A real customer query had 300,000 disjunctions. https://vldb.org/pvldb/vol14/p3207-neumann.pdf
- Front-end rules that follow:
  - Generated code must be **linear in plan size**.
  - Never emit per-row code proportional to the number of disjuncts. Use tables, IN-lists as hash sets, and loops over predicate arrays.
  - Keep functions small by outlining cold paths (error, NULL, overflow, spill).

## 4. Expression-level codegen

### 4.1 Overflow checks
- DuckDB semantics (the rudb compatibility target):
  - Integer `+ - *` and negation **raise** `Out of Range Error: Overflow in addition of INT32 (a + b)!` (and similar messages for the other operators).
  - `/` on integers returns DOUBLE in DuckDB (per Rosetta Code examples), so the overflow surfaces later as a cast error.
  - Literal typing drives the width: two INT32 literals overflow in INT32. https://rosettacode.org/wiki/Integer_overflow, https://github.com/duckdb/duckdb/issues/7094 [snippet]
- Whether the error is raised depends on whether the expression is evaluated, since optimizer rewrites can remove the error. DuckDB issue #12668 (GREATEST) shows logically equivalent queries where one errors and one does not. https://github.com/duckdb/duckdb/issues/12668 [snippet] rudb only needs to match DuckDB on the *common* paths. It must document that error-vs-no-error under reordering is not a stable contract.
- DECIMAL widens: `DECIMAL(18,0) + DECIMAL(18,0)` gives `DECIMAL(19,0)`. The width is capped at 38 digits, and past that the addition errors per row. Observed on DuckDB v2.0.0-dev build per rudb PR #254. https://github.com/tamnd/rudb/pull/254 [snippet]
- **How to generate the check:**
  - Umbra has a checked-arithmetic instruction with an overflow successor (`checkedsadd`), plus a trap form `ssubtrap` that calls `throwOverflow()` (TIDY, CGO24).
  - In machine code this is `add; jo cold_block`. The cold block is outlined and shared per function, so the hot path pays roughly one fused branch.
  - In Cranelift, the equivalent is `iadd` + an overflow flag. Recent Cranelift has `sadd_overflow`-style ops ([background]; verify for the pinned version).
- **SIMD/vector kernels:** compute wrapping in lanes, OR-reduce an overflow mask, then check once per vector. This is a **deferred check**, sound because the error aborts the query anyway and nothing observable was emitted.

### 4.2 Decimal as int128
- Umbra's Codegen API has a `Data128` type and 128-bit IR operations (TIDY). Decimals up to 38 digits map to i128 with a static scale. Scale alignment is a compile-time constant multiply.
- DuckDB stores DECIMAL by width: int16/32/64 for ≤4/9/18 digits, hugeint for ≤38. [background] Generating per-width code avoids i128 in the common ≤18-digit case.
- Rust has native `i128` with `checked_add`/`checked_mul`; lowering to LLVM gives add/adc pairs. For Cranelift, i128 is supported on x86-64/aarch64 ([background]; verify). i128 multiply with overflow detection is expensive, so emit it only when the result type actually needs 38 digits.
- **SUM(decimal) accumulators:** accumulate in i128 even for narrow inputs, and check overflow only at the final cast. This is a deferred check.

### 4.3 NULL handling
- Tidy Tuples: `SQLValue` carries a null indicator. `evaluateBinary` emits the NULL test once and propagates it. For NOT NULL columns, the null indicator is a compile-time constant `false`, so the branch disappears at codegen (TIDY).
- **Rules:**
  - Specialize on nullability, which is known from the schema, from `IS NOT NULL` filters, and from join semantics.
  - For nullable columns in SIMD kernels, compute the value unconditionally and combine validity masks with AND. Branch only for operations that can fail on garbage (division by zero, overflow, casts), and mask those errors by validity before the deferred check.
- Three-valued logic in filters:
  - A filter only needs "is TRUE", so `NULL` and `FALSE` collapse to a single "reject".
  - For `NOT`, `OR` over nullable inputs, and in projections, generate full 3VL, e.g. as a pair of bits (value, valid).

### 4.4 String layouts
- **Umbra "German strings"** (UMBRA, https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf):
  - A 16-byte header with a 4-byte length.
  - Strings of ≤12 chars are stored fully inline.
  - Longer strings keep a 4-byte prefix plus an 8-byte pointer or offset.
  - Pointer storage classes: *persistent* (lives with the DB), *transient* (valid for the current unit of work, e.g. a scan's buffer), *temporary* (created by the query, e.g. string functions).
- **Velox StringView:** the same shape. 4-byte size, 4-byte prefix (or inline content), 8 bytes of buffer id/offset or pointer, fully inline up to 12 bytes. Used for fail-fast prefix comparison and zero-copy `substr`/`trim`. The same layout was adopted in Arrow as StringView (Arrow 15). https://engineering.fb.com/2024/02/20/developer-tools/velox-apache-arrow-15-composable-data-management/
- **DuckDB `string_t`:** 16 bytes. Length is a u32. Strings of ≤12 bytes are inline. Otherwise a 4-byte prefix plus an 8-byte pointer. [background; same family per the Velox article, which says "Umbra and DuckDB follow a similar string representation"]
- **Codegen implications:**
  - Equality: compare the first 8 bytes (len + prefix) as one u64, then the second 8 bytes if inline, else memcmp.
  - Hash: CRC over the len+prefix word first, then the rest.
  - `LIKE 'abc%'` on short patterns: compare prefix words directly.
  - Transient pointers must be copied (made temporary or persistent) before a tuple crosses a pipeline breaker. The Tuples layer's pack function must know the storage class.

### 4.5 LIKE / regex
- ROF: Q13 (a LIKE query) was the only query sensitive to vector size. String predicates behave differently from arithmetic ones (ROF).
- [background, not verified this session] Common practice:
  - Specialize `LIKE` patterns at compile time into prefix, suffix, contains (memmem/SIMD substring search), or exact match.
  - Only general patterns go to a precompiled matcher or a compiled regex (e.g. Rust `regex` crate with a DFA). Regexes are compiled once per query as a constant in query state, never per row.
  - Apply the prefix check on the inline 4-byte prefix before dereferencing.

### 4.6 Hashing
- CRC32 hardware instruction, two chains with rotate/XOR and a multiply finalize (TIDY). Hardware CRC32 was one reason HyPer beat Peloton (MurmurHash3) in ROF, and Typer uses 2xCRC vs Tectorwise's Murmur2 (KER18).
- **Fallback when CRC is absent:** 64x64 to 128 multiply, XOR-folded (CGO24).
- Emit hashing as fused per-key code: with no generic loop over key columns, hashing a (int32,int32) key is ~4 instructions.

### 4.7 Compiled comparators and normalized keys (sorting)
- **Normalized keys** (DSORT, https://duckdb.org/pdf/ICDE2023-kuiper-muehleisen-sorting.pdf):
  - All ORDER BY columns are encoded into one memcmp-comparable byte string (a System R-era technique).
  - NULLs get a prefix byte. DESC is handled by bit inversion. Collations are evaluated before encoding. For strings only a prefix is encoded, and ties fall back to the full comparison.
- **Fixed-size memcmp:** a static-size `memcmp` or `memcpy` beats a dynamic one. It is 25% faster on average for sizes <16, and static memcpy was 55% faster on one CPU. So normalized-key width should be a compile-time constant (DSORT).
- Radix sort (O(nk)) vs quicksort: radix wins for many tuples and short keys, and loses for large k with small n (DSORT).
- The old DuckDB blog reports 100M integers sorted in just under 5 s single-threaded. https://duckdb.org/2021/08/27/external-sorting [snippet]
- **DuckDB v1.4.0 redesign** (DSORT25, https://duckdb.org/2025/09/24/sorting-again):
  - `create_sort_key` normalizes keys.
  - **Compile-time templated fixed-size key structs** (e.g. two u64 words plus an optional payload field) make comparisons static.
  - Three-tier sort: vergesort (detects presorted runs), ska sort (MSD radix on the first 64-bit word), pdqsort fallback.
  - Parallel k-way merge path via binary search, which is skew resistant and order preserving.
  - **Results vs the old sort:**

    | Workload | Speedup |
    |---|---|
    | 1B random ints | 2.7x |
    | 1B ascending ints | 10.4x |
    | TPC-H SF100 (600M rows) | 3.4x |
    | 8-thread scaling | 6.5x (old sort: 3.5x) |

    Single-threaded it is ~30% slower, due to in-place radix; the gap disappears at 2+ threads.
- **For a compiler:** generate the key-normalization function per query (a fused encode of all sort columns into a fixed-width key). Then use *precompiled* sort kernels monomorphized over key width (8/16/24/32 bytes + payload). This is exactly DuckDB's direction, and rudb PR #1304 ("fixed width sort keys") appears in the same line of work [snippet].

### 4.8 Aggregation (MORSEL, GHT)
- Classic approach: thread-local pre-aggregation, spilling to partitions, then per-partition merge (MORSEL; also what DuckDB and DataFusion do per GHT).
- **GHT** (https://arxiv.org/abs/2505.04153):
  - A shared global hash table via *ticketing*: each group gets a dense ticket (index) from a concurrent linear-probing table (a "Folklore*" variant). Aggregate state lives in arrays indexed by ticket.
  - A "fuzzy ticketer" hands out ticket ranges per thread to avoid contention.
  - Partial aggregates are updated with atomics or thread-local slots.
  - 1.78x throughput over partitioned aggregation at low cardinality with 48 threads. Atomics reach a 34.7x speedup at high cardinality.
  - Caveat: the evaluation assumes perfect cardinality estimates.
- **Codegen consequences:**
  - Generate the aggregate *update* as straight-line code per group key, e.g. `sum += v; cnt += 1` fused over all aggregates, with the NULL checks specialized away.
  - Choose global-ticketing or partitioned strategy at runtime from observed cardinality.
  - Keep both paths precompiled, with only the update body generated.
- Top-k / hash / aggregation is tracked for rudb in issue #350 (F5) [snippet].

### 4.9 Window functions
- Not covered by the primary sources read here (not verified).
- [background] Engines typically do three things:
  - Sort by (partition, order) using the normalized-key sort path.
  - Compute frame boundaries with precompiled kernels.
  - Use segment trees for non-invertible aggregates over sliding frames (Leis et al., VLDB 2015, "Efficient Processing of Window Functions in Analytical SQL Queries"; not fetched this session).
- For a compiler, the reasonable split is: generated key-encode and the per-row aggregate update function, plus precompiled frame/segment-tree machinery.

## 5. Error semantics in generated code

### 5.1 How the reference systems raise errors
- **Umbra:** checked instructions branch to an overflow block, or `*trap` instructions call `throwOverflow()`. Functions carry a `noexcept` attribute when they cannot throw (CGO24). Errors propagate as C++ exceptions through generated frames, so unwind info must exist for JIT code ([inference] from `noexcept` marking; the paper does not detail the unwinding mechanism).
- **MORSEL:** errors (numeric overflow, OOM, user abort) set a shared marker. Other workers see it after their current morsel and stop. The query then reports the first error.
- **DuckDB:** raises `OutOfRangeException` from vectorized kernels (C++ exception). [background]

### 5.2 Options for rudb (Rust)
- **(A) Error return, no unwinding (recommended default).**
  - Generated functions return a status code, e.g. `u32` 0 = ok, else an error id.
  - The failing kernel writes details (error kind, operand values, SQL type) into a per-thread error slot in the query state.
  - Cold "raise" blocks are shared per function: store the code, then return.
  - The Rust caller converts the code into `rudb::Error`.
  - This needs no unwind tables, and is robust across LLVM/Cranelift/interpreter tiers.
- **(B) Unwinding through JIT frames.**
  - Requires unwind info for generated code (DWARF CFI `.eh_frame` on Unix, `RUNTIME_FUNCTION` on Windows), registered with the unwinder (`__register_frame` / `RtlAddFunctionTable`).
  - All boundaries must use `extern "C-unwind"`. A panic escaping an `extern "C"` function aborts, and unwinding into Rust through `extern "C"` is UB. https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html, https://doc.rust-lang.org/stable/reference/panic.html
  - Cranelift can preserve frame pointers and emit unwind info, and it gained native exception support in 2025 ("Exceptions in Cranelift and Wasmtime", https://cfallin.org/blog/2025/11/06/exceptions/) [snippet]. libgcc's `__register_frame` takes a whole `.eh_frame` section while LLVM libunwind (macOS) wants one FDE per call ([background]; verify).
  - Use (B) only as a safety net: `catch_unwind` around every JIT entry for *unexpected* panics in Rust runtime helpers. Helpers themselves should catch and convert to status codes.
- **(C) Deferred error checks.**
  - For vector kernels and SIMD loops, accumulate an error mask (overflow, div-by-zero, cast failure) and test once per vector or morsel (see §4.1).
  - This is legal when the error is fatal to the query and no side effects escaped before the check. For DML pipelines, check before the write stage.
  - Error-message fidelity: once the mask is non-zero, re-scan the vector scalar-wise to find the first failing row and produce the DuckDB-exact message with operand values.
- **Error ordering:** DuckDB is itself not stable about which error (or whether any) appears under optimization (§4.1). Specify "some error from the set of errors the query could raise"; do not promise "the first row's error".

### 5.3 Cancellation and timeouts
- Check a cancellation flag once per morsel (MORSEL). Morsels are ~10k-100k tuples, and ADAPT uses ~10,000 per worker call, giving sub-millisecond response.
- Loops with unbounded inner work need a counter-based check on back-edges every N iterations: hash-chain walks on skewed keys, nested-loop joins, `generate_series`, string functions over huge values. This must be emitted by the generator; the backend will not add it.
- Umbra's step state machine lets a query suspend at morsel boundaries (UMBRA). The same mechanism gives cooperative cancellation and tier switching.

## 6. Short-query latency: compile budget and tiers

### 6.1 Evidence that compile time dominates small queries
- HyPer spent up to 29x more time compiling than executing on cheap queries (UMBRA).
- ADAPT, TPC-H Q1 with LLVM (https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf):
  - IR generation: 0.7 ms.
  - Optimization passes: 30 ms.
  - Machine-code compilation: 19 ms.
  - Adaptive path: bytecode translation 0.4 ms, unoptimized LLVM 6 ms, optimized 17 ms + 25 ms.
  - LLVM's built-in interpreter is >800x slower. HyPer's custom VM interpreter is ~800 lines of code.
  - Plans contain 300-19,000 IR instructions, with near-linear compile time.
- LingoDB (MLIR): 3.5x faster than DuckDB at SF1 single-threaded. Its query optimization is <2000 lines of code. HyPer compiles significantly faster at SF1 [snippet]; no exact compile-time numbers were extracted. https://www.vldb.org/pvldb/vol15/p2389-jungmair.pdf
- Nautilus MLIR backend: tens of ms. C++ backend: multi-second (NAUT).
- DuckDB's own guidance: prepared statements help "mostly for repeatedly running small queries (with a runtime of < 100ms)". DuckDB is not designed for many concurrent small queries. https://duckdb.org/docs/lts/guides/performance/how_to_tune_workloads [snippet]

### 6.2 Adaptive execution mechanism (ADAPT)
- Start every pipeline in the bytecode interpreter immediately, so the time to first tuple is ~codegen time (sub-ms).
- **Decision rule:**
  - After a 1 ms delay, and again after every morsel, `extrapolatePipelineDurations` estimates the remaining time under three modes: t0 = keep interpreting, t1 = compile unoptimized, t2 = compile optimized.
  - Each estimate adds compile time, divided across the available threads (compilation runs in the background on one thread while the others keep interpreting).
  - Pick the minimum.
- Switching is cheap because the worker function is called per morsel. The next morsel simply calls the new function pointer, with no on-stack replacement.
  - Same in Umbra (CGO24): "Advanced mechanisms for switching functions are not necessary, as morsel-driven parallelism ensures that the function is called for sufficiently small workloads."
  - Umbra's adaptive backend estimates compile time and benefit "through a simple heuristic on the code size" after a function has run a few times (CGO24).
- **Tiers in Umbra today:**
  - Flying Start (direct x86 emission; <0.04 s for 108k instructions).
  - LLVM optimized for long-running pipelines (TIDY, CGO24).
  - Earlier: a bytecode VM, then LLVM (UMBRA).

### 6.3 Code caching
- Excalibur: a fragment code cache gives 26-30x speedups when warmed (§1.8).
- Hekaton compiles once, at `CREATE PROCEDURE` time (§8).
- **Parameterization vs baking constants:**
  - Baking constants enables folding, per-literal LIKE specialization and IN-list perfect hashing.
  - Parameterizing enables cache reuse.
  - Practical split: bake *types, nullability, collation, pattern shape*. Pass *values* through the query state (loaded once per morsel into registers).
  - Re-specialize only when a parameter changes the plan-relevant shape, e.g. a LIKE pattern that turns from prefix to contains.

## 7. Correctness and testing of a query compiler

### 7.1 Differential and metamorphic testing
- **SQLancer TLP** (https://www.manuelrigger.at/preprints/TLP.pdf) [snippet]:
  - Ternary Logic Partitioning: `Q` must equal `Q WHERE p ∪ Q WHERE NOT p ∪ Q WHERE p IS NULL`.
  - It found 175 bugs (125 fixed, 77 logic bugs) across the systems tested.
  - Of those, 60 came from the WHERE oracle, 10 from aggregates and 3 from HAVING.
  - More bugs were found in DuckDB and TiDB than in other systems. A per-DB slide row suggests ~61 DuckDB bugs, 11 of them assertion failures in a debug build (approximate; derived from a slide snippet).
  - SQLancer's README claims 400+ bugs overall.
  - DuckDB runs SQLancer in CI [snippet]. https://github.com/sqlancer/sqlancer
- **NoREC:** compares an optimized query `SELECT * WHERE p` against an unoptimizable `SELECT (p IS TRUE) FROM t` count. For a compiler this is ideal: it compares compiled filter code against projection code.
- rudb already has TLP-style partition testing in rudb-compat (PR #71, "Split a query three ways and require the parts to add up"). https://github.com/tamnd/rudb-compat/pull/71 [snippet]
- **Tier-differential testing** (the most important one for a JIT): run every test query under every tier and require identical results *and identical error classes*:
  - reference interpreter
  - bytecode VM
  - fast backend
  - optimizing backend
  - "vectorized kernels only"

  Adaptive switching must also be tested by forcing a tier switch at every morsel boundary with random schedules.
- **Differential against DuckDB:** the compatibility target is itself an oracle for results, types and error messages.

### 7.2 Fuzzing the generator and backend
- Cranelift is actively fuzzed. For example CLIR (arXiv 2606.26977) does liveness-driven, structure-aware fuzzing of Cranelift. https://arxiv.org/pdf/2606.26977 [snippet; title only]
- Rudb-level recommendations [background]:
  - Fuzz the IR verifier with random well-typed IR.
  - Fuzz expressions (random expression trees over edge values such as INT_MIN, -1, 0, NULL, empty string, 12/13-byte strings at the inline boundary, DECIMAL(38) max) and compare tiers.
  - Property-test normalized-key encode vs comparator order.

### 7.3 Debugging aids (EVOL, LingoDB-CT)
- Umbra (https://vldb.org/pvldb/vol14/p3207-neumann.pdf):
  - A **C backend** lets generated code be compiled with sanitizers and debugged with normal tools.
  - The IR carries `source_location` of the *generator* C++ code, so a bad instruction maps back to the line of the translator that emitted it.
  - A time-traveling debugger.
  - Profiling is mapped across layers (machine code → IR → operator → plan node).
- LingoDB-CT (SIGMOD 2025 demo): MLIR location tracking and snapshots across lowering stages [snippet].
- The same apply to rudb:
  - Tag every IR instruction with (plan node id, generator `#[track_caller]` location).
  - Keep an IR pretty-printer.
  - Provide an `EXPLAIN (CODEGEN)` that dumps IR and the chosen tiers.
  - Register JIT code with `perf` (perf map / jitdump) so profiles show pipeline names.

## 8. OLTP: compiling point queries and procedures

### 8.1 Hekaton (HEK)
Source: http://sites.computer.org/debull/A14mar/p22.pdf

- **Motivation in instruction counts:** "To go 10X faster, the engine must execute 90% fewer instructions … To go 100X faster, it must execute 99% fewer instructions." A B-tree key lookup may take thousands of instructions, and a simple interpreted transaction several hundred thousand.
- **Pipeline:** T-SQL stored procedures and table definitions go through a Pure Imperative Tree (PIT), then **C code**, then MSVC, then a DLL loaded into the server.
- Compilation happens at `CREATE TABLE` / `CREATE PROCEDURE` time ("compile-once-and-execute-many"; explicitly *not* for ad-hoc queries).
- Tables get customized callbacks (hash/compare/serialize per table). All types are known at compile time.
- Compiled code executes up to 10x fewer instructions. From the Diaconu et al. SIGMOD 2013 Hekaton paper: lookups ~20x (10.8x for a single lookup) and updates ~30x faster [snippet].

### 8.2 HyPer TPC-C (NEU11)
- LLVM-compiled TPC-C: 169,491 tps with 0.81 s total compile (vs the C++ backend at 161,794 tps and 16.53 s). Transactions are compiled once and reused.
- Umbra (TIDY era) had no OLTP implementation, so its compile numbers are OLAP only.
- EVOL: provably small OLTP queries are compiled into a single function, skipping the pipeline state machine.

### 8.3 Design consequences for TPC-C-class workloads in rudb
- **Plan/code cache keyed by** (normalized SQL text or plan hash, parameter types, schema version, settings). Invalidate on DDL.
  - DuckDB itself caches parse and plan in prepared statements, but reportedly may rebind or replan on `EXECUTE` (issue #17237, open April 2025). https://github.com/duckdb/duckdb/issues/17237 [snippet]
  - PR #14616 avoids rebinds caused by prepare and execute running in separate transactions. https://github.com/duckdb/duckdb/pull/14616 [snippet]
  - rudb can beat DuckDB here simply by honoring the cache.
- **Point-lookup specialization:**
  - Generate the index probe inline: hash of the key with CRC, bucket load, key compare fused.
  - Project directly from the row without materializing a chunk.
  - Skip morsel scheduling and parallelism for single-row plans (EVOL's single-function case).
  - Target: a few hundred instructions per lookup (Hekaton's framing).
- **Constants:** parameters are passed in the query state, never baked in, so one compiled procedure serves every parameter value (Hekaton compiles per procedure, not per call).
- **Interpreter first:** for a one-shot OLTP statement, the bytecode tier's sub-ms startup beats any compile. Compile only after the cache-hit count crosses a threshold. This is the same tiering as ADAPT, but keyed on repeat count instead of pipeline duration.

## 9. Adaptivity and profile guidance

### 9.1 Umbra thresholds
- Tier decisions are made per pipeline (ADAPT):
  - First decision after 1 ms.
  - Re-evaluated every morsel.
  - Compile-cost estimates are linear in IR size.
  - Current Umbra uses a code-size heuristic after a function has run a few times (CGO24).

### 9.2 Micro Adaptivity (MICRO) [snippet]
- Vectorwise keeps multiple "flavors" per primitive: branching vs predicated, loop-unrolled or not, compiler variants.
- A vw-greedy (epsilon-greedy) bandit picks per call, using cycles per tuple.
- Rule baseline: branching at <10% or >90% selectivity.

### 9.3 CAKE (CAKE, arXiv 2602.04181, Feb 2026)
Source: https://arxiv.org/abs/2602.04181

- **Per-morsel choice** of a kernel implementation via a *contextual bandit*. Tasks include:
  - Index/filter/slice iterator strategies.
  - Quicksort vs heapsort (e.g. for top-k).
  - Nested predicate evaluation order.
- **Cheap counterfactuals:** because morsels are small, the engine can occasionally run alternative kernels on the same data. This gives low-noise rewards.
- **Learned policies compile into "regret trees":** smaller than a cache line, inference <20 ns, vs ~1 µs for general model inference. The decision fits inside the per-morsel overhead budget.
- Up to 2x lower latency. ~2x gap on IMDb/JOB-style workloads.
- The prototype is in **Rust**.
- Related: Excalibur, VOILA, MICRO.
- Fit for rudb:
  - The morsel boundary already calls a function pointer. CAKE's decision becomes "which precompiled/generated variant pointer to call next".
  - Generate 2-3 variants only for pipelines whose estimated cost is large (Amdahl, §1.8).

### 9.4 White-box micro-adaptivity (WBMA, ICDE 2025) [snippet]
- "Hazard-adaptive" operators read hardware counters (branch mispredictions, cache misses) at runtime and switch between an implementation that is fast when there are no hazards and one that is robust to them.
- Motivation: order-dependent properties (sortedness, clusteredness) that optimizers cannot capture.
- https://ieeexplore.ieee.org/document/11113219/
- For rudb: counters are an optional signal. CAKE's timing-based rewards are portable and need no perf access.

### 9.5 Adaptive joins and deoptimization
- ROF: prefetch-stage placement depends on hash-table size vs cache, which is known after the build phase. Decide at probe-pipeline start, not at plan time (ROF).
- MORSEL: the two-phase build gives exact hash-table sizing, and pointer tagging acts as a Bloom-like early reject (MORSEL).
- Tailwind (arXiv 2604.28079, Yu, Marcus, Kraska, 2026): runtime plan rewrite to accelerators, with geomean speedups of 1.38x, 1.76x and 1.28x in case studies with Redshift and DuckDB. It is planner-level, not codegen. https://arxiv.org/abs/2604.28079
- **Deoptimization** in a morsel engine means "stop calling specialized version A, call generic version B from the next morsel". A pipeline speculates on things like:
  - no NULLs seen
  - strings ≤12 bytes
  - no overflow in narrow accumulators
  - dictionary-encoded input

  It must carry a guard that returns a "deopt" status for the current morsel. The morsel is then re-run in the generic version. This is legal only if the specialized version produced no side effects for that morsel, so emit output to a morsel-local buffer, or guard before the first side effect.

## 10. 2024-2026 work worth tracking

| Year | Work | What | Numbers | URL |
|---|---|---|---|---|
| 2024 | Nautilus (SIGMOD) | Tracing JIT for operator code, multi-backend | MIR up to 2x over Flounder; MLIR tens of ms | https://nebula.stream/paper/grulich_sigmod2024.pdf |
| 2024 | CGO24 compile-time analysis | LLVM vs GCC vs Cranelift vs Umbra single-pass | Cranelift 20-35% faster than LLVM; single-pass 16x faster than Cranelift | https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf |
| 2025 | Data Chunk Compaction (SIGMOD) | Learned compaction threshold, logical compaction in DuckDB | up to 63% | https://people.iiis.tsinghua.edu.cn/~huanchen/publications/data-chunk-compaction-sigmod25.pdf [snippet] |
| 2025 | White-Box Micro-Adaptive QP (ICDE) | HW-counter-driven operator switching | n/a in snippet | https://ieeexplore.ieee.org/document/11113219/ [snippet] |
| 2025 | Global Hash Tables Strike Back (PVLDB) | Ticketing global aggregation | 1.78x (low card, 48 thr); atomics 34.7x speedup | https://arxiv.org/abs/2505.04153 |
| 2025 | DuckDB v1.4 sort | Fixed-width normalized key structs + vergesort/ska/pdq + k-way merge path | 2.7x random, 10.4x sorted, 3.4x TPC-H SF100 | https://duckdb.org/2025/09/24/sorting-again |
| 2025 | LingoDB-CT (SIGMOD demo) | Compiler-level debugging and tracing via MLIR locations | n/a | [snippet] |
| 2025 | Cranelift exceptions | Native exception support; enabled panic unwinding in rustc_codegen_cranelift | n/a | https://cfallin.org/blog/2025/11/06/exceptions/ [snippet] |
| 2026 | CAKE (arXiv 2602.04181) | µs-scale contextual bandits for per-morsel kernel choice; Rust prototype | up to 2x; <20 ns decisions | https://arxiv.org/abs/2602.04181 |
| 2026 | Bespoke OLAP (VLDB'26, arXiv 2603.02001) | LLM-synthesized workload-specific engines, built in minutes to hours | "order-of-magnitude" over DuckDB | https://arxiv.org/abs/2603.02001 |
| 2026 | Tailwind (arXiv 2604.28079) | Accelerator-aware runtime query rewriting | 1.38x / 1.76x / 1.28x geomean | https://arxiv.org/abs/2604.28079 |
| 2026 | GenDB demo (arXiv 2607.20630) | LLM agents generate instance-optimized query code; offline for recurring queries | "significantly better" (no number in abstract) | https://arxiv.org/abs/2607.20630 |
| 2026 | AI Query Compilation (arXiv 2608.10139) | SQL + LLM inference compiled to one tensor graph | 5.3x latency, 9.8x throughput on TPUs | https://arxiv.org/abs/2608.10139 |
| 2026 | CLIR (arXiv 2606.26977) | Structure-aware fuzzing of Cranelift | n/a | https://arxiv.org/pdf/2606.26977 [snippet] |

Reading of the 2026 LLM-synthesis papers (Bespoke OLAP, GenDB):
- They make the ~10x-over-DuckDB target plausible for *recurring* workloads, because much of DuckDB's gap is "structural overhead, including runtime schema interpretation, indirection layers, and abstraction boundaries" (Bespoke OLAP abstract).
- That is exactly what a per-query compiler removes, but without paying minutes or hours of synthesis.
- They are not a substitute for an online compiler on ad-hoc ClickBench/JOB queries.

Not found or not verified in this session:
- The exact title and numbers for GaussDB 30TB TPC-H (arXiv 2608.28352).
- The copy-and-patch paper (arXiv 2011.13127, Xu & Kjolstad, OOPSLA 2021): not re-read. Its relevance is as a stencil-based backend that sits between the interpreter and Flying Start.

## 11. Synthesis: a concrete shape for rudb's generated code

1. **Plan → pipelines.**
   - Split at breakers: hash build, aggregation, sort, window, UNION ALL materialization, and result sinks.
   - Each pipeline = source (precompiled scan kernel per storage format, SIMD filter on compressed data) → generated *body* (the fused operator chain) → sink (generated insert/update into a data structure whose algorithms are precompiled).
2. **Body granularity.** The body is one generated function per pipeline: `fn(state: *const QState, local: *mut TLState, morsel: Range, sel: *const u32, n: u32) -> Status`. It is invoked per morsel, or per scan batch inside a morsel.
3. **Batches inside the body (ROF-style stages).**
   - The source hands a selection vector of ≤1024-2048 rows.
   - The body processes rows tuple-at-a-time in registers.
   - At a probe of a cache-exceeding hash table, it writes probe keys, hashes and TIDs to a stage buffer, then runs a group-prefetch probe loop (group ~16).
4. **Tuples layer.**
   - Generated pack/unpack/hash per tuple type.
   - German-string handling with storage-class promotion at breakers.
5. **SQL values layer.** One `SqlVal { val, null: Option<Reg>, ty }` abstraction:
   - NULL paths are elided when `null == None`.
   - Overflow uses checked ops with a shared cold `raise(code)` block.
   - Decimal widths are specialized.
6. **Runtime calls.** Hash tables, sort, spill, string functions, regex and aggregates with complex state are precompiled Rust with a stable `extern "C"` ABI (no unwinding). They are declared to the generator by a generated proxy table.
7. **Tiers.**
   - T0: IR interpreter/bytecode, used immediately.
   - T1: fast single-pass or Cranelift.
   - T2: LLVM optimized.
   - Switch per morsel using ADAPT's extrapolation.
   - Cache T1/T2 artifacts keyed by plan hash for prepared and repeated statements.
8. **Adaptivity.**
   - Permutable filter arrays (PCQ).
   - SV-vs-bitmap selection mode by selectivity (NGOM, MICRO).
   - Kernel variants chosen by a CAKE-style bandit at morsel boundaries.
   - Aggregation strategy (thread-local+partition vs global ticketing) chosen at runtime (GHT).
9. **Errors and cancellation.**
   - Status return codes.
   - Per-thread error slot.
   - Deferred vector-level checks.
   - Cancellation flag per morsel, plus back-edge counters in unbounded loops.
   - `catch_unwind` at JIT entry as a safety net only.
10. **Testing.**
    - Every query in the test corpus runs on all tiers with forced random tier switches.
    - TLP/NoREC oracles.
    - DuckDB as the differential oracle for values, types and error messages.

## Design rules for a new pipeline compiler

1. **Generate code in one linear pass over the plan, into a compact flat IR** (4-byte value refs, constants folded and deduplicated at append). Code generation must be ≪1 ms for TPC-H-size plans. LB2's 299 ms generation time is the anti-pattern (TIDY).
2. **Layer the generator** as operator translators → data structures → tuples → SQL values → typed codegen API. Only the SQL-value layer knows about NULLs, overflow and casts (TIDY).
3. **Make checked arithmetic, isnull, crc32, rotate, i128 and fused address loads first-class IR instructions.** Do not expand them early; each backend lowers them best (TIDY, CGO24).
4. **Never make runtime a function of expression size where a table or loop works.** A 300,000- disjunct predicate and 10,000-join queries exist in the wild (EVOL). Emit IN-lists as hash sets and big OR-chains as loops over arrays.
5. **One generated function per pipeline, invoked per morsel from a state machine.** Morsel boundaries are where cancellation, tier switching, adaptive kernel choice and deferred error checks happen (MORSEL, UMBRA, ADAPT).
6. **Morsels of ~10k-100k tuples; batches of ~1k rows inside.** Overhead is negligible above 10k (MORSEL). Vector size ~1k is best in most cases (KER18).
7. **Keep scans and format-specific filters precompiled and vectorized (SIMD on compressed data)** and hand a selection vector to the generated body. Do not generate per-format scan code (DATABLK).
8. **Fuse operators tuple-at-a-time inside the body, but insert ROF stage boundaries before cache-exceeding hash probes and prefetch in groups of ~16.** Decide at probe-pipeline start from the *actual* built table size, not from estimates (ROF, KER18).
9. **Pick the filter representation by selectivity.** Selection vector ≤~15%. Full SIMD bitmap evaluation above that when the kernel vectorizes. Branching only outside 10-90% (NGOM, MICRO).
10. **Refill SIMD lanes or compact chunks when utilization falls** (below ~75% lanes, or small chunks after joins) (LANG, COMPACT).
11. **Hash with two hardware CRC32 chains plus rotate/XOR/multiply**, with a multiply-fold fallback when CRC32 is missing. Fuse hashing per key type; no generic column loops (TIDY, CGO24, ROF).
12. **Specialize on nullability, decimal width and string inline-ness at compile time.** Emit NULL checks only for nullable inputs, i128 only for >18-digit results, and heap dereference only after the 12-byte inline and 4-byte prefix checks fail (TIDY, UMBRA).
13. **Adopt the 16-byte German-string layout** (len u32, 4-byte prefix, inline ≤12, else pointer) with explicit storage classes. Promote transient strings at every pipeline breaker (UMBRA, Velox).
14. **Overflow semantics must match DuckDB messages**, but checks should be `add; jo cold` scalar, or mask-accumulated and checked once per vector in SIMD code. On failure, rescan scalar-wise to build the exact message (§4.1, §5.2).
15. **No unwinding through JIT frames on the normal path.** Use status-code returns plus a per-thread error slot. Runtime helpers are `extern "C"` and never panic across the boundary. `catch_unwind` at JIT entry is only a safety net. If unwinding is ever needed, it must be `extern "C-unwind"` with registered unwind tables (RFC 2945).
16. **Check cancellation per morsel, and add counted back-edge checks in any loop whose trip count is not bounded by the morsel** (hash chains, NLJ, generators, big strings) (MORSEL).
17. **Tier every pipeline:** start in an IR interpreter with sub-ms first tuple. Compile in the background with a fast backend, and with an optimizing backend when extrapolated remaining time justifies it (first decision after ~1 ms, re-evaluated per morsel). Switch by swapping the function pointer between morsels, without OSR (ADAPT, CGO24).
18. **The fast tier must compile ~100x faster than LLVM -O3 at ≤1.3x slower code** (the Flying Start point). Cranelift alone is only 20-35% faster than tuned LLVM, so budget for a custom single-pass or copy-and-patch tier if short-query latency matters (TIDY, CGO24).
19. **Generate the sort key normalization per query into a fixed-width key struct**, and sort with precompiled, width-monomorphized radix/pdq/merge-path kernels. Static-size compares and copies beat dynamic ones by 25-55% (DSORT, DSORT25).
20. **Generate only the aggregate *update* body.** Choose partitioned thread-local vs global ticketed hash tables at runtime from observed group counts. Keep both precompiled (MORSEL, GHT).
21. **Express conjunctive filters as permutable arrays of compiled term functions**, and reorder them by sampled cost/selectivity without recompiling (PCQ).
22. **Put adaptive kernel choices behind µs-scale learned policies at morsel boundaries**, using cheap counterfactual runs and regret-tree policies (<20 ns decisions). Only do this for pipelines that dominate runtime, because Amdahl caps late or partial wins (CAKE, EXCAL).
23. **Speculate with guards and morsel-granular deoptimization.** A specialized body returns a DEOPT status before its first side effect, and the morsel is re-run in the generic version.
24. **Pass parameter values through query state; bake in only types, nullability, collations and pattern shapes.** Cache compiled pipelines by plan hash + parameter types + schema version, and really reuse them on EXECUTE (DuckDB reportedly rebinds; issue #17237) (HEK, NEU11).
25. **Compile provably tiny OLTP plans into a single function** with an inline index probe and no morsel machinery. Target Hekaton-class instruction counts (a few hundred per lookup). Interpret one-shot statements, and compile after a repeat threshold (HEK, EVOL).
26. **Every IR instruction carries (plan node, generator call site)**, via `#[track_caller]`. Provide an IR dump, `EXPLAIN (CODEGEN)`, and perf jitdump registration. Keep a C- or Rust-source debug backend for sanitizer runs (EVOL, LingoDB-CT).
27. **Test differentially across all tiers with forced random tier switches**, plus TLP/NoREC oracles and DuckDB as the result/type/error oracle. Fuzz expressions at type boundaries (INT_MIN, DECIMAL(38) max, 12/13-byte strings, NULLs) (TLP, SQLancer).
28. **Measure where time goes before adding adaptivity or SIMD.** SIMD gave ≤1.036x on Q1's scan (ROF) and 1.1-1.4x for gathers and probes (KER18). Memory-level parallelism and hash-function choice often matter more than instruction count.
