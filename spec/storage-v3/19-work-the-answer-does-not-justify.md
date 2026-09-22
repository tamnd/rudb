# Work the answer does not justify

## Why this document exists

Document 18 sums the suite by operator and finds the ceiling on execution work, which is that zeroing every operator above the scans leaves about five times better rather than ten. A ceiling is only useful if it points somewhere, and the place it points is the scan. This document asks what the scans are for, by comparing what each query reads against what it returns, and the answer is that 86% of the suite's CPU goes on queries that read 164,385,251 rows to produce 267.

That ratio, 615,675 to one, is the subject of document 14's title. This document is the measurement under it, the reason the obvious fix does not work, and the correction to document 11 that makes a fix possible for the hardest case in the suite.

All figures are from the same 10,000,000 row native database and the same profiled pass as document 18.

## The class that is the suite

Twenty seven of the 43 queries are a `GROUP BY` with an `ORDER BY` and a `LIMIT`. Together they are 49.54 seconds of the suite's 57.63, which is 86%. Between them they scan 164,385,251 rows and return 267.

The largest of them, with what each one reads and what it gives back:

| query | CPU | `resource.peak_bytes` | rows scanned | rows returned |
| --- | ---: | ---: | ---: | ---: |
| Q29 | 20.16 s | 128 MiB | 10,000,000 | 15 |
| Q23 | 4.72 s | under 1 MiB | 10,000,000 | 10 |
| Q17 | 4.36 s | 406 MiB | 10,000,000 | 10 |
| Q33 | 3.02 s | 308 MiB | 10,000,000 | 10 |
| Q19 | 2.55 s | 364 MiB | 10,000,000 | 10 |
| Q32 | 2.22 s | 41 MiB | 10,000,000 | 10 |
| Q15 | 1.88 s | 49 MiB | 10,000,000 | 10 |
| Q10 | 1.64 s | 279 MiB | 10,000,000 | 10 |
| Q9 | 1.34 s | 276 MiB | 10,000,000 | 10 |

The middle column is worth as much as the first. Document 17 records rudb holding 1.46 GiB against DuckDB's 0.44 across a pass of this suite, which is the one axis of the project's target where rudb is behind rather than ahead, and the table above says where part of that number is made. It is made by hash tables that exist to be thrown away: Q17 builds 406 MiB of groups and emits ten rows.

Only part of it, and the middle column is not the process. Document 21 measures the resident set from outside and finds this counter blind to everything an operator does not register: Q23 is the largest memory consumer in the suite at 712 MiB and appears above as "under 1 MiB", Q29 is 643 rather than 128, and the suite's peak exceeds its heaviest query by 774 MiB that no statement is charged for. The hash tables priced below are real and are the part of the memory this counter can see. They are not the larger part, and the argument in this section should be read as applying to the queries it names rather than to the 1.46 GiB.

## Why the obvious answer does not work

Document 11 already specifies the mechanism this calls for. It stores an exact leading frequency list per column with an upper bound on every omitted value, and permits a grouped `count(*)` ordered by count to read the list instead of the column when the last requested winner is strictly above that bound. It is careful, it carries its proof, and it falls back to scanning when the proof does not hold.

It does not cover this workload, for a reason visible only once the queries are read rather than counted. Document 11 says a scalar column. Of the nine queries above, Q9 and Q10 group by one column and the other seven group by two or three: `UserID, SearchPhrase`, `WatchID, ClientIP`, `SearchEngineID, SearchPhrase`, `UserID, minute, SearchPhrase`. A frequency list for `UserID` and a frequency list for `SearchPhrase` do not give the frequency of a pair, and there is no way to derive one from the other, so the synopsis as specified answers the two smallest entries in the table and none of the seven above them.

The queries carry a second obstacle that is easy to miss. Q9 asks for `COUNT(DISTINCT UserID)`, Q10 for a sum and an average beside its count, Q28 orders by `AVG(STRLEN(URL))` rather than by a count at all. A frequency list is a list of counts, so even where it applies to the key it does not supply the other columns of the answer.

## The precondition, tested rather than assumed

A certified synopsis needs a heavy hitter. The proof is that the last winner beats the bound on everything omitted, so a key whose values are all equally rare can never be certified, and how rare the winners are is a property of the data rather than of the design. Measured on the four queries that matter most, top counts over 10,000,000 rows against the 305 that a 32,768 entry Misra-Gries table can promise:

| query | first three group counts | certifiable |
| --- | --- | --- |
| Q9 | 1,696,554, 897,731, 476,811 | yes, by a wide margin |
| Q15 | 3,481, 2,419, 2,253 | yes |
| Q17 | 2,496, 2,051, 1,651 | yes |
| Q33 | 1, 1, 1 | never |

Q33 is the interesting one and it is the third largest consumer of memory in the suite. Every `(WatchID, ClientIP)` pair in ten million rows occurs exactly once, so its `ORDER BY COUNT(*) DESC LIMIT 10` is asking for any ten of ten million tied groups. rudb spends 3.02 seconds and 308 MiB building the whole tie before discarding all but ten of it. No frequency synopsis can ever help, because the thing a synopsis proves is that somebody is frequent and nobody here is.

## The correction that covers the hardest case

`WatchID` has a maximum frequency of 1 over those ten million rows. That is a per-column fact, it is the kind of fact document 11's writer is already computing, and it settles a composite query that document 11 does not claim to address.

The step is that a bound on one grouping column bounds every composite group containing it. The rows sharing a value of `(WatchID, ClientIP)` are a subset of the rows sharing that value of `WatchID`, so `count(WatchID, ClientIP)` is at most `count(WatchID)`, and more generally the count of a composite group is at most the minimum over its columns of that column's count. A synopsis that certifies a maximum frequency of 1 for any single grouping column therefore certifies that every group of any key containing that column has exactly one row, which makes `ORDER BY COUNT(*) DESC LIMIT 10` satisfiable by any ten distinct keys and turns Q33 from a ten million entry hash table into a read of ten rows.

This extends document 11 in the direction the workload wants without extending what the writer has to store. It stays per column, the bound it uses is the one already specified, and the only new thing is that the planner may take the minimum over the columns of a composite key. It is also exactly the case where the existing design is most embarrassed, since a unique column is the cheapest possible synopsis to build and the most expensive possible key to group by.

What it does not do is cover the skewed composites. Q17's top pair count of 2,496 equals `UserID`'s own top count of 2,496, so the per-column minimum is tight enough to prove a bound and still does not say which pair achieves it. Those queries need a summary over the composite key itself, and since the writer cannot anticipate which pairs of columns a query will name, that summary has to be built when the query runs rather than when the table is written. A Misra-Gries table of 32,768 counters is about a megabyte and stays in cache, against the 406 MiB Q17 currently builds, and a second pass counting only the surviving candidates makes the result exact rather than approximate. That is document 11's proof obligation met by a different party.

## What this is worth, stated against the target

Q32 and Q33 are 5.24 seconds and 349 MiB, and the uniqueness bound above answers both. Q15, Q17, Q19 and Q31 are 9.93 seconds and 860 MiB, and a query time summary over the composite key is what they need. Together that is 26% of the suite's CPU, and document 18's dictionary finding is a further 35% on Q29. The claim that it is also the great majority of the suite's peak memory was made from the counter above and document 21 withdraws it: those hash tables are about a third of what the process holds.

Sixty one percent of the suite being addressable by two structural changes is the largest identified program this series has, and it still does not reach the target. Both changes working perfectly would take the suite from 57.63 seconds to about 22, which is 2.6 times better than rudb is now. rudb is 2.09 times ahead of DuckDB on the quiet host at benchmark scale, so the product of the two is not ten, and the honest reading of documents 18 and 19 together is that the execution ceiling and the precomputation opportunities both fall short of it by roughly the same factor of two.

The memory axis is the one where this changes the sign rather than the size. rudb holds 1.46 GiB against DuckDB's 0.44 on this suite, which is 3.32 times worse, and the hash tables this document prices are part of where that is spent. That comparison is at 10,000,000 rows and does not hold at benchmark scale: document 22 reconciles it with document 15, which measures 4.2 GiB against DuckDB's 8.62 at 100,000,000, and finds the two curves cross between the scales. Calling memory the one axis where rudb is behind is wrong at the scale the target is stated at, and the 3.32 is a fixed overhead divided by a small table. Removing them does not make rudb ten times leaner than DuckDB, but it moves the larger of the two numbers. Document 21 measures the rest of that gap from outside the process and splits it into retention and working set, so the two documents together are what points at the resource half of the target rather than the time half.

## What this document does not claim

The row counts, the group counts and the peak resident figures are exact and are not timings. The CPU figures carry document 18's warning: they were taken on a contended host where the same probe measured 7.17 seconds and 4.48 seconds in two runs, and they should be read as shares of a suite rather than as durations.

It does not claim either mechanism is implemented or that the estimates of what they would save are measurements. What is measured is what the queries read, what they return, what they hold while doing it, and whether a certified synopsis could ever apply to them. The last of those is the one that cost the least to check and changed the plan the most.
