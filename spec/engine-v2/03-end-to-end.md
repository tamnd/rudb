# End to end, from scratch

The first thing this engine does is run a query badly, in public, with the receipts.

## 1. What "end to end" means concretely

One command, six stages, and every stage produces an artefact a human or a harness can read.

```
$ rudb --explain-analyze --metrics run.json \
    "SELECT UserID, COUNT(*) AS c FROM 'hits.parquet'
     GROUP BY UserID ORDER BY c DESC LIMIT 10"
```

1. **Parse.** SQL text to an AST. `rudb-parse` already does this; 16,736 lines, DuckDB's grammar.
2. **Bind.** AST to a logical plan with resolved names, types and function overloads. `rudb-bind` already does this, including `FROM 'hits.parquet'` as a table.
3. **Optimise.** Logical plan to logical plan, through a named, ordered, individually switchable list of passes. At F0 the list is short and every entry is off by default.
4. **Plan physically.** Logical plan to a pipeline graph: sources, streaming operators, sinks, exchanges, and the strategy chosen at every seam.
5. **Execute.** The scheduler runs the pipeline graph over morsels.
6. **Report.** A metrics document, a rendered `EXPLAIN ANALYZE`, and the rows.

The two things that make this end-to-end rather than a demo are that stage 4 exists as a distinct artefact you can print, and that stage 6 is a file with a schema rather than a log line.

## 2. What F0 ships

F0 is allowed to be slow. It is not allowed to be partial. The list below is the whole of it.

**Read a file.** Parquet only, local filesystem only, through the async I/O interface from [`08-execution.md`](08-execution.md) section 7 even though F0's implementation submits one request and waits. `rudb-parquet` is already 5,867 lines with a Thrift decoder, page decoding, hybrid RLE/bit-packing and a metadata reader; F0's work is to turn that into a `Source` that hands out row groups as morsels, and to make the catalog resolve a path to a table without materialising it. CSV comes along free because `rudb-csv` exists; it is not on the critical path and it is how the smoke suite gets fast test data.

**Plan and show the plan.** `EXPLAIN` prints the physical plan: the pipeline decomposition, the operator at each node, the estimated cardinality, the strategy chosen at every seam, and a marker on every node running a reference implementation. That marker is the discipline from Polars' red nodes and it is the single most useful line of output the engine produces in its first year, because for most of F0 through F6 most nodes are red.

**Execute in one thread, through the parallel interface.** The three traits in [`08-execution.md`](08-execution.md) are final at F0. The scheduler behind them is a `for` loop. Nothing above the scheduler knows the difference, which is the point.

**Answer correctly.** All 43 ClickBench queries and all 22 TPC-H queries return what DuckDB returns, modulo the ordering and float rules in [`15-testing.md`](15-testing.md). The operators that make that true are the slow ones: `HashMap<Key, usize>` grouping, nested loop join with a hash side-index only where the nested loop would not finish, `sort_by` over comparators, decode-everything scans. Most of that code already exists in `rudb-exec`, which is 6,714 lines, and F0's job is to put it behind the new traits rather than to write it.

**Measure itself.** The metrics document from [`14-metrics.md`](14-metrics.md), complete, on every run. Per-operator time, CPU time, rows in and out, bytes read, allocations, spill bytes, and the seam-strategy map.

**Be on the board.** `rudb-bench`'s `Engine` trait gets a `Rudb` implementation that returns `can_run() == true`, driven as a subprocess through `rudb-cli` exactly as v1's measurement document specifies. The ClickBench and TPC-H suites run with six engines instead of five. The rudb column is last by a wide margin. It is published anyway, as the baseline every later number is a ratio against, this is the number v1's plan does not have, and the reason its layer gates are ratios against an absence.

## 3. What F0 deliberately does not ship

No parallelism beyond the interface. No spilling; F0 runs out of memory on TPC-H SF100 on `server1` and that is a recorded fact rather than a bug. No optimizer passes on by default. No encoded execution; every column is decoded to a flat form at the scan. No native storage format; F0 reads Parquet and writes nothing. No window functions, no recursive CTEs, no transactions.

Every one of those is a milestone with a number attached. The point of listing them here is that F0's honesty depends on the list being written down before the measurement rather than after it.

## 4. The command line and the settings surface

Every knob in this design is reachable three ways, and the three agree because they are one mechanism.

```
$ rudb --set hash.table=open-addressing-salt ...     # process flag
> SET hash.table = 'open-addressing-salt';           # session setting
> SELECT /*+ hash.table(linear-chained) */ ...       # per-query hint
```

Session settings are DuckDB's surface and compatibility requires them. The flag is what a benchmark script uses. The hint is what a researcher uses to A/B one query without touching the rest of a suite, and it is also what the adaptive policy at F10 writes into the plan when it has decided something, so that `EXPLAIN` of an adaptive run is a document you can replay deterministically.

`SELECT * FROM rudb_strategies()` lists every seam, every registered implementation, which one is the reference, which one is the current default, and where the default came from. `SELECT * FROM rudb_metrics()` returns the last query's metrics as a table, so the whole apparatus is queryable by the engine itself.

## 5. The benchmark loop

The loop the user asked for, written out as the sequence it actually runs in.

``` sql → plan → EXPLAIN → run → metrics → ledger → diff against the previous commit
```

`rudb-bench` owns the right-hand half and already owns most of it: `Suite`, `Distribution` with quartiles and a `publishable` predicate, `memory.rs` for peak RSS, `fleet.rs` with `Role::may_publish` false for every machine the project owns. What F0 adds on that side is three things.

**Ingest the metrics document.** A run record stops being wall clock plus RSS and becomes wall clock plus RSS plus the whole internal breakdown. The cross-check from principle 8 runs here: internal CPU summed over pipelines against the CPU the engine measured around running them, and a discrepancy over five per cent marks the run unusable. A pipeline reports what its driver charged rather than what its operators did, and the CPU that went on building the tree is off the right hand side. See section 3 of 14-metrics.md for why it is not the external process CPU, and for why the check is close to an identity until the engine runs pipelines in parallel.

**The variant sweep.** `rudb-bench sweep --seam hash.table --suite clickbench` runs the suite once per registered implementation of one seam, holding everything else fixed, and emits a table. This is the thing that makes principle 2 worth its cost, and it is four hundred lines because the registry already knows the list. The sweep is what a researcher runs after implementing a paper.

**The ledger.** One row per milestone per suite per machine, with the mechanism switched off and switched on, generated from committed runs. v1's measurement document designed this; v2 adds the column for the switched-off number, because the F-milestone rule in [`00-README.md`](00-README.md) requires it.

## 6. Continuous integration

The smoke suite runs on every commit, on `laptop`, which is the fast-loop machine. It is not a benchmark and `rudb-bench`'s README already says so. Its job is to fail when the measurement path rots, which is a failure mode that costs a week if it is found on the day somebody needs a headline.

The regression gate fires when two distributions do not overlap, which is v1's rule and is the right one for a ten-core laptop with a browser open. The correctness gate is not statistical: the differential oracle in [`15-testing.md`](15-testing.md) runs the whole corpus against every registered strategy combination that the sweep matrix marks as cheap, and any disagreement fails the build.

Weekly, on `server3`, the full ClickBench and TPC-H SF100 runs land in the ledger with the commit they came from.

## 7. Why this ordering is not more expensive than the other one

The obvious objection to F0 is that it writes code to delete. It does: the F0 aggregate, the F0 join, the F0 sort, roughly two thousand lines, all replaced by F5, F6 and F9.

Three answers. Most of that code already exists in `rudb-exec` today and F0 is rehousing it, not writing it. The deleted code does not disappear, it becomes the reference implementation that principle 3 keeps forever, so the F0 `HashMap` aggregate is the oracle that proves the F5 unchained one correct. And the alternative ordering spends the same two thousand lines on scaffolding, mock sources and hand-written drivers that exist only to exercise a layer whose consumer is not written yet, and that code really is deleted.

The cost that is real is calendar: F0 is perhaps six to eight weeks of work that produces the worst number on the board. The benefit is that every number after it is a ratio against something the engine actually did.
