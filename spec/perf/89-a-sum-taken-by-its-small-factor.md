# 89. A sum taken by its small factor

## The problem

q01 is the query rudb is closest to DuckDB on, at 1.3 times, and tuning the loop that adds its rows has stopped paying. It asks for `sum(l_extendedprice * (1 - l_discount))` and the same times `(1 + l_tax)` over six million rows, so every row pays for two decimal products and adds five totals, three of them 38 digits wide. `l_discount` only takes eleven values and `l_tax` nine. The product of a price and one discount summed over the rows with that discount is the discount times the sum of the prices, so the products can be taken once per discount and tax rather than once per row.

Written out by hand as a grouping by flag, status, discount and tax below a grouping by flag and status, q01 gave the same answers and cost more, 1180 M instructions against 883 M. The rows it made were right but the grouping below had 396 groups and 3,468 key places, past both the 16 groups the slot walk took and the 2,048 places the direct map took, so it went to the hash table. #2254 moved both limits and took the hand written query to 805 M.

## The change

`factor::Factoring` is a pass in front of join ordering that does this rewrite on its own. It looks at a grouping whose keys are columns, and at each `sum` of a product in it. A factor that reads only keys, or columns with at most 32 values, is small. When a `sum` has small factors and exactly one factor that is not, the columns the small factors read become keys of a grouping below, the rows add up the one factor, and the grouping above multiplies the partial totals by the small factors. A `count` becomes a sum of counts, a `min` or `max` is taken again, and an `avg` becomes a sum and a count put back together by `__rudb_mean`, which is the one division `avg` does, so the bits are the same. The module doc says when this is the same answer, and the one way it is not: a product that overflowed on one row is taken over a total in a wider type, so a query that failed can answer.

The pass needs to know a column is small. A native file already stated a distinct count for an integer column from its two ends, and `zones::distincts` now does the same for a decimal column whose ends are stored exact, which is every decimal column a checkpoint writes. `l_discount` is 0.00 to 0.10 and so holds at most eleven values.

The rewrite is not free. The grouping below reads one more key per row, and a plan that only drops one product for it gets slower. The first version turned q06's `sum(l_extendedprice * l_discount)` into a grouping by discount and cost 8 M more, and q03's into a grouping by order and discount and cost 54 M more. The pass now only rewrites when the grouping below adds up fewer sums per row than the aggregate did. q01's four sums and one average become two sums below, and q03 and q06 have one sum either way and are left alone.

## Results

server3, SF1 native, one run per query in a fresh process, threads 1, millions of instructions. The first column is main before #2254, the second is #2254, the third is this change.

| query | before | #2254 | this |
|---|---|---|---|
| q01 | 883 | 861 | 806 |
| q03 | 390 | 392 | 392 |
| q06 | 169 | 169 | 169 |

The other nineteen queries are within 1 M of #2254, since the rule leaves every other plan as it was. Every answer is the same as main's at one thread and at eight, q01's averages included. The plan for q01 has the two groupings:

```text
Aggregate #5 groups=[#4.0, #4.1] aggregates=[sum(#4.4), sum(#4.5), sum((1 - #4.2) * #4.5), ...]
  Aggregate #4 groups=[#0.4, #0.5, #0.2, #0.3] aggregates=[sum(#0.0), sum(#0.1), count_star(), ...]
    Filter (#0.6 <= 10471)
      Get lineitem
```

## What is left

q01 is now 806 M and the rows no longer multiply anything. Its profile at one thread is `Aggregate::fold` at 304 M, `walk_once` at 124 M, and unpacking and expanding the stored columns at about 150 M. The adds are the smaller part. Most of the fold is placing each row in one of 396 groups by four keys, which is about 50 instructions a row, and that is the next thing to take apart. Two of the keys are one letter strings and two are packed decimals with a few values each, so a row's place is a few bits from each column, and building it a column at a time over the chunk rather than a row at a time over four columns should be much cheaper.

The grouping below also holds four `count_star` totals that are the same total, because the counts the averages bring each become a `count_star` once the column has no nulls. Adding them up once would take a few more instructions a row.
