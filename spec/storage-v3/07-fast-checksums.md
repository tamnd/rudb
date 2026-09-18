# Fast native page checksums

## Problem

Native pages are validated when they are read. The v4 checksum visited one byte per dependency chain, which put checksum arithmetic on the critical path of scan-heavy queries. Query 18 made this visible: its limited aggregate needed little state but still validated the complete requested pages.

## Design

Version 5 uses a 64-bit checksum with four independent lanes. Each main-loop iteration consumes 32 bytes as four little-endian words. The lanes are mixed together, the remaining bytes are consumed in
8-byte, 4-byte, and 1-byte steps, and a final avalanche produces the stored value.

Checksums cover the same data as v4:

- Every encoded column page stores one checksum in its page slot.
- The committed directory stores one checksum in the file header slot.
- A reader validates the directory while opening the file.
- A reader validates each selected page before decoding it.

The on-disk checksum field remains eight bytes. The magic, directory marker, and major version advance to v5 because the checksum values are different. A v4 file is rejected and must be rebuilt from its source data. Fixed checksum vectors guard the algorithm against accidental format changes.

## Evidence

On the complete 1 million row ClickBench audit, rudb native query time fell from 0.679 seconds to
0.615 seconds and process wall time fell from 1.213 seconds to 0.965 seconds. DuckDB native took
0.542 seconds of query time and 1.458 seconds of process wall time. Query 18 fell from 42.6 ms to
29.7 ms, while DuckDB took 16.0 ms. Native load wall time fell from 2.670 seconds to 2.341 seconds;
DuckDB took 3.438 seconds and used 1693.3 MiB peak RSS compared with rudb's 44.8 MiB.

The checksum is not the controlling cost for high-cardinality string grouping. Queries 34 and 35 improved by only 2.9 and 5.3 percent, so their aggregate architecture remains the next target.
