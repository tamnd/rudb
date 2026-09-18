# The operator catalogue

One section per operator. Each says what it is, what it may assume, which seams it owns, how it spills, and what its reference implementation is.

The shared contract, from [`08-execution.md`](08-execution.md): operators are `&self`, per-thread state is `Local`, sinks have `combine` and `finalize`, nothing spawns a thread, nothing allocates outside a `Budget`, and the reference implementation is never deleted.

## 1. Scan

**Is:** a `Source` over blocks of one table, with projection, a pushed-down filter, and a set of pushed-down Bloom and range filters from joins.

**May assume:** nothing about order unless the table has a sort key, in which case it may assume that order and the planner may rely on it.

**Seams:** `scan.materialisation` (`eager`, `lazy`, `lazy-with-cost`).

Lazy materialisation is ClickHouse 25.4's big win, 219.071 seconds to 139 milliseconds on Amazon reviews cold, forty times less data read, three hundred times lower peak memory, and it is the same idea as principle 4 seen from the scan. Read the filter columns; evaluate; read the payload columns only for surviving rows.

The reason there are three strategies rather than two is the cost term. Random access into an encoded column is not free: a row in an RLE run is a binary search, a row in a bit-packed tile is a tile decode, a row in a front-coded dictionary is a prefix walk. Below some selectivity, gathering costs more than scanning. `lazy-with-cost` asks the encoding what a random access costs and decides per column per block. `lazy` always defers, which is what ClickHouse does, and the difference between the two on `hits` is a measurement in the F7 ledger.

**Spills:** never. It is a source.

**Reference:** read every projected column of every block, decode to flat, apply the filter.

## 2. Filter and project

**Are:** `Stream`s. Filter narrows the selection. Project replaces columns.

**May assume:** their input chunk is theirs to modify in place.

**Seams:** `kernel.filter` (`branchy`, `branchless`, `bitmask`), `chunk.compaction`.

The compaction seam matters more than it looks. After a selective filter a chunk is sparse, and every downstream operator pays for the sparsity. Compacting costs a copy. The SIGMOD 2025 paper's finding is that the right threshold is not a constant and is learnable per operator from a gain function, worth up to ten per cent end to end. The three strategies are `never`, `fixed-threshold` and `learned-gain`, and this is one of the seams most likely to produce a publishable comparison.

**Spills:** never.

## 3. Hash aggregate

**Is:** a `Sink` plus a `Source` for the result.

**May assume:** nothing about input order.

**Seams:** `hash.key`, `hash.function`, `hash.table`, `agg.state`, `agg.parallel`.

Three grouping shapes are chosen at plan time, not at runtime. **Ungrouped** is a single state, no table. **Array-grouped** is used when the key is a dictionary code or a small integer range, and the table is an array indexed directly, this is the shape the global dictionary from [`05-data-model.md`](05-data-model.md) section 7 creates, and it is the fastest aggregate there is. **Hash-grouped** is everything else.

Aggregate functions get the interface v1 specified, unchanged, because it is right:

```rust pub struct AggregateFunction {
    state_size: usize,
    init: fn(&mut [u8]),
    update: fn(&[Column], &Selection, &[*mut u8]) -> Result<()>,
    combine: fn(&[u8], &mut [u8]),
    finalize: fn(&[u8], &mut Column, usize) -> Result<()>,
    serialize: Option<(fn(&[u8], &mut Vec<u8>), fn(&[u8]) -> usize)>,
}
```

Fixed state size is what makes the state live in pages, which is what makes it spillable, which is what makes it sendable. `State::Extreme(Option<Value>)` holding a `String`, which is what `rudb-kernels/src/aggregate.rs` does today, breaks all three at once.

`COUNT(DISTINCT)` is rewritten as a second aggregation, never as a per-group `HashSet`. `count(*)` and unfiltered min/max come from block statistics without reading anything.

**Spills:** radix partitions, operator-chosen, per [`07-memory.md`](07-memory.md) section 5.

**Reference:** `HashMap<Key, usize>` with `Key(Vec<Value>)` and `IS NOT DISTINCT FROM` semantics. This is `rudb-exec/src/group.rs` today and it is two orders of magnitude off, and it becomes the oracle rather than being deleted.

## 4. Top-N and heavy hitters

**Is:** a `Sink` for `ORDER BY ... LIMIT k`.

**Seams:** `topk` (`sort-then-limit`, `bounded-heap`, `heavy-hitter-two-pass`).

The third strategy is why this is its own operator rather than a special case of sort, and it is one of the three mechanisms [`../02-the-goal.md`](../02-the-goal.md) identifies as carrying the ten times.

ClickBench Q32 is `GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10` over a hundred million rows. Umbra spends 1.323 seconds on it, DuckDB 2.035. The query builds a hash table with tens of millions of groups in order to return ten rows. `heavy-hitter-two-pass` replaces that with a Space-Saving or Misra-Gries sketch of bounded size to find the candidates, then a second pass to count the candidates exactly. Memory goes from O(distinct) to O(k). The second pass is what makes it exact, and exactness is not negotiable, this is a DuckDB-compatible engine and an approximate answer is a wrong answer.

The cost is a second pass over the data, which is why this is a strategy with a cost model rather than a default, and the crossover is where the sketch's memory saving outweighs the second read. On `hits`, where the first pass is bandwidth-bound and the group count is enormous, it should win comfortably; that is a prediction and F5 measures it.

**Spills:** never. That is the point.

## 5. Hash join

**Is:** a build `Sink` and a probe `Stream`.

**Seams:** `join.build`, `join.filter`, plus the shared hash seams.

Eight join kinds, inner, left, right, full, semi, anti, single, positional, are already correct in `rudb-exec/src/join.rs`'s nested loop, and that nested loop stays permanently as the oracle and as the non-equi fallback.

Four things the design has to get right and each is a known source of bugs:

**The resumable probe.** One build row may match many probe rows, so a probe that fills a chunk mid-match must resume. A two-index state machine, and v1 is right that this is the single most common source of bugs in this kind of engine. It gets its own property test with adversarial chunk boundaries.

**Match flags for right and full joins.** A bitmap over build rows, updated atomically by every probing thread. A byte per row may beat a bit per row because of contention; that is a measurement, and it is a seam only if the measurement says it is close.

**Null semantics.** An equi-join excludes nulls. A grouping key uses `IS NOT DISTINCT FROM`. These are different and the shared key encoding must not accidentally unify them.

**`single` must error on a second match**, because that is what a scalar subquery means.

**The largest win in this operator is not in this operator.** A Bloom filter built from the build side and pushed into the probe-side scan, plus a min/max range predicate derived the same way, turns a join into a scan-time skip. The 2025 result on pushing Bloom filters into bottom-up join enumeration rather than applying them afterwards is a further 32.8% on 100 GB TPC-H, and that part belongs to [`12-optimizer.md`](12-optimizer.md).

**Spills:** Grace partitioning on top hash bits, no rehashing, with a nested-loop bailout for a partition that does not fit alone.

## 6. Sort

**Is:** a `Sink` plus a parallel `Source`.

**Seams:** `sort` (`comparator`, `normalised-radix`, `normalised-merge`).

`rudb-exec/src/sort.rs` sorts by comparator over `Value`, and `rows::from_chunks` builds `Vec<Vec<Value>>`, which at a hundred million rows by ten columns is a billion heap allocations. That is the reference implementation now.

The design is normalised order-preserving byte keys, radix sort, and the payload gathered once at the end. The key encoding is shared with the hash operators, which is the second reason to have one key encoding and not two. Stability is kept deliberately, because DuckDB's `ORDER BY` is stable and compatibility is the point.

**Spills:** runs, k-way merge, one page pinned per run.

## 7. Window

**Is:** a `Sink` plus a `Source`. There is no `Window` node in `rudb-plan` at all today, which makes this the largest piece of missing SQL surface.

**Seams:** `window` (`per-partition-sort`, `segment-tree`, `streaming-frame`).

Three frame shapes carry almost all real usage: the whole partition, a running frame from the start, and a sliding range. The first two are a single pass. The third is where the segment tree earns its keep, and where a naive implementation is quadratic.

**Spills:** per partition, using the sort's machinery.

## 8. Set operations and the rest

`UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`, `DISTINCT`, all either a pass-through or the hash aggregate with a different finalise. `rudb-exec/src/setop.rs` exists.

`LIMIT` and `OFFSET` are a `Stream` with a counter, and `LIMIT` without `ORDER BY` must be able to stop the scan, which is a `Blocked`-free early termination path through the source.

`VALUES`, table functions, and the recursive CTE. The last one needs care: DuckDB v2.0 rewrote it for 40x and changed the visibility semantics of `USING KEY`, and compatibility means matching the new behaviour rather than the old.

Non-equi joins: IEJoin for inequality predicates, merge join where both sides are already ordered by a sort key, AsOf for time-series. All F9, all fall back to the nested loop until then, all correct in the meantime.

## 9. What each operator contributes to the ten times

Worth writing down, because the catalogue above is a list of work and only some of it is on the critical path.

The scan, through lazy materialisation and encoded predicates, is most of it. The aggregate, through array-grouping on global dictionary codes, is most of the rest. Top-N through heavy hitters is four of the seven expensive ClickBench queries. The join's Bloom pushdown is most of TPC-H. Everything else, sort, window, set operations, the long tail, is table stakes: it has to be correct and within a factor of two, and making it faster than that does not move the number.

Planning the work in that order is the difference between an engine that is 10x and an engine that is uniformly 1.3x.
