# 95. What the last statement holds

## The problem

Between #2312 and #2353 main got much slower on several TPC-H queries with the same plans: q17 went from 72 M to 163 M instructions, q08 from 229 M to 335 M, q07 from 284 M to 373 M and q20 from 353 M to 433 M. Two changes made for JOB's warm runs did it together. A read of a few rows of a packed integer part pays `HOLD_RENT`, 8192 a row, toward holding the part, so the first read that touches a part holds it, and #2337 then writes a part held by a read of some of its rows out flat. Both buy something only for the reads that come later. rudb-bench starts a fresh process for every run of a TPC-H query, so the statement running is the last one, and the only later read is the same statement reading a table a second time, which q17 does at about three rows a part of `lineitem`. In q17 writing held parts out flat was a third of the query, most of it page faults, and decoding them whole on the first touch was another quarter.

## The change

`PagePool::is_final` says whether `last_statement` has been said, whether or not the statement reads a table twice. In that statement `paid_at` charges a read of a few packed integers `SPARSE_RENT` a row, what it costs, so a part is held only once this statement's own reads of it come to a whole decode, and `Reader::keep` keeps a held part packed. Statements before the last one, which is what a warm run is, pay and hold the way they did. A pool following another, the one a Parquet mirror reads through, hears `told` along with `last`.

## Results

Measured on server3 at SF1, one run in a fresh process, threads 1, millions of instructions, against main at #2353 built from the same base:

| query | before | after |
|---|---|---|
| q02 | 70 | 67 |
| q03 | 347 | 322 |
| q04 | 227 | 214 |
| q05 | 418 | 397 |
| q07 | 373 | 287 |
| q08 | 335 | 231 |
| q17 | 163 | 73 |
| q18 | 388 | 385 |
| q19 | 191 | 177 |
| q20 | 433 | 350 |
| q21 | 496 | 463 |

No other query moved. Every answer is the same at 1 and 8 threads. A new test checks the rent `paid_at` charges a packed page in and before the last statement.
