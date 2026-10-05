# 117. Totals owed

## What a chunk paid for

Note 115 adds a chunk's rows up into cells per place of the coded map, and note 116 finds the places with rows without looking at every place. What was left once a chunk ends is the fold: each of the 400 or so places q01 has rows in folds its two sums and its count into the accumulators of its group. That is `fold_wide` per call, which finds the state, matches on what kind it is, checks an `i128` add for overflow and marks the state as having seen a row. It came to about a hundred instructions a group a chunk and an eighth of q01, paid every 8192 rows, for totals nobody reads until the end.

## The change

`PlaceSums` keeps what each group is owed, a plain `i128` total and an `i64` count per call, beside how each call is fed. `fold_places` adds a place's cells to its group's entry and puts the group on a list the first time, and `settle` folds every entry on the list into the accumulators and clears it. A chunk whose calls are fed differently from what is owed settles first, and a total that would leave its `i128` is folded straight in.

Every adding path into the accumulators is a sum or a count, so what other chunks fold in the old way in between adds up the same in either order. What has to settle is anything that reads a group's state or moves the groups somewhere else, so `Building::settle` is the first thing done by finishing, merging (both tables), scattering into partitions, scattering locally, installing a limit's groups, and folding slots in from another table. The first four take the table by value, so nothing can read it after them without going through one of them.

## Measured

Single thread at SF1 on server2, warm instructions as the difference between eleven runs and three in one process, against main at #2582. The answers to all 22 queries are the same bytes as before.

| query | before (M) | after (M) |
| --- | --- | --- |
| q01 | 385 | 365 |

q03, q12 and q18 run the same instructions as before.

## What this leaves

Per row, q01 is now the add into the place cells at about 13 instructions, working out the places from the four key columns at about six, and unpacking the two summed columns at about six. Those are three passes over every row of the chunk, and the next step is one pass that reads the packed codes of the key and the values and adds them where they land, with no vector of places or values written in between.
