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

## What this note does not claim

The suite moved by less than one percent and no single query moved by more than four. This is not a change that reaches the goal, it is a change that makes the top symbol of the two largest queries ten percent cheaper and writes down what the rest of that symbol is made of. The next thing on this list is the format change, and that one has a number worth having.

`table::hash` is the other nine to ten percent on every query in the table above and it is untouched here. It is the largest single symbol across the whole suite and it is the next thing to take apart.
