# 53. Strided chunks in one pass

A strided chunk stores `base + step * stride` for every value, with the steps packed on their own. `l_quantity` is stored this way, as is any column whose values move in fixed steps, so q01, q06, q18 and q19 decode one on every lineitem row.

## What it did

The decode unpacked the steps into one vector and then mapped them into a second. Every value went through a checked multiply and a checked add, and each check was a branch. Decoding into a narrow lane type made it worse: the wide values were built first, then each was tried against the lane one at a time. Together that was about 13 instructions per value, and `decode_chunk` was 10.7% of q01's instructions.

## What it does now

The steps are unpacked once and the values are written over them in place, so there is one allocation instead of two. The multiply and add wrap. That is safe because the encoder picked the base and stride so that every value fits, and a single fold over the steps checks up front that none is negative. The loop has no branch left and vectorizes.

For a narrow lane, the decode folds the lowest and highest values once. If both fit the lane, every value in between does too, so the chunk maps with a plain cast. If they do not fit, it falls back to the checked lane for each value, as before.

## Numbers

TPC-H SF1 on server3, with instructions counted by `perf stat` on a warm run of the native file. All 22 answers match main.

| query | main | this | DuckDB |
|---|---|---|---|
| q01 | 1.997 G | 1.965 G | 1.475 G |
| q06 | 0.519 G | 0.484 G | 0.616 G |
| q18 | 1.043 G | 1.008 G | 1.860 G |
| q19 | 0.605 G | 0.576 G | 1.083 G |
| all 22 | 18.374 G | 18.195 G | |

16 of the 22 queries went down. The rest moved within noise. After this change, `decode_chunk` no longer shows in q01's profile. What is left of q01's cost is aggregation, plus the Packed-to-flat unpack in `unpack_block` at 7.5%.
