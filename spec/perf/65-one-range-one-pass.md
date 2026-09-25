# 65. One range, one pass

## The problem

q06 filters `lineitem` on `l_shipdate >= date '1994-01-01' and l_shipdate < date '1995-01-01'` and on `l_discount between 0.05 and 0.07`, which the plan hands the scan as five conjuncts. The filter runs them one at a time, each over the rows the ones before it kept, and learns which order rejects the most for the least. That works well when one conjunct is selective. Here none is. Each end of the date range keeps between 45 and 70 percent of the rows it is given, and it is only the two together that keep 15 percent. So the first conjunct walks the whole chunk and builds a selection of more than half of it, and the second walks that selection again to throw most of it away. At SF1 on one thread q06 retired 431 M instructions against 319 M for DuckDB.

The same shape, a low and a high bound on one column, is in q04, q05, q10, q12, q14 and q15, each on a date, so this is not a q06 problem alone.

## The change

When the filter walks the operands of an `AND` and the one it is about to run compares a column with a literal as a low end (`>` or `>=`), and another operand not yet run compares the same column with a literal of the same type as a high end (`<` or `<=`), the two are answered together by `rudb_kernels::select_range`. The strict ends are moved in by one so that both are inclusive, and a value is in the range when its distance above the low end, read unsigned, is at most the width of the range. That is one subtraction and one compare a row, in a loop the compiler vectorizes, and the answer goes straight into the selection a block of 64 rows at a time.

A flat column is tested in its own width, so a date is compared in `i32` lanes. A bit packed column is tested in code space: the range moves down by the base and is cut to the codes the width can hold, and a range that misses them keeps nothing without a code being read. When earlier operands already narrowed the rows, only those rows are read.

The plan, `EXPLAIN` and the order the operands learn do not change. Both operands of the pair are told what the pair kept, so whichever of them comes first in the learned order starts the range and the other is skipped. The kernel answers `None` for anything it has no loop for, a column with nulls, a dictionary, a type wider than 64 bits, and the walk then runs the operand on its own as it did before. An end the scan already settled from the part's bounds is not paired, so the other end runs alone. Steps built to be shared are left alone as well, because a later operand may read a slot the skipped one would have filled.

## Results

Instructions per run at SF1 on one thread, on server3, main against this change:

| query | main | after |
|---|---|---|
| q06 | 431 M | 285 M |
| q14 | 414 M | 260 M |
| q15 | 375 M | 229 M |
| q12 | 734 M | 619 M |
| q05 | 822 M | 786 M |
| q04 | 416 M | 393 M |
| q10 | 876 M | 849 M |
| q20 | 546 M | 543 M |
| q01 | 1600 M | 1600 M |

q06 is now below DuckDB's 319 M. All 22 answers match main. There are kernel tests against the two comparisons run one after the other, flat and packed, strict and not, from every row and from some, and for ends past the column or that meet nowhere, and a filter test against the tree walk with the pair in either order, beside another conjunct, doubled up, and with one end settled.

## What is left

q20's range on `l_shipdate` sits in a subquery whose filter did not take the pair and is worth a look. A range on a dictionary column or on a column with nulls still runs as two comparisons.
