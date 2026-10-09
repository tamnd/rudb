# 171. A key held as its runs

## What was slow

`l_orderkey` is stored as runs of about four rows, one run an order, and `ps_partkey` the same way in `partsupp`. A part is decoded once and held, and it was held flat: the runs were written out a row at a time, and then q18's grouping by `l_orderkey` compared every row with the one before it to find the same runs again. That search for the ends of the groups was about a quarter of the query.

## The change

A run length part of a `BIGINT`, `INTEGER` or `DATE` column whose runs average at least three rows is held as its runs, with the rows written out flat beside them. The decoder reads the chunk's run values and lengths once and hands back both, the rows written the way a plain decode writes them, so the flat form costs what the part cost before and the runs are a value and an end a run on top of it.

The two forms have different readers. An aggregate closing groups over a key held as runs reads the ends, which are where its groups are, and leaves the key alone rather than opening it. Every other reader sees the flat rows: `data()`, the accessors a row at a time, and a gather, which takes the gather of the rows rather than walking positions through the runs. A cut of a vector laid out this way is a cut of both forms, so a chunk of a scan is laid out already.

A dictionary over the runs, which is what a filter leaves on a key it did not copy, reads the rows beside them too, so its signed gather and its flatten are what they were over a flat part. Laying the validity of a scan's pieces end to end no longer asks a piece with no nulls about each of its rows.

A gather of runs that were not laid out walks the ends, galloping to a far row from the run the last row was in, and lays the runs out once it has walked as many rows as there are runs. The check that the ends increase and the decode of the run lengths no longer branch a run.

Laying the runs out only when something asked for a row was tried first. q18 got the same win, but a join or a filter asked within a few rows, so every part paid the decode and then a second copy, and q09, which decodes `lineitem` on every run, went up by 58 million instructions.

## Measured

At SF1 on server3, one thread, millions of instructions a run, which is half the difference between a query run four times in one process and the same query run twice, against the build of #2880. The answers to all 22 queries are the same bytes as main's at one thread and at four.

| query | main | this change |
| --- | --- | --- |
| q05 | 81.4 | 79.9 |
| q10 | 72.0 | 70.7 |
| q12 | 66.1 | 64.6 |
| q18 | 169.3 | 143.9 |
| all 22 | 1,887.4 | 1,860.4 |

Every other query moves by under a million. The most any goes up is 0.7 million on q20 and 0.6 million on q21, which is the value and the end of each run kept on top of the rows.
