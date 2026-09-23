# A Parquet file read once

## Why this document exists

Document 25 settled what the Parquet quadrant can be worth to any design that decodes the file on every query. DuckDB's scan of one column, with nothing grouped, is more than four times the whole ten times budget on the query it measured, and rudb's scan is more than six. No aggregate, no hash table and no operator changes that, because the cost is in turning the file's bytes into values and a query that reads the file has to pay it.

That argument has a premise, which is that every query reads the file. This document drops the premise. A Parquet file does not change between two reads of it unless somebody writes it, and the benchmark contract in document 3 already runs every query once cold and then at least five times hot against the same file. The work of decoding it is the same work each time and nothing requires it to be done more than once.

So the Parquet quadrant gets what the native quadrant has: the file is decoded once, into rudb's own format, and every read after that is a native read. This document states what that mirror is, when it is trusted, when it is built, what it costs and where the cost is reported. It changes no query and no answer.

## What a mirror is

A mirror is a native rudb file holding exactly one table, which is exactly what `read_parquet` returns for one Parquet file under one set of options: the same columns with the same names and types, the same rows in the same order. It is what `CREATE TABLE t AS SELECT * FROM read_parquet(path, options)` writes, and it is written by that statement, so there is no second decoder whose answers could drift from the first.

A query that reads a mirrored file binds to the mirror's table in place of the table function, and from there it is a native query in every respect: the zones, the frequency synopses, the exact distinct counts and the global string codes of documents 9 through 32 all apply. A view over the file, which is how the contract spells the Parquet engines, is a view over the mirror.

Only the reads a mirror can answer identically are mirrored:

- one regular file, named directly or by a pattern that matches exactly one file, whose path is UTF-8;
- no `file_row_number`, since it adds a column that is not in the file;
- `binary_as_string` either way, because it changes a column's type and not its bytes, and it is part of the mirror's key.

Only a query asks for a mirror. `EXPLAIN`, a load and any other statement read the file as they do today, though a mirror an earlier query registered is used by any of them. Anything else reads the file as it does today.

## When a mirror is trusted

A mirror is found by a key and there is no other record of it. The key is a 128 bit hash of:

- the file's canonical path;
- its device, inode, size and modification time to the nanosecond;
- the hash of its footer, which is the last eight bytes and the metadata block they point at;
- the options that change what is returned, which today is only `binary_as_string`;
- the native format version, so a build that cannot read an older mirror never opens one.

The mirror is named by its key. A mirror file whose name matches is valid by construction, and a file that changed in any of those respects has a different key and simply has no mirror. Nothing is ever invalidated, compared or repaired; a stale mirror is one nobody asks for.

The footer is in the key because the stat fields alone can be defeated by a tool that restores a modification time, and a Parquet writer cannot produce the same footer for different content: it records every row group's offsets, sizes and statistics. Reading it costs two small reads, and the bind already makes both of them to learn the file's schema.

## When a mirror is built

The first time a query binds a mirrorable read of a file that the process holds no mirror for, the binder records that it wants one and binds the file as it would have. The database then looks for a mirror file with the key, builds it if there is none, registers it beside the catalog and binds the query once more, which now finds it. If the build fails for any reason the query runs against the file, and the process does not try that file again. The build is the statement above, into a temporary file beside where the mirror goes, published by rename, so a crash or a concurrent reader never sees half a mirror and two processes building the same one both succeed and one of them wins.

The build happens in the query's own process, inline, on purpose. Building it in a detached child would make the first query look cheap and hide the load in a process nobody measures, which is precisely the kind of accounting document 21 exists to refuse. The first run of a query over a new file pays the whole load, and it shows.

A file below 1,048,576 rows is not mirrored. Document 32 measured the floor of a native query at about 9.4 MiB, and on a small file the decode is cheaper than anything the mirror saves. `RUDB_MIRROR_ROWS` changes the threshold, which is how the tests mirror their small fixtures.

## Where a mirror lives, and how to say no

In `RUDB_MIRROR_DIR` if it is set and in `$XDG_CACHE_HOME/rudb/mirror` or `~/.cache/rudb/mirror` otherwise. With none of those set nothing is mirrored. A build that runs out of space fails like any other, removes its temporary file and leaves the read to the file.

`RUDB_PARQUET_MIRROR=0` turns the whole mechanism off for a process, and `Config::with_parquet_mirror(false)` does it for one database opened through the embedding API. Off means no key is computed, no directory is touched and every read is today's read.

A mirror is never written into a user's own database file and never appears in its catalog. It is held beside the catalog's schemas, found by the binder under its key, and dropped with the session.

## What it costs, and where that is reported

The contract already has both quantities and names them. A mirror is a native load of the file, so the first run of a query over a new file reports the load's time and the load's peak resident memory, which is larger than a query's. The mirror's bytes on disk are reported the way document 3 reports a native file's size, so a query that is fast because it expanded the data on disk is visible as one.

The hot runs are where the ten times target is assessed, as it is for every other quadrant, because the contract's figure is the median of the hot repetitions. The break even count the contract asks for is the number of reads after which the mirror's one load is repaid by the per query saving, and it is reported for the suite as a whole.

## What the Parquet quadrant becomes

With a mirror, a hot Parquet query is a native query plus one bind time check. The Parquet quadrant's ceiling stops being document 25's decode arithmetic and becomes the native quadrant's, measured against DuckDB reading Parquet, which pays the decode on every run. Every query that clears the native quadrant clears the Parquet one by a wider margin, and every query that does not clear the native quadrant does not clear this one either. The two quadrants now share one list of work.

The view in the contract still converts the four time columns on every row of every query, where the native quadrant's table stores them converted. That is a cost the mirror does not remove, and removing it is a question about pushing a monotone expression through zones and synopses, which is a general property of the optimizer and not of this mechanism.

## What this does not claim

It does not claim anything about a file read once. A process that reads a Parquet file one time and never again pays the load and gets nothing back, and that is what the first run of the suite measures and reports.

It does not claim a file that changes often is served well. Every change is a new key and a new load. A directory of files being appended to is a case for a different design, and patterns over many files are not mirrored at all.

It does not claim the mirror is free on disk. It is a native file of the same data, and document 31's full width file at ten million rows is what it will be at ten million rows.

It does not claim the numbers. They are measured in the document that follows the implementation, against the baseline run of all forty three queries on both engines reading the same ten million row Parquet file, which this document was written beside.
