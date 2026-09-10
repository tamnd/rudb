# Layer ten: adaptivity

This is sub-milestone 2l, the last layer in the stack, and it is the only one that is not a component. Every layer below has a decision in it that is made from an estimate, and an estimate is a guess about data the engine has not read yet. This layer is about what happens when the guess is wrong.

It comes last for a reason that is easy to get backwards. Adaptivity is not a way to avoid getting the static decisions right. An engine that adapts its way out of a bad default is an engine whose worst case is the default plus the cost of noticing, and the cost of noticing is paid on every query including the ones where the default was fine. Adaptivity is worth having after the defaults are good, because then it is a narrow correction rather than a rescue.

## 12.1 The inventory

Nine decisions have been deferred to here or specified with a placeholder policy, and collecting them in one place is most of the value of this document.

Conjunct ordering in a filter, from document 04 section 4.5, which already has the measure-and-reorder mechanism because it was three lines once selection threading existed, and which deferred its exploration policy here.

Chunk compaction after a filter, from document 03 section 3.6, decided at plan time from a gain function fitted to a measured surface, revisited here when the observed selectivity differs from the estimate.

Late materialization, from document 05 section 5.7, the same shape: a plan-time decision from estimated selectivity and payload width that is wrong when the estimate is wrong.

Build side selection in a hash join, from document 08 section 8.4, where the placeholder is catalog row counts and where the operator was specified to be able to switch sides mid-build.

Global against partitioned hash tables, from document 06 section 6.7, where the threshold is on build size and the build size is estimated.

The grouping shape, from document 07 section 7.4, which is chosen at plan time and where the array shape depends on a cardinality bound.

The top-N threshold, from document 09 section 9.5, where a large k makes the heap worse than a sort.

Predicate transfer, from document 11 section 11.7, where Robust Predicate Transfer's whole contribution is deciding when the transfer costs more than it saves.

Kernel flavour selection, which has not been mentioned before and is section 12.4.

## 12.2 One mechanism

Nine ad hoc adaptive loops is nine places to get oscillation, nine places to get an unbounded overhead, and nine different answers to how a decision is reported. So there is one mechanism and the nine are its instances.

A decision has a small set of alternatives, a way to measure the cost of the alternative currently in use, and a rule for switching. The framework provides the measurement, the running statistics, the exploration schedule and the reporting. Each instance provides only the alternatives and the cost function.

The statistics are a running average over a recent window rather than over the whole query, which document 04 section 4.5 already argued for on the grounds that selectivity in a clustered column changes as the scan moves through the file, and that an average over the whole scan is an average over two different distributions. That argument generalizes to every instance here.

Exploration is periodic and bounded: every so many chunks, try an alternative and measure it. Vectorwise's micro-adaptivity work is the reference and its scheme is to explore with a probability that decays as confidence grows, so the steady-state overhead approaches zero while the ability to notice a change never quite disappears. The bound on exploration cost is a stated fraction of total work, it is enforced, and it is what stops adaptivity from being a tax.

## 12.3 The four rules

**An adaptive decision never changes an answer.** Every alternative in every decision must produce identical results, and this is tested by forcing each alternative and comparing, not by inspection. This rule is what makes the whole layer safe to enable by default, and any proposed adaptive decision that cannot satisfy it does not belong here.

**Adaptivity is bounded and it is off-switchable.** There is a setting that disables it entirely, and the benchmark harness records whether it was on. A performance claim made with adaptivity on and reproduced with it off is a claim about the engine, and one that only holds with it on is a claim about the workload.

**It converges.** A decision that flips back and forth between two alternatives on alternating chunks is worse than either, and it is the characteristic failure of this kind of system. The mechanism uses hysteresis, meaning a switch requires the alternative to be better by a margin rather than merely better, and the convergence test in section 12.6 constructs the oscillating case deliberately.

**It is visible.** `EXPLAIN ANALYZE` reports, per decision, which alternative was chosen, how many times it switched, and what the exploration cost was. An adaptive engine whose choices are invisible is an engine nobody can debug, and the report is the difference between a slow query that can be explained and one that cannot.

## 12.4 Kernel flavour selection

This is the instance that is new here rather than deferred here, and it is the one Vectorwise's micro-adaptivity paper is actually about.

For several primitives there are two or three implementations whose relative speed depends on the data rather than on the machine. The clearest is a filter, which can be written with a branch, which is fast when the predicate is predictable and slow when it is not, or branchlessly by computing the result and writing unconditionally, which is constant cost regardless. The crossover is near the selectivity where the branch predictor stops working, and it moves with the data.

Others: a gather that is a dense range can use a copy rather than an index load. A comparison against a constant string can use the prefix trick or go straight to the body depending on how often the prefix ties, which on some columns it does constantly. An integer decode can use the general bit-unpacking path or a specialized one when the bit width is a byte multiple.

None of these is a large factor on its own and together on a scan-heavy workload they are worth a real fraction. They are also the safest possible instance of the mechanism, because the alternatives are provably identical functions and the measurement is a cycle count around a loop that already exists.

## 12.5 Mid-query re-optimization, and why it is limited

The most ambitious form of adaptivity is to stop mid-query, look at the actual cardinalities produced so far, and re-plan the remainder. It is attractive because cardinality estimation errors compound and because the actual number is available for free once a pipeline has run.

It is also where this kind of system does the most damage, because re-planning discards work, because the new plan is chosen from estimates that are only better for the part already executed, and because it makes performance depend on timing in a way that makes a slow query irreproducible.

So the scope here is narrow and specific. After a pipeline that materializes finishes, meaning a join build or an aggregate, the exact cardinality of its output is known rather than estimated. If that number differs from the estimate by more than an order of magnitude, the remaining pipelines are re-planned with the exact number substituted. Nothing is discarded, because the materialized result is still valid and is the input to the new plan, and nothing is re-planned mid-pipeline.

That restriction makes the whole thing safe: it happens at most a few times per query, at points where work is already complete, using a number that is exact rather than better-estimated. It also catches the case that matters most, which is a join build that turns out to be a hundred times larger than predicted and whose successors were all planned on the assumption that it was small.

The order-of-magnitude threshold is deliberate and it is not tunable downward without evidence. Re-planning on a factor of two is churn.

## 12.6 What is not here

**Learning across queries.** Caching statistics observed from one query to improve the plan of the next is tempting and it makes benchmarks look good in a way that does not transfer, because a benchmark runs the same queries repeatedly and real workloads do not. If it is built, it is off by default and the benchmark harness records that it was off.

**Result and condition caching.** ClickHouse has a query condition cache and document 01 recorded the methodologically important detail that they disabled it when measuring lazy materialization, so that the win was attributable to the engine change. That is the right practice and rudb adopts the rule: a cache that stores query results or predicate outcomes is disabled in every published measurement, because a cache measures the benchmark's repetition and not the engine.

**Learned cost models and learned indexes.** The literature is real and some of it works. It is excluded from this directory because a model trained on a benchmark and then evaluated on that benchmark is not evidence, and building the infrastructure to evaluate it honestly is a larger project than the engine work it would improve.

**Adaptive storage layout.** Reorganizing data based on observed queries is a storage decision, it interacts with transactions and checkpointing, and it belongs to a different part of the specification.

## 12.7 The test gate

The answer-invariance property from section 12.3 is the whole correctness story and it is tested exhaustively: for every decision, every corpus query runs with every alternative forced, and the results must be identical. That is a large number of runs and it is a scheduled job rather than a per-commit one, with a per-commit subset.

Convergence gets a constructed adversary: data whose properties flip repeatedly at a period chosen to be exactly wrong for the window size, which is the input designed to make the decision oscillate. The requirement is not that the engine chooses well on it, which is impossible, but that it does not do worse than the worse of the two fixed alternatives by more than the exploration bound.

The exploration bound is asserted directly. Total exploration cost as a fraction of query time, measured, with a limit, on every benchmark query.

Determinism changes shape here and it has to be stated. With adaptivity on, execution timing affects which alternative is chosen, so two runs of the same query may execute differently. They must still produce the same answer, which is section 12.3's first rule, and the answer comparison across runs is what enforces it. `EXPLAIN ANALYZE` output is not deterministic with adaptivity on and the plan stability tests from document 11 section 11.8 therefore run with it off.

## 12.8 The benchmark gate

Adaptivity is measured by its worst case and not by its average, which is the opposite of every other layer in this directory, and the benchmark has to be built that way or it will report a flattering number.

The measurement is the gap between the adaptive engine and the best fixed choice, on inputs designed to defeat each fixed choice in turn. For each decision, construct data where alternative A wins by a lot and data where B wins by a lot, run both fixed and adaptive on both, and report the four numbers. Adaptive should be close to the winner on both, and its distance from the winner is the exploration cost, which is the number this layer is judged by.

On the standard suites the expectation is small and positive on ClickBench and TPC-H, because the static decisions from layers one through nine are already fitted to data of exactly that shape, and larger on TPC-DS and on the join order benchmark, where the estimates are worse and there is more for the corrections to correct. If ClickBench moves by a lot at 2l, the static defaults were badly chosen and that is where to look.

The regression to watch for is the per-query floor. Nine measurement loops and an exploration schedule add fixed cost, and the parent spec's second axis is explicitly about the floor. It is asserted here the same way it was in document 10 section 10.11.

## 12.9 Exit criterion for 2l

**One adaptive framework with the nine decisions from section 12.1 as its instances, each with identical-answer alternatives, hysteresis, a decaying exploration schedule and an enforced exploration cost bound, all reported per decision in `EXPLAIN ANALYZE`, all disableable by one setting, with mid-query re-optimization limited to exact cardinalities at materialization points and an order-of-magnitude threshold, with the answer-invariance property passing across the corpus for every forced alternative, with the constructed oscillation adversary bounded, with the four-number worst-case table published for every decision, and with the per-query floor unregressed.**

Named as excluded rather than deferred, with reasons in section 12.6: cross-query learning, result and condition caching, learned cost models and indexes, and adaptive storage layout.

That closes the stack. Document [13](13-measurement.md) is how all of it is measured and document [14](14-plan.md) is the sub-milestone ordering with the gates written out.
