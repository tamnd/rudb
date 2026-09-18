# The execution model

This is the shortest document in the folder that matters, because the execution model is the part of rudb that is already right. Most of it is a record of what not to change and why, and the changes it does ask for are all about where a number comes from rather than what the machinery does with it.

## 9.1 What is already right, stated so nobody relitigates it

`crates/rudb-pipeline` is 3,000 or so lines across twelve modules and every significant decision in it is defensible on its own documented grounds.

**Three traits, push based.** `Source` produces, `Stream` transforms, `Sink` consumes. `Source::read`, `Stream::push` and `Sink::sink` all take `&self`, so an operator is shared across every thread running it and the per instance state is an associated type. That forces mutable state into `Stream::Local` and `Sink::Local` where it can be seen and counted, rather than letting it accumulate in a `&mut self` nobody audits.

**`Sink::combine` takes its local state by value.** A local state that has been merged is gone, so merging it twice does not compile. That is a small piece of design doing real work, and it is the model document 08 section 8.4 maps declared state onto.

**Four-way `Progress`.** `More`, `Again`, `Done` and `Blocked`. `Again` is the one people would leave out and it is the one that keeps a cross product from holding a million rows: an operator whose one input chunk becomes several output chunks says `Again`, the driver hands the current chunk on and calls it back. `Done` at a `Stream` or `Sink` means no more input is wanted at all, which is how a `LIMIT` stops a scan instead of filtering rows it should not have read.

**A closed set of four blocking reasons.** `Io`, `Memory`, `Dependency` and `Downstream`. The crate's own argument for closing the set is the right one: with four kinds of edge the wait for graph is finite, deadlock is enumerable, and the scheduler can assert acyclicity at plan time and again when every task is blocked. An open ended wait token design gives more exact backpressure and costs the ability to find a deadlock by reading a graph. For a project whose test strategy is a large corpus under many strategy combinations, enumerable beats elegant.

**Order restoration at the root, by morsel index.** DuckDB's minimum batch index scheme, and it works because every streaming operator transforms a chunk in place and none merges chunks across morsels, so a chunk arriving at the sink came from exactly one morsel.

None of that changes. The traits in `crates/rudb-pipeline/src/traits.rs` do not change. Document 08 was written specifically so that they do not have to.

## 9.2 Push, and the two places pull survives

Push is right for an analytical engine and the reasons are the usual ones: the data stays in cache across operator boundaries, there is no per-row virtual call, and an operator with two inputs does not need a coroutine.

Two places keep a pull shape and both are deliberate.

**The root.** `RootReader` pulls, because whoever is consuming the result is outside the engine and is not going to be called back. `root.rs`'s note is worth keeping: taking a lock once per chunk at the root of a query is not a cost worth avoiding, and the alternative is a per instance buffer holding the whole result until the query ends.

**A scan of an external format that hands back its own batches.** Parquet and Arrow readers are pull shaped and wrapping them is cheaper than inverting them.

## 9.3 Morsels, and what changes about them

Morsel-driven parallelism, unchanged in mechanism. What changes is the input to two numbers.

`Source::morsels(threads)` currently answers from whatever the source happens to know. Under document 07 section 7.6 it answers from a plan decision made from facts: the part count is exact, the rows per part are exact, and the estimated output cardinality has a class. The two decisions that unblocks are a small query staying on one thread, and a skewed input getting finer morsels near the heavy value because the frequency synopsis said where it is.

The rule that keeps morsel sizing honest is that **a morsel is a unit of work, not a unit of data.** A morsel over a heavily filtered part is less work than a morsel over an unfiltered one, and when zone maps have already told the planner which parts survive, the morsel cut should be over surviving parts rather than over all of them. That is a one line change to a scan and it is worth naming because the current behaviour silently gives some threads nothing to do.

## 9.4 Order

Three levels, and confusing them is how an engine ends up either slow or wrong.

**Order the query asked for.** An `ORDER BY` at the top. Guaranteed, and produced by a sort or by a plan the sortedness fact proved was already ordered.

**Order the query did not ask for but a user will see.** A `LIMIT` without an `ORDER BY`, or a plain `SELECT` from a table. rudb restores source order at the root when the setting asks for it, which is what `root_in_order` is, and it costs the holding described in `root.rs`. This is a compatibility surface rather than a semantic one and `../12-duckdb-compat.md` owns how far it goes.

**Order inside a pipeline.** Not guaranteed between morsels and never was. Anything that depends on it is a bug, including in tests.

The rule for this folder: **an operator may not rely on an order it did not require.** A join that happens to see its build side in source order has not been promised it. When an operator does require an order, it says so as a physical plan field, so that the requirement is visible to the thing that might otherwise remove the sort.

## 9.5 Memory, and the one decision that stays at runtime

Document 07 section 7.7 reserves memory ahead of a query from facts. This section is about what happens when the reservation turns out to be wrong, which it will, because a distinct count over a filtered input is an estimate even when the distinct count over the whole column is exact.

The position, from document 02 section 2.7 and Saving Private Hash Join: **spilling is an execution-time decision and the degradation has to be continuous.**

Continuous means an operator under memory pressure gets slower in proportion to how much it is over, rather than crossing a threshold and changing algorithm. A hash aggregate that is ten percent over should spill ten percent of its partitions, not restart as an external aggregate. The failure mode a threshold produces is the one everybody has seen: a query that runs in two seconds at one row count and forty seconds at one row more, because a hard switch fired.

Three things make continuous degradation implementable here rather than aspirational.

**State is declared and partitioned.** Document 08 section 8.4. Spilling a partition is a defined operation on a named thing with a known size, rather than an operator improvising over whatever it is holding.

**The memory token already exists.** `Blocked::Memory(MemoryToken)` means an operator that cannot get a reservation parks and the scheduler runs something else, which is already the right shape for backpressure. What is missing is an operator that responds to refusal by shedding rather than only by waiting.

**The decision is per partition, per instance.** Nothing global is consulted and no thread waits on another to decide. A thread that is over its share spills its own partition.

What the planner still does: reserve, set the threshold, and choose a partitioned grouping strategy over a shared one when the reservation cannot be met. What execution does: decide the moment and the amount.

## 9.6 The seam, and what it is actually for

`rudb-seam` is one of the better ideas in the tree and it is easy to mistake for configuration machinery. It is not. It is the mechanism that lets a decision have two implementations and have the choice between them be measurable, named, and switchable in a test.

The rule that makes it affordable is the crate's own: **a seam is crossed once per chunk, never once per row.** Every seam method takes a whole chunk, column, morsel or partition, or a decision already made at plan time.

One correction to make while looking at it. The seam crate's documentation argues the cost is "an indirect call every 122,880 rows", and `root.rs` describes a chunk as "a hundred and twenty thousand rows". But `VECTOR_SIZE` in `crates/rudb-vector/src/vector.rs` is 1,024, deliberately, for the FastLanes unit and for a validity mask of exactly sixteen `u64` words, and `join.rs` and `rows.rs` both fill chunks to `VECTOR_SIZE`. A chunk is 1,024 rows and 122,880 is the row group. The argument still holds comfortably at 1,024, because an indirect call per thousand rows is still nothing, but the documentation is off by two orders of magnitude in its own favour and should say 1,024. This matters beyond pedantry: someone sizing a future seam against the wrong number will put one somewhere it does not belong.

The seam's `Determinism` enum is the other thing worth carrying forward into document 12. `Exact`, `PerThreadCount` and `None`, with `PerThreadCount` documented as the honest answer for a parallel floating point aggregate and `None` documented as something nothing should declare without a written reason. That is exactly the vocabulary the adaptivity rules need and document 12 uses it rather than inventing another.

## 9.7 The `Gauge`, and the thing it is allowed to learn

`compact.rs` holds the only piece of runtime learning in the tree today. `Compaction` decides whether a filter's surviving rows are copied out or left as a selection, and `Gauge` carries two things per pipeline instance: how many more times the kept rows will be read, which is a plan time fact, and how fast this machine copies bytes, which is measured while the query runs and folded back in.

That split is exactly right and it is the template for every future case, so it is worth stating as a rule.

**An implementation may learn a property of the machine. It may not learn a property of the workload.**

Nanoseconds per byte is a machine constant that a benchmark cannot tell you in advance and that does not vary with the query. Learning it changes no plan and changes no answer, and if the measurement is wrong the only cost is a copy that was not worth doing. A selectivity, a good join order or a preferred strategy is a property of the workload, and learning one makes the plan a function of history, which document 12 forbids.

The per instance rather than per process placement is also right, and the crate says why: thirty two threads running one pipeline learn thirty two times rather than fighting over one number.

## 9.8 What may never be in an operator

The list, which is document 03 section 3.3's corollary made concrete enough to use in review.

**No test on the number of aggregate calls, group keys, or join conditions.** All known at plan time.

**No test on an expression's shape.** Whether the argument is a cast, whether the condition is an equality, whether the predicate is a conjunction. All known at plan time and all currently tested at runtime somewhere in `rudb-exec`.

**No test on a type that does not change which instruction runs.** Document 08 section 8.7.

**No decision about parallelism.** `Stream::parallel` and `Sink::parallel` are declarations of what an implementation supports, not decisions about what to do, and that distinction should stay visible.

**No catalog lookup.** An operator is handed what it needs.

**No allocation that nothing declared.** Document 08 section 8.4.

What an operator may do is test the data in front of it: this chunk's selection density, this chunk's code width, this partition's size against the cache. Those are properties of a chunk and they are the entire legitimate content of the layer below artifact 6.

## What we should take from this document

The execution machinery is right and stays. Three traits taking `&self`, four-way `Progress`, a closed set of four blocking reasons because deadlock should be enumerable, `combine` by value, order restored at the root by morsel index.

What changes is the supply. Morsel counts and parallel degree come from facts through the physical plan instead of from constants, and a morsel is cut over surviving parts rather than over all parts.

Spill stays at runtime, degrades continuously rather than switching algorithm at a threshold, and attaches to a declared partitioned state rather than to whatever an operator is holding.

An implementation may learn a property of the machine and never a property of the workload, which is the `Gauge` split generalised, and it is the rule document 12 is built on.
