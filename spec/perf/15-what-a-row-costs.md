# What a row costs, and why a type can be worth six times

Measured on server3 on 22 September 2026, TPC-H SF1 in the native format, instructions retired, best of three with a fresh process per run and rudb and duckdb interleaved. Every pair of numbers called before and after in this note comes from two binaries built out of one tree, the second one with the changed files overwritten by their `origin/main` versions and rebuilt, so the two differ only in those files and in nothing else.

[Note 14](14-what-a-group-costs.md) measured what one more aggregate call costs per group. This note measures what it costs per row, which turned out to be the larger half on TPC-H, and it is the first note here written from a disassembly rather than from a profile.

The question is the same narrow one: **when a grouped aggregate gets one more aggregate call, what does rudb spend on each row?**

## The whole gap is the added call

q01's shape with calls added one at a time. The filter is q01's filter, so the answer cannot come out of the frequency synopsis. Four groups.

| shape | rudb | duckdb | cost |
| --- | --- | --- | --- |
| keys only, count star | 1.163 | 0.869 | 1.33x |
| plus sum(l_quantity) | 1.586 | 0.973 | 1.62x |
| plus sum(l_discount) | 1.947 | 0.992 | 1.96x |
| plus sum(l_extendedprice) | 2.385 | 1.200 | 1.98x |
| plus avg(l_quantity) | 2.647 | 1.103 | 2.39x |

The first row is the interesting one. Scan, filter, hash, probe and a four group table, over 5,916,591 rows, and rudb is 1.33 times behind. That is most of an engine and it is nearly at parity. Everything after it is one more call over the same rows.

| added call | rudb | duckdb | per row |
| --- | --- | --- | --- |
| first sum | 0.423 | 0.104 | 71 against 17 |
| second sum | 0.361 | 0.019 | 61 against 3 |
| third sum | 0.438 | 0.208 | 74 against 35 |
| the avg | 0.262 | -0.097 | 44 against nothing |

Between 60 and 75 instructions a row per call, against duckdb's 3 to 35. On q01, which has seven calls over six groups, that is the entire 3.04x. Note 14's per group cost is real and on this shape it has six groups to spread itself over, so it is not what TPC-H is paying for. TPC-H groups on low cardinality keys nearly everywhere. What TPC-H pays for is the per row half.

## The column's form is not what it costs

Before taking the loop apart it is worth ruling out the obvious suspect, which is that the added cost is the decode of a bit packed column.

An added grouped `sum` over `lineitem` columns of very different shape, same rows, same groups:

| column | distinct values | added |
| --- | --- | --- |
| `l_linenumber` | 7 | 0.355 |
| `l_quantity` | 50 | 0.423 |
| `l_discount` | 11 | 0.361 |
| `l_extendedprice` | 933,900 | 0.438 |
| `l_tax` | 9 | 0.443 |

A column with nine distinct values and a column with nine hundred thousand cost the same added amount. The same query against the parquet file rather than the native one costs 0.434 against the native 0.427, and parquet hands the column over flat where the native file hands it over bit packed. Neither the cardinality nor the packing is what an added call costs. What it costs is the loop that walks the rows.

## What the loop spends, instruction by instruction

`perf annotate --stdio` of `aggregate::scatter<spread::{closure#1}>` on a profiling build, which is the scatter's total loop over a bit packed column, counting the instructions between the loop head and the backward jump. Forty five instructions per row.

| what the loop spends | instructions |
| --- | --- |
| the loop itself | 4 |
| asking whether the row is null | 12 |
| reading the group's slot | 5 |
| turning the slot into the accumulator's address | 5 |
| reading the accumulator's tag | 3 |
| reading the value out of the dictionary | 8 |
| the addition, in `i128` with an overflow branch | 7 |
| recording that the group has seen a row | 1 |

Seven of the forty five are the query. The other thirty eight are the engine finding out where to put the answer, and every one of them is paid again by the next call over the same row.

Read that table against note 14's four consequences and three of them are in it. The slot read and the address arithmetic are "a pass per call rather than a pass per row". The tag read is "a tag per state". Those are [#1173](https://github.com/tamnd/rudb/issues/1173) and this is the per row number behind it, which note 14 did not have.

The twelve for the null question are not on that list and are nobody's design. They are `Live::at`, which is three lines and `#[inline]` and reads like nothing: a discriminant read twice, a load of the bitmap's pointer out of the `Vec` behind it, and a bounds check of the word, all per row and none of them changing from one row to the next.

## The hole a type left

Adding one more call to the ladder above, this time a `min` rather than a `sum`, and the shape changes completely.

| shape | rudb | duckdb | cost |
| --- | --- | --- | --- |
| base, count star | 1.166 | 0.883 | 1.32x |
| min over an integer | 1.697 | 0.947 | 1.79x |
| min over a decimal | 1.709 | 0.943 | 1.81x |
| min over a varchar | 1.894 | 1.303 | 1.45x |
| min over a date | 5.758 | 0.855 | 6.72x |
| max over a date | 5.758 | 0.838 | 6.86x |

A grouped `min` over an `INTEGER` costs 0.531 G added, which is 90 instructions a row, in line with everything above. The same `min` over a `DATE` costs 4.592 G added, which is 776 instructions a row, and duckdb's is free. A `min` over a string is cheaper than a `min` over a date.

The cause is one line. `feed_of` decided which typed loop a scatter takes, and its extreme arm took an integer or a decimal and nothing else, so a date fell through to the row at a time path that builds a `Value` per row and compares two of them. A date, a time and a timestamp are each one signed integer of one unit and each one orders the way that integer does, so the loop that compares a run of `i128` answers them exactly as it answers an `INTEGER`. `Vector::signed_at` already groups them that way for exactly this reason.

This is the part of the note worth generalising. Nothing was slow. A type was missing from a list, the fallback was correct, and the only thing that said so was a counter nobody was reading on this query. The scatter does count its fallbacks, which is how the test that now covers this is written, but no test asserted anything about a date because no test used one.

Putting a date into the two differential tests then turned up something the perf work was not looking for. A `sum` over a `DATE` column answered on the vector path and raised `summing a DATE` on the row at a time path, and an `avg` over one did the same. Both dispatches matched the whole total state against any type at all, because the loops underneath dispatch on how the values are stored and a date is stored as an integer. So the fast path was adding the days up and the slow path was refusing to. Neither reaches SQL today because the planner rejects it first, but the kernel is the thing that is meant to be right about it. Both dispatches now ask whether the column is a number and hand anything else to the loop that refuses it. The lesson is the same one as the paragraph above: the list of types a loop takes was written from the queries that existed, and the test that would have caught either of these was a test over types nobody had put in the array.

## What the two changes are worth

Both on the same exact pair, with duckdb run alongside in the same loop.

| shape | before | after | duckdb | was | now |
| --- | --- | --- | --- | --- | --- |
| base, count star | 1.170 | 1.168 | 1.006 | 1.16x | 1.16x |
| min over an integer | 1.719 | 1.702 | 0.951 | 1.80x | 1.78x |
| min over a decimal | 1.719 | 1.764 | 0.930 | 1.84x | 1.89x |
| min over a varchar | 1.891 | 1.892 | 1.296 | 1.45x | 1.46x |
| min over a date | 5.756 | 1.675 | 0.849 | 6.77x | 1.97x |
| max over a date | 5.758 | 1.675 | 0.869 | 6.62x | 1.92x |
| q01 | 4.519 | 4.387 | 1.690 | 2.67x | 2.59x |
| 1.5M groups, three sums | 6.394 | 6.502 | 2.226 | 2.87x | 2.92x |

A grouped `min` over a date went from 4.586 G added to 0.507 G, which is 775 instructions a row to 86, and the query went from 6.77 times behind duckdb to 1.97. The 86 is the same 90 the same `min` over an `INTEGER` costs, which is what it should be, because it is now the same loop. A date column is no longer a special case and it is no longer the worst thing in a grouped query by a factor of six.

The null hoist is the `live_rows!` macro, which settles the null question once before the loop starts and runs one of three loops. That is loop unswitching written out because the compiler did not do it. The disassembly said twelve of forty five and it delivered about three, which is real and free but is a quarter of what the count predicted, most likely because the annotated loop is one of several the layout macro generates and the compiler had already unswitched some of the others. It is the whole of q01's 2.9 percent here, since q01 has no extreme in it.

The last row moved the wrong way and it is noise rather than a regression. Run five more times each, the best of five is 6.558 before against 6.523 after and the median is 6.739 against 6.616. That shape's noise floor on this box is wider than anything either change does to it, so the honest reading is no measurable difference. duckdb's own base moved from 0.883 in the run above to 1.006 in this one for the same reason, which is worth knowing when reading any single small number here.

The rest of the table is flat, which is the point of running it. An integer, a decimal and a string all took the right loop before and they take the same loop now.

## Where q01 goes now

The instruction profile of q01 before these changes, `perf record -e instructions -c 2000000 --sort symbol`, total 4.519 G.

| symbol | share |
| --- | --- |
| `packed_into<spread::closure#0>` | 21.77 |
| `scatter<spread::closure#1>` | 12.42 |
| `cast::from_packed<swept>` | 10.33 |
| `scatter<identity>` | 9.10 |
| `scalar::sweep`, the three decimal arms | 13.03 |
| `Aggregate::fold` | 6.03 |
| `Vector::copied` | 4.18 |
| `native::decode` | 2.71 |
| `compare::packed_against` | 2.58 |
| `integer::decode_chunk` | 1.97 |
| `update_scattered` | 1.97 |

The top two and the fourth are the scatter, which is 43 percent of q01 on their own. `cast::from_packed` is a third thing and it is not small: it builds a `Vec<i128>` of every row so that an expression over a packed column can be evaluated flat, which is one allocation and one pass over the rows per column per chunk before any arithmetic happens. That is [#1088](https://github.com/tamnd/rudb/issues/1088)'s neighbour and it is the next thing on this query after the scatter itself.

## What follows

The forty five instruction table is the work list and it is ordered by what is in it.

- The slot read and the address arithmetic, 10 of 45, are one pointer per row per chunk shared across calls. That is stage B of [#1173](https://github.com/tamnd/rudb/issues/1173) and it is the largest item that is not already claimed.
- The tag read, 3 of 45, goes when the layout is decided at plan time. That is stage C of the same issue and it is blocked on [#1114](https://github.com/tamnd/rudb/issues/1114).
- The `i128` add with its overflow branch, 7 of 45, is the query, but it is the query done at four times the width the column needs. A sum of a `DECIMAL(15, 2)` column does not need 128 bits until it does, and finding out which is a per column question rather than a per row one.
- The value read out of the dictionary, 8 of 45, is `Packed::code` recomputing its mask per call and branching on whether the code straddles two words. That is the same work [#1088](https://github.com/tamnd/rudb/issues/1088) is about from the other side.

## What this note does not claim

The forty five is one loop of several. The layout macro generates a loop per physical form and the annotated one is the bit packed total, which is the one q01 spends most of its scatter in but is not the only one. The hoist's shortfall against its own prediction is the evidence for that.

The per row numbers are divisions of a marginal instruction count by 5,916,591 rows. They are not a profile line and they include whatever else an added call does outside the loop.

The ladder is four groups at SF1 on one thread. Note 14's ladder is a million and a half groups and it reaches a different answer about the same engine. Both are right about their own shape, and a change sized from either one has to be measured on the suite before it is claimed there.
