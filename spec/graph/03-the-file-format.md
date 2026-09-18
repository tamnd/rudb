# 3. The file format

This document extends rudb's single-file native format, `crates/rudb-native/src/lib.rs`, from version 10 to version 11. It adds one general mechanism, the section table, and three section kinds that use it. Everything stays in one file. Nothing here is a sidecar on the filesystem, a second database, or an external index.

## 3.1 The invariant

**Deleting every graph section from a rudb file must change no answer to any query, only the time it takes.**

That sentence is the load-bearing one in this directory and everything below is constrained by it. It gives four things at once. A wrong index is a performance bug rather than a wrong answer, which is the difference between an incident and a ticket. A stale index after a rewrite is handled by ignoring it, so generation checking is enough and there is no repair path to get right. Every query has a reference execution available by turning the sections off, which makes the differential test in document 09 section 9.2 a loop over the suite with a setting flipped rather than a second engine. And the layer can ship a piece at a time, because a link that has not been built yet is indistinguishable from one that has been deliberately disabled.

The cost of the invariant is that no graph section may hold information that is not derivable from the table's own columns. A section may hold a key map, which is derivable. It may not hold a row's values, and it may not hold a `rid` for a row that is not in the file.

## 3.2 The section table

Version 10 writes a header of eighty bytes with two twenty-eight byte slots for the directory, alternating by generation, and a directory that names one page per column per stripe plus a per-stripe index page of lengths and checksums. Version 11 changes the header's magic to `RUDBNV11`, sets `FORMAT` to 11, and adds to the directory one list: the section table.

A section table entry is fifty six bytes.

| field | width | meaning |
| --- | --- | --- |
| `kind` | 8 | an eight byte tag, `RUDBKM1\0`, `RUDBFL1\0`, `RUDBAJ1\0` for the three kinds below |
| `id` | 8 | which instance of that kind, meaning which relationship or which key column |
| `generation` | 8 | the table generation these bytes describe |
| `extents` | 4 | how many extents the payload is in |
| `extent_page` | 8 | where the extent list is |
| `extent_bytes` | 4 | how long the extent list is |
| `hash` | 8 | checksum of the extent list |
| `flags` | 4 | per-kind, defined below |
| `header_bytes` | 4 | length of the kind-specific header, which is inline at the front of the first extent |

Three rules govern it and all three exist because of something that has already gone wrong in this codebase.

**A reader ignores a `kind` it does not know.** This is what makes version 11 the last bump this mechanism needs. A new section kind, a fifth key map form, a completely different structure invented in 2027, all of them are an entry with an unfamiliar tag in a file that older code still reads correctly, because of section 3.1.

**A section is a list of extents of at most sixty four megabytes, each independently checksummed and independently readable.** Issue #745 is that the version 10 directory is one hundred and twenty eight megabyte buffer and a hundred million row load fails against it. The neighbour array of a backward adjacency on TPC-H `lineitem` at SF100 is two gigabytes. A section that had to be one page, or one buffer, would reproduce #745 immediately and at a larger scale. Extents mean a reader `pread`s the two it needs.

**Sections are written before the directory that names them, and the directory is committed by the existing two-generation header swap.** A crash mid-write leaves unreferenced bytes at the end of the file, which is what a crash mid-column-write already leaves. There is no new recovery path.

Version 10 files are readable by a version 11 reader: their section table is empty. Version 11 files are not readable by a version 10 reader, and that is accepted rather than worked around, because the format is at 0.3.x and is not yet something anyone has on disk that this project did not put there. `../12-duckdb-compat.md` is unaffected; none of this touches the DuckDB format, which has no place to put it.

## 3.3 Kind one: the key map, `RUDBKM1`

One per parent key column that some relationship uses. `id` is the column's position in the table. `flags` carries the form, which is one of the three in document 02 section 2.2.

**Identity** stores twenty four bytes: `base`, `count`, and the logical type it was built against. No extents beyond the header.

**Dense offset** stores `base`, `range`, then a bitmap of `range` bits and a two-level rank index over it, superblocks of four thousand ninety six bits holding a `u32` cumulative count and blocks of five hundred twelve bits holding a `u16` offset within the superblock. Rank is then two loads and a `popcount` over at most eight words. Space is `range / 8` bytes for the bitmap and about 12.5 percent again for the index.

**Sorted** stores the sorted key values as an ordinary rudb column page, using whatever encoding the writer would have chosen for that column, plus a permutation from sorted position to `rid` bit-packed to `ceil(log2(rows))` bits. The reuse is the point: a sorted `VARCHAR` key map is a sorted dictionary, and #747 already wrote the machinery that stores a dictionary's sorted order beside its values and searches that instead of peeling. A key map over a dictionary encoded column stores codes, not text, and the search is over `u32`.

The build records what it observed: whether the values were distinct, whether they were sorted, and the minimum and maximum. Those four facts are the cardinality verification of document 02 section 2.3 and they are written into the header rather than recomputed.

## 3.4 Kind two: the forward link, `RUDBFL1`

One per relationship. It holds, for each row of the child table in `rid` order, the `rid` of its parent, with the maximum representable value reserved to mean *no parent*.

**It is a column.** Not a new structure, not a new page discipline, not a new reader. It is written by `Writer::append` into the same stripe pages as every other column, it appears in the per-stripe index page with its own length and checksum per part, it is bit-packed to `ceil(log2(parent_rows + 1))` bits by the encoder that already exists, and it gets zone maps from `crates/rudb-storage/src/zone.rs` for free. That last one is not a small detail: the minimum and maximum parent `rid` per part is exactly the pruning statistic that document 05 section 5.5 needs to skip a whole part during a semi-join reduction, and it arrives without a line of new code.

The column is not in the table's SQL schema, is not returned by `SELECT *`, and is not nameable in SQL. It occupies a slot in the physical field list past the logical ones, which the directory already distinguishes because it stores the field list.

**The monotone case, which is most of TPC-H.** When the child table's rows are in non-decreasing parent `rid` order, which happens whenever the child is physically clustered by the join key, which is how `dbgen` emits `lineitem` against `orders` and `partsupp` against `part`, the forward link is a non-decreasing sequence and storing it as a bit-packed integer per row is a waste. `flags` records the monotone form and the payload becomes a single bit vector of `child_rows + parent_rows` bits: for each parent in `rid` order, a run of one bits, one per child that points at it, then a zero. With rank and select support over that vector, `forward(child) = rank0(select1(child))` and `backward(parent) = [select0(parent) - parent, select0(parent + 1) - parent - 1]`, so **one structure answers both directions** and the backward adjacency of section 3.5 does not have to exist at all.

The arithmetic, on TPC-H SF100. `lineitem` is 600,037,902 rows and `orders` is 150,000,000. A bit-packed forward link is 600,037,902 × 28 bits, which is 2.10 GB. The monotone bit vector is 750,037,902 bits, which is 93.8 MB, plus a select index. Using a sampled select structure of one position every four thousand ninety six ones plus a two-level rank index, the overhead is about 13 percent, so the whole thing is about 106 MB and it replaces both a 2.10 GB forward link and a 2.25 GB backward neighbour array. That is the number that makes this document worth writing.

**The non-monotone case.** `lineitem.l_partkey` against `part` is not clustered. The forward link is a real bit-packed column: 600,037,902 × 25 bits, which is 1.88 GB, and there is no trick that makes it smaller without losing random access. This is where the budget in section 3.7 bites, and where the honest answer is that not every relationship gets a link.

## 3.5 Kind three: the backward adjacency, `RUDBAJ1`

One per relationship that needs the one-to-many direction and is not monotone. Compressed sparse row in the ordinary sense: an offsets array of `parent_rows + 1` entries, and a neighbours array of child `rid`s grouped by parent.

The offsets array is non-decreasing, so it is stored the same way the monotone link is, as a bit vector with select, rather than as `parent_rows + 1` integers. The neighbours array is bit-packed to `ceil(log2(child_rows))` bits, in extents, and within each parent's list the child `rid`s are ascending, which means the list is itself a monotone sequence and is delta-encodable. For a relationship whose average degree is above about eight, the delta form is smaller; below that the overhead of the delta structure is not repaid. The build measures the degree distribution, which it has in front of it, and chooses.

Empty lists cost one bit each, per document 02 section 2.4, rather than an offsets entry each.

Edge properties are not stored here. PVLDB 14 offers double-indexed property CSRs, which duplicate every edge property to get sequential access in both directions, and single-directional property pages, which do not duplicate and accept random access one way. rudb does neither, because in this design the edge is a row of the child table and its properties are the child table's columns, which are already stored once, columnar, compressed and zone-mapped. A backward traversal that needs a child column pays a gather, and document 05 section 5.6 is about when a backward traversal that needs a child column should be rewritten into a forward pass instead, which is usually.

## 3.6 What is not stored

No adjacency for a relationship whose parent side failed uniqueness verification. No link for a join that is not an equality. No transitive link, meaning `lineitem → nation` through `orders → customer → nation` is not materialized even though it would be useful, because materializing transitive closures is how an index budget becomes unbounded; Parachute's precomputed join-induced columns are the principled version of that idea and document 11 keeps it open with its space budget attached. No secondary index on a non-key column; that is a different feature with a different justification.

## 3.7 The budget

Graph sections are a cache and a cache needs a size. The default is that the total bytes of all graph sections for a table may not exceed **ten percent** of that table's stored column bytes, and the setting `graph_budget` raises or lowers it. `../stats/03-the-file-format.md` takes a further two percent for statistics sections through the same section table, budgeted separately so that one cannot quietly consume the other's room. Parachute allowed itself fifteen percent for a comparable structure and reported 1.54x on JOB, which is the closest published data point for what this kind of space buys.

When the budget binds, relationships are built in order of expected value, which the builder estimates as the number of child rows divided by the section's bytes, biased toward relationships the query log has actually used. A relationship that does not fit is recorded as not built, with its size, so `rudb_links()` shows what a larger budget would buy rather than leaving the user to guess.

The monotone case is the reason this budget is livable. On TPC-H SF100 the three large relationships are `lineitem → orders` at 106 MB monotone, `partsupp → part` at about 14 MB monotone, and `lineitem → part` at 1.88 GB non-monotone. The first two are free against any budget. The third is the one that has to justify itself, and document 09 section 9.5 is the measurement that decides whether it does.

## 3.8 Build cost, and where it happens

A link is built by one pass over the child's key column, with the parent's key map resident. For the identity form that pass is an integer subtract and a bounds check per row, which is bandwidth-bound and vectorizes; for the sorted form it is a binary search per row, which is not. `../storage-v3/04-streaming-load.md` says the load streams chunks from the query pipeline to the writer and its working set is bounded by stripe and writer concurrency, and link building inherits that: the link column is produced chunk by chunk beside the data columns, so a load with links is a load with one more column and one more pass, not a second load.

Two things are deferred to after the write rather than done during it. The key map of a parent table has to exist before the child's link can be built, so a load that inserts both in one statement builds the child's link in a second pass at checkpoint time. And the monotone detection needs the whole column, so a link is written in the general form during a streaming load and rewritten into the monotone form at checkpoint when the detection succeeds. Both of those are background work, both are interruptible, and both leave a file that answers correctly if they never run.

Measured build cost is a G1 deliverable in document 10 and there is no estimate here, because `crates/rudb-storage/src/zone.rs` records what happened last time somebody estimated a build cost in this codebase: the first zone map implementation cost 260 milliseconds where the third cost 60, and the difference was a `Bound` allocated per value.
