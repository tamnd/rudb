# 139. Rows far apart read from locals

## What was slow

A gather of packed rows spread too far apart to unpack the span they cover reads one code a row, see `Packed::values_at` and `Packed::codes_into`. The loop read each code through `self`, so the words, the width and the offset were loaded again for every row, and it pushed each value, so the vector asked at every row whether it had room. In TPC-H q21 the scan of `lineitem` reads the two dates of the 247,140 rows the suppliers' join lists reach, and that gather cost about fifty instructions a value.

## The change

The loop holds the words, the width and the offset in locals and writes each value into an answer already as long as the rows asked for, the way `Vector::gather_runs` reads a run after note 138. The prefetch of the row sixteen on takes the words and the word index, so it reads nothing through `self` either. A code is the two loads and the shift `code_at` always did.

## Measured

At SF1 on server2, one thread, against main at #2681, under a load average around 16.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q09 | 456 | 443 |
| q21 | 310 | 304 |
| q07 | 113 | 110 |
| q14 | 63 | 61 |
| q03 | 87 | 85 |

q04, q06, q10, q12 and q19 run within two million instructions of before. The answers to all 22 queries are the same bytes as before at one thread and at six.
