# Storage: format, blocks, buffer manager, I/O

The disk format is where axis 4 is won or lost and it is where axis 2's largest single factor comes from. This document specifies the native format. Reading and writing DuckDB's format is a separate concern covered in document 12.1, and reading Parquet is covered in document 13.1. The encodings themselves are document 06; this document is about how bytes are arranged, found, cached and read.

## 5.1 The decision to have our own format

DuckDB v2.0 froze storage format v2.0 with DICT_FSST as a default compression method and versions 64 through 68 in the wild. We could adopt it. We are not going to, and the reason is arithmetic rather than preference: DuckDB's format stores this ClickBench dataset in 20.46 GB, Umbra stores it in 8.30, and axis 4 targets 2.05. A format designed around 2048-value vectors, per-column-independent compression and a fixed set of single-column encodings cannot reach 2.05 GB no matter how well it is implemented, because the redundancy the target depends on is between columns and the format has no way to express it.

So: a native format, plus import and export against DuckDB's format at full fidelity. `ATTACH 'file.duckdb'` reads a DuckDB database in place at DuckDB-like speed, and `EXPORT` writes one that DuckDB itself opens. Document 12.1 specifies that and it is a firm requirement, not a stretch goal, because a user with a 400 GB DuckDB file will not try a new engine that cannot open it.

**Multiple readers, one writer, one process.** Same concurrency model as DuckDB. Not a client-server database by default, though the Quack protocol in document 12.6 provides that shape on top.

## 5.2 File layout

A database is one file. The layout is:

```
  [ header 0 ] [ header 1 ]   4 KiB each, alternating, checksummed
  [ metadata blocks ]          catalog, row group index, block map, free list
  [ data blocks ]              variable size, exponential size classes
  [ WAL ]                      separate file, see document 11.3
```

**Two headers written alternately, each with a checksum and a sequence number.** A commit writes the header that is not currently live, fsyncs, and the newer valid checksummed header wins on open. This is DuckDB's scheme and it is correct: it gives atomic commit of a metadata version with one durable write and no journal. Header contents are a magic value, a format version, the sequence number, the root metadata block pointer, the block size class table and the free list root.

**Blocks are variable size in exponential classes starting at 64 KiB**: 64 KiB, 128 KiB, 256 KiB and so on to 16 MiB. LeanStore and Umbra both do this and the reason is that a fixed 256 KiB block is simultaneously too large for a small dictionary and too small for a large one, and the alternative to size classes is chaining, which puts a pointer chase in the middle of a scan. The size class of a block is recorded in the block map and is implied by its identifier's high bits, so a reference does not need a separate lookup to know how much to read.

**Metadata is in blocks like everything else** and participates in the buffer manager. There is no separately managed metadata region with its own caching rules, because a second caching path is a second set of bugs.

## 5.3 Row groups and column chunks

**A row group is 122,880 rows**, which is 120 vectors of 1024. DuckDB uses the same count as 60 vectors of 2048. Keeping the same row group size preserves comparability of every per-row-group statistic against DuckDB and, more practically, means an imported DuckDB table maps one to one onto our row groups with no re-chunking.

**Within a row group, each column is a column chunk**, and a column chunk is a sequence of encoded vectors plus a chunk header. The chunk header holds the encoding tree (document 06.3), the statistics, and the offsets needed for random access to any vector in the chunk. A column chunk is the unit of I/O and the unit at which an encoding decision is made.

**Random access to vector k within a chunk is O(1)**, which requires that either the encoding is fixed-width per vector or the chunk header carries an offset array. Both are supported and the header says which. This matters because a selective filter on one column must be able to fetch only the matching vectors of another column, and a format that only supports sequential decompression forces a full chunk decode to answer a point query. This is the single most important property distinguishing this format from a naive block-compressed one.

**Every column chunk carries statistics**: min, max, null count, distinct count estimate, and for string columns the FSST symbol table identifier and the dictionary identifier if one applies. Statistics are used for zone-map skipping at scan time and for cardinality estimation at plan time, and the same numbers serve both, which means a statistic that is expensive to maintain is expensive twice and one that is cheap is useful twice.

**Zone maps are per column chunk and per vector.** Per-chunk skipping avoids 122,880 rows of work, per-vector skipping avoids 1024. ClickHouse's granule is 8192 and its sparse index operates at that granularity; ours is finer, which costs more metadata and skips more precisely. The per-vector statistics are themselves stored compressed, because 105 columns times 814 row groups times 120 vectors is 10.3 million entries and storing that naively would be a measurable fraction of the 2.05 GB target.

## 5.4 Global structures

This is the part of the format that is genuinely different from DuckDB's and it is where the size and speed arguments both live.

**Global dictionaries are a first-class stored object.** A dictionary is scoped to a column across all row groups, not to a single row group. Storing `URL` as a per-row-group dictionary means the same URL string is stored 814 times, once per row group that contains it. Storing it globally means it is stored once and every row group holds only codes. This is where the largest share of the disk win comes from on this dataset and it is why document 06.5 spends its length on how a global dictionary is built incrementally, how it handles growth beyond a code width, and what happens on delete.

It is also where a large share of the speed win comes from, because a global dictionary makes a string column into a fixed-width integer column for the purposes of grouping, joining and comparison. `GROUP BY URL` becomes `GROUP BY u32` with a decode of only the ten surviving groups at the end. On this dataset that is queries 33 and 34, which are 1.46 seconds of Umbra's 8.10.

**The dictionary is a storage decision made by the system, not a type annotation made by the user.** ClickHouse gets much of the same benefit from `LowCardinality(String)`, but only when the schema author wrote it, which on an imported table or a `CREATE TABLE AS` they did not. Deciding it from measured data at write time is strictly more useful and is the thing that lets an unmodified ClickBench schema get the benefit.

**Shared FSST symbol tables.** FSST builds a 255-symbol table from a sample and compresses strings against it. Sharing one table across `URL` and `Referer` is correct here because they are drawn from the same universe of hostnames and paths, and it halves the symbol table storage while improving the compression of both. The format therefore stores symbol tables as independent objects that column chunks reference, rather than inline in each chunk.

**Cross-column references.** A column can be stored as a function of another column rather than as data. Two cases are in scope for M4: a column that is a derived function of another (`URLHash` is a hash of `URL`, which on this dataset is 800 MB of `BIGINT` that is pure redundancy), and a column whose values correlate strongly with another's such that storing the difference or the mapping is smaller than storing the values (`Title` against `URL`). The format expresses this as a stored recomputation rule in the column chunk header, and the scan layer evaluates it. Document 06.6 covers the rules, their cost model and the correctness constraint that a recomputed column must be bit-identical to what was written.

**The honest risk with recomputation rules** is that they trade disk for CPU, and a query that scans `URLHash` and nothing else now pays to decode `URL` and hash it. The cost model in 06.6 is required to consider that, and the rule is only applied when the column's measured access frequency is low or the recomputation is cheap relative to the I/O saved. If the measurement in M4 says the tradeoff is bad on real workloads, this feature is dropped and the disk target moves. That is stated in document 19 as open question four.

## 5.5 Buffer manager

**LeanStore-style with variable-size pages, pointer swizzling, and optimistic latching.** The reasons are in section 4.8 and the design is well documented in the LeanStore line of papers; this is not a place to innovate.

**Replacement is second-chance clock with a small hot set held out**, not LRU. LRU's failure mode is a large scan evicting everything useful, which is precisely the workload here. Pages touched by a sequential scan are inserted at the cold end and are the first evicted, which is the standard scan-resistance fix and costs one flag bit.

**Pins are RAII.** A pin is a guard object whose `Drop` unpins. This is the case where Rust genuinely removes a whole class of bug: a leaked pin in a C++ buffer manager is a slow leak that manifests as an out-of-memory hours later on an unrelated query, and it is one of the more painful bugs to find in any storage engine. Here it is not expressible.

**The buffer pool is the memory limit.** Query working memory, hash tables, sort runs and buffered pages all draw from one budget. An operator that wants 2 GB for a hash table asks the buffer manager, and the buffer manager may evict pages to grant it or may tell the operator to spill. There is no separate `max_memory` and `buffer_pool_size` with the sum exceeding the machine.

## 5.6 Compression is a storage-time decision with a measured cost model

When a row group is written, the writer samples each column chunk, tries the applicable encodings, and picks by a cost function over compressed size and estimated decode cost. That is what DuckDB does. Two differences.

First, the search is over encoding *trees* rather than single encodings, because cascading is where the ratios are: dictionary then bit-pack, FSST then dictionary, delta then FOR then bit-pack. The search space is bounded by a hand-written set of candidate cascades per type rather than by general search, because general search over encoding trees at write time is too slow and the marginal ratio from the tail of the space is small.

Second, the cost function's decode term is weighted by whether the encoding supports compressed execution. An encoding that is 5 percent larger but lets the group-by operator run on codes without decoding wins, because the disk saving and the CPU saving are the same decision. This is the concrete way the storage layer serves the execution layer and it is why documents 05, 06 and 07 have to be designed together rather than in sequence.

**Write throughput is a real constraint and it is not free.** DuckDB loads this dataset in 126 seconds and Umbra in 164. A cascading search over multi-column candidates will be slower, and the target is to stay under 300 seconds for a single-pass load, with a `PRAGMA optimize`-style background recompression pass available for people who want the best ratio and can wait. Document 15 requires load time to be reported in every benchmark result, so this cost cannot be quietly hidden in the way that a hot-runtime-only comparison would allow.

## 5.7 I/O

**Direct I/O with `O_DIRECT` on Linux and `F_NOCACHE` on macOS, by default.** The buffer manager is the cache. Double buffering through the page cache costs memory that axis 4 does not have and adds a copy that the throughput does not have room for.

**io_uring on Linux, with registered buffers and fixed files.** PVLDB 19(1) 2025 is the current careful study of io_uring for database I/O and its finding is that io_uring with registered buffers and polling substantially beats a thread pool doing `pread`, but that the configuration matters more than the interface does. Registered buffers avoid the per-operation page pinning, fixed files avoid the descriptor lookup, and `IOPOLL` avoids the interrupt. `SQPOLL` dedicates a kernel thread to submission and is a win at high queue depth and a waste at low, so it is on above a measured threshold and off below.

**There is a counter-result and it is worth naming.** Conviva published a migration to io_uring that got worse throughput than the thread pool it replaced, because their access pattern was not deep enough to amortize the setup. The lesson is that io_uring is not a free win, it is a win at depth. So: the I/O layer has two backends, io_uring and a thread pool doing `pread`, both maintained, both tested, chosen by measurement at startup and overridable by setting. This is not fence-sitting, it is that the correct choice is genuinely workload dependent and pretending otherwise would cost us the small-query case.

**Prefetch is driven by the plan, not by heuristics.** The scan knows which row groups survive zone-map pruning before it reads any of them, so it can issue the whole list of reads at once and let the completion order drive the morsel order. This is strictly better than a sequential readahead heuristic because it never prefetches a pruned row group. Velox's adaptive prefetch does something similar and its reported win is significant on selective scans.

**NVMe passthrough via `io_uring` `OP_URING_CMD` is on the list and not in the plan.** It bypasses the block layer for another 10 to 20 percent at the cost of a filesystem-free device, which is not a thing an embedded database can require. It is revisited if a server deployment mode appears.

**Object storage is a separate backend with different rules.** S3-style ranged GETs, 8 to 16 MiB target request size, high concurrency, and aggressive caching to a local file. The format's per-vector random access is what makes this workable, because it means a selective query fetches a few ranges rather than whole column chunks. Document 13.4 covers the lakehouse side.

## 5.8 Row group and file maintenance

**Appends go to new row groups; updates are MVCC deltas** as described in document 11.2. A row group accumulates deltas until a threshold, then is rewritten and recompressed in the background. This is the standard design and the only note is that the rewrite is where a row group gets the benefit of a better global dictionary that was built after it was written.

**Deletes are a deletion bitmap per row group.** A row group whose deletion bitmap exceeds a threshold is rewritten and its space freed. There is no in-place tuple removal.

**`CHECKPOINT` is explicit and also automatic** at a WAL size threshold. Document 11.4 covers it.

**Free space is a per-size-class free list in the metadata.** A freed block goes on its class's list and is reused before the file grows. `VACUUM` compacts, meaning it rewrites live blocks to the front of the file and truncates, and it is offline in the sense that it takes the write lock for the duration. Making vacuum online is a real piece of work and it is scheduled in M9, not before.
