# Physical planning

Stage 15, the last one, and the only stage that produces something other than a logical plan. It is also where this folder's most distinctive pass lives, one with no equivalent in any shipping engine, and where the honest answer to "when do we build this" is mostly "later than you think, except for two things."

## 10.1 What a physical plan is for

One logical `Aggregate` becomes one of several physical shapes: a hash aggregate, a streaming aggregate over already-sorted input, a two-phase partial-then-final aggregate, or a distinct rewritten as a group-by. One logical `Join` becomes a hash join, a merge join on sorted inputs, or a nested loop when there is no equality condition. One logical `Sort` becomes a full sort or a top-N.

Document 02 argues the physical plan is a separate representation rather than annotations on the logical one, for a specific reason: a node that means two things at once is a node every consumer has to interrogate. That argument stands. What it does not settle is *when*, and the answer today is that `crates/rudb-exec/src/build.rs` lowers the optimized logical plan directly with one arm per node, there is exactly one implementation of each operator, and a selection pass that chooses between one option is a pass that does nothing.

**So the physical plan is built when the second implementation of some operator arrives, and not before.** Building the representation first, so that it is ready, is how a layer gets built speculatively and then rewritten when the operators it was designed around turn out differently. `spec/engine/11-optimizer.md` puts the optimizer at layer nine for the same reason on the cost-model side.

Two things do not wait, and they are sections 10.2 and 10.3.

## 10.2 The build side, which is one boolean and ships now

A hash join builds a hash table from one side and probes with the other. Which side builds is the single most consequential physical decision in the engine, because building from the large side and probing with the small one is a memory blow-up and a cache disaster, and the current executor takes whatever the binder emitted.

Document 01 makes the argument and it is repeated here because this is where it belongs: **this is one boolean, it belongs in the logical plan as a flag, and waiting for a physical layer in order to place one boolean is exactly the speculative-layer failure above.** `Node::Join` grows a field saying which side builds, the optimizer sets it from the cardinality estimate, `build.rs` honours it, and no representation work is needed.

**This has shipped, and two things about it came out differently from the paragraph above.** The field is `BuildSide`, a two-variant enum rather than a `bool`, because `build == BuildSide::Left` reads and `build == true` does not, and because the printed plan says `build=left` rather than `build=1`. And the flag means *which side is gathered whole before the other one starts*, which is where the hash table will go and is also where today's nested loop keeps its rescanned side. That wording is deliberate: it is the fact both operators agree on. The policy of which side to prefer is not, and the two operators want opposite answers. A hash join builds from the smaller side because the table has to fit. The nested loop in `crates/rudb-exec/src/join.rs` evaluates the conditions once per chunk of the gathered side per row of the other one, so its calls into the evaluator are the driving side's rows times the gathered side's chunks, and it wants the *larger* side gathered. Measured on server3 at a 100,000 to 4 ratio that is 703ms against 3,566ms, a factor of five in the direction the hash-join rule would have got backwards. So `rudb_opt`'s `sides` pass holds the policy, one function of two estimates, and it inverts when #62 lands rather than the flag's meaning changing under every plan in the repository.

The pass is named `build_side_probe_side`, which is DuckDB's name for it, so a corpus file that turns that optimizer off now turns something off. It only ever swaps a join whose kind has a mirror, since running the inputs the other way round means running the mirrored kind: `LEFT` becomes `RIGHT` and back, `INNER` and `FULL` are their own mirror, and `SEMI`, `ANTI`, `SINGLE`, `MARK` and `POSITIONAL` have none, because their left input is the subject rather than a side.

It is also, deliberately, a decision the runtime can override. Document 09 section 09.5 says: if the build side turns out to be much larger than the probe side and no row has been emitted yet, swap. The planner's flag is a starting configuration, which is precisely how `spec/09-optimizer.md` section 9.7 says to read every physical decision.

**The other local decisions that have to cost themselves.** Document 03 notes that rejecting Cascades means a handful of context-dependent choices have no global search to settle them and must each do a local cost comparison instead. They are all here, and they are all of the same shape, two candidate shapes, one cost model, pick the cheaper:

- hash aggregate against streaming aggregate, which turns on whether the input is already sorted on the group keys
- sort against top-N, which turns on whether there is a limit and how small it is
- hash join against merge join, which turns on whether both inputs are already sorted on the key
- distinct against group-by, which is usually the same operator and is a naming question more than a cost one
- whether to materialize a common subplan or recompute it, which is document 03's pass 8 and is the one genuinely hard comparison in the list, because the cost of materializing depends on the size of a thing that has not been computed
- whether an `IN` list becomes a filter or a join against a materialized side, which document 04 section 04.1 defers to here

Each is ten lines of comparison against document 06's cost model. Written out as a list so that none of them is discovered later as a gap in the framework.

## 10.3 Physical layout adaptation

This is the pass `spec/09-optimizer.md` section 9.6 describes, it has no equivalent in any shipping engine, and it is the second-largest open question in the whole project.

**The result it responds to.** Bespoke OLAP generates a database specialized to one known workload ahead of time and reports 11.17x on TPC-H and 45.33x on CEB against DuckDB. The ablation is the interesting part: **storage layout specialization accounts for 12.35x and code specialization for 1.26x**, and their flat-storage variant, which specializes the code but not the layout, scores **0.57x on CEB, losing to DuckDB outright**.

Read that ablation carefully, because it inverts the intuition most engine work runs on. The compiled-query line of research has spent fifteen years on the 1.26x. The 12.35x is in how the bytes are arranged for the query that will read them.

**Why rudb cannot do what they did.** They compile for a workload known in advance. A general-purpose database does not know the workload. Their number is an upper bound on what layout specialization is worth, obtained under an assumption rudb does not get to make.

**What rudb does instead.** Choose, per scan and per column, which physical representation the scan *produces*, based on what its consumers do with the values. A dictionary-encoded string column can come out as codes, as decoded strings, or as codes plus a dictionary reference, and which one is right is entirely a property of the consumer:

| consumer | wants |
|---|---|
| `GROUP BY` on the column | codes |
| `LIKE` predicate | FSST-compressed bytes, and the needle compressed too |
| equality against a constant | a single code, resolved once against the dictionary |
| projection to the output | decoded strings, but only for surviving rows, so codes until the very end |
| join against a column sharing the dictionary | codes |
| join against a column with a different dictionary | a translation table built once, not a decode per row |

**The pass is a requirement propagation.** Walk up from the consumers to the scan collecting the representation each one wants, resolve conflicts at the scan by cost, and annotate. Where two consumers of the same scan want different forms, produce the cheaper one and insert a conversion node for the other. Structurally this is the same shape as document 04's projection pushdown, a top-down requirement walk and a bottom-up rewrite, which is a reason to write it in the same style and after that pass exists.

**And the runtime overrides it**, per document 09, because the choice depends on a selectivity estimate and document 06 says estimates are wrong. The equality-against-a-constant case is the clean example: if the predicate turns out to be far less selective than estimated, producing codes and decoding survivors stops being the cheaper plan.

**The open question, stated as a question.** Does runtime layout adaptation capture a useful fraction of the 12.35x offline number? `spec/09-optimizer.md` says the honest prior is *some and not all*, because a workload-specialized database can reorder and co-locate the data on disk, which rudb cannot do at query time. `spec/19` records it as open question two, and the spec commits to amending axis 2's target if the answer at M3 is less than 2x. That commitment is the right shape and this folder should not soften it: the pass is a bet with a stated payoff and a stated date to check it.

## 10.4 Parallelism, briefly

Pipeline construction, morsel sizing and exchange placement are physical-planning outputs and they belong to the scheduler documents rather than to this folder. Two things are worth recording here because they constrain passes above:

**Bushy join trees are more parallel than left-deep ones**, which is a second reason document 07 lets the enumerator produce them rather than restricting the search space.

**Vector size is 1024, not DuckDB's 2048**, because 1024 is the FastLanes unit. That does not change any pass in this folder; it changes the constants in document 06's cost model, and the constants come from `rudb-bench` measurements rather than from arithmetic anyway.

## 10.5 The order to build this in

1. **The build-side flag.** Now. One field, one estimate, one `build.rs` change.
2. **The local cost comparisons**, as their second operator implementations arrive. Each is ten lines and none needs a representation.
3. **The physical plan as a distinct representation**, when the first operator has a third implementation and the one-arm-per-node lowering stops being honest.
4. **Layout adaptation**, after the scan layer and the encoding layer are real, because the pass annotates a scan that must be able to honour the annotation. Until then it is a pass with no consumer.

## What we should take from this document

The physical plan is a separate representation and it is built when the second implementation of an operator arrives, not in advance.

The build-side choice is one boolean, it goes in the logical plan now, and the runtime may override it before the first row is emitted.

Six local cost comparisons replace what a Cascades memo would have searched, they are listed here so none is discovered later as a gap, and the only hard one is materialize-or-recompute.

Layout adaptation is the distinctive pass: propagate each consumer's wanted representation up to the scan, resolve by cost, insert conversions, and let the runtime override. It is the same walk shape as projection pushdown.

The Bespoke OLAP ablation is 12.35x for layout and 1.26x for code, with the code-only variant losing to DuckDB. That is the number this pass is chasing, rudb cannot reach it because it does not know the workload in advance, and the project has already committed to amending its target at M3 if the answer is under 2x.
