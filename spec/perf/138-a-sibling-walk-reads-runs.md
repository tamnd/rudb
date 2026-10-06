# 138. A sibling walk reads its rows as runs

## What was slow

A walk over a parent's siblings, the way q21 tests the other rows of an order, found each parent's children on the link as one run of rows. It then listed every row of the run, pushed each into a vector of row ids, and read the stored columns at those ids one at a time. At SF1 q21's first walk visits 783,594 sibling rows and reads three columns of each, about 2.35 million values, and the rows of one order sit next to each other in `lineitem`. Listing them and reading them one id at a time cost about 87 million instructions in `values_at`, 25 million in the pushes and 16 million in the extends, out of 387 million for the whole query.

## The change

The walk keeps the run the link gives, a first row and a length, and the batch holds runs instead of row ids. Only a batch whose parents come back out of order lists its rows, and it sorts them and coalesces them back into runs before it reads. The read splits the runs at the part boundaries and hands each part its runs. A packed column with no nulls unpacks each run straight through into a vector already as long as the batch, with the words, the width and the base held in locals, see `Vector::gather_runs`. Written as a map over a range, the closures loaded all three again for every value and the vector asked at each one whether it had room, about thirty instructions a value. Held in locals it is about fifteen, most of them the two loads and the shift `code_at` always did. Any other column reads the rows of the runs the way it read them before.

The nth set bit of a word, which the link uses to find where a parent's run starts, is found without a branch on its bits. It sums the bits of each byte, picks the byte the bit is in by a compare of all eight sums at once, and looks the bit up within the byte in a table. `pdep` would do it in one instruction, but it is microcoded on AMD before Zen 3 and takes far longer than the loop it replaces there. The second search a parent makes is for the first zero after the run before it, and that is the lowest bit set in the word, so it skips the select entirely.

The comparison a walk makes between a sibling and its row settles its operator once for the batch, so the loop over the siblings has no match in it.

## Measured

At SF1 on server2, one thread, against main at #2678, under a load average around 20.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q21 | 387 | 310 |
| q04 | 92 | 74 |

The other twenty queries run within three million instructions of what they ran before, and q22 is one under. The answers to all 22 queries are the same bytes as before at one thread and at six.
