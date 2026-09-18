# The research, as of September 2026

`../01-research-2026.md` surveyed the field in March and `../engine/01-survey.md` re-checked the engines in September. This document does neither. It reads the last eighteen months of planner and execution work with one question: which of these results changes where a decision should be made in rudb, and which of them is a faster way to do something we were going to do anyway.

Results are grouped by the mechanism they affect. Each section ends with what rudb takes, and section 2.11 is the refusals.

## 2.1 Join ordering matters less than it used to, and the reason is provable

The starting point is Zhao, Su, Yang, Yu, Koutris and Zhang, [Debunking the Myth of Join Ordering: Toward Robust SQL Analytics](https://arxiv.org/abs/2502.15181), SIGMOD 2025, PACMMOD 3(3). Their target is Predicate Transfer, which builds Bloom filters instead of full hash tables to approximate the semi-join reduction phase of Yannakakis' algorithm. Their finding is that Predicate Transfer as published does not inherit Yannakakis' guarantee, because the transfer schedules it generates do not always produce a full reduction.

They fix it with two algorithms, LargestRoot and SafeSubjoin, and call the result Robust Predicate Transfer. The guarantee they recover is the one that matters: intermediate result sizes are at most n times the output size, where n is the number of joins, against baselines where the blow-up is quadratic or worse and grows exponentially with table count. They implemented it inside DuckDB, fixed the join order to whatever DuckDB's own optimizer produced, and measured robustness factors across both left-deep and bushy plans, plus fifty random join trees rooted at the largest relation.

Their own conclusion, which is the sentence this folder is built around: with RPT in place, join order optimization is no longer a critical challenge for acyclic queries, and a future optimizer could limit its search space to left-deep plans, or pick a random join order, and remain tolerant of cardinality estimation error.

Alongside it, Wang et al., [Yannakakis+: Practical Acyclic Query Evaluation with Theoretical Guarantees](https://qichen-wang.github.io/files/yannakakis+.pdf), which attacks the constant factor that has kept Yannakakis out of production for forty years. Their headline experiment is a five-copy SF100 TPC-H query in DuckDB: DuckDB's own plan takes 488 seconds, the textbook Yannakakis plan takes 21.1 seconds, and their plan takes 13.2 seconds. Their plan uses three semi-joins where the textbook one uses ten, with three aggregation-join operations pushed ahead of the semi-joins, and cuts total intermediate results from roughly 556 million to 243 million. Across their full evaluation they report better performance than the native plan on 160 of 162 queries with an average speedup of 2.41x, on a prototype that emits standard relational operators so it plugs into DuckDB, PostgreSQL, SparkSQL and AnalyticDB.

The counterweight is honest and old: Gottlob et al. measured that classic Yannakakis improves the tail but increases average runtime by about 2.4x against ordinary binary joins, because the reduction passes are not free. Every practical variant since is an attack on that constant.

**What rudb takes.** The strategic position. Join ordering gets a bounded search and a left-deep default, and the engineering effort goes into reduction instead. Document 06.

## 2.2 A production optimizer was already doing most of it by accident

Zhao, Tian, Alotaibi, Ding, Bruno, Camacho-Rodríguez, Papadimos, Cervantes Juárez, Galindo-Legaria and Curino, [I Can't Believe It's Not Yannakakis: Pragmatic Bitmap Filters in Microsoft SQL Server](https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf), CIDR 2026.

The observation is that SQL Server's hash joins build a bitmap alongside the hash table and push it down into the probe-side subplan, and that chaining such hash joins cascades the bitmaps through a multi-join before any probing happens. The authors' claim is that the first two execution stages of that arrangement essentially correspond to the bottom-up pass of Yannakakis. They also claim the cost-based framework is doing real work here, surfacing pre-filtering opportunities that the academic line of work had overlooked, and that theirs is the first formal analysis of a phenomenon that is probably present in every engine that ships Bloom filters and semi-joins.

Two things follow for rudb. The first is that the mechanism is not exotic and does not require a new operator: a bitmap built next to a hash table and pushed down is the ninety percent case. The second is a caution, which is that the paper is about bitmaps, and a bitmap is approximate. It reduces most dangling tuples and not all of them, which is exactly the gap between Predicate Transfer and Yannakakis that section 2.1 is about.

**What rudb takes.** Build the filter next to the hash table and push it down, as the general case. And notice that `../graph/` gives rudb something the paper cannot have, which is section 2.3.

## 2.3 rudb can make the reduction exact, and almost nobody else can

This is the only section in this document where the relevant prior art is our own.

`../graph/05-execution.md` section 5.4 specifies a filter that is an exact bitmap over parent row ids, pushed into the child's scan, one bit test per row, no false positives. The reason rudb can do this and a general engine cannot is that `../graph/02-the-data-model.md` promotes the file's row ordinal to a first class row id and `../graph/03-the-file-format.md` stores the forward link, so a set of qualifying parent keys is a bitmap over a dense id space rather than a hash set over values.

Bloom filter based transfer reduces most dangling tuples. An exact bitmap over row ids reduces all of them. Which means the forward and backward passes give the *full* semi-join reduction Yannakakis asks for, at the cost of one sequential pass per join edge, without the hash table that makes a semi-join expensive and without the false positive rate that makes a Bloom filter approximate.

Stoian et al., [Parachute: Single-Pass Bi-Directional Information Passing](https://www.vldb.org/pvldb/vol18/p3299-stoian.pdf), PVLDB 18, is the closest published thing and it is instructive about the cost. They precompute join-induced fingerprint columns on foreign key tables, which they call parachute columns, and report 1.54x and 1.24x end to end on JOB against DuckDB v1.2 without and with probe-side filtering, using fifteen percent extra space and only one-hop columns. Fifteen percent extra space for 1.24x. `../graph/03-the-file-format.md` does the arithmetic for the link structures and gets about a hundred megabytes for six hundred million `lineitem` rows against a hundred and fifty million `orders` rows, because when the child is clustered by the parent both directions collapse into a monotone bit vector with rank and select.

Also relevant and more recent: Qiao, Boncz and Zhang, Robust Predicate Transfer with Dynamic Execution, PVLDB 19(6), 1278 to 1290, online February 2026, which makes the transfer schedule itself a runtime decision rather than a plan-time one. That is the shape document 12 adopts for the abort rule.

**What rudb takes.** Reduction is the primary join mechanism, it is exact where a link exists, approximate where one does not, and its schedule is chosen at plan time and abandonable at runtime.

## 2.4 The compiled versus vectorized question is settled, and the answer is "one IR"

Kersten, Leis et al., [Everything You Always Wanted to Know About Compiled and Vectorized Queries But Were Afraid to Ask](https://db.in.tum.de/~kersten/vectorization_vs_compilation.pdf), PVLDB 11(13), 2018, implemented both models in one system with the same algorithms, data structures and parallelization framework. Vectorization hides cache miss latency better; data-centric compilation executes fewer instructions and therefore wins on cache-resident work. Both are efficient. Neither dominates.

The line of work since has stopped treating that as a choice and started treating it as two back ends over one representation.

- Gubner and Boncz, VOILA, PVLDB 14(6), 2021, 1067 to 1079: a representation well above LLVM IR that can be executed tuple at a time or vector at a time from the same source.
- Gubner and Boncz, [Excalibur](https://www.vldb.org/pvldb/vol16/p829-boncz.pdf), PVLDB 16(4), 829 to 841: a virtual machine that starts fully vectorized and adaptively replaces parts of pipelines with fragments compiled in different flavours. Reported up to 1.8x over Umbra and up to 2x over the best static flavour on specific queries.
- Jungmair and Giceva, [Declarative Sub-Operators for Universal Data Processing](https://www.vldb.org/pvldb/vol16/p3461-jungmair.pdf), PVLDB 16, 2023: operators decompose into sub-operators, sub-operator programs parallelise automatically, and the same program can be compiled data-centrically or executed with precompiled vectorized functions.
- TUM's [Incremental Fusion](https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/inkfuse.pdf) and its prototype InkFuse: factor an arbitrary SQL query into a finite set of building blocks, generate a complete vectorized interpreter for that set ahead of time, and get low latency without a compilation stack. It competes with both DuckDB and Umbra.

The common structure is the one rudb is missing. A small closed set of blocks below the operator level, one program made of them per pipeline, and two or more ways to run that program.

**What rudb takes.** Document 08 is this, adapted. The block set is closed and small, the interpreter over it is tier 0 and stays forever as the differential reference, and the compiler is a second consumer of the same program rather than a second engine.

## 2.5 The compilation back end question has a new answer

`../08-codegen.md` committed to Cranelift on the strength of the CGO 2024 study, which measured LLVM as an order of magnitude slower to compile than a purpose-built single-pass back end, Cranelift as only 20 to 35 percent faster than LLVM, and both as roughly 16x slower than Umbra's own back end.

Since then: Schwarz, Kamm and Engelke, [TPDE: A Fast Adaptable Compiler Back-End Framework](https://arxiv.org/pdf/2505.22610), from the same group. It performs one analysis pass and then does instruction selection, register allocation and encoding in a single pass, driven by an IR-specific adapter plus a semantics specification, with target instructions derived from high-level code through LLVM's Machine IR so porting is cheap. Measured against the LLVM -O0 back end on unoptimized IR from SPECint 2017 it compiles 8 to 24x faster with on-par run time, geometric mean 18.96x on AArch64, back-end time only. The whole framework is 7.7 kLOC of which about 1.4 kLOC is architecture specific across x86-64 and AArch64 together.

Two details matter for rudb specifically. They built a TPDE back end for Cranelift IR inside Wasmtime and it beat Cranelift's own fast compilation mode on both compile time and run time. And they built a compiler for the Umbra database system with it.

That changes the shape of the decision in `../08-codegen.md`. "Write our own single-pass back end" was scoped there as a bounded but real project to be attempted only if Cranelift's compile latency proved binding. TPDE says the bound is about 7.7 kLOC for two architectures, and that most of it is derivable rather than written. It does not change the sequencing, because the first thing to build is still the IR, not the back end over it.

**What rudb takes.** The IR in document 08 is specified so that a single-pass back end is a consumer of it, and document 10 section 10.5 sets the gate that decides between Cranelift and a TPDE-shaped back end on a measurement rather than a preference.

## 2.6 Aggregation: the partitioned default is not obviously right

Xue and Marcus, [Global Hash Tables Strike Back! An Analysis of Parallel GROUP BY Aggregation](https://arxiv.org/abs/2505.04153), PVLDB 19(3), November 2025, 523 to 535.

Analytic engines almost universally partition by key so that every row for a key lands in one partition. The paper revisits the shared concurrent table and finds that the earlier literature dismissing it was measuring the wrong thing: those workloads were update-heavy and did not exploit the lookup-and-insert-only semantics that group aggregation actually has. With a table purpose-built for aggregation, using a ticketing stage followed by partial aggregate update, a fully concurrent table matches or surpasses partitioning in morsel-driven systems across key cardinality, skew and thread count. They also analyse resize cost and memory pressure, which is where partitioning's real advantage lives.

pgEdge attempted the replication in PostgreSQL and framed the key ingredient as moving the group lookup out from under the lock, with the caveat that the shared table is at its best when there are no heavy hitter groups.

Adjacent and still load bearing: Birler et al., Simple, Efficient and Robust Hash Tables for Join Processing, DaMoN 2024, and Fent and Neumann, [A Practical Approach to Groupjoin and Nested Aggregates](https://vldb.org/pvldb/vol14/p2383-fent.pdf), PVLDB 14, which notes that shared aggregation state inside a groupjoin hash table needs synchronisation on parallel update and can produce heavy memory contention.

**What rudb takes.** Grouping strategy becomes a physical plan choice with at least three candidates, not a fixed implementation. Heavy hitter detection comes from the certified frequency synopsis in `../storage-v3/11-certified-frequency-synopses.md`, which is exactly the fact the pgEdge caveat asks for and which rudb already writes. Document 11.

## 2.7 Spilling is an execution decision, not a plan decision

Kuiper, [Saving Private Hash Join](https://www.vldb.org/pvldb/vol18/p2748-kuiper.pdf), CWI, PVLDB 18. Its two claims: the decision to spill should be deferred to execution time rather than taken by the optimizer, and the switch must not be hard, because a hard switch produces a performance cliff. Keep the in-memory strategy as long as possible and degrade continuously.

This is a boundary case for the thesis of this folder, and it is worth being precise rather than ideological. The rule in document 00 is that a decision belongs in the highest artifact that can make it. The optimizer cannot make this one, because whether a hash table fits is a fact about data the optimizer has not read. What the optimizer can and must do is *reserve*, which is a plan-time decision made from facts in `../stats/`, and set the threshold at which the operator changes behaviour. The operator decides when, the plan decides what.

**What rudb takes.** Document 09 section 9.5 states that split precisely, and document 12 lists spill as one of exactly three things allowed to change after a query starts.

## 2.8 Layout specialization is where the order of magnitude is

Restated from `../00-README.md` because everything in documents 07 and 10 depends on it. Wehrstein, Eckmann, Jasny and Binnig, Bespoke OLAP, PVLDB 19(11), 2026, synthesised a C++ engine specialised to one fixed workload and measured 11.17x over DuckDB 1.4.1 on TPC-H and 45.33x on CEB, single threaded and core pinned, plus 7.24x and 9.56x against Umbra.

The ablation is the part to keep. Constrained to a flat struct-of-arrays layout and allowed to specialise only code, the synthesised engine got 1.26x on TPC-H and 0.57x on CEB, meaning it lost. Allowed to specialise the layout too, it got 12.35x and 51.40x.

Code specialization on a fixed layout is worth about a quarter. Choosing the representation is worth an order of magnitude.

**What rudb takes.** Document 07 section 7.5 makes layout requirement propagation a first class pass of the physical planner, not an optimization. A `GROUP BY` over a dictionary column asks for codes and the scan answers with codes, and the decode never happens. This is how rudb gets the layout half of Bespoke OLAP's result without synthesising an engine per workload.

## 2.9 Adaptivity at morsel granularity, and how much of it to believe

[Piece of CAKE: Adaptive Execution Engines via Microsecond-Scale Learning](https://arxiv.org/pdf/2602.04181), arXiv 2602.04181, February 2026. It assumes a morsel-driven engine and treats kernel selection as a learning problem: a kernel that is optimal on average is suboptimal for many individual morsels, because a vectorized kernel pays setup that only amortises on heavy morsels while a branchy scalar kernel is better on light ones. It selects per morsel using policy gradient methods, at microsecond decision latency, and compares against ROQ, BAO, BALSA and Excalibur.

The observation is right and is independent of the learning. A morsel is a natural adaptation unit because it is already the scheduling unit, and the properties that decide the kernel (selectivity, distinct count in this morsel, whether the codes are dense) are cheap to measure on the morsel you are about to process.

The learning is the part rudb cannot take as published, and the obstruction is not performance, it is `../stats/06-the-reward.md` section 6.1: a preference learned from timings makes the plan a function of history, and rudb has committed to the plan being a function of the data. A learned policy also has no answer for the first query an embedded database ever sees, which `../planner/06-cardinality-and-cost.md` section 06.6 already ruled out for a product reason.

**What rudb takes.** Per-morsel kernel selection, with the selection function being a rule over measured properties of that morsel rather than a learned policy over history. Document 10 section 10.4. It is strictly weaker than CAKE and it is deterministic, which is the trade this project has already made twice.

## 2.10 Two smaller results that change specific code

**IN predicates.** Birler and Neumann, [On the Vexing Difficulty of Evaluating IN Predicates](https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf), CIDR 2026. The folklore that `NOT EXISTS` is radically better than `NOT IN` with nullable values turns out to have a complexity reason under it: a `NOT IN` predicate encodes the orthogonal vectors problem, which gives SETH-style conditional lower bounds on evaluating nullable `NOT IN`. Their recommendation to implementers is a specific combination: query decorrelation, mark join operators, and their algorithm for evaluating nullable mark joins.

rudb has mark joins already, and `join.rs` already draws the line between the streamed and gathered paths partly because a mark join asks a question of the whole gathered side per driving row. The paper says that line is in the right place and gives the nullable algorithm to put behind it. Document 11 section 11.7.

**Industrial direction.** Tian, [Query Optimization in the Wild: Realities and Trends](https://arxiv.org/abs/2510.20082), arXiv 2510.20082, v2 March 2026. A position paper from Microsoft. Its three trends are a tighter feedback loop between optimization and execution, an expansion of scope from one query to a workload, and modularization of the optimizer in open lakehouse ecosystems. The first trend is what documents 07 and 12 are. The second is out of scope for an embedded single-node database and rudb declines it. The third is a cloud concern.

Also worth naming because it is a caution rather than a technique: Marcus, Tao, Wu and Zhao, Survivorship Bias in Industrial Database Workloads, CIDR 2026. Workloads that reach a benchmark are the workloads that ran, which biases every benchmark-derived design decision toward queries the incumbent already handled. That is a direct argument for the per-query floor in `../02-the-goal.md` and against tuning to ClickBench totals alone.

## 2.11 What rudb refuses, and why

**Learned cardinality estimation and learned optimizers.** Ruled out in `../planner/06-cardinality-and-cost.md` section 06.6 for a product reason that has not changed: an embedded database has to be reasonable on the first query it ever sees, with no training phase and no model file. `../stats/` reaches the same conclusion from the other direction by making facts exact instead of learning to estimate them better. The 2026 literature here is active and includes CardOOD in the VLDB Journal 35(4) on out-of-distribution robustness, which is a sophisticated answer to a problem rudb avoids having.

**Search-heavy plan enumeration.** Practical MCTS-based Query Optimization, arXiv 2603.16474, and the parameterized query optimization line at SIGMOD 2026 are both answers to "the search space is large and the cost model is trusted". Section 2.1 says the search space stops mattering once reduction is in place, and document 06 spends the budget there instead.

**LLM anything in the plan path.** ReSequel, PVLDB 19(10), and Narasayya and Chaudhuri's CIDR 2026 paper on verifying LLM rewrites with the optimizer are both real work. An embedded library with a per-query floor measured in milliseconds does not have a place to put a model call.

**GPU execution.** Sirius and Theseus at CIDR 2026 are both serious and both are about a hardware envelope rudb has declined in `../04-architecture.md`.

**Pessimistic cardinality bounds.** LpBound at SIGMOD 2025 and the degree-sequence bound work through 2026 are the most intellectually attractive thing in this document, because they give upper bounds rather than guesses, which fits the exact-certified-estimated ladder perfectly. rudb does not build them in v2 for one reason: `../graph/` gives exact join cardinality wherever a link exists, and a proven upper bound is a worse fact than an exact count. Where no link exists the bound would help, and document 15 keeps this open.

## What we should take from this document

Five things change the design, in order of how much.

The 10x on join workloads comes from reduction, not from search, and rudb's version of reduction can be exact rather than approximate because `../graph/` stores row ids.

The layout half of specialization is worth an order of magnitude and the code half is worth about a quarter, so the physical planner's most important job is propagating layout requirements down to the scan.

Compiled and vectorized are two back ends over one IR, and the IR is the thing to build first.

Aggregation strategy and spill strategy are choices with more than one defensible answer, which means they are physical plan decisions and runtime decisions respectively, and neither is an operator's private business.

Everything learned is refused, for the same reason each time: the plan must be a function of the data and not of history.
