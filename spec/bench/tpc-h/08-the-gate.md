# 8. The gate

What each milestone has to show on this suite, and the gates that keep it once it is shown. The milestones are `../../graph/10-milestones.md`'s G0 through G8; this document is the measurement side of each, stated so that "done" is a table rather than an opinion.

## 8.1 The rule that makes a gate a gate

A milestone closes when a named measurement, produced by `rudb-bench` on a named machine against a dated manifest, says what the milestone claimed it would say. Not when the code is merged, not when a microbenchmark improved, and not when the total moved, because a total can move for reasons that have nothing to do with the change, which is the entire argument behind the configuration ladder in document 06 section 6.4.

Each gate below therefore names the run, not the feature.

## 8.2 The gates, one per milestone

**H0, the harness runs before the engine does.** `rudb-bench tpch --scale 1` produces a table with twenty two rows in it, of which about twenty say `timeout`, and that table is committed to `reports/`. This is document 07 step three and it depends on no engine work at all. It is the gate that makes every other one measurable, and it is the one most likely to be skipped because its output looks like failure. It is not failure; it is the zero.

**G0, the hash join.** All twenty two queries at SF1 complete with correct answers, at any speed. Answers checked three ways per document 04 section 4.1, including the specification's SF1 qualification output, which is the one run where that reference is cheap and decisive. The plans are committed to `plans.rs` in the same commit. No performance claim is made and none is expected.

**G1, key maps.** A native SF10 file with key maps on all eight tables: bytes per table, build time per table, total section bytes as a fraction of column bytes, and a version 11 reader opening a version 10 file. Query times are reported and are expected to be unchanged; a change means something is being read that should not be.

**G2, the forward link.** Claim C1 measured on a real SF100 file: total graph section bytes under ten percent of column bytes, with `lineitem → orders` and `partsupp → part` in the monotone form. The document 02 section 2.4 property check is what makes the monotone form legal, so the manifest and the file are reported together. Plus the annotation pass tests, which are unit tests rather than a benchmark and are named here because they are the correctness foundation for G3.

**G3, the link join.** Claim C3, per query, ladder rung 3 against rung 2, with the hash join forced as the control in the same process on the same corpus. The sections-off differential of document 07 step seven green across all twenty two queries at SF0.01 and SF1. A query where the link join loses is a planner rule to fix and is reported as such; a query where it is *wrong* stops the milestone.

**G4, single-edge reduction.** Rows removed per scan on Q3, Q5, Q10 and Q14, against a Bloom filter on the same plans, with wall time and with the last-level cache counters of document 05 section 5.5. This is the first half of claim C2 and the milestone where the design stops being speculative, and it is the one whose negative result is planned for in `../../graph/10-milestones.md` rather than feared.

**G5, full reduction.** Claim C2 in full at SF100, plus the isolation measurement: single-edge filters against full reduction, which is the number that says whether this work found something GRainDB did not (document 06 section 6.5). Ladder rung 4 against rung 3 across the suite, with the maximum per-query ratio beside the mean.

**G6, factorization.** Claim C4 on Q9, Q10, Q18 and Q21, reported as peak RSS first and wall time second, because the argument for factorization is memory before it is time and Q9 is the query where that is decided (document 03 section 3.4).

**G7 and G8** leave this suite. SNB and JOB are `../../graph/09-measurement.md` section 9.7's business, and their gates live there. The only TPC-H requirement at G7 and G8 is that nothing regressed, which is section 8.4.

**C5, the goal.** Ten times DuckDB on SF100 total runtime, same machine, same day, same manifest, all twenty two correct, no timeouts, memory limit stated. It is the last claim that can be evaluated and the only one anyone outside the project cares about.

## 8.3 What each gate must publish, whether or not it passed

The run that closes a gate is published with the losses in it. A milestone that improved eighteen queries and made two worse publishes all twenty two and says which two, per `../../15-rudb-bench.md` rule three, and the two get an issue rather than a footnote.

A gate that fails is published too. A dated table saying the exact reduction lost to the Bloom filter on Q5 is worth more to this project than the absence of a table, because it is what turns `../../graph/11-open-questions.md` section 11.1 from a worry into a decision.

## 8.4 The regression gates, which are what keep a gate closed

Three, all in `regress.rs` per document 07 step eight, all running without anybody asking.

**Every commit, SF0.01.** All twenty two queries, four executions, DuckDB, rudb as it comes, rudb with `graph_sections = off`, rudb with `statistics = off` per `../../stats/09-measurement.md` section 9.3, compared for answers. Under a second per execution. This catches the class of bug that a correctness suite catches, at the commit rather than at the next SF100 run, which is the difference between bisecting one commit and bisecting a week.

**Every commit, the plan baselines.** Twenty two committed plans at SF1. A plan change fails the build and is resolved by reviewing the diff and committing the new one. Not because a plan change is bad, but because an unreviewed one is how an optimizer quietly stops firing.

**Nightly, SF10.** Timed, against the committed distribution. A query regresses when the distributions do not overlap and the median moved past the threshold; a noisy query is reported and not failed, which is `regress.rs`'s existing rule and the reason the gate is still switched on in the second week. Two TPC-H additions: a query that goes from a number to a timeout fails regardless of thresholds, and a changed answer fails as a correctness failure rather than as a timing one.

SF100 is run at each gate and at each release, not nightly, because it costs hours and because nightly noise at that scale would produce a gate people learn to ignore.

## 8.5 What a green TPC-H does not mean

Document 03 section 3.8 lists what this suite does not exercise and it is a long list: no updates, no nulls of consequence, no high-cardinality grouping, no string work beyond `LIKE`, no cyclic join graphs, and a schema whose joins are almost all primary-to-foreign-key with high match rates on a clean snowflake. The GRainDB authors said in print that TPC-H lacks the selective many-to-many joins that reward the kind of structure `../../graph/` builds.

So: TPC-H green is the gate that says the engine can execute a join graph at all, at scale, correctly, without falling over on memory. It is necessary. It is not the finish line, and the suites that test the rest, JOB, CEB, SNB, and the update workloads, are named in `../../graph/09-measurement.md` section 9.7 and in `../../15-rudb-bench.md`'s list of seven.

The failure mode this section exists to prevent is a project that ships a 10x TPC-H number, declares the join done, and discovers on the first customer workload with a cyclic join or a selective many-to-many edge that it built for the benchmark it measured.
