# 157. Codes compared where they lie

## What was slow

A column page whose values sit in a narrow range is held bit packed, each value a code of just enough bits above a base, and a range filter compares those codes in vector registers without unpacking them. For most widths a code does not start on a byte, so before each compare the kernel shuffles, shifts and masks every code into a lane of its own, and that work was most of what the filter cost. The dates of TPC-H need twelve bits and `l_quantity` needs thirteen, so every filter on a date or a quantity paid for it. TPC-H q06 is three such ranges over the six million rows of lineitem, and `within_words` was 22 million of its 53 million instructions at SF1.

## The change

A code of eight or sixteen bits already starts on a byte, so `within_aligned` compares it where it lies: a block of 64 codes is two loads of 32 bytes or four loads of 16 words, each followed by a subtract, a min and an equal, and the answer comes out with one `movemask`. `Vector::on_lanes` widens a packed vector whose codes are a few bits short of either width, six and seven bits to a byte and twelve to fifteen to two, which grows the codes by a third at most. The native reader calls it on the bit packed pages it decodes, so the dates and quantities of TPC-H arrive as sixteen bit codes. A vector whose type cannot hold the wider top is left as it was. A test checks that widened codes read back the same values, including a cut that starts partway into a word, and the existing test of the packed range filter now covers widths 8 and 16.

## Measured

At SF1 on server2, one thread, against main at #2749.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q06 | 52 | 43 |
| q14 | 58 | 55 |
| q12 | 105 | 102 |
| q15 | 56 | 54 |
| q01 | 185 | 183 |
| q03 | 75 | 75 |
| q04 | 68 | 68 |
| q05 | 99 | 99 |
| q07 | 100 | 100 |
| q10 | 149 | 149 |
| q19 | 88 | 88 |
| q20 | 105 | 106 |

The queries that move are the ones that filter lineitem on a date range or on a quantity. The answers to all 22 queries are the same bytes as before at one thread and at six.
