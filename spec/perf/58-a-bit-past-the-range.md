# 58. A bit past the range

When a join's build side holds integer keys that sit close together, it hands the scan under its driving side a bitmap over their range: the domain, in `sideways.rs`. The scan keeps a row when its key's bit is set. In q21 that bitmap is asked about some twenty million rows, three passes over `lineitem` in chunks of 8192. It was 28% of the query's instructions, about 23 per row.

## What it did

The test was `offset < range && bit(offset)`. The `&&` compiled to a branch, and the keys that fall outside the range are a large share of a filtered parent's (the `orders` keys of status `F` are half of them). So the branch mispredicted often, and the bitmap was read behind it.

## What it does now

The bitmap gets one word more than its range needs whenever the range ends on a word boundary, so the bit at `range` always exists and is never set. A key's offset is clamped to `range` with `min`, which compiles to a conditional move. A key outside the range lands on that zero bit and is dropped. The loop has no branch on the key, only the bounds check on the word, which is always taken the same way. Both places that build a domain get the word count from one function, `words_for`, so the extra bit cannot be forgotten by one of them.

## Numbers

TPC-H SF1 on server3, instructions best of three, against the same tree without the change. All 22 answers match.

| query | before | after | DuckDB |
|---|---|---|---|
| q21 | 1.623 G | 1.564 G | 1.884 G |
| q17 | 0.578 G | 0.544 G | 1.054 G |
| q09 | 2.150 G | 2.116 G | 2.257 G |
| q20 | 0.657 G | 0.636 G | 1.267 G |
| all 22 | 18.026 G | 17.699 G | 27.113 G |

18 of the 22 queries went down, and the rest moved within noise.
