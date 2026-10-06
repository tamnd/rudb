# 130. Null checks asked only of a column that can hold a null

## What was slow

Two places in q16 at SF1 asked every row whether it was null when the column could not hold one.

The first is the mark join for `NOT IN`. Before it answers, it looks at the driving side's key for nulls, because a null key turns a miss into a null rather than a false. It asked one row at a time through `is_null_at`, and the key had come through a filter, so each ask read through the filter's codes to the column under them. Almost every line of `partsupp` misses the five suppliers with complaints, so the walk went over nearly the whole side.

The second is the sort that ranks a key. It built a validity mask from the key by asking each row the same way, also through the codes of a filtered column, even when the key had no nulls at all.

## The change

Both now ask `lookup::has_nulls` first, which reads the column's validity, and for a dictionary or a run its codes or runs, once for the whole column. A key with no nulls skips the walk in the mark join and gets an all valid mask in the sort.

## Measured

At one thread at SF1 on server2, against main at #2631, eight runs of q16 in one process, four runs of each binary interleaved. The server's load average was around 20. The answers to all 22 queries are the same bytes as before, at one thread and at six.

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 1,203 | 953 |
| this change | 1,146 | 928 |

The cycles moved by less than the noise on the server, and the instructions went down by about 5 percent. Over one run of each of the other 21 queries the instructions did not move.
