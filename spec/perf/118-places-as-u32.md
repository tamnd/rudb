# 118. Places as u32

## What the places cost

A chunk that groups through the coded map first works out each row's place, the index of its combination of key codes, one pass per key column. q01 groups by four columns, flag, status, discount and tax, so the pass runs four times over every row, and each time it adds the column's code times the column's stride into the row's place. The places were a vector of `usize`. AVX2 has no 64 bit multiply, so the compiler built each one out of three 32 bit multiplies, two shifts and two adds, four rows at a time, and wrote eight bytes a row back. After note 117 that pass was 18 percent of q01, more than any other part of the query except the add into the place cells.

## The change

A place is now a `u32`. No map is longer than `WIDE_COMBOS`, which is 2^18, and a window read by value only takes values inside it, so every place and every product on the way to one fits. Each column's pass is now one `vpmulld` and one `vpaddd` for eight rows, and writes half the bytes. `PlaceSums::add_places`, `PlaceSums::add`, `slots_from` and the rows the filter dropped all take the places as they come, and widen one to `usize` only where it indexes.

## Measured

Single thread at SF1 on server2, warm instructions as the difference between eleven runs and three in one process, against main at #2584. The answers to all 22 queries are the same bytes as before.

| query | before (M) | after (M) |
| --- | --- | --- |
| q01 | 365 | 331 |

q03, q12 and q18 run the same instructions as before.

## What this leaves

The codes of discount and tax are still unpacked 64 at a time into a block of `u64` before they are added into the places, and the two summed columns are unpacked into vectors of `u64` that the add into the cells reads back. The add itself is about 12 instructions a row, of which the bound check on the place and building the row's lanes are a third.
