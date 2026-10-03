# 100. Holding a sparse part packed

## The problem

#2337 wrote a part out flat when a read of some of its rows was the one that held it, so that later gathers out of it would be a load per row rather than an unpack. It was measured in instructions, and in instructions it was a win. In time it is not always one. A flat part of a `BIGINT` or `DECIMAL(15,2)` column is eight bytes a row, while the same part packed is the few bits its values need: 24 for `l_extendedprice`, 14 for `l_suppkey`, 4 for `l_discount` at SF1.

A read that wants one row in twenty lands a row or two in each cache line of a flat part, so what it pays is the memory the whole part spans, not the work per row. On q09 the green parts keep 319,404 of the 6,001,215 rows of `lineitem`, and the gather out of the held flat parts was 36% of a query that reads only those rows of four columns, every load waiting on memory. The same gather was the largest single cost of q09 and q21 at steady state, and a large share of q05 and q10. Writing the part out flat in the first place also costs a page fault for every 4 KB of it.

## The change

`Reader::keep` now takes how many rows the read that holds a part wants, and writes the part out flat only when that is at least one row in `SPARSE_RENT`, which is eight. A part held by a sparser read stays packed, and later gathers out of it unpack the rows they want with `Packed::values_at`. A read of a dense share of the part, the case #2337 was about, still holds it flat, and a whole read still holds it packed as before.

The decision is made once, by the read that holds the part. The reads after it in a warm run are the same query at the same density, so the read that holds the part is a fair guess at the reads to come.

## Results

Pending the release build, posted on the pull request.
