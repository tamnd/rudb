# 120. One mask for the whole filter

## What was left after note 119

Note 119 compares two packed columns in lanes into a mask, but the filter still handed rows from one conjunct to the next as a selection. In q12 the range on `l_receiptdate` wrote a mask and turned it into a list of rows, the first date comparison turned that list back into a mask, compared, and turned its answer into a list again, and the second date comparison did the same. Turning masks into rows was 9 percent of q12 and turning rows into masks another 5.

`Prepared::masked` already kept one mask across the conjuncts of an `AND` that compare a column with literals, and only turned it into rows at the end. It took only those, and only when there were two columns or more, so q12, with one such column, never used it.

## The change

A comparison of two columns of the chunk is now a pass of `masked` of its own, through the new kernel `mask_against`, which narrows the mask with `Packed::against_words`. The passes go in the order the connective has learned, the same as before, and a pass whose columns are not both packed with no nulls is skipped and left to the threaded walk. Two passes of any kind are enough for `masked` to take the filter, so q12's range and its two date comparisons are now three passes over one mask, the rows come out of it once, and the `IN` on `l_shipmode` runs over those rows as before.

## Measured

Single thread at SF1 on server2, warm instructions and cycles as the difference between eleven runs and three in one process, against main at #2594. The answers to all 22 queries are the same bytes as before at one thread, and q4, q12 and q21 also at six.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q12 | 148 | 114 | 126 | 100 |

q06 and q19 run the same instructions as before.

## What this leaves

The compare in lanes and the range test are now a quarter of q12 between them, at under two instructions a row each, and every block of 64 rows still has rows in it after the range, so neither pass skips anything. Decoding the columns from their pages is the next largest part, about 10 percent.
