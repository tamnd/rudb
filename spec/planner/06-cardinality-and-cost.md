# Cardinality and cost

What the planner needs to know about the data, how wrong that knowledge is in every system that has been measured, and the one thing rudb already has that most engines do not.

This is the document where rudb diverges most from the firepanda folder next door. That folder concluded: build no estimator, use exact counts, decide late. It is right for a dataframe library that runs a plan node by node and can therefore measure instead of estimating. rudb plans the whole query before running any of it, faces JOB and CEB and TPC-DS as named benchmark gates, and has a sketch layer already built for a different purpose that happens to be exactly what a join estimator wants. So rudb builds one, and builds it small.

## 06.1 The state of the evidence

Leis, Gubichev, Mirchev, Boncz, Kemper and Neumann, *How Good Are Query Optimizers, Really?*, PVLDB 2015, with the Join Order Benchmark over real IMDB data chosen so that correlations between columns are the norm. The finding that stuck: cardinality estimates for joins of three or more relations are routinely wrong by orders of magnitude in every system tested, and the cost model matters far less than the estimates do.

The same authors published a ten-year retrospective, *Still Asking: How Good Are Query Optimizers, Really?*, PVLDB 18(12), 5531 to 5536, September 2025. It is a reflection rather than a new study. It credits the original with refocusing the community on estimation and triggering the learned-estimator line, and it names robustness, adaptive execution and realistic workloads as the live open questions. It opens by quoting Lohman's framing that cardinality estimation is "the root of all evil, the Achilles Heel of query optimization", with the argument that cost models introduce errors of at most about thirty percent for a given cardinality while cardinality errors are unbounded.

Two 2025 results sharpen what "realistic" means, and both should change what rudb measures rather than what it builds:

**JOB-Complex**, Wehrstein, Eckmann, Heinrich and Binnig, AIDB workshop at VLDB 2025, arXiv:2507.07471. The argument is that JOB itself overstates optimizer quality because it lacks simple real-world properties: joins over string columns, complex filter predicates. Their numbers on PostgreSQL 16, as an optimization gap against the optimal plan: 1.18 on JOB-light, 1.99 on JOB, and a learned zero-shot cost model at 1.12 on JOB-light and 2.39 on JOB, so the learned model beats the traditional one on the easy benchmark and loses on the harder one. The benchmark is 30 queries with about 6000 execution plans. *These numbers are from the paper's abstract as reported in a September 2026 search; I have not read the evaluation section.*

**How Good are Learned Cost Models, Really?**, PACMMOD 3(3) article 172, June 2025. The companion argument on the cost side.

The contrarian thread is still live and still relevant: Datta and Rusu, arXiv:2311.17293, observe that modern main-memory analytical systems including DuckDB operate with limited estimation and remain competitive, and the explanation is that vectorized main-memory execution has a flat enough cost curve that a mediocre plan costs a factor rather than a catastrophe. Document 08 is the strongest form of that argument.

## 06.2 The asset

`crates/rudb-encoding/src/sketch.rs` is 508 lines of k-minimum-values sketch. Its module doc says it was chosen over HyperLogLog specifically because the M1 multi-column encoding chooser needed to ask **which values two columns share**, not only how many distinct values each has. It has `union`, `jaccard`, and a `dependence` function over pair hashes.

That is unusual and it matters for two separate reasons.

**Set intersection is what a join estimator actually wants.** The size of an equi-join is a function of how many key values the two sides share and how often each occurs. A min-max histogram cannot express that. A distinct count plus an independence assumption cannot express that. Jaccard over KMV sketches estimates it directly, and the sketches are already computed per column and per block by the encoder, which means the statistics that matter are a byproduct of writing the data rather than a maintenance job.

**Dependence is the correlation term.** `WHERE country = 'DE' AND language = 'de'` has an actual selectivity far higher than the product of the two, because the columns travel together. Every textbook optimizer multiplies and is wrong. The `dependence` function over pair hashes measures exactly how much two columns in the same table travel together, and the encoder already computed it for the columns the chooser evaluated.

So the statistics layer is mostly plumbing rather than new computation:

- per-block minimum, maximum and null count, which the scan keeps anyway for zone-map pruning
- per-column KMV sketches, which the encoder already builds
- pairwise dependence for the column pairs the chooser already evaluated
- exact row counts from the catalog

What has to be built is the path from those to the planner and the estimator that consumes them, plus a decision about which column pairs get a dependence number when the chooser did not evaluate them.

**The honest caveat, stated in `spec/engine/11-optimizer.md` section 11.3 and repeated here so it is not lost.** Sketches on the base table say nothing about the selectivity of a predicate whose result then feeds a join, and the error still compounds through the tree. This does not solve cardinality estimation. It makes the base cases much better and the correlation term much better, which is where a large share of the catastrophic errors come from, and it does so with data that already exists.

## 06.3 The estimator, in four rules

**Base cardinality is exact.** The catalog knows.

**Single-column predicate selectivity** comes from min, max, null count and distinct count, with an equi-depth histogram and a most-common-values list for the columns that justify one. These are the same statistics that drive zone-map skipping per `spec/05-storage.md` section 5.3, maintained once and read twice.

**Multi-predicate selectivity on one table** uses the dependence term rather than multiplying. Where there is no dependence number for a pair, fall back to a bounded version of independence: multiply, but clamp the result to no less than the most selective conjunct divided by a constant, because the failure mode of independence is always in the same direction, which is underestimating, and underestimating is the dangerous direction.

There is a second mechanism for this case that `spec/09-optimizer.md` section 9.3 specifies and that is worth having: **evaluate the conjunction against a stored sample** of the table. A fixed-size reservoir per table, cheap to evaluate against, and it detects arbitrary correlations among common values that no sketch expresses. Sampling's known weakness is sparsity after a chain of joins has cut the cardinality down, which is precisely why it is used for the single-table conjunction case and not for the join case.

**Join cardinality** uses Jaccard over the two sides' KMV sketches to estimate the shared key space, combined with the two input cardinalities. The containment assumption with distinct counts is the fallback where a sketch is missing.

For the join kinds unnesting produces, the estimator needs explicit rules rather than the inner-join formula, because unnesting produces exactly the joins with the worst estimates:

- A `Semi` join's output is bounded by its left side and is usually a large fraction of it. Estimating it as an inner join over-estimates by the right side's duplication factor.
- An `Anti` join's output is the left side minus the semi join's.
- A `Single` join's output is exactly its left side. Not an estimate.
- An outer join's output is at least the preserved side.

Those four rules are twenty lines and they fix a class of estimate that is wrong by the duplication factor of the right side, which on a fact table is large.

## 06.4 The cost model

Rows produced and bytes moved, weighted by whether a side is likely to fit in cache.

Nothing more. `spec/09-optimizer.md` section 9.4 gives the reason in one sentence and it is the right one: a cost model consuming bad cardinalities in more detail is not more accurate, only more confident. Leis's thirty-percent number says the cost model's own contribution to the error is small compared to what it is fed.

Two properties to keep:

**Fit it to the microbenchmarks rather than guessing it.** `spec/engine/11-optimizer.md` puts the optimizer at layer nine specifically so that the operators below it are built and measured first, and the constants in the cost model are then numbers from `rudb-bench` rather than invented ratios. A cost model written against operators that are about to be replaced is a cost model that gets rewritten with them.

**Keep it additively separable if it can be.** Stoian and Kipf's DPconv, PACMMOD 2(6) 2024 and a SIGMOD Record highlight in April 2026, needs the cost function `c(T1, T2)` to split into two factors that can be sunk into the DP table entries, and it explicitly cannot handle the nested-loop cost `c(T1) · c(T2)` for that reason. Document 07 explains what DPconv buys. The point here is that the shape of the cost function is a decision with an algorithmic consequence and it should be made knowingly rather than discovered.

## 06.5 What "wrong" means, and how to report it

A cardinality estimate is not right or wrong, it is off by a factor. The standard measure is **q-error**, the maximum of estimate over actual and actual over estimate, reported as a distribution over a benchmark rather than as a mean, because the tail is what causes the catastrophe.

rudb publishes the q-error distribution on CEB alongside the timings. `spec/engine/11-optimizer.md` section 11.8 requires it and the reason is worth repeating: an engine whose estimates are bad but whose execution is fast is a different engine from one where both are good, and the difference shows up on workloads that are not in the benchmark. Publishing the estimate quality is what makes the difference visible before a user finds it.

The measurement wants `EXPLAIN ANALYZE` to print estimated and actual side by side with the ratio, which document 12 specifies, because the ratio is the first thing anyone diagnosing a bad plan wants and computing it by hand is tedious.

## 06.6 What rudb deliberately does not build

**No learned model.** Not because the line is uninteresting but because it is a bad fit for the product. rudb is an embedded database that has to be correct and reasonable on the first query it ever sees on a user's data, with no training phase, no workload history and no model file. The 2026 work on the out-of-distribution problem, *CardOOD*, VLDB Journal 35(4), May 2026, is a whole paper about the failure mode this constraint avoids by construction. The MCTS reproducibility study, arXiv:2603.16474, March 2026, reaches a related conclusion from the search side: it finds that AlphaJoin and HyperQO's claimed gains do not hold under diverse workloads, attributes the instability by ablation to the **learned cost models** suffering severe out-of-distribution errors while the search strategy itself remains sound, and then does better by dropping the learned component and using the database's own cost model. *That summary is from the paper's abstract as reported in a September 2026 search.* For a project whose second axis is a per-query floor, a component that is excellent on average and occasionally catastrophic is the wrong trade at any accuracy.

**No sketch building at query time.** The sketches come from the encoder. The literature's standard caveat about sketch-based multi-way join estimation is that building sketches online does not scale, and rudb does not have to, which is the whole value of the asset.

**No multidimensional histograms.** Storage grows dramatically with dimensions and the dependence term covers the case they exist for.

## 06.7 What to watch

Three 2026 results that are adjacent to decisions in this folder and that should be read before the estimator is considered finished. None of them changes the plan above today; each of them could change one section of it.

**CorrBound: Cardinality Estimation Accounting for Inter- and Intra-relation Correlations**, SIGMOD 2026. Directly the problem section 06.3's dependence term addresses, and if its bound is tighter than Jaccard over KMV it goes in the same slot with no other change.

**Coresets for Robust Query Optimization**, Raychaudhury, Xiu, Agarwal, Sintos and Yang, PACMMOD 4(2), DOI 10.1145/3801896, May 2026. Robust optimization in the sense of choosing a plan that is good across the range of cardinalities the estimate could actually have taken, rather than good at the point estimate. That is the same objective document 08 reaches by a completely different mechanism, and the interesting question is whether they compose or whether predicate transfer makes it moot. *I have the bibliographic record and not the abstract; the ACM page returned 403.*

**Succinct Structure Representations for Efficient Query Optimization**, Jiang, Wang and Koch, PACMMOD 4(3), May 2026. Relevant to how the join graph and the statistics are represented inside the planning-time budget, which document 07 section 07.5 says is enforced. *Bibliographic record only.*

## What we should take from this document

rudb builds an estimator where firepanda does not, because it plans the whole query before running it and because JOB, CEB and TPC-DS are named gates.

The asset is `rudb-encoding/src/sketch.rs`: KMV with `jaccard` for join key overlap and `dependence` for the correlation term, already computed by the encoder. Most of the work is plumbing, not statistics.

Four rules: exact base counts, histogram and MCV for single-column selectivity, dependence or a clamped independence for conjunctions with a per-table sample behind it, and Jaccard for joins, plus four explicit rules for the join kinds unnesting produces.

The cost model is rows and bytes fitted to the microbenchmarks, and it should be kept additively separable because that is what DPconv needs.

Publish the q-error distribution on CEB, and print estimated against actual with the ratio in `EXPLAIN ANALYZE`.

No learned model, for a product reason rather than an accuracy one, and the 2026 reproducibility evidence says the search was never the weak part anyway.
