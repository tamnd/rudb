# Work proportional to the answer

## The principle

Document 00 already states this: "Execution runs on encoded data wherever the encoding permits it.
A dictionary encoded column is grouped on its codes. Decoding is a fallback path, not the default path." It is the right principle and it has not held, and the reason it has not held is that
"wherever the encoding permits it" cannot be falsified. Every document in this series satisfies it.
Document 13 measures an engine that satisfies it and loses by 2.30x.

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

An engine whose cost tracks these three is doing less work because it needs less, not because it was tuned. An engine whose cost tracks rows-scanned-times-columns-projected is the one document 13 measured.

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

Each is a claim about how cost must scale, each names the query in the suite that tests it, and each names the measurement that refutes it. None of them is a claim that rudb satisfies it today; document 13 is the evidence that rudb satisfies none of the four.

**O1. Grouping costs `O(R)` probes and `O(G)` memory traffic, not `O(R)` random accesses into a
structure of size `G`.** When `G` is small the distinction is invisible because the structure sits in cache. When `G` approaches `R` every probe is a miss and a linear algorithm acquires a memory latency term that behaves like a second factor. The fix is not a faster hash: it is to stop probing a structure larger than cache, by partitioning on the key's high bits until each partition's group count fits, which converts random misses into sequential passes.

*Tested by:* Q19, key `(UserID, minute(EventTime), SearchPhrase)`, `G` close to `R`.
*Refuted by:* rudb's Q19 growing 117.4x when `R` grew 100x. A linear algorithm grows 100x. DuckDB
grew 13.4x on the same query and the same file.

**O2. A predicate or a scalar function over a column costs `O(D(Q, c))` evaluations, not `O(R)`.**
Evaluating `STRLEN(URL)` 100,000,000 times when the dictionary has 15,000,000 entries is 85,000,000 evaluations of something already known. The result of a deterministic scalar function of a dictionary code is a property of the code. It is computed once per code and gathered per row, and the gather is an integer indexed load, which is the cheapest operation in the engine.

Document 09 already contains this as a sentence, "predicate results may later be cached by dictionary identity, prepared expression, and code when repeated codes make that cheaper than row evaluation", filed under *later*. Document 13 says it is the third largest loss in the suite.

*Tested by:* Q28, `AVG(STRLEN(URL)) GROUP BY CounterID`, a few thousand groups so O1 is not in play,
and no `LIKE` so the cost is purely per-row string handling.
*Refuted by:* rudb's Q28 growing 40.1x against DuckDB's 7.5x, in 128 MB against DuckDB's 1,257 MB.
rudb held ten times less state and spent five times more time, which is what re-deriving looks like.

**O3. A top-N reads `F(Q)` columns for `R` rows and `P(Q)` columns for `K` rows.** Document 06
specifies exactly this and gates it behind "at least eight columns must be deferred beyond the ordering columns" and "TopN plus offset must select at most 1024 rows". Q25 projects one column and orders by another, so it defers one column and the rewrite cannot fire. The eligibility rule was written for `SELECT *` and the suite's top-N queries are not `SELECT *`.

The obligation is not "defer many columns". It is that the number of *values materialized* is
`|F(Q)| * R + |P(Q)| * K`, which for Q25 is `R` ordering keys and ten strings. The string payload of a filtered column is the expensive part and there is one of it.

*Tested by:* Q25, Q26, Q27, each with one projected column and a `K` of ten.
*Refuted by:* rudb's Q25 growing 65.4x against DuckDB's 8.9x while the answer stayed at ten rows.

**O4. An aggregate over an encoded column costs the encoding, not the decoding.** A run-length
column's `SUM` is arithmetic on the runs. A dictionary column's `COUNT(DISTINCT)` is a population count over a presence bitmap of width `D`. `MIN` and `MAX` of a dictionary column whose entries are stored in sorted order are two lookups.

*Tested by:* Q6, `COUNT(DISTINCT SearchPhrase)`, against Q5, `COUNT(DISTINCT UserID)`. Q5 is an
integer and rudb loses it by 1.33x. Q6 is a string and rudb loses it by 5.0x. The gap between those two numbers is the cost of treating the string as bytes rather than as a code.
*Refuted by:* that gap.

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

By measured cost in the 100,000,000-row suite, counting only the excess over DuckDB:

1. **O1, partitioned grouping.** Q19, Q33, Q34, Q35, Q14, Q23, Q22, Q13, Q16, Q31, Q32 account for
   278.8 seconds of rudb's 468.2 and 88.3 seconds of DuckDB's 203.1. That is 190 seconds of a
   265-second deficit, in one mechanism.
2. **O2, evaluation in code space.** Q28, Q21, Q22, Q23 and Q6, roughly 120 seconds against 25.
   Overlaps O1 on Q22 and Q23, so the two must be sequenced rather than measured independently.
3. **O3, top-N eligibility.** Q24 through Q27, roughly 32 seconds against 13. Smallest of the
   three and by far the smallest change, since document 06's machinery exists and the eligibility
   rule is the thing that is wrong.
4. **O4, encoded aggregates.** Q5 and Q6, roughly 20 seconds against 6, and mostly subsumed by O2.

O1 first, and not because it is the largest: because it is the only one of the four whose failure is superlinear. The other three cost a constant factor and will still be there afterwards. A superlinear term is the only kind that gets worse than the measurement says when the data grows again, and ClickBench at 100,000,000 rows is not the largest table rudb is meant to hold.

## What this document does not claim

The four obligations are derived from query shape and from growth rates, not from a profile. Shape evidence is strong enough to rank the work and to rule out the hypotheses that do not fit, since rudb using eight to twelve times less memory than DuckDB on the queries it loses worst rules out spilling and Q28 having only a few thousand groups rules out O1 as its cause, but it does not identify a line of code.

Before each obligation is worked, the profile that confirms it must be taken at full scale:

- **O1.** Cache miss rate and TLB miss rate per probe against `G`, at four group counts spanning
  cache-resident to far larger than memory. The prediction is a knee where `G` leaves L3. If there
  is no knee, the cause is not the hash table and this document is wrong about Q19.
- **O2.** Instruction counts attributed to string decode and to the scalar kernel on Q28, against
  `D(URL)` counted from the dictionary. The prediction is that decode dominates and that its count
  is proportional to `R` rather than to `D`.
- **O3.** Bytes read per column on Q25. The prediction is that the `SearchPhrase` payload is read
  in full rather than for ten rows.
- **O4.** The same on Q6 against Q5.

A profile that contradicts its prediction retires the obligation rather than the profile.
