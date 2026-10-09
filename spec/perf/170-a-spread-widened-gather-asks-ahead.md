# 170. A spread widened gather asks ahead

## What was slow

`gather_widened` reads an integer column at the rows a filter's selection or a link join names and widens each value to an `i64`, which is how a join probe reads its keys out of a vector that is a selection over a stored column. When those rows are far apart each read misses the cache, and the loop waited out every miss in turn. The other gathers of the vector crate, `picked` and `Packed::values_at`, already ask for the line some rows ahead when the rows are spread, and this one did not. On q16 it was about 8 percent of the samples, nearly all of it the wait on `ps_partkey` at the rows the probe found.

## The change

When the first and the last row named are at least four times as far apart as there are rows, the gather asks for the line of the row `PREFETCH_AHEAD` places on before it reads each one, the same test and the same distance `picked` uses. Rows close together take the plain loop as before.

## Measured

At SF1 on server2, one thread, with four interleaved recordings of sixty runs of q16 in one process at 1,500 samples a second, against the build of #2892. The machine was too loaded for a difference of a percent or two in cycles a run to show, so the table counts the samples that landed in the gather.

| build | samples in the gather | samples in all |
| --- | --- | --- |
| main | 149, 150 | 2,057, 1,908 |
| this change | 114, 122 | 1,997, 2,101 |

The prefetch costs instructions. q16 goes from 109,375 to 111,757 thousand instructions a run, q05 from 376,126 to 378,213 and q03 from 69,520 to 69,790, and no other query moves by more than 0.3 percent.

The answers to all 22 queries are the same bytes as before at one thread and at six.
