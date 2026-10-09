# 169. Held distinct values sorted in one pass

## What was slow

A grouped `count(DISTINCT x)` whose groups hold few values each keeps them in a short run per set, and gives them over to the sets in a batch sorted by set. q16 gives 65,536 values at a time to 18,314 sets, about six values a set. The sort was a radix sort a byte at a time over the set number, which for that many sets is two passes over the batch, each a count and a scatter. Each set's run was also sixteen values and a length, 136 bytes, and the length was laid out after the values on a line of its own, so reading a set's length and its first values was two misses rather than one.

## The change

When there are no more sets than values and the batch fits a `u32`, the values are sorted by set in one pass of counting: a count a set, a prefix sum, and a stable scatter into a spare buffer that is then swapped in. The start of each set is kept between batches so its room is not allocated again. Anything else takes the radix sort as before. The run of a set now holds fifteen values, is aligned to a line, and puts its length first, so the length and the first seven values share the first line.

## Measured

At SF1 on server2, one thread, thousands of instructions a run, from a query run five times in one process less once, against the build of #2892. L1 data misses for ten runs of q16 went from about 30.7 million to 29.8 million. No other query moves by more than 0.1 percent.

| query | main | this change |
| --- | --- | --- |
| q16 | 109,373 | 107,988 |

The answers to all 22 queries are the same bytes as before at one thread and at six.
