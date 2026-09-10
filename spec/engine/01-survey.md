# What the other engines actually do, September 2026

Document 01 of the parent spec surveyed the field in March. Six months later three of the four systems rudb is measured against have shipped material engine changes, and one of them shipped the change that most affects how rudb should be built. This document is the re-check, mechanism by mechanism rather than headline by headline, because a headline is not something you can copy and a mechanism is.

Every number in here is one the vendor published with a machine attached to it. Numbers without a machine are not repeated.

## 1.1 DuckDB v2.0, and the thing it changed

**Asynchronous I/O through the whole engine.** This is the largest single engine change in v2.0 and it is the one that changes what rudb has to build. Until v2.0 a DuckDB worker thread that needed bytes issued a read and blocked. On local NVMe that costs a little. On object storage it costs everything, because the thread is not doing arithmetic while it waits and the network is nowhere near saturated.

The design is two thread pools. A REGULAR pool sized to the core count, which decodes, joins and aggregates. An ASYNC pool sized to four times the system thread count and capped at 256, which only issues blocking I/O. The asymmetry is the point: an I/O thread spends its life blocked on a socket and uses almost no CPU, so having many more of them than there are cores is how the network gets filled.

Above the pools is a read-ahead queue. Work is split into jobs, where a job is an independent unit such as one Parquet row group or one CSV scan boundary, and each job holds a set of fetch tasks which are byte range requests. A regular worker that goes looking for scan work tops the queue up by creating jobs and scheduling their fetch tasks onto the async pool. The fetch tasks of a job share a countdown and the last one to finish moves the job to ready. A worker claims the oldest job, and if the I/O is not done it parks the scan task and goes and does something else in the pipeline, and the last fetch task unblocks the scan task which then resumes on whichever regular worker picks it up.

The queue is memory governed rather than fixed. `read_ahead_depth` defaults to -1, which means unlimited depth negotiated with the temporary memory manager: under memory pressure the queue shrinks toward one job at a time, which is to say back to synchronous scanning, and refills when the pressure goes away. A positive value is a fixed job count with no budget. Zero turns read-ahead off.

The numbers, on an EC2 r7i.16xlarge against same-region S3. A 22 GB Parquet file of about 4,880 row groups: 8.230 seconds on v1.5.5, 2.844 on v2.0-dev, 2.227 tuned, so 3.0x and 3.7x. An 80.89 GB CSV: 877.563 seconds to 45.264, which is 19.4x. Four concurrent TPC-H queries: 35.8 seconds at 5.9 average cores and 10.7 Gbit/s, against 15.6 seconds at 48.1 average cores and 24.9 Gbit/s. That last line is the whole argument in one row. The synchronous engine was using six of sixty four cores and a fifth of the network.

Cold local disk moved too, 1.321 seconds to 0.883 on a MacBook Pro, which is 1.5x. That is the number that matters to an embedded database on a laptop, and it says the change is not only about object storage.

**What this means for rudb.** The parent spec put the I/O layer in document 04 and treated blocking reads as acceptable for M2. That is no longer acceptable, and not because 3.7x is a lot. It is because the comparison stops being meaningful. Any benchmark rudb runs against a v2.0 DuckDB on anything but a hot page cache is now partly a measurement of who overlaps I/O better, and an engine that does not overlap it at all will lose that part by a factor and then be unable to say which factor came from where. Async I/O moves into the scan layer, document 05, and it lands in sub-milestone 2c.

**Recursive CTE rewrite.** A complete rebuild. Single source reachability over a million edges went from 4.90 seconds on v1.5.4 to 0.12 on v2.0, which is 40x, and the reported median on their reachability query went 4.051 to 0.095, which is 42.6x. The mechanisms named are retained hash builds across iterations, per-iteration choice between inline and scheduled execution, and probing directly into keyed state rather than rebuilding it. There is a semantics change with it: under `USING KEY`, `UNION` now makes new keys and keys whose finalized payload changed visible to the next iteration.

That is a compatibility item as well as a performance item. rudb has to match the new visibility rule, not the old one, and the mechanism is worth copying wholesale because it is the difference between a recursive CTE being a toy and being usable.

**Aggregation spilling.** Aggregates that exceed memory now spill instead of failing. Combined with the memory governance on the read-ahead queue, v2.0 is the release where DuckDB stops falling over on large aggregates, which removes one of the easier ways for a competitor to look good.

**Optimizer.** Partial aggregate pushdown below joins. Detection and reuse of duplicate aggregations. Partition-aware planning that exploits DuckLake, Iceberg and Hive-partitioned Parquet layouts. Row group pruning extended to structs, lists, decimals, UUIDs, `IN` lists and function predicates, using both min-max indexes and Parquet Bloom filters. Predicate pushdown into PostgreSQL and MySQL rather than pulling tables across the network.

**Storage v2.0.** Lazy column metadata loading, so a wide table opens without reading every column's metadata. `DICT_FSST` as the default string compression. Compact delete storage. ART index vacuuming by incremental row id remapping at checkpoint rather than a full rebuild. Stricter corruption validation on read. Buffer-managed ART indexes are announced but not in this release, which means DuckDB still requires indexes to fit in RAM and rudb has a window there.

## 1.2 Polars, and the engine that replaced the engine

Polars rewrote its streaming engine and the rewrite is the single most relevant piece of engineering to rudb in this survey, because it is in Rust, it is recent, and it solved the exact problem of making morsel-driven parallelism work with an async runtime without either eating the other.

The design is morsel-driven parallelism from the TUM paper, with morsels of roughly 128k rows, combined with Rust async state machines. Each operator compiles down to a state machine and the compiler does the work that would otherwise be a hand written coroutine per operator. Workers pull morsels from a scheduler. Backpressure is exact rather than heuristic: a morsel carries a wait token or a semaphore permit, so an operator that is flushing a partition to disk can decline to pull more work, and the decline propagates. The engine is described as hybrid push and pull, which is the honest description of what you get when the pipeline is push and the source is pull.

The out-of-core work is the 2026 part. Group-by, equi-join and sort all have spilling implementations wired to a lock-free memory manager, plus an out-of-core multiplexer, and the claim is a 100 GB inner join completing on a 16 GB laptop. Their own warning is the useful part for anybody quoting numbers: the widely circulated out-of-memory comparisons from Polars 1.6.0 in September 2024 predate all of this and describe an engine that no longer exists.

Two second order effects are worth reading because rudb will hit both. First, categoricals had to be rebuilt. Under morsel parallelism each morsel carried its own mapping table, which forced constant syncing and re-encoding, and the alternative of a global string cache introduced locks and pipeline pauses. Their answer was a new categorical representation designed for the parallel engine. rudb's global dictionary plan in M4 is the same problem with the same two bad answers available, and this is prior art on it. Second, they concede that time series operations, rolling windows and window functions in general, need more synchronization than the streaming model gives cheaply, and they fall back to the in-memory engine for those. Document 09 of this directory has to answer that question rather than inherit the fallback.

## 1.3 ClickHouse, and two ideas worth stealing

ClickHouse's execution model is well documented and mostly matches the parent spec. Two recent additions are not in the parent spec and both are cheap to build and large in effect.

**Lazy materialization**, added in 25.4 and on by default. Wide columns in the `SELECT` list are not read until after sorting and the `LIMIT` have been applied. For `select id, big_col1, big_col2 from big_table order by rand() limit 5`, the big columns are read for five rows rather than for all of them. Their published measurement on the Amazon reviews dataset with a cold filesystem cache is 219.071 seconds down to 139 milliseconds, with 40 times less data read and 300 times lower peak memory, against 150.96 million rows and 71.38 GB and 1.11 GiB peak on the unoptimized side.

That is a three order of magnitude result from a rewrite rule, and it is a rewrite rule that a column store can implement in the planner plus one operator. It sits on top of the two layers below it, primary key indexing which prunes rows by the sorting key, and PREWHERE which pushes column filters as deep as possible.

**The query condition cache**, added in 25.3. For each filter expression and each granule of 8192 rows, one bit: zero means no row in this granule matches, one means at least one does. Granules marked zero are skipped entirely on a later query with the same filter. Batching is used so the cache write is not itself a bottleneck. Their Bluesky example goes from scanning about 12,000 granules across 10 large ranges to 168 granules across 73 small ones, and from 32 streams to 18 on a 32 core machine because there is less work to spread. A separate demo on 100 million rows goes from 0.8 seconds to about 50 milliseconds.

One bit per granule per filter is essentially free to store and it is always correct, which is what separates it from a result cache. It composes with a result cache rather than competing: an identical query is served whole from the result cache, and when one literal changes the condition cache still prunes on the unchanged conjuncts. rudb's row group is 122,880 rows against ClickHouse's 8,192 granule, so the same structure at rudb's granularity is coarser and worth less, which means the interesting question is whether rudb wants a sub-row-group unit for filter evaluation. Document 05 takes that up.

A methodological note that rudb's harness has to copy: ClickHouse turned the condition cache off for all the lazy materialization measurements, because a cache of filter results makes a filtering benchmark measure the cache. Any rudb benchmark with a persistent structure like this has to say whether it was on.

## 1.4 DataFusion, and what a Rust engine has already proved

DataFusion 43.0.0 took the top of the ClickBench Parquet leaderboard on a c6a.4xlarge, ahead of DuckDB, chDB and ClickHouse on the same hardware, on the hot run over a 14 GB dataset partitioned into 100 files of about 140 MB. It was the first time a Rust engine held that position. DuckDB took it back in early 2025 and DataFusion has an open epic to take it again.

The single largest contributor to that run was the string representation. Moving to `StringView`, which is the German string layout, took over a hundred pull requests in arrow-rs plus three epics in DataFusion, because the representation change is small and the propagation of it through every operator is not. ClickBench improved more than 30 percent between DataFusion 34 and the later releases.

Two things to take from this. The first is the confirmation that a Rust engine is not structurally disadvantaged, which is worth having stated by somebody other than us. The second is the cost profile of a data plane change: the layout was the easy part and the hundred pull requests were the propagation. That is the concrete form of the claim in document 07 of the parent spec that the vector interface has to be right before there are twenty operators, and it is why the data plane is layer one here.

The DataFusion maintainers' own caveat is worth recording because rudb's target is stated against these numbers. They call topping ClickBench a vanity benchmark, they hold that engines within a factor of two of each other deliver similar user experience, and they refuse ClickBench-specific optimizations. rudb's target is ten times, which is well outside that factor of two, so the caveat does not dissolve the target. It does mean that a rudb release note that leads with a leaderboard position rather than a factor is a release note that has drifted.

## 1.5 The papers that changed a decision

**Simple, Efficient, and Robust Hash Tables for Join Processing.** Birler, Schmidt, Fent and Neumann, DaMoN 2024, from the Umbra and CedarDB group. The unchained hash table combines build side partitioning, an adjacency array layout, pipelined probes, Bloom filter tags and software write-combine buffers. In effect it takes the pointer array of a chaining table, which gives collision resistance and a low false positive rate on the tag check, and puts the dense storage of a linear probing table underneath it. Measured across more than 10,000 queries from five benchmarks on a Ryzen 5950X: 2x on average over open addressing on relational queries, and up to 20x over both chaining and open addressing on graph queries. Their argument against reaching for a general purpose table is specific and correct: hopscotch, SwissTable and F14 are tuned for a hit rate that join probes do not have, and they handle multiset semantics badly because duplicates stored inline turn into collisions.

This decides document 06. rudb builds one unchained table and both the join and the aggregate use it.

**Global Hash Tables Strike Back.** The aggregation counterpart, already cited in document 07 of the parent spec. Thread-local tables plus a merge lose to a shared partitioned table with fine grained synchronization above a group count threshold, because the merge is proportional to groups times threads.

**Adaptive Factorization Using Linear-Chained Hash Tables.** Groß et al, CIDR 2025. A bucket chained table where a colliding different key is resolved by linear probing for cache locality and chaining is used only for records that genuinely share a key, with on-the-fly sketches driving runtime factorization decisions. Relevant to document 07 rather than 06, because the factorization idea is about aggregation over joins.

**Data Chunk Compaction in Vectorized Execution.** Qiao and Zhang, SIGMOD 2025. The problem is that a hash join probe can leave a chunk with very few valid rows, and a pipeline downstream of it then runs vectorized code over chunks of thirty rows. DuckDB's answer is a fixed threshold below which it compacts. The paper's answer is a gain function comparing the cost of the remaining pipeline with and without compaction, learned online, plus a logical compaction that avoids the data movement on the probe side output where the left columns are zero copy and only the right columns would need copying. Implemented in DuckDB it was worth up to 10 percent end to end.

Ten percent is not a factor, but the mechanism matters more than the number, because it is the cleanest published example of the general shape rudb needs: a per-operator decision that has no right static answer and a cheap online rule that finds it. Document 12 generalizes it.

**Predicate Transfer**, CIDR 2024, and the two 2025 follow-ups. Predicate transfer generalizes the Bloom join, which only pre-filters within one join, to the whole join graph, using Yannakakis' semi-join idea with Bloom filters in place of semi-joins. Forward pass bottom up through the join tree filtering each table against its children, then a backward pass. Robust Predicate Transfer, in "Debunking the Myth of Join Ordering", is provably robust against arbitrary join orders on acyclic queries, which is the property that makes it interesting: it converts a join ordering problem into a filtering problem, and join ordering is the thing every optimizer gets wrong.

**Parachute**, VLDB 2025. Single pass bi-directional information passing, by statically finding where information flow is blocked and precomputing join-induced fingerprint columns on the foreign key side. On JOB against DuckDB v1.2 it is 1.54x without semi-join filtering and 1.24x with, at 15 percent extra space. The same paper records what DuckDB v1.2 already does, which is the useful baseline: min-max transfer of key columns for coarse pruning, an equality predicate when the build side has one distinct value, and an `IN` list pushdown for partition pruning when the distinct key count is under 50. Their own Bloom join baseline in DuckDB was 1.26x on JOB, and their filter is capped at 8 KiB so it stays in L1, which is m equal to 2^16 and k equal to 2, good for about 5000 distinct keys at a 2 percent false positive rate.

**Including Bloom Filters in Bottom-up Optimization**, 2025. Placing Bloom filters inside the join enumeration rather than in a post pass, because a filter that crosses an intermediate join reduces the input to several joins at once. 32.8 percent further latency reduction on 100 GB TPC-H over post-pass placement.

**Piece of CAKE: Adaptive Execution Engines via Microsecond-Scale Learning**, 2026. Kernel selection as a contextual multi-armed bandit at microsecond timescale, using counterfactuals by occasionally running more than one kernel to get full feedback rather than bandit feedback, with the learned policy compiled into low latency regret trees. Up to 2x end to end against static heuristics. This is the paper that says the kernel table in document 07 of the parent spec should not be a static dispatch table forever, and document 12 is where that lands.

## 1.6 Where this contradicts the parent spec

Three places, and they are contradictions rather than refinements.

**Async I/O is not optional and not late.** The parent spec's document 04 treats the I/O layer as a file abstraction and leaves overlapping for later. DuckDB v2.0 makes that a measurement problem as well as a performance problem, per 1.1 above. Async I/O moves to sub-milestone 2c.

**The hash table is one structure, not two.** The parent spec's document 07 specifies the aggregate hash table and the join hash table separately, in sections 7.5 and 7.6, with different layouts. The unchained result says they should be the same structure with different payload handling, and two implementations is two sets of bugs and two things to make NUMA aware.

**Chunk compaction is not a constant.** The parent spec's document 07.1 says compaction happens when a measured selectivity threshold is crossed and that the threshold is per operator and measured. The SIGMOD 2025 result says a fixed per-operator threshold is also wrong, because the right answer depends on how many operators remain in the pipeline below the compaction point. The threshold is a runtime decision with a cost model behind it.
