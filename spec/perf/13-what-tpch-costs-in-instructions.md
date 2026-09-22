# What TPC-H costs, in instructions

Notes written on 22 September 2026 on `server3`, against main at `5ca016d2` and DuckDB `v2.0.0-dev84237`, TPC-H SF1, rudb reading its own native file and DuckDB reading its own database file.

Every note in this series so far has been ClickBench measured in wall clock. This one is TPC-H measured in instructions retired, and it reaches a different conclusion about what to do next, so the change of instrument is the first thing to explain.

## Why instructions and not seconds

Two reasons, and the second is the one that matters.

The shared boxes have a wall clock noise floor larger than most of the effects being chased. `server2` and `server3` run other people's work, and a query that takes 40 ms takes 40 ms or 90 ms depending on who else is awake. Instructions retired does not move. Best of three, a fresh process per run, rudb and DuckDB interleaved, and the same query lands within a fraction of a percent every time.

The second reason is that instructions retired is the honest measure of the second half of the goal. [`../02-the-goal.md`](../02-the-goal.md) asks for ten times less resource as well as ten times faster, and wall clock can be bought with threads. A query that spreads twice the work over four times the threads looks fast and is not cheap. Counting the work separates the two, and the separation turns out to matter: the wall clock ranking and the instruction ranking of the 22 queries are different lists.

## The suite

All 22 queries, best of three, gigainstructions. The ratio is DuckDB over rudb, so higher is better and 10.0 is the goal.

| query | rudb | DuckDB | ratio |
| --- | --- | --- | --- |
| q01 | 5.716 | 1.475 | 0.25 |
| q02 | 0.579 | 0.443 | 0.76 |
| q03 | 1.881 | 1.244 | 0.66 |
| q04 | 1.295 | 0.965 | 0.74 |
| q05 | 3.025 | 1.176 | 0.38 |
| q06 | 0.855 | 0.619 | 0.72 |
| q07 | 1.955 | 1.370 | 0.70 |
| q08 | 1.622 | 1.320 | 0.81 |
| q09 | 5.733 | 2.165 | 0.37 |
| q10 | 2.666 | 1.586 | 0.59 |
| q11 | 0.530 | 0.450 | 0.84 |
| q12 | 2.052 | 0.976 | 0.47 |
| q13 | 3.715 | 2.553 | 0.68 |
| q14 | 1.137 | 0.917 | 0.80 |
| q15 | 1.135 | 0.774 | 0.68 |
| q16 | 1.069 | 1.234 | 1.15 |
| q17 | 1.958 | 1.118 | 0.57 |
| q18 | 6.695 | 2.107 | 0.31 |
| q19 | 1.468 | 1.033 | 0.70 |
| q20 | 1.462 | 1.132 | 0.77 |
| q21 | 5.078 | 1.994 | 0.39 |
| q22 | 1.556 | 0.975 | 0.62 |

53.18 G against 27.63 G. Taking off the per process floor, which is 0.170 G for DuckDB and 0.009 G for rudb, it is 52.98 G against 23.89 G. rudb does 2.2 times the work DuckDB does on TPC-H SF1, and q16 is the only query ahead.

The goal is ten times less. Against 23.89 G of DuckDB work that is 2.39 G for the whole suite, about 0.109 G a query. rudb is at 52.98 G. **The engine has to do twenty two times less work than it does today.** No amount of tightening the loops it already runs gets there, and the rest of this note is about what does.

## What the wall clock got wrong

[#1160](https://github.com/tamnd/rudb/issues/1160) named its five worst queries from wall clock: q18, q20, q21, q13, q01. By instructions the five worst are q01, q18, q09, q05, q21.

Two queries move a long way. q20 is 0.41 on wall clock and 0.77 on instructions, and q13 is similar. Those are not queries that do too much work, they are queries that do an ordinary amount of work badly spread, and the fix for them is in the scheduler and not in the kernels. q09 is the other direction: 0.37 on instructions and nowhere near the wall clock top five, so it is a query that does far too much work and hides it behind threads.

Any work list taken from the wall clock table alone sends effort to the wrong place. That is the immediate reason this note exists.

## The floor

DuckDB costs 0.170 G to answer `select 1;`. rudb costs 0.009 G. Nineteen times cheaper to start, which is already at the goal, and it has to be subtracted before comparing anything small or the small queries look better than they are.

## What is already ahead, which is most of the engine

This is the part that changes what the work list should say. The scan, the decoder and the arithmetic are not the problem. Measured on the same box, same method:

| shape | rudb | DuckDB | ratio |
| --- | --- | --- | --- |
| `select 1` | 0.009 | 0.169 | 18.8 |
| `count(*)` over lineitem | 0.009 | 0.172 | 19.1 |
| `count` of two columns | 0.010 | 0.178 | 17.8 |
| `min` and `max` of a decimal column | 0.010 | 0.177 | 17.7 |
| `sum` of an integer column | 0.009 | 0.309 | 34.3 |
| `sum` of a decimal column | 0.183 | 0.299 | 1.63 |
| `sum` of two decimal columns | 0.404 | 0.363 | 0.90 |
| scan lineitem, read two columns | 0.010 | 0.175 | 17.5 |

Five of those eight are already past ten times, and four of them are past ten times because rudb never reads the column. `count(*)` comes from the row count in the directory, and `min` and `max` come from the persisted zone maps that [`../storage-v3/05-persisted-zone-maps.md`](../storage-v3/05-persisted-zone-maps.md) put there. The aggregate is answered out of committed metadata and the pages are never touched. That mechanism exists, it is shipped, and it is the shape of everything below.

So rudb is not a slow engine. rudb is an engine with one slow operator, and the operator is the grouped aggregate.

## Where the 2.2 times is

q18 broken into stages, best of three each:

| stage | rudb | DuckDB | cost |
| --- | --- | --- | --- |
| empty | 0.009 | 0.170 | 0.05x |
| scan lineitem | 0.009 | 0.174 | 0.05x |
| read the two columns | 0.010 | 0.175 | 0.05x |
| group by l_orderkey | 2.476 | 1.194 | 2.07x |
| group and sum | 4.143 | 1.126 | 3.67x |
| and the having | 4.164 | 1.606 | 2.59x |
| q18 whole | 6.623 | 2.301 | 2.87x |

The whole query is free until the group by, and then it is not. q01 tells the same story from the other end:

| stage | rudb | DuckDB | cost |
| --- | --- | --- | --- |
| the filter alone | 0.335 | 0.307 | 1.09x |
| and the two keys grouped | 1.380 | 0.894 | 1.54x |
| and sum quantity | 1.968 | 0.904 | 2.17x |
| and sum extendedprice | 2.509 | 1.021 | 2.45x |
| and one decimal product | 3.532 | 1.192 | 2.96x |
| and the double product | 4.217 | 1.329 | 3.17x |
| and the three averages, q01 | 5.710 | 1.469 | 3.88x |

The filter over six million rows matches DuckDB. Every aggregate call added on top costs rudb between 0.54 G and 1.02 G and costs DuckDB between nothing and 0.17 G. q01 groups into six groups, so none of that can be per group cost. It is per row cost, and it is paid once per call.

Separating the two directly:

| shape | rudb | DuckDB | cost |
| --- | --- | --- | --- |
| no filter, 6 groups, count star | 0.854 | 0.706 | 1.20x |
| no filter, 6 groups, one sum | 1.380 | 0.717 | 1.92x |
| no filter, 6 groups, two sums | 1.852 | 0.819 | 2.26x |
| no filter, no group, one sum | 0.208 | 0.269 | 0.77x |
| no filter, no group, two sums | 0.371 | 0.364 | 1.01x |
| no filter, no group, four sums | 0.813 | 0.479 | 1.69x |

Ungrouped, an added sum costs rudb about 0.16 G over six million rows and beats DuckDB. Grouped into six groups, the same sum costs about 0.50 G and costs DuckDB nothing measurable. The column is the same column and the addition is the same addition. The difference is entirely what the grouped path does around it.

And the grouping itself, with no aggregate at all, costs 0.854 G to put six million rows into six buckets, where reading the two key columns costs 0.010 G. **One hundred and forty two instructions a row to decide which of six buckets a row belongs in.**

## Why this is architecture and not a slow loop

Two facts from the profile, which together say the shape is wrong rather than the code.

rudb keeps aggregate state in a `Vec<Accumulator>` beside the hash table, indexed by slot, where `Accumulator` is a tagged enum whose widest variant is 32 bytes and a discriminant. DuckDB keeps the state inline in the hash table row. So every call rudb makes walks the rows again, re-reads the slot array, re-checks the validity mask and re-dispatches on the state tag, while DuckDB computes the row pointer once and every call is a store at a fixed offset from it. That is why the second aggregate call costs rudb as much as the first and costs DuckDB nothing, and it is why q01, with seven calls, is the worst query in the suite.

`Aggregate::split` radix partitions the input rows and not the table. It hashes every row, buckets the row numbers into 64 partitions and then calls `Rows::gather`, which copies every key column and every argument column of every row into fresh vectors. On q18's inner aggregate that is 0.42 G in the copy and another 0.42 G in the allocator and in the kernel mapping and zeroing the pages for it, which together is twenty percent of the query. DuckDB partitions the table it has already built, so it copies the groups and not the rows, and at SF1 that is 1.5 million groups against six million rows.

## What ten times would actually require

Here is the arithmetic that settles it. q01 costs DuckDB 1.468 G, so ten times is 0.147 G. The filter alone, with no grouping and no aggregate at all, costs rudb 0.335 G. **Even a free aggregate leaves q01 more than twice the target.** The only way to reach it is to stop touching six million rows.

So the work splits into two kinds, and it is worth being plain about which is which.

The first kind closes the gap to DuckDB. It is worth about 2.2 times and it stops there, because it is the work of doing the same algorithm as well as DuckDB does it. There are four pieces and all four are measured:

1. **One pass over the rows rather than one pass per call.** Make a group's state a row of a layout computed once from the call list, in an arena, rather than a run of tagged enums in a side vector. One probe, one row pointer, every call a store at a fixed offset. Worth about 0.34 G per call per six million rows, which on q01 with seven calls is around 2.4 G of its 5.162.
2. **Partition the table and not the input.** Build one table per thread and split it when it outgrows the cache, which is the two phase design [`06-partitioned-aggregation.md`](06-partitioned-aggregation.md) already describes. Worth the 0.42 G copy and most of the 0.42 G of allocator and page traffic on a high cardinality aggregate.
3. **Numeric extreme state.** A grouped `min` or `max` over a number costs 6.0 G where the same grouped `sum` costs 4.2, because it holds a `Value`, calls `settle` and `mark` on it every row, and allocates a box on every win. Holding an `i128` fixes all three.
4. **The form pairs that fall off the specialized path.** `sum(l_partkey * l_suppkey)` costs 8.803 G against DuckDB's 0.437, which is twenty times, and `rudb --fallbacks` says why in one line: `scalar flat against bit-packed 733`. There is no kernel for a flat column multiplied by a bit-packed one, so all 733 vectors of that query went row at a time. This is a cliff and not a slope, and it is a new line for the [#1088](https://github.com/tamnd/rudb/issues/1088) ledger.

The second kind is what reaches ten times, and there is only one idea in it: **decide the group at load time instead of at query time.** This is the thesis [`../engine-v2/13-encoded-execution.md`](../engine-v2/13-encoded-execution.md) already states, that the order of magnitude is in the physical layout and not in the operators. What this note adds is the TPC-H number for it. Three forms of the idea, in the order they are worth doing:

**A group by over a bounded key domain is direct addressing.** q01 groups by `l_returnflag` and `l_linestatus`, two single character columns with three and two distinct values. Stored as dictionaries, which is how the native format already stores them, the group number is `code_a * 2 + code_b`. One multiply and one add. No hash, no probe, no key comparison, no key vector to keep, no partition, no merge, and the keys are not materialized until the output, which has six rows in it. That deletes the 0.854 G that grouping six million rows into six buckets costs today, and it deletes it for q01, q12, q13 and q16. The mechanism has to be general, decided from the dictionary sizes the scan reports, and not another entry in the list of fast paths pinned to an exact call list that `crates/rudb-exec/src/group.rs` already has five of.

**A sum belongs in the directory next to the min and the max.** rudb already answers `count(*)`, `min` and `max` from committed metadata without reading the page, which is why those three cost 0.010 G against DuckDB's 0.177. A per page sum and a per page null count are the same kind of fact, the writer already has both in hand, and they cost sixteen bytes a page. `sum(l_extendedprice)` goes from 0.183 G to 0.010. On TPC-H this is small, because most of its aggregates are grouped or filtered. On ClickBench it is most of the suite. It is cheap enough to do anyway and it is the existing mechanism extended by one field.

**A column clustered at load time turns a grouped aggregate into a run scan.** If lineitem is stored ordered by `l_orderkey`, then `group by l_orderkey` has no hash table in it at all. The groups arrive in order, a group ends where the value changes, and the aggregate is a fold over runs with one accumulator live at a time. q18's inner aggregate is 4.16 G today and would be about the cost of the scan, which is 0.2 G. That is twenty times on the dominant stage of the second worst query in the suite. The cost is that the load has to sort and that only one clustering order is free per table, so this needs the load to choose it from the schema, which for TPC-H means the primary key, and it needs the aggregate to be able to prove the order from the directory rather than assume it.

## The work list

In order of measured value, which is a different order from the one the milestones were written in:

1. The group by over a bounded key domain. It deletes 0.854 G from q01 and the same from every low cardinality grouping in the suite, and it is the one item on this list where rudb ends up doing something DuckDB does not do at all.
2. The aggregate state row. It is the multiplier on everything else, because until it lands every saving is paid once per call.
3. Clustering at load time and the run scan aggregate that reads it. The largest single number on the board, 4.16 G to 0.2 on q18, and the most design work.
4. Partition the table rather than the input.
5. The flat against bit-packed kernel and the rest of the #1088 ledger. Twenty times on one shape, small in the suite total, and cheap.
6. The numeric extreme state.
7. The per page sum. Small on TPC-H, large on ClickBench, and one field in a structure that already exists.

## Two things found on the way

[#1168](https://github.com/tamnd/rudb/pull/1168) is the first one and it is already fixed. The grouped fold loops indexed the argument column as `values[at(row)]`, where `values` is a `Buffer` and not a slice, so every row re-ran the match on the store, reloaded the page pointer and the window offsets, and redid the bounds arithmetic. About fifteen instructions a row spent re-deriving a pointer that never changes. The ungrouped path already bound `as_slice()` before its loop, which is the whole reason the ungrouped sum above beats DuckDB while the grouped one does not. Binding it in the four grouped loops takes q01 from 5.716 G to 5.162 and the six group two sum shape from 1.850 to 1.575.

The second is the twenty times on `sum(l_partkey * l_suppkey)` described above, which is not fixed and needs a ledger line.

Neither changes the conclusion. The first is ten percent of one query and the second is a shape TPC-H does not contain. They are worth doing and they are not the road to ten times, and the reason for writing them down here is to be clear about the difference.
