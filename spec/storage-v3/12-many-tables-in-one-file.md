# Many tables in one native file

## Decision

One native file holds every table in the database. The directory gains a level: a small catalog read at open that names each table, and a per table directory read on first touch that holds that table's stripes, pages, and statistics. One generation covers every table, so a checkpoint either publishes all of them or none of them.

This is native format version 22. Version 21 encodes exactly one table name followed by one set of fields and stripes, so there is no room in the grammar for a second table and no shim that could read one. A v22 reader rejects v21 and a v21 reader rejects v22.

## The gap this closes

`CHECKPOINT` on a database with two tables fails today. `persist` in `crates/rudb/src/database.rs:409` takes the first table out of the catalog, looks for a second, and returns "Not implemented Error: more than one table in a native database file" when it finds one. `open_with` reads the file back by calling `create_native_table` once. `encode_directory` and `decode_directory` in `crates/rudb-native/src/lib.rs` write and read one table name, one field list, one row count, and one stripe list.

Nothing in the format wanted it this way. `spec/storage-v3/02-file-layout.md` already says the manifest is a compact directory of catalogs, tables, schema versions, stripes, column pages, and statistics. The implementation stopped one level short of that because ClickBench is a single table and never asked for the second one.

The cost is that the four engine contract in `spec/storage-v3/03-benchmark-contract.md` cannot run on TPC-H at all. TPC-H has eight tables. The native column of that table is empty for every query in the suite, which means the benchmark that decides whether the native format is worth owning has never been run on the workload it matters most for.

## Evidence

Measured on this machine over the 22 TPC-H SF1 queries, best of three per query, one process per engine, process startup subtracted. The machine was busy, so read the ratios and not the absolute times.

| engine | total | peak RSS |
| --- | --- | --- |
| rudb native | 14824 ms | |
| rudb parquet | 6412 ms | |
| duckdb native | 1056 ms | 1617 MiB |
| duckdb parquet | 2557 ms | 663 MiB |

The native pair is 14.04x against us and the parquet pair is 2.51x against us. The direction is the part that matters. DuckDB gets about 2.4x faster moving from parquet to its own format. rudb gets about 2.3x slower. Owning the format is currently a net loss, and the goal of 10x faster and 10x less resource has to start by making our own format at least as good as reading somebody else's.

The native tables in that run were built with `CREATE TABLE t AS SELECT * FROM 't.parquet'` and left in memory, because a native file cannot hold eight tables. So the run measures the in-memory path and not the native file path, which is the second reason this work has to land before the benchmark means anything.

q05 shows what blind planning costs. Over native tables it takes 10.41 s, 12.15 s, and 12.44 s. The same query over parquet views of the same data takes 0.166 s, 0.234 s, and 0.271 s, and duckdb native takes 37 ms. The plans differ: the parquet plan estimates the `o_orderdate` range filter at 227,556 rows from the zone map, close to the true 227,597, while the native plan estimates 60,000 from a constant. Join ordering then builds customer against supplier on `n_nationkey`, a 12 million row intermediate that neither better plan produces. The only `Zones` implementor in the tree is the Parquet footer in `crates/rudb-parquet/src/prune.rs:87`, so a native table is planned with no bounds at all even though the native stripe already carries `zone` and `part_ranges`. `spec/storage-v3/05-persisted-zone-maps.md` covers exposing them and is a dependency of this work, not part of it.

## First-principles reason

A database is a set of tables committed together. A file per table makes every multi table statement a multi file commit, which means either a two-phase protocol across files or a window where one table is at generation n and another at generation n minus one. Neither is worth building to avoid one level of indirection in a directory. One file also amortises the header, gives one fsync per checkpoint instead of one per table, and lets a shared string dictionary span tables later, which `spec/storage-v3/09-global-string-codes.md` wants.

The reason to split the directory into two levels rather than encoding one flat blob is open latency. A flat directory is decoded whole, and on eight TPC-H tables that means decoding stripe and page entries for `lineitem` before answering a query that reads only `nation`. The catalog level is a handful of bytes per table and is the only thing open has to touch. Per table directories are decoded on first touch and cached, so a session that reads two tables pays for two.

This is the shape single file databases converge on. SQLite keeps a schema table in page one and reaches everything else through per object roots. DuckDB keeps a catalog in its metadata blocks and reaches table data through per table pointers held by a block manager with a free list. Lance keeps a manifest per version that names fragments rather than inlining them. The common property is that the thing read at open is proportional to the number of objects, not to the size of the data.

## Encoding

The footer slot scheme in `spec/storage-v3/02-file-layout.md` is unchanged. Two slots at header offsets 16 and 16 plus `SLOT_BYTES`, each with offset, length, generation, and checksum, and open picks the highest valid generation. What changes is what the slot points at.

The slot points at a catalog directory:

- a table count
- one entry per table: name, schema version, row count, field list, and the offset and length of that table's own directory, with its own checksum

A table directory is the existing v21 body from the fields onward: dictionaries, frequencies, distincts, and the stripe list with parts, index, pages, memberships, sieves, part ranges, and zone. That is deliberate. The per table bytes are the bytes we already write, so the change is one level of wrapping and not a rewrite of the page grammar.

The catalog entry repeats name, row count, and field list rather than pointing into the table directory for them. Those are what `open` needs to build a catalog and what `EXPLAIN` needs for row counts, and reading them from the small level is the whole point of having two levels.

## Commit model

A checkpoint writes the pages and table directories for tables whose data changed, then writes one catalog directory naming every table, then writes the inactive footer slot. A table that did not change is named in the new catalog directory with the offset and length it already had. Its bytes are not rewritten.

That gives atomicity across tables for free. The catalog directory is published by the single footer slot write that publishes everything else, so a reader sees either the generation before the checkpoint or the generation after it, with every table at the same generation in both cases.

Publishing by rename through a temporary path, which is what `persist` does today, stops working once unchanged tables are meant to keep their bytes, because a rename writes a whole new file. The two slot scheme in the header was put there for exactly this and this is the milestone that starts using it. Rename stays as the path for creating a file that does not exist yet.

## Space reclamation

Appending a new generation without rewriting the file leaves the previous generation's superseded bytes behind. A free list of byte ranges, held in the catalog directory and updated with it, records ranges that no live generation names. A range is only added once the generation that named it has been superseded by a committed later generation, so a crash during checkpoint cannot leave a live range on the free list.

The first implementation may allocate by appending and let the file grow, with the free list recorded and unused. That is honest about the ordering: correctness of the commit comes first, reuse is a size optimisation, and a file that grows on rewrite is still a file that reopens correctly. Compaction rewrites the whole file as a new generation and is the fallback until reuse lands.

## DDL

`CREATE TABLE` adds a catalog entry at the next checkpoint. `DROP TABLE` removes one, and the dropped table's directory and pages go to the free list once the generation that named them is superseded. `ALTER TABLE ... RENAME` rewrites the name in the catalog entry and touches nothing else, which is a property worth having and a reason the name lives at the catalog level.

Schema version per table is recorded but not yet used. `ALTER TABLE ... ADD COLUMN` and friends need a story about stripes written under an older schema, and that story is not this milestone.

## What this does not do

No MVCC. No concurrent writers. No delete masks or replacement stripes. One writer, one reader at a time, bulk insert and reopen, which is the boundary `spec/storage-v3/02-file-layout.md` already drew and this milestone does not move.

## Exit criteria

1. Load all eight TPC-H SF1 tables into one native file, `CHECKPOINT`, reopen the file in a fresh process, and answer all 22 queries with results matching DuckDB, modulo the floating point summation differences tracked in issue #950.

2. Report the four engine table from `spec/storage-v3/03-benchmark-contract.md` on TPC-H SF1 with every cell filled: wall time per query and total, CPU time, peak RSS, and file size, for rudb native, rudb parquet, duckdb native, and duckdb parquet. That table has never been produced on TPC-H and producing it is the point of this work.

3. rudb native total is at or below rudb parquet total. Getting faster by using our own format rather than slower is the first bar, and 14824 ms against 6412 ms says we are not over it.

4. Open time on the eight table file is proportional to the table count and not the data size, measured by comparing open on SF1 against open on SF10 and showing the two within noise of each other.
