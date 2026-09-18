# The physical plan

The missing artifact. This is the document that, if only one thing from this folder gets built, should be the thing.

## 7.1 What it is for

A physical plan is the logical plan with every choice made and written down. One node per logical node, or occasionally several, each naming the implementation, the inputs it wants in which representation, the memory it reserves, the parallelism it accepts, and the fact that drove each of those.

The reason to have one is not that it produces better plans. It is that it produces *attributable* plans. Today rudb makes these choices, in `if` statements, inside operators, with no name and no output. A physical plan is the same choices in a value that prints. Everything else in this document follows from turning an implicit choice into an explicit field.

`crates/rudb-exec/src/build.rs` already anticipated this in its own module documentation: "The physical planner that section 9.6 describes goes here, and the reason this is a separate module from the operators is so that it can grow into one without any of them moving." The scaffolding is correct. What is missing is the value in the middle.

## 7.2 What it decides

Twelve decisions. Every one of them is currently either an `if` inside `rudb-exec`, a hardcoded constant, or absent.

**Scan implementation and access path.** Full scan, zone-skipped scan, link-driven gather, or answered from metadata without reading anything. Which parts survive the zone maps. Whether the projection is pushed into the decoder.

**Filter placement and order.** Which conjuncts run at the scan, which run after the decode, which run after a join. Conjunct order within a filter, which is a real decision because a cheap conjunct that removes half should run before an expensive one that removes a tenth. `crates/rudb-opt/src/filter.rs` at 1,374 lines decides where a filter goes in the tree; nothing decides the order of conjuncts inside one.

**Join implementation.** Hash lookup, hash gather, link join, nested loop, mark join. `crates/rudb-exec/src/join.rs`'s `streamed` function currently picks between a streaming probe and a buffered join from the join kind alone, which is a semantic constraint rather than a choice, and there is no cost based choice between implementations anywhere.

**Build side.** Currently `sides::BuildSideProbeSide` in the optimizer, which is a physical decision in a logical pass. It moves here.

**Grouping strategy.** Answered from a synopsis, grouped on codes, shared concurrent table, partitioned table, or pre-aggregated then merged. Document 02 section 2.6 is the evidence that this is a genuine choice with more than one defensible answer, and document 11 is the detail. Today there is one strategy plus two spin-off files for special cases.

**Aggregate state layout.** Row-wise in a slot table or column-wise in parallel vectors. `group.rs` already chose column-wise, correctly, and there is no reason to revisit it, but it should be a printed field rather than an unstated property.

**Sort strategy.** In-memory, external, partial for a top-n, or eliminated entirely because the input is already ordered. The last one needs the sortedness fact and is an enabling decision under document 04 section 4.3.

**Materialization points.** Where a pipeline breaks, what the buffer holds, and whether it holds rows or positions. Late materialization, currently `late::LateMaterialization` in the optimizer, is this.

**Layout requirements.** Section 7.5, and the most valuable item on the list.

**Memory reservation.** How much each stateful operator asks for up front, from the value width and distinct count facts. Today nothing reserves in advance.

**Spill thresholds.** The point at which an operator changes behaviour. Not the spill decision itself, which document 09 section 9.5 keeps at runtime.

**Parallel degree and morsel size.** From the parts count rather than from a constant. `Source::morsels(threads)` already exists in `rudb-pipeline` and already has the right shape; what is missing is a planner that fills it from a fact.

## 7.3 What it looks like

A tree, same shape as the logical plan, with a different node type. It prints and it parses, per document 03 section 3.1.

```
PhysAggregate  strategy=concurrent  keys=[codes(hits.UserID)]  reserve=48MB  degree=16
                 fact: distinct=2,914,881 (Exact, Sketch)  heavy_hitters=none (Certified, FrequencySynopsis)
  PhysFilter   order=[c1,c0]  at=scan
                 fact: c1 keeps 0.003 (Certified, Quantiles), c0 keeps 0.2 (Estimated, Default)
    PhysScan   table=hits  parts=1,431 of 100,000  columns=[UserID:codes, EventDate:raw]
                 fact: rows=99,997,497 (Exact, RowCount)
```

Two properties of that sketch matter more than the syntax.

Every decision is a named field with a value, so a test can assert `strategy=concurrent` without running the query.

Every decision is followed by the fact that drove it and the fact's class. When a plan is bad, the question is always whether the planner reasoned badly from a good number or reasoned well from a bad one, and those are different bugs with different owners. Printing the class answers it on sight.

## 7.4 The cost model lives here and nowhere else

Document 05 section 5.8 says a logical node must not carry a cost, because cost depends on the implementation. This is where implementations are chosen, so this is where cost belongs.

The model stays small. `../planner/06-cardinality-and-cost.md` section 06.4 is the reference and the shape is unchanged: a weighted sum of rows scanned, rows probed, bytes materialized, random accesses and hash entries built, with the weights fitted once by measurement and pinned.

Two rules keep it from becoming a research project.

**The model only ever compares two plans for the same logical node.** It never produces a predicted runtime and nothing depends on its absolute value. That kills the entire class of failure where a cost model is miscalibrated against a different machine, because a miscalibration that scales both sides equally changes no decision.

**Any comparison whose two sides are within a factor the model cannot resolve takes the documented default.** Not the one that scored higher by a hair. A tie-break on noise is how a plan changes when a table grows by three rows, and plan stability, per document 13, is worth more than the hair.

## 7.5 Layout requirement propagation

This is the pass with no equivalent in a shipping engine and it is where document 02 section 2.8's order of magnitude lives.

The Bespoke OLAP ablation: code specialization on a fixed struct-of-arrays layout was worth 1.26x on TPC-H and 0.57x on CEB, meaning it lost. Layout specialization took the same system to 12.35x and 51.40x. The representation matters and the code does not, by roughly an order of magnitude.

rudb cannot synthesise a new layout per workload. What it can do is stop destroying the layout it already has. A dictionary encoded column arrives as codes plus a dictionary; today, somewhere on the way to a `GROUP BY`, it becomes strings, and the group by hashes strings. If the group by asked for codes, it would hash a `u16`, the dictionary would never be touched, and the whole decode would not happen. `../07-execution.md` calls this encoded execution and commits to it. What is missing is the mechanism that decides it, which is a negotiation and needs two passes over the physical tree.

**Requirements flow down.** Each physical operator states, per input column, what representation it would prefer and what it will accept. A hash aggregate on a single string key prefers codes, accepts strings. A sum over an integer prefers a frame-of-reference base plus deltas, accepts raw values. A `LIKE` prefers the FSST-compressed form so it can match on compressed bytes, accepts raw. A comparison against a constant prefers codes with the constant translated into code space.

**Capabilities flow up.** Each scan states, per column, what it can produce without extra work: raw, codes plus a dictionary handle, run-length runs, frame-of-reference deltas, FSST symbols.

**They meet at the scan.** If the requirement is in the capability set, it is satisfied and the decode does not happen. If not, a decode node is inserted and printed, so that a query which pays for a decode says so.

Three rules make this tractable rather than combinatorial.

**The representation set is closed and small.** Whatever the writer can produce, enumerated in `rudb-encoding`, and no more. A negotiation over an open set is a research problem; over six alternatives it is a table lookup.

**A requirement that cannot be met is never an error.** It is a decode, which is what happens today unconditionally. So the worst case of this entire mechanism is current behaviour, which is what makes it safe to build incrementally.

**Requirements do not cross a materialization boundary.** When a pipeline breaks, the buffer holds whatever the sink produced, and the next pipeline negotiates against the buffer rather than back through to the original scan. Otherwise the propagation is global and the reasoning is unbounded.

The payoff on ClickBench is concentrated and worth naming: the queries that group by `UserID`, `SearchPhrase`, `URL` or `Referer` and count are the ones where the whole query becomes hashing a small integer, and those are most of the suite.

## 7.6 Parallelism is decided here

`rudb-pipeline` already has the right shape. `Pipeline::degree` reads the pool ceiling, whether every operator will run as more than one instance, and how many morsels the source says it has. `Source::morsels(threads)` lets a source that can choose how finely to cut its work be told how many threads are available.

What is missing is the input. Today a source answers `morsels` from what it happens to know. With facts, the physical planner sets it: rows per part is exact, the part count is exact, the estimated output cardinality is known, and the degree follows from those rather than from a constant.

Two specific decisions this unblocks.

**A small query stays on one thread.** Already the intent, per the `Source::morsels` documentation, which says a scheduler that spins up thirty two instances to read one chunk has spent more on starting them than the query was going to cost. With facts the threshold is a row count rather than a morsel count.

**A skewed query gets finer morsels.** When the frequency synopsis says one group holds forty percent of the rows, equal-sized morsels give one thread forty percent of the work. Finer morsels near the heavy value cost more scheduling and buy better balance, and the synopsis is the fact that says when.

## 7.7 Memory is reserved here

Nothing in rudb reserves memory ahead of an operator today. `Memory`, `Reservation` and `Spent` exist in `rudb-common` and `group.rs` imports them, so the machinery is there; what is absent is a number to reserve.

The number comes from facts: distinct count times the width of the key plus the width of the state, for a hash aggregate. Build side rows times key width plus payload width, for a hash join. Rows times row width, for a sort.

Reserving is worth doing for three reasons that have nothing to do with avoiding an out-of-memory error. A table sized once does not rehash, and rehashing a large aggregate is a copy of the whole thing. Two queries running concurrently can be told there is not room for both before either has started, rather than after both have half filled. And a reservation that cannot be met is the signal to pick the partitioned grouping strategy instead of the concurrent one, which is a plan decision made from a resource fact.

## 7.8 What this does not decide

**It does not decide when to spill.** Document 09 section 9.5, following Saving Private Hash Join: the decision to spill belongs to execution because it depends on data the planner has not read, and the switch must be gradual rather than a cliff. The planner sets the threshold and reserves the memory. The operator decides the moment.

**It does not decide which kernel runs on a given chunk.** Document 10 section 10.4. A chunk's density, its code width and its cache footprint are properties of that chunk.

**It does not decide the reduction schedule.** Document 06. That is logical, because inserting a reduction changes cardinalities and the estimator must see it.

## 7.9 The crate, and what moves into it

`rudb-phys`, new, between `rudb-opt` at 11 and `rudb-exec` at 12.

It may not depend on `rudb-exec`. That constraint is the interesting one, because the physical planner needs to know what implementations exist and what they can do, and it cannot look at them. The answer is `rudb-seam`, which already exists at rank 2 and already requires every implementation to register itself with a name, a reference marker and a policy. A physical planner that reads the seam registry knows the name and the declared properties of every implementation without seeing one, and an implementation that is not registered is one the planner cannot choose, which is the correct failure.

Two existing modules move in: `crates/rudb-opt/src/sides.rs` at 245 lines and `crates/rudb-opt/src/late.rs` at 616 lines. Neither move is urgent and document 14 does not schedule them first, because both work where they are and moving working code is the cheapest thing on the list to defer.

## What we should take from this document

The physical plan is the artifact whose absence is causing the churn measured in document 01, and building it converts twelve unnamed `if` statements into twelve printed fields.

Every field is followed by the fact that drove it and the fact's class, because the question about a bad plan is always whether the reasoning or the number was wrong.

Layout requirement propagation is the highest value item in it, because the Bespoke OLAP ablation says representation is worth an order of magnitude and code is worth a quarter, and because rudb already writes the representations and currently throws them away on the path to the group by.

The cost model lives here, stays small, only ever compares two plans for one node, never produces an absolute runtime, and takes the documented default on a near tie.
