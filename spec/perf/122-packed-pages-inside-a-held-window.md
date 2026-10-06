# 122. Packed pages inside a held window

## What was slow

q01 groups by `l_returnflag`, `l_linestatus` and, after the rewrite that turns its sums into totals per place, by `l_discount` and `l_tax` as well. The first chunks of a scan are cut by the date filter and come through flat, so the coded map reads the two decimal columns by value and holds a window for each, `Window(0, 12)` and `Window(0, 10)`. Every chunk after that is a whole packed page, four bits wide with a base of zero. A packed page is held by its base and width, and since the map was held by a window the page counted as moved off it. So every page went through `window_of`, which widened its codes to `i64` values with `signed_block` and placed each row as its value less the bottom of the window. A debug run of six q01s counted 4338 chunks like that and 42 that were not.

## The change

A packed page whose base is at or above the bottom of the held window, and whose highest possible value (the base plus 2^width less one) still lands inside the window, is now placed out of its codes. Each code gets the distance from the window's bottom to the page's base added to it. `Places::Bits` and `Places::CodedBits` carry the bottom of the window they are placed against, and with it their identity is the window rather than the base and width, so `same_as` and `hold` treat them the way they treat a column read by value and the map lives on. `by_value` and `hash_of` answer for them too, so a row that misses the map is hashed on its own as before.

This only applies to a key of more than one column. A key of one column also finds its runs while it settles the window, and the fold by runs is worth more there than skipping the widening.

## Measured

Single thread at SF1 on server2, warm instructions as the difference between eleven runs and three in one process, against main at #2601. The answers to all 22 queries are the same bytes as before at one thread, and q1, q3, q5 and q9 also at six.

| query | instructions before (M) | after (M) |
| --- | --- | --- |
| q01 | 331 | 320 |

`signed_block` was 2.9 percent of q01 and is gone. Part of what it cost comes back in `Coded::places`, which now unpacks the codes itself.

## What this leaves

`by_place` is 29 percent of q01 and `Packed::unpack` is 12 percent, most of that being the summed columns unpacked into `u64` values before the per row adds. Reading those adds out of the packed words directly, and keeping the place cells across chunks so `fold_places` and the memset are paid less often, are the next steps.
