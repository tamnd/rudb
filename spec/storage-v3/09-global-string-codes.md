# Global string codes

## Decision

A string column may have one immutable dictionary for a committed snapshot. Every physical stripe stores stable `u32` codes into that dictionary. Codes keep the same meaning in every stripe.

Stable codes are part of the storage contract. Page local dictionary codes are an encoding detail and must not be exposed as stable codes.

## Why

Hashing and copying the same URL bytes during every aggregate loses information storage already knows. A stable integer code makes equality, grouping, radix exchange, and repeated predicates work in code space. String bytes are needed for byte predicates, ordering, and final output.

The ClickBench evidence at 100k rows is:

| Query | DuckDB native | rudb native with stable codes |
| --- | ---: | ---: |
| Q34 | 16.00 ms | 1.56 ms |
| Q35 | 19.00 ms | 1.52 ms |

An earlier revision of this section claimed that the evidence above does not hold at benchmark scale, citing Q34 losing by 2.72x and Q35 by 3.85x at 100,000,000 rows. That citation was from document 13, which measures both engines reading Parquet. Reading Parquet, rudb has no dictionary of its own and this document's mechanism is not running at all, so the number tested something else and the claim was wrong.

The mechanism does hold at benchmark scale. Document 15 measures 100,000,000 rows in the native format: Q34 falls from 37.71 seconds to 0.51 and Q35 from 45.84 to 0.45, factors of 73.9x and 101.9x over the same engine reading the same data out of Parquet. The 100,000-row evidence quoted above still should not be cited as a full-scale result, but the full-scale result exists and agrees with it.

## Dictionary page

The version 7 dictionary page contains:

1. value count, payload block size, and payload block count
2. `value_count + 1` little endian `u32` offsets
3. one checksum per payload block
4. concatenated UTF-8 payload bytes

The directory page reference stores a checksum of the complete dictionary index, including the header, offsets, and block checksums. Opening a dictionary verifies that index before exposing any codes. Reading a payload block verifies its checksum before exposing bytes.

The initial payload block size is 64 KiB. A query that emits ten groups reads only blocks containing those strings. A predicate that scans the column reads each payload block at most once.

## Execution contract

The vector form carries the stable code flag and shares one dictionary identity across stripes.
Slicing and gathering must preserve that identity. A kernel may compare or hash codes only when both inputs prove the same dictionary identity.

Grouped `count(*)` with one varying stable string key uses radix owners:

1. scan workers exchange codes by low radix bits
2. each owner counts its disjoint code range with plain integers
3. output vectors retain stable codes and the shared dictionary
4. constant grouping columns are reconstructed in their original positions

No row loop takes a lock or hashes string bytes.

## Predicate contract

Byte oriented predicates read external dictionary bytes directly. They must not create a recursive value per row. Predicate results may later be cached by dictionary identity, prepared expression, and code when repeated codes make that cheaper than row evaluation.

## Error contract

An index checksum failure is reported when the dictionary opens. A payload checksum or I/O failure is reported when the affected block is first read. It must never become a null, an empty string, or a different predicate result. The vector and result APIs therefore need a fallible external byte access path all the way to the query boundary.

## Remaining physical layout work

Input chunks are execution units, not storage page boundaries. Writing one physical stripe per
1,024 row chunk creates about 981 page reads at one million rows. Storage should pack tens of thousands of rows into one physical column extent while the reader continues to return vector sized logical chunks. The extent index must support one batched read and independent logical chunk decode.
