# 104. A semi join asks for one row

## The problem

[`98-keys-a-runtime-filter-cannot-reach.md`](98-keys-a-runtime-filter-cannot-reach.md) gave q05's `customer` scan a semi join on `c_nationkey` against the suppliers of ASIA. That join keeps the 30,183 customers of the five Asian nations out of 150,000, and `EXPLAIN ANALYZE` on server2 put it at 25 ms of CPU, against 28 ms for the whole `lineitem` scan and 75 ms for the query.

The build side is 2003 suppliers with five distinct keys, so each key has a chain of about 400 rows. The probe answers a driving row off the first row of its key only when every key in the table has one row. Otherwise it copied the whole chain of the key into a buffer and then asked whether the buffer was empty. A semi join only needs to know whether a key has a row, so each of the 30,183 customers that matched walked 400 rows to learn one bit, about 12 million steps.

## The change

A semi or anti join with no residual now takes the batch path whatever the table holds. It reads each driving row's first row out of the lookup it already did for the chunk and keeps the row when there is one, for a semi join, or when there is none, for an anti join. The chain is never walked. An inner join still takes the batch path only when every key has one row, and a join with a residual still walks the chain, since the residual has to be asked of each candidate.

## Results

Pending the release build, posted on the pull request.
