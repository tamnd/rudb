# 102. Filtering in masks

## The problem

A filter that is an `AND` of comparisons hands each conjunct the list of row indexes the ones before it kept, and the next conjunct reads its column at those rows and pushes the ones it keeps onto a new list. That is fine once the list is short, but in the first two or three conjuncts it is most of the rows, and every read is a random code fetch out of a bit packed column and a branch on whether to push.

TPC-H q06 pushes five comparisons on four lineitem columns into the scan and keeps 114,160 of 6,001,215 rows. In the steady profile the packed range kernel was 25.5% of the cycles, almost all of it in the path that reads `packed.code(row)` for each row on the live list, and the flat range on `l_quantity` another 13.7%. The query cost about 17 cycles a row against a floor of 2 or 3 for reading four packed columns once.

## The change

When a conjunction has two or more columns compared to integer literals, the prepared filter answers those first as a bit mask, one bit a row, one column at a time. `mask_within` in the kernels intersects every bound on a column into one inclusive range, works in code space for a bit packed column, and writes or ands one word per 64 rows. A word that is already zero is skipped, so the later columns only unpack the blocks that still hold rows. `mask_selection` turns the mask into the selection the remaining conjuncts take.

Each column's read stays sequential and branch free, and the work per row is a compare and a bit. The masks stop as soon as fewer than one row in sixteen is left, because from there a short index list is cheaper than a mask over the whole chunk, and the rest of the conjuncts go on as before. Columns are taken in the order the filter has learned, and what each one kept is fed back into that order.

It refuses a column with nulls, a column that is not integral, a literal of another type and any comparison other than equal, less and greater with or without equal, and leaves those to the conjuncts as they were.

## Results

Pending the release build, posted on the pull request.
