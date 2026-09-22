# What a group costs, and why it is not arithmetic

Measured on server3 on 22 September 2026, TPC-H SF1 in the native format, instructions retired, best of three with a fresh process per run and rudb and duckdb interleaved. rudb is `main` at the merge of [#1194](https://github.com/tamnd/rudb/pull/1194) plus [#1205](https://github.com/tamnd/rudb/pull/1205).

The "Where it goes" section was replaced on the same day. The profile it was first written from was a cycles profile read as though it were an instruction profile, which put the cost in the wrong place. What replaced it is in that section, along with what the first version got wrong and why.

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

A note on how this section is measured, because the first version of it was measured wrong. `perf record` with no `-e` samples the cycles event, so its percentages are shares of time. Everything else in this note, in [note 13](13-what-tpch-costs-in-instructions.md) and in every target this project sets is instructions retired. Those two orderings are not the same and on this query they are very different, because the state vector's memory traffic is expensive in time and nearly free in instructions: zeroing a page is one `rep stos` and unmapping one is a TLB broadcast the kernel waits on. Reading a share of time as a share of instructions is how the first version of this section came to say the arithmetic was two percent and the memory system fifteen to twenty. The table below is `perf record -e instructions -c 2000000` on a profiling build of the same `main`, so it counts the same quantity the ladder above counts.

Three shapes this time, one sum against two sums against three, same query otherwise. Percentages are turned into instructions by multiplying by the ladder totals of 4.040, 5.448 and 7.288, so the columns are in G and can be subtracted. A blank is a symbol below the one percent limit, which at these totals is 0.04 to 0.07 G.

| symbol | one | two | three | added |
| --- | --- | --- | --- | --- |
| `Aggregate::fold` | 0.420 | 0.489 | 0.556 | 0.136 |
| `Aggregate::fold_slots` | 0.262 | 0.337 | 0.436 | 0.174 |
| `aggregate::scatter`, the addition itself | 0.046 | 0.214 | 0.398 | 0.352 |
| `Aggregate::fresh` | 0.173 | 0.252 | 0.357 | 0.184 |
| `Aggregate::finish` | 0.107 | 0.208 | 0.341 | 0.234 |
| `Aggregate::split` | 0.332 | 0.266 | 0.357 | 0.025 |
| `Table::insert` | 0.262 | 0.199 | 0.278 | 0.016 |
| `Vector::copied` | 0.109 | 0.176 | 0.246 | 0.137 |
| `vector::unpack` | | | 0.180 | 0.140 |
| `mi_malloc` | 0.116 | 0.135 | 0.192 | 0.076 |
| `integer::decode_chunk` | 0.164 | 0.211 | 0.192 | 0.028 |
| `clear_page_rep`, kernel | | 0.120 | 0.180 | 0.140 |
| `table::hash` | 0.219 | 0.196 | 0.164 | -0.055 |
| `Table::probe_at` | 0.191 | 0.170 | 0.173 | -0.018 |
| `vector::push_value` | 0.061 | 0.111 | 0.168 | 0.107 |
| `vector::copy_of` | 0.103 | 0.161 | 0.155 | 0.052 |
| `Accumulator::combine` | 0.073 | 0.091 | 0.136 | 0.063 |
| `drop_glue::<Value>` | 0.042 | 0.068 | 0.120 | 0.078 |
| `update_scattered` | 0.067 | 0.059 | 0.111 | 0.044 |
| `group::merge_slot` | 0.061 | 0.096 | 0.101 | 0.040 |
| `Accumulator::finish` | | | 0.098 | 0.060 |
| `__memmove_avx_unaligned_erms` | 0.048 | 0.100 | 0.095 | 0.047 |

`Accumulator::new` was 2.11 percent of the cycles profile and is not here at all, because [#1205](https://github.com/tamnd/rudb/pull/1205) landed in between and a new group now clones a template rather than matching on a name.

Two added calls cost 3.248 G. Group the named lines by what they are doing and 2.16 G of that is accounted for, the rest being below the limit and spread thin:

| what | added | of the 3.248 |
| --- | --- | --- |
| finishing a state into an output value, per group per call | 0.479 | 15 percent |
| reading two more columns, per row | 0.447 | 14 percent |
| starting a group's state, per group per call | 0.447 | 14 percent |
| the addition itself, per row per call | 0.396 | 12 percent |
| walking the chunk again for the added call | 0.310 | 10 percent |
| merging partial tables, per group per call | 0.103 | 3 percent |
| the key side of the table | -0.032 | 0 percent |

Three things in that table are worth saying out loud because they are not what the cycles profile said.

**The arithmetic is twelve percent of an added call, not two.** `scatter` and `update_scattered` together go from 0.113 G to 0.509 G. That is still the minority of what a call costs, which is the point of this note, but it is six times the share the first version claimed and it sets a floor on what any redesign can save.

**The state vector's memory traffic is about five percent, not fifteen to twenty.** `clear_page_rep`, `memmove` and `memset` together are 0.355 G of the 7.288 on the three sum shape. In time they are large, and they are the reason the wall clock on this query is worse than the instruction count suggests, but the project is judged in instructions and in instructions they are a fifth of what the cycles profile implied.

**The largest single per call item is turning a finished state into an output value.** `Aggregate::finish`, `Accumulator::finish`, `push_value` and dropping a `Value` grow by 0.479 G between one call and three. That is `finishing` at `crates/rudb-exec/src/group.rs:1798`, which asks each accumulator for a `Value`, pushes it into a `Vec<Value>`, and then hands the whole vector to `Vector::from_values`, which pushes each `Value` again into the vector's flat data and then drops all of them. A million and a half groups times a call is a million and a half round trips through a boxed, tagged, owning value to move eight bytes. The comment above that loop already says it is per group and points at #61. It is a separate change from the state row, it is much smaller than the state row, and on this evidence it is worth more.

Peak resident memory says the memory side from outside the process:

| shape | rudb | duckdb |
| --- | --- | --- |
| 1.5M groups, one sum | 184 MB | 139 MB |
| 1.5M groups, three sums | 340 MB | 243 MB |

An added call costs rudb about 78 MB and duckdb about 52 MB. `Accumulator` is 32 bytes, so a million and a half of them is 48 MB, and a `Vec` that reaches 48 MB by doubling has touched about 96 MB on the way. That is the 78 MB, and it is the whole of the "ten times less resource" half of the goal on this shape. It is not, by the table above, much of the instruction count.

## Why it is shaped like that

rudb keeps a group's aggregate state in a flat `Vec<Accumulator>` beside the hash table, indexed by `slot * calls + at`. duckdb keeps the state inline in the hash table row.

Four consequences, all of them visible in the tables above.

**One array per operator that has to grow.** The state vector is sized by groups times calls and it is not known in advance, so it doubles. Doubling a 48 MB vector copies it, and the pages it lands in are new pages the kernel has to zero, and the pages it leaves are unmapped. duckdb's state lives in the same blocks as the rows, which are allocated in fixed size chunks that are never copied and never doubled. This is the whole of the peak memory difference and a good part of the wall clock one, and by the profile above it is about five percent of the instructions, so it is a resource problem rather than a work problem.

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

What it is worth, from the profile rather than from the ladder. Of the 3.248 G that two added calls cost, the state row addresses starting a group's state at 0.447, walking the chunk again at 0.310 and merging partials at 0.103, which is 0.86 G, and it leaves the addition itself and the column reads alone because those are work the query asked for. It does not address finishing a state into a value, which is the largest single item at 0.479 and wants its own change.

That is a smaller claim than the first version of this note made and it is the one the measurement supports. The reason to do the state row anyway is not its own 0.86 G. It is that the per group cost is 770 instructions against duckdb's 250 and three of the four things that make up that number are consequences of where the state lives.

It is also the multiplier on everything else on note 13's list, and that is the real argument for doing it first. [#1201](https://github.com/tamnd/rudb/issues/1201), the direct map over a key domain the file bounds, removes the probe. Removing the probe is worth less while every aggregate call still walks the rows again afterwards. Clustering and the run scan aggregate remove the table. Removing the table is worth less while the state is not in it.

Before either of those, though, is the finalise. A typed finalise per call that writes straight into the output vector's flat data, with no `Value` built, pushed, copied and dropped per group, is worth 0.479 G of 3.248 on this shape, it needs no layout change, and it is not blocked on [#1114](https://github.com/tamnd/rudb/issues/1114) the way the state row's third stage is. It is [#1252](https://github.com/tamnd/rudb/issues/1252).

So the order is the typed finalise, then the state row, then the direct map, then clustering. That is note 13's order with one cheap thing moved to the front of it, and the thing that moved it there is the difference between a cycles profile and an instruction profile.

## What this note does not claim

The 770 instructions per group per call is a division, not a profile line. It says the per group cost exists and roughly how large it is. The profile separates two thirds of an added call into named groups and leaves the last third below its one percent limit, so the group sizes in the second table are lower bounds rather than a partition. Landing the typed finalise and the state row and re-running this ladder is what will say the rest.

The instruction profile and the cycles profile disagree about this query and both of them are right about what they measure. A change that removes the doubling of the state vector will show up in the wall clock and in peak memory and barely at all in the instruction count. The goal has two halves and this note only measures one of them properly.

The ladder is one thread's worth of shape at SF1. The partition and merge costs scale with threads and with groups, so the same measurement at SF10 would weigh the merge more heavily than this one does. That is worth doing once the state row lands, because the merge is the part of the design with the most freedom left in it.
