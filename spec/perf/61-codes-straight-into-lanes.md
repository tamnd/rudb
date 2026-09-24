# 61. Codes straight into lanes

Note 60 left two packed columns compared against each other as two flat runs of `i32` or `i64`, but it got there by unpacking each side into a run of `u64` first, zeroed before anything was written into it, and then mapping that run into a second one. In q12 those two mapping passes and the zeroing were 8% of the query's instructions.

## What it does now

`packed_kept` asks `Packed::unpack_mapped` (note 59) for the lanes directly. It unpacks 64 codes at a time into a block on the stack and appends each one, already moved by the base difference and narrowed, into a vector that reserved its room once. Nothing is zeroed and each code is written once.

Note 60 tried a block at a time from the caller's side, calling `unpack` once per 64 rows, and lost. `unpack_mapped` takes the whole run in one call, so the head and tail handling happens once per chunk rather than once per block, and that is the difference.

## Numbers

TPC-H SF1 on server3, instructions best of three, against the same tree without the change. All 22 answers match.

| query | before | after | DuckDB |
|---|---|---|---|
| q12 | 0.913 G | 0.880 G | 1.027 G |
| all 22 | 17.372 G | 17.371 G | 26.151 G |

q12 goes down 3.6%. No other query compares two packed columns against each other, and the rest moved within noise.
