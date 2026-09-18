# Certified frequency synopses

## Decision

A committed native snapshot may store an exact leading frequency list for a scalar column. The list also stores an upper bound for every omitted value. A grouped `count(*)` ordered by count may read the list instead of the column only when the last requested winner is strictly above that bound.

This is query metadata, not an approximate answer. If the proof does not hold, execution scans the column and uses the ordinary aggregate.

## Why this belongs in storage

The writer has information that a later query should not reconstruct. It sees stable string codes, validity, and every encoded numeric page. Rebuilding a high cardinality hash table for each query throws that information away.

At one million ClickBench rows, `UserID` has 898,913 distinct values. Only a few values are frequent:

| Rank | Count |
| ---: | ---: |
| 1 | 291 |
| 2 | 255 |
| 3 | 106 |
| 10 | 51 |

Persisting the complete histogram would be larger than the source column. Persisting only leaders without a proof would give approximate SQL results. A bounded leader list plus an omitted-value bound avoids both problems.

## Numeric construction

The writer finishes pending pages before building the synopsis. It then handles one numeric column at a time:

1. Scan the encoded pages with a 32,768-entry Misra-Gries table.
2. Count how many full-table decrement rounds occurred.
3. If no decrement occurred, keep the candidate counts because they are already exact.
4. Otherwise, compare the tenth candidate's lower bound with the decrement count. Stop without a
   synopsis when that lower bound cannot certify the ClickBench TopN boundary.
5. For a viable synopsis, scan the pages again and compute exact counts for the remaining
   candidates.
6. Sort candidates by count and value and persist the first 512 entries.
7. Set `omitted_max` to the greater of the decrement count and the first exact candidate omitted
   from the stored list.

Misra-Gries proves that a value absent from its final candidate table occurs no more times than the number of decrement rounds. Candidates removed only by the 512-entry storage limit have exact counts, so the stored maximum covers both omitted sets.

Construction reuses the column pages in the target file. It does not retain a candidate table for every column at once. Peak working memory is therefore bounded by one candidate table and one decoded stripe.

## String construction

Every string stripe already stores stable codes into one snapshot-wide dictionary. The dictionary builder increments a count for each valid code and keeps nulls separately. Finalization sorts these exact counts and applies the same 512-entry limit.

Null and the empty string remain different values. A null may use the empty string's physical code inside a validity mask, but its frequency is recorded only in the null counter.

## Directory extension

The optional directory extension starts with `RUDBFQ2\0` and the schema width. Readers also accept the earlier `RUDBFQ1\0` form. Each column then has either no summary or:

1. `omitted_max` as `u64`
2. entry count as `u32`
3. a value tag and value payload
4. exact count as `u64`
5. bounded occurrence count as `u32`
6. table-wide occurrence ordinals as delta-coded unsigned varints

Integer values use signed 128-bit storage so the directory representation does not depend on the column's physical width. String values use stable dictionary codes. A null has its own tag.

Readers accept older version 7 directories with no extension. They validate type tags, count bounds, entry order, and the absence of trailing bytes before exposing a summary.

Numeric recounts collect row ordinals for all surviving Misra-Gries candidates while computing their exact counts. The list is discarded when it exceeds 65,536 rows. This gives composite TopN execution a sparse candidate set without another column scan and places a hard bound on directory growth. A stored list is sorted, unique, and validated against the table row count.

## Execution rule

The initial execution path covers this shape:

```sql
SELECT key, count(*)
FROM table
GROUP BY key
ORDER BY count(*) DESC
LIMIT k;
```

It requires a direct native table scan, one bare grouping column, an unfiltered `count(*)`, and a known TopN bound. It emits the stored entries into the existing TopN operator. Emitting the stored tail preserves secondary ordering among values tied near the requested boundary.

The path is allowed only when:

```text stored_entries >= k and stored_entries[k - 1].count > omitted_max
```

The strict comparison matters. If an omitted value can tie the boundary, a secondary key could choose it, so execution falls back to the full aggregate.

## Deliberate limits

A single-column synopsis cannot answer a joint grouping such as `(UserID, SearchPhrase)`. Adding every column combination would make load time and file size grow quadratically. Composite synopses need a separate selection rule based on declared workload, observed repeated plans, or a small general set of physical sort keys.

Derived groups that are functions of one key, such as `ClientIP`, `ClientIP - 1`, and
`ClientIP - 2`, may reuse one synopsis after the optimizer proves the functional dependency. That proof belongs in planning rather than the file format.

## Benchmark contract

Every change must report:

| Measure | Required comparison |
| --- | --- |
| Native load wall time | previous rudb, new rudb, DuckDB |
| Native load peak RSS | previous rudb, new rudb, DuckDB |
| Native file size | previous rudb and new rudb |
| Covered query wall and RSS | previous rudb, new rudb, DuckDB native |
| Whole ClickBench wall and peak RSS | rudb native, DuckDB native, rudb Parquet, DuckDB Parquet |

Run 1,000 and 10,000 rows before one million rows. A query speedup does not justify an unbounded loader or a file that stores a complete high cardinality histogram.

## Research basis

The bounded candidate pass follows Misra and Gries, [Finding Repeated
Elements](https://www.cs.utexas.edu/~misra/scannedPdf.dir/FindRepeatedElements.pdf). The exact second pass is essential because SQL results cannot use approximate counter values.

The extension is stored beside the table directory so scans that do not use it pay no page decode cost. This follows the same separation principle as the optional [Parquet page index](https://parquet.apache.org/docs/file-format/pageindex/), while serving aggregate leaders rather than page skipping.
