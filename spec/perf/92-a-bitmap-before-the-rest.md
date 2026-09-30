# 92. A bitmap tested before the rest of the columns

## The problem

In q05 at SF1 the orders of 1994 hand `lineitem` its exact rows through the link, about one row in seven, and the suppliers of ASIA hand it a bitmap on `l_suppkey` that keeps one in five of those. A part whose exact rows keep more than one row in eight is read whole, so every part of `lineitem` was read whole. That decoded every `l_orderkey` of the part, which is a delta coded run length page and cost 66 M instructions between `decode_chunk` and the expansion of its runs. The four columns were then gathered at the exact rows, 56 M in `Packed::values_at`, and only after that did the bitmap throw four fifths of those rows away.

## The change

`Scan::read_reduced` in `rudb-exec` now takes a dense part too when a join above left a bitmap on a column the scan reads. It reads that one column at the exact rows, tests it against the bitmap, and reads the other columns at the rows the bitmap kept. The bitmap is not asked again for that chunk afterwards, and it still answers to the same count, so one that stops paying stops being asked here as well. A part with no such bitmap reads whole as before.

## Results

Measured on server3 at SF1, one run in a fresh process, threads 1, millions of instructions, against main at #2277:

| query | before | after |
|---|---|---|
| q05 | 444 | 409 |

No other query moved, and every answer is the same at 1 and 8 threads.
