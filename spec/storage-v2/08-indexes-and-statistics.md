# Indexes and statistics

Document 02 ends by saying that three ClickBench queries are point lookups against an unindexed column, that this format does not fix them, and that an index belongs in a document about indexes. This is that document, and it turned out to be about more than the three.

The goal it is written against is narrower than the rest of the directory. The rest asks what a good format looks like. This one asks what would win ClickBench, because that is the benchmark the project is measured on and because a structure that cannot be shown to pay on a real suite is a structure nobody should carry.

## The measurement first

Everything below is derived from this table and none of it was designed before the table existed. Taken on the bench host over the full `hits` at 99,997,497 rows, in file order, cut into 814 blocks of 122,880 rows and 97,654 tiles of 1024, which are document 02's two units.

| predicate | rows kept | blocks holding one | tiles holding one |
|---|---|---|---|
| `CounterID = 62` | 0.74% | 8 of 814, so 0.98% | 723 of 97,654, so 0.74% |
| `EventDate` between 2013-07-01 and 2013-07-31 | 100% | 814 of 814 | not taken |
| `EventDate` between 2013-07-14 and 2013-07-15 | not taken | 169 of 814, so 20.8% | not taken |
| `CounterID = 62` and the July month range | 0.74% | 8 of 814 | not taken |
| `UserID = 435090932899640449` | 1 row | 1 of 814 | 1 of 97,654 |
| `URL LIKE '%google%'` | 15,911 rows, so 0.016% | 626 of 814, so 76.9% | 6,371 of 97,654, so 6.5% |
| `Title LIKE '%Google%'` | not taken | 714 of 814, so 87.7% | not taken |
| `SearchPhrase <> ''` | 13,172,392 rows, so 13.2% | not taken | 96,407 of 97,654, so 98.7% |

And three facts about the data that decide which structures are possible at all.

| | min | median | max |
|---|---|---|---|
| distinct `CounterID` in a block | 1 | 1 | 115 |
| distinct `EventDate` in a block | 1 | 2 | 11 |
| distinct `URL` in a block | 13 | 29,347 | 93,319 |

## What that table says

**The one predicate that matters most is already answered by a minimum and a maximum.** `CounterID = 62` is the filter in q37 through q43, which is seven of the 43 queries and the largest single block of the suite. The median block holds exactly one distinct `CounterID`, so the minimum equals the maximum and an ordinary zone map is an exact test. Eight blocks of 814 survive. There is nothing to invent here: it is the cheapest statistic there is and it prunes 99 percent of the file.

**The date predicate in those same seven queries prunes nothing, and that was believed otherwise.** Document 01 lists `EventDate` as a clustering column and the July range as a shape that keeps a contiguous stretch. The file runs from 2013-07-02 to 2013-07-31 and nothing else, so the month range in q37 through q42 covers every row in the table. It is a predicate that costs time to evaluate and removes nothing. The data is genuinely clustered by date, at a median of two distinct dates a block, and q43's two day range does prune to 169 blocks of 814, so the clustering is real and only six of the seven queries fail to use it.

**The block is the right unit for the filter columns and the wrong unit for everything else.** `CounterID = 62` gets 0.98 percent at block granularity against 0.74 at tile granularity, so refining it is worth 1.33 times and not worth much. `URL LIKE '%google%'` gets 76.9 percent at block granularity, which is no pruning at all, and 6.5 percent at tile granularity, which is twelve times. The same file, the same query suite, and the two units differ by a factor of twelve on one predicate and not at all on another. A format with one granularity is wrong for one of them.

**A zone map is useless for a point lookup and the exact test is nearly free.** `UserID = 435090932899640449` is in one block. A minimum and a maximum bracket it in 535 blocks of 814, because `UserID` is scattered and every block's range covers most of the space. So the 65 percent of the file that a zone map cannot rule out is 65 percent that any membership test rules out instantly. This is the clearest case in the table for a filter that answers membership rather than range.

**No structure helps `SearchPhrase <> ''` and ten queries have it.** 98.7 percent of tiles hold a nonempty search phrase. There is no granularity at which skipping works, because the matching rows are everywhere. The only lever on those ten queries is invariant 6, which is to do the comparison once per distinct value rather than once per row, and that is a dictionary property rather than an index.

## What we already have, and what it is worth

Row group pruning is in the Parquet reader and it fires. Measured on `hits-1m-snappy.parquet`, which has nine row groups, `SELECT count(*) FROM hits WHERE CounterID = 62` reads 247,265 rows out of 999,975, so seven of the nine groups were skipped and the two the footer said could hold a 62 were opened. That matches the footer exactly.

So what is left after pruning is the question, and q37 answers it. On the same file, `EXPLAIN ANALYZE` puts the query at 32.0 ms wall and 49.8 ms of CPU, of which the scan is 51.7 ms of CPU and everything above it is 2.2 ms: the filter 0.58, the aggregate 1.48, the top ten 0.09. The scan emits 247,265 rows and the filter keeps 6,722 of them. So 97 percent of our time is spent decoding a column that the filter then throws away.

**The gap to DuckDB is per core and it is 2.4 times, not the 1.9 an earlier draft of this paragraph said.** That number came from comparing our wall clock against DuckDB's multi threaded wall clock, which is not a comparison of anything. Twenty runs of q37 in one process with process start subtracted, on `gamingpc-wsl`: we take 23.5 ms a query of wall and 39 ms of CPU, DuckDB takes 14.0 ms of wall and 37 ms of CPU. Pinning both to one thread separates the two effects. We go to 39 ms of wall for 39 ms of CPU, so threads were buying us 1.68 times on two row groups. DuckDB goes to 15.5 ms of wall for 16 ms of CPU, so threads buy DuckDB nothing at all on this file and its multi threaded run is spending 35 ms of CPU to reach the wall clock it already had. The honest comparison is 39 against 16 single threaded, which is 2.4 times behind per core, and the parallelism is a second and smaller problem on top of it.

**That is not a statistics problem and no structure in this document fixes it.** Four of the five columns q37 reads are `EventDate`, `CounterID`, `IsRefresh` and `DontCountHits`, which are two bytes each at most. The fifth is `URL`. Decoding `URL` for 247,265 rows to keep 6,722 is a factor of 37 available with no new statistic, no finer granularity and no format, by reading the four cheap columns first and decoding the fifth only where they all passed. That is late materialisation, it is items three, four and five of note 11's work list, and it is worth more right now than anything below.

**And `URL` is not dictionary encoded, which kills two of the things that were going to be done to it.** The footer of `hits-1m-snappy.parquet` says `URL` is `PLAIN`, one `DATA_PAGE` per row group per column chunk, 4.6 MB compressed and 10.5 MB uncompressed for 123,554 values. `Title` is `PLAIN` too. Only the low cardinality columns, `CounterID` and `EventDate` among them, are `PLAIN_DICTIONARY`. So pushing the filter into the dictionary and skipping a page the filter killed are both unavailable on the one column that costs everything, and a scan cannot even skip the decompression, because a Snappy block is not seekable and `PLAIN` is not randomly addressable once it is decompressed.

**What the per core gap is actually made of, measured rather than guessed.** Isolating the column with `SELECT count(*) FROM read_parquet(f) WHERE URL <> ''` over all nine row groups on one thread puts us at 102 ms against DuckDB's 57 ms, which is 1.79 times and the same shape as q37. Our own stage split for it is read 4.98 ms for 37.86 MiB, decompress 50.96 ms for 90.18 MiB so 1769 MiB/s, decode 26.78 ms for the same bytes so 3368 MiB/s, and assemble 10.68 ms. A callgrind run over the same query, 1,003,228,564 instructions in total, attributes 49.97 percent of them to `snappy::decompress_into` with 9,360,136 branch mispredicts, 11.63 percent to `StringColumn::push_in_place` and 9.38 percent to `core::str::converts::from_utf8`. That is 47 instructions, 12 branches and 0.88 mispredicts for every nine bytes of output a Snappy element produces. **Neither the decoder nor the validator is a statistics problem or a format problem, and together they are most of the column that is 97 percent of the query.**

**Both of those were then taken apart by building the change and measuring it, and the instruction counts turned out to predict almost nothing.** Taking the per element range tests out of the Snappy loop removed eight percent of its instructions and 35 percent of its branch mispredicts and bought 1.4 percent of wall clock, so the loop was never bound by them, and a cache run says it is not bound by memory either: 745,688 L1 read misses over 87 million reads and no last level read misses at all. And the reading of the UTF-8 number was wrong. It looked like the ASCII pre-check in `push_in_place` was pure waste, because hits URLs carry Cyrillic and a failed pre-check means both scans get paid. Measured, 149,915 of the 999,975 URLs are not ASCII, so 85 percent of them settle on the pre-check, and a build with the pre-check taken out is slower. Three builds off the same tree, three rounds of six runs alternating between them, medians of the twelve steady state runs of each: today 125.03 ms, with no validation at all 105.63 ms, with the pre-check dropped 126.04 ms. So validation is 19.4 ms of 125, which is 15.5 percent of the query, and what is left to attack is the 627 instructions the standard library spends on each of the 149,915 values that really are not ASCII.

**Page granularity would not help, which was worth checking before building it.** The obvious next step after row group pruning is Parquet's column index, which carries a minimum and a maximum per data page. Measured on the same file, `CounterID` has exactly one `DATA_PAGE` per row group, so the page and the row group are the same thing and a column index would prune nothing at all. That idea is dead on this data.

**Block size still matters, at scale, and that is a format argument.** The pruning quality is a function of how many rows are in the unit, and the million row file is not representative of it: two of its nine row groups hold a 62, so 22 percent, while blocks of 122,880 rows over the full hundred million give 8 of 814, so 0.98 percent. The predicate's rows are clustered, and a small sample taken from the front of the file lands inside the cluster. So the small scale runs understate pruning by about twenty times, and any conclusion about it drawn at a million rows is a conclusion about the sample.

## The four structures

In the order of what they pay, with the rule that each one has to name the queries it is for.

### The directory record, which document 02 already specifies

A minimum, a maximum, a null count, a row count and a sum, per block per column, 64 bytes, laid out column major. Nothing new here except the evidence that it is the highest value structure in the design.

It prunes q37 through q43 to eight blocks. It answers q1, q3, q4 and q7 with no data read at all, because a count, a sum, a minimum and a maximum over a whole table are sums and extremes over the per block records. It answers q30, which is ninety sums of `ResolutionWidth + k`, because every one of them is the stored sum plus k times the stored count, which is a plan rewrite the directory makes free.

### A tile zone map, which document 02 makes optional

120 entries a block of a minimum, a maximum and an offset. Document 02 says the writer emits it when the block is sorted, nearly sorted, or has a spread inside a tile materially smaller than the spread across the block.

That rule stands and the measurement narrows it. On `CounterID` a tile index is worth 1.33 times over the block record and it costs 2,400 bytes a block against 64, so for the filter columns it is a bad trade and the writer should decline it. On a column nothing filters on it is pure cost. The tile zone map is worth having on exactly one shape, which is a column that is clustered at a scale finer than a block, and the writer decides that by measuring the spread rather than by being told.

### A tile membership filter, which is the new thing

A small Bloom filter per tile per column, over dictionary codes rather than over values. This is what answers the two shapes a zone map cannot.

The point lookups are q20, q41 and q42. `UserID = 435090932899640449` touches one tile of 97,654, and a zone map leaves 535 blocks of 814 standing. A membership test over codes turns the whole scan into a dictionary probe, a code, and one tile.

The substring scans are q21, q22, q23 and q24. Those cannot be answered by any per tile structure directly, because a Bloom filter over codes does not know what a code's bytes look like. The sequence that does work is: run the `LIKE` once per distinct value over the column's global dictionary, which is 18.3 million evaluations rather than 100 million and 3.2 GB of bytes rather than 8.8, collect the codes that matched, then probe each tile for any of them. The measurement says the last step leaves 6.5 percent of tiles, so the tile filter is worth twelve times on top of whatever the dictionary scan costs.

Sizing it. A tile is 1024 rows, so at ten bits a distinct code a tile filter is at most 1,280 bytes, and 97,654 tiles of one column is 119 MiB. On the six columns anything in the suite looks up or matches that is under 750 MiB against a 14.8 GB file, so five percent, and it is off by default on the other 99 columns because nothing asks them a membership question.

Building it is one hash per row, in the same pass that assigns codes, with no coordination between threads and no second pass. That matters more than the size and the next section is about why.

### The dictionary's own statistics

A global dictionary is already in the design for size, in document 03, and for execution, in document 06. Two numbers per dictionary make it a statistic as well.

**The number of entries.** `COUNT(DISTINCT UserID)` and `COUNT(DISTINCT SearchPhrase)` are q5 and q6, and both are exactly the number of codes the column uses. Exact, not estimated, and read from the schema.

**The number of rows behind each code.** This is the column's value histogram, exact rather than sampled, at four bytes a distinct value. For `URL` that is 73 MiB and for a small integer column it is nothing.

It is worth being uncomfortable about how many ClickBench queries that answers. q2 is the sum of counts over the codes that are not zero. q8, q13, q16, q34 and q35 are all `GROUP BY` one column `ORDER BY COUNT(*) DESC LIMIT 10`, which is the top ten codes by count and then ten values decoded. q21 is the sum of counts over the codes whose value contains `google`. Together with the directory that is fourteen of the 43 queries reading no column data at all.

Fourteen of 43 is a number to be suspicious of, so here is the argument for why it is legitimate and where it stops.

An exact per value frequency table is the statistic a cost based optimizer wants for every query in every workload, it is what a histogram approximates, and keeping it exact is cheaper here than sampling because the dictionary build already visits every row. It is not a precomputed answer to a query, it is not keyed on anything a query says, and nothing about it was chosen by looking at the suite. DuckDB answers q1 from its own metadata for the same reason and nobody calls that cheating.

Where it stops is the moment a second predicate appears. q37 is the same shape as q34 with a filter on two other columns and the histogram cannot touch it. The rule is that the statistic answers a query only when every predicate in it is a predicate on the group key itself, and q13's `SearchPhrase <> ''` qualifies because it removes exactly one code while q37's `CounterID = 62` does not because it is a different column. Seven of the suite's hardest queries are on the wrong side of that line, which is the honest measure of how far this goes.

The other place it stops is deletes. A count per code is exact only over blocks with no delete bitmap. F2's checklist has immutable blocks with delete bitmaps, so the rule is that the histogram carries the block range it was computed over, and a query whose range intersects a deleted row falls back to reading. A statistic that is silently wrong after a delete is a wrong answer, and a wrong answer is worse than a slow one.

## What is refused

**An n-gram index.** It is the obvious answer to `LIKE '%google%'` and the arithmetic says no. A three gram filter has to see every position of every value. Built per tile over the tile's distinct values it is roughly 800 values of 88 bytes, so 69,000 hashes a tile and 6.7 billion over the column, which is a second full pass over the data at a cost comparable to the encode. Built once over the global dictionary as a posting list per gram it is 18.3 million values times about 60 distinct grams each, so a billion postings, which is larger than the column it indexes. Both are refused on build cost, and the measured 6.5 percent of tiles is reachable through the dictionary scan plus the code filter instead, which costs one hash a row.

**A materialised top k.** Storing the ten most frequent values of a column would answer q13, q16, q34 and q35 directly. It is a precomputed answer to a query shape, it is useless the moment the limit is eleven or a filter appears, and it exists only because the benchmark asks for ten. The per code histogram gives the same answers from a statistic that is general, and that difference is the whole point.

**A cache of predicate results across queries.** ClickBench runs `URL LIKE '%google%'` in q21, q22, q23 and q24. Caching the matching code set after q21 would make three later queries free. Within one query, evaluating a predicate once per distinct value and reusing it is invariant 6 and it is right. Across queries it is reading the benchmark's running order, and the fact that it would work so well here is the reason to refuse it rather than the reason to do it.

**A sorted index, a B-tree, or anything with its own write path.** Every structure in this document is computed in the pass that encodes the block, from the block's own rows, with no lookup into anything global and no second visit to the data. That is the line, and it is the line because of the next section.

## The ingress budget

The write path is one of the three goals in the README and this document is the part most likely to break it, so each structure gets a stated cost rather than an assurance.

| structure | per row | per block | needs coordination |
|---|---|---|---|
| directory record | one compare against the running minimum and maximum, one add to the sum | 64 bytes written | no |
| tile zone map | the same compares, reset every 1024 | 2,400 bytes, and only when the writer chose it | no |
| tile membership filter | one hash and one or two bit sets | 120 KiB at ten bits a code | no |
| dictionary entry count | nothing, it is the dictionary's length | nothing | at the column level, once |
| per code row count | one increment on a hash table entry the dictionary build already found | four bytes a distinct value | the same merge the dictionary already needs |

Nothing in that table is a second pass over the data and nothing in it is a structure a thread has to lock. The per code count is the only one that has to be merged across threads and it merges the same way the dictionary does, which document 04 already has to solve for a reason that has nothing to do with statistics.

The number to hold this against is note 10's encoder scaling, which is 13.5 MiB/s a thread and 157.8 over thirty two. If adding all of this costs more than ten percent of that, the cheapest structure to drop is the tile membership filter, because it is the only one whose per row cost is a hash rather than a compare.

## The order to build in

**First, and it is not in this document, decode the payload columns only for rows that survived the filter.** Measured above, that is 97 percent of q37 at a million rows and a factor of 37 on the column that costs everything. It needs no statistic, no format and no new structure, and until it is done every number in this document is being measured against a scan that is doing work nobody asked for.

**Second, and also not in this document, the per core cost of the scan that is left.** Late materialisation removes `URL` from q37 and from the six queries shaped like it, and the measurement says those then land under ten milliseconds, which is already ahead of DuckDB's 15.5 single threaded. It does nothing for the queries that genuinely want the column, and there the work is Snappy at half the query and UTF-8 validation at 15.5 percent of it. Both are in `rudb-compress` and `rudb-vector` rather than in any format, so neither waits on F2 and neither is an argument for or against anything below. What is worth carrying out of the two attempts so far is that the instruction count was a bad predictor of both, and the thing that settled each of them was building it and running the query, at a cost of about an hour each.

**Third, the per code row count.** It is four bytes a distinct value, it is free to compute, and it takes seven queries to zero. It needs the global dictionary, so it lands with F2 rather than before it.

**Fourth, the tile membership filter.** Three point lookups and four substring scans, and it is the one with a real per row cost, so it goes last of the three and it goes in behind a measurement of what it costs the write.

**Not yet, and possibly never, the tile zone map on the filter columns.** Measured at 1.33 times on the one column it would matter for, against 37 times the metadata. Document 02 leaves it to the writer and the writer should say no here.

**Not ever, Parquet's column index.** One data page per row group on the column it would have been for, so it prunes nothing. Recorded because it was the obvious next thing and an afternoon of checking is cheaper than a week of building.

## The checks

**The pruning is measured end to end, not asserted.** For each of q37 through q43, the bytes read and the rows decoded, against the 0.74 percent of rows that survive. Anything above about one percent means a granularity is wrong somewhere and the number says which.

**The histogram is checked against a full scan.** Every query it answers is run both ways and the answers compared, on every corpus file, as a differential test rather than as an assertion. A statistic that answers a query is a second implementation of that query and it gets tested like one.

**The write cost is measured before the structure is kept.** `cargo xtask encode` already reports megabytes a second a thread with a split across candidates. The same run with the statistics on and off is the number that decides whether the tile membership filter ships.

**The thing that would make this document wrong** is if q37 through q43 stay slow after the payload columns are materialised late. The measurement above says the scan is 97 percent of that query and that almost all of the scan is a column the filter kills, so fixing that should leave a query which is mostly the aggregate. If it does not, then the time was never where the profile said it was, and nothing in this document would have helped. That measurement is a day of work and it comes first for exactly that reason.
