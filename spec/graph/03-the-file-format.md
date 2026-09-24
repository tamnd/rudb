# 3. The file format

This document extends rudb's single-file native format, `crates/rudb-native/src/lib.rs`, from format 22 to format 23. Earlier drafts of this document said version 10 to version 11, which was the numbering the format had when the document was written and has not been since: the code's `FORMAT` constant is what counts and it reached 22 before anything here was built. It adds one general mechanism, the section table, and three section kinds that use it. Everything stays in one file. Nothing here is a sidecar on the filesystem, a second database, or an external index.

## 3.1 The invariant

**Deleting every graph section from a rudb file must change no answer to any query, only the time it takes.**

That sentence is the load-bearing one in this directory and everything below is constrained by it. It gives four things at once. A wrong index is a performance bug rather than a wrong answer, which is the difference between an incident and a ticket. A stale index after a rewrite is handled by ignoring it, so generation checking is enough and there is no repair path to get right. Every query has a reference execution available by turning the sections off, which makes the differential test in document 09 section 9.2 a loop over the suite with a setting flipped rather than a second engine. And the layer can ship a piece at a time, because a link that has not been built yet is indistinguishable from one that has been deliberately disabled.

The cost of the invariant is that no graph section may hold information that is not derivable from the table's own columns. A section may hold a key map, which is derivable. It may not hold a row's values, and it may not hold a `rid` for a row that is not in the file.

## 3.2 The section table

Format 22 writes a header of eighty bytes with two twenty-eight byte slots for the directory, alternating by generation, and a directory that names one page per column per stripe plus a per-stripe index page of lengths and checksums. Format 23 sets `FORMAT` to 23 and adds to the directory one list, behind its own eight byte tag `RUDBSE1\0` at the end: the section table.

The magic stays `RUDBNV10` and does not move with the format number. The reader already tells the two failures apart and the messages are different for a reason: a wrong magic means a file that was never ours and the answer is to look at the path, while a wrong format means our own file from another build and the answer is the version number this build wants. Moving the magic every time the format changes would turn the second question into the first and throw away the only part of the message that helps.

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
| `header_bytes` | 4 | length of the kind-specific header, which is inline at the front of the first extent, or, when `extents` is zero, what the structure would have cost |

Three rules govern it and all three exist because of something that has already gone wrong in this codebase.

**A reader ignores a `kind` it does not know.** This is what makes format 23 the last bump this mechanism needs. A new section kind, a fifth key map form, a completely different structure invented in 2027, all of them are an entry with an unfamiliar tag in a file that older code still reads correctly, because of section 3.1.

**A section is a list of extents of at most sixty four megabytes, each independently checksummed and independently readable.** Issue #745 is that the format 22 directory is one hundred and twenty eight megabyte buffer and a hundred million row load fails against it. The neighbour array of a backward adjacency on TPC-H `lineitem` at SF100 is two gigabytes. A section that had to be one page, or one buffer, would reproduce #745 immediately and at a larger scale. Extents mean a reader `pread`s the two it needs.

**Sections are written before the directory that names them, and the directory is committed by the existing two-generation header swap.** A crash mid-write leaves unreferenced bytes at the end of the file, which is what a crash mid-column-write already leaves. There is no new recovery path.

Format 22 files are readable by a format 23 reader: their directory ends before the section table's tag, and a directory that ends there is a table with no sections, which by section 3.1 is a table that answers every query correctly and more slowly. The reader carries a list of readable formats rather than a single number, and the list has two entries for this one reason. Format 23 files are not readable by a format 22 reader, and that is accepted rather than worked around, because the format is at 0.3.x and is not yet something anyone has on disk that this project did not put there. `../12-duckdb-compat.md` is unaffected; none of this touches the DuckDB format, which has no place to put it.

## 3.3 Kind one: the key map, `RUDBKM1`

One per parent key column that some relationship uses. `id` is the column's position in the table. `flags` carries the form, which is one of the three in document 02 section 2.2.

**Identity** stores twenty four bytes: `base`, `count`, and the logical type it was built against. No extents beyond the header.

**Dense offset** stores `base`, `range`, then a bitmap of `range` bits and a two-level rank index over it, superblocks of four thousand ninety six bits holding a `u32` cumulative count and blocks of five hundred twelve bits holding a `u16` offset within the superblock. Rank is then two loads and a `popcount` over at most eight words. Space is `range / 8` bytes for the bitmap and about 12.5 percent again for the index.

**Sorted** stores the sorted key values as an ordinary rudb column page, using whatever encoding the writer would have chosen for that column, plus a permutation from sorted position to `rid` bit-packed to `ceil(log2(rows))` bits. The reuse is the point: a sorted `VARCHAR` key map is a sorted dictionary, and #747 already wrote the machinery that stores a dictionary's sorted order beside its values and searches that instead of peeling. A key map over a dictionary encoded column stores codes, not text, and the search is over `u32`.

The build records what it observed: whether the values were distinct, whether they were sorted, and the minimum and maximum. Those four facts are the cardinality verification of document 02 section 2.3 and they are written into the header rather than recomputed.

## 3.4 Kind two: the forward link, `RUDBFL1`

One per relationship. It holds, for each row of the child table in `rid` order, the `rid` of its parent, with the maximum representable value reserved to mean *no parent*.

**It was going to be a column, and it is a section.** The original argument was that a column is not a new structure, not a new page discipline and not a new reader: written by `Writer::append` into the same stripe pages as every other column, bit-packed to `ceil(log2(parent_rows + 1))` bits by the encoder that already exists, and given zone maps by `crates/rudb-storage/src/zone.rs` for free, where the minimum and maximum parent `rid` per part is exactly the pruning statistic document 05 section 5.5 needs to skip a whole part during a semi-join reduction.

The argument does not survive section 3.8 below, and the conflict is between two paragraphs of this document rather than between this document and the implementation. Section 3.8 requires that a load inserting parent and child in one statement builds the child's link in a second pass at checkpoint time, because the parent's key map has to exist first. A second pass can append a section to a committed file. It cannot go back and add a column to stripes that have already been written and checksummed, and making it able to would mean rewriting the table to gain an index, which is not a thing an index gets to cost. So `RUDBFL1` is a section payload, `crates/rudb-graph/src/link.rs` is where its layout lives, and the part-skip statistic is stored rather than inherited: a minimum and a maximum parent `rid` per part, in a head at the front of the payload, which is sixteen bytes per 1024 child rows and is under half a percent of a packed link at SF100 widths. The monotone form below stores no head at all, because a non-decreasing sequence's extremes over a range are its two ends and two selects are cheaper than the nine megabytes SF100 would otherwise spend.

A streaming load that already has the parent's key map in hand is free to write the link as a column in the same pass, and nothing here forbids it. What this section fixes is the form the link takes on disk, which has to be one form and not two.

**The monotone case, which is most of TPC-H.** When the child table's rows are in non-decreasing parent `rid` order, which happens whenever the child is physically clustered by the join key, which is how `dbgen` emits `lineitem` against `orders` and `partsupp` against `part`, the forward link is a non-decreasing sequence and storing it as a bit-packed integer per row is a waste. `flags` records the monotone form and the payload becomes a single bit vector of `child_rows + parent_rows` bits: for each parent in `rid` order, a run of one bits, one per child that points at it, then a zero. With rank and select support over that vector, `forward(child) = rank0(select1(child))` and `backward(parent) = [cum(parent - 1), cum(parent))` where `cum(p) = select0(p) - p`, so **one structure answers both directions** and the backward adjacency of section 3.5 does not have to exist at all.

The backward formula is written here in terms of `cum` because the two formulas are only both true under one layout, and the layout is worth stating rather than deriving twice. Ones first, as above: parent zero's children, then a zero, then parent one's children, then a zero. Under that layout a child's one bit has exactly as many zeros before it as its parent has `rid`, which is the forward formula, and `select0(p) - p` is the count of every child of every parent up to and including `p`, because the `p`th zero has `p` zeros and every earlier one bit before it. The forward direction is the hot one, so it is the one the layout is chosen for.

The monotone form also requires that every child has a parent. The packed form has a reserved value for an unmatched child and this one has nowhere to put it: every bit is either a child of the parent whose run it is in or a parent boundary. A relationship with one unmatched child is therefore packed even when its matched children are in perfect order, and that is the honest answer rather than a missed case. On the two TPC-H relationships this form exists for it does not arise, because a foreign key that `dbgen` emits always matches.

`child_rows + parent_rows` is therefore also `linked + parent_rows`, and the implementation writes the second, which is the same number whenever the form applies at all.

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

Below sixty four kilobytes of sections the share does not apply and everything fits. A percentage is the right rule for a structure whose size is worth arguing about and it stops meaning anything at the bottom: an identity key map is forty bytes on a table of any size, a key column of sequential integers encodes to a constant delta and almost no bytes, and ten percent of almost nothing is under forty. The pure rule would throw away the cheapest structure in the system for being expensive, and what it would be measuring is how well the column compressed rather than what the cache costs.

When the budget binds, relationships are built in order of expected value, which the builder estimates as the rows of the hash table a link saves divided by the section's bytes, biased toward relationships the query log has actually used. A hash join builds its smaller side, so the rows saved are the smaller of the child and the parent row counts. Counting the child rows alone would rank every link of one table by size and nothing else, because they all have the same children, and on `lineitem` it keeps the link to `part`, which saves 200,000 rows at SF1, over the one to `partsupp`, which saves 800,000 for a tenth more bytes. A relationship that does not fit is recorded as not built, with its size, so `rudb_links()` shows what a larger budget would buy rather than leaving the user to guess.

The record is a section table entry with no extents. `kind` and `id` say which structure was turned away, `flags` carries the form it would have taken, and `header_bytes` carries what its payload would have been, which it can do because an entry with nothing behind it has no header for the field to describe. Fifty six bytes per refusal is what this costs, it is paid once per column the builder looked at rather than once per query, and a reader that knows nothing of the convention sees an entry with no payload, which is the same thing as no entry at all. The two reasons a build has for turning a structure away are the budget and a parent key that repeats, and the record does not say which, because the size against the budget is what distinguishes them and both are already in the table.

The monotone case is the reason this budget is livable. On TPC-H SF100 the three large relationships are `lineitem → orders` at 106 MB monotone, `partsupp → part` at about 14 MB monotone, and `lineitem → part` at 1.88 GB non-monotone. The first two are free against any budget. The third is the one that has to justify itself, and document 09 section 9.5 is the measurement that decides whether it does.

## 3.8 Build cost, and where it happens

A link is built by one pass over the child's key column, with the parent's key map resident. For the identity form that pass is an integer subtract and a bounds check per row, which is bandwidth-bound and vectorizes; for the sorted form it is a binary search per row, which is not. `../storage-v3/04-streaming-load.md` says the load streams chunks from the query pipeline to the writer and its working set is bounded by stripe and writer concurrency, and link building inherits that: the link column is produced chunk by chunk beside the data columns, so a load with links is a load with one more column and one more pass, not a second load.

Two things are deferred to after the write rather than done during it. The key map of a parent table has to exist before the child's link can be built, so a load that inserts both in one statement builds the child's link in a second pass at checkpoint time. And the monotone detection needs the whole column, so a link is written in the general form during a streaming load and rewritten into the monotone form at checkpoint when the detection succeeds. Both of those are background work, both are interruptible, and both leave a file that answers correctly if they never run.

Measured build cost is a G1 deliverable in document 10 and there is no estimate here, because `crates/rudb-storage/src/zone.rs` records what happened last time somebody estimated a build cost in this codebase: the first zone map implementation cost 260 milliseconds where the third cost 60, and the difference was a `Bound` allocated per value.
