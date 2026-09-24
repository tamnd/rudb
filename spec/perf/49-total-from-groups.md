# A total read off the groups that already add it up

TPC-H q11 groups the German part supplies by part and keeps the parts whose value is more than a small share of the total value. The total is a subquery over the same three tables with the same filter. As written the join of partsupp, supplier and nation runs twice, once to make the groups and once to add up the total, and the second run adds nothing the first did not already know.

## What it cost

At one scale factor on one thread q11 took 0.162 G instructions against DuckDB's 0.165 G. The two joins were close to the same cost each, since both scan all 800 thousand partsupp rows, probe them against the four hundred German suppliers and multiply two columns for the 32 thousand rows that survive. The grouping itself is small next to that.

No amount of speed in the scan or the probe changes the fact that the work is done twice. The fix is to do it once.

## The change

A new optimizer pass, `total_from_groups`, runs right after the average rewrite and before filter pushdown. It looks for an aggregate with no groups and a grouped aggregate whose inputs are the same query, and whose calls are each one of the grouped calls.

The inputs are compared node by node: the same scans of the same tables, the same filters, projections, cross products, joins and inner aggregates in the same shape, with the same expressions once each column on the total's side is mapped to the column on the grouped side it stands for. A scan or a projection on the total's side may produce fewer columns than the grouped side, since column pruning has usually run on it first. Only inner, left, semi and anti joins are compared, and a column from outside the subtree has nothing to map to, so a correlated input is never called the same. A volatile expression is never the same either.

The calls have to be plain `sum`, `min` or `max`, and a `sum` has to be over a decimal. A sum of exact group sums is the exact total and the smallest of the group minimums is the minimum, where a floating point sum of sums would round differently. Nulls come out the same way: a group whose values are all null sums to null and the outer sum skips it, and no groups at all sums to null, which is what the total over no rows is.

When it finds a pair, the grouped aggregate becomes the definition of a materialisation at the root of the plan, a read of it takes the grouped aggregate's place, and the total becomes an aggregate with no groups over a second read. Everything above either one reads the same index it read before, so nothing else in the plan moves. The executor already ran materialisations for `WITH ... AS MATERIALIZED`, so this needed no new operator.

## Results

On one thread q11 went from 0.162 G to 0.109 G instructions against DuckDB's 0.165 G, and from 38.9 ms to 25.9 ms. All 22 answers match at one thread and at the default. The other queries do not have this shape and their plans do not change.
