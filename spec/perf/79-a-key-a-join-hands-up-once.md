# 79. A key a join hands up once

## The problem

Note 78 left q10 at 141 ms on one thread over SF1, and `EXPLAIN ANALYZE` put 28 ms of that in one aggregate at the top of the plan. It groups on `c_custkey`, `c_name`, `c_acctbal`, `c_phone`, `n_name`, `c_address` and `c_comment`, five of them strings, and it put 37,967 rows into 37,967 groups.

Every group was one row because of how the plan under it is built. Eager aggregation has already pushed the total of `l_extendedprice * (1 - l_discount)` under the join, so the aggregate sits on `customer` joined to a total per `o_custkey` and then to `nation`. The total holds each `o_custkey` once and `nation` holds each `n_nationkey` once, so each `customer` row meets at most one row on the other side of each join and comes out of both at most once. `c_custkey` is a different number on every row of `customer`, so it is a different number on every row the joins produce, and the aggregate over them had nothing to add up.

The rewrite in `unique.rs` already turns an aggregate whose key holds no value twice into a projection, which is what ClickBench 32 and 33 needed. It only followed the key down through filters and projections to a scan, and stopped at the first join, because a join can hand a row up more than once.

## The change

The walk goes through a join now when the key comes from a side whose rows each come out at most once.

For an inner join that is the other side matching each row at most once. The rewrite reads that from the join's condition: one equality between a column of this side and a column of the other side that holds no value twice there, asked with the same walk the key is. A plain integer to integer cast of a column counts as the column. A left join is the same when the key comes from the side that is kept, and it is refused when the key comes from the side it pads with nulls. A semi, an anti, a mark or a single join hands each left row up once whatever it finds, so the key only has to come from the left and be unique there.

The walk also stops at an aggregate. The key of an aggregate with one group key holds each value once, so `o_custkey` is unique in the total per customer without any count from the file.

The total only exists once eager aggregation has pushed it under the join, and that is later in the sequence than where the rewrite first runs. So the rewrite is also registered a second time as `joined_rows_are_groups`, straight after eager aggregation, which makes thirty three passes.

## Results

server3, SF1 native, three copies of each query in one process, `perf stat` against the note 78 build:

| query | threads | instructions before | after | task clock before | after |
|---|---|---|---|---|---|
| q10 | 1 | 2131 M | 1721 M | 499 ms | 413 ms |
| q10 | 8 | 2590 M | 1908 M | 1478 ms | 701 ms |
| q13 | 1 | 2892 M | 2494 M | 1338 ms | 772 ms |
| q13 | 8 | 3478 M | 2514 M | 1308 ms | 929 ms |

q13 is the other query whose plan changed. It counts orders per customer with a left join from `customer` to the orders grouped by `o_custkey`, and the aggregate on `c_custkey` over that join is now a projection too. The answers to all 22 queries are the same as before at one thread and at eight. The task clock numbers were taken while the machine was shared and move more between runs than the instructions do.

## What is left

The key is still found only through an equality to a single column. A join on two columns that are unique together, like `ps_partkey, ps_suppkey`, does not count. No TPC-H query needs that yet.
