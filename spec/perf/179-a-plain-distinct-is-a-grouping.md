# 179. A plain DISTINCT is a grouping

## What was slow

A `SELECT DISTINCT` with no `ON` list ran as its own operator, `Distinct`, which kept a set of the rows it had seen and a `Vec<Value>` for each row it kept. Every input row was turned into values, hashed as a row and looked up one at a time. The same rows written as a `GROUP BY` on every projected column with no aggregates run through the grouping, which hashes coded keys a batch at a time, splits the work across partitions and never builds a row of values. On the TPC-H q16 join at SF1, `SELECT DISTINCT p_brand, p_type, p_size` cost about five and a half times as many instructions as the same query with `count(*)` grouped on those three columns.

## The change

A new rewrite pass, `distinct_rows`, runs third, right after the distinct aggregate rewrite. It turns a `Distinct` with no `ON` list over a `Project` into a `Project` that keeps the old table index, over an `Aggregate` that groups on every projected column and has no aggregates. When the projection holds anything other than bare columns, the projection stays under the aggregate with a fresh index and the aggregate groups on its columns. Everything above the distinct still reads the same index, so no other pass has to know. `DISTINCT ON` is left alone.

## Measured

At SF1 on server2, instructions in millions from perf, two rounds each at one thread and one at six threads, against main just before this change. The answers to all 22 queries at one and six threads are the same bytes as before, since none of them has a plain `DISTINCT`.

| query | threads | main | this change |
| --- | --- | --- | --- |
| `SELECT DISTINCT p_brand, p_type, p_size` over the q16 join | 1 | 415, 415 | 67, 67 |
| the same with `ps_suppkey` added, counted | 1 | 1075, 1075 | 198, 198 |
| `SELECT DISTINCT p_brand, p_type, p_size` over the q16 join | 6 | 443 | 98 |
| the same with `ps_suppkey` added, counted | 6 | 1092 | 205 |
