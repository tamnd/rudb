# The architecture

This document is the shape of the whole path, from a string a user typed to a loop running over bytes. It names six artifacts, the five arrows between them, what each artifact is allowed to know, and the one rule that decides where a decision goes.

## 3.1 The six artifacts

Every one of them is a value. Not a builder, not a callback graph, not a set of side effects on a shared context. A value that can be printed, parsed back, compared for equality, stored in a test file, and handed across a thread boundary.

| # | artifact | crate | exists today |
| --- | --- | --- | --- |
| 1 | statement text and AST | `rudb-parse` | yes |
| 2 | bound logical plan | `rudb-plan`, built by `rudb-bind` | yes |
| 3 | rewritten logical plan | `rudb-plan`, rewritten by `rudb-opt` | yes |
| 4 | annotated logical plan | `rudb-plan` plus facts from `rudb-stats` | no |
| 5 | physical plan | `rudb-phys`, new | no |
| 6 | pipeline program | `rudb-ir`, currently a 9 line stub | no |

Below artifact 6 sit the things that are not artifacts because they are machinery rather than descriptions: the drivers in `rudb-pipeline`, the kernels in `rudb-kernels`, the vectors in `rudb-vector`, the storage in `rudb-native` and `rudb-storage`.

Artifacts 2 and 3 are the same type. That is deliberate and it is the property that makes the rewriter testable: a pass is a function from a `Plan` to a `Plan`, `Plan::parse` of `Plan::print` is the identity, so a pass test is a text file in and a text file out with no catalog, no data and no executor anywhere in it. `crates/rudb-plan/tests/roundtrip.rs` and `crates/rudb-plan/tests/property.rs` are 612 and 856 lines of exactly that and they already work.

Artifacts 4, 5 and 6 inherit that property. Each one prints, each one parses, each one round trips, and each arrow into it is a pure function. That is not a nice-to-have. It is the only reason the design in this folder is affordable, because it means every decision in documents 05 through 11 can be asserted from text without running a query.

## 3.2 The arrows

**Text to bound plan.** `rudb-bind`. Name resolution, type resolution, function resolution, subquery decorrelation setup, and the production of `ColumnBinding` pairs. This arrow may fail and its failures are the ones a user reads. It is not this folder's subject and `../planner/01-the-front-end.md` remains the reference for it.

**Bound plan to rewritten plan.** `rudb-opt`. A fixed sequence of named passes, each preserving the plan invariant and the output width. Document 05.

**Rewritten plan to annotated plan.** `rudb-opt` plus the statistics service. One walk that attaches a `Fact` to every node output and every predicate, each fact carrying its class. This is a separate arrow rather than a field that passes fill in, because the class of a fact has to be uniform across the plan for a cost comparison to mean anything, and because a pass that runs before annotation must not be able to read a number at all. Document 04.

**Annotated plan to physical plan.** `rudb-phys`. Every choice between two ways of running the same logical node, made here, named here, printed here. Document 07.

**Physical plan to pipeline program.** Lowering. One physical operator becomes a sequence of blocks; a pipeline becomes a program; a query becomes a list of programs with a dependency order. Document 08.

## 3.3 The decision rule

**A decision belongs in the highest artifact that has the information to make it.**

Highest means earliest, which means executed fewest times. The logical plan is built once per statement. The physical plan is built once per statement. The pipeline program is built once per statement. An operator runs once per chunk, which on ClickBench `hits` is about eight hundred times per query per thread, and a block inside a fused loop runs once per row.

So the rule has a corollary that is easier to check in review: **a test on query shape may not appear below artifact 6.** Whether the aggregate has one call or three, whether the key is one column or two, whether the argument is a cast, whether the join condition is an equality, whether the input arrives sorted: these are all known before the query starts and every one of them is currently tested at runtime somewhere in `rudb-exec`.

The rule has one exception and it is the subject of document 12. A decision that depends on data the planner has not read cannot be made by the planner. There are exactly three of those and they are enumerated, not open-ended.

## 3.4 What each artifact is allowed to know

This is the layer rule from `xtask/layers.toml` applied to the new artifacts, and it is what keeps the arrows pure.

**The logical plan** knows about tables, columns, types, expressions and relational operators. It does not know about chunks, vectors, encodings, threads, memory or files. A logical plan is the same whether the data is in memory, in a native file or in Parquet.

**The annotated plan** knows everything the logical plan knows plus facts. It does not know what an encoding is. It knows that a column has 512 certified leading frequencies, not that the column is stored as `DICT_FSST`. This is a real line and it will be under pressure, because the physical planner wants to know about encodings and the temptation will be to put encoding facts in the annotation. Encoding capability arrives at artifact 5, from the catalog, not through the annotation.

**The physical plan** knows about operator implementations, encodings, layouts, memory, parallelism and cost. It does not know what a block is or what a kernel is called. It says "group on codes with a shared concurrent table", not "call `group_codes_u16_concurrent`".

**The pipeline program** knows about blocks, state, batches and types. It does not know what a `GROUP BY` is. A program that computes a grouped sum and a program that builds a hash join are made of the same blocks arranged differently, which is the property that makes a compiler over the program tractable.

**The kernels** know about primitives and memory. They know nothing about anything above.

## 3.5 Where the crates go

Two new crates and a change of contents in two existing ones.

`rudb-phys`, new, rank between `rudb-opt` at 11 and `rudb-exec` at 12. It depends on `rudb-plan`, on the statistics interface, and on the catalog for encoding capability. It does not depend on `rudb-exec`, which is the important constraint: the physical planner may not see an operator implementation, so every capability an operator has must be described declaratively in a table the physical planner reads. That table is the seam registry in `rudb-seam`, which already exists for exactly this purpose and already requires an implementation to declare itself.

`rudb-ir`, currently a 9 line stub at rank 2, gets the pipeline program and becomes the lowering target. Its own module documentation says it is "the typed SSA expression IR that all four execution tiers share", which is the right idea at too small a scope: document 08 widens it from expressions to whole pipelines, because the state a pipeline carries is the part that is expensive and an expression IR does not describe state. Rank 2 is correct and stays. It sits below `rudb-plan` at 9, which is exactly right for a program type that knows nothing about plans. The program is a value over blocks and types, the lowering from `rudb-phys` into it lives in `rudb-phys`, and `rudb-exec` becomes a consumer of programs.

`rudb-opt` loses its estimation module to the statistics interface and keeps the passes. `rudb-exec` loses `build.rs`'s planning role and keeps the operators, which over time become block implementations. Document 14 sequences that so that nothing has to move on the day the new crates appear.

## 3.6 The invariant that applies to every arrow

**No arrow may change an answer.**

Stated per arrow, because the test differs. For the rewriter, the annotator and the physical planner, running with the arrow's work disabled must produce the same rows as running with it enabled, modulo an order nothing promised. For lowering, the interpreter over the lowered program must produce the same rows as the interpreter over a program lowered with every optimization off.

This is not a new rule, it is `../planner/12-explain-and-testing.md` section 12.2 extended to artifacts that did not exist then. What is new is that it is now enforceable at four places instead of one, because there are four intermediate values to diff rather than one plan and one answer. Document 13.

## 3.7 The one thing this architecture costs

Four artifacts where there was one means four printers, four parsers, four sets of round trip tests and four `EXPLAIN` levels. That is real work and it is front loaded, which is the honest objection to this whole folder.

The answer is in document 01 section 1.2. `group.rs` is 5,249 lines. `rudb-plan`'s printer and parser together are 2,263 lines and they cover an entire artifact including its expression language. A physical plan is a smaller language than a logical plan, because it is the logical plan plus a choice per node, and a pipeline program is smaller still because its block set is closed and its types are four wide.

The estimate is that all four printers and parsers together are smaller than `group.rs` is today. That is not a rhetorical point. It is the reason to believe the trade is favourable.

## 3.8 What EXPLAIN shows

One command per artifact, because the failure modes are different and a single blob helps nobody.

- `EXPLAIN (LOGICAL)` prints artifact 3. This is what DuckDB's `EXPLAIN` prints and it is the compatibility surface.
- `EXPLAIN (LOGICAL, STATISTICS)` prints artifact 4, with each number followed by its class and its provenance. A plan built on a guess and a plan built on an exact count look different on the page, which `../stats/00-README.md` requires.
- `EXPLAIN (PHYSICAL)` prints artifact 5, including every choice and the fact that drove it.
- `EXPLAIN (PROGRAM)` prints artifact 6, one block per line with its state and its types. This is the one a kernel author reads.
- `EXPLAIN ANALYZE` prints artifact 5 with measured counters attached, and per block counters under a flag.

The compatibility rule from `../12-duckdb-compat.md` applies to the first form only. The other four are rudb's own and may print whatever is useful.

## What we should take from this document

Six artifacts, five arrows, every artifact a value that prints and parses, every arrow a pure function that may not change an answer.

Two of the artifacts do not exist and building them is the work. The physical plan is where a choice between two implementations is written down, and the pipeline program is what that lowers to. Everything that is currently an unnamed `if` inside an operator becomes a named field in one of those two.

The decision rule is the review test: a decision goes in the highest artifact that can make it, and a test on query shape below artifact 6 is a bug regardless of whether it produces the right answer.
