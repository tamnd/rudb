# Where the repeats are

## Why this document exists

Document 23 measured the Parquet quadrant, found rudb 2.13 times behind DuckDB on processor time, and traced almost all of the gap to one flag: a Parquet dictionary belongs to one row group, so the reader cannot mark it stable, and every fast path in the group operator is gated on stable. It then priced the fix by counting string hashes, 81.0 million against 21.3 million, and projected two to four times on the grouping. It said in as many words that this was a model rather than a measurement.

A profile of that query says hashing is 2.66% of it. The projection was right about the size and wrong about the mechanism, which is the same error document 20 made and the sixth error class this series collected: taking one layer of a cost for the whole of it.

This document profiles the query, finds what the grouping is actually made of, measures where the repeated values sit, changes one constant, measures the change, and finds it worth nothing. Chasing that null result is what the document is for. The reason the constant bought nothing is that `hits.parquet` does not hand this column over as codes: the writer's dictionary page fills after about ten thousand of the row group's sixty thousand values and the rest of the chunk is written as plain bytes. Seven rows in eight arrive as strings. Document 23's proposal was to intern per row group dictionaries into one code space, and for this column, which is the column it was proposed for, most of the values are never in a dictionary at all.

## The profile

`SELECT Referer, COUNT(*) FROM read_parquet('hits.parquet') WHERE Referer <> '' GROUP BY Referer ORDER BY COUNT(*) DESC LIMIT 10`, the query document 23 measured, under `perf record` at 199 Hz. 26,000 samples over 287 billion cycles, on the same host at load average 24 over eight cores.

| share | symbol | what it is |
| ---: | --- | --- |
| 12.37% | `rudb_exec::table::Table::probe_at` | finding a row's group |
| 9.10% | `__memcmp_avx2_movbe` | comparing the key against the one the table stored |
| 5.79% | `smp_call_function_many_cond` | kernel, cross processor page table work |
| 4.95% | `clear_page_rep` | kernel, handing out zeroed pages |
| 4.66% | `rudb_exec::group::Aggregate::fold` | the row loop itself |
| 4.58% | `rudb_compress::snappy::decompress_into` | reading the file |
| 3.44% | `__memmove_avx_unaligned` | copying |
| 3.39% | `rudb_common::utf8::valid` | checking the dictionary's strings are text |
| 2.67% | `__memmove_avx_unaligned_erms` | copying |
| 2.66% | `rudb_exec::table::hash` | hashing the key |
| 1.98% | `rudb_exec::table::Table::insert` | starting a group |
| 1.67% | `__memset_avx2_unaligned_erms` | clearing |
| 1.58% | `zap_pte_range` | kernel, giving pages back |
| 1.47% | `rudb_exec::group::Aggregate::split` | dividing rows between partitions |
| 1.13% | `asm_exc_page_fault` | kernel, taking a page |
| 1.07% | `rudb_parquet::values::plain` | decoding a page |
| 0.99% | `rudb_vector::StringColumn::push_in_place` | putting a string in a column |

Nothing else reaches 1%, and everything above a tenth of a percent adds to 78%.

## What the grouping is made of

The probe and the comparison behind it are 21.5% of the query between them. The hash is 2.66%, the insert is 1.98%, and the row loop that drives all three is 4.66%. Another 15% is the kernel giving out pages, clearing them, taking them back and shooting down the other processors' page tables, which is what a table of 19,720,796 string keys costs to build in a process that is also holding a Parquet reader's buffers.

So the grouping is a lookup, and the lookup is a trip to memory and a comparison of bytes at the end of it. Document 23's proposal reduces the number of lookups, which is the right change, but it reduces the hashes as a side effect rather than as the mechanism. Had the hashing been the whole of it, the fix would have been worth the 2.66% and this series would have built it and found out afterwards.

The comparison is worth a sentence of its own. 9.10% of the query is `memcmp` against the key the table already holds, which happens on every row that finds its group, and is the price of a hash table whose keys are strings and whose contents do not fit in any cache.

## Where the repeats are

Every one of those lookups is a row asking a question some earlier row already asked. How much earlier decides whether anything can be done about it, because the structure that remembers an answer has a scope, and a value that repeats outside that scope repeats for nothing.

`hits.parquet` holds `Referer` in 226 row groups averaging 442,467 rows, and rudb cuts a row group into morsels of 32,768 rows and hands each morsel to a thread. Distinct values, measured over windows of each of those sizes, at the front of the file and at the fifty millionth row:

| window | rows | distinct | rows per distinct |
| --- | ---: | ---: | ---: |
| morsel, at the front | 32,768 | 6,549 | 5.00 |
| morsel, at 50,000,000 | 32,768 | 10,544 | 3.11 |
| row group sized, at the front | 442,467 | 60,358 | 7.33 |
| row group sized, at 50,000,000 | 442,467 | 127,927 | 3.46 |
| whole file | 81,032,736 | 19,720,796 | 4.11 |

The two middle rows are windows of the mean row group's size and not row groups. The real ones vary: 450,560 rows in the first, 344,064 in the fifty first, 224,831 in the hundred and fourteenth, 263,704 in the last. Document 23 called them 442,467 rows each, which is the mean and not the shape, and the distinct counts above are close enough either way that nothing in either document turns on it.

The morsel figure is the one that matters and it was the surprise. A uniform model of 128,000 values scattered over 442,467 rows predicts about 28,000 distinct in a morsel of 32,768, which would leave a morsel almost all first occurrences and nothing worth remembering. The file says 10,544. Referers arrive clustered, so a thread holding one morsel already sees two rows in three that it has seen before, which is nearly the whole of the redundancy in the row group and most of the redundancy in the file.

That kills three designs and leaves one. Interning each row group's dictionary into a file wide code space, which is what document 23 proposed, would reach 4.11 and costs a canonical dictionary of twenty million strings and a change to what dictionary identity means. Hashing each dictionary once and gathering, which `table.rs` considers and rejects in a comment, costs a pass over 128,000 entries to serve 32,768 rows and loses. Remembering per chunk loses for the same reason at a smaller size. Remembering per morsel, against the dictionary the morsel arrived under, reaches 3.11 for the price of a table indexed by code.

The last of those is the one this document built, and two sections below it turns out to reach an eighth of the rows rather than all of them, because the phrase "the dictionary the morsel arrived under" assumes a dictionary that for most of this column is not there. The redundancy measured in the table above is real and it is sitting in a morsel. What is missing is a code to index it by.

## The map that was already there

That table exists. `crate::table::coded` turns a chunk whose whole key is a few codes into one index per row, and the caller keeps one slot per combination beside it and throws it away when the dictionaries change. It was built for TPC-H q1, where `l_returnflag` and `l_linestatus` have three values and two between them and six million rows ask for one of six answers.

Its bound was `COMBOS`, two thousand and forty eight places, and a `Referer` dictionary is about ten thousand entries. Under that bound the query above hashed, probed and compared once a row, including on the pages that did arrive as codes.

The bound is about the map rather than about the key, and it was one number covering two different things. A map over several key columns is their product and most of it is empty, so a few thousand places is the right size for it. A map over one column is not a product: every place in it is a value that column's dictionary holds, so it is never larger than the distinct values the reader could hand over, and a Parquet writer bounds that at the size of a dictionary page.

So the bound is now two. `COMBOS` is unchanged at 2,048 and still covers the product of several columns. `WIDE_COMBOS` is 262,144 and covers one column on its own, which is a megabyte of `u32` per thread and fits any Parquet column chunk dictionary this file has with a great deal of room to spare.

Two other things had to move with it. A dictionary is asked once per chunk whether any row under it can be null, and the answer used to be a pass over the dictionary, which at two thousand entries is nothing and at ten thousand is a pass over the dictionary for every chunk of a few thousand rows. The cheap answer, which is the two validities saying they have no nulls anywhere, now comes first, and a wide dictionary that cannot be answered cheaply gives the map up rather than paying for the scan. The packed integer path keeps the old bound, because a packed run's span is two to the width rather than the values it holds, and nothing in these measurements says anything about that case.

## What it measures

Six runs of the query document 23 measured, alternating the two binaries, both built from the same tree at the same profile, on the same host. The host was busy and got quieter as the runs went on, which is why the first four are slower than the last two and why the pair to read is the last one.

| run | binary | wall | CPU | peak resident |
| --- | --- | ---: | ---: | ---: |
| 1 | before | 92.14 s | 164.05 s | 4,022 MiB |
| 1 | after | 74.03 s | 166.56 s | 4,094 MiB |
| 2 | before | 83.23 s | 190.26 s | 3,949 MiB |
| 2 | after | 77.61 s | 162.42 s | 4,028 MiB |
| 3 | before | 45.17 s | 147.03 s | 3,924 MiB |
| 3 | after | 48.35 s | 148.62 s | 3,976 MiB |

The best of each is 147.03 seconds of processor time before and 148.62 after, which is one percent apart and is no change. The answer is the same ten rows with the same counts. So the constant was widened, the map now takes a key it used to refuse, and the query did not get faster.

## Why it did not

The file does not hand `Referer` over as codes. Every one of the 226 column chunks is written in `PLAIN_DICTIONARY, PLAIN, RLE, PLAIN`, which is a writer that started dictionary encoding and gave up partway through each chunk. Parquet writers cap the dictionary page, a megabyte by default, and fall back to plain data pages for the rest of the chunk once it fills.

`Referer` fills it early. The first row group holds 450,560 rows and 61,303 distinct values between them, and those values are 115 bytes long on average, so writing them all would take 7.0 MB against the megabyte allowed. Taking the distinct values in the order they first appear and stopping at a megabyte gives 9,857 of them, and the 9,857th first appears at row 52,705. The dictionary page on disk is 533,704 bytes compressed, which is the right size for that. So about the first eighth of the row group arrives as codes over a ten thousand entry dictionary and the other seven eighths arrive as flat strings, and the dictionary page sizes across the file, 533,704 and 554,211 and 582,844 and 560,425 bytes in the four row groups sampled, say the same thing happens in all of them.

That puts a ceiling on the change of about an eighth of the rows, and those are the rows at the front of a row group where the redundancy is thinnest, 5.00 rows per distinct rather than 3.11. An eighth of the rows saving four lookups in five is a tenth of the lookups, and the lookups are 21.5% of the query, so the change is worth about two percent of it. Two percent is under what this host resolves, and the measurement above is what two percent looks like from here.

This is the thing to carry forward, and it is larger than the constant that prompted it. Document 23 proposed interning each row group's dictionary into a file wide code space. For this column there is no row group dictionary to intern for seven rows in eight. The proposal was priced against 21.3 million dictionary entries; the file only has about 2.2 million, because a dictionary that stops at ten thousand entries per row group cannot hold the other fifty thousand values that row group contains. Those fifty thousand are on disk as bytes and arrive as bytes, and no code space reaches them.

So the Parquet quadrant's grouping has to be fast over flat strings. A high cardinality string column is exactly the case where a writer abandons its dictionary, which is to say the case where the dictionary machinery would pay off is the case where the file declines to provide one. That is a property of the format's writers and not of this reader, and it applies to DuckDB reading the same file, which is part of why DuckDB's 87.85 seconds on it is so much worse than rudb's 9.54 on the native copy.

The error underneath this is a new one for the series, which makes it the tenth. Documents 23 and the first draft of this one reasoned about how a column is encoded on disk from what the column contains, and the two are not the same thing: `Referer` is a repetitive string column, so both documents assumed it is a dictionary on disk, and the writer had given up on that idea two hundred and twenty six times. The file had the answer in its metadata the whole time and neither document read it. Call it the encoding error: taking a column's logical shape for its physical one. The cure is four lines of `parquet_metadata` and it costs a second to run.

## What this document does not claim

The measurement above is one column of one file. `Referer` is clustered, and a column whose repeats are spread evenly over a row group would get almost nothing out of a morsel wide map. Which shape a column has is a property of the data, which encoding it is written in is a property of the writer, and nothing here measures a second column of either.

It does not claim the change is worthless, only that it is worth nothing here. The bound it removed was wrong on its own terms: a map over one column can never hold more places than that column's dictionary has entries, so bounding it by the same number that bounds a product of several columns was a category mistake, and it refused every dictionary between two thousand and a quarter of a million entries. Files written with a larger dictionary page, and the native format's own per column dictionaries, are both in that range. The measurement says the wider bound costs nothing, not that it never pays.

It does not claim the map is free. It is a megabyte per thread, it is not charged against the memory budget, and it is cleared once per dictionary per thread.

It does not say what to do instead. The grouping is a probe and a byte comparison over flat strings, seven rows in eight, and nothing in this document makes that faster. What it establishes is that the next attempt has to, and document 25 measures what that attempt can be worth before anybody makes it.

The profile is from a host at load average 24, and the kernel share in it is the part most likely to be inflated by that. The shares of one symbol against another inside the process are what the argument uses, and the 21.5% the probe and the comparison hold between them is the number this document stands on.
