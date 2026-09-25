# A unit unpacked where the chunk holds it

[`75-a-packed-unit-copied-eight-bytes-at-a-time.md`](75-a-packed-unit-copied-eight-bytes-at-a-time.md) took the packed unit of a bit packed chunk out of the checked cursor as one range instead of a bounds check a word, and ended on a profile of `SELECT MIN(UserID) FROM hits` in which `__memmove_avx_unaligned_erms` was 28 percent of the query. That memmove was the range it took. The unit was copied into a `Vec<u64>` held for the thread and unpacked out of the copy, so the decode wrote 8 KB per thousand rows that nothing ever read twice. This is that copy.

It was there for one reason, which is that `unpack_mapped` takes a `&[u64]` and the unit inside a chunk is not eight byte aligned. A unit follows a tag, a row count, a frame of reference base and a width byte, and the chunk itself begins wherever the page put it, so the words of a unit start at whatever offset they start at. rudb-encoding forbids unsafe code, so there is no cast of those bytes to a `&[u64]` and the words had to be moved somewhere aligned before the unpacker would look at them.

What that missed is that the unpacker never wanted the alignment. It reads one word per lane, and `bitpack` already had `word_at` for reading a word out of bytes:

```rust
fn word_at(input: &[u8], at: usize) -> u64 {
    let run: [u8; 8] = input[at..at + 8].try_into().expect("eight bytes");
    u64::from_le_bytes(run)
}
```

On every machine this runs on that is a single unaligned load, which is the same single instruction the aligned load was. The copy was paying for an alignment that bought nothing.

So `unpack_unit_into` takes the unit as `&[u8]` and reads every word where the chunk holds it. The three call sites in `crates/rudb-encoding/src/integer.rs` went from a copy and an unpack to a range and an unpack:

```rust
let unit = reader.bytes(bitpack::unit_len(width))?;
bitpack::unpack_unit_into(unit, width, into, |offset| value_from(offset, base))?;
```

With the copy gone, everything that existed to hold it goes as well: the `Decoding` struct, the thread local it lived in, the `with_decoding` wrapper that borrowed it, the `ready` call that sized it, and the `scratch` parameter that `decode_chunk`, `decode_chunk_as` and `decode_selected_chunk` all carried so that a nested chunk could reach it three levels down. `Reader::words`, which note 75 added, has no caller left and goes too. A cascade of a dictionary of deltas now reads the packed bytes of each level out of the chunk where they lie, and there is no buffer for the levels to take turns in.

The word form of the unpacker stays, with no caller outside the crate's own tests. It is what the byte form is checked against at every width from 0 to 64, and since what both do is reproduce a transposed layout exactly, two independent spellings of it are what say either one is right.

Everything below is server2, one thread, counted at ring 3, with the stored answers off. Both sides are built from one tree, which the last section of this note explains at some length.

## What the copy was worth

Nine ungrouped aggregates over a hits corpus of 999,975 rows:

| 999,975 rows | before | after | |
| --- | --- | --- | --- |
| `SUM(UserID)` | 21.6 M | 19.9 M | 0.925x |
| `AVG(UserID)` | 21.7 M | 19.5 M | 0.898x |
| `MIN(UserID)` | 16.3 M | 14.7 M | 0.900x |
| `SUM(ResolutionWidth)` | 21.9 M | 20.5 M | 0.935x |
| `SUM(UserID), SUM(ResolutionWidth)` | 41.7 M | 38.8 M | 0.930x |
| `COUNT(*)` | 1.3 M | 1.3 M | 1.000x |
| all nine | 0.14 G | 0.13 G | 0.931x |

A column packed at 62 bits is about eight bytes a row of copy and it comes to about 1.6 instructions a row, which is what a memmove of eight bytes costs when it is one of thousands in a row and the destination is already hot. `ResolutionWidth` is packed at 12 bits, so it copies a fifth as much and saves a fifth as much.

ClickBench over 43 queries, one query per process:

| | before | after | |
| --- | --- | --- | --- |
| suite | 5.79 G | 5.70 G | 0.985x |
| q4, `AVG(UserID)` | 21.7 M | 19.5 M | 0.898x |
| q3 | 31.4 M | 30.1 M | 0.957x |
| q11 | 38.7 M | 37.1 M | 0.959x |
| q12 | 48.2 M | 46.4 M | 0.963x |
| q25 | 15.3 M | 14.8 M | 0.965x |

Nothing reads above 1.000x and 40 of the 43 read below it. TPC-H at SF1 is 11.39 G to 11.25 G, 0.988x, with q06 0.960x, q19 0.949x and q12 0.975x the best of it and nothing above 1.000x. All 43 ClickBench answers and all 22 TPC-H answers are unchanged.

## A narrowing that reads the same either way

The same branch carries a second change that is worth writing down for what it did not do. `decode_chunk_as` decodes the four kinds it can write directly into the column's own type, and hands every other kind to the wide decoder and narrows the result. That narrowing was a checked conversion per value, except for strided chunks, which took the lowest and highest value of the chunk once and then narrowed with no check at all when both ends fit. Every wide kind takes that path now, which for a `BIGINT` column makes the narrowing a walk over a vector that stays where it is, because the standard library reuses the allocation when the two widths match, where the checked form collected a second vector and copied every row into it.

It reads 1.000x on all nine probe queries, 1.000x on all 43 ClickBench queries to the last digit printed, and 1.000x on the five TPC-H queries it was measured against on its own. It stays because it is less work per value and one arm of a match instead of two, but nothing in either suite decodes enough dictionary or delta chunks into a narrow type for it to show.

## The baseline that was a different tree

The first measurement of this branch said TPC-H q18 went from 792.5 M to 419.2 M instructions, 0.529x, on a change that touches nothing but the integer decoder. That should not have been believed for as long as it was. Cut down, the win was in `SELECT l_orderkey, sum(l_quantity) FROM lineitem GROUP BY l_orderkey`, which read 679.4 M against 307.0 M, and a profile of the slow side had 25 percent of the query in `group::interior::bounds` while the fast side had none of it there and 10 percent in `group::run_starts::starts` instead. Two builds of the same source do not take different paths through the grouping of a chunk.

They were not the same source. The before binary had been built by applying note 75's diff to a tree that was a few commits behind, and one of the commits it was behind was the one that added `run_starts`, whose own doc comment says that the two passes it replaces were half of the grouping of `lineitem` by order on q18. The 0.529x was that commit, measured by a session that did not know it was measuring it.

Rebuilt from one tree, with the branch applied and reverse applied and nothing else different, q18 reads 430.8 M to 419.2 M, 0.973x, which is the decode saving and nothing else. The suite reads 11.39 G to 11.25 G rather than 11.75 G to 11.25 G, because 11.75 G was that older tree's total and current main is 0.36 G below it for the same reason q18 is.

Note 68 made the instrument trustworthy and this is the other half of the same lesson, so it goes beside it: the number is only about the change if both sides come out of one tree, built one after the other, with the diff applied and reverse applied. A binary kept on the box from last week is not the baseline it is labelled as, however carefully it was built, because main moves under it. `scripts/instructions` takes two binaries and cannot check this, and the cheapest guard is to build both of them in the same minute, which costs two builds and would have cost this note an afternoon less.

## What is left of the decode

The profile of `MIN(UserID)` that note 75 ended on had memmove at 28 percent, `unpack_mapped` at 8, the aggregate at 7, `decode_chunk` at 6 and memset at 5. The same profile now has `unpack_unit_into` at 18 percent, `decode_chunk` at 18, memmove at 2.3 and memset at 2.0, which is the same query with the copy taken out of the middle of it and the unpack and the fold left standing.

`MIN(UserID)` is 14.7 M against DuckDB's 4.3 M over a million rows. What is left in the decode is the zeroing of the buffer the values are written into, which is `vec![0i64; count]` and one memset of the chunk before the unpack writes all of it, and the handing of that buffer to the caller, which is a move rather than a copy but arrives as a fresh allocation per chunk. Neither is arithmetic anybody asked for. Beside them is the 5.4 M that a cold `MIN(UserID)` pays and a repeat in the same session does not, which is still unexplained and is the same shape of number as the 1.4 M first touch cost note 72 found for the statistics.
