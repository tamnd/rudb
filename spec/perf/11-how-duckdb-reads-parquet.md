# How DuckDB reads Parquet

Note 10 was about the format we are going to write. This one is about the format we already share with the engine we are trying to beat by ten times. Both engines read Parquet, so pointing both at the same file and asking both the same SQL holds the storage format fixed and measures nothing but the reader, the decoder and what runs on top of the values. If we are slower there it is our code and not our format, and no amount of format work will fix it.

There was no instrument for this, so the first thing was building one. `cargo xtask parquet` landed in #567.

## What the instrument does

Seven shapes through both engines over the same file, each one a rung on a ladder so that the difference between two rows says what the stage between them cost.

| shape | SQL | what it adds |
|---|---|---|
| count | `SELECT count(*)` | the footer, and no column |
| int1 | `SELECT sum(UserID)` | one eight byte integer column |
| int10 | ten sums | nine more integer columns, of four widths |
| str1 | `SELECT sum(length(URL))` | one large string column instead of the integers |
| str4 | four sums | the four string columns the file is mostly made of |
| filter | `count(*) WHERE CounterID = 62` | a predicate, so the rows kept are a fraction of the rows read |
| load | `CREATE TABLE loaded AS SELECT *` | every column of every row, into a table |

Both engines are measured the same way, as a subprocess running a script of SQL, because linking one of them and shelling out to the other leaves an argument about which side the difference came from. rudb's shell takes the same flags DuckDB's does, so the two commands differ only in the binary at the front of them. Process start is measured separately against an empty script and subtracted, and the repeat count in the script is calibrated so the queries are at least four hundred milliseconds of the total, which puts what is left of the start time's spread below one percent.

## The numbers

Bench host, thirty two threads, rudb 0.3.12 against duckdb 1.5.x, nine samples a cell, process start subtracted. The ratio is rudb over duckdb, so above one is us losing.

Process start on its own is 480 microseconds for rudb and 22.2 milliseconds for duckdb. That is forty six times and it is the reason every small file row below reads the way it does.

| shape | 1k | 10k | 100k | 1M |
|---|---|---|---|---|
| count | 0.22x | 0.23x | 0.34x | 0.97x |
| int1 | 0.18x | 0.30x | 0.95x | 1.04x |
| int10 | 0.15x | 0.32x | 1.33x | 1.35x |
| str1 | 0.35x | 1.04x | 1.36x | 1.52x |
| str4 | 0.47x | 1.26x | 1.56x | 1.54x |
| filter | 0.17x | 0.22x | 0.50x | 0.73x |
| load | 0.29x | 0.48x | 0.59x | 3.29x |

The first thing to know before reading any of that: the one thousand, ten thousand and one hundred thousand files are a single row group each, and the one million file is nine. So the first three columns are two engines with nothing to parallelize over, and the last one is the first column where either engine gets to use more than a core.

So the table says three separate things.

**We win everything that is fixed cost.** Small files, cheap shapes, anything where the work is opening a file and reading a footer. At one thousand rows we are three to six times faster on every shape, and the widest margin is the one with a predicate. That is worth keeping and it is not an accident: a process that starts in half a millisecond instead of twenty two is a different tool for anything interactive.

**We lose per row decode, and the gap grows with the column's width.** By one hundred thousand rows, still one row group and so still one core doing the work, we are 1.33x on ten integer columns, 1.36x on one string column and 1.56x on four. Those ratios barely move at one million.

**The load shape falls off a cliff at one million, and only there.** 0.59x at one hundred thousand, 3.29x at one million. Nothing about the decode changed between those two rows. What changed is that the one million file has nine row groups.

## The same table with both engines pinned to one thread

This is the cut that separates the two causes.

| shape | 100k 32t | 100k 1t | 1M 32t | 1M 1t |
|---|---|---|---|---|
| count | 0.34x | 0.42x | 0.97x | 1.16x |
| int1 | 0.95x | 1.22x | 1.04x | 2.21x |
| int10 | 1.33x | 1.65x | 1.35x | 2.68x |
| str1 | 1.36x | 1.57x | 1.52x | 1.75x |
| str4 | 1.56x | 1.74x | 1.54x | 1.89x |
| filter | 0.50x | 0.61x | 0.73x | 0.82x |
| load | 0.59x | 0.98x | 3.29x | 1.10x |

Read the last row first. Single threaded, the full width load is 1.10x: we and duckdb cost about the same per core. With threads it is 3.29x. Working out what each engine got from the nine row groups it had: duckdb went from 1170 milliseconds to 218, so 5.4 times. We went from 1290 to 716, so 1.8 times. The entire load gap is parallelism and none of it is decode.

Now read the rest. Single threaded at one million we are 2.21x on one integer column and 2.68x on ten. With threads those become 1.04x and 1.35x, because we get more out of the nine row groups than duckdb does on this shape: we go 41.1 milliseconds to 8.6, which is 4.8 times, and duckdb goes 15.3 to 6.4, which is 2.4 times. Same on strings, 5.7 times for us against 4.7 for them.

So the aggregate path already scales better than theirs and is losing purely on what one core does with a page of bytes, and the table build path is at parity per core and losing purely on not using the other thirty one.

Two root causes, cleanly separated, and neither one is the other's fault.

## Root cause A, what one core does with a page

Between 1.7x and 2.7x, worst on integers. The rest of this note is mostly about this, because it is the one that reading duckdb's source explains.

## Root cause B, the table build does not spread out

Nine row groups buy the aggregate path about five times and buy the table build about 1.8. duckdb gets 5.4 on the same file with the same nine row groups, so the row groups are not the limit and the thread count is not the limit. Something in the path from a decoded vector to a stored table is serial. This one is not a Parquet question at all and it wants its own investigation, but it is the largest single number in the whole table and it belongs at the top of the work list.

## What DuckDB does that we do not

This is from reading `extension/parquet` at DuckDB main. Five things, in the order of how much they are worth on ClickBench.

### 1. A dictionary column is never materialized

`DictionaryDecoder::Read` decodes the dictionary page once into a `Vector`, then hands the result out as a dictionary vector: the dictionary plus a selection vector of offsets. The RLE stream of indices is decoded straight into the selection vector's own buffer. No string is ever copied and no value is ever expanded to one per row.

We already do half of this. `Vector::dictionary_over` exists and `values.rs` builds one, and the test named `a_dictionary_encoded_column_stays_a_dictionary` says so. What we do not do is the other four things on this list, and they are all about what happens next.

### 2. The filter is pushed into the dictionary, not applied per row

This is the big one and it is not an optimization, it is a different algorithm.

`DictionaryDecoder::InitializeDictionary` takes the table filter and evaluates it against the dictionary, once, producing a `bool` array with one entry per dictionary entry. Then `DictionaryDecoder::Filter` walks the rows and the whole per row cost is `filter_result[offset]`, an array lookup on an index it already had.

For `URL LIKE '%google%'` over a million rows whose page dictionary holds ten thousand distinct URLs, DuckDB runs the LIKE ten thousand times and we run it a million times. That is a hundred to one on the predicate, before anything vectorizes. It is the same shape as the aggregate hash table lesson from note 06: the win is in doing the work once per distinct value instead of once per row.

`PageIsFilteredOut` then closes the loop. If the filter matched nothing in the dictionary, `HasFilteredOutAllValues` is true and the entire page is skipped at the transport, so its bytes are never even decompressed.

### 3. Definition levels cost nothing when nothing is null

`ColumnReader::PrepareRead` asks `defined_decoder->HasRepeatedBatch(read_now, max_define)` first. A page of an optional column with nothing null in it is one RLE repeat run, so that question is answered by looking at the run header, and `GetRepeatedBatch` then just decrements the run counter. Zero bytes are written and zero bytes are read.

We do the opposite. `chunk.rs`'s `definitions` builds a `Vec<u32>` with one entry per row, so an optional column costs four bytes a row of allocation and four bytes a row of writes before any value is decoded. Then `values.rs`'s `presence` walks the whole vector to count the valid ones, and `spread` allocates a second vector of the full length and copies into it whenever anything is null. For a hundred and five columns at a million rows that is hundreds of megabytes of traffic to discover that nothing is null.

This is the cheapest thing on the list to fix and it is on the path of every column of every page.

### 4. Only the filter columns are read, and only if they match

`EvaluateFilters` reads the filter columns first, in an order the adaptive filter chooses from observed selectivity, and it stops as soon as `filter_count` reaches zero. `DecodeRemainingColumns` then reads the projection columns, and if `filter_count` is zero it calls `Skip` on all of them instead.

`ColumnReader::Skip` is lazy. It adds to `pending_skips` and does nothing, so consecutive skips coalesce and the skip is only paid when a read forces it, at which point whole pages can be jumped over at the transport rather than decoded and discarded.

The I/O follows the same shape. `ColumnWisePrefetch` with a selective filter registers only the filter columns' byte ranges, issues that read, and only registers the projection columns' ranges once it knows some row survived. On a selective ClickBench query that is the difference between reading four columns and reading four columns' worth of bytes out of a hundred and five columns' worth of file.

### 5. Bit unpacking is branch free over groups of thirty two

`ParquetDecodeUtils::BitUnpack` splits the batch into a multiple of thirty two and a remainder. The aligned part goes through `BitpackingPrimitives::UnPackBuffer`, which is a generated unrolled routine per width that the compiler vectorizes. Only the remainder goes through the scalar loop. The bounds check is hoisted: `src.available()` is called once for the whole batch and then everything inside is `unsafe_get` and `unsafe_inc`. `RleBpDecoder::NextCounts` does the same trick, picking an unchecked variant of the run header parser when there is provably enough buffer left.

Our `hybrid.rs` `unpack` is already better than the naive version, with a 128 bit accumulator that reads each byte once, and its module doc explains why. It is still one call to a `FnMut` and one branch per value. The gap to a generated width specialized unpacker is real but it is the smallest of the five, and it is the one we already knew about.

## Two things worth copying that are not about speed of code

`ShouldAndCanPrefetch` and the footer read. DuckDB guesses the footer size from the file size, clamped between sixteen kilobytes and two hundred and fifty six kilobytes, and reads that much in one go, so the footer parse almost never needs a second round trip. Page headers get the same treatment: two hundred and fifty six bytes assumed, read in one call, because letting Thrift read a byte at a time from storage is the pathology.

`enable_external_file_cache` is on by default and `parquet_metadata_cache` is off. So DuckDB caches the file's bytes in its own buffer manager across queries but reparses the footer each time. That is worth knowing before quoting any repeated query number, ours or theirs, and the instrument's caveats say so.

## What to do, in order

0. **Find out why the table build will not use more than two cores.** Worth 3.29x down to something near 1.0x on the widest shape there is, it is the biggest number in the table, and it is not Parquet work. Everything below is worth less than this. **Done, in #570, and it went further than the estimate.**
1. **Stop materializing definition levels when nothing is null.** A run header says whether the whole page is one repeat of the maximum level. If it is, there are no levels to build, no count to walk and no spread to do. This is local to `chunk.rs` and `values.rs`, it changes no answers, and it is on the path of every column of every page. **Done, in #569, and it does not show on these shapes.**
2. **Push the filter into the dictionary.** Evaluate the predicate over the dictionary entries once per page, keep a bit per entry, and filter rows by indexing that bit with the code we already decoded. This needs the scan to know its predicates, which is a plan level change rather than a reader level one, and it is the hundred to one item.
3. **Skip what the filter killed, without decoding it.** A lazy pending skip, a page that is jumped at the transport when every row in it is skipped, and projection columns that are not read at all for a row group where the filter matched nothing.
4. **Read the filter columns first and the rest only if they match.** Order the filter columns by observed selectivity, the same adaptive idea DuckDB uses, and only issue the I/O for the projection columns once a row survives.
5. **Generate the bit unpacker per width.** Smallest of the five and the one we already had on the list. It is also the one root cause A points hardest at, because integers are where we are furthest behind per core and integers are what the unpacker is for.

Items 1, 3 and 5 are reader work. Items 2 and 4 need the scan to receive filters, which is the same plumbing F2's block level statistics and Bloom filters want, so it is not throwaway work for the Parquet path.

## What items 0 and 1 actually did

Both arms in one session on a quiet machine, same duckdb binary, nine samples a cell, thirty two threads. Before is the commit ahead of #569 and after is #569 and #570 together.

| rows | load before | load after | faster by | ratio before | ratio after |
|---|---|---|---|---|---|
| 1k | 1.572ms | 1.223ms | 1.29x | 0.30x | 0.23x |
| 10k | 10.696ms | 8.394ms | 1.27x | 0.45x | 0.35x |
| 100k | 124.514ms | 83.856ms | 1.48x | 0.59x | 0.40x |
| 1M | 715.554ms | 206.243ms | 3.47x | 3.29x | 0.94x |

Item 0 was one `flatten` on the thread draining the query. A result set going to a caller outside the engine is flattened because a caller reads a value at a time and has never heard of a dictionary vector, and `CREATE TABLE AS SELECT` was going through the same path even though storage holds the same forms execution does. So the widest shape in the suite was doing one copy per row per dictionary column, on one thread, at the end of a query that was parallel up to that point. Removing it took the statement from getting 1.8 times out of thirty two threads to getting 6.3, and 3.29x behind duckdb became 0.94x ahead.

The rest of the shapes at a million rows after the change: count 0.95x, int1 1.05x, int10 1.32x, str1 1.55x, str4 1.53x, filter 0.75x. Those are within noise of the before arm, which is the honest reading of item 1. Answering a page of definition levels from its run header removes an allocation and two walks per page and it is on the path of every column of every page, and on an aggregate over one to ten columns at thirty two threads none of that was the bottleneck. It should show on a wide cold read and there is no instrument for that yet.

So the table at the top of this note now reads: we are ahead of duckdb on every shape at a thousand and ten thousand rows, ahead on `count`, `filter` and `load` at a million, and behind by 1.3x to 1.6x on the aggregate shapes, which is root cause A and which the four remaining items are all aimed at.

## What not to do

Do not chase the small file columns. We are already three to six times faster there and the reason is a fast process and a cheap footer, both of which we should protect rather than extend. The ten times has to come from the right hand side of the table, where the work is per row and per byte, and where today we are behind.
