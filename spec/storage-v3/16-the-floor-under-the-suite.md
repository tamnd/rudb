# The floor under the suite

## Why this document exists

Document 14 states four obligations and document 15 reports that three are satisfied in the native format and one, O1, is not. Both were written from the outside: from query wall times and from query texts, without reading the engine that produced them. This document reads the engine, measures its operators one at a time, and reaches three conclusions that the outside view could not reach.

The first is that document 15 attributes rudb's remaining deficit to the wrong mechanism. The second is that both mechanisms document 14 prescribes are already built, tuned, and measured in the repository, so there is no missing mechanism of the kind those obligations ask for. The third is the one that matters: the target this series is aimed at sits at roughly the cost of scanning the columns the queries name, which prices every remaining execution improvement at once and leaves no room for any of them to be partial.

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

The eight queries that are 75.5% of the suite, each profiled on the same 10,000,000 row database on the same quiet host. The scan column is the `Get` operator alone and the total is the whole query. Both are CPU summed across threads, which is what `EXPLAIN ANALYZE` reports and what `crates/rudb-metrics/src/counters.rs` accumulates, so the two are in the same units and their ratio is the only thing read off them here. Neither is a wall time and neither is comparable to one.

| query | scan CPU | query CPU | scan share |
| --- | ---: | ---: | ---: |
| Q33 | 1.386 s | 2.564 s | 54.1% |
| Q19 | 595.421 ms | 1.927 s | 30.9% |
| Q10 | 410.834 ms | 1.353 s | 30.4% |
| Q17 | 352.262 ms | 1.391 s | 25.3% |
| Q23 | 1.059 s | 5.412 s | 19.6% |
| Q22 | 580.252 ms | 2.966 s | 19.6% |
| Q21 | 310.243 ms | 2.392 s | 13.0% |
| Q29 | 674.618 ms | 13.709 s | 4.9% |

Across the eight the scan is 5.368 seconds of 31.714, which is 16.9%. The floor is real but it is low, and the interesting entry is the last row: Q29, the single largest query in rudb's native suite at 20.58 seconds, spends 95.1% of its CPU on a regular expression and 4.9% on reading the column it applies it to.

So the eight queries that are three quarters of this suite would run about 5.9 times faster if every operator above the scan cost nothing. That is the ceiling on execution work, measured rather than assumed, and it is not the foreclosure an earlier draft of this section claimed. It is a budget.

## What the budget buys

rudb takes 116.63 seconds and DuckDB takes 244.12, so ten times better is 24.41 seconds and rudb needs 4.78x more than it has.

Give the top eight the whole 5.9x, which is grouping, distinct counting, substring matching, regular expressions, top-N and materialisation all reduced to zero on the dominant three quarters of the suite at once. Those eight fall from 88.09 seconds to about 14.89, the other 35 stay at 28.54, and the suite lands at 43.43 seconds. That is 5.62x against DuckDB, and it is short of the target by a factor of 1.78.

The remaining 1.78x has to come from the 35 queries outside the top eight, which cost 28.54 seconds between them and 20 of which already finish in under half a second each. Reaching 24.41 seconds requires the top eight at their scan floor *and* the tail cut to about a third of what it is.

That is the arithmetic, and the conclusion it supports is narrower than foreclosure and more useful. The target is not out of reach, but it sits at roughly the cost of scanning the columns the queries name. There is no version of it that an operator gets to by being twice as good. Every operator in the engine has to stop costing anything measurable, on every query, which is not optimisation of the work but removal of it.

It also explains why document 13 found zone maps to be the one mechanism whose advantage grew with the data. Zone maps are the only mechanism in the series whose benefit is rows never read. Everything else in the series makes a pass over the data cheaper, and at a target set near the price of the pass, a cheaper pass is still a pass.

## What it leaves

Two things, and the measurements above rank them.

The first is the one this suite actually spends its time on. Q29 is 95.1% regular expression and Q21 is 87.0% substring match, and the five string matching queries are 41.1% of the suite against a 1.38x lead. Document 14's O2 is satisfied on all of them, at one evaluation per distinct value, and the section above shows what satisfying it leaves on the table. This is where the budget is, and it is a question about evaluating a pattern against a dictionary, not about scanning.

The second is the quantity no obligation in document 14's cost model touches: `S(Q)`, the stripes that survive pruning, and through it `R(Q)`, the rows in those stripes. Every obligation stated so far takes `R(Q)` as given and argues about the cost per row.

Reducing `R(Q)` for these queries means deciding, without reading a chunk's codes, that the chunk holds no row the predicate can pass. rudb's zone maps hold ends, which decide a range predicate and cannot decide a substring one, because the dictionary is rank ordered and the values containing `google` are scattered through the whole rank order rather than gathered into an interval of it. Deciding a substring predicate per chunk needs a per chunk summary over codes rather than over ends.

Reducing `R(Q)` for these queries means deciding, without reading a chunk's codes, that the chunk holds no row the predicate can pass. rudb's zone maps hold ends, which decide a range predicate and cannot decide a substring one, because the dictionary is rank ordered and the values containing `google` are scattered through the whole rank order rather than gathered into an interval of it. Deciding a substring predicate per chunk needs a per chunk summary over codes rather than over ends.

Whether that pays was measured rather than argued, and the answer is that it does not at any chunk size a column store would choose. The 646 rows `URL LIKE '%google%'` matches in 10,000,000 were bucketed by row position at five granularities.

| rows per chunk | chunks | chunks holding a match | table skipped |
| ---: | ---: | ---: | ---: |
| 1,024 | 9,766 | 404 | 95.9% |
| 8,192 | 1,221 | 320 | 73.8% |
| 65,536 | 153 | 123 | 19.6% |
| 122,880 | 81 | 78 | 3.7% |
| 1,000,000 | 10 | 10 | 0% |

Placing 646 matches at random into 9,766 chunks would touch about 625 of them, so 404 is clustering and not noise, but it is mild clustering and it has decayed to nothing by the time a chunk is large enough to be worth reading as a unit. rudb's chunk is 120,000 rows, recorded in `crates/rudb-pipeline/src/root.rs`, so the row of that table that describes this engine is the 122,880 one: 78 of 81 chunks hold a match and pruning skips 3.7% of the table. The file is ordered by `CounterID` and `EventTime`, and nothing in that order gathers the URLs that contain a particular substring.

This is also the friendly case. `URL LIKE '%google%'` matches 0.0065% of the rows. `Referer LIKE '%google%'` matches 659,778 of the 10,000,000, which at every granularity in that table would touch every chunk, and a `SearchPhrase <> ''` gate of the kind Q21 through Q23 carry is not selective at all.

So the coarse version of the remaining direction is closed, and what it leaves is the expensive version: not a per chunk summary that lets a chunk be skipped, but a posting list that maps a dictionary value straight to the rows holding it, so that a substring predicate resolves to matching codes and then to row positions without a pass over the column. That makes the query cost `O(K(Q))` and it is the only structure discussed here that does. It also costs, at load, an index over 18.3 million values covering 100,000,000 row positions, which is a second copy of the column in a different shape.

That trade is the one place the series has room, and it is worth naming plainly. rudb's load takes 1372.10 seconds against DuckDB's 395.21, which document 15 reports as its clearest deficit, and its file is 1.82x smaller, which document 15 reports as one of its three exact wins. An index of this kind spends both of them. Load is paid once and queries are paid 43 times, so the direction is right, but the size of the payment has to be stated before it is made rather than discovered after, and this document does not propose making it.

## What this document does not claim

It does not claim the target is reachable. It claims the opposite about one whole category of approach, and the one direction it leaves open it also prices rather than recommends. The measurements here are operator timings from a single pass at a tenth of benchmark size, on a host document 15 shows cannot support per query wall times to two significant figures. They are used only for ratios within one pass, between operators measured side by side in the same process, for counts of rows and chunks that are exact, and for a lower bound whose direction of error is known.

The obligation this suggests adding is not a fifth alongside the four, because it subsumes them. A mechanism that makes a pass cheaper must state what fraction of the suite's floor it removes, and the floor is the scan. Document 14's four obligations are all statements about the cost of touching a row, and this document is the measurement showing that touching the rows is the cost.
