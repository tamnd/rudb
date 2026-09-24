# An average off a sum

Notes written on 24 September 2026, after #1699, on the last piece of q01 that cost more than DuckDB.

## The question

After #1699 the q01 scan and the four sums behind its filter were close to DuckDB, 0.78 G instructions against 0.75 G on one thread, and the whole query was still 2.33 G against 2.13 G. A profile of q01 run in a loop on one thread put 14 percent of the samples in `mean_into`, which is `avg` adding its argument into its own running total, beside 16 percent in `total_into`, which is `sum` doing the same.

q01 asks for `sum(l_quantity)` and `avg(l_quantity)`, and `sum(l_extendedprice)` and `avg(l_extendedprice)`. So two of the six million row columns were being added up twice in the same grouping, once by the sum and once by the average, and both totals came out the same.

## What changed

An optimizer pass, `common_aggregate`, finds an `avg(x)` whose aggregate also has a `sum(x)` of the same argument. The average turns into that sum and a `count(x)`, and a projection above the aggregate divides the one by the other with an internal function, `__rudb_mean`. The projection takes the aggregate's table index and output order, so nothing above it has to change.

A count over a column with no nulls is taken from the chunk's group tally, which the sums already build, so it costs almost nothing. An average with no sum of the same argument beside it is left as it is, because a sum and a count together cost at least what the mean does.

The pass runs next to the other pass that puts a projection over an aggregate, before filter pushdown. Placed late, it left a `HAVING` filter above the new projection that filter pushdown then moved below it on the second run, and the optimizer refuses a set of passes that does not settle.

## Why the answer is the same bits

`avg` over whole numbers adds into an exact `i128` and divides once, by the count times the power of ten of the scale, which is the one rounding DuckDB makes too. `__rudb_mean` calls the same function the mean state finishes with, `divide_mean`, on the sum's unscaled total and the count, so the two ways of reaching an average cannot drift apart.

Where they could differ is overflow: the mean goes on in floating point when its exact total overflows, and the sum raises. So the pass only takes integers of at most 64 bits and decimals of width 18 or less, where the total cannot overflow before the row count passes two to the sixty four. A `DISTINCT` or a `FILTER` on either call is refused, since the sum and the average would then be over different rows. `crates/rudb/tests/shared.rs` compares answers with the pass on and off over nulls, groups with only nulls, and no rows at all.

## What it did

| q01 | before | after | DuckDB |
|---|---|---|---|
| one thread, on #1699 | 2.33 G | 2.03 G | 2.13 G |
| one thread, on #1700 | 2.05 G | 1.87 G | 2.14 G |
| default threads, on #1700 | 2.06 G | 1.87 G | 2.16 G |

#1700 landed while this was in review and took the filter's cutting out of q01, and the two add up: the saving here is in the sums and the saving there is in the scan.

All 22 answers are the same bytes as before. No other query has a sum and an average of the same argument, so nothing else moved.
