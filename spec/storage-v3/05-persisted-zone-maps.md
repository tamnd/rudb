# Persisted zone maps

## Decision

The native directory stores a minimum, maximum, and null count beside every column page. These statistics are part of the committed directory and share its checksum and generation boundary.
They are available without reading or decoding the page.

The implementation uses native format version 4 because adding variable-length bounds changes the directory grammar. A v3 reader must reject a v4 file, and a v4 reader must reject v3 rather than guessing where page entries end.

## First-principles reason

A columnar file saves work only when the reader can avoid irrelevant columns and irrelevant row ranges. Projection already avoids columns. A selective predicate still requires every projected page unless metadata can prove that a stripe contains no matching value. Reading a small bound from the directory is cheaper than reading, checksumming, decoding, filtering, and discarding a page.

Bounds make a one-sided claim. They may prove that a stripe cannot match. They never prove that a row does match. Unsupported types and incomparable values keep the stripe, preserving correctness.

## Encoding

Each stripe records all page locations and then one range per schema column. A range contains:

- an optional lower bound
- an optional upper bound
- a 32-bit null count

Bound tags distinguish no bound, signed 128-bit integer domains, 64-bit floating-point domains, and length-prefixed bytes. Integer domains include booleans and dates. Bytes cover strings and blobs.

## Measured result

On 1 million ClickBench rows, native Query 40 fell from 22.2 ms to 7.3 ms. Queries 37 through 39 fell from 12.1 to 13.4 ms into a 2.9 to 3.4 ms range. Loading the v4 native file took 2.762 seconds and 45.3 MiB peak RSS, compared with DuckDB native at 3.578 seconds and 2008 MiB. The rudb file was
561,062,577 bytes and the DuckDB file was 281,817,088 bytes.

The result supports persisted pruning metadata. It also shows that the format still needs better compression and larger physical pages before it meets the size and overall performance goals.
