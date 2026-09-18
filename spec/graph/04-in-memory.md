# 4. The in-memory model

Document 03 says what is on disk. This document says what is in memory while a query runs, what it costs, who pays for it, and when it is given back. It is short by intent: the design goal is that the graph layer adds three resident structures and no resident copy of any table.

## 4.1 The rule about copies

The layer never materializes a table into memory to join it. That is the whole difference between this design and a hash join, and it is the difference that has to survive implementation. A hash join's build side is a full copy of one input in a row layout. A link join's build side is a page of a bit-packed column, read through the same buffer path as any other column, and the parent's values are read only for the rows that a probe actually reached and only for the columns the query actually projected.

The consequence to watch is that the link path trades a large sequential allocation for many small random reads. Document 09 section 9.3 measures that trade rather than assuming it, because on a parent table that does not fit in the page cache the hash join can win and the answer is then to use the hash join, which document 06 section 6.4 is the mechanism for.

## 4.2 What is resident

**Per open table, built once at open.** The per-part row count prefix sum, which turns a `rid` into a `(part, offset)` pair. Sixty four `u32` per stripe plus one `u64` per stripe, which for a hundred million rows is about four hundred kilobytes. It is built at open because it is walked millions of times per join and never changes.

**Per relationship, on first use and cached after.** The parent's key map, in whichever of the three forms document 03 section 3.3 chose. Identity is twenty four bytes. Dense offset and sorted are real: a sorted key map over fifteen million `customer` rows is a sorted key column plus a permutation of fifteen million times twenty four bits, about sixty megabytes. It is resident because a join probes it per row and paging it per probe would be the whole cost.

**Per relationship, on first use, and paged rather than resident.** The forward link column and the backward adjacency. These are read through the ordinary page path. A scan of the child table reads the link page for the part it is scanning, exactly as it reads any other column of that part, and drops it when the part is done. The monotone bit vector of document 03 section 3.4 is the exception that has to be thought about: rank and select over it are random access over the whole vector, so either it is resident, 106 MB for TPC-H SF100 `lineitem → orders`, which is affordable, or its select samples are resident and the vector itself is paged, which costs one page fault per lookup and is the form to use above a size threshold. The threshold is a setting with a measured default and not a constant somebody picked.

## 4.3 The bitmap, which is the object the execution model is built on

A `Rids` is a bitmap over the row ids of one table, with a cardinality maintained alongside. It is the type that a semi-join reduction produces and consumes, and it is what document 05 section 5.4's argument depends on being cheap.

Three representations behind one interface, chosen by density and switched as the cardinality changes:

- **Sparse**, a sorted `Vec<u64>` of row ids, below about one in a thousand density. Intersection is a merge, iteration is free, and a test is a binary search, which is why the sparse form is used where the consumer iterates and not where it tests.
- **Dense**, one bit per row with a rank index, above that. A hundred and fifty million rows is 18.75 MB, which fits in the last level cache of nothing but is streamed sequentially by a scan that tests it in `rid` order, so the access pattern is the good one rather than the random one. This is the form that matters and the form the sizes in document 01 section 1.6 refer to.
- **Full**, meaning every row, which is a flag and no allocation. A reduction that removed nothing must cost nothing downstream, and without this form it costs a bitmap of all ones and a test per row.

Three operations: intersect, union, and *push through a relationship*. The third is the one that is not in a normal bitmap library. Pushing a parent `Rids` forward to the child through a forward link is one pass over the link column testing each parent `rid` against the bitmap; pushing a child `Rids` backward to the parent is one pass over the link column setting a bit per surviving child. Both are sequential over the child, both are one bit test or one bit set per row, and both are exactly the primitive Bloom-filter predicate transfer approximates.

Against a Bloom filter at the same cardinality, the dense form is smaller, a Bloom filter with a one percent false positive rate is about ten bits per *inserted key*, and the bitmap is one bit per *possible key*, whenever the selectivity is above about ten percent, and it is exact at every selectivity. Below that, the sparse form is smaller than either. There is no regime in which a Bloom filter over a dense id space is the right structure, which is the entire content of the observation in document 01 section 1.9, and it is why a Bloom filter appears nowhere in this directory except as the fallback for a join with no link.

## 4.4 Who pays

Every allocation in this document is charged against `rudb_common::Memory` through a `Reservation`, with no exceptions and no structure exempted for being small. Issue #735 says the engine currently holds about three times the memory it charges against its limit, and the way that number got to three is one uncharged structure at a time. A `Rids` over a hundred and fifty million rows is 18.75 MB and a query with six of them is 112 MB, which is not noise.

Charging has a second purpose here, which is that it makes the graph path *fail over* rather than fail. When a reduction's bitmap cannot be reserved, the reduction is skipped and the join runs without it. That is a slower query and a correct one, and it is the same shape as the spill decision in `crates/rudb-exec/src/spill.rs`.

Resident caches, key maps, select indices, prefix sums, are charged to a separate budget from the query's, evicted least-recently-used when it binds, and rebuilt on next use. They are a cache of something on disk, so eviction is free of correctness consequences, which is section 3.1 paying off again.

## 4.5 Concurrency

Every structure here is immutable once built. A key map is built under a `OnceLock` per relationship, and the losing thread waits rather than building a second copy. Link pages are immutable. A `Rids` is built by one operator, sealed, and then read by every worker, which is the same lifecycle the gather's `Rows` already has in `crates/rudb-exec/src/gather.rs`.

The one structure built concurrently is a `Rids` produced by a parallel scan, where each worker sets bits for the rows in its own morsel. Since morsels partition the `rid` space and the dense form is words of sixty four bits, two workers can contend on at most the word at each morsel boundary. Workers write into per-morsel word ranges and the ranges are merged by the boundary words only, so there are no atomics in the loop, which matters because issue #512 says threads already cost twice the CPU at ten million rows and this layer must not add to that.
