# 136. The bitmap first in a sparse part

## What was slow

A scan whose table a join reduced reads each part at the rows the reduction left, see `Scan::read_reduced`. When the part keeps more than one row in eight it is read whole, unless a join's bitmap can cut it on one column, in which case that column is read at the rows and tested, and the other columns are read at what the test kept. When the part keeps one row in eight or fewer, the bitmap was never tested first. Every column was read at every row the reduction left, the key the link can give was worked out at every one of them, and the bitmap cut the chunk afterwards.

TPC-H q05 at SF1 reaches `lineitem` through the 46,008 orders of the year whose customers are in Asia. Those hold 184,082 lines, one in thirty two, so the parts were sparse. The suppliers of the region keep about one line in five of those, and the scan handed up 36,718. The order key worked out from the link and the key map, and the price, the discount and the supplier key read at all 184,082 rows, came to about a fifth of the query.

q07 is the same shape from the other end. It reaches `lineitem` through the 798 suppliers in France and Germany, 478,523 lines, one in twelve, and the orders of the two years whose customers are in the other country keep 11,723 of them.

## The change

The bitmap goes first in a sparse part as well. The column it tests is read at the rows the reduction left, the rows it keeps are what the pushed filter and the other columns are read at, and the bitmap is not tested again on the chunk. A bitmap measured keeping more than three rows in four is not used first, so a join that cuts little costs a second read call a part and nothing more.

## Measured

At SF1 on server2, one thread, against main at #2656, under a load average around 20.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q05 | 147 | 111 |
| q07 | 169 | 126 |

q03, q08, q09, q10, q12, q20 and q21 run the same number of instructions. The answers to all 22 queries are the same bytes as before at one thread and at six.
