# The rows a filter kept

Measured on server3 on 23 September 2026, TPC-H SF1 in the native format, instructions retired, best of three with a fresh process per run and every binary interleaved. Each pair was built out of one tree with the changed files overwritten by their base versions and rebuilt, so the two binaries differ in those files and in nothing else.

This note writes down two things that came out of the last two changes, #1353 and #1366. Neither is a faster loop. Both are the engine doing work for a shape it was not going to see, and both are the kind of thing that stays hidden until a profile is read with the plan open next to it.

## Room nobody was going to use

The presize pass in `rudb-opt` works out how many groups an aggregate can make from the column statistics, and the executor takes that much room in its hash table up front, so the table does not grow and rehash its way there. On q18 that made the query slower by a quarter and its peak memory seven times larger.

There were two mistakes and the larger one was in the executor. The partitioned aggregate has three kinds of table. Each instance has a table it fills before it decides to split, which it gives up at 4096 groups. Each instance then has 64 tables of its own, one per partition, which only pass rows on. And there are 64 shared tables the partitions are merged into, which are the only ones that hold the groups at the end. The executor was giving the full estimate to the first kind and a sixty fourth of it to each of the second, on every instance, so a machine with many threads took the room many times over in tables that would never hold more than a few thousand rows.

Now only the tables that hold the groups take room in advance. The table before the split takes no more than the 4096 it can hold before it is given up, and the per instance partition tables take none.

The smaller mistake was in the pass. It bounded the groups by the product of the key's distinct counts and by the rows arriving, and took the smaller. Where the rows arriving are the smaller bound, the pass has learned nothing about the key, and the number it hands on is a guess at the input size, which the table grows to anyway. It now asks for room only where the key's own count is the tighter bound.

| | before | after | pass off |
| --- | --- | --- | --- |
| TPC-H SF1, all 22 queries, G | 53.039 | 51.172 | 51.242 |
| q18, G | 7.552 | 6.072 | 6.018 |
| q18 peak memory, MiB | 1182 | 174 | |
| q18 inner group by alone, G | 5.134 | 3.744 | |
| q18 inner group by peak memory, MiB | 1737 | 170 | |

The honest reading of the last column is that the pass now costs nothing on TPC-H and gains nothing there either. Its groups are either small enough that growing costs little or large enough that the rows are the tighter bound. Whether it earns its place has to be measured on a group by with a key that is known to be selective and large, and ClickBench has those.

## A dictionary of the rows that got through

A filter does not copy the columns it keeps. It hands each one on as a dictionary vector whose codes are the rows that passed, pointing into the column as the scan produced it. That is the right design for the filter, because a filter over five columns that feeds a projection of two would otherwise copy three columns for nothing.

It has a cost the kernels downstream have to know about. A kernel with a fast path for a flat column or a bit packed column and a general path for everything else never takes the fast path behind a filter, because behind a filter nothing is flat. On TPC-H most scans have a filter over them, so most of the time the general path is the only one that runs.

`table::hash` was the first place this was found. It is the largest single symbol on the suite and its fast paths were for flat and packed columns. Behind a filter it went through a closure per row that matched on the form, followed the code and read the value. #1366 teaches it to follow the codes into the flat or packed payload directly, when neither side has a null and the payload is not more than four times the rows kept.

The first version made q17 seven percent worse. `fold` stopped being inlined into `hash`, and the packed loop kept two flags that are the same for every row on the stack and tested both each row. The loop is now one generic function copied four ways on those two flags, so neither test is in it.

| query | before G | after G | duckdb G |
| --- | --- | --- | --- |
| q03 | 1.800 | 1.716 | |
| q04 | 1.611 | 1.535 | |
| q17 | 1.864 | 1.824 | |
| q21 | 5.589 | 5.401 | |
| all 22 | 51.161 | 50.690 | 27.059 |

Sixteen queries went down and none went up by more than one percent.

## What this means for the rest of the engine

The lesson from the second half is general. Any kernel whose fast path asks whether a column is flat is asking a question a filtered query answers with no. The fix is the same every time: a dictionary whose codes are rows is a flat column read through an index, and the fast path takes an index as easily as it takes a position.

The places to check next are the ones a filtered TPC-H query goes through after the hash. Those are the group key readers that load a block of integers at a time, the aggregate update loops, the join probe's key compare, and the expression kernels under a projection. Each of them should be measured before and after the same way, since the saving depends on how much of the query sits behind a filter, and the hash showed that a change which saves work on paper can lose it again in the generated code.

The other way to close this is for the filter to compact when it has kept few rows and the next operator is one that benefits, which is what `spec/07-execution.md` section 7.1 already says and what the engine does not yet do. Reading through the codes is the better default when most rows survive, and compacting is better when few do, so both are wanted and the threshold between them is a number to measure rather than pick.
