# 9. Measurement

Nothing in this directory is true until `rudb-bench` says so. This document specifies what gets measured, against what, and which ablations have to be run before any number from this layer is allowed out of the repository. The TPC-H half of the apparatus is `../bench/tpc-h/`, which is written to be usable before any of this exists.

## 9.1 The claims, stated so they can fail

Five, each with the measurement that kills it.

**C1. The links fit.** Total graph section bytes for TPC-H at SF100 are under ten percent of the native file's column bytes, with `lineitem → orders` and `partsupp → part` in the monotone form. Killed by measuring the file.

**C2. The exact reduction beats the Bloom filter.** On TPC-H SF100, full reduction with `Rids` bitmaps removes more rows and costs less wall time than probe-side Bloom filtering on the same plans. Killed by running both, which requires that the Bloom filter of `../engine/08-join.md` section 8.5 be built first, and that is why it is not optional in document 10.

**C3. The link join beats the hash join where the planner says it should.** Per query, on the join it was chosen for, with the hash join forced as the control. Killed per query, and a query where it loses is a planner rule to fix rather than a claim to withdraw, unless it loses everywhere.

**C4. Factorization pays.** The expanded body against the flattened one on Q9, Q10, Q18 and Q21. FFX reports a mean 2.08x in a comparable engine; anything under 1.2x here means rudb's flat path was already doing something the paper's baseline was not, and that is a finding worth having.

**C5. The whole thing reaches the goal.** Ten times DuckDB on TPC-H SF100 total runtime, per `../02-the-goal.md`. This is the only claim anyone outside the project cares about and it is the last one that can be evaluated.

## 9.2 The ablation that runs on every commit

`graph_sections = off` is a setting, and the suite runs both ways. With the sections off, every query takes the hash join, the nested loop and the ordinary scan; with them on, the planner may take any path it likes. **The two runs must produce identical answers, byte for byte, on every query of every suite.**

That is the differential test for this entire layer and it is nearly free, because the reference implementation is the engine with a flag flipped rather than a second system. It catches the failure mode document 05 section 5.1 warns about, a `rid` used after an operator invalidated it, which is the only way this layer produces a wrong answer, and which no unit test will find because it requires a plan shape rather than a value.

It also keeps the fallback path alive. A fallback that is only taken in production is a fallback that is broken in production.

## 9.3 The link join against the hash join

A microbenchmark and then the suite, in that order.

The microbenchmark sweeps four axes and reports the crossover surface: parent row count from ten thousand to a hundred and fifty million, projected parent width from four to two hundred and fifty six bytes, child clustering from perfectly ordered to uniformly random, and page cache state from warm to dropped. The output is the two default thresholds document 06 section 6.4 needs, measured rather than chosen, and a table a reader can check the planner's rule against.

The thing this is looking for is the case the design is most likely to be wrong about, which is a random gather into a parent larger than memory. The hash join reads the parent once, sequentially. The link join reads it in random order, and if it does not fit, every gather is a page fault. The measurement either finds that the planner rule avoids that case or it finds that the rule is wrong.

## 9.4 The reduction, which is the claim most likely to fail

Three runs of each query: no reduction, Bloom-filter reduction, exact-bitmap reduction. Per query, report the rows removed at each scan, the passes taken, the wall time of the reduction phase separately from the join phase, and the last-level cache miss rate of the reduction phase.

The last of those is not decoration. Document 05 section 5.4 records the risk in full: a bit test into an 18 MB bitmap is a cache miss, and six hundred million of them may cost more than the join they saved. The counter that decides is misses per row during the push, and the three mitigations, `rid` order, zone-map part skips, sparse form, each have a measurable effect that has to be attributed separately or the result is unreadable.

A negative result here is survivable and should be planned for. If the exact bitmap loses to the Bloom filter on cache behaviour, the fallback is a hybrid: a Bloom filter for the first pass, which is small and cache resident, and the exact bitmap only where the surviving set is small enough for the sparse form. That is a worse design and it is still better than the state of the art, and knowing which one is true is the point of measuring.

## 9.5 The space decision

`lineitem → part` costs 1.88 GB at SF100 and is not monotone. Run the suite with it and without it. If the queries it accelerates, Q14, Q17, Q19 and Q20 are the candidates, do not gain more than the load cost and the space, it does not get built by default and the budget rule of document 03 section 3.7 is what expresses that.

This measurement is the template for every future relationship: a link is not built because it exists, it is built because the suite got faster.

## 9.6 The result this has to beat

GRainDB put predefined joins into DuckDB and reported a large win on LDBC SNB and a small one on TPC-H, saying plainly that TPC-H lacks selective many-to-many joins. That is the closest published prediction of what this layer is worth on the workload rudb's goal names first, and it predicts a small number.

rudb's answer is that GRainDB used the RID index mostly to generate a semi-join filter on one edge, and that the mechanism here is full reduction across the whole join graph with exact membership, which is a different thing. That answer is an argument. Document 09's job is to replace it with a measurement, and the measurement to run first is the one that isolates it: TPC-H SF100 with links and single-edge semi-join filters only, against TPC-H SF100 with links and full reduction. If those two numbers are the same, the argument was wrong and GRainDB's prediction was right.

## 9.7 Beyond TPC-H

TPC-H's join graph is shallow and its joins are almost all primary to foreign key with high match rates, which is the workload this layer looks *worst* on, per the note above. The suites where it should look best are the ones `../15-rudb-bench.md` already lists: JOB and CEB, where the join graphs are deep and the filters are selective and where Parachute measured 1.54x from a much weaker version of the same idea, and LDBC SNB, which is not currently one of rudb's suites and which document 10 adds at G6 because it is where the many-to-many traversals live.

Adding LDBC SNB is a change to `rudb-bench` with a real cost and it should be justified by this layer rather than by general interest. The justification is that a design derived from graph systems that is never measured on a graph workload cannot tell whether it inherited the wins or only the complexity.

## 9.8 Reporting rules

The seven rules in `../15-rudb-bench.md` apply unchanged. Three additions specific to this layer.

A time that includes the benefit of a section must report the cost of building that section, in the same table, as a load-time column. An index that takes four minutes to build and saves two seconds per query is a different product from one that takes four seconds, and a table that shows only the query time hides which one it is.

A run with sections must state which sections existed, by name, from `rudb_links()`. "rudb with the graph layer" is not a description of a configuration.

A per-query ratio against DuckDB must be reported for both the sections-on and the sections-off configuration. The floor from `../02-the-goal.md` applies to both, because a user whose data has no declared keys gets the second one.

## 9.9 What has been measured

One subsection per claim that has a number against it. A claim with no number here has not been measured, which is a different state from measured and passing, and the difference is the whole point of writing the claims down first.

### C1, the links fit. Measured at SF10, projected to SF100, and it holds because the budget turns two links away

Reproduced with `cargo xtask graph`, which is the build and not a simulation of it, over a native TPC-H SF10 file loaded from the reference Parquet. Eight key maps and then nine relationships, which is every primary to foreign key edge in the schema that a single column expresses.

| relationship | children | form | bytes | share of child table | kept |
| --- | ---: | --- | ---: | ---: | --- |
| `lineitem -> orders` | 59,986,052 | monotone | 9,739,468 | 0.57% | yes |
| `lineitem -> part` | 59,986,052 | packed | 158,400,739 | 9.28% | no, over budget |
| `lineitem -> supplier` | 59,986,052 | packed | 128,407,713 | 7.52% | yes |
| `orders -> customer` | 15,000,000 | packed | 39,609,440 | 9.26% | no, over budget |
| `partsupp -> part` | 8,000,000 | monotone | 1,298,888 | 0.36% | yes |
| `partsupp -> supplier` | 8,000,000 | packed | 17,125,064 | 4.73% | yes |
| `customer -> nation` | 1,500,000 | packed | 960,996 | 0.93% | yes |
| `supplier -> nation` | 100,000 | packed | 64,124 | 0.98% | yes |
| `nation -> region` | 25 | packed | 82 | 2.86% | yes |

Every child row found a parent in all nine, so every one of them is *exactly one* and not merely *at most one*. The two relationships C1 names, `lineitem -> orders` and `partsupp -> part`, are the two that came back monotone, and between them they cost 11.0 MB against the 2.66 GB of column bytes in the file, which is 0.42 percent. Key maps cost 7,793,220 bytes more, almost all of it the dense map over `o_orderkey`. Graph sections in the file total 165,389,555 bytes against 2,659,368,507 column bytes, which is 6.22 percent.

The number that matters more is the one underneath it. Had nothing been turned away the total would have been 13.66 percent, so C1 is not a claim the design satisfies by being small, it is a claim the budget of document 03 section 3.7 enforces by refusing two links. `lineitem -> part` is the one section 9.5 predicted would be the expensive one and it is, at 158 MB for one column of one table. `orders -> customer` fits under ten percent on its own and does not fit beside the dense key map already on `orders`, which is the budget behaving as specified: it is a share of a table, not of a section.

Projected to SF100 by scaling row counts and recomputing each packed width from the parent's row count, holding the measured 3.91 percent rank and select overhead of the monotone form: 1.898 GB of graph sections against 26.60 GB of column bytes, which is 7.14 percent, with the same two relationships monotone and the same two turned away. The projection puts `lineitem -> part` at 1.88 GB, which is the figure document 03 section 3.4 arrived at independently, so the two agree.

C1 is therefore met at SF10 on a real file and met in projection at SF100, and the SF100 run itself is owed on a machine with the disk for it. What the projection cannot settle is build time at scale: the nine links took 3.11 seconds in total at SF10, and whether that stays linear is a measurement and not an argument.
