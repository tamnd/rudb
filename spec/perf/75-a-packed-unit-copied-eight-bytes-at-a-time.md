# A packed unit copied eight bytes at a time

[`74-a-minimum-that-was-taken-in-a-hundred-and-twenty-eight-bits.md`](74-a-minimum-that-was-taken-in-a-hundred-and-twenty-eight-bits.md) ended on a profile. With the extreme loop fixed, `SELECT MIN(UserID) FROM hits` over a million rows still cost 26.9 M instructions against DuckDB's 4.5 M, and 55 percent of it was the decode turning a bit packed column into a run of `i64` against 6 percent in the aggregate that read it. This is what that 55 percent was.

`UserID` is a `BIGINT` of a million distinct values and it is stored frame of reference coded at 62 bits, which is 7.69 bytes a row. A packed unit is 1024 values, so at that width it is 992 words of 64 bits, and the decoder read those words like this:

```rust
for word in &mut scratch.packed[..words] {
    *word = reader.u64()?;
}
```

`Reader` is the checked cursor every decoder in this crate reads through, and it is checked for a good reason: the bytes came off a disk that has been there longer than the process has, so a length field in them can say anything. What it costs is a bounds check, an eight byte copy into a local array, a `from_le_bytes` and a `Result` to test, and at 62 bits that is one of each for about every row of the column. The loop was one word short of one instruction per bit.

So the cursor now has a `words` method that takes the whole unit as one range and converts it in place, which is one bounds check for the unit and a loop of eight byte copies that a little endian machine does as a copy of the range and nothing else. Three call sites in `crates/rudb-encoding/src/integer.rs` had the same loop and all three take it.

Everything below is server2, one thread, over a hits corpus of 999,975 rows, counted at ring 3, with the stored answers off.

## What one bounds check a row was worth

| 999,975 rows | before | after | |
| --- | --- | --- | --- |
| `SUM(UserID)` | 32.2 M | 21.5 M | 0.667x |
| `AVG(UserID)` | 32.4 M | 21.7 M | 0.669x |
| `MIN(UserID)` | 26.9 M | 16.2 M | 0.602x |
| `SUM(UserID), SUM(ResolutionWidth)` | 54.7 M | 41.7 M | 0.762x |
| `SUM(ResolutionWidth)` | 24.2 M | 21.9 M | 0.906x |
| `COUNT(*)` | 1.3 M | 1.3 M | 1.000x |

Every query that reads `UserID` is 10.7 M instructions lighter, which is 10.7 a row and does not depend on what the query then does with the column. `ResolutionWidth` is packed at 12 bits and loses 2.3 M, which is the same saving scaled by how many words a row its width asks for.

ClickBench over 43 queries, one query per process:

| | before | after | |
| --- | --- | --- | --- |
| suite | 6.04 G | 5.79 G | 0.958x |
| q4, `AVG(UserID)` | 32.4 M | 21.7 M | 0.669x |
| q11 | 49.7 M | 38.7 M | 0.778x |
| q12 | 59.5 M | 48.2 M | 0.810x |
| q14 | 91.4 M | 79.7 M | 0.873x |
| q32 | 127.6 M | 111.7 M | 0.876x |

Nothing reads above 1.000x and 38 of the 43 read below it, which is what a cost paid per word of every packed column in the file looks like when it comes off. TPC-H at SF1 is 11.84 G to 11.75 G, 0.993x, with q02 0.961x, q19 0.976x and q11 0.977x the best of it and nothing worse than 1.000x. All 43 ClickBench answers and all 22 TPC-H answers are unchanged.

## Against DuckDB, and what is left

The nine ungrouped aggregates of [`74`](74-a-minimum-that-was-taken-in-a-hundred-and-twenty-eight-bits.md) against DuckDB v2.0.0-dev on the same corpus, one thread:

| | DuckDB | rudb | |
| --- | --- | --- | --- |
| `COUNT(*)` | 3.5 M | 1.3 M | 0.372x |
| `SUM(UserID)` | 26.2 M | 21.5 M | 0.819x |
| `AVG(UserID)` | 26.6 M | 21.7 M | 0.814x |
| `MIN(EventDate), MAX(EventDate)` | 5.5 M | 5.4 M | 0.985x |
| `MIN(UserID)` | 4.3 M | 16.2 M | 3.738x |
| all nine | 0.14 G | 0.14 G | 1.042x |

A sum of a `BIGINT` column is now faster than DuckDB's, which decodes nothing because it stores that column uncompressed and which accumulates into a hugeint because that is what the type rules say the total is. The probe suite as a whole went from 1.323x to 1.042x.

`MIN(UserID)` is the one left, at 16.2 M against 4.3 M. DuckDB reads eight bytes a row out of a file and takes a minimum of them in four and a half instructions a row; rudb reads 7.69 bytes a row, unpacks them out of a 62 bit frame of reference layout, and spends sixteen. A profile of it now has `__memmove_avx_unaligned_erms` at the top with 28 percent, `bitpack::unpack_mapped` at 8 percent, the aggregate at 7 percent, `integer::decode_chunk` at 6 percent and `__memset_avx2_unaligned_erms` at 5 percent, so what is left is not one loop but the shape of the decode: a buffer zeroed before it is written, written once, and copied once more on the way to the caller. Those three are worth about four instructions a row between them and none of them is arithmetic anybody asked for.

Worth writing down beside that: a repeat of the same query in the same session costs 11.0 M rather than 16.4 M, so 5.4 M of a cold `MIN(UserID)` is paid once. That is the same shape of number as the 1.4 M first touch cost note 72 found for the statistics, measured a different way, and the two should be looked at together.
