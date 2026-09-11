# Fixtures

Small CSV files the reader's tests run against.

They are committed rather than generated at test time because a test that builds its own input only proves the reader agrees with itself, and the whole point of these is that something else wrote them.

The answers the tests assert come from the `duckdb` binary reading the same file, not from this reader.

They are written down here so a failing test can be checked against the file rather than against the code that failed.

## mixed.csv

Written by duckdb v1.4.1, from the same query that wrote `crates/rudb-parquet/testdata/mixed.parquet`, so the two fixtures hold the same 4096 rows and a reader that disagrees with the other reader is visible.

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
) TO 'mixed.csv' (FORMAT csv, HEADER);
```

DuckDB sniffs it as `a BIGINT, b BIGINT, s VARCHAR, d DOUBLE, flag BOOLEAN, day DATE, t TIMESTAMP`, which is the Parquet fixture's schema except that `a` is a `BIGINT` here, because a CSV file does not say how wide its integers are and the sniffer's rung is `BIGINT`.

It reads as n=4096, sum(a)=195783, sum(b)=2002560000, count(s)=3510, sum(d)=193536.0, 2048 trues, day from 1970-01-01 to 1972-09-26, and t from 2013-07-15 10:00:00 to 2013-07-15 10:14:59.

## noheader.csv

Three rows of three columns and no header line, written by hand.

```
1,x,2.5
2,y,3.5
3,z,4.5
```

DuckDB names the columns `column0`, `column1` and `column2` and types them `BIGINT`, `VARCHAR` and `DOUBLE`. The first line is not a header because it fits the types the rest of the file has, which is the whole of the rule.

## punctuation.tsv

Four lines, tab separated, with a quoted tab, a quoted newline and a doubled quote in it, written by hand.

The extension is what sends a file to the CSV reader, and the reader is what works out that this one is tabs. A `.tsv` file full of commas is read as commas in duckdb v1.4.1, which was measured, so the extension picks the reader and nothing more.

DuckDB reads it as `name VARCHAR, note VARCHAR` and three rows, the second of which holds a newline inside a field.

## parts

Two small files, for the patterns that read more than one file at a time, written by hand.

```
c1.csv      a,s / 1,one / 2,one
c2.csv      a,s / 10,two
```

DuckDB reads `parts/c*.csv` as 3 rows summing to 13.

## sniff

Three small files for the rule that every file a pattern matched is sniffed, not only the first one, written by hand.

```
sniff/s1.csv        id,tag / 1,x / 2,y
sniff/s2.csv        id,tag / 3,z
sniff/s3.csv        id,tag / 4.5,w
sniff/odd/o1.csv    id / 1
sniff/odd/o2.csv    other / 2
```

DuckDB reads `sniff/*.csv` as `id DOUBLE, tag VARCHAR`, 4 rows, `sum(id)` 10.5. Only the third file holds a decimal and it widens the whole read, which is the point: a reader that took the first file's word would answer BIGINT. The same was checked at four and at six files before these three were written down, with only the last file holding the value that widens, and the answer was the same both times.

DuckDB reads `sniff/s[12].csv`, which is the same directory without the file holding the decimal, as `id BIGINT`, 3 rows, `sum(id)` 6.

DuckDB reads `sniff/odd/*.csv` as an error, because the second file does not have the column the first one has.

```
Invalid Input Error: Schema mismatch between globbed files.
Main file schema: odd/o1.csv
Current file: odd/o2.csv
Column with name: "id" is missing
Potential Fixes 
* Consider setting union_by_name=true.
* Consider setting files_to_sniff to a higher value (e.g., files_to_sniff = -1)
```

The trailing space after `Potential Fixes` is the binary's and is kept. This is a different sentence from the one the Parquet reader gives for the same situation, which is next to the Parquet fixtures, because the two readers in DuckDB are two pieces of code that each wrote their own.

## given

Three small files for the named parameters that overrule the sniffer, written by hand. Each one is written so that the sniffer's answer and the answer the parameter asks for are both whole files rather than one of them being an error, because a parameter that only turns a working read into a failing one does not show that the parameter was used.

```
given/semicolon.csv     name,x;note,y / a,b;c,d / e,f;g,h
given/hashquote.csv     name,note / #one#,x / #two#,y
given/escaped.csv       name,note / #a\#b#,x / #c\#d#,y
```

Every line of `semicolon.csv` splits into three fields on a comma and into two on a semicolon, so both are consistent and the sniffer has a real choice to make. DuckDB picks the comma and reads it as `name, x;note, y` with rows `a | b;c | d` and `e | f;g | h`. With `delim=';'`, and the same with `sep=';'`, it reads it as `name,x` and `note,y` with rows `a,b | c,d` and `e,f | g,h`.

`hashquote.csv` is quoted with a hash, which is not a quote character DuckDB looks for, so it reads the quotes as part of the value and answers `#one#` and `#two#`. With `quote='#'` it answers `one` and `two`.

`escaped.csv` is the same file with a backslash escaped hash inside each quoted field. With `quote='#', escape='\'` DuckDB answers `a#b` and `c#d`, and without them it answers the whole of the field including its punctuation.
