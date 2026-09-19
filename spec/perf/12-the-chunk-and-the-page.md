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
