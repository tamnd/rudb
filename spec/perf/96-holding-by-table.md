# 96. Holding by table in the last statement

## The problem

Measured by instructions, rudb did about half of DuckDB's work on most TPC-H queries, yet a warm run at six threads was only even with it on q03, q05 and q08. Instructions counted in user space miss the kernel, and `/usr/bin/time` showed system time as large as user time on q07, q08, q09, q18 and q21. A profile of q09 at one thread put half its samples in the kernel, nearly all of it faulting in and zeroing fresh pages. q07 took 38 thousand page faults, about 150 MB of new memory, for a query whose own data is a fraction of that.

The faults came from decoded parts that stayed alive. In the last statement a read holds what it decodes only when the statement reads a table twice, and that was one flag for the whole statement. q07 and q08 read `nation` twice, so every part of `lineitem` and `orders` they read once was held as well. Each part then decoded into memory of its own that stayed taken to the end, where let go after its chunk the next part would have decoded into the same memory.

## The change

The planner hands the pool the tables two scans of the statement both read, by name, rather than whether there are any. A reader asks for its own table with `PagePool::is_last_for`, so in the last statement it holds parts of a table only when another scan of the same table will read them. A table read twice is held as before, which is what q18 and q20 do with `lineitem` and q22 with `customer`. A pool following another shares the set. Dictionary blocks still go by the flag for the whole statement.

## Results

Measured on server3 at SF1, one run in a fresh process, against main at #2361. Instructions at threads 1 in millions, page faults at threads 1 and 8:

| query | instructions before | after | faults t1 before | after | faults t8 before | after |
|---|---|---|---|---|---|---|
| q07 | 287 | 282 | 38520 | 17933 | 42386 | 21704 |
| q08 | 234 | 228 | 39420 | 16889 | 40747 | 19531 |
| q20 | 343 | 342 | 17536 | 9735 | 19878 | 12005 |
| q21 | 442 | 448 | 32085 | 29226 | 37432 | 35288 |

System time on q07 at one thread went from 0.32 s to 0.21 s and on q08 from 0.34 s to 0.21 s, though server3 was loaded and those are rough. No other query moved by more than noise, and every answer is the same at 1 and 8 threads.

q18 and q09 still fault about 40 and 25 thousand times. q18 genuinely reads `lineitem` twice, and most of what it holds is `l_orderkey` expanded from its runs to eight bytes a row. A reader that hands out runs as runs is the next step there.
