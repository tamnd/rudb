# 177. A selection kept as its mask

## What was slow

A filter that compares a column with a literal works out one mask word for every 64 rows, and then `mask_selection` turned the words into a list of the rows kept. Every reader got the list, including readers that never look at a single row of it. On `SELECT count(*) FROM lineitem WHERE l_shipdate` in a year at SF1, listing the rows was 28% of the query. The chunk behind the filter had no columns left after the scan dropped the date, so the list was built, checked against the chunk length and thrown away.

q01 hit the same thing from the other side. Its date filter keeps 98.6% of lineitem, and the aggregate reads whole columns and puts each row the filter dropped into no group. To find those dropped rows it listed the kept rows from the mask and then walked the list looking for gaps, which is two passes to recover what the mask already said.

## The change

`Selection` now holds either a list of rows or the mask a filter made with its count of set bits. A selection made from a mask lists its rows the first time something calls `indices` and keeps that list for later calls, so a reader that wants the rows pays what it paid before and a reader that does not pays nothing. `len` is the count of set bits and `below` checks the words past the end of the chunk, so neither of them lists anything.

`Chunk::select` on a chunk with no columns returns the count straight away. The totals by place that q01 adds through read the dropped rows straight out of the mask words, filling a whole word's places when the word is empty and walking the clear bits otherwise.

## Measured

At SF1 on server2, one thread, instructions a run from perf against main just before this change. The answers to all 22 queries are the same bytes as before.

| query | main | this change |
| --- | --- | --- |
| count(*) with a filter on l_shipdate | 25M | 13M |
| the same with l_quantity < 24 as well | 25M | 19M |
| q01 | 165.5M | 140.2M |

No other TPC-H query moved by more than 0.9 percent.
