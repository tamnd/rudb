# Compact dictionary construction

## Current representation

The writer builds each global string dictionary from four structures:

1. one byte arena containing each distinct string once
2. one `u32` end offset per value
3. a `u64` hash to `u32` primary code table
4. collision chains created only after two unequal strings have the same full hash

A hash hit always compares bytes in the arena. Hash equality alone never decides string equality.

The dictionary page writer moves the arena into the output path. It writes the authenticated index first and streams the arena after it. It must not assemble a second index plus payload allocation.

## Evidence

At one million ClickBench rows:

| Writer | Wall | Peak RSS |
| --- | ---: | ---: |
| String keys in `HashMap<String, u32>` | 2.83 s | 431 MiB |
| Compact hash index and arena | 2.50 s | 339 MiB |
| Compact index with streamed finalization | 2.51 s | 295 MiB |
| DuckDB load | 3.51 s | 1,933 MiB |

The target requires a peak below 193 MiB on this measurement.

## Next design

Large dictionary payloads and membership indexes must not all remain resident until commit. The next writer uses per-column scratch extents:

1. append unique payload bytes to a scratch extent
2. retain offsets and the compact membership index
3. compare a repeated hash against scratch bytes with block caching
4. write the final index and transfer the scratch extent without copying it through a second arena
5. remove every scratch extent on success, error, or writer drop

If membership indexes still exceed the budget, partition construction by high hash bits. Finish one partition at a time, write its dictionary segment, and remap provisional codes into final codes while producing the column extent. This is an external radix dictionary build and bounds resident state by one partition.
