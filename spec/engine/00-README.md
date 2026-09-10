# Engine: the layered plan

This directory is a second pass over documents 04 through 09 of spec 2140, written after the first six months of building rudb, and written because the way the work was being picked was wrong.

The M2 work list was ordered by how many records in DuckDB's `sqllogictest` corpus each missing feature would unlock. `BoundedListExpression` 2151 records, `SetStatement` 1336, `WithClause` 1244, `ExplainStatement` 1127. That is a real ordering and it does move the published pass rate, and after a few rounds of it you have a database that parses a great deal of SQL and executes all of it through a nested loop join with no filter pushdown. The measurement that made this obvious was accidental: four files in the corpus timed out at ten seconds each, and all four were joins of ten thousand rows against fifty thousand. That is the first honest number rudb has ever produced about its own execution speed, and it was produced by a conformance harness that was not trying to measure speed.

Corpus count is a measure of surface area. It is not a measure of the engine. An engine is built bottom up, one layer at a time, and each layer is finished when it is fast on real data and not when the next layer can be started on top of it. Surface area added on top of a slow layer has to be revisited when the layer underneath is replaced, and revisiting it is more expensive than not having written it.

So the plan changes shape. The work is now organized by layer rather than by feature count, each layer has a benchmark that is run on real data on a real machine before the layer is called done, and the sub-milestones in document 14 are that ordering.

## The order the layers go in

The stack, from the bottom up, with the document that specifies each one.

| | layer | document |
|---|---|---|
| 1 | the data plane: vectors, selection, validity, strings, chunk shape | [03](03-data-plane.md) |
| 2 | expressions: the interpreter, null handling, folding, the compiled path | [04](04-expressions.md) |
| 3 | scan and filter: zone maps, ordering, late materialization, encoded predicates, async I/O | [05](05-scan.md) |
| 4 | the hash table, shared by join and aggregate | [06](06-hash.md) |
| 5 | aggregation | [07](07-aggregate.md) |
| 6 | join | [08](08-join.md) |
| 7 | sort, top-N and window | [09](09-sort-and-window.md) |
| 8 | the scheduler, memory and spilling | [10](10-scheduler.md) |
| 9 | the optimizer above all of it | [11](11-optimizer.md) |
| 10 | adaptivity across all of it | [12](12-adaptivity.md) |

Two documents sit outside the stack. [13](13-measurement.md) is the harness every gate depends on, which is what has to be added to `rudb-bench` and `rudb-compat` before any of the numbers below can be produced. [14](14-plan.md) is the executable plan: thirteen sub-milestones from 2a to 2m, each with its entry state, its test gate, its benchmark gate and its exit criterion, plus the two items that are not layers, which are the write path and the M1 report that has to be finished first.

The order is not arbitrary and it is not the order of a textbook. It is the order in which one layer's performance depends on the one below it being finished.

The data plane is first because every other layer is written against it, and because document 07 already said that changing the vector interface after twenty operators exist is expensive. Expressions are second because a filter is an expression and every scan runs one. Scan is third because on ClickBench, which is the benchmark the target is stated against, most queries are a scan, a filter and an aggregate, so a database that is only good at those three is already competitive on the headline number. The hash table is fourth and it is one layer rather than two, because the join and the aggregate want the same structure and building two of them is how they end up with two different sets of bugs. The scheduler is eighth rather than first, which is the decision most likely to be argued with, and section "Why the scheduler is not first" below is the argument.

## The rule that makes this different from the last plan

**A layer is not done when it works. A layer is done when it is measured on real data against DuckDB and ClickHouse, the number is published in `rudb-bench`, and the number is better.**

That rule has one consequence worth stating separately: the baseline has to exist before layer one starts. There is no way to say a layer improved anything without a number from before it. Document [02](02-baseline.md) is the baseline and it is sub-milestone 2a, ahead of everything else in this directory.

The second consequence is that the benchmark is not TPC-H alone and it is not ClickBench alone. Each layer has a microbenchmark that isolates it, and each layer also has to move at least one whole query on at least one real suite. A layer that makes a microbenchmark five times faster and moves no query is a layer that was not on the critical path, and finding that out at the microbenchmark is cheap.

The third consequence is that conformance does not stop while this happens. `rudb-compat` runs on every commit and the corpus pass rate is published on every run. It is allowed to move slowly during a layer that is pure performance work. It is not allowed to fall, and a wrong answer found by the corpus takes priority over every performance item in this directory, because the whole thing is worthless if it is fast and wrong.

## Why the scheduler is not first

The obvious objection to this ordering is that parallelism is where the factors are, that morsel-driven scheduling touches everything, and that retrofitting it is exactly the kind of expensive revisit this document is written to avoid.

The answer is that the interface has to be right and the implementation does not. The scheduler's contract with the rest of the engine is small: an operator declares whether it is a source, a stateless transform or a sink, a source hands out morsels, a sink has a thread-local state and a merge, and everything is written so that it never assumes it is the only copy of itself. That contract is imposed from layer one and it costs almost nothing to impose. What comes later is the work stealing, the NUMA placement, the async I/O pool, the backpressure and the spilling, and none of that changes an operator that already obeys the contract.

The reason it comes later rather than sooner is that a parallel implementation of a slow operator is a slow operator that is harder to profile. Single threaded per-core throughput is the number that says whether a layer is right, it is the number the Bespoke OLAP ablation in document 00 was measured at, and it is much easier to reason about. Get each layer right at one core, then turn on the cores. An engine that is four times slower than DuckDB per core and hides it behind sixteen threads is an engine that will never reach ten times faster, because the threads are already spent.

The one part of the scheduler that does come early is async I/O, and it comes early because DuckDB v2.0 made it a correctness-of-measurement issue rather than an optimization. Document [05](05-scan.md) covers it in the scan layer, where it belongs, because what it actually changes is how a scan asks for bytes.

## What is deliberately not in this directory

Transactions, MVCC, the WAL and checkpointing. Those are document 11 of the parent spec and they are not on the read path.

The storage format itself. Documents 05 and 06 of the parent spec, and M1, which is finished apart from its report. The engine specs here take the format as given and specify what execution does with it.

The C API, extensions, the wire protocol, the bindings. Document 13 of the parent spec, and M7 and later.

The SQL surface, apart from where a feature forces an engine decision. Window functions force a whole operator and are in document [09](09-sort-and-window.md). `WITH RECURSIVE` forces a loop in the scheduler and is in document [10](10-scheduler.md). `SET` and `PRAGMA` do not force anything and are not here.

## How to read this with the parent spec

The parent spec's documents 04 through 09 are the design. This directory is the implementation plan for the same design, with six months more evidence, the 2026 state of the art re-checked in document [01](01-survey.md), and every decision attached to a measurement that will either confirm it or not.

Where this directory contradicts the parent spec, this directory is newer and this directory wins, and the contradiction is stated in the text rather than left for the reader to find. Three came out of the survey and they are collected at the end of document [01](01-survey.md): async I/O is not optional and not late, the hash table is one structure and not two, and chunk compaction is not a constant.

Four more came out of reading the code while writing the layer documents, and each one is stated where it belongs rather than being collected here. Document [03](03-data-plane.md) section 3.5 closes the string view layout question in favour of block and offset over a pointer, which the parent spec asks for the other way round. Document [04](04-expressions.md) section 4.6 reschedules tier 1 fusion behind six other layers and gives the number that decides it. Document [05](05-scan.md) section 5.2 corrects document [02](02-baseline.md), because rudb turns out to have no path from a file on disk into a chunk at all, which changes what the baseline can measure. Document [11](11-optimizer.md) section 11.6 rejects a Cascades optimizer rather than deferring it.

Written 10 September 2026, against DuckDB `v2.0.0-alpha39998`, ClickHouse 25.x, Polars with the rewritten streaming engine, and DataFusion 5x.
