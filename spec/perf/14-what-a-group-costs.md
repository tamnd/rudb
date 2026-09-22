# What a group costs, and why it is not arithmetic

Measured on server3 on 22 September 2026, TPC-H SF1 in the native format, instructions retired, best of three with a fresh process per run and rudb and duckdb interleaved. rudb is `main` at the merge of [#1194](https://github.com/tamnd/rudb/pull/1194) plus [#1205](https://github.com/tamnd/rudb/pull/1205).

[Note 13](13-what-tpch-costs-in-instructions.md) established that every bit of the TPC-H gap is in the grouped aggregate and listed seven things to do about it. This note takes the top of that list apart properly, because two of the seven are large enough to need a design rather than a patch and neither had a number saying which to do first.

The question this note answers is narrow and it is the one that decides the order: **when a grouped aggregate gets one more aggregate call, what does rudb actually spend?**

## The ladder

Two cardinalities, the same six million rows, calls added one at a time. The low cardinality shape carries q01's filter so that the answer cannot come out of the frequency synopsis, which is what happens to an unfiltered single column grouped count and which would otherwise measure nothing.

| shape | rudb | duckdb | cost |
| --- | --- | --- | --- |
| 6 groups, no call | 1.098 | 0.876 | 1.25x |
| 6 groups, one sum | 1.498 | 0.959 | 1.56x |
| 6 groups, two sums | 1.872 | 1.019 | 1.83x |
| 6 groups, three sums | 2.228 | 1.111 | 2.00x |
| 1.5M groups, no call | 2.490 | 1.150 | 2.16x |
| 1.5M groups, count star | 2.966 | 1.619 | 1.83x |
| 1.5M groups, one sum | 4.040 | 1.613 | 2.50x |
| 1.5M groups, two sums | 5.448 | 1.984 | 2.74x |
| 1.5M groups, three sums | 7.288 | 2.190 | 3.32x |

Read down the marginal cost of one added `sum`, which is the number that matters:

| added call | 6 groups | 1.5M groups |
| --- | --- | --- |
| rudb, first sum | 0.400 | 1.550 |
| rudb, second sum | 0.374 | 1.408 |
| rudb, third sum | 0.356 | 1.840 |
| duckdb, first sum | 0.083 | 0.463 |
| duckdb, second sum | 0.060 | 0.371 |
| duckdb, third sum | 0.092 | 0.206 |

At six groups an added sum costs rudb 0.38 G and duckdb 0.08 G, so the per row part of a call is about 64 instructions a row against duckdb's 13.

At 1.5 million groups the same added sum costs rudb 1.55 G. The column is the same column, the addition is the same addition and the row count is the same row count, so the extra 1.17 G is not per row. Divided by the groups it is **about seven hundred and seventy instructions per group per call**, and duckdb's equivalent is about two hundred and fifty. Nothing about summing a decimal into a running total costs seven hundred instructions. That number is not arithmetic and it has to be found somewhere else.

Note the direction of the third row as well. rudb's marginal call gets *more* expensive as calls are added, 1.550 then 1.408 then 1.840, while duckdb's gets cheaper, 0.463 then 0.371 then 0.206. An engine whose per call cost rises with the call count is paying for something that grows, and an engine whose per call cost falls is amortising a fixed setup over more work. Those are two different shapes, not two constants.

## Where it goes

`perf record` on the profiling build, one sum against two sums, same query otherwise, percent of samples:

| symbol | one sum | two sums |
| --- | --- | --- |
| `smp_call_function_many_cond`, kernel | 13.57 | 9.94 |
| `Table::probe_at` | 8.42 | 6.26 |
| `Table::insert` | 7.14 | 4.69 |
| `Aggregate::fold` | 4.98 | 5.11 |
| `Aggregate::split` | 4.45 | 3.39 |
| `Aggregate::fold_slots` | 3.43 | 5.37 |
| `Aggregate::fresh` | 2.39 | 5.63 |
| `Accumulator::new` | 2.11 | 2.25 |
| `__memmove_avx_unaligned_erms` | 2.46 | 4.68 |
| `clear_page_rep`, kernel | 2.56 | 4.65 |
| `aggregate::scatter`, the addition itself | below the limit | 1.98 |
| `integer::decode_chunk`, reading the column | 3.83 | 2.21 |

The four entries that grow when a call is added are `fresh` 2.39 to 5.63, `fold_slots` 3.43 to 5.37, `memmove` 2.46 to 4.68 and `clear_page_rep` 2.56 to 4.65. Every one of those is the cost of having a second array of a million and a half states: making it, growing it, copying it while it doubles, and asking the kernel for pages to put it in. The addition the query was written to perform is two percent.

`smp_call_function_many_cond` is the kernel broadcasting TLB invalidations, which is what a process does when it unmaps memory. Together with `clear_page_rep` the memory system is between fifteen and twenty percent of this query at both call counts, and it is there because the state vectors are large, short lived and doubled into existence.

Peak resident memory says the same thing from outside the process:

| shape | rudb | duckdb |
| --- | --- | --- |
| 1.5M groups, one sum | 184 MB | 139 MB |
| 1.5M groups, three sums | 340 MB | 243 MB |

An added call costs rudb about 78 MB and duckdb about 52 MB. `Accumulator` is 32 bytes, so a million and a half of them is 48 MB, and a `Vec` that reaches 48 MB by doubling has touched about 96 MB on the way. That is the 78 MB, and it is also the memmove and the page clearing above.

## Why it is shaped like that

rudb keeps a group's aggregate state in a flat `Vec<Accumulator>` beside the hash table, indexed by `slot * calls + at`. duckdb keeps the state inline in the hash table row.

Four consequences, all of them visible in the tables above.

**One array per operator that has to grow.** The state vector is sized by groups times calls and it is not known in advance, so it doubles. Doubling a 48 MB vector copies it, and the pages it lands in are new pages the kernel has to zero, and the pages it leaves are unmapped, which is the TLB broadcast. duckdb's state lives in the same blocks as the rows, which are allocated in fixed size chunks that are never copied and never doubled.

**A pass per call rather than a pass per row.** Because the states of one group are `calls` entries apart, each call walks the slot array again from the start. Every call re-reads the slots, re-checks the validity mask and re-dispatches on the state tag. With the state inline the row pointer is computed once and every call is a store at a fixed offset from it, which is why duckdb's marginal call gets cheaper as calls are added.

**A tag per state.** `Accumulator` is a tagged enum, so every update on the general path branches on a discriminant that was decided when the query was planned. The vectorized paths hoist that branch, but the state still carries the tag and still costs the space.

**A merge per state.** Partial tables are merged per group, and with the state in a side vector the merge is a probe, an index computation and a tag dispatch per group per call rather than a walk of two rows.

None of this is a slow loop. It is the same algorithm as duckdb's with the state kept somewhere else, and the price of keeping it somewhere else is the whole of the difference in the ladder.

## What follows

The change is [#1173](https://github.com/tamnd/rudb/issues/1173), the aggregate state row, and this note is the number behind it. A group's state becomes one row of a layout computed once from the call list, allocated in the table's own blocks, so that:

- the state is allocated where the row is, in fixed size blocks, never doubled and never copied
- one probe yields one pointer and every call is a store at a known offset from it
- the layout is decided at plan time, so the per row work has no tag to read
- a merge walks two rows rather than probing once per call

What it is worth, from the ladder. The per group part of a call is the 1.17 G difference between the 6 group and 1.5M group marginal costs. Three of the four profile entries that grow with the call count are the state vector's own memory traffic and the state row deletes all three. On the 1.5M group three sum shape that is somewhere between 2 and 3 G of 7.288, and on q01, which has seven calls over six groups, it is most of the per call 0.38 G times seven.

It is also the multiplier on everything else on note 13's list, and that is the real argument for doing it first. [#1201](https://github.com/tamnd/rudb/issues/1201), the direct map over a key domain the file bounds, removes the probe. Removing the probe is worth less while every aggregate call still walks the rows again afterwards. Clustering and the run scan aggregate remove the table. Removing the table is worth less while the state is not in it.

So the order is the state row, then the direct map, then clustering, which is note 13's order with a number now attached to why.

## What this note does not claim

The 770 instructions per group per call is a division, not a profile line. It says the per group cost exists and roughly how large it is. It does not say which of the four consequences above is the largest part of it, and the profile only separates the memory traffic, which is about half. Landing the state row and re-running this ladder is what will say the rest, and that measurement is part of #1173 rather than a thing to argue about first.

The ladder is one thread's worth of shape at SF1. The partition and merge costs scale with threads and with groups, so the same measurement at SF10 would weigh the merge more heavily than this one does. That is worth doing once the state row lands, because the merge is the part of the design with the most freedom left in it.
