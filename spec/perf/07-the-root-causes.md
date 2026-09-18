# What the profiler says is actually wrong

The last three notes were written from the benchmark harness, which reports per operator time and nothing below it. This one is written from callgrind and from a thread sweep, which is the first look under the operator. The conclusion is that the ordering in `03-roadmap.md` is spent and the next one is different.

Everything here is `gamingpc-wsl`, 32 hardware threads, ClickBench at ten million rows unless the section says otherwise. The suite run had another tenant on the machine so the wall clock totals are inflated for both engines, but CPU seconds are CPU seconds and the instruction counts come from a simulator, so the two numbers this note rests on are not affected.

## The one number that reframes it

Over the 41 shared queries, rudb answered in 9.605s of query time for 88.880s of CPU. DuckDB answered in 10.207s for 43.330s of CPU.

So rudb is level on wall clock and burns 2.05 times the CPU to get there. Put the other way, rudb is spending 9.25 effective cores where DuckDB is spending 4.25 and finishing at the same time.

That kills the argument in `03-roadmap.md`. That note said parallelism was worth an order of magnitude on everything at once and everything else was worth two to eight times on a fraction, so parallelism went first. Parallelism went first, it landed, and it bought exactly what it was supposed to buy. We are now using more than twice the cores DuckDB uses and we are not ahead. There is no second order of magnitude sitting in the scheduler.

The remaining gap is CPU per row, and to be ten times faster from here the CPU line has to come down by about a factor of ten. Nothing about that is achievable by scheduling.

## Where the CPU goes, under the operator

Callgrind, one million rows, one thread, so that the numbers are about the code and not about contention.

**q34, group by URL.** 2.71 billion instructions.

| what | share |
| --- | --- |
| `memcpy` | 23.1% |
| `snappy::decompress_into` | 19.5% |
| `core::str::converts::from_utf8` | 12.1% |
| `StringColumn::push_from` | 5.1% |
| `table::hash` | 4.4% |
| `Table::probe` | 3.5% |
| `Vector::copied` | 2.8% |

**q40, a date range and a group by.** 136 million instructions.

| what | share |
| --- | --- |
| `snappy::decompress_into` | 46.5% |
| `core::str::converts::from_utf8` | 17.8% |
| `memcpy` | 7.5% |

**q16, group by UserID, which is the one query with no strings in it.** 836 million instructions.

| what | share |
| --- | --- |
| `Table::insert` | 15.0% |
| `Vector::copied` | 7.8% |
| `Aggregate::fold` | 7.6% |
| `Table::probe` | 7.5% |
| `Vector::signed_at` | 6.3% |
| `Aggregate::finish` | 6.3% |
| `Aggregate::sink` | 5.3% |
| `memset` | 5.2% |
| `push_value` | 5.1% |
| `memcpy` | 4.8% |

Two of those three queries spend most of their instructions turning bytes in a file into bytes in memory, and the third spends 836 instructions per row to count integers into groups.

## Root cause one, we decode the whole file for a query that wants a hundredth of it

`crates/rudb-parquet/src/metadata.rs` parses `min_value` and `max_value` into a `Stats` for every column chunk. Nothing reads them. The only field of `Stats` any caller touches is `nulls`, in `chunk.rs`, for a count shortcut. `reader.rs` says so in its own module comment: row group statistics are in the footer and are not consulted.

That is five queries in the suite. q39 through q43 filter on `EventDate` over a file that is written in `EventDate` order, which is the exact case row group pruning was invented for.

| query | duckdb time | rudb time | duckdb peak | rudb peak |
| --- | --- | --- | --- | --- |
| q39 | 30.4ms | 177.9ms | 60.5 MiB | 789.0 MiB |
| q40 | 43.3ms | 281.0ms | 102.6 MiB | 1.11 GiB |
| q41 | 30.8ms | 40.2ms | 57.1 MiB | 206.0 MiB |
| q42 | 118.2ms | 34.7ms | 56.6 MiB | 168.9 MiB |
| q43 | 118.6ms | 31.4ms | 53.6 MiB | 136.6 MiB |

Six times the time and eleven times the memory on q40, and the whole of it is that DuckDB never opens the pages and we open all of them. This is the one place where the ten times faster and the ten times less resource are the same change.

It is also the cheapest thing on the list to build. The bounds are already parsed, `FilterPushdown` already lands the predicate directly above the scan, and the only missing piece is a conjunct on the scan node and a comparison against the bounds before a row group is handed out as a morsel. Pruning a row group also prunes its snappy, its UTF-8 pass and its memcpy, so it takes a slice out of all three of the top lines at once.

## Root cause two, we validate every string as UTF-8 on every scan

`StringColumn::push_in_place` calls `std::str::from_utf8` on each string as it builds the view. The Parquet plain byte array path calls it once per value. That is 12.1 percent of q34 and 17.8 percent of q40, and it is a second full pass over every byte of every string column in the query, every time the query runs.

The arena is one contiguous buffer. Validating the page's byte range once gives exactly the same guarantee for one call rather than a million, and one call over a megabyte vectorizes where a million calls over twenty bytes each cannot. Nothing about the safety argument changes.

Worth something like eight of the 88.9 CPU seconds on its own, and it is the smallest change in this note.

Worth noting that `table::fold` already worked this out for its own path. Its comment says the input reader already validated the column and validating the same bytes again for every row was most of the string group path. The same reasoning applies one layer down and was not applied there.

## Root cause three, the compact forms cost memory to build and buy nothing

This is the data model answer, and it is not the answer I expected to write.

The layout is right. `Form` already carries `Dictionary`, `BitPacked`, `StringView`, `Fsst` and `Rle`. `StringColumn` is already the sixteen byte view with a four byte prefix over a shared arena, which is DuckDB's `string_t`. The Parquet reader already emits `Vector::dictionary_over` for a dictionary page. None of that is wrong and none of it needs rebuilding.

What is wrong is the contract above it. A kernel is allowed to ask any vector for `value_at(row)` and every form has to answer, so every kernel has a correct slow path and the compact forms are optional decoration that the fast paths do not have to know about. Look at `table::fold`: there is a typed run for every fixed width `Data` and one for flat `Varlen`, and then a row at a time loop at the bottom for everything else. A dictionary encoded string column falls into that loop and rehashes the full string for every row.

q34 groups by URL. URL is dictionary encoded in the file, and it has far fewer distinct values than rows. Hashing the dictionary once and carrying the code per row turns an O(rows times length) hash into an O(distinct times length) hash plus an O(rows) gather. The same argument applies to the probe, to the group key comparison and to the `LIKE` in q21 through q23, where q23 alone spends 10.8 CPU seconds against DuckDB's 1.38.

That is the Abadi 2006 result and it is what F7 is for. The point of writing it here is that it is not a later optimisation on top of a working data model. It is the thing that makes the existing data model pay for itself. Today we spend memory and decode time building compact forms and then flatten them before doing any work.

The contract has to invert. The fast path per form becomes the obligation and the fall through becomes the thing that gets counted and reported, in the same way `xtask lint rowloop` makes a scalar loop something you have to justify in writing.

## Root cause four, a grouped aggregate has a hard parallel ceiling

The thread sweep, ten million rows, wall clock and CPU seconds at one through thirty two threads.

| query | 1t | 2t | 4t | 8t | 16t | 32t | speedup at 32 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| wide, four ungrouped aggregates | 0.22s | 0.12s | 0.06s | 0.04s | 0.03s | 0.02s | 11.0x |
| q24, select star and top k | 1.64s | 0.97s | 0.62s | 0.44s | 0.38s | 0.37s | 4.4x |
| q16, group by UserID | 0.83s | 0.51s | 0.34s | 0.27s | 0.26s | 0.25s | 3.3x |
| q34, group by URL | 2.72s | 1.75s | 1.30s | 0.96s | 0.88s | 0.90s | 3.0x |

An ungrouped aggregate scales 11 times on 32 threads. A grouped one scales three times and q34 is slower at thirty two threads than it is at sixteen.

The cause is the design `06-partitioned-aggregation.md` already named and chose anyway. Sixteen partitions behind sixteen mutexes, each held across a whole fold, on a machine with thirty two threads. The sweep in #525 turned a queue into a rotation and bought 1.07 times, which is what you can buy without changing the shape. What is left is the shape. That note listed the third design as fold partitioning into local tables, which is what DuckDB does and what nobody built, and it is exactly what the user's own five point description asks for: each worker builds local tables divided by radix bits, workers merge matching partitions independently, each group ends in one partition.

The CPU side is worse than the time side and matters more, because F4's second exit criterion is total CPU at eight threads within twenty percent of total CPU at one.

| query | cpu 1t | cpu 8t | cpu 32t | rss 1t | rss 32t |
| --- | --- | --- | --- | --- | --- |
| wide | 0.22s | 0.25s | 0.41s | 11.9 MiB | 92.9 MiB |
| q24 | 1.97s | 2.18s | 3.52s | 182.7 MiB | 780.2 MiB |
| q16 | 0.82s | 1.09s | 1.97s | 235.2 MiB | 324.2 MiB |
| q34 | 2.72s | 3.18s | 5.25s | 889.4 MiB | 1.44 GiB |

q16 is already 33 percent over at eight threads, so F4 exit criterion two fails today. Exit criterion one wants 6.5 times on eight cores and the best grouped query manages 3.7. F4 is not close to closed, and the checklist in #349 says so honestly: the morsel queue size, work stealing, the wait for graph, async I/O and `Exchange` are all still unticked.

Memory rising with thread count is its own problem against the ten times less resource half of the goal. q24 uses 4.3 times the memory at thirty two threads that it uses at one, for 4.4 times the speed, which is a straight trade rather than a win.

## Root cause five, nothing blocks, so nothing overlaps

`Progress::Blocked` is returned in exactly one place in the tree, `root.rs`, and only ever as `Blocked::Downstream`. No source returns `Blocked::Io`. No aggregate returns `Blocked::Memory`.

The consequences run further than they look:

`rudb-io` has a submission interface in `submit.rs` and the scan does not use it. `reader.rs` says a row group's worth of reads is meant to go through it as one batch and that wiring it is the next change. So today the open, the `read_exact_at`, the snappy decompress and the decode all happen on the worker thread that wanted the chunk, in order, inside `FileScan::read`, under the morsel's mutex. A thread that is waiting on a read is not available to compute, and a thread that is computing is not issuing the next read. That is the whole of the read to compute to execute overlap that DuckDB has and we do not.

The `Blocked` document in `rudb-metrics` has `io_ns`, `memory_ns`, `dependency_ns` and `downstream_ns` and every one of them is zero on every query. The engine cannot tell us where it waits because it never admits to waiting. That is why this note had to be written from callgrind and a shell loop rather than from our own instrument, and fixing it is worth doing for its own sake.

`Query::run` takes pipelines strictly one at a time, so a build pipeline and the probe pipeline that depends on nothing it produces still serialise.

`Pipeline::degree` is capped by `Source::morsels`, which is row groups times files. The ten million row file has 81 row groups, so that cap is not binding at ten million, but it is binding at one million where there are nine.

## What this changes about the F series

The milestone numbers still do not change. The order does, for the second time, and this time the trigger is a measurement rather than an argument.

`03-roadmap.md` put parallelism first because it was the only item worth an order of magnitude on everything at once. That was true and it has been collected. We now spend 9.25 effective cores against DuckDB's 4.25 for the same wall clock, so there is no second order of magnitude in the scheduler and the next one has to come out of CPU per row.

**First, do not read what the query does not want.** Row group pruning from the statistics we already parse, which is an F2 item sitting inside the Parquet reader rather than inside the storage format, so it does not wait for F2. Then page level skipping behind it. This is the only item that moves time and memory by the same factor, it is worth six times on five queries, and it is a week rather than a milestone.

**Second, stop paying twice for bytes we already have.** One UTF-8 validation per page instead of one per string. Roughly eight CPU seconds of 88.9 for a day of work, and there is nothing to design.

**Third, invert the vector contract.** F7 moves ahead of the rest of F4. Encoded execution is not a polish pass on a finished data model, it is the thing that makes the data model we already built worth having, and until it lands every compact form in `rudb-vector` is memory we spend to build something we then throw away. The instrument comes first: count every fall through to the row at a time path, per form and per kernel, and report it the way the row loop lint reports a scalar loop. Then write the dictionary paths for hash, probe, compare and `LIKE`, in that order, because that is the order the CPU table puts them in.

**Fourth, thread local aggregate partitions.** This is F5 and it is the third design in `06-partitioned-aggregation.md`, the one that was named and not built. It takes grouped aggregation off a three times ceiling and it is the only way F4's second exit criterion gets met, because the CPU inflation from one thread to thirty two is lock contention and cache line sharing on those sixteen mutexes.

**Fifth, make blocking real.** A source that returns `Blocked::Io` with a token, reads issued through `rudb-io`'s submission interface, and a scheduler that runs something else. This is lower than it feels because the machine has warm page cache and local NVMe, so the win is the overlap rather than the I/O, and DuckDB measures 1.5 times cold on local files from it. But it is the item that makes the `Blocked` document non zero, and an engine that cannot measure its own waiting is going to keep needing callgrind for questions it should be able to answer itself.

The storage format keeps its place after these. It is still the largest single CPU line and still the only route to the memory target on q34 and q35, and it is still the slowest thing here to build. Pruning gets a slice of it now without waiting.

## The rule this leaves

Per operator timing tells you which operator to look at and stops there. Every conclusion in this note came from a level below that, and three of the five would have been invisible with the instrument we had: a UTF-8 pass hiding inside a scan, a dictionary falling through to a scalar loop inside a hash, and a grouped aggregate that stops scaling at eight threads while the suite total looks fine because the ungrouped ones scale at eleven.

So the next instrument goes in before the next optimisation, and the first thing it has to report is what we fell back to and what we waited on.

## What the first item turned out to be worth

Row group pruning is in, #531, and it beat the estimate in this note. The estimate was six times on five queries out of a footer we already parse. What it did at ten million rows on gpc over 32 threads, median of three passes, against DuckDB on the same view and the same file.

| query | before | after | duckdb | CPU before | CPU after | RSS before | RSS after | duckdb RSS |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| q39 | 0.16s | 0.02s | 0.05s | 2.75s | 0.04s | 768 MiB | 48.5 MiB | 92 MiB |
| q40 | 0.27s | 0.09s | 0.07s | 5.73s | 0.18s | 1.13 GiB | 81.8 MiB | 145 MiB |
| q41 | 0.04s | 0.01s | 0.05s | 0.60s | 0.02s | 202 MiB | 22.6 MiB | 65 MiB |
| q42 | 0.03s | 0.01s | 0.05s | 0.56s | 0.02s | 161 MiB | 21.6 MiB | 57.5 MiB |
| q43 | 0.03s | 0.01s | 0.04s | 0.42s | 0.02s | 136 MiB | 17.6 MiB | 59 MiB |

Four of the five are now faster than DuckDB and all five hold between a half and a third of its memory, which is the first place in this benchmark where both targets are met at once. The CPU column is the one that matters for the headline number in this note: q39 gave back 2.71 seconds of CPU and q40 gave back 5.55. Two queries with no filter to prune on were run as controls and neither moved.

Three things the build taught that the note did not predict.

The saving is bigger than the estimate because pruning removes the work of every stage at once. The estimate was reasoning about the read, but a row group that is never handed out is also never decompressed, never decoded, never hashed and never grouped, and the memory is the aggregate's rather than the reader's.

Nothing prunes on EventDate, because the ClickBench view wraps it in `make_date` and a comparison against a function of a column is not a comparison against a column. All of the above comes from `CounterID = 62` and the hash equalities alone. Recognising a monotone function would get the date columns as well and is worth doing.

A timestamp constant makes no test at all, because a rudb timestamp is microseconds and a file may store the same column in milliseconds or nanoseconds with the statistics at the file's own unit. That is the shape of a whole class of pruning bug and it is worth remembering when the storage format writes its own statistics: the unit has to travel with the bound.

## What the fourth item turned out to be, which was not what this note said

The note said the grouped aggregate's ceiling was lock contention on the sixteen mutexes and that thread local partitions were the way off it. Half of that was wrong, and finding out which half took one probe build and an afternoon.

The probe timed four things separately: the per partition gather, the fold under the lock, the wait for a lock the first sweep found held, and the whole of `finalize`. Ten million rows on gpc, one instance line per thread, summed across instances.

| query | threads | gather | fold | wait | contended | finalize | query wall |
| --- | --- | --- | --- | --- | --- | --- | --- |
| q34 | 1 | 0s | 0s | 0s | 0% | 0.54s | 2.67s |
| q34 | 8 | 0.33s | 0.92s | 0.44s | 15.6% | 0.44s | 0.94s |
| q34 | 32 | 0.43s | 1.55s | 4.85s | 40.7% | 0.47s | 0.93s |
| q16 | 32 | 0.09s | 0.96s | 2.57s | 41.4% | 0.07s | 0.25s |
| q17 | 32 | 0.31s | 1.46s | 3.82s | 43.0% | 0.32s | 0.60s |

The lock wait is real: at thirty two threads q34 waits three times longer than it folds. But the line nobody had looked at is the last one. `finalize` was one thread walking sixteen partitions and building the whole answer, it was half of q34 and half of q17, and it did not get shorter when the threads went from one to thirty two because nothing about it was parallel. That is a plain Amdahl floor and it is why q34 ran the same at sixteen threads as at thirty two.

Each partition holds every row of every group that hashes to it, so finishing one says nothing about any other. #532 gives them to as many threads as the pipeline ran instances on. It is thirty lines and it is worth this, best of three at ten million rows:

| query | before | after | duckdb | rudb peak | duckdb peak |
| --- | --- | --- | --- | --- | --- |
| q13 | 0.25s | 0.14s | 0.10s | 249 MiB | 370 MiB |
| q16 | 0.25s | 0.19s | 0.12s | 324 MiB | 444 MiB |
| q17 | 0.62s | 0.32s | 0.24s | 683 MiB | 858 MiB |
| q34 | 0.92s | 0.50s | 0.32s | 1533 MiB | 2304 MiB |

`finalize` itself went from 0.47s to 0.057s on q34, from 0.32s to 0.041s on q17 and from 0.073s to 0.016s on q16.

The rule to take from this: an operator that reports one wall and one CPU number per instance cannot tell you that its single threaded tail is half the query, because the tail is charged to whichever instance happened to run it. Per operator timing was the instrument that found the aggregate and it was not the instrument that found this. Timing the phases inside the operator was.

## Two things that were tried for the lock wait and lost

Both were measured on top of #532, so the serial finalize was no longer masking anything.

**More partitions.** Sixteen, thirty two, sixty four and a hundred and twenty eight, at ten million rows and thirty two threads. Nothing moved and q34 got worse.

| query | 16 | 32 | 64 | 128 |
| --- | --- | --- | --- | --- |
| q13 | 0.14s / 245 MiB | 0.13s / 251 MiB | 0.13s / 256 MiB | 0.14s / 260 MiB |
| q16 | 0.19s / 330 MiB | 0.16s / 313 MiB | 0.17s / 357 MiB | 0.17s / 357 MiB |
| q17 | 0.33s / 669 MiB | 0.30s / 660 MiB | 0.30s / 675 MiB | 0.31s / 653 MiB |
| q34 | 0.51s / 1471 MiB | 0.55s / 1692 MiB | 0.54s / 1674 MiB | 0.56s / 1672 MiB |

That result says what the waiting actually is. If it were queueing for a busy lock then four times the locks would quarter the queue. It is not: it is the cost of going to sleep and being woken again, and with more partitions the collisions get shorter and more frequent and the sleeping stays.

**Recycling the local table.** An instance folds into a table of its own until it holds `PARTITION_FROM` groups, and today it then hands that table to the partitions and folds straight into them for the rest of the query. The idea was to give it a fresh table instead, so the partition locks are taken once per full table rather than once per chunk. At four thousand groups that is the same number of locks; the point was to then raise the threshold.

| query | main | 4k | 16k | 64k | 256k |
| --- | --- | --- | --- | --- | --- |
| q13 | 0.13s / 246 MiB | 0.11s / 258 MiB | 0.11s / 285 MiB | 0.34s / 325 MiB | 0.35s / 344 MiB |
| q16 | 0.19s / 330 MiB | 0.21s / 327 MiB | 0.21s / 345 MiB | 0.20s / 358 MiB | 0.76s / 566 MiB |
| q17 | 0.33s / 694 MiB | 0.36s / 724 MiB | 0.35s / 709 MiB | 0.36s / 745 MiB | 0.98s / 950 MiB |
| q34 | 0.49s / 1505 MiB | 0.54s / 1572 MiB | 0.54s / 1608 MiB | 0.61s / 1924 MiB | 1.59s / 2212 MiB |

A loss everywhere but q13 and a rout at the large thresholds. The reason is that handing a table over is not free the way handing a chunk over is. Scattering rebuilds the key vectors out of the table and probes every group into the shared partition, so a group that appears in ten recycled tables is probed ten times and materialised ten times. Spreading a chunk probes each row once. On q34, where the groups are nearly the rows, recycling buys one extra probe per group and pays for it in full.

## What the lock wait is actually worth, which is less than it looks

Worth writing down before anybody spends a milestone on it. At thirty two threads q34 folds for 1.60 seconds across its instances. Those folds happen inside sixteen mutually exclusive locks, so the fold alone cannot finish in less than 1.60 / 16, which is 0.10 seconds of wall however the waiting is arranged. Today wait plus fold is 4.77 + 1.60 over thirty two threads, which is 0.199 seconds per thread. So perfect lock handling is worth about 0.10 seconds of a 0.51 second query, and getting below that floor means not sharing the tables at all.

Not sharing them is the thread local design, and the price is the one #520 already measured: every instance holds its own copy of every group it saw. On q34 thirty two instances hold about eight million entries between them against three and a half million shared, and most of q34's 1.5 GiB is that table. It would land near or above DuckDB's 2.3 GiB, which trades the memory target for a fifth of a second. That is not a trade this project should take, and the note's fourth item should be read as closed rather than pending until somebody has a way to bound the duplication.

## The fifth item, which was one operator having two implementations

The full forty three query sweep that followed #532 put the three `COUNT(DISTINCT)` queries at the bottom of the table, and q6 at the very bottom: 0.39 seconds against DuckDB's 0.09, the worst ratio in the suite at 0.23.

The cause was not a hot loop. It was that the aggregate operator has two implementations of grouping in it and only one of them has been worked on. `group.rs` says so in a comment written when #237 landed: `DISTINCT` inside an aggregate is a grouping of its own, one set per group per call, and the set is keyed the way grouping was keyed before #237. Giving it the table in `table.rs` was deliberately deferred. So every improvement since, the vector at a time probe, the sixteen radix partitions, the parallel merge, and the parallel finalise that #532 had just added, went to the path `COUNT(DISTINCT)` does not take.

The fix in #533 is not to make the second implementation faster. It is to stop having one. An aggregate whose calls are all `DISTINCT` over the same arguments is rewritten into a grouping on the group keys plus those arguments, with a plain aggregate over it, which is what DuckDB's `distinct_aggregate_rewrite` does and which is why the name was already in the list of upstream optimizers rudb accepts and does nothing about.

| case | argument | before | after | DuckDB |
| --- | --- | --- | --- | --- |
| alone | SearchPhrase | 0.37 | 0.15 | 0.10 |
| alone | UserID | 0.18 | 0.16 | 0.11 |
| grouped | URL | 2.96 | 0.59 | 0.32 |
| grouped | SearchPhrase | 0.55 | 0.15 | 0.12 |
| grouped | ResolutionWidth | 0.08 | 0.08 | 0.06 |
| grouped | UserID | 0.19 | 0.20 | 0.11 |

The sweep is the interesting part, because it says the rewrite is worth exactly what the set it replaces cost. The general set keys on an encoded row and the rewrite beats it by two to five times. The specialised set for a single `BIGINT` is a hash and a compare of one word, there is nothing to win back, and the wider grouping key the rewrite leaves behind makes it slightly worse. The exception is an aggregate with no group key, where the old path keeps one set for the whole query, never partitions it and merges it by walking it, and the rewrite wins even there. So the pass takes everything except a grouped distinct over one `BIGINT`.

q6 went from 4.1 times slower than DuckDB to 1.6 and from 295 MiB to 213. q5 went 0.19 to 0.16. Nothing else in the suite moved outside noise.

The general lesson is worth more than the query. A second implementation of something the engine already does well does not announce itself in a profile, because it is not slow in any one place, it is merely absent from every improvement. The instrument that would have found it is the one the fourth item asked for and nobody has built: a count, per operator, of what fell through to the row at a time path. It would have said that q5, q6, q9, q10, q11, q12, q14 and q23 all fell through and that nothing else did.

## What the suite now says, which is one word: keys

With the distinct cluster dealt with, the ClickBench table has one pattern left in it and it is easy to state. Every query that groups by one column beats DuckDB or is close to it. Every query that groups by more than one is behind.

| query | group key | rudb | DuckDB | ratio |
| --- | --- | --- | --- | --- |
| q33 | WatchID, ClientIP | 0.52 | 0.24 | 0.46 |
| q36 | ClientIP and three expressions of it | 0.26 | 0.09 | 0.35 |
| q19 | UserID, minute, SearchPhrase | 0.48 | 0.25 | 0.52 |
| q18 | UserID, SearchPhrase | 0.40 | 0.19 | 0.47 |
| q12 | MobilePhone, MobilePhoneModel | 0.03 | 0.07 | 2.33 |

q12 is the one that does not fit, and it does not fit because its filter throws away almost every row before the key is built, which is the same thing as saying the key cost is what the others are paying. The grouped `BIGINT` distinct case that #533 leaves alone lands here too: after the rewrite it is a two column key, and the reason the rewrite does not pay there is that the two column key costs what the specialised set saved.

q33 is also the only query in the whole suite where rudb uses more memory than DuckDB, 1630 MiB against 1097, and it is the widest key in the suite. That is not a coincidence worth explaining away.

So the next root cause is the key encoding, which is the first open item on F5 and which that milestone already describes as one encoding shared by the hash operators and by sort. #491 tried a narrow key case chosen at plan time and was reverted in #492 because flattening one or two columns into a run of words cost more than the per row type match it removed. That result is about the one and two column case where the columns are already stored a column at a time and read where they lie. It says nothing about what a key made of three columns and a varchar costs, and the queries above are all in that range. The measurement to take first is a count of bytes hashed and compared per row against the minimum the key actually needs, per query, which is the instrument version of the same question.

## What the phase timers said, which is that the probe is waiting and not working

Before touching the key encoding I put timers inside the fold on a throwaway branch, one around the hash, one around the probe loop, one around each insert and one around the aggregate fold, thread local counters flushed into globals at merge time so the numbers are CPU nanoseconds summed across all thirty two threads. Ten million rows.

| case | rows | inserts | probe_ms | insert_ms | fold_ms | probe_ns_row | insert_ns_ins |
| --- | --- | --- | --- | --- | --- | --- | --- |
| int-1 | 9999750 | 3572796 | 719 | 241 | 53 | 71.94 | 67.68 |
| int-2 | 9999750 | 3573382 | 1033 | 314 | 61 | 103.34 | 87.98 |
| int-4 | 9999750 | 3572192 | 1372 | 437 | 59 | 137.28 | 122.51 |
| str-1 | 9999750 | 898245 | 193 | 87 | 21 | 19.30 | 97.28 |
| str-2 | 9999750 | 898037 | 271 | 125 | 21 | 27.18 | 139.78 |
| wide-2 | 9999750 | 9999750 | 1597 | 949 | 12 | 159.78 | 94.91 |
| wide-1 | 9999750 | 9999750 | 1586 | 708 | 11 | 158.66 | 70.84 |

Two things fall out of it. The probe is four to thirteen times the fold it feeds, so the aggregate is not where an aggregate spends its time. And seventy two nanoseconds a row for a single `INTEGER` key is far too much for the instructions involved, which are a mask, a load, a compare and a compare. It is three dependent cache misses: the bucket, then the stored hash of the slot the bucket held, then the key beside that slot, each address unknown until the load before it lands.

That is a different fix from the key encoding even though both show up as key cost. The encoding makes each row's compare cheaper. The dependency makes the waiting overlap. #535 did the second one, because it is the smaller change and because it does not have to answer #491.

The mechanism is the one the hash already uses. Rows are independent of each other, so probe sixty four of them together and do one kind of load at a time across the whole batch: every bucket, then every stored hash, then every key. Inside a pass every address is known before the pass starts. There are no prefetch intrinsics available here and there do not need to be, because a pass of sixty four independent loads is a prefetch that the compiler cannot get wrong.

The part that needed care is the vacancies. A row whose walk reaches an empty bucket cannot be inserted inside the batch, because an insert moves the table under the rest of the batch and because two rows in one batch can be the first two rows of one group. So they come back on a list, in row order, and the caller finishes them with the probe and insert it already had. The second probe those rows pay for is cheap, because the buckets it walks are the ones the batch just read, and it is only paid on rows that were about to pay for an insert anyway.

Measured on generated data with no parquet reader in the number, best of three after a warm up:

| case | groups | before | after |
| --- | --- | --- | --- |
| few | 100 | 0.10 | 0.09 |
| mid | 100 thousand | 0.36 | 0.37 |
| big | 4 million | 1.23 | 0.91 |
| big2 | 4 million over two key columns | 2.92 | 2.28 |
| wide | 10 million, every row a new group | 0.75 | 0.54 |

Twenty two to twenty eight percent off every case where the table is larger than the cache and nothing lost where it is not, which is why a table of eight thousand buckets or fewer keeps the row at a time walk. A table that small has no misses to overlap and the batch would be pure overhead on exactly the queries that are already fast.

The general lesson here is the opposite of the one the distinct finding gave. That one was a second implementation hiding in plain sight and no amount of tuning the first one would have found it. This one was the first implementation being right about everything except the order it issued its loads in, and the only way to see it was to time the phases separately. A profile that says "group by is sixty percent of the query" is true and useless. A profile that says "the probe is seventy two nanoseconds a row and the fold is five" tells you what to change.

The key encoding is still the next thing, and the sweep above still says we are linear in key column count where DuckDB is flat. The batch does not change that slope, it lowers the whole line.

### The other half, which is what the batch does once the loads land

The batch fixed the waiting and left the working alone, and the working turned out to be worth as much again on the shapes the batch did not help.

The comparison after a probe was a row at a time inside the batch. It matched on the stored column's type, asked the vector for one row through `signed_at` or `bytes_at` which matched on the vector's form, and widened both sides to `i128` so that every integer width had one answer. Three things, none of which depend on the row, all paid once per row per key column. That is the same mistake #237 fixed in the hash and it survived in the compare because nobody had a reason to look at it until the batch put a run of rows in scope.

So the compare goes a column at a time too. Both matches happen once for the batch, the arm underneath walks two runs of the same width where they lie, and each key column narrows the set of rows still in the running rather than replacing it, so a four column key whose rows differ in the first column never reads the other three. Anything without an arm, which is every form that is not a flat integer or a flat string, falls through to the old row at a time path unchanged.

Ten million ClickBench rows, thirty two threads, idle machine, best of three after a warm up. `base` is main before the batch, `batch` is the batch alone, `cols` is with the column compare.

| case | base_s | batch_s | cols_s | duck_s |
|---|---|---|---|---|
| int-1 | 0.15 | 0.14 | 0.13 | 0.10 |
| int-2 | 0.19 | 0.16 | 0.16 | 0.10 |
| int-3 | 0.22 | 0.19 | 0.19 | 0.10 |
| int-4 | 0.26 | 0.22 | 0.21 | 0.10 |
| str-1 | 0.16 | 0.17 | 0.15 | 0.10 |
| str-2 | 0.25 | 0.28 | 0.24 | 0.11 |
| mix-2 | 0.34 | 0.31 | 0.31 | 0.24 |
| mix-1 | 0.19 | 0.18 | 0.18 | 0.11 |
| wide-2 | 0.32 | 0.28 | 0.29 | 0.17 |
| wide-1 | 0.28 | 0.24 | 0.24 | 0.13 |

The two string columns are the whole argument for doing both halves. The batch alone made them worse, because a string compare is a call into the vector whether or not the rows arrive in a run, so the batch added bookkeeping and removed nothing. Only the dispatch coming out of the loop pays for it, and then both go past where they started. If we had measured the batch on the string cases and stopped there we would have concluded the batch was a wash and reverted it, which is how #492 happened.

The slope is down from about eleven hundredths of a second per three extra integer key columns to about six, and DuckDB is still flat. What is left is not dispatch, it is that a key column is its own run with its own `Vec<bool>` of validity beside it, so k key columns is 2k places in memory per compare where a packed row is one. The next change is the layout and it has to be conditional on the key shape, because #492 is the receipt for what happens when it is not.
