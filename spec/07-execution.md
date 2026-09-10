# Execution: vectors, operators, morsels, hash tables, spilling

Document 06 specified what an encoded vector is and what an operator is allowed to do with one. This document specifies the operators themselves, the data structures they use, and the runtime behaviour that makes them adaptive. It is the largest single body of code in the project and it is where the second and third factors of the performance target come from after the format has delivered the first.

## 7.1 The vector interface

This is the widest interface in the system, every operator depends on it, and changing it after twenty operators exist is expensive. It is specified before any operator is written and changed only by RFC.

A vector is a type, a length up to 1024, a physical form, a validity representation and a buffer. The physical forms are flat, constant, dictionary, sequence and encoded, per document 4.3.

**Validity has three representations and the distinction is load-bearing.** All-valid, represented by the absence of a mask, which is the case that gets the fastest kernels. All-invalid, represented by a flag, which short-circuits entirely. And a bitmap. Photon's published result is that separate no-null kernels are worth a measurable amount on real data because real data is mostly not null, and the cost of the distinction is one branch per vector rather than per value.

**Strings are a 16-byte inline structure**: 4-byte length, 4-byte prefix, 8-byte pointer or inline continuation. Strings of 12 bytes or fewer are stored entirely inline. The prefix means most comparisons and most equality tests resolve without dereferencing, which on the string-heavy queries in ClickBench is the difference between a cache hit and a cache miss per row. This is the Umbra design, it is what DuckDB adopted, it is what Arrow's `StringView` standardized, and there is no reason to differ.

**Buffers are owned or borrowed with a pin.** A vector borrowed from a buffer-managed page carries the pin. Nothing on the hot path is reference counted.

**Selection vectors, not compaction, by default.** A filter produces a `u32` selection vector. Compaction happens when a measured selectivity threshold is crossed and the downstream operator is one that benefits, and the threshold is per-operator and measured rather than a single global constant.

## 7.2 Operators

Sources: table scan, Parquet scan, Arrow scan, values, table function, recursive CTE working table.

Stateless: projection, filter, cast, unnest.

Pipeline breakers: hash join build, hash aggregate, sort, top-N, window, distinct, set operations, materializing CTE.

Streaming with state: streaming aggregate on sorted input, merge join, nested loop join, positional and as-of joins, and the `NEAREST` join that DuckDB v2.0 added and compatibility requires.

Every operator has a general path over any physical form and zero or more specialized paths, per the contract in document 6.7.

## 7.3 The kernel generator

The cross product of operator, physical form and type is too large to write by hand and too valuable to skip. So it is generated.

A table declares, for each kernel family, which type and form combinations get a specialized implementation. A build-time generator emits the specialized functions and a dispatch table. The generic implementation is the fallback and is always present. This is a macro and code generation problem, not a research problem, and Rust's macro system handles it well, but it does produce a large amount of generated code and compile times will need watching. A budget of ten minutes for a clean release build of the whole workspace is the line, and if the generator pushes past it the table gets trimmed.

**Runtime SIMD dispatch.** `std::simd` is still nightly-only in 2026, so stable code means `core::arch` intrinsics behind `is_x86_feature_detected!` and `std::arch::is_aarch64_feature_detected!`, with a scalar fallback. Targets: AVX-512 where available including the `VBMI2` instructions that matter for bit unpacking, AVX2, SSE4.2, and NEON. One binary, dispatch chosen once at startup per kernel family and stored in a function pointer table, which is what ClickHouse does and it works.

**The scalar fallback is not vestigial.** It is what runs under Miri, it is the reference the differential tests compare against, and it is what runs on a platform we have not thought about. It is tested on every commit.

## 7.4 Morsels and the scheduler

**A morsel is one row group, 122,880 rows,** with a smaller unit for small tables so that a 200,000-row table still parallelizes. Workers pull from a shared queue with work stealing.

**Pipelines form a DAG and the scheduler respects it.** Independent branches run concurrently. A sink's dependents do not start until it finalizes.

**Backpressure is by bounded queues between pipeline stages.** A fast source feeding a slow sink blocks on a bounded channel rather than accumulating unbounded intermediate results, which is one of the more common ways an analytical engine turns a large query into an out-of-memory.

**Scheduling is NUMA-aware to the extent of preferring local morsels.** A worker prefers morsels whose pages are on its node, steals remotely only when its local queue is empty. On a single-socket machine this is a no-op. On a 448-core instance it is not, and the trend in document 01.5 says those machines are what performance claims will be measured on.

**Cancellation is checked at morsel boundaries.** Section 4.9.

## 7.5 Aggregation

This is where the largest remaining wins are after the format, per document 03.4, and it needs three separate mechanisms.

**Small group counts: thread-local hash tables, merged at the end.** The standard design, correct when the group count is small enough that per-thread tables fit in cache and the merge is trivial. Queries grouping by `RegionID` or `SearchEngineID` are here.

**Large group counts: a partitioned global table with atomic insert.** The merge cost of thread-local tables is proportional to group count times thread count, and at 100 million groups on 16 threads it dominates everything. "Global Hash Tables Strike Back" is the paper and its finding is that a shared partitioned table with fine-grained synchronization beats per-thread-plus-merge above a threshold. Query 32, `GROUP BY WatchID, ClientIP`, is exactly this case and it is 16.3 percent of Umbra's total time.

**The switch between them is at runtime, not at plan time.** After a fixed number of morsels, each worker reports its local table's cardinality, and the aggregate switches strategy if the extrapolated group count crosses the threshold. Basing this on the optimizer's estimate would mean getting it wrong on exactly the queries where it matters, because a two-column group-by cardinality estimate is a product of two estimates and is routinely off by an order of magnitude.

**Top-k aggregation with a heavy-hitter pre-pass.** This is the mechanism that document 02.4 hangs a large part of the target on, and it applies to `GROUP BY x ORDER BY count(*) DESC LIMIT k`, which is 14 of the 43 ClickBench queries.

The observation is that these queries build a hash table with tens of millions of entries and then throw away all but ten of them. A Space-Saving or Misra-Gries sketch with m counters, m being a few thousand, identifies the heavy hitters in one pass in a fixed amount of memory that fits in L2. The result is approximate, and approximate is not acceptable, so there is a second pass: take the candidate set from the sketch, which is small, and compute exact aggregates for exactly those keys with a small exact hash table, while also computing an exact bound on the largest possible count of any key not in the candidate set. If the k-th candidate's exact count exceeds that bound, the answer is provably exact and we are done in two cheap passes. If not, fall back to the full hash table.

**The properties that make this acceptable.** The answer is always exact, never approximate, because the verification either proves it or falls back. The failure case costs one extra pass over the data, which on a query that would otherwise build a 100-million-entry hash table is a small fraction of the total. The win case avoids building that table entirely. And on real data with heavy-tailed distributions, which is what web analytics is, the win case is the common one.

**The honest uncertainty** is what fraction of ClickBench queries actually take the win path and what the sketch width has to be. That is measured in M5 and it is document 19 open question six. If the answer is that most of these queries have flat enough distributions that verification fails, this mechanism contributes little and the axis-2 target loses roughly a factor of 1.5.

**Aggregate function state is fixed-size where possible.** `SUM`, `COUNT`, `MIN`, `MAX`, `AVG` are 8 or 16 bytes. `COUNT(DISTINCT)` uses HyperLogLog when the query is a top-level approximate function and an exact hash set otherwise, and the exact case is what ClickBench measures, so it is the one that gets the work. Exact distinct counting per group is queries 4, 5, 8, 10, 11, 13 and 22, which is a substantial share, and the implementation is a per-group hash set with small-set inlining so that a group with three distinct values does not allocate.

## 7.6 Joins

**Hash join is the default and it is a partitioned radix hash join above a size threshold and a non-partitioned one below.** "To Partition or Not to Partition" and the DaMoN 2024 hash table survey are the references, and the summary is that the answer depends on build size relative to cache and there is no single right choice. So: measured threshold, switched at runtime on observed build cardinality.

**The hash table is open addressing with linear probing and a tag byte.** A bucket holds a one-byte hash tag and a pointer or an inline key. The tag rejects most non-matching probes without touching the key, which is the same trick as the string prefix and pays for the same reason.

**Build-side bloom or blocked-bloom filters are pushed to the probe side's scan** as a dynamic filter. Velox does this and reports a large win on selective joins. Combined with zone maps, a bloom filter pushed to a scan can prune whole row groups.

**Robust Predicate Transfer from M4.** RPT is the 2024 line of work extending Yannakakis-style semi-join reduction to general acyclic and near-acyclic queries with a bounded number of transfer passes. The mechanism is a forward and backward pass of bloom filters along a spanning tree of the join graph, which reduces every relation to approximately its contribution to the final result before any join executes. On JOB and CEB the published wins are large and the reason is that it makes the plan much less sensitive to cardinality estimation error, which is the actual disease. This does very little for ClickBench, which has no joins, and it is most of what makes TPC-H, TPC-DS and JOB good rather than merely acceptable.

**Join order is cost-based with a DP enumerator up to a size limit and a greedy heuristic past it.** Document 09.4.

**Mark, semi, anti, single and outer joins are all first-class**, not rewrites onto inner join plus filter, because the rewrite loses the ability to stop early.

## 7.7 Sort and window

**Sort is a parallel merge sort over normalized keys.** Keys are normalized into a byte-comparable form so that comparison is a `memcmp` regardless of type, including for multi-column keys with mixed direction and null ordering. This is standard and it is worth the encoding cost because it turns a chain of type-dispatched comparisons into one instruction sequence.

**Radix sort for narrow fixed-width keys**, chosen by width. A sort on a `u32` dictionary code is a radix sort, which is another way the global dictionary pays.

**Top-N does not sort.** A bounded heap per thread, merged. This is the difference between 0.005 and 0.166 seconds on ClickBench queries 24 through 26.

**Window functions are evaluated over sorted partitions** with a shared sort where multiple window functions share a partitioning, which is a common shape in TPC-DS. Ranking functions, aggregate frames with `ROWS` and `RANGE`, and the DuckDB-specific frame extensions are all in scope for compatibility per document 10.

## 7.8 Spilling

**Every blocking operator spills: hash aggregate, hash join, sort, distinct.** A query that exceeds the memory limit gets slower and does not fail. This is scheduled in M6 and it is not optional.

**Spilled data is partitioned by hash so that a spilled partition can be processed independently.** Hash aggregate spills partitions of its table and reprocesses them one at a time. Hash join spills matching partitions of both sides. Sort spills runs and merges.

**Spilled data is compressed with the same encoding machinery**, at a fast setting. It is temporary data on a local disk and the tradeoff favours speed over ratio, but a 3x reduction in spill volume is a 3x reduction in spill I/O and the encoder is already there.

**The memory limit is a real budget across all concurrent queries**, not per query, and admission control queues a query that cannot get a minimum working set rather than admitting it into a thrash.

## 7.9 Adaptivity, listed explicitly

The engine makes six decisions at runtime that a conventional engine makes at plan time. Each is here because the estimate that would drive it at plan time is unreliable.

Aggregation strategy, on observed group count. Join partitioning, on observed build size. Filter ordering within a conjunction, on observed per-predicate selectivity, reordered every few vectors. Selection versus compaction, on observed selectivity. Execution tier, on observed tuple count. Scan physical layout, on what the consuming operator actually does with the values, which is document 09.6.

**Every adaptive decision is logged and visible in `EXPLAIN ANALYZE`.** An engine that changes its mind at runtime and does not say so is undebuggable, and the first thing anyone investigating a performance regression needs is which way each switch went.

**Every adaptive decision is overridable by a session setting**, for testing and for the case where a user has found a pathology. The differential harness runs with adaptivity pinned in each direction to verify that all paths produce identical results.
