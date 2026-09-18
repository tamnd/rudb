# 4. In memory

## 4.1 The shape of the answer

One type, returned everywhere:

```
Known { value, class: Exact | Certified { bound } | Estimated { source } }
Unknown
```

`Unknown` is a first-class answer and it is not zero, not one, and not a default. The caller decides what to do without a number, which is a decision the caller can make well and the statistics layer cannot. A subsystem that returns a made-up number instead of `Unknown` produces plans that are confidently wrong, and the whole point of section 2.1's classes is to make "we do not know" a thing the plan can say out loud.

`Estimated` carries its source, sketch, quantile, sample, default constant, or an observation from document 06, because `EXPLAIN` prints it and because a bad plan is diagnosed by asking which number was wrong and where it came from.

## 4.2 Nothing is read at open

Opening a table reads the header and the directory. It does not read statistics. An embedded database is opened by a process that may be about to run one trivial query, and a hundred milliseconds of eager statistics loading is a hundred milliseconds nobody asked for. `../02-the-goal.md`'s per-query floor is about every query, including the cheap ones.

Loading is therefore on first use and at the granularity of a section, which is what the extent layout of document 03 section 3.2 is for: a column summary is one small `pread`, and reading it does not bring in that column's sketches, quantiles or sample.

## 4.3 The rule that keeps plans reproducible

**The plan is a function of the data, the generation and the settings. It is never a function of what happened to be in cache.**

This is the constraint that shapes the whole module and it is easy to get wrong in a way nobody notices for months. The tempting design is "use a statistic if it is resident, otherwise use the default and prefetch it for next time", and it is poison: the same query on the same data produces two different plans depending on what ran before it, `plans.rs`'s committed plan baselines start flapping, a bisect stops being possible, and a benchmark's second run is faster than its first for a reason that has nothing to do with the engine being good.

So: **if a statistic exists in the file, the planner consults it, reading it synchronously if it must.** Availability is a property of the file, not of the cache. The cache makes it fast; it never makes it different.

Two consequences worth stating because they are the costs of the rule:

The planner can do synchronous I/O. That is normally a bad idea, and it is bounded here to what document 03 keeps small on purpose: column summaries are a few hundred bytes, a merged sketch is tens of kilobytes, the sample is one chunk. Per-stripe structures are the large ones and they are consulted only when a predicate's stripe-level selectivity is actually being asked for.

Prefetching is still worth doing and is purely a latency optimization: on first bind of a table, the summaries for the columns the query mentions are requested together, so the reads coalesce. It changes when the number arrives, never which number.

## 4.4 The cache

Keyed by `(table, column, kind, generation)`, the same key shape `../graph/07-maintenance.md` section 7.5 uses, for the same reason: a generation is immutable, so an entry is never invalidated, only evicted.

It has its own budget, charged through the existing `Memory` and `Reservation` machinery, defaulting to the smaller of one percent of the memory limit and sixty-four megabytes. Under pressure it evicts; it never fails a query and it never starves an operator. A statistics cache that can cause an out-of-memory is a cache that made the engine worse in exchange for making it faster, which is issue #735's territory and not a place to add load.

Eviction is LRU with one exception: column summaries are pinned once loaded, because they are tiny, because they are consulted by every plan touching the table, and because evicting one buys back a few hundred bytes and costs a synchronous read on the next query.

## 4.5 Concurrency

A statistics object for a given generation is immutable, so it is an `Arc` handed out to readers with no lock on the read path. Building one is done once under a per-key guard so that ten concurrent queries wanting the same sketch produce one read.

The one mutable structure is the observation log of document 06, and it is written on the *completion* path of a query rather than on any hot loop, batched per statement. Issue #512 is the standing warning here, the project has already measured that atomics in the wrong place cost more than they look like they cost, and an observation counter incremented per row would be exactly that mistake. Per statement, once, off the hot path.

## 4.6 The memory-table gap

`Rows::distinct_values`, `null_count`, `top_frequencies`, `text_extremes` and `frequency_occurrences` all answer `None` for a `Memory` table. That means an in-memory table, a `CREATE TABLE` that has been inserted into, a `CREATE TABLE AS`, a materialized CTE, anything in a test, plans on defaults.

The fix, and it is a cheap one because the values are in hand: a memory table maintains the **cheap half** of document 02's catalogue as it is built. Row count and null count are counters. Minimum and maximum are two comparisons per chunk, which `Zone::of` already does. A KMV sketch is one hash per value. Together that is a small constant per appended chunk and it converts the entire class of in-memory table from "no statistics" to "exact counts, exact nulls, exact ranges, and a distinct count that is exact below `k`".

What a memory table does *not* build is the expensive half: no frequency synopsis with a Misra-Gries second pass, no sample, no per-stripe structures. Those are checkpoint work and they arrive when the table is written.

This is also what makes the statistics available during a load rather than only after it, which matters for the query somebody runs immediately after an `INSERT`, which in an embedded database, is most of them.

## 4.7 Statistics on intermediates

Base-table statistics answer questions about base tables. A plan is mostly not base tables, and `../planner/06-cardinality-and-cost.md` section 06.2's honest caveat is precisely this: the error compounds up the tree.

Two mechanisms, and the second is the one that is underused in this industry.

**Propagation, at plan time.** Each operator has a rule for what its output statistics are given its inputs'. A filter keeps distinctness and sortedness and scales counts by a selectivity. A projection keeps everything for the columns it passes through. A group-by makes its key columns exactly distinct, which is *exact*, not estimated, and downstream operators should be told so, because it eliminates a later `DISTINCT` and changes a join's cardinality rule. An inner join over a verified relationship keeps the child's row count exactly (document 07). The class degrades as it propagates and the propagation rules must degrade it honestly: exact combined with estimated is estimated.

**Measurement, at run time, within the query.** The moment a build side is complete, the join knows its exact distinct count, its exact minimum and maximum, and its exact value set. `../planner/09-runtime-filters-and-adaptivity.md` section 09.4 already makes this argument for the filter tier decision and calls it the best part of the layer rule: the planner annotates, the executor decides with the real number. The same observation applies more widely, an aggregate knows its exact group count when it finishes, a scan knows its exact surviving row count, and those numbers are available to *downstream* operators in the same query, free, exact, and with no determinism problem at all because they are a function of the data.

That second mechanism is the cheapest accuracy in the whole directory and it is where a pipelined engine has an advantage over its own optimizer. It is in scope here; what is out of scope is using it to *re-plan*, which `../engine/12-adaptivity.md` settles.

## 4.8 What the interface looks like from an operator

An operator asks for a property of a column or an expression on a plan node, and gets `Known` with a class or `Unknown`. It does not know whether the answer came from a file section, a memory table's running counters, a propagation rule or a build side that just finished, and it does not need to.

That indifference is the design. It is what lets document 05 be a list of decisions rather than a list of plumbing, and it is what makes the statistics-off ablation of document 09 a single switch: return `Unknown` for everything, and every consumer falls back to the behaviour it has today.
