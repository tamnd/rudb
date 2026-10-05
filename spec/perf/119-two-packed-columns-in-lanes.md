# 119. Two packed columns compared in lanes

## What the comparison cost

q4, q12 and q21 compare two date columns of the same row, `l_commitdate < l_receiptdate` and `l_shipdate < l_commitdate`. A stored lineitem hands both up bit packed, and `packed_kept` answered the comparison in one of two ways. When the conjuncts in front had left an eighth of the chunk or more, it unpacked both columns whole into two vectors of `i32` and compared those. Otherwise it read the two codes of each kept row one at a time. In q12 the range on `l_receiptdate` runs first and keeps a seventh of the chunk, which is just over the line, so the first date comparison unpacked both columns of every chunk, and the second one read about 7 percent of the rows a code at a time. Between them that was 33 percent of q12's instructions: 20 percent unpacking into the two vectors, 5 percent comparing them and 9 percent reading codes one at a time at about 37 instructions a row.

## The change

`Packed::against_words` compares two packed columns a block of 64 rows at a time in the AVX2 lanes note 108 built for the range test. Eight codes of each side go into 32 bit lanes with one shuffle, one shift and one mask each, the right side has the difference between the two bases added, and one signed compare and a `movemask` give eight bits of the answer. Nothing is stored but a word of the answer per block. The rows the conjuncts in front kept come in as a mask, and a block they emptied is not read at all. A width over 25 bits, a vector that does not start on a block, or the last block of a column without the slack the loads read past it goes through a scalar loop that unpacks each block onto the stack.

`packed_kept` still reads codes one at a time when the rows coming in are fewer than one in 32, since a block in lanes costs about what reading two rows does.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process, against main at #2589. The answers to all 22 queries are the same bytes as before at one thread, and q4, q12 and q21 also at six.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q04 | 147 | 140 | 109 | 101 |
| q12 | 177 | 148 | 141 | 132 |
| q21 | 395 | 387 | 257 | 274 |

The machine was under a load of about 30, so the cycles move by several percent from run to run and the q21 cycles are within that.

## What this leaves

The compare in lanes is now 12 percent of q12 at about 1.5 instructions a row. Each conjunct of the filter still hands the next one a selection, so the range writes a mask and turns it into rows, the first date comparison turns those rows back into a mask and its answer into rows again, and so on. Turning masks into rows is now 9 percent of q12 and turning rows into masks another 5. A conjunction over packed and coded columns could pass its mask from one conjunct to the next and turn it into rows once at the end.
