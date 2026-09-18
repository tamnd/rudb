# First principles

Eight of them. Each is stated as a rule, followed by what it rejects, followed by the place in this design where it becomes a concrete interface. A principle that does not become an interface is a slogan.

## 1. The engine runs end to end before it runs fast

**The rule.** At every commit from the first one, there is a path from a SQL string on a command line, through a bound and optimised plan, through `EXPLAIN` and `EXPLAIN ANALYZE`, through execution, to a row set and a metrics document that the benchmark harness can read. Work that breaks that path does not land.

**What it rejects.** The bottom-up plan. v1 builds the data plane, then expressions, then scan, then hash, then aggregate, then join, then sort, then the scheduler, then the optimizer, and the first time the whole thing is a query engine is at the end. Every one of those layers has a gate expressed as a ratio against a previous measurement, and the baseline document says plainly that rudb is a stub in the harness that declares it cannot run. The ratios are therefore against nothing until quite late.

There is a second thing it rejects, which is subtler. A bottom-up plan optimises each layer against a guess about what the layer above will ask for. An end-to-end plan optimises each layer against the profile of a real query. Those are different objectives and they disagree often enough to matter: v1's layer 1 gate is ClickBench Q1 through Q5 with four layers of the engine not yet written, so the profile it is tuned against is not the profile it will run in.

**Where it becomes an interface.** [`03-end-to-end.md`](03-end-to-end.md). The skeleton is F0 and it is the only milestone with no performance gate at all.

## 2. Every mechanism that a paper could improve is a seam

**The rule.** If two published designs disagree about how to do something, that something is a trait with a registry behind it, at least two implementations in the tree, a policy that picks one, a name for the choice that appears in `EXPLAIN`, and a way to pin the choice from a session setting. The list is long on purpose: hash table structure, hash function, key encoding, aggregation state layout, join build strategy, chunk compaction policy, sort algorithm, string representation, filter kernel flavour, spill policy, eviction policy, morsel size, partitioning fan-out, scan order, Bloom filter parameters, expression evaluation strategy, every optimizer rule.

**What it rejects.** Choosing. v1's hash document decides that join uses an unchained table and aggregation uses open addressing with a salt byte. That is probably the right pair of choices, the DaMoN 2024 unchained paper and the 2025 "Global Hash Tables Strike Back" result both point that way, but writing it into the plan as a decision means the alternative is never measured on this engine's data, and the two papers disagree about where the crossover is. The honest form of that sentence is not "join uses unchained" but "join defaults to unchained; open addressing and chained are in the tree; here is the sweep that says which wins on `hits` and on TPC-H SF100."

It also rejects a thing that sounds like modularity and is not: the plug-in architecture where the seam exists but only one implementation was ever written. A seam with one implementation is an abstraction tax. The rule is two, in tree, both tested by the same oracle, both in the sweep.

**Where it becomes an interface.** [`04-modularity.md`](04-modularity.md), and it is the spine of the whole directory.

## 3. Correctness is differential, and the reference implementation is never deleted

**The rule.** For every seam, one implementation is marked as the reference. It is chosen for being obviously correct rather than for being fast, the nested loop join, the `HashMap` group by, the decode-then-compare kernel. It stays in the tree forever, it is run by the test suite on every query in the corpus, and its answer is the definition of the right answer. Any other implementation of that seam that disagrees with it is a bug in the other implementation.

**What it rejects.** The idea that the slow path is temporary. v1 already gets this right in one place, its join document keeps the nested loop permanently as an oracle and a non-equi fallback, and v2 generalises it to every seam. The cost is real: a second implementation of everything, maintained. The benefit is that the entire test strategy collapses to one sentence, that adding a new implementation of a paper costs no new tests, and that a researcher who swaps in their own hash table finds out within one `cargo test` whether it is correct on ninety thousand lines of corpus.

**Where it becomes an interface.** [`15-testing.md`](15-testing.md), section on the oracle.

## 4. The layout is the engine

**The rule.** The physical representation of a column is the primary design object. Operators are written against encoded data and decode only where no kernel exists, decoding is counted and reported, and a query that decodes a column it did not need to decode is a performance bug with a name.

**What it rejects.** The Arrow-shaped mental model in which the in-memory truth is a flat buffer and encodings are a storage detail you undo at the scan. That model is what puts DataFusion and Polars at 45 seconds on a board where DuckDB is at 26 and Umbra at 8. It also rejects the sequencing implied by a bottom-up plan, where the data plane is designed first and the storage format arrives four layers later and has to fit through an interface that was drawn without it.

The evidence for this principle is the strongest evidence in the whole design and it is worth restating. The Bespoke OLAP ablation attributes roughly a factor of twelve on TPC-H and a factor of fifty on CEB to layout specialisation, and about twenty-six per cent to everything that is usually called query engine work, compilation, fusion, prefetching, kernel quality. The 2026 paper reports 11.78x on TPC-H and 9.76x on CEB over DuckDB end to end. Ten times DuckDB exists. It is not on the operator side of the ledger.

**Where it becomes an interface.** [`05-data-model.md`](05-data-model.md) for the forms, [`06-storage.md`](06-storage.md) for what is written, [`13-encoded-execution.md`](13-encoded-execution.md) for the bridge.

## 5. Memory is a single currency and no operator prints its own

**The rule.** There is one buffer manager. It owns persistent pages and operator temporaries alike. Every stateful operator holds its state in pages obtained from it. An operator that wants to be resident pins; an operator that can tolerate eviction says so and says which pages to take first. There is no `Vec` of query state that the buffer manager does not know about, and a lint enforces it.

**What it rejects.** Two things that are usually presented as alternatives and are both wrong. The first is v1's ordering, in which spilling is layer 8 and every operator before it is written as if memory were infinite; retrofitting a spill path into an aggregate that holds a `HashMap` is a rewrite of the aggregate. The second is the pure buffer-manager position that the firepanda notes object to correctly, that a buffer manager which evicts by clock or LRU will evict the page an operator is one instruction away from reading.

The resolution is in the literature and it is not new. Kuiper, Boncz and Mühleisen's unified memory management for DuckDB's external aggregation makes temporaries and persistent data share one manager with a page layout that spills without a serialisation step. Umami goes further and gives each operator its own buffers inside the shared manager, so that the operator chooses what to evict, thread-local hash table bucket ranges, in their case, while the manager still owns the total. Otaki et al. at CIDR 2025 add the piece that makes it adaptive: consumers know what is pinned and can trade memory for cost along a curve they publish. v2 takes all three: one manager, operator-owned eviction, published cost-memory curves.

**Where it becomes an interface.** [`07-memory.md`](07-memory.md).

## 6. Parallelism is a property of the plan, not of the operators

**The rule.** The operator interface is push, in three traits, source, stream, sink, from the first commit, and it is identical whether one thread or thirty-two threads are running it. Parallelism is expressed by how many copies of a pipeline the scheduler instantiates and by explicit exchange nodes in the plan. An operator never spawns a thread, never consults the thread count, and never knows whether it is the only instance.

**What it rejects.** The pull model, and the plan to convert to push later. v1 has `Operator::next() -> Result<Option<Chunk>>`, thirty-two lines, and a layer-8 document that turns it into source/stream/sink. That conversion touches every operator, every test, and every place where an operator's internal loop assumed it could block. There is no reason to pay it. The push interface is not harder to implement single-threaded; the single-threaded driver is fourteen lines.

It also rejects implicit parallelism, the Polars-shaped design in which the operator is an async state machine and the runtime decides. That design has a real advantage, exact backpressure through wait tokens, and a real cost, which is that a deadlock is a property of the whole graph and is not enumerable. DuckDB's blocked-task shape, where an operator returns `Blocked(reason)` from a closed set of reasons, is enumerable, and v1 chose it for that reason. v2 keeps that choice; it is one of the places where v1 was right.

**Where it becomes an interface.** [`08-execution.md`](08-execution.md) for the traits, [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md) for the scheduler.

## 7. Distribution is a constraint on interfaces today and code later

**The rule.** Every point where data would cross a machine boundary is an explicit plan node today, even though both sides are threads in one process. Repartition, broadcast, gather and merge are `Exchange` nodes with a declared distribution. Every piece of operator state that would have to move is serialisable through the same path that spills it to disk. Nothing else about distribution is built.

**What it rejects.** Both of the usual answers. Building the distributed engine now is wrong because the project's whole claim is a single-node claim and every hour spent on a shuffle is an hour not spent on the 2.63 seconds. Ignoring distribution entirely is wrong because the two things that make a single-node engine impossible to distribute later are exactly the two things that are free to get right now: state that cannot be serialised, and operators that assume they can see all the data.

Note the pleasing coincidence. The serialisation requirement for distribution and the spill requirement for larger-than-memory are the same requirement. If an aggregate's state can be written to a temporary file, it can be sent over a socket. Principle 5 pays for principle 7.

**Where it becomes an interface.** [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md), section 6.

## 8. The measurement is part of the engine

**The rule.** Execution produces a metrics document as a normal output, not as a debug feature. It carries per-operator rows, time, CPU time, bytes read, bytes spilled, allocations, and the identity of every strategy chosen at every seam. `EXPLAIN ANALYZE` is a rendering of that document. `rudb-bench` ingests the same document. The schema is versioned and its compatibility is tested.

**What it rejects.** Profiling as an activity you do with `perf` when something is slow. The project's claim is a performance claim against five rival engines on six benchmark suites, and v1's own measurement document is right that the credibility rests on the apparatus more than on any decision inside the engine. What v1 does not do is make the engine responsible for producing the evidence; it drives rudb as a subprocess and reads CPU seconds from `getrusage`, which is the correct way to get a fair total and a useless way to find out which operator spent them.

v2 does both. The external measurement stays exactly as v1 designed it, subprocess and `wait4` and `/proc/<pid>/io`, because it is the fair one. The internal metrics document is what makes a number attributable, and the two are cross-checked: internal CPU time summed over pipelines must land within five per cent of the CPU the engine measured around running them, and a run where it does not is a broken run. A pipeline's number is the loop that ran it rather than the sum of the operators in it, because the loop is a third of a short query and an operator never charges itself for it. Building the tree is off the right hand side, since no pipeline exists yet to account for it. The gap out from there to the external total is process startup and printing, which is measured and reported and is not a rule. While the engine is serial this check is close to an identity and 14-metrics.md section 3 says so plainly.

**Where it becomes an interface.** [`14-metrics.md`](14-metrics.md).

## What these principles cost

They are not free and the design should say what they cost before anybody discovers it.

Principle 1 costs throwaway work. The F0 aggregate is a `HashMap` and it will be deleted at F5. That is roughly two thousand lines written to be deleted, and it buys a working measurement loop eight months earlier.

Principle 2 costs a dispatch and a maintenance burden. Every seam is a virtual call or a generic instantiation, and every seam has at least two implementations to keep correct. The dispatch cost is bounded by putting seams at chunk granularity rather than row granularity, this is the single most important implementation rule in the design and [`04-modularity.md`](04-modularity.md) section 3 is about nothing else. The maintenance cost is real and is paid.

Principle 3 costs a second implementation of everything, forever.

Principle 4 costs the ability to borrow. An engine whose in-memory truth is Arrow can use somebody else's kernels. This one cannot, and does not want to, and already does not, zero external dependencies is an existing project rule.

Principle 5 costs a page indirection on every access to operator state.

Principle 6 costs nothing, which is why it is strange that v1 deferred it.

Principle 7 costs a serialisation path for state that would otherwise be allowed to contain a pointer.

Principle 8 costs a counter increment per chunk per operator, and the discipline of never putting one in a row loop.

Six of the eight costs are paid once, at design time. Two of them, the dispatch and the page indirection, are paid per query forever, and both are measured at F1 and F3 respectively with the mechanism switched off, so the tax is a number in the ledger rather than an article of faith.
