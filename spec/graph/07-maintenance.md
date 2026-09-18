# 7. Maintenance

An index that is wrong is worse than no index, unless it cannot be wrong, and section 3.1 is the mechanism that makes it cannot. This document says what happens to the sections under append, delete, update, checkpoint and concurrent readers, and it is deliberately the least ambitious document in the directory: rudb is an analytical engine whose file is written by append and rewritten by checkpoint, and the mutable-CSR literature of document 01 section 1.7 is solving a problem rudb does not have yet.

## 7.1 The four states a section can be in

**Current.** Its generation matches the table's and it covers every stripe. It is used.

**Partial.** It covers stripes zero through `k` and the table has more. It is used for the stripes it covers and the rows past `k` take the fallback path. This is the state a background build passes through and it is a first-class state rather than an intermediate one, because a build over a hundred million rows should not be all-or-nothing and because a query that arrives mid-build should get the part that is ready.

**Stale.** Its generation does not match. It is ignored and scheduled for rebuild. It is not read, not repaired and not trusted for a single row.

**Absent.** Everything falls back.

A reader decides which of the four it is looking at from the section table entry and the directory, at open, once. There is no per-query validation of contents and no checksum verification beyond the per-extent one the read already performs, because a section whose bytes are wrong but whose checksum passes is a class of failure that the differential test in document 09 section 9.2 catches and that no runtime check would catch more cheaply.

## 7.2 Append

An append adds rows at the end of the `rid` space, which renumbers nothing. Three things happen per section kind.

A **forward link** is a column, so it is appended like a column: the new rows' parent `rid`s are resolved against the parent's key map and written into the new part. If the parent's key map is absent, because the parent is being loaded in the same statement, or because it was evicted, the link goes to partial and the rows past the boundary take the fallback. The monotone form of document 03 section 3.4 survives an append only if the appended rows continue the order; a streaming load therefore writes the general form and the checkpoint rewrites it, which document 03 section 3.8 already said.

A **key map** in identity form survives an append of ascending keys and is invalidated by anything else. In sorted form it does not survive an append at all, since insertion into a sorted array is not an append, so the map goes stale and is rebuilt at checkpoint. Between the append and the checkpoint, a lookup consults the map for the rows it covers and scans the tail, which for a tail of a few parts is cheaper than a rebuild and which is the standard delta-plus-base arrangement written in the smallest possible way.

A **backward adjacency** does not survive an append that adds children of existing parents, because that inserts into the middle of the neighbours array. It goes partial: the adjacency covers the first `k` stripes and a query wanting the children of a parent gets those from the CSR and the rest from a scan of the tail. Above a tail fraction, default five percent, it goes stale instead, because at that point the scan is the cost.

## 7.3 Delete and update

A delete sets a bit in the delete mask and renumbers nothing, so every section stays current. Document 05 section 5.9 covers the consequence, which is a link that points at a deleted row and a gather that is wasted.

An update in rudb's storage model is a delete and an append. The appended row gets a new `rid` and is handled by section 7.2; the deleted row is handled by the paragraph above. An update to a *key column* is the case that needs naming: it invalidates the parent's key map for that value, and since the map is the thing that is stale rather than wrong, the old value's entry points at a row the delete mask now excludes, and the new value is in the tail, the delta-plus-base arrangement of section 7.2 is already the answer.

Nothing here supports a high update rate well, and nothing here pretends to. The paged-CSR structure of Bw-Graph and the LSM arrangement of BACH and Aster exist because a graph DBMS serving transactions has a rate at which the above stops being acceptable. Document 11 records that rudb does not know where that rate is for its own workloads and that finding out is a measurement rather than a design decision.

## 7.4 Checkpoint

The checkpoint is where sections are built, rewritten and compacted, and it is the only place a section is built synchronously with a statement the user typed.

The order is fixed: column pages, then key maps, then forward links, then backward adjacencies, then the section table, then the directory, then the header swap. It is fixed because a forward link is built against a key map and an adjacency is built against a link, so any other order either builds against something unwritten or reads back what it just wrote.

A checkpoint that runs out of its budget, per document 03 section 3.7, stops at a section boundary and commits what it has. The file is then correct with fewer sections than it would like, which is the state section 3.1 makes safe.

`CHECKPOINT INTO`, which `../12-duckdb-compat.md` defines as the path from a DuckDB file to a rudb file, is where a user with an existing database gets the whole layer: the DuckDB file has the `PRIMARY KEY` and `FOREIGN KEY` constraints in its catalog, document 02 section 2.5 turns those into relationships, and the rewrite builds them. That is the best acquisition path this feature has and it needs no new syntax at all.

## 7.5 Transactions

`../11-transactions.md` gives rudb snapshot isolation over a single-file format with a two-generation commit. Sections inherit it without addition: a reader holds a directory, the directory names sections by offset and generation, and a concurrent writer's new sections are not in that directory. A reader therefore sees a consistent set of sections matching a consistent set of column pages, and nothing has to be latched.

The one interaction that is not free is the resident cache of document 04 section 4.2. A key map cached in memory is keyed by `(table, column, generation)` and not by `(table, column)`, so a transaction that rebuilt a map does not hand the new one to a reader that is still on the old generation. This is three words in a cache key and it is exactly the kind of three words that is missing the first time.

## 7.6 Rebuild policy

A stale section is rebuilt by a background task with the same shape as the recompressor in `../06-compression.md`: it runs at low priority, it is interruptible at extent boundaries, it charges its memory to the background budget, and it writes a new section rather than mutating the old one so that a reader is never looking at a section being written. A rebuild that is interrupted leaves a partial section, per section 7.1, which is useful rather than wasted.

Priority order among pending rebuilds is by the query log: a relationship that recent queries used gets rebuilt before one that nothing has touched. That is the same signal document 02 section 2.5 uses to infer relationships in the first place, and the two share one structure rather than two.

## 7.7 What is measured about all of this

Per section: build wall time, build CPU, bytes written, and the state it ended in. Per table: the fraction of stripes covered by each section. Per query: how many times a fallback was taken because a section was partial or stale, which is the number that tells a user their index is not doing what they think it is.

The one alarm worth wiring is a ratio: fallbacks taken divided by link joins planned. When that is not near zero, something upstream is renumbering more than anyone intended, and finding that out from a counter is much cheaper than finding it out from a benchmark that mysteriously regressed.
