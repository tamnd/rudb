# 105. A key is its own link

## The problem

q14 joins `lineitem` to `part` on `l_partkey`, and `EXPLAIN` on server2 says it builds a hash table because "the relationship is declared and its link is not in the file". `rudb_links()` says why: the link from `lineitem(l_partkey)` to `part` was measured at 13.6 MB and turned away by the budget, and so was the 10.6 MB one to `supplier`. Lineitem's ten percent went to the link to `orders` and the 15 MB one to `partsupp`. So q14 built a hash table over 76 thousand `lineitem` rows, scanned 63 thousand `part` rows to probe it, and spent 16 ms of CPU there.

Neither link holds anything the file did not already have. `part`, `supplier`, `customer`, `nation` and `region` all have a key map in the identity form, which means the row of a key is the key less the smallest one. A link to one of them is a packed copy of `key - 1`, and the key is already in the child's rows. Spending the budget on it bought nothing, and losing it cost a hash join.

The link has a second limit. It is indexed by the child's row id, so it can only be read while the rows reaching the join are still rows of the stored child table. Once the child has been through the build side of another join, the rows lost their row id and the join goes back to a hash table, even when the parent would answer by key.

## The change

A link join can now find the parent by key. When the parent's key map is the identity form, the planner rewrites the join to a `LinkJoin` that reads the child's key, cast to `BIGINT`, in place of a row id. The operator turns each key into `key - base` and checks it is under the parent's row count, and a null key or one out of range has no parent. Everything after that is the same as reading a link: the inner kind drops the rows with no parent, the left kind gathers null for them, semi and anti test it, and the parent's columns are gathered from the stored parts.

This needs no section in the child's file, no row id column in the child's scan and no stored child at all. The rewrite only needs the child's key column to name the declared relationship, and it finds that column's scan anywhere under the join. It is chosen whatever the parent's size, since nothing is built. The one check it keeps is that something above the join names its columns, because the link join puts the child's columns first where the hash join might have put the parent's.

The planner reads the form off the key map's section flags, so it costs nothing to ask. The plan prints `key=` in place of `rid=`, and `EXPLAIN` says the join "reads the key map".

## Results

Pending the release build, posted on the pull request.
