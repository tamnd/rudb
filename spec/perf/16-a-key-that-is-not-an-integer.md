# A key that is not an integer

Measured on server3 on 22 September 2026, TPC-H SF1 in the native format, instructions retired, best of three with a fresh process per run and every binary interleaved. Every before and after pair in this note comes from two binaries built out of one tree, the second one with the changed file overwritten by its base version and rebuilt, so the two differ in that file and in nothing else.

[Note 15](15-what-a-row-costs.md) found a grouped `min` over a `DATE` costing six times what the same `min` over an `INTEGER` cost, because the type was missing from one list. This note is the same defect one operator over, found the same way and worth more. It also writes down two things that did not work, because both of them cost a day and neither of them should cost anyone else one.

## How it was found

The question this started from was whether q01 can be rewritten to do less arithmetic. `sum(l_extendedprice * (1 - l_discount))` over a column with eleven distinct discounts is a grouped sum over `(l_returnflag, l_linestatus, l_discount)` folded afterwards, which turns five million multiplies into eleven. Written out as SQL it gives byte correct answers with the right decimal types, so the idea is sound.

| q01 | rudb | duckdb |
| --- | --- | --- |
| as written | 4.508 | 1.474 |
| over cells | 13.371 | 2.110 |

Three times worse on rudb and half again worse on duckdb. The rewrite removes arithmetic and adds a key column, and on rudb a key column costs far more than the arithmetic it saves. That is the refutation, and it is worth keeping: nobody should implement this rewrite until a key column is cheap.

Then the obvious question is what a key column costs, which nobody here had measured on its own.

## What a key column costs, by type

One group by over `lineitem` behind q01's filter, counting rows, changing only which column is the key.

| group by, filtered | rudb | duckdb | ratio |
| --- | --- | --- | --- |
| a varchar key | 0.852 | 0.738 | 1.15x |
| an integer key | 2.379 | 0.714 | 3.32x |
| a bigint key | 3.074 | 0.920 | 3.33x |
| a date key | 5.842 | 0.632 | 9.23x |
| a decimal key | 5.928 | 0.651 | 9.09x |

A string key is the cheapest thing on that list and a date is seven times a string. That is not a cost model, that is a hole.

## The hole

A group key column is stored in `Column`, and `Column` has a run per type: `Vec<i8>`, `Vec<i16>`, `Vec<i32>`, `Vec<i64>`, a string column, and `Vec<Stored>` for everything else. `Stored` is a tagged value. So a `DATE`, a `TIME`, a `TIMESTAMP`, a `DECIMAL` and a `HUGEINT` key all landed in the last of those, and the compare that runs once per input row per probe step built a `Value` on each side and compared the two.

Over a packed column that is worse than it sounds. A packed row has no value to hand over, so building one calls `unpack` for a single row, which allocates. The profile of the decimal key query says so plainly:

| symbol | share |
| --- | --- |
| `vector::unpack` | 14.22 |
| `Vector::value_at` | 13.25 |
| `Table::probe_at` | 6.36 |
| `table::hash` | 5.92 |
| `drop_glue<Value>` | 5.48 and 4.95 |
| `mi_malloc` | 5.39 |
| `empty_data_for` | 5.12 |
| `value_from` | 4.24 |

Nine of the top ten lines are the allocation and the two values, not the comparison. `--fallbacks` reports that every kernel call took a specialized path, so this is not a kernel falling back. It is one list of types, in one file, that a type is not in.

## The change

The five types that are one signed integer get a run of their own. A date is a day count, a time and a timestamp are microsecond counts, and a decimal and a hugeint are integers that can want all 128 bits, so all five are held as an `i128` and compared as one. That is the same grouping `Vector::signed_at` already makes and the same one `rudb_kernels::aggregate` makes for a grouped `min`, which is note 15's change arriving at the same set of types from the other side.

The batched compare gets an arm for the packed form and arms for the four narrower physical widths a decimal can be stored in, since a `DECIMAL(4, 2)` is two bytes a value and a `DECIMAL(30, 2)` is sixteen and both are stored here as the wider of the two.

| group by, filtered | before | after | duckdb | was | now |
| --- | --- | --- | --- | --- | --- |
| a varchar key | 0.852 | 0.845 | 0.738 | 1.15x | 1.14x |
| an integer key | 2.379 | 2.384 | 0.714 | 3.32x | 3.33x |
| a bigint key | 3.074 | 3.085 | 0.920 | 3.33x | 3.35x |
| a date key | 5.842 | 2.374 | 0.632 | 9.23x | 3.75x |
| a decimal key | 5.928 | 2.397 | 0.651 | 9.09x | 3.67x |
| a date and a decimal key | 7.730 | 4.912 | 1.844 | 4.19x | 2.66x |
| a date key, no filter | 5.297 | 1.794 | 0.522 | 10.13x | 3.43x |
| a decimal key, no filter | 0.421 | 0.420 | 0.578 | 0.72x | 0.72x |
| q01 | 4.382 | 4.377 | 1.485 | 2.95x | 2.94x |
| q03 | 1.818 | 1.818 | 0.943 | 1.92x | 1.92x |
| q10 | 2.586 | 2.567 | 2.149 | 1.20x | 1.19x |

A date key goes from 9.23 times behind to 3.75, a decimal key from 9.09 to 3.67, and a date key with no filter at all from 10.13 to 3.43. Two of those are better than two and a half times less work for the same answer.

The rows that do not move are the point of running them. A varchar, an integer and a bigint key took the right run before and take the same run now. q01, q03 and q10 are flat because q01 and q03 group on keys that already had runs and q10's cost is elsewhere. The decimal key with no filter was already ahead of duckdb and stays there.

What is left is the shape of the answer rather than a number in it. A date key is now 3.75 times behind and an integer key is 3.33, which is the same cost. A wide key is no longer a special case, and every fixed width key is now behind for one reason rather than two.

## The hash the test found

The test written for the change asserts that a column hashes the same in every form it can arrive in, which is the invariant the whole module rests on, and it failed on a `DECIMAL(9, 2)`.

A decimal was hashed as two words in every path that had the value in hand. But a `DECIMAL(9, 2)` column is a run of `i32`, and the pass over a run hashes four bytes as one word. So the same value hashed one way when the column arrived flat and another when it arrived packed. Our own format decides packing page by page, so that is one column of one table, and a group by over it would have returned the same decimal twice.

A decimal is now wide only when its width says it is stored in 128 bits, asked in one place and answered off `LogicalType::physical` rather than off a second copy of the digit ranges, so the hash and the layout cannot drift apart. Nothing about `HUGEINT` changes, because that one really is 128 bits in every form.

This is the second bug in two notes that was found by a test over a type rather than by a test over a query, and it is the same lesson: the list of types a loop takes was written from the queries that existed.

## What did not work

Two ideas were measured and dropped before the one above. Both looked obviously right.

**Flattening the dictionary the filter leaves behind.** A filter over a native column produces a dictionary whose codes are the kept rows and whose payload is the whole chunk column, so every read above the filter pays an indirection. Unpacking it once looked free. It is not. The engine already has a seam for exactly this decision, `seam.chunk.compaction`, with three strategies, and all three measure the same on q01: 4.513 for never, 4.516 for the fixed threshold, 4.533 for the learned gain. The arithmetic agrees: unpacking costs about ten instructions a value and saves about fifteen a row, so it breaks even at about one reader, and q01 reads each aggregate column once or twice. The engine's own cost model was right and there was nothing to win.

**The guard that could not fire.** The first attempt at the above was a guard in the vector layer that unpacked a dictionary when its payload was no larger than the rows it fed. That guard never fired once, because the producer is `Chunk::select` and its payload is the whole chunk column, so the payload is never smaller than the selection. A change that measures at exactly zero on every query is usually a change that never ran.

## What follows

- An `INTEGER` key behind a filter is 3.33 times behind duckdb and a `BIGINT` key is 3.35. That is now the whole of the remaining gap for every fixed width key, and it is one thing rather than five. The profile of the date key query after this change is `Column::holds` at 21.96 percent, `table::hash` at 16.63, `Vector::signed_at` at 16.42 and `Table::probe_at` at 11.30, which is half the query in the row at a time probe.
- That probe is row at a time because the table is small. A group by on 2,526 distinct dates is under the eight thousand bucket threshold that sends a table down the one row path, so the batched compare this change added arms to is never reached on it. The threshold was chosen against cache misses, which a small table does not have, but the batched compare also hoists the type dispatch out of the row loop, which a small table does pay for. That is the next measurement.
- The cell rewrite of q01 stays refuted until a key column costs what an arithmetic column costs.

## What this note does not claim

The ladder is one key column at SF1 on one thread, and the two key row is the only one that says anything about what happens when a query groups on several. A query whose group table is large enough to miss cache is a different shape and note 14 is the note about that one.

The before and after columns were taken in one interleaved loop on a shared box. duckdb's own numbers move by ten percent between runs of the same loop, so a difference of a few percent in any row here is noise and only the rows that move by half are being claimed.
