# What a busy host cannot move

## Why this document exists

Documents 13 through 16 all reason from wall and CPU times taken on one shared host, and document 15 says plainly that no per query wall time from that machine means anything to two significant figures. This document reports the quantities that machine cannot move: counts of page faults, peak resident bytes, file bytes, and whether a query returned an answer at all. It exists because four of those turned up results the timing documents did not predict, one of them says a suite total in this series may be counting 35 queries as 43, and one of them is a configuration change worth making.

Everything below was measured on the same host at 10,000,000 and 30,000,000 rows, against a rudb built at `d41b8a24` whose `--print-config` reports `execution-tiers: interpreted`, and whose every query warns that all of its operators ran a reference implementation with no alternative registered to choose from. That is the slow path the project keeps for differential testing, and it is the path all of documents 13 through 16 measured too.

## What a busy host does, measured

The same query was run forty times inside one rudb process. Its wall time averaged 3,763 ms over the first five repeats and 8,613 ms over the last five, a factor of 2.29, while its CPU time stayed between 4,990 and 7,766 ms with no trend and its peak memory stayed flat near 800 MiB. Wall rising while CPU holds still is the signature of an engine losing parallelism, and that is what it was written up as.

It was wrong. Running the identical forty repeats twice back to back put the ratio at 1.06x both times, and `/proc/loadavg` read 31.43 on eight cores at the end of the session against 0.09 at the start. The first run had measured somebody else's workload arriving. The lesson is not that the host is noisy, which document 15 already establishes, but that the noise has a shape: it arrives during a run, so it reads as a trend rather than as scatter, and a trend is exactly what a profile is read for.

This is the third error of this class in the series. Document 13 read a Parquet measurement as a native one, document 16 read a CPU sum as a wall time, and this one read a rising external load as an internal decay. All three were caught by re-measuring rather than by re-reading, and the control that caught this one cost two minutes.

## The allocator is in the profile

One pass of the 43 queries at 10,000,000 rows, one process per engine, counted by `/usr/bin/time -v`:

| | rudb | DuckDB |
| --- | ---: | ---: |
| minor page faults | 1,019,444 | 122,037 |
| major page faults | 0 | 0 |
| peak resident | 1.46 GiB | 0.44 GiB |

No major faults on either side, so none of this is disk. A minor fault is the kernel handing over a page the process has mapped but never touched, and rudb takes 8.35 of them for every one DuckDB takes while holding 3.32 times the memory. At about four kibibytes a page that is close to four gibibytes of first touch in a process whose resident set never exceeds one and a half, which means the same pages are being taken, returned and taken again.

Across the 43 queries the count tracks the engine's system time closely enough to be the explanation rather than a correlate: `Q1` faults 1,492 times and spends 0.01 s in the kernel, `Q23` faults 192,201 times and spends 2.49 s, `Q29` faults 200,336 times and spends 4.05 s. The fault counts are exact and the seconds are not, but a ratio that holds from 1,492 to 200,336 is not a property of the host.

## One environment variable

rudb's allocator is mimalloc, chosen deliberately and documented in `crates/rudb-cli/src/heap.rs`. mimalloc returns unused pages to the operating system on a delay, and that delay is what makes the same page fault more than once. Five configurations, same binary, same database, same 43 queries at 10,000,000 rows:

| setting | minor faults | peak resident |
| --- | ---: | ---: |
| default | 972,251 | 1.51 GiB |
| default, repeated | 954,775 | 1.54 GiB |
| `MIMALLOC_PURGE_DELAY=-1` | 463,958 | 1.76 GiB |
| `MIMALLOC_ALLOW_LARGE_OS_PAGES=1` | 995,121 | 1.47 GiB |
| both | 464,361 | 1.76 GiB |

Turning the purge off halves the faults for 16% more resident memory, and it is the only one of the three that does anything. Asking for large pages does not help and mildly hurts, because this host has `/sys/kernel/mm/transparent_hugepage/enabled` set to `madvise` and the fallback path is worse than the ordinary one. At 30,000,000 rows the purge setting takes the fault count from 2,513,567 to 803,819, a factor of 3.13, so the effect grows with the table rather than washing out.

This is a smaller lever than any of the four obligations in document 14 and it is not competing with them. It is worth recording because it costs nothing, because it is measured in counts rather than seconds, and because half of rudb's page faults being avoidable by configuration is a fact about where the engine's work goes that no operator profile was going to surface.

## The same table, loaded two ways

Two statements that produce the same 30,000,000 row table from the same Parquet file, verified identical by `COUNT(*)`, `SUM(UserID)` and `SUM(LENGTH(URL))`:

| statement | peak resident | outcome |
| --- | ---: | --- |
| `CREATE TABLE hits AS SELECT * FROM read_parquet(...)` | 20.85 GiB | ran out of memory, no file |
| `CREATE TABLE ... LIMIT 0` then `INSERT INTO hits SELECT ...` | 4.15 GiB | 3,090,866,020 bytes written |

A factor of 5.02 in peak memory between two spellings of one load, on a machine with 23 GiB, where one of them does not finish. Document 01's fourth requirement says a load's working set depends on stripe and writer concurrency and not on table row count, and these two spellings sit on opposite sides of it.

Document 15 reports rudb's 100,000,000 row load peaking at 17.58 GiB and concludes that requirement 4 is not met at benchmark scale. That conclusion may still be right, but it is now underdetermined, because the measurement that supports it does not record which of these two spellings it used, and the gap between them is larger than the gap document 15 is reasoning about. The requirement should be restated as a property of the writer rather than of the load, and tested against the streaming spelling, which is the one the requirement describes.

## A row count gate does not certify a suite

Document 15 records a DuckDB load killed partway through that left an empty table which then answered all 43 queries in 12.52 seconds without erroring once, and it added a gate: the harness refuses to measure a database that does not hold the expected row count. That gate is necessary and it is not sufficient.

Loading the published `hits.parquet` with a plain `SELECT *`, without the type conversion the ClickBench setup performs, leaves `EventDate` as `UINT16` holding a day number and `EventTime` as `BIGINT` holding epoch seconds. Both engines then hold exactly the right number of rows, and their `COUNT(*)`, `SUM(UserID)` and `SUM(LENGTH(URL))` agree to the digit, so a row count gate and a checksum gate both pass on both copies. Eight of the 43 queries nonetheless fail, on both engines, with the same two errors:

| queries | error |
| --- | --- |
| Q19 | `Binder Error: No function matches the given name and argument types 'date_part(VARCHAR, BIGINT)'` |
| Q37 to Q43 | `Conversion Error: Could not convert string '2013-07-01' to UINT16` |

The failing set is identical across the two engines, which is a compatibility result in rudb's favour and is why the omission is invisible in a ratio: both totals lose the same eight queries and each of them returns in about a tenth of a second. It is not invisible in the totals themselves, which are 35 queries of work carrying a 43 query label.

This document first recorded that section as a DuckDB-only failure, on the strength of a grep for `^error` that a line reading `Conversion Error:` does not match. rudb fails the same eight. The claim was checked only because it had already been written down, which is the argument for writing measurements down in a form specific enough to be checkable.

The gate this suggests is not another property of the data. It is that every query in the suite must be recorded as having returned, per query, per engine, per pass, and that a pass missing any of them is not a pass. That is cheap, it is exact, and it is the only one of the three gates that would have caught all three of the failures this series has now hit.

## What this document does not claim

It claims nothing about which engine is faster. Every timing taken for it came from a host whose load average went from 0.09 to 31.43 during the session, and the one timing-shaped conclusion it started with turned out to be the load and not the engine. The numbers it does stand behind are counts of faults, counts of bytes, and counts of queries that returned, and those are stated because they are the kind of number this host cannot touch.

It also does not claim these findings move the project's target. Halving a fault count is not a factor of ten and is not offered as one. What it offers is that two of the four measurements here contradict something a timing document concluded, that the contradiction was visible only in quantities nobody was recording, and that the cheapest available improvement to this series is to record them.
