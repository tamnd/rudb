# ClickBench

The first benchmark the project is judged on, and the one where most of this folder is worth nothing. This document says which passes matter on it, which do not, and what the planner's honest share of the axis-1 number is.

Writing it this way round is deliberate. A folder about the planner has an obvious incentive to claim the planner wins the headline benchmark, and on this benchmark it mostly does not.

## 11.1 What the workload actually is

43 queries, all over one table, `hits`, with 105 columns. **There is not a single join in the suite.** Reading the shapes out of `tamnd/rudb-bench`'s `src/suite.rs`, which is the definition this project measures against:

- 22 are some form of `GROUP BY`, most with `ORDER BY ... LIMIT`
- 5 are bare counts or global aggregates
- 4 are top-k
- 7 (q37 through q43) share one predicate shape: `CounterID = 62 AND EventDate >= '2013-07-01' AND EventDate <= '2013-07-31' AND IsRefresh = 0` plus one or two more conjuncts
- the remainder are `LIKE`/substring scans, a regular-expression group-by, a `SELECT *` top-k, and q30, which is ninety `SUM(ResolutionWidth + N)` aggregates over one column

The baseline the project recorded at layer 2a on `gamingpc-wsl`, total hot time over the suite: DataFusion 55.0.0 at 23.72 s, ClickHouse 26.9 local at 25.62 s, DuckDB v1.5.5 at 25.93 s, Polars 1.44.2 at 25.95 s, and ClickHouse in server mode at 10.33 s. CPU seconds tell a different and more interesting story than wall clock, DuckDB 313.6, ClickHouse-local 393.5, Polars 507.3, DataFusion 555.3, and CPU seconds is the axis-3 measure.

rudb abstains on that run, because at 2a there was no path from a file on disk into a chunk.

## 11.2 What the planner is worth here

Five passes matter. The rest are worth zero by construction.

**Projection pushdown. Decisive.** 105 columns, and the median query reads two or three. `spec/09-optimizer.md` prices the gap at 20 GB against 200 MB once the scan layer is real. Today, with `crates/rudb-bind/src/binder.rs` handing every `Node::Get` the whole column list, it is the difference between copying 105 columns per chunk and copying three. This is the single highest-value pass on this benchmark and it is also the easiest one in the folder, which is why document 04 makes it the first pull request.

The exception is q24, `SELECT * FROM hits WHERE URL LIKE '%google%' ORDER BY EventTime LIMIT 10`, which genuinely wants all 105 columns, but only for ten rows, which is section 11.3's point.

**Filter pushdown into the scan. Large, and mostly not this folder's win.** Pushing `EventDate >= '2013-07-01' AND EventDate <= '2013-07-31'` to the scan turns it into a zone-map check that skips whole blocks. The pass that moves the predicate is three days of work; the layer that makes the skip happen is 2e. The planner's job here is to deliver the predicate in a shape the scan can recognize, which is what document 04 section 04.1's comparison normalization exists for.

**Top-N instead of sort-then-limit. Large, and cheap.** At least four queries are explicitly top-k and most of the 22 group-bys end in `ORDER BY ... LIMIT 10`. Sorting a full group-by result to return ten rows is the obvious waste, and a bounded heap is a well-understood operator. Two queries, q39 and q42, have `OFFSET`, one of them deep, so the top-N must take `limit + offset` and not `limit`. That is exactly the kind of off-by-one that returns a wrong answer quietly, so it goes in the corpus.

**Common subexpression elimination, for q30 specifically.** Ninety `SUM(ResolutionWidth + N)` aggregates over one column. Every one of them re-reads and re-adds. CSE on the column read is the obvious part.

The non-obvious part is that `SUM(w + N)` is algebraically `SUM(w) + N * COUNT(*)`, which collapses ninety aggregate accumulators into two. That rewrite is worth an order of magnitude on this query and it is **the most dangerous rewrite in this document**, because it changes overflow behaviour and it changes what happens when `w` is null in a way that depends on DuckDB's exact `SUM` semantics. Do not ship it on the strength of the algebra. Ship it when the corpus says DuckDB agrees, with the optimizer-on/off differential of document 12 covering it, and be prepared for the answer to be no.

**Aggregate rewrites for `COUNT(DISTINCT x)`.** Several queries use it, including on `UserID`, which is high cardinality. This is a two-phase aggregate question and an operator question more than a planner one; the planner's part is recognizing the shape.

## 11.3 What is worth nothing

Stated explicitly, because each of these is a document in this folder and each costs real time.

**Join ordering. Zero.** No joins. Document 07 does not move this benchmark by one microsecond.

**Subquery unnesting. Zero.** No subqueries in the suite.

**Predicate transfer. Zero, and it must cost zero.** Document 08's two-equality-edge minimum guarantees that by construction, and this is precisely why that escape hatch is built first rather than last.

**Runtime join filters. Zero.** No joins to build them from. Document 09's value is entirely on the other suites.

**Cardinality estimation. Nearly zero.** With one table and no join, there is no plan choice that depends on an estimate, except the group-by's hash-table sizing, which the operator does better from the data than the planner does from a sketch. Document 06 earns itself on JOB, CEB and TPC-DS, not here.

One qualification worth keeping: the 2026 contrarian thread, Datta and Rusu, arXiv:2311.17293, observes that main-memory analytical systems with limited estimation remain competitive, and ClickBench is exactly the workload that observation was made on. It is not evidence that estimation does not matter. It is evidence that this benchmark cannot see whether it does.

## 11.4 The honest split

The axis-1 target is 2x faster than any rival on ClickBench, against a field whose best embedded hot total is DataFusion's 23.72 s.

Where that factor comes from, in the project's own accounting:

- **2d, storage.** `spec/05-storage.md` is direct about this: DuckDB's format stores this dataset in 20.46 GB, Umbra in 8.30, and rudb's axis-4 target is 2.05. A format that reads a quarter of the bytes is most of the way to a query that takes a quarter of the time on a scan-bound workload, and ClickBench is a scan-bound workload.
- **2e, the scan.** Zone maps, late materialization, dictionary codes held as codes. Document 10 section 10.3's layout adaptation is the planner's contribution to this and it is the one distinctive pass in the folder, but it annotates a scan that does not exist yet.
- **2f, the operators.** Hash aggregate quality is what 22 of the 43 queries measure.
- **This folder.** Projection pushdown, predicate delivery, top-N, and q30. Real, necessary, and not the factor of two.

**So the honest statement is: the planner is a prerequisite for the ClickBench number and not the source of it.** Every pass in section 11.2 has to exist before the storage and scan work can be measured properly, because a scan that is handed 105 columns cannot demonstrate that it reads three. That is a real argument for doing this work now, and it is a different argument from "this is where the speed comes from."

Document 00 makes the same point about the folder as a whole, and it is worth having stated twice with the numbers attached the second time.

## 11.5 What to measure

`rudb-bench` already has the mechanism and the discipline; this is what this folder asks of it.

**Per-query, not per-suite.** `baselines/<suite>.txt` holds three quartiles per query specifically because the regression gate asks whether one query got twice as slow. The optimizer is the layer most likely to make one query much worse while improving the total, so this is the gate that matters for this folder.

**Optimizer on against optimizer off, per query, on the whole suite.** Not for correctness, which document 12 covers on the corpus, but for attribution: the ledger in `spec/engine/02-baseline.md` section 2.8 asks what each layer bought, and this folder should be able to answer with a number per pass rather than a claim.

**Planning time as its own column.** 43 queries against one table plan in microseconds and should be asserted to. If ClickBench planning time is ever visible in the totals, something in document 07's budget is wrong.

**Report the tail.** A 2x mean with one query regressed is a failure of `spec/02-the-goal.md`'s second axis, and this suite is where that will first be visible.

## What we should take from this document

ClickBench is 43 single-table queries over a 105-column table with no joins, no subqueries, and 22 group-bys.

Five passes matter: projection pushdown (decisive, and the easiest in the folder), filter delivery to the scan, top-N with `limit + offset` handled correctly, CSE on q30, and `COUNT(DISTINCT)` shape recognition.

Five documents are worth zero here: join ordering, unnesting, predicate transfer, runtime join filters and cardinality estimation. Predicate transfer must also *cost* zero, which its two-edge minimum guarantees.

The `SUM(w + N)` to `SUM(w) + N * COUNT(*)` rewrite is worth an order of magnitude on q30 and is the most likely thing in this document to change an answer. The corpus decides, not the algebra.

The factor of two on axis 1 comes from 2d and 2e. The planner is a prerequisite for measuring it, not the source of it, and saying so is the only way the attribution ledger stays worth anything.
