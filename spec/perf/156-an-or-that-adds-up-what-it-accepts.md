# 156. An OR that adds up what it accepts

## What was slow

An `OR` of several operands runs them one after another, and each operand only needs to look at rows that no earlier operand accepted. The executor kept that set as the rows still in play: after the first operand it wrote out the complement of what it accepted, and after each later one it cut what that operand accepted out of the set again. When few rows are accepted, which is the usual case for a filter, that set is nearly every row, so each operand paid for a pass that wrote nearly every row position out again. TPC-H q19 is an `OR` of three branches over the lineitem rows that survive the join, and at SF1 that bookkeeping cost about twice what the comparisons themselves did.

## The change

An `OR` now starts by running every operand over all the rows it was given and keeping the union of what they accept, which is a merge of two short sorted lists. Only once more than half of the given rows are accepted does it switch to the old way and carry the rows not yet accepted, because then that set is the smaller one. If it never switches, the union is the answer. Running an operand over a row an earlier one already accepted costs extra only for those rows, and while they are fewer than half that is cheaper than the pass it replaces. `Selection::union` is new, with a test that it holds each position of either side once and in order.

## Measured

At SF1 on server2, one thread, against main at #2749.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q19 | 88 | 65 |
| q07 | 100 | 100 |
| q12 | 105 | 105 |
| q22 | 52 | 52 |

q07, q12 and q22 also have an `OR` or an `IN`, and they did not move. The answers to all 22 queries are the same bytes as before at one thread and at six.
