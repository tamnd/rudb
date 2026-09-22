# The native quadrant

## Why this document exists

Document 13 measured rudb against DuckDB with both engines reading the same Parquet file, and concluded that rudb loses the 43-query suite by 2.30x at 100,000,000 rows. It then used that measurement to write correction notes into documents 06, 08 and 09, and document 14 used it to state four obligations with the measurement that refutes each one.

Three of those refutations were wrong, and they were wrong for a reason worth recording rather than quietly fixing. Documents 06 and 09 specify mechanisms of the native format: stable global string codes, and late materialization over native stripes. A Parquet-to-Parquet measurement cannot test either of them, because when rudb reads Parquet it has no dictionary of its own to trust and no stripe of its own to defer. Document 13 measured those mechanisms switched off and reported that they had failed.

This document is the same suite at the same size with the mechanisms switched on.

## How it was measured

Same machine, same harness, same 43 unmodified query texts, same rule of three runs per query with the first discarded and the slower of the remaining two kept. The difference is the input: each engine loads the published `hits.parquet` into its own format through `CREATE TABLE`, `INSERT INTO ... SELECT`, `CHECKPOINT`, closes the process, and answers from the file it wrote.

The load runs under `SET memory_limit = '12GB'` and is gated on a row count. An earlier attempt at this measurement had DuckDB's load killed by the OOM killer partway through, leaving an empty table that answered all 43 queries in 12.52 seconds without erroring once. A suite total is not evidence that a suite ran. The harness now refuses to measure a database that does not hold 99,997,497 rows.

rudb's load took 1280.28 seconds of wall time and 5605.27 seconds of CPU, peaked at 16.99 GiB resident, and wrote 11,232,108,477 bytes, which is 0.76x the source Parquet.

## The result

| | rudb Parquet | rudb native |
| --- | ---: | ---: |
| 43-query suite | 468.21 s | 190.11 s |
| peak resident | 4.61 GiB | 4.21 GiB |

Reading its own format makes rudb 2.46x faster on the same queries on the same machine. For scale, DuckDB over Parquet took 203.12 seconds, so rudb native is already faster than the number document 13 reports rudb losing to. That is not the native-versus-native comparison and must not be read as one; DuckDB's native quadrant is a separate measurement and is not in this document.

## What it corrects

Per query, rudb native against rudb Parquet:

| Query | shape | Parquet | native | factor |
| --- | --- | ---: | ---: | ---: |
| Q35 | grouped count on a string key | 45.84 s | 0.45 s | 101.9x |
| Q6 | `COUNT(DISTINCT SearchPhrase)` | 16.65 s | 0.19 s | 87.6x |
| Q34 | grouped count on a string key | 37.71 s | 0.51 s | 73.9x |
| Q16 | grouped count on a string key | 7.03 s | 0.11 s | 63.9x |
| Q36 | grouped count on a string key | 6.28 s | 0.10 s | 62.8x |
| Q25 | top-N, one projected string | 7.85 s | 0.21 s | 37.4x |
| Q20 | top-N | 1.98 s | 0.14 s | 14.1x |
| Q24 | top-N | 14.15 s | 1.38 s | 10.3x |
| Q27 | top-N, one projected string | 4.91 s | 0.58 s | 8.5x |
| Q23 | per-row string work | 44.44 s | 5.39 s | 8.2x |
| Q28 | `AVG(STRLEN(URL))` | 23.28 s | 2.89 s | 8.1x |
| Q26 | top-N, one projected string | 5.63 s | 0.70 s | 8.0x |

Every mechanism document 13 declared refuted appears in that table. Q34 and Q35 are document 09's stable global string codes, quoted there as a tenfold win at 100,000 rows and written off in document 13 as a 2.72x and 3.85x loss at 100,000,000. In the format the document actually specifies they are 0.51 and 0.45 seconds. Q25 through Q27 are document 06's late materialization. Q6 is document 14's O4. Q28 is document 14's O2.

The correction notes in documents 06, 08 and 09 asserted that evidence for a native mechanism does not survive at scale. The evidence that does not survive at scale is Parquet's, and rudb reading Parquet is not the subject of any of those three documents.

## What it does not correct

Nine queries are slower in native than in Parquet, and they are not scattered:

| Query | key | Parquet | native |
| --- | --- | ---: | ---: |
| Q29 | `GROUP BY REGEXP_REPLACE(Referer, ...)` | 47.73 s | 60.79 s |
| Q33 | `GROUP BY WatchID, ClientIP` | 21.30 s | 34.87 s |
| Q10 | `RegionID, COUNT(DISTINCT UserID)` | 6.58 s | 10.37 s |
| Q9 | `RegionID, COUNT(DISTINCT UserID)` | 5.78 s | 9.32 s |
| Q31 | `GROUP BY SearchEngineID, ClientIP` | 4.79 s | 5.92 s |
| Q5 | `COUNT(DISTINCT UserID)` | 3.24 s | 4.42 s |

Every one of them builds a structure whose cardinality grows with the table: a hash table keyed on something close to unique per row, or a distinct set over `UserID`. That is document 14's O1 exactly, and the native format does not merely fail to help it. The native format makes it worse.

Adding Q19 and Q17, which are the same shape and which native improves but does not fix, the O1 queries account for 152.40 of the 190.11 second suite. Four fifths of rudb's native cost at benchmark scale sits in one mechanism.

## The conclusion this forces

Document 14 ranked four obligations by measured cost and put O1 first because it was the only superlinear failure. On the native measurement the ranking collapses into a single item. O2, O3 and O4 are satisfied by the format as specified, at factors between 8x and 102x, and the queries that tested them are now between 0.10 and 5.39 seconds each. They are not where the remaining time is.

O1 is not satisfied, is not improved by the format, and is the one obligation whose cost the format currently increases. Every other conclusion in documents 13 and 14 about where rudb's work goes at scale should be read as a statement about rudb over Parquet.
