# 152. A widened decimal sum in one pass

## What was slow

Two decimals of at most 18 digits are each stored in 64 bits. Their sum or difference can have one more digit, so the binder types `DECIMAL(18, 4) - DECIMAL(18, 4)` as `DECIMAL(19, 4)`, which is stored in 128 bits, and casts both sides to it before the call. That is three passes: one cast building a vector of `i128` from each side, and then a subtraction over the two. TPC-H q09's `amount` is exactly this, `l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity`, for every row that reaches the aggregate, so each such row paid for two 128 bit vectors that were only read once.

## The change

When the prepared expression meets `+` or `-` whose answer is a decimal of more than 18 digits and whose two sides are plain casts to that type from decimals of at most 18 digits at the same scale, it calls `__rudb_widened_add` or `__rudb_widened_subtract` on the sides from under the casts instead. The kernel reads the two 64 bit runs and writes each sum or difference straight into the 128 bit answer. Nothing can overflow: each side is under 10^18 in size, so the answer is under twice that, which fits both 64 bits and 19 digits.

Any other form of the sides, such as a constant or a dictionary, is cast and handed to `+` or `-` as the query wrote it, and so is the row at a time path, so the two ways always agree. A kernel test checks the fast path against the casts at the ends of 18 digits, with nulls on either side and with a constant side.

## Measured

At SF1 on server2, one thread, against main at #2728.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q09 | 366 | 333 |
| q01 | 185 | 185 |
| q05 | 101 | 101 |
| q07 | 103 | 102 |
| q08 | 67 | 67 |
| q10 | 150 | 150 |

The answers to all 22 queries are the same bytes as before at one thread and at six.
