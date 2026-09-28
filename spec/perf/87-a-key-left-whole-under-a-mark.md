# 87. A key left whole under a mark

## The problem

q01's date filter keeps 98 percent of `lineitem`. Since #1700 a filter that keeps that much and feeds an aggregate directly marks the rows it kept instead of cutting them out, so the arguments of the eight aggregates are read over every row and a dropped row lands in no group. The keys were the exception. `Aggregate::rows_of` cut them to the kept rows before anything read them, so `l_returnflag` and `l_linestatus` were gathered for 98 percent of every chunk, and the fold then moved every kept row's slot back to the row it came from with `spread_slots` so the slots lined up with the arguments again.

A cycle profile put the gather at 5.6 percent of q01, which looked small. The instruction profile is the one that says what the work is, and there `Vector::gather` was 10.2 percent of q01 on its own. It is a tight loop that retires several instructions a cycle, so its share of the cycles is about half its share of the work. `Aggregate::fold` was another 21 percent, and moving the slots back was a pass over every row inside it.

## The change

A marked chunk's keys stay whole. `rows_of` no longer cuts them, and `Rows::settled` cuts them with everything else for the consumers that want the kept rows alone, which is every path but the fold into an instance's own table.

The fold reads the keys whole only when the direct map answers the chunk, which is the map from dictionary codes to groups that q01's two keys go through. Every other way of finding a group, the hash table and the run pass and the closed groups, would open a group for a row the filter dropped, so for those the fold cuts the keys itself before it looks at them, the way `rows_of` used to.

With the keys whole the map finds a slot for every row, dropped rows included. Two things keep a dropped row from counting.

1. A row the map has no group for yet is looked up in the table only if the filter kept it. A dropped row whose key is new is left in no group rather than opening one, so a key that only dropped rows hold never shows up in the answer. That check is a binary search over the kept rows, and it only happens on a miss, which is the first row of each group.
2. After the slots are found, the rows between the kept ones are put in no group. `gaps` walks the kept rows and, when the sixteenth kept row from here is fifteen past this one, takes all sixteen with one compare. At 98 percent kept that is most of them, so the walk costs far less than a pass over every row. Runs found over every row, which is what a one column key gets, are cut at the same stretches by `cut_runs` and handed on as runs.

## Results

server3, SF1 native, three copies of each query in one process at one thread, `perf stat -e instructions:u` against main at `76fddf60`, per query:

| query | instructions before | after |
|---|---|---|
| q01 | 1077 M | 889 M |

The other 21 queries run within 1 M of the instructions they ran before, and the answers to all 22 are the same as before at one thread and at eight. An instruction profile of q01 has `Vector::gather` gone, 1108 samples before, and `Aggregate::fold` down from 2322 samples to 1642. Between them that is the whole 188 M.

q01 cycles, five copies a process, two runs each, with server3 at a load of about 45:

| threads | cycles before | after |
|---|---|---|
| 1 | 611 to 613 M | 542 to 562 M |
| 8 | 639 to 651 M | 548 to 604 M |

## What is left

The fold now spends a quarter of q01's instructions in `many_runs`, walking the runs of rows that share a group, and another 18 percent in the rest of `Aggregate::fold`. q01 has only four groups, so runs may not pay for themselves there at all. A pass that adds each row into its group's totals directly, with the totals for four groups held in registers, is the next thing to measure.

`unpack_block` is 14 percent, which is the packed argument columns read out to `i64` for the arithmetic and the sums. The arithmetic `Prepared::step` does is another 11. An aggregate that summed the packed codes and added the frame base times the count at the end would skip the first of those for `l_quantity`, which is summed as it is stored.
