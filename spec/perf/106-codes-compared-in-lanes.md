# 106. Codes compared in lanes

## The problem

TPC-H q06 cost 104 M user cycles a run at SF1 on one thread, about 17 cycles for each of the 6,001,215 `lineitem` rows. DuckDB took 175 M. Ten times DuckDB is 17 M, which is about 3 cycles a row for the whole query, so the scan, the filter and the sum have to get there together.

The steady profile put 35% of the cycles in the mask filter of note 102: `mask_within` at 21% and the `unpack_block` it calls at 15%. For every block of 64 rows it unpacked 64 codes into words on the stack, compared each word into a flag byte, and folded the flags into a mask word. That is three passes over the block and two stores a row, for an answer that is one bit a row.

A single comparison over a packed column was worse. `count(*) WHERE l_quantity < 24` with the column packed went through the general path, which wrote a `bool` a row and then read the flags back into rows, and cost 43 M cycles, about 7 a row, for one compare on six bit codes.

## The change

Every eight codes of a packed block are exactly `width` bytes, so the eight codes of a group start on a byte, and where each code starts inside the group depends only on the width. For a width of 25 or less a code and its shift fit in four bytes. `Packed::within` reads a block of 64 codes eight at a time in AVX2 lanes: one shuffle puts each code's four bytes in a 32 bit lane, one variable shift and one mask leave the code, and a subtract, an unsigned minimum and a compare say whether it is in the range. A `movemask` gives eight bits of the answer. Nothing is unpacked and nothing is stored but the word.

The shuffle and the shifts for each width are a table built at compile time. A block that does not start on a word, a width over 25 and the last block of the words, where the sixteen byte loads could read past the end, go the old way.

`mask_within` takes a word a block from `within` rather than flags from an unpack. A single comparison of a packed column with a literal now goes through `mask_within` and `mask_selection` too, so it is a word a block and one pass to make the rows, rather than a flag a row read back.

## What was tried and left out

The other half of q06's filter was `l_quantity`, which the cascade stores as a stride of 100 over six bit codes. A cascade page is decoded once and held flat after that, eight bytes a row, 48 MB at SF1, and q06 read all of it from memory on every run. 93% of the samples in the loop that filtered it were on the load. Holding every cascade page packed over the range of its values made q06 a little faster but made q18 four times slower, because `l_orderkey` is a cascade page too and the grouping by runs over it is much slower packed than flat. A page should be held in the form its readers are fastest on, and for now that is a decision per reader rather than one rule at the decoder.

## Results

Pending the release build, posted on the pull request.
