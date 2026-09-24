# Counts in arrays the key indexes

TPC-H q13 counts each customer's orders with a left join from customer to orders, grouped by `c_custkey`. The planner already knows the key is an integer between one and the number of customers, because the `aggregate_dense` pass reads the two ends the store kept for the column. Before this change, the aggregate only used that range to find a group's slot faster. The rest of the general hash table still ran.

## What the table cost

At one scale factor the grouping took about 0.36 G instructions for 1.55 million rows, against about 0.22 G for DuckDB. That is roughly two hundred and twenty instructions a row, spent on three things.

1. Every group of the 150,000 took a hashed bucket, a copy of its key, a fresh accumulator and a probe on the way in, and the table doubled and rehashed several times on the way to that size.
2. The key read by value went through the window map, and the first sixteen chunks were hashed because the window had not yet earned its room.
3. The customers with no orders come out of the join after the probe has finished, through a pipeline instance of their own. That second instance saw two instances started, partitioned at its first chunk and scattered the probe's whole table into the sixty four partitions, where every group was hashed and inserted again.

## The change

An aggregate whose every call is `COUNT(*)` or `COUNT(x)`, with no filter, no `DISTINCT` and nothing above it reading the groups, and whose one key is a signed integer with a known range, now counts into arrays as long as the range. There is one array for the rows of each key and one for each `COUNT(x)`. A row costs a subtract, a compare and an add per call. An instance hands its arrays over when it finishes, and the shared arrays are the sum of every instance's. The answer is every place whose row count is not zero, in key order.

The null key has a place of its own past the range. The range is one the values are inside of, so no value should land outside it. A value that does is sent to one more place that the answer never reads, and is then counted again by its value in a small map beside the arrays. So a wrong bound costs speed and never an answer, the same promise the direct index in the table makes. The range is at most a million values, which is what `aggregate_dense` allows, so an instance holds at most eight megabytes an array.

## Results

Instructions at one thread, before and after, with DuckDB after that:

| query | before | after | DuckDB |
|---|---|---|---|
| q13 | 1.849 G | 1.597 G | 1.550 G |
| q13 join and group, no LIKE | 0.671 G | 0.419 G | 0.530 G |
| group by `o_custkey` on orders | 0.294 G | 0.073 G | 0.247 G |

Wall time for q13 went from 405 ms to 323 ms at one thread and from 104 ms to 69 ms at default threads. No other TPC-H query changed. What is left between q13 and DuckDB is the scan of `o_comment` and its `NOT LIKE`.
