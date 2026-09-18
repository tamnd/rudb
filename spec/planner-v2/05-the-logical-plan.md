# The logical plan

Artifacts 2 and 3 from document 03, and the arrow between them. This is the layer rudb already has and does best, so most of this document is about what to add rather than what to change, and the largest single item is where the peepholes go.

## 5.1 What is already right

`rudb-plan` is 6,548 lines and the design is arena based: `NodeRef`, `ExprRef`, `StrRef` and `Slice` index into pools, so a plan is a flat value with no pointer chasing and no lifetime problems. The printer is 704 lines, the parser is 1,559, and `tests/roundtrip.rs` plus `tests/property.rs` are 1,468 lines asserting that parse of print is the identity.

Two consequences that this folder depends on and that are easy to undervalue. A pass test is a text file. And a plan survives being handed to another thread, stored, compared and diffed, which is what makes plan stability testing possible at all.

`Node::Get` carries its projection in `columns`, so a two column scan of a 105 column table is a two column scan in the plan rather than a filter over a wide one. Every operator that introduces new columns carries a table index; `Filter`, `Sort`, `Limit`, `TopN`, `Distinct` and `Join` deliberately do not, because they pass columns through and a binding that survives a filter should not be rewritten by it.

None of that changes.

## 5.2 The four analyses

Every pass is written in terms of one or more of these, and they belong in `rudb-plan` next to the plan rather than in `rudb-opt` next to the passes, so that whoever adds a node maintains them rather than whoever remembers.

**Column liveness.** Which outputs of which node are read by something above it. Drives `UnusedColumns`, which is already the highest value pass in the tree.

**Null propagation.** For each expression, whether it can be null, and for each node, which of its outputs are null-extended by an outer join above. This is what makes filter pushdown through an outer join correct, and `crates/rudb-opt/src/nulls.rs` at 415 lines is already most of it.

**Functional dependency and uniqueness.** Which sets of columns are unique at which node. Declared keys give the base cases, a `DISTINCT` or a `GROUP BY` gives one, an equi-join with a unique build side preserves the probe side's uniqueness. This is the analysis that unlocks the whole enabling class from document 04 section 4.3: `DISTINCT` elimination, group by elimination, join elimination and outer join simplification are all one analysis with four consumers. It does not exist today and it is the single most valuable addition to `rudb-plan`.

**Row identity preservation.** For every output of every node, whether it still carries a base table row id. `../graph/06-the-optimizer.md` section 6.1 asks for this pass and asks for it to live in `rudb-plan` beside the other plan properties for the same maintenance reason. It is mechanical, it has no cost model in it, and it is the precondition for everything in document 06.

All four are computed by one walk, cached on the plan, and invalidated by any pass that mutates. `crates/rudb-opt/src/walk.rs` at 704 lines is the existing walk machinery and this is what it grows into.

## 5.3 What is missing from the node set

`Node` covers what the binder can produce, which `../planner/02-what-a-plan-is.md` argued was the right discipline: an operator nothing constructs is an operator whose printer, validator and rewrite rules have never run. That discipline stands. What follows is the list of nodes that are about to have constructors, with the document that constructs each.

**`Node::Reduce`.** A semi-join reduction step. Names the edge, the direction, the filter kind (exact bitmap, Bloom, min-max) and the abort budget. Constructed by document 06. It is a node rather than an annotation because a reduction changes cardinalities and the estimator has to see it.

**`Node::LinkJoin`.** An equi-join answered through a stored link. Constructed by document 06, from `../graph/`. Alternatively this is `Node::Join` with a link field, and document 07 section 7.2 argues for the node because the two have different cost functions and different outputs.

**`Node::MarkJoin` with a null mode.** rudb already has a `MARK` join kind on `Node::Join`. What it does not have is the three-valued mode that `IN`, `NOT IN`, `EXISTS` and `NOT EXISTS` need to be distinguished by, which Birler and Neumann's CIDR 2026 paper makes into an algorithm rather than a special case. Document 11 section 11.7.

**`Node::Window` frame sharing.** Exists. Not this folder's subject.

Nothing else. In particular there is no `Node::Materialize` and no `Node::Exchange`, because materialization points and parallelism are physical decisions and belong in artifact 5.

## 5.4 The pass order, revisited

The current eleven in order, from `crates/rudb-opt/src/lib.rs`:

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

The arguments for that order in the module documentation are good and mostly survive. Folding before pruning because folding removes column references. Pushdown between them because moving a filter below a projection rewrites it in terms of the projection's inputs. Empty result pullup after pushdown because pushdown is what makes a predicate unsatisfiable. Limit pushdown immediately before top-n because top-n fuses the pair.

Two changes.

**`sides::BuildSideProbeSide` moves out.** It is a physical decision. It is in `rudb-opt` because there is nowhere else, which is document 01's whole argument. It moves to artifact 5 when artifact 5 exists, and `Node::Join::build` becomes a physical field. Until then it stays where it is and nothing breaks.

**`late::LateMaterialization` is at the wrong altitude.** Late materialization decides when to fetch a column's values rather than its positions, which is a choice about representation, which is physical. Same treatment: it moves, not today.

The rest of the order stands. The additions go in these positions.

**Unnesting, before everything.** `crates/rudb-opt/src/unnest.rs` is already 1,264 lines and `dependent.rs` is 155, so this exists in part. Its position ahead of the sequence is not negotiable: a plan with a dependent join in it is a plan the other ten passes cannot reason about, and `../planner/05-subquery-unnesting.md` remains the reference for the algorithm and the five special shapes.

**Transitive predicate generation, with filter pushdown.** `crates/rudb-opt/src/transitive.rs` at 436 lines is currently a helper of the filter pass. It should be a named pass so it can be toggled and bisected, running immediately after pushdown, because a predicate derived from `a.x = b.x AND a.x = 5` has to be pushed after it is derived.

**Aggregate argument common subexpression elimination, after folding.** This is `mark_affine_sums` done properly. Section 5.6.

**The four enabling rewrites, after annotation.** Join elimination, group by elimination, `DISTINCT` elimination and sort elimination all read `Exact` facts and all depend on the uniqueness analysis. They run after artifact 4 exists, which means the pass pipeline is cut in two: a mechanical prefix that needs no facts, then annotation, then a fact-consuming suffix. That split is new and it is a better structure than one long list, because it makes it syntactically impossible for a mechanical pass to read a number.

## 5.5 Fixed sequence, and the one place a loop is allowed

`../planner/03-the-pass-pipeline.md` argued for a fixed sequence over a loop to a fixed point, on the grounds that a fixed point is easy to write and hard to bound, and that a version which stops after a few rounds gives a plan that depends on how many rounds it was given. That argument is correct and this folder keeps it.

The one place a loop is defensible is the enabling suffix, because those four passes genuinely feed each other: join elimination removes a join, which makes a group by key unique, which eliminates the group by, which makes a sort unnecessary. Running them once in a fixed order gets most of it and running them to a fixed point gets all of it.

The rule that makes the loop safe: **each of the four is monotone in the same direction.** Every one of them removes a node and none of them adds one. A loop over passes that only delete terminates in at most as many rounds as there are nodes, and the bound is checkable. If a fifth pass is ever added to that group and it adds a node, the loop goes away and the sequence becomes fixed again.

## 5.6 Where the peepholes go

This is the section document 01 was written to justify.

Every optimization currently living inside an operator has exactly one of three correct destinations, and the way to tell which is to ask what information it needs.

**If it needs only the plan, it is a logical pass.** `mark_affine_sums` is the example. It looks at the aggregate's call list and the expressions under it. It needs no statistics, no encoding, no data. So it is a pass, its general form is common subexpression elimination over aggregate arguments, and it fires on every spelling rather than on `CAST(smallint AS integer) + literal`. Same for the cross product rewrites in the recent commits, which are pattern matches on `Node::Cross` under `Node::Filter` and belong next to filter pushdown. Same for splitting an `AND` join condition into conjuncts, which is normalisation and belongs in folding.

**If it needs facts or encodings, it is a physical choice.** Whether to group on codes or on strings. Whether to build a shared concurrent table or a partitioned one. Whether the join is a lookup or a gather. Whether to reserve four megabytes or four hundred. All of these need a number or a capability the logical plan does not have, and all of them are currently `if`s inside `rudb-exec`. Document 07.

**If it needs the data in front of it, it is kernel selection.** Whether this morsel's selection is dense enough to copy rather than carry. Whether this chunk's codes fit in a byte. Whether this partition is small enough to sort in cache. These are properties of a specific chunk, not of the query, and they are the only things that legitimately live below artifact 6. Document 10 section 10.4.

A fourth destination exists and it is the one to be suspicious of. **If it needs to know which benchmark query it is, it is not an optimization.** The test is whether the rule fires on a query nobody has written yet. `mark_affine_sums` matching `HUGEINT` return type and `SMALLINT` argument type fails that test as written and passes it once generalised, which is why the recommendation is to generalise rather than to delete.

## 5.7 The budget

Planning time counts against the per-query floor in `../02-the-goal.md`, and a planner that takes ten milliseconds on a query that runs in three is a regression no matter how good the plan is.

The budget is stated as a fraction of the estimated execution time with a floor, and it is enforced by checking a deadline between passes rather than inside them. A pass that has not finished when the deadline passes is abandoned and the plan it was rewriting is discarded in favour of the plan before it, which is safe because every pass is a function from a valid plan to a valid plan.

The only pass that can plausibly spend real time is join ordering, and document 06 gives it its own budget and its own fallback. Everything else in the sequence is linear or near linear in plan size, and plan size is bounded by query text.

`Context` grows a deadline field. `../planner/03-the-pass-pipeline.md` already anticipated this and `crates/rudb-opt/src/pass.rs` documents why it is not there yet, which is that a field no pass reads is a field whose meaning nobody has had to decide.

## 5.8 What the logical plan must not gain

Three temptations, listed because each will come up.

**Cost.** A logical node must not carry a cost. Cost depends on the implementation and the implementation is chosen in artifact 5. Cardinality is different and belongs in artifact 4, because cardinality is a property of the data and the query, not of how they are run.

**Encoding.** A logical node must not know that a column is dictionary encoded. Document 03 section 3.4 draws this line and it will be under pressure from the grouping rewrites, because grouping on codes is enormously valuable and the temptation to decide it early is real. Decide it in artifact 5 where the capability is visible.

**Threads.** A logical node must not know how many. Parallelism is physical, and the current `Stream::parallel` and `Sink::parallel` booleans are operator properties rather than plan properties, which is the right place for them.

## What we should take from this document

The logical layer is the healthiest part of the tree and needs four additions: a uniqueness analysis, a row identity analysis, a handful of nodes with named constructors, and a split of the pass sequence into a mechanical prefix and a fact-consuming suffix with annotation in between.

Two existing passes are at the wrong altitude and move up to artifact 5 when it exists.

Every peephole currently inside an operator routes to one of three places by asking what information it needs, and the general form of the routing rule is more useful than any individual peephole, because it is what stops the next one from landing in `group.rs`.
