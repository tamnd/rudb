# Runtime filters and adaptivity

Everything in documents 04 through 08 happens before a single row is read. This document is about the information that only exists once the query is running, and about how a planner that is forbidden from seeing the executor gets to use it anyway.

It is the cheapest large win in the folder after pushdown, and it is the one that most directly addresses the thing document 06 admits it cannot fix.

## 09.1 Why it is separate from predicate transfer

Document 08 is a whole-graph, plan-time-scheduled reduction phase that runs before the joins. This document is a single join handing its probe side a filter derived from its own build side, decided at plan time and computed at run time. They share the filter machinery and they are not the same mechanism.

The difference that matters operationally: **sideways information passing is per-join and always applicable**, including on a two-table query where document 08's escape hatch declines to do anything. It is the first one to build, it is a few hundred lines, and it is worth a large factor on the star-schema shape that TPC-H and TPC-DS are made of.

## 09.2 The three filter kinds

A hash join builds its hash table from the build side and then probes with the other side. By the time the build is complete, the join knows things about the key column that no statistic could have told the planner: the exact minimum and maximum, the exact distinct values if there are few of them, and the exact set membership. Hand that to the scan below the probe side and most of the probe side never gets read.

**Min and max, always.** The build side's key range is two values. The probe-side scan compares them against its per-block zone maps and skips whole blocks. This costs two comparisons per block and nothing else, it needs no memory, and on a sorted or clustered column it eliminates most of the table. `spec/05-storage.md` section 5.3 already keeps per-block min and max for exactly this kind of check, so the consumer exists. **This is always on and there is no threshold on it.**

**An `IN` list, when the build side has few distinct keys.** Below a threshold, hand the scan the actual set of values. It is exact, which means no false positives, and the scan can evaluate it directly. `spec/09-optimizer.md` names a threshold around 64 distinct values and DuckDB's own threshold is in the same range, my recollection is 50, and the right thing to do is read it out of the DuckDB source at the vendored commit rather than trust that number. The exact value does not matter much; having the tier does.

**A blocked Bloom filter, above that.** For large build sides. Blocked means the k probes land in one cache line, so a probe costs one cache miss rather than k. Size it from the build side's actual count, which is known exactly by the time the filter is built. Document 08 section 08.3 gives the starting constants: m = 2^16, k = 2, 8 KiB to stay in L1, about 5000 keys at roughly 2% false positive.

**Reuse the hashes.** The join has already hashed the build keys to 64 bits to put them in its table. Take slices of that for the filter's k probes. A separate hash function over the same column is work that buys nothing.

## 09.3 Where the filter is applied

Two cases and they are genuinely different.

**Adjacent.** When the probe side's scan is directly below the join with nothing in between that changes row identity, the filter goes into the scan, where it prunes blocks and stops rows from being materialized at all. This is the case worth almost all of the value, because the win is I/O and decompression that never happens.

**Not adjacent.** When there are operators in between, the filter is applied as a mask at the earliest point where its column is available. This is worth much less, the rows have already been read, but it is still cheaper than probing the hash table.

The planner decides which case applies and says so in the annotation, because the planner is the thing that knows the plan shape. It does not decide how the filter is represented, because that is `rudb-exec`'s business.

## 09.4 The layer constraint, again

`rudb-opt` is rank 11 and `rudb-exec` is rank 12, so the optimizer cannot construct a filter object or name one. Every runtime decision in this document is therefore expressed as **plan data that the executor interprets**:

- which join builds a filter, on which key column
- which scan or operator consumes it
- whether it is applied in the scan or as a mask
- which tier is permitted, as a policy rather than as a choice, "min/max always, exact set below the threshold, approximate above" is a rule the executor applies with the counts it has, not a decision the planner makes with counts it does not have

That last point is the important one and it is a better design than it would have been without the layer rule. The planner does not know the build side's distinct count; it has an estimate, and document 06 says the estimate is wrong. The executor knows it exactly, at the moment it matters. So the planner's annotation says *build a filter here and use it there*, and the tier is chosen at build time from the real count. Getting the decision to where the information is, is the whole content of this document.

## 09.5 Adaptivity, and how little of it to build

The full adaptive-execution literature, re-optimization mid-query, plan switching, eddies, is a large research area and rudb should build almost none of it. The reason is the per-query floor: a system that can change its mind mid-query is a system whose worst case is hard to reason about, and unpredictability is the axis the project cannot spend.

Three mechanisms are worth it, all of them local, all of them bounded, none of them re-planning:

**Bail out of a filter that is not filtering.** Measure the first few thousand probes. If the filter is rejecting almost nothing, stop applying it and stop paying for it. The cost is a counter and a branch that predicts perfectly; the benefit is that the estimate-driven decision to build a filter has a floor on how wrong it can be. This is also document 08's bail-out and it should be one mechanism.

**Switch the build side when the estimate was backwards.** The planner picks which side of a hash join builds, on an estimate. If the build side turns out to be much larger than the probe side, and the join has not yet emitted a row, swapping is correct and cheap. Bounded, local, no re-planning. Document 10 section 10.2 has the plan-time half of this decision.

**Detect skew in the build key.** A hash join whose build keys are dominated by a handful of values degenerates. Counting the top few during the build is cheap and lets the operator handle the heavy hitters separately. This is an operator-level concern rather than a planner one; it is here because the planner's cost model assumes a distribution it should be honest about not knowing.

**What not to build:** mid-query re-planning, a feedback loop that writes observed cardinalities back into a catalog for the next query, or anything that makes the same query run differently the second time. All three make the engine's behaviour depend on history, and `spec/02-the-goal.md`'s floor is a promise about every run, not about the average one. A learned-from-history optimizer is also a benchmark-scoring device, the second run of a benchmark query is not what a user experiences.

## 09.6 What it is worth

`spec/09-optimizer.md` states the case plainly: a Bloom filter that eliminates 90% of the probe side before it is read is worth more than any join order decision, because it removes I/O and decompression rather than reorganizing comparisons.

That framing is the right one for this project specifically. rudb's differentiating claim is 2x less resource, and this is a mechanism whose entire value is measured in bytes not read. A join order improvement makes the CPU do less work on data it has already paid to load. A runtime filter means the data was never loaded.

It also interacts well with the storage layer in a way the design should not lose: the min/max tier prunes at the block level through zone maps, so its win is whole blocks skipped, not rows filtered. The value scales with how clustered the column is, which is a property the encoding chooser already measures.

**The prediction to record.** On TPC-H SF10, min/max filters alone, the tier with no memory cost and no threshold, should be visible on the queries with a selective dimension filter joined to a large fact table, which is most of them. If they are not, the reason is that the scan is not consulting zone maps, and that is a scan bug found cheaply.

## What we should take from this document

Three tiers: min/max always with no threshold, an exact `IN` list below roughly 50 to 64 distinct build keys with the real number read from DuckDB's source at the vendored commit, and a blocked Bloom filter above, all built from the 64-bit hashes the join already computed.

Applied inside the scan when the scan is adjacent to the probe side, which is where nearly all the value is, and as a mask otherwise.

The planner emits an annotation naming the producer, the consumer and the application point; the executor picks the tier from the exact counts it has and the planner does not. The layer rule forced that split and the split is correct on its own merits.

Adaptivity is three local mechanisms, bail out of a useless filter, swap a backwards build side before the first row, handle skew in the build, and nothing that re-plans or remembers.

This is the mechanism whose value is bytes never read, which is the axis the project is differentiating on.
