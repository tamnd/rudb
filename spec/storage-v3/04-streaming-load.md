# Streaming native load

## Decision

A file-backed initial insert sends the producing pipeline directly to a native storage sink. The sink owns the temporary snapshot and publishes it when the pipeline finalizes. The catalog switches from its empty mutable table to the committed native reader only after publication succeeds.

The root sink records `(morsel, chunk)` for every stripe. The writer may place page bytes in completion order, but it sorts directory entries by that key before commit. Physical placement and logical scan order are separate decisions.

The first implementation uses one pipeline instance because page encoding and file writes currently share one writer lock. On one million ClickBench rows this uses 33.6 MiB peak RSS versus DuckDB's 1,953.1 MiB, while loading in 2.620 seconds versus 3.529 seconds. A parallel source reduced wall time to 2.068 seconds but raised peak RSS to 209.4 MiB and did not parallelize encoding behind the writer lock.

## Required next step

The writer needs a page builder that is local to each pipeline instance. A local builder gathers a bounded number of execution vectors, chooses encodings, and produces immutable encoded pages without holding the file lock. The shared writer lock assigns offsets and writes completed byte buffers. The directory retains the source key, page row count, execution-vector boundaries, checksum, and column statistics.

This keeps memory bounded by `instances * columns * target_page_bytes`. It also separates the storage page size from the 1,024-row execution size, which is required for shared dictionaries, one FSST table per useful sample, and fewer checksums and directory entries.
