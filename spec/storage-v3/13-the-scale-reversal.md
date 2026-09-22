# The scale reversal

## What this document is

Every design document in this series before it carries an evidence section, and every one of those evidence sections was measured at 100,000 or 1,000,000 rows. The benchmark the project is judged on has 100,000,000. This document is the measurement at both ends of that gap, run through one harness on one machine, and it reports that the conclusion reverses.

It is filed as evidence rather than as a design because nothing here proposes a mechanism. It establishes what is true, which is the thing document 14 then has to explain.

**Scope.** Both engines here read Parquet. That isolates execution from storage, which is what makes the comparison fair, and it is also the limit of what the measurement can say. Every storage mechanism this series specifies is switched off in it: reading Parquet, rudb has no dictionary of its own to hold stable codes over and no native stripe to defer a projection past. A result here is a result about rudb's execution over a foreign file. It is not evidence about any document in this series that specifies the native format, and the first revision of this document used it that way and was wrong to. Document 15 is the same suite at the same size over each engine's own format, and the sections below are corrected against it.

## How it was measured

The source is the published ClickBench `hits.parquet`, 14,779,976,446 bytes, and its one million row prefix. Both engines read the same file through a view with the loader conversion ClickBench itself applies, so the 43 query texts are unmodified. Both engines therefore run against Parquet, not against their own formats: the comparison isolates execution from storage, and neither side gets the advantage of a file it wrote.

One process per run, three runs per query, the first discarded as cold, the slower of the remaining two kept. A query that fails is recorded as a failure rather than dropped. No query failed at either size on either engine. The machine is a 32-core host with 23 GiB of memory; the engines are rudb at `d41b8a24` and DuckDB `v2.0.0-dev84237`.

## The result

| | rudb | DuckDB | standing |
| --- | ---: | ---: | --- |
| 1,000,000 rows | 16.83 s | 18.65 s | rudb ahead by 1.11x |
| 100,000,000 rows | 468.21 s | 203.12 s | rudb behind by 2.30x |

One hundred times the data costs rudb 27.8x and DuckDB 10.9x. Both are sublinear because a one-million-row query is mostly process startup, which is exactly why the small measurement cannot be extrapolated: at that size the harness is measuring the floor, not the engine.

The middle of the ladder agrees. Document 01 records a ten-million-row audit at 5.765 seconds of rudb Parquet against 4.108 seconds of DuckDB Parquet, which is rudb behind by 1.40x. Placed between the two measurements here that gives 1.11x ahead, 1.40x behind, 2.30x behind, monotone across two decades. That audit summed in-process query timers rather than child process wall time, so it is corroboration of the trend and not a fourth point on the same curve.

Peak resident set moves the other way. Across the 43 queries the largest rudb process held 4.61 GiB and the largest DuckDB process held 6.77 GiB, so rudb uses 1.47x less memory at the same time as it spends 2.30x more of it. rudb is not losing because it ran out of room. On the five queries where it is furthest behind on time it is also using between eight and twelve times less memory than
DuckDB. That combination of less memory, more time and the same answer is the shape of an engine that is re-deriving something instead of holding it, and it is the single most useful fact in this document.

## What it falsifies

Three decisions in this series were accepted on measurements taken two orders of magnitude below the benchmark size. Documents 09, 06 and 08 record wins at 100,000 and 1,000,000 rows as the justification for stable global string codes, for native late materialization, and for the shared arena transition. None of the three had ever been measured at 100,000,000.

The first revision of this document went further and said those wins reverse at scale, citing Q34 at 0.37x, Q35 at 0.26x and Q24 at 0.44x here. That was an error of scope rather than of arithmetic. All three mechanisms are native-format mechanisms and all three are inert in this measurement, so these numbers are the cost of rudb executing without them and say nothing about whether they work. Document 15 measures them at full scale in the format they are specified for, where Q34 is 0.51 seconds, Q35 is 0.45, and Q24 through Q27 are between 0.21 and 1.38. They work.

What survives is the narrower claim, which is still worth making: three documents closed a design question on a number from the wrong end of the scale, and until document 15 the series had no record of whether any of them held.

One decision was already tested across the gap, and the reason it holds up is the important part. Document 05's persisted zone maps were measured on Q37 through Q40 at 1,000,000 rows. At 100,000,000 rows Q41, Q42 and Q43 are rudb wins of 3.24x, 1.68x and 2.92x, and Q37 and Q40 are the mildest losses in their shape class. Zone maps are the one mechanism in this series whose benefit is defined as *work not done* rather than as a smaller constant on work still done, and it is the one mechanism whose advantage grows with the data instead of evaporating.

## The four shapes

Sorting all 43 queries by how much rudb's standing moved between the two sizes separates them into four groups with almost no overlap. The drift column is rudb's growth factor divided by DuckDB's; below 1.0 means rudb degraded faster.

**Shape A, grouped aggregation whose group count grows with the table.** Q19, Q33, Q35, Q34, Q14,
Q23, Q22, Q32, Q31, Q16, Q13. rudb degrades 2 to 9 times faster than DuckDB. Q19 is the extreme and the clearest case in the suite: its key is `(UserID, minute(EventTime), SearchPhrase)`, which is close to unique per row, so the group count rises with the table. One hundred times the rows costs rudb 117.4x and DuckDB 13.4x. That is superlinear against a linear baseline, on a query whose input grew exactly linearly.

**Shape B, top-N with a small limit.** Q25, Q26, Q27, Q24, Q20. rudb degrades 3 to 7.5 times
faster. These queries emit ten rows. Q25 is `SELECT SearchPhrase FROM hits WHERE SearchPhrase <> ''
ORDER BY EventTime LIMIT 10`; at one million rows rudb won it by 2.58x, at one hundred million it loses by 2.86x, having grown 65.4x against DuckDB's 8.9x. The answer did not get bigger. Only the input did.

**Shape C, per-row work on a string column.** Q28, Q22, Q23, Q21, Q6. rudb degrades 1.5 to 5.3
times faster. Q28 is the cleanest instance because it has no `LIKE` and no large group count: it is
`AVG(STRLEN(URL)) GROUP BY CounterID`, a few thousand groups, and the only thing that scales is that 100,000,000 URLs have to yield their lengths. rudb grew 40.1x and DuckDB 7.5x, and rudb did it in 128 MB against DuckDB's 1,257 MB. The length of a dictionary-encoded string is a property of the dictionary entry, not of the row.

**Shape D, everything else.** Q1 through Q4, Q8, Q11, Q12, Q15, Q18, Q29, Q30, Q37 through Q43.
Drift between 0.4x and 3.2x with no pattern, and the queries rudb still wins at scale are all here.
This group needs no explanation beyond noise and fixed costs.

## The drift table

Worst first. `ratio` is DuckDB wall over rudb wall, so above 1.00 means rudb is ahead. `growth` is each engine's own wall at 100,000,000 rows over its wall at 1,000,000.

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

No decision in this series is accepted on evidence measured below the scale of the benchmark it claims to serve. A mechanism measured at 1,000,000 rows may be reported, and its number may be encouraging, but it does not close a design question. Three of the twelve documents here closed one at 100,000 or 1,000,000 rows, and the cost of that was not that the mechanisms were wrong, because document 15 later found all three of them right, but that for the length of this series nobody could tell.

A second rule follows from how the first revision of this document was misused, including by itself. A measurement is evidence only for the configuration it ran in. This one runs both engines over Parquet, so it is evidence about execution and about nothing that the native format does. Stating which mechanisms a measurement holds switched off is part of reporting it.

Where a full-scale measurement is genuinely impractical, the document must say so, state the scale it did reach, and name the quantity it is assuming stays flat. Document 14 is about why that quantity is almost never flat.
