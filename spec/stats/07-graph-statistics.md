# 7. Graph statistics

What a declared relationship knows about itself. This is the smallest section of the catalogue by bytes and the largest by consequence, because these are the numbers that turn an estimate into an exact count and, in three cases, turn a join into nothing at all.

## 7.1 The four numbers, already specified

`../graph/06-the-optimizer.md` section 6.2 records four per relationship at build time, in the section header: the child row count, the parent row count, the number of child rows with no parent, and the maximum degree.

From those, an inner link join's output cardinality is **exact**, child rows minus unmatched rows, which is a better number than any cost model this project will have for years, and it is a side effect of storing the join rather than an additional feature.

This document adds five more facts of the same kind and the same cost.

## 7.2 The degree distribution

A log-bucketed histogram of how many children each parent has: thirty-two buckets, a count per bucket, plus the mean, the maximum and the ninety-ninth percentile. About two hundred and fifty six bytes per relationship, computed during the link build from the counts that build already produces.

It answers four questions nothing else can:

**How much does a backward traversal expand?** `orders → lineitem` at TPC-H is one to seven with a mean near four. `part → lineitem` is one to thousands. Those are different operators with different memory profiles, and the mean alone does not distinguish them.

**Is the expansion skewed?** A mean of four with a maximum of four is a uniform fan-out that parallelises by parent. A mean of four with a p99 of nine hundred is a workload where one worker gets the whole tail, and the scheduler should partition by *edge* rather than by parent. This is the same skew question as section 5.4's aggregate and section 09.5's join build, answered from persisted data rather than by sampling at runtime.

**Is factorization worth it?** `../graph/08-vector-engine.md` section 8.3's `Expanded` body avoids materializing the parent's values once per child. Its win is proportional to the mean degree and is nothing at all when the degree is one. The degree histogram is the plan-time number that decides whether to reach for it, and `../graph/10-milestones.md`'s G6 exit is measured on the four TPC-H queries where the number says it should pay.

**How much memory will the expansion need?** Mean degree times child width times surviving parents, with the maximum as the bound for section 5.1's asymmetric reservation.

## 7.3 The certificates, which are the ones that delete work

`../graph/02-the-data-model.md` section 2.3 says cardinality is *verified, not trusted*, the build checks rather than believing the declaration. That check produces facts, and they are worth writing down as statistics because operators other than the join consume them:

**Uniqueness verified.** Every value of the parent key is distinct. This is the distinctness flag of document 02, arrived at by a different route, and it is what makes a join non-duplicating.

**Totality verified.** Every child row has a parent, the unmatched count is zero. With uniqueness, this is a genuine foreign key rather than a declared one.

Together they license three rewrites that remove operators rather than accelerate them:

- **Join elimination.** A join to a parent table whose columns the query never projects, over a verified total unique relationship, does not change the row set and is deleted. This is worth stating plainly because it is the largest single win available from a certificate and it applies far beyond graph workloads: view definitions that join to a dimension nobody selects from, ORM-generated SQL, and star schemas queried through a wide view are all made of exactly this.
- **Outer becomes inner.** A `LEFT JOIN` over a total relationship preserves no extra rows, so it is an inner join, which is reorderable where an outer join is not.
- **Semi-join becomes nothing.** `EXISTS` over a total relationship is `true` for every child row.

Each is legal only on the `Exact` class, each is recorded in `EXPLAIN` with the certificate it used, and each disappears the moment the relationship's generation goes stale, which is document 08's business and is the reason a certificate carries a generation rather than a boolean.

## 7.4 Locality

Two flags, both exact, both free at build time.

**Monotone.** The child is clustered by parent, `lineitem` in `l_orderkey` order, `partsupp` in `ps_partkey` order, which is what `../graph/03-the-file-format.md` section 3.4 collapses into a bit vector instead of storing a full link column, and what makes the gathers of a link join ascending rather than random.

**Gather locality.** For a non-monotone link, the average absolute distance between consecutive parent row ids, which is the number that predicts whether the link join's gathers hit cache. `../graph/06-the-optimizer.md` section 6.4 says the link-versus-hash crossover is a memory hierarchy question and then has to approximate it at plan time; this is the measurement that replaces the approximation.

## 7.5 Multi-hop, bounded honestly

Composing degree statistics along a path gives an expansion estimate for a two-hop or three-hop traversal, `region → nation → customer → orders` is four exact row counts and three degree distributions.

The rule is that composition **degrades the class**: exact counts composed with independence assumptions produce an estimated result, and it is reported as estimated. The mean degrees multiply; the maxima multiply into a bound that is correct and usually useless; and the true answer depends on correlations no per-relationship statistic expresses.

What this is *not* is a reachability index. `../graph/10-milestones.md` explicitly does not schedule transitive link materialization, and nothing here changes that: a two-hop estimate is four cheap numbers multiplied, not a structure.

## 7.6 What the graph layer gives back to ordinary queries

Worth its own section because it is the answer to a fair objection, that a graph layer is paid for by every table and used by join queries.

**The key map is a statistic.** `../graph/03-the-file-format.md` section 3.3 records, at build time, whether the key values were distinct, whether they were sorted, and their minimum and maximum. Those are four of document 02's catalogue entries for that column, exact, and they benefit `GROUP BY`, `DISTINCT`, `ORDER BY` and range predicates on that column whether or not a join is anywhere in the query.

**The link column carries zone maps.** Because it is an ordinary column, per section 3.4, it gets per-part minimum and maximum for free, which prunes parts during a reduction, and also answers ordinary range questions about parent identity.

**The row id space makes direct addressing available.** A dense row id domain is exactly the precondition section 5.4 needs to turn a hash aggregate into an array, and `../graph/05-execution.md` section 5.6 already builds that operator. Once it exists for row ids it exists for any dense integer key, which is most surrogate keys in most schemas.

## 7.7 Cost

Everything in this document is computed inside the link build that `../graph/03-the-file-format.md` section 3.8 already budgets, from counts that build already materializes. The marginal cost is a histogram of thirty-two buckets and a running sum, and the marginal space is under a kilobyte per relationship.

That is the reason these statistics come first in document 10's ordering: they are the cheapest exact numbers in the system and they license the only rewrites in this directory that delete an operator outright.

## 7.8 What is not computed

Per-parent degree, which is the adjacency structure itself and is `../graph/03-the-file-format.md` section 3.5's business. Triangle counts, clustering coefficients, centrality, graph analytics, not query planning, and nothing in the planner would consume them. Correlations between degree and a column's value, which is the honest gap behind section 7.5's class degradation and which is left open in document 11.
