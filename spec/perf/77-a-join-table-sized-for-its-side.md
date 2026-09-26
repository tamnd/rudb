# 77. A join table sized for its side

## The problem

q09 joins `lineitem` to `partsupp` on two keys, `ps_partkey` and `ps_suppkey`. After the filter on `part` the `partsupp` side is 42,656 rows, and the step that builds the hash table over them took about 7.5 ms on one thread. That is 175 ns a row for a table that fits in cache. It is also below the 65,536 rows where a build is split into partitions, so at eight threads one thread builds it while the other seven wait.

A trace inside the partition fill split the 7 ms like this on one thread:

| part | time |
|---|---|
| batched probe of each batch | 2.0 to 3.1 ms |
| probe again and insert, a row at a time | 4.1 to 4.8 ms |
| chaining the rows onto their keys | 0.13 ms |

Every one of the 42,656 rows was a new key. Two things followed from that.

1. The table was the one a group by uses and started at 64 buckets. It doubled eleven times on the way to 131,072, rehashing every key it held at each step, and its key columns and hashes grew by doubling beside it. The side is all in hand before the first row goes in, so how many rows a partition holds is known exactly, and it is an upper bound on how many keys it can hold.
2. Each batch of 64 rows was probed as a batch, which found nothing, and then each row was probed again on its own and inserted. The batched probe exists to find rows whose key is in already. On a side whose keys are all different, which is what a join side usually is, it only ever learns that it found nothing.

## The change

`Table::for_rows` builds a table with room for every row of a partition to be a key of its own: the buckets for that many keys at half full, and the hashes and the fixed width key columns reserved for the same count. A partition's fill uses it, and reserves its chain heads and tails the same way.

The fill keeps track of whether the last batch found any key that was in already. When it found none, the next batch skips the batched probe and goes straight to the row at a time probe and insert. That path is exact on its own, because it probes before it inserts, so a key that repeats is found there whichever path the batch took. The first batch that finds one switches the batched probe back on. Before a skipped batch the fill reads the bucket each of its rows starts at, in one pass with no dependency between loads, so on a table larger than the cache the misses still overlap the way they did in the batched probe.

## Results

All 22 answers match the binary before this change at one and at eight threads. The `partsupp` table step in q09 on one thread, from the build trace on server3:

| build | table step |
|---|---|
| before | about 7.5 ms |
| sized for its side | about 6.7 ms |
| sized, and new keys probed once | 3.6 to 4.4 ms |

The table's footprint went from 2,979,072 bytes to 2,424,232, because it is now sized for what it holds rather than left somewhere between half full and full after its last doubling.

Instructions at SF1 on one thread, a run each:

| query | before | after |
|---|---|---|
| q09 | 1266 M | 1256 M |
| q20 | 503 M | 501 M |
| q11 | 94 M | 93 M |

No query goes up.

## What is left

The fill is still about 70 ns a row, most of it the insert, which pushes each key through the general per row path of the group by table. The q09 side is also still built on one thread at eight threads. Lowering the partition threshold to 16,384 rows cut the fill to 3.6 ms across eight partitions, but it makes every probe row deal itself to a partition too, and on a loaded box the wall times of the two could not be told apart, so the threshold is unchanged.
