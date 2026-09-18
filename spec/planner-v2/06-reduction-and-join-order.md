# Reduction, and what is left of join ordering

This is where the 10x on join workloads comes from, and it is the one document in this folder where rudb has a mechanism that is not available to the systems it is measured against.

The claim in one sentence: **because `../graph/` stores the join as a link over a dense row id space, rudb's semi-join reduction can be exact rather than approximate, which turns the approximation every other engine ships into the algorithm Yannakakis specified, at one sequential pass per edge.**

Document 02 sections 2.1 through 2.3 are the evidence that this is the right thing to spend on. This document is how.

## 6.1 The three reduction mechanisms, in increasing strength

Each one filters a table by what its join partners will accept, before the join runs. They differ in how completely they do it and what they cost.

**Min and max transfer.** The cheapest. A predicate on one side gives a range, the range is pushed to the other side as a zone map test, and whole parts are skipped without being read. Costs nothing beyond the range computation, works only for ordered domains, and removes only the parts that are entirely outside. rudb has the zone maps already, per part, exact, per `../stats/02-the-catalogue.md`.

**Bloom or bitmap transfer.** The middle. Build a filter over the qualifying keys of one side, push it into the other side's scan, test one row at a time. This is what SQL Server does next to its hash tables and what Predicate Transfer formalised, and CIDR 2026's paper is the account of how far it gets in a production engine. It reduces most dangling tuples. It has false positives, so the join still has to be run, and the residual work after a Bloom pass is what separates it from the guarantee.

**Exact bitmap over row ids.** The strong one, and rudb's. When the join edge has a stored link, the qualifying set on the parent side is a set of row ids, a set of row ids is a bitmap over a dense space, and the test on the child side is one bit lookup through the forward link with no false positives. Every dangling tuple is removed. This is a full semi-join reduction in the sense Yannakakis requires, and it costs one sequential pass per edge.

The strength ladder is also a cost ladder, and the decision of which to use per edge is a plan decision in artifact 5. But the availability of the third one is not a tuning parameter. It is the reason this project can claim an order of magnitude on join workloads rather than a factor of two.

## 6.2 Why exactness is worth this much

Robust Predicate Transfer's guarantee is that intermediate results are at most n times the output size, where n is the number of joins, and RPT needs LargestRoot and SafeSubjoin to get there because a Bloom-based transfer does not automatically produce a full reduction. Yannakakis+ gets from 488 seconds to 13.2 seconds on a five-copy SF100 TPC-H query in DuckDB by cutting to three semi-joins from ten and moving aggregation ahead of them. Parachute buys 1.24x on JOB with semi-join filtering already on, for fifteen percent extra space.

All three of those are engineering around the fact that the reduction is approximate and the semi-joins are expensive. An exact bitmap over row ids removes both problems at once. There is no false positive rate to engineer around, so the schedule does not have to be clever to be complete. And the semi-join is not a hash join, it is a bit test, so the constant factor that has kept Yannakakis out of production since 1981 does not apply.

The honest limit: this only works on an edge where a link exists, which means a declared or inferred relationship over the native format. A join between two Parquet files, or on a non-key column, or across a computed expression, gets the Bloom path and the ordinary guarantees. Document 06 does not get to claim the strong result on those, and `../graph/06-the-optimizer.md` section 6.3's floor applies: the graph path is taken only where the plan without it is still available.

## 6.3 The schedule

Four passes in the logical layer, matching `../graph/06-the-optimizer.md` section 6.1 and extending it to the non-link case.

**Annotate.** Row identity preservation, per document 05 section 5.2. Mechanical, no cost model, lives in `rudb-plan`.

**Match.** For each equi-join edge, find whether a verified relationship exists and whether its link is built. Record direction and the four exact numbers from the link header. Still no cost model. Edges with no link are recorded as Bloom-eligible if both sides are scans or filtered scans.

**Schedule.** Build the join graph, choose a transfer order, insert `Node::Reduce` nodes. This is where the literature's algorithms go. For an acyclic graph, LargestRoot picks the root and SafeSubjoin gives the order, per Robust Predicate Transfer, and the result is a forward pass toward the root and a backward pass away from it. For a cyclic graph, the transfer graph is a DAG rather than a join tree, which is Predicate Transfer's generalisation, and the guarantee weakens to "reduces a lot" rather than "reduces fully".

**Rewrite.** Turn surviving link edges into link joins and pick the build side for the rest, from facts.

The ordering is the graph document's and the argument is good: reduction before join rewriting, because a reduction changes the cardinalities the join decision is made against, and because after a reduction the join often does not need to be a link join at all.

## 6.4 Which edges are worth reducing

A reduction is not free. It is one pass over the reduced side plus the cost of building the filter, and on a query where nothing is filtered it removes nothing and costs the pass. So there is a gate, and the gate is where the facts from document 04 get spent.

The condition is that the reducing side must actually be selective. Three sources, in the order `../graph/06-the-optimizer.md` section 6.2 gives them: the certified frequency synopsis, which is exact for leading values and bounded for the rest; the zone maps, which bound a range predicate by counting surviving parts; and a default, recorded as a guess so a bad plan is attributable.

Two rules on top.

**An edge whose reducing side has no predicate on it is not reduced.** Nothing is being filtered, so nothing can be transferred. This is worth stating because it is the single most common case in a benchmark that measures joins without filters, and a scheduler that reduces every edge unconditionally regresses exactly there.

**An edge whose child is already clustered by its parent gets the reduction cheaply.** When the child is physically clustered, the forward link collapses to a monotone bit vector with rank and select, per `../graph/03-the-file-format.md`, and the reduction is a range intersection rather than a per-row test. For TPC-H `lineitem` against `orders` this is the case, and it is why the TPC-H numbers are the ones to chase first.

## 6.5 The abort rule

From `../graph/06-the-optimizer.md` section 6.3, and it is the cheapest possible adaptivity: **a reduction that has processed a third of its input and removed nothing may stop, keep the filter it has as a `Full` bitmap, and cost only the third it did.**

This works because a reduction filter is allowed to be incomplete. A filter that says "everything survives" is a correct filter. So abandoning a reduction partway costs the work done and changes no answer, which makes it the one runtime decision in this folder with no correctness argument attached to it at all.

Qiao, Boncz and Zhang's Robust Predicate Transfer with Dynamic Execution, PVLDB 19(6), February 2026, makes the whole schedule dynamic on this principle. rudb takes the abort and not the dynamic reschedule, because a reschedule mid-query means the plan depends on data arrival order, and document 12 lists exactly three things allowed to vary at runtime.

## 6.6 What is left of join ordering

Less than the literature spends on it, and the argument is Robust Predicate Transfer's own: with reduction in place, an optimizer could limit its search to left-deep plans, or pick a random order, and stay tolerant of estimation error.

rudb does not go as far as random. Order still decides two things reduction does not fix. It decides peak memory, because a bushy plan holds two build sides where a left-deep plan holds one. And it decides which side of each join is small, which the build-side choice then consumes.

So the policy is:

**Default: left-deep, greedy, ordered by reduced cardinality.** After reduction the cardinalities are much better known, often exactly known on link edges, and a greedy walk over good numbers beats an exhaustive search over bad ones. This is the whole plan for the first version and it is expected to be within a small factor of optimal on JOB and CEB once reduction is on.

**Where the budget allows and the query is small: DPhyp.** DPccp handles connected subgraph enumeration for simple graphs and DPhyp extends it to hyperedges, which is what non-inner joins and complex predicates produce. The gate is table count and a time budget, both settings. `../planner/07-join-ordering.md` remains the reference for the algorithms and this folder does not restate them.

**Never: Cascades.** `../planner/07-join-ordering.md` section 07.6 argued this and nothing since changes it. A transformation-based optimizer with a memo is the right architecture for a system that needs an extensible rule set across many storage engines. rudb has one storage engine and a closed set of operators.

The budget is a hard deadline. When it expires, the greedy order stands. That is an abandonment rather than a partial result, which is safe because the greedy order was computed first and is always available.

## 6.7 Where the runtime filter is built and applied

Independent of the reduction schedule, and the two are easy to conflate.

A reduction is a planned pass that happens before the join. A runtime filter is built as a side effect of a join that is happening anyway: the build side finishes, and the set of keys it has is pushed to the probe side's scan. SQL Server's bitmaps are this. It costs almost nothing because the build side was going to be built regardless.

Both should exist and they compose. The rule that keeps them from fighting:

**A runtime filter is emitted by every hash join build, always, unless the build side is larger than a threshold.** No cost model, no gate beyond size, because the cost is a bitmap the build already has the keys for. This is the SQL Server lesson from CIDR 2026 taken literally.

**A reduction is scheduled only where section 6.4's gate passes.** Because a reduction is a pass that would not otherwise happen.

The layer problem from `../planner/09-runtime-filters-and-adaptivity.md` section 09.4 still applies: `rudb-opt` at rank 11 cannot see `rudb-exec` at rank 12, so a runtime filter is expressed as an annotation on the physical plan saying which scan node accepts a filter from which build, and the executor wires it up. Artifact 5 is the right place for that annotation, which is one more thing the physical plan's absence has been blocking.

## 6.8 What EXPLAIN has to show

Per edge: whether a link was found, which reduction mechanism was chosen, the gate's input fact and its class, the estimated and actual reduction, and whether the abort fired.

Per join: the implementation chosen, the build side, the cardinality with its class, and whether a runtime filter was emitted and consumed.

The reason this list is long is that reduction is the part of the engine most likely to be silently doing nothing. A Bloom filter with a bad hash, a bitmap over the wrong id space, a schedule that reduced the side with no predicate on it: all three produce correct answers at the speed of not having the feature, and without per-edge output nobody finds out.

## 6.9 The floor

`../02-the-goal.md` says no query may be slower than DuckDB, ever. Applied here that is three bounded costs.

A link join's cost is bounded by the child row count, which is exact from the link header.

A reduction's cost is bounded by one pass per edge, which is exact.

A join ordering search's cost is bounded by the deadline, which is a setting.

All three are bounded before they run, and the third degrades to a greedy order that was already computed. That is what makes the floor defensible here rather than aspirational.

## What we should take from this document

Reduction is the mechanism, not search. rudb's reduction is exact where a link exists, which is stronger than Bloom-based predicate transfer and achieves what Yannakakis asks for without the semi-join cost that kept Yannakakis out of production.

The schedule is four passes, the gate spends the facts from document 04, and the abort rule is free because an incomplete filter is still a correct filter.

Join ordering gets a greedy left-deep default and DPhyp under a budget, and the literature's own conclusion is that this is enough once reduction is in place.

Runtime filters and planned reductions are different things and both exist. The filter is emitted unconditionally because it is nearly free. The reduction is scheduled only where a fact says the reducing side is selective.
