# 123. Pairs by place in lanes

## What was slow

Once q01's key has places and its sums are totals per place (notes 115 and 117), the pass that does the adding is `by_place::<2, 4>`. Every row adds its `l_quantity` code, its `l_extendedprice` code and a one for its count into the four cells of its place. Written as a row at a time it was 13 instructions a row. Each row's two values were two scalar loads, two `vmovq` and an unpack to make a pair, its place was checked against the cells with a compare and a branch, and the place was also spilled to the stack for the error message. That was 29 percent of q01's warm instructions.

## The change

`lanes::add_pairs` adds eight rows at a time with AVX2. The eight places are one load, and one unsigned maximum and one compare check all eight against the cells before any of them is added. Four rows of each column are one load, and an unpack low and an unpack high make four pairs, each the low or high half of a register. A row is then its place, one add of its pair into its cells, a store and one add for its count. A block with a place past the cells stops the pass before it adds anything, and the caller's loop adds that block and the last few rows one at a time as before, so a bad place still turns into the same error.

`rudb_vector::vector::add_pairs_by_place` is the safe entry, since rudb-kernels forbids unsafe code. `by_place` uses it whenever there are two summed calls.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process. The answers to all 22 queries are the same bytes as before at one thread, and q1 also at six.

| query | instructions before (M) | after (M) |
| --- | --- | --- |
| q01 | 320 | 272 |

Against DuckDB in the same run, q01 takes 755 M cycles there and 232 M here.

## What this leaves

Cycles fell much less than instructions. With the adds cheaper, q01's cycles are now mostly in `Packed::unpack` at 16 percent, `Coded::places` at 14 percent and the adds themselves at 11 percent. `Coded::places` makes one pass over the places for each of the four key columns, so the places vector is read and written four times a chunk, and the two key columns that are packed are unpacked into a block and then added. Working a row's place out of all four columns in one pass, straight from the packed words, is the next step.
