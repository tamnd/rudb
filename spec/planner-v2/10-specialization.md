# Specialization

`../08-codegen.md` settled the tiers in September and this document does not reopen that. What it does is change the order the tiers are worth building in, because of one measurement published since, and widen what the compiler compiles, because document 08 gave it something bigger than an expression tree to work on.

## 10.1 What stands from the codegen document

Four tiers: interpreted vectorized, fused vectorized kernels, Cranelift, and a single-pass emitter that is explicitly speculative and explicitly conditional on tier 2's compile latency being measured to hurt.

**Tier 0 is never removed and never optional.** It is the correctness reference and every other tier is validated against it bit for bit. This is the single most valuable sentence in that document and it is the reason a compiler is affordable here at all.

**Compilation is asynchronous and the pipeline switches at a morsel boundary.** A query that finishes before the compile does never pays for it.

**The compiled and interpreted paths must agree exactly**, including on overflow, null propagation, string comparison and floating point, which means no reassociation in generated code and no fused multiply-add unless the interpreter uses it too. A query whose answer changes when it gets faster is the worst bug this project could ship.

**Generated code is registered with the platform profiler.** Without that a performance problem inside the compiler's output is undiagnosable and the tier becomes a box nobody can improve.

All of that stands unchanged.

## 10.2 The one structural change: the unit is a pipeline, not an expression

`../08-codegen.md` section 8.5 says the shared IR is a typed SSA expression IR, and `crates/rudb-ir/src/lib.rs` says the same in its module documentation. Document 08 in this folder widens it to a pipeline program, and that changes what the compiler can do.

An expression IR lets tier 2 compile `a + b * c` into one loop. A pipeline program lets tier 2 compile the scan, the filter, the hash and the insert into one loop, which is the classic compilation win and is worth considerably more than fusing an arithmetic tree. `../08-codegen.md` section 8.4 already says "fused across operator boundaries within the pipeline" is what gets compiled, so the intent was always this. What was missing is a representation of a pipeline for the compiler to be handed, which is exactly what artifact 6 is.

The consequence for tier 1 is the same widening. Today fusion is a pattern match over expression shapes from a table. With a program, a fused kernel covers a run of blocks, and the runs worth covering are known from measurement rather than guessed: scan then compare then select, hash then insert then update, probe then gather.

## 10.3 The ablation that changes the order of work

Document 02 section 2.8. In the Bespoke OLAP evaluation, specializing the generated code while holding a fixed struct-of-arrays layout was worth 1.26x on TPC-H and 0.57x on the Cardinality Estimation Benchmark, which is to say it lost on one of the two. Adding layout specialization took the same system to 12.35x and 51.40x.

Take that result seriously and the priority order in `../08-codegen.md` inverts. Compiling the loop is worth tens of percent. Changing what the loop reads is worth an order of magnitude. rudb already writes dictionary codes, run lengths, frame-of-reference deltas and FSST symbols, and then decodes all of them into raw values before anything interesting happens to them.

So the order of work in document 14 is: encoded execution first, tier 1 fusion second, tier 2 compilation third. That is a reordering of an existing plan rather than a new plan, and it is the single most consequential recommendation in this document.

## 10.4 Encoded execution

Four operations carry most of the benefit and all four have a shape that is already in the tree.

**Group on codes.** A `GROUP BY` on a dictionary column hashes a `u16` or `u32` instead of a string, and when the dictionary is per column rather than per part the codes are comparable across the whole table so nothing has to be reconciled at merge time. Most of ClickBench is this query.

**Aggregate over runs.** A `SUM` over a run-length encoded column is a multiply per run rather than an add per row. A `COUNT` over one is an add per run. The kernel is trivial and the only hard part is making sure a selection over a run-encoded vector does not silently expand it.

**Compare on compressed bytes.** An equality or a prefix `LIKE` against an FSST-compressed column can run against the compressed representation when the pattern compresses under the same symbol table. `crates/rudb-vector/src/fsst.rs` exists.

**Compare in code space.** `x = 'value'` on a dictionary column becomes `code = k` where `k` is looked up once per query. `x < 'value'` becomes the same thing when the dictionary is order preserving, and a range over codes otherwise.

Two rules.

**An encoded path may never change an answer.** Same rows, same nulls, same order guarantees. This is document 03 section 3.6 applied to a representation choice, and it is testable directly: run the query with encoded execution disabled by setting and diff.

**A representation that the operator cannot handle is a decode, not an error.** Document 07 section 7.5. The worst case of the whole mechanism is today's behaviour, which is what makes it safe to add one operation at a time.

## 10.5 Kernel selection, and why it is the only thing allowed below artifact 6

Document 03 section 3.3 says a test on query shape may not appear below artifact 6. The exception is a test on the data in front of you, and kernel selection is that exception in full. The set is enumerable and it is worth enumerating, because "properties of the chunk" is otherwise a loophole wide enough to put anything through.

**Selection density.** Whether to compact the surviving rows into a chunk of their own or carry a selection over the original. `crates/rudb-pipeline/src/compact.rs` already implements this as a seam with a gain function.

**Code width.** Whether this chunk's dictionary codes fit in a byte, so the hash is over `u8` rather than `u32`. A property of the part, not of the query.

**Partition size against cache.** Whether this partition fits in L2, which decides between an in-cache sort and a radix pass.

**Match density on a probe.** Whether this chunk's probe found matches for most rows or a few, which decides between a gather and a filtered gather.

**Validity density.** Whether this chunk has no nulls at all, which lets a kernel skip the validity computation entirely. This is the cheapest and most broadly applicable of the five.

That is the list. A branch below artifact 6 that is not one of these five needs an argument, and the argument has to be that the thing it tests could not have been known when the program was built.

## 10.6 What an implementation may learn

Document 09 section 9.7 states the rule and this is where it gets used most: **an implementation may learn a property of the machine and may not learn a property of the workload.**

`Gauge` is the worked example. It holds how many more times the kept rows will be read, which is a plan time fact handed down, and nanoseconds per byte, which is measured on this machine while the query runs. A future kernel selector may measure how fast this machine gathers, how large its cache actually is, or how much a branch miss costs here. It may not measure that this query's filter usually keeps a tenth and adjust accordingly, because that makes the plan a function of history.

The per instance placement stays. Thirty two threads learning the same machine constant thirty two times is cheaper than thirty two threads contending over one copy of it.

## 10.7 The back end, revisited

Cranelift for tier 2, unchanged, and the reasons in `../08-codegen.md` section 8.4 all still hold: compile times roughly an order of magnitude below LLVM, code quality close enough that end to end is better at typical query durations, a Rust library with no C++ toolchain.

Tier 3 stays deferred and stays conditional. Document 02 section 2.5 records what has appeared since, which is TPDE and the single-pass emitter line of work generally, at 10 to 20x faster compilation than LLVM `-O0` for 10 to 30 percent worse code. That is a real result and it does not change the decision, because the condition in `../08-codegen.md` is about rudb's measured compile latency and nobody has measured it yet. The tier interface must keep admitting it and nothing should be built for it speculatively.

What this folder adds is a sharper version of the condition. **Build tier 3 only if a workload we care about spends more than a stated fraction of its total time waiting for tier 2, measured with tier 2's asynchronous compilation already working.** The asynchronous clause matters, because most of the apparent cost of a JIT disappears when the pipeline runs on tier 0 while the compile happens, and measuring before that is in place would produce a number that argues for a year of encoding work nobody needs.

## 10.8 What must never be specialized

**A path reachable only by a query somebody has written down.** Document 05 section 5.6's fourth destination. The test is whether the specialization fires on a query nobody has written yet.

**A path that exists in only one tier.** A fused kernel or a compiled path with no tier 0 equivalent is a path with no reference implementation, and the differential test that makes this whole architecture safe silently stops covering it.

**A path whose correctness argument is different from the general path's.** If the fast path handles nulls differently, or overflows differently, or orders floating point additions differently, then it is not a specialization of the general path, it is a second implementation of the semantics. `Determinism::PerThreadCount` in `rudb-seam` is the honest declaration for a parallel float aggregate and there should be no others without a written reason.

## What we should take from this document

The four tiers stand, tier 0 stays the reference, and Cranelift stays the tier 2 answer with the single-pass emitter deferred behind a condition this document makes measurable.

The compile unit widens from an expression tree to a pipeline program, which is what `../08-codegen.md` always intended and what artifact 6 finally supplies.

The order of work inverts on the evidence: encoded execution before fusion before compilation, because layout specialization measured an order of magnitude where code specialization measured tens of percent and sometimes a loss.

Kernel selection is the one legitimate thing below artifact 6 and the list has five entries. Anything else down there needs to argue that it could not have been known when the program was built.
