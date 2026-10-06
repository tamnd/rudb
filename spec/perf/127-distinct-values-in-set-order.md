# 127. Distinct values given in set order, and coded sort keys ranked by code

## What was slow

TPC-H q16 counts the distinct suppliers of 18,314 groups made of 118,274 rows. Each row went to its group's BIGINT distinct set right away, and the rows of a chunk come in no group order at all. A row read the set where it sits in `seen`, the values the set points at, and the accumulator beside it. Those are three arrays of about a megabyte or more each, so each read missed the second level cache. At one thread `BigIntDistinct::insert` was 14 percent of the query's cycles, about 160 cycles a row, and the distinct call as a whole was about a fifth.

The final sort of q16 is 18,314 rows by the count, two strings and the size. The strings are ranked before the rows are sorted, so a row compares as bytes. The ranking hashed every row's bytes, and those bytes were fetched out of storage one row at a time. That was most of the sort, about 9 percent of the query.

## The change

Once a table holds 1,024 distinct sets or more, a value offered to a BIGINT set is held instead, as the index of its set and the value, pushed onto one vector. Every 65,536 values, and before anything reads or moves the groups, the held values are sorted by set and given to the sets in that order. The sort is a byte of the set index at a time from the lowest, a count and a scatter each pass, so it is stable and the values of one set keep the order they came in. Each set and its accumulator see exactly what they saw before, a row at a time, and so the answers are the same even for a call whose answer depends on that order. Giving them out walks `seen` and the accumulators from front to back. Once any value is held, every later one is held too, so a set never sees a later value before an earlier one. `Building::settle` gives out what is held, and it already runs before a merge, a scatter and the end of a pass.

A sort key whose every chunk is codes into one shared stable dictionary is now ranked by its codes. That is how a grouped string key comes out of the group table. A row is a load from an array indexed by its code. Only the codes the rows use are read out of the dictionary and sorted, and equal strings take one rank between them, so a dictionary that holds a string twice still ties on it. A key in any other form is hashed as before.

## Measured

At one thread at SF1 on server2, against main at #2618, eight runs of q16 in one process. The server's load average was around 30 throughout, so the cycles are the mean of seven runs of each binary, interleaved. The answers to all 22 queries are the same bytes as before, at one thread and at six.

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 1,574 | 1,354 |
| held values and coded sort ranks | 1,546 | 1,210 |

Holding the values alone, measured on its own earlier, was 71 M more instructions over the eight runs for the sort and about 8 percent fewer cycles for the reads. Ranking by code saves about 12 M instructions a run. Giving the values out 16,384 at a time rather than 65,536 took a quarter of the page faults away but was 1,300 M cycles, because a smaller batch has fewer values for each set and the walk over the sets is less of a front to back read.
