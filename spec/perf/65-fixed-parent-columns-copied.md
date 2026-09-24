# 65. Fixed width parent columns copied

## The problem

With `graph_sections` on, q09 joins `lineitem` to `partsupp` and to `orders` through stored links, and at SF1 on one thread it retired 2.35 G instructions against about 2.0 G for the hash join plan. A profile had `Data::signed_at` at 12 percent of the time, with `cast::convert`, `value_at` and `call_values` close behind, which is a kernel reading a value at a time.

The parent columns q09 reads are `ps_supplycost`, a decimal, and `o_orderdate`, a date. The link join handed each of them up as a gather, a form that points at the parent column through the row ids and reads nothing until asked. None of the arithmetic, cast or date kernels reads a gather in bulk, so `ps_supplycost * l_quantity` and `extract(year FROM o_orderdate)` each fell back to asking the gather for one row, which asked the parent column for one row, for every row of every chunk.

## The change

`LinkJoin::gather` copies a parent column of fixed width values straight into a flat vector with `Vector::gather`, which is a typed loop over the row ids. That is four or eight bytes a row, the same as the ids the gather would have held, and what comes out is a form every kernel reads as a slice. Every row id is still checked against the length of the parent column first, which is the check the gather used to make. A chunk where some child has no parent, which only a left join has, keeps the gather, since that is what makes the missing rows null. Strings stay a gather, or codes when the table holds them as codes (spec/perf/64), because copying a string is not cheap.

## Results

Instructions per run at SF1 on one thread with the setting on, on server3, main against this change:

| query | before | after |
|---|---|---|
| q09 | 2349 M | 1633 M |
| q12 | 738 M | 734 M |
| q13 | 898 M | 887 M |
| q10 | 885 M | 882 M |

q03, q05, q07 and q08 did not move. All 22 answers with the setting on match main with it off. There is a new test for a fixed width parent column copied flat, left as a gather when a row has no parent, refused past the end of the column, and declined for strings. The test that held an integer parent column to be a gather now holds it to be one copied value per child row.

## What is left

q09 with the setting on is now about 20 percent under the hash join plan, and q13 is about 15 percent under it, with nothing measured worse. That makes the case for turning `graph_sections` on by default, which is the next thing to measure across all 22 queries.
