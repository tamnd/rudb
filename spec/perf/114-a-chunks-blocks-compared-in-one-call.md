# A chunk's packed blocks compared with a range in one call

Notes written on 5 October 2026, while finding out where the instructions of TPC-H q06 go.

## The question

q06 reads four packed columns of `lineitem`, keeps the rows shipped in 1994 with a discount between 0.05 and 0.07 and a quantity under 24, and adds up price times discount over the 114,160 rows left. Counted in instructions over warm runs on one thread at SF1, the query was 89 million, and taking the filter apart one condition at a time gave 43 million for the range on `l_shipdate` alone, 17 million more for the discount, 15 million more for the quantity and 14 million more for the sum.

The range on `l_shipdate` is the first thing a filter of q06 does to every row, and it is already compared in AVX2 lanes eight codes at a time, see `108-codes-unpacked-in-lanes.md`. The compare of a block of 64 codes is about 95 instructions. What the profile showed was that a block cost about 245. Each block was its own call: the filter kernel called `Packed::within` once per block through a closure, which worked out the block's place in the words, checked the width and the room after the block, loaded the shuffle and shift tables, broadcast the mask and the range, and then compared. The setup around 64 rows was as much again as the compares.

The quantity was the other surprise. A filter of several columns builds a mask a word for every 64 rows a column at a time, and stops once fewer than one row in sixteen is left, handing the rest to the walk that reads only the rows left. After the date and the discount about one row in 25 is left, so the quantity went to the walk, which gathers each row's code, compares it and writes the row out. That was about 67 instructions a row, while a mask over the whole column costs under two a row.

## What changed

`lanes::within_words` compares every block of a run of words in one call. The tables, the mask and the range go into registers once, and each block is the eight groups of compares and nothing else. With `fresh` a word is set to its block's answer, and without it a word is narrowed and an empty word is skipped, which is what the second and later columns of a filter want. `Packed::within_words` works out how many blocks the bytes hold with the sixteen the loads read past them, gives those to the lanes, and does any blocks after them, and any cut that does not start on a word, a block at a time as before. The filter kernels call it for the whole blocks of a chunk and work out the rows past the last block a code at a time.

The mask of a filter now goes on down to one row in 64 rather than one in 16, since a mask is the cheaper of the two down to about one row in 40.

## Measured

Single thread at SF1 on server2, the warm instructions of a query as the difference between eleven runs and three in one process, against main at #2523. The answers to all 22 queries are the same bytes as before. The machine was loaded, so the cycles move a few percent either way from run to run.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q06 | 89 | 66 | 64 | 52 |
| q14 | 76 | 65 | 58 | 56 |
| q15 | 80 | 69 | 66 | 63 |
| q12 | 187 | 176 | 139 | 134 |
| q01 | 446 | 435 | 232 | 244 |

q03, q04, q05, q10 and q20 run two to four million fewer instructions, and the rest the same as before within one million, apart from q02, which moves between 30 and 37 million from one measurement to the next on either side.

## What this leaves

Of the 66 million instructions left in q06, the range on `l_shipdate` and the masks of the other two columns are about half, and the sum over the rows kept is about a fifth, most of it reading price and discount a row at a time from the packed words. The query reads every row of three columns, and the compares are now close to two instructions a row each, so going much further means reading fewer rows. That is a question of how `lineitem` is laid out and what its zone maps can rule out, rather than of the compares.
