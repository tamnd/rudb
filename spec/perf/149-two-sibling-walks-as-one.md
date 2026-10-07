# 149. Two sibling walks as one

## What was slow

TPC-H q21 keeps a late line when another line of its order has a different supplier and drops it when another line of its order with a different supplier was also late. Both are sibling walks (see notes 133 and 138), and the plan puts the `EXISTS` straight on top of the `NOT EXISTS`. Run one after the other, the two walks looked up the same order in the key map, found the same lines through the link, and gathered `l_suppkey` at them twice.

A second cost showed up in the cold run. The page pool holds a decoded part for a table the statement reads twice, so that the second read does not decode it again. Counted off the plan, `lineitem` is read three times in q21, so every column of it was held. But the walks never read the order key, which the link and the key map answer, and they read only the columns their conditions test. So the pool held decoded parts of columns that were read once and then dropped.

## The change

A sibling walk whose input is another sibling walk, over the same link and key map, from the same key, where both compare a column of the sibling with the row, now runs as one operator. Each row's parent is looked up once and its children found once. The columns either walk reads are gathered once, as the union of the two lists, and each comparison then runs over the columns it needs, in its own order, with its own conditions on the sibling alone. A row is kept when it passes both, which is what the two operators in a row did, since each one kept or dropped a row on its own siblings and nothing else.

The planner now tells the page pool which columns are read twice rather than which tables. A scan the walk replaces counts only the columns the walk reads, so in q21 the order key of `lineitem` counts once and is no longer held.

## Measured

At SF1 on server2, one thread, against main at #2711.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q21, warm | 258 | 248 |
| q21, a cold single run | 430 | 383 |
| q21 with only the `NOT EXISTS` on the key, a cold single run | 227 | 216 |

The other queries run as before. The semi join in q21 only ever saw the lines the anti join kept, so most of the warm walk is still the anti join's, and that is where the next change goes. The answers to all 22 queries are the same bytes as before at one thread and at six.
