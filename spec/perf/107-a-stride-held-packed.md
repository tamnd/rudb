# 107. A stride held packed

## The problem

A page the cascade codec wrote is decoded once and then held for the statements that come after it, so the form it is held in is the form every later scan reads. Every cascade page was held flat, eight bytes a row, whatever the cascade had found in it.

`l_quantity` is a decimal of whole units, which the cascade writes as a stride of 100 over six bit codes. Held flat it is 48 MB at SF1. After note 106, q06 tests `l_shipdate` and `l_discount` as packed codes in lanes and keeps about 4% of the rows, and then reads `l_quantity` for those rows out of the 48 MB flat array, one cache line for each kept row it touches. That load was most of what was left of the filter.

Holding every cascade page packed was tried with note 106 and left out. It made q18 four times slower, because `l_orderkey` is a cascade page too, written as deltas, and the grouping by runs over a sorted key is much slower over packed codes than over flat values.

## The change

`integer::is_strided` says whether a chunk is a stride, from its tag. The native reader asks it of a codec 5 page after the cascade decodes it, and holds a stride packed over the range of its values and anything else flat as before. `l_quantity` is held in 13 bits a row, under 10 MB at SF1, and the packed filter kernels of notes 102 and 106 take it. `l_orderkey` and the other delta pages stay flat.

The rule is about the shape of the values rather than about a query. A stride is a narrow range of codes that decodes to a wide range of values, so flat costs it the most against packed. A delta is a sorted column, which is a key, which is grouped and joined on, and those paths are fastest flat.

## Results

Measured on server2 at SF1 on one thread, user cycles in millions, against main with #2431. All 22 queries give the same answers as before.
