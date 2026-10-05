# Totals by group through the map

Notes written on 5 October 2026, while working out why the aggregate of TPC-H q01 still cost more per row than its plan should.

## The question

The pre-aggregate of q01 groups by `l_returnflag`, `l_linestatus`, `l_discount` and `l_tax`, which is about 400 groups, and its calls are two sums over packed columns and a count. Every chunk went through the same steps to get there. `Coded::places` wrote each row's place in the coded map, the map was read at every place into a vector of slots, the rows the filter dropped were taken back out of that vector, and the slots were read again to see whether they came in runs. Then each packed argument was unpacked into a vector of `i64` of its own, and only then did the walk add anything up.

A total does not need a slot written down for each row. It needs the group, the value, and an add.

## What changed

`PlaceSums` in `rudb-kernels` takes a chunk whose calls are all totals or counts over packed columns of at most 32 bits or flat integers of at most 32 bits. It reads each of those columns once for the chunk into a list it keeps from chunk to chunk. Then it walks the rows the filter kept, reads the map at each row's place, and adds that row's values and a one for its count into its slot's cells. A row whose place holds no group yet stops the walk, the caller opens a group for it and writes it into the map, and the walk goes on from that row.

A packed column is added up as its codes. Its base is added once per slot at the end of the chunk, as many times as the slot had rows, so no code is widened into its value one row at a time. Over fewer than 2^31 rows no total of codes of 32 bits can leave an `i64`, so the adds need no check.

Each slot's cells are padded to a power of two, a total for each call and then the count, so that a row's adds are one vector add. For q01 that is four cells, two totals, a count, and one that stays zero.

## What did not work

The first version still went through the old limits on how many places a map may have, and on q01 it ran on 9 chunks of 736. A page read as a window of values gives a map far larger than the chunk, and those limits were written for maps the size of the chunk's combinations. Reading the map at each row's place has no such limit.

The second version walked every row in blocks of 64 and unpacked each block of each column as it went. A chunk of a page read as a window of values starts anywhere in its words, so every block of 64 fell across two of the unpack's blocks, and the unpack went a code at a time. It also built a word of kept rows for every block and sent the dropped rows to a group nothing read. That version ran more instructions than main and about the same cycles. Reading each column once for the chunk and visiting only the kept rows by their index fixed both.

## Measured

Single thread at SF1 on server2, against main at #2481. The answers to all 22 queries are the same bytes as before.

| query | instructions before (M) | after (M) | cycles before (M) | after (M) |
| --- | --- | --- | --- | --- |
| q01 | 486 | 446 | 284 | 232 |

Over a batch of q01 runs the cycles were 12 to 16% fewer in each of three runs of each binary. The other 21 queries run the same instructions as before within one million.

## What this leaves

In q01 the walk is now 17% of the query, the unpack of the two summed columns 10%, and `Coded::places` 9%. Folding each chunk's totals into the accumulators is another 6%, for about 400 slots a chunk over 736 chunks. Keeping the totals in wide cells across chunks and folding them once at the end would remove most of that. Working the place out for each block of kept rows rather than for the whole chunk would let the places stay in the first level cache instead of going through a vector.
