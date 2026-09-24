# Other columns read only at the rows a tight join keeps

TPC-H q17 joins all of lineitem to the parts of one brand and one container, which is about two hundred parts of two hundred thousand. The part scan hands the lineitem scan a bitmap over part keys, and the bitmap keeps about one lineitem row in a thousand. The scan still decoded every row of every column it read.

## What it cost

At one scale factor on one thread q17 took 0.79 G instructions against DuckDB's 0.74 G. A third of that was decoding lineitem pages and a fifth was copying the decoded keys into a block for the bitmap test. The quantity and the price were decoded for all six million rows, and all but about six thousand of those rows were thrown away by the bitmap before anything read them.

The scan could already put off reading a column until the filters had run. It read the other columns first, ran the filters, and then read the put-off columns only at the kept rows. This was only done for string columns, because those are the ones that cost a lot to decode. An integer page was decoded whole and then picked from, so putting it off saved nothing.

## The change

There are three parts.

1. An integer page now reads single rows when its top level allows it. That covers a constant, packed units, and strides or dictionary codes over packed units. Within a unit where more than one value in 32 is wanted, the whole unit is unpacked and the wanted values are picked out, because past that point finding each value on its own costs more. Run length and delta chunks have to walk everything before a row to find it, so those are still decoded whole. A read of more than one row in eight of a part also decodes the whole part.
2. Once a join's bitmap or filter has seen 65536 rows and kept fewer than one in 16, the scan puts off every column that neither the pushed filter nor a join key reads, whatever its type. It reads the key first, runs the bitmap, and reads the rest at the rows that are left.
3. The kept row numbers come back as a dictionary over the row numbers of the whole part. Turning them into positions used to lay out all 8192 numbers of the part for every chunk, which cost more than the reads it saved on q11. They are now worked out from the codes.

## Results

Instructions at one thread, before and after, with DuckDB after that:

| query | before | after | DuckDB |
|---|---|---|---|
| q17 | 0.789 G | 0.582 G | 0.733 G |
| q20 | 0.727 G | 0.606 G | 0.796 G |
| q09 | 1.807 G | 1.750 G | 1.765 G |
| q11 | 0.157 G | 0.159 G | 0.147 G |

Wall time for q17 went from 66 ms to 46 ms on one thread and from 18 ms to 15 ms at default threads. q20 went from 62 ms to 47 ms on one thread. No other query moved by more than one percent, and every answer is the same as before. What is left in q17 is the key column itself, which is still decoded whole and copied into a block before the bitmap test.
