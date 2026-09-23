# A total per chunk

Notes written on 23 September 2026, after the fixes for #1410 and #1414 brought the TPC-H suite back to 46.7 G instructions.

## The question

q01 was 4.29 G instructions against DuckDB's 1.47 G, the widest gap left on the suite after q18. Its wall time was already ahead of DuckDB's, 0.12 s against 0.15 s on server2, so this is about the work a row costs rather than about the time, and the work a row costs is what decides how far ahead the time can go.

[Note 15](15-what-a-row-costs.md) asked the same question a week ago and answered it with a disassembly: an added grouped call cost 60 to 75 instructions a row, and 38 of the 45 in the loop were the engine finding out where to put the answer. That note's work list went after the pieces one at a time. This note asks why the loop needs to find out at all.

## The ladder again

q01's shape with calls added one at a time, on the same four groups and the same filter, best of three on server3.

| shape | rudb | DuckDB |
|---|---|---|
| count(*) of the filter, no groups | 0.308 | 0.316 |
| the two keys and count(*) | 1.105 | 0.879 |
| plus sum(l_quantity) | 1.502 | 0.906 |
| plus sum(l_extendedprice) | 1.881 | 1.025 |
| plus sum(l_extendedprice * (1 - l_discount)) | 2.710 | 1.192 |
| plus the charge | 3.342 | 1.336 |
| plus the three averages | 4.293 | 1.460 |

The filter costs the same in both. Every call after that costs rudb between 0.38 G and 0.95 G and DuckDB between 0.03 G and 0.17 G. The three averages are over columns the sums already read, and they still cost 0.95 G.

## What the loop is for

A grouped call with four groups over 2,048 rows does 2,048 additions into four places. The loop in `update_scattered` found each row's place the long way: the row's slot times the number of calls per group plus this call's position, a bounds check into the accumulators, a read of the accumulator's tag to find out it is a sum, an add at 128 bits with an overflow branch, and a store of the flag that says the group has seen a row. Four places, and every row paid to find one of them again.

None of that is needed until the chunk is done. A chunk of 2,048 rows is small, so a total of the chunk can be kept in a local per group and handed to the accumulator once at the end. The row then costs its slot, one add into a local and one count, and the tag, the address, the overflow branch and the seen flag are paid four times a chunk rather than 2,048.

## Why the add needs no check

Every value that takes this path fits in 65 bits. A flat column is taken at 64 bits or less, and a packed one only when its base fits in 64 bits, so its values are a 64 bit base plus a code of at most 64 bits. A local total of values that size cannot overflow a 128 bit integer before a call holds 2^62 rows, and a call holds one chunk. So the local add is a plain add.

The accumulator's own add is still checked, once per group per chunk, and a sum is range checked again against its declared type when it finishes. A total that does not fit raises the same error it raised before. A mean that runs past 128 bits still widens the way it did.

A float total does not take this path. Adding in a different order rounds differently, and the row at a time path adds in row order, so a local per group would change the last digits of an answer. `min` and `max` are not totals and do not take it either.

## When it pays

The locals are 256 totals and 256 counts on the stack, cleared on every call. That clearing is a fixed cost, so the path is taken only when there are at most 256 groups and at least four rows a group in the call.

The second condition came from the suite. Without it q15 went from 1.02 G to 1.11 G, because a partitioned aggregate hands each partition a few dozen rows at a time over a hundred or so groups, and clearing six kilobytes of locals for thirty rows costs more than it saves. With it q15 is back where it was.

## Numbers

TPC-H SF1 from the native file with summaries. Instructions are the best of three fresh processes on server3, main at ffdd5c11 against the same tree with this change. Time is the best of seven on gamingpc, for the reason given under the table.

| measure | main | this change | DuckDB |
|---|---|---|---|
| q01 instructions | 4.271 G | 3.762 G | 1.47 G |
| q01 ladder, the first sum added | 0.395 G | 0.313 G | 0.027 G |
| q01 ten times in one process, one thread, gamingpc | 1.73 s | 1.57 s | 0.72 s |
| suite instructions, 22 queries | 45.133 G | 44.606 G | |

All 22 answers are the same with and without the change. q01 is the only query that moves by more than one percent, because it is the only query on the suite whose aggregate has a handful of groups and millions of rows. q15 is 0.981 G against 0.989 G, which is inside its noise on server3.

The time is on gamingpc, an i9-13900K, because server1, server2 and server3 were all running at a load of 35 to 40 when this was measured. At the default thread count every build finishes q01 in 0.02 to 0.03 s there, which is too short to compare, so the row in the table runs q01 ten times on one thread in one process. That is also the honest number to compare against DuckDB, and on one core rudb is still twice as slow on q01. On the whole machine rudb's wall time is ahead of DuckDB's, 0.03 s against 0.07 s, with 71 MB of peak memory against 111 MB.

One more thing turned up while checking the answers against DuckDB. rudb's `avg(l_discount)` for the `N`, `O` group is 0.04999658605370408 and DuckDB's is 0.049996586053704085. They are different doubles. The exact answer is 146008.73 over 2,920,374 rows, and the double nearest to it is rudb's, so DuckDB is one unit in the last place off here and rudb is not.

## What is left on q01

The scatter was 38 percent of q01 and most of it is gone. What is left is the arithmetic in front of it. `sum(l_extendedprice * (1 - l_discount) * (1 + l_tax))` builds a flat vector of every row out of the packed columns, then a second one for each multiply, and then sums the last one. That is three passes and three allocations per chunk to feed one add per row. Folding the arithmetic into the sum, so that a row's product is formed and added in the same loop, is the next step on this query.
