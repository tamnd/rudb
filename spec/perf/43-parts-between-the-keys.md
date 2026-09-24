# Parts between the keys

Notes written on 24 September 2026, on q18, after the grouping it spends most of its time in got answered from its runs (note 35).

## The question

q18 reads lineitem twice. The first read groups every line by order key to find the 57 orders with more than 300 units. The second read joins those 57 orders back to their lines, and the orders scan does the same for the orders themselves. With the grouping made cheap, the second read and the orders scan were most of what was left. On one thread the second lineitem scan took 105 ms to hand up 399 rows, and it skipped 2 of lineitem's 733 parts.

The join does tell the scan under it what its build side holds, once that side is in. It hands over the smallest and largest key, which rules out a part outside that range, and then a bitmap or a Bloom filter over the keys, which drops rows. The 57 order keys run from near the start of the table to near the end, so the range covers every part. The bitmap drops all but 399 rows, but only after every part has been read and decoded.

lineitem and orders are both stored in order key order, so each part of lineitem covers about two thousand order keys and each part of orders about eight thousand. With 57 keys spread over six million, almost no part holds any of them. The stored range of a part already says which keys it could hold, so the scan only has to ask whether any of the 57 falls inside it.

## What changed

When the build side has at most 4096 integer keys and no exact row set was handed down, it now also keeps its keys sorted, each once (`Keys` in `crates/rudb-exec/src/sideways.rs`). Before it reads a part, the scan looks up the first key at or past the part's smallest value and rules the part out if that key is past the part's largest value, or if there is none. It asks about a stripe first, from the bounds already in memory, and then about each part from the per part range page that the plain range test already reads. A part ruled out this way counts as skipped, so the morsels are cut from the parts that are left and `EXPLAIN ANALYZE` shows how many went.

The stores answer through a new `ruled_by` on a table's rows, which hands the stored range of one column of one part to a rule the caller passes in. A probe is one comparison against one constant, and a set of keys is not something a probe can say, so the rule stays with the join that knows what the keys are.

The limit is there because a set of keys only rules out a part when the keys are sparse next to what one part covers. The first version kept up to 65536 keys. On q4 that meant sorting the 57 thousand orders of one quarter, which land in every part of lineitem, and q4 got 16 M instructions slower for nothing. At 4096, q4 is back where it was.

## Numbers

On server3 against the native file at SF1, instructions over all threads, best of three:

| | main | this change | DuckDB |
|---|---|---|---|
| q18 | 1.477 G | 1.068 G | 1.91 G |
| the 22 queries | 23.409 G | 22.992 G | |

q18's plan on one thread: the orders scan now skips 134 of 184 parts and hands up 57 rows in 13 ms, and the second lineitem scan skips 678 of 733 parts and hands up 399 rows in 8.5 ms. The other 21 queries moved by less than one percent either way, which is the noise of counting over all threads. Every one of the 22 answers is the same as main's.

q18's CPU time over three runs, with server3 under a load of about 12, was 662 ms to 828 ms on main, 402 ms to 538 ms with this change, and 1163 ms to 1570 ms for DuckDB. The best of five wall times at the same load were 413 ms, 325 ms and 591 ms.

## What this leaves

The first lineitem read is now almost all of q18, and it reads every line to add up totals the outer grouping then adds up again for the 57 orders. The next step is still handing the subquery's totals to the outer grouping instead of reading those lines a second time.
