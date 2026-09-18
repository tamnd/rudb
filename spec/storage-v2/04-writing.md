# Writing

The design in document 02 has exactly one structure that every thread has to agree about, which is the per column dictionary in the globals region. Everything else is per block and embarrassingly parallel. So this document is almost entirely about that one structure, because it is the only thing standing between the write path and the scaling note 10 already measured for the encoder on its own, which was 13.5 MiB/s a thread and 157.8 MiB/s over thirty two with the fall off starting at eight because the bench host has eight performance cores.

## The rule the write path has to obey

**No phase where every thread waits for one thread.** That is the whole requirement and it is worth stating as a rule because it is easy to violate by accident. Note 11's root cause B was exactly this failure one layer up: a load that was parallel through the scan and the encode and then serial in the one thread draining the query, which cost a factor of three. A global dictionary built by one thread at the end would do the same thing to the writer.

## Codes are not sorted, and that is what makes this work

The obvious way to build a global dictionary is to collect every distinct value, sort them, and assign codes in sorted order so that comparing codes compares values. That is a lovely property and it forces a barrier: no code can be assigned until every value has been seen.

So the design separates the two things.

**A code is an identity.** It is assigned on first sight, it is dense, it never changes, and it carries no meaning beyond the promise that equal codes mean equal values and different codes mean different values. That promise is all invariant 5 needs for grouping and joining, and it is assignable in parallel.

**A rank is an order.** It is a separate array, `rank[code]`, giving the position of that code's value in sorted value order. It is computed after the fact from a sort of the dictionary, it is rebuilt when the dictionary grows, and it is what makes comparison and ordering work on codes. For a column with ten million distinct values it is 40 MB, read once and cached, and it is the thing that turns `ORDER BY URL` into `ORDER BY rank[code]`.

Splitting them costs one array lookup in the operators that need order and it buys the entire write path its parallelism. That is a good trade and it is the single most important decision in this document.

## How the order is written down

The rank array is not one flat array of codes at the front of the dictionary page, and the reason is measured rather than assumed. On ClickBench at a million rows, `URL` has five hundred and sixteen thousand distinct values. Four bytes a code is two megabytes. Putting that in the part of the page a reader checksums the moment the column is first touched made nine of the forty three queries between ten and fifty percent slower, because every query that reads a string column paid for a search that most of them never make.

Two things follow from that, and both are general rather than tailored to a query.

**The order is read a block at a time and only when something searches it.** Five hundred and twelve entries a block, one checksum per block in the index, the blocks themselves outside the checksummed region. A binary search over half a million entries makes nineteen probes, the first ten land in ten different blocks and the last nine land in the block that holds the answer, so a whole search reads about sixty six kilobytes of a two megabyte array. Nothing that does not search reads any of it.

**The first eight bytes of each value are written beside its code, in rank order.** This is the poor man's normalised key that sorting has used for decades, and it is what a paper on order preserving dictionaries means by a search that does not touch the payload. A probe compares two integers. It only reads the value when the two agree on their first eight bytes, which for a search that ends in a hit is once, and for a search that ends in a miss is usually never.

The second one is what makes the search worth doing at all. Without it, the nineteen values a binary search lands on are scattered across a payload of thirty megabytes, so nineteen probes is nineteen different blocks of the file, which is more than a filter on a selective query would have read by asking each value it actually met. With it, a search of a half million value dictionary reads one value.

Eight bytes an entry is a real cost in file size and the tradeoff is deliberate. A block holds its heads first and then its codes rather than pairing them, because the head is asked for at every probe and the code about once a search.

## The dictionary during a write

A sharded append only map, sharded by the high bits of the value's hash, with one lock per shard. Thirty two writer threads over two hundred and fifty six shards means contention is rare, and most operations are lookups of a value already present rather than inserts, because invariant 6 says the whole point is that values repeat.

A shard does not own a fixed range of the code space. It draws a run of codes from one global counter, sixty five thousand at a time, and hands them out with a local increment. Two reasons for that shape rather than fixed interleaved ranges. The global counter is touched once per sixty five thousand new values, so it is not contention. And a writer thread's new codes come from one or two contiguous runs rather than from two hundred and fifty six scattered ones, which is what makes the code stream compressible per block. Document 03 explains why that matters: a block's codes being near each other is the difference between storing twenty five bits a row and storing a frame of reference plus a much narrower integer, and it is worth more than the dictionary body on some columns.

Codes are therefore in something close to first arrival order across the table rather than in sorted order, which nobody needs, since order comes from `rank`.

There is no phase two. A block is written with its final codes and never rewritten.

The one thing that happens at the end is the sort that produces `rank`, and it is a sort of the distinct values rather than of the rows, so for `URL` on hits it is a sort of tens of millions of strings rather than of a hundred million. It parallelises the way any sort does, it happens once per load rather than once per block, and it is the only part of the writer that is not per block work.

## When a dictionary is the wrong answer

A column with a distinct value per row gets a dictionary the same size as the column plus a code stream on top, which is strictly worse than not having one. A UUID column, a free text comment, a high precision timestamp.

The distinct ratio is the obvious rule and it is not the right one. hits `UserID` has 17.6 million distinct values in a hundred million rows, so a ratio of 0.18, which passes any sensible threshold, and a dictionary makes it 1.7 times larger than Parquet already stores it. The reason is arithmetic rather than skew: a dictionary pays when a value is wider than its code, and an eight byte integer against a twenty five bit code is not. Document 03 has that worked out.

So the rule is the comparison itself. Build a dictionary when `distinct * value_width + rows * code_width` is less than what the column costs without one, and for a fixed width type both sides are exact. For a variable width type the value width is the average length, which is measured rather than guessed, so it is still arithmetic.

The writer decides from data rather than from the type. It runs the first super block with a dictionary, computes both sides of that comparison from what it saw, and if the dictionary loses it abandons it for that column and re encodes that one super block without it. The waste is bounded at one super block of one column and the decision is made from 1,966,080 rows, which is enough to be right about.

Parquet's own writer gets this wrong in a way that is worth noting because it is free money. `WatchID` in hits is 99,997,493 distinct values in 99,997,497 rows, and Parquet still spends 226 MiB of dictionary pages on it before falling back to plain. That is 1.6 percent of the whole file spent on a dictionary that can never match twice.

The abandoned columns fall back to FSST for strings and to the integer encoders for everything else, which is what `rudb-encoding` already does and what note 10 already measures.

## A column stored as a map over another column's codes

This is the general form of a trick that is worth a lot on wide denormalised tables, which is most analytics tables, and it costs nothing when it does not apply.

If column B is functionally determined by column A, meaning every row with the same A has the same B, then B needs one value per distinct A rather than one per row. Store B as an array indexed by A's code and store no per row data for B at all.

The obvious candidate on hits was `URL` determining `URLHash`, and it was measured and it is false. 804,392 of URL's 18,342,019 distinct values have more than one `URLHash`, so the rule fires on 95.6 percent of the column and 95.6 percent is not a functional dependency. Document 03 has the numbers and what they cost the size case.

What did fire, on the same dataset, is the weaker and more useful version: **a column whose difference from another column has a much narrower range than its own.** `LocalEventTime - EventTime` spans 344,747, which is 19 bits against the column's 32, because it is a timezone offset. `ClientEventTime - EventTime` spans two billion, because client clocks are wrong, and the rule correctly declines. One hit and one miss out of two candidates, which is a better record than the exact rule's zero out of two.

The weaker rule is also far cheaper to detect. It needs two numbers, the minimum and maximum of the difference, and those are computed in the same pass that computes the per block minimum and maximum the directory already holds. There is no map, no sample and no verify, and a candidate pair is rejected by an arithmetic comparison rather than by finding a counterexample.

The detection is a sample followed by a verify. For each candidate pair, hash a sample of rows into a map from A's code to B's value and look for a disagreement. A disagreement anywhere means the rule is false and the sample finds it fast when it is common. Candidates are chosen by comparing the distinct estimates already in the directory: B can only be determined by A if B has no more distinct values than A. The verify is exact and runs during the write, since the writer is already touching every row, and a violation at any point abandons the rule and writes B normally.

This is not the same thing as knowing which hash function was used and it does not need to be. It works for `country_code` determining `country_name`, for a product id determining a product category, and for every other denormalised pair, and the format never has to have heard of the function.

**The cost is on the read side and it has to be paid honestly.** Reading B now requires reading A as well, so a query that wanted only B reads two columns instead of one. The writer only applies the rule when what B costs as a difference plus what A costs to read is less than what B costs on its own, and that is arithmetic over numbers the writer already has rather than a judgement.

## The order rows are written in

The format does not require a sort key and it benefits from one. The directory's per block minimum and maximum are only useful for skipping when the values are clustered, and a table written in random order gets zone maps that never prune.

Both benchmarks already arrive clustered. ClickBench's hits is in `EventTime` order, which is why the `EventDate` range in q37 through q43 prunes. TPC-H's lineitem is in `orderkey` order and the ship dates correlate with it.

So the rule is: **preserve insertion order by default and never reorder without being asked.** A table that arrives sorted stays sorted and gets the zone maps for free. A `CREATE TABLE ... ORDER BY` clause, or a sort key recorded in the catalog, makes the writer sort, and that is a choice the user makes with knowledge of their queries rather than one the format makes for them.

Sorting is not free and the write path pays for it: a sort of the whole input before blocks can be cut, which is an external sort for anything larger than memory and which serialises the block cutting behind it. That is why it is off by default.

## Where a super block comes from

Document 02 sets a super block at sixteen blocks, so 1,966,080 rows, buffered by one thread before being emitted column major. Two shapes of input have to work.

**A bulk load from a file.** The input is a Parquet file or a CSV, it is already splittable, and each writer thread takes a range of it and owns a super block from start to finish. This is the case that matters for a load benchmark and it is fully parallel.

**A stream of chunks from a query.** `CREATE TABLE ... AS SELECT` and `INSERT ... SELECT`. Chunks arrive from the pool in whatever order the pool produces them, and the thing that turns them into blocks must not be one thread, which is the mistake #570 just fixed one layer up.

The answer for the second case is that a writer thread claims a super block's worth of row numbers up front, by an atomic add on a row counter, and then fills it from chunks it pulls itself rather than from chunks handed to it. Block boundaries are a function of row number, so two threads filling two super blocks never have to agree about anything except the counter. The rows land in a different order than a single thread would have produced, which is why the sort key is a user decision rather than an assumption.

## What this costs in memory

Sixteen blocks, so 1,966,080 rows, held per writer thread before encoding. hits is 148 bytes a row as Parquet stores it, so a super block is about 290 MB a thread, and thirty two threads is 9.3 GB, which is too much for a machine with 31 GiB that also wants to hold a dictionary.

So the super block size is a budget rather than a constant. The writer computes it from the memory budget divided by the thread count divided by the estimated row width, clamped to at least one block and at most sixteen. On a small machine it degrades to one block per super block, which is Parquet's arrangement, and the read side loses its contiguity but nothing breaks. The directory has no idea how many blocks were in a super block, because it holds an offset per block, so this is a writer decision with no format consequence.
