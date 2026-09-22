# What a join filter costs a row

Measured on server3 on 22 September 2026, TPC-H SF1 in the native format, instructions retired, best of three with a fresh process per run and every binary interleaved. The before and after binaries were built out of one tree with the changed file overwritten by its base version and rebuilt, so the two differ in that file and in nothing else.

Note 16 closed the group key gap. The profile after it names a different symbol at the top of the two largest queries, and it is not an aggregate at all.

| symbol | q21 | q09 | q18 |
| --- | --- | --- | --- |
| `sieve::Blocked::holds_run` | 11.98 | 11.05 | 4.30 |
| `table::hash` | 10.37 | 9.41 | 9.11 |

That first one is the runtime join filter. A hash join builds a blocked Bloom filter over its build side's keys and hands it down to the scan under its other side, and the scan asks it about every row it produces. On q21 that is about eleven million rows, which is lineitem three times over, and a twelfth of the whole query is spent answering them.

## The row

The filter is 512 bits a block, which is one cache line, and a value sets four of them. The four bits of one hash are all inside one block, so a row is one trip to memory and the three loads after the first are already there. That part of the design is right and it is why the answer is asked for a whole chunk at a time rather than a row at a time, so the core can have several of those trips outstanding instead of waiting out each one.

What it was not is cheap in instructions. Counted out of the disassembly, the loop body was 62 instructions a row. Four of those are the multiply each lane makes, four are the immediate loads of the salts, four are the loads out of the block, and six are bounds checks that did not need to be there.

The words of a filter are a `Vec<u64>` and each lane indexed it separately, so each lane carried its own compare and branch against a length nothing in the loop knew. Reading the block out once as an eight word array puts the length in the type, and masking the lane's word index into that array is free because a lane already answers under 512. The four checks become one.

| | before | after |
| --- | --- | --- |
| instructions a row in the loop | 62 | 56 |

That is ten percent of the row, and it is arithmetic rather than a measurement, because the two loop bodies are both there to count.

## What it comes to

| | before | after | duckdb |
| --- | --- | --- | --- |
| TPC-H SF1, all 22 queries | 52.105 | 51.694 | 27.516 |
| q21 | 5.705 | 5.603 | 2.072 |
| q17 | 2.022 | 1.945 | 1.103 |
| q08 | 1.569 | 1.530 | 1.209 |
| q05 | 2.956 | 2.914 | 1.214 |
| q09 | 5.771 | 5.724 | 2.450 |

Eighteen of the twenty two queries came down and the suite came down by 0.79 percent. Most of the individual rows are inside the noise of a shared box taken one at a time, and the claim here is the direction and the total rather than any one of them. The five above are the ones that moved most and they are the five with a runtime filter under a large scan, which is the set the change can reach.

The share of q21 the filter takes, sampled three times a binary rather than once, went from 12.26 percent to 11.40. Against 5.705 G and 5.603 G that is 0.699 G of instructions before and 0.639 after, which is 8.6 percent off and agrees with the ten percent the disassembly counts.

## What is left in that loop

- Four of the 56 instructions are `movabs` of the salt constants, reloaded every row because the loop runs out of registers holding four 64 bit constants, four products and a block pointer at once. Two lanes at a time would fit and would cost a second pass.
- The four lanes are four dependent shifts and loads over one cache line, which is what a 512 bit compare does in one instruction on a machine with AVX-512. The obstacle is that a lane names any of the 512 bits, so two lanes can land in the same word and the check is not eight independent words. A filter whose lane `i` takes a bit of word `i` is the standard shape and is one AND of two 512 bit registers, and it is a format change because which bits a hash names is written into the file. There is a test pinning those bits now so that this cannot happen by accident.
- `Scan::sift` allocates two vectors per chunk, one for the hashes and one for the flags, and then reads the flags back through a bounds checked index to build the selection. That is small against 56 instructions a row but it is not nothing.
- Ten bits a value and four lanes is about one percent false positives. Whether that is the right point is a measurement nobody here has taken, and it trades directly against the bytes the filter costs and against how much of it stays in cache while a fact table goes past it.

## The other symbol

`table::hash` is the nine to ten percent under the filter on every query in the table above, and it is the largest single symbol across the whole suite. Two things were tried on it and the interesting result is which one paid.

**Taking the per row null question out.** A key column arrives with a validity that says either that a row is null or that nothing in the column is. The second is most columns, and the passes over a column were still asking a row at a time, which is a validity read, a branch, an `Option` and a bounds check per row to say what the column said once. Asking the column once and walking two runs side by side when the answer is no removes all four.

It is worth 1.4 percent on a group by on an unfiltered `BIGINT` key, 2.551 G against 2.515. It is worth nothing anywhere else, and the reason is the same shape note 16 ends on. A filter over a column hands on a dictionary whose codes are the rows that got through, so behind a filter a column is never flat, and the pass this speeds up is not the pass that runs. TPC-H filters almost everything it groups or joins on.

That is worth writing down on its own. Every fast path in the engine that begins by asking whether a column is a flat run is dead on a filtered column, and a filtered column is what most of a query works on. #1316 fixed one such path in the group table's direct map. This is the second one found and it will not be the last.

**Folding the spread into the last column's pass.** The hash mixes each key column into a running word and then walks the whole run again to spread the entropy from the high bits back into the low ones, because every table here buckets on the low bits. That second walk is a load, five operations and a store a row, and it only has to happen once at the end, so it belongs in the last column's pass rather than in a pass of its own.

| | before | after | duckdb |
| --- | --- | --- | --- |
| TPC-H SF1, all 22 queries | 53.793 | 53.415 | 26.377 |
| q17 | 1.959 | 1.883 | 1.020 |
| q02 | 0.567 | 0.552 | 0.404 |
| q08 | 1.509 | 1.473 | 1.213 |
| q18 | 8.515 | 8.347 | 2.141 |
| q05 | 2.912 | 2.866 | 1.225 |

Sixteen of the twenty two came down and the suite came down by 0.70 percent, both changes together. Measured against the same before binary in an earlier run, the first change on its own is 0.15 percent, so nearly all of this is the second one. The queries that move are the join heavy ones, which is where the hash is called most, and the group by ladder above does not move at all, because a group by on a narrow key takes the direct map and hashes nothing.

The suite total in this pair is 53.8 G against the 52.1 in the pair above it. Main moved underneath between the two runs and q18 is 2 G of the difference on its own. Only the before and after of one pair are comparable to each other, which is why every table in these notes is one pair.

## What this note does not claim

The suite moved by less than one percent on each of the two changes and no single query moved by more than four. Neither of these reaches the goal. What they do is make the two symbols at the top of the profile measurably cheaper and leave behind an exact account of what the rest of them is, which is the format change for the filter and, for the hash, the fact that its fast paths are looking for a shape that a filtered query does not produce.
