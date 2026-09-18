# The measured state

Everything in this document was produced by running a command against the tree at `a99b3d8` on 18 September 2026, and the command is given so the number can be rechecked when it stops being true.

## 1.1 Where the commits go

`git log --oneline -400 --name-only --pretty=format: | grep -o 'crates/[a-z-]*' | sort | uniq -c | sort -rn`

| crate | commits touching it, of the last 400 |
| --- | --- |
| rudb-exec | 352 |
| rudb | 219 |
| rudb-opt | 122 |
| rudb-bind | 103 |
| rudb-kernels | 96 |
| rudb-functions | 95 |
| rudb-parse | 74 |
| rudb-pipeline | 62 |
| rudb-plan | 59 |
| rudb-common | 52 |
| rudb-vector | 49 |
| rudb-parquet | 44 |
| rudb-metrics | 39 |
| rudb-native | 38 |
| rudb-catalog | 28 |

Eighty eight percent of recent commits touch the executor. That is not a sign that the executor is where the value is. It is a sign that the executor is where everything has to be expressed.

The distinction matters because the two look identical from a distance. An engine whose commits concentrate in the executor because the executor is being tuned is healthy. An engine whose commits concentrate in the executor because there is nowhere else for a decision to live is accumulating a liability, and the way to tell them apart is to read the commit titles and ask whether the change is about data or about query shape.

Recent titles, verbatim: "Expand a star over a semi or an anti join to the left side alone", "Count a mixed aggregate's distinct pairs the same way a plain one does", "Rewrite a cross product under a null safe equality as well", "Turn a cross product under an equality into an inner join", "Partition a grouped distinct on the pair and count the groups afterwards", "Look up a null safe equality instead of looping over it", "Split a join condition written as an AND into its conjuncts", "Answer a join without holding the side that drives it".

Every one of those is about query shape. Two of them, the cross product rewrites and the conjunct split, are plainly logical rewrites that happen to have been implemented below the optimizer. The rest are physical choices with no artifact to be written in.

## 1.2 Where the lines are

`find crates/rudb-exec -name '*.rs' | xargs wc -l | sort -rn`

`rudb-exec` is 23,413 lines in 37 files. The top of the list:

| file | lines |
| --- | --- |
| group.rs | 5,249 |
| source.rs | 2,030 |
| prepared.rs | 1,946 |
| build.rs | 1,658 |
| table.rs | 1,616 |
| join.rs | 1,268 |
| window.rs | 1,251 |
| tests.rs | 1,192 |
| spill.rs | 655 |
| group_mixed.rs | 582 |
| group_distinct.rs | 478 |

`group.rs` has 140 function definitions and 37 top level items. It has already shed two files, `group_mixed.rs` and `group_distinct.rs`, and it is still the largest file in the repository. Grouping, plus its two spin-offs, is 6,309 lines, which is more than `rudb-plan` and `rudb-pipeline` put together.

For contrast, the two crates that are supposed to hold the missing artifacts:

- `crates/rudb-ir/src/lib.rs` is 9 lines. It is a doc comment saying it will hold fused kernels and a `RANK` constant so the layer check has something to read.
- `crates/rudb-jit/src/lib.rs` is 9 lines, same shape.

## 1.3 The sentence that explains it

`crates/rudb-exec/src/build.rs`, module documentation, line 3:

> One match, one arm per logical operator, and nothing else. There is no physical plan and no cost based choice between two ways of running the same node, which is the honest description of tier 0: there is one implementation of each operator so there is nothing to choose between.

That was an honest and correct description when it was written, because with one implementation per operator there genuinely was nothing to choose. It stopped being true some time in the last few weeks. There are now two join implementations, `Probe` as a stream and `Join` as a sink, and `crates/rudb-exec/src/join.rs` has a function called `streamed` that decides between them. There are at least three grouping paths across `group.rs`, `group_mixed.rs` and `group_distinct.rs`. There is a nested loop path and a hash path inside the join.

So the choices exist. They are just made inside the operators instead of above them, one `if` at a time, with no name, no `EXPLAIN` line, no toggle, and no way to write a test that asserts which one ran.

## 1.4 The peephole that proves the point

`crates/rudb-exec/src/group.rs` contains `mark_affine_sums`. It walks the aggregate's call list looking for a `sum` whose argument is `CAST(smallint AS integer) + integer_literal` and whose return type is `HUGEINT`, finds an earlier `sum` over the same base column, and records an offset so the second sum is computed from the first by arithmetic rather than by a second pass over the data.

This is a good optimization. It is also, in order: a common subexpression problem, expressed as a pattern match on a specific type pair, implemented inside a hash aggregate operator, discoverable only by reading five thousand lines, untestable except through end to end query results, and correct only for the exact spelling it matches. Change `SMALLINT` to `TINYINT` and it silently does not fire. Write `sum(x) + count(*)` and it does not apply. There is no `EXPLAIN` output that says it fired.

The general form of this is aggregate argument common subexpression elimination, it is a logical pass, it is a hundred lines, it is tested as printed plan in and printed plan out, and it subsumes the special case along with every other spelling of it. That is the whole argument of this folder in one function.

## 1.5 What the optimizer actually has

`crates/rudb-opt` is 11,652 lines in 21 files and `PASSES` has eleven entries, in this order:

1. `fold::ExpressionRewriter`
2. `distinct::DistinctAggregateRewrite`
3. `dependent::DependentGroupKeys`
4. `filter::FilterPushdown`
5. `empty::EmptyResultPullup`
6. `cte::UnusedMaterialization`
7. `columns::UnusedColumns`
8. `limit::LimitPushdown`
9. `topn::TopN`
10. `late::LateMaterialization`
11. `sides::BuildSideProbeSide`

DuckDB's `duckdb_optimizers()` lists forty four names on the pinned binary, and `UPSTREAM` in the same file holds all forty four so that `SET disabled_optimizers` never fails on a name rudb has not built.

Look at what is in the list and what is not. Everything present is either mechanical, meaning it needs no number to decide (folding, column pruning, filter pushdown, limit pushdown, top-n fusion), or it needs exactly one comparison between two numbers (`BuildSideProbeSide`). Nothing present needs a distribution.

Absent: join ordering, join elimination, aggregate pushdown, group by elimination, sort elimination, transitive predicate generation as a scheduled pass (there is a `transitive.rs`, 436 lines, but it is a private helper of the filter pass rather than a pass of its own), semi-join reduction, runtime filter placement, and every form of physical planning.

The reason is in the next section and it is not laziness.

## 1.6 What the optimizer has to reason with

`crates/rudb-opt/src/estimate.rs`, 606 lines, two constants:

```
const KEPT_BY_A_CONDITION: f64 = 0.2;
const KEPT_BY_A_GROUP_BY: f64 = 0.1;
```

Its own module documentation is candid about what that means: no column histograms, no distinct counts, no correlation between predicates, no sample, and a filter on a primary key gets the same selectivity as a filter on a boolean. `Context` carries `statistics` which is a `BTreeMap` of table name to row count, copied at statement start so the plan stays a value.

A pass that reorders four joins on top of that is a pass that produces a different wrong answer than the one before it. So the passes that need numbers correctly did not get written, and the optimizer stalled at the mechanical eleven while the executor absorbed everything else. That is the mechanism by which an absent statistics layer causes executor churn, and it is worth naming because the fix looks like it is in the executor and is not.

## 1.7 What did land, and it is a lot

This is not a folder about a stalled project. In the seven days since `../planner/` was written the tree grew a hash join with eight join kinds and a null rule that is per column rather than per table, a parallel driver that is actually used by `Query::run`, a spill path, window functions, a top-n, set operations, distinct aggregates over mixed calls, and a `Prepared` expression compiler at 1,946 lines. Projection pushdown landed, which `../planner/` called the highest value single pass in the folder, and so did filter pushdown, limit pushdown and top-n fusion.

The first pass predicted five things. Four of them happened. The fifth, that cardinality and join ordering would follow the mechanical passes, did not, and document 04 is why.

## 1.8 The execution interfaces are right

`crates/rudb-pipeline` is 3,621 lines and it is the healthiest crate in the execution stack. `Source`, `Stream` and `Sink` all take `&self` with per-instance mutable state passed separately, which is what lets one pipeline be instantiated on thirty two threads without thirty two copies of a predicate. `Sink::combine` takes local state by value so merging one thread's partial aggregate twice is a compile error. `Watched` measures any of the three from outside, so an operator written next year is measured the day it is written. `Progress::Blocked` already enumerates four reasons, which makes the wait-for graph finite.

None of that needs replacing. Document 08 keeps all three traits and changes what sits on top of them. The problem is not the execution interface. The problem is that there is no artifact between the logical plan and the operators, so the operators are where planning happens.

## 1.9 The four symptoms, named

So that a reviewer can recognise the same disease later without rereading this document.

**A choice with no name.** Two code paths exist, one is picked by an `if` inside an operator, and no string anywhere in the system identifies which was picked. `streamed` in `join.rs` is the example.

**A pattern match on query shape below the optimizer.** `mark_affine_sums` is the example. Any function in `rudb-exec` that inspects `Expr` structure to decide strategy rather than to evaluate it belongs above.

**A file that grows on every feature.** `group.rs` at 5,249 lines is the example, and the tell is that it grew past the point where the two spin-off files were extracted and kept growing.

**A test that can only be written end to end.** If the only way to assert that an optimization fired is to run a query and time it, the optimization is in the wrong layer. Every decision in documents 05, 07 and 08 is assertable from printed text.

## What we should take from this document

The executor is taking eighty eight percent of the commits because it is the only place a decision can be written down. The statistics layer's absence is why the optimizer could not take those decisions instead, and `../stats/` fixes that as of today. The two artifacts that are missing are a physical plan and an executable IR, and their absence has a measurable cost: one 5,249 line file, 140 functions, and a per-query-shape peephole implemented inside a hash aggregate.

Nothing about the execution interfaces is wrong. Nothing about the eleven passes is wrong. What is wrong is that there is nothing between them.
