# 8. How this lands in the vector engine

The requirement is that the graph layer be seamless with rudb's vector query engine, and "seamless" has to mean something falsifiable or it means nothing. Here it means three things. No operator has two implementations. No kernel has a graph variant. And there is no path through the engine that is entered by a graph query and not by an ordinary one. This document argues that all three are achievable, and the argument rests on a property rudb already has rather than on one it would need to acquire.

## 8.1 The property that makes this cheap

`crates/rudb-vector/src/vector.rs` defines a `Vector` as a logical type, a length, a validity and a *body*, and the body today is one of seven: flat, constant, sequence, dictionary, packed, views, external text. The engine's stated default, in `../07-execution.md` and demonstrated by the last dozen merged changes, is that a kernel works on the body it is given rather than on a decoded copy: #739 composes codes when a selection lands on a stable dictionary, #741 decides a minimum over a filed dictionary on the bytes, #743 decides a text comparison once per distinct value, #731 totals a column through a slice or a padded dictionary.

That is exactly the machinery a factorized execution engine needs, and the 2026 FFX result in document 01 section 1.5 is the paper that says so: the way to add factorization to a DuckDB-shaped engine is to add a vector type, not a processor. rudb is in a better position than the paper's subject, because rudb already has six non-flat bodies and a discipline for adding them, and because `for_each_layout!` already exists to make a missing arm a compile error rather than a wrong answer.

So the graph layer's entire footprint in the vector engine is **two new bodies**.

## 8.2 `Gathered`

```
Gathered {
    source: Arc<Vector>,
    rids: Arc<Vec<u32>>,     // or a packed form, see below
    offset: usize,
}
```

Row `r` of this vector is row `rids[offset + r]` of `source`. It is what a link join produces for every parent column, per document 05 section 5.2, and it is late materialization expressed in the type system: the values are not read until a kernel asks for them, and a column that is projected but never inspected before the final output is read once at the end, for the rows that reached the end.

It is very close to `Dictionary { codes, values, stable }`, which is `Gathered` with the additional promise that the code space is small and the values are distinct. That closeness is load bearing. Every kernel that has a dictionary arm already does the right thing for a gather: fold the operation over `source` once, then index. The difference is only in whether folding over `source` is cheaper than folding over the rows, which for a dictionary is always and for a gather is when the distinct count is low. So the dispatch rule is one comparison, `source.len()` against `len`, and below a ratio the kernel folds over the source and above it the kernel materializes. `../storage-v3/06-native-late-materialization.md` is where the same decision already lives for the scan, and this is that decision at one more place rather than a second mechanism.

The `rids` are `u32` when the source is under four billion rows, which is every source, and they may be a `Packed` buffer rather than a `Vec` when they came from a link column, so that the gather reads the bit-packed link directly without widening it first. That is one more arm and it is the arm that keeps the common case allocation-free.

Interaction with validity: a gathered row is null when the source row is null or when the `rid` is the *no parent* sentinel, so the validity of a `Gathered` is computed lazily from the source's validity and the sentinel test, and materialized only when a kernel asks for a validity bitmap. Interaction with selection: applying a `Selection` to a `Gathered` composes into the `rids` and copies nothing, which is exactly what #739 did for the dictionary body.

## 8.3 `Expanded`

```
Expanded {
    values: Arc<Vector>,
    starts: Arc<Vec<u32>>,   // len = values.len() + 1
}
```

Row `r` is row `i` of `values` where `starts[i] <= r < starts[i+1]`. It is the factorized form of document 05 section 5.7: one parent value shared by a run of consecutive output rows. It is a run-length encoding whose runs are determined by a join rather than by the data, which means every kernel that folds over runs, and `crates/rudb-kernels/src/aggregate.rs` has those, because run-length is one of the storage encodings, handles it by construction.

The three operations that have to be right:

**Aggregate over an expanded column.** `sum` is `sum(values[i] * (starts[i+1] - starts[i]))`, `count` is `starts.last()`, `min` and `max` ignore the multiplicities entirely. This is where the factor comes from, and it is the same arithmetic the run-length aggregate already does.

**A predicate over an expanded column.** Evaluate once per value, then expand the result, which is `values.len()` evaluations instead of `len`. On Q9's join of `lineitem` to `nation` through two hops, that is twenty five evaluations instead of six hundred million.

**Flatten.** One operation, one place, called by any operator that has not been taught the form. It is a gather with the positions derived from `starts`, so it is implemented in terms of `Gathered` rather than separately.

The selector and the cascade update that FFX specifies are the part rudb does *not* copy. FFX needs a bit-array selector per level and a dependency tree to propagate invalidations, because its factorization trees are branched and deep. rudb's expansions come from a join tree over primary and foreign keys, they are a chain rather than a branch in every TPC-H query, and a chain's invalidation propagates by applying the existing `Selection` to the existing bodies. When a workload appears that needs branched factorization, the cascade update is the thing to build, and document 11 records that this design has not proved it never will.

## 8.4 The kernels that change

None of them gain a graph concept. What they gain is an arm.

`crates/rudb-kernels/src/compare.rs` and `scalar.rs` gain the fold-over-source rule of section 8.2, which most of them already have in their dictionary arm and which becomes a shared helper rather than a copy. `aggregate.rs` gains the multiplicity rule of section 8.3, which its run-length arm already has. `cast.rs` casts the source and rewraps, which costs `values.len()` casts instead of `len`. The comparison of a `Gathered` against a constant, and the sum of an `Expanded`, are the two that carry the measured win and are the two to write first.

The row-loop lint that `crates/rudb-exec/src/group.rs` refers to applies unchanged: an arm that loops per row rather than per value is a regression in this layer specifically, since the entire point is that the loop is over the distinct side.

## 8.5 The operators that change

`crates/rudb-exec/src/join.rs` gains a third strategy beside the nested loop and the hash join, and the strategy has no gather-side state, which makes it the simplest of the three. `crates/rudb-exec/src/source.rs` and the native scan gain the ability to accept a `Rids` bitmap as a filter and to consult the link column's zone maps before decoding a part, which is the same interface `../engine/08-join.md` section 8.5 already requires for a runtime Bloom filter, so this is that interface with a different payload rather than a new one. `crates/rudb-exec/src/group.rs` gains the direct-addressed table of document 05 section 5.6, which is a `Table` whose probe is the identity.

`crates/rudb-exec/src/gather.rs` is not extended; the link join has no build side and so has nothing to gather in the old sense. That is worth saying because it is the clearest statement of what the layer is: the build side is on disk and was written once.

## 8.6 Parallelism

Morsel-driven, per `../engine/10-scheduler.md`, with no change. A link join is a single pipeline with no dependency edge, which is a strict simplification: a hash join is two pipelines and a scheduling constraint, and a link join is one pipeline that reads an extra column. The reduction passes of document 05 section 5.4 are each one parallel scan producing a bitmap, partitioned by `rid` range so that workers write disjoint words, per document 04 section 4.5.

Issue #512 says threads already cost twice the CPU at ten million rows and issue #510 says N instances of an aggregate is N hash tables. The link join does not have that second problem at all, since it has no table, and the direct-addressed group table of document 05 section 5.6 has it in a worse form, since a 600 MB array per worker is not affordable. So the direct-addressed table is shared and updated with atomics, or it is partitioned by `rid` range with each worker owning a range and rows routed to it, and the second is the one that matches the engine's existing partitioned aggregation in `../perf/06-partitioned-aggregation.md`. Routing by `rid` range is free because `rid` order is scan order.

## 8.7 Codegen

`../08-codegen.md` compiles pipelines with Cranelift. The two new bodies are two more cases in the same dispatch the existing seven go through, and the `Gathered` case in particular is one the compiler handles well: a fused filter over a gathered column is a load, an index and a compare, with no branch on the body because the body was resolved at compile time. Nothing here needs codegen to work and nothing here is blocked on it.

## 8.8 What would falsify "seamless"

Three observable things, any of which means this document was wrong.

A second scan implementation, or a second join operator file, or a `graph` module inside `rudb-exec` that is not just the link strategy. A kernel with a `if is_graph_query` in it. And a benchmark result that improves with the sections present and regresses with them absent by more than the cost of building them, which would mean the fallback path rotted because nothing exercised it, which is why document 09 section 9.2 runs the whole suite both ways on every commit rather than occasionally.
