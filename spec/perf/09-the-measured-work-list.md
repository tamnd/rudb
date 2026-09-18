# The measured work list

Note 08 ended with a change I was sure about and that turned out to be worth nothing. The way I picked it was line level callgrind attribution on one query, which is how you find a plausible hot spot and not how you find the work. So before picking the next one, this note is the whole suite measured by the engine's own instruments rather than by me reading a profile.

## How it was taken

All 43 ClickBench queries, one million rows, thirty two threads, `gamingpc-wsl`, main at 0.3.11, each run under `--metrics` and the documents added up. The numbers below are the engine's own counters and not a sampling profiler, so they are exact and they are attributed to the operator that spent them rather than to the symbol that happened to be on the stack.

## Where the CPU goes

| operator | cpu | share |
|---|---|---|
| FileScan | 3.694s | 47.0% |
| Aggregate | 2.987s | 38.0% |
| Filter | 0.766s | 9.7% |
| Project | 0.108s | 1.4% |
| TopN | 0.063s | 0.8% |
| Fetch | 0.006s | 0.1% |
| Sort | 0.000s | 0.0% |
| Limit | 0.000s | 0.0% |

Two operators are 85 percent of the engine and everything else is rounding. This is the same shape the ten million row run gave, so it is not an artefact of the small size, and the small size costs four minutes instead of forty.

## Inside the scan

The scan reports its own stages, and this is the first time they have been added up across the suite:

| stage | wall | share |
|---|---|---|
| decompress | 1.598s | 41.3% |
| decode | 1.161s | 30.0% |
| read | 0.550s | 14.2% |
| assemble | 0.401s | 10.4% |
| dictionary | 0.163s | 4.2% |

Decompression is 41 percent of the scan and therefore 19 percent of the whole engine. Note 08 spent a day establishing that there is nothing left in it: the room proving split that every fast Snappy does is worth 0.1 percent here, because LLVM was already folding the checks it removes, and at roughly 50 instructions an element and 1538 MB/s the decoder is within reach of the reference one. So 19 percent of the engine is a cost that cannot be reduced and can only be avoided, by not decompressing the same file forty three times. That is #103 and F2, and it is now measured rather than asserted.

Decoding is another 30 percent of the scan, 14 percent of everything. That one is ours: it is the hybrid RLE and bit packing reader and the plain reader, and unlike Snappy it is code we can change the shape of rather than a format we have to follow.

## The fall through counters, which say the thing I expected is not true

`crates/rudb-kernels/src/fallback.rs` counts every kernel call that gives up on a specialization and goes row at a time. F1 asks for this by name and the reason it asks is that the alternative to counting is guessing. Over the whole suite:

| cause | calls |
|---|---|
| aggregate | 12,811 |
| scalar | 354 |
| compare | 167 |

Thirteen thousand calls, across forty three queries over forty three million rows. Root cause 3 in note 07 said to invert the vector contract because kernels fall through to the row at a time path, and the instrument that was built to find out says they do not. Nearly all of what is left is three queries: q29 has 10,797, q23 has 1,676 and q22 has 320, and q23's aggregate spends two milliseconds in total so its 1,676 are not costing anything. The one worth a look is q29, whose aggregate is 0.313s over 810,666 rows, or 386 nanoseconds a row against a suite average of 128, and which is the query with a `REGEXP_REPLACE` in the group key.

So the fall through is not the problem and the specialization matrix is not the work. That is a good thing to know before writing sixty four loops.

There was a hole in the instrument and this note is what found it. The counters keep two tables, one per operator per kernel that goes into the metrics document, and one per kernel per form pair that answers which specialization is missing. The second one was computed by every kernel call and read by nothing, because `fallback::report` had no caller anywhere in the tree. So the suite could say thirteen thousand calls and could not say what forms forced them. `rudb --fallbacks` now prints it.

## What the list says to do, in order

1. The native format, #103 and F2. It removes decompress and most of decode, which is 33 percent of the scan and about 25 percent of the engine on its own, and it is the only thing on the board with that size. DuckDB does not pay any of it during a ClickBench run because it loaded the data first.
2. The aggregate, F5, at 38 percent with no fall throughs to blame. Whatever is slow there is slow in the specialized path, which means the two phase radix partitioned design is the answer rather than a missing kernel.
3. The decoder, the 30 percent of the scan that is ours rather than Snappy's.

Everything below those three is noise at this scale, and the next note should be about the first of them and not about another kernel.
