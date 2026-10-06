# 141. Per row loops with nothing else in them

## What was slow

Four loops that q09 runs over every one of its 319,404 `lineitem` rows were written a row at a time, each with a push, a conversion that can fail or an early return in it. A loop like that cannot be turned into vector instructions, and each row pays for the capacity check, the branch and the error path even though none of them ever fires.

1. `Selection::from_predicate` pushed each kept row, so every row was a branch, a capacity check and a conversion to `u32`. A link join calls it to find the rows with a parent, and in q09 every row has one.
2. `LinkJoin::resolve` pushed each child row id after a fallible conversion to `u64`, and then pushed each parent row id after a fallible conversion to `u32`.
3. `extract(year from o_orderdate)` read each date through the mapping, summed the new year days it was past in a loop of its own, and wrote the answer through a closure that could fail, about 44 instructions a row.
4. An exact cast to a narrower integer pushed each value after a `try_from` that returned on the first one that did not fit.

## The change

Each loop now does only its own work and keeps the rest out of it.

1. `from_predicate` writes every row's index at the current length and moves the length on only for a row that is kept, which is what `Selection::from_indices` already asked kernels to do.
2. `resolve` checks the child ids for a negative one in a pass of its own and then converts them in a plain `extend`. The parent ids are converted in one pass that notes a parent too large to gather rather than returning from the middle.
3. The year reads the dates out into a column once and then makes one pass over that column for each new year day the dates span, a compare and an add with nothing carried between rows.
4. The exact cast narrows every value and notes whether any did not fit, then returns `None` after the loop if one did.

## Measured

At SF1 on server2, one thread, against main at #2684.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q09 | 426 | 376 |
| q14 | 61 | 59 |
| q12 | 109 | 107 |

q02, q04, q05, q06, q07, q08, q19, q20, q21 and q22 run within one million instructions of before. The answers to all 22 queries are the same bytes as before at one thread and at six.
