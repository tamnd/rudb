# The planner folder

Written 11 September 2026, against rudb at `b5ff414`, a DuckDB v2.0 alpha built at the same commit the grammar is vendored from, `v2.0-cyanoptera`, `cc7e7bac7f`, and the corpus numbers published by `tamnd/rudb-compat` on that commit. Document 01 says why the binary is an alpha at that hash rather than the newest release.

## Why this exists

`spec/09-optimizer.md` is the parent design and it is right about the shape. `spec/engine/11-optimizer.md` is sub-milestone 2k and it is right that a cost model fitted to operators that do not exist yet is a cost model that gets rewritten. Neither of them is a document you can implement from, because both were written to say what the optimizer will be and neither was written to say what to type on Monday.

This folder is the implementable version. It exists now, ahead of its position in the layer order, for three reasons that are each a measurement rather than an opinion.

**The first is the corpus timeout.** Four files in DuckDB's `sqllogictest` corpus are killed at ten seconds each. All four are joins of ten thousand rows against fifty thousand. That is not a large join. It takes more than ten seconds because `crates/rudb-exec/src/join.rs` is a nested loop and because no predicate has ever been pushed below one. Five hundred million comparisons where a hash join does sixty thousand is not an execution problem that a faster kernel fixes, it is the absence of a planner, and it is already visible in a harness that was not trying to measure it.

**The second is the scan.** `crates/rudb-bind/src/binder.rs` line 831 builds every `Node::Get` with `plan.add_fields(&fields)` where `fields` is the whole of `table.columns()`. Every scan in rudb today is a scan of every column. On ClickBench `hits` that is 105 columns where the median query reads three. `crates/rudb-exec/src/source.rs` already resolves the projection by name against the stored table and already reads only the positions it was given, and its own doc comment says it was written that way so it stays correct "after projection pushdown makes the plan's list a subset". The operator is waiting. The pass that would narrow the list does not exist.

**The third is that none of this is blocked.** `rudb-opt` is rank 11 in `xtask/layers.toml`, above `rudb-plan` at 9 and `rudb-bind` at 10 and below `rudb-exec` at 12. It cannot see the executor, which means every pass in it is a pure function from a plan to a plan, which means every pass in it can be written and tested today against the in-memory catalog with no scan layer, no Parquet reader, no scheduler and no hash table. `rudb-plan` already has a printer and a parser that round-trips, so a pass test is a text file in and a text file out. The optimizer is the one layer in `spec/engine/14-plan.md` whose dependencies are all already met.

## The correction to the framing

The premise that started this folder was that a proper planner is what is missing to run queries. That is two thirds right and the missing third matters, so it is stated here rather than discovered in month three.

**A planner is not what stops a query being accepted.** `crates/rudb-bind/src/lib.rs` says in its own module doc that it does not do subqueries, window functions or `WITH`. Those are binder gaps, and in the corpus they are `SubqueryExpression` at 805 records and `WithClause` at 1244. No optimizer pass makes a query that does not bind start binding. Document 01 is about that half and about the order to close it in, because the planner and the binder are built by the same people and the sequencing between them is a real decision.

**A planner is not what makes ClickBench run.** `crates/rudb-parquet/src/lib.rs` is nine lines and `rudb-storage` is a memory table. rudb has no path from a file on disk into a chunk at all, which is sub-milestones 2d and 2e, and until that exists the ClickBench number is an abstention rather than a loss. Document 11 is honest about exactly which of the 43 queries the planner touches and by how much, and the answer is smaller than the ones the storage layer touches.

**A planner is what decides whether a query that binds and reads its bytes finishes.** Every query above two tables, every query with a filter, every query on a wide table. That is the whole of TPC-H, the whole of TPC-DS, all of JOB and CEB, and it is the reason the parent spec's second axis, no query slower than DuckDB on any suite ever, is an optimizer property before it is an execution property.

So the honest sentence is that the planner is the largest missing layer, it is the one with no unmet dependencies, and it is not the only missing layer. Build it now, in parallel with the scan, and do not let either one wait on the other.

## What is in here

`01-the-front-end.md` is the path from text to a plan and back out as an answer. What the parser gives, what the binder refuses, what `rudb-compat` measures, and the rule that the optimizer must never change an answer. This is the document that connects the planner to the DuckDB compatibility claim rather than treating them as separate projects.

`02-what-a-plan-is.md` is the intermediate representation. rudb already has one and it is good, so this is mostly an inventory of what `rudb-plan` has, what it is missing, and the three analyses every pass is written in terms of.

`03-the-pass-pipeline.md` is the framework. What a pass is, what invariant it preserves, how it is toggled, how the whole thing is budgeted, and the fixed order with the argument for the order.

`04-pushdown.md` is projection and predicate pushdown in the detail they deserve, because between them they are worth more than everything else in this folder and because the outer-join cases are where correctness goes wrong.

`05-subquery-unnesting.md` is the pass a dataframe library never needs and a SQL database cannot ship without. Neumann and Kemper's dependent join elimination, the five special shapes, and the `NOT IN` null rule that every database has been wrong about.

`06-cardinality-and-cost.md` is what we need to know about the data. rudb has an unusual asset here, `crates/rudb-encoding/src/sketch.rs`, and this document is mostly about spending it.

`07-join-ordering.md` is the search. DPccp, DPhyp, DPconv, greedy, and the planning time budget that keeps the per-query floor from being breached by the optimizer itself.

`08-predicate-transfer.md` is the strategy that makes document 07 matter less. Yannakakis, Predicate Transfer, Robust Predicate Transfer, Parachute, and the CIDR 2026 result that says a production system was already approximating this by accident.

`09-runtime-filters-and-adaptivity.md` is the decisions made after the query starts, and the layer-rule problem that the optimizer cannot see the executor so every runtime decision has to be expressed as an annotation.

`10-physical-planning.md` is operator selection and the pass with no equivalent in any shipping engine: propagating physical layout requirements from consumers back to the scan, so a `GROUP BY` on a dictionary column gets codes and never sees a string.

`11-clickbench.md` is the focus workload, query by query, with what the planner is worth on each and where it is worth nothing.

`12-explain-and-testing.md` is how any of this is known to be correct. The optimizer-never-changes-an-answer property, per-pass bisection, plan stability baselines, and q-error publishing.

`13-the-plan.md` is what gets built, in what order, and what each step is expected to be worth.

`14-checklist.md` is the milestone issue.

## How to read this if you are short of time

Read `04-pushdown.md` and `11-clickbench.md`.

The first one is where the work is. Two passes, both of them mechanical, both of them worth more on every suite than the search algorithms everybody finds more interesting. Projection pushdown is a first pull request that changes no operator and turns a 105-column scan into a three-column one.

The second one is the answer to what any of this does for the workload the project is currently pointed at, and part of that answer is "nothing, and here is which part". A folder about query optimization that cannot say where its own subject does not apply is a folder that will be believed when it is wrong.

If you have time for a third, read `08-predicate-transfer.md`, which is the differentiated bet, and `12-explain-and-testing.md`, which is the only thing that keeps the differentiated bet from silently returning wrong answers.

## A note on sources

Everything here was checked against a primary source or against the rudb tree in September 2026. Where a number is quoted from a paper, the paper is named and the number is the paper's own. Where a number is from the rudb tree it names the file. Where a claim came from a search summary rather than from the paper's own text it says so, because the difference matters and the parent spec's `01-research-2026.md` sets that rule.

The corpus numbers are from the `rudb-compat` README at the commit above: 4084 files, 12803 passed, 51427 failed, 19.9 percent of what was attempted, exit criterion above 60. The ClickBench baselines are from `rudb-bench`, recomputed from the official result files on 10 September 2026: DuckDB 26.25 seconds hot total and 20.46 GB on disk, ClickHouse 18.07, Umbra 8.10 and 8.30 GB.

This folder deliberately does not repeat the research survey in `01-research-2026.md`. It cites it and adds what has appeared since, which is section 06.6 and section 07.4.
