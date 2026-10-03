# 103. Reading a code without a branch

## The problem

A filter that keeps few rows hands the rows it kept to the columns it did not read, and each of those columns reads its code at each kept row. In TPC-H q14 the filter on `l_shipdate` keeps 75,983 of 6,001,215 lineitem rows, and the gather of `l_partkey`, `l_extendedprice` and `l_discount` at those rows was 16.6% of the query's samples. 90.8% of the gather's own samples sat on one instruction, the load of the packed word.

That load misses the cache, which is expected, since one kept row in 80 is one every few cache lines. What is not expected is that the misses did not overlap. `code_at` read the first word, then asked whether the code straddles into the next one and read that only if it did. A code of 23 bits starting at a random bit straddles about a third of the time, so the branch goes wrong often, and every time it does the processor throws away the loads it had started for the rows after it. The gather paid for one miss at a time.

## The change

`code_at` reads both words every time, puts them together as one `u128` and shifts the code out of it. There is no branch left that depends on the data, so the loads for the next rows go out while the current one is still waiting, and several misses are in flight at once. The second word is almost always in the same cache line or the next, so reading it costs little.

Every random read of a packed code goes through `code_at`, so the same change reaches the live rows path of the packed range filter, which q06 spends most of its filter time in, and the reads a link join makes into a packed parent column.

## Results

Pending the release build, posted on the pull request.
