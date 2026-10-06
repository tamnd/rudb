# 129. Distinct values held without reading their sets, selections gathered at their codes, and a few keys ranked

## What was slow

After 128, three things in q16 at SF1 still cost more than the work they did.

The first is offering a row's supplier to its group's distinct set. Since 127 the value is held and given out later in set order, but each row still went through the general path first, which read the set's entry in `seen` to learn whether it was a BIGINT set. That entry is a cache miss a row, and it was what 127 set out to avoid. `Aggregate::distinct` was about 4 percent of the query on its own.

The second is the gather of a filtered column. A filter that keeps some rows of a chunk leaves each column as codes into the column it did not copy. A join's probe then gathers the rows it matched out of that, and the gather took the general copy, a position at a time through the chain. The column under the codes is flat, and a flat column has a gather that is one load a row.

The third is the `NOT IN` over suppliers whose comment names complaints. That side is five rows at SF1 spread over ten thousand keys, and a million at SF100. The ranked join index lets a key span 256 places per row, so five rows got 1,280 places and the side was hashed. Every line of `partsupp` that got past `part` paid a hash and a probe of the table, and `Table::probe_at` was about 4 percent of the query.

## The change

When a distinct set is already holding values, its call has no filter of its own, and its argument is a flat run of integers, a chunk's rows go straight onto the held values. Each value is the row's set index and its value, and the rows with no group or a null are skipped. Nothing reads `seen` until the held values are given out in set order.

Gathering a dictionary that is not stable and has no nulls of its own is now its values gathered at the codes the positions name. That is one pass over the positions for the codes and the flat column's own gather.

The ranked join index now also takes any side whose key spans at most 1,048,576 places, whatever its number of rows. At that size its bits and counts are 192 KB, under half of a core's second level cache. q16's suppliers are ranked, and a probe is a bit test and a count.

## Measured

At one thread at SF1 on server2, against main at #2630, eight runs of q16 in one process, four runs of each binary interleaved. The server's load average was around 20. The answers to all 22 queries are the same bytes as before, at one thread and at six.

| binary | instructions (M) | cycles (M) |
| --- | --- | --- |
| main | 1,333 | 971 |
| this change | 1,203 | 909 |

In the profile `Table::probe_at` and `Aggregate::distinct` are gone, and holding the rows is about 1 percent. Over one run of each of the other 21 queries the instructions moved by less than one percent. q02, q05, q08 and q17 went down by 0.3 to 1.2 percent, because they gather filtered columns through a join too.
