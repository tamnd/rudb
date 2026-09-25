# A minimum that was taken in a hundred and twenty eight bits

ClickBench q7 is `SELECT MIN(EventDate), MAX(EventDate) FROM hits`. Over a million rows it cost 47.2 M instructions where DuckDB spends 5.4 M, which made it rudb's worst query in the suite by a factor of nearly nine, and [`72-every-rule-off-one-at-a-time.md`](72-every-rule-off-one-at-a-time.md) is why it was the thing to look at: the planner has nothing left to give ClickBench, so the suite has to be earned in the kernels. A profile of sixty repeats of that query in one process put 57 percent of it in one function, `collect` in `crates/rudb-kernels/src/aggregate.rs`, which is the loop every ungrouped aggregate over a flat column goes through.

Everything below is server2, one thread, over a hits corpus of 999,975 rows, counted at ring 3 as [`68-the-counter-that-was-not-load-immune.md`](68-the-counter-that-was-not-load-immune.md) requires, with the stored answers off because a whole table extreme answered out of a file header is not a loop.

## What the loop was

Two things, and each of them on its own is enough to stop a vector register being used.

The running best was an `i128`. One loop was written for every width and every row was widened into it, which is a comparison the hardware does in a pair of instructions and which the compiler will not put in a vector lane at any width. The widening bought one loop for nine types and it cost that on every row of all nine.

The row number of the best so far was carried alongside it. An extreme has to hand back a row rather than a number, because the value is built once per vector by `try_value_at` and compared through the ordering kernel, which is what keeps a `DATE` a date and a decimal a decimal. So the loop kept the winning row, and a row number is carried from one row to the next with no lane to carry it in, which means a loop that keeps one is scalar however wide its values are.

## What it is now

The best so far is held as the column's own type. The loop is written once and compiled per width, which is what a generic is for, and the direction is a constant rather than an argument so that the body is a plain minimum or a plain maximum and the vectorizer recognises both.

The row number is not carried at all. A block of a thousand rows is reduced to a value, and only a block that improved on what came before is searched for the row holding that value. Any row holding it answers, because two rows with the same number in them build the same value, so the second pass is a search rather than a record of where the first pass had got to. A run that arrives in the order the extreme wants is the case that searches every block, and that is one vectorized pass over everything plus one scalar pass over a sixteenth of a vector at a time.

The null mask path and the dictionary path keep their row at a time loops and get the native width, which is all they were asking for.

## What it bought

| 999,975 rows, one thread | before | after | |
| --- | --- | --- | --- |
| `MIN(EventDate), MAX(EventDate)` | 47.2 M | 5.4 M | 0.115x |
| `MIN(UserID)` | 46.8 M | 26.9 M | 0.576x |
| `SUM(UserID)` | 33.0 M | 32.2 M | 0.977x |
| `AVG(UserID)` | 33.2 M | 32.4 M | 0.977x |
| `COUNT(*)` | 1.3 M | 1.3 M | 1.000x |

A date is thirty two bits and AVX2 has an instruction that takes the smaller of eight of them at once, which is why q7 comes down by a factor of nine and lands at 5.4 M against DuckDB's 5.3 M. A `BIGINT` is sixty four bits and AVX2 has no signed minimum at that width, so it is a compare and a blend for four values and the win is a factor of 1.7 rather than nine.

The three queries that add `UserID` up came down by 0.8 M as well. That is not a loop this touched and it is not being claimed as one: the sum loop is the same instructions in a different place in the binary, which is what a file with nine fewer 128 bit comparisons in it does to everything near them.

ClickBench, 43 queries, one query per process, with the stored answers off on both sides: q7 0.115x, q4 0.977x, every other query 1.000x, and the suite 6.08 G to 6.04 G, which is 0.993x. All 43 answers unchanged. TPC-H at SF1 reads 11.84 G on both sides with all 22 answers unchanged, which is the right answer for a suite whose extremes are over a handful of rows each.

## Where the rest of a minimum goes

Against DuckDB v2.0.0-dev on the same corpus, one thread:

| | DuckDB | rudb | |
| --- | --- | --- | --- |
| `MIN(EventDate), MAX(EventDate)` | 5.3 M | 5.4 M | 1.015x |
| `MIN(UserID)` | 4.5 M | 26.9 M | 6.044x |
| `SUM(UserID)` | 26.2 M | 32.2 M | 1.231x |
| `AVG(UserID)` | 26.6 M | 32.4 M | 1.219x |
| `COUNT(*)` | 3.5 M | 1.3 M | 0.372x |

q7 is at parity and `MIN(UserID)` is six times behind, and the profile says the aggregate is no longer the reason. Of the 26.9 M, `rudb_encoding::integer::decode_as` is 33.9 percent, `bitpack::unpack_mapped` 11.9 percent and `integer::decode_chunk` 9.5 percent, which is 55 percent of the query spent turning a bit packed column into a run of `i64`, against 6.1 percent in the aggregate that reads it. DuckDB takes a minimum over a million `BIGINT`s in four and a half instructions a row, so twenty two instructions a row of decode is the whole of the difference.

That number is under the three whole table sums as well, which is most of what is left of rudb's ClickBench losses, and it is the same decode that q10 and q19 pay per row while grouping. So the next thing to attack is the decode of a high cardinality integer column, not another kernel.

One more thing worth keeping out of that table. DuckDB takes a minimum of a `BIGINT` column in 4.5 M and a sum of the same column in 26.2 M, because a sum of a `BIGINT` is declared `HUGEINT` and is accumulated in a hundred and twenty eight bits. That is the cost this note just took out of the extreme, still being paid by both engines in the sum, where it is the arithmetic rather than a convenience.
