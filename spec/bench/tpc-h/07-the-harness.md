# 7. The harness

Every change to `rudb-bench`, by file, in an order where each step leaves the tree working and produces something. Nothing here waits on the engine except where it says so.

The crate as it stands is twenty one modules: `suite.rs` declares the suites and the query text, `data.rs` turns a suite into files every engine reads the same way, `engine.rs` is the five engines behind one trait, `answer.rs` decides whether two engines said the same thing, `measure.rs` holds the statistics the reporting rules require, `metrics.rs` reads rudb's own per-operator JSON back out and cross checks it, `report.rs` and `markdown.rs` print a run, `regress.rs` gates a commit, `plans.rs` keeps committed plan baselines, `attribute.rs` runs a suite twice with something turned off, `ledger.rs` records what a layer bought, and `machine.rs`, `memory.rs`, `measure.rs`, `fleet.rs`, `saved.rs`, `sweep.rs`, `kernels.rs` do the rest. Most of what TPC-H needs is already there. What follows is mostly small.

## 7.1 Step one: the suite learns that it has a scale factor

`suite.rs`. `Suite::directory` is the single string `"tpch100"` and `Suite::rows` is `None`, which together mean the harness can address exactly one scale factor and cannot compute a rate for it.

The change is a `scale` on the plan, not a new suite per scale. A suite stays one entry; the directory becomes a function of the suite and the scale, `tpch/sf100`, and `rows` becomes a function of the scale for the suites whose row counts are fixed by a specification, which TPC-H's are (document 02 section 2.1). The comment on `rows` explaining why it is declared rather than counted stays true and gets stronger: the counts come from the specification's table, reviewed, and a corpus whose actual counts disagree is a corpus with a problem the manifest check in step two will catch.

Nothing else in `suite.rs` changes. The twenty two query texts are correct, the per-query shape descriptions are there, the Polars absences are declared, and the tests that assert all of that keep passing.

The `--rows` refusal stays exactly as written. Document 02 section 2.6 is the reasoning and the message should gain one clause pointing at `--scale`, which is the thing the person typing `--rows` actually wanted.

## 7.2 Step two: generation and the manifest

`data.rs`, plus a new `generate` subcommand in `main.rs`.

`rudb-bench generate tpch --scale 100` prints what it will write and how much space it needs, then: runs `dbgen` (or DuckDB's `CALL dbgen` at SF10 and below), converts `.tbl` to Parquet with DuckDB, writes the eight files under `$RUDB_BENCH_DATA/tpch/sf100/`, and writes a manifest beside them.

The manifest is the part worth specifying. It records the generator and its version, the provenance (`dbgen` or `duckdb-tpch`, per document 02 section 2.2), the DuckDB version that did the conversion, the date, the per-table row count and file size and content hash, and the four verified physical properties of document 02 section 2.4, `lineitem` non-decreasing in `l_orderkey`, the observed density of `o_orderkey`, the density of the three identity keys, `partsupp` ordered and its degree. The verification is one pass per table at generation time and its results are facts in the manifest rather than assumptions in a spec.

`prepare` then reads the manifest instead of just stat-ing eight paths, and its failure message says which scale factor is missing and what command makes it. The manifest hash goes into every report header (document 05 section 5.9), which is what makes "the same corpus" a checkable claim rather than a convention.

This step is entirely independent of the engine and can be done first.

## 7.3 Step three: the refusal becomes a timeout

`engine.rs` and `measure.rs`. This is the consequential one.

`Rudb::can_run` stops refusing `tpch`. The refusal's reasoning was right and its shape was wrong: the fact it is protecting against, a nested loop join does not finish, is a per-query fact and it belongs in a per-query result.

So the `Engine::run` path gains a deadline. The trait's contract already says the timing is the caller's, taken around the call, so the caller is also the right place for the deadline. The three process-spawning engines run their child with a wait-with-deadline instead of `output()`, and on expiry the child is killed, its group is cleaned up, and the call returns a timeout rather than an error. The two in-process paths need a cancellation the same way; rudb has one already, since `Join` holds a `cancel`.

`Ran` gains an outcome that is one of completed, timed out with the limit, failed with a message, or refused. That is a wider change than adding a `bool` and it is the right one, because four states that print differently and aggregate differently should not be three states and a sentinel.

`measure.rs` then enforces what document 05 section 5.6 says: a distribution containing a timeout is not publishable, the same way a distribution of fewer than five samples is already not publishable. `Distribution::publishable` is the existing mechanism and it already has exactly the right shape for this.

The default timeout is five minutes at SF10, scaled by the scale factor, and `--timeout` overrides it. It is printed in the report header whether or not it fired.

**After this step, `rudb-bench tpch --scale 1` produces a table.** Twenty rows of it say `timeout` and two say a number. That table is the deliverable of document 08's first gate and the fixed point every later run is measured against.

## 7.4 Step four: the join counters

`metrics.rs`, and the engine side of it in rudb.

`metrics.rs` already reads `rudb --metrics FILE`, one JSON document per statement, with what each operator consumed, produced and spent, and already cross checks it rather than trusting it, which is the discipline that makes the per-operator table believable. TPC-H needs the join operator to write more fields into that document: build rows and bytes, probe rows, output rows, the algorithm chosen, and the reasons the others were not (document 05 section 5.4). The reduction counters and the bitmap and zone-map counters of section 5.5 are more of the same JSON.

The harness side is the derived number: the sum of intermediate cardinalities over the result cardinality, computed from the operator tree the metrics file already describes, per query. It is one traversal and it is the most valuable column in the report.

The cross check extends with it. An operator tree whose child output rows do not match its parent's input rows is a bug in the instrumentation, and finding that in the harness rather than in a chart is the whole reason `metrics.rs` verifies instead of transcribing.

## 7.5 Step five: the report

`report.rs` and `markdown.rs`.

Both already do the hard parts, cold beside hot, load and on-disk size beside every runtime, peak resident memory as a column, the machine recorded, the full suite including the losses, and no mode that prints only a total. TPC-H adds columns rather than structure: the answer status from document 04, the intermediate-over-output ratio, the timeout, the scale factor and manifest in the header, the memory limit and thread count, and for rudb the list of graph sections that existed.

Two rules need code rather than a column. A total over fewer than twenty two completed correct queries prints as incomplete with the count, and no ratio is derived from it (documents 04 section 4.6 and 05 section 5.6). And the maximum per-query ratio prints next to the geometric mean, always, because `suite.rs` already argues for that on JOB and the argument is not suite-specific.

`markdown.rs` is where the artifact that survives the week lives, so the join counters and the reduction counters go there in full and the terminal table keeps only the ratio.

## 7.6 Step six: the answer comparison

`answer.rs`. It already parses rather than diffs, already handles the `avg` over `DECIMAL(15,2)` that three engines render three ways, and already carries three real SF100 q01 answers as fixtures.

What it needs is document 04 section 4.4: the ordered comparison, then the multiset fallback recorded as `tied`, then the boundary check that needs the query re-run without its `LIMIT`. The third step is the only one that touches anything outside this module, because it needs to ask the harness to run a modified query, and it must only ever happen after the first two have failed.

It also needs the specification's SF1 qualification answers as fixtures, which is a data entry job of twenty two small tables and is the only third-party correctness reference in this directory.

## 7.7 Step seven: the differential, which is already built

`attribute.rs` runs the same suite, the same data, the same machine, the same process, twice, once as the engine comes and once with every optimizer turned off, and reports the difference per query rather than as a total. That apparatus is exactly what `../../graph/09-measurement.md` section 9.2 asks for, with `graph_sections = off` in place of the optimizer switch.

The same apparatus takes a third switch, `statistics = off`, from `../../stats/09-measurement.md` section 9.3, and it should be one implementation with three switches rather than three implementations of one idea.

So the differential is a new switch in an existing command, not a new command, and the argument in that module's own doc comment, that an optimizer which makes one query a hundred times faster and forty two slightly slower has a good total and is a bad optimizer, transfers to the graph layer word for word.

The correctness half of the differential, which is that both runs must produce the same answers, is `answer.rs` comparing two rudb runs to each other rather than to DuckDB. That is a comparison the module can already make; nothing about it is new except who is on each side.

## 7.8 Step eight: the gates

`regress.rs` and `plans.rs`.

`regress.rs` fails a build when a distribution does not overlap and the median moved past a threshold, and reports rather than fails a query too noisy to publish. TPC-H needs two additions in the same spirit: a query that regresses from a number to a timeout is a failure regardless of thresholds, and a query whose answer changed is a failure that is not about time at all.

`plans.rs` keeps committed plan baselines so a plan change is a reviewed diff. This matters more on TPC-H than on ClickBench by a large margin, because there is no join reordering yet, which means when it arrives every one of the twenty queries with a join changes plan on one commit. A reviewed diff of twenty plans is a good afternoon; the same change noticed three weeks later in a performance run is the failure mode that module exists to prevent. The twenty two plans get committed at SF1 as soon as step three makes them producible.

## 7.9 Step nine: the ledger

`ledger.rs` records what each layer bought, with before and after on total time, CPU, peak resident and bytes read, on the same machine, with the comparison engines held fixed. The milestones of `../../graph/10-milestones.md` are layers in exactly that sense, and each one closes with a row. The release note that says the link join made TPC-H faster then points at a row instead of guessing, which is the rule that module already states.

## 7.10 What is not changed

`kernels.rs`, `sweep.rs` and `fleet.rs` need nothing. The seam registry `sweep.rs` drives is where the link-join-versus-hash-join choice would eventually be registered if it becomes a seam rather than a cost decision, and `../../graph/06-the-optimizer.md` says it is a cost decision, so it stays out.

The query text is not touched. Not for rudb, not for any engine, not to help a plan. `suite.rs`'s module comment settles this and it is the rule most likely to be quietly broken by somebody trying to make a number move.
