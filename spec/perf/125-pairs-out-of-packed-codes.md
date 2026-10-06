# 125. Pairs out of packed codes

## What was slow

q01's two summed columns, `l_quantity` and `l_extendedprice`, are packed runs, and `PlaceSums::ready` read each of them out into a vector of `u64` before the pass that adds them by place. That is a store of sixteen bytes a row and a load of the same sixteen bytes back in `add_pairs` (note 123). After note 124, `Packed::unpack` was 9 percent of q01's cycles, nearly all of it these two columns.

## The change

`ready` now leaves a packed call's codes where they are and only marks the call as pending. `add` and `add_places` take the same inputs `ready` was given, and read the pending calls out the first time they want them.

When the two calls of a pass by place are both pending, `add_places` hands both packed runs to the new `lanes::add_pair_codes`. For each group of eight rows it reads eight codes of each run with the shuffle, shift and mask the other lanes code uses, widens each half to 64 bits, makes four pairs with an unpack low and high, and adds each pair and a one for the count into the row's cells. The places are checked eight at a time as in `add_pairs`. It takes runs that start on a group of eight codes and are no wider than 25 bits, and the last few rows of a chunk, which the sixteen byte loads would read past, are added a code at a time. Any other pair of calls is read out and added as before.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process, against main at #2611. The answers to all 22 queries are the same bytes as before at one thread, and q1 also at six.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q01 | 241 | 227 | 160 | 141 |

## What this leaves

`fold_places` is now the largest cost in q01 after the adds. Every chunk folds about 400 places into what each group is owed and clears their cells. Keeping the cells across chunks while the map, the calls and the bases stay the same would fold them once a run of chunks.
