# The chunk and the page

Notes written on 19 September 2026 against rudb 0.3.59, measured on server2, which is six AMD EPYC cores with 32 KiB of L1d and 512 KiB of L2 each and 8 MiB of shared L3, and which was idle, with DuckDB v2.0.0-dev84237 on the same machine and the same data.

The question these notes answer is whether the in memory data model is the wrong one. One piece of it is, and it is not the piece I went looking for, so this note is the measurement first and the design second.

The short version. rudb's inner loops are competitive with DuckDB's. What rudb loses on is everything that happens once per chunk, it pays that cost twice as often as DuckDB does because its chunk is half the size, and on top of that its chunk is also its unit of storage, so every chunk of every scan costs an allocation and a copy that DuckDB does not pay at all.

## 1. The workload

One table, twenty million rows, two `BIGINT` columns, built in memory and never written to a file.

```sql
CREATE TABLE fact AS SELECT (i * 104729) % 20000000 AS k, i AS v FROM range(20000000) t(i);
```

One thread, best of four repetitions, wall clock out of `--metrics`. Nothing here reads a file or a page cache, so what is being measured is the scan, the chunk and the aggregate and nothing else. `k` is scattered over the whole range and `v` is the row number, which matters because it decides what a zone map can prove.

## 2. The floor

Before any comparison, what the machine can do. A C program that reads a 160 MB array of eight byte values and adds them up, unrolled by four, compiled at `-O3 -march=native`:

| | seconds | ns per row | GB per second |
| --- | ---: | ---: | ---: |
| one core, straight through | 0.0167 | 0.84 | 9.6 |
| one core, in 1024 row pieces, each malloced, copied and freed | 0.0227 | 1.13 | |

So 0.84 ns a row is the floor for any query that has to look at every value of `v` on this machine, and the difference between the two rows is what cutting the array into 1024 row pieces and copying each one costs, which is about 0.3 ns a row on top of a read that was going to happen anyway.

## 3. Where the time goes

Nanoseconds per row, one thread, best of four.

| query | rudb | duckdb | what duckdb does |
| --- | ---: | ---: | --- |
| `count(*)` | 0.71 | 0.10 | answers from the row group metadata |
| `count(v)` | 2.66 | 0.10 | metadata again, because the column has no nulls |
| `count(*) WHERE v >= 0` | 3.99 | 0.10 | the minimum of `v` is zero, so the filter is always true and is removed, then metadata |
| `sum(v)` | 3.43 | 1.75 | reads the column |

Three of those four are not a fair comparison and they are in the table because the unfairness is the point. DuckDB answers them out of statistics it wrote when the table was built, in two milliseconds each, and rudb reads twenty million rows to answer the same questions. That is a real gap and it is not a loop to make faster, it is a fact that never reaches the planner, which is what milestone P0 is about.

`sum(v)` is the fair one. DuckDB is 2.1 times off the floor and rudb is 4.1 times off it, so rudb is 1.96 times behind on the one query here that both engines actually run.

## 4. Seven hundred nanoseconds a chunk

The same four queries, same machine, same data, one binary per value of `VECTOR_SIZE` and nothing else changed.

| rows per chunk | `count(*)` | `count(v)` | `count(*) WHERE v >= 0` | `sum(v)` |
| ---: | ---: | ---: | ---: | ---: |
| 1024 | 0.71 | 2.66 | 3.99 | 3.43 |
| 2048 | 0.35 | 1.80 | 2.38 | 2.49 |
| 8192 | 0.11 | 1.47 | 1.81 | **1.76** |
| 32768 | 0.02 | 1.17 | 1.85 | 1.84 |

DuckDB answers `sum(v)` in 1.75. So one constant, changed and nothing else, takes rudb from 1.96 times behind DuckDB to level with it on this query.

The curve turns back up between 8192 and 32768 for the two queries that read a column, which is the cache effect Kersten and others measured and is section 8. The arithmetic on this machine is that a vector of 32,768 eight byte values is 256 KiB, and a scan that copies one is holding 512 KiB of them at once against an L2 of 512 KiB, so the copy stops fitting. A vector of 8,192 is 64 KiB, which is over the 32 KiB L1d and well inside L2, and that is where the bottom of the curve is.

Read the `count(*)` column as nanoseconds per chunk rather than per row and it stops moving.

| rows per chunk | ns per chunk for `count(*)` |
| ---: | ---: |
| 1024 | 727 |
| 2048 | 717 |
| 8192 | 901 |
| 32768 | 655 |

A chunk costs about seven hundred nanoseconds to push through the pipeline before anything has looked at a value. That number is a property of the chunk and not of the rows in it, and at 1024 rows it is the whole of what counting twenty million rows costs.

## 5. What a chunk costs, in instructions

Wall clock on a shared machine is an argument. Instruction counts are not, so the same two queries under callgrind, over a one million row copy of the same table, twenty repetitions, with a run that only builds the table subtracted so that what is left is the queries. These counts were taken against 0.3.58, which differs from 0.3.59 by one optimiser pass for disjunctions that none of these queries has one of.

`count(*)` costs about 3,800 instructions a chunk, which is 3.7 instructions for every row it counts without reading a single value.

| what | instructions per chunk | share |
| --- | ---: | ---: |
| the metrics shim and the clocks it reads | 1,497 | 40% |
| malloc and free | 630 | 17% |
| the aggregate | 368 | 10% |
| memcpy | 210 | 6% |
| the scan and the table read | 159 | 4% |
| everything else | about 940 | 25% |

`sum(v)` costs about 13,700 instructions a chunk, which is 13.4 a row.

| what | instructions per chunk | share |
| --- | ---: | ---: |
| the accumulator's own loop | 5,648 | 41% |
| malloc and free | 1,869 | 14% |
| memcpy of the chunk's column | 1,803 | 13% |
| the metrics shim and the clocks it reads | about 1,400 | 10% |
| everything else | about 3,000 | 22% |

Two things in those tables are worth saying out loud.

The accumulator's 5,648 instructions for 1024 values is 5.5 instructions a value, and the reason is that `whole_sum` in `crates/rudb-kernels/src/aggregate.rs` adds `i128::from(value)` a row at a time, because the sum of a `BIGINT` column is a `HUGEINT` and a 128 bit accumulator does not vectorise. That looks like the thing to fix until you check what DuckDB returns from the same query, which is also `HUGEINT`, so it is accumulating at the same width for the same reason and it answers in 1.75 ns a row. There is something to win here eventually, by summing in 64 bits and widening only when the zone map cannot prove that the chunk fits, but it is not where rudb is losing today.

Where rudb is losing is the other 8,000 instructions. Of those, 3,672 are malloc, free and memcpy, which exist because of the data model, and about 1,400 are the cost of measuring a chunk, which at 1024 rows a chunk is a tax on the engine's shape rather than on its work. Forty percent of a `count(*)` chunk is the measurement of that chunk.

## 6. What the data model forces

`crates/rudb-vector/src/buffer.rs` holds the one structural fact this note is about.

```rust
enum Store<T> {
    Owned(Vec<T>),
    Shared(Arc<Vec<T>>),
}
```

A buffer is a whole allocation. It has no offset and no length of its own, so a buffer cannot be a window into a larger buffer, and a run of 1024 values out of a page of 122,880 is not something this type can express.

Every other form of a vector can already be cut for nothing. `Vector::slice` moves an offset for a bit packed body, shares the dictionary and copies only the codes for a dictionary body, shares the arena and copies only the views for a string body, shares the code page and copies only the spans for an FSST body, and keeps the runs it touches for a run length body. The flat body is the only one that copies the values, in `run_of`, because it is the only one whose buffer cannot name a piece of itself.

That has three consequences and they are the whole finding.

A table has to be stored pre cut. `MemoryTable` in `crates/rudb-storage/src/memory.rs` is a `Vec<Chunk>` of vector sized chunks, which for this table is 19,532 chunks and about 39,000 buffers, and its own module doc says it is not the storage format and that there are no row groups in it. It is that way because handing out a piece of a larger column is not something the vector type can do.

A read still copies. `MemoryTable::read` clones the columns of the chunk it was asked for, and its doc says why: the columns are copied because a vector owns its buffer and there is nothing to borrow from yet. So the copy is not an oversight, it is the honest consequence of the buffer type, and it is the 1,803 instructions of memcpy and most of the 1,869 of malloc and free in the table above.

The storage granularity is welded to the vector size. Because a chunk of the table is a chunk of the execution, one number has to be right for two unrelated questions at once: how many values fit in cache while an operator works on them, and how many rows a zone map, a compression scheme and a parallel morsel should cover. Those have different answers. This table gets one zone map for every 1024 rows, which is finer than any system in section 7 and is built at a cost `stats_ns` already reports.

## 7. What the other systems do

None of these systems has one number for both questions.

**Arrow**, and therefore **Polars**, defines a buffer as a pointer, a length and a shared owner of the allocation, so `Buffer::slice` is pointer arithmetic and a reference count, and `ArrayData` carries an element offset on top of that. Polars holds a column as a `ChunkedArray`, which is a list of Arrow arrays, and provides `rechunk` because too many small chunks costs dispatch and contiguity. Slicing anywhere in that stack is free and always has been.

**DuckDB** stores a table in row groups of 122,880 rows, which is 60 times its vector size of 2048, with a zone map and a compression choice per column segment inside the row group. A `Vector` either owns its data or points into something else through a `VectorBuffer`, and for an uncompressed column a scan sets a pointer into the buffer managed block rather than copying, holding the block with a pin for as long as the chunk lives. The 2,048 and the 122,880 are separate numbers because they answer separate questions.

**Velox** holds a vector as reference counted `BufferPtr`s, and slicing, dictionary wrapping and constant wrapping are all done by rewrapping rather than by copying. Lazy vectors go further and defer the read itself.

**ClickHouse** has granules of 8,192 rows in MergeTree with a mark per granule, and `IColumn::cut` on top, which is again two numbers.

## 8. What the papers say about the two units

The separation of the two units is not new and it is not controversial.

Moerkotte's small materialized aggregates, VLDB 1998, put the synopsis on a block, and the point of the paper is that the block is large enough that the synopsis is cheap to keep and small enough that it still prunes.

Boncz, Zukowski and Nes, MonetDB/X100, CIDR 2005, chose the vector to fit in cache and were explicit that it is a different unit from the storage block, which is the sentence the whole vectorised model rests on.

Leis, Boncz, Kemper and Neumann, Morsel-Driven Parallelism, SIGMOD 2014, put the unit of parallel work at about a hundred thousand rows, which is a third unit again, chosen for scheduling and for NUMA locality rather than for cache.

Lang and others, Data Blocks, SIGMOD 2016, is the closest ancestor of what DuckDB stores: blocks of tens of thousands of rows, each one compressed with a scheme chosen for that block, each one carrying its own synopsis, scanned into vectors that are much smaller than the block.

Kersten and others, Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask, VLDB 2018, swept the vector size and found the curve flat over a wide range and falling off only when the vector stops fitting in cache. The interesting half for us is the other end: their curve also degrades below about a thousand values, because the per vector costs stop being amortised, which is the wall this note has walked into from the other side.

Afroozeh and Boncz, The FastLanes Compression Layout, 2023, is why rudb's vector is 1024, and it is a claim about the unit a bit packed run is encoded in. It is not a claim about how many rows a table should store together.

## 9. What to change

Four things, separable, in the order the measurement puts them.

**A buffer becomes a window.** `Store::Shared` grows an offset and a length, `Buffer::slice` becomes pointer arithmetic and a reference count bump, and a producer that means its buffer to be cut many times promotes it to a page once. The flat body stops being the one form that cannot be cut. Nothing above the buffer changes, because every reader goes through `as_slice` and `Deref`, and there is exactly one caller of `Buffer::from_arc` in the workspace today.

**The in memory table holds pages rather than chunks.** One page per column per row group, a zone map per row group, and `MemoryTable::read` hands out windows into the page instead of copies of a chunk. The chunk numbering that `Rows::read` takes does not have to change, so the operators above see nothing. For the table in section 1 the allocation count goes from about 39,000 to a few hundred, and the memcpy and most of the malloc in section 5 go to zero.

**The chunk size gets decided on its own merits.** That is issue #480, which has been open on the grounds that nobody had measured it. Section 4 is the measurement. It is not the whole answer, because these four queries are one scan and one aggregate and the number also has to be right for a hash join's build side and for many operators in flight at once, but 1024 is not defensible as a default any more and the reason it was chosen, the FastLanes unit, is a claim about encoding rather than about execution.

**The measurement of a chunk gets cheaper or rarer.** Forty percent of a `count(*)` chunk is the shim. #963 took the thread clock out of it and what is left is the wall clock, read twice per operator per call, plus the counter writes. Once a chunk is larger this is amortised and may not be worth touching, which is why it is last rather than first.

## 10. What this means for ten times

The floor in section 2 is worth restating as a limit. On `sum(v)` DuckDB is 2.1 times off what one core of this machine can read, so the entire available win on that query, for any engine, by any means, is 2.1 times. Ten times DuckDB is not reachable on a query that reads the bytes.

It is reachable on the other three queries in section 3, and by a lot more than ten times, because DuckDB answers those without reading anything and rudb reads everything. That is the shape of the goal: the wins are in not touching the data, through facts that reach the planner, zone maps that prune, dictionaries that make a group key an integer and execution that runs on the encoded form. Those are what [`../engine-v2/05-data-model.md`](../engine-v2/05-data-model.md) sections 6 and 7 and [`../engine-v2/13-encoded-execution.md`](../engine-v2/13-encoded-execution.md) are for, and they are what milestones P0 and P2 are about.

The work in section 9 is not that. It is the floor under it: a data model in which a scan can hand an operator a window into stored memory, so that the engine is paying for the work the query asks for and nothing else. It is worth doing first because everything above it is measured through it.

## 11. What the first two changes measured, and the one that was not on the list

Written on 19 September 2026, after the first two items of section 9 landed. server2 was not idle for these, so they are internal comparisons: the same tree built twice, differing only in the change under test, run alternately and best of each. Nanoseconds per row over the same twenty million rows.

The buffer window, #971, is a type change and measures nothing on its own, which is why it landed on its own. The pages, #972, took the copy out of `MemoryTable::read`.

| query | before pages | after pages | |
| --- | ---: | ---: | ---: |
| `count(*)` | 0.70 | 0.64 | -9% |
| `count(v)` | 2.18 | 1.26 | -42% |
| `count(*) WHERE v >= 0` | 2.85 | 2.56 | -10% |
| `sum(v)` | 2.88 | 2.55 | -11% |

The spread is the right shape. `count(v)` reads the validity and never touches a value, so the copy was pure waste and all of it went. `sum(v)` reads every value out of RAM either way, so what came off is the copy's own cost and not the read it was feeding. The copy was worth between a third of a nanosecond and nine tenths of one per row depending on whether the query was going to look at the bytes.

Then the box went idle and the four queries were run again against DuckDB, and that measurement found something section 9 had not listed. DuckDB answered three of the four out of metadata with no operators at all, `EXPLAIN ANALYZE` reporting a total time of zero and an empty plan, and only `sum(v)` scanned its 163 row groups. rudb scanned all four. The gap on those three was not the scan loop, it was that rudb was scanning at all.

rudb had the numbers already. A table in memory builds a zone map for every chunk as it arrives, and a zone holds an exact null count for every column whatever form it is in, the two ends, and the total of an integer column. A native file had been answering `COUNT`, `MIN`, `MAX`, `SUM` and `AVG` over a whole table out of its directory since the format was written. The in memory table answered `None` to all of it, and `whole_table` in the builder turned away anything that was not a file, so the whole apparatus was there and switched off for the tables most queries actually run against.

Turning it on, with the answers checked against DuckDB value for value:

| query | reading rows | out of the zone maps |
| --- | ---: | ---: |
| `count(*)` | 0.72 | 0.00 |
| `count(v)` | 1.32 | 0.01 |
| `count(*) WHERE v >= 0` | 3.01 | 0.05 |
| `sum(v)` | 2.72 | 0.02 |

The filtered count is in there because the optimizer folds a predicate the statistics prove is true for every row, which left a bare read of the whole table for the counting to be answered from. `sum(v)` is the one DuckDB still scans for, so on that query rudb is now faster than DuckDB by about two orders of magnitude rather than slower by 1.6 times.

This is a load time cost being spent rather than a query getting faster, and the load was already paying it. Building the table takes 1.0 seconds either way, because the zone maps were always built. What changed is that somebody finally asked them.

Queries that have to read rows are unaffected, which is the thing worth checking about a change like this. `count(*) WHERE v >= 10` is 4.32 before and 4.48 after, `sum(v) WHERE v >= 10` is 4.15 and 4.19, `sum(k + v)` is 1.56 and 1.49, `count(*) WHERE k = 7` is 1.54 and 1.45. That is noise in both directions.

The lesson to carry out of this is the one section 10 stated and then did not act on. The wins are in not touching the data, and the cheapest of those are the ones where the engine already computed the fact and never wired it to the question. It is worth going looking for the others.

## 12. What the filter moving into the scan measured

Written on 19 September 2026, after the third item of the list that section 11 ended with. A zone map that can prove a chunk holds nothing the filter wants can also prove a chunk holds nothing the filter would throw away, and only an operator with the zone and the predicate in front of it can act on the second half. So the filter moved. A predicate whose every conjunct reads as a comparison of one of the scan's own columns against a constant is handed to the scan and no filter operator is built above it, and the scan then decides per chunk between skipping the chunk, skipping the comparison, and running it.

These were run on gamingpc rather than server2, so they are not comparable with the numbers above, only with each other. One thread, the same twenty million rows, best of four, nanoseconds per row. The DuckDB column is v2.0.0-dev84237 on the same box with the same threads setting.

| query | filter above the scan | filter in the scan | DuckDB |
| --- | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 1.58 | 0.25 | 0.40 |
| `sum(v) WHERE v >= 10` | 1.86 | 0.99 | 1.00 |
| `sum(k + v)`, no filter | 1.57 | 1.58 | 1.80 |
| `count(*) WHERE k = 7` | 0.38 | 0.25 | 0.10 |

The first row is the case this exists for. `v` runs with the rows, so every chunk but the first is one where the comparison was going to keep every row and the only thing it produced was the knowledge that it had. Six times, and now faster than DuckDB on the same query.

The second row is the same predicate with the values read. Half the win survives, which is the right amount: what came off is the comparison and the selection vector, and what is left is twenty million eight byte values going past the summing loop whatever anybody proved about them. This is the query section 10 said had 2.1 times in it for any engine by any means.

The third row is the control. No filter, no change, and the number says the scan loop was not touched.

The fourth row is the one to keep looking at. A needle in a scattered column is chunks ruled out rather than chunks waved through, and that part was already here, so the win is only the filter operator that is no longer built and no longer handed anything. DuckDB is still two and a half times faster, and the reason is that it decides this over row groups of 122,880 rows while rudb decides it 19,532 times over chunks of 1,024. That is issues #984 and #480, in that order, and it is the next thing.

One note about the apparatus rather than the engine. `scan.py` grouped its runs by counting positions past the `CREATE TABLE`, which stopped reporting a timing at some point, so every label was one query out. It groups by the query text now. The numbers in section 11 were taken before that happened and a spot check of them holds.

## 13. What the row groups measured, and the allocator that was hiding in front of them

Written on 21 September 2026, after the second item of section 9, the one section 12 called the next thing. The in memory table now holds one page per column per row group of 122,880 rows, and a chunk is a window into a page rather than an allocation of its own. Twenty million rows of two columns went from 19,532 chunks and about 39,000 buffers to 163 groups and 326 pages.

The first time this was measured it read as a mixed result and one clear regression: `count(*) WHERE k = 7` eleven percent slower, the load nineteen percent slower, and peak memory up from 336 MB to 619 MB. None of that was the change. `perf record` over the query phase alone put eleven percent of the time in `unlink_chunk`, seven and a half in `malloc_consolidate` and three in `_int_malloc`, none of which appear in the build without row groups. Sealing a group allocates a 983 KB page per column and frees 240 chunk buffers of 8 KB, the chunks were produced on the worker threads and are freed on the thread draining the query, and glibc returns a block to the arena of the thread that allocated it. Three checks confirmed it: turning sealing off put the memory back at 337 MB, a `malloc_trim` per group held it flat, and `SET threads=1` on the load made the whole thing disappear.

So the allocator went first, as #1060, and everything below is measured on top of it. That is worth stating as a method rather than as a footnote. A change that moves allocation patterns cannot be measured against a baseline whose allocator is the bottleneck, because what gets measured is the allocator.

### What the row groups are worth

server2, six cores, twenty million rows, nanoseconds per row, best of four, against the commit before this one. The DuckDB column is v2.0.0-dev84237 on the same box with the same `threads` setting, best of four, out of its own timer.

One thread:

| query | before | after | | DuckDB |
| --- | ---: | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 1.00 | 0.83 | -17% | 0.70 |
| `sum(v) WHERE v >= 10` | 2.82 | 2.11 | -25% | 1.50 |
| `sum(k + v)` | 4.21 | 3.69 | -12% | 2.50 |
| `count(*) WHERE k = 7` | 0.84 | 0.77 | -8% | 0.10 |

Six threads:

| query | before | after | | DuckDB |
| --- | ---: | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 0.53 | 0.50 | -6% | 0.30 |
| `sum(v) WHERE v >= 10` | 0.82 | 0.88 | +7% | 0.45 |
| `sum(k + v)` | 1.03 | 1.04 | 0% | 0.75 |
| `count(*) WHERE k = 7` | 0.39 | 0.20 | -49% | 0.10 |

The single threaded column is the one the change was made for and it moves everywhere, because the read that used to copy a chunk out of the table now bumps a reference count. The six thread column is flat on the three queries that read every row, which is the honest reading of it: six cores over twenty million rows are already waiting on memory, and taking a copy out of a loop that was bound by bandwidth gives the bandwidth back rather than the time.

### The needle, which is not the query anybody thought it was

`count(*) WHERE k = 7` is the row section 12 said to keep looking at, and looking at it turned up something about the benchmark rather than about either engine.

DuckDB answers it in 2.3 ms and its own `EXPLAIN ANALYZE` says why: `Row Groups Scanned: 5 / 163`. That is not a clever technique, it is the data. `k` is `(i * 104729) % 20000000`, which is a permutation of the whole range, so a row group of 122,880 rows holds values spread over all of it: the second group runs from 1,029 to 19,999,972. The maximum rules nothing out and the minimum rules out almost everything, because the smallest of 122,880 values drawn from twenty million lands near a thousand and the needle is 7. Move the needle into the middle of the range and the pruning goes away completely. `count(*) WHERE k = 12345678` scans 163 of 163 row groups and takes 49.9 ms.

So there are two queries here and the table above only has one of them. Both, one thread, wall clock:

| query | before | after | DuckDB |
| --- | ---: | ---: | ---: |
| `count(*) WHERE k = 7`, prunes | 19.7 ms | 16.1 ms | 2.3 ms |
| `count(*) WHERE k = 12345678`, does not | 101.4 ms | 83.0 ms | 49.9 ms |

The second row is a scan and rudb is 1.7 times behind on it, which is the same gap as everywhere else in this note. The first row is a pruning benchmark and rudb is seven times behind, and it is behind for a reason that has nothing to do with how well it prunes. rudb prunes better than DuckDB does here: a zone map over 1,024 rows has a minimum near twenty thousand rather than near a thousand, so almost every chunk is ruled out. Then it pays 16.1 ms to rule them out, which over 19,532 chunks is 824 nanoseconds each, and 824 nanoseconds a chunk is section 4's number. The scan is proving there is nothing to read and then paying the full price of a chunk to say so.

The cause is in `crates/rudb-exec/src/source.rs`. A morsel for an in memory table is one chunk, so `Source::morsels` hands out 19,532 of them, and the loop in `Scan::read` that walks past ruled out parts without returning cannot walk anywhere, because the morsel it is walking is one chunk long. Every pruned chunk costs a full call into the pipeline to produce nothing. The native file path already solves this: a morsel there is a stripe, the walk has somewhere to go, and `stripe_skips` rules out a whole stripe out of the directory before any of it is looked at.

That is the rest of this box and it is the next change: a zone map per row group, and a morsel that is a row group. The group bound is weaker than the chunk bound, so it is asked first and the chunk bounds still decide inside a group that survives, which is the two level shape the native reader already has. On this query it should take the 824 nanoseconds a chunk down to 163 comparisons and the chunks of whichever groups those leave.

### The load, which got worse

| | before | after |
| --- | ---: | ---: |
| wall | 0.44 s | 0.73 s |
| peak RSS | 336 MB | 588 MB |

This is real and it is worth being plain about. A `CREATE TABLE AS` materialises the whole result before it appends any of it, so at the moment the last chunk arrives the process is holding twenty million rows as chunks and is about to hold them again as pages. Before row groups the append moved the chunk into the table and there was only ever one copy. Now it copies into the page and frees the chunk, and freed 8 KB blocks do not come back as 983 KB pages, so both are resident at the peak. The extra 0.3 seconds is mostly the fresh page faults on the 250 MB the second copy takes.

The fix is not in the storage layer. It is to stop materialising the result: `run` in `crates/rudb/src/database.rs` collects every chunk into a `Vec` and hands it back, and the insert paths then walk it. A sink that takes each chunk as it is produced would mean the chunks are freed as fast as they are made and the peak would be the table and nothing else. That is the next box after this one, and it is what the 588 MB is waiting for.

### What this says about the list in section 9

Two of the four items are done and the third, the chunk size, is now the biggest single number left: section 4 measured 8192 as worth about 1.9 times on `sum(v)` and the row groups have removed the reason the chunk had to be small, which was that it was also the unit of storage. It is not removed for free, because 163 groups of 1024 row chunks are 19,532 pipeline calls and at 8192 they would be 2,442, which is most of what the morsel change above is trying to win. The two interact and the morsel change is the cheaper of them, so it goes first.

## 14. The zone map per row group, and the morsel that is one

Written on 21 September 2026, the same day as section 13 and directly on top of it. Section 13 ended by naming what was left of the box: a zone map per group, asked before the chunk zones, and a morsel that covers a run of chunks rather than one. Both are in.

The fold is the part with a decision in it. A group's zone is its chunk zones folded together as the group fills, not a second walk over the rows at the seal, because the chunks are right there and walking twice would cost the load what the zone maps already cost it. Folding is conservative in every direction: the ends open out, the null counts add, an exact range folded with a wide one is wide, and the sums drop to nothing if either side had nothing or if they overflow, which on a hundred and twenty chunks of `BIGINT` is where a `i128` accumulator finally has to be checked rather than assumed. The one case worth naming is a chunk that looked at every row and found no value, which is a column of nothing but nulls. That is not the same as a chunk whose form the walk cannot read, and treating them the same would cost a whole group its ends over one empty chunk, so a range that is exact and has no low end means the first and a range that is not exact and has no low end means the second. Those are the only two ways to have no ends and they have to be told apart.

Above that the change is small because the native file already had the shape. A stripe and a row group are the same thing to a scan, a run of parts written together that can be ruled out together, so the catalog answers `stripe_parts`, `stripe_rows` and `stripe_skips` off the row groups for a table in memory, and `Source::morsels` stopped returning early for a table that is not native. The test there is now on whether the table says how its chunks are grouped, not on what format it is, which is the right test and was the wrong one only because memory used to be the format with no groups.

### What it measured

server2, six cores, twenty million rows, nanoseconds per row, best of four inside a run and the best of three interleaved runs, against the commit before this one. DuckDB is v2.0.0-dev84237 on the same box with the same `threads` setting and out of its own timer.

One thread:

| query | before | after | | DuckDB |
| --- | ---: | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 0.81 | 0.70 | -14% | 0.90 |
| `sum(v) WHERE v >= 10` | 2.33 | 2.02 | -13% | 1.85 |
| `sum(k + v)` | 3.81 | 3.66 | -4% | 3.00 |
| `count(*) WHERE k = 7` | 0.73 | 0.01 | -98% | 0.15 |

Six threads:

| query | before | after | | DuckDB |
| --- | ---: | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 0.46 | 0.26 | -44% | 0.45 |
| `sum(v) WHERE v >= 10` | 0.86 | 0.57 | -35% | 1.00 |
| `sum(k + v)` | 1.12 | 0.87 | -22% | 1.05 |
| `count(*) WHERE k = 7` | 0.20 | 0.01 | -95% | 0.15 |

The needle is the row this was built for and it does what section 13 said it would. `count(*) WHERE k = 7` was 14.6 milliseconds at one thread, which over 19,532 chunks is 748 nanoseconds each to decide to read nothing, and it is now 0.3 milliseconds. The 163 group zones rule out 158 of the groups without any of their chunks being looked at, the five that survive have their chunks ruled out by the chunk zones as before, and the scan walks past them inside one morsel instead of returning through the pipeline between each. At six threads it is 4.0 milliseconds down to 0.2. DuckDB answers the same query in 3.0 at either thread count, so this is the first row of the pushdown table where rudb is ahead rather than behind, and it is ahead for the reason section 13 gave: a zone map over 1,024 rows rules out more of this data than one over 122,880 does.

The three queries that read every row were not the point and they moved anyway, more at six threads than at one. That is the morsel and not the zone map. A worker used to be handed one chunk per call into the pipeline and is now handed a run of a hundred and twenty, so the fixed cost of entering and leaving the pipeline is paid 163 times rather than 19,532, and at six threads the handout itself was contended. Two of the four rows now beat DuckDB at six threads and a third is level with it, which is not a claim about either engine so much as a note that the gap section 13 measured as uniformly 1.7 times was partly this.

### The other needle, which did not move

| query, one thread | before | after | DuckDB |
| --- | ---: | ---: | ---: |
| `count(*) WHERE k = 7`, prunes | 14.6 ms | 0.3 ms | 3.0 ms |
| `count(*) WHERE k = 12345678`, does not | 84.5 ms | 88.5 ms | 51.0 ms |

The second row is the one to be careful about, because the first reading of it said four percent slower and a second said twenty four. Neither is a measurement. That query on this box spans 74 to 105 milliseconds run to run for both binaries, the order the binaries are run in moves the answer more than the change does, and `perf stat` over the same work has the branch executing 15.591 billion instructions against 15.848 billion for the commit before it. Fewer instructions and a slower wall clock is the box talking. It is worth writing down because a minimum over repeated runs usually is a good enough filter and here it was not: taking the minimum of a wide distribution and the minimum of a narrow one and subtracting them invents a difference.

The load is unchanged: 0.60 seconds and about 600 MB either way, which is what folding a zone per chunk into a running total costs, which is nothing measurable. It is still the 0.44 seconds and 336 MB of section 13's regression away from where it should be, and that is still waiting on the streaming insert.

### What is left

The chunk size, which is section 9's third item and now the biggest one. The morsel change has taken most of the pipeline call overhead the 8192 chunk was going to win, so the number will be smaller than section 4's 1.9 times, but the rest of that number was the vectorised loops themselves and those are untouched. Then the streaming insert for the load, and then the page pool, so that the 983 KB pages a seal allocates come from somewhere other than the allocator every time.

## 15. What the vector size measured

Written on 21 September 2026, the day after section 14, and it closes #480. Section 9 named the chunk size as the third of four things to change and section 14's last paragraph said it was the biggest number left. This is the sweep.

Five binaries were built from the same commit with `VECTOR_SIZE` set to 1024, 2048, 4096, 8192 and 32768, and each was run against the twenty million row table of section 1 and against ClickBench over `hits_0.parquet`. Every scan number below is the minimum of four runs of the query in one process, which is what `~/jb/scan.sh` has reported since section 1, and every ClickBench number is the best of three whole processes. Milliseconds.

### The table in memory, one thread

| query | 1024 | 2048 | 4096 | 8192 | 32768 | DuckDB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 14.0 | 7.3 | 4.5 | 1.9 | 0.9 | 18.0 |
| `sum(v) WHERE v >= 10` | 39.6 | 32.5 | 30.4 | 29.6 | 25.5 | 37.0 |
| `sum(k + v)` | 66.8 | 58.7 | 54.6 | 52.6 | 46.3 | 60.0 |
| `count(*) WHERE k = 7` | 0.3 | 0.3 | 0.3 | 0.2 | 0.8 | 3.0 |

### The same, six threads

| query | 1024 | 2048 | 4096 | 8192 | 32768 | DuckDB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `count(*) WHERE v >= 10` | 6.7 | 4.5 | 1.8 | 2.0 | 1.2 | 9.0 |
| `sum(v) WHERE v >= 10` | 11.8 | 8.6 | 7.2 | 6.8 | 6.5 | 20.0 |
| `sum(k + v)` | 17.2 | 16.1 | 13.3 | 11.7 | 11.6 | 21.0 |
| `count(*) WHERE k = 7` | 0.3 | 0.3 | 0.6 | 0.6 | 0.5 | 3.0 |

The shape is one curve, not four. Everything that reads every row gets faster monotonically and the gain is nearly all spent by 8192, and the query with the most time in it per row gains the least, which is what it should do if what is being removed is a fixed cost per call. `count(*)` with a filter is the extreme of that: at 1024 it is almost entirely pipeline overhead and it loses fifteen sixteenths of itself by the time the vector is 32768.

### The needle, and why it is the reason 32768 is not the answer

`count(*) WHERE k = 7` is the query that is answered by zone maps rather than by reading, and it is the only row in either table that gets worse. At 32768 a chunk zone covers thirty two thousand rows of a column whose values are `(i * 104729) % 20000000`, which is to say scattered, so a wider zone is a wider bracket and fewer chunks can be ruled out. It goes from 0.2 milliseconds to 0.8. That is four times on a number small enough not to matter here, and it is a real property that would matter on a table where the needle query is the workload.

The same effect is visible from the other side on `count(*) WHERE k = 12345678`, the needle that matches nothing and prunes nothing, which is a full scan wearing a filter: 72.7, 65.7, 60.0, 55.2 and 47.6 milliseconds at one thread against DuckDB's 51.0. It improves all the way to 32768 because it never prunes, so there is no zone map precision to lose.

### The load, and the string case

Loading the table takes 0.59, 0.53, 0.55, 0.52 and 0.64 seconds, with peak RSS flat at about 590 MB. 32768 is the slowest of the five and it is the one that allocates a quarter megabyte at a time to hold a chunk that will be copied into a page and freed.

That is also the argument that does not show up in any of these numbers, because none of these queries has a string in it. A vector of 16 byte string views at 8192 is 128 KB and at 32768 it is half a megabyte, and an operator holding several of those at once is out of L2 on the reporting target. `spec/engine/03-data-plane.md` section 3.7 expected the cache to decide this and it did not, but it decides the top of the range rather than the whole of it.

### ClickBench

Twenty nine of the forty three queries run today. Over those, the total is 1.649, 1.565, 1.500, 1.532 and 1.489 seconds and the geometric mean per query is 33.3, 31.8, 31.1, 31.3 and 31.4 milliseconds. Every answer was hashed and every hash agrees across the five sizes, except q18, which is a `GROUP BY` with a `LIMIT` and no `ORDER BY`, so its ten rows are not determined and a rerun had all five agreeing.

Eight percent between the worst and the best, and nothing to choose between 4096, 8192 and 32768. That is the expected result and it is worth stating plainly: on Parquet the time is Snappy, the page decoders and the hash aggregation, and next to those the cost of entering and leaving the pipeline is small however often it is paid. The in-memory numbers are where the vector size shows, because there is nothing else in them, and eight percent on the queries people quote is the honest version of what this change is worth outside the microbenchmark.

### The answer

8192. It is best or within noise of best on every full scan at both thread counts, it keeps the pruning that 32768 gives up, it is the fastest of the five on the load, and it leaves a string view vector at 128 KB rather than half a megabyte. It is eight FastLanes units, so nothing in the encoding layer has to regroup, and the reason the constant was 1024 in the first place survives as the rule that the size is a multiple of 1024 rather than as the size itself.

It also unlocks a piece of the DuckDB corpus that was out of reach. 141 files in upstream's test tree carry `require vector_size 2048`, which upstream reads as a floor, and every one of them was skipped at 1024 and is eligible at 8192. The 5 that say `require exact_vector_size 2048`, the 1 that says 512 and the 1 that says 2 stay skipped, which is correct, because they are testing a boundary at a size we do not use. Collecting that is a change in `rudb-compat` rather than here: it keeps its own copy of the constant, which it had to because the `rudb` facade did not export one, and this PR exports `rudb::VECTOR_SIZE` so the harness can read it instead of repeating it.

### What is left

Section 9's fourth item, the per-chunk cost of the metrics shim, is now mostly amortised by the same change that made it worth measuring, which was the point of putting it last. What is left of the list is not on the list: the streaming insert, so that a `CREATE TABLE AS` stops holding the result and the table at the same time, and the page pool, so that the 983 KB pages a seal allocates come from somewhere other than the allocator every time. Both are about the load rather than the scan, and the load is the row of every table in this document that has not moved.
