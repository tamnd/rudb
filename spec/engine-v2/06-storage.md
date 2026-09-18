# Storage

Where the compact data model becomes bytes on a device, and where larger-than-memory starts.

## 1. What exists and what does not

`rudb-encoding` is 4,587 lines and is M1's result: a working encoder with a chooser that picked a six-level nest for `URL` and took `hits` to 9.65 GB. `rudb-parquet` is 5,867 lines and reads Parquet including Thrift metadata, page decoding, and hybrid RLE/bit-packing. `rudb-io` is 3,549 lines with a `File`/`Filesystem` trait pair, a real implementation, and a 945-line deterministic simulator. `rudb-compress` is 2,627 lines.

`rudb-storage` is 254 lines and is entirely `MemoryTable`, a `Vec<Chunk>`.

So the encoder is a laboratory, the reader is a reader, and there is no storage engine between them. F2 is the milestone that makes one.

## 2. Three storage surfaces, and only one of them is ours

**Parquet**, read-only at first, because ClickBench and TPC-H are distributed as Parquet and because `FROM 'hits.parquet'` is how the queries are written. It is the F0 path and it stays forever.

**DuckDB's format**, read and write, because on-disk compatibility is one of the three compatibility surfaces in [`../00-README.md`](../00-README.md) and storage v2.0 is what DuckDB v2.0 ships. `rudb-duckdb-format` is nine lines today. This is not an engine milestone; it is a compatibility milestone and it lives in the parent spec.

**The native format**, which is where the layout thesis gets to be true. Everything below is about that.

The relationship between the three matters for honesty: any ClickBench number measured on the native format is measured after a load step, and the load step's time and the resulting on-disk size are part of the published result. That is already how the board works, DuckDB loads in 126 seconds to 20.46 GB, ClickHouse in 219 to 9.42, Umbra in 164 to 8.30, and it is the column where rudb is currently weakest, because M1 measured encode throughput at 5 MB/s of values per core, which is 5.6 CPU hours for `hits`.

## 3. The shape

Four levels, and each exists because something needs exactly that granularity.

**File.** A footer with a catalog, a schema, the column-scoped dictionaries, and the block directory. Written last, read first, and following DuckDB v2.0's lazy column metadata, the block directory is loaded per column on demand, not wholly at open. On a 105-column table where a query touches three, that is the difference between parsing 105 columns of metadata and parsing three.

**Block.** 122,880 rows, matching the chunk width from [`05-data-model.md`](05-data-model.md) section 5 and DuckDB's row group. One block of one column is the unit of encoding: a block has one encoding nest, one bit-packing base, one FSST symbol table, one RLE run space. It is also the unit of morsel assignment, the unit of statistics, and the unit at which a scan can be skipped entirely.

**Page.** 256 KiB, the unit the buffer manager moves and pins. A block of a narrow column is one page; a block of `URL` is many. The page is the boundary between [`07-memory.md`](07-memory.md) and this document, and the reason it is fixed-size is that a buffer manager over variable-size pages is a memory allocator, and a memory allocator with an eviction policy is a bad memory allocator.

**Tile.** 1024 values, the unit FastLanes bit-packing interleaves over and the unit a kernel's inner loop works in. Not a stored structure, just an alignment constraint on the encodings, and the constraint is what lets an encoded kernel process a tile without a shuffle.

## 4. What a block carries besides data

Per block, per column, written at encode time:

Min and max, in the logical type, which is the zone map and gives row-group skipping.

Null count and distinct count. Distinct is exact when it is small and from the KMV sketch when it is not.

A KMV sketch. This is the asset v1's optimizer document identified correctly and it already exists: `rudb-encoding/src/sketch.rs`, 508 lines, with `union`, `jaccard` and `dependence`. Per-block sketches union into per-column sketches, and the set-intersection capability is exactly what join cardinality estimation needs. Nothing else in this design gives a cheap answer to "how many rows survive this join", and [`12-optimizer.md`](12-optimizer.md) depends on it entirely.

The encoding nest, as a small tree, so the reader knows what it is looking at without a dispatch table.

Optionally a Bloom filter, for columns the write path expects to be probed. Parachute's parameters are a starting point, capped at 8 KiB, m = 2^16, k = 2, about fifteen per cent space, and the decision to build one is a load-time heuristic with an override, like the global dictionary.

The total metadata budget is capped at two per cent of the data size. That number is a decision, not a measurement, and it is the kind of decision that should be revisited once sketches are actually driving plans.

## 5. Ordering is a storage decision

The most valuable thing a write path can do, and the one most likely to be underrated because it does not look like engineering.

`hits` is clustered by `EventTime`. That is why v1's expression document is right that adaptive conjunct ordering must use a running window rather than a whole-scan average. It is also why a min/max zone map on `EventTime` skips nearly everything for a date-ranged query and a zone map on `UserID` skips nothing.

The native format therefore records a **sort key** per table, chosen at load time, honoured on write, and used by the planner. It is not an index; there is no secondary structure. It is the physical order of the blocks, and its effects are: zone maps become selective, RLE becomes effective on correlated columns, front coding on a sorted dictionary becomes effective, and a merge join becomes possible without a sort.

Two costs. Sorting at load makes the already-slow write path slower. And a single sort key serves some queries and not others, which is the reason this is a table property with a default of "as written" rather than something the loader decides on its own.

The measurement that matters here is not a ratio against DuckDB; it is the ablation. F2's gate includes running ClickBench against the native format sorted and unsorted, because the difference is the honest price of the decision.

## 6. Larger than memory, on the storage side

Two distinct problems that get confused because they have the same name.

**Data larger than memory** is solved by the block and page structure and by nothing else. A scan reads a page, processes it, and lets it go. The buffer manager bounds residency. There is no design work here beyond not doing anything stupid, and the stupid thing, reading a whole column into a `Vec`, is prevented by the rule in [`07-memory.md`](07-memory.md) that no operator allocates outside the manager.

**Query state larger than memory** is the hard one and it belongs to [`07-memory.md`](07-memory.md) and the operators. Storage's contribution is one thing: the page layout that operator state spills into is the same page layout that persistent data uses. Kuiper, Boncz and Mühleisen's result is that this is what removes the serialisation cost from spilling, and it is the reason this document and that one share a page format.

Concretely: a hash table's partitions are pages. An external sort's runs are pages. A join's build side is pages. Spilling one of them is unpinning it; reading it back is pinning it; there is no encode or decode step in between. The only structure that needs a serialised form is one containing pointers, and the rule that makes that rare is in [`07-memory.md`](07-memory.md) section 6.

## 7. The write path

The part of the system that is currently twenty times slower than the read path and will embarrass the project if it is not planned for.

M1 measured 5 MB/s of values per core. `hits` is 105 columns and about 28 GB of values, so a full encode is 5.6 CPU hours. DuckDB loads the same file in 126 seconds. That is not a ratio anybody can publish.

Three things bring it down and they should be planned as a milestone rather than discovered as a crisis.

**The chooser is the cost, not the encoder.** An exhaustive search over a six-level nest per block per column is a search, and most of its cost buys nothing because the answer is stable across blocks of the same column. The fix is to choose on a sample of blocks and apply, with a periodic re-check and a fallback when the re-check disagrees. This is `storage.encoder` as a seam with `chooser-exhaustive` and `chooser-sampled` both in tree, and the ablation is how much size the sampled chooser gives up. M1's own data suggests very little.

**Encoding is embarrassingly parallel by block and by column.** The write path should saturate cores. That it does not today is a property of it being a laboratory.

**FSST and front coding dominate.** They are the two that produced the win and the two that cost the most, and they are the two worth hand-optimising.

The F2 gate for this is v1's and it is the right one: load time within 2x of DuckDB, with on-disk size not regressing past 0.50 of DuckDB's. On `hits` that is 252 seconds and 10.23 GB, against the 9.65 GB M1 already achieved. The size is already there. The time is 80x away.

## 8. Appends, updates and transactions

Out of scope for the engine, in scope for the parent spec, and named here only to fix the constraints the engine imposes on them.

Blocks are immutable once written. An append writes new blocks. A delete writes a deletion bitmap alongside the block, which the scan applies as an extra selection, and which costs one AND per chunk. An update is a delete plus an append. This is DuckDB's shape and it is compatible with everything above, including the global dictionary, provided appends may extend the code space.

`rudb-txn` is nine lines and stays that way until the engine number lands.

## 9. I/O

The interface is async from F0, which was v1's first contradiction with the parent spec and was correct.

```rust pub trait Filesystem {
    fn submit(&self, requests: Vec<Request>) -> Completion;
}
```

`read_at` becomes submit-one-and-wait, implemented in terms of `submit` rather than beside it, so there is no path through the engine that bypasses the queue. Two pools, as in DuckDB v2.0 and as in v1's design: compute sized to hardware threads, I/O sized to the device. Read-ahead depth governed by the memory limit rather than by a constant, because a read-ahead queue is memory.

No `io_uring`, no `xNVMe`. Zero external dependencies is a project rule, the fleet is a mix of Linux, macOS and Windows, and a thread pool over the standard library saturates every device the project owns. This is recorded as a decision with a trigger: if a measurement shows the I/O pool as the bottleneck on the reporting machine, it gets revisited, and the measurement to look at is in the metrics document already.

The 945-line simulator in `rudb-io/src/sim.rs` is the reason larger-than-memory can be tested without larger-than-memory data: it can report a small device, inject latency, and fail writes deterministically. [`15-testing.md`](15-testing.md) section 5 uses it for exactly that.
