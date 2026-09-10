# Architecture

This is the shape of the system: what the layers are, what crosses between them, who owns memory, who owns threads, and what happens when something goes wrong. Details of each layer are in documents 05 through 11. This document is about the seams, because the seams are what a specification is actually for and they are the part that is expensive to change later.

## 4.1 The one-paragraph version

A query arrives as text or as a prepared plan. The parser produces an AST, the binder resolves names against the catalog and produces a bound logical plan with fully resolved types, the optimizer rewrites it and chooses join orders and filters, the physical planner turns it into a pipeline graph, and the scheduler runs those pipelines over morsels of data drawn from the storage layer. Every layer has a textual form that round-trips. Data in flight is a vector of at most 1024 values, and the entire performance argument of this project is that a vector is allowed to still be compressed.

## 4.2 Layers

```
  rudb-cli   rudb-c-api   rudb-python   (clients)
       \         |          /
        \        |         /
         rudb  (embedding API, connections, prepared statements)
           |
     rudb-engine  (scheduler, pipelines, operators)
       /       \
rudb-plan     rudb-exec-jit
  |    \           |
  |   rudb-opt   rudb-codegen
  |      |
rudb-bind  ---- rudb-catalog
  |
rudb-parse
       \
     rudb-vector  (vectors, validity, selection, encoded views)
       |
     rudb-storage (row groups, blocks, buffer manager, WAL)
       |
     rudb-io      (files, io_uring, direct I/O, object stores)
       |
     rudb-common  (types, values, errors, arena, allocator)
```

Document 18 gives the exact crate list, the dependency rules and the stability tiers. Two rules matter here. Dependencies point downward only, enforced in CI by a graph check rather than by convention. And `rudb-vector` is the widest interface in the system, so it is specified in document 07 before any operator is written and changed by RFC afterwards.

## 4.3 The unit of data

**A vector is up to 1024 values of one type.** DuckDB uses 2048. We use 1024 because it is the FastLanes unit and because every bit-packing, delta and RLE kernel in the encoding layer works in units of 1024. Using a different execution vector size than the storage vector size means every scan does a regrouping step, and that step is pure overhead that also destroys the ability to hand an operator a still-encoded vector. This is the single decision that most constrains the rest of the system and it is settled in document 00.

**A vector has a physical form, and there are more than two of them.** The forms are: flat, meaning an array of values; constant, meaning one value logically repeated; dictionary, meaning an index vector plus a shared dictionary; sequence, meaning start and stride; and encoded, meaning the vector carries a compressed run of a known encoding along with the metadata needed to decode it. Encoded is the form that does not exist in DuckDB and it is why document 06.7 exists.

**Every vector carries a validity mask** as a bitmap, one bit per value, with a fast path for the all-valid case that is a null pointer rather than a mask of ones. Photon's measured win from separate no-null kernels is the reason the all-valid case is a distinct code path and not a branch inside the general one.

**Selection vectors are used, not filtering by copying.** An operator that filters produces a selection vector of indices into the underlying data rather than a compacted copy. Compaction happens when the selectivity is low enough that it pays for itself, and the threshold is measured per operator rather than guessed. This is the same design as DuckDB and it is right.

**A DataChunk is a set of vectors of the same length**, which is what flows between operators. Its ownership rule is that the chunk owns its vectors' buffers or borrows them from the buffer manager with a pin, and pins are released when the chunk is reset. There is no reference counting on the hot path.

## 4.4 The threading model

**Morsel-driven parallelism, one worker thread per hardware thread by default, work stealing between them.** A pipeline is split into morsels, a morsel being a fixed number of row groups, and workers pull morsels. This is the Leis 2014 design and it is what DuckDB, Umbra and CedarDB all use, because it gives load balance without static partitioning and it degrades gracefully when one morsel turns out to be expensive.

**A pipeline runs from a source, through operators, to a sink.** Sources are scans and the outputs of previous pipelines. Sinks are hash table builds, sorts, aggregations and result collection. A sink is a synchronization point: the pipelines that feed it must finish before pipelines that read from it start. The pipeline graph is a DAG and the scheduler runs it respecting that dependency order, with independent branches running concurrently.

**Sink state is thread-local first and combined at the end.** Each worker builds its own hash table or its own sort run, and a finalize step merges them. This is the standard answer and it is right for aggregation with few groups. It is wrong for aggregation with many groups, where the merge dominates, which is why document 07.5 specifies a partitioned global table with atomic insert for the high-cardinality case and a runtime switch between the two based on observed group count. "Global Hash Tables Strike Back" is the direct source and its finding is that the conventional wisdom here was backwards for large group counts.

**No thread ever blocks on I/O in the execution path.** A scan that misses the buffer pool registers the read with the I/O layer and yields the morsel back to the scheduler, which runs something else. This requires operators to be resumable at morsel boundaries, which they are because morsel boundaries are already the scheduling unit. Document 05.7 covers how the I/O layer's completion queue drives this.

**Rust's role here is concrete and not decorative.** Morsel-driven execution with work stealing and per-thread sink state is exactly the code where C++ data races hide: a shared hash table partially initialized, a vector's validity mask read while another thread writes it, a buffer unpinned while still referenced. The borrow checker makes the ownership of every buffer in that graph a compile-time question. This does not make the design correct, it makes the design's violations into compile errors instead of into a heisenbug found by a user in production.

## 4.5 Front end

**Parser.** A PEG matcher driven by DuckDB's own grammar, vendored verbatim. This document originally said hand-written recursive descent and that we start where DuckDB ended up minus the generator, and the second half of that was wrong: where they ended up is the generator. Their v2.0 parser is driven by sixty one kilobytes of declarative PEG text under the MIT license, with no semantic actions in it, and that text is the definition of the dialect this project claims compatibility with. Transcribing 1,086 rules into Rust by hand, and then transcribing the diff of those rules on every upstream release, is 1,086 chances to reject valid DuckDB SQL, which is the one failure mode document 00 promises we do not have. So we take the file. Document 20 is the whole argument, the measurements behind it, and the three places where fidelity still leaks.

**The tokenizer and everything after the parse tree are still ours.** The grammar has no tokenizer in it, so string literals, dollar quoting, numeric literal forms, case folding and the operator rules are matched to DuckDB by behaviour and checked by a differential fuzzer, which document 20.7 enumerates. The transformer from parse tree to rudb's AST is ours and has to be total over the rule table. The AST is arena-allocated with `u32` indices rather than `Box`, which is the standard Rust idiom for tree-shaped data and avoids a pointer chase per node.

**Error recovery is a tooling feature and it is not on the query path.** A PEG parse stops at the first failure, and a matcher that resynchronizes past an error accepts strings DuckDB rejects, which puts a hole in the compatibility claim to buy a better message. So there are two entry points over the same rule table. The query path parses strictly and reports one error with DuckDB's own text and caret. The tooling path, which is what a language server or an editor integration calls, is allowed to continue past a failure and report multiple diagnostics with spans, because a client tool that gets one error at a time makes the user edit their query fifteen times. Spans are byte offsets into the original text and every diagnostic carries one, on both paths.

**Binder.** Resolves identifiers against the catalog, resolves function overloads, inserts casts, expands star, resolves correlated references, and produces a bound plan where every expression has a known type and every column reference is a stable identifier rather than a name. Binding is where the DuckDB dialect surface lives and it is the largest single body of compatibility work in the project, which document 10 covers.

**The catalog is versioned and read without a lock.** Each transaction sees a catalog snapshot identified by a version number, DDL produces a new version, and readers hold an `Arc` to the version they started with. Document 11.6 covers DDL and catalog transactions.

## 4.6 Optimizer

Document 09 is the detail. The structure is a fixed sequence of rewrite passes over the bound plan, each of which is a pure function from plan to plan, followed by cost-based join ordering, followed by physical planning.

**Every pass is individually switchable** by name from a session setting, which is not a user feature but a debugging and testing feature: the differential harness in document 14 runs the same query with each pass disabled in turn to find which one caused a wrong answer.

**Physical planning chooses more than operators.** It chooses the physical layout to scan a column in, which is the mechanism document 09.6 describes and which is where the Bespoke OLAP result enters the design. The same logical scan can be executed against a dictionary-encoded column as a dictionary scan producing codes, or as a decoded scan producing strings, and which one is correct depends on what the consuming operator does with the values.

## 4.7 Execution tiers

Four tiers, described fully in document 08, chosen per pipeline at runtime.

Tier 0 is the interpreted vectorized executor, a tree of operators calling typed kernels. It is always available, it is the correctness reference, and it is what runs for the first morsels of every pipeline.

Tier 1 is fused vectorized, where an expression tree is compiled into a fused kernel chosen from a precompiled set, avoiding the intermediate materialization between expression nodes. This covers most of the win at almost none of the cost.

Tier 2 is Cranelift JIT compilation of the expression and the tuple-at-a-time inner loop, triggered when a pipeline has processed enough tuples that compilation pays for itself. The CGO 2024 study of Cranelift for query compilation is the evidence that this is viable and gives the compile-time and code-quality figures used in document 08.4.

Tier 3 is a single-pass machine code emitter in the style of TPDE and Umbra's Flying Start, for the case where compilation latency matters and Cranelift is too slow. Tier 3 is explicitly deferred past M8 and may never be built, because it is only worth building if tier 2's compile latency is measured to be a real cost.

**Tiers are a runtime decision based on observed tuple counts, not a planner decision based on estimated ones.** Cardinality estimates are wrong often enough that basing a compile decision on them means compiling for a million tuples and processing eight.

## 4.8 Memory

**One allocator, one accounting path.** Every allocation that can be large goes through the buffer manager, which knows the total budget and can evict or spill. Small allocations go to arenas scoped to a query. There is no third path, and in particular an operator does not call the global allocator for a growing buffer, because then the memory limit is a lie.

**Variable-size pages with exponential size classes from 64 KiB**, per LeanStore and Umbra. Fixed-size pages force large values to be chained across pages and force small structures to waste a page, and the exponential class scheme handles both without a second allocator. Document 05.5 covers the buffer manager.

**Pointer swizzling for buffer references.** A page reference is either a swizzled pointer to the resident page or an unswizzled page identifier, distinguished by a tag bit, so a hit costs a tag test and a hit in the common case costs nothing else. This avoids a hash table lookup per page access, which at the rates this engine needs to run at is not a rounding error.

**Memory limit is enforced, and exceeding it spills rather than aborts.** Hash aggregation, hash join and sort all have spilling implementations, specified in document 07.8. A query that needs more memory than the limit gets slower, it does not fail. This is the single largest robustness difference between a research prototype and a usable database and it is scheduled in M6, not deferred to "later".

## 4.9 Errors and cancellation

**Errors are values, `Result` everywhere, no panics on any path reachable from user input.** A panic in a worker thread during a query is a bug and CI treats it as one: the fuzzing harness in document 16.4 fails on any panic, including ones that would be caught.

**Every error carries a code, a message, and optionally a span into the query text.** The codes are stable and documented because clients switch on them. The messages match DuckDB's where the DuckDB message is what a test asserts on, which is a compatibility obligation covered in document 12.5, and it is genuinely one of the more tedious parts of this project.

**Cancellation is cooperative and checked at morsel boundaries.** A cancelled query stops within one morsel of work, which at 122,880 rows per morsel and the throughputs involved is single-digit milliseconds. There is no thread killing, and there is no unwinding across the FFI boundary: the C API catches at the boundary and converts to an error code, per document 12.3.

**Out of memory is an error, not an abort.** All large allocations are fallible and return `Result`. This is more annoying to write than the alternative and it is the difference between a database and a toy.

## 4.10 What is deliberately not in this architecture

**No distributed execution.** Not a cluster, not a shuffle, not a coordinator. This is settled in document 00 and it is what makes the resource axis achievable at all. An embedded engine that is also a distributed engine is two projects.

**No GPU in the core.** Sirius demonstrated that GPU acceleration works as a Substrait-consuming extension against DuckDB, which is exactly the right shape: out of the core, behind a plan-level interface. Document 13.6 leaves the door open by making the physical plan serializable to Substrait. Nothing in the core knows a GPU exists.

**No row store, no OLTP path, no secondary B-tree index as a query accelerator.** ART indexes exist for primary key and unique constraint enforcement because DuckDB has them and compatibility requires it, and the optimizer will use one for a point lookup, but the engine is not designed around index access paths and no plan in the ClickBench or TPC-H sense depends on one.

**No plugin system beyond the DuckDB extension ABI.** One extension mechanism, the one compatibility requires, described in document 12.4. A second, native, better one is exactly the kind of thing that sounds free and costs a year.
