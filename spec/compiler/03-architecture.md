# Architecture

The shape of the compiled engine: the artifacts it produces, the arrows between them, what it shares with the first engine, and the rule that decides which engine runs a query.

## 3.1 The artifacts

Every stage produces a value with a textual form that prints and parses back, which is the rule `../00-README.md` settled for the whole project. Here that rule does more work than usual. A compiler bug is almost always visible as a wrong artifact at exactly one arrow, and the arrow is found by dumping each artifact and diffing it against a known good one.

| # | Artifact | Produced by | Owned by | Textual form |
|---|---|---|---|---|
| A1 | SQL text | the client | | |
| A2 | syntax tree | `rudb-parse` | shared | yes, existing |
| A3 | bound logical plan | `rudb-bind` into `rudb-plan` | shared | yes, existing |
| A4 | rewritten logical plan | logical passes in `rudb-opt` | shared | yes, existing |
| A5 | physical plan | `rudb-qc-plan` | compiled engine | new, document 04 |
| A6 | pipeline graph | `rudb-qc-pipe` | compiled engine | new, document 05 |
| A7 | QIR module | `rudb-qc-gen` | compiled engine | new, document 06 |
| A8 | machine code, or interpreter bytecode | a backend in `rudb-qc-*` | compiled engine | disassembly, document 16 |
| A9 | result chunks | the result sink | shared | the client protocol |

The boundary is between A4 and A5. Everything to the left is shared with the first engine, unchanged. Everything to the right is new, and none of it imports `rudb-exec` or `rudb-pipeline`.

## 3.2 Why the boundary is at the logical plan

The user asked for the frontend to be reused and everything else designed fresh. That puts the boundary somewhere between the parser and the physical plan, and there are three candidate places. The choice matters, so the rejected ones are stated.

**At the syntax tree, A2.** Rejected. Binding is where DuckDB's semantics live: name resolution, implicit casts, overload resolution, the type of every expression. A second binder is a second definition of what a query means, and `rudb-compat` would spend its life reconciling the two.

**At the bound plan, A3, with our own logical optimizer.** Rejected for now. Unnesting, predicate pushdown and column pruning are not engine choices. They are rewrites whose result the compiled engine wants in exactly the same form as the vectorized one. Duplicating them buys nothing.

**At the rewritten logical plan, A4, with our own physical planner.** Chosen. Every decision that is really about how to run the query is physical: which join algorithm, whether to reduce and in what order, which hash table, which aggregation strategy, which encoding a scan hands up, where a probe needs a prefetch stage. Those decisions are made differently when the executor is a compiler. A compiler can afford a specialized loop per decision, while a vectorized engine has to bound the number of kernels it ships. So the physical planner belongs to this engine.

**Join ordering is the one grey area.** `rudb-opt` has join ordering and predicate transfer today, and both are logical in the sense that they reorder the plan without choosing algorithms. The compiled engine takes the join order that `rudb-opt` produces as its input. It owns the reduction schedule, the build-side choice and the filter placement, which are physical, as document 04 argues. If the compiled engine's planner later wants to re-order joins for a reason only it can see, such as a stored link making one order nearly free, it does that as a physical-plan transformation with the textual diff in the `EXPLAIN` output.

**What A4 must guarantee.** The compiled engine relies on four properties of the logical plan. They are asserted by a verifier at the boundary, not assumed:

1. Subqueries are decorrelated. What remains are joins, semi-joins, anti-joins and mark joins. There are no dependent joins.
2. Every expression is typed and every implicit cast is explicit.
3. Column pruning has run, so every scan names only the columns the query reads.
4. Filters are pushed to the lowest operator that can evaluate them.

If the verifier fails, the query goes to the first engine and the failure is counted. That is how a gap in the shared frontend becomes visible instead of becoming a compiled-engine workaround.

## 3.3 The two engines

```
                 SQL
                  |
          parse -> bind -> rewrite                (shared, A2..A4)
                  |
               router  ---- refuses ---->  vectorized engine (first engine)
                  |                              |
               accepts                           |
                  |                              |
   physical plan -> pipelines -> QIR -> backend  |
                  |                              |
          runtime: scheduler, runtime library    |
                  |                              |
                result sink  <-------------------+
                  |
                client
```

**What the two engines share.**

- The frontend (A2 to A4).
- The catalog.
- Storage: the native format, the DuckDB format reader and writer, Parquet and CSV readers.
- The buffer manager and the memory reservation system.
- The transaction manager and snapshot.
- The thread pool.
- The function registry's semantic definitions.
- The result format that crosses the client boundary.

**What they do not share.** Operators, hash tables, aggregate state layouts, sort implementations, pipeline drivers, expression evaluation.

The compiled engine has its own hash table, its own row layout for materialized state, and its own sort. Sharing them would tie the compiled engine's data structure choices to the first engine's, and the data structures are where much of the performance is. Document 10 specifies a hash table built for probe loops generated per query, which the first engine's tables are not.

**Why a second engine rather than a new tier inside the first.** A tier inside the first engine inherits its interfaces: `Chunk` between operators, a `Vector` in one of seven physical forms, an operator trait with a push method. Every one of those is right for a vectorized engine and a cost for a compiled one. A compiled pipeline passes values between operators in registers, and a chunk interface between them is exactly the materialization compilation exists to remove. The compiled engine needs the storage decoders and the kernels, not the operator interface. Designing it as a second engine lets it take the first two and leave the third.

## 3.4 The router

**The rule.** A query runs on the compiled engine if and only if every operator in its A4 plan has a translator, every expression's functions are compilable, and the A4 verifier passes. Otherwise it runs on the first engine. There is no partial acceptance until milestone C9 in document 18.

**"Compilable" for functions is broad by design.** DuckDB ships about 1,500 built-in functions and the compiled engine will not have an inline translator for most of them. It does not need one. QIR has a `vcall` instruction (document 06, section 6.6) that collects arguments for a batch of up to 1,024 tuples into a small vector buffer, calls the first engine's vectorized kernel for that function, and reads the results back into the pipeline. That costs roughly what the first engine pays for the function plus a copy, and it means every scalar function the first engine supports is compilable from the first milestone. Inline translators exist only for the functions that measurement shows are hot: arithmetic, comparison, casts, `LIKE`, string prefix and length functions, date parts, `hash`, `CASE`, `COALESCE`, and a list that grows by profiling.

The same trick covers aggregates. A rare aggregate function whose state the compiled engine does not lay out inline gets an opaque state slot and calls into the first engine's `update` and `combine` through the runtime ABI. The compiled engine lays out inline only `COUNT`, `SUM`, `MIN`, `MAX`, `AVG`, `ANY_VALUE`, `BOOL_AND/OR` and `COUNT(DISTINCT)` at first, the ones that are nearly all of the benchmark time.

**What the router records.** Every refusal is logged with its reason: the operator without a translator, the function that is not compilable, or the verifier rule that failed. `rudb-bench` reports the refusal rate per suite. That number is the coverage metric for the compiled engine, and it is published next to every performance claim. A claim like "10x on JOB with 0% refused" means something different from "10x on JOB with 20% refused", and both have to be stated.

**The setting.** `SET engine = 'auto' | 'vectorized' | 'compiled'`. The default is `auto`. `compiled` makes a refusal an error, which is how the coverage tests find gaps. `vectorized` is the escape hatch.

**Partial acceptance, from C9.** Once both engines exist and are correct, a plan can be split at a pipeline boundary. The compiled engine runs the pipelines it covers, and a bridge operator hands chunks to and from the first engine for the ones it does not. The bridge is a materialization point by construction, so it costs one copy, and it is never used inside a pipeline. It is scheduled late on purpose. Before C9 a refusal means the whole query runs somewhere with a known behavior, which is easier to reason about than a query that runs half in each engine.

## 3.5 Inside the compiled engine

Five stages between A4 and a running query. Each is a crate in document 19 and a document here.

**Physical planning (document 04).** Chooses an algorithm for every logical operator. Places the reduction filters and decides their schedule. Chooses build sides. Decides what representation each scan hands up (codes, decoded values, row ids). Attaches to each choice the **fact** that justified it and the **guard** that checks the fact at runtime. A fact is something like "`kind_type` has 7 rows", "`movie_info.info_type_id` is dictionary coded with 113 entries" or "`cast_info` is clustered by `movie_id`". Facts come from `../stats/` and `../graph/` and are exact, certified or estimated. Only exact and certified facts may be relied on without a guard.

**Pipeline decomposition (document 05).** Splits the physical plan at pipeline breakers: hash builds, aggregation, sorts, materialization points. Each pipeline becomes a step function with declared state: the hash tables it builds or probes, the aggregate tables it updates, the buffers it fills. The pipeline graph records the dependency order and the parallelism of each pipeline.

**Code generation (documents 06 and 07).** One linear pass over each pipeline produces one QIR function. The translator is layered: operator translators on top, then data structure helpers, then tuple and SQL-value helpers, then a typed builder over QIR. Only the SQL-value layer knows about NULLs, overflow and collations. The layering is Umbra's Tidy Tuples, adapted to Rust and made to append into a flat buffer with 4-byte value references so that generation stays under 0.1 ms for a JOB query.

**Backends (document 08).** An interpreter over QIR, `direct` (our single-pass emitter), `clif` (Cranelift, behind the cargo feature `qc-clif`, per the zero dependency rule in document 19) and, behind `qc-llvm` and off by default, `llvm`. All four take the same QIR function and produce something with the same calling convention: `fn(state: *mut PipelineState, morsel: *const Morsel) -> Status`.

**Execution (documents 09 and 13).** The scheduler hands morsels of a pipeline to worker threads. Each call runs one morsel through the current best function for that pipeline. Between morsels the policy may swap in a better-compiled function, check for cancellation, collect counters, or deoptimize a specialized function whose guard failed. Generated code calls the runtime library for everything that is not the hot loop.

## 3.6 What may appear in generated code

A list, because what is not on it is a call into the runtime.

- Arithmetic, comparison and bit operations on integers up to 128 bits and on floats, with the overflow checks DuckDB's semantics require.
- Loads and stores into morsel columns, state rows and scratch buffers whose layout was fixed at plan time.
- Hashing, as CRC32 chains with a multiply fold.
- The probe loop of the engine's own hash table, including the Bloom tag test and chain walk.
- The update step of inline aggregates.
- String comparisons and prefix tests that stay inside the 16-byte string header, with a call for anything longer.
- Branches, loops over a morsel, and loops over hash chains with a counted cancellation check.
- Calls into the runtime library through the ABI in document 13.

**Not in generated code.** Memory allocation, hash table growth, spilling, the buffer manager, catalog access, string construction, regex matching, decimal division, formatting, and anything that can panic. The split is what Umbra calls its cogwheel design, and it is the reason generated code stays small enough to compile in microseconds. Document 13 lists the runtime functions and their costs.

## 3.7 Threading and memory

**One thread pool for the process.** It is shared with the first engine and the storage layer's background work. The compiled engine submits pipeline tasks to it, and a task is a (pipeline, morsel range) pair. Background compilation also runs as tasks on this pool at a lower priority, never on a dedicated thread that competes with query execution for cores it does not own.

**Morsels are about 16,384 tuples by default.** They are aligned to storage row groups so that a morsel never straddles two encodings, which lets the generated code specialize per morsel. Inside a morsel, the scan kernel hands the generated body batches of 1,024 tuples with a selection vector, which is the unit at which vectorized decoding and SIMD filtering happen before the tuple-at-a-time body runs. Document 05 justifies both sizes.

**Memory for state is reserved at plan time and allocated by the runtime.** The physical planner reserves memory for each hash table and buffer from its size facts. The runtime allocates it at pipeline start. Generated code never allocates and never frees, which is the property that makes a morsel restartable: a deoptimized morsel can be rerun because the only thing it changed is state the runtime can roll back. Document 13, section 13.4.

## 3.8 Determinism

The plan is a function of the query and the data facts, never of timing or history. That is the rule `../planner-v2/` settled and it holds here.

The compiled engine adds one kind of nondeterminism that is allowed, and says so. **Which backend compiled a pipeline depends on timing.** It must never change the result. Every backend must produce bit-identical results for the same QIR function. The guarantee is per function and per thread count. Parallel floating-point aggregation merges thread-local partials in an order that depends on which worker took which morsel, so at more than one thread `SUM(DOUBLE)` can differ in the last bits between runs, exactly as in DuckDB. Document 15 compares multi-threaded results as sets with that tolerance, and bit for bit at one thread. That includes floating point, where no backend may reassociate, contract into fused multiply-add, or use a different rounding than the interpreter. Document 15 is how that is checked on every commit. A query whose answer changes when it gets faster is the worst bug this engine can ship, and the architecture is built so that finding one is a matter of forcing a tier and diffing.

## 3.9 What this architecture deliberately does not do

**No whole-query function.** Some research compilers generate one function for the entire query, which is simpler and wrong for us. It ties parallelism, cancellation, tier switching and spilling to control flow inside generated code, which is where they are hardest to get right.

**No source-to-source compilation.** Generating C++, C or Rust source and invoking a compiler costs seconds per query. HyPer 2011 measured 1.6 to 2.6 s per query with gcc, and Velox codegen measured up to 10 s. A C emitter exists as a debug backend for sanitizer runs (`rudb-qc-cdebug`, document 15) and is never on the query path.

**No MLIR.** LingoDB shows MLIR produces small, clean compilers, and it also spends about 80 ms per query in MLIR plus LLVM and later added a TPDE baseline mode to get around it. For a Rust engine it is also a large C++ dependency. The multi-level structure is worth taking, and we take it: A5, A6 and A7 are levels. The framework is not.

**No learned components on the query path.** A learned kernel selector such as CAKE is attractive at morsel granularity. It still makes the choice a function of history, which the determinism rule excludes. Document 09 section 9.7 keeps the morsel-level adaptivity and replaces the learned policy with a rule over measured properties of the morsel in hand.

## What we should take from this document

The compiled engine starts at A4, the rewritten logical plan, and owns everything below it: physical plan, pipelines, QIR, backends, runtime. Sharing the frontend is what makes the two engines comparable query by query. Owning everything below it is what lets the compiler choose algorithms the first engine never had.

The pipeline is the unit of compilation, and the step-function ABI is the contract every backend, the scheduler and the tiering policy share. Tier switches, cancellation, deoptimization and parallelism all happen at the boundary that ABI defines, so none of them needs support inside generated code beyond returning a status.

The first engine stays as oracle and fallback. `vcall` gives the compiled engine every function from day one. Operator translators decide acceptance, and the refusal rate is the published measure of coverage.

Generated code is the hot loop and nothing else. Everything rare, allocating or fallible is a runtime call that returns a status, which is what keeps functions small enough to compile in microseconds.

