# 144. A distinct count by the run, and an IN list by the mask

## What was slow

Two parts of TPC-H q16 did per row work that only needed doing once.

1. `count(DISTINCT ps_suppkey)` holds each value beside the index of its group's set, sorts the held values by set, and gives them out. The sort puts a set's values next to each other, but the loop that gave them out still found the set and its accumulator again for every value, and called the accumulator's general `update` with a `Value` for every value that was new to the set. For a `count` that update is one added to a number. In a profile with call stacks the give was about a sixth of q16.
2. `p_size IN (49, 14, 23, 45, 19, 3, 36, 9)` was tested with eight compares a row over all 200,000 parts. The list kernel writes out one, two and three members by hand and folds over anything longer.

## The change

1. The give walks the sorted values a run of one set at a time. It finds the set and the accumulator once for the run, inserts every value of it, and tells a `count` how many were new once at the end of the run through a new `Accumulator::count_more`. Any other aggregate over a distinct set still gets an `update` for each new value, in the order they came.
2. When a list of integers has more than three members and every one is within 64 of the smallest, the kernel keeps the smallest and a 64 bit mask with a bit for each member. A row is then a subtract, a range check and a shift. A list that does not fit the window keeps the compares.

## Measured

At SF1 on server2, one thread, against main at #2690.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q16 | 133 | 120 |

q12, q13, q19 and q22 run within one million instructions of before. The answers to all 22 queries are the same bytes as before at one thread and at six.
