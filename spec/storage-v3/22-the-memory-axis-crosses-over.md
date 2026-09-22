# The memory axis crosses over

## Why this document exists

Document 19 calls memory "the one axis of the project's target where rudb is behind rather than ahead" and supports it with document 17's measurement of 1.46 GiB against DuckDB's 0.44. Document 21 accepted that framing and built a program on it. Document 15, in the same series and about the same suite, reports 4.2 GiB against DuckDB's 8.62.

Both measurements are real, neither document mentions the other, and they disagree about the sign. The reconciliation is that document 17 measured 10,000,000 rows and document 15 measured 100,000,000, and rudb's memory and DuckDB's do not grow at the same rate. ClickBench is 100,000,000 rows, so the benchmark scale answer is document 15's, and the claim that memory is the axis where rudb is behind is wrong at the scale the target is stated at.

This is the eighth error class in the series and a new one: two documents holding opposite measurements of the same quantity, each correct, neither aware of the other. The previous seven were errors of reading a measurement. This one is an error of not comparing two.

## The numbers, with their scales attached

| rows | | rudb | DuckDB | source |
| ---: | --- | ---: | ---: | --- |
| 10,000,000 | suite peak resident | 1.46 GiB | 0.44 GiB | document 17 |
| 10,000,000 | minor page faults | 1,019,444 | 122,037 | document 17 |
| 100,000,000 | suite peak resident | 4.2 GiB | 8.62 GiB | document 15, quiet host |
| 100,000,000 | suite wall | 116.63 s | 244.12 s | document 15, quiet host |
| 100,000,000 | file bytes | 11,232,108,477 | 20,435,972,096 | document 15 |

Ten times the rows takes rudb from 1.46 GiB to 4.2, which is 2.88 times, and DuckDB from 0.44 to 8.62, which is 19.6. The two lines cross, and a measurement taken on either side of the crossing supports the opposite conclusion about the target.

Document 15's rudb figure was re-measured for this document on the 99,997,497 row native file, which is the row count its harness gates on, with the type conversion the published setup performs:

| | value |
| --- | ---: |
| peak resident | 6.53 GiB |
| wall | 1296.14 s |
| user | 3072.25 s |
| system | 308.19 s |
| minor faults | 13,525,511 |
| major faults | 1 |

That host was at load average 27 on eight cores and document 15's was quiet, which is why the wall is eleven times its 116.63 seconds and why the peak is 6.53 rather than 4.2. A contended run holds more chunks in flight than a quiet one. Both figures are below DuckDB's 8.62, so the direction of the comparison survives the contention even though neither number is a clean one.

One major fault over an eleven gigabyte file on a machine with 23 GiB says the page cache held the file and none of this is disk.

## Where the crossing is, roughly

rudb's memory has a large fixed part and a small part that grows. Document 21 measured the fixed part directly at 10,000,000 rows: 774 MiB sits between the suite's peak and its heaviest single query, it is bounded rather than leaking, and it does not depend on how many rows the table holds. That is 53% of the 1.46 GiB figure and 18% of the 4.2, which is the whole of why the ratio to DuckDB looks so different at the two scales.

Fitting a line through the two points for each engine, which two points do not really justify and which is offered as an order of magnitude rather than a number, puts the crossing near 27,000,000 rows. Below it DuckDB is leaner because rudb is paying a fixed overhead over a small table. Above it rudb is leaner because DuckDB's footprint tracks the data and rudb's mostly does not.

So document 17's 3.32 times is not a property of the engines. It is a fixed overhead divided by a small table, and quoting it as the state of the resource axis is the same kind of mistake as quoting a per process suite total as a query comparison.

## What this does and does not change

It changes the sign of one claim and the priority of one program. rudb is not behind on memory at benchmark scale; on document 15's quiet host it holds about half what DuckDB holds, and it also writes a file 1.82 times smaller. Document 19's sentence and document 21's closing section are both corrected below, and neither correction touches their measurements, which were right about the scale they were taken at.

It does not make the target met. Half of DuckDB's memory is 2.05 times better and the target is ten times, and the re-measurement above under contention is only 1.32 times better. Closing document 21's 774 MiB of retention would take the quiet host figure toward 3.4 GiB and the ratio toward 2.5, which is further from ten than the performance axis currently stands.

It also does not make DuckDB's 8.62 GiB a clean number. Document 15 ran DuckDB's load under `SET memory_limit = '12GB'` and does not record whether the query passes carried the same setting, and DuckDB's buffer manager will expand to whatever limit it is given on a machine with memory free. A figure that is bounded by policy rather than by need is not evidence about what an engine requires, and re-measuring it was not possible here: there is no DuckDB copy of this table on the host and building one needs about twenty gigabytes against the ten that are free. So the 100,000,000 row comparison should be read as rudb's number being firm and DuckDB's being document 15's.

## What this document does not claim

It does not claim a crossover point. It claims the two curves cross somewhere between 10,000,000 and 100,000,000 rows, because rudb is behind at the first and ahead at the second, and the 27,000,000 above is an interpolation between two points per engine on a shared host.

It does not claim rudb's memory is flat. It grows by 2.88 times for ten times the rows, and its minor fault count grows by 13.3 times, which is faster than the data. Something in the process does track the row count and this document has not identified it.

It does not revisit the load, which is the part of the resource axis where rudb is behind at every scale measured: document 15 records 3.47 times DuckDB's wall and 4.09 times its CPU, and document 17 records a factor of 5.02 in peak memory between two spellings of the same load. Nothing here touches any of that.
