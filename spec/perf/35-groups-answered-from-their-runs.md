# Groups answered from their runs

Notes written on 24 September 2026, on q18, which was the query furthest behind DuckDB after q01.

## The question

q18 finds the orders whose lines add up to more than 300 units, which is `GROUP BY l_orderkey HAVING sum(l_quantity) > 300` over all of lineitem, and then joins the 57 orders it finds back to their customers and lines. On server3 the whole query was 2.85 G instructions against DuckDB's 1.91 G, and taking it apart put nearly all of the gap in the grouping:

| query over lineitem | rudb | DuckDB |
|---|---|---|
| a count and a sum, no grouping | 0.19 G | 0.28 G |
| the sum grouped by `l_orderkey` | 1.72 G | 1.14 G |

lineitem is stored in order of `l_orderkey`, and the `aggregate_cluster` pass tells the aggregate so. A group whose rows sit strictly inside one chunk cannot have a row anywhere else, so it is closed there and skips the hash table. But a closed group still went into a table of its own, its key copied a row at a time, and got a fresh accumulator for each call, and then every row was folded into its accumulator through a slot. At about four lines to an order that is a table row, an accumulator and four slot updates for each group, which came to about 250 instructions a row. Adding up four numbers that already sit next to each other should cost a few.

## What changed

When every call of the aggregate is a `count(*)`, a `count` or a `sum` whose answer is the sum of the raw integers, which is a total over an integer column or over a decimal at the scale the total is declared at, the groups a sorted chunk closes are now answered on the spot as a chunk of their own (`Aggregate::close_runs` in `crates/rudb-exec/src/group.rs`). The runs of the key are found with the same pass the table path used. The key column is gathered once at the first row of every run, and each call is one pass over its argument that adds each run up where it lies into an `i128`, skipping nulls, with a run of nothing but nulls answered as null. The answers go through the same range check the accumulators' finish uses (`whole_answers` in `crates/rudb-kernels/src/aggregate.rs`), so a total that does not fit its declared type raises the same error. The chunks wait beside the rest of the answer until the aggregate finishes.

Anything else leaves the chunk to the table as before: a `FILTER` clause, a distinct call, a mean, a minimum or maximum, a `HAVING count(*)` or top count selection made from the accumulators, and an argument in a layout the pass does not read. The first and last runs of a chunk still go the ordinary way, since either can carry on into the next chunk.

## Numbers

On server3 against the native file at SF1, instructions over all threads, three runs each:

| | main | this change | DuckDB |
|---|---|---|---|
| the sum grouped by `l_orderkey` | 1.72 G to 1.74 G | 0.86 G to 0.87 G | 1.14 G |
| the same with `HAVING sum(l_quantity) > 300` | 1.77 G to 1.82 G | 0.93 G | 1.49 G |
| q18 | 2.86 G | 2.04 G | 1.91 G |

q18's CPU time over three runs each, with server3 under a load of about 25, was 953 ms to 1130 ms on main, 606 ms to 798 ms with this change and 1074 ms to 1472 ms for DuckDB.

The 22 TPC-H queries, best of three: q18 went from 2.851 G to 2.033 G and the total from 28.227 G to 27.456 G. The other 21 moved by less than one percent either way, which is the noise of counting over all threads. Every one of the 22 answers is the same as main's.

## What this leaves

What is left of q18's gap is the second read of lineitem and the join back to it. The subquery has already added up every order's lines, and the outer query adds up the lines of the 57 orders that pass again, so the next step is to hand the subquery's totals to the outer grouping rather than read lineitem twice. The run pass could also take a mean, which is a total and a count, and a minimum or maximum over a number, which would let a clustered grouping of any of the usual calls skip the table.
