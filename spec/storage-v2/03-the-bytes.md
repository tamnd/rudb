# The bytes

The goal is half of Parquet. This document says where the half comes from, with the arithmetic written out and the assumptions named, because a compression target without arithmetic behind it is a wish.

Everything measured here is ClickBench's `hits.parquet` on the bench host: 99,997,497 rows, 105 columns, 14,779,976,446 bytes on disk, 226 row groups, Snappy.

It was written before the numbers in it were measured and then corrected against them, and the corrections were large enough that the conclusion changed. What follows is the corrected version, with the section at the end saying what the first draft got wrong and how. The short version is that the 2x target is not reached, the design as specified lands between 1.45 and 1.79 times against the Snappy file, and against Parquet given its best settings it is roughly at parity.

## Which Parquet the target is measured against

The goal in the README says half of what Parquet writes with Parquet given its best settings rather than its defaults, and then anchors that on 14.8 GB. Those two halves of the sentence disagree, and the disagreement is worth about 1.6 times.

`hits.parquet` is Snappy throughout: 23,730 column chunks, 14,090.6 MiB compressed from 34,757.6 MiB raw, no other codec anywhere in the file. Snappy is Parquet's habit, not its best. Rewriting the ten million row subset through DuckDB with ZSTD and the same row group size gives 1,219,105,668 bytes against the Snappy file's 1,959,749,457, so ZSTD is 1.608 times smaller on this data. Carried across, Parquet at its best settings holds hits in about 8763 MiB rather than 14091.

Both numbers appear below. Beating the Snappy file is the easier claim and it is the one the rest of this document works out, because it is the file that exists and the file every other measurement here was taken from. Beating the ZSTD file is the claim the README actually makes, and the design does not currently do it.

## Where Parquet's bytes actually are

Summed from `parquet_metadata`, compressed bytes per column across every row group. The dictionary column is the size of the dictionary pages, taken as the gap between the dictionary page offset and the first data page offset.

| column | MiB | share | dictionary pages MiB |
|---|---|---|---|
| URL | 2530 | 18.0% | 99.7 |
| Title | 2319 | 16.5% | 112.5 |
| Referer | 2144 | 15.2% | 122.1 |
| OriginalURL | 1412 | 10.0% | 72.3 |
| WatchID | 823 | 5.8% | 226.0 |
| HID | 479 | 3.4% | 212.9 |
| ClientEventTime | 394 | 2.8% | 117.2 |
| URLHash | 391 | 2.8% | 166.5 |
| LocalEventTime | 385 | 2.7% | 112.6 |
| EventTime | 384 | 2.7% | 112.6 |
| SearchPhrase | 356 | 2.5% | 96.5 |
| RefererHash | 347 | 2.5% | 158.5 |
| UserID | 258 | 1.8% | 140.4 |
| FUniqID | 235 | 1.7% | 125.8 |
| ClientIP | 179 | 1.3% | 75.2 |
| RemoteIP | 177 | 1.3% | 73.8 |
| the other 89 columns | 1278 | 9.1% | |
| **total** | **14091** | | **2078** |

Four string columns are 8405 MiB, which is 59.6 percent of the file. Nothing else in the table comes close, so a design that does not do something about those four cannot reach 2x no matter what else it does.

## What Parquet is doing with those four columns, which is not what I assumed

The starting assumption was that Parquet's cost on a high cardinality string column is that it stores the dictionary once per row group, so the same URL is written 226 times. The measurement says otherwise. URL's dictionary pages total 99.7 MiB across the whole file, which is four percent of what the column costs. The repetition is real and it is not the problem.

The encodings say what is actually happening.

| column | encodings | chunks | MiB |
|---|---|---|---|
| URL | PLAIN_DICTIONARY, PLAIN, RLE, PLAIN | 217 | 2521 |
| URL | PLAIN_DICTIONARY, PLAIN, RLE | 9 | 8.5 |
| Title | PLAIN_DICTIONARY, PLAIN, RLE, PLAIN | 209 | 2303 |
| Title | PLAIN_DICTIONARY, PLAIN, RLE | 17 | 15.4 |
| Referer | PLAIN_DICTIONARY, PLAIN, RLE, PLAIN | 226 | 2144 |
| OriginalURL | PLAIN_DICTIONARY, PLAIN, RLE, PLAIN | 178 | 1403 |

The trailing `PLAIN` is the tell. A Parquet writer starts a column chunk with a dictionary and abandons it when the dictionary page passes a size limit, at which point it writes the rest of the chunk as plain strings. 217 of URL's 226 chunks did that, and those 217 hold 2521 of the column's 2530 MiB. The 9 chunks that kept their dictionary to the end hold 8.5 MiB between them.

So Parquet is not repeating the dictionary. **Parquet is giving up on the dictionary and storing a hundred million copies of eighteen million strings**, Snappy compressed, at 26.5 bytes a row for URL against an average length of 87.9.

That is a much better thing to attack than the one I thought was there, and it is the whole size case of this design.

## What a global dictionary costs instead

Two costs. The dictionary body, which is the distinct values stored once and sorted, and the code stream, which is one code a row.

The first draft sized the body as the distinct count times the column's average value length, and that is wrong in a way worth naming because it is easy to repeat. The average length of a value in the column is not the average length of a distinct value, and on a column that repeats it is not close. The short values are the ones that repeat, so they are over represented per row and under represented per distinct value. Every body in the first draft was too small by a factor of nearly three.

The bodies were then dumped and measured. `SELECT DISTINCT c FROM hits ORDER BY c` for each of the five columns, written out, counted.

| column | distinct | average length per row | average length per distinct value | body raw MiB |
|---|---|---|---|---|
| URL | 18,342,019 | 87.9 | 183.95 | 3217.8 |
| Title | 9,425,424 | 56.7 | 133.35 | 1198.6 |
| Referer | 19,720,797 | 63.4 | 136.35 | 2564.5 |
| OriginalURL | 8,510,123 | 53.1 | 392.71 | 3187.2 |
| SearchPhrase | 6,019,103 | 4.1 | 65.23 | 374.4 |
| **total** | | | | **10542.5** |

OriginalURL is the column that shows the effect at its worst. It looks like a cheap column at 53.1 bytes a row, and its distinct values average 392.71 bytes, because the column is empty in 83 percent of rows and the values that are there are long. The first draft had its body at 431 MiB. It is 3187.

So the raw dictionary bodies are 10,542 MiB, not the 3692 the first draft assumed, and the question of how well they compress matters three times more than it did.

## How well the bodies actually compress

Two encoders over the same bytes. The comparison is a stratified sample, every eighth chunk of 122,880 sorted values taken in order, which keeps each chunk contiguous in the sort and so front codes exactly like the real chunk would. The sample tracks the full file to within four percent on every column, so it is standing in for the whole thing honestly.

| column | sample raw MiB | ZSTD on the sorted dictionary | our encoder, first measurement | our encoder, with a matcher |
|---|---|---|---|---|
| URL | 420.61 | 5.33x | 2.6x | **4.3x** |
| Title | 158.28 | 4.29x | 2.8x | **4.0x** |
| Referer | 363.95 | 3.58x | 2.3x | **3.0x** |
| OriginalURL | 399.24 | 6.66x | 2.8x | **5.4x** |
| SearchPhrase | 52.38 | 3.68x | 3.4x | **3.7x** |

The fourth column is what the encoder did when this document was first corrected against measurement. The fifth is the same instrument over the same five files after PR #576, which closed #575 by adding a match finder to the string cascade. That change came out of the three paragraphs below, so they are left as they were written and the outcome is recorded after them.

Document 07 said the thing to do was point `cargo xtask encode` at a sorted dictionary and stop if URL came back under four. URL came back at 2.6. The check fires.

The two columns of that table are the whole finding and they say opposite things.

**The redundancy is there.** ZSTD gets 5.33 times on URL and 6.66 on OriginalURL over the identical bytes, so a sorted global dictionary really is highly compressible and the premise of the design survives. The first draft's guess of five was close for URL and low for OriginalURL.

**Our encoder does not capture it.** We are below ZSTD on every column and below it by 2.05 times on URL and 2.38 on OriginalURL. That is an encoder gap and not a format gap, which is a much better problem to have, but it is not a problem that goes away by writing a format. The shape says something about why: front coding fires, and then the suffixes go to `DICT` rather than to FSST, and a dictionary over the suffixes of eighteen million sorted URLs is close to no dictionary at all. Front coding only reaches sharing between adjacent values, and ZSTD's window reaches a domain name that last appeared a thousand values ago. That distance is where the missing factor of two is.

**Sorting is worth having and it is not where most of the win is.** The control: the same sampled dictionary shuffled by hash and written with the same ZSTD. URL goes from 82,782,188 bytes sorted to 129,361,698 shuffled, so sorting is worth 1.56 times. Referer goes from 106,581,410 to 140,868,376, worth 1.32 times. The rest of ZSTD's factor of five is redundancy that has nothing to do with order, which means an encoder that exploits only adjacency is leaving most of it behind by construction.

**What happened when that was acted on.** The last paragraph is the whole diagnosis and the fix followed from it directly: add the one thing the crate did not have, which is a matcher that can reach a repeat six hundred values back, and hand its output to the encoders that were already there. The fifth column is the result. SearchPhrase now beats ZSTD, OriginalURL and Title are within about twenty and seven percent of it, and URL and Referer are still nineteen and sixteen percent behind. The shape the chooser now picks on a sorted URL dictionary is front coding for the shared prefix, then the matcher over the suffixes, then FSST on the literal runs the matcher did not cover, then a dictionary and run length encoding over the FSST codes. Nobody named that shape.

## The code stream

A code is 23 to 25 bits on these columns and there are a hundred million rows, so a flat code stream is 274 to 298 MiB a column and 1442 MiB across the five. That is a tenth of the file spent on pointers, so **the code stream has to be encoded per block like any other integer column, and it is not a special case exempt from the encoders**. Three things make it compressible.

**Runs.** A column that is mostly one value is mostly one code repeated. Measured, the empty string accounts for:

| column | rows that are empty | share |
|---|---|---|
| URL | 67,763 | 0.07% |
| Title | 14,910,417 | 14.91% |
| Referer | 18,964,761 | 18.97% |
| OriginalURL | 82,975,640 | 82.98% |
| SearchPhrase | 86,825,105 | 86.83% |

OriginalURL and SearchPhrase are the columns this rescues. Their code streams should collapse to roughly the non empty share, so 286 MiB becomes about 49 and 274 becomes about 36. URL gets nothing from this and Title and Referer get about a sixth. The numbers below take the credit in proportion to the non empty share, which assumes the empty rows come in runs rather than every other row, and that assumption is not measured.

**Locality of code assignment.** This is the reason document 04 assigns codes in first arrival order rather than in sorted order, and it is a benefit on top of the parallelism argument. hits is clustered by time, so the URLs a block contains are mostly URLs that were first seen near that block, so their codes are mostly consecutive. A per block frame of reference over the codes then stores a much narrower integer than 25 bits. Sorted codes would have destroyed this, because a block's URLs are spread across the whole sorted order.

For that to hold, the shards in document 04 have to hand out code ranges in large chunks from one counter rather than owning fixed interleaved ranges, so that a thread writing a block draws its new codes from one or two contiguous runs. That is a constraint on the writer and it is recorded here because it is a size argument rather than a concurrency one.

**Nothing else.** There is no third thing. If the first two do not hold on a dataset, the code stream is 25 bits a row and the win is whatever the dictionary body saved.

## What does not get better

**The high cardinality integers.** WatchID, HID, URLHash, RefererHash, UserID, FUniqID, ClientIP and RemoteIP are 2889 MiB between them and a global dictionary makes them worse, not better.

UserID is the clean example. 17,630,976 distinct values in a hundred million rows, eight bytes each. A dictionary is 17.6M times 8, so 135 MiB of body, plus a 25 bit code stream at 298 MiB, so 433 MiB. Parquet stores the column in 258 MiB. The dictionary loses by a factor of 1.7 and it loses for a reason that generalises: **a dictionary only pays when a value is wider than its code, and an eight byte integer against a twenty five bit code is not.**

So the distinct ratio threshold in document 04 is not the right rule on its own. The rule is arithmetic: a dictionary is used when `distinct * value_width + rows * code_width` is less than what the column costs without one, and for a fixed width type that comparison is exact rather than estimated.

WatchID is the extreme, at 99,997,493 distinct values in 99,997,497 rows, which is unique for practical purposes. Parquet spends 226 MiB of dictionary pages on it and gets nothing, which is 1.6 percent of the file wasted on a dictionary that never matches. Our writer will not build one, and that is a small win the table above does not count.

**The long tail.** The 89 columns outside the table are 1278 MiB and most of them are already constant or near constant, which every format handles. Assume parity.

## Cross column rules, one hit and one miss

Document 04 proposes storing a column as a function of another when one determines the other. Both of hits' obvious candidates were tested and the results are worth recording because one of them killed the headline example.

**URL does not determine URLHash.** 804,392 distinct URL values have more than one URLHash, which is 4.4 percent of URL's groups. The exact rule does not fire. Whatever `URLHash` is a hash of, it is not the string stored in `URL`, or the stored string is truncated where the hash was not. So the 800 MB saving I expected from that pair is not there.

That is worth more than the saving was. A rule that fires 95.6 percent of the time is a different kind of rule, and the design has to either carry an exception list or not make the claim. An exception list over 804,392 groups is affordable in the metadata but the number of rows in those groups is not measured and could be anything, so the honest position is that the near miss variant is an open question and the exact variant is worth keeping because it costs nothing when it does not apply.

**LocalEventTime is EventTime plus a small number.** Measured, `LocalEventTime - EventTime` ranges from -172,508 to +172,239, which fits in 19 bits against the column's own 32. It is a timezone offset, so the number of distinct deltas is probably tens rather than hundreds of thousands, which would make it a handful of bits a row. Storing LocalEventTime as a delta against EventTime takes it from 385 MiB to somewhere under 100.

**ClientEventTime is not.** Measured, `ClientEventTime - EventTime` ranges from -1,343,759,580 to +768,620,273, because client clocks are wrong in both directions by years. The rule fails and the column stays as it is.

So the generalisation worth making is not functional determination, which fired zero times out of two. It is **a column whose difference from another column has a much narrower range than its own**, which fired once out of two and is detectable from two numbers the directory already holds.

**EventTime itself.** The whole column spans 1372708800 to 1375300799, which is thirty days, so 22 bits covers the entire table and a per block frame of reference covers far less. Parquet stores it in 384 MiB, which is 32 bits a row, so it is doing nothing. Call it 16 bits a block conservatively and it goes to 190 MiB.

Timestamps together: 384 to 190, 385 to 95, and ClientEventTime unchanged at 394. 484 MiB saved.

## The total

Two columns, because the encoder gap is the difference between a design that misses and a design that nearly works, and both are worth seeing.

The five string columns, dictionary body at the measured ratio plus a code stream credited for its empty runs:

| column | body today | body at ZSTD parity | codes | v2 today | v2 at parity | Parquet Snappy |
|---|---|---|---|---|---|---|
| URL | 748.3 | 618.8 | 297.8 | 1046.1 | 916.6 | 2530 |
| Title | 299.7 | 279.4 | 243.4 | 543.1 | 522.8 | 2319 |
| Referer | 854.8 | 691.2 | 241.5 | 1096.3 | 932.7 | 2144 |
| OriginalURL | 590.2 | 468.7 | 48.7 | 638.9 | 517.4 | 1412 |
| SearchPhrase | 101.2 | 100.6 | 36.1 | 137.3 | 136.7 | 356 |
| **total** | **2594.2** | **2158.7** | **867.5** | **3461.7** | **3026.2** | **8761** |

So the five columns save 5299 MiB with the encoder we have and 5735 MiB with an encoder that matches ZSTD on a sorted dictionary. The first draft claimed 6544 and the first measurement said 3864.

| where | saved today | saved at parity |
|---|---|---|
| the five string columns | 5299 | 5735 |
| EventTime and LocalEventTime | 484 | 484 |
| everything else | 0 | 0 |
| **total saved** | **5783** | **6219** |

14091 MiB becomes 8308 MiB today, which is **1.70 times**, and 7872 MiB at encoder parity, which is **1.79 times**. Neither reaches two, and the distance between them is now 436 MiB where it was 1871.

Against Parquet given its best settings, which is the comparison the README actually asks for and which puts the same data in about 8763 MiB, the design is **1.05 times today** and **1.11 times at parity**. It was 0.90 times, meaning larger than Parquet, before the matcher landed.

That is the honest state of the size case and it is a long way from where the first draft put it.

## What has to change for this to reach the target

Three things, in the order of how much they are worth. The first has mostly happened since this section was written and the numbers below are what is left of it.

**The encoder has to stop losing to ZSTD on its own best case, and it has mostly stopped.** When this was written we got 2.6 times on a sorted URL dictionary where ZSTD got 5.33, and closing the whole gap was worth 1871 MiB. PR #576 added a match finder and took URL to 4.3 and OriginalURL to 5.4, which banked 1435 of that 1871. What is left is 436 MiB, most of it on URL and Referer, and it is no longer the largest item on this list. The diagnosis in this document was right about the cause: the suffixes after front coding had nowhere to go that could reach further than the adjacent value, and giving them somewhere was the whole fix.

**The 4167 MiB this document gives up on has to be looked at rather than assumed, and it is now the largest item.** The high cardinality integers are 2889 MiB and the long tail is 1278, and both are written off above at parity with Parquet. Parity with Snappy is not parity with a good encoder, and the same 1.608 times that ZSTD found across the whole file is presumably sitting in those columns too. Nobody has put `cargo xtask encode` at them and read the per column ratios, which is an afternoon.

**The empty run assumption in the code stream has to be measured.** 867 MiB of the total is a credit taken on the belief that OriginalURL's 83 percent empty rows arrive in runs. If they are interleaved the credit is worth much less and the total moves the wrong way by a few hundred megabytes.

If all three land the design reaches two times. The first has largely landed and took the design from 1.45 times to 1.70, so the remaining two carry the rest. The first draft's conclusion that the target was already reached with conservative assumptions was wrong, and it was wrong because it sized a dictionary body with the wrong average and then guessed a compression ratio it could have measured in an afternoon.

## What the first draft got wrong

Recorded rather than quietly edited out, because the pattern is more useful than the corrections.

**Dictionary bodies were sized with the average value length per row instead of per distinct value.** Off by 2.85 times across the five columns. This is the one that mattered and it is arithmetic, not judgement.

**The front coding ratio was guessed at five and measured 2.6 with the encoder we had.** Guessed from the literature, where the numbers are real and are reported for encoders that are not the one we have. The guess was closer to right than the encoder was: with a matcher the same measurement is 4.3, so the literature was describing a cascade that included something ours was missing, and the honest reading of this mistake is that a number taken from a paper is a claim about a design and not about your code.

**The saving was called conservative.** It was not conservative. It took an optimistic compression ratio on an undersized body and then listed the things it had left out, which reads as caution and was not.

**The target was quoted against the wrong Parquet.** 14.8 GB is Snappy, which is Parquet's default, and the README asks for Parquet at its best. Two different targets differing by 1.608 times were being used in the same sentence.
