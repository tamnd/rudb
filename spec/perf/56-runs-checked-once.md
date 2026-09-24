# 56. Runs checked once

`l_orderkey` is stored as run lengths over delta-coded values: about a million and a half runs of one to seven rows each, over six million rows. Ten of the 22 TPC-H queries read it, and a scan of it cost 118 M instructions against 14 M for `l_linenumber`, which is packed plainly.

## What it did

The run expansion made every check a run could fail inside the loop. Each run converted its length, did a checked add of the end against the chunk, checked the value against the lane type, and picked between two write shapes. That came to about 25 instructions per run, against the four vector stores that write it. Under the runs, the delta decode pushed every sum into a second vector and asked it for room once per value.

## What it does now

One fold over the run lengths finds whether any is negative, the longest, and the total. The total has to be the chunk's count, and once it is, every run ends inside the chunk. For a lane narrower than `i64`, one fold over the run values takes the lowest and highest and checks those two. For `i64`, the test folds away. After that, when no run is longer than eight, the loop is a fixed write of eight values and a step by the run's length, with no exit to predict. Both decoders, wide and narrow, go through the same function.

The delta decode writes each sum over the difference that follows it and pushes only the last one, so it uses one vector and no capacity check per value.

## Numbers

TPC-H SF1 on server3, instructions best of three, against main at the same commit. All 22 answers match.

| query | main | this | DuckDB |
|---|---|---|---|
| q03 | 0.773 G | 0.736 G | 1.024 G |
| q10 | 1.218 G | 1.176 G | 1.889 G |
| q13 | 1.251 G | 1.218 G | 2.540 G |
| q21 | 1.690 G | 1.621 G | 1.884 G |
| all 22 | 18.347 G | 17.971 G | 27.113 G |

20 of the 22 queries went down. A bare `count(*)` over `l_orderkey > 0` went from 118 M to 99 M.

The profiles behind this were taken after raising `perf_event_max_sample_rate` on server3 from 1000 to 100000. At the old cap a query of a tenth of a second got about 300 samples, and the shares they showed moved by several points from run to run.
