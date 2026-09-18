# Storage v2

Document 05 in the parent directory specifies a native format. F2 is the milestone that builds it. This directory is a second pass at the same question, done from the queries rather than from the file, because the first pass started from what a file should look like and only then asked what runs over it.

The order matters more than it sounds. If you start from the file you end up with Parquet with better encodings, because Parquet is what a sensible person designs when the question is how to lay out a table. If you start from the queries you find out that the thing every query actually does is read five columns out of a hundred and five, keep one percent of the rows, and return ten of them, and that almost none of the file's design is about that.

## The three goals, as numbers

**Size.** Half of what Parquet writes for the same data, with Parquet given its best settings rather than its defaults. Note 03 in the parent directory has Umbra at 8.30 GB and DuckDB at 20.46 GB for the same dataset, and this target sits below both. Document 02 of the parent directory sets a longer term figure of 2.05 GB, which needs more than a format, so 2x over Parquet is the number this design is accountable for.

**The design does not currently reach that and document 03 says by how much.** The first draft of document 03 worked it out at 1.99 times and called the assumptions conservative. They were not. Measured, the design lands at 1.70 times against the Snappy `hits.parquet` on disk with the encoder we have today, and 1.79 times if the encoder is fixed. Against Parquet given its best settings, which is ZSTD and holds the same data in about 8763 MiB rather than 14091, it is 1.05 times today and 1.11 at best. The first of those numbers was 1.45 until PR #576 landed a match finder in the string encoder, which is the one item on document 03's list that has been acted on. The three things that would close the rest are named at the end of that document and none of them is a format problem.

**Write speed.** Linear in threads up to the machine's real cores, and faster per core than Parquet's writer. Note 10 measured our encoder at 13.5 MiB/s a thread and 157.8 MiB/s over thirty two, with the fall off starting at eight because the bench host has eight performance cores. The format must not add a serial phase that changes that shape.

**Read speed.** A scan reads bytes in proportion to the columns and rows the query wants, and the metadata it reads to find those bytes is also in proportion. The second half of that sentence is the part Parquet fails and it is the part this design is mostly about.

**Query speed.** The format hands execution codes rather than values wherever it can, and hands it enough per block summary to skip work without reading data. A format that decodes faithfully and then makes the engine do everything from scratch has given the engine nothing. Document 08 is where that is worked out against the suite, and the headline of it is that the cheapest statistic in the design, a minimum and a maximum per block, prunes 99 percent of the file for the seven queries that are the largest block of ClickBench.

## What is in here

| file | what it settles |
|---|---|
| `01-what-the-queries-do.md` | The evidence. All 43 ClickBench queries and all 22 TPC-H queries, classified, and the six invariants that fall out of them. |
| `02-the-layout.md` | The format. Levels, sizes, addressing, and the metadata arrangement that is the main departure from Parquet. |
| `03-the-bytes.md` | Where 2x over Parquet comes from, itemised, with the arithmetic and the assumptions it rests on. |
| `04-writing.md` | The write path, and how the one global structure in the design stays out of the way of thirty two threads. |
| `05-reading.md` | The read path, what a scan issues and in what order, and why nothing in it blocks. |
| `06-querying.md` | What the format promises execution, which is mostly codes and summaries, and what that buys on the queries in 01. |
| `07-the-checks.md` | The instruments, the exit criteria, and the specific measurements that would say this design is wrong. |
| `08-indexes-and-statistics.md` | What the file knows without reading itself, measured against the 43 ClickBench queries, and what it costs to build. |

## How this relates to F2

F2's checklist is a format with four levels, a global dictionary, per block statistics, a sort key and a parallel writer. Nothing here contradicts that. What this pass adds is the reasoning for each of those from the query side, which F2's issue asserts rather than derives, plus four things F2 does not have: the metadata arrangement in document 02, the rank permutation in document 03, the two phase write in document 04 and the functional dependency rule in document 03.

If any of that turns out to be wrong the right outcome is to change these documents, not to quietly build the thing in F2's checklist and call it done. Several things already were wrong and have been changed.

The reason a global dictionary shrinks hits is not that Parquet repeats the dictionary per row group, which is four percent of what the column costs, but that Parquet abandons the dictionary on a high cardinality string column and writes plain strings for the rest of the chunk. The functional dependency rule, which was the headline of document 04's cross column section, does not hold for the one pair on hits it was written for. The dictionary bodies in document 03 were sized with the average value length per row where it should have been per distinct value, which made every one of them nearly three times too small. And the compression ratio the whole size case rested on was guessed at five and measured 2.6 with the encoder we had, against 5.33 for ZSTD over the same bytes, which moved the top item in this directory out of the format and into the string encoder until it was fixed and measured 4.3.

## What would make this whole direction wrong

Three things, and they are worth stating before anything is built.

The first is if the gap to DuckDB turns out not to be in the format. Note 11 measured us against DuckDB over the same Parquet file and found us 1.7x to 2.7x behind per core on decode and 3.3x behind on a wide load. Neither of those is a format problem. If the whole ten times is reachable by fixing the reader and the parallelism over a format we already share, then a new format is a distraction and the honest thing is to say so. The counter argument is that the reader work has a ceiling at parity and the ten times needs the format, but that is an argument and not a measurement, and document 07 says how to settle it.

The second was measured, acted on, and is mostly gone. The size case turns on compressing a sorted dictionary, our encoder got 2.6 times on one where ZSTD got 5.33 over the identical bytes, and the whole gap was worth 1871 MiB. PR #576 added the match finder the cascade was missing and banked 1435 of it. What that episode says about the directory is worth keeping even though the item is closed: the first work item turned out to be in `rudb-encoding` rather than in a new file layout, and it was found by taking a falsification check rather than by designing.

The third is if the write cost of the global structures eats the read win. A global dictionary makes the read side better and the write side harder, and if a load ends up three times slower for a scan that ends up twice as fast, that is a bad trade for anybody who loads once and queries a few times. Document 04 is the answer to that and document 07 is the measurement.
