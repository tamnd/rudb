# 121. Bitmap tests in lanes

## What was slow

When a scan has both a pushed filter and a join bitmap, the filter runs first over the whole chunk and each bitmap is then asked only about the rows the filter kept, through `Domain::retain`. In q20 that is lineitem's date range, which keeps about a seventh of the rows, followed by the bitmaps of the part and supplier keys the build sides hold. `retain` read the key of each kept row with `Packed::code`, which works out the word the code is in, loads it, checks whether the code runs into the next word, shifts and masks, and then tested the bit with another load. That was about 28 instructions a row, and `retain` was 14 percent of q20 and 5 percent of q5.

## The change

`Packed::retain_set` does the same test eight rows at a time with AVX2. For a group of eight rows it works out each row's bit position with one multiply, reads each code's four bytes with one gather, and shifts and masks them in lanes. The offset into the bitmap is the code plus the shift between the two bases, clamped to the bitmap's range with an unsigned minimum, and each row's 32 bit word of the bitmap is a second gather. A variable shift puts each row's bit in the sign bit and a `movemask` makes eight bits of the answer. The rows kept are moved to the front of the group with one permute, from a table of the 256 ways eight answers can fall, and stored where the kept rows end. Rows are in order, so the last row of a group says whether its reads stay inside the bytes, and the last few rows of the list go one at a time as before.

It takes widths up to 25 bits, a bitmap of under 2^31 bits and a shift between the bases under 2^30 either way, so every lane fits 32 bits, and anything else goes the old way. A key column with nulls is tested the same way and its null rows are dropped after.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process, against main at #2597. The answers to all 22 queries are the same bytes as before at one thread, and q3, q5, q9, q17, q20 and q21 also at six.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q20 | 175 | 138 | 125 | 118 |
| q05 | 158 | 149 | 143 | 137 |

q09 and q17 run the same instructions as before. Against DuckDB in the same run, q20 takes 394 M cycles there and 105 M here.

## What this leaves

Cycles fell less than instructions because a gather on this AMD host costs several cycles, and the bitmap reads are now waits on memory more than work. The rest of q20 is spread out, with the pushed filter's mask, the scan's decoding and the part name `LIKE` the largest parts.
