# 1. What the literature has settled

Surveyed 18 September 2026. Every claim in this document is attributed, and where a number appears it is the number the paper reports rather than one this project measured. Section 1.9 says which four ideas rudb takes and which it declines, and it is the only section with an opinion in it.

## 1.1 The system that proved the shape: Kùzu

[Kùzu](https://www.cidrdb.org/cidr2023/papers/p48-jin.pdf) (Feng, Jin, Chen, Liu, Salihoğlu, CIDR 2023; [tech report](https://cs.uwaterloo.ca/~ssalihog/papers/kuzu-tr.pdf)) is an embedded, disk-based, columnar property graph DBMS with a vectorized morsel-driven processor, which is to say it is architecturally the same kind of object as rudb with a different query language on the front. Its storage is explicitly described as disk-based versions of columnar in-memory designs: node properties in plain column files, and edges double-indexed in compressed sparse row adjacency structures that the paper calls join indices, with edge properties in parallel CSR structures beside them.

Two design goals drive the processor and they are in tension. Intermediate results of many-to-many joins should stay factorized, meaning represented as a Cartesian product rather than flattened into redundant tuples. And scans should stay sequential, meaning the engine should never be reduced to random reads to fetch a neighbour's properties. Relational systems get the second by hashing and lose the first; native graph systems get the first with index nested loop joins and lose the second. Kùzu's answer is the ASP-Join (accumulate, semi-join, probe), which performs a worst-case optimal multiway join by intersecting several adjacency lists at once through nested hash tables while keeping the scans sequential.

The project was archived on 10 October 2025 after the team was acqui-hired by Apple; the MIT-licensed code and the papers remain, and the [2025 graph database survey](https://arxiv.org/pdf/2505.24758) and [NaviX](https://arxiv.org/pdf/2506.23397) both describe the storage design in more detail than the original paper does. That history is relevant only in that it makes Kùzu a specification to learn from rather than a moving target to chase.

## 1.2 The paper the file format is derived from

[Columnar Storage and List-based Processing for Graph Database Management Systems](https://www.vldb.org/pvldb/vol14/p2491-gupta.pdf) (Gupta, Salihoğlu et al., PVLDB 14) is the closest thing in the literature to what document 03 specifies, and it is the one to read before arguing with it.

Its results that matter here. Edges of single cardinality, one-to-one and many-to-one, which is every foreign key in TPC-H, should be stored in a vertex column rather than in the structures built for many-to-many, because a column is one value per row and an adjacency list is a length and an offset and a payload. Double-indexed property CSRs give sequential access in both directions and pay for it by duplicating every edge property; single-directional property pages avoid the duplication and accept random access in the reverse direction, and the paper argues for the second. Null values and empty lists are compressed with Jacobson's bit vector index rather than stored. And the processor is list-based rather than block-based specifically to avoid the data copies a block-at-a-time processor performs under many-to-many joins.

rudb takes the first and the third of those directly. It declines the fourth, for the reason in section 1.5.

## 1.3 The same idea, already applied to DuckDB

[Making RDBMSs Efficient on Graph Workloads Through Predefined Joins](https://www.vldb.org/pvldb/vol15/p1011-jin.pdf) (Jin, Salihoğlu, PVLDB 15, and [GRainDB](https://www.cidrdb.org/cidr2022/papers/p57-jin.pdf) at CIDR 2022) is the experiment that most directly predicts what this layer is worth, because it was performed inside DuckDB.

The mechanism: users predefine an equality join between two tables, which materializes row ids into extended columns on those tables and optionally builds a RID index, stored in CSR format, over them. The design choice worth noting is what the index is used for. It is mostly **not** used to perform the join by lookup. It is used inside a hash join to generate a semi-join filter that is passed to the scan by sideways information passing, which keeps the scan sequential. On LDBC SNB this closed most of the gap between DuckDB and GraphflowDB. On TPC-H the authors report improvement but say plainly that no large win should be expected, because TPC-H lacks selective many-to-many joins.

That last sentence is the single most important caution in this directory and document 09 section 9.6 is written against it. The honest reading is that predefined joins alone do not deliver 10x on TPC-H, and that anything claiming otherwise has to explain what it does that GRainDB did not. rudb's answer is section 1.6, exact bitmaps make full reduction affordable, which is a different mechanism from a semi-join filter on one edge, and it is a claim that document 09 has to falsify or confirm by measurement rather than by argument.

## 1.4 Worst-case optimal joins, and why they are not the headline

The [WCOJ line](https://arxiv.org/abs/1803.09930) (Ngo, Porat, Ré, Rudra, and the survey) joins one variable at a time instead of two relations at a time and is asymptotically better on cyclic queries. The practical finding, repeated everywhere, is that WCOJ loses to binary hash joins on the acyclic queries that actually occur, which is why systems that support it use it only on the cyclic subpart and end up maintaining two optimizers.

[Free Join](https://arxiv.org/abs/2301.10841) (Wang, Willsey, Suciu, SIGMOD 2023) is the unification: one plan type that generalizes both binary and generic join plans, derived by transforming a conventional optimizer's binary plan, with a column-oriented lazy trie (COLT) that makes the trie building cost close to a hash table's, plus a vectorized execution algorithm. It is implemented in Rust and it matches or beats both paradigms. HoneyComb (Wu, Suciu, SIGMOD 2025) is the multicore parallel WCOJ, *New compressed indices for multijoins on graph databases* (Arroyuelo, Barisione, Fariña, Gómez-Brandón, Navarro, Information Systems 137, April 2026) is the succinct-index direction, and the [PODS survey](https://dl.acm.org/doi/10.1145/3196959.3196990) is the theory entry point. No URL is given for the two that this project has not fetched a public copy of, rather than a guessed one.

For TPC-H this is nearly irrelevant: TPC-H is acyclic. For JOB, CEB and LDBC it is not irrelevant. Document 05 section 5.7 schedules a multiway intersect for the cyclic case and document 10 puts it last, because the ordering of work should follow the workload that the goal document names first.

## 1.5 Factorization, and the 2026 result that makes it implementable

Factorized representations keep a many-to-many intermediate as a product instead of a flattening. The GDBMS line, Graphflow, Kùzu, implements it as list-based processing over factorized vectors aligned to adjacency lists.

[FFX](https://arxiv.org/html/2609.09002) (*Factorized and Vectorized Execution*, 2026) is the paper that changes the cost of adopting this. Its criticism of list-based processing is that it supports only restricted factorization layouts and produces sparsely populated vectors with high interpretation overhead. Its alternative is to implement a factorized vector as **an additional vector type inside an otherwise ordinary DuckDB-style vectorized engine**: a contiguous packed representation with an offset array encoding parent-to-child groupings, a bit-array selector for live positions, and a state with start and end bounds. Non-expanding joins share state between input and output vectors and touch only the selector; expanding joins populate the offset array; a cascade update operator propagates invalidations up and across the hierarchy. Reported: a mean 2.08x over flat execution on an internal workload, up to 9.39x on branched factorization trees, 1.35x to 3.59x fewer cycles, and 1.46x to 1.66x even where factorization itself buys nothing. Limitations it states: acyclic join graphs for the guarantees, no outer or anti joins in the prototype, and reduced gains under low-selectivity predicates.

This is the result that settles rudb's approach. rudb's vector already has seven bodies, flat, constant, sequence, dictionary, packed, views, external text, and execution on encoded data is already the engine's stated default rather than an optimization. A factorized vector in rudb is therefore not a new execution model. It is an eighth body. Document 08 is that argument in detail and it is why this directory does not adopt list-based processing.

## 1.6 Information passing: from filters to an algorithm

Four papers, in the order they build on each other, because rudb's execution design is the fifth step.

[Predicate Transfer](https://arxiv.org/abs/2307.15255) (Yang, Zhao, Yu, Koutris, CIDR 2024) generalizes probe-side Bloom filtering from one join edge to the whole join graph: build a Bloom filter per edge, run a forward pass and a backward pass over a DAG derived from the join graph, and every table's scan is then filtered by the transitive consequence of every predicate in the query. *Accelerate Distributed Joins with Predicate Transfer* (PACMMOD 3(3), June 2025) extends it.

**Robust Predicate Transfer** (SIGMOD 2025) identifies the flaw: PT picks its DAG by a heuristic that orients each edge from the smaller table to the larger, which does not guarantee full reduction, so it does not inherit Yannakakis' guarantee for acyclic queries. RPT's LargestRoot builds a maximum spanning tree on the weighted join graph to warrant full reduction, and SafeSubjoin checks that a join order is within a constant factor of optimal when the query is not γ-acyclic. Integrated into DuckDB and measured on TPC-H, JOB, TPC-DS and DSB, it improves join-order robustness by orders of magnitude.

[Parachute](https://www.vldb.org/pvldb/vol18/p3299-stoian.pdf) (Stoian et al., PVLDB 18) attacks the remaining cost, which is that PT and RPT need extra passes over the data. It precomputes join-induced fingerprint columns on foreign-key tables, parachute columns, so that a predicate on a primary-key table can be evaluated directly against the foreign-key table's scan, chaining across several joins so a filter on table A prunes table C. On JOB with DuckDB 1.2 and a 15% space budget it reports 1.54x without semi-join filtering and 1.24x with it. It depends on primary-key/foreign-key structure and says so.

[Yannakakis+](https://qichen-wang.github.io/files/yannakakis+.pdf) and the ML-gated [selective application](https://ceur-ws.org/Vol-4186/paper2.pdf) work are the cost-based and the learned versions of the same decision, when to pay for the reduction, with the latter reporting 4.4x end-to-end on DuckDB over a hard-join workload.

The gap rudb fits into: every one of these approximates set membership with a Bloom filter because the join keys are arbitrary values. **If the key is a row id, membership is a bitmap.** A bitmap over a hundred and fifty million parent rows is eighteen megabytes, it is exact, and an exact reduction is a full reduction, which is what Yannakakis' guarantee requires and what RPT had to construct a maximum spanning tree to approximately recover. Document 05 section 5.4 is that argument and document 09 section 9.4 is the ablation that tests it, including against the case the argument ignores, which is that a random bit test over eighteen megabytes is not free.

## 1.7 Making stored links survive updates

The relevant storage question is not how to build a CSR but how to keep one when rows arrive. [LiveGraph](https://dl.acm.org/doi/10.14778/3384345.3384351) (PVLDB 13(7), 2020) is the transactional baseline with purely sequential adjacency scans. [BACH](https://www.vldb.org/pvldb/vol18/p1509-miao.pdf) (PVLDB 18) bridges adjacency lists and CSR with LSM trees for hybrid transactional-analytical graph workloads and flushes with key-value separation into a densely encoded columnar layout, and its framing of the tradeoff is the useful part: a predicate on an edge property is a sequential scan in CSR and four random I/Os in an adjacency list, and the comparison reverses under update. *Bw-Graph: An Efficient Graph Storage System Harmonizing Topology-Aware Tree with Paged CSR* (PACMMOD 4(3), May 2026) pairs a topology-aware tree with a **paged CSR**, which is the structure closest to what document 07 needs. [Aster](https://arxiv.org/pdf/2501.06570) is the LSM alternative, [RadixGraph](https://arxiv.org/pdf/2601.01444) (2026) and VCSR are the mutable-CSR structures, and *A Comprehensive Survey on Dynamic Graph Processing: Storage and Analytics* (IEEE TKDE 38(5), May 2026) maps the space.

rudb's position is that it does not have this problem yet and should not adopt a solution to it yet. An analytical engine whose file is written by append and rewritten by checkpoint can treat a link section as immutable per stripe and rebuild per stripe, which document 07 specifies. The paged-CSR work is the thing to return to when transactional update rates make that false.

## 1.8 Robustness, which is the other reason to do this

[Debunking the Myth of Join Ordering](https://arxiv.org/pdf/2502.15181) and [One Join Order Does Not Fit All](https://arxiv.org/pdf/2510.25684) both argue that on modern hardware with modern information passing, the returns from better join ordering are smaller than the returns from making the plan's cost less sensitive to the ordering. [Optimizing Queries with Many-to-Many Joins](https://arxiv.org/pdf/2412.16323) covers the case TPC-H does not have and JOB does.

This matters to rudb specifically because `crates/rudb-opt/src/lib.rs` is 461 lines and there is no cost model and no join reordering in it. A design whose performance depends on getting join order right is a design that depends on a component that does not exist. A design whose performance comes from reducing every table to its contributing rows before the joins run is much less sensitive to the order they then run in, and that is the property to buy first given what the codebase actually contains.

## 1.9 What rudb takes

**Taken: row ids as the join key, and links stored in the file.** From GRainDB and Kùzu. Document 02 and document 03. This is the foundation and nothing else in the directory works without it.

**Taken: the exact bitmap replacement for the Bloom filter, used to get full semi-join reduction.** From the predicate transfer line, extended by the observation in section 1.6. Document 05 section 5.4. This is the part that is not in any of the cited papers, which means it is also the part most likely to be wrong, and document 09 treats it as a hypothesis with a named falsification.

**Taken: factorization as an additional vector body, not as a new processor.** From FFX. Documents 05 and 08.

**Taken: single-cardinality edges live in a column, and nulls and empty lists are compressed rather than stored.** From PVLDB 14. Document 03.

**Declined: list-based processing.** It would fork the execution engine, and FFX shows the fork is unnecessary.

**Declined: double-indexed property CSRs.** Duplicating edge properties to buy sequential access in both directions is the wrong trade for a workload whose backward traversals are mostly aggregations that can be expressed as a forward pass instead.

**Declined for now: worst-case optimal multiway joins, mutable CSR structures, and a graph query language.** Scheduled in document 10 at G7, deferred in document 07, and not planned respectively.
