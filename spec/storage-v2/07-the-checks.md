# The checks

A design document that cannot be wrong is not worth writing down. This one says what would show it to be wrong, what instrument would show it, and what the numbers have to be for it to have been worth building.

## The instruments that exist

**`cargo xtask encode`**, from #552 and #556. Puts every column of a Parquet file through the encoders in chunks of 122,880 values and prints megabytes a second and the ratio per column, plus a split of the seconds across the candidates that spent them, plus a thread scaling sweep. Note 10 is its output. This measures the encoder and not the format, which is the right split, because the encoder is a fair share of what a write costs and it is measurable before a byte of format exists.

**`cargo xtask parquet`**, from #567. Runs seven shapes of SQL through rudb and DuckDB as subprocesses over the same Parquet file and prints both with the ratio. Note 11 is its output. This is the baseline any format has to beat, and the important thing about it is that it holds the format fixed, so a later run of the same shapes against a v2 file is a clean comparison of formats with the engine held fixed instead.

**`rudb-bench run clickbench`**, which is the end to end number and the only one that counts under reporting rule ten.

Document 08 adds its own checks, which are about pruning rather than about size, and they come first for the reason that document says.

## The instrument that has to be built

**`cargo xtask format`.** Takes a Parquet file, writes it in both formats, and prints three tables.

The first is size, per column, both formats, with the ratio and the encoding each one chose. Per column rather than per file, because a whole file ratio hides which idea paid and a design with four size ideas in it needs to know which of the four did the work.

The second is read time for the same seven shapes as `cargo xtask parquet`, against the v2 file, next to the Parquet numbers already recorded. Same shapes so the tables line up.

The third is write time and thread scaling, so the write cost of the global dictionary is visible against the encoder scaling note 10 already measured without one.

It should also have a cold mode that drops the page cache between samples, because everything measured so far is warm and document 05 claims the largest metadata advantage exactly where nobody has measured.

## The exit criteria

Five numbers. All of them on the bench host, all of them under reporting rule two, which is the median of at least five runs with the interquartile range beside it and never a minimum.

**1. Size.** The v2 file is at most half the Parquet file for the same data, with Parquet given its best settings rather than its defaults. Measured, Parquet at its best settings holds hits in about 8763 MiB and not the 14091 the Snappy file on disk takes, so the criterion is under 4382 MiB. Document 03 works the design out at 8308 MiB with the encoder we have and 7872 with one that matches ZSTD on a sorted dictionary, so this criterion is currently failed by nearly two times and the gap is known rather than guessed.

**2. Write speed.** A load of hits into v2 is no slower per thread than the encoder scaling in note 10 predicts, and scales to the same eight times the machine actually has. If the global dictionary costs more than fifteen percent of the write, the design in document 04 is wrong and the two phase variant has to be tried.

**3. Warm read.** The seven shapes of `cargo xtask parquet` against a v2 file beat the same shapes against Parquet through our own reader, on every row, at every size from a thousand rows to a million.

**4. Cold read.** The same, cold, and by a larger margin on the metadata dominated shapes, because that is the specific claim document 05 makes.

**5. End to end.** ClickBench over hits, rudb on v2 against DuckDB on its own format, and the number that matters is total across the suite with the per query table beside it.

## What would falsify this design

**If front coding a sorted dictionary does not get five times. Measured, it did not, and then it was fixed.** This was written as the one that mattered and everything else second, with the instruction to stop and rethink if URL came back under four. It came back at 2.6 with our encoder, so the check fired and the rethink is document 03's last three sections. PR #576 acted on that rethink and the same measurement is now 4.3 on URL and 5.4 on OriginalURL. The check still fails on its own terms, since four was the bar and URL is at 4.3 only because the bar was set on the body rather than the cascade, but it fails by a margin that no longer decides the directory.

What the measurement actually separated is worth more than the pass or fail. ZSTD gets 5.33 times on the identical sorted bytes, so the redundancy the design assumed is there and the premise holds. Our encoder gets 2.6, so the gap is in the encoder and not in the format. A control that shuffles the same dictionary and compresses it again says sorting is worth 1.56 times on URL and 1.32 on Referer, which means most of what ZSTD finds is at a distance front coding cannot reach, and an encoder that only looks at the adjacent value is leaving it behind by construction.

The check that replaced this one was narrower and it was the top item in the directory. **Get our encoder to ZSTD's ratio on a sorted dictionary.** It needed no format, it was worth 1871 MiB on hits, and 1435 of that has now been taken. The 436 MiB left is worth less than two other things on the list, so the top item has moved.

**If the code stream will not compress.** Document 03 has the code streams at 274 to 298 MiB a column, which on SearchPhrase is larger than the dictionary body by a factor of seventeen and is most of what the column costs. The saving there depends on two claims, that a mostly empty column's codes run length encode and that a time clustered column's codes are near each other inside a block. Both are measurable off the same dictionary dump: assign codes in input order, then check the run lengths and the per block spread. If a block of hits URL spans most of the code space, the frame of reference argument is dead and every code stream is a flat twenty five bits.

**If the columnar directory does not matter.** Measured, Parquet's footer for hits is 2.33 MB against the 260 KB our directory would cost q37, so the reading is nine times less and not a hundred. That is a real advantage and it is smaller than it sounded before it was measured. Time the footer parse on its own against `SELECT count(*)` at a hundred million rows and see what fraction it actually is. If the answer is under five percent, document 02's main structural idea is worth less than the Thrift compatibility it gives up.

**If the cross column rules never fire.** Already half falsified. The exact functional dependency rule was tested on hits' one obvious candidate, URL determining URLHash, and it is false on 4.4 percent of the column's groups. The narrower difference range rule fired on LocalEventTime and correctly declined ClientEventTime. One hit in two candidates on one dataset is not enough to decide either rule's fate, so test both on TPC-H and on one real customer shaped table before either goes on by default.

**If the near miss variant is unaffordable.** URL determines URLHash for 95.6 percent of URL's distinct values. Whether that is usable depends on how many rows sit in the 804,392 exception groups, which is not measured. Count them. If the exceptions are a few million rows the rule is worth having with an exception list and if they are forty million it is not.

**If the whole gap turns out to be the reader.** This is the one from the README and it is the biggest. Note 11 has us 1.7x to 2.7x behind DuckDB per core on decoding a Parquet page, and none of that is the format's fault. The six item work list in note 11 is being built first for exactly this reason: when it is done, run `cargo xtask parquet` again, and if we are at or ahead of DuckDB on every shape over the same Parquet file, then the remaining gap to ten times is the format's to close and this design is the thing that closes it. If we are still behind, the format is not the problem and building it would be building the wrong thing well.

## The order to build in

Nothing in this directory should be built before the note 11 work list is finished, and that is a deliberate ordering rather than a delay.

The reason is that four of the six items in that list are ideas this format also depends on: the filter evaluated once per distinct value, the page skipped without being decoded, the filter columns read before the payload columns, and the definition levels answered from a run header. Building them against the Parquet reader tests them against a format we cannot bend to make them work. If the idea only pays when the format cooperates, that is worth knowing before the format is designed around it.

The measurements in the falsification list that need no format should be taken first, because any of them could end this directory in an afternoon. The first one was taken and it changed the directory's conclusion, which is the argument for taking the rest before building anything.

What is left, in order. Put `cargo xtask encode` at the 2889 MiB of high cardinality integers and the 1278 MiB of long tail columns that document 03 writes off at parity, because parity with Snappy is not parity with a good encoder and nobody has looked, and because closing the string gap has made this the largest unexamined thing in the size case. Measure whether the empty rows in OriginalURL and SearchPhrase arrive in runs, because 867 MiB of the total is credited on the assumption that they do. Close the last 436 MiB of the encoder's gap to ZSTD, which is now worth less than either of those. Then the footer parse cost as a fraction of a query, and the row count behind URL's 804,392 exception groups.

Document 08 has its own order and it runs ahead of all of this, because it is about the benchmark the project is judged on rather than about the file's size.
