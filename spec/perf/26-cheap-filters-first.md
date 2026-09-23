# Cheap filters first

Notes written on 23 September 2026, after #1584, while finding out why q03 got slower after #1576.

## The question

q03 went from 5.93 G to 6.85 G instructions over five runs at SF1 between two commits of main, and the commit that did it was #1576. That change made `Chunk::select` gather a bit packed column right away instead of carrying a selection over it, which is the right call when most of the rows are kept, since the next operator reads them all anyway. It is the wrong call when the next step throws nearly all of them away.

That is what the native scan did. For each chunk it ran the filter pushed into it, narrowed every column to the rows the filter kept, and only then ran the runtime filters the joins above had built. In q03 the date filter keeps about half of lineitem and the join to orders keeps about one row in a hundred of those, so the scan unpacked about fifty rows for every row a join ever read.

## What changed

The runtime filters come in two kinds. An exact bitmap, which a join builds when its keys are integers close together, costs a subtraction and a bit test a row. A Bloom filter costs a hash a row. The scan now runs the bitmaps before the pushed filter, and it runs the Bloom filters before the pushed filter only once it has measured that filter keeping more than half of the rows it sees, with the same warmup of 65,536 rows the runtime filters already use to decide whether they are worth their hash. Until then, and whenever the pushed filter is the one that throws rows away, the old order stays.

Running every runtime filter first was tried and was worse on q10, where the pushed filter keeps a quarter of the rows and hashing the whole chunk cost more than the narrowing it saved. Running only the bitmaps first lost the gain on q04, where the date filter keeps about two thirds of lineitem and hashing first saves a third of the scan. The measured switch keeps both.

A part the graph reduction has narrowed keeps the old order, because the reduction names rows by their place in the part as it was read.

## Measured

Instructions a run over five runs at SF1, on a machine busy enough that CPU time was too noisy to read.

| query | main | every runtime filter first | bitmaps first | this change | DuckDB |
|---|---|---|---|---|---|
| q03 | 1.34 G | 0.85 G | 0.82 G | 0.82 G | 1.01 G |
| q04 | 1.15 G | 0.78 G | 1.14 G | 0.79 G | 0.98 G |
| q07 | 1.01 G | 0.84 G | 0.84 G | 0.84 G | 1.05 G |
| q10 | 1.80 G | 1.91 G | 1.80 G | 1.79 G | 1.44 G |
| q19 | 0.87 G | 0.81 G | 0.86 G | 0.86 G | |
| q20 | 0.98 G | 0.90 G | 0.90 G | 0.90 G | |
| q21 | 3.44 G | 2.26 G | 2.25 G | 2.25 G | 2.41 G |

The seven queries went from 10.60 G to 8.25 G, and all 22 answers are the same bytes as before. q19 keeps the order it had, since its pushed filter keeps few rows and the gain there came from hashing ahead of it anyway.
