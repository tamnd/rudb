# Packed columns compared in one pass

A native table stores `l_shipdate`, `l_commitdate` and `l_receiptdate` as frame of reference with bit packing, and a scan hands them up still packed. q12 compares two of them to each other twice, `l_commitdate < l_receiptdate` and `l_shipdate < l_commitdate`, and q4 and q21 have the first of the two.

## What it cost

A comparison of two packed columns read each code on its own. Every read worked out which word the code was in, read it through a bound and asked whether it ran over into the next word, and the answer went into a flag per row that a second pass then turned into the selection. On one thread `SELECT count(*), sum(l_orderkey) FROM lineitem WHERE l_commitdate < l_receiptdate` ran 0.508 G instructions against DuckDB's 0.402 G, and the filter of q12 was about half of the query.

The same two passes were there for a flat integer column against a literal in any conjunct after the first. The first conjunct already had a one pass loop for that, and a later one did not.

## What changed

When both sides of a comparison are straight packed runs of the same type with no nulls, the kernel unpacks each side in blocks of sixty four codes, where the width is a constant in the loop, and compares the two runs while writing the selection directly. The two bases are folded into one difference added to the right side, so the loop compares two `i64`. When the conjuncts before this one kept fewer than one row in eight, it reads the codes of just those rows one at a time instead of unpacking the whole vector. Two ranges that do not meet still go to the general path, which answers them without reading a code.

The one pass loop for a flat column against a literal is now shared by the first conjunct and the later ones, and it also covers two flat integer columns against each other.

## Results

Instructions at one thread, before and after, with DuckDB after that:

| query | before | after | DuckDB |
|---|---|---|---|
| `l_commitdate < l_receiptdate` | 0.517 G | 0.348 G | 0.415 G |
| q12 filter alone | 0.817 G | 0.589 G | 0.757 G |
| q12 | 1.084 G | 0.856 G | 0.916 G |
| q06 | 0.491 G | 0.430 G | 0.538 G |

At default threads q12 is 0.866 G against DuckDB's 0.959 G and q06 is 0.424 G against 0.543 G. q4 and q21 did not move, because their date comparison does not reach the kernel as two packed runs, and that is the next thing to look at for them.
