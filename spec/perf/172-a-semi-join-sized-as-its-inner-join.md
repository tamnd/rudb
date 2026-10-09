# 172. A semi join sized as its inner join

## What was wrong

A semi join was estimated as a flat fifth of its left side, whatever the right side was. A semi join produces each left row at most once, and only when the inner join over the same conditions would produce it, so it can never be more than the left side and never more than that inner join. The fifth ignored both bounds. On q17 the semi join that the keys pass puts under the decorrelated aggregate keeps the lineitem rows whose part is one of the 198 parts the outer query asks about, and it was estimated at 1,200,243 rows when it makes 6,088. The aggregate over it was then sized for 120,024 groups and its plan reserved room for 200,000 of them, to hold 204.

## The change

`estimate::join` sizes a semi join as the smaller of its left side and the inner join over the same two sides and conditions, when both are known, and falls back to the fifth when they are not. The inner join is sized by the same code every inner join is, so a semi join to a filtered dimension table keeps the share of the fact table that the filter keeps, and a semi join to a whole dimension table keeps all of it. An anti join keeps the fifth for now.

The keys pass decides whether to copy a source under an aggregate by comparing the source's rows to the rows the aggregate reads. On q20 the source is partsupp semi joined to the `forest%` parts, and it used to come in under that comparison only because a join sized by default skipped it. It is now estimated at 9,432 rows against 8,508 real ones and passes the comparison on its own. Its unit test had a filter of default selectivity standing for `forest%`, which made the source too large to be worth copying, so the test now gives that filter a distinct count that keeps one row in a hundred, about what `forest%` keeps of part.

## Measured

At SF1 on server2, against main at #2896.

| query | node | before | after | real |
| --- | --- | --- | --- | --- |
| q17 | semi join under the aggregate | 1,200,243 | 5,941 | 6,088 |
| q17 | the aggregate | 120,024, room for 200,000 | 594, no room reserved | 204 |
| q20 | partsupp semi joined to part | 160,000 | 9,432 | 8,508 |
| q20 | semi join under the aggregate | 173,604 | 10,233 | 9,741 |

No query moves by more than 0.4 percent in instructions a run at one thread: q17 goes from 77,625 to 77,336 thousand and q20 from 126,460 to 126,767. Cycles on q17 are within the noise of the machine in both orders. At SF1 the aggregate on q17 is still addressed directly over all 200,000 part keys, because that choice reads the key range and not the rows arriving, so what this buys is a correct number for the passes that do read it.

The answers to all 22 queries are the same bytes as before at one thread and at six.
