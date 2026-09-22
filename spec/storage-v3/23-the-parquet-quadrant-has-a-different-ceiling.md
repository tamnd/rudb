# The Parquet quadrant has a different ceiling

## Why this document exists

The target names four quadrants and this series has spent documents 15 through 22 on one of them. Document 13 measured the Parquet quadrant, concluded rudb loses it by 2.30x at benchmark scale, and was then corrected by document 15 for using that measurement to refute native format mechanisms it could not test. The correction was right and it left the original number standing and unexplained: rudb reading Parquet is slower than DuckDB reading the same Parquet, and nothing in this series says why.

This document says why, on one query at benchmark scale, and the answer is not the one the series has been assuming. It is also the first document here to measure all three of the relevant corners against each other on the same data rather than reasoning about two of them.

## One query, three corners, ninety nine million rows

`SELECT Referer, COUNT(*) FROM ... WHERE Referer <> '' GROUP BY Referer ORDER BY COUNT(*) DESC LIMIT 10`, against the 99,997,497 row native file and against the published `hits.parquet` those rows were loaded from. Same host, load average 27 on eight cores, so the ratios are what matters and not the absolute figures.

| | wall | CPU | peak resident |
| --- | ---: | ---: | ---: |
| rudb, native | 5.86 s | 9.54 s | 825 MiB |
| DuckDB, Parquet | 38.64 s | 87.85 s | 4,763 MiB |
| rudb, Parquet | 109.51 s | 187.44 s | 4,021 MiB |

The query returns ten rows and the aggregate builds 19,720,796 groups out of the 81,032,736 rows that pass the filter.

So the Parquet quadrant is real and it is where rudb loses: 2.83x behind DuckDB on wall and 2.13x on CPU, against 1.18x ahead on memory. That is the same direction and roughly the same size as document 13's suite level 2.30x, which is the first independent confirmation that number has had.

## Separating the format tax from the grouping

The same filter without the grouping, which reads the same column and applies the same predicate:

| | wall | CPU | peak resident |
| --- | ---: | ---: | ---: |
| rudb, native | 4.01 s | 5.07 s | 186 MiB |
| rudb, Parquet | 10.25 s | 35.94 s | 126 MiB |

Subtracting one table from the other gives what the grouping costs on each side:

| | wall | CPU |
| --- | ---: | ---: |
| rudb, native, grouping alone | 1.85 s | 4.47 s |
| rudb, Parquet, grouping alone | 99.26 s | 151.50 s |

Reading and filtering the column out of Parquet costs 2.6 times the wall and 7.1 times the CPU of reading it out of the native file, which is the format tax and is what a compressed general purpose file charges over one written for this engine. Grouping costs 53.7 times the wall and 33.9 times the CPU, which is not a format tax. It is one mechanism being available on one side and not the other.

## The mechanism, and the one line that switches it off

`Vector::dictionary_over` in `crates/rudb-vector/src/vector.rs` builds a dictionary vector with `stable: false`. `Vector::stable_dictionary` is the same constructor with the flag set. Every fast path in `crates/rudb-exec/src/group.rs` gates on `stable_dictionary_parts`, which returns nothing unless the flag is set: the dense count array indexed by storage code, the encoded top count, and the four byte code per row that document 18 measured at twelve to fourteen times on a statement.

`crates/rudb-parquet/src/values.rs` line 93 calls `dictionary_over`. `crates/rudb-native/src/lib.rs` calls `stable_dictionary`. That is the whole of the difference, and the flag is not an oversight. A Parquet dictionary belongs to one column chunk, so code 7 in one row group and code 7 in the next are different strings, and the `Arc::ptr_eq` guard at `group.rs:1016` exists precisely to stop an aggregate treating them as one. Marking those dictionaries stable would give wrong answers.

So rudb reading Parquet groups 81 million strings by hashing 81 million strings, and rudb reading its own format groups them by hashing 19.7 million dictionary entries once and then counting into a dense array. The native format's single global dictionary per column is the property being exploited, and Parquet does not have one.

## What canonicalising the dictionaries would be worth

The fix this suggests is to map each row group's dictionary into one session wide code space per column as it is read, rewrite that row group's codes into it, and hand every chunk the same `Arc`. The machinery downstream already exists and is exercised by the native path every day. The question is what the mapping costs, and this is where the arithmetic has to be done rather than assumed.

`hits.parquet` holds the `Referer` column in 226 row groups of about 442,467 rows each. Distinct values in one of them, measured on the first and on one from the middle of the file:

| row group | rows | distinct |
| --- | ---: | ---: |
| first | 442,467 | 60,358 |
| middle | 442,467 | 127,927 |

At the mean of those two, 226 row groups hold about 21.3 million dictionary entries between them, against 19,720,796 values that are distinct across the whole file. The redundancy is about 1.08, which is to say a `Referer` almost never appears in two row groups, and a canonical code space would be barely larger than the sum of the parts.

Canonicalising therefore costs about 21.3 million string hashes and insertions where the current path costs 81.0 million, a factor of 3.8, after which the grouping is over integers and costs what the native side pays. Scaling the measured 151.50 seconds by 21.3 over 81.0 and adding the native grouping's 4.47 gives about 44 seconds against 151.50 now, and a statement CPU around 80 seconds against 187.44.

**That is a model and not a measurement, and this series has been wrong in exactly this way before.** Document 20 records a projection from cycles per value that a profile then refuted, and the scaling above assumes the cost of hashing a string is the same whether it is one of 81 million rows or one of 21 million dictionary entries, which is false in at least one direction: dictionary entries are all distinct, so every one of them is an insertion and a cache miss, where the 81 million include 61 million repeats that hit. The honest reading is that the factor is somewhere between two and four and the way to find out is to build it.

## What it does to the target

Eighty seconds of CPU against DuckDB's 87.85 is a quadrant that rudb roughly draws rather than loses by 2.13 times. That is worth having, it flips the sign on half of the target's framing, and it is nothing like ten times.

More importantly it bounds what the quadrant can ever be worth. Both engines read the same file with the same per row group dictionaries and neither can have a global one, so the contest there is decode speed and hash table quality against a mature implementation of both. rudb's advantages in this project live in its format: a global dictionary per column, stripe zone maps, and a file 1.82 times smaller than DuckDB's own. None of them are available when the file is somebody else's.

The two halves of the target therefore have different ceilings, which no document here has said before. The native quadrant has rudb 2.09 times ahead with a program documented in 18, 19 and 20 that prices at about 2.6 times more, giving something near five. The Parquet quadrant has rudb 2.13 times behind on this query with one structural change worth two to four times, giving something near one. Ten times better in both, which is what the target asks for, is not reachable by anything measured in this series, and the Parquet half is the harder of the two rather than the easier.

## What this document does not claim

It is one query. `Referer` is a dictionary encoded string column with high cardinality, which is the shape the mechanism in question applies to, and a suite is 43 queries of which many are integer aggregates where none of this matters. Document 13's suite level 2.30x is consistent with the 2.83x here and that consistency is the only evidence offered that this query is representative.

The host was at load average 27 and the absolute numbers are inflated throughout. The three corners were measured within a few minutes of each other and the native file had just been read by another run, so its page cache was warmer than the Parquet file's, which flatters the native column of the first table by an amount this document has not measured.

It does not claim DuckDB's 87.85 seconds is DuckDB's best. No memory limit or thread count was set on either engine, and DuckDB peaked at 4,763 MiB on a machine with memory free, so that figure may be bounded by policy as document 22 notes of a different measurement.
