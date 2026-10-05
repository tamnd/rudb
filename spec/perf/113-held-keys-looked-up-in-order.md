# Held keys looked up in a chunk in order

Notes written on 5 October 2026, while finding out where the instructions of TPC-H q02 go.

## The question

q02 keeps 747 of the 200,000 parts, those of size 15 whose type ends in `BRASS`, and reads `partsupp` twice, once for the outer query and once for the minimum cost in the subquery. Both scans of `partsupp` take a bitmap of the kept parts from the join above them and test `ps_partkey` against it, so 1.6 million rows are tested to keep about 3,000. Counted in instructions over sixty warm runs on one thread, that test was 15% of the query, and the other bitmap tests another 4%.

`partsupp` is stored in `ps_partkey` order, four rows a part. A chunk of eight thousand rows covers about two thousand parts, and of those the bitmap holds about seven. Only those seven keys can be kept, and each one is a run of four rows that a binary search finds.

## What changed

`Domain::over_sorted` takes a chunk of flat `INTEGER` or `BIGINT` keys of at least 1,024 rows. It works out the stretch of keys between the chunk's first and last row, counts the keys the bitmap holds in that stretch out of the words that cover it, and gives up if there are more than one for every 32 rows or if the stretch is wider than one word for every 64 rows. Only then does it check that the chunk's keys never go down, in one pass with no branch inside a block of 256. If they do not, each key the bitmap holds in the stretch is found with two binary searches from where the last one ended, and its rows are set in the answer.

A chunk whose keys are in no order mostly fails on the first compare of its first and last key, or on the width of the stretch, and costs a few words of the bitmap. One that passes those and is still out of order costs the pass that checks the order on top of the test it would have had anyway.

The file has a summary that says which columns never go down, which the grouped aggregate reads, but a bitmap test sees one chunk and no column name, and the check per chunk costs a fraction of a cycle a row. So the chunk is checked rather than the summary plumbed down.

## Measured

Single thread at SF1 on server2, the warm instructions of a query as the difference between eleven runs and three in one process, against main at #2521. The answers to all 22 queries are the same bytes as before.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q02 | 43 to 47 | 32 to 38 | 39 to 40 | 38 to 39 |

The other 21 queries run the same instructions as before within one million.

The cycles moved much less than the instructions. The bitmap test ran four instructions a cycle over memory it streamed in order, so it was a sixth of the instructions and a tenth of the time. What is left in q02 is spread thin: the operators and the vectors between them are about half, allocation about an eighth, and parsing, binding and optimizing the query about a tenth.

## What this leaves

At 38 million cycles q02 is about two and a half times faster than DuckDB's 90 million. Most of what is left is the cost of running a plan of 33 operators over a few thousand rows rather than of reading data, so the next step for it is making each operator cheaper to start, feed and finish, and allocating less between them.
