# rudb-bench: measurement and reporting

This project's entire claim is a performance claim, which means its credibility rests on the honesty of its measurements more than on any technical decision in documents 05 through 09. A benchmark number without its methodology is marketing. This document constrains what we are allowed to say.

`rudb-bench` is a separate repository so that it can be run by someone who does not trust us, and so that a result can be reproduced without building the engine from a specific commit of the engine's own repository.

## 15.1 Reporting rules

These apply to the README, release notes, the dashboard, any talk, any post, and any conversation.

1. **State the comparison exactly.** Which `rudb` commit, which DuckDB version, which ClickHouse version, which machine, which kernel, which filesystem, which settings. "10x faster than DuckDB" is not a claim, it is a mood.
2. **Report the distribution, not the best run.** Median of at least five runs with the interquartile range. Never a minimum. Never a single run. ClickBench's convention of taking the best of three is fine for ClickBench because it is the board's convention and comparability matters more than rigour there, and any number we publish in that form is labelled as ClickBench-convention.
3. **Report the whole suite including the losses.** Every query, in a table, including the ones where we are slower. A geometric mean with no per-query table is not a result.
4. **Report cold and hot separately.** Never quote a hot number without the cold one next to it. Cold is what a user's first query does.
5. **Report load time and on-disk size with every runtime result.** Document 5.6 says the encoder is slower at write time and document 06 says the format is smaller. Both belong in every table, because a runtime win paid for with a 10x load time is a different product than it appears.
6. **Report peak resident memory with every runtime result.** Axis 4 is a first-class claim and memory is half of it. A query that is fast because it used 30 GB is not fast, it is expensive.
7. **Never compare across machines**, and never compare a number measured today against one measured on different hardware six months ago.
8. **Any published number is reproducible by one documented command** on a named machine type, and the command is in the same artifact as the number.
9. **State the mode.** Native format, attached DuckDB file, and Parquet-in-place are three different products with three different performance profiles, per document 12.1. A number without its mode is not interpretable.
10. **A micro-benchmark number never appears without the end-to-end number it is supposed to explain.** A 20x kernel improvement that moves the query by 3 percent is an engineering note, not a result.

**Document 02's specific claims must be restated wherever numbers appear.** The 10x is against DuckDB on aggregate ClickBench time, not against every system on every query, and document 02.5 names the queries where 10x is not physically available. A reader who sees a geometric mean without that context will infer a claim we did not make, and letting them is the same as making it.

## 15.2 The suites

**ClickBench**, run to the official rules with the official script, so the number is comparable to the public board. 43 queries, `hits` at 99,997,497 rows, three runs per query, first is cold. Reported both as the official Combined metric and as the raw sum, because the sum is the diagnostic number and the Combined is the comparable one.

**TPC-H at SF10, SF100 and SF1000.** The three scales matter separately: SF10 fits in cache on a large machine and measures the engine, SF100 is the standard comparison point, SF1000 exceeds memory on most machines and measures spilling, which is where a lot of engines quietly fall over.

**TPC-DS at SF100.** All 99 queries. The suite that punishes a narrow optimizer.

**JOB and CEB.** Where document 9.5's Robust Predicate Transfer either works or does not, and where per-query variance matters more than the total. Reported with the maximum per-query ratio prominently, not just the mean, because the failure mode being measured is a single catastrophic plan.

**H2O.ai group-by and join.** Fast, widely quoted, good regression canary.

**Micro-benchmarks.** Per-encoding decode throughput, per-kernel throughput, hash table insert and probe rates, scan throughput at various selectivities, I/O throughput at various queue depths. For diagnosis, subject to rule 10.

**Real workloads where we can get them.** Anonymized query logs from anyone willing to share, run against synthesized data of matching shape. These are worth more than every synthetic suite combined and they are also the hardest to obtain, so this is aspirational and marked as such.

## 15.3 Who we measure against

DuckDB at the version we claim compatibility with, in its native format. This is the primary comparison and it is the one axis 2 is stated against.

ClickHouse, because it is the fastest widely deployed single-node analytical engine and because document 03.2 puts it at 18.07 seconds against DuckDB's 26.25. Any claim of being the fastest that does not beat ClickHouse is not a claim of being the fastest.

Umbra where a binary is obtainable, because it is the current ClickBench leader at 8.10 seconds and because document 02.4's entire scenario arithmetic is stated relative to it. If a binary is not obtainable, its published board numbers are cited as published board numbers with the date, and never mixed into a table of numbers we measured.

DataFusion and Polars, because they are the Rust ecosystem's answer and because document 03.2 shows them at 45 seconds, which is 1.7x behind DuckDB. Being ahead of them is a floor, not an achievement.

Vortex-backed DuckDB and DataFusion, specifically, because they are the closest published thing to our design and their result is negative. Tracking them is how we find out whether we have avoided their failure mode or reproduced it.

## 15.4 The machine

**The primary reporting machine is `c6a.4xlarge`**, 16 vCPU, 32 GiB, gp2, because that is what the ClickBench board uses and comparability is worth more than picking a machine that flatters us.

**A large machine is reported alongside**, at least 64 cores, because scaling behaviour is a real property and because document 01.5 says 448-core instances exist and are where the industry is going. An engine that is fast at 16 threads and does not scale to 128 is a different product.

**A small machine is reported alongside**, 4 cores and 8 GiB, because the embedded use case is frequently a laptop and because axis 4's resource claim is most meaningful where resources are scarce. This is also the configuration where spilling gets exercised on the standard scale factors.

**ARM is reported**, on Graviton and on Apple silicon, because a large share of embedded database usage is on ARM laptops and because a SIMD story that only works on x86 is half a story.

**Measurement hygiene.** Frequency pinned where possible, turbo state recorded, page cache dropped before cold runs, filesystem and mount options recorded, and every one of those facts printed in the result artifact rather than assumed.

## 15.5 Continuous tracking

**Every nightly writes a row per benchmark**: commit, machine, suite, query, metric, value, IQR, peak memory, plus the diagnostic counters.

**The diagnostic counters are part of the record, not just the timings.** Vectors processed encoded versus decoded, per document 6.7. Execution tier per pipeline. Adaptive decisions taken. Bytes read from disk. Row groups pruned. These are what makes a regression explicable a month later, and collecting them after the fact is impossible.

**Regressions block merges.** The threshold is per-benchmark and derived from that benchmark's own measured run-to-run noise rather than a global percentage, because a benchmark with 5 percent variance and one with 0.3 percent need different gates and a single global threshold makes one of them useless.

**The escape hatch is stating the tradeoff in the pull request.** "This costs 3 percent on Q18 and fixes a wrong answer on correlated subqueries with nulls" is an obviously correct trade. The purpose of the gate is not to prevent regressions, it is to prevent unnoticed ones.

**A dashboard plots every metric over time**, and it is public. A project whose entire premise is a performance claim should be showing its work continuously, not at release boundaries.

## 15.6 Where the performance actually comes from

For the record, so that optimization effort goes where the leverage is, and in honest order.

**Storage layout, by a wide margin.** Compression ratio and therefore bytes read; global dictionaries turning string columns into integer columns; encoded execution avoiding decode entirely; multi-column compression. Bespoke OLAP's ablation measured storage specialization at 12.35x against code specialization at 1.26x, and every design decision in this project follows from taking that seriously.

**Then algorithmic choices in the operators.** The heavy-hitter top-k mechanism, the global versus thread-local hash table switch, radix partitioning decisions, Robust Predicate Transfer on join workloads. These are individual factors of 1.5x to 5x on the queries they apply to.

**Then adaptivity.** Getting the right strategy at runtime instead of the wrong one from a bad estimate. Worth a lot on the queries where the estimate was wrong and nothing on the rest, which makes it a variance reduction more than a mean improvement, and variance is what users actually experience.

**Then I/O.** io_uring at depth, direct I/O, plan-driven prefetch. Matters on cold runs and on data larger than memory, which is 20 percent of the Combined metric and 100 percent of a user's first impression.

**Then code generation.** The four tiers of document 08, worth roughly 1.2x to 1.5x on this class of workload, which is real and is not where the story is.

**Then micro-optimization.** SIMD kernels, branch elimination, cache-line alignment. Necessary to not lose the gains above and never sufficient on its own.

The ordering matters because the last item is the enjoyable one and the first item is where the time is.
