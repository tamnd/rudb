# Subquery unnesting

The pass a dataframe library never needs and a SQL database cannot ship without. It is third in the pipeline, it is the highest-value single rewrite in the set on TPC-DS, and it is the one where the difference between rudb and DuckDB will be a wrong answer rather than a slow query.

## Why it is not optional

A correlated subquery that is not unnested is a nested loop: the subquery is evaluated once per row of the outer query. That is asymptotically wrong in the same way the nested loop join is asymptotically wrong, and no amount of work on the operators underneath changes the exponent.

TPC-DS has ninety-nine queries and a large share of them are correlated. `spec/09-optimizer.md` section 9.2 calls unnesting "the single most valuable rewrite in the set" and says it "is not optional if TPC-DS is a target". The corpus prices the binder side of it at 805 `SubqueryExpression` records, and document 01 argues that binding those without this pass existing is surface area on a shape that times out.

## The general algorithm

Neumann and Kemper, *Unnesting Arbitrary Queries*, BTW 2015. The reference implementation is Umbra's and DuckDB implements it too.

The shape is two steps. First, express the correlation as an operator: a **dependent join**, which is the same thing the literature calls a lateral join or a correlated join, whose right side may reference columns of its left side. Binding produces one of these for any correlated subquery, mechanically, with no cleverness. Second, **push the dependent join down** the right subtree, applying an equivalence at each step, until the right side no longer references the left, at which point the dependent join is an ordinary join and the correlation is gone.

The key insight that makes it work: the subquery result depends only on the values of the correlated columns, so rows of the outer query with the same correlated values produce the same subquery result. The transformation therefore starts by taking the **distinct** values of the correlated columns from the outer side and joining the subquery against those, which bounds the work by the number of distinct correlation values rather than by the number of outer rows.

**Why implement the general algorithm rather than a catalogue of recognized shapes.** Because it terminates on arbitrary nesting, and a catalogue does not. A query with a correlated subquery inside a correlated subquery inside an aggregate is a query the catalogue does not have a rule for and the general algorithm handles without a special case. The catalogue is still worth having, for the reason in the next section, but not instead.

**The one part that has no equivalence.** If the right subtree contains an operator for which no push-down equivalence exists, the dependent join stops there and the plan keeps it. The executor then has to have something to run, and the honest thing to run is a nested loop over the distinct correlation values. That is slow and it is correct, and the alternative is a query that does not execute. Name it in `EXPLAIN` loudly, because a dependent join that survived optimization is almost always a missing equivalence rather than an unavoidable query.

## The five special shapes

The catalogue produces better plans than the general algorithm for the shapes that are common, so it runs first and the general algorithm is the fallback. All five already have a join kind waiting for them in `rudb-plan`, which is why the eight kinds were built before the optimizer that produces them.

**`EXISTS` becomes a semi join.** `WHERE EXISTS (SELECT ... WHERE inner.k = outer.k)` is `Join { kind: Semi }` on `k`. The subquery's projection list is discarded, which is why the correlation condition has to be lifted out of it first.

**`NOT EXISTS` becomes an anti join.** Same shape, `kind: Anti`. Note that the anti join's null behaviour is the easy one: `NOT EXISTS` is true when the subquery is empty, regardless of nulls, so no special rule is needed. This is the case people expect `NOT IN` to behave like and it does not.

**`IN` becomes a semi join.** `x IN (SELECT y FROM ...)` with the correlation lifted. Uncorrelated `IN` against a subquery is a semi join against a materialized side.

**`NOT IN` becomes an anti join with the null rule**, which is section 05.6 and is the one everybody gets wrong.

**A scalar subquery becomes a `Single` join.** `kind: Single` emits at most one right row per left row and pads with nulls when there is none, which is exactly scalar subquery semantics: no match is null, not no row. The more-than-one-row case is a runtime error and the join has to produce it, which means the operator counts rather than stopping at one. An uncorrelated scalar subquery is simpler still: evaluate it once, and if it is in a filter, treat the result as a constant. A 2024 line of work, *Enabling Data Dependency-based Query Optimization* (arXiv:2406.06886), argues that this is sometimes better than unnesting even for the correlated case, since a predicate containing a scalar subquery result can be evaluated once and treated as a constant whose value is unknown until execution; it names cardinality estimation and partition pruning as the two things that get harder. Worth knowing about; not worth building before the general algorithm exists.

## 05.6 `NOT IN` and nulls

This gets its own section because every database has been wrong about it at some point and because the correct behaviour is not obvious from the syntax.

`x NOT IN (SELECT y FROM t)` is defined as `NOT (x IN (...))`, which is `NOT (x = y1 OR x = y2 OR ...)`, which by De Morgan is `x <> y1 AND x <> y2 AND ...`. If any `yi` is null, that conjunct is null, so the whole conjunction is null or false and never true. **A single null anywhere in the subquery result makes `NOT IN` return no rows at all**, regardless of what `x` is.

That is not what an anti join does. An anti join returns left rows with no match, and a null on the right does not match anything, so a plain anti join returns the rows that `NOT IN` must not return.

The standard fix is a **mark join**: instead of a boolean "has a match", the join produces a three-valued marker, true when there is a match, false when there is no match and no null on the right, and null when there is no match but there was a null. The filter above then keeps only the rows where the marker is false, and null markers are dropped by `WHERE`'s own rule.

**How rudb gets the exact behaviour.** From DuckDB, through the corpus, and from `rudb-compat query` on the cases the corpus does not cover. Not from a reading of the standard, and not from the paragraph above, which is a summary written to explain why the case is hard and is not the specification. `spec/engine/11-optimizer.md` section 11.5 states this rule and it is repeated here because this is the single place in the folder where a plausible-looking implementation is wrong in a way that passes casual testing: the null case does not appear unless the data has nulls in the subquery column.

The same class of care applies to `x <> ALL (...)`, `x = ANY (...)` and the other quantified comparisons, all of which have the same three-valued structure.

## Where unnesting interacts with the rest of the folder

**With pushdown.** Unnesting runs first, at position 3, so that documents 04's passes never see a dependent join. This is stated in document 03 and the reason is that a pass which has to decide whether a predicate references a correlated column is a pass with a wrong answer available.

**With join ordering.** Unnesting produces joins, and the joins it produces are exactly the ones with the worst cardinality estimates, because a semi join's output size is bounded by its left side and an estimator that treats it like an inner join will be badly wrong. Document 06 section 06.5 says what to do about that.

**With predicate transfer.** A semi join is an edge in the join graph and a `Single` join is not, because a `Single` join's output is one row per left row and reducing its right side does not reduce its output. Document 08's `SafeSubjoin` is the general form of that question.

**With the corpus.** Unnesting is where a compatibility gap will show up first, because DuckDB's exact behaviour on correlated subqueries with nulls is subtle and any difference is a wrong answer rather than a slow query. `spec/09-optimizer.md` says document 14's corpus specifically targets this area, and document 12 of this folder says how.

## What this costs to build

The general algorithm is the largest single item in this folder by code volume. Umbra's is the reference and DuckDB's is readable. The realistic estimate is that the five special shapes are a week and the general algorithm is a month, and the ordering that follows is: build the five special shapes first, behind a check that falls back to an error for anything they do not cover, so that the common queries work while the general one is written. An error naming what was written is the rule the whole front end already follows, per `crates/rudb-bind/src/lib.rs`.

**Do not skip the general one.** A catalogue that grows one shape at a time as queries are found is a catalogue that is still growing in three years, and every shape it does not cover is a query that errors or times out. The five shapes are a schedule, not a design.

## What we should take from this document

The pass is not optional, it is third in the pipeline, and the five special shapes ship before the general algorithm as a schedule rather than as a substitute.

A dependent join is the representation, pushing it down the right subtree until the correlation resolves is the algorithm, and taking the distinct correlated values first is what bounds the work.

All five shapes already have a join kind in `rudb-plan`, including `Single` for the scalar case, which is why unnesting is cheaper to build here than it would be in a plan language designed without it.

`NOT IN` with a null anywhere in the subquery returns no rows, a plain anti join returns the opposite, and the fix is a three-valued mark. The exact behaviour comes from the corpus and from a real binary, not from this document.

A dependent join that survives optimization means a missing equivalence, and `EXPLAIN` should say so loudly rather than quietly running a nested loop.
