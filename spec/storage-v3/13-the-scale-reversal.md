# The scale reversal

## What this document is

Every design document in this series before it carries an evidence section, and every one of
those evidence sections was measured at 100,000 or 1,000,000 rows. The benchmark the project is
judged on has 100,000,000. This document is the measurement at both ends of that gap, run through
one harness on one machine, and it reports that the conclusion reverses.

It is filed as evidence rather than as a design because nothing here proposes a mechanism. It
establishes what is true, which is the thing document 14 then has to explain.

## How it was measured

The source is the published ClickBench `hits.parquet`, 14,779,976,446 bytes, and its one million
row prefix. Both engines read the same file through a view with the loader conversion ClickBench
itself applies, so the 43 query texts are unmodified. Both engines therefore run against Parquet,
not against their own formats: the comparison isolates execution from storage, and neither side
gets the advantage of a file it wrote.

One process per run, three runs per query, the first discarded as cold, the slower of the
remaining two kept. A query that fails is recorded as a failure rather than dropped. No query
failed at either size on either engine. The machine is a 32-core host with 23 GiB of memory; the
engines are rudb at `d41b8a24` and DuckDB `v2.0.0-dev84237`.

## The result

| | rudb | DuckDB | standing |
| --- | ---: | ---: | --- |
| 1,000,000 rows | 16.83 s | 18.65 s | rudb ahead by 1.11x |
| 100,000,000 rows | 468.21 s | 203.12 s | rudb behind by 2.30x |

One hundred times the data costs rudb 27.8x and DuckDB 10.9x. Both are sublinear because a
one-million-row query is mostly process startup, which is exactly why the small measurement cannot
be extrapolated: at that size the harness is measuring the floor, not the engine.

The middle of the ladder agrees. Document 01 records a ten-million-row audit at 5.765 seconds of
rudb Parquet against 4.108 seconds of DuckDB Parquet, which is rudb behind by 1.40x. Placed between
the two measurements here that gives 1.11x ahead, 1.40x behind, 2.30x behind, monotone across two
decades. That audit summed in-process query timers rather than child process wall time, so it is
corroboration of the trend and not a fourth point on the same curve.

Peak resident set moves the other way. Across the 43 queries the largest rudb process held 4.61 GB
and the largest DuckDB process held 6.77 GB, so rudb uses 1.47x less memory at the same time as it
spends 2.30x more of it. rudb is not losing because it ran out of room. On the five queries where
it is furthest behind on time it is also using between eight and twelve times less memory than
DuckDB. That combination — less memory, more time, same answer — is the shape of an engine that is
re-deriving something instead of holding it, and it is the single most useful fact in this
document.

## What it falsifies

Three decisions in this series were accepted on measurements that do not survive.

Document 09 records Q34 at 1.56 ms against DuckDB's 16.00 ms and Q35 at 1.52 ms against 19.00 ms,
measured at 100,000 rows: a tenfold win, and the stated justification for stable global string
codes. At 100,000,000 rows Q34 is 0.37x and Q35 is 0.26x, both losses. The mechanism may still be
right. The evidence offered for it is not evidence at the scale that decides it.

Document 06 records Q24 falling from 76.3 ms to 20.5 ms against DuckDB's 43.0 ms at 1,000,000 rows,
a 2.1x win, as the result that justifies native late materialization. At 100,000,000 rows Q24 is
0.44x. The rewrite still fires; it stops being sufficient.

Document 08 records Q34 falling from 60.5 ms to 47.8 ms at 1,000,000 rows against DuckDB's 26.0 ms.
That one is honest about being a loss, and the loss widens with scale rather than closing.

One decision survives, and the reason it survives is the important part. Document 05's persisted
zone maps were measured on Q37 through Q40 at 1,000,000 rows. At 100,000,000 rows Q41, Q42 and Q43
are rudb wins of 3.24x, 1.68x and 2.92x, and Q37 and Q40 are the mildest losses in their shape
class. Zone maps are the one mechanism in this series whose benefit is defined as *work not done*
rather than as a smaller constant on work still done, and it is the one mechanism whose advantage
grows with the data instead of evaporating.

## The four shapes

Sorting all 43 queries by how much rudb's standing moved between the two sizes separates them into
four groups with almost no overlap. The drift column is rudb's growth factor divided by DuckDB's;
below 1.0 means rudb degraded faster.

**Shape A, grouped aggregation whose group count grows with the table.** Q19, Q33, Q35, Q34, Q14,
Q23, Q22, Q32, Q31, Q16, Q13. rudb degrades 2 to 9 times faster than DuckDB. Q19 is the
extreme and the clearest case in the suite: its key is `(UserID, minute(EventTime), SearchPhrase)`,
which is close to unique per row, so the group count rises with the table. One hundred times the
rows costs rudb 117.4x and DuckDB 13.4x. That is superlinear against a linear baseline, on a query
whose input grew exactly linearly.

**Shape B, top-N with a small limit.** Q25, Q26, Q27, Q24, Q20. rudb degrades 3 to 7.5 times
faster. These queries emit ten rows. Q25 is `SELECT SearchPhrase FROM hits WHERE SearchPhrase <> ''
ORDER BY EventTime LIMIT 10`; at one million rows rudb won it by 2.58x, at one hundred million it
loses by 2.86x, having grown 65.4x against DuckDB's 8.9x. The answer did not get bigger. Only the
input did.

**Shape C, per-row work on a string column.** Q28, Q22, Q23, Q21, Q6. rudb degrades 1.5 to 5.3
times faster. Q28 is the cleanest instance because it has no `LIKE` and no large group count: it is
`AVG(STRLEN(URL)) GROUP BY CounterID`, a few thousand groups, and the only thing that scales is
that 100,000,000 URLs have to yield their lengths. rudb grew 40.1x and DuckDB 7.5x, and rudb did it
in 128 MB against DuckDB's 1,257 MB. The length of a dictionary-encoded string is a property of the
dictionary entry, not of the row.

**Shape D, everything else.** Q1 through Q4, Q8, Q11, Q12, Q15, Q18, Q29, Q30, Q37 through Q43.
Drift between 0.4x and 3.2x with no pattern, and the queries rudb still wins at scale are all here.
This group needs no explanation beyond noise and fixed costs.

## The drift table

Worst first. `ratio` is DuckDB wall over rudb wall, so above 1.00 means rudb is ahead. `growth` is
each engine's own wall at 100,000,000 rows over its wall at 1,000,000.

| Query | ratio @1m | ratio @100m | rudb growth | DuckDB growth | drift |
| --- | ---: | ---: | ---: | ---: | ---: |
| Q19 | 1.82 | 0.21 | 117.4 | 13.4 | 0.11 |
| Q26 | 2.46 | 0.33 | 43.3 | 5.8 | 0.13 |
| Q25 | 2.58 | 0.35 | 65.4 | 8.9 | 0.14 |
| Q20 | 2.00 | 0.29 | 28.3 | 4.1 | 0.15 |
| Q27 | 2.91 | 0.52 | 44.6 | 7.9 | 0.18 |
| Q28 | 1.03 | 0.19 | 40.1 | 7.5 | 0.19 |
| Q22 | 0.97 | 0.22 | 32.4 | 7.5 | 0.23 |
| Q23 | 0.75 | 0.18 | 47.8 | 11.3 | 0.24 |
| Q35 | 1.10 | 0.26 | 55.9 | 13.2 | 0.24 |
| Q36 | 1.19 | 0.31 | 24.2 | 6.3 | 0.26 |
| Q33 | 2.63 | 0.75 | 78.9 | 22.4 | 0.28 |
| Q14 | 0.84 | 0.26 | 53.6 | 16.3 | 0.30 |
| Q24 | 1.43 | 0.44 | 10.6 | 3.3 | 0.31 |
| Q30 | 3.60 | 1.13 | 4.7 | 1.5 | 0.31 |
| Q42 | 5.20 | 1.68 | 6.2 | 2.0 | 0.32 |
| Q34 | 1.05 | 0.37 | 33.7 | 11.8 | 0.35 |
| Q31 | 2.53 | 0.96 | 25.2 | 9.6 | 0.38 |
| Q32 | 2.05 | 0.89 | 31.4 | 13.7 | 0.44 |
| Q21 | 0.68 | 0.35 | 25.3 | 12.8 | 0.51 |
| Q13 | 0.74 | 0.39 | 24.9 | 12.9 | 0.52 |
| Q16 | 0.58 | 0.33 | 14.6 | 8.3 | 0.57 |
| Q6 | 0.29 | 0.20 | 19.8 | 13.6 | 0.69 |

Thirty-four of the forty-three drift against rudb. The nine that drift towards it are Q2, Q4, Q8,
Q15, Q17, Q18, Q39, Q41 and Q43, and with the exception of Q18 they are small queries where the
1,000,000-row number was mostly startup.

## The rule this imposes

No decision in this series is accepted on evidence measured below the scale of the benchmark it
claims to serve. A mechanism measured at 1,000,000 rows may be reported, and its number may be
encouraging, but it does not close a design question. Three of the twelve documents here closed one
on a measurement that the next two orders of magnitude reverse, and the cost of that is not the
three mechanisms — two of them are probably correct — but that the series has no record of which
of its decisions were ever tested against the thing it is trying to beat.

Where a full-scale measurement is genuinely impractical, the document must say so, state the scale
it did reach, and name the quantity it is assuming stays flat. Document 14 is about why that
quantity is almost never flat.
