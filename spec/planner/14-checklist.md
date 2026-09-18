# The checklist

Document 13 is the schedule. This one is the form the schedule takes in the tracker: what each pull request contains, what amendments #65 needs, and what belongs to somebody other than the optimizer.

Items are sized to be one pull request each. Where an item is larger than that it says so.

## 14.1 Amendments to #65

#65 is a good issue and most of it stands. Five changes.

- [ ] **Swap the first two rewrites.** The issue lists filter pushdown first and projection pushdown second. Document 13 section 13.1 argues for the reverse *for the first two pull requests only*, on the grounds that projection pushdown needs no operator change and its correctness condition is one sentence, so it is the right place to land the framework. Value ordering is unchanged; the issue's reasoning is not being contradicted.
- [ ] **Split subquery unnesting into two items.** The issue has one bullet. Document 05 prices the five special shapes at about a week and the general algorithm at about a month, and they ship separately with an error in between naming what is not covered.
- [ ] **Add the four analyses as an explicit prerequisite item.** Elementwise, constant, table set, null rejecting. They are not mentioned in #65 and every rewrite in it is written in terms of at least one.
- [ ] **Correct the predicate transfer citation.** The issue says "the CIDR 2023 paper". It is CIDR 2024, arXiv:2307.15255, and the successors are RPT (arXiv:2502.15181) and Parachute (arXiv:2506.13670). Document 08 has the numbers.
- [ ] **Add the two escape hatches as first-class items rather than implied ones:** predicate transfer's two-equality-edge minimum, and join ordering's greedy-first-always-available. Both are what keep `spec/02-the-goal.md`'s second axis from being violated by this milestone, and both are easy to defer into nonexistence if they are not written down.

## 14.2 Stage 0: the framework

- [ ] `Expr` analyses: elementwise; constant with the volatile-function list taken from DuckDB at the vendored commit; table set as a bitset over `Node::table_index`, cached by `ExprRef` and invalidated when the pool grows; null rejecting, defaulting to false for anything not proven
- [ ] The `Pass` trait with `name()` and `run(plan, ctx)`, and `Context` holding catalog, statistics, settings and deadline
- [ ] `SET disabled_optimizers = '...'`, DuckDB's spelling, since the corpus files already use it
- [ ] `Plan::validate` asserted in debug builds after every pass, and the root-schema-unchanged check
- [ ] The text-in / text-out pass test harness, built on `rudb-plan`'s existing printer and parser
- [ ] The idempotence assertion, with an opt-out for passes that declare themselves non-idempotent

## 14.3 Stage 1 to 5: what pulls forward today

Nothing in this section is blocked on 2h or 2j. All of it is expressible against `rudb-plan` as it stands.

- [ ] **Projection pushdown.** Required-set walk down, rebuild up, re-bind. Include the `Sort`-key rule and the positional `SetOp` rule, and default to keeping a `Project` expression that can raise.
- [ ] **Corpus on-against-off gate in CI.** Lands with projection pushdown, not after it.
- [ ] **Plan stability baselines** for TPC-H, TPC-DS, JOB and ClickBench, committed and diffed.
- [ ] **Expression simplification**, to a fixed point with a round bound.
- [ ] **The pass bisector**, binary search over `disabled_optimizers`. Before the third pass lands. Query reduction is a separate, later item.
- [ ] **Filter pushdown**, in two pull requests: the pass-through table and inner/left joins first; then anti, full, semi, single and positional with the null-rejecting outer-to-inner conversion. The second one is where the classic wrong answers live and it should be reviewed as such.
- [ ] **Transitive predicates**, with the equality-class code factored so document 08 can reuse it, and the derived-predicate marker so it terminates.
- [ ] **The build-side flag** on `Node::Join`, set from the estimate, honoured by `crates/rudb-exec/src/build.rs`.
- [ ] The six deferred local cost comparisons from #65, as they become real, each is ten lines and none needs a physical plan.

## 14.4 Stage 6 and after: what waits, and for what

- [ ] **Statistics plumbing.** KMV sketches out of `crates/rudb-encoding/src/sketch.rs` into a side table keyed by `NodeRef`. Waits on the encoder producing them for real data, which is 2d/2e.
- [ ] **The estimator.** Four rules plus the four join-kind rules for Semi, Anti, Single and outer.
- [ ] **The per-table sample** for correlated conjunctions.
- [ ] **The cost model**, fitted to `rudb-bench` numbers from layers three through nine, kept additively separable, with the reason in its own doc comment.
- [ ] **q-error published on CEB** as a distribution, alongside the timings.
- [ ] **Join ordering**, in three pull requests: hypergraph and conflict edges; greedy plus the budget and the `EXPLAIN` reporting; DPhyp.
- [ ] **Unnesting, five shapes**, behind an error naming what is uncovered.
- [ ] **Unnesting, general.** The largest single item in the folder.
- [ ] **Runtime filters.** Min/max tier first on its own, since it has no memory cost and no threshold; then the `IN` and Bloom tiers with the executor choosing between them.
- [ ] **Predicate transfer**, after the equality classes and the Bloom machinery exist. The two-edge minimum in the same pull request as the pass itself.
- [ ] **The multi-consumer materialization node**, on its own, before CSE.
- [ ] **CSE and common subplan elimination.**
- [ ] **Limit pushdown and top-N**, with `limit + offset` handled, q39 and q42 of ClickBench have `OFFSET` and one is deep.
- [ ] **Set-op rewrites, aggregate and distinct rewrites, window rewrites, empty and constant pruning.**
- [ ] **Layout requirement propagation**, after the scan can honour the annotation.

## 14.5 Not the optimizer's, but this folder depends on it

Four items that belong to other parts of the tree and that documents in this folder are blocked on or weakened by. Each should be an issue of its own rather than a line in #65.

- [ ] **The reference binary becomes a v2.0 alpha at the vendored commit.** `rudb-compat`'s `DuckDb::is_pinned_version` currently prefix-matches `PINNED = "v2.0"` against the version string, which accepts any alpha regardless of how far it is from `cc7e7bac7f`. Change it to compare the short hash `duckdb --version` prints against the first ten characters of `crates/rudb-parse/grammar/VENDOR`'s `commit`, and either build DuckDB at that commit or, better, re-run `cargo xtask vendor-grammar` at a published alpha's commit so that the grammar and the binary are the same hash with no local build. Same-version-different-hash becomes a named, reported state rather than a pass. Document 01 has the argument; the two known `corpus/dialect.sql` failures should disappear the day this lands, and there is already a test asserting exactly those two and only those two.
- [ ] **The binder learns `WITH`.** 1244 corpus records. Document 01 says bind it before subqueries.
- [ ] **The binder learns subquery expressions.** 805 records. Ships in the same milestone as stage 8, or it is surface area on a shape that times out.
- [ ] **`EXPLAIN` and `EXPLAIN ANALYZE`**, with estimated against actual and the ratio, plus the four things document 12 section 12.8 requires be visible: budget exhaustion, surviving dependent joins, runtime filter tiers and what they rejected, and per-column physical representation.

## 14.6 The gates

#65's benchmark gate is JOB, CEB, TPC-DS all 99 and TPC-H SF100, with the target being to beat DuckDB on total CPU seconds across TPC-H SF100. That is the right gate and it is correctly placed after 2h and 2j.

Three intermediate checks, so that stages 1 through 5 are not unmeasured for months:

- [ ] **After stage 1:** the four corpus files currently killed at ten seconds. Projection pushdown alone may not fix them; the number is worth recording either way.
- [ ] **After stage 3:** the recorded prediction from document 04 section 04.5, filter and projection pushdown together move TPC-H SF10 total CPU seconds by more than 2x, with Q19 and Q21 moving more than the rest. If they do not, the binding constraint is the tier-0 operators, which says finish 2f before finishing this, and that is worth knowing early.
- [ ] **After stage 5:** ClickBench per-query, optimizer on against off, as an attribution row in the ledger of `spec/engine/02-baseline.md` section 2.8.

And one gate that is not a benchmark:

- [ ] **Planning time asserted per query in CI**, with counters for how often the join-ordering budget fired and how often a dependent join survived optimization.

## What we should take from this document

#65 needs five amendments: swap the first two rewrites for framework reasons, split unnesting into two, add the four analyses as a prerequisite, fix the predicate transfer citation to CIDR 2024, and make the two escape hatches first-class items.

Stages 0 through 5 pull forward and are unblocked today; stages 6 onward want the operators and the statistics.

Four items outside the optimizer that this folder depends on, and the first of them, getting the reference binary onto the hash we bind, is a day of work that makes every differential number mean what it says.

Three intermediate benchmark checks so the early stages are measured rather than assumed, and one recorded prediction that is allowed to be wrong.
