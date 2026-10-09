# 175. The gathered side takes the driving side's keys

## What was slow

After #2904 the last join of q05 matches the lineitem rows that came through customer, nation and orders against supplier on `l_suppkey = s_suppkey` and `s_nationkey = n_nationkey`. Supplier is the build side, and a runtime filter only ever goes down the driving side of a join, so nothing restricted the supplier scan. All 10,000 suppliers went into the hash table where the 2,003 of Asia would have done, and the key bitmap that table hands the lineitem scan held five times the suppliers it needed, so 184,082 lineitem rows were gathered and carried to the join that kept 7,243 of them.

Writing the restriction by hand, an `s_nationkey IN (...)` over the nations of Asia, took q05 from 177 to 99 million instructions with the same answer, which is what this change makes the planner do.

## The change

`join_key_reach` already wrote a semi join over a scan under the driving side of a join, against a copy of the part of the gathered side that held the other key. It now tries each equality the other way round too. The scan under the gathered side gets a semi join against a copy of the part of the driving side that holds the key, when that part is filtered and its tables hold a tenth of the rows the scan reads or less. In q05 that is the supplier scan against a copy of nation joined to region under the filter on Asia, 30 rows read for 10,000. The driving side is tried first, so a join that had a semi join written before still gets the same one.

The copy is also bounded by the rows the side over the scan produces, not only the rows the scan reads. Without that q02 went from 23.6 to 32.3 million instructions, because partsupp is the gathered side's scan of one of its joins and part's keys already leave about 3,000 of its 800,000 rows, so a copy of the 10,000 suppliers cost more than everything it could drop.

## Measured

At SF1 on server2 against main at #2904, one thread, steady state.

| query | main | this change |
| --- | --- | --- |
| q05 instructions | 177M | 99M |
| q05 cycles | 138M to 147M | 81M to 100M |
| q05 cycles, DuckDB | 406M | 406M |

No other query moves by more than 1.8 percent in instructions, and q08, the largest of those at 68.5 to 69.7 million, has the same plan and the same cycles. The answers to all 22 queries are the same bytes as before at one thread and at six.
