# 12. The order the suite asks for

Written 24 September 2026, against rudb 0.4.31.

Documents 02 through 10 were written before the whole suite could be measured with the layer on. It can be now, and this document is that measurement, what it says is in the way, and the order the milestones take because of it. It replaces guesses in 10-milestones.md with numbers wherever the two disagree.

## 12.1 How it was measured

TPC-H SF1 on server2, one thread, instructions retired in user space by `perf stat`, best of two runs, one process per query. DuckDB is the development build in `/root/duckdb-oracle`, reading its own file, and its queries are the ones its `tpch` extension ships. rudb is 0.4.31 reading a native file loaded from the same rows through Parquet. One thread and instructions rather than wall time because the box is shared, and because instructions are what a change to the engine moves while wall time also moves with whoever else is on the machine.

The same rows were loaded twice for each engine. **Base** is the order `dbgen` writes, which is key order. **Clustered** is `orders` sorted by `o_orderdate, o_orderkey` and `lineitem` sorted by its order's date and then by `l_orderkey, l_linenumber`, so each order's lines stay together and follow their order. No other table was touched. Clustering on a date column is allowed by TPC-H clause 1.5.4, and nothing about the query text changes.

## 12.2 The numbers

Billions of instructions. The last column is rudb on the base file with all ten relationships declared and `graph_sections` and `graph_reduction` on.

| query | DuckDB base | DuckDB clustered | rudb base | rudb clustered | rudb base, layer on |
| --- | ---: | ---: | ---: | ---: | ---: |
| q01 | 1.298 | 1.034 | 1.190 | 0.806 | 1.209 |
| q02 | 0.255 | 0.253 | 0.121 | 0.121 | 0.140 |
| q03 | 0.698 | 0.354 | 0.558 | 0.307 | 0.592 |
| q04 | 0.688 | 0.632 | 0.378 | 0.275 | 0.401 |
| q05 | 0.808 | 0.452 | 0.736 | 0.530 | 0.775 |
| q06 | 0.464 | 0.221 | 0.328 | 0.077 | 0.347 |
| q07 | 0.889 | 0.658 | 0.584 | 0.284 | 0.615 |
| q08 | 0.826 | 0.783 | 0.462 | 0.387 | 0.484 |
| q09 | 1.526 | 1.627 | 1.597 | 1.622 | 2.003 |
| q10 | 1.003 | 0.659 | 0.795 | 0.555 | 0.818 |
| q11 | 0.317 | 0.317 | 0.099 | 0.099 | 0.120 |
| q12 | 0.754 | 0.439 | 0.725 | 0.235 | 1.229 |
| q13 | 1.477 | 1.474 | 0.984 | 0.972 | 0.871 |
| q14 | 0.511 | 0.240 | 0.335 | 0.114 | 0.354 |
| q15 | 0.536 | 0.265 | 0.300 | 0.083 | 0.319 |
| q16 | 0.598 | 0.598 | 0.282 | 0.281 | 0.303 |
| q17 | 0.817 | 0.812 | 0.430 | 0.430 | 0.448 |
| q18 | 1.305 | 1.380 | 0.816 | 2.170 | 0.964 |
| q19 | 0.660 | 0.710 | 0.401 | 0.402 | 0.420 |
| q20 | 0.755 | 0.472 | 0.525 | 0.349 | 0.545 |
| q21 | 1.470 | 1.418 | 1.270 | 1.211 | 1.290 |
| q22 | 0.533 | 0.533 | 0.294 | 0.294 | 0.314 |
| all | 18.187 | 15.332 | 13.212 | 11.604 | 14.559 |

rudb is 1.38 times DuckDB on the base file. The goal in `../02-the-goal.md` is ten times, which on this table is 1.8 billion instructions for the whole suite.

## 12.3 What the table says

**Stored order is the largest lever there is, and it is a larger one for rudb than for DuckDB.** The same rows in date order cost DuckDB 16 percent less and rudb 12 percent less, and the rudb number hides a regression. Leave q18 out and rudb goes from 12.40 to 9.43, which is 24 percent. q06 is 4.3 times cheaper, q15 3.6, q12 3.1, q14 2.9. The reason is the zone maps rudb already persists: a date filter over rows stored in date order skips whole parts, and over rows in key order every part holds every date and nothing is skipped. That is `../storage-v3/13-the-scale-reversal.md`'s point about zone maps again, from the other side: it is the one mechanism whose benefit is work not done, so it is the one that grows with the order being right.

**The layer as built makes the suite slower.** 14.56 against 13.21, and no query gets more than 12 percent cheaper. Three separate causes, each measured:

- Declaring the relationships costs about 20 million instructions on every statement, including the ones that use no link. q02 and q11 move by exactly that. The declaration is a session setting, so every statement rereads and reverifies what the file already recorded.
- The link join reads the parent whole. q12's one link join over 30,988 surviving `lineitem` rows costs 193 ms of CPU, because `LinkJoin::read_parent` decodes all 1.5 million `o_orderpriority` values before the first child row arrives. q09 spends 306 ms building the tree, most of it decoding links, before any row moves. The gather is cheap. Everything around it is proportional to the parent, which is what a hash join also pays, so the link join inherits the cost it was supposed to remove.
- Reduction works and the join above it does not use it. On q09 the part filter keeps 10,664 of 200,000 parts and the key map pushes that into the `lineitem` scan, which then emits 319,404 rows instead of 6 million. The joins above it still build hash tables over all 1.5 million orders and all 800,000 `partsupp` rows, 400 ms between them, for 319,404 probes.

**Two structures are tied to the load order and break when it changes.** On the clustered file:

- q18 goes from 0.82 to 2.17. Its inner aggregate groups `lineitem` by `l_orderkey`, and the closed groups rule of `../../crates/rudb-opt/src/cluster.rs` fires only when the summary says the column is ascending. Clustered by date, every order's lines are still together, so every group still closes, but the column is no longer ascending and the rule never fires. What the rule needs is that each value's rows are contiguous. Ascending is one way to prove it and it is not the only one.
- The `lineitem(l_orderkey) -> orders(o_orderkey)` link is lost. The dense key map form of section 2.2 answers a key's rid as its rank, which is only right when the keys are stored ascending. Clustered by date they are not, so the build falls to the sorted form at 8.25 MB, the budget refuses it, and no link is written. The key map should answer the same question whatever order the rows are stored in.

**What is left is spread thin.** A profile of all 22 queries on the base file puts no symbol above 7 percent. The largest are the sideways domain test at 6.1, allocation and copying at about 15 between `malloc`, `memset`, `memmove` and the kernel clearing fresh pages, decoding at about 9 for values that a filter then throws away, and `read` copying file pages at about 5. Those are real and each is its own piece of work, but none of them is a factor of ten, and the sixty documents in `../perf/` are the record of what chasing them one at a time buys.

## 12.4 From first principles

The only way to be ten times cheaper than an engine that already runs vectorized over compressed columns is to touch a tenth of the data it touches. Nothing done per row gets there. So the question for each query is which rows it has to touch at all, and there are three places that decides.

1. **Where the rows are.** A filter on a column the rows are stored in order of reads the parts that hold the answer and no others. That is decided when the file is written and not when the query runs.
2. **Which rows survive the joins.** Exact reduction through key maps and links, which exists and works, as q09's 319,404 rows show.
3. **What a surviving row costs to join.** A gather per survivor, reading only the parts of the parent it lands in, instead of a table over every parent row.

The third depends on the first. A child stored in its parent's order has monotone links, its gathers walk forwards through the parent, and the parts a filtered child touches are the parts of the parents it belongs to, which is a range and not a scatter. So the order of the file is not a tuning knob beside the graph layer. It is the thing that makes the graph layer's structures small and its gathers local, and it is the precompute the rest of this series assumed without writing down.

## 12.5 The decision

**The stored order becomes a committed, declared precompute.** A table may have a declared order: a list of its own columns, or its parent's order through a declared relationship. The writer puts the rows in that order at checkpoint, and every structure the writer builds afterwards, zone maps, summaries, key maps and links, is built over rows in that order. The declaration is a setting in the manner of `graph_links`, `graph_order`, because this layer adds no SQL, and what the setting is when a checkpoint runs is what the file records. A child ordered through its parent is sorted by its parent's rid and then by its own row id, so that its lines keep the order they arrived in within each parent.

This is not tuned to the queries. It is chosen per table, once, from the schema, and it is the same choice a data warehouse makes with a sort key. TPC-H permits it on date columns and every columnar engine with a sort key uses it.

**Order stops being an accident that structures depend on.** Two facts and one form:

- **Grouped.** A column is grouped when each of its values occupies one contiguous run of rows. Ascending implies it, and so does a forward link in the monotone form that every child row followed to exactly one parent, since that form is only taken when the children are in their parents' row order and the parent key is distinct. The writer cannot prove it on the summary pass for a column like `l_orderkey`, because the distinct sketch stops being exact after a few thousand values and a set of every value seen is the size of a key map, but the link build already reads the column in row order against a key map and the form it picks is the proof. The closed groups rule reads either proof, and the executor closes a run with a different value on both sides of it without asking the chunk to go up.
- **The permuted key map.** A key map over distinct keys that are dense in their range but not stored in key order: the dense form's bitmap and rank, and a permutation from rank to rid. 0.78 MB of bitmap and 3.9 MB of permutation for `o_orderkey` on the clustered file, half the sorted form, and it answers in constant time.
- **The relationships the file holds are the relationships the planner sees.** A link the file verified and kept is a fact about the file, and a session that did not declare it still gets it. `graph_links` becomes what to build at the next checkpoint and stops being what to read on every statement.

**The link join reads the parent by part.** A gathered column holds the parent's parts that were touched, decoded when first touched, and no part that no survivor reached. The link is read the same way. A link join over 30,988 children then costs about 30,988 gathers and the parts they land in, which on a clustered file is a few hundred.

**Reduction leads to the link join.** A join whose child side was reduced through a key map and whose relationship has a kept link takes the link join whatever size the parent is, because the reduction already made the parent side sparse. The size rule of section 6.4 is about a hash table over the whole parent, and after a reduction there is no reason to build one.

## 12.6 The order of work

Each item is measured against section 12.2 before it merges, on both files. The expected numbers are what the section 12.2 table implies and are there to be checked, not believed.

1. **Grouped.** A monotone total link proves it, the planner reads the form off the link's header, closed groups reads it. Expected: q18 on the clustered file with links from 2.17 back to about 0.8, and nothing else moves.
2. **Permuted key map.** Expected: the `lineitem -> orders` link kept on the clustered file.
3. **`graph_order` and the ordered checkpoint.** Expected: loading the base rows and declaring `orders(o_orderdate)` and `lineitem` through `orders` gives the clustered column of section 12.2 without an external sort, so about 10.3 for the suite with item 1 in.
4. **Relationships from the file.** Expected: the 20 million per statement gone, so the layer-on column is never above the layer-off one on q02 and q11.
5. **Link join by part.** Expected: q12 with links at or under q12 without them, and `building the tree` on q09 under 10 ms.
6. **Reduction leads to the link join.** Expected: q09's two big hash builds gone, and q09 under 1.0 on the clustered file.

After those six the table gets measured again and this section gets the next list. Section 12.3's last paragraph is where that list is likely to come from, and it will come from a profile rather than from this document.
