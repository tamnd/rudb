# Aggregation and join

Two operators, most of the time on both benchmarks, and between them `crates/rudb-exec/src/group.rs` at 5,249 lines, `group_mixed.rs` at 582, `group_distinct.rs` at 478, `pairs.rs` at 352, `key.rs` at 334 and `join.rs` at 1,268. This document is what those become.

## 11.1 What is actually inside group.rs

It is worth listing, because the file's reputation is that it is a big hash aggregate and that is not what it is. It is eight or nine hash aggregates in a trench coat, and the types say so:

`DenseCount`, `DensePartition`, `FixedExchange`, `FixedRecord`, `FixedPartition`, `BigIntDistinctExchange`, `BigIntDistinctRuns`, `BigIntDistinctPartition`, `EncodedCountExchange`, `EncodedCountRecord`, `EncodedCountPartition`, `CompactNumeric`, `Spreading`, `Agreed`, `Rows`, `Built`, `Partition`, `Folding`, `Held`.

Each of those is a specialization that pays off on a real query. A count over a dense integer key really should be a direct addressed array rather than a hash table. A count over a dictionary column really should stay in code space. A `COUNT(DISTINCT bigint)` really should sort runs rather than build a set. None of these are wrong and this document does not propose removing any of them.

The problem is where the choice between them is made. All of it is inside one `impl Sink for Aggregate`, and the operator discovers which one it is while rows are arriving. That is why the file is five thousand lines and why it takes the commits: every new specialization has to interleave with every existing one, inside a hot loop, under a lock, with a correctness argument about the switch.

Two flags on `Built` are the clearest illustration. `partitioning` says the groups now live in radix partitions rather than one table per instance, and its comment explains that once it is true every table has to be scattered including one belonging to an instance that never grew, because otherwise a group comes out of `finalize` twice. `local` says a partitioned instance still keeps its own table per partition, is one way, and its comment explains that an instance takes the lock before handing a table in so it cannot be doing that while the switch happens.

Both comments are correct and both are load bearing. That is the point. Every strategy pair in that file needs an argument of that quality, the number of pairs grows quadratically in the number of strategies, and nothing about the design bounds the number of strategies.

## 11.2 Which of those choices are plan decisions

Sort them by what information they need, which is document 05 section 5.6's routing rule.

**Needs only the plan.** Nothing here. Aggregation strategy always needs a number.

**Needs a fact.** The choice between a dense direct addressed array and a hash table, which needs the key range and the distinct count. The choice between a shared table and per instance tables, which needs the distinct count and the memory reservation. The choice to stay in code space, which needs to know the column is dictionary encoded. The choice to answer the whole aggregate from a synopsis, which needs the certificate. Every one of these is a physical plan field from document 07, and `../stats/` now supplies every fact they need.

**Needs the data in front of it.** The moment to spill and how much. That is it. Document 09 section 9.5 keeps it at runtime and document 10 section 10.5 keeps a per chunk code width test at runtime.

So of the nine or so strategies in `group.rs`, the selection of all nine is a plan decision and only the spill transition is not. Today the selection of all nine happens at runtime.

## 11.3 The aggregation design

One logical node, one physical node with a strategy field, five strategies, each lowering to a program.

**Answered.** The result comes from a fact and no rows are read. `COUNT(*)` from the row count, `COUNT(col)` from the row count minus nulls, `COUNT(DISTINCT s)` from the dictionary size, `MIN` and `MAX` from merged zone maps, a top-k `GROUP BY` from a certified frequency synopsis when the certificate discharges. Document 04 section 4.3 governs the class each of these may read. This is the strategy with the largest speedup and the smallest amount of code, and it is the reason `../stats/` is worth its bytes.

**Direct addressed.** The key is an integer or a dictionary code with a known range, the range is small enough to index, and the table is an array. `DenseCount` and `EncodedCount` are this and they already work. What changes is that the planner decides it from the exact range in the zone map rather than the operator discovering it.

**Shared concurrent table.** One table, every thread inserts into it, no per thread copies and no merge phase. Document 02 section 2.6's evidence is that this is now competitive with partitioning on modern hardware and that the ticketing schemes that make it work have been measured. The case it wins is high cardinality with low skew, where per thread tables duplicate the whole key space thirty two times and the merge is the query.

**Partitioned.** Per thread tables, radix partitioned, merged at the end. `Built` and `Partition` are this. The case it wins is low cardinality, where each thread's table is tiny and the merge is trivial, and the case where memory is tight enough that a shared table cannot be reserved.

**Pre-aggregated.** A small fixed size per thread cache in front of either of the above, absorbing heavy hitters so that the hot group is updated in cache rather than contended. The frequency synopsis says whether there are heavy hitters, which is exactly the fact that decides whether this is worth its overhead.

The physical plan prints which one and prints the fact. `EXPLAIN (PHYSICAL)` on a ClickBench group by should read `strategy=direct` with the range that justified it, and if it reads `strategy=partitioned` on a query somebody expected to be direct, the reason is on the page.

## 11.4 Aggregate state

Column-wise, in parallel vectors, not row-wise in a slot table. `group.rs` already chose this and it was right: a sum over one column touches one vector, and a chunk of updates to the same aggregate is a contiguous write.

Three things change.

**The state is a declaration**, per document 08 section 8.4, with its width and its reservation computed from facts in document 07 section 7.7. A table sized from an exact distinct count does not rehash, and rehashing a large aggregate copies the whole thing.

**Every aggregate function is an `update` block with a different step**, taken from `crates/rudb-kernels/src/aggregate.rs` rather than from a match inside the operator. Adding an aggregate function stops being an edit to `group.rs`.

**The memory transition stays at runtime and gets printed.** The `partitioning` and `local` flags survive as a state machine on a declared state rather than as fields on a private struct, and `EXPLAIN ANALYZE` says whether the switch fired. A one way switch that nothing reports is a switch nobody can debug.

## 11.5 Distinct and mixed aggregates are plan rewrites

`group_distinct.rs` and `group_mixed.rs` exist because `COUNT(DISTINCT x)` and a mix of distinct and plain aggregates in one `GROUP BY` each need a different shape, and the shape was built as a second operator.

Both are rewrites. `crates/rudb-opt/src/distinct.rs` already is one, as `DistinctAggregateRewrite`, so the tree already contains the right idea. The general form: a distinct aggregate becomes a grouped aggregate on the pair of the group key and the distinct argument, then an aggregate over that result on the group key alone. A mix becomes the plain aggregates on one branch, the distinct ones on the pair on another, joined on the group key. Recent commits added exactly these shapes to the executor, which is the churn document 01 measured, and they belong in `rudb-opt` where they are a plan diff in a text file.

The cost of doing this properly is that the rewrite has to be good enough that the executor path can go away, and a naive rewrite is slower than the fused operator. That is a real objection and the answer is the physical plan: the rewritten shape is two aggregates and a join, each of which gets its own strategy field, and a grouped aggregate on a pair that direct addresses is not slower than the fused path was.

## 11.6 Joins

Less in need of rescue than aggregation. `join.rs` is 1,268 lines, the `Probe` stream and `Join` sink split is clean, and the null rule is per column rather than per table, which is correct and is the kind of detail that is painful to retrofit.

What is missing is the choice. `streamed` picks between the streaming probe and the buffered join from the join kind alone, which is a semantic constraint: `Right`, `Full` and `Mark` have to know which build rows never matched, so they cannot stream. That is right and it stays. But it means there is no cost based choice between join implementations anywhere in rudb, and there are at least four worth choosing between.

**Link join.** Follow a stored link. No hash table at all. Document 06 and `../graph/`.

**Hash join.** What exists.

**Nested loop.** For a tiny build side or a non-equi condition, where a hash table costs more than the loop.

**Reduction then join.** The join runs after a semi-join reduction has removed the dangling tuples, which changes the build side size and sometimes the build side choice. Document 06.

Two additions to the operators themselves.

**A runtime filter emitted by every build**, unconditionally unless the build side exceeds a size threshold, per document 06 section 6.7. The build has the keys already, so the filter is nearly free, and the SQL Server result from CIDR 2026 is that this is worth more in practice than anything clever done with it.

**A three-valued mark join.** rudb has a `MARK` join kind. What it does not have is the explicit null mode that distinguishes `IN`, `NOT IN`, `EXISTS` and `NOT EXISTS` when either side can be null, and Birler and Neumann's CIDR 2026 treatment makes that an algorithm with a stated null semantics rather than four special cases. `Node::MarkJoin` with a null mode, per document 05 section 5.3, and the mode is a field the physical plan prints.

## 11.7 How group.rs comes apart

In the order document 14 schedules it, and with the property that the engine is whole at every step.

First `group.rs` gets wrapped in a single block and lowered from a `PhysAggregate` node whose strategy field is always `partitioned`. Nothing is faster and nothing is slower. What changes is that the strategy is a printed field.

Then the `Answered` strategy lands, which touches no existing code at all, because it is a physical node that reads a fact and emits a constant. It is pure addition and it is the largest win.

Then the strategy selection moves out one at a time: dense, encoded count, shared. Each move deletes the runtime recogniser for that case from the operator and adds a planner rule and a lowering. The operator gets smaller with each one, which is the opposite of what has been happening.

Then the distinct and mixed paths become rewrites and `group_distinct.rs` and `group_mixed.rs` go away.

Then the remaining table maintenance becomes blocks, and what is left of `group.rs` is the spill state machine and the merge, which is the part that genuinely belongs at runtime.

The expected end state is a few hundred lines of runtime memory transition plus a set of blocks shared with the join, which is the whole point: a hash aggregate and a hash join build are the same four blocks over different state.

## 11.8 What says it worked

**Edits per operator file per week.** Document 13 makes this a tracked metric. If `group.rs` is still taking a third of the commits after the strategy selection has moved, the move did not take.

**Strategy coverage in the plan corpus.** Every strategy must be chosen by at least one query in the corpus and the count per strategy is published. A strategy nothing chooses is dead code with a maintenance cost.

**The differential test across strategies.** Every query in the corpus, run with each legal strategy forced by setting, must produce the same rows. This is the test that makes it safe to add a strategy, and it is impossible to write today because the strategies are not named.

## What we should take from this document

`group.rs` is nine aggregates in one operator, choosing between themselves at runtime, and the choice of every one of them is a plan decision that facts can now make.

Five strategies, named and printed, with the answered-from-statistics strategy first because it is the biggest win and the smallest change.

Distinct and mixed aggregates are plan rewrites and the tree already has the pass that proves the shape works.

Joins need a cost based choice between four implementations, a runtime filter emitted unconditionally by every build, and a mark join with an explicit null mode.

The decomposition is incremental at every step, the operator shrinks with each move rather than growing, and the metric that says it worked is edits per file per week.
