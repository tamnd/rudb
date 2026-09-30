# 93. A scan's own filter run before the rest of the columns

## The problem

In q03 at SF1 the orders before 1995-03-15 hand `lineitem` its exact rows through the link, 588,507 of its six million rows, which is about 800 rows a part and under the line where a part is read at its rows rather than whole. `Scan::read_reduced` read all four columns the scan projects at those rows, the order key, the price, the discount and the ship date, and only then ran the filter on the ship date, which keeps 30,519 of them, one row in twenty. The order key is a delta coded run length page and the other three are packed, so nineteen of every twenty gathers from them were thrown away. q07 has the same shape.

## The change

`read_reduced` in `rudb-exec` now reads only the columns the pushed filter reads at the exact rows, runs the filter over them, and reads the other columns at the rows it kept. The filter is not run again on the chunk afterwards. A part whose zones settle the filter, a scan whose filter reads every column it projects, and a filter measured keeping more than half its rows all read the way they did before.

## Results

Measured on server3 at SF1, one run in a fresh process, threads 1, millions of instructions, against a build before #2290 so only the two queries this touches are listed:

| query | before | after |
|---|---|---|
| q03 | 378 | 318 |
| q07 | 346 | 287 |

Every answer is the same at 1 and 8 threads.
