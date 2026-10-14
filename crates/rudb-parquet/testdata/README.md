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

## counted.parquet

Written by duckdb v1.5.5, Snappy, 2000 rows in one row group.

One row group is the whole point of it. A row group's stated distinct count is exact for that row group, so a file with one of them is the only file whose footer counts the column rather than bracketing it, and this is the fixture where a distinct count comes back exact instead of certified. `few` and `label` get a count because DuckDB wrote them with a dictionary page, `many` gets none because every value in it is different and the writer gave up on the dictionary, so the same file holds both the stated case and the unstated one.

```sql
COPY (
  SELECT (i % 97)::INTEGER AS few,
         i::BIGINT AS many,
         ('tag' || (i % 5)) AS label
  FROM range(2000) tbl(i)
) TO 'counted.parquet' (FORMAT parquet, ROW_GROUP_SIZE 100000, COMPRESSION snappy);
```

DuckDB reads it as n=2000, sum(few)=94890, min(few)=0, max(few)=96, min(many)=0, max(many)=1999, min(label)='tag0', max(label)='tag4', and it counts 97 distinct in `few`, 5 in `label` and 2000 in `many`. The footer states 97 for `few` and 5 for `label` and nothing for `many`, so the first two agree with the count exactly.

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

## sorted.parquet

Written by DuckDB, Snappy, eight row groups of 2048 rows. The file the zone map estimates are measured against, and the one thing that makes it different from the others here is that `k` is in ascending order, so every row group's bounds cover a different stretch of it and a filter on `k` rules groups out.

```sql
COPY (
  SELECT i::BIGINT AS k, (i % 97)::INTEGER AS g
  FROM range(16384) tbl(i)
) TO 'sorted.parquet' (FORMAT parquet, ROW_GROUP_SIZE 2048, COMPRESSION snappy);
```

Eight groups is not an accident. The estimator holds its guess under the rows in the groups the bounds leave, and the guess for one condition is a fifth of the file, so a ceiling only bites when fewer than a fifth of the groups survive. Four groups of 4096 would never reach that and would test nothing.

DuckDB reads it as n=16384, sum(k)=134209536, sum(g)=786036, and answers 100 rows for `k < 100`, 8192 for `k >= 8192`, 169 for `g = 5` and none for `k > 999999`. `g` is `i % 97`, so every row group holds every value of it and no filter on `g` rules a group out, which is the control.

## scaled.parquet

Written by DuckDB, Snappy, eight row groups of 1024 rows. `sorted.parquet` is the same idea for a column of integers and this is it for the three types that are an integer with a power of ten under it, which are the ones the two sides of a comparison can disagree about the scale of.

```sql
COPY (
  SELECT (i / 100.0)::DECIMAL(15,2) AS d,
         ('2020-01-01 00:00:00'::TIMESTAMP + (i * INTERVAL 1 MINUTE)) AS t,
         (('2020-01-01 00:00:00'::TIMESTAMP + (i * INTERVAL 1 MINUTE)))::TIMESTAMP_MS AS ms
  FROM range(8192) tbl(i)
) TO 'scaled.parquet' (FORMAT parquet, ROW_GROUP_SIZE 1024, COMPRESSION snappy);
```

All three columns ascend, so a filter on any of them rules groups out and the groups that survive are narrow enough to interpolate inside. `d` is stored as an `INT64` of hundredths, which is what makes reading its bounds as plain integers wrong: the footer's 7 is 0.07. `t` and `ms` are the same instants in two units, microseconds and milliseconds, which is what makes reading a timestamp's bounds without the unit wrong the same way.

DuckDB reads it as n=8192 with `d` from 0.00 to 81.91 and `t` from 2020-01-01 00:00:00 to 2020-01-06 16:31:00, and answers 1000 rows for `d < 10.00`, 3192 for `d >= 50.00`, 1001 for `d BETWEEN 20.00 AND 30.00`, none for `d > 999.00`, 1440 for `t < '2020-01-02 00:00:00'` and 2432 for `t >= '2020-01-05 00:00:00'`.

## bloom.parquet

Written by duckdb v2.0.0-dev84237 (cc7e7bac7f), Snappy, two row groups of 2048 rows, for `parquet_bloom_probe`. Each column holds three values in a row group, and the second group's values are the first group's plus ten, so a value present in one group is ruled out in the other.

```sql
SET threads = 1;
COPY (
  SELECT i // 2048 * 10 + i % 3 AS a,
         (i // 2048 * 10 + i % 3)::INTEGER AS n,
         'v' || (i // 2048 * 10 + i % 3) AS s,
         ((i // 2048 * 10 + i % 3) / 2)::DOUBLE AS d,
         (i // 2048 * 10 + i % 3)::DECIMAL(4,1) AS dc,
         ('00:00:' || lpad((i // 2048 * 10 + i % 3)::VARCHAR, 2, '0') || '+01')::TIMETZ AS tz,
         ('00000000-0000-0000-0000-0000000000' || lpad((i // 2048 * 10 + i % 3)::VARCHAR, 2, '0'))::UUID AS u,
         (TIMESTAMP '2026-01-01' + INTERVAL (i // 2048 * 10 + i % 3) SECOND)::TIMESTAMP_NS AS tn,
         i AS w,
         {'x': i % 3, 'y': i % 3 + 100} AS st,
         i % 3 + 10 AS z
  FROM range(4096) r(i)
) TO 'bloom.parquet' (ROW_GROUP_SIZE 2048);
```

Every column has a bloom filter except `w`, which has too many values for a dictionary. The struct is there for `z` after it. The pin looks up a column's filter by its place among the top-level columns rather than among the leaves, so for `z` it reads the filter of `st.y`, and it answers that 11 is ruled out of both groups and 101 is in both. That is tamnd/duckdb#51, and the answers the tests hold for `z` are the ones its own filter gives.

## nested-list.parquet, nested-struct.parquet, nested-map.parquet and nested-lists.parquet

Written by duckdb v2.0.0-dev84237 (cc7e7bac7f), uncompressed, one column each, for the writer's nested columns. They are small enough that a byte that comes out wrong can be found by reading the file in a hex dump, and between them they hold a null, an empty and a full value of a list, a struct, a map and a list of lists.

```sql
COPY (SELECT * FROM (VALUES ([1,2,NULL]),([]),(NULL),([3])) t(l)) TO 'nested-list.parquet' (FORMAT parquet, CODEC uncompressed);
COPY (SELECT * FROM (VALUES ({'a':1,'b':'x'}),(NULL),({'a':NULL,'b':'y'})) t(s)) TO 'nested-struct.parquet' (FORMAT parquet, CODEC uncompressed);
COPY (SELECT * FROM (VALUES (MAP {1:'a',2:NULL}),(MAP {}),(NULL)) t(m)) TO 'nested-map.parquet' (FORMAT parquet, CODEC uncompressed);
COPY (SELECT * FROM (VALUES ([[1],NULL,[]]),(NULL),([[2,3]])) t(ll)) TO 'nested-lists.parquet' (FORMAT parquet, CODEC uncompressed);
```

The pin's footer gives `l.list.element` 6 values and 3 nulls, `s.a` 3 values and 2 nulls, `s.b` 3 values and 1 null, `m.key_value.key` 4 values and 2 nulls, `m.key_value.value` 4 values and 3 nulls, and `ll.list.element.list.element` 6 values and 3 nulls. A null count is not the number of null values a leaf holds. A null struct counts once for every leaf under it, and once more if the struct is itself inside a list.

## nested.parquet

Written by duckdb v2.0.0-dev84237 (cc7e7bac7f), Snappy, 5000 rows in one row group, with dictionaries and bloom filters on every leaf.

```sql
SET threads = 1;
COPY (
  SELECT CASE WHEN i % 7 = 0 THEN NULL WHEN i % 5 = 0 THEN [] ELSE [i % 4, CASE WHEN i % 3 = 0 THEN NULL ELSE i % 6 END] END AS l,
         CASE WHEN i % 11 = 0 THEN NULL ELSE {'a': CASE WHEN i % 4 = 0 THEN NULL ELSE i % 9 END, 'b': 'v' || (i % 3)} END AS s,
         CASE WHEN i % 13 = 0 THEN NULL WHEN i % 6 = 0 THEN MAP {} ELSE MAP {'k' || (i % 3): i % 5} END AS m,
         (CASE WHEN i % 9 = 0 THEN NULL ELSE [i % 2, i % 3] END)::BIGINT[2] AS a,
         CASE WHEN i % 17 = 0 THEN NULL ELSE [{'x': i % 3, 'y': CASE WHEN i % 2 = 0 THEN NULL ELSE [i % 5] END}, NULL] END AS ls,
         (CASE WHEN i % 10 = 0 THEN NULL ELSE {'p': [i % 2], 'q': CASE WHEN i % 8 = 0 THEN NULL ELSE {'r': i % 7} END, 'z': [i % 3, 1]} END)::STRUCT(p BIGINT[], q STRUCT(r BIGINT), z BIGINT[2]) AS sl
  FROM range(5000) t(i)
) TO 'nested.parquet' (FORMAT parquet);
```

The array casts sit outside the `CASE` because the pin cannot build a `CASE` of a fixed size array. Every column has a `CASE` that can make it null, because the pin writes a column that cannot be null as REQUIRED, without definition levels for the top level. A null array still takes an entry for each of its elements, so `a.list.element` has 10000 values. `ls.list.element.x` has 9705 values and 9705 nulls, though 4705 of its values are there, since the null second element of every list is counted once as a null struct and once as a null under a list. The pin reads `a` and `sl.z` back as `BIGINT[]`, since the file stores a fixed size array as a list, and reads a null array as NULL rather than as the elements it was written with.

## list-paged.parquet

Written by duckdb v2.0.0-dev84237 (cc7e7bac7f), Snappy, 5000 rows in one row group, for where a data page under a list ends.

```sql
SET threads = 1;
COPY (
  SELECT CASE WHEN i % 7 = 0 THEN NULL WHEN i % 5 = 0 THEN [] ELSE [i % 4, CASE WHEN i % 3 = 0 THEN NULL ELSE i % 6 END] END AS l,
         i AS f
  FROM range(5000) t(i)
) TO 'list-paged.parquet' (FORMAT parquet, DATA_PAGE_SIZE_LIMIT 1000);
```

The pin keeps the values of the 2048 rows it hands a list writer at once on one page, so `l` only gets a new page at the first entry of such a block, while `f` gets one every time its values reach the limit.

## units-dict.parquet and units-delta.parquet

Written by pyarrow 23.0.1, uncompressed, five rows, for a time in each of the three units and a timestamp in milliseconds with and without UTC. The pin never writes a time in milliseconds itself, which is why something else wrote these.

```python
t_ms = pa.array([3723500, None, 86399999, 0, 86400000], pa.time32('ms'))
t_us = pa.array([3723500001, None, 86399999999, 0, 86400000000], pa.time64('us'))
t_ns = pa.array([3723500001002, None, 86399999999999, 0, 86400000000000], pa.time64('ns'))
ts_ms = pa.array([981173106500, None, -1500, 0, 253402300799999], pa.timestamp('ms'))
ts_tz = pa.array([981173106500, None, -1500, 0, 1], pa.timestamp('ms', tz='UTC'))
table = pa.table({'t_ms': t_ms, 't_us': t_us, 't_ns': t_ns, 'ts_ms': ts_ms, 'ts_tz': ts_tz})
pq.write_table(table, 'units-dict.parquet', compression='NONE', store_schema=False, coerce_timestamps=None)
pq.write_table(table, 'units-delta.parquet', compression='NONE', store_schema=False, use_dictionary=False, column_encoding='DELTA_BINARY_PACKED', coerce_timestamps=None)
```

The pin reads them as TIME, TIME, TIME_NS, TIMESTAMP and TIMESTAMP WITH TIME ZONE, with the first row 01:02:03.5, 01:02:03.500001, 01:02:03.500001002, 2001-02-03 04:05:06.5 and 2001-02-03 04:05:06.5+00, and the last row 24:00:00 in all three times, 9999-12-31 23:59:59.999 and 1970-01-01 00:00:00.001+00.
