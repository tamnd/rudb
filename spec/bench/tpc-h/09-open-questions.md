# 9. Open questions

Six. Each has what would settle it and what happens in the meantime, because an open question without a fallback is a decision deferred onto whoever hits it first.

## 9.1 Which scale factor is the headline

The goal document says SF100 and this directory has taken that as given. But SF100 on a laptop is a different benchmark from SF100 on a machine with three hundred gigabytes of memory, on the second, every table is in page cache after the first run and the suite measures the executor; on the first it measures the storage path too. SF10 is fully cached almost anywhere, and SF1000 measures spilling on everything.

There is a real possibility that the honest headline is SF10 on a stated machine, with SF100 as the scaling check, because SF10 is the one where the result is about the engine rather than about the memory the reviewer's machine happened to have.

**Settled by:** running both on two machine classes once SF100 runs at all, and seeing whether the ratios agree. If they do, the question is moot and SF100 stands.

**Meanwhile:** SF100 is the headline because the goal document says so, and every report carries the machine and the page-cache state per `machine.rs` so a reader can tell which benchmark they are looking at.

## 9.2 Whether the memory limit should be binding

Document 05 section 5.7 requires a memory limit be set and reported, and issue #735 says rudb's own limit is not currently a limit. The question underneath is what a published run should do: run every engine under a stated limit and record failures, or run every engine unconstrained and record peaks.

The first is more informative and is how a user would deploy. The second is what almost every published chart does, which matters only for the comparability this directory has already said it cannot have (document 06 section 6.1).

**Settled by:** Q9 at SF100. If DuckDB and rudb both fit comfortably under a natural limit, the question is academic; if either spills under it, the limit is the interesting variable and should be binding.

**Meanwhile:** unconstrained, with peak RSS reported, and the limit reported as unlimited, which is itself a statement.

## 9.3 Whether the substitution parameters should vary

TPC-H defines substitution parameters and the specification's qualification run fixes them. Every published chart uses the fixed ones, and so does `suite.rs`'s query text, character for character from DuckDB's copy.

Fixed parameters are reproducible and they are also a target. An engine tuned, even unconsciously, through weeks of looking at the same plans, to `region = 'ASIA'` and the specific date ranges in Q3 and Q6 is an engine whose TPC-H number does not generalise. Varying them is what the specification's own throughput test does.

**Settled by:** running the suite once with a randomised parameter set drawn per the specification's rules and seeing whether any ratio moves materially. If one does, that query's result was about the parameter.

**Meanwhile:** fixed parameters, because comparability against our own dated series is worth more right now than robustness against a threat that has not been observed.

## 9.4 Whether to add the refresh functions

RF1 and RF2 insert and delete orders and line items, and they are the only part of TPC-H that tests the write path. Document 03 section 3.8 excludes them.

They are also the only cheap test of what `../../graph/07-maintenance.md` specifies, a link section that goes partial on append and stale on delete, and a query that has to be correct against both. That is a substantial amount of specified behaviour with no workload pointed at it in this directory.

**Settled by:** whether a maintenance bug shows up somewhere else first. If `../../graph/07-maintenance.md`'s section states get a bug that the unit tests did not catch, RF1 and RF2 arrive the same week.

**Meanwhile:** not run, not claimed, and the exclusion stated in every report so that nobody reads the power test as a full TPC-H.

## 9.5 How the intermediate-over-output ratio is compared across engines

Document 05 section 5.4 makes it the most valuable column in the report, on the grounds that it is comparable across engines and scale factors in a way wall time is not. That is true in principle and the practice is unclear: DuckDB reports operator cardinalities in its profiler, ClickHouse in a different shape, DataFusion in a third, and Polars barely at all. Comparing rudb's number to DuckDB's requires that both be counting the same thing, and "rows out of a join" is not as unambiguous as it sounds once a probe side is pipelined and a build side is not.

**Settled by:** defining it against one engine's profiler output and checking the definition against a query whose true intermediate cardinality can be computed by hand, Q3 at SF0.01 is small enough to count.

**Meanwhile:** the ratio is reported for rudb and for DuckDB where its profiler makes it derivable, and it is used as a within-engine diagnostic across commits rather than as a cross-engine comparison.

## 9.6 Whether the differential run stays affordable

Document 08 section 8.4 puts three executions of twenty two queries on every commit, and document 07 step seven makes the graph differential a switch in `attribute.rs`, which itself already runs a suite twice. At SF0.01 that is seconds. As the configuration ladder of document 06 section 6.4 grows, four rudb columns, plus sections on and off, plus a forced-hash-join control, the number of full-suite executions per gate grows multiplicatively, and the nightly SF10 run is the one that will notice first.

**Settled by:** measuring the nightly's wall time once the ladder is complete at G5. If it exceeds the night, the ladder gets sampled rather than the gates loosened: rungs 1 and 4 nightly, all four at each gate.

**Meanwhile:** all of it runs, because it is currently cheap and because the cost of discovering later which rung mattered is higher than the cost of the machine time.
