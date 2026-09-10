# Layer five: aggregation

This is sub-milestone 2g. It sits directly on the hash table from document 06 and it is where most of the remaining ClickBench time lives once the scan is fast, because after the scan and the filter every ClickBench query is a `GROUP BY` and a count or a sum.

It is also the first pipeline breaker in the engine, which means it is the first operator whose contract with the scheduler is non-trivial, and section 7.9 is about that contract even though the scheduler is three layers away.

## 7.1 What exists today

`crates/rudb-kernels/src/aggregate.rs` is 301 lines and defines `Accumulator`, which is a `Kind`, a return type and a `State`. Six aggregates: `count_star`, `count`, `sum`, `avg`, `min` and `max`. The state is a Rust enum with variants for a count, a whole running total in `i128` with a seen flag, a floating total with a count, a decimal total at a fixed scale, and `Extreme(Option<Value>)` for min and max. `update(&mut self, args: &[Value])` folds one row in. `finish(&self) -> Result<Value>` produces the answer.

The numeric care in it is real and worth keeping. `sum` over integers accumulates in `i128` and reports overflow rather than wrapping, `sum` over a decimal keeps the scale, `avg` over integers goes through a float total with an integer count, and the empty case gives null for `sum` and zero for `count`. Those are the details DuckDB gets right and a first implementation usually gets wrong, and they are already right here.

`group.rs` is 291 lines and drives it: a `HashMap<Key, usize>` from group key to slot, a `Vec<Accumulator>` per slot, a `Vec<Key>` preserving first arrival order, and a `HashSet<Key>` per slot per aggregate for the `DISTINCT` variants.

## 7.2 The finding

`Accumulator` cannot be used by anything except the operator it was written for, for four separate reasons, and each one is a hard blocker for a different later layer.

It has no fixed size. `State::Extreme(Option<Value>)` holds a `Value`, which for a varchar holds a `String`, which is a pointer to a heap allocation. A state that is not a fixed number of bytes cannot live inline in a hash table payload, so the aggregate cannot use the table from document 06 at all and has to keep a separate `Vec<Accumulator>` indexed by slot, which is a second indirection per row on top of the probe.

It has no combine. Two accumulators of the same kind cannot be merged into one. Without that there is no per-thread partial aggregation and therefore no parallel aggregate, which means layer eight cannot parallelize the operator that most needs it.

It has no serialize. A state that cannot be written to bytes cannot be spilled, so the memory work in document 10 has no way to move an aggregate to disk.

It updates one row at a time from a `&[Value]`, which is the same defect as everywhere else and costs the same allocations.

So the interface is replaced. The arithmetic inside it is kept, moved behind the new interface, and used as the oracle for the vectorized versions in the same way `compare_values` was in layer one.

## 7.3 The aggregate function interface

An aggregate becomes five pieces of behaviour over a state of a size it declares.

```rust
pub struct AggregateFunction {
    state_size: usize,
    init: fn(&mut [u8]),
    update: fn(&[Vector], &Selection, &[*mut u8]) -> Result<()>,
    combine: fn(&[u8], &mut [u8]),
    finalize: fn(&[u8], &mut Vector, usize) -> Result<()>,
    serialize: Option<(fn(&[u8], &mut Vec<u8>), fn(&[u8]) -> usize)>,
}
```

The shape is DuckDB's and it is the shape for a reason: it is the minimum set of operations that supports grouped and ungrouped evaluation, parallel partial aggregation, spilling, and window frames, and leaving any one of them out closes off one of those.

The state is a fixed number of bytes with no pointers in it for every aggregate that can manage it, which is all six of the current ones except min and max over a varchar. For those the state holds the inline string view when it fits and an arena reference when it does not, with the arena owned by the operator rather than by the state, which is the same solution as the out-of-line key case in document 06 section 6.4 and reuses the same arena.

`update` is the vectorized one and it is the part that matters. It receives the argument vectors, a selection of which rows to fold in, and a vector of pointers to the states those rows belong to. For a grouped aggregate the pointers come from the hash table probe and they are scattered, so the update is a gather-modify-scatter over the states. For an ungrouped aggregate every pointer is the same one, which is a case worth specializing because it collapses to a straight loop over one register. A third case, all rows in the chunk belonging to the same group, happens constantly on clustered data and collapses the same way, and detecting it costs one comparison.

The scatter update is where the aggregate spends its time on a high-cardinality group by, and it has the same cache problem the probe does. It gets the same treatment: the state pointers for a whole chunk are computed first and prefetched, then applied.

## 7.4 Three shapes of grouping, chosen at plan time

**Ungrouped.** One state, no hash table, no key encoding. `SELECT count(*) FROM t` and `SELECT sum(x) FROM t WHERE ...` are this, several ClickBench queries are this, and it should be a loop over a vector with an accumulator in a register.

**Hash grouped.** The general case, the table from document 06, one state block per group held inline in the payload.

**Array grouped.** When the group key is a small integer or a dictionary code with a known bound, the group index is the key itself and there is no hashing at all. The state array is indexed directly and the update is a scatter. This is what makes ClickHouse fast on the low-cardinality ClickBench aggregates, the native format knows its dictionaries, and document 05 section 5.8 already establishes that the scan can hand a dictionary code up unchanged. The bound has to come from the catalog or from the scan's own metadata rather than from a runtime observation, because falling back mid-query is a complication that is not worth the cases it buys.

The choice is made at plan time from bound types and catalog statistics, and it is recorded in the plan so that `EXPLAIN` shows it and so that a regression in the choice is visible rather than being a mysterious slowdown.

## 7.5 Parallel aggregation

Each thread aggregates its own morsels into its own table, and the tables are merged at the end. That is what `combine` exists for and it is the whole design.

The merge is the part that is easy to get wrong. Merging n tables pairwise on one thread is a serial tail proportional to the number of groups, and on a high-cardinality group by that tail can be most of the query. The fix is radix partitioning of the merge: every thread's table is partitioned by the top bits of the hash, which document 06 section 6.9 already requires the table to support, and then partition k of every thread's table is merged by one thread with no coordination. That parallelizes the merge perfectly and it needs no locking.

For a low-cardinality group by the per-thread tables are tiny and the merge is trivial, and for a high-cardinality one the partitioned merge is what keeps it scaling. Both cases appear in ClickBench, which is why both are handled rather than one being assumed.

The thing that has to be decided here rather than discovered: a parallel `sum` over `DOUBLE` gives a different answer depending on how the work was divided, because floating point addition is not associative. DuckDB has the same property and does not promise determinism, so matching DuckDB means matching that behaviour, and it means the corpus tests for float sums have to compare with a tolerance rather than exactly. That is recorded here because it is the kind of thing that turns into a bug report about non-deterministic results, and the answer needs to be a considered position rather than an improvisation. The position is: results are deterministic for a fixed thread count and a fixed data order, they are not promised across thread counts, and this matches DuckDB.

## 7.6 `COUNT(DISTINCT)`

This deserves its own section because it is in a large fraction of the ClickBench queries and because the current implementation is a `HashSet<Key>` per group per aggregate, which for a query grouping by one column and counting distinct `UserID` means one hash set per group holding every distinct user in that group. That is the whole dataset in memory in the worst shape possible.

The correct implementation is the one DuckDB uses: a distinct aggregate is rewritten into a second aggregation. The group key and the distinct argument together become the key of an inner table, which deduplicates, and the outer aggregate then counts over the deduplicated rows. That turns a per-group set into one more hash table of the kind already built, it partitions and parallelizes and spills exactly like every other table, and it reuses everything from document 06.

Approximate distinct is a separate function and not a silent substitution. `approx_count_distinct` exists and is fast and is not what `COUNT(DISTINCT)` runs, because a benchmark number produced by silently approximating an exact aggregate is a number that is not comparable with anyone else's. `rudb-encoding` already has a k-minimum-values sketch in `sketch.rs` with a `hash64`, chosen over HyperLogLog because the chooser needed set intersection and Jaccard rather than only cardinality, and it merges, so it is what `approx_count_distinct` is built from. That reuse also means the optimizer in document 11 and the aggregate share one cardinality estimator, which is worth more than either use alone.

## 7.7 Aggregates that do not need to read the data

`SELECT count(*) FROM t` with no filter is a metadata read. `SELECT min(x), max(x) FROM t` with no filter is a metadata read, because the scan layer already keeps per-block minimum and maximum for pruning. `SELECT count(*) FROM t WHERE p` where the zone maps prove `p` is true for every row of a block is a metadata read for those blocks.

DuckDB does the first two and they turn a query over a hundred million rows into a query over a few thousand block headers. They are cheap to implement, they are visible in any benchmark that includes such a query, and the reason to state them here rather than in the optimizer document is that the rewrite needs the aggregate to declare that it can be satisfied from statistics, which is a property of the aggregate function.

## 7.8 The rest of the surface

`FILTER (WHERE ...)` on an aggregate is a selection passed to `update`, which the interface in section 7.3 already takes, so it costs nothing extra.

Ordered aggregates, meaning `string_agg(x ORDER BY y)` and the ordered set functions, need the input sorted per group, which needs the sort from document 09, so they are scheduled after it.

`GROUPING SETS`, `ROLLUP` and `CUBE` are multiple aggregations over the same input with different key subsets. The naive implementation runs one aggregation per set, and the better one computes the finest set once and rolls up from it where the aggregates permit, which is when they are decomposable, which `sum` and `count` are and `count(distinct)` is not. The naive version goes in first because it is correct and because these appear rarely in the benchmarks and regularly in the corpus.

The aggregate function catalogue grows past six, and the order it grows in is set by the corpus and by the benchmarks rather than alphabetically. The next ones are `count(distinct)` from section 7.6, `stddev` and `var` in their sample and population forms, `string_agg`, `first` and `last`, `arg_min` and `arg_max`, `bit_and`, `bit_or` and `bit_xor`, `bool_and` and `bool_or`, `median` and `quantile`, `list`, and `histogram`. Every one of them is a state size, five functions and a test against DuckDB.

## 7.9 The sink contract

The aggregate is a pipeline breaker: nothing comes out until everything has gone in. That makes it the first operator with the sink shape the scheduler needs, and the shape is fixed here.

A sink has a global state shared by every thread, a local state per thread, a `sink` that folds a chunk into the local state, a `combine` that folds a local state into the global one, a `finalize` that runs once when every thread has combined, and a source phase that hands the result out in parallel. That is four methods and it is the contract document 00 said would be imposed from layer one and implemented at layer eight.

The aggregate implements it now, single-threaded, with one local state. Turning that into eight local states at layer eight is a scheduler change and not an aggregate change, and that is the whole point of writing the contract down three layers early.

The source phase is the part people forget. Reading a merged hash table out in parallel means partitioning the read, which the radix partitioning from section 7.5 already provides, and an aggregate that can only be read out serially has a serial tail exactly as bad as the merge it avoided.

## 7.10 Spilling, again only the seam

An aggregate that does not fit in memory has to spill, DuckDB v2.0 shipped aggregation spilling and document 01 recorded it, and rudb has no memory manager until document 10.

What this layer owes is `serialize` in the interface from section 7.3 and the guarantee that every aggregate state written can be read back into an equal state, tested by round trip. Without it the memory work later has to revisit every aggregate function, which is exactly the retrofit cost document 00 exists to avoid. With it, spilling is a change to the operator and not to the functions.

## 7.11 The test gate

The existing `Accumulator` is the oracle. Every vectorized `update` is checked against folding the same rows one at a time through the old code, over random data with random nulls, for every aggregate and every input type.

`combine` gets a specific property: aggregating a sequence in one piece must equal aggregating it in two pieces and combining, for every split point and every aggregate. That single property finds almost every combine bug and it is cheap to run over thousands of random splits.

`serialize` gets a round trip property against `combine`: serialize, deserialize, combine with an empty state, and the result must equal the original.

The numeric edges are kept from the current tests and extended. Integer sum overflow reports rather than wraps. Decimal sum keeps its scale. `avg` of an empty group is null and `count` of an empty group is zero. `sum` of only nulls is null. `min` and `max` ignore nulls but a group of only nulls has a null extreme. Every one of those is a case DuckDB has a definite answer to and the corpus checks it.

Float sums are compared with a tolerance and the tolerance is justified in the test rather than being a magic number, for the reason in section 7.5.

## 7.12 The benchmark gate

Microbenchmarks: grouped aggregation throughput in rows per second per core, at group counts of 1, 100, 10 thousand, 1 million and 50 million over a hundred million rows, which spans the ungrouped case, the array case, the cache-resident hash case and the cache-hostile hash case. Each with one, two, four and eight aggregate functions, because the per-group state grows and the cache behaviour changes with it. The ungrouped `sum` over `BIGINT` is the one number that should be within a small factor of memory bandwidth, and if it is not, the update loop did not vectorize.

The whole-query gate is the ClickBench aggregate queries, meaning Q6 to Q11 for the simple counts and sums, Q28 to Q33 for the grouped ones, and Q4, Q5 and Q7 which are the `COUNT(DISTINCT)` ones and which are expected to move by the most because section 7.6 replaces the worst code in the engine. Plus TPC-H Q1, which is six aggregates over a filtered scan grouped by two small columns and is the canonical aggregate benchmark.

The target at 2g is that rudb beats DuckDB on TPC-H Q1 and on the ClickBench low-cardinality grouped queries in CPU seconds on `server3`, single threaded. That is a real target rather than a directional one, and it is set there because by 2g the scan, the expressions and the hash table are all done, the aggregate is the last piece of those queries, and if the whole stack is right then Q1 is where it shows first. If rudb does not win Q1 at 2g, something in layers one through five is wrong and the right response is to find it rather than to proceed to the join.

## 7.13 Exit criterion for 2g

**Aggregates are a state size and five functions, states live inline in the hash table payload, `update` is vectorized with prefetched scatter, `combine` and `serialize` exist and pass their properties, the three grouping shapes are chosen at plan time and shown in `EXPLAIN`, `COUNT(DISTINCT)` is a second aggregation rather than a per-group set, `count(*)` and unfiltered `min` and `max` are answered from statistics, the sink contract is implemented, the corpus pass rate has risen because the aggregate catalogue grew, and rudb beats DuckDB single threaded on TPC-H Q1 on `server3`.**

Named as deferred: ordered aggregates, which wait for the sort in document 09, the decomposable rollup optimization for `GROUPING SETS`, spilling itself, and parallel execution, which is present as a contract and not as an implementation.
