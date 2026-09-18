# 11. Open questions

Seven. Each has what would settle it and what happens until then.

## 11.1 Whether per-stripe statistics earn their bytes

Document 03 section 3.8 found the problem honestly: per-stripe sketches and quantiles for every column of TPC-H `lineitem` at SF100 come to several hundred megabytes, which is over the budget, and the resolution was a rule, per-stripe structures only for columns that are read, that depends on knowing which columns those are.

The uncomfortable part is that the writer does not know, the observation log only knows after the fact, and a first load therefore writes the wrong set.

**Settled by:** measuring what stripe-level statistics are actually worth. If stripe skipping on equality (document 05 section 5.2) and stripe-range selectivity are small wins next to the table-level numbers, the whole per-stripe tier goes away and the budget problem with it.

**Meanwhile:** table-level summaries and merged sketches for every column, per-stripe structures only for columns the encoding chooser already sketched or that appear in a declared key or relationship, and promotion at the next checkpoint.

## 11.2 Synchronous statistics I/O in the planner

Document 04 section 4.3 chose plan reproducibility over planning latency: if a statistic exists in the file, the planner reads it, blocking if it must. That is the right trade for a plan that has to be bisectable and for `plans.rs` baselines that must not flap, and it puts I/O on the latency path of every first query against a table.

**Settled by:** the plan-time measurement of document 09 section 9.6, on SF0.01 TPC-H and on a small ClickBench, where planning is most of the runtime.

**Meanwhile:** the rule stands, with the summaries pinned in cache once read and the columns a query mentions requested in one coalesced batch. The fallback if it costs too much is *not* "use it if resident", that breaks reproducibility, but a per-table eager prefetch on first bind, which changes when the read happens and not which numbers are used.

## 11.3 Which quantile summary

Document 03 section 3.5 requires a mergeable summary with a stated ε and does not name one. The candidates behave differently on the two things this directory cares about: merge cost under frequent appends, and accuracy in the tails, which is where a top-n threshold seed and a range partition boundary both live.

**Settled by:** implementing one behind the S0 interface and measuring the two consumers in document 05 sections 5.5 and 5.7. The interface makes the choice swappable, which is the point of having it.

**Meanwhile:** sixty-four boundaries per stripe with a recorded ε, and the structure treated as an implementation detail behind the section header rather than as a format commitment.

## 11.4 Whether checkpoint-only commit is too conservative

Document 06 section 6.2 pays for reproducibility with a real cost: a purely read-only analytic workload, which is a very common way to use an embedded database, never crosses a commit boundary and therefore never benefits from anything execution learned. The user with a read-only file gets tier 0 and nothing else.

That is the correct default and it may be the wrong only option.

**Settled by:** whether the q-error on a read-only workload is materially worse than on the same data after an `ANALYZE`. If the gap is large, the case for an opt-in `statistics_feedback = commit-on-read` setting becomes real, and it would carry the same replay and reporting rules as tier 2.

**Meanwhile:** checkpoint, `ANALYZE` or write transaction, and `ANALYZE` documented as the answer for the read-only user.

## 11.5 The sample, and what it means to store rows

Document 03 section 3.6 stores 4096 real rows of real data inside the file, which is a genuinely useful statistic and also a category of thing the other statistics are not: every other entry in the catalogue is an aggregate, and an aggregate does not reproduce a person's row.

The rule written there, the sample is data, inherits the column's access control, and is not printed by metadata tools, covers the obvious cases. It does not settle whether a sample should exist at all for columns a user marks sensitive, or what happens when a file is shared for a bug report.

**Settled by:** whether the sample is load-bearing. If the dependence sketches and quantiles cover the multi-predicate case well enough, the sample is a nice-to-have that carries a disclosure surface and it should go.

**Meanwhile:** the sample is built, the rules in section 3.6 hold, and a setting disables it per table and per column.

## 11.6 Correlation between degree and value

Document 07 section 7.5 composes degree statistics along a path and downgrades the class to estimated, because the true multi-hop cardinality depends on whether high-degree parents are the ones that survive the filter, and in real data they usually are. A customer with many orders is more likely to be in the segment being filtered for.

This is the same correlation problem the dependence term solves inside one table, and no statistic in this directory expresses it across a relationship.

**Settled by:** measuring the error on the TPC-H chain queries, where the true numbers are computable. If the estimate is consistently off in the same direction by a similar factor, a single per-relationship correction term would capture most of it, which is cheap and is not in the catalogue yet.

**Meanwhile:** the composition is reported as estimated, and `../graph/`'s reduction does not depend on it being right, it depends on measured removal at runtime, per `../graph/06-the-optimizer.md` section 6.5's gate.

## 11.7 Whether the class discipline survives contact

Document 05 section 5.10's rule, fold on `Exact` only, on `Certified` only with its obligation discharged, on `Estimated` never, is the rule that keeps the invariant true, and it is enforced today by a sentence in a specification.

A sentence is not an enforcement mechanism. The class could be carried in the type system so that the estimated variant has no method that returns a value usable in a fold, which makes the mistake a compile error rather than a code review.

**Settled by:** the first time somebody tries to write the fold and finds out whether the type discipline is workable or whether it infects every call site.

**Meanwhile:** the sentence, plus the ablation of document 09 section 9.3 on every commit, which catches the class of mistake after it is made rather than before. The ablation is not a substitute for the type; it is the thing that means a mistake is caught in a minute rather than in a release.
