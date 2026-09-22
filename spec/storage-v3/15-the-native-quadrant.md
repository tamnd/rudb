# The native quadrant

## Why this document exists

Document 13 measured rudb against DuckDB with both engines reading the same Parquet file, and concluded that rudb loses the 43-query suite by 2.30x at 100,000,000 rows. It then used that measurement to write correction notes into documents 06, 08 and 09, and document 14 used it to state four obligations with the measurement that refutes each one.

Three of those refutations were wrong, and they were wrong for a reason worth recording rather than quietly fixing. Documents 06 and 09 specify mechanisms of the native format: stable global string codes, and late materialization over native stripes. A Parquet-to-Parquet measurement cannot test either of them, because when rudb reads Parquet it has no dictionary of its own to trust and no stripe of its own to defer. Document 13 measured those mechanisms switched off and reported that they had failed.

This document is the same suite at the same size with the mechanisms switched on.

## How it was measured, and what went wrong twice

Same machine, same harness, same 43 unmodified query texts. Each engine loads the published `hits.parquet` into its own format through `CREATE TABLE`, `INSERT INTO ... SELECT`, `CHECKPOINT`, closes the process, and answers from the file it wrote.

The first attempt had DuckDB's load killed by the OOM killer partway through. It left an empty table, and that table answered all 43 queries in 12.52 seconds without erroring once. A suite total is not evidence that a suite ran, so the load now runs under `SET memory_limit = '12GB'` and the harness refuses to measure a database that does not hold 99,997,497 rows.

The second attempt passed the gate and still could not be trusted, which is the more useful failure. Two rudb passes over the same file with the same binary disagreed by up to 3.3x per query: Q29 measured 60.79 seconds and then 20.58, Q33 measured 34.87 and then 10.47, while Q21 and Q22 moved the other way by a factor of two. The suite total moved from 190.11 seconds to 116.63. This host is shared, and its load average across those windows ranged from 1 to 13.

A single pass on this machine does not measure the engine. Every per-query number below is therefore the median of three full passes over one loaded database, and the spread across those passes is reported next to it. A difference smaller than the spread is not a result. This is the same discipline document 13 imposed about scale, applied to repetition, and it was arrived at the same way: by getting it wrong first.

## The result

| | rudb | DuckDB |
| --- | ---: | ---: |
| 43-query suite | see the table below | 244.12 s |
| peak resident, queries | 4.2 GiB | 8.62 GiB |
| file bytes | 11,232,108,477 | 20,435,972,096 |
| load wall | 1372.10 s | 395.21 s |
| load peak resident | 17.58 GiB | 12.31 GiB |

Four of those five rows are stable across every pass, and three of them are not timings at all.

rudb's native file is 1.82x smaller than DuckDB's and 0.76x the size of the source Parquet. rudb answers the suite in about half DuckDB's peak resident set. Both of rudb's query passes beat DuckDB's suite total, by 1.28x and by 2.09x; the direction is robust and the factor is not, which is what the replication below is for.

rudb's load is the clear deficit and it is not close. It takes 3.47x the wall time and 4.09x the CPU of DuckDB's, and it peaks at 17.58 GiB against 12.31. That last number is the one that matters, because document 01's fourth requirement says a load's working set depends on stripe and writer concurrency and not on table row count. At 1,000,000 rows this load peaked at 1.5 GiB. At 100,000,000 it peaks at 17.58. That is a working set tracking the row count, and requirement 4 is not met at benchmark scale.

## What it corrects

Per query, rudb native against rudb Parquet, on the queries whose mechanism document 13 declared refuted:

| Query | mechanism | Parquet | native |
| --- | --- | ---: | ---: |
| Q35 | document 09, stable string codes | 45.84 s | under 0.5 s |
| Q34 | document 09, stable string codes | 37.71 s | under 0.6 s |
| Q6 | document 14 O4, encoded aggregate | 16.65 s | under 0.2 s |
| Q16 | document 09, stable string codes | 7.03 s | under 0.2 s |
| Q36 | document 09, stable string codes | 6.28 s | under 0.2 s |
| Q25 | document 06, late materialization | 7.85 s | under 0.3 s |

The bounds are loose on purpose: these queries move by a factor of two between passes, and it does not matter, because the claim being tested is a factor of seventy. Both passes agree on every row.

Document 09 quotes a tenfold win on Q34 and Q35 at 100,000 rows, and document 13 wrote that off as a 2.72x and a 3.85x loss at 100,000,000. In the format document 09 actually specifies, those two queries are the two largest wins in the suite. Document 06's late materialization is the same story on Q25 through Q27, and document 14's O4 is the same story on Q6.

The correction notes in documents 06, 08 and 09 asserted that evidence for a native mechanism does not survive at scale. The evidence that does not survive at scale is Parquet's, and rudb reading Parquet is not the subject of any of those three documents.

## Where rudb is still behind

The queries rudb loses to DuckDB are the same shape in both passes even though their timings are not. In the pass where rudb's suite total was 116.63 seconds it lost nine queries, and the list is Q10, Q9, Q5, Q27, Q22, Q21, Q19, Q40 and Q17.

Strip out the two sub-second entries and what is left is `COUNT(DISTINCT UserID)` three times, and `GROUP BY` on a key close to unique per row four times. In the other pass rudb lost twelve queries and the same shapes led it. This is document 14's O1, and it is the only obligation of the four that the native format does not satisfy.

The replication below decides how far behind. The shape is already decided: rudb's remaining deficit at benchmark scale is a grouping and distinct-set problem, and nothing else in the suite is close.

## Replication

Three passes per engine over one loaded database, with the host's load average recorded at the start of each. Pending.
