# 88. A few groups read by slot

## The problem

q01 has four groups, and its rows are in no order on the key, so a chunk changes group every 2.81 rows. The fold still went by runs. `slot_runs_of` made a pass over the slots to cut the chunk into runs of one group, and then `many_runs` visited each run once for all five totals the query shares. That was worth it when it was written, since one walk of the runs for five calls is much less than five walks, and it is still the right way to read a chunk sorted on its key. At 2.81 rows a run, though, the visit is most of what a run costs. Each one loads its slot, bounds checks a slice of every column, loads the group's totals and stores them back, and then there are fewer than three rows to add.

The obvious other way, adding each row into its group's totals, has a problem of its own. Two rows next to each other in one group add into the same memory, so the second waits for the first one's store to reach the load, and on q01 that is most rows.

## The change

A chunk with at most 16 groups whose first 64 rows change group more than once every four rows goes by slot. `short_runs` answers that from the first block alone, which is enough for the chunks it is asked about: a key the rows are sorted on has long runs from its first row, and q01's keys change group from its first row too.

`update_shared_slots` takes the same calls `update_shared_runs` takes and gives the same answers, and the two share everything except the walk. The slot walk holds the totals four times over and row `r` adds into copy `r % 4`, so two rows of one group next to each other never add into the same memory. The copies are added together once at the end of the chunk. A row in no group adds into a group past the last one, which nothing reads, so the loop has no branch for the rows a filter dropped. The rows are read in blocks of eight, so that a row's read of each column needs no bounds check of its own.

## Results

server3, SF1 native, against main at `0c9d5512`. Instructions are three copies of the query in one process at one thread. Cycles are five copies in one process, three runs each, with server3 at a load of about 37.

| | main | this |
|---|---|---|
| q01 instructions | 883 M | 884 M |
| q01 cycles, one thread | 561 to 577 M | 520 to 528 M |
| q01 cycles, eight threads | 552 to 556 M | 523 to 534 M |

The instructions come out level: `slot_runs_of` is gone from the fold, and the slot walk runs about as many as the run walk did. What changes is that the walk no longer waits. Every row adds into memory no row near it touches, where before each run loaded totals the last run of that group had just stored. The other 21 queries are within 5 M instructions of main, and all 22 answers are the same as main's at one thread and at eight.

## What is left

The slot walk is still about 40 instructions a row for five totals and a count, where the adds themselves are about 25. The compiler runs out of registers and reloads where the totals start once a column. Holding the totals in a fixed array on the stack with the index masked into range was tried, and the mask cost what the bounds check had cost. Reading a column at a time over an index of where each row's totals start should keep every pointer in a register and is the next thing to measure.

Unpacking the stored decimals into `i64` before they are summed is still 14 percent of q01. A group's total of packed values is its count times the frame base plus the total of the packed codes, so the codes could be summed as they are.
