# 110. Run starts and closed runs in one pass

## The problem

Note 109 left q18 mostly finding runs. An aggregate over a key the rows are sorted on closes each run of the key inside a chunk on the spot, and for that it needs two things from the key: where the closed runs begin and end, and where each of them starts. They came from two passes. `interior` read the key to check that it never goes down and to find the first and the last change, and then `run_starts` compared the same neighbours again between those two to find every start. On q18 at SF1 on one thread the two were 16% of the cycles, `run_starts` 10% and `interior` 6%.

Around that, two buffers were zeroed on every chunk and written over straight after. The running total that note 109 adds a packed argument up with was cleared and grown again, 64 KB a chunk of q18, and the answers the `HAVING` checks were a fresh vector of zeroes, 32 KB a chunk. memset was 5% of the query.

## The change

`closed_runs` replaces both passes. It walks the whole chunk once, finds every row whose key is not the one before it, and checks the order in the same loop. The first start it finds is where the closed runs begin, the last is the first row of the last run, which goes to the table, and what is between them is every closed run's start. `interior` stays for the closing path that folds into a table, which needs the ends and not the starts.

The walk compares each row with the one before it into a bit of a word, sixty four rows to a word, and reads the starts out of the set bits, which needs no branch a row on a run of four. A packed key is unpacked once into a buffer the thread keeps and walked like a flat one.

The running total and the answers for the `HAVING` are buffers the thread keeps and only grows. The unpack writes every word of the running total after the first, and the call writes every answer before the check reads it, so neither needs zeroing.

## Results

Measured on server2 at SF1 on one thread, per warm run, three pairs run one after the other against main with #2457. The box was shared and loaded, so the cycles move from pair to pair, and every pair went the same way. All 22 queries give the same answers as before.

| Build | User cycles (M) | Instructions (M) | Wall (ms) |
| --- | --- | --- | --- |
| main | 125, 137, 126 | 175 | 44, 53, 40 |
| one pass and kept buffers | 111, 125, 116 | 175 | 32, 51, 38 |

The instructions are the same, and the cycles are about a tenth fewer. The second pass read neighbours the first had just brought into cache, so it cost cycles more than instructions, and so did the memset. In the profile `interior` is gone, the one walk is where `run_starts` was, and memset went from 5% to 1%.

Comparing four rows at a time with AVX2 was tried as well, with the walk moved into `rudb-vector` for its `unsafe`. It took two million instructions off and no cycles, and the walk's share of the profile went up. The walk is the first thing to read `l_orderkey` after the scan, 48 MB at SF1, and a quarter of its samples sat on the instruction after the loads, so it is waiting on memory and not on the compares. The portable walk stays.

## What this leaves

The walk reads eight bytes a row of a key whose runs are four rows long and whose values within a page span about seventeen bits. The way to make it cheaper is to read fewer bytes, which means holding a sorted key packed. Note 107 held delta pages flat because grouping by runs was four times slower over packed codes, but that was with a code read a row at a time and an argument flattened every chunk, and neither is true now. That was measured afterwards and did not pay, see note 111.
