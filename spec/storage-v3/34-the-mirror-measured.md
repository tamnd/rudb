# The mirror measured

## Why this document exists

Document 33 moved the Parquet quadrant's decode out of the query and into one load per file, and said the numbers would come in the document after the implementation. This is that document. It measures all forty three queries on both engines over the same ten million row Parquet file the baseline was taken on, and it finds the next thing standing between the quadrant and the target, which is not the mirror.

## How it was run

server3, `/root/cb/pq/run.sh`, the same script and the same file as the baseline in `base.txt`: the contract's view, with its four time conversions and `binary_as_string=True`, over `hits-10m.parquet` at 1.41 GB. Each query runs in a fresh process, once to warm and once timed, and the two engines are interleaved query by query so that both see the same host. The host was not quiet. Its load average ran between 13 and 22 on eight cores through the run, so every ratio here is between two neighbours and no absolute time is worth more than its neighbour.

rudb is main at the merge of document 33, with `RUDB_MIRROR_DIR` pointed at a directory the mirror was built into once, before the run, by a single query.

## What the load cost

The mirror of the ten million row file is 1,122,546,863 bytes, 0.80 of the Parquet file. Building it took 358.8 seconds of wall time, 653 seconds of user processor and 111 of system, and 1.97 GiB peak resident. The query that asked for it then answered in the same process. The next process counted the rows in 0.16 seconds and 14.3 MiB.

The load is the weak number. Document 8 measured a native load of one million rows at 2.36 seconds, and ten times that is not 359. Some of the difference is the host, but not a factor of fifteen. The mirror's break even count is dominated by it, and the load is where the next measurement of the write side has to start.

## What the queries cost

The suite's timed runs sum to 44.56 seconds for rudb against 85.91 for DuckDB. The baseline, reading the file on every query, was 82.60 against 83.87. So the mirror took the quadrant from even to about twice as fast in total, which is what document 33 said it would do and a long way short of what document 33 hoped.

| query | rudb s | duckdb s | time | rudb MiB | duckdb MiB | memory |
|---|---|---|---|---|---|---|
| q1 | 0.20 | 0.67 | 3.4x | 16 | 44 | 2.8x |
| q2 | 0.43 | 1.74 | 4.0x | 21 | 47 | 2.2x |
| q3 | 0.38 | 0.94 | 2.5x | 28 | 54 | 1.9x |
| q4 | 0.51 | 1.38 | 2.7x | 33 | 71 | 2.1x |
| q5 | 0.64 | 1.08 | 1.7x | 144 | 112 | 0.8x |
| q6 | 2.18 | 1.98 | 0.9x | 71 | 230 | 3.3x |
| q7 | 1.38 | 0.55 | 0.4x | 26 | 47 | 1.8x |
| q8 | 0.24 | 0.80 | 3.3x | 23 | 50 | 2.1x |
| q9 | 1.08 | 1.38 | 1.3x | 243 | 147 | 0.6x |
| q10 | 1.11 | 2.64 | 2.4x | 253 | 154 | 0.6x |
| q11 | 0.72 | 1.04 | 1.4x | 48 | 93 | 2.0x |
| q12 | 0.42 | 0.70 | 1.7x | 50 | 97 | 1.9x |
| q13 | 0.41 | 1.59 | 3.9x | 57 | 267 | 4.7x |
| q14 | 0.56 | 2.87 | 5.1x | 96 | 438 | 4.6x |
| q15 | 0.52 | 1.85 | 3.6x | 109 | 269 | 2.5x |
| q16 | 1.09 | 1.05 | 1.0x | 160 | 135 | 0.8x |
| q17 | 1.66 | 2.59 | 1.6x | 350 | 328 | 0.9x |
| q18 | 0.53 | 1.86 | 3.5x | 45 | 320 | 7.2x |
| q19 | 5.77 | 3.41 | 0.6x | 362 | 568 | 1.6x |
| q20 | 0.09 | 0.58 | 6.4x | 18 | 61 | 3.3x |
| q21 | 1.99 | 2.00 | 1.0x | 343 | 461 | 1.3x |
| q22 | 1.13 | 1.84 | 1.6x | 368 | 699 | 1.9x |
| q23 | 1.74 | 4.01 | 2.3x | 703 | 1427 | 2.0x |
| q24 | 1.87 | 2.83 | 1.5x | 387 | 667 | 1.7x |
| q25 | 0.61 | 1.62 | 2.7x | 49 | 167 | 3.4x |
| q26 | 0.48 | 0.69 | 1.4x | 53 | 130 | 2.4x |
| q27 | 0.67 | 1.52 | 2.3x | 56 | 195 | 3.5x |
| q28 | 0.35 | 1.38 | 3.9x | 57 | 426 | 7.5x |
| q29 | 5.40 | 18.22 | 3.4x | 673 | 739 | 1.1x |
| q30 | 0.29 | 1.62 | 5.6x | 27 | 63 | 2.3x |
| q31 | 1.00 | 1.89 | 1.9x | 90 | 224 | 2.5x |
| q32 | 0.85 | 2.17 | 2.6x | 119 | 283 | 2.4x |
| q33 | 2.51 | 3.74 | 1.5x | 316 | 794 | 2.5x |
| q34 | 0.94 | 2.77 | 2.9x | 138 | 1081 | 7.8x |
| q35 | 0.27 | 3.86 | 14.3x | 134 | 992 | 7.4x |
| q36 | 1.07 | 0.75 | 0.7x | 131 | 130 | 1.0x |
| q37 | 0.26 | 0.71 | 2.7x | 64 | 191 | 3.0x |
| q38 | 0.57 | 0.34 | 0.6x | 59 | 69 | 1.2x |
| q39 | 0.22 | 0.77 | 3.5x | 60 | 169 | 2.8x |
| q40 | 1.12 | 0.96 | 0.9x | 179 | 334 | 1.9x |
| q41 | 0.35 | 0.52 | 1.5x | 55 | 72 | 1.3x |
| q42 | 0.23 | 0.43 | 1.9x | 52 | 64 | 1.2x |
| q43 | 0.72 | 0.57 | 0.8x | 40 | 62 | 1.5x |

No query clears both axes. q35 clears time at 14.3 and misses memory at 7.4. Four of the seven queries that clear the native quadrant, q5, q16, q34 and q36, sit between 0.7 and 2.9 times on time here, and that is the finding.

## Why the native wins did not carry over

Document 33 said every query that clears the native quadrant would clear this one by a wider margin. It does not, and the reason is the view, not the mirror. On the same mirror file, q5 opened as a table answers in 0.09 seconds and 10 MiB. Named through `read_parquet` directly, so that the binder substitutes the mirror, it answers in 0.10 seconds and 14 MiB. Named through the contract's view it takes 0.64 seconds and 139 MiB, and through a view that is nothing but `SELECT *` it takes the same.

A view expands inline, and column pruning narrows its projection to the columns the query reads, which for q5 is `[#0.0 AS UserID]`. That projection computes nothing and it still stands between the aggregate and the scan. Every rule that answers from what the native file stores, the exact distinct counts, the frequency synopses, the zone decisions, looks for its operator directly over the scan and finds a projection instead. The same holds on the mirror file with no Parquet anywhere: `SELECT COUNT(*) FROM (SELECT * FROM mirror) WHERE AdvEngineID <> 0` takes 0.31 seconds where the query without the subquery takes 0.07.

So the Parquet quadrant is not bounded by the native quadrant yet. It is bounded by the native quadrant as seen through one projection, and that projection is the next piece of work: take out an interior projection that only forwards columns, so that the rules see the scan. It is general, since any view over a table has the same shape.

## What that leaves

A projection that converts a column is not a forwarding one, and the contract's view converts four. The queries that read `EventDate` or `EventTime`, q7, q19, q24 and q37 to q43, keep a computing projection after the forwarding ones go. Their filters are ranges over `DATE '1970-01-01' + EventDate`, which is monotone in the stored column, so a zone decision over the stored value answers them, and making the optimizer see that is the second piece.

The heavy group bys, q9, q10, q17, q19, q21 to q24 and q29 and q33, are at or near one on memory here as they are in the native quadrant. They share that list with it, as document 33 said they would.

## What this does not claim

It does not claim precise times. The host was loaded and the ratios are between neighbours.

It does not claim the load is acceptable. It claims the load was measured and is fifteen times what the earlier rate predicts.

It does not claim that removing the forwarding projection clears any query. It claims that the queries measured through it lose what they have without it, by the factors above.
