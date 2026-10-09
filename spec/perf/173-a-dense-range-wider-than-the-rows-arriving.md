# 173. A dense range wider than the rows arriving

## What was slow

The dense pass addresses a grouped aggregate's slots directly by the key when the key is one integer column whose range is at most about a million values. It refused a range more than eight times the column's counted distinct values, since such an array is mostly holes, but it never asked how many rows the aggregate is handed. An aggregate cannot build more groups than it gets rows, so the rows arriving bound the groups just as the distinct count does.

q17 is the case. Its decorrelated aggregate groups the lineitem rows of the parts the outer query asks about by part key. The range of `l_partkey` is all 200,000 parts and the column holds every one of them, so the distinct count passes, but the aggregate is handed 6,088 rows and builds 204 groups. The array was 800 kilobytes touched at random for groups a hash table holds in a few lines.

## The change

A range is now also sparse when it is more than eight times the estimated rows arriving, and a sparse range with fewer rows arriving than places is left to the hash table as before. Since #2899 the rows arriving at q17's aggregate are estimated at 5,941, so its range of 200,000 is left alone.

## Measured

At SF1 on server2 against main at #2899, one thread. Across all 22 queries only q02 and q17 change plan. Cycles a run are within the noise of the machine, so the table is the aggregate's own time out of twelve runs of EXPLAIN ANALYZE in one process.

| query | aggregate | main | this change |
| --- | --- | --- | --- |
| q17 | grouped by `l_partkey` | 1.15 to 1.32ms, most near 1.17 | 0.54 to 0.65ms, most near 0.61 |
| q02 | grouped by `ps_partkey` | 120 to 350us | 120 to 350us |

Instructions a run are 77,439 thousand before and 77,327 after on q17, and 23,548 and 23,639 on q02. The answers to all 22 queries are the same bytes as before at one thread and at six.
