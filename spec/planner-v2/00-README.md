# The planner and execution folder, second pass

Written 18 September 2026, against rudb 0.3.42 at `a99b3d8`. It replaces `../planner/`, which was written on 11 September 2026 against `b5ff414`. Seven days and four hundred commits separate the two, and the reason for a second pass is in where those commits went.

## Why there is a second pass so soon

Three hundred and fifty two of the last four hundred commits touched `crates/rudb-exec`. The next largest is the top level `rudb` crate at 219, then `rudb-opt` at 122. `rudb-exec` is now 23,413 lines across 37 files, and one of those files, `group.rs`, is 5,249 lines with 140 functions in it. That crate is where the work is going, and the commit titles say what kind of work it is: turn a cross product under an equality into an inner join, partition a grouped distinct on the pair and count the groups afterwards, count a mixed aggregate's distinct pairs the same way a plain one does, look up a null safe equality instead of looping over it.

Read those titles again. Every one of them is a decision about how to run a query. Not one of them is a decision the executor should be making, because the executor is the thing that runs once per chunk and the decision only needs making once per query. They are in the executor because there is nowhere else for them to go, and there is nowhere else because `crates/rudb-exec/src/build.rs` says so in its own module documentation: "There is no physical plan and no cost based choice between two ways of running the same node."

That is the whole diagnosis. The code is moving too much because rudb is missing an artifact. Every optimization that arrives has to be expressed as a branch inside an operator, each branch is a new path through a hot loop, each path is a new correctness surface, and the operator that collects the most of them is the one that grows to five thousand lines and takes most of the commits. `group.rs` already contains a function called `mark_affine_sums` that recognises `sum(CAST(smallint AS integer) + integer literal)` and arranges for it to reuse an earlier sum of the same column. That is a plan rewrite. It is implemented inside a hash aggregate.

## The other reason, which is more encouraging

The supply side changed today. `../stats/` and `../graph/` were both written on 18 September 2026 and between them they change what a planner in this tree is allowed to assume.

`../graph/` stores the join. An equi-join over a declared or inferred relationship becomes a link, the link is in the file, and the join's output cardinality is the child row count minus the unmatched count, which is a number recorded at build time and is therefore exact. `../stats/` generalises that into a catalogue where every fact carries its class: exact, certified, or estimated. A frequency synopsis with 512 leading counts and a proven bound on everything else can answer a top-k group by rather than estimate it.

The old planner folder was designed against guesses. `crates/rudb-opt/src/estimate.rs` still assumes a conjunct keeps a fifth of its input and a group by keeps a tenth, which are Selinger's constants with a different spelling, and it has no histograms, no distinct counts, no correlation and no sample. A planner built on those numbers cannot be trusted with a decision that has no cheap fallback, which is why every pass that landed in the last week is a mechanical one and every pass that needs a number did not land.

With `../stats/` and `../graph/` the position inverts. A planner that can tell an exact count from a default guess can be given decisions that matter, and decisions that matter are exactly the ones currently sitting inside `group.rs`.

## The thesis

**A decision belongs in the highest artifact that can make it, and the executor is the lowest artifact there is.**

Stated as a rule with teeth: no operator may contain a test on the shape of the query. An operator may test the data it was handed, because that is what an operator is for. It may not test whether the aggregate it is running has two calls or one, whether the argument is a cast of a smallint, or whether the group key came from a dictionary. Those are properties of the plan and the plan knew them before the query started.

Applying that rule requires two artifacts rudb does not have, and building them is most of what this folder specifies. A **physical plan**, which is where every choice between two ways of running the same logical operator is made and written down. And a **pipeline program**, which is the executable intermediate representation the physical plan lowers to, made of a small closed set of blocks, with one interpreter and later one compiler over the same program.

The payoff is not elegance. It is that a new specialization becomes a new lowering rule or a new block instead of a new branch in a five thousand line file, and a new lowering rule is testable as text in and text out with no data anywhere near it.

## What this folder covers that the first pass did not

The first pass was about the planner. This one is about the planner and the executor together, because the boundary between them is the thing that is wrong, and you cannot fix a boundary from one side.

It is also end to end on purpose. `../stats/` says what is written to disk and what is resident. `../graph/` says what a stored join is. Neither of them says how a fact becomes a plan decision or how a plan decision becomes a loop, and that path is this folder. Where consistency required it, this folder made small additions to both, and document 04 section 4.6 lists them in one place rather than leaving them to be found by reading three directories.

## The documents

| | |
| --- | --- |
| [01-the-measured-state.md](01-the-measured-state.md) | The deep dive. Churn, line counts, what the first pass predicted and what actually landed |
| [02-research-2026.md](02-research-2026.md) | The literature as of September 2026, mechanism by mechanism, and what rudb takes and refuses |
| [03-the-architecture.md](03-the-architecture.md) | The six artifacts, the arrows between them, and the decision rule |
| [04-facts-not-estimates.md](04-facts-not-estimates.md) | How `../stats/` and `../graph/` reach the planner, and the small changes they need |
| [05-the-logical-plan.md](05-the-logical-plan.md) | The plan IR, the four analyses, the pass order, and where the peepholes go |
| [06-reduction-and-join-order.md](06-reduction-and-join-order.md) | The 2026 position, exact reduction over row ids, and how little search is left |
| [07-the-physical-plan.md](07-the-physical-plan.md) | The missing artifact. Operator selection, layout requirements, memory, parallelism |
| [08-the-pipeline-program.md](08-the-pipeline-program.md) | The executable IR. The block set, the state model, and what replaces the big match |
| [09-the-execution-model.md](09-the-execution-model.md) | Push, morsels, blocking, order, memory, and what may never be in an operator |
| [10-specialization.md](10-specialization.md) | The tiers, kernel selection, the codegen backend decision, and encoded execution |
| [11-aggregation-and-join.md](11-aggregation-and-join.md) | The two operators that are most of the time, and how `group.rs` comes apart |
| [12-adaptivity-and-feedback.md](12-adaptivity-and-feedback.md) | What may change after the query starts, and the determinism rule it obeys |
| [13-explain-and-testing.md](13-explain-and-testing.md) | How each arrow is proven, plan stability, the bisector, and the churn metric itself |
| [14-the-plan.md](14-the-plan.md) | P0 through P9, each with an exit measurement |
| [15-open-questions.md](15-open-questions.md) | The eight things this design does not settle |

## How to read this if you are short of time

Read `01-the-measured-state.md` and `07-the-physical-plan.md`.

The first is the evidence that there is a problem and what kind of problem it is. The second is the artifact whose absence causes it. Everything else in the folder is either the supply that makes the physical plan trustworthy, the lowering that makes it executable, or the testing that makes it safe.

If you have time for a third, read `06-reduction-and-join-order.md`, because it is where the 10x on join workloads comes from and it is the one place where rudb has a mechanism nobody else has.

## A note on sources

Every number about rudb in this folder was measured against the tree at `a99b3d8` on 18 September 2026 by running the command, and the command is named where the number is used. Every number from a paper is the paper's own and the paper is named. Where a claim comes from a search summary rather than the paper's text, it says so. This folder does not restate `../01-research-2026.md` or `../engine/01-survey.md`. It adds what has appeared since and it re-reads two things those documents treated as settled, which is document 02's job.
