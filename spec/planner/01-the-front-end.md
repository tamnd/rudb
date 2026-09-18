# The front end

The path from a string to an answer, what is on it today, and where the optimizer sits in it. This document exists because a planner is not a project on its own: it is a stage in a pipeline whose input is produced by the binder and whose output is consumed by the executor, and whose entire justification is a compatibility claim measured by a third repository.

## The path

``` text
  → rudb-parse      tokenize, match against the vendored PEG rule table, build an AST
  → rudb-bind       resolve names, types and overloads; produce a Plan
  → rudb-opt        rewrite the Plan                          ← this folder
  → rudb-exec       build an operator tree and run it
  → rudb            a result set
```

Five crates, ranks 1, 10, 11, 12 and 13 in `xtask/layers.toml`. The interesting property of that list is that the optimizer's two neighbours are both already built. `rudb-bind` produces a `rudb_plan::Plan`, `crates/rudb-exec/src/build.rs` consumes one, and today the plan goes from one to the other untouched because `crates/rudb-opt/src/lib.rs` is nine lines of module doc.

That is the seam to fill, and it is a narrow one. The whole of the optimizer's interface to the rest of the system is `fn optimize(plan: Plan, catalog: &Catalog, settings: &Settings) -> Result<Plan>`. Everything in this folder happens inside that function.

## What the parser already does, and why it is not the risk

`rudb-parse` is fifteen thousand lines, of which nine thousand are `generated/rules.rs`, generated from DuckDB's own PEG grammar at the pinned ref. That is the single most important compatibility decision in the project and it has already been made: the dialect is not reimplemented from the documentation, it is generated from the grammar, so it cannot drift between releases without the vendoring step saying so.

`rudb-compat parse corpus/m0.sql` comes back 62 of 62, and `corpus/dialect.sql` comes back 9 of 11 with both differences attributed to the binary on the machine being older than the vendored ref rather than to rudb. Both of those should disappear the moment the binary is an alpha at the vendored hash, which is the next section's point and is a test the harness already has written for it.

**So "connect to the parser" is not a parser task.** The parser is connected. `rudb-compat` reports 12803 passing records, which means text goes in and answers come out on every commit. What is missing is in the two stages after it.

## What the binder refuses, and what that costs

`crates/rudb-bind/src/lib.rs` names its own gaps in its module doc: subqueries, window functions, `WITH`, and every statement outside `SELECT`, `CREATE TABLE`, `DROP TABLE` and `INSERT`. The corpus prices them, from the bucket counts on issue #3:

| bucket | records | who closes it |
|---|---|---|
| `BoundedListExpression` | 2151 | parser and binder |
| `SetStatement` | 1336 | statement layer |
| `WithClause` | 1244 | binder |
| `ExplainStatement` | 1127 | binder, and document 12 of this folder |
| `SubqueryExpression` | 805 | binder, and document 05 of this folder |
| `PragmaStatement` | 800 | statement layer |
| `DeleteStatement` | 613 | write path, 2m |
| `AlterStatement` | 611 | catalog |
| `UpdateStatement` | 460 | write path, 2m |

Plus roughly 17345 catalog errors, a large share of which is cascade damage from an earlier failed `CREATE` in the same file and therefore worth more than its own count.

Two entries on that list are this folder's business and the rest are not. `SubqueryExpression` is, because a bound subquery that is not unnested is a nested loop over the outer query and is asymptotically wrong, so binding subqueries without document 05 existing buys 805 records of surface area on top of a plan shape that times out. `ExplainStatement` is, because `EXPLAIN` is the optimizer's user interface and 1127 records is a large amount of coverage for a feature that is mostly a printer over a structure `rudb-plan` already prints.

**The sequencing rule that follows.** Bind `WITH` before subqueries, because a common table expression is a plan reuse problem and not a correlation problem, and the binder can produce a correct plan for one with no help from this folder. Bind subqueries and land document 05 in the same milestone, not in adjacent ones, because the intermediate state is a database that accepts a query shape it cannot survive. That rule is the same one issue #3's own comment states about surface area on a slow layer, applied one level up.

## What the executor does with a plan, and what it will not do

`crates/rudb-exec/src/build.rs` is one match with one arm per logical operator and its doc comment is candid: "there is no physical plan and no cost based choice between two ways of running the same node, which is the honest description of tier 0: there is one implementation of each operator so there is nothing to choose between."

That is the right shape to grow from and it has one consequence for this folder. **Until there is a physical plan, every decision the optimizer makes has to be expressible in the logical plan.** Narrowing a `Get`'s column list is expressible. Moving a `Filter` below a `Join` is expressible. Choosing a hash join over a nested loop is not, because there is one join operator. Document 10 says when the physical plan arrives and what it holds; until then, the passes in documents 04 through 08 are all logical and all shippable, which is why they come first.

The one thing that has to be added early even though it is physical is the bit that says which side of a join builds. Document 10 section 10.2 argues that it belongs in the logical plan as a flag rather than waiting for a physical layer, because it is one boolean and waiting for a layer in order to place one boolean is how layers get built speculatively.

## Where DuckDB semantics come from

Not from the SQL standard and not from anybody's memory. From two places, in this order.

**The corpus, for behaviour.** `tamnd/rudb-compat` runs every `.test` file under `test/sql` at `v2.0-cyanoptera`, each file in its own process with ten seconds and two gigabytes on it. A `.test` file carries what every statement is supposed to produce, so the check needs no DuckDB binary and runs in fourteen seconds on every commit. When this folder says a rewrite has a correctness condition, the condition is checked by running the corpus with the rewrite on and off and comparing, which is document 12.

**A real binary, for the cases the corpus does not cover.** `rudb-compat query 'SELECT 1'` runs one statement on both and prints what differs, with errors compared by kind so that two engines rejecting a statement counts as agreement only when they reject it as the same kind of error. This is how a `NOT IN` null question gets settled, and document 05 section 05.6 says explicitly that it gets its answer this way rather than from a reading of the standard.

### The binary is an alpha at the hash we bind, not the newest release

This is a correction to how the reference binary has been chosen so far and it changes what every differential number means.

The parser is generated from DuckDB's grammar at a **commit**, not at a version: `crates/rudb-parse/grammar/VENDOR` records `ref: v2.0-cyanoptera`, `commit: cc7e7bac7fcb6e0994359965a87ac4f6a96f2e17`, retrieved 10 September 2026. That commit is what rudb's dialect *is*. Comparing against a v1.5.5 release binary therefore compares against a different language than the one rudb implements, and the two known `corpus/dialect.sql` failures are exactly that: `ORDER BY x ASCENDING` parses here and is a syntax error on 1.5.5, and `[1, 2] <-> [3, 4]` is array distance on 1.5.5 and cannot be one token under the v2.0 tokenizer. Neither is a rudb bug and neither is fixable while the binary is older than the grammar. It is worse than a missing measurement: it is a measurement that reports a compatibility failure where there is agreement.

v2.0 has not been released, general availability is projected for the second half of October 2026, but **it is in public alpha**, at `v2.0.0-alpha39998` at the time `spec/00-README.md` was written, and an alpha is a binary. So the rule:

**The reference binary is a v2.0 alpha at the same commit the grammar is vendored from.** Same hash, both sides, by construction. Two ways to get there and the second is better:

1. Build DuckDB from `cc7e7bac7f`. Always possible, costs a build.
2. **Move the vendor pin to a published alpha's commit.** Pick an alpha build that ships as a downloadable binary, take the hash it reports, and run `cargo xtask vendor-grammar` at that hash. Then the grammar and the binary are the same commit with no local build at all, and re-pinning is a normal, already-automated operation rather than a special case. `spec/20-the-grammar.md` says the grammar moves 76 lines across five patch releases within a series, so tracking alpha to alpha inside v2.0 is a small and bounded diff.

**Match on the hash, not on the version string.** `rudb-compat`'s `DuckDb::is_pinned_version` currently prefix-matches `PINNED = "v2.0"` against the version string, which would accept any v2.0 alpha, including one thousands of commits away from the vendored grammar on a branch that is by definition still moving. `duckdb --version` prints the short commit last, `v1.5.5 (Variegata) d8cdaa33fd`, so the check that is actually wanted is available for free: compare that short hash against the first ten characters of `VENDOR`'s `commit`. Same-hash is green; same-version-different-hash is a distinct, named, yellow state that every report carries, because on a pre-release branch it is a real and ordinary condition rather than an error.

**And the fallback stays, labelled.** When no alpha is on the machine, the harness still runs against whatever is there, and every number it produces is stamped with the binary's own version and hash. The rule from `rudb-compat`'s README is unchanged and is the reason this section exists at all: a percentage without a version attached to it does not mean anything. The amendment is only that on a pre-release target, the version is not enough either, the hash is the version.

**What this buys this folder specifically.** Document 05 section 05.6 settles the `NOT IN` null question by asking a real binary. An answer from 1.5.5 is an answer about a different engine on a question where v2.0 rewrote the surrounding machinery, and the optimizer-never-changes-an-answer rule below is only as good as the oracle behind it. Getting the oracle onto the hash we bind is a prerequisite for documents 05, 09 and 12 rather than a tidiness matter, and it is a day of work, not a milestone.

## The rule this folder is built around

**The optimizer must never change an answer.**

That is not a testing goal, it is the design constraint that decides what a pass is allowed to be. A pass that improves the average query and changes one answer is a regression of the whole project, because the compatibility claim is the product and the performance claim is what makes it worth having. Every rewrite in documents 04 through 08 is stated with its correctness condition next to it, and every one of those conditions is checked by the corpus rather than argued in review.

There is one deliberate exception and it is worth naming so it is not discovered as a bug. Floating point aggregation is not associative, so a `SUM` over a `DOUBLE` column can differ in the last bits depending on the order rows arrive in, and every rewrite that changes that order can change that sum. DuckDB has the same property and the corpus encodes tolerances where it matters. The rule is that the optimizer may change the order of floating point accumulation and may change nothing else, and the differential harness compares doubles with the corpus's own tolerance rather than bit for bit.

## The bisector, and why it is the highest-value tool in the repository

`rudb-compat`'s README already commits to this and it should be built before the third pass lands, not after the tenth: "Every failure is reduced and bisected automatically. A forty-line generated query that returns the wrong answer tells you nothing about why. The harness shrinks it and then bisects it against the optimizer passes, so the report names the pass that introduced the difference."

The mechanism this folder owes it is in document 03: every pass is individually addressable by name from a session setting, so the harness can run a query with pass *k* enabled and everything else off. With *n* passes that is *n* runs to localize a wrong answer instead of an afternoon of reading. At the dozen passes this folder specifies, a full per-pass sweep of the corpus is twelve times fourteen seconds, which is a nightly job rather than a per-commit one, and the per-commit version is on-and-off only.

This is the single reason the pass framework in document 03 insists on passes being pure functions with names rather than a sequence of mutations inside one function. Everything else about that choice is taste. This part is not.

## What we should take from this document

The parser is not the problem and is already generated from DuckDB's grammar, which is the reason the dialect cannot drift.

The binder's gaps and the optimizer's gaps are different lists. `WITH` and `SubqueryExpression` are binder work; the thing that makes a bound subquery survivable is document 05, and they ship in the same milestone or not at all.

The optimizer's whole interface is one function from a plan to a plan, both of its neighbours already exist, and nothing about it is blocked on the scan layer.

Every decision the optimizer can make today has to be expressible in the logical plan, because there is one implementation of each operator and therefore no physical plan yet.

The reference binary is a v2.0 alpha at the same commit the grammar is vendored from, matched on the hash rather than on the version string, with the pin moved to a published alpha's commit so that no local build is needed. Comparing against v1.5.5 compares against a different language and reports failures where there is agreement.

The optimizer must never change an answer, with floating point accumulation order as the one named exception, and the per-pass bisector in `rudb-compat` is what makes that property enforceable rather than aspirational.
