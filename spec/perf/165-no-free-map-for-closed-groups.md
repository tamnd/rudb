# 165. No free map for an aggregate that closes its groups

## What was slow

q02's subquery takes the least supply cost of each part, grouped by `ps_partkey`. The rows reach the aggregate in runs of one part each, so the aggregate closes a group as soon as its run is behind it, and only the first and the last run of each chunk go into its table. That table still kept a map of the key's values, and since #2630 the table an instance holds before the split may clear one map of up to 262,144 places before any row pays for it. The parts of q02 run from 1 to 200,000, so the map grew chunk by chunk to 200,000 places on fresh pages for 642 rows and 460 groups. In a warm `EXPLAIN ANALYZE` the aggregate took 920 microseconds, and the run took about 250 page faults that went to nothing but this map.

## The change

An aggregate that closes its groups gets no free map. Its table can still keep a map, paid for out of the rows it folds the way a partition's table pays for one, at two places a row.

## Measured

At SF1 on server2, one thread. The first two rows are a query run 21 times in one process, the median of seven such runs, against the build of #2792. The page faults are per run, from the same counts.

| measure | main | this change |
| --- | --- | --- |
| q02 task clock, 21 runs (ms) | 237 | 213 |
| q02 page faults, 21 runs | 10,789 | 5,586 |
| q02 page faults per run | 253 | 4 |
| q18 page faults per run | 343 | 25 |
| q02 aggregate in a warm `EXPLAIN ANALYZE` (us) | 920 | 134 |

The user cycles of q02 move by less than the noise, since most of what went was the kernel handing out pages. The branch sits on a main a few changes past #2792, and the instructions of every query but q19, which note 164 made faster in between, are the same as before. The answers to all 22 queries are the same bytes as before at one thread and at six.
