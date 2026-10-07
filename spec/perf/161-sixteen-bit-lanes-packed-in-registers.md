# 161. Sixteen bit lanes packed in registers

## What was slow

Note 159 made `on_lanes` unpack 64 codes at once with the AVX2 unpack an aggregate uses, which writes each code out as a 64 bit word, and then fold every four of those words into one word of sixteen bit lanes with a shift and an or per code in scalar registers. The dates of lineitem and orders are packed pages of twelve bit codes, so every such page goes through that fold as it is decoded. In a profile of q06 run once in a fresh process with notes 159 and 160 in, `on_lanes` was still about a sixth of the instructions, nearly all of it the fold.

## The change

`lanes::widen_16` takes the codes through the same shuffle, shift and mask, eight 32 bit lanes to a register, and packs two such registers down to sixteen 16 bit lanes with one unsigned pack and one permute, which is then stored as four words. A code is at most fifteen bits, so the pack never saturates. `on_lanes` calls it for every block of 64 that has the sixteen bytes past it the loads read, and folds in scalar registers otherwise, which is the last block of a page and every block of a page widened to bytes. The widened codes test cuts at the first row and at row 128, which go through the new path.

## Measured

At SF1 on server2, one thread, each query run once in a fresh process, which is when pages are decoded. The figures are the instructions of the whole process, against the build of #2775.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q21 | 569 | 421 |
| q12 | 496 | 347 |
| q03 | 295 | 184 |
| q14 | 133 | 83 |
| q06 | 342 | 293 |
| q01 | 641 | 592 |

Each of these reads one or more twelve bit date columns. Run again in the same process nothing changes, since the pages are already decoded. The answers to all 22 queries are the same bytes as before at one thread and at six.
