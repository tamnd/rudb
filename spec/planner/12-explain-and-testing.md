# EXPLAIN and testing

How a wrong plan is found, how a wrong answer is attributed to the pass that caused it, and what the planner shows a person who asks why a query was slow.

This document is longer than its subject usually gets, for one reason. The rule from document 01, **the optimizer must never change an answer**, is not enforceable by review. Twelve passes over a plan language with eight join kinds and three-valued logic will produce a wrong answer, and the difference between a project that ships and one that does not is whether that wrong answer takes an afternoon or ten minutes to attribute. Everything here is about the ten minutes.

## 12.1 The differential, which already exists

`tamnd/rudb-compat` runs every `.test` file in DuckDB's `sqllogictest` corpus at the vendored ref, each file in its own process under a ten-second and two-gigabyte cap. The files carry expected results, so the run needs no DuckDB binary and finishes in about fourteen seconds. That is the oracle.

Where the corpus does not cover a case, `rudb-compat query` runs one statement against both engines and diffs, with errors compared by kind so that two engines rejecting a statement counts as agreement only when they reject it the same way. Document 01 section "The binary is an alpha at the hash we bind" specifies what that binary must be, and this document assumes it: a v2.0 alpha at the same commit as the vendored grammar, matched on the hash. An oracle that is a different version of the language is not an oracle.

## 12.2 The one test that matters most

**Run the corpus with the optimizer on and with the optimizer off, and require identical results.**

That single check is the rule of document 01 made mechanical. It needs no new expected outputs, no hand-written cases, and no judgement about what the right answer is, the unoptimized plan *is* the right answer, because it is the plan the binder produced and the binder's correctness is tested separately. Any difference is an optimizer bug, immediately, by definition.

It runs on every commit. At fourteen seconds a pass it is twenty-eight seconds of CI, which is not a budget conversation.

**And then the per-pass sweep.** With *n* named passes, run the corpus with pass *k* enabled and the rest off, for each *k*. A difference localizes to one pass in one run. At a dozen passes that is roughly three minutes, which is a nightly job rather than a per-commit one; the per-commit version stays on-and-off only.

The mechanism this needs is in document 03 and it is the reason every pass has a name: `SET disabled_optimizers = 'filter_pushdown,join_order'`, DuckDB's own spelling, which the corpus files already use.

## 12.3 The bisector

`rudb-compat`'s README already commits to this and it should exist before the third pass lands, not after the tenth:

> Every failure is reduced and bisected automatically. A forty-line generated query that returns the wrong answer tells you nothing about why. The harness shrinks it and then bisects it against the optimizer passes, so the report names the pass that introduced the difference.

Two halves and they are worth separating because they have different costs.

**Bisection over passes** is cheap and mechanical: binary search over the `disabled_optimizers` set, log *n* runs, output is a pass name. Build this first; it is a loop around a thing that already exists.

**Reduction of the query** is the harder half, shrink the SQL while preserving the disagreement. It is the standard delta-debugging shape and it can be built incrementally: dropping `SELECT` items, dropping predicate conjuncts, dropping joins, replacing a subquery with a constant. Each reduction step is valid only if the disagreement survives it, so the harness cannot be wrong, only slow.

This is the highest-value tool in the repository for this folder and it is worth more than any two passes in it.

## 12.4 Plan tests are text in, text out

`rudb-plan` has a printer and a parser that round-trip. Document 02 draws the consequence and it decides how every pass in this folder is tested: **a pass test is an input plan as text and an expected output plan as text.** Diffable, reviewable in a pull request, and greppable.

No assertions about tree structure. No `assert!(matches!(plan.node(root), Node::Filter { .. }))`. A test written that way asserts one thing about a plan and silently permits everything else, and when it fails it says nothing about what actually changed.

The corollary is that cardinality estimates must not appear in the plan's printed form, which is why document 02 puts them in a side table keyed by `NodeRef`. An estimate in the plan text means every plan test churns when the cost model moves.

## 12.5 Plan stability

A committed baseline of the chosen plan per query, over TPC-H, TPC-DS, JOB and ClickBench, diffed in CI.

The point is not that the baseline plan is correct. It is that **a plan change becomes a reviewable event**. Someone tuning the cost model who silently changes the join order on eleven TPC-DS queries should have to look at those eleven and say so in the pull request. Without this, plan changes arrive invisibly and are attributed to whatever performance run notices them three weeks later.

Document 07 names this specifically for join ordering, which is where it matters most, and the mechanism is the same one: the plan's text is stable, so a baseline is a file and a check is a diff.

## 12.6 Estimate quality is published

Document 06 section 06.5: **the q-error distribution on CEB is published alongside the timings**, as a distribution and not a mean, because the tail is what causes the catastrophe.

`spec/engine/11-optimizer.md` section 11.8 requires it. The reason to hold to it: an engine whose estimates are bad but whose execution is fast is a different engine from one where both are good, and the difference only shows up on workloads that are not in the benchmark. Publishing the estimate quality makes that visible before a user finds it.

**Optimality gap on JOB** belongs in the same report, measured against the best plan found by exhaustive search where exhaustive search is feasible. JOB-Complex (arXiv:2507.07471) reports PostgreSQL 16 at 1.18 on JOB-light and 1.99 on JOB, so the number is directly comparable to a published one, which is rare enough to be worth the effort.

## 12.7 Planning time is asserted

Per query, against a committed budget, in CI.

Document 03 gives the pipeline a deadline scaled to estimated execution cost, and the argument is `spec/02-the-goal.md`'s per-query floor: a query that plans for four hundred milliseconds and runs for two hundred is a query the optimizer made slower. Without an assertion that case is invisible, because nobody profiles the planner.

Two things to record rather than only assert:

- **how often the join-ordering budget fires**, and with what relation count and graph density. That counter is what tells us whether DPconv (document 07 section 07.4) is ever worth implementing, and without it that question can only be answered by guessing.
- **how often a dependent join survives optimization**, which document 05 says means a missing equivalence rather than an unavoidable query.

## 12.8 EXPLAIN

`EXPLAIN` shows the logical plan, the physical plan and estimated cardinalities. `EXPLAIN ANALYZE` shows actual cardinalities, per-operator time and memory, encoded-versus-decoded vector counts, which execution tier each pipeline ran at, and every adaptive decision with the observation that triggered it.

**Estimated and actual side by side, with the ratio.** That ratio is the first thing anyone diagnosing a bad plan wants and computing it by hand is tedious. It is also, not coincidentally, the q-error of section 12.6 shown per node, so the diagnostic tool and the published metric are the same number.

Four things this folder specifically requires `EXPLAIN` to say, each because a document above depends on someone being able to see it:

- **when the join-ordering budget fired** and a greedy plan was returned (document 07 section 07.5)
- **when a dependent join survived** unnesting (document 05)
- **which runtime filters were built, at which tier, and what they rejected** (document 09), an unhelpful filter that was correctly abandoned should be visible as such rather than silently absent
- **which physical representation each scanned column was produced in** (document 10 section 10.3), because the layout-adaptation pass is a bet and the bet is unreadable otherwise

`EXPLAIN` output is **not** a stable interface and is excluded from the compatibility guarantee by `spec/12-duckdb-compat.md` section 12.5. Matching DuckDB's explain text would freeze rudb's optimizer to their operator names, which is the one place where compatibility would buy nothing and cost the ability to have a different optimizer. It is stable enough for tests within one minor version, which is what plan stability in section 12.5 relies on.

## 12.9 What is not built

**No plan-shape fuzzing before the basics.** Random query generation finds real bugs and it finds them in a form that takes a day each to understand. The corpus is a large body of queries somebody already wrote down the right answer for, and it is not exhausted.

**No performance assertions in unit tests.** Timing in a unit test is a flaky test. Performance lives in `rudb-bench`, with quartiles and a machine name attached.

**No expected-output tests written by hand where the corpus covers the case.** A hand-written expectation is a second opinion about DuckDB's behaviour, and document 01's rule is that opinions do not decide semantics.

## What we should take from this document

Corpus on against corpus off, per commit, identical results required. That one check is the optimizer's correctness rule made mechanical and it needs no new expected outputs.

Per-pass sweeps nightly, and a bisector over the `disabled_optimizers` set built before the third pass lands. Pass-bisection is cheap and comes first; query reduction is the harder half and can arrive incrementally.

Pass tests are plan text in and plan text out, never structural assertions, which is why estimates live beside the plan and not in it.

Plan stability baselines make a plan change a reviewable event rather than a surprise found three weeks later.

Publish the q-error distribution on CEB and the optimality gap on JOB, and assert planning time per query with counters for how often the budget fired and how often a dependent join survived.

`EXPLAIN ANALYZE` prints estimated against actual with the ratio, plus the four things this folder needs to be visible: budget exhaustion, surviving dependent joins, runtime filter tiers and outcomes, and per-column physical representation.
