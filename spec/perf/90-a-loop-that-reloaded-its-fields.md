# 90. A loop that reloaded its fields

## The problem

After note 89, q01 groups by flag, status, discount and tax below its grouping by flag and status, and at one thread `Aggregate::fold` was 304 M of its 806 M instructions on its own, not counting the kernels it calls. The annotated profile put most of that on one loop, the one that reads each row's slot out of the direct map once a key has more columns than `Coded::look_up` takes. Its body was five loads, three compares and a store per row. The places, the map and the slots are all fields the fold borrows out of the aggregate, and the store into a slot could have changed any of them as far as the compiler knew, so each row loaded the three pointers and two lengths again before doing the one load it needed.

## The change

The loop is now `table::slots_from`, a function over three plain slices that fills slots from a given row on and gives back the first row whose place holds nothing. The caller is the same loop as before around it: a row that finds nothing opens its group and the call goes on from the row after.

## Results

server3, SF1 native, one run per query in a fresh process, threads 1, millions of instructions:

| | before | after |
|---|---|---|
| q01 | 806 | 725 |
| q01 by flag, status, discount and tax, written by hand | 805 | 725 |
| `Aggregate::fold` in q01, itself | 304 | 228 |

The other queries do not reach this loop, and moved by what main moved by in between. Every answer is the same at one thread and at eight.

## What is left

q01 is now 725 M. `Aggregate::fold` is still 228 M of it, which is the places written a column at a time and the rest of the fold, and `walk_once` is 108 M. `Coded::look_up` reads the map straight from one or two dictionary columns without writing a place per row first, and taking it to four columns, two of them packed, is the next step for this query.
