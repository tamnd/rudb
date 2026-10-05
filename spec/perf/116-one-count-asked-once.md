# 116. One count asked once

## What q01 asked for

After the averages of q01 are split into sums and counts and the sums are factored by discount and tax, the grouping under the top one asks for two sums and four counts:

```text
Aggregate #4 groups=[flag, status, discount, tax] aggregates=[sum(l_quantity), sum(l_extendedprice), count_star(), count_star(), count_star(), count_star()]
```

Three of the counts started as `count(l_quantity)`, `count(l_extendedprice)` and `count(l_discount)`, one for each average, and the fourth is the `count(*)` the query asks for. None of the three columns has a null, so the pass that answers the null questions a store has settled turned each of them into `count(*)`. Nothing then noticed that the four were the same call. The totals by place in note 115 add one count per place, but each group folds every call into its own state, so every place folded the same count four times a chunk, about 400 places and 730 chunks.

That turned out to be cheap. A count folds into its state as one add, and the profile of `fold_wide` moved by about three percent with the repeats gone, because what it spends is the two sums, an `i128` add with an overflow check each. The repeats are taken out anyway, since a query that repeats a call that is not a count pays the full price for each one.

## The change

`shared::once` finds the calls an aggregate asks more than once and asks each one once, under a projection that keeps the aggregate's index and its output order, so nothing above it moves. Only calls whose arguments are columns, or that have none, and that have no `FILTER` are merged, since two calls of a function of a row such as `random()` are two different answers. The no nulls pass runs it after it rewrites a count, because that is where the repeats come from. The grouping above reads the one count through the projection four times, over a few hundred rows.

The same chunk also asks fewer places for their counts. `PlaceSums::touched` went over the count of every one of the 3468 places a chunk to find the 400 or so with rows. Now it asks the places that had rows the chunk before first. Every row adds one to its place's count, so when those counts add up to the rows the chunk put in the map no other place can have any. A chunk whose rows went somewhere new still looks at every place.

## Measured

Single thread at SF1 on server2, warm instructions as the difference between eleven runs and three in one process, against main at #2566. The answers to all 22 queries are the same bytes as before.

| query | before (M) | after (M) |
| --- | --- | --- |
| q01 | 404 | 385 |

q04, q12, q13 and q18 run the same instructions as before. Nearly all of the 19 million comes from asking fewer places for their counts. Asking only the places of the chunk before was tried first and went the other way, to 412 million, because q01 has groups with a row in every few chunks, so nearly every chunk fell back to the pass over every place and paid for the list as well.

## What this leaves

The fold is now the largest cost a chunk pays once rather than per row, about an eighth of q01: two sums and a count for each of about 400 groups, every 8192 rows. Keeping the totals by place from chunk to chunk and folding them when something reads the states would take it away, and that needs every reader of a group's state to fold first. Per row, the add is about 13 instructions with the place, two values and a four lane add, and it waits on the store of the row before it when two rows share a place. Unpacking the values and working out the places are each about a tenth of the query and are the other half of a fused pass.
