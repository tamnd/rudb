# 176. A count moved below a padded join counts the rows

## What was slow

q13 counts the orders of each customer with `count(o_orderkey)` over `customer LEFT JOIN orders`. Above the join `o_orderkey` is null for a customer with no orders, so the count has to read the key and test it a row. Eager aggregation moves the count below the join as `count(o_orderkey)` grouped by `o_custkey`, and puts `coalesce(count, 0)` above the join for the customers the join pads.

Below the join the key has no nulls, which the file says, and the pass that turns a `count` of a column with no nulls into `count(*)` would have rewritten it. That pass runs before eager aggregation, though, and when it ran the count was above the padded join where the column does have nulls. So the scan of orders read `o_orderkey` for every row kept, the aggregate tested every key, and nothing used the keys after.

The `coalesce` above the join was the other cost. It went through the general path, which asks each row whether it is null and then picks the column or the constant, 27.4 million instructions a run over the 150 thousand customers in a profile at SF1.

## The change

When eager aggregation moves anything it runs the no-nulls pass again, which settles the new aggregate, turns its count into `count(*)`, and prunes the columns nothing reads. On q13 the orders scan reads `o_custkey` and `o_comment` and no longer `o_orderkey`.

`coalesce` of a column and a constant that is not null, with the column flat or gathered, now copies the column's values and writes the constant over the rows its validity marks null, walking the clear bits a word at a time. The result has no nulls.

## Measured

At SF1 on server3 against main at #2900, one thread, instructions a run from perf as the difference between four runs and two. No other query moves by more than 0.3 million, and the answers to all 22 queries are the same bytes as before at one thread and at four, on both databases.

| query | main | this change |
| --- | --- | --- |
| q13 | 193.5M | 145.6M |
| all 22 | 2,095.3M | 2,047.1M |

A callgrind profile of the third run of q13 against the second went from 239 to 199 million instructions. The `coalesce` went from 27.4 million to too little to show, the gather of the orders columns the join reads from 41.8 to 20.3 million with `o_orderkey` gone from it, and the aggregate's sink from 88.4 to 73.9 million.
