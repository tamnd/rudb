# 3. The queries

All twenty two, classified by what they demand of the engine rather than by the business question they describe. The text is already in `src/suite.rs` and is not repeated; what is here is the reading of it that tells a reader which queries a change should have moved, which is the thing a benchmark table cannot say on its own.

## 3.1 The table

Join edges counts only equality edges between base tables, counting each occurrence of a table separately. "Link" says whether `../../graph/`'s forward link applies to the query's largest join, and "reduce" says whether a predicate on a small table can be pushed through the join graph to a large one, which is what `../../graph/05-execution.md` section 5.4 is for.

| q | tables | edges | shape | demands | link | reduce |
| --- | --- | --- | --- | --- | --- | --- |
| 01 | 1 | 0 | pricing summary | scan, eight aggregates over `DECIMAL`, two group keys |, |, |
| 02 | 5 + 4 | 4 + 3 | minimum cost supplier | correlated scalar subquery, `LIMIT 100` with ties | yes | yes |
| 03 | 3 | 2 | shipping priority | two date filters, group by three, `LIMIT 10` | yes | yes |
| 04 | 2 | 1 | order priority checking | semi join (`EXISTS`) | yes | yes |
| 05 | 6 | 5 | local supplier volume | chain through `region`, one join on two columns | yes | yes |
| 06 | 1 | 0 | forecasting revenue change | scan, three predicates, one sum |, |, |
| 07 | 6 | 5 | volume shipping | two aliases of `nation`, disjunctive nation pair | yes | yes |
| 08 | 8 | 7 | national market share | deepest join graph in the suite, `CASE` in an aggregate | yes | yes |
| 09 | 6 | 5 | product type profit | no selective filter, largest intermediate in the suite | yes | weak |
| 10 | 4 | 3 | returned item reporting | `LIMIT 20`, group by eight columns | yes | yes |
| 11 | 3 + 3 | 2 + 2 | important stock | `HAVING` against a scalar subquery over the same join | yes | yes |
| 12 | 2 | 1 | shipping modes | `CASE` aggregates, `IN` on a small list | yes | yes |
| 13 | 2 | 1 | customer distribution | left outer join with a predicate in the `ON`, group of a group | yes | no |
| 14 | 2 | 1 | promotion effect | `CASE` inside a sum, ratio of two sums | yes | weak |
| 15 | 2 | 1 | top supplier | a view, a scalar `max` over it, ties possible | yes | no |
| 16 | 2 | 1 | parts supplier relationship | anti join via `NOT IN`, `count(DISTINCT)` | yes | yes |
| 17 | 2 | 1 | small quantity order revenue | correlated subquery over the same table | yes | yes |
| 18 | 3 | 2 | large volume customer | `IN` over a grouped subquery, `LIMIT 100` | yes | yes |
| 19 | 2 | 1 | discounted revenue | three-way disjunction of conjunctions, no useful pushdown | yes | no |
| 20 | 2 + 3 | 1 + 2 | potential part promotion | three levels of nesting, `IN` over a correlated aggregate | yes | yes |
| 21 | 4 + 2 | 3 + 2 | suppliers who kept orders waiting | self-join of `lineitem` twice, `EXISTS` and `NOT EXISTS` | yes | yes |
| 22 | 1 + 1 | 0 + 1 | global sales opportunity | anti join, `substring` on a key, scalar avg subquery | yes | no |

## 3.2 The two with no join

Q1 and Q6 are ClickBench queries wearing different column names, and they are the two that run today. They are worth keeping in view for two reasons. They calibrate: if Q1 is 3x behind DuckDB, nothing about the join explains that and the answer is in `../../perf/`. And Q1 is the suite's decimal test, eight aggregates over `DECIMAL(15,2)`, including `avg`, which is where `src/answer.rs`'s existing three-renderings problem came from.

## 3.3 The chains, which is where the reduction has to pay

Q3, Q5, Q7, Q8, Q10 and Q12 are the same shape at different depths: filter a small table, follow foreign keys to `lineitem`, aggregate. Q5 is the clearest, `region = 'ASIA'` reaches five nations, which reach a fifth of `customer`, which reach a fifth of `orders`, which reach a fifth of `lineitem`, and it is the query `../../graph/05-execution.md` section 5.5 uses as its worked example, because the chain is four links long and every one of them is a foreign key with a built link.

These six are where the whole argument of `../../graph/` gets tested. If exact-bitmap reduction is worth what section 5.4 of that document claims, it shows here first and largest. If it is not, these are the queries where a Bloom filter was already enough and the extra machinery bought nothing, which is the outcome `../../graph/11-open-questions.md` section 11.1 is written for.

Q8 is the deepest at eight tables and seven edges, and it is also the one most sensitive to join order, because a bad order on eight tables is a much worse plan than a bad order on three. Since rudb has no join reordering at all, Q8 is the query that measures how much reduction substitutes for it, which is the claim `../../graph/06-the-optimizer.md` section 6.6 makes.

## 3.4 Q9, which is the one that breaks engines

Six tables, five edges, and no selective filter: `p_name LIKE '%green%'` matches about a fifteenth of parts and nothing else is restricted. The intermediate is large by construction and the result is small, which is the exact shape that punishes an engine for materializing.

`../../engine/08-join.md` section 8.4 already names it: getting the build side backwards on Q9 is the difference between a fast query and a query that runs out of memory. Three things in this specification are aimed at it. The link join has no build side at all. The expanded vector body of `../../graph/08-vector-engine.md` section 8.3 keeps the `nation` and `orders` values unreplicated across six hundred million rows. And the memory accounting has to be honest, which issue #735 says it currently is not by a factor of about three.

Q9 is therefore the query to report peak RSS on most prominently, and the one where a time achieved by using three times the memory should not be accepted.

## 3.5 The subqueries

Eight queries have one: Q2, Q11, Q15, Q16, Q17, Q18, Q20, Q22. Four are correlated: Q2, Q17, Q20, Q22.

`crates/rudb-opt/src/unnest.rs` is 1240 lines and this is what it is for. Unnesting turns a correlated subquery into a join, which means these eight queries do not merely *use* the join path, they generate join shapes the query text does not contain, and a bug in unnesting shows up as a wrong answer rather than a slow one. Document 04's correctness protocol matters more for these eight than for any others.

Two deserve individual notes. **Q16**'s `NOT IN` over a subquery is the classic wrong-answer case in every database ever written, because a null anywhere in the subquery's result makes the whole thing null, and `../../engine/08-join.md` section 8.3 already says it gets its own test suite checked against DuckDB. TPC-H's data has no nulls, so Q16 will pass while the engine is still wrong; the test for that lives in the SQL logic corpus and not here. **Q20**'s three levels of nesting with a correlated aggregate at the bottom is the hardest unnesting problem in the suite and is the one most likely to produce a plan the optimizer cannot improve.

## 3.6 The self-joins

Q21 joins `lineitem` to itself twice, once through an `EXISTS` and once through a `NOT EXISTS`, over the same `l_orderkey`. It is the only query in the suite with anything resembling a cycle, and it is the closest thing TPC-H has to the workload `../../graph/05-execution.md` section 5.8's multiway intersect is for.

It is also the query where the backward adjacency earns its place: "another line item on the same order, with a different supplier" is literally a backward traversal from `orders` to `lineitem`, and expressing it as a CSR list walk over the few orders that survive the other predicates is a different algorithm from the semi-join the planner would otherwise produce.

## 3.7 The tie hazards

Q2, Q3, Q10, Q18 and Q21 have a `LIMIT` with an `ORDER BY` that does not fully determine the order. Two engines can both be right and return different rows. Document 04 section 4.4 is the comparison rule; it is flagged here because the natural reaction to a Q2 mismatch is to look for a bug in the join, and the bug is usually in the comparison.

## 3.8 What the suite does not cover

No updates, TPC-H's refresh functions RF1 and RF2 are part of the specification's throughput test and are not part of the power test this suite runs, and rudb should not claim a TPC-H result of the audited kind in any case. No `NULL` handling of consequence, since the data has none. No string-heavy work beyond `LIKE`. No high-cardinality grouping, which is ClickBench's job. No cyclic join graphs to speak of, which is JOB's.

That list is the argument for why TPC-H is a gate rather than a finish line, and it is restated in document 08 so that a green TPC-H does not get mistaken for a done engine.
