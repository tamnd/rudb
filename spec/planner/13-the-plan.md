# The plan

What to build, in what order, and what each stage is expected to be worth. Everything before this document is an argument; this one is a schedule.

## 13.1 The disagreement with #65, settled

Issue #65 lists the rewrites "in the order they are worth doing in" and puts **filter pushdown first, projection pushdown second**, on the grounds that filter pushdown is the largest and everything depends on it. That is right about the value and this folder still proposes the opposite order for the first two pull requests. The reason is not a disagreement about value:

- **Projection pushdown is already half built.** `crates/rudb-exec/src/source.rs` resolves a projection by name against the stored table and reads only those positions; its doc comment says it was written in anticipation of this pass. `crates/rudb-bind/src/binder.rs` line 831 hands it every column. The gap is one pass and **zero operator changes**.
- **Its correctness condition is one sentence**. never drop a column something above reads, and the root-schema check from document 03 catches every violation automatically. Filter pushdown's correctness condition is the eight-row join-kind table in document 04 section 04.3, which is where the classic wrong answers live.
- **So projection pushdown is the pass that builds the machinery**: the `Pass` trait, the `Context`, the toggle, the text-in/text-out test harness, the re-bind step, the corpus-on-against-off gate. Landing all of that behind the safest possible rewrite means that when filter pushdown arrives, the only new thing in the pull request is the risky part.

Filter pushdown is still the largest win and it is still second, arriving days rather than weeks later. The folder's order and the issue's order agree from stage 3 onward.

## 13.2 The stages

Twelve stages. The numbers in the "worth" column are the honest ones: where a number comes from another engine or another paper it says so, and where there is no number it says that too.

**Stage 0, the four analyses and the framework.** Elementwise, constant with the volatile-function exclusion, table set cached by `ExprRef`, null rejecting defaulting to false. Plus the `Pass` trait, `Context`, `disabled_optimizers`, the debug-build `Plan::validate` after every pass, and the root-schema check. *Worth: nothing on its own. Every later stage is written in terms of it, which is why it is not folded into stage 1.* **Document 02, document 03.**

**Stage 1, projection pushdown.** One top-down required-set walk, one bottom-up rebuild, one re-bind. *Worth: on ClickBench today, 105 columns per chunk down to about three. The 20 GB to 200 MB number in `spec/09-optimizer.md` arrives with 2e.* **Document 04 section 04.2.**

**Stage 2, expression simplification.** Constant folding, comparison normalization, connective flattening, boolean identities, range collapsing, `IN` normalization. *Worth: tens of percent, and it is a precondition for stages 3 and 4 being effective rather than a win in itself. `WHERE false` not reading the table is a correctness matter, not a speed one.* **Document 04 section 04.1.**

**Stage 3, filter pushdown.** Split at `AND`, the pass-through table, the join-kind table, null-rejecting outer-to-inner conversion, bottom-up rebuild, re-bind. *Worth: the four corpus files currently killed at ten seconds are all joins of 10k against 50k rows with predicates that never moved. firepanda measured its versions at q19 83→70 ms and q21 285→169 ms, on a different engine. The recorded prediction is >2x on TPC-H SF10 total CPU seconds together with stage 1.* **Document 04 section 04.3.**

**Stage 4, transitive predicates.** Equality classes, derive, push, with the derived-predicate marker so it terminates. *Worth: large on star schemas, nothing on ClickBench. Builds the equality-class code stage 11 needs.* **Document 04 section 04.4.**

**Stage 5, the build-side flag and the local cost comparisons.** One boolean on `Node::Join` plus the six deferred decisions from #65, each a ten-line comparison. *Worth: building from the wrong side of a hash join is a memory blow-up, and the executor currently takes whatever the binder emitted. This is the cheapest large win in the list.* **Document 10 section 10.2.**

**Stage 6, statistics and the estimator.** Plumb the KMV sketches out of `rudb-encoding`, the side table keyed by `NodeRef`, the four estimation rules, the four join-kind rules, the rows-and-bytes cost model fitted to `rudb-bench`. *Worth: nothing directly. It is what stages 7 and 11 consume and what stage 5's flag is set from.* **Document 06.**

**Stage 7, join ordering.** Hypergraph construction with conflict edges, greedy first and always available, DPhyp against the deadline, a relation-count ceiling, `EXPLAIN` reporting when the budget fired. *Worth: the difference between a factor and an exponent on JOB and TPC-DS. Target is the optimality gap, comparable to JOB-Complex's PostgreSQL 16 figures of 1.18 on JOB-light and 1.99 on JOB.* **Document 07.**

**Stage 8, subquery unnesting, five shapes.** `EXISTS`→Semi, `NOT EXISTS`→Anti, `IN`→Semi, `NOT IN`→Anti with the mark, scalar→Single. Behind an error naming what was not covered. *Worth: the common cases, and it unblocks the binder's 805 `SubqueryExpression` corpus records shipping in the same milestone rather than landing as surface area on a shape that times out. Estimated a week.* **Document 05.**

**Stage 9, subquery unnesting, the general algorithm.** Dependent join, push-down equivalences, distinct correlated values, loud `EXPLAIN` on a surviving dependent join. *Worth: TPC-DS, where `spec/09-optimizer.md` calls it the single most valuable rewrite in the set. Estimated a month and it is the largest single item in the folder.* **Document 05.**

**Stage 10, runtime filters.** Min/max always, exact `IN` below the threshold, blocked Bloom above, applied in the scan when adjacent. Annotation from the planner, tier chosen by the executor from exact counts. *Worth: `spec/09-optimizer.md` argues a filter eliminating 90% of the probe side before it is read beats any join-order decision, because it removes I/O rather than reorganizing comparisons. Zero on ClickBench.* **Document 09.**

**Stage 11, predicate transfer.** RPT with LargestRoot and SafeSubjoin, blocked Bloom, the two-edge minimum, the runtime bail-out, the schedule as a plan annotation. *Worth: Parachute reports its Bloom predicate-transfer baseline at 1.26x on JOB; RPT reports about 1.5x geomean. Zero on ClickBench, and it must cost zero there.* **Document 08.**

**Stage 12, the remaining pipeline stages and layout adaptation.** Set-op rewrites, CSE with the multi-consumer node, aggregate and distinct rewrites, limit pushdown and top-N, window rewrites, empty and constant pruning, then once the scan layer is real, the layout requirement propagation pass. *Worth: top-N is large on ClickBench's 22 group-bys ending in `ORDER BY ... LIMIT`. Layout adaptation is the folder's one distinctive pass and it is a bet, against Bespoke OLAP's 12.35x layout ablation, with a stated date to check it.* **Documents 03, 10 section 10.3.**

## 13.3 What runs alongside, not after

Three things are not stages because they are not sequential. They land with stage 1 and grow with every stage after it.

**The corpus on-against-off gate.** Per commit, from stage 1. Twenty-eight seconds of CI. Nothing lands without it.

**The pass bisector.** Binary search over `disabled_optimizers`. Document 12 says build it before the third pass lands, which by this schedule means it ships with stage 2 or stage 3. Query reduction can come later.

**Plan stability baselines.** From stage 1, so that the baselines exist before there is anything interesting to diff.

## 13.4 What this folder does not schedule

**The physical plan as a representation** waits for the second implementation of an operator. Document 10 section 10.5.

**The multi-consumer materialization node** lands on its own, before stage 12's CSE, because it turns the plan from a tree into a DAG and touches the printer, the parser, the validator and every walk.

**Window nodes, `GROUPING SETS`, recursive CTEs.** Named in document 02 so they are not discovered as omissions, not scheduled here.

**Anything learned.** Document 06 section 06.6 gives the product reason, and the 2026 reproducibility evidence (arXiv:2603.16474) says the learned cost model is the component that breaks.

## 13.5 The dependency question

#65 lists 2k as depending on 2h and 2j. That is right for the *gate*: 2k's benchmark gate is JOB, CEB, TPC-DS and TPC-H SF100, and none of those can be run meaningfully without the join operator and the scheduler.

It is not right for the *work*, and document 00 established why: `cargo xtask layers` puts `rudb-opt` at rank 11 with `rudb-bind` at 10 and `rudb-exec` at 12, both built. The optimizer's whole interface is `fn optimize(plan: Plan, catalog: &Catalog, settings: &Settings) -> Result<Plan>`, and every stage from 0 through 5 is expressible today against a plan language that already round-trips.

So the correct reading is: **stages 0 through 5 pull forward and should not wait for 2h or 2j**, stages 6 through 12 want the operators and the measurements they depend on, and the gate stays where it is. Document 14 maps that onto the issue.

## What we should take from this document

Projection pushdown is the first pull request, ahead of filter pushdown, not because it is worth more but because it is the safest place to land the framework, the toggle, the test harness and the re-bind step. From stage 3 onward this folder and #65 agree.

Twelve stages, with the two unnesting stages split into a week and a month, and with the estimator sitting between the cheap rewrites and the two things that consume it.

Stages 0 through 5 are unblocked today and should pull forward; 6 through 12 want the operators; the 2k gate is correctly placed where it is.

Three things run alongside from stage 1 rather than after: the corpus on-against-off gate, the bisector, and plan stability baselines.

Every "worth" number in section 13.2 names where it came from, and the two that are rudb's own are the four timing-out corpus files and 105 columns per chunk.
