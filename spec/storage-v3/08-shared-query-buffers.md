# Shared query buffers

## Problem

A completed string column is immutable, but a flat vector owns its arena directly. Cloning that vector therefore clones the arena. Pipeline breakers such as TopN retain rows by cloning vectors, so an aggregate that emits hundreds of thousands of string groups can copy large result arenas even when only ten rows survive.

Callgrind on one million row ClickBench Query 34 attributed 20.0 percent of instructions to `memcpy` and 9.15 percent to vector cloning before the ownership transition.

## Design

Mutable producers build `StringColumn` values in their ordinary owned arenas. At the boundary where an aggregate key column becomes output, the vector calls `shared_text`. This moves the completed arena behind a reference-counted handle and changes the vector body to string views over that arena.
No payload is copied during the transition.

The boundary is explicit. General mutable buffers remain uniquely owned, which avoids copy-on-write cost during loading and vector construction. Consumers receive immutable shared storage and may clone it without copying payload bytes.

## Evidence

On the complete 1 million row ClickBench audit, Query 34 fell from 60.5 ms to 47.8 ms in native mode and from 57.6 ms to 47.6 ms in Parquet mode. Query 35 fell from 58.2 ms to 49.3 ms in native mode and from 56.5 ms to 48.1 ms in Parquet mode. DuckDB native took 26.0 ms for both queries.

The native suite fell from 0.615 seconds to 0.590 seconds. DuckDB native took 0.542 seconds. Native load took 2.358 seconds and 45.7 MiB peak RSS, while DuckDB took 3.505 seconds and 1932.7 MiB.

This document is honest that Q34 remained a loss after the change. An earlier revision added that the loss widens to 2.72x at 100,000,000 rows, taken from document 13, which measures rudb reading Parquet rather than its own format and so measures the arena transition without the stable codes that feed it. In native at 100,000,000 rows Q34 is 0.51 seconds, and document 15 has the measurement.

Avoiding a copy of the result arena is still worth exactly what it says it is worth and no more. The 1,000,000-row gap to DuckDB that this section records was not closed by it, and whether it is closed at benchmark scale is a question for the native-versus-native comparison rather than for this document.
