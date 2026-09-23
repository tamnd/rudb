# Bitmaps that fit the cache

Notes written on 23 September 2026, after #1592, while looking at q17, which took 1.245 G instructions against DuckDB's 0.819 G at SF1.

## The question

q17 joins lineitem to the parts of one brand and container, 204 of 200,000, and scans lineitem a second time for the correlated average. A profile of it run in a loop put the largest share of the busy samples in `Blocked::holds_run` and `Scan::sift_hashed`, which is the Bloom filter the part join hands the lineitem scan. Each scan hashed all six million part keys, and the filter let 75,505 rows through where 6,088 matched.

Note 23 added an exact bitmap for integer keys that sit close together, one bit per value between the smallest key and the largest, and it takes the filter's place when there are at most 64 bits a key. The 204 part keys are spread over the whole range, which is a thousand bits a key, so the rule said no and the filter was built.

## What changed

The limit of 64 bits a key is there because a bitmap much bigger than the filter misses the cache on every test, and then it is no cheaper than a hash. That is a question about bytes, not about keys. A bitmap of 32 KiB or less sits in the first level data cache of every core this is built for, and a bit test from there is cheaper than the hash the filter has to take before it looks anything up. So a bitmap up to 262,144 bits is now taken however few keys it holds, and the old rule still decides past that. The q17 bitmap is 25 KB.

The bitmap is also exact, so the scan hands up only the rows the join will keep, and every operator above it has less to do.

## Measured

Five runs of each query at SF1, instructions, both built without the size setting #1589 put on the CLI crate.

| query | before | after | DuckDB |
|---|---|---|---|
| q02 | 0.212 G | 0.188 G | 0.345 G |
| q08 | 0.895 G | 0.674 G | 0.920 G |
| q17 | 1.245 G | 0.817 G | 0.819 G |
| q19 | 0.858 G | 0.588 G | 1.365 G |
| q20 | 0.905 G | 0.834 G | 0.953 G |
| suite | 25.57 G | 24.54 G | 25.41 G |

In CPU time over ten runs, q17 went from 94 ms to 63 ms against DuckDB's 73 ms, q08 from 76 ms to 58 ms against 76, and q19 from 68 ms to 51 ms against 132. No other query moved by more than noise, and the answers of all 22 queries are the same bytes as main.
