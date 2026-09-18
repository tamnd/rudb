# Adaptivity, and the rule it obeys

Two different questions get called adaptivity and conflating them is how engines end up non-reproducible. **Within a query**: what may change after execution has started. **Across queries**: whether what one query observed may change the next one's plan. `../stats/06-the-reward.md` owns the second and settles it well. This document owns the first.

## 12.1 The rule, restated for execution

`../stats/06-the-reward.md` section 6.1 states the test that every design in that directory has to pass: a read-only workload, run twice against the same file, produces byte-identical plans with byte-identical statistics provenance, in the same order, forever.

The execution-side version:

**A query's answer is a function of the data, the query text and the thread count. Nothing else.**

Thread count is in that list because floating point addition is not associative and a parallel sum merged in a different order is a different sum. `rudb-seam`'s `Determinism` enum already says this honestly with `PerThreadCount`, and pretending otherwise would make the differential test fail on a machine with a different core count. What is not in the list is time, memory pressure, cache state, which morsel finished first, or how many times this query has run before.

That rule permits quite a lot. An operator may spill under pressure, because spilling changes when work happens and not what the answer is. What it forbids is the whole class where the engine reaches a different plan because of something it noticed, which is the class that makes a slow query impossible to reproduce and a benchmark number impossible to trust.

## 12.2 The three things that may change after a query starts

Three. The set is closed and each one has to state what triggers it, what it may change, what it may not, and what it reports.

**One. The memory transition.**

Trigger: a reservation refused, or a declared state exceeding its threshold.

May change: whether an instance keeps its own table or folds into shared partitions, how many partitions are resident, and which partitions are on disk. `Built::partitioning` and `Built::local` in `group.rs` are this today.

May not change: the strategy the physical plan named, the join order, the grouping keys, or anything that appears in `EXPLAIN (PHYSICAL)`.

Reports: `EXPLAIN ANALYZE` prints whether the transition fired, at what row count, and how many bytes went to disk. A one way switch that nothing reports is a switch nobody can debug, and both of `group.rs`'s flags are one way today and neither is reported.

Bounded: the degradation is continuous, per document 09 section 9.5, so the cost is proportional to how far over the reservation was rather than a step.

**Two. The reduction abort.**

Trigger: a reduction has processed a third of its input and removed nothing.

May change: whether the rest of that reduction runs.

May not change: anything else. Document 06 section 6.5's argument is that a reduction filter is allowed to be incomplete, because a filter that says everything survives is a correct filter, so abandoning one costs the work done and changes no answer. This is the cleanest of the three and the only one with no correctness argument attached at all.

Reports: per edge, whether the abort fired and at what fraction.

Bounded: by construction, at a third of one pass.

**Three. Tier promotion.**

Trigger: a pipeline passing a tuple threshold, and a background compile finishing.

May change: which implementation of the same program runs the next morsel.

May not change: the program. Both tiers run the same blocks in the same order over the same state, which is the property document 08 section 8.5 exists to give, and it is what makes the promotion safe to do in the middle of a query.

Reports: which tier ran how many morsels, and how long the compile took.

Bounded: by `../08-codegen.md`'s asynchronous compilation. A query that finishes first never waits.

## 12.3 Three things that look like adaptivity and are not

Worth naming, because otherwise the list of three gets argued about by people counting different things.

**Kernel selection per chunk.** Document 10 section 10.5's five tests: selection density, code width, partition size against cache, match density, validity density. These choose an instruction sequence for a chunk based on that chunk. They do not change the plan, they do not change the program, and they cannot change an answer. A kernel that produces a different answer on a denser chunk is a bug, not an adaptation.

**Morsel scheduling and work stealing.** Which thread runs which morsel is already non-deterministic and already may not affect the answer, other than through the thread count caveat in section 12.1.

**The compaction gauge.** `compact.rs` learns nanoseconds per byte on this machine. Document 09 section 9.7's rule is the general form: an implementation may learn a property of the machine and may not learn a property of the workload. The machine constant does not vary with the query and learning it wrong costs a copy that was not worth doing.

## 12.4 What is refused, and what it costs

**Mid-query re-optimization at materialization points.** The technique is real, it is in shipping systems, and it works: when a pipeline finishes, the actual cardinality is known exactly, and the rest of the plan could be rebuilt against it. It is refused because the plan then depends on which estimate happened to be wrong, two runs of the same query can take different plans, and the reproduction of a bad plan requires reproducing the estimation error that caused the re-plan.

What it costs: the case where the first join's cardinality was estimated at a thousand and was ten million. rudb's answer is that the estimate should not have been wrong, because document 04 and `../stats/` exist to make it exact where it can be exact and certified where it cannot. That answer is only as good as the statistics, and section 12.6 is honest about what happens when it is not.

**Dynamic reduction rescheduling.** Qiao, Boncz and Zhang's Robust Predicate Transfer with Dynamic Execution, PVLDB 19(6), February 2026, makes the entire transfer schedule dynamic and reports good results. rudb takes the abort from that line of work and not the reschedule, because a reschedule means the reduction order depends on data arrival order, and arrival order depends on the scheduler.

**A learned join order, in any form.** Document 02 section 2.11. Not a bandit, not a model, not a cache of orders that worked. This is the position `../stats/06-the-reward.md` section 6.1 takes and the argument that carries it is that a system which gets faster the second time has benchmark numbers that are about the benchmark.

**Timing-derived preference at a seam, by default.** `../stats/06-the-reward.md` section 6.2's tier 2 exists as a setting, off, with a replay guarantee attached. This folder does not switch it on and does not ask for it.

**Plan caching keyed by anything observed.** Caching a compiled program keyed by the program's structure is fine and `../08-codegen.md` section 8.4 specifies it. Caching a plan keyed by something the engine noticed is the same prohibition in different clothes.

## 12.5 What feedback is allowed, which is more than it sounds

`../stats/06-the-reward.md` tier 0 is always on and tier 1 is on by default, and between them they get most of the value the refused mechanisms were after.

**Everything is measured and reported.** Estimated against actual, per operator, with the estimate's class attached per document 04 section 4.8. A 100x error on an `Estimated` fact is a known limitation and a 100x error on an `Exact` fact is a bug, and reporting them the same way is how a real problem stays invisible.

**A fact execution proved may be kept, and committed only at a boundary the user can name.** A completed aggregate proves a distinct count. A completed join proves a match fraction. These are measurements of data the engine already paid to read, not preferences learned from timings, and the restriction that they are written at a checkpoint, an `ANALYZE` or a write transaction is what keeps the read-only replay test true.

The difference between that and a learned optimizer is worth stating plainly, because they can be made to sound alike. A proven fact is a property of the data that would have been the same had nobody run a query. A learned preference is a property of the history of execution. The first survives the file being copied to another machine. The second does not.

## 12.6 The honest cost of this position

Refusing mid-query re-optimization means rudb has no recovery from a bad estimate. When the statistics are absent, stale, or defeated by a predicate nothing has a synopsis for, the plan is wrong for the whole query and stays wrong. An adaptive engine would notice at the first materialization point and recover, and on that query it would win.

Three things make the trade defensible rather than stubborn.

**The estimate is exact far more often here.** A link header gives a join cardinality that is exact rather than estimated, which removes the single largest source of catastrophic misestimation in the literature's own benchmarks.

**Reduction makes plans less sensitive to order.** Document 06 section 6.6, following Robust Predicate Transfer's own conclusion: with reduction in place an optimizer can restrict itself to left-deep plans and stay tolerant of estimation error. A plan that is insensitive to a bad estimate does not need recovery from one.

**The failure is visible rather than silent.** `EXPLAIN ANALYZE` shows the class of every estimate and the q-error against it. A query that was slow because a default guess was used says so, and the fix is a synopsis on that column, which is a durable fix that helps every query rather than a recovery that helps one execution.

If those three turn out not to be enough, the thing to reopen is the statistics, not the prohibition. That is a deliberate ordering: adding a synopsis is reversible and adding a feedback loop is not, because once plans depend on history every performance report has to include the history.

## 12.7 What EXPLAIN has to show

For each of the three transitions: whether it fired, when, and what it did.

For the query: the determinism class it ran under, from `rudb-seam`'s vocabulary. `Exact` for most queries, `PerThreadCount` for a query containing a parallel floating point aggregate, and `None` for nothing, because nothing in the tree declares `None` and nothing should without a written reason next to it.

For a run with any non-default setting in play: enough to replay it. A transition that fired, recorded and pinned, reproduces the run exactly. That is the replay guarantee `../engine-v2/16-milestones.md` already specifies for the adaptive seam policy, applied to the three transitions this document permits.

## What we should take from this document

A query's answer is a function of the data, the query text and the thread count, and thread count is in the list only because floating point addition is not associative.

Exactly three things may change after a query starts: the memory transition, the reduction abort, and tier promotion. Each is bounded, each is reported, and none of them may change anything that `EXPLAIN (PHYSICAL)` printed.

Mid-query re-optimization and every form of learned plan are refused, the cost of refusing is real and named, and the answer to a bad plan is a better fact rather than a recovery mechanism.

Feedback that proves a property of the data is allowed and is written at a boundary the user can name, which is what keeps a read-only workload's plans identical forever.
