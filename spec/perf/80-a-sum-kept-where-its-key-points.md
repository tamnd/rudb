# 80. A sum kept where its key points

## The problem

At eight threads q15 cost 673 M instructions against 455 M on one, q11 330 M against 261 M and q10 1892 M against 1721 M. The extra work was in one aggregate in each query: q15 sums revenue by `l_suppkey`, q11 sums stock value by `ps_partkey` and q10 sums revenue by `o_custkey` under the join, which eager aggregation put there.

Each key is an integer in a range the `dense` pass already knows, and each group gets only a handful of rows. The general table gives every instance of the aggregate its own table, so at eight threads each instance made nearly every group for itself, with a hashed bucket, a copy of the key and a fresh accumulator, and the merge then made them all a ninth time. With few rows to a group that cost was most of the aggregate, and it grew with the number of threads.

Note 42's `group_ranged` already answered `COUNT` this way for q13: one array per call, as long as the range, indexed by the key, and instances combined by adding their arrays. It turned away every other call.

## The change

`group_ranged` takes `SUM(x)` too when the argument is a signed integer or a decimal and the result is stored in 128 bits, which is every `SUM` of those types. Each sum keeps an `i128` array beside a count of the values that were not null, so a group whose values were all null answers null the way the general sum does.

An argument stored in 64 bits is read through the same `SignedBlock` the key is, where a null reads as a zero, so a row is a subtract, a compare and a 128 bit add with no branch. Those adds cannot overflow. An argument stored in 128 bits, which is what q11's `ps_supplycost * ps_availqty` is as a `DECIMAL(34,2)`, is added straight from the flat column and checks each add. An overflow is the same out of range error the general sum raises. Combining two instances checks its adds as well.

A value the range does not cover is still summed in the small map beside the arrays, as it was for counts.

## Results

server3, SF1 native, three copies of each query in one process, `perf stat` against main after note 79:

| query | threads | instructions before | after |
|---|---|---|---|
| q15 | 1 | 455 M | 396 M |
| q15 | 8 | 673 M | 400 M |
| q11 | 1 | 261 M | 198 M |
| q11 | 8 | 330 M | 240 M |
| q10 | 1 | 1721 M | 1605 M |
| q10 | 8 | 1892 M | 1685 M |

q15 now costs the same at eight threads as at one. With the machine idle, q15's task clock at eight threads went from 230 ms to 160 ms. Other people's builds had server3 at a load of 38 while the second set of numbers was taken, so the task clock is not in the table. The answers to all 22 queries are the same as before at one thread and at eight.

## What is left

The key still has to be a single signed integer with a known range, and every call has to be a count or a sum. `MIN`, `MAX` and `AVG` could be kept the same way. q16's `COUNT(DISTINCT ps_suppkey)` over three keys is the other query that costs more at eight threads than at one, and it is a different shape.
