# 08. Backends

How a QIR function (A7) becomes something a worker thread can call (A8). There are four backends: `interp`, `direct`, `clif`, and `llvm`. This document covers what each one is for, how each is built, the register and calling conventions shared by the two native emitters, the platform layer that turns bytes into executable memory, and the limits we accept.

Document 09 decides *which* backend runs a pipeline and when. This document decides what each backend *is*.

**The default build ships only `interp` and `direct`.** rudb has a zero-outside-dependency rule for the engine (`rudb/spec/18-package-layout.md` §18.5). Cargo features may add optional functionality but must never change answers (§18.2).

- `clif` sits behind the non-default cargo feature `qc-clif`. That feature is turned on in CI and bench builds.
- `llvm` sits behind `qc-llvm`.
- Disassemblers used by the encoder tests are dev-dependencies only.
- Every policy in document 09 must degrade to `interp → direct` when neither feature is compiled in.

Markers: `[snippet]` means the number came from a search snippet, not the primary text. `[derived]` means we computed it from cited numbers. `[GK]` means general knowledge that has not been re-verified against a primary source for this spec.

## 8.1 The four backends and their jobs

**Every backend takes the same QIR function and produces code with the same entry signature: `extern "C" fn(state: *mut PipelineState, morsel: *const Morsel) -> Status`.** A pipeline can therefore switch backends between two morsels by swapping one pointer (document 09, section 9.5). No backend is allowed a private state layout. The `PipelineState` layout is fixed in QIR (document 05) before any backend sees the function.

| Backend | Crate | Role | Compile cost target | Code quality target |
|---|---|---|---|---|
| `interp` | `rudb-qc-interp` | Reference semantics. Tier 0 for statements that cannot amortize anything. First backend to exist (C1). | ≤5 µs per function | ≤4x slower than `direct` |
| `direct` | `rudb-qc-direct` | The default. Single-pass native emitter for AArch64 and x86-64 (C3, C4). | ≤10 µs per function | within 1.25x of `clif` |
| `clif` | `rudb-qc-clif` (feature `qc-clif`) | Stepping stone before `direct` exists (C2), then the optimizing tier for long pipelines when the feature is on. | ≤200 µs per function | the best code we ship by default |
| `llvm` | `rudb-qc-llvm` (feature `qc-llvm`) | Off by default. Evaluated at C12 only. | ms per function | whatever LLVM gives |

The targets in the table are ours. They are set from the published measurements in 8.2 and are checked by gate G1 in document 02, section 2.8.

**`direct` is the default because of arithmetic, not taste.** A JOB query has 5 to 18 pipeline functions (document 02). The per-query compile budget is 0.5 ms at the median. At Cranelift's measured rate, 18 functions cost about 2.9 ms, which is over budget before any other work. At DirectEmit's rate, they cost under 0.2 ms.

## 8.2 The evidence

**Compile time and run time for the same 6,678 TPC-DS SF10 functions under six backends.** The source is Engelke & Schwarz, CGO 2024, Table III (https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf). The machines were a Xeon Gold 6338 and an Apple M1 running Asahi Linux.

| Backend | x86 compile | x86 execute | M1 compile | M1 execute | x86 µs per fn `[derived]` |
|---|---|---|---|---|---|
| Umbra interpreter | 0.03 s | 15.40 s | 0.02 s | 64.55 s | 4.5 |
| DirectEmit | 0.06 s | 4.83 s | n/a | n/a | 9 |
| Cranelift | 1.07 s | 4.62 s | 0.61 s | 16.37 s | 160 |
| LLVM cheap | 1.63 s | 5.23 s | 0.74 s | 19.45 s | 244 |
| LLVM optimized | 11.36 s | 4.12 s | 5.86 s | 12.88 s | 1,701 |
| GCC | 48.88 s | 4.28 s | 41.64 s | 13.99 s | 7,320 |

What we derive from that table, all `[derived]`:

- **The interpreter runs 3.19x slower than DirectEmit on x86** (15.40 / 4.83). On M1 it runs 3.94x slower than Cranelift (64.55 / 16.37).
- **Cranelift runs 4.3% faster than DirectEmit on x86** (4.62 vs 4.83), and it pays 18x the compile time for that.
- **LLVM-opt runs 17% faster than DirectEmit** (4.12 vs 4.83), and it pays 190x the compile time.
- **On M1 the gap between Cranelift and LLVM-opt is wider:** 1.27x, against 1.12x on x86. Cranelift's AArch64 backend is weaker relative to LLVM than its x86 backend is.

Other results that constrain the design:

- **Flying Start (Umbra's first single-pass backend).** It compiles 108x faster than LLVM -O3, and its code runs 1.2x slower. HyPer's bytecode VM compiled 91x faster than LLVM and ran 4.1x slower. Source: Kersten, Leis & Neumann, VLDBJ 30:883-905, 2021 (https://doi.org/10.1007/s00778-020-00643-4).
  - On a 2,000-join query, LLVM-opt took 150 s, LLVM-cheap took 4 s, and Flying Start took under 0.04 s.
  - Register allocation cut execution time by 32% compared with keeping every value on the stack. Linear scan gained 1% more execution speed for 14% more compile time, so it was rejected.
  - Against LLVM, Flying Start's code runs 1.6x more cycles and 2.3x more instructions, and it is 2.4x larger. Branch misses and LLC misses are about equal. **The gap is instruction count, not memory behavior.** For JOB, which is dominated by hash probes and cache misses (document 10), that is the right gap to accept.
- **TPDE.** Schwarz, Kamm & Engelke, CGO 2026 (https://home.cit.tum.de/~engelke/pubs/2602-cgo1.pdf, arXiv 2505.22610, https://github.com/tpde2/tpde).
  - TPDE-LLVM compiles 13.88x faster than LLVM -O0 on x86 and 18.29x faster on AArch64. Runtime is within ±9%.
  - The framework is 7.7 kLOC in total, of which 1.4k is architecture-specific.
  - Fusing instruction selection, register allocation and encoding into one pass is worth 8% of compile time.
  - TPDE's Umbra backend is 3.3 kLOC (1.4k target-specific) and matches DirectEmit on TPC-DS SF1. DirectEmit itself is 11 kLOC for AArch64 plus x86.
  - It supports ELF only: no Mach-O, and no Rust port.
- **Copy-and-patch.** Xu & Kjolstad, OOPSLA 2021 (https://arxiv.org/abs/2011.13127).
  - Up to 276x faster compilation than -O0. The code is 14% faster than -O0 and 22-25% slower than -O1 to -O3.
  - It needs 98,831 stencils totalling 17.5 MB.
  - TPDE's comparison compiler built on copy-and-patch compiled 19.56x faster than -O0, but its code was 2.32x slower and 4.27x larger. **We do not use copy-and-patch.** A register-allocating single-pass emitter compiles at the same order of speed and produces much better code.
- **Kohn's adaptive execution.** ICDE 2018 (https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf).
  - TPC-H Q1 took 59 ms to compile with LLVM. A pg_catalog query took 54 ms to compile against under 1 ms of execution.
  - LLVM's own interpreter, `lli`, was over 800x slower than compiled code. Its custom VM, about 800 lines, was not.
- **The cost of skipping register allocation.** TPDE-CLIF against Cranelift's backtracking allocator (Ion): 4.94x faster compile, 1.58x slower code. Against Cranelift's fastalloc: 3.10x faster compile and 1.37x *better* code. Against Winch: 1.53x slower compile.

**Conclusion.** The only design that fits the 0.5 ms budget and stays within about 20% of an optimizing compiler is a register-allocating single-pass emitter. The literature has built it three times: Flying Start, DirectEmit, and TPDE. Nobody has built it in Rust, and nobody has built it for Mach-O, so `direct` is ours to write.

## 8.3 The backend contract

```rust
pub trait Backend {
    const KIND: BackendKind;                 // Interp | Direct | Clif | Llvm
    fn compile(&self, f: &qir::Function, env: &CompileEnv) -> Result<CodeHandle, CompileError>;
}

pub struct CompileEnv<'a> {
    pub target: TargetDesc,                  // arch, CPU feature bits, OS
    pub runtime: &'a RuntimeSymbols,         // runtime entry points, document 13
    pub arena: &'a CodeArena,                // section 8.8
    pub limits: CodeLimits,                  // section 8.10
}
```

These rules bind every backend, and document 15 tests each one:

1. **Bit-identical results.** No reassociation. No FMA contraction. No fast-math flags. No change of rounding mode. Integer overflow, division by zero and cast failure are detected exactly where QIR says. The interpreter is the definition, and the native backends must match it.
2. **Status codes, never unwinding.** Generated code returns `Status` (Ok, Deopt, Error, Yield). Error details go into the per-thread error slot (document 13). Every call from generated code into Rust goes through an `extern "C"` function that catches panics at the boundary. No backend emits unwind tables.
3. **No allocation.** Generated code calls runtime functions that may allocate from reservations made at plan time, but it never calls the system allocator directly.
4. **Refusal is allowed; wrong code is not.** A backend may return `CompileError::Unsupported(op)`, and the caller falls back to the next *compiled-in* backend down the list `llvm → clif → direct → interp`. The interpreter supports every QIR instruction. A native backend missing an opcode is a coverage bug, not a correctness bug.
5. **Compile is a pure function** of (QIR function, `TargetDesc`, rudb build). This is what makes the code cache in document 09 sound.

## 8.4 `interp`

**A register-based bytecode, lowered from QIR in one linear pass, run by a `match` loop with typed opcodes and superinstructions.**

- **Frame.** Each QIR value gets a slot in a `[u64]` frame, found by index. A 128-bit value (decimals and hashes up to 128 bits) takes two adjacent slots. A string view (16 bytes, document 12) also takes two.
  - Slot numbers come from the same liveness pass that `direct` uses (8.5.1), so dead values reuse slots.
  - The frame for a pipeline function is allocated once per worker in `PipelineState`, not per morsel.
- **Instructions.** Three-address, with 16-bit operands: `AddI64Chk dst, a, b, err_block`. Opcodes are fully typed, so dispatch never branches on type.
- **Superinstructions** cover the five sequences that dominate pipeline loops:
  - compare-and-branch;
  - load-column-at-index;
  - hash-combine;
  - bounds-checked `vcall` argument packing;
  - loop back-edge with a morsel-limit check.

  The list is chosen from dynamic opcode-pair counts over JOB and TPC-H at C1, and each entry must cite its count.
- **Dispatch.** A `loop { match op { … } }` over a `&[Insn]`. Explicit tail calls in Rust (`become`) are not stable as of September 2026 `[GK]`, so direct threading is not portable. If `become` stabilizes, we switch to one handler function per opcode with tail calls, and we measure the change before merging it.
- **`vcall` and precompiled kernels are shared** with the native backends, so the interpreter spends its time in the same scan kernels. That is why a factor of 3.2-4x (8.2) is plausible even with a plain `match` loop: much of the work in a scan pipeline is not bytecode.
- **Size target: ≤3 kLOC.** HyPer's VM was about 800 lines (Kohn 2018). Ours has more types.
- **Entry.** One shared trampoline, `interp_entry(state, morsel)`, with the ABI signature. The bytecode pointer lives in `PipelineState`, so the interpreter swaps in and out like any native backend.

**It is also the oracle.** The tier-diff harness (document 15) runs every query on `interp` and on each native backend and requires byte-identical results. ClickHouse shipped wrong Int128 results from the fourth execution onward, once an expression was JIT-compiled and cached (issue #118334, https://github.com/ClickHouse/ClickHouse/issues/118334, `[snippet]`). That is the bug class the oracle exists for.

## 8.5 `direct`

**Two passes over each QIR function: one analysis pass and one codegen pass that fuses instruction selection, register allocation and encoding.** This is the TPDE structure, written in Rust against QIR rather than LLVM IR, with our own encoders. The target is about 1.5 kLOC per architecture plus about 3 kLOC shared. That is between TPDE-Umbra (3.3 kLOC) and DirectEmit (11 kLOC).

### 8.5.1 Analysis pass

The steps follow TPDE; QIR is already in SSA form with block parameters (document 06).

1. **Loop detection** (Wei et al., as in TPDE), producing a loop tree and a nesting depth per block.
2. **Block layout** in reverse post-order, keeping each loop body contiguous. The layout is final: codegen emits blocks in this order, and fall-through is decided here.
3. **Liveness in the style of Kohn.** For each value, record the first and last block of its live range in layout order. When a value is live into a loop, extend its range to the end of that loop. Also record a use count.
   - There is no per-instruction interval. A value dies when its use count reaches zero in its last block.
   - The cost is one pass over the uses. In DirectEmit, liveness was about 75% of analysis time (CGO 2024).

The output is three arrays indexed by value number (range start, range end, use count) and one per block (layout index, loop depth, loop end). No hash maps are allowed on this path.

### 8.5.2 Value assignments and register allocation

Each live value has a 16-byte assignment record, as in TPDE:

```rust
#[repr(C)]
struct Assignment {        // 16 bytes
    reg: u8,               // physical register, or NONE
    reg2: u8,              // second half of an i128 / string view, or NONE
    flags: u8,             // IN_REG | ON_STACK | CLEAN (stack copy current) | CONST | FIXED
    size: u8,              // bytes
    stack_off: i32,        // frame slot, allocated on first spill
    uses_left: u32,
    const_idx: u32,        // constant pool index when CONST (rematerialize, never spill)
}
```

The allocation rules:

- **Greedy allocation.** A result gets a free register of its class. If none is free, evict a victim in this order: a CONST value (it can be rematerialized); then a CLEAN value (it already has a stack copy); then the value in the current block whose next use is furthest away, found by a short look-ahead (at most 16 instructions) within the block. The TPDE paper spills at block boundaries without look-ahead. The look-ahead is our addition, and it must pay for itself in measurement at C3 or it is removed.
- **Block boundaries.**
  - An edge to a block with one predecessor that falls through in layout keeps register assignments.
  - An edge to a join block moves block parameters into their assigned homes (parallel-move resolution with one scratch register), and spills values that stay live, unless they are pinned (next bullet).
- **Pinning in the innermost loop.** Up to N values that are live throughout the innermost loop and used inside it are pinned to callee-saved registers for the loop's duration. N is 4 on x86-64 and 8 on AArch64. Pinned values never spill inside the loop. This is where Flying Start's 32% comes from: the loop-carried cursor, the morsel limit, the hash table base and the aggregate accumulators stay in registers.
- **Fixed registers.** Two callee-saved registers hold `state` and the morsel cursor for the whole function (table below). They are never allocated.
- **No linear scan and no graph coloring.** Flying Start measured +1% execution for +14% compile. A better allocator is `clif`'s job.

**Registers, by architecture.**

| Use | x86-64 (System V) | AArch64 (AAPCS64, Apple and Linux) |
|---|---|---|
| `state` (fixed) | `r15` | `x28` |
| morsel cursor / base (fixed) | `r14` | `x27` |
| pinnable loop values | `rbx`, `r12`, `r13`, `rbp`* | `x19`-`x26` |
| allocatable caller-saved | `rax rcx rdx rsi rdi r8-r11` | `x0`-`x15` |
| scratch for veneers and far calls | `r11` (reserved at call sites only) | `x16`, `x17` (IP0, IP1) |
| never touched | `rsp` | `x18` (platform register on Apple), `sp`, `x29` frame pointer, `x30` link register |
| float / 128-bit | `xmm0`-`xmm15` (all caller-saved) | `v0`-`v7`, `v16`-`v31` caller-saved; `v8`-`v15` low halves callee-saved |

\* `rbp` is pinnable only in functions that omit the frame pointer. The default is to keep frame pointers on every target, because document 16's profilers need them and Apple's ABI requires them `[GK]`. So in practice x86-64 has three pinnable registers.

**Apple's AAPCS64 variant matters in exactly two places.** `x18` is reserved, and stack-passed arguments are packed to their natural size instead of 8 bytes `[GK]`. We remove the second difference by rule: **a runtime function callable from generated code takes at most 6 integer and 4 floating-point arguments, all in registers.** Document 13 enforces the rule on its ABI table. With it, the call lowering is identical on macOS and Linux.

### 8.5.3 Instruction selection

These are pattern rules applied while emitting, with no pattern-matching pass. Each rule looks at the current instruction and, at most, the operand's defining instruction when that operand has exactly one use.

| QIR pattern | x86-64 | AArch64 |
|---|---|---|
| `load (add base (mul idx k))`, k ∈ {1,2,4,8} | `mov r, [base+idx*k+disp]` | `ldr r, [base, idx, lsl #log2 k]` |
| `icmp` whose only use is `br` | `cmp; jcc` | `cmp; b.cond`, or `cbz`/`cbnz` against zero |
| test one bit, then branch | `test r, imm; jcc` | `tbz`/`tbnz` (±32 KB range: backward targets only, see 8.5.5) |
| `add.chk` / `sub.chk` i64 | `add; jo err` | `adds; b.vs err` |
| `mul.chk` i64 | `imul; jo err` | `mul; smulh; cmp hi, lo, asr #63; b.ne err` |
| i128 add / sub | `add; adc` / `sub; sbb` | `adds; adc` / `subs; sbc` |
| i128 multiply (decimal) | `mul` (rdx:rax), cross terms, overflow check | `mul; umulh`, cross terms, overflow check |
| `select` | `cmov` | `csel` |
| hash step (document 07's function) | `crc32 r64` (SSE4.2), `imul`, `rorx`/`ror` | `crc32cx` (ARMv8.1 CRC), `mul`, `ror` |
| null bit test | `bt` + `jc` | `tbnz` on a loaded word |

- **Error branches go to one cold block per error kind per function.** That block stores the error code and operand values into the error slot and returns `Status::Error`. The error blocks are laid out after all hot blocks.
- **i128 is always two 64-bit registers.** Umbra's LLVM backend did the same for speed (CGO 2024). Decimal division is a runtime call (document 12).

### 8.5.4 Calls

- **Calls into the runtime library use a per-chunk literal table, not direct relative calls.**
  - On x86-64: `call qword ptr [rip + disp32]`, which is 6 bytes.
  - On AArch64: `ldr x16, <literal>; blr x16`. The literal has ±1 MB range, and the table sits at the end of the function's code chunk.
- **Why not relative calls.** x86 `call rel32` reaches ±2 GB. AArch64 `bl` reaches ±128 MB. Neither is guaranteed to reach the rudb binary from an mmap'd region under ASLR `[GK]`. cranelift-jit's lack of GOT/PLT and its resulting far-call crashes are documented in CGO 2024.
- **Cost of the choice.** One extra load per call, which is predicted and cached. Calls are rare in hot loops by construction, because the cogwheel split in document 03 keeps them out.
- **Calls between generated functions** go through the same table. Each generated function is a separate entry point, and inter-function calls are rare (comparators, document 11).
- **Stack alignment.** The frame is 16-byte aligned at every call. There is no red zone on either target, because Apple forbids one on arm64 `[GK]` and we keep the lowering uniform.

### 8.5.5 Encoders, fixups, code buffer

**We write our own table-driven encoders for the roughly 150 instruction forms each architecture needs.**

- Each form is a `const` entry: opcode template, operand kinds and field positions. The encoder is a function from (form, operands) to bytes, with no allocation.
- **Why not dynasm-rs or iced-x86 on the hot path.** We need exact instruction lengths while emitting, no macro-time assembly, and one table format for both architectures. `iced-x86` and `yaxpeax` are dev-dependencies for testing.
- **Differential encoder test** (runs in CI on every commit):
  - For every form, generate random legal operands.
  - Encode them, then decode with `yaxpeax-x86` / `yaxpeax-arm` and, on x86, also `iced-x86`.
  - Compare the decoded operands with the input.
  - Also: exhaustive register sweeps for every form, and every immediate boundary (0, ±1, the encoding limits, and limit ±1).
  - **An encoder change without a green differential run does not merge.**

**Fixups.**

- Forward branches are emitted in their long form and patched when the target block is placed: `jcc rel32` on x86, and `b.cond` (±1 MB) on AArch64.
- Backward branches know their distance, so they use the short form when it fits (`jcc rel8`, `tbz`).
- **One function's code is capped at 1 MB,** so that `b.cond` always reaches. The code generator splits a pipeline that would exceed it (document 07). In practice we expect functions to be far smaller (8.10).

**The code buffer** is a `Vec<u8>` grown by doubling and reused across compilations on the same thread. The finished bytes are copied once into the code arena (8.8). The two exceptions are macOS, where we emit directly into `MAP_JIT` memory, and Linux in dual-map mode, where we emit through the RW view. Both avoid the copy.

### 8.5.6 Prologue, epilogue, frame

- The **prologue** pushes the callee-saved registers the function actually uses. Codegen is single-pass, so the used set is known only at the end. We handle this by reserving the maximum prologue size at the entry and patching it at the end, filling unused bytes with a jump over them (x86) or starting execution at an offset (AArch64).
- The **frame size** is also patched at the end: the spill slots used, rounded to 16 bytes.
- Every function **ends with one epilogue block**, and all `ret` paths jump to it. That gives one place to restore registers and one place for document 16's exit counter.

### 8.5.7 What `direct` does not do

- No inlining. The only inlining is what QIR already did, in the generator.
- No loop-invariant code motion. The generator hoists invariants explicitly in QIR (document 07).
- No SIMD at all in the first version (8.9).
- No instruction scheduling. Out-of-order cores hide most of it, and Flying Start's residual gap is instruction count, not scheduling.
- No peephole pass after emission.

**Every one of these is `clif`'s job, or the generator's.**

## 8.6 `clif`

**`clif` exists only in builds with the `qc-clif` feature.** It is on in CI (tier-diff needs a second optimizing opinion) and in bench builds. It is off in the default build, so the default build has no tier above `direct`. Section 8.13 records that as an open question.

**Cranelift is used as a library, not as a JIT.** We use `cranelift-codegen` and `cranelift-frontend`, pinned to one version per rudb release (0.136.0 was released 2026-09-21). We do not use `cranelift-jit` or `cranelift-module`.

- **Lowering.** QIR lowers to CLIF one instruction at a time through `FunctionBuilder`. QIR block parameters map to CLIF block parameters one-to-one. There is no second IR in between.
- **Settings.**
  - `opt_level = "speed"`.
  - The Ion backtracking register allocator.
  - aegraphs on, the default. They cost 7-8% more compile time for about 2% faster code (https://cfallin.org/blog/2026/04/09/aegraph/).
  - `preserve_frame_pointers = true`.
  - `enable_verifier` in debug and test builds only.
- **Not fastalloc.** It is 1.07-5x faster to compile but produces 1.06-7.50x slower code (https://d-sonuga.netlify.app/gsoc/regalloc-iii/), and its bug history includes PRs #10554, #11533, #11544 and #11850. The fast-compile niche belongs to `direct`. A second fast tier with worse code and more bugs has no job.
- **Linking.** Cranelift emits bytes and relocations, in the same style as `cranelift-object`, and we map them into our own arena in `rudb-qc-rt`. From `MachBufferFinalized` we take the bytes and relocations and resolve them ourselves against the same per-chunk literal table as `direct` (8.5.4). This removes both cranelift-jit problems at once: far calls, and the missing GOT/PLT.
- **Missing operations.** CGO 2024 reports that Umbra had to add CRC32 to Cranelift (+19% on TPC-DS, +9% on TPC-H), plus overflow-checked arithmetic and wide multiply.
  - Current Cranelift has `sadd_overflow`/`smul_overflow` style instructions and `umulhi`/`smulhi` `[GK]`. At C2 we verify each one and use it.
  - CRC32 has no generic instruction. We carry a **vendored patch of at most 500 lines** that adds target-specific CRC32 lowering on x86-64 and AArch64. It is rebased with each Cranelift upgrade. If the patch becomes a maintenance problem, the fallback is to compute the hash with generic `imul`/`rotl`, and `clif` then loses the hash speed that `direct` has.
- **Expected speed**, all `[derived]` from 8.2:
  - Compile: about 160 µs per function on x86 and about 90 µs on M1.
  - Code about 4% faster than `direct` on x86 on average.
  - The speed gain is concentrated in loop-heavy functions with many live values. On TPC-DS Q17, LLVM-opt ran 0.93 s against DirectEmit's 1.29 s. **That is the case `clif` exists for,** and document 09 is the one that decides when it applies.
- **SIMD.** Cranelift's vectors are 128-bit only: no AVX-512 and no SVE. See 8.9.

## 8.7 `llvm`

**Behind the `qc-llvm` cargo feature, off by default, and not on any milestone gate before C12.**

- **Implementation:** ORC `LLJIT` with our own memory manager in the code arena. One `TargetMachine` is cached per thread. The code model is small-PIC. i128 is lowered as two i64 in QIR-to-LLVM translation. All of these follow Umbra (CGO 2024).
- **Pipeline:** a fixed, short pass list, not `-O2`. Its contents are decided from profiles at C12.
- **Justification bar.** Umbra's measurements show LLVM-opt paying off only at TPC-H SF100 (CGO 2024). We enable `llvm` in release builds only if the C12 evaluation shows **more than 10% end-to-end on long SF100 pipelines** without breaking G1 on any suite. If it does not, the crate stays behind the feature as an experiment.
- **Cost we accept by having it at all.** A pinned LLVM version, much longer rudb build times, and a large binary when the feature is on. None of this applies to default builds.
- **A TPDE-LLVM option** (ORC integration: 329 ms vs 6,796 ms at -O2 and 4,060 ms at -O0, https://weliveindetail.github.io/blog/post/2025/09/30/tpde-in-llvm-orc.html) is not interesting to us. It is a fast *LLVM IR* compiler, and we already have a fast *QIR* compiler.

## 8.8 Platform layer: executable memory

**All executable memory comes from one `CodeArena` in `rudb-qc-rt`.**

- The arena maps 1 MB chunks and hands out 4 KB-aligned extents by bump allocation.
- Freed extents go to per-size-class free lists.
- Empty chunks are returned to the OS after one epoch (below).

| Platform | Mapping | Write / execute switch | Instruction cache |
|---|---|---|---|
| macOS arm64 | `mmap(MAP_JIT)`; entitlement `com.apple.security.cs.allow-jit` under the hardened runtime | `pthread_jit_write_protect_np(0)` → emit → `pthread_jit_write_protect_np(1)`. The switch is per thread, cheap, and needs no syscall. | `sys_icache_invalidate(ptr, len)` |
| Linux x86-64 | dual mapping of one `memfd`: RW view for the emitter, RX view for execution | none needed | coherent; nothing to do |
| Linux AArch64 | same dual mapping | none needed | `__builtin___clear_cache` on the RX range; consumers issue an `isb` (below) |
| strict mode, any Linux (`qc.jit_strict_wx`) | single mapping | `mprotect` RW → RX per extent | as above |

All platform facts in the table are `[GK]`. Each is verified by a platform test in C3 or C4.

**Why dual mapping is the Linux default.** `mprotect` on a multithreaded process costs a syscall plus a TLB shootdown per extent, and that cost would land inside G1. Dual mapping never exposes a page that is writable and executable at the same address, but a writable alias does exist. Deployments that forbid aliases set strict mode and pay the syscalls.

**Publication and the AArch64 `isb`.** Code is always written to addresses that no core has executed since they were last freed. It is published by a release store of the function pointer (document 09, 9.5). Each worker keeps the code generation number it last ran. When the number at a morsel boundary differs, it executes one `isb` before calling. That is the ARM architectural requirement for executing newly written code on another core `[GK]`, and it costs one instruction per tier switch, not per morsel. `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE)` is not needed, because we never modify code in place.

**Reclamation is epoch-based.**

- A code extent is freed when three things hold: no cache entry references it, no running query references it, and every worker has passed a morsel boundary since the last reference was dropped.
- Workers announce the global epoch at each morsel boundary. This is one relaxed store per morsel.
- Extents retired in epoch *e* are freed once every worker has announced epoch *e*+1.
- There is **no in-place patching** of live code, ever. Patching happens only on a code buffer that has not yet been published.

**Unsupported in v1:** Windows, iOS, and any OS without JIT-capable memory. On those platforms `interp` is the only backend, and the router says so.

## 8.9 SIMD policy

**`direct` emits no SIMD in its first version. SIMD in compiled pipelines comes from precompiled kernels that generated code calls.** This follows document 07's split between SIMD and scalar code. The precompiled scan and filter kernels produce batches of 1,024 with a selection vector, and they are built per target with `std::arch` and runtime feature detection.

| Target | Hardware `[GK]` | Precompiled kernels | `clif` | `direct` |
|---|---|---|---|---|
| AMD c6a (Zen 3) | AVX2 | AVX2 | 128-bit SSE | scalar |
| Intel c7i | AVX-512 | AVX-512 | 128-bit SSE | scalar |
| Graviton3 | SVE 256-bit, NEON | NEON (+SVE after measurement) | 128-bit NEON | scalar |
| Graviton4 | SVE2 128-bit, NEON | NEON | 128-bit NEON | scalar |
| Apple M4 | NEON; no userland SVE | NEON | 128-bit NEON | scalar |

- **Adding SIMD to `direct`** is allowed only for specific QIR vector operations that document 07 emits in generated loops, such as the group-prefetch hash batch. It needs a measured JOB or TPC-H gain, and it must have both architectures in the same PR (the rule from document 00).
- **Floating-point SIMD must not change results.** A vector sum is a different summation order. Document 12 decides whether any floating-point aggregate may be vectorized; the default is no.

## 8.10 Code size and limits

Flying Start's code was 2.4x larger than LLVM's (VLDBJ 2021). TPDE's was 22% larger than LLVM -O0 on x86 and 16% larger on AArch64. ClickHouse reports about 8 KB per compiled function `[snippet]`. Our targets, which are ours and are measured at C3/C4:

| Limit | Value | On breach |
|---|---|---|
| one function | 1 MB, hard (8.5.5) | the generator splits the pipeline |
| typical pipeline function | 2-16 KB, expected | a regression report in document 17's dashboard |
| one query, all backends | 1 MB | the query stops tiering up; `direct` code is kept |
| code arena, process | 256 MB default (`qc.code_arena_bytes`) | the cache evicts (document 09, 9.8) |
| QIR instructions per function for `clif` | 20,000 | stay on `direct` |

The last limit exists because Cranelift's compile time grows faster than linearly with Ion on very large functions `[GK]`, and a single 100 ms compile would poison a whole pipeline's tier-up estimate.

## 8.11 Testing and tooling hooks

- **Tier-diff.** Every query in every suite runs on each backend with results compared byte for byte (document 15).
- **Encoder differential tests** (8.5.5).
- **Register-allocator stress.** A build flag reduces the allocatable set to three registers per class, so that every spill and reload path runs across the whole test suite.
- **Perf map.** `qc.perf_map=on` writes `/tmp/perf-<pid>.map` lines per function. The GDB JIT interface is available only in debug builds. Document 16 owns both.
- **Tailored profiling hooks.** Each function records a code-range-to-operator map at emission. Beischl et al. needed 44+6+6 lines of code for this and measured 2.8% overhead (EuroSys 2021, https://db.in.tum.de/~beischl/papers/Profiling_Dataflow_Systems_on_Multiple_Abstraction_Levels.pdf).

## 8.12 Milestone mapping

| Milestone | Backend work | Gate |
|---|---|---|
| C1 | `interp` complete for all QIR | all 113 JOB queries correct on `interp` |
| C2 | `clif` (feature `qc-clif`), including literal-table linking and the CRC32 patch | tier-diff clean vs `interp` |
| C3 | `direct` AArch64, encoders and the differential test | G1 on M4 |
| C4 | `direct` x86-64 | G1 on c6a.4xlarge |
| C12 | `clif` tuning; `llvm` evaluation | the >10% SF100 bar in 8.7 |

**A lowering added on one architecture without the other does not merge.** Between C3 and C4, x86-64 runs on `interp` in default builds, and on `clif` in `qc-clif` builds, rather than getting a partial `direct`.

## 8.13 Open question, for document 20

**Does `direct` eventually need a cheap optimizing mode, so that the default build has a tier above baseline?**

- Candidates:
  - a better register allocator for the innermost hot loop only;
  - a local peephole pass;
  - loop-invariant hoisting for loops the generator missed.
- The evidence for how much room there is: DirectEmit is 17% behind LLVM-opt on average and 39% behind on TPC-DS Q17 `[derived]`, from 8.2.
- The decision needs C12 data: how much of the `clif` gain on SF100 pipelines a `direct -O1` mode could recover, and at what compile cost.
- Until then, a default build tiers `interp → direct` only.

## What we should take from this document

**One fast native backend, written by us, is the whole story for compile latency.** The published numbers leave no room for anything else inside a 0.5 ms JOB budget. Cranelift is about 160 µs per function and DirectEmit about 9 µs. Umbra spent 89% of its JOB end-to-end time compiling. `direct` and `interp` are also the only backends in the default build, because of rudb's zero-dependency rule. `direct` is therefore the default and the first thing to optimize, and a register-allocating single pass is the smallest design that stays within about 20% of LLVM.

**The code-quality gap we accept is instruction count, not memory behavior.** Flying Start's code had 2.3x the instructions of LLVM's but equal cache and branch misses. JOB is dominated by probes and misses. The instruction gap therefore shows up mainly in long arithmetic pipelines at large scale factors, and that is exactly the case `clif` tier-up (document 09, `qc-clif` builds) targets.

**Correctness comes from a common contract and an oracle.** All backends share one entry signature, one state layout, status codes instead of unwinding, and bit-identical semantics. `interp` defines the answer, and every backend is diffed against it. ClickHouse's cached-JIT Int128 bug is the precedent for why this is non-negotiable.

**We own the linking and the memory.** Calls go through a per-chunk literal table. Code lives in our arena with W^X handled per platform. It is published with a release store and an `isb` on AArch64, and freed by epochs. This removes cranelift-jit's far-call crashes, makes macOS and Linux behave the same, and lets tiers swap without ever patching live code.

**Scope is held down on purpose.** `direct` has no SIMD, no inlining, no scheduling and no peepholes, and its encoders cover about 150 forms per architecture. `llvm` stays behind a feature until it proves more than 10% at SF100. Each addition must bring a measurement, and must support both architectures in the same PR.
