# Where the ratio comes from

## Why this document exists

Document 29 classified the twenty three blocked queries into four causes, counted queries against each cause, and recommended attacking the one with the most. The recommendation was wrong, and it was wrong in a way that is worth more than the recommendation was.

A query is not unblocked when one of its blockers is removed. It is unblocked when all of them are. Document 29 counted per blocker and never intersected, so it recommended work that unblocks nothing at all.

This document corrects that, prices the two directions properly, and ends up somewhere neither document expected: with an account of where every ratio in this series actually came from, which predicts the ones measured here before they were measured.

## The overlap

Of the twelve queries whose aggregate is not a plain count, eight carry another blocker as well. q22, q23, q28 and q29 also filter on a column they do not group by. q31, q32 and q33 also group by a composite key, and q31 and q32 also filter elsewhere. Remove the aggregate restriction from all of them and not one becomes answerable.

Four are blocked by the aggregate alone: q9, q10, q11 and q14. Every one of them has a `COUNT(DISTINCT UserID)`.

So document 29's split was exactly backwards. It called a count beside a `SUM` or an `AVG` the tractable half, and that half unblocks zero queries. It called a `COUNT(DISTINCT)` in the `ORDER BY` hopeless, and that is the only half where any query is waiting.

Name the mistake, because it is cheap to make and this is twice now that counting has gone wrong in this series. Document 28's was the inventory error, pricing a structure the system already has. This one is **the union error, which is counting a set of obstacles per obstacle rather than per thing obstructed.** Its signature is a table whose columns sum to more than its subject.

## A bound for the half that was called hopeless

Take a group `g` after the filter, let `c(g)` be its rows and `u(g)` its distinct count of some other column. Then `u(g) <= c(g)`, because `g` has only `c(g)` rows to hold distinct values in. That is the whole design, and it is the same shape as document 26's: a bound that is free, and an ordering that respects it.

For `ORDER BY COUNT(DISTINCT x) DESC LIMIT k`:

Take the groups in descending `c`, which the frequency synopsis already has. Choose a candidate budget `B` at least `k`, and compute `u` exactly for the leading `B` groups with one pass that reads only rows belonging to them. Let `u_(k)` be the kth largest of those. If `u_(k)` is greater than `c` at rank `B + 1`, certify.

The proof is two lines. Any group outside the candidates has `c` no greater than `c` at rank `B + 1`, because the candidates are the heaviest. So its `u` is no greater than that either, which is below `u_(k)`, so it cannot enter the top k. Every candidate's `u` is exact because it was counted and not bounded.

This is strictly better than it sounds, because the ranking pass is not a pass. In the Parquet quadrant document 26 had to read the column once to get the counts. Here the counts are on disk, and document 29 measured reading them at 0.28 seconds and 48 MiB.

## What the file says

Query 14 is `SELECT SearchPhrase, COUNT(DISTINCT UserID) AS u FROM hits WHERE SearchPhrase <> '' GROUP BY SearchPhrase ORDER BY u DESC LIMIT 10`, over the 13,172,392 rows with a non empty `SearchPhrase`. Candidate budgets around the crossing:

| budget | tenth largest `u` | bound, `c` at `B + 1` | rows the pass reads | certifies |
| ---: | ---: | ---: | ---: | :--- |
| 10 | 7,088 | 9,990 | 253,034 | no |
| 13 | 7,572 | 8,840 | 281,429 | no |
| 16 | 7,572 | 7,880 | 306,517 | no |
| 17 | 7,572 | 7,515 | 314,397 | yes |
| 20 | 7,572 | 6,646 | 336,631 | yes |

Seventeen candidates. The pass reads 314,397 rows, which is 2.4 percent of the rows that pass the filter and 0.31 percent of the table. The answer stops moving at a budget of thirteen and the remaining four are spent waiting for the bound to fall, which is the certification being conservative rather than the answer being uncertain.

The seventeen candidate answer was run against the real thing and returns the same ten rows in the same order, with the same counts, ending at 7,572. DuckDB computes that answer in 33.65 seconds of processor and 2,212 MiB.

## Where the same design is worth nothing

Two of the four queries it was built for.

`RegionID` has 9,040 distinct values across the table and its leading seventeen hold 50,983,572 rows, which is 51 percent. The candidate pass would read half the table, so q9 and q10 get nothing. The cost of this design is the sum of `c` over the candidates, and a column with few groups puts most of the table into its heaviest few.

`MobilePhoneModel` has 165 distinct non empty values in total. A budget of 27 certifies and reads 5,538,926 rows, against roughly thirteen million that pass the filter, so q11 gets something but not an order of magnitude.

That leaves q14. One query, with a proof, a measured margin and a validated answer. It is worth building and it is worth saying plainly that it is one query, because the previous two documents each opened by discovering that the last one had counted something optimistically.

## The blocker nobody had priced

Eight queries filter on columns they do not group by, and six of them share one predicate: `CounterID = 62` over a date range. Document 29 called this a zone map question and passed it on.

It is very selective and it is extremely clustered. At 65,536 rows to a stripe, the whole table is 1,526 stripes, and the rows matching q37's filter live in **14 of them**. That is 0.92 percent of the stripes holding 738,172 rows, or 0.74 percent of the table. A reader that skips stripes on a filter can skip ninety nine percent of this query.

rudb's reader has `stripe_skips` and uses it. So does DuckDB. Measured on a five column table at a hundred million rows, both returning the same ten rows:

| | processor | peak |
| --- | ---: | ---: |
| rudb | 0.87 s | 130 MiB |
| DuckDB | 1.16 s | 159 MiB |

1.3 times on processor and 1.2 times on memory. Six queries, already fast on both sides, and no order of magnitude anywhere in them. The direction document 29 set aside as a scan problem is a scan problem that is already solved, by both engines, and pricing it took one table and two commands.

## Where every ratio in this series came from

Put the measurements together and they say one thing.

| | rudb against DuckDB | what rudb has that DuckDB does not |
| --- | ---: | --- |
| q16, q34, q35, q36, q13 | 29x to 575x | a table wide exact frequency synopsis |
| q37 | 1.3x | nothing, both skip stripes |
| `WatchID`, the refused shape | 0.22x, four and a half times worse | nothing, and a slower hash aggregate |

**The ratio tracks structural asymmetry and not effort.** Where rudb's format carries something Parquet and DuckDB's format cannot, the ratio is two orders of magnitude and the work is a walk over five hundred entries. Where both formats carry the same structure, the ratio is one. Where rudb's structure does not apply and the fallback runs, the ratio is below one.

This is not a discouraging result. It is the first one in this series that predicts rather than reports. It says the 314,397 row pass above will be worth an order of magnitude, because the synopsis that picks the candidates is something DuckDB has no counterpart for. It says q37 was never going to be, and one table would have shown that before six documents of classification. And it says the work that raises the suite's floor is not a faster scan but another structure the writer can afford and the competitor's format cannot express.

It also says where the target's remaining difficulty actually lives, which is not in the queries that are slow. It is in the queries where both engines do the same thing, and in the fallback, where rudb is behind.

## What to build, in order

First the `u <= c` operator, for q14. It is proved, the margin is measured at seventeen candidates, the answer is validated, and the ranking half of it is already on disk and already read.

Then the fallback. `WatchID` is the one measurement here where rudb is worse than DuckDB, by 4.6 times on processor and 1.9 on memory, and it is the shape every unserved grouped query falls into. Nothing in documents 26 through 30 touches it, and an engine whose fast path is two orders of magnitude ahead and whose slow path is five times behind is an engine that loses the average.

Not the composite keys. Document 29's argument stands and this document's table sharpens it: there is no structural asymmetry to exploit there, because no per column synopsis of `WatchID` or `ClientIP` says anything about the pair, and the pair's top count is 2.

## What this document does not claim

It does not claim the `u <= c` operator is built. The proof is written here, the budget is measured on the real column, and the answer at that budget was checked against the real answer, all of it in DuckDB standing in for an engine that does not have the operator yet. The projection that rudb would answer q14 in about the cost of q13 plus a narrow pass is a projection, and this series has been wrong about projections before.

It does not claim seventeen is a constant. It is what this column's distribution gives for k of ten. A column whose distinct counts sit close together needs a larger budget or fails, and the operator has to test rather than assume, which is the same discipline documents 26 and 28 both landed on.

It does not claim the load side is fine. The five column table took rudb 807 seconds of processor and 6,742 MiB to write, against DuckDB's 267 and 5,290. rudb's file is 2.4 times smaller, at three times the processor to produce. No document in this series has looked at that and this one is not going to either, but it should be on the list.

It does not claim the substitution in q37 was free of consequence. rudb cannot cast an integer to a date, which is how the ClickBench table is normally built, so `EventDate` was loaded as the integer the Parquet file holds and the predicate written as the equivalent numeric range. The rows matched are the same rows. The missing cast is a real gap and it is recorded here because it was found here.
