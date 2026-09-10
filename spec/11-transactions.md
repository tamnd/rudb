# Transactions, durability and the catalog

An analytical engine's transaction system is small compared to an OLTP system's and it is not optional. Users load, update, delete and expect the file to survive a crash. This document specifies the concurrency model, MVCC, the write-ahead log, checkpointing and DDL. None of it is novel and that is intentional: the innovation budget is spent in documents 05, 06 and 07, and here the goal is to be boring and correct.

## 11.1 The concurrency model

**One process, multiple reader threads, one writer transaction at a time.** Same as DuckDB. Concurrent read transactions run without blocking each other or the writer. A second concurrent write transaction gets a conflict error rather than a queue.

This is a real limitation and it is the right one for an embedded analytical engine. Supporting concurrent writers means a lock manager, deadlock detection, and a much larger conflict surface, in exchange for a workload this engine is not for. Document 12.6's Quack server mode does not change it; it serializes writes from multiple clients.

**Isolation is snapshot isolation.** A transaction sees the database as of its start. Write-write conflicts on the same row abort the later transaction.

**Read-only mode is a first-class open mode** and multiple processes may open the same file read-only. Read-write is single-process, enforced by a lock file, because cross-process write coordination in an embedded database is a source of corruption reports out of proportion to its usefulness.

## 11.2 MVCC

**Versioning is per row within a row group, with an undo chain.** A row group holds its base data plus, when modified, a version structure recording which rows were deleted or updated by which transaction. A reader with an older snapshot walks the undo information to reconstruct what it should see.

**Updates are delete plus insert at the storage level.** The old row's version is marked deleted by the updating transaction and a new row is appended. This keeps the encoded column chunks immutable, which is essential: an update that had to modify a bit-packed, dictionary-encoded, cascaded column chunk in place is not a thing that can be done efficiently, and making column chunks immutable is what lets the whole of document 06 exist.

**The cost is that update-heavy workloads accumulate versions and degrade**, which is the standard tradeoff for a columnar store and is why background recompaction exists.

**Transaction identifiers are 64-bit and monotonic.** A snapshot is a transaction identifier plus the set of transactions active at its start. Cleanup of versions older than the oldest active snapshot happens in the background.

**Deletion bitmaps per row group** are the fast path for the common bulk-delete case, per document 5.8, with the version chain used only where a snapshot needs the pre-delete state.

## 11.3 Write-ahead log

**A separate file, appended, fsynced on commit.** Contents are logical: a transaction's inserts as row batches, its deletes as row identifiers, its updates as delete plus insert, and its DDL as catalog operations.

**Logical rather than physical logging**, because physical logging of an encoded column chunk means logging the whole chunk when one row changes. Logical entries are small and replay is a normal insert path.

**Each entry is checksummed and length-prefixed**, and replay stops at the first entry that fails to validate, which is how a torn write at the tail of the log is handled.

**Bulk loads bypass the WAL.** A `COPY` or `CREATE TABLE AS` writes its row groups directly, fsyncs them, and logs only a small entry recording which blocks became live. Logging 20 GB of data twice to load 20 GB is not acceptable and every analytical database has this escape hatch. The correctness requirement is that the data blocks are durable before the log entry that references them is written, which is an ordering constraint on two fsyncs.

**Durability is configurable and the default is durable.** A setting turns off the commit fsync for people doing bulk ETL who can rerun, which is a legitimate use and should be explicit rather than accidental.

## 11.4 Checkpointing

A checkpoint writes all dirty pages, writes a new metadata version, writes the alternate header with the new root pointer, fsyncs, and truncates the WAL.

**Automatic at a WAL size threshold, manual via `CHECKPOINT`, and forced on clean shutdown.**

**The header alternation is the atomic commit**, per document 5.2. Until the new header is durable, the old one is live and the database is in its previous state. There is no window in which a partially written metadata tree is reachable.

**Checkpointing is where recompaction happens.** A row group whose deletion bitmap or version chain has exceeded a threshold is rewritten during checkpoint: live rows re-encoded, possibly with a better encoding decision now that the data has changed, and the old blocks freed. This is also where a row group written before a global dictionary grew gets the benefit of the current dictionary.

**Checkpoint does not block readers.** It does take the write lock, so it blocks the writer, and a long checkpoint on a large database is a real pause for a write-heavy workload. Incremental checkpointing, which spreads the work across multiple commits, is scheduled at M9.

## 11.5 Crash consistency

**The requirement is that after any crash at any point, opening the database yields a state consistent with some prefix of committed transactions**, and that every transaction reported committed is present.

**Testing this is a specific piece of apparatus, not a hope.** Document 16.5 specifies it: a deterministic simulation that intercepts every write, fsync and rename, and can replay a workload with a failure injected at every possible point, reordering writes that were not separated by an fsync. This is the technique that has actually found corruption bugs in real systems and it is scheduled at M6 rather than after 1.0.

**The specific hazards being tested for.** A torn header write, which the checksum plus alternation handles. A torn WAL entry, which the per-entry checksum handles. Data blocks written but the referencing metadata not, which leaks space until vacuum and must not corrupt. Metadata written referencing blocks that were not, which is the one that corrupts and which the fsync ordering in 11.3 exists to prevent. And a free list that reuses a block still referenced by an older header, which is prevented by not freeing a block until the header that referenced it is no longer reachable.

## 11.6 Catalog and DDL

**The catalog is versioned, immutable per version, and read without a lock.** A transaction takes an `Arc` to the catalog version current at its start. DDL builds a new version by structural sharing and installs it at commit.

**DDL is transactional.** `CREATE TABLE` inside a transaction that rolls back leaves no table. This is table stakes and DuckDB has it.

**Catalog contents.** Schemas, tables with their column definitions and constraints, views with their bound plan cached and invalidated on dependency change, sequences, indexes, macros, types including enums, triggers, and the global dictionary and symbol table objects from document 5.4, which are catalog objects because they outlive any single row group.

**Dependency tracking is explicit.** Dropping a table that a view depends on errors unless `CASCADE`. This requires a dependency graph in the catalog and it is one of those features whose absence is not noticed until data is lost.

**Constraints.** `NOT NULL` and `CHECK` are enforced on insert. `PRIMARY KEY` and `UNIQUE` are enforced with an ART index, matching DuckDB. `FOREIGN KEY` is enforced. None of these are query accelerators by design, though the optimizer will use a unique constraint for cardinality and a primary key index for a point lookup.

**The ART index is the one row-oriented structure in the system** and it exists for constraint enforcement. Its memory cost on a large table is real and it is why DuckDB users are told not to create indexes they do not need. We inherit that and document it.

## 11.7 What this costs the performance story

**MVCC version checking is on the scan path.** A scan of a row group with no versions checks one pointer and proceeds, which is free. A scan of a row group with versions pays per vector. Keeping the common case at one pointer check is the design requirement and it is why versions live outside the encoded data rather than as a per-row visibility column.

**The deletion bitmap is checked per vector** and combined into the validity mask, which is one bitwise operation per 1024 rows when the bitmap is all-live and is the reason a mostly-unmodified table scans at full speed.

**Bulk-loaded, never-modified data has zero transactional overhead in the scan path.** That is the ClickBench case and it is not an accident that it is the case with no overhead.
