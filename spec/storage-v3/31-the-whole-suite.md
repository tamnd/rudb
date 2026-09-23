# The whole suite

## Why this document exists

Documents 28, 29 and 30 measured seven queries between them, each on a table holding only the columns that query needed, because that is what the host's disk would hold. Every one of them closed by saying the six or seven were not the suite, and document 29 said it most directly: the queries measured were the easiest shape in it, chosen because they were the shape the reader served.

This document measures all forty three, unmodified, on the full hundred and five column table, on both engines' own formats, and checks the answers. It is the first measurement in this series that nobody chose.

## How it was run

Ten million rows of `hits.parquet`, all hundred and five columns, loaded into a rudb file and a DuckDB file by the same statement. The statement converts the four time columns the Parquet file stores as integers, which is how every ClickBench loader does it:

```
CREATE TABLE hits AS SELECT * REPLACE (
  (TIMESTAMP '1970-01-01 00:00:00' + INTERVAL (EventTime) SECOND) AS EventTime,
  (DATE '1970-01-01' + EventDate::INTEGER) AS EventDate,
  (TIMESTAMP '1970-01-01 00:00:00' + INTERVAL (ClientEventTime) SECOND) AS ClientEventTime,
  (TIMESTAMP '1970-01-01 00:00:00' + INTERVAL (LocalEventTime) SECOND) AS LocalEventTime
) FROM read_parquet('hits.parquet') LIMIT 10000000;
```

Adding an integer to a date is the route around the missing `INTEGER -> DATE` cast document 30 recorded, and it means the date predicates in q37 through q43 run as written rather than rewritten into numbers. Both engines ran the statement without change.

Then each query from `crates/rudb/testdata/clickbench.sql`, exactly as that file holds it, once to warm and once under `/usr/bin/time`, one engine at a time. Processor is user plus system. The host was idle when the run started and another tenant began a compile about halfway through, so wall clock is not reported and processor is the column to read. Processor time is recorded to a hundredth of a second, so a ratio on a row where rudb took 0.01 seconds is a statement about resolution as much as about rudb.

Ten million rather than a hundred million because the full width DuckDB file at a hundred million would not fit beside rudb's on this host. Section "What this does not claim" comes back to what that changes.

## The answers agree

All forty three returned without error on both engines. Compared cell by cell after taking out display differences, thirty two return the same rows in the same order and three the same rows in another order. The remaining eight differ only where SQL allows them to: q18 has no `ORDER BY` at all, and q22, q32, q33, q39, q40 and q41 order by a count that ties across the limit, and in every one of the six the ordering column is identical row for row between the engines. q24 orders by `EventTime` and its ten `EventTime` values are identical.

## The measurement

| | rudb processor | DuckDB processor | ratio | rudb peak | DuckDB peak | ratio |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| q1 | 0.04 s | 0.19 s | 4.8x | 12.8 MiB | 36.1 MiB | 2.8x |
| q2 | 0.01 s | 0.21 s | 21.0x | 12.8 MiB | 43.2 MiB | 3.4x |
| q3 | 0.01 s | 0.32 s | 32.0x | 12.9 MiB | 51.4 MiB | 4.0x |
| q4 | 0.02 s | 0.36 s | 18.0x | 12.9 MiB | 57.7 MiB | 4.5x |
| q5 | 1.20 s | 1.23 s | 1.0x | 153.4 MiB | 107.9 MiB | 0.7x |
| q6 | 0.01 s | 1.65 s | 165.0x | 12.8 MiB | 222.3 MiB | 17.4x |
| q7 | 0.01 s | 0.14 s | 14.0x | 12.8 MiB | 37.4 MiB | 2.9x |
| q8 | 0.05 s | 0.36 s | 7.2x | 13.1 MiB | 46.2 MiB | 3.5x |
| q9 | 2.23 s | 1.19 s | 0.5x | 253.9 MiB | 137.3 MiB | 0.5x |
| q10 | 2.17 s | 1.93 s | 0.9x | 258.7 MiB | 153.0 MiB | 0.6x |
| q11 | 0.22 s | 0.37 s | 1.7x | 56.0 MiB | 84.1 MiB | 1.5x |
| q12 | 0.37 s | 0.55 s | 1.5x | 55.6 MiB | 90.8 MiB | 1.6x |
| q13 | 0.08 s | 1.72 s | 21.5x | 27.9 MiB | 273.1 MiB | 9.8x |
| q14 | 0.82 s | 2.54 s | 3.1x | 112.4 MiB | 353.4 MiB | 3.1x |
| q15 | 0.82 s | 2.03 s | 2.5x | 111.3 MiB | 276.2 MiB | 2.5x |
| q16 | 0.01 s | 1.14 s | 114.0x | 13.2 MiB | 126.1 MiB | 9.5x |
| q17 | 2.84 s | 2.69 s | 0.9x | 360.8 MiB | 337.4 MiB | 0.9x |
| q18 | 0.53 s | 2.37 s | 4.5x | 51.0 MiB | 324.1 MiB | 6.4x |
| q19 | 3.80 s | 5.20 s | 1.4x | 401.2 MiB | 579.4 MiB | 1.4x |
| q20 | 0.06 s | 0.18 s | 3.0x | 16.1 MiB | 49.1 MiB | 3.0x |
| q21 | 2.78 s | 2.74 s | 1.0x | 348.5 MiB | 324.3 MiB | 0.9x |
| q22 | 3.08 s | 3.13 s | 1.0x | 394.4 MiB | 385.5 MiB | 1.0x |
| q23 | 5.33 s | 4.50 s | 0.8x | 729.4 MiB | 587.6 MiB | 0.8x |
| q24 | 2.60 s | 2.51 s | 1.0x | 411.5 MiB | 409.5 MiB | 1.0x |
| q25 | 0.21 s | 0.24 s | 1.1x | 52.2 MiB | 57.2 MiB | 1.1x |
| q26 | 0.43 s | 0.62 s | 1.4x | 66.2 MiB | 103.8 MiB | 1.6x |
| q27 | 0.26 s | 0.27 s | 1.0x | 63.0 MiB | 61.7 MiB | 1.0x |
| q28 | 1.67 s | 2.66 s | 1.6x | 71.5 MiB | 337.3 MiB | 4.7x |
| q29 | 19.76 s | 28.50 s | 1.4x | 647.8 MiB | 672.8 MiB | 1.0x |
| q30 | 0.16 s | 0.44 s | 2.8x | 30.2 MiB | 56.8 MiB | 1.9x |
| q31 | 1.25 s | 1.46 s | 1.2x | 104.6 MiB | 215.2 MiB | 2.1x |
| q32 | 1.42 s | 2.47 s | 1.7x | 128.4 MiB | 346.8 MiB | 2.7x |
| q33 | 5.58 s | 7.05 s | 1.3x | 330.9 MiB | 824.7 MiB | 2.5x |
| q34 | 0.11 s | 9.47 s | 86.1x | 38.8 MiB | 989.9 MiB | 25.5x |
| q35 | 0.20 s | 9.86 s | 49.3x | 38.0 MiB | 1013.7 MiB | 26.7x |
| q36 | 0.01 s | 1.07 s | 107.0x | 13.6 MiB | 118.7 MiB | 8.7x |
| q37 | 0.38 s | 0.81 s | 2.1x | 75.0 MiB | 143.4 MiB | 1.9x |
| q38 | 0.20 s | 0.34 s | 1.7x | 64.4 MiB | 59.7 MiB | 0.9x |
| q39 | 0.17 s | 0.66 s | 3.9x | 52.9 MiB | 77.5 MiB | 1.5x |
| q40 | 1.79 s | 1.42 s | 0.8x | 173.6 MiB | 230.6 MiB | 1.3x |
| q41 | 0.28 s | 0.25 s | 0.9x | 46.2 MiB | 59.3 MiB | 1.3x |
| q42 | 0.20 s | 0.38 s | 1.9x | 46.4 MiB | 53.7 MiB | 1.2x |
| q43 | 0.21 s | 0.21 s | 1.0x | 37.4 MiB | 46.4 MiB | 1.2x |

Summed over the suite, rudb spends 63.4 seconds of processor to DuckDB's 107.4, which is 1.7 times. The geometric mean of the per query ratios is 3.4 on processor and 2.3 on memory.

Three queries clear ten times on both axes: q6, q34 and q35. Ten clear it on processor. Three clear it on memory.

## Loading and disk

| | processor | peak | file |
| --- | ---: | ---: | ---: |
| rudb | 698.2 s | 2,507 MiB | 1,069 MiB |
| DuckDB | 440.5 s | 3,710 MiB | 1,866 MiB |

rudb's file is 1.75 times smaller for 1.6 times the processor to write it. Disk is not ten times on any table in this series and nothing here changes that.

## The shape of the distribution

Sort the forty three by processor ratio and they fall into four groups:

| processor ratio | queries |
| --- | --- |
| rudb behind | q9, q10, q17, q23, q40, q41 |
| within a factor of two | q5, q11, q12, q19, q21, q22, q24, q25, q26, q27, q28, q29, q31, q32, q33, q38, q42, q43 |
| two to ten | q1, q8, q14, q15, q18, q20, q30, q37, q39 |
| ten or more | q2, q3, q4, q6, q7, q13, q16, q34, q35, q36 |

The middle group is the largest and it is the one document 30 predicted. These are the queries where both formats carry the same structure, a column to scan and a zone map to skip with, and both engines do the same work at about the same speed. q29 alone, a `REGEXP_REPLACE` over every `Referer`, is 31 percent of rudb's processor for the whole suite, and nothing in either format lets it read fewer strings.

## Document 30's prediction, checked

Document 30 said the ratio tracks structural asymmetry and not effort. It had been fitted to seven measurements and this is the first test against queries it was not fitted to.

It holds where it said it would. The five synopsis queries from document 29, q13, q16, q34, q35 and q36, are the five largest ratios after the scalar ones, between 21 and 114 times on processor. The seven `CounterID = 62` queries are between 0.8 and 3.9 times, which is parity with some noise.

It also explains the ten times group's other members, which no document had looked at. q3, q4, q6 and q7 are table wide aggregates without a grouping, and `stored_summary` in `rudb-exec/src/build.rs` answers them from numbers every file already carries: stripe totals for `SUM` and `AVG`, stripe extremes for `MIN` and `MAX`, and for `COUNT(DISTINCT)` of a string column the number of dictionary codes any row holds, which the writer records at checkpoint. q2 is a count under a filter and does not take that path, and it still runs at the timer's resolution in an empty process's memory. q6 is the clearest case: `COUNT(DISTINCT SearchPhrase)` is a number rudb's directory holds and DuckDB's format has no place for, and it is 165 times on processor and 17.4 on memory. That is the same asymmetry as document 28's, one level simpler, and it is the third of the three queries that clear both axes.

It fails in one place, and the failure is informative. The refused shape, a composite key with no skew, was 0.22 times at a hundred million rows on a narrow table. Here q33, which is that shape, is 1.3 times ahead on processor and 2.5 on memory. The fallback's deficit at a hundred million was not a property of the operator. It is a property of the operator at a size where its table no longer fits in cache or memory the way DuckDB's does, and a ten million row run cannot see it.

## What rudb loses, and why

Six queries are behind on processor and seven on memory. Four of the seven memory losses, q5, q9, q10 and q23, have one thing in common, which is `COUNT(DISTINCT UserID)`. q5 is the plainest: one distinct set of 1,530,334 sixty four bit integers costs rudb 153.4 MiB against DuckDB's 107.9. That is about a hundred bytes an entry for a value eight bytes wide, and DuckDB at seventy is not good either. Grouping multiplies it: q9 keeps one set per region and loses on both axes by half.

q5 is also the numeric twin of q6. Both are `COUNT(DISTINCT column)` over the whole table, and the only difference is the column's type. q6 is 165 times ahead and q5 is level, because `Reader::distinct_values` in `rudb-native` returns the writer's exact count for a string column and `None` for everything else, since only string columns have a dictionary. The writer already sees every value of every integer column on its way to the frequency summary. What it does not keep is the set.

So the fallback that matters at this scale is not the hash aggregate document 30 named. It is the distinct set inside it, which is the only aggregate state in the suite whose size grows with the data rather than with the number of groups. A distinct set over a sixty four bit key that took sixteen bytes an entry would put q5 at around 24 MiB and q9 at a similar fraction, which is four to six times less than DuckDB rather than 1.4 times more, and it would do it by storing less rather than by storing it faster.

## The memory floor

The smallest peak rudb reaches on any query is 12.8 MiB, which is what the process costs before it reads anything. Ten times less memory therefore requires DuckDB to use at least 128 MiB, and at ten million rows it does so on only 22 of the 43 queries. On the other 21 the memory half of the target is unreachable by construction at this scale, however little rudb does. q2, q3, q4 and q7 are the clearest: rudb is between 14 and 32 times ahead on processor and reads almost nothing, and still cannot clear the memory axis because DuckDB is under 60 MiB.

This is the same kind of fact as document 25's arithmetic on the Parquet quadrant, a ceiling set by a number neither engine's design controls. It moves with scale, which is why documents 28 and 29 cleared memory on q13, q16 and q36 at a hundred million rows and this run does not at ten.

## What to build, in order

First the distinct set. It is behind on four queries, it is the largest single cause of rudb losing, and the fix is a representation, not an algorithm: a flat open addressed table of the raw keys at their native width. The same table kept by the writer for an integer column, and its size written to the directory beside the string columns' count, would put q5 where q6 is, which is the structural kind of win and not the faster kind. It is not free: on a near unique column such as `WatchID` at a hundred million rows the writer's set is over a gigabyte at sixteen bytes an entry, on a load that is already 1.6 times DuckDB's processor, so the writer needs a limit past which it records nothing and the reader counts the rows as it does now.

Second, document 30's `u <= c` operator for q14, which is 3.1 times here and was measured at seventeen candidates on the real column.

Third, the load, which is 1.6 times DuckDB's processor. It is the one cost every query pays for and no document in this series has profiled.

Not the middle group. Eighteen queries at parity are eighteen queries where both engines read the same bytes, and document 30's argument is that no amount of work on the scan makes that ten times.

## The first item, built

The writer now keeps the set. `rudb-native/src/distinct.rs` holds each integer column's values as their sixty four bits in a flat table with linear probing, filled on the frequency pass the writer already made, capped at 2^25 slots so a near unique column gives up rather than holding a gigabyte, and the count goes into the directory slot a string column's dictionary count already used. The format did not change.

The same ten million rows, loaded and run again:

| | before | after | DuckDB |
| --- | ---: | ---: | ---: |
| q5 processor | 1.20 s | 0.01 s | 1.23 s |
| q5 peak | 153.4 MiB | 13.0 MiB | 107.9 MiB |
| load processor | 698.2 s | 745.0 s | 440.5 s |
| file | 1,069 MiB | 1,071 MiB | 1,866 MiB |

q5 goes from level to 123 times on processor and 8.3 times on memory, and it does not clear both axes, for the reason the section on the memory floor gives: DuckDB answers it in 107.9 MiB and rudb cannot go below 12.8. The load pays 7 percent more processor for it, and the file 1.7 MB.

The other forty two moved by what the host's load moved them by, in both directions, and their answers are the ones they were apart from ties at the limit. The suite total went from 63.4 to 54.7 seconds, most of that noise. Three of forty three still clear both axes. The change is worth recording as the kind of change that works, a number the writer can afford and the competitor's format has no place for, and as a measurement of how little one such number moves a target stated over the whole suite.

## The second item, tried and not kept

q9, q10 and q23 keep their grouped distinct pairs in `rudb-exec/src/pairs.rs`, where every row becomes a sixteen byte record in one of sixty four radix partitions per scan thread and duplicates are removed only when the partitions are finished. That is why q9 holds 253.9 MiB for about one and a half million users: the memory follows the ten million rows, not the pairs.

The obvious fix is to have each thread's partition sort itself and drop repeats whenever it doubles. It was built and measured on three million rows shaped like the real column, 450,000 users seen about six and a half times each: the peak went from 127 to between 117 and 125 MiB and the processor time doubled, 0.04 to 0.08 seconds, with the same answers. On three million distinct users there was nothing to drop and it cost half again.

The reason is arithmetic, not tuning. A pair seen six times over the whole table, dealt across ten threads and sixty four partitions, turns up in any one thread's partition less than once on average, so the thread that holds a record almost never holds its duplicate. Repeats only meet where the threads' partitions meet, at the finish. A fix that saves memory has to deduplicate in a structure the threads share, or change what a record costs, and neither is the local change it looked like.

It also cannot reach the target on this query. DuckDB's peak on q9 is 137.3 MiB, so ten times less is 13.7 MiB, one megabyte over the process floor, and no representation of a million and a half pairs fits in one megabyte. The grouped distinct queries are worth making no longer behind, and that is the whole of what they can give.
## What the planner held

q13 was 21.5 times ahead on processor and 9.8 times on memory, 27.9 MiB against DuckDB's 273.1, the nearest miss in the suite. Its operators held 6.2 KiB at the peak, because the answer comes out of the writer's frequency synopsis and not out of the rows. A plain `EXPLAIN` of it peaked at 27.8 MiB. The memory was the planner's.

The synopsis stores a string column's frequent values as dictionary codes. Turning its 512 codes into values for an estimate read each one through the dictionary's point read, which decodes the payload block a code sits in and keeps it for as long as the reader lives, and on `SearchPhrase` the 512 codes land in a hundred and twenty five of the six hundred and seventy seven blocks. A trace of the `EXPLAIN` showed 6.4 MB read, about 4 MB of it those blocks, which decode to about three times that.

`TextSource::visit` now reads a set of positions a block at a time and keeps nothing it decoded, and the reader keeps the synopsis's values instead, a few kilobytes per column, so the estimates that ask again do not decode again. Nothing about the file changed.

The same file and binary built before and after, on the same host, with a load average near twenty three from other work on it, so the processor column moved in both directions and the memory column is the one to read:

| | before | after | DuckDB |
| --- | ---: | ---: | ---: |
| q13 processor | 0.10 s | 0.11 s | 1.72 s |
| q13 peak | 28.3 MiB | 18.3 MiB | 273.1 MiB |
| q34 peak | 39.0 MiB | 25.9 MiB | 989.9 MiB |
| q35 peak | 38.9 MiB | 25.3 MiB | 1013.7 MiB |
| q17 peak | 375.2 MiB | 328.6 MiB | 337.4 MiB |
| q19 peak | 399.6 MiB | 349.4 MiB | 579.4 MiB |
| q28 peak | 81.5 MiB | 62.7 MiB | 337.3 MiB |
| q38 peak | 62.4 MiB | 44.8 MiB | 59.7 MiB |

q13 now clears both axes, 15.6 times on processor and 14.9 on memory, which makes four of forty three with q6, q34 and q35. q17 and q38 go from behind DuckDB on memory to ahead of it. Twenty four of the forty three peaks fell by more than a megabyte. Four rose in that single pass, q7, q24, q39 and q43, by up to 10.6 MiB, and run three times each interleaved with the old binary they read the same as it, 402 to 411 MiB on q24 and 35 to 36 on q43, so those were the host. Every answer is the one it was, apart from which rows tie at the limit in q22, q32 and q33 and the order of equal rows in q23.

The lesson is the one this document keeps arriving at from a different side. The synopsis was built so that a query could skip the rows, and it did, and then the path that read the synopsis paid in dictionary blocks what the rows would have cost in memory. A structure that lets a query do less work is only worth what its own read costs.

## What this does not claim

It does not claim the target is met. Four of forty three queries clear both axes, in the native quadrant, at one scale. The Parquet quadrant is not measured here and document 25's arithmetic still stands in it.

It does not claim the ratios hold at a hundred million. Every query here is ten times smaller than the scale documents 28 through 30 measured at. The synopsis queries should improve with scale, because DuckDB's work grows and rudb's does not, and documents 28 and 29 measured that. The fallback should get worse, because document 29 measured that too.

It does not claim a quiet host. The tenant's compile ran through roughly the second half of the suite, and a single timed run per query means a row's ratio carries whatever that cost it.

It does not claim ratios on the fastest rows are precise. Where rudb took a hundredth of a second or less the ratio is bounded by the timer, not measured by it.
