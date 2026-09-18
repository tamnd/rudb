# Facts, not estimates

`../stats/` says what is written to disk and what is resident. `../graph/` says what a stored join knows about itself. Neither says how a number gets from there into a plan decision, what happens when it is not there, or what a pass is allowed to do with one. That is this document, and it is the first of the two documents that make the physical plan in document 07 trustworthy rather than a second place to guess.

## 4.1 What the planner has today, and why it stopped there

`crates/rudb-opt/src/pass.rs`:

```
pub struct Context {
    disabled: Vec<&'static str>,
    statistics: crate::estimate::Statistics,
    ...
}
```

`Statistics` is a `BTreeMap` from table name to row count, copied at statement start. `crates/rudb-opt/src/estimate.rs` turns that into a cardinality for every node using two constants, 0.2 kept per filter conjunct and 0.1 kept per group by, and it returns `Option<f64>` with `None` meaning unknown, deliberately, so that uncertainty travels up rather than being rounded away.

That last design choice is correct and this document keeps it. What it cannot support is any pass whose wrong answer is expensive. `rows_stat` already carries a `Class` from `rudb_common::stat`, so the vocabulary exists in the tree; what does not exist is anything filling it in with a real measurement.

## 4.2 The Fact

One type, used by every consumer, replacing the bare `Option<f64>`.

```
Fact {
    value:      f64,
    class:      Class,
    provenance: Provenance,
}

Class {
    Exact,
    Certified { bound: f64, direction: Bound },
    Estimated,
    Unknown,
}
```

`Class` is `../stats/02-the-catalogue.md` section 2.1's three classes plus `Unknown`, which the statistics directory does not need because it is about what is written and this document is about what is read. A fact that is not resident, not written, or not applicable is `Unknown`, and `Unknown` is not the same as `Estimated`. `Estimated` means somebody computed a number with no proof. `Unknown` means nobody computed anything. A planner that conflates them treats a missing statistic as a guess and produces a plan it cannot attribute.

`Bound` says which side the certificate bounds: `AtMost` for a frequency synopsis `omitted_max`, `AtLeast` for a lower bound on distinct count from a sketch, `Within` with an epsilon for quantile boundaries.

`Provenance` is what `EXPLAIN` prints and it names the source, not the value: `RowCount`, `ZoneMap`, `NullCount`, `Sketch`, `FrequencySynopsis`, `Quantiles`, `Dictionary`, `Sortedness`, `Distinctness`, `LinkHeader`, `DegreeDistribution`, `Sample`, `Default`, `Observed`.

## 4.3 The three uses, and the rule attached to each

This is the part that makes the type worth having. A consumer declares which of three things it is doing with a fact, and the class it is allowed to read follows from that.

**Answer.** The fact *is* the result. `COUNT(*)` from the row count. `COUNT(column)` from the row count minus the null count. `COUNT(DISTINCT s)` from the dictionary size. `MIN(x)` from the merged zone map. A top-k `GROUP BY` from the certified frequency synopsis when the certificate discharges, per `../storage-v3/11-certified-frequency-synopses.md`.

Rule: `Exact` always. `Certified` only where the consumer can discharge the proof obligation and has a fallback for when it cannot. `Estimated` and `Unknown` never, under any circumstance, in any mode, behind any setting.

**Decide.** The fact chooses between two plans that both produce the same rows. Build side. Grouping strategy. Join order. Reduction schedule. Memory reservation. Parallel degree.

Rule: any class. A decision made from `Unknown` is a decision made from the documented default, and the default is printed as such. This is where the bulk of the facts go and it is why `Estimated` is allowed to exist at all.

**Enable.** The fact licenses a rewrite that changes the shape of the plan in a way that would be wrong if the fact were wrong. Join elimination from a uniqueness and totality certificate. Sort elimination from a sortedness fact. `DISTINCT` elimination from a distinctness flag. Group by elimination when the key is already unique. Partition pruning from an exact value set.

Rule: `Exact` only. Never `Certified`, because a bound is not an equality and the rewrites in this class need an equality. Never `Estimated`. This is the strictest of the three and it is the one that would be easiest to get wrong, because an enabling rewrite that fires on a stale fact does not produce a slow query, it produces a wrong answer.

The three-way split is the whole contract. A pass author picks a use, the type system and the review both check the class, and `EXPLAIN` prints which one happened.

## 4.4 The service, and the rule that it never blocks

`../stats/04-in-memory.md` states that no query ever waits on a statistic. Restated as a planner contract:

**`Facts::get` returns immediately, always.** If the statistic is resident it returns it. If it is written but not resident it schedules the load, returns `Unknown` for this statement, and the next statement gets the real number. If it is not written it returns `Unknown` forever.

That has one consequence worth naming, because it looks like a bug the first time somebody hits it. **The first execution of a query on a cold database may get a different plan from the second.** This does not violate the determinism rule in `../stats/06-the-reward.md`, because the plan is still a function of the data and not of history: the second plan is the one the data always justified, and the first was made with part of the data unread. But it does mean plan stability tests must run against a warm statistics service, and `EXPLAIN` must print `Unknown` facts rather than hiding them, so that a stability diff is attributable to a cold load rather than mysterious.

The snapshot key from `../stats/04-in-memory.md` binds a statement to a generation of the statistics, so that two passes in the same statement cannot see different numbers. `Context` holds the key, not the numbers.

## 4.5 What the planner consumes, by decision

`../stats/05-every-query.md` is the supply side of this table, operator by operator. This is the demand side, decision by decision, and the two must not disagree. Where they do, the statistics directory wins, because it is the one that pays for the bytes.

| decision | fact | use | class needed | document |
| --- | --- | --- | --- | --- |
| scan parallel degree, morsel size | rows per part | decide | Exact | 09 |
| zone skip | per-part min and max | decide | Exact | 07 |
| range predicate selectivity | quantile boundaries | decide | Certified | 05 |
| equality predicate selectivity | frequency leaders, distinct count | decide | Certified or Estimated | 05 |
| multi-predicate selectivity | column dependence | decide | Estimated | 05 |
| arbitrary conjunction selectivity | sample | decide | Estimated | 05 |
| `COUNT(*)`, `COUNT(col)` | row count, null count | answer | Exact | 11 |
| `COUNT(DISTINCT s)` on a dictionary column | dictionary size | answer | Exact | 11 |
| top-k `GROUP BY` | frequency synopsis | answer | Certified | 11 |
| group by hash presizing | distinct count | decide | any | 11 |
| grouping strategy, heavy hitters | frequency leaders | decide | Certified | 11 |
| group by elimination | distinctness flag | enable | Exact | 05 |
| distinct elimination | distinctness flag | enable | Exact | 05 |
| sort elimination, merge path | sortedness | enable | Exact | 05 |
| join cardinality over a link | link header four numbers | decide | Exact | 06 |
| join cardinality without a link | key-value overlap sketch | decide | Estimated | 06 |
| build side | either of the above | decide | any | 07 |
| join elimination | validity certificate | enable | Exact | 05 |
| link join versus hash join | degree distribution, gather locality | decide | Exact | 06 |
| reduction schedule and worth | parent predicate selectivity | decide | any | 06 |
| exact `IN` list runtime filter | value set below k | enable | Exact | 06 |
| partition pruning | value set | enable | Exact | 07 |
| memory reservation | value width, distinct count | decide | any | 07 |
| spill threshold | value width, row count | decide | any | 09 |
| string kernel choice, view versus inline | value width | decide | Exact | 10 |
| layout requirement satisfiable | dictionary, encoding capability | decide | Exact | 07 |

Every row names a document in this folder that makes the decision and a class it is allowed to make it from. A row with no consumer is a statistic `../stats/00-README.md` says should not be written, and this table is how that rule gets checked from the other end.

## 4.6 The four changes this needed in `../stats/` and `../graph/`

Four small things, all of them for consistency rather than capability, all of them cheap because the information already exists, and all four now written into those two directories rather than described here. They are listed together so that the edits are findable from one place.

**One. The `Fact` type belongs in the statistics interface, not in the planner.** `../stats/02-the-catalogue.md` defined the three classes and said every consumer interface returns the class along with the value, which is right, but the shape of that return was not specified anywhere, which would have let `rudb-opt`, `rudb-phys` and `rudb-exec` each invent a different one. It is now specified once, in the directory that owns the supply, with `Unknown` as a fourth class and with `Provenance`, at `../stats/02-the-catalogue.md` section 2.1.1.

**Two. The three-use rule is stated in `../stats/05-every-query.md`.** That document is organised by operator and says which statistic changes which decision. It did not say which class each decision is entitled to, and section 4.3 here was the missing column, because without it a reader can reasonably conclude that a certified frequency synopsis licenses sort elimination. It is now section 5.1.1 there.

**Three. `../graph/06-the-optimizer.md` section 6.2's four numbers are typed as Facts.** They were already exact and already recorded in the section header at build time. Saying so in the type is what lets the join cardinality for a link join stay `Exact` all the way up through the physical planner's cost comparison, which is the single most valuable fact in the whole catalogue and would have been a shame to lose to a bare `f64` at the boundary.

**Four. `../stats/06-the-reward.md` says what a cold start is.** Section 4.4 here observes that a cold statistics service produces a different plan from a warm one, which is correct behaviour and looks exactly like the thing the determinism rule forbids. That document draws the line between a verified fact about the data and a preference learned from timings, and it now says that a fact arriving late is the first kind and not the second, along with the two procedural consequences for a plan stability corpus.

None of the four adds a byte to the file format.

One related question is deliberately left open rather than answered by an edit. `../stats/06-the-reward.md` section 6.5 already says an observation is `Exact` only for the generation it was taken on, which is the right rule for observations. The same question for a persisted statistic, which is what an `Exact` fact means when the statement's snapshot is newer than the statistic's generation, is not settled anywhere, and document 15 section 15.2 says so. It matters because the enabling class in section 4.3 is the one where a stale fact gives a wrong answer rather than a slow query.

## 4.7 Estimation is still needed, and it gets smaller

Facts do not replace estimation, they shrink its domain. Three places still need a formula.

**Selectivity of a predicate the synopses do not cover.** A `LIKE` with a leading wildcard, a function call, a comparison between two columns. The sample answers some of these and a default answers the rest. `../planner/06-cardinality-and-cost.md` section 06.3 remains the reference for the formulas and this folder does not restate them.

**Join cardinality without a link.** The key-value overlap sketch in `crates/rudb-encoding/src/sketch.rs` is already 508 lines of k-minimum-values with `jaccard` and `dependence`, built by the encoder, currently persisted nowhere. `../stats/` persists it. The estimator uses it. This is the single largest accuracy improvement available for the price of writing bytes that are already computed.

**Cardinality above the first estimate.** The rule from `estimate.rs` stands unchanged: uncertainty travels up. A node whose input is `Estimated` produces `Estimated`, a node whose input is `Unknown` produces `Unknown`, and nothing rounds a class upward anywhere.

What shrinks is the compounding. Today three conjuncts take a table to one row in a hundred and twenty five, which `estimate.rs` calls out as the part most likely to be wrong. With quantile boundaries for ranges, frequency leaders for equalities and a dependence term for the pair, the compounding applies only to the residue, and the residue on ClickBench and TPC-H is small because those workloads filter on columns that have synopses.

## 4.8 How the error is reported

`../stats/09-measurement.md` owns q-error publishing and this folder consumes it. Two additions specific to the planner.

Every `EXPLAIN ANALYZE` line carries estimated and actual, and the class of the estimate. A 100x q-error on an `Estimated` fact is a known limitation. A 100x q-error on an `Exact` fact is a bug in the statistics layer or a staleness bug, and the two should never be reported the same way.

The q-error histogram is published per class. Publishing one number over all facts hides the thing worth knowing, which is whether the exact facts are actually exact.

## What we should take from this document

One `Fact` type with four classes, carrying provenance, returned by one service that never blocks.

Three uses with three different rules. Answer takes `Exact` or a discharged `Certified`. Enable takes `Exact` only, because an enabling rewrite on a wrong fact gives a wrong answer. Decide takes anything, including `Unknown`, because the worst case is a slow query and a printed default.

Four small consistency changes asked of `../stats/` and `../graph/`, none of which costs a byte on disk.

And the reason this document comes before document 07: a physical planner reading `Option<f64>` from two hardcoded constants is a second place to guess wrong, not a place to make decisions.
