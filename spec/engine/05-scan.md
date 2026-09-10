# Layer three: scan, filter and I/O

This is sub-milestones 2d and 2e, and it is the largest layer in this directory by a wide margin. It covers reading bytes off a disk, turning them into vectors, deciding which bytes not to read, and evaluating predicates as early and as compressed as possible. It is where the parent spec's headline claim about ClickBench is either won or lost, because on ClickBench most queries are a scan, a filter and an aggregate, and the scan is the part that touches fourteen gigabytes.

It is also the layer where the plan meets a fact that the baseline document did not account for, and section 5.2 is that fact.

## 5.1 What exists today

`rudb-io` is 1126 lines: a `File` trait with `read_at`, `read_exact_at`, `write_all_at` and `sync`, a `Filesystem` trait, a real implementation and a 652 line deterministic simulator that models unsynced writes being present, absent or reordered after a crash. The simulator is a genuine asset and section 5.10 uses it for more than crash testing.

`rudb-encoding` is 4587 lines and it is the M1 result: bit packing, frame of reference, delta, run length, FSST, dictionaries, the multi-column chooser, and the sketches that drive it. This is the code that got `hits` to 9.65 GB against Parquet's 13.76 and DuckDB's 20.46. It works, it is measured, and nothing in the query engine can reach it.

`rudb-storage` is 254 lines and all of it is `MemoryTable`, which is a `Vec<Chunk>` with an append and a read by chunk index.

`rudb-parquet` is 9 lines. It is a crate with a module doc and nothing in it.

## 5.2 The fact that reshapes the plan

**rudb cannot read a file.** Every table in the database is a `MemoryTable`, built by `INSERT` and `VALUES`, and there is no path from a file on disk into a chunk. The M1 encoder is a standalone laboratory that reads Parquet through its own harness and writes rudb blocks, and neither end of it is connected to the executor.

Document 02 said that turning the `rudb-bench` stub into a real engine was the first task of 2a. That is true and it is not sufficient. A real engine that can only be given data through `INSERT` cannot run ClickBench, because loading a hundred million rows through `INSERT` is not a load, and it cannot run TPC-H SF100 for the same reason.

So the baseline splits in two, and this is a correction to document 02 rather than an elaboration of it.

The 2a baseline measures DuckDB, ClickHouse, DataFusion and Polars on `hits` and TPC-H SF100 in full, and measures rudb only on the `smoke` suite and on TPC-H SF1 loaded through `INSERT`, with the rudb columns on the large suites recorded as unable to run rather than as a slow number. That is a worse first table than the one document 02 predicted, and it is the true one. The prediction in document 02 section 2.6 that rudb loses every query stands, with the correction that on the two large suites it does not lose, it abstains.

The consequence for ordering is that this layer is what makes rudb measurable at all on the benchmarks the project is aimed at, and every layer above it is being specified against numbers that cannot be produced until it exists. That is uncomfortable and it is the correct reading. Layers one and two are still first, because they are cheap, they are prerequisites, and their microbenchmarks are real measurements even without a file scan. But the first whole-query number on `hits` comes from here.

## 5.3 Async I/O, and why the `File` trait changes

`File::read_at` is synchronous. A thread that calls it stops until the bytes arrive.

Document 01 recorded that DuckDB v2.0 made this a measurement-correctness issue rather than an optimization. Their change was a separate I/O thread pool alongside the compute pool, so that a thread waiting on a read is not a compute thread that is not computing, with the practical effect that on a machine where the data does not fit in the page cache the engine keeps the CPU busy while the disk works. On network storage the effect is larger still, and on a laptop NVMe with data in the page cache it is close to nothing, which is exactly why a design validated only on a warm laptop gets this wrong.

The constraint that shapes the answer is that the published rudb workspace has zero external dependencies. There is no tokio, there is no futures crate, and there will not be one. So the answer is the same one DuckDB reached and it is implementable with the standard library alone: a dedicated pool of I/O threads with a submission queue, separate from the compute threads, with the compute thread submitting a batch of reads and then either doing other work or waiting on a completion.

The trait grows one method and keeps the old one.

```rust
fn submit(&self, requests: Vec<Request>) -> Completion;
```

where a `Request` is an offset, a length and a destination buffer, and a `Completion` can be waited on or polled. `read_at` stays and is defined as a submit of one request followed by a wait, so nothing that exists breaks and the simple call sites stay simple.

Two things make this worth the trouble rather than being ceremony. Batching: a Parquet row group scan knows every byte range it needs before it reads any of them, so it submits all of them at once and the I/O pool can issue them concurrently and, where the ranges are adjacent, coalesce them into one larger read. Coalescing is worth more than concurrency on spinning media and on object storage, where the per-request cost dominates. And overlap: the scan submits the reads for row group n plus one while it is decoding row group n, which is the read-ahead that turns a stop-and-go scan into a continuous one.

`io_uring` is deliberately not in this layer. It is Linux only, it is a large amount of unsafe code against a raw syscall interface with a zero-dependency constraint, and the thread pool captures most of the win. It is recorded as a possible later change with the measurement that would justify it, which is the I/O pool showing up as a bottleneck on `server1` where there are only four cores to spare.

The simulator in `sim.rs` gets the same interface, which means completion ordering, partial completion and read failure become things that can be injected deterministically. That is worth a great deal and it is why the interface change is done here rather than being bolted on.

## 5.4 Two readers, and which one first

There are two formats rudb has to scan, and they are for different things.

**Parquet** is the interchange format. ClickBench `hits` is a Parquet file, TPC-H SF100 is being generated as Parquet, almost every real dataset a user will point at is Parquet, and DuckDB's own ClickBench entries include a Parquet-scanning row precisely because that is what people actually do. A database that cannot read Parquet is a database that cannot be tried.

**rudb's native format** is where the size win lives. 9.65 GB against 13.76 means the native scan reads seventy percent of the bytes the Parquet scan reads for the same data, and the encodings are chosen so that predicates can run without decoding, which Parquet's encodings mostly do not allow.

Parquet goes first, and it goes first for reach rather than for speed. It is what makes the benchmark runnable, it is what makes the engine usable by anyone, and it produces a number that is directly comparable with DataFusion, which is the other Rust engine and the fairest control for the question of whether a difference is the language or the engine. The native reader follows immediately and the comparison between the two on the same data on the same machine is one of the most informative measurements this project can make, because it isolates the format from everything else.

That split is the 2d and 2e boundary. 2d is async I/O plus a Parquet reader plus projection pushdown, and it ends with ClickBench running end to end for the first time. 2e is pruning, late materialization, encoded predicates and the native reader, and it ends with the scan being fast rather than merely present.

## 5.5 What a Parquet reader costs with no dependencies

This is the largest single piece of implementation work in the directory and it is worth being explicit about its size rather than discovering it.

The file metadata is Thrift compact protocol, so a Thrift compact decoder has to exist. It is a few hundred lines for the subset Parquet uses and it is entirely mechanical, and the schema of the metadata is fixed so the structures can be hand-written rather than generated.

Page decoding needs the Parquet encodings: plain, dictionary with RLE and bit-packed hybrid indices, delta binary packed, delta length byte array and delta byte array, and byte stream split for floats. `rudb-encoding` already has bit packing and delta and RLE for its own format, so the arithmetic is written and what is needed is the framing.

Definition and repetition levels have to be decoded even for flat schemas, because that is how Parquet expresses nulls. Nested types can be deferred, and are, because ClickBench and TPC-H have none, but the level decoding for the flat nullable case is required from the start.

Decompression is the part that is easy to underestimate. `hits.parquet` is Snappy. Snappy decompression is a small and well specified algorithm, a few hundred lines, and it goes in a new `rudb-compress` crate at rank 1. Zstd is a much larger job and it is common in the wild, so it is scheduled after the first ClickBench run rather than blocking it, with an explicit unsupported-codec error in the meantime. Gzip, LZ4 and Brotli follow on demand. This is a real cost of the zero-dependency rule and it is being paid knowingly.

The reader is written against the `submit` interface from section 5.3 from the first line, not retrofitted. A row group scan reads the metadata, computes the byte ranges for the projected columns, submits them as one batch, and decodes as they complete.

## 5.6 Reading less: projection, pruning and the page index

The comment in `source.rs` already names the biggest number in this document: projection pushdown on ClickBench is the difference between reading 20 GB and reading 200 MB, because the average ClickBench query touches two or three of a hundred and five columns. In a columnar format that is not an optimization, it is the entire point of the format, and it is the first thing the reader does.

Below projection there are three levels of pruning and Parquet supports all three.

Row group statistics give min, max and null count per column per row group, and a predicate that cannot be satisfied inside the range skips the whole row group without reading a byte of it. On `hits`, which is clustered by time, a date predicate skips almost everything, and several ClickBench queries have one.

The page index, meaning `ColumnIndex` and `OffsetIndex`, gives the same statistics per page and the byte offsets to reach a page directly. This is much less used than it should be and it is what makes a selective predicate on a clustered column cheap rather than merely cheaper. rudb reads it when it is present.

Bloom filters, where the writer wrote them, answer equality predicates on high cardinality columns that statistics cannot help with, because a min and a max over a URL column tell you nothing.

The same three levels exist in the native format by construction and better, because the M1 encoder already computes per-block sketches and knows the exact dictionary of a dictionary-encoded block, which is a stronger statement than a min and a max.

The number that has to be reported alongside every query is bytes read, which document 02 section 2.5 already requires, because a pruning bug that reads too much shows up as a slow query only when the data is bigger than the cache and shows up as a wrong byte count immediately.

## 5.7 Late materialization

ClickHouse 25.x shipped lazy materialization and measured large wins on ClickBench, with the methodological note recorded in document 01 that they disabled the query condition cache for those measurements so the win was attributable.

The mechanism: for a query that filters on one column and projects five others, read the filter column, evaluate the predicate, and then read only the surviving rows of the other five instead of reading all five columns and then discarding. When the filter is selective and the payload columns are wide, which describes most of ClickBench, this is the difference between reading five columns and reading five percent of five columns.

The decision of when to do it is not free, because at low selectivity the row-wise gather over the payload columns costs more than the sequential read it replaced. This is the same shape as the compaction decision in document 03 section 3.6 and it is decided the same way, with the actual inputs known at plan time: how selective the filter is estimated to be, how many payload columns there are, how wide they are, and whether they are compressed in a way that makes a random row access cheap or expensive. Dictionary-encoded columns make it cheap, delta-encoded columns make it expensive because reaching row n means decoding from the start of the block.

That last point deserves emphasis because it is a place where the format and the execution strategy are coupled, and rudb owns both. An encoding chosen only for size can make late materialization impossible, and the M1 chooser did not know that. The chooser gains a term for random access cost, and quantifying that term is a deliverable of 2e.

## 5.8 Predicates on encoded data

This is where `Form::Encoded` from document 03 section 3.9 lands, and it is where the parent spec's ten times scan claim actually comes from.

A dictionary-encoded column with a thousand distinct values and a million rows, filtered by an equality against a constant, does not need to look at a million values. It looks up the constant in the dictionary once, gets a code or gets nothing, and then the predicate over the rows is an integer equality against a code, or the answer is that nothing matches and the block is skipped entirely. Range predicates work the same way when the dictionary is sorted, becoming a range over codes. On `hits` this applies to a large fraction of the columns, because that is the data's shape and it is why the dictionary won so often in the M1 chooser.

A bit-packed integer column with frame of reference can answer a range predicate on the packed representation, comparing packed words against a packed constant with a small number of SIMD instructions per word, which is several times fewer instructions per row than unpacking.

FSST-compressed strings can answer an equality against a constant by compressing the constant with the same symbol table and comparing the compressed bytes, which is shorter and therefore faster than comparing the decompressed strings. A prefix predicate works similarly with care about symbol boundaries. `LIKE '%x%'` does not work this way in general and falls back to decompression, and knowing which patterns fall back is part of the specification rather than something discovered at runtime.

Run-length-encoded columns evaluate the predicate once per run, which for a low-cardinality clustered column is a reduction of several orders of magnitude, and the output is naturally a run-length-encoded boolean.

The rule that keeps this honest: every encoded predicate path has a decoded reference path, and a property test asserts they agree on random data for every encoding and every predicate. There is no case where a fast path exists without the slow path it is checked against.

## 5.9 Morsels and the shape of parallelism

The scan is the source, and the source is what hands out morsels, so the granularity decision is made here even though the scheduler is layer eight.

The unit is a row group in Parquet and a block in the native format, both of which are of the order of a hundred thousand rows. Smaller than that and the per-morsel overhead and the metadata reads dominate. Larger and the tail of a scan is unbalanced, with one thread finishing a huge morsel while seven idle, which on `server1` with four cores is a visible fraction of the query.

The source hands morsels out on demand rather than partitioning up front, which is the whole point of morsel-driven scheduling: a thread that gets an easy morsel comes back for another one. That is a small amount of code, it is a mutex around an index in the first version, and it is imposed now because a source that pre-partitions is a source that has to be rewritten at layer eight.

Read-ahead is per thread and bounded, and the bound is a memory decision rather than a throughput decision, because eight threads each reading ahead four row groups of a hundred and five columns is a large amount of memory on a machine with five gigabytes. The bound is stated in bytes and enforced, and it is one of the few places before layer eight where memory has to be accounted for at all.

## 5.10 The test gate

Correctness of a Parquet reader is not something to be confident about, because the format has corners and every writer uses different ones.

The corpus is files written by other people. DuckDB, pyarrow, parquet-java and parquet-cpp write the same logical data differently, they disagree about which encodings to use and about how to write statistics, and older files use encodings newer writers never emit. The test is a directory of files from all four writers, read by rudb, compared row for row against what DuckDB reads from the same file. That is a strong oracle and it is available for free.

The deterministic simulator is used for what it is for. Every read fails, at every request, one at a time, and the scan either produces the right answer or a clean error and never a wrong answer or a hang. Completions arrive out of order. A submit batch completes partially. These are the failure modes an async I/O layer has, they are almost impossible to hit reliably in a real filesystem test, and `sim.rs` already exists to make them deterministic.

Byte counting is a test and not just a metric. For a query with a known selective predicate over a file with known statistics, the number of bytes read is a computable quantity, and asserting it exactly is what stops a pruning regression from being invisible. This is the single most valuable test in this layer because pruning bugs are silent.

Encoded predicates are property tested against their decoded reference paths, as section 5.8 requires, across every encoding, every predicate, every physical type and both nullabilities.

`gamingpc` runs all of it, because file I/O on Windows has different semantics for concurrent reads on one handle and this is exactly the layer where that matters.

## 5.11 The benchmark gate

This is the first layer where the gate is the real benchmark rather than a microbenchmark, because this is the first layer where the real benchmark runs.

At the end of 2d, ClickBench runs end to end against rudb on `server3` reading `hits.parquet`, and every one of the forty-three queries produces an answer that matches DuckDB's. That result, whatever the times are, is the most significant milestone in this directory, because it converts every later claim from an argument into a measurement.

The 2d target is modest and deliberately so: within three times of DuckDB's Parquet-scanning time on the scan-dominated queries, meaning ClickBench Q1 to Q5 and Q12 to Q19. Beating DuckDB at 2d would mean the reader is doing something wrong or the aggregate is not being reached.

The 2e target is the real one. On the scan-and-filter-dominated queries, reading the native format, with pruning and encoded predicates and late materialization, rudb reads fewer bytes than DuckDB and spends fewer CPU seconds. The bytes-read claim is the one to lead with because it follows from the 0.47 size ratio plus pruning and it is hard to argue with. The CPU seconds claim is the one that matters, and the target for the layer is a factor of three on those queries against DuckDB, with the remaining factor coming from the aggregate in layer five.

Microbenchmarks alongside: page decode throughput per encoding in megabytes per second and rows per second, predicate evaluation on encoded against decoded per encoding, pruning effectiveness as the fraction of row groups and pages skipped for a set of predicates of known selectivity, and I/O throughput with the pool at one, two, four and eight threads on both `server1` and `server3` because they have very different disks.

The load-time number gets its first real measurement here too, and it is expected to be bad. The M1 encode rate is 5 MB/s of values per core, DuckDB loads `hits` in minutes, and the changelog already records that the write path being twenty times slower than the read path is not shippable. It is not fixed in this layer, it is measured in this layer, and it is scheduled honestly in document 14 rather than being left as a known embarrassment.

## 5.12 Exit criteria

**2d.** rudb reads Parquet through an async I/O pool with projection pushdown, all forty-three ClickBench queries return answers matching DuckDB on the full `hits.parquet` on `server3`, the other-writers corpus passes, the simulator's fault injection produces no wrong answers and no hangs, and the times are within three times of DuckDB on the scan-dominated queries.

**2e.** The native format reader exists and runs the same forty-three queries, row group and page pruning and Bloom filters are used where present, late materialization is decided at plan time with the encoding-aware cost term, predicates run on dictionary, bit-packed, FSST and run-length data with property tests against their decoded references, and on the scan-and-filter queries rudb reads fewer bytes and spends fewer CPU seconds than DuckDB on the same machine.

Named as deferred: zstd and the other codecs beyond Snappy, nested types in Parquet, `io_uring`, and the write path, which is measured here and fixed later.
