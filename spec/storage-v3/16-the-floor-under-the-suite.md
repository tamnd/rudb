# The floor under the suite

## Why this document exists

Document 14 states four obligations and document 15 reports that three are satisfied in the native format and one, O1, is not. Both were written from the outside: from query wall times and from query texts, without reading the engine that produced them. This document reads the engine, measures its operators one at a time, and reaches three conclusions that the outside view could not reach.

The first is that document 15 attributes rudb's remaining deficit to the wrong mechanism. The second is that both mechanisms document 14 prescribes are already built, tuned, and measured in the repository, so there is no missing mechanism of the kind those obligations ask for. The third is the one that matters: the target this series is aimed at is below the cost of reading the columns the queries name, so no obligation stated in terms of execution can reach it.

## Where the time actually is

rudb's 116.63 second native pass from document 15, with all 43 queries sorted into the mechanism that dominates each one. `LIKE` and `REGEXP_REPLACE` are counted as string matching whatever else the query does, because the measurements below show that term dominates wherever it appears.

| class | queries | rudb | share | DuckDB | ratio |
| --- | ---: | ---: | ---: | ---: | ---: |
| grouping | 21 | 50.53 s | 43.3% | 152.86 s | 3.03x |
| string matching | 5 | 47.98 s | 41.1% | 66.25 s | 1.38x |
| distinct | 7 | 13.94 s | 12.0% | 17.44 s | 1.25x |
| everything else | 10 | 4.18 s | 3.6% | 7.57 s | 1.81x |

Twenty of the 43 queries finish in under half a second and together account for 3.43 seconds, which is 2.9% of the suite. Eight queries account for 88.09 seconds, which is 75.5%. Any statement about this suite that is not a statement about those eight is a statement about the last quarter of it.

Five queries containing a substring or regular expression match are 41.1% of rudb's time, and they are the class where its lead is smallest by a wide margin. Twenty one grouping queries are 43.3% of the time and rudb is already 3.03x ahead on them.

## What document 15 attributed wrongly

Document 15 lists the nine queries rudb loses on the quiet host and writes that what is left after removing two sub-second entries is `COUNT(DISTINCT UserID)` three times and `GROUP BY` on a near-unique key four times, and that this is document 14's O1.

Q21 is `SELECT COUNT(*) FROM hits WHERE URL LIKE '%google%'`. It has no `GROUP BY`, no `DISTINCT`, and one output row. It cost rudb 9.10 seconds, which is more than any grouping query in the suite except Q19. Q22 and Q23 do group, but each is gated by a `LIKE` over `URL` or `Title`, and the operator timings below show the filter costing several times the grouping. Three of the nine losses are therefore not O1 at all, and the largest single entry in the whole suite, Q29 at 20.58 seconds, is a regular expression applied to `Referer`, which rudb wins 1.86x and which O1 does not describe either.

The error is the same one document 13 made and this series has now made twice: a conclusion drawn from what queries are called rather than from what the engine does when it runs them.

## Both prescribed mechanisms already exist

O1 asks that grouping stop probing a structure larger than cache, "by partitioning on the key's high bits until each partition's group count fits". `crates/rudb-exec/src/group.rs` does exactly that. It hashes each chunk once and divides the rows by the high six hash bits into 64 partitions, each owning one open addressed table behind its own lock, with 24 bits of slot and 8 of tag per bucket. `RADIX_PARTITIONS` is 64 and its comment records why it was raised from 16. `PARTITION_FROM` is 4,096 and its comment records the sweep that chose it over 16,384. Five specialised exchanges sit above the general path, and Q19, the query document 14 names as O1's test, matches `encoded_top_count` and takes one of them rather than the general table.

O2 asks that a predicate over a dictionary encoded column cost one evaluation per distinct value. `crates/rudb-kernels/src/scalar.rs` does exactly that, in `like_stable` and `StableLike`. Each distinct value is decided once into a two bit memo, 32 values to a 64 bit word, decided 1,024 consecutive values at a time so the dictionary is read in the order it decodes rather than scattered, with a sweep that does not retain decoded blocks. Its comment records the measurement that shaped it and states the cardinality this whole document turns on: ClickBench `URL` has 18.3 million distinct values.

Neither obligation is waiting on an implementation. Both describe work that has been done.

## The slope test, run inside one database

Document 14 asks that a mechanism be tested by comparing an engine's own growth against the growth of the quantity its obligation says cost depends on, and notes that this needs no rival. The same test works across columns at one size. Four `LIKE '%google%'` filters over the same 10,000,000 rows of the same table, so the row count is held fixed and only the distinct count and the answer size vary.

| column | distinct values | filter CPU | rows produced |
| --- | ---: | ---: | ---: |
| Referer | 2,719,021 | 3.686 s | 659,778 |
| URL | 2,620,109 | 2.829 s | 646 |
| Title | 1,603,008 | 2.337 s | 120 |
| SearchPhrase | 835,093 | 0.744 s | 269 |

The distinct count spans 3.26x and the cost spans 4.95x. The answer size spans 5,498x and the cost does not follow it anywhere: Title produces 120 rows for 2.337 seconds while SearchPhrase produces 269 rows for 0.744 seconds. Cost tracks `D(Q, c)` and is indifferent to `K(Q)`.

That is O2 satisfied, stated as O2 states it. It is also the problem.

## Why satisfying O2 is not enough

Q21 profiled on the same 10,000,000 rows: the scan of `URL` costs 448.603 ms, the filter costs 2.680 s, and the filter produces 646 rows. The predicate is 86% of the query's CPU and it is already paying the discounted price.

The discount is smaller than the series has assumed. `URL` has 18.3 million distinct values against 100,000,000 rows, so evaluating once per distinct value rather than once per row is a 5.5x reduction and nothing more. rudb has collected that 5.5x. What is left is 18.3 million substring searches to answer a query whose answer is a few hundred rows, and O2 as written licenses every one of them, because O2 bounds cost by the distinct values a column holds rather than by the answer the query asked for.

This is the gap between document 14's title and document 14's obligations. The title says work proportional to the answer. O2 says work proportional to the distinct values. On ClickBench `URL` those two differ by four orders of magnitude, and the engine is sitting on the second one.

The estimator cannot see the difference either. Every one of these filters is estimated at 2,000,000 rows from a default, against actuals of 646, 120 and 269, which are q-errors of 3,096, 16,667 and 7,435. An optimiser that cannot tell a few hundred rows from two million cannot choose a selective access path over a scan even once one exists, so any mechanism built for this has to arrive with the statistics that let it be chosen.

## The floor

The eight queries that are 75.5% of the suite, each measured at the scan alone, with the operators above it excluded. Same 10,000,000 row database, same quiet host.

| query | columns read | scan |
| --- | ---: | ---: |
| Q22 | 2 | 1.229 s |
| Q23 | 4 | 1.049 s |
| Q19 | 3 | 872.984 ms |
| Q33 | 4 | 737.433 ms |
| Q17 | 2 | 548.142 ms |
| Q10 | 4 | 505.104 ms |
| Q29 | 1 | 431.773 ms |
| Q21 | 1 | 346.321 ms |

Those eight scans total 5.720 seconds at 10,000,000 rows. Scaled linearly they are 57.20 seconds at 100,000,000, against the 88.09 seconds those eight queries actually cost, so roughly two thirds of the suite's dominant block is already the scan and not the work above it.

Document 13's rule forbids closing a question on a measurement taken below benchmark size, and this one is taken ten times below it, so 57.20 seconds is not an estimate of the floor. It is a lower bound on it, and the direction of the error is known rather than guessed: per row scan cost rises with table size as the working set leaves cache, and it does not fall. The 10,000,000 row prefix is also a prefix and not a sample, holding 112 distinct `CounterID`, so its per column cardinalities are lower than the full file's and its dictionaries are correspondingly cheaper to walk.

Set that against the target. DuckDB answers this suite in 244.12 seconds, so ten times better is 24.41 seconds. rudb takes 116.63. The floor under the eight queries that are three quarters of the suite is at least 57.20 seconds, which is 2.3x the entire budget for all 43.

## What this forecloses

No improvement to any operator above the scan reaches the target. Grouping, distinct counting, substring matching, top-N and late materialisation could all become free, on every one of the 43 queries at once, and the suite would still finish outside the budget, because the budget is less than half of what it costs to read the columns the queries name.

That is a statement about arithmetic and not about effort, and it settles a question this series has been circling since document 13. Document 14 ranks an order of work. Document 15 records that nine queries stand between a 2.09x lead and a larger one. Both are asking which execution mechanism to build next, and the answer is that the choice does not matter at this target, because the sum of all of them is bounded below by a quantity none of them touch.

It also explains why document 13 found zone maps to be the one mechanism whose advantage grew with the data. Zone maps are the only mechanism in the series whose benefit is rows never read. Everything else in the series makes a pass over the data cheaper, and a cheaper pass is still a pass.

## What it leaves

One quantity in document 14's cost model is untouched by every mechanism the series has built: `S(Q)`, the stripes that survive pruning, and through it `R(Q)`, the rows in those stripes. Every obligation stated so far takes `R(Q)` as given and argues about the cost per row. The floor above is what that assumption costs.

Reducing `R(Q)` for these queries means deciding, without reading a chunk's codes, that the chunk holds no row the predicate can pass. rudb's zone maps hold ends, which decide a range predicate and cannot decide a substring one, because the dictionary is rank ordered and the values containing `google` are scattered through the whole rank order rather than gathered into an interval of it. Deciding a substring predicate per chunk needs a per chunk summary over codes rather than over ends.

Whether that pays was measured rather than argued, and the answer is that it does not at any chunk size a column store would choose. The 646 rows `URL LIKE '%google%'` matches in 10,000,000 were bucketed by row position at five granularities.

| rows per chunk | chunks | chunks holding a match | table skipped |
| ---: | ---: | ---: | ---: |
| 1,024 | 9,766 | 404 | 95.9% |
| 8,192 | 1,221 | 320 | 73.8% |
| 65,536 | 153 | 123 | 19.6% |
| 122,880 | 81 | 78 | 3.7% |
| 1,000,000 | 10 | 10 | 0% |

Placing 646 matches at random into 9,766 chunks would touch about 625 of them, so 404 is clustering and not noise, but it is mild clustering and it has decayed to nothing by the time a chunk is large enough to be worth reading as a unit. The file is ordered by `CounterID` and `EventTime`, and nothing in that order gathers the URLs that contain a particular substring. Pruning at 65,536 rows per chunk skips a fifth of the table for this predicate, which is a fifth of a term that is two thirds of the cost of eight queries, and that is not the missing 4.78x.

This is also the friendly case. `URL LIKE '%google%'` matches 0.0065% of the rows. `Referer LIKE '%google%'` matches 659,778 of the 10,000,000, which at every granularity in that table would touch every chunk, and a `SearchPhrase <> ''` gate of the kind Q21 through Q23 carry is not selective at all.

So the coarse version of the remaining direction is closed, and what it leaves is the expensive version: not a per chunk summary that lets a chunk be skipped, but a posting list that maps a dictionary value straight to the rows holding it, so that a substring predicate resolves to matching codes and then to row positions without a pass over the column. That makes the query cost `O(K(Q))` and it is the only structure discussed here that does. It also costs, at load, an index over 18.3 million values covering 100,000,000 row positions, which is a second copy of the column in a different shape.

That trade is the one place the series has room, and it is worth naming plainly. rudb's load takes 1372.10 seconds against DuckDB's 395.21, which document 15 reports as its clearest deficit, and its file is 1.82x smaller, which document 15 reports as one of its three exact wins. An index of this kind spends both of them. Load is paid once and queries are paid 43 times, so the direction is right, but the size of the payment has to be stated before it is made rather than discovered after, and this document does not propose making it.

## What this document does not claim

It does not claim the target is reachable. It claims the opposite about one whole category of approach, and the one direction it leaves open it also prices rather than recommends. The measurements here are operator timings from a single pass at a tenth of benchmark size, on a host document 15 shows cannot support per query wall times to two significant figures. They are used only for ratios within one pass, between operators measured side by side in the same process, for counts of rows and chunks that are exact, and for a lower bound whose direction of error is known.

The obligation this suggests adding is not a fifth alongside the four, because it subsumes them. A mechanism that makes a pass cheaper must state what fraction of the suite's floor it removes, and the floor is the scan. Document 14's four obligations are all statements about the cost of touching a row, and this document is the measurement showing that touching the rows is the cost.
