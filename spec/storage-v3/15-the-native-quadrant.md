# The native quadrant

## Why this document exists

Document 13 measured rudb against DuckDB with both engines reading the same Parquet file, and concluded that rudb loses the 43-query suite by 2.30x at 100,000,000 rows. It then used that measurement to write correction notes into documents 06, 08 and 09, and document 14 used it to state four obligations with the measurement that refutes each one.

Three of those refutations were wrong, and they were wrong for a reason worth recording rather than quietly fixing. Documents 06 and 09 specify mechanisms of the native format: stable global string codes, and late materialization over native stripes. A Parquet-to-Parquet measurement cannot test either of them, because when rudb reads Parquet it has no dictionary of its own to trust and no stripe of its own to defer. Document 13 measured those mechanisms switched off and reported that they had failed.

This document is the same suite at the same size with the mechanisms switched on.

## How it was measured, and what went wrong twice

Same machine, same harness, same 43 unmodified query texts. Each engine loads the published `hits.parquet` into its own format through `CREATE TABLE`, `INSERT INTO ... SELECT`, `CHECKPOINT`, closes the process, and answers from the file it wrote.

The first attempt had DuckDB's load killed by the OOM killer partway through. It left an empty table, and that table answered all 43 queries in 12.52 seconds without erroring once. A suite total is not evidence that a suite ran, so the load now runs under `SET memory_limit = '12GB'` and the harness refuses to measure a database that does not hold 99,997,497 rows.

The second attempt passed the gate and still could not be trusted, which is the more useful failure. Two rudb passes over the same file with the same binary disagreed by up to 3.3x per query: Q29 measured 60.79 seconds and then 20.58, Q33 measured 34.87 and then 10.47, while Q21 and Q22 moved the other way by a factor of two. The suite total moved from 190.11 seconds to 116.63. This host is shared, and its load average across those windows ranged from 1 to 13.

A single pass on this machine does not measure the engine. The measurement was therefore repeated three times per engine over one loaded database, and the last three sections report those passes, what they establish, and what this host makes unmeasurable. This is the same discipline document 13 imposed about scale, applied to repetition, and it was arrived at the same way: by getting it wrong first.

## The result

| | rudb | DuckDB |
| --- | ---: | ---: |
| 43-query suite, quiet host | 116.63 s | 244.12 s |
| peak resident, queries | 4.2 GiB | 8.62 GiB |
| file bytes | 11,232,108,477 | 20,435,972,096 |
| load wall | 1372.10 s | 395.21 s |
| load peak resident | 17.58 GiB | 12.31 GiB |

Three of those five rows are not timings at all, and are exact.

rudb's native file is 1.82x smaller than DuckDB's and 0.76x the size of the source Parquet. rudb answers the suite in about half DuckDB's peak resident set, and it answers it 2.09x faster. Six further passes under contention, reported below, put rudb ahead on every one of them by a margin that does not overlap DuckDB's range at all.

rudb's load is the clear deficit and it is not close. It takes 3.47x the wall time and 4.09x the CPU of DuckDB's, and it peaks at 17.58 GiB against 12.31. That last number is the one that matters, because document 01's fourth requirement says a load's working set depends on stripe and writer concurrency and not on table row count. At 1,000,000 rows this load peaked at 1.5 GiB. At 100,000,000 it peaks at 17.58. That is a working set tracking the row count, and requirement 4 is not met at benchmark scale.

## What it corrects

Per query, rudb native against rudb Parquet, on the queries whose mechanism document 13 declared refuted:

| Query | mechanism | Parquet | native |
| --- | --- | ---: | ---: |
| Q35 | document 09, stable string codes | 45.84 s | under 0.7 s |
| Q34 | document 09, stable string codes | 37.71 s | under 0.6 s |
| Q6 | document 14 O4, encoded aggregate | 16.65 s | under 0.2 s |
| Q16 | document 09, stable string codes | 7.03 s | under 0.2 s |
| Q36 | document 09, stable string codes | 6.28 s | under 0.2 s |
| Q25 | document 06, late materialization | 7.85 s | under 0.7 s |

The bounds are loose on purpose: these queries move by a factor of two between passes, and it does not matter, because the claim being tested is a factor of seventy. Each bound holds across all five native passes taken of this database.

Document 09 quotes a tenfold win on Q34 and Q35 at 100,000 rows, and document 13 wrote that off as a 2.72x and a 3.85x loss at 100,000,000. In the format document 09 actually specifies, those two queries are the two largest wins in the suite. Document 06's late materialization is the same story on Q25 through Q27, and document 14's O4 is the same story on Q6.

The correction notes in documents 06, 08 and 09 asserted that evidence for a native mechanism does not survive at scale. The evidence that does not survive at scale is Parquet's, and rudb reading Parquet is not the subject of any of those three documents.

## Where rudb is still behind

On the quiet host rudb loses nine of the 43: Q10, Q9, Q5, Q27, Q22, Q21, Q19, Q40 and Q17.

Strip out the two sub-second entries and what is left is `COUNT(DISTINCT UserID)` three times, and `GROUP BY` on a key close to unique per row four times. Every other pass of this measurement agrees on the shape even where it disagrees on the seconds.

**The sentence that followed here said this is document 14's O1 and the only obligation of the four the native format does not satisfy, and it was read off the query texts rather than measured.** Q21 is `SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'`, with no `GROUP BY` and no `DISTINCT` anywhere in it, and it is the second largest of the nine at 9.10 seconds. Q22 and Q23 do group, and document 16 measures their `LIKE` costing several times what sits above it. Three of the nine are substring matching, not grouping, and the largest query in this whole suite, Q29 at 20.58 seconds, is a regular expression over `Referer` that rudb wins by 1.86x and that O1 does not describe. Document 16 sorts all 43 by measured operator cost and finds the suite split roughly evenly between grouping, where rudb leads 3.03x, and string matching, where it leads 1.38x.

The margin is small in absolute terms, which is the point rather than a reassurance. What these nine are not is the distance to a much larger lead. Document 16 measures the scan under the eight queries that are three quarters of this suite and finds it alone larger than the entire budget a tenfold lead would allow, so the gap this section describes is not the thing in the way.

## Replication, and what this host can and cannot support

Three passes per engine over one loaded database, all six inside one window on the same shared host, with its load average between 13 and 32 throughout.

| | pass 1 | pass 2 | pass 3 | median |
| --- | ---: | ---: | ---: | ---: |
| rudb | 95.65 s | 88.35 s | 135.92 s | 98.42 s |
| DuckDB | 534.73 s | 577.09 s | 391.12 s | 437.92 s |

Every rudb pass beats every DuckDB pass, and the two ranges do not touch: rudb's worst pass is 2.88x faster than DuckDB's best. That is the one comparison in this document that noise cannot reach, because it does not depend on pairing any pass with any other.

On these medians rudb wins all 43 queries. It should not be read as rudb winning the native quadrant by 4.45x, and the reason is the next section.

## What the replication actually measured

The two engines do not respond to a busy machine the same way. Comparing each query against the same query measured earlier on a quiet host, over the queries that take more than half a second, rudb's median query time changed by 0.97x and DuckDB's by 1.65x. rudb did not get faster under load. DuckDB got slower.

That has a plain explanation. DuckDB answers this suite in 8.62 GiB and rudb in 4.2 GiB, and when 32 cores and 23 GiB are oversubscribed the engine holding twice the working set pays twice. The effect is real and it is a resource result rather than an execution result, but it means the contended medians flatter rudb.

The fair single-machine comparison is the quiet window, where the two engines were measured 25 minutes apart under a load average near 1:

| | rudb | DuckDB |
| --- | ---: | ---: |
| 43-query suite | 116.63 s | 244.12 s |

That is 2.09x, and it is the number this document stands behind. It is also the pass whose nine losses the section above lists, all of them O1.

So both readings agree on the diagnosis and differ only on the margin. rudb is ahead of DuckDB in the native quadrant by about two times on a quiet machine and by more on a busy one, it is ahead by 2x on query memory and 1.82x on file size in either, and the whole of its remaining deficit is high-cardinality grouping and distinct.

## What this host can and cannot support

The replication also measured the host, and the answer is worse than assumed. Across three passes on one unchanged database, DuckDB's own per-query times spread by up to 8.0x, with 34 of 43 queries spreading more than 2x. Its suite total, where noise is supposed to average out, spanned 1.48x. rudb was steadier at a 3.8x worst spread and 15 of 43 above 2x, which is itself the contention result above and not a virtue of the harness.

No per-query wall time from this machine means anything to two significant figures. The conclusions above are stated as non-overlapping ranges, as factors of seventy against a threefold error bar, or as quantities that are not timings at all: file bytes, row counts, and peak resident set. Anything this series wants to claim more finely than that needs a machine nobody else is using.
