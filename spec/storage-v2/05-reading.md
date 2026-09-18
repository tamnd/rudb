# Reading

A scan's job is to read as little as the query allows and to never have a moment where the pool is waiting on one thread. Those are two separate requirements and the layout in document 02 is what makes both of them achievable, but the layout does not achieve them on its own.

## The order a scan does things

Seven steps, and the point of listing them is that steps 1 through 4 read no column data at all.

**1. Read the directory runs.** One request per column the query mentions, each of `blocks * 64` bytes, all issued together. For hits at a hundred million rows and the five columns q37 needs that is five requests of 52 KB. These are the only thing the scan has to have before it can decide anything, and they arrive in one round trip.

**2. Prune blocks from the directory.** For each predicate, compare against the per block minimum, maximum and null count. `CounterID = 62` against a table clustered by time prunes nothing, because counter 62 appears throughout. `EventDate >= '2013-07-01' AND EventDate <= '2013-07-31'` against the same table prunes most of it, because the table is in time order. The two together are q37's predicate and the date half does the work.

**3. Answer whole blocks from the directory where the query allows it.** A block whose minimum equals its maximum answers a filter on that column without being read, either keeping all of it or none. A `count(*)` with no filter is answered from the row counts. A `sum` or a `min` or a `max` over an unpruned block is answered from the directory's own fields. q1, q7 and part of q3 finish here having read 52 KB.

**4. Read the globals for the columns that have them, and evaluate what can be evaluated once.** The dictionary and the rank permutation are cached for the life of the process, so this is free after the first query. Then every predicate over a dictionary column is evaluated against the dictionary rather than against the rows: `URL LIKE '%google%'` runs once per distinct URL and produces a bit per code. This is note 11's finding 2 and it is the largest single per row saving in the design. A predicate that matches nothing in the dictionary ends the query here.

**5. Read the small columns whole.** Document 02's rule: a column under 64 MiB stored is one request. For hits that covers every integer predicate column in the whole suite. The filter is now fully evaluated against the whole table, in memory, from a few tens of megabytes, before a byte of `URL` or `Title` has been touched.

**6. Read the surviving blocks of the large columns.** Only the blocks step 2 kept and step 5 did not empty, and within a block only the tiles the tile index says can contain a match. The requests are sorted by offset before being issued so the queue sees a sequential pattern, and they are issued as one batch rather than one at a time.

**7. Decode, keeping the form.** A dictionary column comes up as a dictionary vector with the table's global dictionary attached, not as strings. A run of equal values comes up as a run. Nothing is flattened, because flattening is a decision for whoever is reading the answer and not for the scan, which is the rule #570 established one layer up.

## Why nothing blocks

There are four places a scan could serialise and the design has an answer for each.

**The metadata read.** One round trip for every column at once, in step 1, rather than a footer parse whose cost is the whole file's metadata. This is the thing Parquet cannot do and it is why document 02 puts the directory in fixed width column major form.

**The globals read.** Issued together with step 1 rather than discovered when the first block needs them, and cached across queries so it happens once per process per column.

**Deciding which blocks to read.** Steps 2 and 3 are a loop over a fixed width array and they parallelise trivially, but for any realistic block count they are microseconds on one thread and not worth splitting.

**The work assignment.** A block is a morsel. The number of morsels is `rows / 122,880` and it is a property of the data rather than of whoever wrote the file, which is issue #511 fixed by the format rather than worked around in the scheduler. Below one block, a block's 120 tiles are the morsels instead, so a table of ten thousand rows still spreads across threads. Threads take morsels from a shared counter and never wait for each other, and a morsel that is not yet in memory is skipped and retried rather than blocked on, so a slow read never idles a thread that has other work.

## Reading Parquet with the same machinery

Nothing above needs the file to be in our format, and most of it can be done over Parquet too, which matters because note 11 says we are behind on Parquet today and the ten times has to be visible there as well.

What carries over: reading the filter columns first, evaluating the predicate over the dictionary page once, skipping a page whose dictionary matched nothing, lazy skips that coalesce, and reading column chunks in offset order. Those are items 2 through 4 of the work list in note 11 and they are being built against the Parquet reader first, which means the ideas are tested against a format we cannot change before they are baked into one we can.

What does not carry over: the columnar directory, the global dictionary, the rank permutation and the functional dependency rule. Those are format, and they are the part of this design that a Parquet reader can never have.

## What a cold read costs

Every number in note 11 is warm, because the caveats say so and because both engines cache the file's bytes. The design's cold behaviour is different and it is worth stating separately.

A cold query in this format reads the directory runs for its columns, the globals for its columns, and the surviving blocks. For q37 over hits that is 260 KB of directory, whatever `URL`'s dictionary costs, and the blocks of `URL` that survived a one month date range out of a dataset covering about a month, so most of them. A cold query in Parquet reads the whole footer first, measured at 2.33 MB for hits, and then the same column chunks in eight hundred scattered pieces rather than fifty one contiguous ones. Nine times the metadata and sixteen times the request count, both paid before the first row.

That difference is largest exactly where it is least often measured, which is the first query against a file nobody has touched. Document 07 says to measure it.
