# 116. One count asked once

## What q01 asked for

After the averages of q01 are split into sums and counts and the sums are factored by discount and tax, the grouping under the top one asks for two sums and four counts:

```text
Aggregate #4 groups=[flag, status, discount, tax] aggregates=[sum(l_quantity), sum(l_extendedprice), count_star(), count_star(), count_star(), count_star()]
```

Three of the counts started as `count(l_quantity)`, `count(l_extendedprice)` and `count(l_discount)`, one for each average, and the fourth is the `count(*)` the query asks for. None of the three columns has a null, so the pass that answers the null questions a store has settled turned each of them into `count(*)`. Nothing then noticed that the four were the same call. The totals by place in note 115 add one count per place, but each group folds every call into its own state, so every place folded the same count four times a chunk, about 400 places and 730 chunks.

## The change

`shared::once` finds the calls an aggregate asks more than once and asks each one once, under a projection that keeps the aggregate's index and its output order, so nothing above it moves. Only calls whose arguments are columns, or that have none, and that have no `FILTER` are merged, since two calls of a function of a row such as `random()` are two different answers. The no nulls pass runs it after it rewrites a count, because that is where the repeats come from. The grouping above reads the one count through the projection four times, over a few hundred rows.

The same chunk also asks fewer places for their counts. `PlaceSums::touched` went over the count of every one of the 3468 places a chunk to find the 400 or so with rows. Now it asks the places that had rows the chunk before first. Every row adds one to its place's count, so when those counts add up to the rows the chunk put in the map no other place can have any. A chunk whose rows went somewhere new still looks at every place.

## Measured

Single thread at SF1 on server2, warm instructions as the difference between eleven runs and three in one process, against main at #2566. The answers to all 22 queries are the same bytes as before.

| query | before (M) | after (M) |
| --- | --- | --- |
| q01 | 404 | RESULT |

OTHERS

## What this leaves

LEAVES
