# Keys by rank

Notes written on 24 September 2026, on the joins whose integer keys are too far apart for the direct index and so were hashed.

## The question

After note 46, building and probing the two tables in q21's subqueries was about 40 percent of the query. Each table holds about 155 thousand late lineitem lines keyed on the order key, and those keys are spread over six million values, about 38 values a line. The direct index in `crates/rudb-exec/src/lookup.rs` makes the key its own slot in the head array, so it takes a range of at most four values a line, or else the head would be mostly empty. Anything sparser went to the partitioned hash table: a hash per row on both sides, a stored key, a probe that compares keys and a collision to walk now and then. The same shape comes up whenever a filter keeps a scattered fraction of orders or customers and the join is on their key, as in q03, q05, q10 and q22.

## What changed

Between four and 256 values a row, the index now keeps a bitmap over the key range, one bit for each value that some gathered row holds, and a count of the set bits before each 64 bit word. A key's slot is its rank among the keys held: the count before its word plus a popcount of the bits below it in the word. That is a perfect hash with no hash to take, no stored key and no collision. The head array stays one entry a distinct key, the same as the hash table, and the chains and everything that reads them are unchanged. At 256 values a row the bitmap and the counts cost about 48 bytes a gathered row, about what the hash table costs, and beyond that the join still hashes.

The build is the direct one with slots in place of places. The gathered rows are dealt to partitions that each own a run of whole words, so a partition's share of the head starts at the count kept for its first word and each partition fills its share on its own thread in row order. A probe is a subtraction, a bounds check, a bit test and a popcount.

The unit tests in `lookup.rs` build the ranked form over enough rows to be split, with repeats, gaps and nulls, and check that every key's rows come out in the order the side holds them and that keys between keys or past either end miss. Another test puts keys on both sides of a word's edge so that the count and the popcount both matter.

## Numbers

On server3 against the native file at SF1, instructions best of three, main with note 46 before and this change after:

| | main | this change |
|---|---|---|
| q03 | 0.933 G | 0.823 G |
| q05 | 1.103 G | 0.964 G |
| q07 | 0.955 G | 0.877 G |
| q10 | 1.302 G | 1.257 G |
| q16 | 0.472 G | 0.429 G |
| q21 | 2.224 G | 1.854 G |
| q22 | 0.518 G | 0.379 G |
| all 22 | 21.698 G | 20.623 G |

18 of the 22 queries went down and none went up by more than half a percent. DuckDB takes 1.830 G on q21. All 22 answers are the same as main's.

Wall time and peak memory, best of five with server3 under a load of about 12:

| | main | this change | DuckDB |
|---|---|---|---|
| q03 | 0.11 s, 88 MB | 0.09 s, 78 MB | 0.18 s, 126 MB |
| q05 | 0.11 s, 88 MB | 0.09 s, 82 MB | 0.19 s, 129 MB |
| q21 | 0.19 s, 151 MB | 0.17 s, 137 MB | 0.20 s, 116 MB |
| q22 | 0.04 s, 41 MB | 0.04 s, 40 MB | 0.16 s, 66 MB |

## What this leaves

q21 is now within about 1 percent of DuckDB's instructions. It still holds more memory than DuckDB, which comes from the two gathered sides being kept whole while the driving side streams through them. Reading packed integers a row at a time in the domain filter and the null checks is the next cost on q21's driving side.
