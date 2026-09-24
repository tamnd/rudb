# 60. Two columns in narrow lanes

q12 compares two pairs of date columns row by row, `l_commitdate < l_receiptdate` and `l_shipdate < l_commitdate`. Both sides come off disk bit packed, each with its own base, and `packed_kept` in `compare.rs` answers the pair in code space with the difference of the two bases added to the right side. That compare was 21% of q12's instructions, the largest single piece of the query.

## What it did

For each row it called a closure that read one code from each side and stored a flag for the row. The compiler could not prove the flag store did not alias either packed buffer, so it reloaded both behind every store, and nothing in the loop turned into vector code.

## What it does now

Both sides are unpacked into flat runs with the base difference already added on the right, and those go through the same `flat_where` a compare of two flat columns uses, so a block of 64 is a zip of two slices with no row index to check. When both widths are 30 bits or fewer and the base difference is under 2^30, the runs are `i32` rather than `i64`. A date code is well inside that. It matters because SSE2 has a native signed compare for 32 bit lanes and has to build one out of several instructions for 64 bit lanes, and a register holds twice as many values. Wider codes keep the `i64` runs.

## What did not work

Unpacking 64 codes at a time into a block on the stack and mapping them out of it, to avoid the two chunk sized `u64` buffers, made q12 worse, 0.914 G to 0.943 G. Each block pays for a call into `unpack` and its head and tail handling, and that cost more than the buffers did. Note 59 does the same thing for a whole column with a better result, since there the block feeds values straight out and there is no second pass to save.

## Numbers

TPC-H SF1 on server3, instructions best of three, against the same tree without the change. All 22 answers match.

| query | before | `i64` runs | `i32` runs | DuckDB |
|---|---|---|---|---|
| q12 | 0.937 G | 0.920 G | 0.914 G | 1.027 G |
| all 22 | 17.734 G | 17.698 G | 17.673 G | 26.151 G |

q12 goes down 2.5%. The rest of the suite moves within noise, because no other query compares two packed columns against each other. In the q12 profile the compare is now 12% of instructions instead of 21%. The two conversions from `u64` codes into lanes cost another 6%, and they are the next thing to remove, by unpacking directly into the lane type.
