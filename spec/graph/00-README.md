# The graph layer

Written 18 September 2026, against rudb 0.3.33.

This directory is the design for the part of rudb that makes a join stop being a search. It is not a graph database bolted onto a SQL engine, and it does not add a query language. The thesis is narrower and, if it holds, larger: **an equi-join between two tables in a database is a function between two sets of row identifiers, that function is fixed until the data changes, and an engine that stores it does not have to recompute it on every query.** A hash join recomputes it. A graph engine stores it. Everything in these eleven documents follows from that sentence.

The reason to write this now, rather than after the hash join lands, is in `rudb-bench`. Today `crates/rudb-bench/src/engine.rs` refuses the whole TPC-H suite with a sentence that says every join in rudb is a nested loop and twenty two of them would be a timing of a hang. The obvious fix is the hash join in `../engine/08-join.md`, and that fix is necessary and is scheduled and is not in question. But a hash join is what DuckDB has. Building the thing the competitor already has, well, gets parity with the competitor, and the goal in `../02-the-goal.md` is ten times DuckDB, which parity does not reach. The 10x on the join workloads has to come from doing less work rather than from doing the same work faster, and the way to do less work on a join is to have already done it.

## What the layer actually is

Three things, in the order they have to exist.

**A dense identifier space.** Every row of every table in a rudb native file already has a position: the file is stripes of sixty four parts of a thousand rows, appended in order, so a row has an ordinal and the ordinal is stable until the file is rewritten. Document 02 promotes that ordinal to a first class object, the row id, and defines the maps between a table's declared key and its row ids. Once a key is a row id, a set of keys is a bitmap rather than a hash set, and that single change is what makes the rest cheap.

**Stored links, in the file.** Document 03 adds two section kinds to the native format, both of them caches and neither of them the source of truth. A forward link is a column, not in the SQL schema, that holds for each row of the child table the row id of its parent, which is the many-to-one direction of a foreign key. A backward adjacency is a compressed sparse row structure over the parent's row ids giving the child rows that point at it, which is the one-to-many direction. When the child table is physically clustered by the parent, which for TPC-H `lineitem` against `orders` it is, both directions collapse into one monotone bit vector with rank and select, and the whole join index for six hundred million `lineitem` rows against a hundred and fifty million `orders` rows is about a hundred megabytes rather than the two gigabytes a naive array of parent ids would take. Document 03 does that arithmetic properly.

**Execution that uses them.** Document 05. A join whose link exists is not a build and a probe. It is a gather, which is one random read per output row into a parent page that is very often already resident, expressed as a vector body so that a parent column nobody projected costs nothing at all. A filter on the parent side becomes an exact bitmap over parent row ids, pushed into the child's scan, where it is one bit test per row with no false positives. That last property is the one that turns an approximation into an algorithm: Bloom filter based predicate transfer reduces most of the dangling tuples, and an exact bitmap reduces all of them, which means the forward and backward passes give the full semi-join reduction that Yannakakis' algorithm asks for at the cost of one sequential pass per join edge.

## What this is not

It is not a second storage engine. Every structure here lives in the same single file, written by the same writer, read through the same buffer path, checksummed the same way, and committed by the same two-generation header. Document 03 states the invariant that governs the whole directory: **deleting every graph section from a rudb file must change no answer to any query, only the time it takes.** That invariant is what makes the layer testable, because it means every query has a reference execution sitting right next to it, and it is what makes the layer safe to ship incrementally, because a wrong or stale index degrades to the path that was going to run anyway.

It is not Cypher, it is not a property graph API, and it adds no non-DuckDB SQL. rudb's compatibility axis forbids inventing syntax, so relationships are declared with the `PRIMARY KEY` and `FOREIGN KEY` clauses DuckDB already accepts and which rudb has to parse anyway, or they are inferred from the data and from the queries that ran, which is document 02 section 2.5. A user who never declares anything still gets the layer, later and less completely.

It is not a bet against the hash join. Document 05 keeps the hash join as the general case and the nested loop as the non-equi case, and the planner in document 06 has to prove a link path is cheaper before it takes one. The floor from `../02-the-goal.md`, no query slower than DuckDB, ever, applies to this layer with no exemption, and a layer that can only be fast when its index exists is a layer that has to be honest about when it does not.

## The documents

| | |
| --- | --- |
| [01-research-2026.md](01-research-2026.md) | What the graph systems literature has settled, what it has not, and which four ideas rudb takes |
| [02-the-data-model.md](02-the-data-model.md) | Row ids, key maps, how a relationship gets declared or inferred, and what a null does |
| [03-the-file-format.md](03-the-file-format.md) | Native format v11: section table, key map, forward link, backward CSR, the monotone case, sizes |
| [04-in-memory.md](04-in-memory.md) | What is resident, what is paged, the bitmap type, memory accounting and eviction |
| [05-execution.md](05-execution.md) | Link join, exact semi-join reduction, full reduction, factorized expansion, multiway intersect |
| [06-the-optimizer.md](06-the-optimizer.md) | When the graph path is taken, the cost model, transfer scheduling, and the never-slower floor |
| [07-maintenance.md](07-maintenance.md) | Building, updating, invalidating, checkpointing, and what a transaction sees |
| [08-vector-engine.md](08-vector-engine.md) | The two new vector bodies, the kernels that change, parallelism, spill, and why none of it forks the engine |
| [09-measurement.md](09-measurement.md) | What is measured, against what, and the ablations that have to be run to claim any of this |
| [10-milestones.md](10-milestones.md) | G1 through G8, each with an exit measurement |
| [11-open-questions.md](11-open-questions.md) | The seven things this design does not know |
| [12-the-order-the-suite-asks-for.md](12-the-order-the-suite-asks-for.md) | The whole suite measured with the layer on, what is in the way, and the order of work that follows |

The benchmark half lives in [`../bench/tpc-h/`](../bench/tpc-h/), which is where the workload that justifies all of this is specified, including the harness changes that let it be measured before it is fast.

The statistics half lives in [`../stats/`](../stats/), which generalises three things this directory invents. The section table of document 03 carries statistics sections as well as graph ones. The four exact numbers per relationship of document 06 section 6.2 become a catalogue of exact, certified and estimated facts about every column, so that queries with no join in them get something back for the bytes. And the runtime observations this directory's reduction gate already takes become a bounded feedback loop with a determinism rule. The degree distributions and validity certificates that `../stats/07-graph-statistics.md` specifies are computed inside the link build here, and they are what lets the plan-time approximation in document 06 section 6.4 be replaced by a measurement.
