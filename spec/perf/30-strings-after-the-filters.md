# Strings after the filters

Notes written on 24 September 2026, after #1607, while looking at q10, which took 1.75 G instructions and 173 ms of CPU at SF1 against DuckDB's 1.38 G and 143 ms.

## The question

q10 joins lineitem and orders, then joins the result to customer and groups by seven customer columns, four of them strings. A profile of it run in a loop put the largest share of the busy samples in `SymbolTable::decompress`, and nearly all of it came from the customer scan. The join hands that scan an exact bitmap of the 37,967 customers the orders name, which is a quarter of the table, and the scan used it only after it had read every column of the part. So the name, address, phone and comment of all 150,000 customers were decompressed and checked as text, and three quarters of that was then thrown away when the part was narrowed.

## What changed

A scan with a filter of any kind now reads first only the columns that are not strings, plus any string column the pushed filter or a join's filter reads, with a null in place of each string it left out and a row number on the end. The filters run over that exactly as they ran over the whole part. The row numbers that survive are the rows kept, and the string columns are then read at those rows alone.

A compressed text page keeps the compressed length of every value, so reading it at some rows steps over the rest by adding their lengths and decompresses only what was asked for, and only those rows are checked as text. Every other page is decoded whole and gathered, which is what reading it and narrowing it cost before. A part whose rows were all kept is read the ordinary way.

The scan counts what its filters keep, and once they are measured keeping more than three quarters of the rows it goes back to reading the whole part at once. Without that, q01 got 7 percent more instructions, because its date filter keeps nearly all of lineitem and the two flag columns were being read a second time for nothing.

## Measured

Five runs of each query at SF1, instructions, both built without the size setting #1589 put on the CLI crate.

| query | before | after | DuckDB |
|---|---|---|---|
| q02 | 0.187 G | 0.166 G | 0.349 G |
| q10 | 1.743 G | 1.448 G | 1.377 G |
| q20 | 0.833 G | 0.821 G | 0.952 G |
| suite | 23.87 G | 23.57 G | 25.38 G |

In CPU time over ten runs, q10 went from 173 ms to 156 ms against DuckDB's 143 ms. No other query moved by more than noise, and the answers of all 22 queries are the same bytes as main.

## What is left in q10

The aggregate still groups 114,705 rows on seven columns, four of them strings, when the sum only depends on the customer key. Summing by `o_custkey` before the join to customer would group on one integer and leave 37,967 rows for the join and the final grouping. That is eager aggregation, and it is the next note.
