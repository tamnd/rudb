# Packed columns read a block at a time

Notes written on 24 September 2026, on the driving side of the joins that read a packed integer column into a run of `i64` before looking each row up.

## The question

The join filters and the join lookups read their keys through `Vector::signed_block`, which hands back the first rows of a column as a run of `i64`. For a packed column, which is the form most integer columns of the native file are in at rest, the run was built a code at a time: work out the word, read it through a bound, ask whether the code straddles into the next word, shift and mask. That came to about twenty instructions a row. `Packed::unpack` already had the better loop, sixty four codes at a time with the width a constant so every shift and straddle is known before it runs, and the aggregates used it, but the join paths did not.

Next to it, the null check the join's build and its sideways filter make on a key column said yes for every dictionary and every run length vector without looking, so a key that came through a filter as a dictionary over a stored column with no nulls in it was checked for a null row by row.

## What changed

`signed_block` on a packed column now unpacks through `Packed::unpack` into a sixty four entry buffer on the stack and adds the base as it copies out. The blocks are lined up on the words, so only the first block of a cut that starts inside a word goes a code at a time and every block after it takes the constant width loop.

`has_nulls` in `crates/rudb-exec/src/lookup.rs` now asks `Vector::none_null` for a dictionary or a run length vector, which looks at the mask of the values they point at, and only says yes when that mask has a null in it.

A new test in `crates/rudb-vector/src/vector.rs` reads packed columns as a block at widths that do and do not straddle, cut at rows that do and do not start a word, and checks every row against the row at a time read.

## Numbers

On server3 against the native file at SF1, instructions best of three, main before and this change after:

| | main | packed blocks | and the null check | DuckDB |
|---|---|---|---|---|
| q07 | 0.889 G | 0.769 G | 0.767 G | 1.427 G |
| q08 | 0.747 G | 0.643 G | 0.640 G | 1.143 G |
| q17 | 0.774 G | 0.580 G | 0.579 G | 1.103 G |
| q19 | 0.721 G | 0.614 G | 0.617 G | 0.991 G |
| q20 | 0.789 G | 0.690 G | 0.687 G | 1.222 G |
| q21 | 1.869 G | 1.773 G | 1.671 G | 1.801 G |
| all 22 | 19.978 G | 19.141 G | 18.867 G | |

q21 now runs fewer instructions than DuckDB, the last query of the 22 besides q01 and q09 where it did not. All 22 answers are the same as main's. server3 was under a load of 25 to 30 from other work while this was measured, so the wall times are loose, but best of five they were 0.51 s against 0.71 s on main and 0.93 s for DuckDB on q21, and 0.27 s against 0.38 s and 1.18 s on q08. Peak memory did not move by more than 5 percent on any of the six.

## What this leaves

q01 and q09 are the two queries still above DuckDB in instructions. On q21 the two gathered sides are still held whole, 117 MB at peak against DuckDB's 117 MB.
