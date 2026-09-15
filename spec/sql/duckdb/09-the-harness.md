# The harness

`spec/14-rudb-compat.md` says what the harness is for. This says what it is, measured at `995f0e8`, and what has to be added before the numbers in documents 01, 08 and 11 can be computed at all.

## 9.1 What exists

`rudb-compat` is a binary with eight working subcommands: `duckdb`, `parse`, `query`, `run`, `slt`, `slt-one`, `vendor` and `levels`. Two more, `reduce` and `report`, parse their arguments and print that they are not built yet.

It pins DuckDB by commit, `PINNED_COMMIT = "cc7e7bac7fcb6e0994359965a87ac4f6a96f2e17"`, and `vendor` fetches the upstream test suite at `v2.0-cyanoptera` into `target/corpus`.

It compares full result sets in `src/compare.rs`: column count, column names, column types, row count and every cell, with the sorted or as written decision taken from a real parse of the statement through `rudb::row_order` rather than from a string search. Cells are compared as the text each engine printed. Section 1.3 already said this is the right set of decisions and they are the ones most likely to be quietly weakened later.

It runs each file in a child process with a ten second and two gigabyte default and a `BACKSTOP` of twelve seconds outside that, so one hanging file cannot take the run down.

It classifies every failure into nine reasons: `Stopped`, `Syntax`, `NotImplemented`, `Unbound`, `Runtime`, `WrongAnswer`, `MissedError`, `ErrorClass`, `ErrorText`.

Against the upstream corpus that gives 4096 files, 13868 records passed, 50925 failed, 21.4 percent of attempted, and 14314 skipped of which 13392 the file turned off itself. The committed corpus is 8 files, 1152 lines, 192 records, gated at zero failures and zero skips.

There is no fuzzing, no query generation, no reducer and no bisector.

## 9.2 Two corpora with opposite rules, and a third that does not exist

This is the most important structural thing the harness already gets right and it is worth stating as a rule so it survives.

**The committed corpus is a gate.** Zero failures and zero skips, checked on every change. A record enters it only when it passes. Its purpose is that a regression is impossible to merge.

**The upstream corpus is a measurement.** It fails by tens of thousands of records and must never become a gate, because a corpus that has to pass is a corpus that gets trimmed. Its purpose is to produce a number that goes up. The only thing gated about it is the number itself: a change that lowers the pass rate has to say why.

**The real query corpus does not exist**, and section 1.1 makes it the denominator's weights and section 2.8 says every percentage in this folder is unweighted until it does. It is queries people actually run: the ones in DuckDB's documentation examples, the benchmark suites, the queries in the issue tracker, the ones in tutorials and blog posts. Its rule is neither of the other two: it is not a gate and its pass rate is secondary to its histogram, because what it is for is telling us which of the 1159 names and 36 statements are worth anything.

Building it is the first item in document 12 for that reason. Everything else in this folder is ordered by numbers that it supplies.

## 9.3 The skip count is the number to attack first

13392 records the file turned off itself is a third of the corpus never attempted, and section 1.1 already said that a corpus where a third of the records never ran can improve without anything improving.

Those skips are mostly the sqllogictest dialect rather than the SQL. The format has `require` for an extension or a feature, `skipif` and `onlyif` on an engine name, `mode` lines, `loop` and `foreach` and `concurrentloop`, `restart`, `halt`, `hash-threshold`, three sort modes, result labels, and the `<REGEX>:` and `<FILE>:` comparison forms. Every one of those that is unimplemented turns into a skip that looks like a DuckDB feature gap and is not.

So the first harness change is to count and publish skips by reason, separating "this file needs an extension we do not have", which is a real gap, from "this file uses `foreach`", which is a harness gap that hides real records behind it. That split is a day of work and it is the difference between a pass rate that means something and one that does not.

The hash modes deserve a decision rather than an implementation. `hash-threshold` exists so a test file can store a hash instead of a large expected result, and section 1.3 says comparing a hash is the most common way a differential harness reports success it has not earned. We run those files against the live binary and compare the full result, and we use the stored hash only as a second opinion.

## 9.3.1 The hash modes, decided, and the corpus barely uses them

The paragraph above was written before anybody counted, so here is the count at the pin. Of 34329 `query` records in `test/sql`, 231 store a digest instead of their values, in 32 files. 212 of those are in `.test_slow` files, which `slt` leaves out by default, so an ordinary corpus run sees 19 hashed records in 15 files. And there is no `hash-threshold` line anywhere in the corpus at the pin, not one, so the threshold mechanism the paragraph is named after is unreachable from these files and the 231 digests are written into them directly.

That changes what the decision has to protect against. It is not that our pass rate leans on digests, because 19 records out of 34329 cannot move a percentage that is printed to one decimal place. It is that a harness with a hash comparison in it acquires a hash mode of its own the first time a result is inconvenient to store. So the rule is about us rather than about the corpus: nothing in the harness hashes a result it could have compared, no corpus we write stores a digest, and the harness never grows a `hash-threshold` setting. A digest is read because 32 of DuckDB's files contain one, and for no other reason.

For those records, the digest match stands as the outcome and the record is counted like any other, which is a deliberate weakening of the paragraph above. Running 19 records against a live binary on every corpus run would make a run that needs no DuckDB into a run that needs one, and section 9.8 keeps that property because it is what makes the corpus run something CI does on every commit. Nineteen records is not worth that, and the day the count changes, either because the pin brings more or because `--slow` becomes the default, this is the sentence to come back to.

What a digest genuinely cannot do is produce a difference. `expected 100000 values hashing to 8e3f... and got 4c21...` is one bit of information, so it cannot tell a wrong value from a wrong order from the wrong number of columns, and there is nothing in it for the reducer in section 9.5 to work on. The count of values is checked before the digest, which is the one thing the format gives us besides the digest itself, and it is what catches most of the interesting failures anyway. Beyond that, a failing hashed record goes to the live binary to get its values back before anybody reads it, and that belongs to the reduce path where a person is already waiting, rather than to the scoring path that runs on every commit.

A digest also depends on our sort being DuckDB's sort, since it is taken over the values in the order the record's sort mode asked for, and it depends on our formatting of every value matching. That is the hidden cost of the format and it is the argument against ever writing one ourselves: a value comparison that fails tells you which value, and a digest that fails tells you that something about a hundred thousand values is different.

One record in the corpus hashes 24004860 values. That record cannot exist in a file any other way, which is the honest case for the format existing, and it is also the reason the second opinion is per record on demand rather than on by default, because comparing it means twenty four million values held on both sides at once.

Last, a digest in a file is a frozen answer rather than a live one. If it was written against an older DuckDB and the pin has since moved, then a rudb that matches the digest disagrees with the pinned binary. That is the first case in section 9.4, the file being stale rather than the engine being wrong, and it comes out as a note about the corpus and not as a failure.

## 9.4 Two oracles, and what it means when they disagree

Every record in the upstream corpus has an expected result written in the file, and section 14.2 says a claim is always checked against a real binary of a named version. So there are two oracles and they are both available.

Run both. When rudb and the pinned binary agree and the file disagrees with both, the file is stale or the pin has moved and that is a note, not a failure. When rudb and the file agree and the binary disagrees, we are running the record differently from how the file means it to be run, and that is a harness bug and a valuable one, because it usually means a `require` or a `mode` line was ignored.

That second case is invisible to a harness with one oracle, and it is exactly the case that quietly inflates a pass rate.

## 9.5 The reducer

`reduce` prints that it is not built yet, and it is the highest leverage missing piece, because the cost of a compatibility project is not finding differences, it is the human minutes spent per difference.

The algorithm is published and old: delta debugging, Zeller and Hildebrandt, "Simplifying and Isolating Failure-Inducing Input", 2002, the `ddmin` procedure. The refinement that matters for SQL is that reduction on characters produces garbage, so the reducer works on the parse tree: drop a select item, drop a join, replace a subquery by a constant, drop a `WHERE` conjunct, shrink a literal, drop a table column and the corresponding values. Every step re parses and re runs, and keeps the step if the difference survives.

We have the parse tree and an arena AST, so a tree aware reducer is a few hundred lines rather than a project. The requirement it earns: no failure reaches a human unreduced. That is the rule that makes a corpus with fifty thousand failures workable, because fifty thousand failures reduce to a few hundred distinct minimal cases and those are a week of reading rather than a year.

## 9.6 The pass bisector

`crates/rudb-opt/src/lib.rs` line 67 has six passes, in a `PASSES` array. Upstream `duckdb_optimizers()` has 44 rows and upstream has a `disabled_optimizers` setting that takes a list of names.

Add the same setting here, by name, over the same array. Then when a record disagrees the harness re runs it with each pass disabled in turn and reports which pass, if any, changes the answer. With six passes that is six extra runs on a failing record only, which is free, and it turns "wrong answer" into "wrong answer, and the filter pushdown pass causes it" without anybody reading a plan.

The version of this for the other side is the same trick against the pinned binary, since `disabled_optimizers` already exists upstream, which tells us whether the difference is in our optimizer or in our semantics.

## 9.7 What every run records besides the answer

Section 1.7 puts the resource axis in this harness rather than only in the benchmark suite. This is the mechanism.

Every record already runs on both engines in a child process with a deadline and a memory cap. That child is where the numbers come from: wall clock around the statement, CPU seconds for the process, and peak resident set, on both sides, recorded per record. On Linux that is `getrusage` with `ru_maxrss` and `ru_utime` plus `ru_stime`, which the harness can read for a child it already waits on, so no profiler and no instrumentation inside either engine is involved.

Three ratios come out, rudb over DuckDB: time, CPU and peak memory. Each is a median of at least five runs with the interquartile range beside it, never a minimum, following the reporting rules in `spec/15-rudb-bench.md` section 15.1. The five runs only happen for records that are candidates, which is the ones that pass on both sides and are above the noise floor, so the cost of this is bounded by the interesting fraction of the corpus rather than by all of it.

The exclusions are written down rather than applied by feel. A record that fails on either side is not timed, because timing an error path measures the error path. A record under a few milliseconds on both engines is not timed, because process startup dominates and the ratio is noise. Setup records, `PRAGMA` and the error message cases, are not timed because they are not work.

Two things make this worth more than a benchmark run. It is per record, so a regression names the file that regressed and therefore the feature that caused it, which is the same argument as the reducer and the pass bisector. And it runs on the corpus rather than on a chosen suite, so it covers the shapes nobody chose, which is where a feature that is fast on the benchmark and quadratic on the long tail shows up.

The numbers go on the report page as their own block, per record, rolled up per feature and per milestone. `spec/15-rudb-bench.md` stays the place whole query suites against whole engines are run and is still where any number anybody quotes in public comes from. This is the early warning, not the benchmark.

## 9.7.1 Which corpus the ratios are measured over

The paragraphs above say the ratios come from the sqllogictest records and that turned out to be wrong, so here is what was built instead and why. A record in that corpus only means anything under a session that replays every statement before it, because the file makes a table, fills it and then asks questions about it. Timing the record therefore times the replay, and the replay is the harness rather than the engine. There is no arrangement that fixes it while rudb has no storage format, since the usual answer is to build the tables once into a database file and open it per record.

The ratios are measured over upstream's benchmark corpus instead, which is the same 845 queries `rudb-compat queries` counts for the function histogram. Those are independent by construction: each file carries its own load and one query and nothing in it depends on the file before it. Using one corpus for the weights and the ratios is worth something on its own, because the question "which of this is worth making fast" and the question "how fast is it" are then asked about the same queries.

A number is the load and the query in one process, not the query alone, for the storage format reason above. The load is measured a second time on its own with a trivial statement after it, so the share of each number that is ingestion is printed beside every ratio. Most of these are eighty percent load and a ratio read as a statement about the query would be read wrong. When F2 lands and a database file can be built once and reopened, this becomes the query alone and the two numbers stop being comparable with these, which is a thing to say on the page rather than to discover later.

Row counts are cut down. The suite builds tables of a hundred million rows because it is a benchmark suite for a finished database, and every `range` and `generate_series` argument above a million is brought down to a million before either engine sees it. Nothing else is rewritten, so a modulus, a seed or a hash constant still says what its author wrote. Both engines get the same text so the ratio is still the answer, but it is a ratio at a million rows and claims nothing about a hundred million.

Both engines get a wall clock limit per process, thirty seconds in the published run. A process that goes over it is stopped and the benchmark is refused rather than measured. That is not neutral and the page says so: a benchmark stopped on our side is one rudb was losing badly, so the timeouts leave the ratios rather than making them worse, and the timeout count is part of the result. Without the limit the first query rudb cannot answer takes the rest of the run with it, which is how a corpus of 845 queries produces nothing.

## 9.8 Where it runs

Locally means server1, server2, server3 or the gaming machine, and the harness should assume that. 4096 files over a machine with 32 cores is a sharding problem and nothing more: `cargo nextest` already partitions with `hash:m/n` and `slice:m/n`, and the per file child process model already makes the run embarrassingly parallel.

What the run has to record so a number is reproducible: the rudb commit, the rudb-compat commit, the DuckDB commit and the hash of the actual binary, the corpus commit, the machine, the seed for anything generated, and the wall clock. A published percentage without those six is a rumour.

## 9.8.1 Sharding is built, and it buys nothing until one join lands

Built and measured rather than assumed. `slt --shard k/n` takes every kth file of the sorted list, `--out` writes what that slice found in the form `merge` reads back, and `scripts/shard` in rudb-compat is the loop over server1, server2, server3 and the gaming machine. Round robin over the sorted list rather than contiguous blocks, which is what `cargo nextest --partition` does and for the same reason: the list is sorted by path, so a block is a directory, and `copy` and `aggregate` are not the same amount of work.

What the measurement says is that this is not where the time goes. The whole corpus is 364 processor seconds of work over 4106 files at the pin, one file, `optimizer/table_filters.test`, is 125 of them on its own, and seven files are 310 of the 364. A run cannot finish before its slowest file finishes, so the floor is two minutes on any machine and any number of machines. The whole corpus takes 2 minutes 14 seconds on the gaming machine's 32 cores and 2 minutes 23 on server2's 6, and the same corpus sharded over both of those machines took 3 minutes 9, because the slowest file is still one file and the sync and the merge are new. Sharding this corpus today makes the run slower.

The file at the top of that list is the point. It is million row joins under `PRAGMA threads=1`, which is the nested loop join gap, so the number that moves this is a join operator and not another machine. The sharding is written down and tested because it is fifty lines and because that operator is coming, not because anything is faster for it now.

A shard is an index into a sorted list of files, which means every machine has to be looking at the same list, and the first real run proved they were not. The corpus is fetched per machine into `target`, and `v2.0-cyanoptera` is a branch rather than a commit, so server2 was at `10de957` with 4096 files and the gaming machine was at `3e0f8b6` with 4106. The merged answer had 4101 files in it and a pass rate of 20.9 percent, which was neither machine's. Each shard now writes the corpus commit it ran over at the top of its file and `merge` refuses a set of shards that do not agree on it, and the script refreshes every machine before it starts. The deeper fix is a corpus ref that is a commit and not a branch, which nothing depends on yet and which section 9.7 will want anyway, because a published percentage against a moving corpus is a percentage against nothing.

The other thing the two machines showed is that the page is machine dependent. With both at the same commit, the gaming machine on its own reports 16014 passes and 15 records the engine stopped, and the same corpus sharded over it and server2 reports 16010 passes and 19 stopped, and the four records are ones where server2 is slow enough that the statement clock fires and a pass becomes a `stopped` failure. The same run earlier had server2 cutting off three files the gaming machine finished. That is the limits in section 9.1 working as designed, and it is also a published number that moves with the box it ran on. The provenance block names the machine, which is why the block exists, but naming the machine explains the difference rather than removing it. Removing it means a backstop measured in work rather than in seconds, and nothing here builds that yet.

`merge` prints the corpus summary and writes no page and no series row. A page carries the provenance of one machine and a sharded run has several, and a provenance block that names four machines, four rudb builds and four wall clocks is a worse artifact than no page at all while sharding is something nobody should be running.

## 9.9 What it reports

One machine readable artifact per run, appended to a series so the numbers have a history rather than a current value. It carries the three level two denominators from section 1.1, the five error levels from section 8.2, the skip counts by reason from section 9.3, the failure counts by the nine reasons, the three resource ratios from section 9.7 at record, feature and milestone granularity, and the list of minimal reduced cases from section 9.5.

`report` being unbuilt is the reason the sentence "rudb-compat levels prints the four levels and says none of them has been measured yet" is still true, and that sentence is what this whole folder was written to end.

## 9.10 What it cannot do

Section 14.9 already lists the limits and they do not change: a differential harness proves agreement on what was tried, and nothing about what was not. It cannot tell you that an untested input agrees, it cannot find a bug both engines share, and it cannot measure anything about performance, durability under crash, or concurrency, each of which has its own document. Document 10 is about making "what was tried" much larger, which is the only honest answer to the first limit.
