# Adds that wait on each other

Notes written on 23 September 2026, after [note 21](21-codes-in-bulk.md) cut q01's instructions but left its cycles at about twice DuckDB's.

## The question

Note 21 ended by saying the cycles are the number to watch now. So this note started from cycles, not instructions, and asked where q01 spends them one piece at a time.

## Measuring the pieces

The same ladder as note 21, each step adding one thing to the query before it, run ten times on one thread on server3 against the native file. The numbers are how many G cycles each step adds.

| step added | rudb | DuckDB |
|---|---|---|
| filter, the two group keys and count(*) | 3.09 | 2.48 |
| sum(l_quantity) | 1.31 | 0.17 |
| sum(l_extendedprice) | 1.53 | 0.50 |
| sum(l_extendedprice * (1 - l_discount)) | 3.00 | 1.13 |
| the charge sum | 2.23 | 0.67 |
| the three means | 2.42 | 0.13 |

The filter and the keys are within a quarter of DuckDB. The aggregates are where q01 is lost: 10.5 G cycles in rudb against 2.6 G in DuckDB, four times as much.

## Why a sum over four groups was slow

A profile of the one step that adds sum(l_quantity) put most of its cycles on two instructions in the row loop of [note 20](20-a-total-per-chunk.md)'s local totals: the add with carry into a group's 128 bit total, and the increment of that group's count. Neither is expensive on its own. What makes them slow is that q01 has four groups and neighbouring lineitem rows are usually in the same one, so a row's add reads the total the row before it has only just stored. The processor can forward a store to a later load, but it takes several cycles, and every row waits for the one before it. The loop ran at the speed of that chain, not the speed of its instructions.

There was a second cost of the same kind. Every sum, mean and count over a few groups counted its own rows as it added them, which on q01 is eight counts a row, and when the argument has no nulls every one of them comes out the same.

## What changed

Each group now keeps four local totals instead of one, and a row adds into the one its position in the chunk picks. Rows next to each other add into different locals, so their adds overlap, and the four are summed when the chunk is folded into the accumulator. The sum is exact integer arithmetic, so the order it is added in does not change the answer.

The fold also counts once per chunk how many rows land in each group, in the same four lane form, and hands that count to every call. A call whose argument has no nulls takes its counts from there and its row loop is the add alone, and a count(*) has no row loop left at all. A call whose argument has nulls, or that has a FILTER, counts as it did before.

## Numbers

TPC-H SF1 from the native file on server3. Main is d8b223c7, built the same way.

| measure | main | this change | DuckDB |
|---|---|---|---|
| ten q01 on one thread, cycles | 15.01 G | 13.53 G | 7.1 G |
| ten q01 on one thread, instructions | 34.36 G | 35.41 G | 11.4 G |
| ten q01 on eight threads, user cycles | 15.43 G | 13.99 G | 7.72 G |
| ten q01 on one thread, wall time | 5.44 s | 4.90 s | 3.48 s |
| suite instructions, 22 queries | 43.250 G | 43.295 G | |

The cycles are the best of three fresh processes and the wall times the best of seven. Every step of the ladder that adds an aggregate costs fewer cycles than on main, and the step with only the keys and a count is the same. q01 runs about 3 percent more instructions, because the four locals are folded per chunk and the loop carries the lane, and 10 percent fewer cycles, which is the trade note 21 said to make. The other 21 queries move by less than the noise and all 22 answers are the same as main's. The wall time with all threads could not be measured, because server3 had a load average of 20 on its 8 cores while this ran and the other machines were busy as well, so the table gives the user cycles summed over every thread instead.

## What is left

A profile of q01 after this change has its cycles in the passes that each write a vector of 8192 rows. `packed_into` gathers the filtered rows' codes into a fresh vector before it adds them. The cast of a packed decimal and the decimal multiply each write a vector, and memset spends 7 percent of the query zeroing them. The next step is to evaluate the charge sums from the packed codes inside the sum. The range of each packed column in a chunk is known, so a chunk can prove its products fit in 64 bits before it starts and then need no overflow check and no vector in between.
