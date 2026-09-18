# 10. Milestones

Seven, each with one exit measurement, each shippable alone. The ordering is by dependency and then by measured value per unit of work, and the first two change no query's behaviour, which is stated rather than disguised.

Nothing here depends on `../graph/`, and S5 is where the two directories meet. That independence is deliberate: the statistics work should be able to deliver on ClickBench, which has no joins at all, before the join work lands.

## S0: the interface and the switch

Document 04 section 4.1's `Known`/`Unknown` type with its three classes, the per-rule settings of document 09 section 9.2, and the ablation harness of section 9.3, with every consumer returning `Unknown` and every operator taking today's path.

Nothing gets faster. What exists at the end is the apparatus that makes everything after it measurable, and the class histogram, which at this point reads one hundred percent `Unknown` and is the honest zero.

**Exit:** every suite produces identical answers and identical timings with the switch in both positions, and the class histogram is published.

## S1: the numbers that already exist, wired up

No new persisted bytes. Route what the file already knows, exact row counts, exact null counts, zone-map extremes, dictionary sizes, the frequency synopsis, through the S0 interface, and fix the two exactness gaps of document 02 section 2.6: `distinct_values` returning `None` for any column containing a null, and `distinct_values` answering only for strings.

Plus document 04 section 4.6: memory tables maintain counts, extremes, null counts and a sketch as they are built, which is where the biggest single jump in the class histogram comes from, because every in-memory table currently has nothing at all.

**Exit:** the class histogram on ClickBench and on TPC-H SF1, before and after. No timing claim.

## S2: persistence

`RUDBCS1` and `RUDBSK1` from document 03, the merged sketches, the column summaries, and the per-stripe rule of section 3.8 that keeps them affordable.

**Exit:** claims S1 and S2 of document 09, under two percent of column bytes and under ten percent of write time, measured on TPC-H SF100 and ClickBench at a hundred million rows, plus a version 11 reader opening a version 10 file with every statistic `Unknown` and every answer unchanged.

## S3: the consumers with the largest measured win

The first milestone that makes a query faster. Four rules, chosen because their inputs are exact and their mechanisms are mechanical: aggregate hash-table presizing, direct-addressed grouping on a dense key, validity-free kernels on a column with a zero null count, and conjunct ordering by selectivity over cost.

**Exit:** the per-rule table of document 09 section 9.4 on ClickBench, four rows, each with the queries it fired on and its own delta, and claim S5 evaluated on the suite total. ClickBench specifically, because it has no joins, and because if this directory cannot move a workload where Aggregate is 26 percent and Filter is 10 percent of the runtime, its premise is wrong.

## S4: bounds, and the decisions with asymmetric cost

`RUDBQT1` quantiles and `RUDBSM1` the sample, then the consumers that need a bound rather than a point: memory reservation from exact widths, spill sizing, top-n threshold seeding, sort range partitioning, and the parallelism thresholds of document 05 section 5.8.

**Exit:** peak RSS and the top-n queries on ClickBench, and peak RSS on TPC-H Q9, which `../bench/tpc-h/03-the-queries.md` section 3.4 names as the query where memory is the whole game. A time improvement with a memory regression does not close this milestone.

## S5: graph statistics and the certificates

Document 07. The degree distribution and the locality flags in the link build, the uniqueness and totality certificates, and the three rewrites they license, join elimination, outer to inner, semi-join removal.

Depends on `../graph/`'s G1 and G2 for the link sections to exist. Gives back the plan-time number that `../graph/`'s G3 and G6 decisions are currently approximating.

**Exit:** join elimination firing on a named query set with the row counts to prove it changed nothing, and the link-versus-hash decision of `../graph/06-the-optimizer.md` section 6.4 made from the measured gather locality instead of the plan-time approximation, with the two decisions compared.

## S6: tier 0 and tier 1 feedback

Document 06. Observation everywhere, `EXPLAIN ANALYZE` with estimate, actual and ratio, the q-error distribution published, the bounded log, the correction rules with their clip and their oscillation detector, and commit only at checkpoint, `ANALYZE` or a write transaction.

**Exit:** claim S6, a read-only workload run twice produces byte-identical plans and provenance, as an automated test rather than an observation, plus the q-error distribution on CEB before and after a checkpoint on a write-heavy workload, plus the oscillation detector demonstrated firing on a constructed case.

## What is not scheduled here

**Tier 2, the bandit.** It belongs to `../engine-v2/`'s F10 and arrives with the seam registry's adaptive policy, not with this directory. Document 06 section 6.7 specifies what it would have to obey; it does not schedule it.

**Learned models of any kind.** `../planner/06-cardinality-and-cost.md` section 06.6.

**Multidimensional histograms**, transitive reachability statistics, and correlations between a relationship's degree and a column's value. Document 11 says what would put the third one on a list.

## The order, and what would reorder it

S1 before S2 because wiring up numbers that already exist is free and tells you whether anything consumes them. S3 before S4 because its inputs are exact and S4's are bounded, and a directory whose first shipped win depends on a quantile summary's ε is a directory that will spend its first month arguing about ε.

One thing would reorder it. If S3's per-rule table shows the wins concentrated in memory behaviour rather than in CPU, which is plausible, since issue #735 says the engine holds about three times what it charges, then S4 moves ahead of S3's remaining rules, because the reservation work is then the larger lever and it is the one users feel as a failure rather than as a delay.
