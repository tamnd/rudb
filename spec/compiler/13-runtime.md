# The runtime

Generated code is the hot loop and nothing else. Everything around it is `rudb-qc-rt`, a precompiled Rust library:

- the calls it makes when it needs memory, a long string, a spill or a function it does not inline;
- the scheduler glue that feeds it morsels;
- the memory its code lives in;
- the rules for errors and cancellation.

This document specifies that library: its ABI, its function catalog, how memory is reserved and allocated, how errors travel, how cancellation reaches a loop, how pipelines are scheduled, and how spilling fits. Umbra calls the split between generated and precompiled code its cogwheel design. The split is why generated code stays small enough to compile in microseconds, and the runtime is the other half of it.

## 13.1 Principles

**The runtime owns every operation that can allocate, block, panic, format or grow.** Document 03 section 3.6 lists what generated code may contain. This document is the complement. A runtime function is ordinary Rust, compiled once with the rest of rudb and optimized by rustc. Its cost is paid per call, so the generator calls it only off the per-tuple hot path, or in the per-tuple path only for operations that are expensive anyway: `LIKE` on a 200-byte string, decimal division, regex.

**Every runtime function has a C ABI, does not unwind, and says in its declaration what it may do.** The generator knows the runtime through a declaration table, not through Rust types. That is how Umbra's generated proxies work (Tidy Tuples, https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf), and a build script produces the table from attributes on the Rust functions.

**The runtime is shared by every backend.** The interpreter calls the same functions through the same table. So a difference between tiers can never come from the runtime. That is a property document 15's tier-differential testing depends on.

## 13.2 The ABI

**One entry signature for every generated function.**

```rust
pub type PipelineFn = unsafe extern "C" fn(state: *mut PipelineState, morsel: *const Morsel) -> Status;

#[repr(C)]
pub struct Status(u64);   // low 8 bits: kind, high 56 bits: payload. Owned by document 05 section 5.4.1.

// kind 0 Ok          morsel done; effects committed
// kind 1 Yield       stopped at a batch boundary; cursor saved in state; call again with the same morsel
// kind 2 Done        pipeline may stop early (LIMIT reached, top-N closed)
// kind 3 Deopt       payload = guard site; a guard failed before the first side effect; rerun in the fallback variant
// kind 4 NeedMemory  payload = state slot; grant a chunk, call again with the same morsel
// kind 5 Cancelled   the cancel word was seen set
// kind 6 Error       payload = error code; the error slot is filled; the query fails
```

`Ok = 0` so the check after every runtime call and at every return is one `cbnz`/`test+jnz`. The values are defined once, in document 05, because four backends and the interpreter must agree on them.

A point function (document 14) is a `PipelineFn` whose `morsel` is null. There is one ABI, not two.

**`PipelineState` begins with the one-cache-line `StateHeader` of document 05 section 5.5.** The runtime depends on four of its fields: `rt` (the runtime table, including the query context with its cancel word, memory accounting and first-error cell), `poll_left` (the countdown `poll` decrements; the cancel word is loaded only when it reaches zero), `params` (the parameter block of document 14, null for unparameterized statements) and `error` (the deferred error word). The rest of the state is operator slots at offsets fixed by `rudb-qc-pipe`.

There is one `PipelineState` per (pipeline, worker) instantiation. The runtime builds these at pipeline start, and generated code never writes the header. Backends keep `state` in a fixed callee-saved register for the whole function, `x28` on AArch64 and `r15` on x86-64, and the morsel cursor in `x27`/`r14` (document 08 section 8.5.2). There is no process-wide pinned register for a runtime table. It would remove a register from allocation in every function, and Cranelift supports a pinned register only as a global flag.

**Runtime calls go through a per-chunk literal table of absolute addresses** (document 08 section 8.5.4): `call qword ptr [rip + disp32]` on x86-64 and `ldr x16, <literal>; blr x16` on AArch64. Relative calls are not used, because neither `rel32` nor `BL`'s ±128 MB is guaranteed to reach the rudb binary from an mmap'd region under ASLR. The cost is one predicted, cached load per call, and the cogwheel split keeps calls out of hot loops. The code cache lives in memory only (document 09), so a compiled function never outlives the process whose addresses it embeds.

The number of runtime calls per tuple on the hot path is shown per pipeline in `EXPLAIN (CODEGEN)` (document 16).

**Calling convention for runtime functions.**

- Arguments and results are integers, pointers, `f64`, or `i128` passed as two `u64`.
- A fallible function returns `u32` status (0 = ok) and writes its results through out-pointers.
- An infallible function returns its value directly.
- Every runtime function takes `state` as its first argument if it may fail, so it can reach the error slot.
- **At most 6 integer and 4 floating-point arguments, all in registers.** More are passed through a pointer to a struct. This removes the one place Apple's AAPCS64 variant differs from Linux in a way the call lowering would see (document 08 section 8.5.2), and the catalogue in `rudb-qc-ir` rejects a signature that breaks it.

## 13.3 The function catalog

Each function is declared with five attributes the generator reads:

| attribute | meaning |
|---|---|
| `fallible` | may return a non-zero status |
| `yields` | may return `Yield` |
| `pure` | has no side effects, so CSE and hoisting are allowed |
| `reads_state` | reads operator state |
| `cold` | the backend places the call on a cold path and does not keep values in caller-saved registers across it |

The catalog at C1, grouped. Costs are targets for C0/C5 measurement, not measurements.

| group | functions | called | target cost |
|---|---|---|---|
| hash tables | `rt_ht_insert_slow` (chunk full: allocate next chunk), `rt_ht_finalize` (two-phase build, directory sizing), `rt_ht_grow` (aggregation table growth) | per chunk, per pipeline, per doubling | amortized under 1 ns per tuple |
| aggregation | `rt_agg_partition_flush`, `rt_agg_merge`, `rt_agg_opaque_update`, `rt_agg_opaque_combine` | per full partition, per pipeline end, per batch for opaque aggregates | opaque update: the first engine's kernel plus a copy |
| sort | `rt_sort_run_append_slow`, `rt_sort_finalize`, `rt_topn_heap_replace` | per chunk, per pipeline, per replacement | |
| strings | `rt_str_persist`, `rt_str_eq_long`, `rt_str_cmp_long`, `rt_like_bmh`, `rt_like_twoway`, `rt_like_ac`, `rt_ilike`, `rt_regex_match`, `rt_utf8_*` | per long string, per row that reaches the matcher | `rt_str_persist`: one bump allocation plus `memcpy` |
| numerics | `rt_dec_div`, `rt_dec_round`, `rt_cast_str_to_*`, `rt_cast_*_to_str`, `rt_cast_f64_to_int`, `rt_interval_*` | per row where used | tens of ns [derived] |
| functions | `rt_vcall(state, fn_id, args, sel, n, out)` | per batch of up to 1,024 | the first engine's kernel time plus a copy (document 03 section 3.4) |
| sinks | `rt_sink_flush`, `rt_result_emit` | per full output buffer | |
| errors | `rt_raise(state, kind, op, ty, a, b, inst)`, `rt_raise_rescan` | on the error path only | irrelevant |
| cancellation | `rt_poll(state)` | every N back-edges in unbounded loops | one load inline; the call only when set |
| storage | `rt_index_lookup`, `rt_row_lock`, `rt_row_update`, `rt_row_insert` (from `../engine-v4/`) | per point operation | owned by engine-v4 |

**Every runtime function is wrapped so that it cannot unwind.** An `extern "C"` Rust function that panics aborts the process on current Rust (RFC 2945, https://rust-lang.github.io/rfcs/2945-c-unwind-abi.html). We never let one reach that point. A proc-macro `#[rt_fn]` wraps each body in `catch_unwind`. A caught panic becomes `Error` with kind `Internal`, the panic message and the function name in the slot, and a counter increment that document 16 surfaces. `catch_unwind` costs nothing on the non-panicking path. The query fails with an internal error, the process keeps running, and a fuzzer that finds one gets a readable report instead of a core dump.

**The dispatcher has the second net.** The Rust code that calls a `PipelineFn` also runs inside `catch_unwind`. This is research-notes E design rule 12: `catch_unwind` as a safety net, never as a control-flow mechanism. The net can only catch panics raised in Rust frames above the JIT frame. Nothing is allowed to unwind through a JIT frame. If a future feature needs that, it requires `extern "C-unwind"` and registered unwind tables for generated code, and it is an open question in document 20, not a design option here.

## 13.4 Memory

**Three owners, and generated code is none of them.**

1. **Query memory.** Hash table chunks, aggregate tables, sort runs, persistent strings and result buffers. Document 04 section 4.8 reserves it at plan time as `Reservation { bytes_lo, bytes_hi, on_exceed }`. The runtime allocates it from the shared buffer manager at pipeline start. The query's arena is released at query end in one operation.
2. **Worker scratch.** One bump arena per worker for `Transient` strings, reset per batch. One stage buffer per worker for staged probes and `vcall` argument vectors, sized at pipeline start. These are reused across queries and never freed on the hot path.
3. **Code memory.** Covered below.

**Growth is the runtime's, and it happens at defined points.** The generated insert into a thread-local materialization buffer is a bounds check plus stores. When the check fails, it calls `rt_ht_insert_slow`, which allocates the next chunk and returns the new write pointer. The call is `cold`.

An aggregation table that reaches its load factor calls `rt_ht_grow`. That happens inside the call, on the worker thread, with no other worker involved, because the table is thread-local (document 11). Global structures, meaning the finalized join hash table and the merged aggregation, are built only in pipeline-finish work, which the runtime runs between pipelines.

**Restartability: a morsel's effects are either none or committed.** Deopt reruns a morsel in another variant, so the specialized variant must not have left partial effects behind (research-notes E section 9.5). The runtime supports exactly two patterns, and the planner's guards (document 04 section 4.7) must use one of them.

- **Entry guards.** The guard is evaluated from the morsel's row-group metadata before the first side effect: null count, max string length, encoding tag. Morsels are aligned to row groups (document 03 section 3.7), so all three are single loads. The narrow-accumulator guard also takes this form where zone maps exist. It checks at entry that `(rows seen by this worker + rows in this morsel) × max |v|` fits in 63 bits. The check covers every group of a grouped aggregation at once. If it fails, the variant returns `Deopt`, and the runtime widens the worker's table before calling the generic variant.
- **Morsel-local effects.** When the fact can only be checked during the morsel, the variant writes its effects to morsel-local scratch, and the runtime commits them to worker state on `Ok` or drops them on `Deopt`. This applies to an ungrouped narrow `SUM` whose partial lives in registers, a sink into a morsel-sized output buffer, and staged materialization. A variant that updates a thread-local hash table in place cannot use a mid-morsel guard.

Document 04's guard table follows this rule: the morsel-end overflow check is used only for scalar SUM in a morsel-local partial, the second pattern, and grouped aggregation uses the entry bound.

**JIT memory.** Code lives in the `CodeArena` of document 08 section 8.8, reserved at startup (64 MB of address space by default, committed on demand, more arenas if it fills). Because runtime calls go through each chunk's literal table, the arena may sit anywhere in the address space, and no placement near rudb's text is needed.

- **macOS on Apple silicon.** The arena is mapped with `MAP_JIT`. The compiling thread calls `pthread_jit_write_protect_np(false)`, writes, re-protects and calls `sys_icache_invalidate` on the written range (research-notes B). Write protection is per thread, so workers keep executing other code in the same arena while a compile writes.
- **Linux.** A `memfd` is mapped twice: once read-write for the compiler and once read-execute for execution. So there is never an `mprotect` or TLB shootdown on the compile path. On AArch64, `__builtin___clear_cache` runs over the written range before the function pointer is published. Missing that is the classic "works on x86, crashes on Graviton" bug (document 01).
- **Allocation.** A size-class allocator over 4 KiB-aligned blocks. A module's functions are contiguous so that one `perf` map entry per function stays simple (document 16).

**Code is freed by epoch, not by reference count.** A function pointer is published to a pipeline descriptor or a cache entry with a release store. Workers announce an epoch on their own cache line when they pick up a morsel or start a point call. That is the same per-worker epoch `../engine-v4/13-the-point-path.md` section 13.5 uses for catalog snapshots, and we share it.

A module retired by cache eviction or by the end of its query is freed when every worker's epoch has passed the retire epoch. No shared counter is incremented on the execution path.

## 13.5 Errors

**Generated code reports an error by filling the worker's error slot and returning `Error`.** It does not throw, and nothing unwinds (research-notes E section 5.2, option A).

```rust
#[repr(C)]
pub struct ErrorSlot {
    pub kind: u32,        // Overflow, DivideByZero, Conversion, OutOfRange, OutOfMemory, Internal, ...
    pub op: u16,          // QIR opcode that raised
    pub ty: u16,          // SQL type id of the operation
    pub node: u32,        // plan node id (every QIR instruction carries it, document 16)
    pub inst: u32,        // QIR instruction id
    pub a: [u64; 2],      // operands as raw i128, or pointers to string headers
    pub b: [u64; 2],
    pub row: u32,         // row within the batch; u32::MAX when set by a deferred check
    pub message: *mut u8, // filled by the runtime when the message needs a string, else null
}
```

**First error wins, deterministically enough.** When a function returns `Error`, the dispatcher CASes the query's first-error cell from empty to this worker's slot contents. It then sets the cancel word, so other workers stop at their next poll or morsel boundary (MORSEL, https://db.in.tum.de/~leis/papers/morsels.pdf). The error returned is whichever worker won the CAS. That matches the promise in document 12 section 12.4: some error the query could raise, never one it could not.

**Messages are formatted once, by the same code the first engine uses.** The dispatcher turns the slot into `rudb::Error` by calling the first engine's formatter with (kind, op, ty, a, b). An `Out of Range Error: Overflow in addition of INT32 (…)!` from the compiled engine is therefore the same bytes as from the first engine, and `rudb-compat` compares them.

A deferred check that fired sets `row = u32::MAX`. The generated rescan then fills in the operands before returning, so the formatter never sees a slot without operands.

**Transactions are not the runtime's business.** A statement that returns an error leaves the transaction in whatever state the first engine's transaction manager puts it in after an error. The compiled engine reports the error the same way the first engine does and lets the shared transaction manager apply the rule. Write pipelines check deferred errors before their write stage (research-notes E section 5.2), so an erroring DML statement has written nothing when it reports.

**No signal handlers.** Integer division is guarded explicitly (document 12 section 12.4), and generated code has no trapping instruction on any path. A SIGSEGV or SIGFPE in generated code is a compiler bug. It crashes the process, and the crash is caught by the fuzzers in document 15, not masked.

## 13.6 Cancellation and timeouts

**The runtime checks the cancel word before every morsel.** Generated code does not check at morsel granularity. The dispatcher does it, because that is where the next morsel is picked (MORSEL). With morsels of about 16,384 tuples, the check interval is one morsel's run time, which is well under a millisecond for the scan-and-probe pipelines of JOB.

**Generated code checks only in loops whose trip count the plan cannot bound** (research-notes E section 5.3). These are:

- hash chain walks, which are unbounded under skew;
- nested-loop and `Expand` joins;
- `generate_series` and `unnest` producers;
- string loops over values that can be arbitrarily long.

The generator emits the check. A backend never adds one.

```
chain.head:
    %k1   = sub i32 %k, 1
    %due  = icmp eq i32 %k1, 0
    br %due, chain.poll, chain.body         ; predicted not taken
chain.poll (cold):
    %c    = load.relaxed u32 [%state + 24]→[0]
    br %c, ret.cancelled, chain.reset
chain.reset:
    %k    = 4096                            ; reset the countdown
    br chain.body
```

That is one decrement and one predicted branch per iteration. 4,096 is the initial interval. The interval is tuned so that the worst-case iteration keeps the gap between polls under 100 µs [derived], and it is a constant in the generator, not a setting.

**A statement timeout and a user cancel are the same mechanism.** A timer thread sets the cancel word with a reason code. The dispatcher then reports `Cancelled` as DuckDB's interrupt error. A long runtime call polls on its own: regex over a large value, a sort finalize, a spill write.

**Yield, for the scheduler's sake.** A generated function returns `Yield` at a batch boundary when either of these holds:

1. A runtime call reported that it needs a global action before it can continue: a spill that must coordinate partitions across workers (section 13.8).
2. The scheduler has raised the worker's `yield` flag because a higher-priority pipeline is waiting. The flag is read in the same poll as the cancel word, at batch granularity.

The batch cursor is saved in the worker context, and the next call with the same morsel resumes there. Every effect up to the cursor is committed, so a yielded morsel is never deoptimized.

## 13.7 Scheduling

**One process-wide pool, shared with the first engine and storage** (document 03 section 3.7). The runtime adds three kinds of tasks:

| task | priority | what it does |
|---|---|---|
| pipeline morsel | normal | picks the next morsel of a pipeline, calls the current function pointer, handles the returned `Status` |
| pipeline finish | normal | merges worker state, finalizes hash tables (two-phase build), sorts, releases scratch, starts dependent pipelines |
| compile | low | lowers one QIR function with a backend chosen by document 09's policy, publishes the pointer |

**Morsel dispatch is an atomic cursor per pipeline over its row-group list.**

- A worker that finishes a morsel takes the next one from the same pipeline if one exists. That keeps its thread-local state warm.
- Otherwise it moves to another runnable pipeline of the same query, and then to another query.
- Pipelines with a single morsel, and point functions, run inline on the calling thread without touching the pool. This is the fast path document 14 needs.

**Tier switching is a pointer swap the dispatcher sees at the next morsel.** Each pipeline descriptor holds an `AtomicPtr<PipelineFn>` and a variant table for deopt targets. A compile task stores the new pointer with release ordering. Workers load it with acquire ordering before each morsel.

There is no on-stack replacement, because the function returns after every morsel (research-notes E section 6.2, ADAPT and CGO24). The policy that decides when to compile which backend is document 09. The runtime only executes the decision and supplies its inputs: per-morsel tuple counts and elapsed time per pipeline, collected per worker and summed on demand.

**Handling a returned status.**

| status | dispatcher action |
|---|---|
| `Ok` | commit morsel-local effects; take the next morsel |
| `Yield` | perform the requested global action or yield; requeue the same morsel with its cursor |
| `Deopt` | drop morsel-local effects; mark the variant failed for this pipeline, or for this row group when the guard is per row group; rerun the morsel with the generic variant; count it |
| `Cancelled` | stop taking morsels for this query; release the worker |
| `Error` | CAS the first-error cell; set the cancel word; stop |

A pipeline whose guard fails on more than a quarter of its morsels switches to the generic variant for the rest of its run [derived], so a bad fact does not cost a failed attempt per morsel. The count is reported in `EXPLAIN ANALYZE` (document 16).

**Compile tasks never block execution.** A pipeline always has a runnable function, which is the interpreter at worst. Compilation happens on a worker that would otherwise be idle, or preempts one morsel slot when document 09's extrapolation says the remaining time justifies it. That is ADAPT's rule of charging compile time across the available threads. At most one compile task per pipeline is in flight.

## 13.8 Spilling

**Spilling is decided at execution, and the compiled engine starts with the smallest set.** Document 04 section 4.8 fixes the scope. If the sum of `bytes_lo` over simultaneously live pipelines does not fit the query budget, the query goes to the first engine before it starts. In-flight spilling in the compiled engine is a C8 item, and only for aggregation partitions.

**Aggregation spills by partition, inside the runtime.**

- Thread-local pre-aggregation hashes into partitions (MORSEL). A full partition is flushed by `rt_agg_partition_flush`, called from generated code as a `cold` call.
- Under memory pressure, the flush writes the partition to the buffer manager's temporary storage instead of keeping it in memory.
- The merge pipeline reads partitions one at a time.

Generated code does not know whether a flush spilled. Global ticketing aggregation (https://arxiv.org/abs/2505.04153, 1.78x at low cardinality) is a runtime strategy choice document 11 owns. It does not change this interface.

**Join spilling is not in the compiled engine until the evidence asks for it.** Kuiper et al. (https://www.vldb.org/pvldb/vol18/p2748-kuiper.pdf) argue that spilling should be decided at execution and degrade continuously, and that is the design when it comes. Until then, a join build that exceeds its reservation's `bytes_hi` is handled in one of two ways:

- If the query's result sink has not yet delivered a row to the client, the runtime aborts the compiled execution and reruns the query on the first engine, counted as a late refusal.
- Otherwise the query fails with the first engine's out-of-memory error.

The late refusal rate is published with the router's refusal rate (document 03 section 3.4). If it is non-zero on any benchmark, join spilling moves up in document 18.

## What we should take from this document

`rudb-qc-rt` is the other half of every generated function. Generated code does arithmetic, loads, stores, hashing and probes. The runtime does everything that allocates, grows, spills, formats or can panic. Every runtime function is `extern "C"`, wrapped in `catch_unwind`, and declared to the generator with attributes, so the generator knows what each call may do without seeing Rust types.

The ABI is one signature, `fn(state, morsel) -> Status`, for pipelines and point functions alike. There are five statuses, `Ok = 0` so the check is one branch. There is a fixed 32-byte state header, the state pointer lives in a callee-saved register, and nothing is pinned process-wide.

Errors fill a per-worker slot with numbers, not text. The first error wins by CAS, the cancel word stops the other workers, and the first engine's formatter makes the message identical to the first engine's. There are no signal handlers and no unwinding through JIT frames.

Restartability comes from where guards sit, not from rollback. A guard reads row-group metadata before the first side effect, or its variant buffers effects morsel-locally. Nothing else may return `Deopt`.

Cancellation is checked by the dispatcher per morsel and by generated code only in unbounded loops, with a countdown the generator emits. Tier switches are pointer swaps at morsel boundaries. Code memory is freed by the same per-worker epochs engine-v4 uses, so the execution path writes no shared cache line.

Spilling starts with aggregation partitions only. A join that outgrows its reservation before any row is delivered reruns on the first engine, and that rate is published.
