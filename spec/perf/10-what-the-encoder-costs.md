# What the encoder costs

Note 09 ended with a list of three things and the first of them was the native format, which is F2. F2's first exit criterion is a load time and its own body says the first thing to do is profile the encoder rather than redesign it. There was nothing to profile it with, so the first thing was building the instrument. It is `cargo xtask encode` and it landed in #552.

## What it does

It reads a Parquet file column by column and puts each column through `rudb_encoding::string::encode` or `rudb_encoding::integer::encode` in chunks of 122,880 values, which is the row group size the shell prints and is what a writer would hand the encoder if a writer existed. It prints two tables. The first is megabytes a second per column with the ratio and the shape, and a whole file line. The second splits the same seconds across the candidates that spent them, with how many chunks offered each candidate and how many kept it.

The second table needed two new functions on the encoding crate, `offered` and `encode_only`, which are the chooser's own candidate list and the chooser's own per candidate call. They are not a second copy of the rules. There is a test in each module that walks the list, encodes each candidate alone and asserts that the smallest is byte for byte what `encode` returned, so the split cannot drift away from the thing it is splitting.

## The numbers, on gamingpc-wsl at one million rows

913.60 MiB of raw value bytes across 105 columns, one thread, bench profile, median of nine passes.

| | |
|---|---|
| whole file | 20.3 MiB/s |
| ratio | 5.0 to one |
| the four string columns that matter | URL 20.3, Title 22.3, Referer 16.4, OriginalURL 17.2 MiB/s |

Where the seconds went:

| encoder | candidate | offered | kept | seconds | share |
|---|---|---|---|---|---|
| string | FRONT | 142 | 1 | 15.538 | 32.2% |
| string | DICT | 223 | 216 | 10.033 | 20.8% |
| string | FSST | 223 | 2 | 5.390 | 11.2% |
| integer | DELTA | 607 | 0 | 5.187 | 10.8% |
| integer | DICT | 541 | 128 | 4.876 | 10.1% |
| string | PLAIN | 223 | 4 | 4.178 | 8.7% |
| integer | RLE | 466 | 139 | 2.068 | 4.3% |
| integer | FOR+BITPACK | 634 | 162 | 0.708 | 1.5% |
| integer | SPARSE | 227 | 205 | 0.249 | 0.5% |
| string | CONSTANT | 29 | 29 | 0.014 | 0.0% |
| integer | CONSTANT | 59 | 59 | 0.002 | 0.0% |

Kept is the top level of a chunk only, so a candidate that recurses can be in the shape of every chunk and be kept zero times, because what won at the top was the thing that called it. Read the rows with seconds and no keeps: string FRONT, string FSST, integer DELTA and string PLAIN together are 62.9 percent of the encoder's time and were kept seven times out of 1,195 offers. That is what an exhaustive chooser is, stated in seconds rather than in the abstract, and F2's plan already names the fix.

The other half of the same table is that string DICT was offered 223 times and kept 216. On this dataset the top level answer is a dictionary nearly always, and it takes a full encode of four other candidates to find that out each time.

## Whether it scales, measured rather than assumed

Everything below rests on the encode being parallel, so the next thing was to stop assuming it. `cargo xtask encode --threads N` landed in #556. It holds every column in memory, builds a work list of one chunk of one column, hands those out to N threads through an atomic counter and times the whole set. One chunk of one column is exactly the unit F2's checklist calls "parallel by block and by column", so this is a measurement of the ceiling that item is aiming at, with the Parquet read and the block write taken out.

Same file, same machine, 945 chunks over 105 columns, median of nine passes:

| threads | wall s | MiB/s | speedup | efficiency |
|---|---|---|---|---|
| 1 | 67.725 | 13.5 | 1.0 | 100% |
| 2 | 30.745 | 29.7 | 2.2 | 110% |
| 4 | 16.415 | 55.7 | 4.1 | 103% |
| 8 | 9.466 | 96.5 | 7.2 | 89% |
| 16 | 6.047 | 151.1 | 11.2 | 70% |
| 32 | 5.788 | 157.8 | 11.7 | 37% |

IQR was 1 to 2 percent on every row, so none of this is noise. The encode is embarrassingly parallel up to eight threads and then it is not: sixteen threads buys 11.2 times and thirty two buys 11.7. That is the machine rather than the encoder. An i9-13900K is eight performance cores and sixteen efficiency cores, so the first eight threads get a real core each, the next sixteen get a slower one, and the last eight get a hyperthread of a core that is already busy.

The single thread number here is 13.5 MiB/s and the per column table above says 20.3 MiB/s for the same work on the same file. The difference is what is resident. The per column table measures one column nine times with that column in cache, and the scaling sweep holds all 913 MiB of all 105 columns and walks them once per pass. A loader looks like the second one, so 13.5 is the number to plan with and 20.3 is the number to compare candidate encoders with.

## What this does to criterion 1

The milestone body says criterion 1 is eighty times away, on the strength of M1's figure of 5 MB/s of values per core and 5.6 CPU hours for `hits`. The per column table says 20.3 MiB/s per core and the scaling sweep says 13.5. I have not explained the gap to M1's number and it should not be waved away: it came from a different measurement on a different machine, and these are bench profile builds with the passes and the spread printed, so I am quoting these and flagging that M1 disagrees with them by three to four times.

`hits` is about 89.2 GiB of raw value bytes, or 91,360 MiB, because a million rows is 913.60 MiB and there are a hundred million of them. Criterion 1 is 252 seconds. The arithmetic that matters is not per core, because the machine does not give thirty two cores' worth of anything. It is the 157.8 MiB/s the whole machine reached:

91,360 MiB at 157.8 MiB/s is 579 seconds. Criterion 1 is 252. The encoder on its own, with the whole machine, with no Parquet reading and no block writing and no compression, is 2.3 times over the entire budget.

I had this wrong in the first version of this note. I multiplied the per core rate by the thread count, got 650 MiB/s, and concluded the encoder would use 56 percent of the budget and that the parallel write path was the only thing that mattered. The measured machine delivers a quarter of that. Both halves of the conclusion change.

## What the sampled chooser is worth, measured

#559 put the search behind `rudb_encoding::chooser` with two implementations and added `cargo xtask encode --ablate` to price the trade. Same machine, same million row file, both sides in one process over the same values in memory, median of nine passes each.

| | exhaustive | sampled | |
|---|---|---|---|
| whole file | 11.1 MiB/s | 40.0 MiB/s | 3.61x faster |
| bytes out | 190,351,151 | 191,374,483 | 0.54% bigger |
| ratio | 5.03 to one | 5.01 to one | |

3.61 times the encode speed for half a percent of size. The two choosers picked a different top level shape on 32 of the 105 columns and it cost half a percent, which says the exhaustive search was mostly confirming what a sample already knew. The four columns the file is made of are the best of it, because they are where the search costs most and the data is most uniform: URL 2.50x for 0.02 percent, Title 2.55x for 0.29 percent, Referer 2.42x for 0.01 percent, OriginalURL 2.45x for 0.01 percent.

One column in that run was a defect rather than a trade. `Params` came out 490 percent bigger, because it is a million values that are almost all empty strings, it cleared a guard that counts values, and then the sample missed what little structure it had. The search cost scales with bytes and the guard was counting values, so #562 added a floor of one page of bytes underneath it.

Taking 3.61 times against the 579 seconds the scaling sweep implies gives roughly 195 seconds for the encode. That is under the 252 second criterion for the first time. It is also the encode alone, with no Parquet reading, no decompression and no block writing, so it would leave 57 seconds for all of those, which is not enough. Two things soften that: the sampled side has not been through the thread sweep so its scaling is assumed rather than measured, and the run predates #560.

## What the encoder was doing three times

Separately from the chooser, the encoders were repeating themselves, and both choosers paid for it because it happens before any candidate is offered.

On the string side, `candidates` decided whether a dictionary applies by building the whole sorted dictionary and comparing its length against the input, then throwing it away. That is a copy of every value onto the heap and a sort of the copies, per chunk per level. Then `Kind::Dict` built it again and found the code of every value by binary searching the dictionary back, which is a `memcmp` per level of the search per row. #560 made the first one a linear probe over hashes that stops at the first repeat, and the second one a sort of a permutation whose walk hands back the codes for free. On the ten thousand row fixture that is 26.6 to 43.5 MiB/s, a factor of 1.64, with the output byte for byte identical.

On the integer side `candidates` sorted the chunk twice, once to count distinct values and once to find the most frequent one, and built the whole delta array just to check that it did not overflow. One sort answers both questions and a predicate answers the third.

The same permutation trick does not carry over to the integer dictionary. Building the dictionary and the codes together from a sort of index pairs measured slower, which makes sense once stated: an integer dictionary only exists when the distinct count is at most half the row count, so the binary search is over something small and cache resident and compares one integer rather than a string, while carrying the source index through the sort means sorting a padded sixteen byte pair instead of an eight byte value. The search is cheaper than the wider sort. Worth writing down because it is the kind of thing somebody tries twice.

## What to do, in order

1. ~~The encoder chooser as a seam, with an exhaustive implementation and a sampled one.~~ Done, in #559 and #562. 3.61 times for 0.54 percent. This was second on the list before the scaling sweep showed parallelism alone is not enough.
2. **The write path parallel by block and by column.** Still required and still not built, but no longer sufficient by itself. The ceiling it is aiming at is now known rather than hoped for, which is worth more than the position in this list: eight threads is the point where efficiency is still 89 percent, so a scheduler that hands out chunks has a real target to hit and a known place where adding threads stops paying.
3. **Find the rest of the factor.** With the sampler the encode alone is about 195 seconds of a 252 second budget, so there is 57 seconds left for reading the Parquet, decompressing it and writing the blocks, and that is not enough. The next places to look, in order: run the thread sweep on the sampled chooser, because its scaling is currently assumed rather than measured and everything above rests on it; the string encoders themselves, which are three quarters of the seconds and have had no attention paid to them beyond #560; not encoding what will not be read, since ClickBench touches a minority of the 105 columns; and whether 122,880 values a chunk is the right unit for the encode when the cache says otherwise.
4. Everything else in F2, which is format work rather than performance work, and which none of the above is blocked on.
