# A rewrite that read a node nothing runs

`../stats/07-graph-statistics.md` section 7.3 calls join elimination the largest single win available from a certificate, and G4's exit criterion asks what it is worth on a named query set. The keyed TPC-H SF1 corpus has all eight relationships built and every one of them carries both certificates, uniqueness verified and totality verified, so the licence is there on all eight. The ablation over the twenty two TPC-H queries, one binary with `stats_join_elimination` off against the same binary with it on, reported this:

```
all 22 answers unchanged
0 plans differ
```

Not one query in the suite moved by more than a tenth of a percent, which is the noise floor of the instrument. A rule that is on and does nothing is either a rule the workload cannot reach or a rule that is broken, and the first thing to do is find out which.

## The syntax was the whole difference

A set of twelve queries was written over the same schema to reach the three rewrites deliberately, and three of the twelve fired. The three that fired are written with `JOIN ... ON`. The nine that did not include `SELECT count(*) FROM lineitem, orders WHERE l_orderkey = o_orderkey`, which is the same join to the same parent over the same certificate as `SELECT count(*) FROM lineitem JOIN orders ON l_orderkey = o_orderkey` a line above it, and that one fired and went from 630 M instructions to 92 M. `EXPLAIN` on the pair confirms it: the `ON` form is a scan of `lineitem` under an aggregate and the comma form is a hash join with both scans still under it.

That is not a difference SQL has. A comma join with the equality in `WHERE` and an explicit inner join on the same equality are the same relational expression, and by the time the optimizer's fifteenth pass runs they are supposed to be the same plan. All twenty two TPC-H queries are written with comma joins, which is how a rewrite that works fired on none of them.

## A node nothing runs, read anyway

`rudb_opt::eliminate::rewrite` asks two questions before it deletes an inner join: `verified`, which is about the file, and `unread`, which is about the plan. Instrumented, the comma form answers `verified=true, unread=false`. The parent side is `orders`, the query reads nothing out of `orders` except the key it joins on, and `unread` said something read it.

Something did, in the arena, and nothing ran it. A comma join is bound as a cross product with the equality sitting in a filter above it. `crate::order::JoinOrder` turns that pair into a join, and it does so by building a new join node and a new chain above it rather than by editing the two nodes it replaces, because a plan is an arena that never removes a node: the way a node stops running is that nothing points at it. So the arena after join ordering holds both chains, the cross product and its filter with nothing above them, and the join the root actually reaches. The dead filter still holds the predicate `l_orderkey = o_orderkey`, and that predicate reads `o_orderkey`, and `o_orderkey` is a column the parent side produces.

`unread` walked every node in the arena. It found the dead filter, counted the parent as read, and declined. Every comma join in every query has one of those behind it, so the rule declined every comma join there has ever been.

The sibling function twenty lines below it, `unread_side`, already walked only the nodes the root reaches, and its doc comment already said why: the plan it is asked about is the one that runs, and a node a pass left behind is a node nothing runs. The fix is to make `unread` do what the function beside it does, which is one reachability mark and one condition in the loop.

## What it is worth

The twelve query set over the keyed TPC-H SF1 corpus, `stats_join_elimination` off against on, `graph_sections` on both sides, counted at ring 3, one thread, one query per process, three rounds, minimum of rounds, `SELECT 1` subtracted. Before the fix:

| | off | on | |
| --- | --- | --- | --- |
| q01 `count(*)` comma | 1014.9 M | 1015.0 M | 1.000x |
| q02 `sum` on | 629.9 M | 91.9 M | 0.146x |
| q03 `count(*)` comma | 236.0 M | 236.0 M | 1.000x |
| q04 `sum` on, child filtered | 117.7 M | 117.7 M | 1.000x |
| q05 `count(*)` comma | 128.6 M | 128.7 M | 1.000x |
| q06 two column key, comma | 1158.3 M | 1158.3 M | 1.000x |
| q07 grouped, comma | 33.1 M | 33.1 M | 1.000x |
| q08 `count(*)` left join | 1541.7 M | 0.5 M | 0.000x |
| q09 left join, parent read | 1991.9 M | 1942.3 M | 0.975x |
| q10 `EXISTS` | 1226.3 M | 1226.3 M | 1.000x |
| q11 `EXISTS` | 255.6 M | 255.6 M | 1.000x |
| q12 parent filtered, control | 480.2 M | 480.2 M | 1.000x |
| suite | 8.81 G | 6.69 G | 0.759x |

After it, with eight plans changed rather than three:

| | off | on | |
| --- | --- | --- | --- |
| q01 `count(*)` comma | 1014.8 M | 0.5 M | 0.000x |
| q02 `sum` on | 629.9 M | 91.9 M | 0.146x |
| q03 `count(*)` comma | 235.9 M | 0.4 M | 0.002x |
| q04 `sum` on, child filtered | 117.6 M | 44.9 M | 0.382x |
| q05 `count(*)` comma | 128.6 M | 0.4 M | 0.003x |
| q06 two column key, comma | 1158.2 M | 1158.2 M | 1.000x |
| q07 grouped, comma | 32.9 M | 7.7 M | 0.233x |
| q08 `count(*)` left join | 1541.8 M | 0.5 M | 0.000x |
| q09 left join, parent read | 1991.8 M | 1942.1 M | 0.975x |
| q10 `EXISTS` | 1129.7 M | 1129.7 M | 1.000x |
| q11 `EXISTS` | 231.4 M | 231.4 M | 1.000x |
| q12 parent filtered, control | 482.3 M | 482.3 M | 1.000x |
| suite | 8.69 G | 5.09 G | 0.585x |

All twelve answers are unchanged in both runs, which is checked before anything is measured and would have stopped the run. The two tables are two binaries a few releases apart, which is why q10 reads 1226.3 M in one and 1129.7 M in the other on a query neither rule touches, so read down each table rather than across the pair.

q12 is the control and has to stay at 1.000x. It is q01 with a date filter on the parent side, which drops parent rows, so the join is doing something after all and no certificate licenses removing it. `verified` declines it because the parent side is no longer a bare scan, and that is the answer that keeps the rewrite honest: the fix widens what `unread` allows and leaves `verified` exactly where it was.

## What it is not worth

Nothing, on TPC-H. The same ablation over the twenty two queries after the fix still reads `0 plans differ` and 8.70 G on both sides, worst 1.001x and best 0.999x. The reason is the one the twelve query set was written around: every join in TPC-H is read from on both sides. The suite filters on the dimension it joins to and projects out of it, so `n_name`, `c_mktsegment`, `p_container`, `o_orderdate` and the rest are all in the answer or in a predicate, and a parent whose columns the query reads is a parent the join has to visit. The rewrite is for the shapes section 7.3 names, a view that joins a dimension nobody selects from, an ORM's generated SQL, a star schema queried through a wide projection, and TPC-H is not one of them. That is worth writing down rather than leaving as a zero in a table, because the zero looks like a broken rule and this one is a workload that has nothing to give it.

Two shapes in the twelve are still at 1.000x and both are the rule declining rather than the workload. q06 joins `lineitem` to `partsupp` on two columns and `equated_pair` returns a pair or nothing, so a composite key relationship is never offered to `verified` at all, and six of the eight relationships in TPC-H are single column so this has not come up before. q10 and q11 are `EXISTS`, which section 7.3 says is `true` for every child row of a total relationship, and the semi join arm of `rewrite` is written and reached by the unit tests, so the thing to find out is what the subquery is bound and decorrelated into by the time the pass sees it. Neither is in this change.

One more thing was found and left alone. `rudb_opt::link::consumers` builds its map by walking every node in the arena, so a dead node is recorded as the consumer of whatever it points at, and `stand_in` rewires through that map. It is safe today because `JoinOrder` appends the live replacement after the nodes it replaces and the map keeps the last write, so the live consumer always wins. That is a property of one pass's allocation order rather than a guarantee, and the same reachability mark this note adds to `unread` is what would make it one.
