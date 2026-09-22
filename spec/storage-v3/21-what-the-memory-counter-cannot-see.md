# What the memory counter cannot see

## Why this document exists

The project's target has two halves and this series has spent five documents on the first one. Time is measured, attributed and argued about down to the operator; memory appears in document 19 as a single sentence saying rudb holds 1.46 GiB against DuckDB's 0.44, and as a column in one table naming the queries that spend it. That column is `resource.peak_bytes`, which is the number rudb reports about itself, and this document went looking for the process behind it.

It is not there. The three largest memory consumers in the suite are three queries that counter reports as using nothing, the process holds two thirds of a gigabyte that no statement is charged for, and the per process suite totals that produced the project's most puzzling measurement turn out to have been measuring how long DuckDB takes to start. None of that was visible from inside the engine, and all of it was one `/usr/bin/time` away.

This is the seventh error class the series has collected and the first one that is about an instrument rather than an inference: reading an engine's self reported counter as the resource the target is stated in.

## The comparison that was never a comparison

At 30 million rows rudb beat DuckDB 19.80 seconds against 40.71 summed over 43 fresh processes, and lost 29.73 against 7.95 in one batch process. A 7.7 times swing from the process model alone has sat unexplained in this project for three sessions. Half of it is one measurement:

| engine | process startup, five runs in order |
| --- | --- |
| rudb | 0.23, 0.11, 0.09, 0.09, 0.12 s |
| duckdb | 3.52, 0.79, 0.51, 0.88, 0.49 s |

DuckDB's per process penalty over the suite was 40.71 minus 7.95, which is 32.76 seconds over 43 processes, or 0.762 seconds each. That lands inside the band above and near its middle. The entire DuckDB half of the inversion is the cost of starting DuckDB, paid 43 times, and it has nothing to do with how either engine answers a query.

So a suite total summed over one process per query is not a query performance comparison. It is a startup comparison weighted by 43, and it favours whichever engine starts faster, which here is rudb by six times. The batch number is the comparable one and the per process number should not be quoted beside it as though the two measured the same thing.

## rudb does not degrade in a long process

The other half of the inversion was a guess about rudb, which a previous session refuted as external load and which this one can settle with the clock that load moves least. The same suite, the same database, back to back, on a host at load 28 over eight cores:

| | process wall | summed statement CPU |
| --- | ---: | ---: |
| one process, 43 statements | 73.88 s | 91.04 s |
| 43 processes, one statement each | 106.43 s | 104.43 s |

One process is faster on both clocks. Whatever the 30 million row measurement was, rudb running 43 statements in a row is not slower per statement than rudb running them one to a process, and the batch mode has no accumulating cost that shows up in CPU. That measurement rested on a database that answered 35 of 43 queries and has since been deleted, so it cannot be re-run, and on a correctly loaded database the effect does not appear at all. It should be retired rather than explained.

## The counter and the process disagree

Every query in the suite, alone in a fresh process, with the peak resident set taken from outside and `resource.peak_bytes` taken from within:

| query | rudb reports | process peak RSS | shape |
| --- | ---: | ---: | --- |
| q23 | 0.4 MiB | 712 MiB | `LIKE` over `Title` and `URL`, group by `SearchPhrase` |
| q29 | 128.3 MiB | 643 MiB | `REGEXP_REPLACE` over `Referer`, group by the result |
| q24 | 0.0 MiB | 399 MiB | string predicates |
| q22 | 0.0 MiB | 386 MiB | string predicates |
| q19 | 397.9 MiB | 357 MiB | group by two integer columns |
| q33 | 310.1 MiB | 343 MiB | group by `WatchID`, `ClientIP` |
| q17 | 395.9 MiB | 330 MiB | group by `UserID`, `SearchPhrase` |

The counter is close on the bottom three and useless on the top four. It is measuring hash aggregate group tables, because those are what the operators register, and it is blind to string materialisation, to decode buffers, to the reader's page cache and to whatever else the process acquires on the way. The three queries it reports as needing nothing are the first, third and fourth largest consumers in the suite.

That corrects document 19 on a specific point. Its middle column is this counter, its Q23 row reads "under 1 MiB", and Q23 is the largest memory consumer in the suite by a clear margin. Its conclusion that the memory is spent on "hash tables that exist to be thrown away" describes the part of the memory that happens to be instrumented, and the larger part is spent on strings.

## Where the gigabyte is

The suite in one process peaks at 1,486 MiB. Its heaviest single query alone peaks at 712. The difference is 774 MiB that the process holds and no statement needs, and the counter's maximum across the run is 429, so from inside the engine that difference does not exist.

It is bounded rather than leaking. Running the suite twice and three times over in the same process, which is 86 and 129 statements:

| statements | peak RSS |
| ---: | ---: |
| 43 | 1,486 MiB |
| 86 | 1,557 MiB |
| 129 | 1,552 MiB |

A second pass adds 4.8% and a third adds nothing. The process fills something and then stops, which is the signature of a cache reaching its size and not of memory being lost.

It is also not spread evenly over the suite. The first 21 queries in a fresh process peak at 407 MiB and the last 22 peak at 1,481, which is the whole of the full suite's figure:

| what ran | peak RSS |
| --- | ---: |
| queries 1 to 21 | 407 MiB |
| queries 22 to 43 | 1,481 MiB |
| all 43 | 1,494 MiB |

The back half contains q22, q23, q24 and q29, whose standalone peaks are 386, 712, 399 and 643. Two of those coexisting accounts for the figure, and sampling the resident set every half second through a run shows exactly that: it oscillates between 69 and 412 MiB through the front half, returning memory freely, then ratchets from 412 to 1,466 with only small dips, then falls to 197 before the process exits. The memory was releasable the whole time and was not released between statements.

## The allocator holds a fifth of it

mimalloc is configured in `crates/rudb-cli/src/main.rs` with two features chosen by measurement and no purge setting, and document 17 priced its retained pages as a configuration note. The suite with the settings that make it give memory back promptly:

| setting | peak RSS | wall | system time |
| --- | ---: | ---: | ---: |
| default | 1,486 MiB | 43.40 s | 19.54 s |
| `MIMALLOC_PURGE_DELAY=0` | 1,279 MiB | 50.24 s | 48.55 s |
| `MIMALLOC_RESET_DELAY=0` | 1,280 MiB | 56.85 s | 47.54 s |
| `MIMALLOC_PURGE_DECOMMITS=1` and delay 0 | 1,283 MiB | 37.41 s | 41.44 s |

So the allocator is holding about 205 MiB of the 774, and taking it back costs between two and two and a half times the system time, which is the page table work document 20 found at 4.9% of a scan profile. That is a fifth of the excess and a bad trade at this price. The other four fifths are live allocations that rudb itself is keeping across statement boundaries, and no setting reaches them.

## What this does to the target

It does not reach it, and it changes the shape of the memory half from a single number into two separable problems.

The first is retention. 774 MiB sits between the suite's peak and its heaviest query, it is bounded, it is releasable, and about 570 MiB of it is rudb's own. Releasing it takes the suite from 1,486 MiB to roughly 712, which is the largest thing any one statement genuinely needs.

The second is that 712 MiB. DuckDB runs the entire suite in 0.44 GiB, which is 450 MiB, so even a rudb that gave back every byte between statements would still need more memory for its single heaviest query than DuckDB needs for all 43. That is the working set problem, and it is the one document 19 described correctly even though it named the wrong queries: a query that returns ten rows should not build a structure over ten million.

Fixing the first alone moves rudb from 3.3 times worse than DuckDB to about 1.6 times worse. That is not ten times leaner, it is not close, and it is the first thing in this series that turns the resource axis from a number nobody has looked behind into a program with two parts and a measurement under each.

## What this document does not claim

The DuckDB memory figure is document 15's, taken at 100 million rows, and everything measured here is at 10 million on a host at load 17 to 28 with a disk at 100%. Building a matched DuckDB database was not possible without filling that disk, so the cross engine comparison in the previous section is indicative and the rudb numbers are the ones with a measurement under them. The wall times throughout are inflated by contention and only the comparisons made back to back should be read as ratios.

It does not identify what holds the 570 MiB. The reader's page cache keeps four stripes per column and `Reader::keep_stripes` raises that with `fetch_max` and never lowers it, which is a candidate and not a finding, and a stripe is 64,000 rows so a 105 column table at this scale puts that cache in the low hundreds of megabytes rather than the high ones. Naming the holder is the next measurement and this document does not pretend to have made it.

It does not claim `resource.peak_bytes` is broken. It measures what it was built to measure, which is what operators register, and the error was reading it as the process. A counter that is silently partial is more expensive than no counter, because document 19 built an argument on it and the argument named the wrong four queries.
