# Layer one: the data plane

This is sub-milestone 2b, the first layer above the baseline, and it is the layer every other document in this directory is written against. It covers the vector, the chunk, validity, strings, selection and the shape of a batch. It does not cover any operator. It covers the thing all of them touch.

The reason it is first is the reason document 07 of the parent spec already gave: changing the vector interface after twenty operators are written against it costs twenty rewrites. There is now a second reason, which is that the interface as built has a defect that makes every kernel in the workspace row-at-a-time, and no amount of work in layers two through ten fixes a layer one that hands out one boxed value per row.

## 3.1 What exists today

`crates/rudb-vector` is 1950 lines across six files and most of the design is right.

`Vector` is a logical type, a length, a `Validity` and a `Body`. `Body` has four variants and they are the four forms the parent spec asks for: `Flat(Data)`, `Constant(Box<Value>)`, `Sequence { start, step }` and `Dictionary { codes: Vec<u32>, values: Box<Vector> }`. `Form` is the public projection of that, so an operator can ask what shape it has without seeing the payload.

`Data` is keyed by physical type rather than by logical type, which is the decision that lets DATE and INTEGER share one loop, and it has the full integer ladder from `Int8` through `Int128` and the unsigned mirror of it. That is the right call and it does not change.

`Validity` is three states, `AllValid`, `AllInvalid` and `Mask(Bitmap)`, with the Photon argument for the all-valid fast path written into the module doc. Also right, also does not change, with one addition in section 3.4.

`Chunk` is at most `VECTOR_SIZE` rows of columns, and `Chunk::select` takes the chunk by value and turns every column into a dictionary vector whose codes are the selection. That is the correct shape for a filter and it is genuinely free at the point of the filter, because the payload is moved and not copied.

`StringView` is 16 bytes, 4 of length and 12 of payload, with the payload being either the whole string or a 4 byte prefix plus a 4 byte block index plus a 4 byte offset into a `StringColumn` whose blocks are append only. The divergence from the parent spec, which asks for a pointer in the last 8 bytes, is deliberate, is Arrow's `StringView` layout rather than Umbra's, and is recorded as a divergence in the module doc rather than being quietly done.

`Selection` is a `Vec<u32>` with the empty selection distinguished from the absent one by being a type rather than an `Option`.

So the shapes are there. What is missing is the thing that makes the shapes pay.

## 3.2 The finding that sets the agenda

`Vector::value_at` has this in its doc comment, and it is correct:

> This is the slow path on purpose. It is what a result set is read out with and what a test asserts on, and an operator that calls it per row is an operator that has already lost the argument the vector interface exists to win.

Every kernel in `crates/rudb-kernels` calls it per row. There are 31 call sites of `value_at` and `from_values` across the five kernel files, and the comparison kernel is the clearest case. After a fast path for constant against constant, `compare` is a loop from zero to length that calls `left.value_at(index)`, calls `right.value_at(index)`, calls `compare_values` on the two owned `Value`s, and pushes the result into a `Vec<Value>` which is then handed to `from_values` to be turned back into a bitmap.

Count what one row of `WHERE url = 'x'` costs in that loop. Two `Value` constructions, and for a varchar column `Value::Varchar` holds a `String`, so that is a heap allocation and a memcpy of the string body per row per side. One `Value` clone on the constant side. A match on the comparison operator, inside the loop, per row, which is a branch the compiler cannot hoist because it cannot see through the enum. A `Vec<Value>` of a thousand boxed booleans as the output. Then `from_values` walks that vector and packs it into a bitmap, which is a second pass over data that was just written.

That is somewhere between fifty and two hundred times the cost of the loop this should be, which is a load of two 16 byte views, a 4 byte prefix compare that resolves the great majority of inequalities without touching the block, and a bit set in a word. It is also the answer to the question the corpus raised when four files timed out at ten seconds on joins of ten thousand rows against fifty thousand. Five hundred million comparisons, each allocating two strings, is ten seconds, and the nested loop join is only half the story. The other half is that the comparison inside it is not vectorized at all.

This is the single most valuable thing found in six months of building, it was found by reading the code rather than by profiling, and it means layer one is not a refactor for tidiness. It is where the first order of magnitude is.

## 3.3 The interface that fixes it: dispatch once, loop tight

The problem is not that kernels are written badly. It is that the vector interface offers exactly two ways to read a vector, one value at a time through `value_at`, or `flatten()` into `Data` and then match on the physical type by hand. The first is correct for every form and unusably slow. The second is fast and forces every kernel author to write the flattening, so nobody does.

There are two known answers and rudb takes a hybrid of them.

DuckDB's answer is `UnifiedVectorFormat`. Every vector, whatever its form, can be presented as a data pointer, an optional selection vector and a validity mask, and every kernel is written once against that presentation. The cost is one indirection per row forever, including on flat vectors where the selection is the identity, because the kernel cannot see that it is the identity. DuckDB accepts that cost and it is a real one, visible in their own microbenchmarks as the gap between a flat and a dictionary path that should not exist.

ClickHouse's answer is the opposite. `IColumn` is virtual, dispatch happens once at the top of an operation, and what runs after the dispatch is a concrete loop over a concrete `PODArray` with no per-row indirection at all. The cost is code size and a combinatorial number of specializations.

rudb does both, split by which case it is. The kernel entry point matches once on the pair of forms and the physical type, and it has hand-written specializations for the three combinations that are almost all of real work: flat against flat, flat against constant, and dictionary against constant, which is the shape immediately after a filter. Everything else falls through to a unified presentation that is correct and slower, and the fallback is instrumented so that a form combination showing up hot in a real query is a signal to specialize it rather than a guess.

The mechanism is a macro, because writing fifteen physical types by three form pairs by eight comparison operators by hand is how a wrong answer gets in. The signature the macro generates is a closure over the physical slice:

```rust
pub fn binary<L, R, O>(left: &Vector, right: &Vector, op: impl Fn(L, R) -> O) -> Result<Vector>
```

with the operator passed as a monomorphized closure so the branch on which comparison it is happens once, outside the loop, and the loop the compiler sees is `a[i] < b[i]` over two slices with a known type. That is the loop that autovectorizes. The current loop cannot, and no amount of target feature flags will make it.

Three rules hold this in place, and each one is a test rather than a convention.

`Value` is not constructed inside a loop bounded by a vector length anywhere outside `rudb-vector` and the result reader. This is checkable by a lint in `xtask` and it goes in with this layer, because a rule that is only in a document gets violated in month three.

`flatten()` is not called on any path an operator reaches. It has exactly one legitimate caller today, `database.rs:196`, which is materializing a result set, and that one is fine. A second caller appearing inside an operator is a defect.

Every kernel's fallback path increments a counter, and the counters are dumped by the benchmark harness. A fallback taking more than one percent of a suite is a bug filed against this document.

## 3.4 Validity

Three states stay. One thing is added and one thing is fixed.

The addition is that a kernel has to be able to compute the output validity without looking at the input rows at all in the common case, which is what the three states are for and what the code does not yet exploit. `AllValid` against `AllValid` is `AllValid`, and the kernel that establishes that with one comparison of two enum discriminants has saved itself a bitmap AND over a thousand rows. `AllInvalid` on either side of a null-propagating operator is `AllInvalid` and the data loop does not run at all. Those two cases are most of ClickBench, because most of `hits` is not null, and they are currently paid for in full.

The fix is that validity and selection interact and the interaction is not written down. When a vector is a dictionary over a parent, there are two validities in play, the dictionary's own and the parent's, and a null can come from either. The rule is that the dictionary's validity is authoritative for a position and the parent's applies to the value the code points at, and a position is valid only if both are. Getting this wrong produces a wrong answer under a filter over a nullable column, which is the kind of bug that survives a year because it needs a filter and a null and a specific form to reproduce. It gets a property test in section 3.12.

The bitmap itself wants two operations it does not have: `count_ones` maintained incrementally rather than recomputed, so that an operator can ask how many rows survive without a pass, and a fast path in `and` for the case where one side is all valid. Both are small and both show up in profiles.

## 3.5 Strings

The layout question is the one real open decision in this layer, and this section closes it.

The current layout is length in the first 4 bytes, and then either 12 bytes of inline string or 4 bytes of prefix plus a 4 byte block index plus a 4 byte offset. The parent spec asks for a pointer in the last 8 bytes, which is what DuckDB and Umbra do.

The argument for the pointer is one load rather than two on the long-string path, and no bounds check on the block index. The argument for block and offset is that the representation is safe code, it survives being moved between threads and being written to disk without fixup, and it does not need a pinning discipline that does not exist until the buffer manager arrives.

The survey in document 01 found the number that decides this. DataFusion migrated arrow-rs from `StringArray` to `StringView` and got large wins on ClickBench string queries, and the migration took more than a hundred pull requests. Almost none of that hundred was the layout. It was propagating the layout through every kernel, every operator, every cast and every aggregate that had assumed the old one.

So the cost of this decision is not the load. It is the migration, and the migration is cheap exactly once, which is now, at layer one, with five kernel files and no real operators. The decision is therefore: **keep block and offset, and stop treating it as provisional.** The extra load is on the path that is already going to main memory for the body, where one predictable load off a small block table that stays in L2 disappears into the cache miss it accompanies. If a later measurement says otherwise, the change is contained inside `string.rs` and the accessor, because nothing outside `rudb-vector` will be allowed to see the payload bytes.

The issue tracking this stays open but changes shape. It is no longer "decide the layout at M3". It is "measure the long-string path at layer three when the buffer manager exists, and record the number", which is a measurement rather than a decision.

Three things are missing and go in with this layer.

The prefix is computed and never used. `StringView::prefix` exists and no comparison calls it. A 4 byte prefix comparison decides the great majority of inequality comparisons on real URL data without touching the block at all, which is the entire point of the representation, and on `hits` the URL and Referer columns are where the time goes. This alone is expected to be worth more than anything else in this document on the ClickBench string queries.

There is no way to build a `StringColumn` without copying, which means a scan that reads a Parquet page of strings copies them into blocks and then the query reads them out of the blocks. The block should be the page where the format allows it, and that seam has to exist in the type before layer three can use it.

`StringColumn::blocks` is `Vec<Vec<u8>>`, which is one allocation per block and a pointer chase to get at a block. It becomes one arena with block boundaries recorded as offsets, which makes the block index an index into a small offset table rather than into a vector of pointers.

## 3.6 Selection, compaction, and the stacking problem

`Chunk::select` turns each column into a dictionary over the old column, which is right. What is not handled is what happens on the second filter.

Two filters in a row produce a dictionary whose values are a dictionary, and three produce three levels, and every read at depth n is n indirections. A `WHERE` clause with four conjuncts, which is common in TPC-H and in ClickBench, builds four levels if each conjunct filters separately. Nothing in `Vector::dictionary` collapses them.

The fix is compulsory and it is small. `Vector::dictionary` checks whether `values` is itself a dictionary, and if it is, composes the codes and points at the grandparent. Composition is a gather of one `Vec<u32>` through another, it is cheap, and it makes the depth invariantly one. The invariant is then asserted in debug builds, because a second place that constructs the body directly will otherwise reintroduce the problem.

Then the harder question, which is when to stop carrying a selection and compact.

The current code never compacts, and the module doc gives the correct reason for the default: a filter over five columns that compacts has copied five columns to save the next operator a redirection, and if the query then projects two of them, three copies were wasted. That reason is right and it is not the whole story. The other half is that a dictionary vector at ten percent selectivity is a random gather over a range ten times larger than the output, which destroys the sequential access the hardware prefetcher was giving the flat loop, and at some selectivity the copy is cheaper than the gathers that follow it.

DuckDB uses a fixed threshold. The Data Chunk Compaction paper at SIGMOD 2025 shows that a fixed threshold is wrong in both directions depending on how many operators the chunk passes through afterwards and how wide it is, and proposes deciding with a gain function over the estimated downstream cost rather than a constant.

rudb takes the gain function and makes the inputs concrete rather than estimated, because in a pull-based pipeline the operator knows its own downstream at plan time. The decision is made once per pipeline at plan time and not per chunk at runtime, using the number of columns the pipeline still reads after this point, the number of operators between here and the next materialization, and the measured selectivity so far in this pipeline. Compaction is a plan property, `compact_after: bool` on the filter, revisited by the adaptivity layer in document 12 when the measured selectivity turns out to differ from the estimate by enough to change the answer.

What layer one owes is the mechanism and the measurement, not the policy. The mechanism is `Chunk::compact`, which is the copying counterpart to `Chunk::select`. The measurement is a microbenchmark that sweeps selectivity from 0.1 percent to 100 percent across chunk widths of 1, 5 and 20 columns and pipeline depths of 1, 3 and 8, and produces the surface that the gain function is fitted to. That surface is a deliverable of 2b and it goes in `rudb-bench` as a committed data file, because the constants in the gain function have to be traceable to a run rather than chosen.

## 3.7 How big is a vector

`VECTOR_SIZE` is 1024. DuckDB uses 2048. Velox uses 1024. Neither number has an argument attached to it in this project, and the constant is used in three places so it is still free to change.

The argument that matters is what has to stay resident while an expression tree is evaluated. A five-node expression over 8 byte data at 1024 rows is five intermediates of 8 KB, which is 40 KB, and it does not fit in a 32 KB L1d and does fit in a 128 KB one. At 2048 it is 80 KB and fits in neither of the first and still fits in the second. The fleet has both kinds of machine, the reporting target `c6a.4xlarge` is 32 KB per core, and the laptop is 128 KB, which is exactly the configuration where a constant tuned on the laptop is wrong on the machine the number gets published from.

So it gets measured, on `server3` and on the laptop, sweeping 256, 512, 1024, 2048 and 4096 over the ClickBench scan-and-aggregate queries and over TPC-H Q1 and Q6. The output is either a number with a reason or a finding that the curve is flat between 512 and 2048, which is the likely outcome and is itself worth knowing, because it means the constant is not where the performance is and nobody needs to argue about it again.

The constant stays a constant and does not become a runtime parameter. A runtime vector size means every kernel has a dynamic bound where it could have a known one, and the whole point of the number is that the compiler knows it.

## 3.8 Ownership, and the seam for the buffer manager

`Data` holds `Vec<T>`, which means every vector owns its payload and every scan allocates. That is the correct starting point and it is not where this ends.

At layer three the scan reads from a buffer manager and the vector wants to point into a pinned page rather than copy out of it. The type that supports both is one enum in `Data`, holding either an owned `Vec<T>` or a borrowed slice with a pin guard, and the choice of how to express the lifetime is the part that has to be right the first time because it is in every signature.

The decision is that the pin is a reference-counted handle held by the vector rather than a Rust lifetime parameter on `Vector`. A lifetime parameter on `Vector` propagates to `Chunk`, to every operator's state, to every trait object in the pipeline, and to every place a chunk is put in a queue for another thread, which is exactly what the scheduler in document 10 does. Rust will not let a borrowed chunk cross that boundary, and fighting it produces either unsafe code or a copy at the boundary, and the copy at the boundary is the thing being avoided. A refcounted pin handle costs an atomic increment per vector construction, which is nothing against a page read, and it makes the ownership story uniform.

This layer does not implement the buffer manager and does not implement the borrowed variant. It puts the enum in with only the owned variant present, so that adding the second variant at layer three is a change inside `rudb-vector` and not a change to every signature in the workspace. That is the whole reason it appears in layer one.

## 3.9 The encoded form is deferred, the seam is not

Section 4.3 of the parent spec asks for a fifth form, a vector that holds encoded data and runs predicates against it without decoding, which is where the ten times claim on scans is supposed to come from and which is M3 in the milestone plan.

It is not built at layer one. It cannot be built usefully before the scan layer exists, because what makes it pay is a predicate pushed into it from a scan over a real encoded column, and there is no such scan yet.

What layer one owes is that adding the variant later is not a breaking change. That means `Form` is non-exhaustive from the start, every match on `Form` outside `rudb-vector` has a fallback arm rather than an exhaustive list, and the fallback arm is the unified presentation from section 3.3. Adding `Form::Encoded` then makes every existing kernel correct immediately and slow on encoded input, and specializing them is incremental rather than a flag day. Without that, the day `Form::Encoded` lands is a day every kernel stops compiling, and a change that breaks everything at once is a change that gets postponed.

## 3.10 The scheduler contract, imposed now

Document 00 argued that the scheduler's interface goes in at layer one and its implementation at layer eight. The part of that interface which lands in the data plane is small and it is the part that is expensive to retrofit.

A `Chunk` is `Send`. That is the whole requirement and it is not free: it rules out `Rc` anywhere in a vector, it rules out any borrow of thread-local state, and it is what the pin handle decision in section 3.8 is protecting. It is enforced by a static assertion so that the day something non-Send is added to a vector body is the day the build fails, rather than the day the scheduler is written.

Nothing in `rudb-vector` may hold a global or a thread local. The string blocks are per column and not per thread, which they already are.

## 3.11 What this breaks and how it lands

The five kernel files are rewritten. That is 1934 lines and it is most of the work in this layer. `compare.rs` and `scalar.rs` are the two that matter and they go first, `logic.rs` is small, `cast.rs` is mostly a per-type table and mechanical, and `aggregate.rs` is deliberately last because layer five replaces it anyway and rewriting it twice is waste.

`compare_values` and `order` stay. They are the row-at-a-time definitions of SQL semantics, they are the one written-down place the sort order of a type lives, and the vectorized kernels are checked against them by property test rather than replacing them. That is the standard trick for this rewrite and it is what makes it safe: the slow path becomes the oracle.

The corpus pass rate must not fall. This is a pure performance rewrite of code whose semantics are already covered by `rudb-compat`, so any movement in the pass rate is a defect in the rewrite and not a change in scope. That is the strongest available check and it is the reason this layer is safe to do aggressively.

The rewrite ships as a sequence of pull requests, one per kernel file, each green on the full gate and each with its microbenchmark number in the body, rather than as one change. A single pull request that rewrites 1934 lines is a pull request nobody can review and nobody can bisect.

## 3.12 The test gate

Property tests are the mechanism, because the space is form by type by nullability and it is too large to enumerate by hand and too structured to sample randomly by hand.

For every kernel, for every pair of forms, for every physical type, generate a random vector with a random validity and assert that the vectorized result equals the result of the row-at-a-time oracle position by position, including which positions are null. That is the test that catches the dictionary-over-nullable-parent bug from section 3.4 and it catches it the first time it runs.

Round trip properties on the forms: flattening any form and comparing to the original by value must agree; composing two selections and applying them must equal applying them in sequence; a constant vector of length n must equal a flat vector of n copies for every operation.

Strings specifically: the inline boundary at 12 bytes gets exact cases at 11, 12 and 13 bytes, the prefix path gets pairs that agree in the first 4 bytes and differ later and pairs that differ inside the first 4, empty strings, and non-ASCII where the byte order and the character order differ. The prefix comparison being wrong on a pair that shares a prefix is the single most likely defect in this layer and it must be impossible for it to reach a release.

The `Send` assertion, and the lint that fails the build on `Value` construction inside a vector-length loop outside the allowed files.

Everything runs on `gamingpc` as well, because the arena change in section 3.5 is the kind of change that has an alignment assumption in it.

## 3.13 The benchmark gate

Microbenchmarks first, in `rudb-bench` under a new `kernels` suite, all of them against rudb only because there is no comparable single-kernel entry point in DuckDB or ClickHouse to run them against.

Comparison of 1024 rows for every physical type, in all four form pairs, at 0, 1 and 50 percent nulls. Arithmetic the same. String comparison at prefix-decides and prefix-ties. The compaction surface from section 3.6. The vector size sweep from section 3.7. Every one of these is reported as nanoseconds per row and as rows per second per core, and the target for the fixed-width flat-against-flat comparison is under one nanosecond per row, because that loop is a load, a compare and a bit set and anything above a nanosecond means it did not vectorize.

Then the whole-query gate, which is the one that decides whether the layer is done, and which is the rule from document 00 applied here.

ClickBench Q1 to Q5 are scans with simple filters and aggregates, and they are the closest thing to a direct measurement of this layer at whole-query level. TPC-H Q6 is a filter over four predicates on one table and it is the cleanest test of the compaction decision and of predicate evaluation. TPC-H Q1 is an aggregate over a filter with arithmetic in it. Those seven queries are the layer's whole-query gate, on `server3`, against the 2a baseline and against DuckDB.

The claim to be established is not that rudb beats DuckDB on those seven at layer one, because the scan underneath is still naive and the aggregate is still layer five. It is that CPU seconds on those seven fall by at least a factor of five against the 2a baseline, and that the ratio against DuckDB moves in the right direction by a comparable amount. A factor of five is the conservative reading of what removing an allocation per value costs, and if the measured number is a factor of one and a half, the rewrite did not do what this document says it does and the reason has to be found before layer two starts.

## 3.14 Exit criterion for 2b

**The five kernel files are vectorized with no `Value` on any loop path, the property tests pass against the row-at-a-time oracle for every form pair and every physical type, the corpus pass rate has not fallen, the `kernels` suite is committed in `rudb-bench` with the compaction surface and the vector size sweep, and CPU seconds on the seven gate queries have fallen by at least five times against the 2a baseline on `server3`.**

Plus the ledger row, which is the first real row: what layer one bought, on which suite, on which machine, against which commit range.

Three things are explicitly allowed to be unfinished when this closes. The buffer manager is not built and `Data` has only its owned variant. `Form::Encoded` does not exist. The compaction policy is a constant fitted to the measured surface and is not yet adaptive. All three are named in the exit note so that a later reader knows they were deferred rather than forgotten.
