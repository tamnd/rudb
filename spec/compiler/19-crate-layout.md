# 19. Crate layout

The compiled engine is eleven crates named `rudb-qc` or `rudb-qc-*`, one of them debug-only, plus a new router role for `rudb`. They are placed in the workspace's existing layer rule. This document says where each one sits, what it may depend on, where `unsafe` lives, and how the engine lives with the workspace's zero dependency rule. The rules here are checked by `cargo xtask layers`, the same way the rest of the tree is. They are not conventions.

Two facts about the tree as it stands at `97a146a2` shape everything below.

**The layer rule is numeric and strict.** `xtask/layers.toml` gives every crate a rank, and a crate may depend only on crates of strictly lower rank. Today `rudb-plan` is 9, `rudb-bind` 10, `rudb-opt` 11, `rudb-exec` 12 and `rudb` 13. Section 18.2 of `spec/18-package-layout.md` adds that nothing depends on `rudb-exec` except `rudb` and the tools.

**The engine does not depend on outside crates for anything it could write** (`spec/18-package-layout.md` section 18.5). The only exception is the allocator, and only in the shell. Cranelift is an outside crate, and so is LLVM. Section 19.4 is how the design survives that.

## 19.1 The crates

| Crate | Rank | Holds | Depends on (workspace) | `unsafe` |
|---|---|---|---|---|
| `rudb-qc-ir` | 2 | QIR types, arena, builder with append-time folding, printer, parser, verifier, the runtime function catalogue as data (ids, signatures, attributes) | `rudb-common` | forbidden |
| `rudb-qc-interp` | 3 | QIR to register bytecode lowering, the dispatch loop | `rudb-qc-ir` | only in the dispatch loop, documented per block |
| `rudb-qc-direct` | 3 | analysis pass, combined isel, register allocation and encoding pass, `aarch64` and `x86_64` modules, encoders, relocation records | `rudb-qc-ir` | forbidden |
| `rudb-qc-clif` | 3 | QIR to CLIF lowering; emits bytes and relocations, does not map memory | `rudb-qc-ir`; outside: `cranelift-codegen` | forbidden |
| `rudb-qc-llvm` | 3 | QIR to LLVM IR through the C API | `rudb-qc-ir`; outside: LLVM | FFI only |
| `rudb-qc-cdebug` | 3 | debug only, never in release builds: prints a QIR function as C11 for clang under ASan, UBSan and TSan (document 15) | `rudb-qc-ir` | none in the crate; the C it prints is outside Rust |
| `rudb-qc-rt` | 8 | the runtime library of document 13: hash table growth, string heaps, spilling, Bloom publish, decimal division, regex calls, `vcall` trampolines; the code arena and W^X platform layer; the loader that applies relocations | `rudb-qc-ir`, `rudb-vector`, `rudb-kernels`, `rudb-storage`, `rudb-graph`, `rudb-regex`, `rudb-functions` | yes: the platform layer, the loader, the trampolines |
| `rudb-qc-plan` | 12 | the physical planner of document 04: algorithm choice, reduction schedule, facts and guards, reservations | `rudb-plan`, `rudb-opt`, `rudb-stats`, `rudb-graph`, `rudb-catalog` | forbidden |
| `rudb-qc-pipe` | 13 | pipeline decomposition, the state layout, parallel instantiation (document 05) | `rudb-qc-plan`, `rudb-qc-ir` | forbidden |
| `rudb-qc-gen` | 14 | the translators of document 07, A6 to A7 | `rudb-qc-pipe`, `rudb-qc-ir` | forbidden |
| `rudb-qc` | 15 | the engine entry: acceptance check, tiering policy, code cache, the driver that runs pipeline functions on the shared pool, `EXPLAIN (CODEGEN)`; plus the developer binary `rudb-qc` with `run`, `diff`, `time`, `bisect` and `repro` (document 16) | `rudb-qc-gen`, the backends, `rudb-qc-rt`, `rudb-metrics` | calling generated code, one function |
| `rudb` | 16 (was 13) | the router between the two engines, and the C9 bridge | `rudb-exec`, `rudb-qc` | unchanged |

**`rudb` moves from rank 13 to 16, and that is the only rank that changes.** Nothing depends on `rudb` except the tools and the shell, and those are outside the rank table's numbered chain. The move is one line in `layers.toml`, and its comment should point here.

**The backends are at rank 3 and know nothing but QIR.** A backend takes a verified QIR function and a target description. It returns a byte buffer, a relocation list, a symbol map for observability and a size report. It does not map memory, does not know what a pipeline is, and cannot reach the runtime library except by catalogue id. That makes the backend replaceable, which is what a seam in the sense of `rudb-seam` needs, and it lets each backend be tested with QIR text files and nothing else. It also means `rudb-qc-direct` can be `forbid(unsafe_code)`. An encoder writes bytes into a `Vec<u8>`, and there is nothing unsafe about that.

**The runtime library sits at rank 8, below the catalog and the planner.** It has to reach the kernels (3), storage (5), graph links (5) and the function library (7), the last for `vcall`. It must not reach anything that knows about plans. Rank 8 is the lowest rank that satisfies both, and it shares the rank with `rudb-catalog` without depending on it.

**The generator does not depend on the runtime library.** It needs the signatures of runtime functions, not their code, and the signatures are data in `rudb-qc-ir`'s catalogue. Addresses are bound by the loader in `rudb-qc-rt` when code is installed. So the translators can be tested and fuzzed without linking the runtime, and a new runtime function is a catalogue row plus an implementation, with no change to the generator's dependencies.

**`rudb-qc-plan` is above `rudb-opt` on purpose.** The physical planner reads the rewritten logical plan (A4) and calls `rudb-opt`'s cardinality interface for estimates where no fact exists. It must not be called by `rudb-opt`. Document 03 section 3.2 puts the boundary at A4, and the rank makes that boundary a build fact.

## 19.2 What happens to `rudb-ir` and `rudb-jit`

Both are nine-line scaffolds at `97a146a2`. `rudb-ir` (rank 2) is described as "the typed SSA expression IR that all four execution tiers share", and `rudb-jit` (rank 4) as "fused kernels, Cranelift lowering and the compiled code cache". Both come from `../08-codegen.md`'s four-tier design, which document 00 replaces for the compiled engine.

**Both scaffolds are deleted when `rudb-qc-ir` lands at C1.** Keeping them would leave two crates whose descriptions promise an architecture the tree no longer has. Fused kernels in the first engine, if `../planner-v2/10-specialization.md` still wants them, belong in `rudb-kernels` or `rudb-exec`, not in a crate named for a JIT. The workspace manifest loses two lines and gains eleven.

The name `rudb-qc-ir` and not `rudb-ir` is deliberate. The first engine may yet grow an expression IR of its own, for fusing kernels, and the two should not be confused. QIR is a pipeline IR with loops, memory and calls. It is not an expression IR.

## 19.3 Where `unsafe` lives

The workspace default is `forbid(unsafe_code)`, with `undocumented_unsafe_blocks = "warn"` where it is allowed. The compiled engine keeps `unsafe` to three places, and each gets a module-level argument in the style of `crates/rudb-cli/src/heap.rs`.

1. **`rudb-qc-rt::platform`.** Mapping memory, `MAP_JIT` and `pthread_jit_write_protect_np` on macOS, `mprotect` or dual mapping on Linux, instruction-cache invalidation, freeing code once no thread can be executing it. Document 08 specifies the protocol.
2. **`rudb-qc-rt` runtime functions and trampolines.** Every function the generated code calls takes raw pointers, because it is `extern "C"`. Each one validates what it can in debug builds and documents what it trusts.
3. **`rudb-qc::driver::call`.** One function turns a code address into `extern "C" fn(*mut PipelineState, *const Morsel) -> Status` and calls it. Everything the generated code will touch is reserved before this call, per document 13.

`rudb-qc-interp`'s dispatch loop may use `unsafe` for unchecked register-file indexing. That is only allowed after the bytecode verifier has proven every index in range, and each block cites that proof. A build flag replaces every unchecked access with a checked one, and the differential suite runs under it nightly.

**Generated code is outside Rust's safety argument entirely.** Its safety comes from the QIR verifier (document 06), the bounds-check debug mode (document 15) and the tier differential. That is stated here so no one reads the absence of `unsafe` in `rudb-qc-gen` as a claim about the code it generates.

## 19.4 Living with the zero dependency rule

**The default build contains `interp` and `direct`, and nothing else.** Both are ours. That is consistent with document 00, where `direct` is the default backend, and it means the release binary has no outside code in its compiled engine.

**`clif` is behind the cargo feature `qc-clif`, which is off by default and on in CI and in `rudb-bench` builds.** Section 18.2 of the package layout allows features for optional functionality that never change an answer. A backend is exactly that: the tier differential proves `clif` and `direct` return bit-identical results. The feature brings in `cranelift-codegen`, `cranelift-frontend`, `regalloc2` and `target-lexicon`, and not `cranelift-jit` or `cranelift-module`. Loading goes through our own arena, so there is one W^X protocol in the tree, not two.

**`llvm` is behind `qc-llvm`, off everywhere except the C12 evaluation job.** It needs a system LLVM of a pinned major version. Per document 18, the crate is deleted if C12 does not justify it.

**What this costs.** Without `qc-clif`, the release build has no optimizing tier. Pipelines that run for seconds execute `direct` code, which the CGO 2024 measurement puts at about 4.83 s against Cranelift's 4.62 s on its TPC-DS workload, and LLVM -O2's 4.12 s (`research-notes/B-backends.md`). That is about 5% behind Cranelift and 17% behind LLVM -O2 on that workload. It is a real cost on SF100, and it is zero on JOB, which is where compile time dominates. Two ways out are left open in document 20:

- Grow `direct` a second, slower mode with a better register allocator for hot loops.
- Ask for a second exception to the zero dependency rule, as the allocator got one, argued on the same terms: a measured number, stated in the package layout document.

**The build-time budget.** The workspace budget is ten minutes for a clean release build. `direct` has to fit inside it. The target-specific code is about 1.5k lines per architecture, following TPDE, plus a shared core of a few thousand, which is small next to `rudb-kernels`' generated cross product. `qc-clif` adds Cranelift's build time only to the builds that enable it. CI measures both configurations.

**Tests may use outside crates as dev-dependencies**, for example a disassembler for encoder differential tests or a fuzzing harness. Those are not linked into the engine. Fuzzers go in the existing `fuzz/` workspace, which is on its own toolchain for the sanitizers.

## 19.5 The seam registration

`rudb-seam` defines a seam as a point where published designs disagree, with a trait, a registry, a reference implementation, at least two implementations, a policy and an `EXPLAIN` line. The compiled engine registers four:

| Seam | Implementations | Reference | Policy |
|---|---|---|---|
| backend | `interp`, `direct`, `clif`, `llvm` | `interp` | document 09's extrapolation |
| probe shape | fused, staged | fused | built size against LLC, document 10 |
| aggregation table | thread-local partitioned, global ticketed, dense array | thread-local partitioned | document 11 |
| reduction filter | exact bitmap, blocked Bloom, min-max, none | none | document 04's schedule, adaptive keep or drop |

Each is settable per session for ablation, which is how document 17's `SET qc_*` switches are implemented. Each one prints the choice in `EXPLAIN (CODEGEN)`, per pipeline.

## 19.6 Tests, fixtures and where they live

- **QIR text fixtures** live in `crates/rudb-qc-ir/tests/qir/`, one file per instruction group. Every backend crate runs all of them through itself and through `rudb-qc-interp`, and compares the results.
- **Encoder tests** live in the backend crate, against a disassembler dev-dependency: every encoding form, every register, every immediate class.
- **Translator tests** live in `rudb-qc-gen`, as golden QIR for small plans. The goldens are regenerated by one command and reviewed as a diff, as `EXPLAIN` goldens are today.
- **SQL-level differential tests** live in `rudb-compat`, not in this workspace, per document 15.
- **Benchmarks** live in `rudb-bench`, per document 17. There are no Criterion-style micro-benchmarks inside the engine crates. A number that matters is measured by the harness that reports it.

## What we should take from this document

The compiled engine fits the existing layer rule with one rank change: `rudb` moves from 13 to 16. The backends sit at the bottom and know only QIR. The runtime sits in the middle and knows data, not plans. The planner, pipeline builder and generator stack above `rudb-opt` in that order. The rank table makes document 03's boundaries into build facts.

The zero dependency rule decides the release shape: `interp` and `direct` by default, `clif` behind a feature that CI always turns on. That makes `direct` the backend the product actually ships, not just the default. It also makes the missing optimizing tier in release builds a stated cost with two named ways out, not a surprise found on SF100.

`unsafe` is confined to three places, all in the runtime and the driver. The emitter is safe Rust writing bytes. The generated code's safety argument lives in the verifier and the differential, not in the Rust type system, and that is said out loud.

The two scaffold crates from the four-tier design are deleted, not repurposed. Their names describe an architecture that document 00 replaces.
