# The forward run

## Why this document exists

Document 34 measured the mirror and named what stood between the Parquet quadrant and the target. This document runs the suite again at that point, reads the result query by query against the two axes the target names, processor time and resident memory, and lists what is left.

## How it was run

server3, `/root/cb/pq/run.sh`, the same script, view and ten million row file as document 34. Each query runs in a fresh process, once to warm and once timed, and the engines are interleaved. rudb is main at c56b83bb with the mirror already built. The run is tagged `forward` and its lines are in `forward.txt`.

The host was busier than it was for document 34. DuckDB's timed runs summed to 157.12 seconds of wall time here against 85.91 there, on the same binary and the same file. Wall time on this run says more about the neighbours than about either engine, so this document reads processor time, user plus system, as the time axis. Resident memory is the peak the kernel reports for the process and does not depend on the host.

## What the queries cost

rudb's timed runs used 64.66 seconds of processor time against 145.34 for DuckDB, and 70.94 seconds of wall time against 157.12. In the table, the time columns are processor seconds and the ratio is DuckDB over rudb. The memory columns are peak resident MiB, with rudb's figure from document 34 beside the one from this run.

| query | rudb cpu s | duckdb cpu s | time | rudb MiB before | rudb MiB | duckdb MiB | memory |
|---|---|---|---|---|---|---|---|
| q1 | 0.14 | 0.41 | 2.9x | 15 | 15 | 44 | 2.9x |
| q2 | 0.04 | 0.49 | 12.2x | 21 | 15 | 47 | 3.1x |
| q3 | 0.05 | 0.44 | 8.8x | 28 | 14 | 53 | 3.6x |
| q4 | 0.03 | 0.77 | 25.7x | 32 | 15 | 71 | 4.7x |
| q5 | 0.04 | 1.00 | 25.0x | 144 | 15 | 120 | 8.0x |
| q6 | 0.04 | 2.42 | 60.5x | 70 | 15 | 256 | 17.1x |
| q7 | 0.03 | 0.42 | 14.0x | 25 | 15 | 45 | 3.0x |
| q8 | 0.11 | 0.33 | 3.0x | 23 | 16 | 49 | 3.1x |
| q9 | 1.67 | 1.61 | 1.0x | 243 | 288 | 144 | 0.5x |
| q10 | 2.28 | 2.29 | 1.0x | 252 | 292 | 151 | 0.5x |
| q11 | 0.41 | 1.05 | 2.6x | 47 | 48 | 96 | 2.0x |
| q12 | 0.40 | 0.88 | 2.2x | 49 | 50 | 102 | 2.0x |
| q13 | 0.14 | 2.24 | 16.0x | 56 | 20 | 275 | 13.4x |
| q14 | 0.79 | 2.99 | 3.8x | 95 | 101 | 335 | 3.3x |
| q15 | 0.64 | 2.41 | 3.8x | 108 | 107 | 278 | 2.6x |
| q16 | 0.04 | 1.12 | 28.0x | 160 | 15 | 133 | 8.5x |
| q17 | 2.32 | 3.51 | 1.5x | 350 | 436 | 320 | 0.7x |
| q18 | 0.48 | 2.67 | 5.6x | 44 | 47 | 350 | 7.4x |
| q19 | 12.12 | 7.49 | 0.6x | 361 | 479 | 556 | 1.2x |
| q20 | 0.05 | 0.48 | 9.6x | 18 | 18 | 61 | 3.4x |
| q21 | 4.23 | 5.14 | 1.2x | 343 | 345 | 641 | 1.9x |
| q22 | 3.33 | 4.33 | 1.3x | 367 | 396 | 555 | 1.4x |
| q23 | 3.90 | 9.81 | 2.5x | 702 | 466 | 1456 | 3.1x |
| q24 | 3.07 | 6.48 | 2.1x | 386 | 405 | 795 | 2.0x |
| q25 | 1.44 | 1.71 | 1.2x | 49 | 54 | 253 | 4.6x |
| q26 | 0.50 | 1.46 | 2.9x | 53 | 53 | 128 | 2.4x |
| q27 | 1.59 | 2.48 | 1.6x | 56 | 63 | 151 | 2.4x |
| q28 | 1.19 | 4.14 | 3.5x | 57 | 59 | 414 | 7.0x |
| q29 | 13.12 | 36.30 | 2.8x | 673 | 713 | 776 | 1.1x |
| q30 | 0.15 | 0.47 | 3.1x | 27 | 26 | 62 | 2.3x |
| q31 | 0.96 | 2.13 | 2.2x | 90 | 93 | 209 | 2.2x |
| q32 | 1.06 | 3.36 | 3.2x | 118 | 126 | 368 | 2.9x |
| q33 | 4.27 | 8.24 | 1.9x | 316 | 375 | 919 | 2.5x |
| q34 | 0.24 | 9.78 | 40.8x | 138 | 28 | 991 | 34.5x |
| q35 | 0.15 | 7.61 | 50.7x | 134 | 28 | 984 | 34.2x |
| q36 | 0.06 | 1.56 | 26.0x | 131 | 16 | 120 | 7.5x |
| q37 | 0.28 | 0.89 | 3.2x | 63 | 63 | 188 | 3.0x |
| q38 | 0.21 | 0.49 | 2.3x | 58 | 42 | 66 | 1.6x |
| q39 | 0.15 | 0.86 | 5.7x | 59 | 49 | 170 | 3.4x |
| q40 | 1.76 | 1.76 | 1.0x | 179 | 179 | 346 | 1.9x |
| q41 | 0.26 | 0.46 | 1.8x | 55 | 47 | 71 | 1.5x |
| q42 | 0.18 | 0.38 | 2.1x | 52 | 42 | 65 | 1.5x |
| q43 | 0.74 | 0.48 | 0.6x | 40 | 39 | 62 | 1.6x |

Four queries clear both axes by ten times or more: q6, q13, q34 and q35. Five more clear time and miss memory by a small margin: q5 at 8.0x, q16 at 8.5x, q36 at 7.5x, and q18 and q28 at about 7x with time at 5.6x and 3.5x. The short queries that answer from the mirror's statistics and small columns, q2 through q7 and q20, are well ahead on time. Their memory ratio is capped by DuckDB's own floor of about 45 MiB against rudb's 15, which is 3x and no more.

## Where rudb loses

Six queries are at or below even on at least one axis.

q9 and q10 group by region and count distinct users. They ran at even time and at half of DuckDB's memory, 288 and 292 MiB against 144 and 151. The pairs they scatter into partitions were kept until the end of the input, one per row, even when most of them repeated. Pull request 1458 compacts a partition run in place when it fills, which on the local ten million row copy brings q9's peak well under DuckDB's.

q17 groups by user and search phrase with no limit on the groups kept, and peaked at 436 MiB against 320. It is the next memory target.

q19 extracts the minute from the event time and was 0.6x on time, 12.12 processor seconds against 7.49. The cost was the view's conversion, `TIMESTAMP 0 + to_seconds(EventTime::DOUBLE)`, which went through the row at a time path for both the interval and the addition. Pull request 1457 gives both a vector loop.

q43 groups by the minute of the event time and loses on time for the same reason, 0.74 against 0.48. q40 is even on time at 1.76 seconds each; it filters on a referer hash and a URL hash, and the filter is where its time goes.

Neither 1457 nor 1458 is in this run. The next run will carry both.

## Whether the answers agree

Every answer was compared after normalising rudb's box output to DuckDB's. Thirty four are the same text. q4 differs only in how a large average is printed, 2.5131007489380997e+18 on both sides with different formatting. q18 has no ORDER BY, so any ten groups are a correct answer. q22, q24, q32, q33, q39, q40 and q41 end in ORDER BY with a LIMIT that cuts through a run of equal keys: every row of q32 and q33 has a count of one, every row of q40 has thirteen page views, and q24's ten event times are the same on both sides. The rows chosen from inside a tie differ, which the SQL allows.

## What that leaves

The totals are 2.2x on processor time and the memory ratio is below 10x on thirty nine of forty three queries. The target is not near on the suite as a whole, and the work that follows is ordered by what this run found.

1. Rerun with 1457 and 1458 and confirm q9, q10, q19 and q43 move.
2. q17's grouped memory.
3. The decoded text blocks the native reader keeps across statements, up to 256 MiB per column. In a process that runs one query they are never read again, and on a URL LIKE over the local ten million rows they left rudb at 400 MiB against DuckDB's 510 while the query itself needs far less.
4. The memory floor per worker thread. A scan over the local copy peaks at 22 MiB on one thread and 54 on ten.
5. The mirror load, which is still 38 seconds of wall and 233 of processor for ten million rows on a quiet laptop.
