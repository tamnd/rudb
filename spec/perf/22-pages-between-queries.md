# Pages between queries

Notes written on 23 September 2026, after #1517, while looking for the reason rudb does more work than DuckDB on TPC-H SF1 when both run the same plan.

## The question

A profile of q12 run in a loop put 12 percent of the samples in reading pages off the file and checking their checksums, and about 22 percent in decoding them and copying the results. Each reader lives as long as its database, but its page cache held four stripes a column and let the oldest go first, so on a table the size of lineitem every query read every page again. The storage spec in [`../05-storage.md`](../05-storage.md) section 5.5 asks for a buffer manager with one budget for the whole database, and there was none.

The question for this note was how much of the gap to DuckDB that explains, measured before anything was built rather than after.

## Measuring it

Three builds were run over the 22 queries five times in one process, on a laptop with ten cores that was busy with other work, so the wall times were too noisy to read and the numbers below are CPU seconds from `time`.

| build | user | system |
|---|---|---|
| main after #1517 | 15.2 to 16.4 | 0.91 to 1.00 |
| every page kept, nothing let go | 15.4 to 16.7 | 0.37 to 0.45 |
| every page kept and no checksum at all | about the same as the line above | 0.45 |
| every decoded part kept as well | 13.7 to 14.9 | 0.65 |

Keeping the pages takes two thirds off the system time, which is the `pread` calls, and leaves user time where it was. Dropping the checksum entirely saves nothing that can be seen, so checking each part once instead of on every read is not worth its own change. Keeping the decoded vectors as well saves about 8 to 10 percent of user time. Keeping every page of the SF1 database costs 38 MB of resident memory on top of the 418 MB a run already uses.

Measured per query, with each query run five times in its own process, rudb spent 2,960 ms of CPU a pass against DuckDB's 2,481 ms. The difference is not spread evenly. q05 is 189 ms against 86, q09 is 364 against 254, and q01, q13, q18 and q21 are each about 45 ms over. All of those are joins and grouped aggregates, and none of them is reading pages.

So the profile overstated the storage share. Reading and decoding together are worth somewhere around 10 to 15 percent of the CPU, and the rest of the gap is in the operators.

## What changed

A `PagePool` in `rudb-native` holds one budget in bytes for every reader of a database. A reader still keeps its pages in one slot per stripe per column, which is what makes finding one an index rather than a walk, and each page now has a bit that a read sets. When a new page comes in and the pool is over budget, it walks from the oldest page once: a page with the bit set loses it and goes round again, and a page without it is let go. A new page comes in with the bit set, so the pass its arrival starts cannot take it before the worker that read it has used it.

The old count of four stripes a column is still there as a floor. The pool never takes a page from a column that holds no more than that, because a scan whose workers evict each other's pages reads a quarter of a megabyte for every part it takes. With a budget of zero the pool behaves as the cache did before it existed, which is what `Catalog::open` gives a caller that does not ask for more.

A database opens its file with `Catalog::open_in` and one pool, and hands the same pool to the catalog each checkpoint opens, so a checkpoint does not start a second budget. The budget is half the memory limit, and with no limit every page is kept, which is what DuckDB does. The pool holds its readers weakly, so the readers a checkpoint replaces take their pages with them.

The page's bytes are not counted by the query memory tracker yet. That is the second half of section 5.5, and it matters once a table is bigger than the budget, which on this machine means SF100.

## What it did not do

On the suite the system time fell from about 0.95 s to 0.30 s over five passes, and the answers of q01, q05, q09, q12 and q18 were the same bytes as before. The user time did not move and the wall time was inside the noise. That is the right size for what storage was worth, and it is why the next notes are about the join probe and the grouped aggregate, not about the scan.

Keeping decoded parts is the one storage change left with a measured payoff, and it is not free: a decoded part is bigger than its page, often by four to eight times, so it needs its own place under the budget and a reason to prefer it over a page. It waits until the operators have had their turn.
