# Layer seven: sort, top-N and window

This is sub-milestone 2i. It is two operators that look unrelated and are not: a window function needs its input partitioned and ordered, so a window is a sort with an evaluation pass on top, and building them separately means building the sort twice.

It is also the layer that unblocks four things scheduled earlier and deferred to here: ordered aggregates from document 07 section 7.8, and merge join, IEJoin and `AsOf` join from document 08 section 8.7.

## 9.1 What exists today

`crates/rudb-exec/src/sort.rs` is 119 lines. It is a pipeline breaker that reads everything into a `Vec<Chunk>`, converts to rows, and calls Rust's `sort_by` with a comparator that walks the sort keys comparing `Value`s.

The module doc makes one decision worth keeping. The sort is stable, SQL does not promise stability, and it is kept anyway because `ORDER BY a` over rows that tie producing a different order on two runs makes a compatibility diff useless. That reasoning is right and it survives the rewrite, with one qualification in section 9.4 about what stability means once the sort is parallel.

There is no `Window` node in `rudb-plan` at all. Window functions do not exist in any layer of the system, which is a compatibility gap of some size, because `ROW_NUMBER`, `RANK`, `SUM(...) OVER (...)` and `LAG` are in a large fraction of real analytical SQL and in a large number of corpus records.

There is no top-N operator. `ORDER BY x LIMIT 10` sorts a hundred million rows and takes ten.

## 9.2 The finding

`sort_by` with a `Value` comparator costs, per comparison: a virtual call into the closure, a walk over the key list, and a `Value` comparison per key which for a varchar compares two `String`s that were materialized when the row was built. At n log n comparisons over a hundred million rows that is tens of billions of `Value` comparisons.

The row materialization before it is worse in a different way: `rows::from_chunks` turns every chunk into `Vec<Vec<Value>>`, which for a hundred million rows of ten columns is a billion heap allocations. That is not a slow sort, it is a sort that cannot run at all at benchmark scale, and it is why no TPC-H query with a large `ORDER BY` has ever completed on rudb.

The absence of top-N is separately expensive. ClickBench has `ORDER BY ... LIMIT` in more than half its queries and TPC-H has it in several. A heap of size k over a streaming input is a well-known operator, it is a hundred lines, and it converts most of those queries from a full sort into a scan.

## 9.3 Normalized keys, which is the whole design

The sort does not compare values. It compares bytes.

Every sort key is encoded into a byte sequence whose unsigned byte order is the requested SQL order, which means the comparator becomes `memcmp` over a fixed-width prefix and the sort becomes a sort of fixed-width integers. This is the same normalized key encoding document 06 section 6.4 specifies for hash keys, with two additions: the encoding has to be order-preserving and not merely equality-preserving, and it has to handle `DESC` and the null ordering.

`DESC` is a bitwise complement of the encoded bytes for that key. Null ordering is the leading flag byte from document 06, with the value of that byte chosen by whether nulls sort first or last, which is a per-key property in SQL and is what the `NULLS FIRST` and `NULLS LAST` clauses set. The session default for it is the `default_null_order` setting that `rudb-common/src/settings.rs` already classifies as honoured, and this is the operator that has to honour it, which is the wiring that branch was parked waiting for.

Once keys are bytes, the algorithm is a radix sort rather than a comparison sort. DuckDB's sort does exactly this and the reason it is fast is not a clever comparison, it is that there is no comparison. A most-significant-digit radix sort over the first few key bytes partitions the data into buckets that are then sorted recursively or, once small enough, insertion sorted. For the common case of a single integer or date key, which is most `ORDER BY` clauses in practice, the whole sort is two or three radix passes and no comparison at all.

Variable-length keys break fixed-width normalization, and the standard answer is to normalize a prefix, sort on it, and resolve the ties that remain by comparing the full values. For a URL column the prefix decides almost every pair, which is the same observation that makes the string view prefix trick work in document 03 section 3.5, and it uses the same prefix.

The payload does not move during the sort. What is sorted is an array of fixed-width entries holding the normalized key prefix and a row index, and the payload rows are gathered once at the end in sorted order. That keeps the sort's working set small enough to stay in cache for far longer, and the single gather at the end is one pass over the data instead of a copy per swap.

## 9.4 Parallel sort, and what happens to stability

The sort is a sink with the contract from document 07 section 7.9: each thread sorts its own morsels into a sorted run, and the runs are merged.

The merge is where the design choices are. A k-way merge on one thread is a serial tail. The alternative is to partition by key range before merging, so that each thread merges one range independently and the concatenation of the ranges is the sorted output, which parallelizes fully and needs the key range boundaries, which come from sampling the sorted runs. Sampling from already-sorted runs is nearly free and gives good boundaries even under skew, which random sampling of the input does not.

Stability is the casualty. A parallel sort that partitions by key range preserves the relative order of ties only if the merge respects the original arrival order, and arrival order in a morsel-driven scan is not deterministic. So the honest position is: rudb's sort is deterministic for a fixed thread count and a fixed input order, it is stable within a partition, and it does not promise stability across a parallel merge. That is what DuckDB does too. The compatibility consequence is that a corpus test whose expected output depends on tie order is a test that has to be compared as a multiset or with a total order added, and `rudb-compat` already has to handle that for DuckDB's own output.

That is a change from the current module doc's promise and it is stated here rather than quietly dropped.

## 9.5 Top-N

`ORDER BY x LIMIT k` with small k is a bounded heap and not a sort. Keep a k-element max-heap of the smallest k rows seen so far, compare each incoming row against the heap root, and discard immediately if it loses. For a hundred million rows and k of ten, almost every row loses on one comparison against one value that is in a register.

The comparison is against the normalized key, so the root comparison is an integer comparison. The heap holds normalized keys and row references, and the payload is gathered for k rows at the end.

Parallel top-N is one heap per thread and a merge of the heaps, which is trivially correct because a merge of k-element heaps has at most k times the thread count elements.

The threshold at which top-N stops being better than a full sort is where k approaches the input size, and it is measured rather than guessed. `ORDER BY x LIMIT 1000000` over ten million rows is a different question from `LIMIT 10`.

`LIMIT` with a large `OFFSET` is the same operator with k equal to the offset plus the limit, which is worth stating because a naive implementation sorts everything and skips.

## 9.6 Window functions

A window operator is: partition the input, order each partition, and then for each row evaluate a function over a frame of rows relative to it.

The partition and the order are the sort from section 9.3, over the `PARTITION BY` keys followed by the `ORDER BY` keys, which puts every partition contiguous and internally ordered in one pass. Partitions can then be processed independently, which is how the operator parallelizes, and which fails badly when there is one partition or when one partition holds most of the rows. The single-partition case is common, `ROW_NUMBER() OVER (ORDER BY x)` has no partition at all, and it needs a different parallel strategy, which is to compute per-chunk partial results and combine them with a prefix scan.

The functions divide into three classes and each class has a different implementation.

**Ranking functions**, meaning `ROW_NUMBER`, `RANK`, `DENSE_RANK`, `PERCENT_RANK`, `CUME_DIST` and `NTILE`. These need no frame, only the position within the partition and the peer group boundaries, and they are one pass over the sorted partition.

**Value functions**, meaning `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE` and `NTH_VALUE`. These need one specific row relative to the current one, which is an index computation and a gather, and they are also one pass with no accumulation.

**Aggregate functions over a frame**, meaning `SUM(...) OVER (ROWS BETWEEN ...)` and every other aggregate with a frame. These are the expensive class and they are the reason the Leis et al. SIGMOD 2015 paper exists. The naive implementation recomputes the aggregate over the frame for every row, which is quadratic when the frame is large. Three implementations cover the real cases:

A running aggregate for the cumulative frame, meaning `UNBOUNDED PRECEDING` to `CURRENT ROW`, which is one pass with an accumulator and is what most real window aggregates are.

A sliding aggregate for a fixed-size frame, adding the entering row and removing the leaving one, which needs the aggregate to be invertible. `SUM` and `COUNT` and `AVG` are invertible, `MIN` and `MAX` are not, and using the inverse where it does not exist is a wrong answer, so invertibility is a declared property of the aggregate function from document 07 section 7.3 rather than an assumption.

A segment tree for the general frame, which is the paper's contribution: build a balanced tree of partial aggregates over the partition, and any frame is then answered by combining a logarithmic number of nodes. This handles `MIN` and `MAX` over sliding frames, `RANGE` frames whose bounds are data-dependent, and everything else that the first two cannot. It costs a linear build and it is the fallback rather than the default.

The `combine` function from document 07 is exactly what a segment tree node needs, which is another reason it was specified there rather than being invented here.

Document 01 recorded that Polars' streaming engine falls back to a non-streaming path for window functions, which is the honest admission that this operator is hard to stream. rudb does not stream it either at this layer, and says so.

## 9.7 What this unblocks

Ordered aggregates from document 07 section 7.8 become available, because the input to an aggregate can now be sorted per group, and `string_agg(x ORDER BY y)` and the ordered set functions follow.

Merge join becomes available, and with it the recognition that an input is already sorted, which on a clustered fact table is free.

IEJoin becomes available, which turns inequality joins from quadratic into close to linear, and `AsOf` join becomes available, which is a DuckDB feature with real users.

Those four are scheduled immediately after 2i rather than inside it, because each is small once the sort exists and because bundling them into this sub-milestone would hide whether the sort itself is right.

## 9.8 The test gate

The existing `sort_by` implementation is the oracle, as in every previous layer. Every normalized key encoding is checked by the property that the byte order of two encoded keys equals the SQL order of the two original key rows, over random values of every type, both null orderings, both directions, and multi-key combinations. That single property is the whole correctness of the sort and it is checkable exhaustively for small types and by sampling for the rest.

Edge cases that have to be directed rather than random: the extreme values of every integer type, positive and negative zero, NaN in both signs, empty strings, strings that are prefixes of each other, and strings that differ only past the normalized prefix.

Top-N is checked against a full sort followed by a limit, for every k from zero to more than the input size, with ties at the boundary, which is where an off-by-one shows.

Window functions are checked against DuckDB through the corpus, which has extensive window coverage, and the three frame implementations are checked against each other: the running, sliding and segment tree paths must agree on every frame they can all handle, which is a strong property because it cross-checks three independent implementations.

The invertibility declaration gets a test that fails if an aggregate declares itself invertible and its inverse does not reproduce the recomputed value, run over random sequences.

## 9.9 The benchmark gate

Microbenchmarks: sort throughput in rows per second per core at ten thousand, one million and a hundred million rows, for a single `BIGINT` key, a single `VARCHAR` key, and three mixed keys, with payload widths of one, ten and fifty columns because the final gather is what payload width costs. Top-N at k of 1, 10, 1000 and 100000 over a hundred million rows. Window aggregate throughput for cumulative, sliding and general frames at partition sizes spanning cache resident to not.

The whole-query gate is ClickBench, where more than half the queries end in `ORDER BY ... LIMIT`, and the expectation is that top-N alone moves those queries substantially because they are currently doing a full sort of the aggregate output. Plus the TPC-H queries with large sorts, Q3 and Q10 and Q18.

The target at 2i is that rudb beats DuckDB on a single-key `BIGINT` sort of a hundred million rows single threaded, and on top-N at small k by a wide margin, because top-N at small k is nearly a scan and a scan is what layers three and one made fast. Window functions have no rudb baseline to beat because they did not exist, so their target is stated against DuckDB directly: within a factor of two at 2i on the corpus-shaped queries, with the segment tree path being the one most likely to be slower and the one to look at first if it is not met.

## 9.10 Exit criterion for 2i

**Sorting is a radix sort over normalized keys with the payload gathered once at the end, `default_null_order` and per-key `NULLS FIRST` and `NULLS LAST` are honoured, the parallel sink and range-partitioned merge are in place with the stability position from section 9.4 documented, top-N is a bounded heap chosen by the plan and shown in `EXPLAIN`, the `Window` node exists through the parser, binder, planner and executor with all three function classes and all three frame implementations, the normalized key order property passes, window results match DuckDB on the corpus, and rudb beats DuckDB on a single-key sort of a hundred million rows single threaded.**

Named as deferred: merge join, IEJoin and `AsOf` join, which are unblocked here and scheduled immediately after, ordered aggregates, which are the same, external sort and spilling, which belong to document 10, and streaming window evaluation, which nobody does well.
