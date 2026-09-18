# The layout

Everything here is the consequence of one of the six invariants in document 01, and each section says which.

## The shape of the file

```
  [ root ]              4 KiB, two copies written alternately, checksummed
  [ schema ]            column names, types, rules, and where each column's directory lives
  [ directory ]         the per block per column records, stored column major
  [ globals ]           dictionaries, rank permutations, FSST symbol tables
  [ data ]              super blocks, each of them column major inside
```

Four levels of granularity, and they are chosen rather than inherited.

| level | size | what it is for |
|---|---|---|
| file | whole | a database, or one table exported |
| super block | 16 blocks, so 1,966,080 rows | the unit of write buffering and the unit that makes a column's bytes contiguous |
| block | 122,880 rows, fixed and never anything else | the unit of parallelism, of skipping and of an encoding decision |
| tile | 1024 values | the unit of random access inside a block and the finest zone map |

122,880 rows is 120 tiles of 1024, and it is the same count DuckDB uses as 60 vectors of 2048, which means an imported DuckDB table maps one to one onto our blocks with no rechunking. That is the only reason for that particular number and it is a good enough one.

**The block size is fixed by the format and not chosen by the writer.** This is issue #511 stated as a format rule. A Parquet reader gets whatever parallelism the file's author happened to write, which for a file written by pandas is one row group and one thread. Here the block boundaries are a function of the row count, so a reader can compute them, and a scan over a file with one million rows gets eight blocks whether or not anybody planned for that. Below 122,880 rows a block is split into tile runs for parallelism instead, so a small table still spreads.

## The directory is columnar, and that is the main departure

From invariant 1. A query reads 2.6 columns out of 105 on average, and it should read 2.6 parts in 105 of the metadata too.

A Parquet footer is one Thrift structure holding every column chunk of every row group, and it has to be parsed from the front to find anything in it. Measured on the bench host, ClickBench's `hits.parquet` at a hundred million rows is 14.8 GB in 226 row groups of 105 columns, and its footer is 2,439,316 bytes. That is 23,730 `ColumnChunkMetaData` records, each with a nested `Statistics`, each field length prefixed and variable width, and a reader that wants `CounterID` and `EventDate` parses all 23,730 to find 452 of them.

Two and a third megabytes is 0.017 percent of the file, so this is not a claim that Parquet's footer is large. It is a claim about proportion. The footer is the entire fixed cost of every query, it is read and parsed in full no matter how few columns the query names, and for the small file and cheap shape end of note 11's table it is most of what the query does. `SELECT count(*)` reads nothing else at all.

The directory in this design is larger in total and smaller to read. At 122,880 row blocks the same data is 814 blocks times 105 columns times 64 bytes, so 5.5 MB, which is more than twice Parquet's footer. Reading the five columns q37 needs costs 814 times 64 times 5, so 260 KB, against Parquet's 2.33 MB, which is nine times less for a query with three and a half times the skipping granularity. A query naming one column reads 52 KB. That is the trade: pay more on disk for metadata nobody reads, in exchange for reading only the part you want.

So: **the directory is an array of fixed width records, laid out column major.** All of column 0's records for every block, then all of column 1's, and so on. The schema says where each column's run starts. A reader that wants five columns issues five reads of `blocks * 64` bytes each and parses nothing it does not want.

The record is 64 bytes and holds this:

| field | bytes | what it is |
|---|---|---|
| offset | 6 | where this column's bytes for this block start |
| stored | 4 | how many bytes are there |
| raw | 4 | how many they decode to |
| minimum | 8 | the value, or the dictionary rank for a dictionary column |
| maximum | 8 | the same |
| nulls | 4 | how many |
| distinct | 4 | the estimate, from the sketch |
| sum | 8 | for numeric columns, so `sum` over a whole block is answered from here |
| encoding | 1 | which tree the block used |
| flags | 1 | sorted, all null, all one value, has a tile index |
| tiles | 6 | where the tile index is, or zero |
| spare | 10 | |

Six bytes of offset is 256 TiB of addressable file, which is enough, and it keeps the record at a power of two so a block number is a shift rather than a multiply.

For hits at a hundred million rows that is 814 blocks times 105 columns times 64 bytes, so 5.5 MB of directory for a 14.8 GB Parquet file's worth of data. Reading the five columns q37 needs costs 260 KB in five sequential reads, and none of it is parsed in the sense of being decoded, because a fixed width record is read by pointing at it.

The `sum` field is not padding. Eleven ClickBench queries are an unfiltered or lightly filtered aggregate, and `SELECT count(*)`, `SELECT min(EventDate), max(EventDate)` and `SELECT sum(AdvEngineID)` over a whole table are answered out of the directory without a byte of data being read. That is q1, q7 and part of q3 going to zero.

## Data is column major, within a super block

From invariants 1 and 3. Parquet is row group major and column major inside it, so reading one column across the whole file is one seek per row group. For hits that is eight hundred seeks to read one column, and the runs between them are small because one column of one row group of a hundred and five is a small thing.

Here a super block holds sixteen blocks and inside it the layout is column major, so one column's sixteen blocks are one contiguous run. Reading `CounterID` over the whole table is fifty one reads rather than eight hundred and fourteen, and each of them is sixteen times larger.

Sixteen is a compromise and the reason is the write side. A super block has to be buffered by whoever is writing it, because you cannot emit column major output from a row major input without holding the rows. Sixteen blocks is 1,966,080 rows, which for a hundred byte row is about 200 MB per writer thread before encoding, and that is the largest number this design is willing to ask for. Document 04 says what happens when the input arrives in a shape that makes even that too much.

### Small columns are read whole

Invariant 3 says the filter columns are narrow, low cardinality and few, and the payload columns are wide, high cardinality and many. That difference is worth a rule rather than a hope.

**A column whose total stored size is under 64 MiB is read in one request and cached, rather than read by block.** For hits, `CounterID`, `EventDate`, `IsRefresh`, `DontCountHits`, `AdvEngineID`, `ResolutionWidth`, `SearchEngineID`, `RegionID`, `TraficSourceID`, `IsLink` and `IsDownload` are all under that, most of them far under, and together they are every predicate column in the suite except the strings. So the seven queries q37 through q43 start by issuing four reads totalling a few tens of megabytes, evaluate the whole predicate against the whole table in memory, and only then find out which blocks of `URL` or `Title` they need.

That is the same idea as DuckDB's `EvaluateFilters` reading the filter columns first, from note 11's finding 4, except it is a property of the layout rather than a scheduling decision, and it comes out contiguous rather than as eight hundred scattered ranges.

TPC-H gets the same treatment for a different reason. `l_shipdate`, `l_discount`, `l_quantity`, `l_returnflag`, `l_linestatus` and `l_shipmode` are all tiny, and twelve of the twenty two queries filter on a date column that lands in this category.

## Tiles, and skipping inside a block

From invariant 2. A block of 122,880 rows is a good unit for deciding not to read. It is a bad unit for deciding not to decode, because a filter that keeps one percent spread evenly keeps something in every block.

So a block carries an optional tile index: 120 entries of a minimum, a maximum and an offset. Per block statistics skip 122,880 rows and per tile statistics skip 1024, and the second one is what makes a selective filter stop being a full decode.

The tile index is optional per block, because for a column whose values are random within a block it is 120 entries of noise. The writer emits it when the block is sorted, nearly sorted, or has a spread inside a tile that is materially smaller than the spread across the block. The rule is measured at write time rather than asserted, and the flag in the directory record says whether it is there.

**Random access to tile k inside a block is O(1) and that is a format requirement rather than a nice to have.** It is what lets a filter on one column fetch only the matching tiles of another, and a format that only supports sequential decompression turns every selective query into a full block decode. Encodings that cannot offer it, which means anything with a stream cipher shape like a general purpose byte compressor over the whole block, are only allowed where the tile index is absent.

## Globals

Three kinds of thing live outside the blocks because they are shared by all of them.

**A dictionary is scoped to a column across the whole table, not to a block.** This is where most of document 03's size win comes from and most of document 06's speed win, and it is the reason the design has a globals region at all.

**A rank permutation** per dictionary, mapping a code to its position in sorted value order. Document 03 explains why this exists rather than sorting the codes themselves.

**An FSST symbol table** per column that uses FSST, shared by every block, so the 2 KiB table is stored once rather than eight hundred times.

All three are read once and cached for the life of the process, and they are the reason the format is fast at the second query even when it was ordinary at the first. They are also the thing that has to be got right on the write side, which is document 04.

## What is deliberately not here

**No row groups with writer chosen boundaries.** Covered above.

**No per block dictionaries.** A block that wants a dictionary uses the column's. If the column has no dictionary then the block does not get one, because a per block dictionary is exactly the thing whose repetition costs Parquet most of its size on this data.

**No general purpose byte compressor over a whole block by default.** Snappy or zstd over a block is a good ratio and it destroys tile addressability, which invariant 2 says is worth more. A block may use one when the directory says it has no tile index, and the writer will choose that for columns nothing filters on.

**No index structures.** Three ClickBench queries are point lookups against an unindexed column and this format does not fix them. That is a deliberate scope line: an index is a separate object with its own maintenance story and it belongs in a document about indexes.
