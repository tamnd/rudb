# Join bitmaps up to a megabyte

When a hash join's build side finishes, it hands the scan on the other side something to drop rows with before they reach the join. For an integer key there are two choices: a bitmap with one bit per value between the smallest and largest key, or a blocked Bloom filter at ten bits a key. The bitmap is exact and costs a subtraction and a bit test a row. The filter costs a hash and a probe of four bits in one cache line, which counted out at about sixty instructions a row, and it lets about one row in a hundred through that the join then drops.

## The old rule

The bitmap was taken when it cost at most sixty four bits a key, or when it fit in 32 KB, which is the first level cache. Past both, the filter was built instead, on the reasoning that a bitmap bigger than the filter is no faster once both miss the cache.

TPC-H joins on order keys fall between the two. The orders of one quarter in q4 are about fifty seven thousand keys spread over six million values, which is a hundred bits a key and a 750 KB bitmap, so q4 got the filter, and the sieve was over a third of the query at one thread. q18, q10, q8 and q5 have the same shape.

## The new rule

The size under which a bitmap is taken however sparse its keys are is now one megabyte, which fits in the second level cache of any core this is built for. Out there a bit test is still far cheaper than the hash and the probe, and it drops exactly the rows the join would drop. The sixty four bits a key rule and the thirty two megabyte budget are unchanged, so at larger scale factors, where the order key range is a hundred times wider, the filter is still what gets built.

## Results

Instructions at one thread, before and after, with DuckDB after that:

| query | before | after | DuckDB |
|---|---|---|---|
| q04 | 0.756 G | 0.474 G | 0.861 G |
| q18 | 1.557 G | 1.240 G | 1.884 G |
| q10 | 1.014 G | 0.871 G | 1.191 G |
| q08 | 0.637 G | 0.550 G | 0.786 G |
| q12 | 0.854 G | 0.791 G | 0.910 G |
| q21 | 2.049 G | 2.007 G | 2.038 G |

Wall time at one thread moved the same way: q04 went from 451 ms to 323 ms, q18 from 1014 ms to 803 ms, q10 from 515 ms to 367 ms and q08 from 166 ms to 129 ms. No other query changed by more than the noise.
