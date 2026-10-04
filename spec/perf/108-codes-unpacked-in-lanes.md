# 108. Codes unpacked in lanes

## The problem

An aggregate over a packed column reads every code of it. `Packed::unpack` and `Packed::unpack_mapped` turn each block of 64 codes into words with `unpack_block`, which is a scalar loop with the width made a constant, so each code is a load, a shift, an or where it straddles two words and a mask. On TPC-H q01 at SF1 that was 9% of the query on main, and after note 107 held `l_quantity` packed it was 12%, which made q01 8% slower than with the column flat.

Note 106 reads eight codes of a block with one shuffle, one variable shift and one mask in AVX2 lanes, to compare them. The same three instructions leave the eight codes in 32 bit lanes, and two widening moves make them words.

## The change

`lanes::unpack` does that for a block of 64 codes: for each group of eight, the two sixteen byte loads, the shuffle, the shift and the mask of `lanes::within`, then `vpmovzxdq` on each half and two stores of four words. `unpack_block_at` takes the lanes when the width is 25 or less and the sixteen bytes past the block are there, which is every block of a column but the last, and `unpack_block` otherwise. `Packed::unpack` and `Packed::unpack_mapped` both go through it, so every caller that unpacks a run of whole blocks gets it, which is the aggregates and the gathers of a dense selection.

## Results

Measured on server2 at SF1 on one thread, user cycles in millions, against main with #2436. All 22 queries give the same answers as before.
