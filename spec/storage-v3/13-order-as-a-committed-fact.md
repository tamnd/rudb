# Order as a committed fact

## Decision

The native directory records two more facts per column per snapshot. Whether the committed rows are non-decreasing in that column, and whether the column's values are distinct and cover a contiguous integer range with no gaps.

Both are proved by the writer and carried in the directory beside the bounds and the null count that `05-persisted-zone-maps.md` already puts there. Both are one sided in the same way those are: present, they license a cheaper plan, absent, the ordinary plan runs and the answer is the same.

A reader that has them may replace a hash table with direct addressing or with a scan over runs. A reader that does not have them must not guess, and a writer that cannot prove them must leave them unset.

## First-principles reason

A hash aggregate and a hash join exist to answer one question: which rows share a key. Hashing is how you answer that when you know nothing about where the rows are. It costs a hash, a probe, a table, a partition and a merge, and rudb spends 2.2 times what DuckDB spends on TPC-H SF1 doing exactly those five things, which `../perf/13-what-tpch-costs-in-instructions.md` measures.

But rows that share a key are adjacent when the table is stored in key order, so the question answers itself. And a key is a row number when the keys are dense integers, so there is no question. Neither of those is a trick. They are properties the data already has, and the only reason the engine pays to rediscover which rows share a key is that nothing wrote down what it already knew.

This is the argument the engine already accepts elsewhere. `count(*)` is answered from the row count and `min` and `max` from the zone maps, and those three cost 0.010 G of instructions over six million rows where DuckDB pays 0.177, because the page is never read. Order and density are facts of the same kind, held by the same writer, at the same moment, and the same argument applies to them.

## What the data actually is

TPC-H SF1 as rudb loads it today, `server3`, counted with a window function over the stored order:

| column | inversions | rows |
| --- | --- | --- |
| lineitem.l_orderkey | 0 | 6001215 |
| lineitem.l_shipdate | 2982601 | 6001215 |
| lineitem.l_partkey | 3000214 | 6001215 |
| lineitem.l_suppkey | 2999706 | 6001215 |
| orders.o_orderkey | 0 | 1500000 |
| orders.o_orderdate | 750023 | 1500000 |
| customer.c_custkey | 0 | 150000 |
| part.p_partkey | 0 | 200000 |
| partsupp.ps_partkey | 0 | 800000 |

Every primary key in the schema is already stored non-decreasing, with zero inversions, and no other column is. The ones that are not sorted have inversions on about half their rows, which is what random order gives, so this is not a near miss anywhere. The file is either ordered on a column or it is not.

And the domains:

| column | min | max | distinct | rows |
| --- | --- | --- | --- | --- |
| customer.c_custkey | 1 | 150000 | 150000 | 150000 |
| part.p_partkey | 1 | 200000 | 200000 | 200000 |
| supplier.s_suppkey | 1 | 10000 | 10000 | 10000 |
| orders.o_orderkey | 1 | 6000000 | 1500000 | 1500000 |
| lineitem.l_orderkey | 1 | 6000000 | 1500000 | 6001215 |

Three of the five are dense, sorted and unique over one to N. In `customer`, row *i* holds `c_custkey` *i* plus one. There is no lookup to do. The same for `part` and `supplier`.

None of this was arranged for. It is what the loader wrote from the file it was given, because TPC-H is generated in key order and rudb preserves the order it reads. The facts are in the file today and the engine has no way to know them.

## What each fact buys

**A grouped aggregate over an ordered column has no hash table in it.** The groups arrive in order, a group ends where the value changes, and the fold keeps one accumulator live at a time. No hash, no probe, no table to size, no partition, no spill and no merge of two tables. q18's inner aggregate, `group by l_orderkey` over six million rows into 1.5 million groups, costs 4.16 G of instructions today. A run scan over the same rows is about what the scan costs, which is 0.2 G.

**A join to a dense, sorted, unique integer key is an array index.** Every join to `customer`, `part` or `supplier` in TPC-H is `row = key - min`. No build, no hash, no probe, no collision, and no memory beyond the column already being read. That is five of the eight joins in the suite, and it is the whole of the build side of q02, q14, q16, q17, q19 and q20.

**A join to a sorted unique sparse key is a merge or a binary search.** `orders.o_orderkey` is sorted and unique over 1.5 million rows in a domain of six million, so it is not dense. Sorted and unique is still enough to drop the hash table, either by merging against a sorted probe side, which `lineitem.l_orderkey` is, or by a direct index of six million `u32` row numbers, which is 24 MB and smaller than the hash table for 1.5 million rows that is built instead.

**The parallel story gets better rather than worse.** This is the part that usually kills a sort based plan and here it goes the other way. A hash aggregate on N threads is N hash tables that have to be merged, and the merge is proportional to the number of groups, which is why `../perf/13-what-tpch-costs-in-instructions.md` finds 0.42 G in the partition copy and another 0.42 G in the allocator behind it. A run scan on N threads is N morsels, and a group can only be split where a morsel boundary falls inside it, so there is at most one partial group per boundary. The merge is proportional to the number of threads. At SF1 that is 1.5 million partial groups to merge against about 32.

## How it is proved, and why it is nearly free

The writer already visits every value of every column to build the zone map. Proving non-decreasing is one compare against the previous value on that pass, and it stops early the first time it fails, which on an unsorted column is after a handful of rows.

Most of the proof does not need the per row pass at all. A column is non-decreasing across pages exactly when `page[i].max <= page[i + 1].min` for every adjacent pair, and both of those are already committed in the directory. So the cross page half of the proof is derivable from data that is already written, for every file that already exists, at the cost of reading the directory. Only order within a page needs the compare.

Density needs a minimum, a maximum and a distinct count. The first two are in the zone map. The third is the one new thing the writer has to carry, and for a column that is being proved non-decreasing on the same pass it is free, because a non-decreasing column's distinct count is its run count.

There is a weaker fact worth having on its own. If page ranges are disjoint, meaning `page[i].max < page[i + 1].min` strictly, then no value appears in two pages, so every group of a `group by` on that column lives entirely inside one page. An aggregate over such a column is per page, independent, with no exchange and no merge at all. That one is purely derivable from committed zone maps, costs nothing to establish on an existing file, and is the best parallel shape available for a grouped aggregate.

## Encoding

Per schema column, in the per snapshot column entry that already holds the bounds and the null count:

- one bit, the rows are non-decreasing in this column
- one bit, the page ranges are disjoint
- one bit, the values are distinct
- an optional dense domain, which is a lower bound and a count, set only when the values are distinct and cover the range with no gaps

Four bits and two integers a column. The bits are set only when proved. A file written by an older writer has them clear, which is exactly the behaviour a reader needs anyway.

This is a directory grammar change, so it is a format version bump, with the older reader rejecting the newer file rather than guessing where entries end, which is how `05-persisted-zone-maps.md` handled the same problem.

## What clears the bits

An append clears non-decreasing unless the first value of the appended run is at or above the last committed maximum. That exception is not a corner case, it is the common one: a table that grows by time or by an increasing identifier stays ordered forever, and those are the tables where a grouped aggregate on the ordered column is also the query people run.

A delete does not clear it. Deleting rows from a non-decreasing column leaves it non-decreasing. It can break density, so the dense domain clears unless the delete is proved to be a suffix or a prefix.

An update clears everything about the column it touches, until the next full rewrite proves it again.

## What this is not

It is not a sort at load time. Nothing here asks the loader to reorder anything, and the whole of the measurement above is about noticing what is already true. A later note can argue for sorting on load, and it will have to argue about load time and about which single order to pick, which are real costs. This note has no such costs because it adds no work: it is one compare per value on a pass the writer already makes, and for the cross page half it is not even that.

It is not a guarantee about any other dataset. TPC-H is generated in key order, which is why every key in it is sorted, and the honest reading of the table above is that a generated benchmark is friendly to this. What makes it worth building anyway is that the friendly shape is also the common one in practice, because real tables are usually appended in time order or identifier order, and because the fallback costs nothing. ClickBench's `hits` has not been measured for this and nothing should be claimed about it until it has.

It is not a licence for the reader to assume. Every use of these facts needs the ordinary plan intact behind it and a test that forces the fallback rather than hoping for it, which is the rule `../planner-v2/13-explain-and-testing.md` section 13.3 already sets for answering from statistics.

## Measured result

Not implemented. The numbers above are what it would remove, measured on what the engine spends today.

TPC-H SF1 by instructions retired, rudb 52.98 G against DuckDB 23.89 G. The grouped aggregate carries all of the gap. q18's inner aggregate is 4.16 G of it and a run scan would make it 0.2 G. Five of the eight joins in the suite are to a dense sorted unique key and would have no hash table in them at all.

## Depends on

P0, for the facts to reach the planner as facts with classes rather than as a side channel. The planner has to be able to ask whether a column is ordered and get a yes that it may act on, and the `EXPLAIN` output has to say which of the two plans it chose and why, or none of this is testable.
