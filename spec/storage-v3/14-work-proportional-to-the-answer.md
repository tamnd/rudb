# Work proportional to the answer

## The principle

Document 00 already states this: "Execution runs on encoded data wherever the encoding permits it.
A dictionary encoded column is grouped on its codes. Decoding is a fallback path, not the default path." It is the right principle and it has not held, and the reason it has not held is that
"wherever the encoding permits it" cannot be falsified. Every document in this series satisfies it, and the series had no way to tell which of its mechanisms were carrying their weight until documents 13 and 15 measured them at full size. One of the two measurements then read as refuting three mechanisms that the other shows working, which is what an unfalsifiable principle costs.

This document restates the same idea as a cost model with named quantities, so that a mechanism can be tested against it by measurement rather than by reading.

A query does not have a cost. It has a *lower bound*, and the bound is set by three quantities and not by the size of the table:

1. **The rows it must distinguish.** Everything a predicate can prove irrelevant before reading is
   not work. Zone maps are the existing mechanism and document 05's are the one thing in this
   series whose advantage grew with scale.
2. **The distinct values it must consider.** Anything that depends on *what* a value is, and not on
   *which row* it sits in, is bounded by the number of distinct values, not by the number of rows.
   The length of a URL, whether a URL contains `google`, which group a URL belongs to: all three
   are properties of the string, and a column of 100,000,000 URLs drawn from 15,000,000 distinct
   ones should cost 15,000,000 of them and not 100,000,000.
3. **The values it must emit.** A `LIMIT 10` emits ten rows. The columns not used for filtering or
   ordering should be read for ten rows.

An engine whose cost tracks these three is doing less work because it needs less, not because it was tuned. An engine whose cost tracks rows-scanned-times-columns-projected is the one document 13 measured, and document 15 shows that the native format moves rudb off that curve for the second and third quantities above, the distinct values it must consider and the values it must emit, and leaves it on the curve wherever the answer itself grows with the table.

## The quantities

For a query `Q` over table `T`:

- `S(Q)` is the stripes that survive pruning.
- `R(Q)` is the rows in those stripes, so `R(Q) <= |T|`.
- `D(Q, c)` is the distinct values of column `c` within `R(Q)`.
- `G(Q)` is the groups in the answer before any limit.
- `K(Q)` is the rows the query emits.
- `P(Q)` is the columns the query projects, and `F(Q)` the columns a predicate or an ordering reads.
  `F(Q)` is a subset of `P(Q)` and on most of ClickBench it is very much smaller.

## The obligations

Each is a claim about how cost must scale, each names the query in the suite that tests it, and each names the measurement that decides it.

The first revision of this section reported all four as refuted, on document 13. Document 13 measures both engines over Parquet, where rudb has no dictionary of its own, and three of these four obligations are about what an engine can do with a dictionary it owns. Document 15 measures the same suite at the same size over rudb's own format and finds O2, O3 and O4 satisfied at factors between 8x and 102x. Only O1 is refuted, and it is refuted harder than document 13 could see.

**O1. Grouping costs `O(R)` probes and `O(G)` memory traffic, not `O(R)` random accesses into a
structure of size `G`.** When `G` is small the distinction is invisible because the structure sits in cache. When `G` approaches `R` every probe is a miss and a linear algorithm acquires a memory latency term that behaves like a second factor. The fix is not a faster hash: it is to stop probing a structure larger than cache, by partitioning on the key's high bits until each partition's group count fits, which converts random misses into sequential passes.

*Tested by:* Q19, key `(UserID, minute(EventTime), SearchPhrase)`, `G` close to `R`. Also Q29, Q33, Q17, Q31, Q32, and the `COUNT(DISTINCT UserID)` family Q5, Q9, Q10, where the distinct set is the structure that grows.

*Refuted by:* rudb's Q19 growing 117.4x when `R` grew 100x. A linear algorithm grows 100x. DuckDB grew 13.4x on the same query and the same file.

*Refuted again by the native format, which is the part that matters.* Every other obligation here is satisfied once rudb reads its own file, at factors between 8x and 102x. This one is not. Document 15 measures the native quadrant at full scale and every query rudb loses to DuckDB there is either a `COUNT(DISTINCT UserID)` or a `GROUP BY` on a key close to unique per row, in both of its passes, with no other shape in the list above a second.

A format change that improves 34 queries by up to two orders of magnitude and leaves exactly the unbounded-cardinality ones behind is pointing at its grouping path and at nothing else.

**O2. A predicate or a scalar function over a column costs `O(D(Q, c))` evaluations, not `O(R)`.**
Evaluating `STRLEN(URL)` 100,000,000 times when the dictionary has 15,000,000 entries is 85,000,000 evaluations of something already known. The result of a deterministic scalar function of a dictionary code is a property of the code. It is computed once per code and gathered per row, and the gather is an integer indexed load, which is the cheapest operation in the engine.

Document 09 already contains this as a sentence, "predicate results may later be cached by dictionary identity, prepared expression, and code when repeated codes make that cheaper than row evaluation", filed under *later*.

*Tested by:* Q28, `AVG(STRLEN(URL)) GROUP BY CounterID`, a few thousand groups so O1 is not in play, and no `LIKE` so the cost is purely per-row string handling. Also Q23, Q34, Q35, Q16 and Q36, whose work is a grouped count over a string key with a bounded group count.

*Satisfied in native.* Document 15 measures Q28 at 2.89 seconds against 23.28 in Parquet, Q23 at 5.39 against 44.44, Q34 at 0.51 against 37.71, Q35 at 0.45 against 45.84, Q16 at 0.11 against 7.03 and Q36 at 0.10 against 6.28. The first revision of this section called Q28's 40.1x growth in Parquet a refutation. It was a measurement of rudb without a dictionary, which is the one condition under which this obligation is impossible to meet, and the 8.1x that appears the moment rudb reads its own file is the mechanism working.

What remains open is the *later* in document 09's sentence. Q28 at 2.89 seconds is the largest of the six and it is the only one that evaluates a scalar function rather than grouping on the code directly, so it is the one query in the suite that would still test a predicate result cache. That is a question worth an experiment, not a deficit worth ranking.

**O3. A top-N reads `F(Q)` columns for `R` rows and `P(Q)` columns for `K` rows.** Document 06
specifies exactly this and gates it behind "at least eight columns must be deferred beyond the ordering columns" and "TopN plus offset must select at most 1024 rows". Q25 projects one column and orders by another, so it defers one column and the rewrite cannot fire. The eligibility rule was written for `SELECT *` and the suite's top-N queries are not `SELECT *`.

The obligation is not "defer many columns". It is that the number of *values materialized* is
`|F(Q)| * R + |P(Q)| * K`, which for Q25 is `R` ordering keys and ten strings. The string payload of a filtered column is the expensive part and there is one of it.

*Tested by:* Q25, Q26, Q27, each with one projected column and a `K` of ten, and Q20 and Q24.

*Satisfied in native.* Document 15 measures Q25 at 0.21 seconds against 7.85 in Parquet, Q26 at 0.70 against 5.63, Q27 at 0.58 against 4.91, Q20 at 0.14 against 1.98 and Q24 at 1.38 against 14.15. The first revision cited Q25's 65.4x growth in Parquet as the refutation, where the native stripes this rewrite defers past do not exist and the rewrite cannot fire at all.

The eligibility rule is still too narrow and document 06 now says so on its own terms. Q25 through Q27 defer one column, fail the eight-column test, and reach those timings anyway, so widening the rule is an improvement to a path that is already working rather than a repair to a broken one.

**O4. An aggregate over an encoded column costs the encoding, not the decoding.** A run-length
column's `SUM` is arithmetic on the runs. A dictionary column's `COUNT(DISTINCT)` is a population count over a presence bitmap of width `D`. `MIN` and `MAX` of a dictionary column whose entries are stored in sorted order are two lookups.

*Tested by:* Q6, `COUNT(DISTINCT SearchPhrase)`, against Q5, `COUNT(DISTINCT UserID)`. The two queries are the same aggregate over a string column and an integer column, so the difference between them is the value of the encoding.

*Satisfied in native, and the comparison reverses.* In Parquet, Q6 costs 16.65 seconds and Q5 costs 3.24, and the first revision of this section read that 5.1x gap as the cost of treating a string as bytes. In native, Q6 costs 0.19 seconds and Q5 costs 4.42. Counting the distinct values of a string column is now 23.3x *cheaper* than counting the distinct values of an integer column over the same rows, because the string column has a dictionary and the answer is a property of it, while the integer column has to build a set sized with the data.

That reversal is the clearest statement of the principle in the suite, and it relocates the problem. The expensive case is not the encoded column. It is the unencoded one, and `COUNT(DISTINCT UserID)` builds exactly the kind of structure O1 is about, which is why Q5, Q9 and Q10 are all slower in native than in Parquet. O4 is met; what Q5 is waiting for is O1.

## The slope test

The obligations above are stated as scaling claims on purpose, because a scaling claim can be checked by one engine against itself. That matters more than it sounds.

The measurement in document 13 needed a rival, a matched harness, two data sizes and a quiet machine, and it took most of a day. The result it produced, that rudb is 2.30x behind, is a scoreboard entry rather than a diagnosis. The *slope* it produced, that rudb's cost grew 2.6x faster than DuckDB's across the two decades measured, is the diagnosis, and the slope did not actually need DuckDB in it. rudb grew
117x on Q19 for 100x the rows. Nothing about the query's lower bound predicts superlinear growth.
That is a bug, and it is a bug that is visible without ever running a rival.

So the test each obligation carries is:

> Run the query at two sizes differing by at least 30x. Compute the engine's own growth factor.
> Compare it to the growth of the quantity the obligation says the cost depends on. An obligation
> holds when those two agree within the measurement noise, and fails when they do not.

For O1 the reference quantity is `R + G`. For O2 it is `D(Q, c)`. For O3 it is `|F| * R + |P| * K`.
For O4 it is whatever the encoding makes it. Every one of these can be counted directly out of the file's own metadata before the query runs, which means the predicted growth is available without a second measurement and the test can run in CI at one size against a computed expectation.

This is the enforcement mechanism that document 00's principle never had, and it is strictly cheaper than the comparison it replaces.

## Order of work

The first revision ranked four items by their excess over DuckDB in the Parquet suite. Ranked instead by rudb's own measured cost in the native suite, which is the configuration the project ships and which needs no rival to compute, there is one item.

1. **O1, partitioned grouping and partitioned distinct.** Q29, Q33, Q19, Q17, Q22, Q21, Q10, Q9 and
   Q5. These are the largest queries in rudb's native suite and they are every query it loses to
   DuckDB there.
2. **O2's remainder, a predicate result cache keyed by dictionary identity.** Q28 is the only query
   left that would measure it, at a few seconds.
3. **O3's remainder, widening document 06's eligibility rule.** Q24 is the whole opportunity, at
   about a second and a half.
4. **Load working set.** Not one of the four obligations, and the largest single gap in the native
   quadrant: document 15 measures rudb's load at 3.47x DuckDB's wall time and 17.58 GiB peak,
   against document 01's requirement that a load's working set not track the row count.

O1 was already first in the previous ranking, for the right reason: it is the only one of the four whose failure is superlinear, and a superlinear term is the only kind that gets worse than the measurement says when the data grows again. ClickBench at 100,000,000 rows is not the largest table rudb is meant to hold. The native measurement adds a second reason, which is that the other three are no longer costing enough to rank.

It also narrows what O1 means. Q5, Q9 and Q10 are `COUNT(DISTINCT UserID)`, not `GROUP BY`, and they regress in native alongside the grouped queries. Whatever partitioning is built has to serve the distinct path and the grouping path as one mechanism, because the suite penalises them identically.

## What this document does not claim

The four obligations are derived from query shape, from growth rates, and from one format-to-format comparison, not from a profile. That evidence is strong enough to rank the work and to rule out the hypotheses that do not fit, since rudb using half DuckDB's peak resident set across the native suite rules out spilling, and since a format change that improves most of the suite by up to two orders of magnitude while leaving one shape behind is hard to attribute to anything but that shape. It does not identify a line of code.

Document 15 also establishes how much noise this host contributes, and it is more than the project assumed. Across three passes on one unchanged DuckDB database, 34 of 43 queries spread by more than 2x and the worst spread 8.0x. No claim in either document rests on a difference smaller than that, and claims that did have been withdrawn.

One consequence is worth carrying into the work itself. rudb's own query times moved by 0.97x between a quiet host and one at load 32 while DuckDB's moved by 1.65x, which is what holding half the working set buys when a machine is shared. Whatever O1 mechanism is built has to keep that property. A partitioned grouping path that fixes the nine remaining losses by allocating its way out would trade a measured advantage for an unmeasured one.

It is also worth saying plainly what the first revision of this document got wrong, because the mistake was not in the obligations. O1 through O4 were stated before the native measurement existed and all four survived it. The mistake was attaching each one to a refutation drawn from a configuration in which the mechanism under test was not running, and then ranking a work plan by those refutations. The obligations were falsifiable, which is what they were for. The evidence chosen to falsify them was measuring something else.

Before O1 is worked, the profile that confirms it must be taken at full scale in the native format:

- **The knee.** Cache miss rate and TLB miss rate per probe against `G`, at four group counts
  spanning cache-resident to far larger than memory. The prediction is a knee where `G` leaves L3.
  If there is no knee, the cause is not the hash table and this document is wrong about Q19.
- **The asymmetry.** Why the native format improves the bounded-cardinality grouped queries by up
  to two orders of magnitude and the unbounded ones barely at all, when both take the same code
  path once the keys are in hand. The prediction is that the small-`G` queries are winning on the
  scan and on code-space keys while the probe cost is unchanged underneath, so the probe is
  invisible at small `G` and is the entire cost at large `G`. If instead the large-`G` queries are
  taking a different path, that is a smaller bug than partitioning and should be found first.

The second of those is the one to take first, because both sides of the comparison already exist in document 15 and it costs a profile rather than a design.

O2, O3 and O4 no longer need a confirming profile. They needed one while they were reported as refuted, and they are not.

A profile that contradicts its prediction retires the obligation rather than the profile.
