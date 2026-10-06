# 132. One key that fixes the others, closed by its runs

## What was slow

TPC-H q03 groups the join of `lineitem` and `orders` on `l_orderkey, o_orderdate, o_shippriority`. At SF1 that is 30,519 rows into 11,620 groups, and the aggregate was 6 to 9 ms of a 35 ms query on one thread. Every row went through the hash table on three keys: a probe, and for each new group an insert of three columns and a fresh set of accumulators.

Two things made it so. `lineitem` drives the join, and the cluster pass stopped at any join, so the aggregate did not know that `l_orderkey` still arrives in runs after the probe. And even if it had known, it only closes groups on one key, and q03 has three.

## The change

Both are in the cluster pass, `crates/rudb-opt/src/cluster.rs`.

The walk now goes through a join that streams its driving side: an inner join to the side that drives it, and a semi or anti join that is not turned around. The probe answers a driving chunk at a time, in the order of its rows, with each row's matches next to each other, so a key in runs at the probe is in the same runs after it. The walk asks for an equality between two plain columns of one keyed type, which is the probe's condition and narrower, so it never says yes to a join the executor would answer with the general join, which collects its driving side first.

An aggregate with more than one key now keeps only one of them when that key arrives in runs and fixes every other key. The others come back as `min` calls, which over a group whose values are all the same is that value, a null included, and a projection above puts the columns back in the aggregate's order. What fixes what is read off the rows under the aggregate:

- An equality between two columns in a filter or an inner join condition, which every row passed, makes them one value. Only over integers, decimals and dates, where equal values are the same value. A double has `0.0` and `-0.0`, and a string may be compared under a collation.
- A scan column that holds every value once fixes every column of that scan. Exact counts say so, or the uniqueness certificate of a link to that column.
- The keys of an aggregate below fix all of its columns.

In q03 the join says `l_orderkey = o_orderkey`, the link from `lineitem` to `orders` says `o_orderkey` is distinct, so `l_orderkey` fixes every column of `orders`. The aggregate becomes one key and three calls, `sum`, and the `min` of a date and of an integer, and all three are ones the executor answers straight from a run without a table.

An aggregate the dense pass marked keeps its keys, since an array indexed by the key is already cheaper, and so does one with a `DISTINCT` call, which does not close.

## Measured

At one thread at SF1 on server2, against main at #2639. The load average was between 16 and 20. The answers to all 22 queries are the same bytes as before, at one thread and at six.

Eight steady runs of q03:

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 109 | 88 to 105 |
| this change | 86 | 80 |

q18's last aggregate groups on five columns that `o_orderkey` fixes, but `o_orderkey` is on the built side of its join and `l_orderkey` drives, so it is left as it was. Following the walk across the join's equality would reach it, and at SF1 it sees 399 rows, so it is not worth it yet.
