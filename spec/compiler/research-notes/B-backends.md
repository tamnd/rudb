# B. Compiler backends and low-latency JIT techniques (research notes, 2026-09-24)

Scope: backends for rudb's query-compiling engine (Rust, DuckDB-compatible, target ~10x DuckDB on
ClickBench/TPC-H/JOB/TPC-C). Dev box Apple M4 (AArch64, macOS/Mach-O); servers x86-64 (c6a Zen3/Zen4,
c7i Sapphire Rapids) and Graviton (Neoverse V1/V2). Both ISAs are first-class.

Conventions:
- Numbers are copied from the cited source. "[snippet]" = only seen in a search-result snippet, not verified in full text.
- "[GK]" = general knowledge, no number claimed; verify before relying on it.
- Ratios are "A vs B" as stated by the source. Watch direction (compile-speed-up vs runtime slowdown).

Corrections to the task brief (verified):
- QBE is NOT evaluated in Engelke & Schwarz CGO'24. That paper covers Umbra interpreter, DirectEmit, Cranelift, LLVM (cheap/opt), and GCC via C.
- Tailored Profiling is EuroSys 2021 (Beischl et al.), not SIGMOD.
- TPDE numbers differ between arXiv v1 (May 2025) and the CGO 2026 final version. The CGO'26 numbers are used below; v1 numbers are listed separately.
- CGO'24 says DirectEmit is x86-64 only (AArch64 port never merged). The TPDE CGO'26 paper says DirectEmit is 11 kLOC "for AArch64 and x86-64". So an AArch64 DirectEmit exists by 2025/26. Treat CGO'24's statement as describing the 2023 state.

---------------------------------------------------------------------------------------------------

## 1. Umbra: Flying Start / DirectEmit, Umbra IR, LLVM tier, adaptive switching

### 1.1 Umbra IR (Tidy Tuples paper + CGO'24)
Sources:
- Kersten, Leis, Neumann, "Tidy Tuples and Flying Start: fast compilation and fast execution of relational queries in Umbra", VLDB Journal 30:883-905 (2021). https://doi.org/10.1007/s00778-020-00643-4 (PDF mirror at db.in.tum.de/~kersten/)
- Engelke & Schwarz CGO'24 (sec. 1.3).

Design points:
- Custom SSA IR designed for fast generation, not fast optimization.
  - Variable-length instructions stored contiguously in one buffer.
  - No use-lists.
  - The only transform is dead-code elimination, which removes about 4% of code (Tidy Tuples).
  - Types are DB-centric: 128-bit decimals with overflow checks, 16-byte strings (German strings), CRC32-based hashing (CGO'24).
- Tidy Tuples is the codegen framework layered above the IR. It uses a staged-programming style in C++: operators emit IR through typed "SQL value" wrappers. That makes the code generator itself cheap. Planning plus codegen for TPC-H SF0.01 takes 0.66 ms in Umbra vs 1.33 ms in HyPer, and codegen is more than 2x faster than HyPer's.
- For comparison: DuckDB takes 0.47 ms and MonetDB 0.53 ms for plan+prepare at SF0.01 (same paper). A compiled engine can match interpreted engines on latency only if the whole front half is cheap.

### 1.2 Flying Start (now "DirectEmit")
Source: Tidy Tuples/Flying Start (VLDBJ 2021).
- It is a single-pass x86-64 backend from Umbra IR to machine code. It has:
  - its own liveness analysis;
  - "stack-space reuse";
  - a machine-code register allocator that is cheap and greedy per block, with loop-aware lifetimes;
  - address-mode folding and comparison/branch fusion.
- Headline (geomean TPC-H SF1, 20 threads, i9-7900X): Flying Start compiles 108x faster than LLVM -O3 and executes 1.2x slower.
  - HyPer's bytecode interpreter compiles 91x faster and executes 4.1x slower.
  - HyPer LLVM -O0 compiles 6x faster and executes 1.3x slower.
- Scaling stress test, a query with 2000 joins producing 108,000 Umbra IR instructions:

| Backend | Compile time |
|---|---|
| LLVM optimized | 150 s |
| LLVM unoptimized/FastISel | 4 s |
| Flying Start | < 0.04 s |

- Register allocation reduces execution time by 32% relative to a no-RA (all-stack) version.
- Linear scan was evaluated: it gave 1% faster execution for 14% more compile time, so it was not adopted. Lesson: for a baseline tier, a greedy allocator is at the knee of the curve.
- Code-quality gap vs optimized LLVM (perf counters): 1.6x cycles, 2.3x instructions, IPC 1.4x (higher, because there are more simple instructions), 2.4x code size. Branch misses and LLC misses are about equal. The gap is instruction count, not memory behavior, which matters for memory-bound analytic queries.

### 1.3 Engelke & Schwarz, CGO 2024, "Compile-Time Analysis of Compiler Frameworks for Query Compilation"
Source: https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf (DOI 10.1109/CGO57630.2024.10444856)

Setup:
- Umbra with pluggable backends. The compile-time workload is all TPC-DS queries: 6678 generated functions.
- x86: Intel Xeon Gold 6338 (32 cores).
- AArch64: Apple M1 (4 P-cores) running Asahi Linux.

Table III, TPC-DS SF10, total compile time / total execution time:

| Backend | x86-64 compile | x86-64 exec | AArch64 compile | AArch64 exec |
|---|---|---|---|---|
| Umbra interpreter (on Umbra IR) | 0.03 s | 15.40 s | 0.02 s | 64.55 s |
| DirectEmit | 0.06 s | 4.83 s | n/a (x86 only) | n/a |
| Cranelift | 1.07 s | 4.62 s | 0.61 s | 16.37 s |
| LLVM-cheap (-O0 + FastISel) | 1.63 s | 5.23 s | 0.74 s | 19.45 s |
| LLVM-opt (-O2-ish) | 11.36 s | 4.12 s | 5.86 s | 12.88 s |
| GCC (C source, -O3 -march=native, external process) | 48.88 s | 4.28 s | 41.64 s | 13.99 s |

Observations from the paper:
- DirectEmit compiles all 6678 functions in 64 ms total, which is about 10 µs per function.
  - Its analysis pass is about 75% liveness.
  - Register allocation is about 30% of its codegen phase.
- Cranelift compiles only 20-35% faster than LLVM-cheap. DirectEmit is about 16x faster than Cranelift at similar execution speed (4.83 vs 4.62 s).
- Cranelift's code is slightly better than LLVM-cheap's (4.62 vs 5.23 s x86; 16.37 vs 19.45 s AArch64).
- LLVM-opt's code is the best, but only 12-15% better than DirectEmit/Cranelift in aggregate. Per query it can be larger: TPC-DS Q17 runs in 0.93 s with LLVM-opt vs 1.29 s with DirectEmit (38%).
- End-to-end (compile+exec):
  - TPC-H SF10: DirectEmit is almost always the best choice (Cranelift wins once).
  - TPC-H SF100: LLVM-opt pays off for several queries.
  - The optimizing tier only helps for long-running queries, consistent with adaptive execution.
- The interpreter is 3.2x slower than DirectEmit on x86 (15.40 vs 4.83 s), and 3.9x slower than Cranelift on AArch64 (64.55 vs 16.37).

LLVM compile-time findings:
- Tricks that helped cheap mode by more than 50% combined:
  - Small-PIC code model (FastISel supports only the small code model);
  - 128-bit data as 2 x i64 (about 7% faster);
  - a cached TargetMachine per thread;
  - an upstreamed FastISel CRC32 patch (4%).
- SelectionDAG ISel is about 30% of optimized compile time.
- There were 3876 FastISel fallbacks to SelectionDAG, costing 36% of ISel time: 2486 from intrinsics/calls and 1328 from 128-bit types. Lesson: keep IR within the FastISel-supported subset (avoid i128, prefer plain calls over intrinsics).
- GlobalISel on AArch64:
  - optimized builds: 1.4x faster than SelectionDAG (-10% total compile time);
  - cheap builds: 2.7x slower than FastISel (+52% total).
- Register-allocation-related passes are 25-45% of RA time.
- Pipelines have 146 passes (opt) vs 67 (cheap).
- Cheap-mode overheads: AsmPrinter/MC 12%, fixups 3%, linking/JIT about 7%, prologue/epilogue insertion 4%, MachineInstr::addOperand 3%, legacy pass manager 5%, module destruction 1%. A long tail of fixed costs remains no matter how cheap ISel gets.

Cranelift findings:
- Missing features required custom instructions:
  - CRC32 (improves TPC-DS mean by 19% and TPC-H by 9% once added);
  - overflow-checked arithmetic;
  - full (wide) multiply.
- cranelift-jit is "not quite mature": no GOT/PLT handling (crashes when callee code is far away) and no unwind info.
- Phase split: IR generation 8%, ISel prep 8%, ISel 12%. Register allocation (regalloc2) is the largest phase. LLVM's RA was 42% faster.
  - Within RA, 37% of time is liveranges and 6% is B-trees.
- C++ <-> Rust interop overhead was significant (Umbra is C++). This does not apply to rudb, which is Rust.

GCC:
- Total 46.34 s, of which cc1 is 44.92 s and parsing about 13%. External-process compilation is out of the question for latency.
- libgccjit was excluded because of GPL licensing.

Umbra's policy (as described in CGO'24):
- Start every pipeline with DirectEmit.
- After a few executions, a size heuristic may trigger LLVM-opt.
- The authors say no advanced switching is needed, because morsel-driven execution keeps individual calls short.

### 1.4 Umbra and TPDE (CGO'26), DirectEmit size
- TPDE's Umbra back-end is 3.3 kLOC, of which 1.4k are target-specific. For comparison:
  - DirectEmit is 11 kLOC (AArch64 + x86-64);
  - Umbra's LLVM back-end adapter is 2.3 kLOC.
- On TPC-DS SF1, TPDE-for-Umbra IR matches DirectEmit in both compile and run time with "almost zero overhead".
- TPDE-LLVM (Umbra IR -> LLVM IR -> TPDE) pays a visible cost for building LLVM IR.
- Source: https://home.cit.tum.de/~engelke/pubs/2602-cgo1.pdf

### 1.5 CedarDB (Umbra commercialization)
- Blog by Bandle, 2025-04-02, https://cedardb.com/blog/compilation/
- Custom IR plus a custom "ASM" backend. It achieves "nearly 90% of the throughput in 1% of the compile time" and switches to LLVM for long-running queries.
- UmbraPerf / tailored profiling won the VLDB 2025 Best Demo (see sec. 10).

---------------------------------------------------------------------------------------------------

## 2. Adaptive execution: switching at morsel boundaries

Source: Kohn, Leis, Neumann, "Adaptive Execution of Compiled Queries", ICDE 2018 (Best Paper). https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf

Motivating numbers (HyPer, LLVM):
- A pg_catalog query compiles in 54 ms with LLVM-opt but executes in < 1 ms.
- TPC-H Q1 compiles in 59 ms. The largest TPC-H query compiles in 146 ms; the largest TPC-DS query in 911 ms.
- Compile time is near-linear in LLVM instruction count, which ranges from 300 to 19,000 per plan.
- The LLVM IR interpreter (lli) is more than 800x slower than machine code. HyPer therefore wrote its own fast bytecode VM from LLVM IR, compiled with linear-time liveness ("Kohn et al." liveness is reused by TPDE).

Mechanism:
- Each pipeline starts in the bytecode VM. All worker threads run morsels.
- After a 1 ms delay, one thread evaluates after each morsel whether to compile, using extrapolation:
```
extrapolatePipelineDurations(f, n, w):     # n = remaining tuples, w = workers
  r0 = avg(rate in threadRates)            # current tuples/s/thread
  r1 = r0*speedup1(f); c1 = ctime1(f)      # unoptimized: est. speedup, est. compile time
  r2 = r0*speedup2(f); c2 = ctime2(f)      # optimized
  t0 = n / r0 / w
  t1 = c1 + max(n - (w-1)*r0*c1, 0) / r1 / w   # other w-1 threads keep going during compile
  t2 = c2 + max(n - (w-1)*r0*c2, 0) / r2 / w
  choose argmin(t0,t1,t2): DoNothing / Unoptimized / Optimized
```
- Compile time estimates are linear in instruction count, and speedups are constants calibrated per mode.
- Morsel sizes grow dynamically, so switch-check overhead is amortized.
- Code swap is at a morsel boundary. Because pipeline state lives in memory (hash tables, local aggregates), switching tiers needs no OSR frame translation. This is the key simplification compared with JS/JVM OSR (sec. 8).

Relevance to rudb: this maps directly onto a morsel-driven engine.
- Tier 0 = vectorized/AOT kernels.
- Tier 1 = baseline JIT.
- Tier 2 = optimizing JIT.
- Tiers must share the same pipeline-state ABI (in-memory structs), which is the precondition for switching.

---------------------------------------------------------------------------------------------------

## 3. TPDE and TPDE-LLVM

Sources:
- Schwarz, Kamm, Engelke, "TPDE: A Fast Adaptable Compiler Back-End Framework", CGO 2026. https://home.cit.tum.de/~engelke/pubs/2602-cgo1.pdf
- arXiv 2505.22610 (v1 May 2025): https://arxiv.org/abs/2505.22610
- Code: https://github.com/tpde2/tpde (Apache-2.0 WITH LLVM-exception, C++)

### 3.1 Architecture
- It is a framework for building single-pass back-ends for any SSA IR.
  - The IR adapter is a C++ template parameter (static polymorphism), with no virtual calls on the hot path.
  - The framework needs the IR to expose blocks, values, successors, phis and operands.
- Analysis pass (single):
  - loop detection (Wei et al. algorithm);
  - block layout in RPO keeping loops contiguous;
  - liveness via Kohn et al. (live ranges as [first,last] block intervals with loop extension).
- Codegen pass (single, fused): instruction selection, register allocation and encoding happen together.
  - Values have 16-byte "value assignments" (register/stack slot/constant state per part).
  - Greedy on-the-fly RA with spill at block/loop boundaries per liveness.
- Snippet encoders:
  - Instruction patterns are written as small C/LLVM-IR functions and compiled by LLVM to MIR.
  - A generator turns the MIR into C++ encoder functions with operand placeholders.
  - This gives copy-and-patch-like convenience without the runtime copying, and the allocator can still choose registers.
- It emits ELF object files or in-memory code, with x86-64 and AArch64 (Armv8.1) targets.

### 3.2 TPDE-LLVM (LLVM IR -> machine code)
Size:
- 7.7 kLOC total, of which 1.4k are architecture-specific.
- An earlier version without snippet encoders was 12.8 kLOC.

SPEC CPU2017 int (-O0 IR), CGO'26 final, hardware Xeon Gold 6430 and Apple M1 on Asahi Linux, LLVM 20.1.7:
- Compile time: 8-26x faster than LLVM -O0 back-end.
  - Geomean 13.88x on x86-64.
  - Geomean 18.29x on AArch64, where LLVM -O0 uses GlobalISel, which is slower than x86's FastISel.
  - The arXiv abstract says "8-24x".
- Runtime is within ±9% of LLVM -O0, and instruction fusion is worth 8%.
- Code size: +22% (x86) and +16% (AArch64) vs -O0.
- In the same paper, a copy-and-patch compiler built for comparison was 19.56x faster than -O0 at compile time. Its code was 2.32x slower and 4.27x larger. That is direct evidence that TPDE-style beats copy-and-patch on code quality at similar compile speed.
- On -O1-optimized IR:
  - compile is 106x (x86) / 107x (AArch64) faster than LLVM -O1 back-end, and 18.4x / 21.1x faster than LLVM -O0 back-end;
  - code is 1.50x / 1.71x slower than LLVM -O1 codegen.
- Where LLVM's time goes: ISel is 40% of LLVM back-end time on x86 (FastISel) and 52% on AArch64 (GlobalISel).
- In a Clang build, TPDE's back-end is about 1% of end-to-end time, and the end-to-end build speedup is 9-24%. With TPDE, the front-end dominates.

### 3.3 TPDE for Wasmtime/Cranelift IR (CLIF)
Size: 4.7 kLOC (0.7k arch-specific, 1.6k C++/Rust glue). No SIMD support.

CGO'26 final numbers:

| Comparison | Compile time | Runtime |
|---|---|---|
| vs Cranelift (Ion RA) | 4.94x faster | 1.58x slower |
| vs Cranelift with fast RA | 3.10x faster | 1.37x better (faster) |
| vs Winch | 1.53x slower | 1.19x better |

- 41% of TPDE-CLIF compile time is Wasm -> CLIF translation (Cranelift's frontend), not TPDE.
- arXiv v1 numbers differ:
  - compile 4.27x faster than Cranelift, 2.68x faster than Cranelift-fastRA, 1.74x slower than Winch;
  - runtime better than Winch by 1.14x and fastRA by 1.31x, 1.64x slower than Cranelift-Ion.
- Implication: TPDE-style RA on an IR already in memory is very cheap. Building the IR (translation) becomes the bottleneck, so a query engine should generate IR directly in the backend's input format.

### 3.4 Status (as of 2026-09)
- README: "10-20x faster than -O0" for typical code. Vector ops at -O2 "typically fail" (unsupported).
- Supports LLVM 19-22, with 21.1 preferred. Output is ELF only. Targets are x86-64 and AArch64. There is NO Mach-O/macOS support, which means the rudb dev box (M4, macOS) cannot use it natively.
- LLVM ORC integration (blog by S. Graenitz, 2025-09-30, https://weliveindetail.github.io/blog/post/2025/09/30/tpde-in-llvm-orc.html):
  - 329 ms vs 6796 ms for LLVM -O2 (about 20x) and 4060 ms for LLVM -O0 (about 12x);
  - ELF only;
  - falls back to LLVM for unsupported functions.
- Not merged into upstream LLVM.
- Rust: there is no Rust port of the TPDE framework.
  - rustc_codegen_tpde is a prototype, mentioned in rust-lang/rust-project-goals PR #791 and in Rust's 2026 "fast builds" roadmap [snippet].
  - That is a rustc backend via LLVM IR, not a library usable from a Rust DB.
- For rudb, reuse means either FFI to C++ TPDE with our own IR adapter (C++ templates, which is awkward from Rust) or re-implementing the design in Rust. The algorithms are compact and published; 3.3 kLOC for Umbra IR is the size reference.

---------------------------------------------------------------------------------------------------

## 4. Cranelift (2025-2026)

Sources:
- https://cranelift.dev
- https://github.com/bytecodealliance/wasmtime/tree/main/cranelift
- crates.io cranelift-jit 0.136.0 (released 2026-09-21)

### 4.1 Status
- Targets: x86-64, aarch64, s390x, riscv64. The codebase is about 200 kLOC.
- Pure Rust, Apache-2.0 WITH LLVM-exception. It is embeddable in rudb with no C++ toolchain.
- APIs:
  - `cranelift-frontend` (FunctionBuilder with SSA construction via use_var/def_var);
  - `cranelift-module`;
  - `cranelift-jit`: JITModule/JITBuilder with memory providers (Arena/System) and symbol lookup;
  - `cranelift-object` for AOT objects.
- cranelift.dev claims about 10x faster codegen than an LLVM-based system, with code about 2% slower than V8 TurboFan and about 14% slower than LLVM. The site attributes the numbers to the copy-and-patch paper, arXiv 2011.13127.
- CGO'24 (sec. 1.3) is the only rigorous DB-workload comparison. On it, Cranelift compiled only 20-35% faster than LLVM-cheap and gave slightly better code, and was about 16x slower to compile than DirectEmit.
- Exceptions support was added in November 2025. Unwind-info emission in cranelift-jit remains a sore point (CGO'24 complaint). [GK: check 0.136 docs]

### 4.2 Mid-end: aegraph
Source: Fallin, 2026-04-09, https://cfallin.org/blog/2026/04/09/aegraph/
- Cranelift's mid-end is an "acyclic e-graph" (aegraph): rewrite rules in ISLE, GVN, LICM, const-prop and alias analysis, with elaboration back into the CFG.
- Retrospective numbers:
  - about 2% faster generated code for about 7-8% more compile time, vs a classical pass pipeline;
  - average e-class size is 1.13 e-nodes, so there is little actual equality saturation;
  - "union" nodes are worth only about 0.1%.
- Takeaway: for query code, the mid-end is not where compile time or quality comes from. Most of the value is GVN/const-fold/LICM, which a query IR generator can do itself during emission.

### 4.3 Register allocation: regalloc2 (Ion) and fastalloc
- regalloc2 (2022, Ion-derived backtracking allocator): about 20% faster compiles than the old allocator and 10-20% faster code on high-pressure benchmarks. It remains the default.
- fastalloc / single-pass allocator (GSoC 2024, D. Sonuga; https://d-sonuga.netlify.app/gsoc/regalloc-iii/):
  - It is a reverse linear scan (SSRA-like).
  - The RA phase is about 6x faster, and Sightglass compile is 1.07-5x faster.
  - rustc: 0.3-18% instruction-count reduction.
  - Generated code is 1.06-7.50x slower.
  - Enable with `-Oregalloc-algorithm=single-pass` (setting `regalloc_algorithm`).
- fastalloc history: disabled due to bugs (PR #10554), re-enabled (PR #11533), then more bugs (#11544, #11850). Treat it as not production-hardened as of late 2025.
- TPDE-CLIF compiles 3.10x faster than Cranelift+fastalloc and produces 1.37x faster code (sec. 3.3). So even with fastalloc, Cranelift's fixed per-function pipeline cost (legalization, lowering via ISLE, VCode, emission, MachBuffer) stays several times above a single-pass design.

### 4.4 rustc_codegen_cranelift
- Rust Project Goals 2025H2, "Production-ready cranelift backend": https://rust-lang.github.io/rust-project-goals/2025h2/production-ready-cranelift.html
  - On larger projects (Zed, Tauri, hickory-dns), codegen time is about 20% lower, which gives about 5% faster clean builds.
- LWN 2024 (https://lwn.net/Articles/964735/):
  - a Cranelift self-build takes 29.6 s with Cranelift vs 37.5 s with LLVM (-20% wall time);
  - 125 vs 211 CPU-s.
- Still nightly-only. Unwinding support is a work item on Linux and macOS.

### 4.5 AArch64 and SIMD
- The AArch64 backend is mature (it runs Wasmtime production on AArch64; macOS aarch64 is supported, including Apple's MAP_JIT).
  - [GK] cranelift-jit on macOS aarch64 must deal with W^X. Check the version's memory provider for MAP_JIT and pthread_jit_write_protect_np use before relying on it.
- SIMD: CLIF has 128-bit vector types (i8x16 ... f64x2) that cover Wasm SIMD. There is no AVX-512 or SVE, and no 256-bit vectors.
  - For analytic kernels needing AVX2/AVX-512 (c7i) or SVE2 (Graviton3/4), Cranelift cannot express them natively. Use AOT Rust kernels called from JIT code.

### 4.6 Winch (Wasmtime baseline compiler)
- RFC: https://github.com/bytecodealliance/rfcs/blob/main/accepted/wasmtime-baseline-compilation.md
  - Baseline compilers typically compile 15-20x faster, with code 1.1-1.5x slower (RFC's survey figures).
  - Winch is a single pass over Wasm operators with a value stack. It has no IR and no complex RA, and reuses Cranelift's MacroAssembler/encoding layer.
- AArch64 is complete for Core Wasm since Wasmtime 35 (August 2025): https://bytecodealliance.org/articles/winch-aarch64-support
  - Later releases added Wasm SIMD on AArch64 and experimental exception handling.
  - Tiering docs: https://docs.wasmtime.dev/stability-tiers.html
- Wasmtime does NOT tier between Winch and Cranelift. A module is compiled entirely with one or the other.
- Relevance: it is a Rust existence proof that a small, single-pass baseline compiler for both x86-64 and AArch64, sharing an encoder with the optimizing tier, is maintainable. But Winch consumes Wasm, not a query IR.

---------------------------------------------------------------------------------------------------

## 5. Copy-and-patch

### 5.1 Original paper
Source: Xu & Kjolstad, "Copy-and-Patch Compilation", OOPSLA 2021. https://arxiv.org/abs/2011.13127 (DOI 10.1145/3485513)

Technique:
- Stencils are pre-compiled (via Clang/LLVM at build time) binary fragments with holes (relocations) for constants, jump targets and stack offsets.
- At runtime the compiler memcpys a stencil and patches the holes.
- Continuation-passing via GHC calling convention/tail calls lets register-passing between stencils avoid spills.

Database experiment (high-level language, TPC-H SF0.3, 8 queries):
- Compile time: two orders of magnitude faster than LLVM -O0 (up to 276x) and up to 1435x faster than LLVM at higher -O levels.
- Generated code: 14% faster than LLVM -O0 code; 22% / 25% / 24% slower than -O1 / -O2 / -O3; about 10x faster than an interpreter.
- MemSQL's compiler (as measured/estimated) takes 4.8-52x (avg 16.4x) longer than theirs, and up to 4.5 s per query.
- LLVM has about 1 ms fixed startup cost per compilation (module/context/TargetMachine creation). This is a floor that stencils avoid.
- A mem2reg-like pass gives up to 10% better runtime for 3x compile time.

WebAssembly experiment:
- Startup: 4.9-6.5x faster than V8 Liftoff and 12.7-18.5x faster than Wasmer SinglePass.
- Code: 39% (CoreMark) and 63% (PolyBench) faster than Liftoff's.

Stencil library sizes:
- Wasm: 1666 stencils (35 kB).
- High-level language: 98,831 stencils (17.5 MB). The combinatorial explosion over types, operand locations and register configurations is the main engineering cost.

### 5.2 CPython JIT (copy-and-patch in production)
- The JIT landed in 3.13 (experimental). Stencils are built by LLVM at CPython build time and templated from the uop trace (tier-2 IR).
- 3.13/3.14 were often slower than or equal to the interpreter.
- 3.15 alpha, "JIT on track" (https://blog.python.org/2026/03/jit-on-track/): about 11-12% faster on macOS AArch64 and 5-6% on x86-64 Linux, geomean over pyperformance. The per-benchmark range is -20% to +100%.
- 3.15 beta: 8-9% / 12-13% [snippet, Real Python].
- PEP 836 (July 2026) reports 4-12% geomean and proposes a method-based frontend [snippet].
- Lesson: copy-and-patch gives cheap codegen, but gains depend on what the stencils fuse. Without cross-stencil register allocation, gains over a good interpreter are modest. For rudb, the analogy is that copy-and-patch over vectorized primitives gives little. It must fuse whole tuple-at-a-time pipelines to win.

### 5.3 Deegen / LuaJIT Remake
- Source: Xu & Kjolstad, arXiv 2411.11469 (OOPSLA 2026 version) [snippet]
- Deegen generates the interpreter plus a copy-and-patch baseline JIT from bytecode semantics written in C++.
- The baseline JIT compiles 19.1M Lua bytecodes/s and emits 1.62 GiB/s of code, about 91 bytes per bytecode.
- Generated code runs 4.60x faster than PUC Lua 5.1 and is 33% slower than LuaJIT's optimizing trace JIT.

### 5.4 Copy-and-patch in query engines
- pgrust (Malis, Aug 2026; https://malisper.me/jit-compiling-code-in-5-us/):
  - Hand-written ARM64 stencils as u32 instruction words, compiling in about 5 µs.
  - The post only demonstrates regex compilation, not whole query plans.
  - Uses MAP_JIT, pthread_jit_write_protect_np and sys_icache_invalidate on macOS.
  - Targets Graviton4. Claims 30% higher throughput than PG 18.3 and 18.5% over ClickHouse on ClickBench [snippet: runtimewire].
- pg-copyjit (https://github.com/pinaraf/pg-copyjit): experimental PostgreSQL expression JIT. No published numbers.
- The Xu/Kjolstad TPC-H experiment (sec. 5.1) is the main DB datapoint.
- TPDE CGO'26 built a copy-and-patch comparison compiler: 19.56x faster compile than -O0, code 2.32x slower and 4.27x larger than TPDE's (sec. 3.2).

---------------------------------------------------------------------------------------------------

## 6. LLVM options (ORC, -O0, FastISel, GlobalISel), and other backends

### 6.1 LLVM
- ORC JIT (LLJIT/LLLazyJIT) with JITLink supports ELF, Mach-O and COFF on x86-64 and arm64, so it works on macOS arm64 [GK].
  - PostgreSQL's LLVM JIT uses LLVM (ORC) only, with inlining of bitcode for operators. Its high compile cost is well known (defaults: jit_above_cost=100000).
- Engelke, EuroLLVM 2025, "Faster LLVM back-end" (https://llvm.org/devmtg/2025-04/slides/technical_talk/engelke_faster.pdf):
  - LLVM 18 -> 20 back-end compile time improved by 18% (x86-64) and 13% (AArch64).
  - GlobalISel is still 47% slower than FastISel.
  - 15-20 passes run before ISel even at -O0.
  - Getting more than 10x would require a separate -O0 back-end, i.e. TPDE, not incremental work.
- Practical recipe for LLVM as an optimizing tier, from CGO'24:
  - cache the TargetMachine per thread;
  - small code model;
  - avoid i128 and intrinsics (FastISel fallbacks);
  - custom pass pipeline;
  - compile on a background thread.
  - Expect about 1 ms fixed startup [Xu/Kjolstad] and tens to hundreds of ms per large query.
- Rust bindings: inkwell / llvm-sys. They bring a heavy C++ dependency with version pinning to LLVM major releases [GK].

### 6.2 libgccjit
- GPL-3 (with runtime exception for the library only). Excluded by CGO'24 for licensing.
- Compile-speed class is like GCC (CGO'24: GCC via C takes 48.88 s vs LLVM-opt's 11.36 s, though that includes C parsing and process overhead).
- Not a candidate for rudb.

### 6.3 MIR (Vladimir Makarov)
- https://github.com/vnmakarov/mir plus Red Hat developer blog posts [snippet]
- A lightweight JIT with a MIR IR and a C-to-MIR compiler (c2m). Targets: x86-64, aarch64, ppc64, s390x, riscv64.
- Goals: compile 100x faster than GCC -O2 with at least 70% of its code speed.
- Reported: a sieve benchmark compiles in 80 µs, 180x faster than GCC -O2 (the GCC timings include process startup, so it's unfair). Code is about 6% slower. Across benchmarks, generated code reaches 91% of GCC -O2 performance.
- It is C with no Rust bindings of note, and a single maintainer.

### 6.4 QBE
- https://c9x.me/compile/ [GK]
- A small (about 10-15 kLOC C) SSA backend aiming at "70% of the performance of industrial compilers in 10% of the code".
- Targets amd64, arm64, riscv64. It emits assembly text, which needs an external assembler, so it is not suitable as an in-process JIT without writing an encoder.
- No DB benchmark exists (not in CGO'24).

### 6.5 Assemblers and encoders usable from Rust
- dynasm-rs [GK]:
  - A proc-macro assembler (DynASM port) for x64, aarch64 and riscv. Instructions are encoded at Rust compile time into templates with runtime-patched operands, so runtime emission is essentially memcpy+patch ("copy-and-patch at the assembler level").
  - Provides an `ExecutableBuffer` and `Assembler` with alter/commit (handles W^X by remapping).
  - A good substrate for a hand-written baseline tier on both ISAs.
- iced-x86 [GK]: a full x86/x64 encoder/decoder/formatter in Rust, with a code assembler API. x86 only; use it as a disassembler for debugging too.
- asmjit (C++) (https://github.com/asmjit/asmjit):
  - Supports X86/X64/AArch64 with Assembler, Builder and Compiler (with RA) emitters.
  - Used by Erlang BeamAsm (about 50% more Estones than the interpreter).
  - No published emitter throughput numbers were found; the repo has benchmarks in asmjit-testing/bench.
- Cranelift's `cranelift-assembler-x64` (the new x64 assembler crate in Wasmtime, 2025) and Winch's MacroAssembler are other Rust encoder options [GK: verify API stability].
- yaxpeax (decoders) is useful for disassembly in tests [GK].

### 6.6 Wasm as an IR (mutable, V8)
- mutable (Haffner & Dittrich, arXiv 2104.15098; plus later papers):
  - Generates Wasm per query and runs it in embedded V8.
  - Liftoff (baseline) compiles complex queries to machine code in < 1 ms. V8 then tiers up to TurboFan by hot-swapping (dynamic tier-up).
  - It gets tiering, OSR-free switching, sandboxing and portable (x86/ARM) codegen for free.
  - Cost: a V8 embedding in C++, Wasm's 32-bit memory model (memory64 now available), boundary crossings for runtime calls, and no access to the host's SIMD beyond Wasm SIMD128.
- Rust route: Wasmtime with Winch (baseline) and Cranelift (optimized). There is no automatic tier-up, so rudb would have to drive tier switching itself at morsel boundaries by compiling the same module twice.
- The copy-and-patch paper's Wasm compiler beats Liftoff at startup by 4.9-6.5x (sec. 5.1). Liftoff is not the floor.

### 6.7 Flounder IR / ReSQL (low-level IR close to x86)
- Funke & Teubner, PVLDB 14(12) 2021, https://vldb.org/pvldb/vol14/p2691-funke.pdf
- Flounder IR is a thin, x86-oriented IR with virtual registers and a lifetime-aware fast RA, translated to machine code via asmjit.
- TPC-H compile times range from 0.21 ms (Q6) to 1.71 ms (Q19), vs HyPer's 15-90 ms: 70.1x shorter on average and up to 101.1x.
- Synthetic queries compile 24.6x faster than LLVM -O0 and 60.9x faster than -O3, up to 283x.
- Execution is competitive with LLVM for TPC-H (paper figures).
- Only x86-64.

---------------------------------------------------------------------------------------------------

## 7. Summary table: compile speed vs code quality (DB-relevant measurements only)

| System | Workload | Compile speed | Code quality | Source |
|---|---|---|---|---|
| Umbra DirectEmit | TPC-DS, 6678 fns | 64 ms total (~10 µs/fn); 16x faster than Cranelift | 4.83 s vs LLVM-opt 4.12 s (+17%) | CGO'24 |
| Flying Start | TPC-H SF1 geomean | 108x faster than LLVM -O3 | 1.2x slower than -O3 | VLDBJ'21 |
| TPDE (Umbra IR) | TPC-DS SF1 | about DirectEmit | about DirectEmit | CGO'26 |
| TPDE-LLVM | SPEC int | 13.88x (x86) / 18.29x (A64) faster than -O0 | about -O0 (±9%) | CGO'26 |
| Cranelift | TPC-DS SF10 | 1.07 s (x86) / 0.61 s (A64) | 4.62 s / 16.37 s | CGO'24 |
| LLVM-cheap | TPC-DS SF10 | 1.63 s / 0.74 s | 5.23 s / 19.45 s | CGO'24 |
| LLVM-opt | TPC-DS SF10 | 11.36 s / 5.86 s | 4.12 s / 12.88 s | CGO'24 |
| Copy-and-patch | TPC-H SF0.3 | up to 276x faster than -O0 | 14% faster than -O0, 22-25% slower than -O1..3 | OOPSLA'21 |
| Flounder | TPC-H | 0.21-1.71 ms/query; 70.1x faster than HyPer | about LLVM (paper) | PVLDB'21 |
| HyPer LLVM | TPC-H/DS | 59 ms (Q1) ... 911 ms (largest DS) | baseline | ICDE'18 |
| Interpreter (Umbra) | TPC-DS SF10 | 0.03 s | 3.2x (x86) / 3.9x-5x (A64) slower | CGO'24 |
| mutable/V8 Liftoff | complex queries | < 1 ms | tier-up to TurboFan | arXiv 2104.15098 |

---------------------------------------------------------------------------------------------------

## 8. Tiering lessons from JS and JVM engines, and mapping to morsels

### 8.1 V8
Sources: https://v8.dev/blog/sparkplug, https://v8.dev/blog/maglev, https://v8.dev/blog/holiday-season-2023

Pipeline: Ignition (interpreter) -> Sparkplug (baseline) -> Maglev (mid-tier SSA) -> TurboFan/Turboshaft (optimizing).

- Sparkplug:
  - A single linear pass over bytecode ("a switch in a for loop"), with no IR and no RA beyond the interpreter's register file.
  - Frames are identical to the interpreter's, which makes OSR trivial in either direction.
  - Mostly emits calls to shared builtins.
  - Improved Speedometer by 5-10%.
- Maglev:
  - SSA CFG with feedback-driven specialization and a simple RA.
  - Compiles about 10x slower than Sparkplug and about 10x faster than TurboFan (the holiday-2023 post says roughly 20x slower / 10-100x faster).
  - Gains: JetStream +8.2%, Speedometer +6%.
- Tier-up thresholds (Intel blog): about 500 invocations to Maglev and about 6000 to TurboFan [snippet]. V8's thresholds are budget-based ("interrupt budget" scaled by bytecode size) [GK].
- Turboshaft (2023-2025) replaced TurboFan's sea-of-nodes backend with a CFG IR, roughly halving compile time for the backend [GK, no verified number].

### 8.2 JavaScriptCore
Source: https://webkit.org/blog/10308/speculation-in-javascriptcore/
- Four tiers: LLInt (interpreter), Baseline (template JIT), DFG (optimizing), FTL (B3 backend).
- Counting: each call adds 15 points and each loop iteration adds 1.
- Thresholds: LLInt -> Baseline at 500 points, Baseline -> DFG at 1000, DFG -> FTL at 100000.
- Relative compile cost: DFG is about 4x Baseline, and FTL about 6x DFG.
- Uses exponential backoff after deoptimization/reoptimization.
- OSR entry happens at loop headers. OSR exit needs full state-reconstruction metadata.

### 8.3 HotSpot (tiered compilation) [snippet]

| Tier | Invocation | Min invocation | Compile threshold | Back-edge |
|---|---|---|---|---|
| Tier 3 (C1 full-profile) | 200 | 100 | 2000 | 60000 |
| Tier 4 (C2) | 5000 | 600 | 15000 | 40000 |

- Tier-3 predicate: `i > Tier3InvocationThreshold || (i > Tier3MinInvocationThreshold && i + b > Tier3CompileThreshold)`.
- Thresholds are scaled by compile-queue length (feedback on compiler backlog) [GK].
- Graal (GraalVM, Truffle) partial evaluation plus a Graal optimizing compiler: high peak, high compile cost [GK].

### 8.4 Mapping to rudb morsel boundaries
- Language VMs need OSR because hot loops are unbounded and state lives in frames. A morsel-driven engine can switch tiers between morsels, with state in heap structures. No OSR metadata, deopt or frame mapping is needed (Kohn ICDE'18; CGO'24 says Umbra needs no advanced switching).
- Counters don't map directly. JS counts invocations and back-edges; a query engine knows tuple counts (cardinality estimates plus observed rates). Kohn's extrapolation (sec. 2) is a cost model rather than a counter, and fits better.
  - Suggested inputs: remaining tuples n, measured tuples/s at the current tier, worker count w, and predicted compile time (linear in IR size, calibrated per backend and ISA).
- Background compilation (as in JSC/V8/HotSpot concurrent compilers) is essential: workers keep processing morsels at the current tier while one thread compiles. That is the `(w-1)*r0*c` term.
- Backoff: JSC-style exponential backoff maps to "don't recompile a pipeline template that recently lost the race". It could be applied via the code cache (sec. 11).
- Speculation/deopt: JS tiers speculate on types. SQL is statically typed, but a query engine could speculate on data properties: no NULLs in a morsel, no overflow, dictionary codes, sortedness. That requires a guard plus fallback to the lower tier for that morsel. The same switching mechanism is reused per morsel.

---------------------------------------------------------------------------------------------------

## 9. AOT Rust kernels: monomorphization, code size, icache

[GK unless cited; no measured numbers found for DB kernels specifically]
- Approach A: vectorized interpreter (DuckDB/Velox style). Primitives are Rust generics monomorphized per (type x operator x nullability x selection-vector). Each primitive is a tight loop, auto-vectorized or with explicit `std::arch`/`core::simd`.
- The combinatorial blow-up is the copy-and-patch stencil problem again: 98,831 stencils / 17.5 MB for the high-level language in the OOPSLA'21 paper (sec. 5.1). Monomorphized kernels face the same explosion. Mitigations:
  - restrict type specialization to physical types (i8/i16/i32/i64/i128/f32/f64/string-view);
  - share code across logical types;
  - use dyn dispatch per vector (amortized over about 1-2K values).
- Code size and icache:
  - Tidy Tuples measured 2.4x larger code from Flying Start vs LLVM-opt, with equal LLC/branch-miss behavior (sec. 1.2). That is evidence that JIT'd code-size growth at this level does not hurt analytic queries.
  - TPDE's +16-22% size vs -O0 is similarly benign.
  - Large AOT kernel libraries mostly cost binary size and cold-start, not icache misses in the hot loop: one pipeline's working set is small.
- Hybrid: JIT'd pipeline code can call AOT Rust kernels (hashing, string ops, decimal arithmetic, SIMD filters, AVX-512/SVE paths selected at startup by CPU feature detection). This was CGO'24's approach for complex ops. Umbra calls runtime functions for complex operations; Cranelift needed custom CRC32 instructions because calls were too expensive in the hot loop.
- ISA-specific kernel dispatch: use `is_x86_feature_detected!` / `std::arch::is_aarch64_feature_detected!` with `#[target_feature(enable=...)]` multiversioning. Graviton3 has SVE (256-bit), Graviton4 SVE2 (128-bit), Apple M4 has no SVE for userland [GK: M4 has SME streaming mode, not general SVE]. c7i has AVX-512. c6a (Zen3) has AVX2 only.

---------------------------------------------------------------------------------------------------

## 10. Debugging and profiling JIT code

### 10.1 perf map files
- `/tmp/perf-<pid>.map` is a text file with one line per symbol: `START SIZE name` (hex start, hex size). perf reads it at report time.
- Simple, but gives no line info and the file must stay present after exit. Good enough for "which pipeline/operator is hot".

### 10.2 jitdump
- Spec: linux `tools/perf/Documentation/jitdump-specification.txt`.
- The file is `jit-<pid>.dump`. The writer must mmap it with PROT_EXEC so perf records an MMAP event marking the file.
- Record types: JIT_CODE_LOAD (code bytes plus name plus address), JIT_CODE_MOVE, JIT_CODE_DEBUG_INFO (address -> file:line), JIT_CODE_UNWINDING_INFO, JIT_CODE_CLOSE.
- Workflow: `perf record -k 1 ...` (monotonic clock), then `perf inject --jit -i perf.data -o perf.jit.data`. This creates per-function ELF .so files so annotate/disassembly works.
- References: Wasmtime `crates/jit-debug/src/perf_jitdump.rs` (Rust implementation); theunixzoo 2025 blog with a Rust example.
- Linux only. On macOS use Instruments or samply [GK: samply supports perf-map-style JIT symbol files on macOS].

### 10.3 GDB JIT interface [GK]
- The process exposes `__jit_debug_descriptor` and a no-inline function `__jit_debug_register_code()`. The JIT links a `jit_code_entry` pointing at an in-memory ELF object (with symbols, DWARF and unwind info), sets action_flag=JIT_REGISTER_FN, and calls the hook. GDB and LLDB set a breakpoint there.
- LLVM ORC has a GDB registration plugin (with JITLink `GDBJITDebugInfoRegistrationPlugin` for ELF and Mach-O).
- Wasmtime has `crates/jit-debug/src/gdb_jit_int.rs`.
- Cost: generating an ELF object per compiled pipeline. Do this only in debug mode.

### 10.4 Tailored Profiling (Umbra)
- Beischl, Kersten, Bandle, Giceva, Neumann, "Profiling Dataflow Systems on Multiple Abstraction Levels", EuroSys 2021. https://db.in.tum.de/~beischl/papers/Profiling_Dataflow_Systems_on_Multiple_Abstraction_Levels.pdf
- Mechanism:
  - A Tagging Dictionary is kept per lowering step (SQL operator -> Umbra IR -> machine instruction address), and an Abstraction Tracker attributes samples to operators.
  - Register Tagging stores the current operator/tuple-source id in a reserved register so samples can be attributed across shared runtime functions.
- Overhead:
  - PEBS sampling every 5000 cycles: 35%.
  - Adding register sampling: 38% (+3%).
  - Call-stack sampling: 529%.
  - Overall over TPC-H at normal sampling rates: 2.8%.
- Implementation cost in Umbra: 44 + 6 + 6 LOC, which is tiny.
- Follow-up: UmbraPerf won the VLDB 2025 Best Demo.
- For rudb: emit (code-address-range -> pipeline/operator id) tables from every backend tier from day one. That is cheap, and it feeds perf-map, jitdump and in-engine EXPLAIN ANALYZE per-operator cycle attribution.

---------------------------------------------------------------------------------------------------

## 11. Code caching

- Amazon Redshift compiles each query segment to C++, then to machine code, and caches it:
  - June 2020: serverless (off-cluster) compilation made compilation 2x faster, and an unlimited cache raised the hit rate from 99.60% to 99.95% (AWS blog).
  - July 2026: more than 99% of queries run on cached code, and the P50 compile time of cache misses was 4.3 s [snippet].
  - The cache is invalidated by version patches. Redshift Observatory measured an ETL job where 48 of 100 runs recompiled after a patch.
  - Lesson: with a slow compiler you need a cache and it becomes a failure mode. With a µs-ms baseline compiler, the cache is an optimization for the optimizing tier only.
- Plan templates: key the cache by a normalized pipeline IR hash, with constants parameterized.
  - Constants should be loaded from a parameter block rather than baked in. Otherwise the hit rate collapses on literal changes. The tradeoff is losing constant-folding.
  - HyPer/Umbra prepared statements and Redshift segments both work this way.
- Persisting compiled code across restarts:
  - Needs relocatable code, plus fixups for runtime-function addresses (ASLR) and CPU-feature keys (AVX-512 vs AVX2, SVE vs not) and compiler version.
  - cranelift-object / ELF objects plus a loader, or ORC with an object cache (`ObjectCache`).
- Cache the optimizing tier's (Cranelift/LLVM) result per template so that repeated TPC-C transactions and dashboard queries start at peak tier. TPC-C strongly favors this: few distinct statements, executed millions of times.

---------------------------------------------------------------------------------------------------

## 12. Security and platform: W^X, MAP_JIT, icache

### 12.1 macOS arm64 (dev box: M4)
[GK plus pgrust post; Apple docs page could not be fetched]
- Hardened-runtime processes need the `com.apple.security.cs.allow-jit` entitlement to use `MAP_JIT`. Unsigned CLI binaries run from a terminal are not hardened, so MAP_JIT works without the entitlement for local dev. Verify for the packaged app.
- Allocate with `mmap(..., PROT_READ|PROT_WRITE|PROT_EXEC, MAP_PRIVATE|MAP_ANON|MAP_JIT, ...)`.
- Apple Silicon enforces W^X per thread: `pthread_jit_write_protect_np(0)` makes MAP_JIT pages writable (not executable) for the calling thread, and `pthread_jit_write_protect_np(1)` flips them back to executable.
  - The toggle is per thread and cheap (APRR/SPRR register write, no syscall). Other threads can keep executing the same pages while one thread writes.
  - Good fit for a background compile thread.
- After writing code, call `sys_icache_invalidate(addr, len)` (from libkern/OSCacheControl.h) before executing.
- mprotect-based RW->RX flipping of non-MAP_JIT memory also works but is a syscall per flip.
- The pgrust post (sec. 5.4) uses exactly MAP_JIT + pthread_jit_write_protect_np + sys_icache_invalidate.

### 12.2 Linux x86-64 / AArch64 (servers)
- W^X by convention: mmap RW, write, then mprotect RX. Alternatively, double-map a memfd (one RW view, one RX view) to avoid mprotect TLB shootdowns under concurrency (as Wasmtime/V8 options do) [GK].
- AArch64 Linux requires explicit icache maintenance after writing code: `__builtin___clear_cache(begin, end)` (DC CVAU + IC IVAU + DSB/ISB). On Neoverse, `CTR_EL0.DIC/IDC` may make parts unnecessary [GK].
  - Cross-thread: other cores need an ISB (context synchronization) before executing newly written code. In practice this is ensured by the synchronization (release/acquire) publishing the function pointer, plus `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE)` if code is modified in place [GK].
  - x86 is coherent for code, so none of this is needed. That is a classic source of "works on x86, crashes on Graviton" bugs.
- mprotect on hot paths: batch pipelines into one code region per query (or per compile), and flip permissions once.
- Code memory reclamation: pipeline code for a finished query must be freed. Use a per-query arena, or refcounting when shared via the cache.
- Mach-O vs ELF matters for backends that emit object files:
  - TPDE (ELF only) cannot run in-process on macOS without its own loader; it can emit raw code into memory. [GK: check whether TPDE's JIT mapper is ELF-dependent]
  - Cranelift and dynasm-rs work on both.
- Cranelift JIT far-call issue (CGO'24): without GOT/PLT, calls to runtime functions beyond +/-2 GB (x86 rel32) or +/-128 MB (AArch64 BL) crash. Allocate JIT code near the binary, or call runtime functions via absolute-address indirect calls (load imm64 + blr/call reg).

---------------------------------------------------------------------------------------------------

## 13. Other notes and open questions
- Numbers on AArch64 are systematically worse relative to x86 for LLVM (GlobalISel). TPDE's speedup over LLVM is larger on AArch64 (18.29x vs 13.88x). A custom baseline backend pays off more on the Graviton/M4 side.
- CGO'24 AArch64 exec times (M1) are about 3x the x86 times. That reflects 4 cores vs 32 cores, not code quality.
- No published system implements a TPDE-style backend in Rust. That would be novel engineering, but Winch shows the encoder layer and ABI handling are achievable in Rust.
- DuckDB itself does not JIT (vectorized interpretation). To beat DuckDB by 10x, codegen alone is insufficient. Umbra vs DuckDB comparisons in the literature typically show about 1-3x, not 10x, on TPC-H [GK: see companion notes A/C]. Backend choice affects the latency floor (short queries, ClickBench's 43 queries) and TPC-C more than the long-running TPC-H throughput.
- Open: an actual measurement of Cranelift compile time on a DB-style IR at 2026 versions (0.136) on M4 and Graviton. CGO'24 used a 2023 version. Suggest a micro-benchmark in the spec phase: generate a TPC-H Q1/Q3/Q9 pipeline in CLIF, and measure µs per pipeline with Ion vs fastalloc.

---------------------------------------------------------------------------------------------------

## Backend recommendation inputs

### A. Decision-relevant facts (condensed)
1. A single-pass baseline is the proven latency floor for compiled query engines:
   - about 10 µs/function (DirectEmit, 64 ms for 6678 TPC-DS functions);
   - 0.21-1.71 ms per TPC-H query (Flounder);
   - < 1 ms (V8 Liftoff in mutable);
   - about 5 µs for tiny stencil programs (pgrust).
2. Code-quality cost of a good single-pass backend on DB workloads is about 1.1-1.2x vs optimized LLVM:
   - DirectEmit 4.83 vs 4.12 s;
   - Flying Start 1.2x;
   - worst cases about 1.4x (Q17).
   Memory-bound behavior (LLC/branch misses) is unchanged.
3. Cranelift sits in the "cheap LLVM" band for DB code. It is only 20-35% faster to compile than LLVM -O0/FastISel and about 16x slower than DirectEmit, with code about as good as DirectEmit. It is NOT a baseline tier. It is a reasonable, pure-Rust, portable optimizing-lite tier.
4. LLVM -O2 buys another 10-15% in aggregate (up to 38% per query) for about 10x Cranelift's compile time. It only pays off on large scale factors (TPC-H SF100 in CGO'24). It is optional and can be a later add-on via ORC (works on Mach-O and ELF).
5. TPDE is the state of the art for single-pass backends:
   - 13.9-18.3x faster than LLVM -O0 with the same code quality;
   - Umbra-IR adapter only 3.3 kLOC.
   But it is C++, ELF-only, with no Rust port, and SIMD at -O2 is unsupported. The design (one analysis pass, one fused codegen pass, greedy RA, snippet encoders) is re-implementable in Rust.
6. Copy-and-patch is simplest to build and compiles fastest, but code is 2.32x slower and 4.27x larger than TPDE (CGO'26 comparison). Stencil counts explode for rich type systems (98,831 stencils / 17.5 MB). CPython's experience shows limited gains without register allocation across stencils.
7. Interpreter vs baseline gap: 3.2x (x86) to about 4x (AArch64) (CGO'24). HyPer's VM is 4.1x slower than LLVM -O3. A vectorized AOT interpreter narrows this for scan/filter/agg-heavy queries [GK].
8. Tier switching at morsel boundaries needs no OSR. Kohn's extrapolation (remaining tuples, measured rate, compile-time estimate linear in IR size, w-1 workers continue) is the proven policy. Umbra now uses a simpler "after a few executions + size" heuristic.
9. Two ISAs:
   - LLVM's AArch64 -O0 path (GlobalISel) is 47% slower than FastISel.
   - A custom backend must implement x86-64 and AArch64 encoders plus two ABIs (SysV x86-64 and AAPCS64 with the Apple variant).
   - TPDE: 1.4k arch-specific LOC per target in TPDE-LLVM.
   - Winch AArch64 took about 1.5 years to reach parity (2024-04 tracking issue to Wasmtime 35, 2025-08).
10. Platform: macOS M4 needs MAP_JIT + pthread_jit_write_protect_np + sys_icache_invalidate. Linux AArch64 needs __clear_cache. x86 needs nothing for icache. Far-call range must be handled (AArch64 BL is +/-128 MB).
11. Observability must be built into the codegen API: perf-map (trivial), jitdump (Linux), GDB JIT (debug only), and Umbra-style tagging (44+6+6 LOC, 2.8% overhead).
12. Caching: with a µs-class baseline, a cache is optional for tier 1 but valuable for tier 2 and for TPC-C (Redshift: >99% hit, 4.3 s P50 miss cost with a slow compiler). Parameterize constants, and key by IR hash + CPU features + compiler version.

### B. Tradeoffs table

| Option | Compile latency (DB evidence) | Code quality vs LLVM-O2 | x86-64 + AArch64 | macOS (M4 dev) | Rust integration | Build effort / risk | Role for rudb |
|---|---|---|---|---|---|---|---|
| Vectorized AOT Rust kernels (monomorphized) | 0 (no compile) | ~3-4x slower than JIT on compute-bound pipelines [CGO'24 interp gap as proxy]; near-par on memory-bound scans [GK] | yes (per-ISA multiversioning, AVX-512/SVE possible) | yes | native | low-medium; code size grows with type x op matrix | Tier 0 + runtime library called from JIT code |
| Custom single-pass backend in Rust (TPDE/DirectEmit design) | ~10 µs/fn; about ms/query (DirectEmit, TPDE) | ~1.1-1.4x slower | must write both (TPDE: ~1.4k LOC/arch; DirectEmit 11 kLOC total) | yes (we control memory mapping) | native | high initial effort (RA, encoders, ABIs, unwind); no existing crate | Tier 1 (primary) |
| TPDE (C++) via FFI | same as above | same | yes | NO (ELF only) | poor (C++ template adapter) | medium; external dep; macOS blocker | Not recommended except as reference/benchmark |
| Copy-and-patch (stencils from Rust/LLVM at build time) | fastest (µs); up to 276x faster than -O0 | ~-O0 level; 2.32x slower than TPDE | yes (stencils per target) | yes | build.rs + stencil extraction; tricky with rustc (needs tail calls/GHC-cc) | medium; stencil explosion | Alternative Tier 1 if RA-backend too costly; or micro-JIT for expressions |
| dynasm-rs template JIT (hand-written per op) | µs | depends on templates; no global RA | x64 + aarch64 | yes | native | medium; two hand-written emitters | Substrate for custom Tier 1 encoders |
| Cranelift (cranelift-jit) | ~0.16 ms/fn avg (1.07 s / 6678 fns, x86); 16x slower than DirectEmit | ~1.12x slower (4.62 vs 4.12 s) | yes (+ s390x, riscv64) | yes | native, pure Rust | low; gaps: CRC32/overflow/wide-mul, 128-bit SIMD only, far calls, unwind | Tier 2 (optimizing-lite), or interim Tier 1 before custom backend |
| Cranelift + fastalloc | 1.07-5x faster than Ion (Sightglass) | 1.06-7.5x slower than Ion code | yes | yes | native | fastalloc had repeated correctness bugs (2025) | Not recommended yet |
| LLVM ORC -O0/FastISel | ~1.5x Cranelift (1.63 s) + ~1 ms fixed per module | ~1.27x slower | yes (AArch64 via slower GlobalISel) | yes (JITLink Mach-O) | llvm-sys/inkwell; heavy C++ dep, version pinning | medium | Not worth it (Cranelift dominates) |
| LLVM ORC -O2 | ~10x Cranelift (11.36 s) | 1.0 (reference) | yes | yes | as above | medium; large binary | Optional Tier 3 for long queries (SF100+) |
| Wasm + Wasmtime (Winch -> Cranelift) | Winch fast (TPDE-CLIF 1.53x slower than Winch); plus Wasm translation | Winch ~1.1-1.5x slower than Cranelift | yes | yes | native | medium; Wasm memory/call-boundary overhead; no auto tier-up | Not recommended (indirection with no benefit for trusted code) |
| MIR / QBE / libgccjit | MIR µs-ms [snippet]; QBE text asm; gcc slow | MIR ~0.9x GCC-O2 [snippet] | MIR yes; QBE yes (asm text) | partial | C FFI | single-maintainer / license (GPL for gccjit) | Not recommended |

### C. Suggested direction (input for the spec, not a decision)
- Tier 0: vectorized AOT Rust kernels (DuckDB-compatible semantics, also the correctness oracle). They double as the runtime library that JIT code calls for complex ops (strings, decimals, hashing, SIMD filters).
- Tier 1:
  - Start with Cranelift, to get a working compiled pipeline on both ISAs and macOS quickly.
  - Design the pipeline IR so a TPDE/DirectEmit-style single-pass Rust backend (dynasm-rs or custom encoders; greedy RA; Kohn liveness) can replace it later as the low-latency tier.
  - Budget: target about 10 µs per pipeline function, ~1.2x of optimized code.
- Tier 2: Cranelift (Ion) for long-running pipelines. LLVM-O2 via ORC only if benchmarks show the 10-15% matters (large SF TPC-H/JOB).
- Switching: morsel-boundary with Kohn-style extrapolation and a background compile thread. No OSR. Pipeline-state ABI shared across tiers.
- Cache: template cache keyed by (normalized IR hash, CPU features, compiler version) for Tier 2 and prepared statements (TPC-C).
- Platform layer, from day one:
  - W^X abstraction (MAP_JIT + per-thread toggle on macOS; RW->RX or dual-mapping on Linux);
  - icache flush on AArch64;
  - near-allocation or indirect calls for runtime functions;
  - perf-map + jitdump + operator tagging.
