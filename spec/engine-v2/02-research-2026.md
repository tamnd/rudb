# The research, as of September 2026

What the literature says, what this engine takes from it, and what it refuses. Ordered by how much it changes the design rather than by date.

The rule for this document: every entry says what the paper claims, what number it claims it with, which seam in [`04-modularity.md`](04-modularity.md) it lands at, and whether we believe it. A survey that does not say where a paper lands in the code is a reading list.

## 1. The papers that decide the architecture

### 1.1 Bespoke OLAP

*Synthesizing Workload-Specific One-size-fits-one Database Engines.* Wehrstein, Eckmann, Jasny, Binnig, TU Darmstadt. VLDB 2026, [arXiv:2603.02001](https://arxiv.org/abs/2603.02001), v2 July 2026.

The claim: a synthesis pipeline generates a C++ engine specialised to a fixed workload, a set of parameterised SQL templates plus a Parquet dataset, and beats DuckDB by 11.78x on TPC-H total runtime and 9.76x on CEB. The mechanism is hard-coded workload-specific columnar layouts and per-template execution kernels. The ablation splits the win: code-only specialisation is 1.26x on TPC-H and 0.57x on CEB, that second number is a *loss*, while layout specialisation is 12.35x and 51.40x.

Why it decides the architecture: it is an existence proof that ten times DuckDB is available, and it says exactly where. It is also the strongest available argument that the usual list of query engine improvements, compilation, operator fusion, prefetching, better kernels, is worth about twenty-six per cent in total. [`../02-the-goal.md`](../02-the-goal.md) already priced the project's own claim on this basis before the 2026 numbers existed; the numbers arrived and they agree.

What we take: the thesis, and nothing else. Specifically not the method. A synthesised one-size-fits-one engine cannot be 100% compatible with DuckDB, cannot answer a query it was not synthesised for, and is not a product. What we take is the observation that the layout is where the factor lives, and the discipline of measuring layout and code separately so that our own ledger has the same two columns theirs does.

Lands at: the whole of [`13-encoded-execution.md`](13-encoded-execution.md), and the F7 gate.

Do we believe it: the direction, yes, strongly. The magnitude, partly. CEB at 51.40x from layout alone suggests the baseline was doing something pathological on that workload, and the general-engine version of a layout win is always smaller than the specialised version because a general engine has to keep the ability to answer the other query. Our own planning number in [`../02-the-goal.md`](../02-the-goal.md) is 9x to 11x with three mechanisms landing and 4x to 6x if any one of them fails, and that remains the honest range.

### 1.2 Unified memory management, and the operator's right to evict

*Robust External Hash Aggregation in the Solid State Age.* Kuiper, Boncz, Mühleisen. ICDE 2024. [PDF](https://duckdb.org/pdf/ICDE2024-kuiper-boncz-muehleisen-out-of-core.pdf).

*High-Performance Query Processing with NVMe Arrays* (Umami). TUM. [PDF](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/umami.pdf).

*Resource-Adaptive Query Execution with Paged Memory Management.* Otaki, Benello, Elmore, Graefe. CIDR 2025. [PDF](https://www.vldb.org/cidrdb/papers/2025/p2-otaki.pdf).

Three papers that together answer the question v1 left open and the firepanda notes argued about. Kuiper's contribution is unifying temporary and persistent data under one manager with a page layout that spills without serialisation overhead, which is what lets a blocking operator use all the memory there is. Umami's contribution is the criticism: DuckDB unpins pages the hash table still references, whereas Umami manages buffers independently per operator so that the operator decides which of its own pages leave, in their aggregation, thread-local hash table bucket ranges get evicted to partitions on the operator's terms. Otaki's contribution is that a memory consumer which knows what is pinned can publish a curve relating memory to cost and let the system move along it.

What we take: all three, composed. One manager owning the total. Per-operator buffer ownership inside it. A published `MemoryCurve` per stateful operator. This is principle 5 and it is the reason larger-than-memory is F3 rather than F8.

Lands at: [`07-memory.md`](07-memory.md).

### 1.3 The unchained hash table

*Simple, Efficient, and Robust Hash Tables for Join Processing.* Birler, Schmidt, Fent, Neumann. DaMoN 2024.

Build-side partitioning into an adjacency array, so the table is a contiguous run per bucket rather than a linked list; Bloom tags in the pointer's spare bits; software write-combine buffers on the build; pipelined probes. Two times the average of open addressing across their workload, up to twenty times on graph-shaped queries with long chains.

*Global Hash Tables Strike Back* (2025) argues the opposite direction for aggregation: a single concurrent global table beats thread-local-plus-merge above a group-count threshold, because the merge is the cost nobody prices.

*Adaptive Factorization Using Linear-Chained Hash Tables.* Groß et al., CIDR 2025. Chains that stay factorised when the build side has duplicates, which is where unchained's adjacency array wins turn into wins.

What we take: all of them, as registered strategies rather than as a decision. This is the clearest case in the whole survey for principle 2. v1 wrote "join uses unchained, aggregate uses open addressing" into the plan. That is very likely correct and it is also exactly the kind of statement that two of these three papers exist to complicate. The tree carries `unchained`, `open-addressing-salt`, `linear-chained`, and `global-concurrent`, plus `hashmap-reference` as the oracle, and the answer is a sweep on `hits` and TPC-H SF100, not a sentence.

Lands at: [`04-modularity.md`](04-modularity.md) seam `hash.table`, implemented in F5 and F6.

### 1.4 Data chunk compaction

*Data Chunk Compaction in Vectorized Execution.* Qiao, Zhang et al. SIGMOD 2025. [PDF](https://people.iiis.tsinghua.edu.cn/~huanchen/publications/data-chunk-compaction-sigmod25.pdf).

Selection vectors make chunks sparse; sparse chunks waste the vector; compacting costs a copy. The paper replaces the fixed threshold every engine uses with an online-learned per-operator threshold driven by a gain function, and adds logical compaction for hash join probe output, where the left side is zero-copy and only the right side has to be gathered. Up to ten per cent end to end in DuckDB.

What we take: the gain function, the per-operator threshold, and the left/right asymmetry in probe output. v1's survey document already identified "chunk compaction is not a constant" as one of its three contradictions with the parent spec, which is the same finding.

Lands at: seam `chunk.compaction`, with `never`, `fixed-threshold`, and `learned-gain` in tree. F1 introduces it; F10 replaces the learner with the CAKE bandit if that turns out to be better on our data.

### 1.5 Piece of CAKE

*Adaptive Execution Engines via Microsecond-Scale Learning.* [arXiv:2602.04181](https://arxiv.org/pdf/2602.04181).

Kernel selection framed as a contextual multi-armed bandit with counterfactual estimation and regret trees, at microsecond decision granularity. Up to two times.

Why it matters more to this design than to most: our principle 2 produces a registry of alternative implementations as a side effect of wanting them swappable for research. CAKE is the algorithm that turns that registry into a runtime win instead of a configuration burden. The registry is built at F0 for the researcher and harvested at F10 for the engine, and the second use costs almost nothing because the first one paid for the structure.

Lands at: [`04-modularity.md`](04-modularity.md) section 6, `Policy::Adaptive`. Milestone F10.

The constraint that keeps it honest, inherited from v1's adaptivity document and not negotiable: adaptation never changes an answer, is bounded and switchable, converges, and is visible in `EXPLAIN ANALYZE`.

## 2. The papers that decide individual operators

### 2.1 Predicate transfer and its relatives

*Predicate Transfer.* Yu et al., CIDR 2024. *Robust Predicate Transfer* / "Debunking the Myth of Join Ordering". *Parachute*, VLDB 2025, precomputed reachability filters, 1.54x on JOB against DuckDB v1.2 for fifteen per cent extra space, filters capped at 8 KiB with m = 2^16 and k = 2. And the 2025 result on pushing Bloom filters into bottom-up join enumeration rather than applying them afterwards: a further 32.8% latency reduction on 100 GB TPC-H.

What we take: Bloom filters pushed into the probe-side scan at F6, because v1's join document is right that this is the largest single win available in that layer, and the sideways-information-passing generalisation at F8 as an optimizer pass behind the rule registry. Parachute's precomputed filters are a storage decision, not an execution one, and land as an optional artefact in [`06-storage.md`](06-storage.md) with the fifteen per cent space cost stated on the tin.

The claim in "Debunking the Myth of Join Ordering", that with robust predicate transfer the join order matters far less, is the most consequential claim here if true, because it would mean the expensive part of [`12-optimizer.md`](12-optimizer.md) is the cheap part. We do not assume it. F8 measures with transfer on and join enumeration off, which is a two-line experiment once both are registered passes.

### 2.2 FastLanes

*The FastLanes Compression Layout.* VLDB vol. 18. Interleaved bit-packing that vectorises without shuffles, and multi-column structures over it.

What we take: the layout, for the integer and dictionary-code paths. This is one of the three mechanisms [`../02-the-goal.md`](../02-the-goal.md) names, and M1's encoder work in `rudb-encoding` is where it goes. It matters most where it is least visible: a bit-packed layout you can compare against without unpacking is what makes [`13-encoded-execution.md`](13-encoded-execution.md) possible for integers.

### 2.3 The GPU line, and why we are not on it

*Rethinking Analytical Processing in the GPU Era* (Sirius). Yogatama, Yang, Yu, McKinney et al., CIDR 2026. [PDF](https://vldb.org/cidrdb/papers/2026/p12-yogatama.pdf). *From Custom-Fit to Portable* ([arXiv:2607.07632](https://arxiv.org/pdf/2607.07632)). *Eiger* ([arXiv:2607.04489](https://arxiv.org/pdf/2607.04489)). *Tailwind: A Practical Framework for Query Accelerators* ([arXiv:2604.28079](https://arxiv.org/pdf/2604.28079)).

We are not building a GPU engine and the board we are measured against is a CPU board. Two things from this line are still ours.

The first is negative and useful: *From Custom-Fit to Portable* observes that on GPUs the residual gap between a synthesised engine and a well-fused portable one is small, because memory bandwidth dominates and a portable engine that saturates it has nothing left to lose. The same argument applies to a CPU engine at the point where it becomes bandwidth-bound, and it is the reason principle 4 is about layout rather than about kernels: bytes moved is the budget, and compression is the only mechanism that reduces it.

The second is the specific one [`../02-the-goal.md`](../02-the-goal.md) already recorded: Sirius gets thirteen times on ClickBench Q28, the `REGEXP_REPLACE` query, and Q28 is 1.393 seconds of Umbra's 8.10. That is not a GPU result so much as a result about not running a general regex engine on a hundred million URLs. It lands as the pattern analyser in [`13-encoded-execution.md`](13-encoded-execution.md) section 5.

### 2.4 Buffer management and I/O

*Predictive buffer management for OLAP scans*, VLDB 2026, eviction that uses knowledge of long-running scans to approximate the optimal policy, with sampling so that eviction examines a small random sample rather than everything. *Virtual-Memory Assisted Buffer Management in Tiered Memory*, [arXiv:2603.03271](https://arxiv.org/pdf/2603.03271), the vmcache line extended to CXL. *Flexible I/O for Database Management Systems with xNVMe*, CIDR 2026. *REMOP: REmote-Memory-aware Operator Optimization*, [arXiv:2606.19576](https://arxiv.org/abs/2606.19576).

What we take: the sampled eviction policy as a registered strategy at seam `buffer.eviction`, alongside clock and LRU, because it is cheap and the benchmark workload is exactly the concurrent-scan shape it targets. REMOP's contribution, that when spilling goes somewhere with a fixed per-transfer latency the cost model must count transfer *rounds* and not just bytes, is not relevant to a local NVMe spill and is extremely relevant the day [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md) section 6 becomes real code. It is recorded there so that the spill cost model has the right shape before it needs it.

We do not take xNVMe or io_uring. Zero external dependencies is a project rule, and v1's scan document already excluded io_uring deliberately. A dedicated I/O thread pool on top of the standard library is what F0 ships and it is enough to saturate the devices in the fleet.

## 3. What the engines themselves did

Not papers, and more useful than most papers, because they are measurements on the workload we care about.

**DuckDB v2.0.** Async I/O threaded through the entire engine rather than bolted onto the scan: a regular pool sized to cores and an async pool at four times the thread count capped at 256, with a read-ahead queue governed by the memory limit. Measured 3.0x to 3.7x on 22 GB of S3 Parquet, 19.4x on an 80.89 GB CSV, and the number that matters to us because it is not a cloud number, 1.5x cold on local files on a laptop. Also: aggregation spilling, partial aggregate pushdown below joins, storage v2.0 with lazy column metadata and `DICT_FSST` by default, and the recursive CTE rewrite at 40x.

What we take: async I/O as a property of the I/O interface from F0, not an optimisation at F5. This was v1's first contradiction with the parent spec and it was right. Partial aggregate pushdown below joins is an optimizer rule at F8. Lazy column metadata is a storage decision at F2 and matters more than it sounds: `hits` has 105 columns and a query that touches three should not parse metadata for 105.

**Polars' streaming engine.** Rust async state machines with exact backpressure through wait tokens and semaphore permits, hybrid push-pull, out-of-core group-by, equi-join, sort and multiplexer, a lock-free memory manager, and a hundred-gigabyte inner join on a sixteen-gigabyte laptop. Also two pieces of hard-won evidence: the categorical rewrite that morsel parallelism forced, and `lower_ir()` partial lowering with permanent in-memory fallbacks that `visualize_plan()` colours red.

What we take: the red. A plan display that shows where the fast path was not taken is the single cheapest honesty mechanism in any engine, and [`14-metrics.md`](14-metrics.md) makes it mandatory, `EXPLAIN` marks every node that fell back to a reference implementation, every column that was decoded, and every operator that spilled. What we do not take is the token-based backpressure, for the reason in principle 6.

**ClickHouse 25.x.** Lazy materialisation on by default, with a measured 219.071 seconds to 139 milliseconds on Amazon reviews cold, forty times less data read and three hundred times lower peak memory. The query condition cache, one bit per filter per 8192-row granule, 0.8 seconds to about 50 milliseconds on a hundred million rows.

What we take: lazy materialisation, everywhere, as a default rather than a feature, it is the same idea as principle 4 seen from the scan. We do not take the condition cache, and v1's methodological rule stands: it is off in every published measurement, because a cache makes a repeated-query benchmark measure the cache.

## 4. What we are not doing, and why

**LLM engine synthesis.** GenDB, SpecDB ([arXiv:2605.31097](https://arxiv.org/pdf/2605.31097)), *Test-Time Optimization of Physical Query Plans with LLMs* ([arXiv:2602.10387](https://arxiv.org/html/2602.10387)), DBPlanBench. The thesis is right and we have taken it. The method produces an artefact that cannot answer an unanticipated query, which disqualifies it for a DuckDB-compatible engine. Worth revisiting only for the write path, where a "compile the storage layout for this dataset" step is a batch job with a correctness oracle and no latency requirement.

**Cascades.** v1 rejected it and the reasoning holds: the per-query planning floor is part of the claim, and the available wins are rewrite wins rather than search wins. A registry of passes with a cost-based join enumerator inside it is what F8 builds.

**Learned cost models, learned indexes, result caching, cross-query learning.** Excluded, same as v1. Every one of them makes a benchmark number less attributable, and the adaptivity we do take is bounded within a query.

**Distributed execution.** Principle 7. Interfaces now, code at F11, and only if the single-node number lands first.

## 5. Where the survey leaves the arithmetic

Unchanged from [`../02-the-goal.md`](../02-the-goal.md), and worth keeping in front of us because nothing in this survey moves it.

Ten times DuckDB on ClickBench is 2.63 seconds. Umbra, the fastest CPU engine anybody has published on that machine, is at 8.10. The target is 3.1x past the state of the art. The Rust and Arrow ecosystem is at 45 seconds, which is 1.7x behind DuckDB rather than ahead of it.

Seven queries are 70.4% of Umbra's 8.10 seconds: Q28, Q32, Q18, Q34, Q33, Q16, Q13. Three technical problems cover them, high-cardinality grouping where the query wants only the top, strings as group keys, and string transformation at scale, and this survey's contribution to that arithmetic is that the 2026 literature now contains an existence proof for the mechanism and no shortcut around it.

Sources: [Bespoke OLAP](https://arxiv.org/abs/2603.02001), [Piece of CAKE](https://arxiv.org/pdf/2602.04181), [Data Chunk Compaction](https://people.iiis.tsinghua.edu.cn/~huanchen/publications/data-chunk-compaction-sigmod25.pdf), [Kuiper et al. external aggregation](https://duckdb.org/pdf/ICDE2024-kuiper-boncz-muehleisen-out-of-core.pdf), [Umami](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/umami.pdf), [Otaki et al. CIDR 2025](https://www.vldb.org/cidrdb/papers/2025/p2-otaki.pdf), [Sirius CIDR 2026](https://vldb.org/cidrdb/papers/2026/p12-yogatama.pdf), [From Custom-Fit to Portable](https://arxiv.org/pdf/2607.07632), [Tailwind](https://arxiv.org/pdf/2604.28079), [Eiger](https://arxiv.org/pdf/2607.04489), [REMOP](https://arxiv.org/abs/2606.19576), [Virtual-Memory Assisted Buffer Management in Tiered Memory](https://arxiv.org/pdf/2603.03271), [SpecDB](https://arxiv.org/pdf/2605.31097), [Test-Time Optimization of Physical Query Plans with LLMs](https://arxiv.org/html/2602.10387), [CIDR 2026 accepted papers](https://www.cidrdb.org/cidr2026//papers.html), [VLDB 2026 program](https://vldb.org/2026/program.html), [SIGMOD 2026 accepted papers](https://2026.sigmod.org/sigmod_papers.shtml).
