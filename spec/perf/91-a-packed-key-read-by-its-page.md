# 91. A packed key read by its page

## The problem

After note 90, q01's grouping below had `Aggregate::fold` at 228 M instructions on its own, and two test queries over the same four keys cost more than q01 did: the grouping without the date filter was 709 M, and `l_discount` by `l_tax` with a count was 420 M, of which the fold was 389 M, about 65 instructions a row for two keys and a count. Both keys are 4 bit packed pages, and the fold was paying for them in one of two ways.

A key that stayed on its page was read a code at a time. `Packed::code` works out the word, reads it through a bound and asks whether the code straddles the next word, which is about twenty instructions a row for each column.

A key whose page was packed against another base than the map's is read by value against a window instead, and from then on every chunk of it is, since the map now holds a window. That widened the codes into 64 bit values, which is cheap, and then walked every value for the lowest and highest, which is a compare and a blend a value and was a fifth of the fold in q01. The page already says both: the lowest is its base and the highest is the base plus the largest code its width allows.

## The change

Two changes in `rudb-exec`, both in the direct map's places.

A key of several columns read by value takes its window from the page's base and width when it is packed, or a packed run behind the rows a filter kept, and does not walk its values. The window is the width's rather than the chunk's, so `l_discount` takes seventeen places where it took twelve, and q01's four keys take 3,468 places, inside the 4,096 the map allows since #2254. A key of one column keeps the walk, because the walk is also where its runs are found.

A packed key placed by its codes is unpacked sixty four codes at a time with `Packed::unpack`, the first block ending where the packed rows reach a whole word so that every block after it unpacks as one. A new test places a packed column cut five rows into a word, over more rows than a block.

## Results

server3, SF1 native, one run per query in a fresh process, threads 1, millions of instructions:

| | before | after |
|---|---|---|
| q01 | 725 | 631 |
| q01 by flag, status, discount and tax, written by hand | 725 | 630 |
| the same grouping without the date filter | 709 | 527 |
| `l_discount` by `l_tax` with a count | 420 | 234 |
| `Aggregate::fold` in q01, itself | 228 | 128 |

The other 21 queries are within 1 M, and every answer is the same at one thread and at eight.

## What is left

q01 is 631 M, down from 883 M before note 89. The fold and the adds are now about the same size, 128 M and 124 M. Decoding the columns is most of the rest: RLE expansion of `l_linestatus`'s codes at 34 M, the stride decode of `l_quantity` at 21 M, and unpacking and widening the packed columns at about 75 M. Summing `l_extendedprice` and `l_quantity` from their codes, as a count times the base plus a sum of codes, would take most of that unpacking off the path.
