# 145. A LIKE that walks only the rows the sketch keeps

## What was slow

TPC-H q13 filters orders with `o_comment NOT LIKE '%special%requests%'`. The comment column is compressed with FSST and has a sketch of grams for each row, so the reader can tell from a single word that a row cannot hold the pattern. The sketch did its job: only about 37,000 of the 1.5 million comments got past it at SF1. The cost was in the rows it threw away. The reader still stepped over every row one at a time, adding its length to an offset, checking the sketch, writing a flag for the row, and then turned the flags into a list of rows. Fitting the cost over patterns that pass more or fewer rows put the walk at about 260 instructions a row that is searched, and the stepping and flags at about 41 instructions a row of every row, which is most of the filter.

## The change

1. The compressed chunk reader takes the rows to search as an iterator and adds up the lengths of the rows it skips in one sum, so a skipped row is a load and an add. It returns the rows that hold the pattern as a list rather than a flag a row.
2. The reader feeds it the rows whose sketch word has every bit the pattern needs, so rows the sketch throws away never reach the loop that searches.
3. For NOT LIKE with no nulls, the rows kept are the runs between the rows that hold the pattern, and each run is filled in one go.

## Measured

At SF1 on server2, one thread, against main at #2693.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| `count(*)` of orders with the q13 NOT LIKE | 100 | 59 |
| `count(*)` of orders with LIKE `'%special%requests%'` | 102 | 59 |
| q13 | 232 | 191 |

A pattern the sketch cannot narrow, such as `'%ironic%'`, runs as before. q09, q16 and q21 run as before. The answers to all 22 queries are the same bytes as before at one thread and at six.
