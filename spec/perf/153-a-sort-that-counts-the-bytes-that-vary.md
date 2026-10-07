# 153. A sort that counts the bytes that vary

## What was slow

A sort whose keys all have a fixed width writes each row's keys into 24 bytes, with 16 more bytes for where the row arrived, and sorts those rows with a comparison sort. The comparisons already read a big endian word at a time, so what is left is the shape of the sort itself: about fourteen rounds of comparing and moving 48 byte rows for eighteen thousand of them. The final sort of TPC-H q16 is that size, and `Keyed::sort` took 11.8 million instructions on it, about 640 a row.

Most of those 24 bytes are the same in every row. q16 sorts by a count under 256, the ranks of a brand and a type, and a size under 50, so four bytes tell the rows apart and twenty never do.

## The change

From a thousand rows up to a million, `in_order` makes one pass that finds which key bytes differ from the first row's anywhere, and then sorts by those bytes alone, from the last that varies to the first, with a count and a stable scatter for each. It also checks in that pass whether the rows are already in the order they arrived. When they are, which is always so when one thread gathered them, the stable passes keep that order among equal keys and the arrival needs no pass. When they are not, the bytes of the arrival that vary get passes of their own before the key's.

Comparing the keys and then the arrivals is comparing those bytes in that order, so the answer is the order the comparison sort gives, row for row. Below a thousand rows the comparison sort is cheaper than 256 counts per pass. Above a million it is kept as well, because counting needs a second copy of the rows and a sort that large is one the memory limit has to see. The ranked arm, the normalized arm and each instance's own sort before it combines all go through it. A test checks it against the comparison sort for keys that tie, keys that vary in a few bytes and in all of them, and arrivals in and out of order.

## Measured

At SF1 on server2, one thread, against main at #2745.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q16 | 119 | 113 |
| q18 | 164 | 164 |
| q13 | 190 | 190 |
| q10 | 149 | 149 |
| q21 | 246 | 246 |
| q03 | 75 | 75 |

The other sorts in TPC-H are of a handful of groups or are a top N, so q16 is the one that moves. The answers to all 22 queries are the same bytes as before at one thread and at six.
