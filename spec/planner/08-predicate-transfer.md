# Predicate transfer

The strongest idea in the last few years of query processing research, the one that changes what a bad cardinality estimate costs, and the one that most directly serves `spec/02-the-goal.md`'s second axis. It is stage 14, it runs after join ordering, and it is the reason document 06 is allowed to stay small.

## 08.1 The idea

A join order is a bet on cardinalities. Document 06 says those cardinalities are wrong by orders of magnitude in every system that has been measured, and document 07 says the search is not the weak part. The natural conclusion is that the profitable move is not a better bet, it is **a plan whose cost is much less sensitive to the bet being wrong**.

Predicate transfer does that by making every relation smaller before any join runs. Before the join phase, push approximate semi-join filters around the join graph so each base table is reduced to roughly the rows that can actually contribute to the final answer. Then join. The intermediate results are small no matter what order they are joined in, and a join order chosen on a bad estimate costs a fraction rather than a catastrophe.

The classical root is **Yannakakis, 1981**: for an acyclic join query, a bottom-up pass of semi-joins followed by a top-down pass removes every dangling tuple, and the resulting join runs in time linear in input plus output. The reason it was never adopted in practice is that the semi-join passes themselves are expensive, you pay real joins to avoid real joins.

**Predicate Transfer**. Yang, Mo, Chandramouli, Pavlo et al., CIDR 2024, arXiv:2307.15255, is the modern form: run Yannakakis's two passes with **approximate** filters, specifically Bloom filters, instead of exact semi-joins. False positives are harmless because the real join afterwards removes them. The passes become cheap enough to be worth it, and the reported speedups on join-heavy workloads are large.

## 08.2 Robust Predicate Transfer

**RPT**. arXiv:2502.15181, 2025, is the version to actually implement, and it fixes the two things that make the CIDR version hard to ship.

**It handles cyclic queries.** The CIDR formulation is stated for acyclic ones. RPT extends the guarantee to a broader class via **γ-acyclicity**, which is what makes it applicable to real schemas rather than only to the ones that happen to be trees.

**It gives a transfer order that is deterministic and defensible.** Two named pieces:

- **LargestRoot**, which picks the relation to root the transfer at. The choice matters because the transfer is directional and rooting it badly wastes a pass.
- **SafeSubjoin**, which decides which reductions are *safe* to apply, that is, which ones cannot remove a row that the final answer needs. This is the correctness core of the whole document and it is where an implementation will get it wrong if it treats every join edge as transferable. A `Single` join's right side is not transferable: reducing it does not reduce the output, because the output is one row per left row regardless. An outer join's null-producing side is not transferable for the same reason document 04's table forbids pushing a filter into it. SafeSubjoin is the general statement of the rule those two cases are instances of.

The reported result is about **1.5x geomean** improvement, and the number that matters more for this project, **RPT retains only 0.29% dangling tuples**, meaning the approximate passes get within a third of a percent of what exact semi-joins would have achieved. That is the empirical justification for using Bloom filters instead of exact semi-joins: the approximation is nearly free of cost in reduction power.

## 08.3 Parachute, and what the numbers actually are

**Parachute**. arXiv:2506.13670, 2025, is the most useful paper in this area to read closely, because it publishes the baseline as well as its own result and the baseline is the thing rudb builds first.

Its reported figures on JOB: **1.54x** and **1.24x** on two configurations, at about **15% extra space** for precomputed structures. Its Bloom-filter predicate-transfer baseline is **1.26x**. So the honest reading, and the one that decides what to schedule:

**The plain blocked-Bloom predicate transfer is most of the win.** 1.26x of a 1.54x, for no extra storage, no precomputation, and no new on-disk structure. The remaining 0.28x costs 15% of the data size and a build step.

So: build the Bloom version. Read the Parachute version and do not build it until there is a workload asking for it.

Parachute also publishes the filter-sizing detail worth copying rather than re-deriving: an **8 KiB filter fits in L1**, and at **m = 2^16 bits with k = 2 hash functions** it holds roughly **5000 keys at about a 2% false positive rate**. Those are the constants to start from. `spec/09-optimizer.md` and document 09 both want blocked Bloom filters, and blocked means one cache line per probe, which is what makes the filter cost a cache hit rather than k random accesses.

## 08.4 The 2026 position

Three more results, because this area moved fast and the framing has shifted from "a trick" to "the default".

**I Can't Believe It's Not Yannakakis!**. CIDR 2026. The title is the argument: the community has spent forty years explaining why Yannakakis is impractical and the modern re-derivations keep arriving at it.

**Yannakakis+**. PACMMOD 3(3), 2025. A practical rewriting-based framework, reported as competitive with or better than DuckDB and Spark on graph and relational workloads.

**Bekkers, Neumann and Kemper**. PVLDB 18(8), 2025, on making semi-join reductions practical inside a real system, which is the engineering side of the same question.

The reason this cluster matters for rudb beyond the speedups: it is the concrete mechanism behind what the *Still Asking* retrospective (PVLDB 18(12), 2025) names as the open problem. Robustness is not obtained by estimating better. It is obtained by making the plan care less.

## 08.5 What it costs, and when not to do it

Predicate transfer is not free. It is extra passes over the base tables, filter construction, and filter probing, all before the first join emits a row. On a query where the joins were never going to be expensive, that is pure overhead, and the second axis of the parent spec says pure overhead on any query is a failure regardless of what it buys elsewhere.

So there is an escape hatch and it is the first thing to build, not the last:

- **Below two equality join edges, do not transfer at all.** A single join does not have a graph to transfer around.
- **Below a size threshold on the base tables, do not transfer.** Building a filter over ten thousand rows to save probing ten thousand rows is a loss.
- **When the estimated reduction is small, do not transfer.** This is the one place in the folder where a bad estimate causes overhead rather than a bad plan, and the mitigation is document 09's: measure the first filter's actual selectivity at runtime and abandon the transfer if it is not reducing anything.

**ClickBench gets nothing from this document.** ClickBench is 43 single-table queries over `hits`; there is no join graph. Predicate transfer is worth zero there and must therefore cost zero there, which the two-edge minimum guarantees by construction. Say this out loud because axis 1 of the project is a ClickBench number and this document does not move it, its workloads are TPC-H, TPC-DS and JOB, all of which are in `rudb-bench`.

## 08.6 How it is expressed, given the layer rule

`cargo xtask layers` puts `rudb-opt` at rank 11 and `rudb-exec` at rank 12, so the optimizer cannot name an executor type. Predicate transfer is a runtime mechanism decided at plan time, which makes this the document where that constraint bites hardest.

The answer is the same one document 09 uses: **the plan carries an annotation and the executor interprets it.** The optimizer emits, into the plan, a transfer schedule, which relation builds a filter on which key, which relations probe it, and in what order, as plan data. `rudb-exec` reads it and does the work. The optimizer never names a filter implementation, a hash function or a bit width; it names a column, a direction and a position in the schedule.

This has a pleasant side effect that is worth the constraint: the transfer schedule is part of the plan's textual form, so it round-trips through the printer and parser like everything else, and a transfer schedule is testable as text in and text out with no executor involved.

Two further implementation notes:

- **Reuse the hashes.** The join already hashes its keys to 64 bits. A Bloom filter built from a different hash of the same column is a second hash computation for no benefit. Take slices of the existing 64-bit hash for the k probes, which is what document 09 also assumes.
- **Build it with document 04's equality classes.** The transfer graph is the join graph with equality-class edges, which is the same structure section 04.4's transitive predicates already builds. Transitive predicates are the exact, free, plan-time case of exactly this idea; predicate transfer is the approximate, runtime, whole-graph case. They should share the code that computes the classes and they should be built in that order, exact first, approximate second.

## What we should take from this document

Predicate transfer reduces every base table before any join runs, which makes the cost of a plan much less sensitive to a wrong cardinality estimate. That is the robustness answer, and it is why document 06's estimator is allowed to be small.

Implement RPT rather than the CIDR formulation: LargestRoot for the transfer root, SafeSubjoin for which reductions are safe, γ-acyclicity for cyclic queries. SafeSubjoin is the correctness core and `Single` joins and outer-join null-producing sides are its obvious instances.

Blocked Bloom filters are most of the win, 1.26x of Parachute's 1.54x, at zero extra storage, so build that and stop. Start from m = 2^16, k = 2, 8 KiB to stay in L1, about 5000 keys at 2% false positive.

Two equality join edges minimum, a base-table size floor, and a runtime bail-out when the first filter does not reduce. ClickBench must pay exactly nothing for this document.

The schedule is a plan annotation because `rudb-opt` cannot see `rudb-exec`, which also makes it testable as text.

Build it with document 04's equality classes: the exact plan-time case first, the approximate runtime case second, sharing one graph.
