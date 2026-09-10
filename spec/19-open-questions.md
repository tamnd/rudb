# Open questions

Every question here is one whose answer changes the specification. They are ranked by how much damage a wrong answer does, and each has a milestone by which it gets answered, because a question with no deadline is a question that gets answered by discovering the consequences.

The first six are load-bearing: a bad answer to any of them means a document gets rewritten and a published claim gets reduced. The rest are design choices where either answer is survivable.

## Q1: Does multi-column compression deliver the ratios on real data?

**Why it matters.** The axis-4 target of 2.05 GB for ClickBench `hits`, which is 10x under DuckDB and 4x under Umbra, assumes that shared dictionaries, shared symbol tables and correlation encodings capture a large amount of redundancy that per-column encoding leaves behind. Document 03.5 works the arithmetic and shows that the fixed-width half is straightforward and the string half is not. Nobody has published this measurement for this dataset.

**What is actually unknown.** The distinct counts of `URL`, `Referer` and `Title`, the overlap between `URL` and `Referer` value sets, and the strength of the functional dependency between `Title` and `URL`. Three numbers.

**If the answer is no.** The number lands around 4 to 5 GB, which is 4 to 5x under DuckDB and roughly Umbra parity on disk. Still a good result. Not 10x, and documents 00, 02 and 03 get amended and the axis-4 claim gets restated.

**Answered at M1**, which exists for this purpose and comes before the storage engine is built.

## Q2: Does runtime layout adaptation capture the offline specialization win?

**Why it matters.** This is the project's thesis. Bespoke OLAP measured 12.35x from storage layout specialization, with a workload known ahead of time. We do not know the workload ahead of time and adapt at query time instead. The entire gap between 3.2x and 10x in document 02.4 assumes that adaptation captures a meaningful fraction of what offline specialization gets.

**The honest prior.** It captures some and not all. An offline specializer can physically reorder and co-locate data for the queries it knows about; a query-time adapter chooses among representations that already exist. Those are different amounts of freedom.

**If the answer is no.** Document 02's target drops to 3 to 4x, which is matching or slightly beating Umbra. That is a real database and a reasonable project, and it is not the claim in document 00.

**Answered at M3**, whose exit criterion is stated as a specific number precisely so that this question gets a yes or a no rather than an impression.

## Q3: Why did Vortex make its host engines slower, and have we avoided it?

**Why it matters.** This is the closest published system to our design and it is the only direct evidence available about whether the design works. Document 03.2 records the result: Vortex in DuckDB moves 26.25 to 40.99 seconds, and in DataFusion moves 45.57 to 91.30. Both worse, by 1.6x and 2.0x.

**The hypotheses, in order of plausibility.** That the integration decodes at the scan boundary and therefore pays the format's cost without getting encoded execution's benefit. That the host engines' operators have no encoded paths, so decoding is forced regardless of what the format supports. That the decode kernels are slower than the I/O they save on a machine where the data is in page cache anyway. That the integration is simply young.

**If it is the first two, our design specifically addresses them** and document 6.7's contract plus document 16.2's equivalence testing is the mechanism. If it is the third, the problem is more fundamental and applies to us too.

**This one is answered by reading their code and profiling their integration, not by waiting for a milestone.** It should happen during M1, and it is the cheapest available information about whether the thesis is sound.

## Q4: Do recomputation rules pay for themselves?

**Why it matters.** Worth roughly 1.6 GB out of 20.46 on ClickBench, which is significant at a 2.05 GB target and irrelevant at a 5 GB one. The cost is CPU on any query touching the derived column and a permanent maintenance obligation, because document 6.6 requires every rule version to stay in the binary forever for bit-exactness.

**The uncomfortable part.** The maintenance obligation is unbounded in time and the benefit is bounded and modest. This is the feature in the specification with the worst ratio of long-term cost to measured benefit.

**If the answer is no**, it is cut, the disk target moves by around 1.6 GB, and documents 05.4 and 06.6 lose a section. This is the least painful of the six to answer negatively.

**Answered at M1** as part of the format experiment.

## Q5: Can a global dictionary be built during a load without an unacceptable memory or time cost?

**Why it matters.** Global dictionaries are the single highest-value structure in the format per document 6.5, and building one means holding a live hash table of every distinct value of a column for the duration of the load. On `URL` in this dataset that could be hundreds of megabytes, and on a larger dataset on a smaller machine it does not fit.

**The mitigation already in the design.** A spilling dictionary builder, which makes the load path depend on document 7.8's spilling infrastructure and makes loads slower. Document 5.6 budgets 300 seconds against DuckDB's 126 and Umbra's 164, and this is the main thing that could blow that budget.

**The fallback if it does.** Per-partition dictionaries scoped to groups of row groups rather than to the whole table, which captures most of the benefit at some of the cost and keeps the memory bounded by the partition size. This fallback is good enough that Q5 is unlikely to be fatal, which is why it ranks below Q1 through Q4.

**Answered at M1 for the memory profile and at M4 for the production implementation.**

## Q6: Does the heavy-hitter verification pass succeed often enough to matter?

**Why it matters.** Document 7.5's two-pass exact top-k mechanism is a large part of what document 02.4 claims for the last factor of two, and it applies to 14 of the 43 ClickBench queries. It only pays when the sketch's candidate set provably contains the true top k, which requires the distribution to be heavy-tailed enough that the k-th candidate's exact count exceeds the bound on everything outside the candidate set.

**What is unknown.** What fraction of real top-k queries have distributions where this holds, and how wide the sketch has to be. Web analytics data is heavy-tailed, which is the reason to expect a yes, but expecting is not measuring.

**If the answer is no**, the mechanism falls back to a full hash table on most queries, having paid one cheap extra pass, and the axis-2 target loses roughly a factor of 1.5, landing near 5x rather than 10x.

**Answered at M5.**

## Q7: One binary or one binary per microarchitecture?

Runtime dispatch per document 7.3 costs an indirect call per kernel invocation and constrains inlining. Compiling separate binaries for AVX-512, AVX2 and baseline avoids both and multiplies the distribution problem by three.

The default is runtime dispatch, which is what ClickHouse does and it works. The question is whether the measured cost is large enough to justify a `-march=native` build path for people who care, which is a small amount of work and a real amount of user confusion.

**Answered by measurement at M3. Either answer is fine.**

## Q8: How much does the DuckDB C API constrain the Rust API?

Document 13.7 says the Rust API is native and not a wrapper. Document 12.3 says the C API is `libduckdb`-compatible. Those are two APIs over one engine and the risk is that the C API's design, which reflects DuckDB's internals, leaks into the engine's shape and constrains what the Rust API can offer.

The mitigation is that the C API is a thin translation layer in its own crate with no privileged access. Whether that survives contact with the harder parts of the ABI, particularly the extension host side and the streaming result interface, is not known.

**Answered at M7. Either answer is survivable; the bad one costs an awkward layer.**

## Q9: Is io_uring worth two backends?

Document 5.7 keeps both io_uring and a thread pool because PVLDB 19(1) says io_uring wins at depth and Conviva's published experience says it lost for them. Maintaining two I/O backends forever is a real cost.

If measurement shows io_uring is better across the whole range of queue depths we care about, the thread pool becomes a portability fallback rather than a maintained peer and the cost drops.

**Answered at M2. Either answer is fine.**

## Q10: Should there be a user-visible encoding override?

Document 6.8 says no user-visible encoding annotations, on the grounds that a schema-level annotation means an imported table never gets the benefit and that is precisely the gap this design closes. ClickHouse's `LowCardinality` and `CODEC` are the counter-example and their users do use them.

The question is empirical: how often is the automatic decision wrong enough that a user would want to override it. If the answer is rarely, the `PRAGMA` stays a debugging tool. If the answer is often, it becomes a documented feature and the automatic decision needs work.

**Answered by usage after M6.**

## Q11: How much of the compatibility surface is actually load-bearing?

Document 12.7 calls the treadmill one of two project killers and document 10.7 measures the surface by weighted coverage. The unknown is the shape of the tail: whether 97 percent weighted coverage means a user hits a gap once a month or once an hour.

This is answerable only with real users and real queries, which means it is not answerable before there are any, which is uncomfortable given that it is one of the two things most likely to kill the project.

**Partially answered at M2 by the `sqllogictest` pass rate. Really answered after M10.**

## Q12: Is the vector size right?

Settled at 1024 in document 00 and it is the decision with the widest blast radius per document 18.4. It is here not because it is open but because it is the one settled decision that would be catastrophically expensive to revisit, and it is worth writing down what would make it wrong: if FastLanes-layout kernels turn out not to be the dominant cost, and if the per-vector overhead of operator dispatch turns out to matter more than the encoding alignment, 2048 would be better.

**Measured at M3 by running the interpreted operators at both sizes with decoding forced, which is cheap. Not expected to change and worth checking once.**

## What is not an open question

**Whether to write it in Rust.** Settled in document 00 and not revisited.

**Whether to have our own storage format.** Settled in document 05.1 by arithmetic. A format that stores this dataset in 20.46 GB cannot reach 2.05 GB by better implementation.

**Whether to support distributed execution.** No. Document 4.10.

**Whether to put a GPU in the core.** No. Document 4.10, with the door left open through Substrait per document 13.6.

**Whether the compatibility target is DuckDB.** Yes, that is the project. If it were not, this would be a different and much easier project, and also a much less useful one.
