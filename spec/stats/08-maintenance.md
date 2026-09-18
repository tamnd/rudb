# 8. Maintenance

A statistic is a claim about data that has moved on. This document is about what happens when it has.

## 8.1 Four states, the same four

`../graph/07-maintenance.md` section 7.1 defines them for sections and they apply unchanged here.

**Current.** The section's generation equals the table's. Every class holds.
**Partial.** Rows have been appended since. Mergeable statistics are updated in place and stay current; the rest are correct about a prefix.
**Stale.** Rows have been deleted or updated since. Section 8.3.
**Absent.** There is no section. Every consumer answers `Unknown`, which document 04 section 4.1 made a first-class answer precisely so that this state needs no special handling anywhere.

No state is an error and no state makes a query wrong. The class flag is what carries the difference into the plan.

## 8.2 Append

Document 02 section 2.4's mergeability rule is what makes this the easy case. Counts add, extremes merge, sketches union exactly, quantile summaries merge with their ε, the frequency synopsis merges with the bound from *Mergeable Summaries*, and sortedness merges when the new stripe's range does not overlap the previous maximum.

So an append leaves the statistics **current**, not partial, for everything mergeable. The section's generation advances with the table's.

Two exceptions:

**The sample** is updated by reservoir sampling, which is defined over a stream, so it is also current, but the reservoir's guarantee is about the whole stream and it needs the total count, which it has.

**A validity certificate** is not mergeable and must be re-established. Checking that the appended rows' foreign key values exist in the parent is a lookup per appended row against a key map that is already there, which is cheap and is worth doing eagerly rather than downgrading the certificate, because section 7.3's three rewrites are worth more than the microsecond, and because the alternative is that the first bulk insert silently turns off join elimination for the rest of the file's life.

## 8.3 Delete and update, and the direction that survives

A delete invalidates exactness and it does not invalidate everything. Which half survives is a per-statistic fact and it is worth writing down, because the lazy answer, mark it all stale, throws away most of the value at the first `DELETE` anybody runs.

| statistic | after a delete |
| --- | --- |
| row count | exact; deletes are counted |
| null count | an upper bound, and exact if the delete's predicate could not touch a null |
| minimum, maximum | still bounds, still valid, possibly wide |
| distinct count | an upper bound |
| the value set below `k` | a superset, so an `IN`-list filter built from it is still correct, merely less selective |
| distinctness flag | still exact; deleting rows cannot create a duplicate |
| sortedness | still exact; deleting rows cannot unsort a column |
| quantiles | estimated, ε no longer guaranteed |
| frequency leaders | the counts become upper bounds; `omitted_max` still bounds the omitted set |
| validity certificate | survives a delete from the *child*; a delete from the *parent* invalidates it |
| feedback observations | decayed by generation distance, per document 06 section 6.4 |

Every one of those is a class downgrade, not a discard: `Exact` becomes `Certified { bound }` with the direction recorded. And the direction is what the consumer needs, a hash table sized from an upper bound on distinct values is over-allocated and correct, while one sized from a lower bound rehashes.

An update is a delete and an insert and is treated as both.

## 8.4 Checkpoint

Where the work happens, and the order is fixed by document 03 section 3.9: columns, then graph sections, then statistics sections.

Three things occur at a checkpoint and nowhere else:

1. Non-mergeable statistics are rebuilt for the columns whose staleness has passed the threshold of section 8.6.
2. Tier 1 observations accumulated since the last checkpoint are committed, per document 06 section 6.2. This is the boundary that keeps a read-only workload's plans identical forever.
3. Promotion decisions are applied: which columns get per-stripe sketches, which column pairs get a dependence sketch. Document 03 sections 3.7 and 3.8.

A checkpoint that rebuilds statistics rewrites **only the statistics sections**. The column data is untouched, the section table gets new entries and the old ones become unreferenced. That is a property of the section layout rather than an optimization, and it is what makes statistics maintenance affordable on a hundred-gigabyte file.

## 8.5 `ANALYZE`

The explicit, foreground, bounded form of the same work. `ANALYZE`, `ANALYZE table`, and `ANALYZE table (column, ...)`.

It prints what it is about to do and what it will cost before it does it, in the same spirit as the generation subcommand of `../bench/tpc-h/02-the-data.md` section 2.7: a command that reads a hundred gigabytes because somebody typed four characters is a command people run once.

It is never required. Every statistic in this directory is built by the writer or merged by an append, and `ANALYZE` exists for three cases: after a bulk delete, when a certificate needs re-establishing, and when somebody wants the expensive half computed for a table that was built in memory.

**The compatibility question.** DuckDB accepts `ANALYZE`; exactly what it does at the vendored commit is read out of the source rather than asserted here, and rudb's rule is the one `../12-duckdb-compat.md` gives generally, accept what DuckDB accepts, do something defensible, and never fail a statement DuckDB succeeds at. `SUMMARIZE` is a different thing, is a user-facing query, and is not this.

## 8.6 When a rebuild happens on its own

Thresholds, all settings, all printed by the system view of section 8.8:

- more than ten percent of a table's rows deleted since a statistic's generation: the statistic is downgraded now and rebuilt at the next checkpoint
- more than half deleted: rebuilt at the next checkpoint regardless of kind
- a validity certificate whose parent table has changed: invalidated immediately, re-established at the next checkpoint or by `ANALYZE`

**Never during a query.** A query that discovered stale statistics does not stop to fix them. It records the observation, uses the downgraded class, and runs. A database that occasionally takes forty seconds on a query because it decided to rebuild a synopsis is a database with an unpredictable per-query floor, which is the axis `../02-the-goal.md` says cannot be spent.

## 8.7 Transactions

Statistics belong to a committed generation, so a reader at a snapshot reads the statistics of that snapshot, keyed as document 04 section 4.4 says.

A transaction that has written rows it has not committed sees base statistics plus its own in-memory deltas, counts, extremes and null counts over its own uncommitted chunks, which is the memory-table path of document 04 section 4.6 applied to a write set. It does not see, and cannot corrupt, the committed statistics. A rollback discards the deltas and the observations taken under it.

## 8.8 Being able to see all of it

`rudb_statistics()`, a system view, one row per table, column and kind, with the class, the generation, the staleness, the byte size and when it was built. Named `rudb_` because it is not a DuckDB view and `../12-duckdb-compat.md`'s rule is that rudb's own surfaces carry rudb's prefix.

`rudb_links()` already exists for relationships, per `../graph/02-the-data-model.md` section 2.6, and gains the degree facts of document 07.

And the metrics, which `rudb-metrics` already carries into the harness: statistics bytes read per query, cache hit rate, how many plan decisions were made on `Exact` against `Certified` against `Estimated` against `Unknown`, and the list of feedback keys currently disabled by oscillation.

That last counter is the health metric for this entire directory. A release where the fraction of decisions made on `Unknown` goes up is a regression even if every timing improved, because it means the engine got lucky rather than informed.
