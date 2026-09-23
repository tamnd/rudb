# Codes in bulk

Notes written on 23 September 2026, after [note 20](20-a-total-per-chunk.md) took q01 from 4.27 G to 3.76 G instructions.

## The question

Note 20 ended with q01 at 3.76 G instructions against DuckDB's 1.47 G, and with a list of what was left: the key mapping in `Aggregate::fold`, the gather of the two flag columns after the filter, the page decode, and the arithmetic in front of the sums. This note measured each of those on its own before touching any of them, and found that most of what was left came down to one line of code.

## Measuring the pieces

The same filter as q01 on one thread, with one piece added at a time, run ten times in one process on server3. Per query, in G instructions.

| piece | rudb | DuckDB |
|---|---|---|
| filter and count(*) | 0.286 | 0.124 |
| an ungrouped sum(l_extendedprice) added | 0.223 | 0.076 |
| the two group keys added | 0.670 | 0.513 |
| a grouped sum(l_extendedprice) added | 0.300 | 0.021 |

The grouped sum is the outlier. DuckDB adds a column into four groups for 3.5 instructions a row, and rudb spent 50. A profile of the difference put 0.239 G of the 0.300 G in `packed_into`, and the source lines under it were not the add. They were `code_at`, the function that reads one code out of a packed column.

The full q01 then showed the same function under three different callers.

| function | G per q01 | what it does |
|---|---|---|
| `packed_into` | 0.64 | the sums and means over a packed column |
| `cast::from_packed` | 0.41 | widening a packed column into the arithmetic of the charge sums |
| `packed_against` | 0.17 | the filter on `l_shipdate` |

That is 1.22 G of 3.76 G reading codes one at a time.

## Why a code at a time is slow

A packed column stores each value as `width` bits laid end to end, so row `r` starts at bit `r * width`. Reading one code works out the word, reads it through a bounds check, and asks whether the code runs over into the next word, which for a width of 20 it does about a third of the time and not in a pattern a branch predictor settles on. That is twelve to fifteen instructions and a branch per code, paid by every row of every packed column every time it is read.

Sixty four codes of one width fill exactly `width` words, and the places they straddle are the same in every block. So a block of sixty four can be unpacked by code in which the width is a constant, and every shift, every word index and every straddle is settled before the program runs. That is the standard answer to bit unpacking, and it is what DuckDB's bitpacking does too.

## What changed

`Packed::unpack` reads a range of rows. The rows before the first whole block and after the last one go a code at a time, and everything between goes sixty four at a time through a function the width is a const generic in. `Packed::codes_at` does the same for a list of rows, which is what a filter leaves: it finds the span the rows cover, unpacks that span, and reads each row out of it. Rows spread over more than four times their count go a code at a time, because unpacking a span mostly skipped costs more than it saves.

The three callers now call one of those once per chunk and then loop over plain numbers.

The first version of the block loop was an ordinary `for` over sixty four codes, and it made the suite slower, 44.6 G to 45.4 G. The compiler kept the loop, so the width was a constant but the row was not, and every code still paid a shift by a variable and a branch on the straddle. The whole of `unpack` came out at 9.6 kilobytes for 63 widths. Written out as sixty four steps, each with its row as a const generic, every step folds to a shift, an or where it straddles and a mask. The suite went to 43.9 G.

A third version unpacked one block at a time into a buffer on the stack and read each row out of the block it fell in, to keep less in the cache. Asking which block a row is in for every row cost more than the misses it saved, 40.2 G instructions for ten runs of q1 against 34.1 G, and more cycles too. The span version stays.

## The two smaller pieces

The fold's key mapping for a key of one or two dictionary columns with no nulls is now one pass. Before, it was a pass to write each row's place into a vector, a pass to read each place back and look it up, and a check of every code against its dictionary. The check was redundant, because a dictionary vector is range checked when it is built and nothing changes its codes after. Now a row's place is worked out in a register and used at once, and the vector of places is only built when a row finds nothing, which is the first chunk of a row group.

Filtering a stable dictionary column used to check every row index, gather the codes, and then build the result through the constructor that checks every code again. A running maximum over the indices now answers the first check in a loop that vectorizes, and the gathered codes are taken as they are, since each one is a code the source vector already held.

The global dictionary codes of the two flag columns are decoded from the cascade as 64 bit integers and narrowed to 32. That loop pushed one code at a time, and at sixteen instructions a row it was most of `native::decode`. It is now an or over the page and a narrowing map, and each is a vector loop.

## Numbers

TPC-H SF1 from the native file. Instructions are the best of three fresh processes on server3. Main is 7da82535, whose code is the same as the build note 20 measured.

| measure | main | fused lookup | this change | DuckDB |
|---|---|---|---|---|
| q01 instructions | 3.762 G | 3.663 G | 3.468 G | 1.47 G |
| suite instructions, 22 queries | 44.589 G | 44.533 G | 43.926 G | |
| ten q01 on one thread, instructions | 36.92 G | | 34.09 G | 11.39 G |
| ten q01 on one thread, cycles | 14.9 G | | 14.3 G | 7.1 G |

19 of the 22 queries go down and none goes up by more than the noise on server3. The filters gain as well as the sums: q19 goes from 1.773 G to 1.698 G, q03 from 1.557 G to 1.507 G and q06 from 0.713 G to 0.682 G. All 22 answers are the same as main's.

On gamingpc the ten q01 runs took 1.45 s on main and 1.48 s with this change, best of nine each. The spread between runs is larger than that difference. The cycles on server3 fell by 4 percent, about half as much as the instructions, and the L1 misses rose from 0.69 G to 0.96 G because the unpacked codes are a vector the old loop never built. So this change is a real cut in the work a row costs, and not yet a cut in the time q01 takes on one core.

## What this says about the goal

The cycles are the number to watch now. DuckDB runs q01 on one core at 1.6 instructions a cycle and rudb at 2.4, so rudb is doing three times the work and getting twice the time out of it. Taking out instructions that were cheap to begin with, like the ones here, moves the time less than the count suggests. What is left on q01 is mostly passes over whole chunks that each write a vector, the widened columns and the products of the charge sums among them, and every one of those vectors is another round trip through the cache. Folding the arithmetic into the sum, so that the product for a row is formed and added without a vector in between, removes both the instructions and the traffic, and it is the next step on this query.
