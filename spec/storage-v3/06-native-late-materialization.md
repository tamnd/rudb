# Native late materialization

## Problem

A projected columnar scan avoids unreferenced columns, but `SELECT *` still requests the whole row.
When a selective filter and TopN retain ten rows from a million, decoding the whole row before the selection makes columnar storage do almost the maximum possible work.

## Design

Every catalog scan can produce a synthetic table-wide row ordinal. The ordinal is stable across native stripes and in-memory chunks and follows source order. The optimizer narrows eligible scans to predicate columns, ordering columns, and the ordinal. After TopN, a table fetch reads the selected columns for only the surviving ordinals and restores their requested order.

The ordinal is an execution identity rather than a stored user column. It is carried only inside the rewritten plan and never added to the table schema or native file.

The fetch contract accepts ordinals in any order. The current implementation groups work through a small chunk cache and reconstructs vectors from the selected values. This is appropriate for the
1024-row rewrite limit. A later block directory can map ordinals to blocks directly and gather from pinned decoded pages.

## Eligibility

The rewrite keeps the existing late-materialization limits:

- TopN plus offset must select at most 1024 rows.
- At least eight columns must be deferred beyond the ordering columns.
- The path from scan to TopN must preserve a usable row ordinal.
- Computed projections must be replayable from the fetched row.

## Evidence

On 1 million ClickBench rows, native Query 24 fell from 76.3 ms to 20.5 ms. DuckDB native took 43.0 ms. rudb CPU time fell from 1.03 seconds to 169 ms and peak RSS fell from 79.3 MiB to 33.3 MiB.
