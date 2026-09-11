# Fixtures

Small Parquet files the reader's tests run against.

They are committed rather than generated at test time because a test that builds its own input only proves the reader agrees with itself, and the whole point of these is that something else wrote them.

The answers the tests assert come from the `duckdb` binary reading the same file, not from this reader.

They are written down here so a failing test can be checked against the file rather than against the code that failed.

## mixed.parquet

Written by DuckDB, Snappy, two row groups of 2048 rows.

```sql
COPY (
  SELECT (i % 97)::INTEGER AS a,
         ((i % 1000) * 1000)::BIGINT AS b,
         CASE WHEN i % 7 = 0 THEN NULL ELSE 'tag' || (i % 5) END AS s,
         ((i % 64) * 1.5)::DOUBLE AS d,
         (i % 2 = 0) AS flag,
         (DATE '1970-01-01' + INTERVAL (i % 1000) DAY)::DATE AS day,
         TIMESTAMP '2013-07-15 10:00:00' + INTERVAL (i % 900) SECOND AS t
  FROM range(4096) tbl(i)
) TO 'mixed.parquet' (FORMAT parquet, ROW_GROUP_SIZE 2048, COMPRESSION snappy);
```

DuckDB reads it as n=4096, sum(a)=195783, min(a)=0, max(a)=96, sum(b)=2002560000, count(s)=3510, min(s)='tag0', max(s)='tag4', sum(d)=193536.0, 2048 trues, day from 1970-01-01 to 1972-09-26, and t from 2013-07-15 10:00:00 to 2013-07-15 10:14:59, which is 1373882400000000 to 1373883299000000 in the micros the file holds.

## zstd.parquet

Written by duckdb v2.0.0-dev84237, ZSTD, 20000 rows in three row groups of 8192, plain and dictionary pages.

The codec is the only thing this one is about. It is the same shape as `mixed.parquet` with a string column wide enough that a page has something for a Huffman coder to do, because a fixture where every page is a handful of bytes exercises the frame and never the entropy coder inside it.

```sql
COPY (
  SELECT (i % 97)::INTEGER AS a,
         ((i % 1000) * 1000)::BIGINT AS b,
         CASE WHEN i % 7 = 0 THEN NULL ELSE ('https://example.com/page/' || (i % 3000)::VARCHAR) END AS s,
         ((i % 64) * 1.5)::DOUBLE AS d
  FROM range(20000) tbl(i)
) TO 'zstd.parquet' (FORMAT parquet, ROW_GROUP_SIZE 8192, COMPRESSION zstd);
```

DuckDB reads it as n=20000, sum(a)=959289, min(a)=0, max(a)=96, sum(b)=9990000000, count(s)=17142, min(s)='https://example.com/page/0', max(s)='https://example.com/page/999', sum(d)=944232.0.

## delta.parquet and lengths.parquet

Written by pyarrow 23.0.1, Snappy, format version 2.6, two row groups of 2048 rows, dictionaries off so the delta encodings are actually used.

These cover the four encodings DuckDB does not write, so there is no way to produce them with the DuckDB binary.

```python
import pyarrow as pa, pyarrow.parquet as pq

n = 4096
words = [None if i % 9 == 0 else "prefix_%05d" % (i // 4) for i in range(n)]
table = pa.table({
    "ints": pa.array([i * 3 - 500 for i in range(n)], pa.int32()),
    "longs": pa.array([1_000_000_000_000 - i * 7 for i in range(n)], pa.int64()),
    "words": pa.array(words, pa.string()),
    "downs": pa.array([i * 0.5 for i in range(n)], pa.float64()),
})
pq.write_table(table, "delta.parquet", compression="snappy", version="2.6",
               row_group_size=2048, use_dictionary=False,
               column_encoding={"ints": "DELTA_BINARY_PACKED",
                                "longs": "DELTA_BINARY_PACKED",
                                "words": "DELTA_BYTE_ARRAY",
                                "downs": "BYTE_STREAM_SPLIT"})
pq.write_table(table.select(["words"]), "lengths.parquet", compression="snappy", version="2.6",
               row_group_size=2048, use_dictionary=False,
               column_encoding={"words": "DELTA_LENGTH_BYTE_ARRAY"})
```

`longs` counts down on purpose, because a decreasing column is the one whose block minimum is negative, and a reader that subtracts the minimum instead of adding it gets a plausible looking wrong answer on it.

`words` repeats each value four times so neighbouring values share a long prefix, which is what the delta byte array encoding is for, and every ninth value is null so the definition levels have work to do.

DuckDB reads `delta.parquet` as n=4096, sum(ints)=23111680, min(ints)=-500, max(ints)=11785, sum(longs)=4095999941294080, count(words)=3640, min(words)='prefix_00000', max(words)='prefix_01023', sum(downs)=4193280.0.

It reads `lengths.parquet` as n=4096, count(words)=3640 with the same bounds.

## bytes.parquet

Written by pyarrow 23.0.1, Snappy, two row groups of 1024 rows, two `binary` columns.

A `binary` column comes out as a `BYTE_ARRAY` with no logical type annotation on it, which is what DuckDB and this reader both call a `BLOB`, and there is no way to get one out of the DuckDB binary because DuckDB annotates everything it writes.

That matters more than a fixture usually does, because every one of the twenty eight byte array columns in the ClickBench file is unannotated in exactly this way.

```python
import pyarrow as pa, pyarrow.parquet as pq

n = 2048
words = [None if i % 11 == 0 else b"byte_%04d" % (i // 2) for i in range(n)]
raw = [bytes([0xff, 0xfe, i % 256]) for i in range(n)]
table = pa.table({
    "words": pa.array(words, pa.binary()),
    "raw": pa.array(raw, pa.binary()),
})
pq.write_table(table, "bytes.parquet", compression="snappy", row_group_size=1024)
```

`words` holds text, so it is the column that has to read, and it repeats each value twice and goes null every eleventh row so that a reader which decoded the values densely and never spread them over the nulls gets the count right and the positions wrong.

`raw` starts every value with `0xff 0xfe`, which is not valid UTF-8 under any reading, so it is the column that has to be refused by name rather than decoded into something.

DuckDB reads both columns as `BLOB` and answers n=2048, count(words)=1861, sum(octet_length(words))=16749, min(words)='byte_0000', max(words)='byte_1023'.

## parts

A directory of small files, for the patterns that read more than one file at a time. Written by duckdb v1.4.1.

```sql
COPY (SELECT i::INTEGER AS a, 'one' AS s FROM range(3) t(i)) TO 'parts/p1.parquet';
COPY (SELECT (i+10)::INTEGER AS a, 'two' AS s FROM range(4) t(i)) TO 'parts/p2.parquet';
COPY (SELECT (i+100)::INTEGER AS a, 'three' AS s FROM range(2) t(i)) TO 'parts/deep/p3.parquet';
COPY (SELECT 1::INTEGER AS a, 'full' AS s) TO 'parts/odd/a_full.parquet';
COPY (SELECT 2::INTEGER AS a) TO 'parts/odd/b_narrow.parquet';
COPY (SELECT 1::INTEGER AS a) TO 'parts/widen/a_int.parquet';
COPY (SELECT '5' AS a) TO 'parts/widen/b_text.parquet';
```

DuckDB reads `parts/p*.parquet` as 7 rows summing to 49, `parts/*/p*.parquet` as 2 rows summing to 201, and `parts/**/p*.parquet` as 9 rows summing to 250.

The three names that are not `p` something are the awkward cases and are kept out of the way of the patterns above on purpose. `odd` is a pair of files that do not agree about their columns, which is the schema mismatch message. `widen` is a pair that agree about the name and not the type, where the first file decides and the second is cast to it, so the two rows read as the integers 1 and 5.
