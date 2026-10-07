# 146. A sibling value beside a flag, and a packed code in one load

## What was slow

TPC-H q21 asks, for each late line of a Saudi supplier, whether another supplier on the same order was also late. The walk answers that by reading the lines of the order from the link and testing them, about 780,000 siblings for 157,000 lines at SF1. Two things cost more than they had to. Each sibling's value was held as an `Option<i64>`, and the conditions on the sibling alone, such as `l3.l_receiptdate > l3.l_commitdate`, picked the rows that passed by building a smaller chunk and then the values were read again from it, so a sibling was copied and branched over several times before it was compared. Below that, every packed code was read by loading two words, joining them into 128 bits and shifting, which came to about 17 instructions a value.

## The change

1. The walk holds the sibling values as plain `i64` with a `bool` beside each one that says whether the row has a value and passed the conditions on the sibling alone. The conditions mark the rows they keep in that flag instead of handing back a smaller chunk to read the values from again, and the compare runs over the values and flags with no `Option` and no branch on null.
2. A packed code at most 57 bits wide is read with one unaligned eight byte load from the byte it starts in and a shift, since it starts at most seven bits into that byte. Wider codes and a code near the end of the words go the old way. The gather over runs of packed rows uses the same read rather than its own copy of the two word one.

## Measured

At SF1 on server2, one thread, against main at #2697.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q21 with only the late test on the sibling | 236 | 219 |
| q21 with only the supplier test on the sibling | 166 | 157 |
| q21 with both tests, as a count | 240 | 209 |
| q21 | 300 | 269 |
| q09 | 375 | 366 |

The other queries read packed codes too and run the same or up to three million instructions fewer. The answers to all 22 queries are the same bytes as before at one thread and at six.
