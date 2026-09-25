# 67. A join table built on every thread

## The problem

q09 builds two join tables before it probes with `lineitem`. One is on `orders`, 1.5 M rows keyed by `o_orderkey`, which takes the direct form: a head slot for each key place, 6 M places at SF1. The other is on `partsupp`, 800 k rows keyed by two columns, which takes the hashed form. The query then probes with 319 k `lineitem` rows, so the builds are most of its work, and at six threads the main thread did 36 percent of the query's CPU while the other five waited on it.

A trace of the `orders` build at eight threads showed where the time went:

| phase | time |
|---|---|
| lay out the rows | 100 ms |
| key range | 3 ms |
| deal rows to partitions | 28 ms, on one thread, a 64 bit division a row |
| fill each partition | 19 ms |
| join the partitions' heads | 149 ms, on one thread |

Each partition filled a head of its own and the table then joined them into one 6 M slot vector. That copy was the largest phase, and it ran on one thread, as did the deal. The zeroed head was also read before it was written, so the kernel mapped the shared zero page first and then had to flush every core's TLB when the page was written, which showed as 4.5 percent of the profile in `smp_call_function_many_cond`.

The `partsupp` build had a waste of its own. The part filter narrows it to about one row in twenty through the sideways domain, but the domain was only asked inside the table build, after every row had been copied into the laid out columns, hashed and dealt.

## The change

The direct form now builds one head for the whole table. Each partition covers a run of places whose length is a power of two, so a row's partition is its place shifted right, not divided. The head is cut into one share for each partition with `split_at_mut`, and each partition writes its own share, zeroing it first so that the page is written before it is read. Rows are walked in reverse, so each place's chain comes out in row order without a tail vector. A head slot stores the first row plus one, with zero meaning empty, so the zeroed vector is already the empty table.

The deal now runs on every thread. Rows are cut into one slice for each thread, each slice deals its rows into a vector per partition, and each partition then reads its vectors from every slice in order. The hashed form uses the same deal and fills each partition from those vectors, so the old serial `deal_rows` is gone.

Before the build side is laid out, `Probe::built_with` asks the sideways domain of each key that is a plain integer column, one chunk at a time on every thread, and drops the rows that cannot match. A chunk that keeps more than half its rows is kept whole, since a row that cannot match does no harm in the table and copying the chunk to drop a few costs more than it saves. This is only done for a plain probe, which never hands back a build row without a match, so no answer changes.

## Results

All 22 answers match main. Instructions at SF1 on one thread, on server3:

| query | main | after |
|---|---|---|
| q09 | 2038 M | 1789 M |

The other 21 queries are within two percent, q08 the most at 506 M to 515 M, and that is about what main moved between the two builds. At six threads on server2, q09's CPU went from 554 to 691 ms a run to 478 to 559 ms, and its wall time from 686 to 926 ms to 530 to 596 ms. The TLB flush went from 4.5 to 0.1 percent of the profile. Both machines were shared with other builds, so the six thread numbers are ranges over several runs.

## What is left

Laying out the build side is still one column per thread, with a flatten and a concat of each column, and for `orders` that is 44 to 100 ms. The key is copied into a signed block before it is placed, the `next` vector is set up on one thread, and the rank over the key places used when the range is sparse is built on one thread too. Each of these is a serial pass over the whole build side and is the next thing to take on.
