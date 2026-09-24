# Strings sorted by their rank

TPC-H q16 ends with `ORDER BY supplier_cnt DESC, p_brand, p_type, p_size` over 18,314 groups. On one thread at SF1 that sort was about a fifth of the query, 1,300 of 6,300 samples, for fewer than twenty thousand rows.

## Why it was slow

Under #1210 the sort got a normalized key: every key of a row written into one 24 byte buffer whose byte order is the sort order, so a comparison is a byte compare. A string has no fixed width, so a key list with a string in it took the old path, a `Vec<Value>` per row compared through the `Value` enum one key at a time. In q16 most rows tie on the count, so nearly every comparison went on to compare two strings through that path. The allocation per row and the walk over every value to count its memory made up the rest.

## What changed

A sort does not need a string's bytes in the key. It needs to know where the string falls among the other strings of the same key, and since strings order by their bytes, that is the string's rank among the distinct values, which fits in four bytes.

So a key list that has no normalized layout, but has one once every `VARCHAR` or `BLOB` key is four bytes and a tag, now takes a third arm. The ranks are only known once the last row is in, so this arm keeps each chunk's key columns and each row's arrival as they come in. When the sort runs, each string key is ranked with one sort of its values and a walk that counts the distinct ones. The ranks are written through the same column writer as an unsigned integer column with the string's nulls, the other keys are written as they always were, and the rows sort as bytes with the arrival settling ties. q16's key list, a `BIGINT`, two strings and an `INTEGER`, comes to exactly 24 bytes.

Ranking a key costs one sort of its strings, which the old path paid anyway and then paid again inside every row comparison. A list that does not fit even with ranks, and a list with a float in it, still take the old path. Like that path, this arm does not spill, so a string keyed sort bigger than memory is still #1301.

## Results

Instructions per run, SF1, against DuckDB 1.5.

| threads | before | after | DuckDB |
|---|---|---|---|
| one | 0.52 G | 0.435 G | 0.33 G |
| default | 0.630 G | 0.538 G | 0.547 G |

All 22 answers are the same. What is left of q16 is mostly the grouping on the two strings and the size, whose hash table compares string keys by their bytes on every probe.
