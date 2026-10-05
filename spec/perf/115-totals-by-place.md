# Totals added up by place for a small map

Notes written on 5 October 2026, while finding out where the instructions of TPC-H q01 go.

## The question

q01 groups `lineitem` by return flag and line status, but the plan groups it one level finer first, by flag, status, discount and tax, adds up quantity and price for each of those, and works out the discounted price and the charge from the totals afterwards. That is about 400 groups. The four keys are packed codes, so a row's place in the map is worked out from its codes, and the map has 3468 places, one for every combination of codes with room for a null in each column. Counted in instructions over warm runs on one thread at SF1, q01 was 435 million, and 44 percent of it was `by_slot`, the pass of `PlaceSums::add` that adds each kept row into its group.

That pass is about 27 instructions a row. The adds are a few of them. The rest is the index of each kept row out of the filter's list, the row's place, the group the map holds there, a check for a place with no group yet, a check for a slot past the cells, and a bounds check on each of those loads.

The first idea was a pass per group over the chunk, adding every column under a mask of the rows in that group, for a chunk whose rows fall in only a few groups. It did nothing for q01, since the chunk does not fall in four groups but in about 400 at this level, and it was dropped.

## What changed

When the map is less than half as long as the chunk, `PlaceSums::add_places` adds every row into cells kept per place rather than per group, with no map in the loop. The rows the filter dropped are given one more place past the map, so the loop goes over every row in order with no list of rows and nothing to stop at. The dropped rows are found by cutting the list of kept rows in two wherever a half has a gap, since a run of kept rows with no gap ends as many rows after its first as it is long. On q01 about one row in seventy is dropped, and a walk of the kept rows to find them was seven instructions a row. The loop reads the columns eight rows at a time, so each column is checked against the rows once a block.

After the pass, `PlaceSums::touched` writes down the places that have rows and says whether the map has no group yet for any of them. If it does, the caller goes over the rows once and opens a group for the first row of each such place, which is the same order the old pass opened them in, so the groups come out in the same order. `PlaceSums::fold_places` then folds the places on that list into their groups and leaves their cells at nothing for the next chunk. A larger map, or one as long as the chunk, goes through the map as before.

## Measured

Single thread at SF1 on server2, the warm instructions of a query as the difference between eleven runs and three in one process, against main at #2530. The answers to all 22 queries are the same bytes as before. The machine was loaded and the cycles of q01 moved between 230 and 260 million on both sides from run to run, so only the instructions are given.

| query | instructions before (M) | after (M) |
| --- | --- | --- |
| q01 | 435 | 409 |

q04, q05, q07 to q10, q12, q13, q16, q18, q21 and q22 run the same instructions as before within one million.

## What this leaves

The add itself is now about 17 instructions a row, and what is left of q01 is mostly elsewhere. Folding the totals into the aggregate states is about 14 percent of the query, because it happens for every group and every call once a chunk, about 3800 folds for every 8192 rows. Keeping the totals from chunk to chunk and folding them once, when the map or the columns change and at the end, would take most of that away, but every place that reads a group's state would have to fold first. Unpacking the values and working out the places are about a tenth each, and both are a pass over the chunk that writes a vector the next pass reads back. A pass that reads the codes, works out the place and adds the values a block at a time, with nothing written in between, is the larger step after this one.
