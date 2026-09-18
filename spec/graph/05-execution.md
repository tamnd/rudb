# 5. Execution

Six mechanisms, in the order they pay off on TPC-H. Every one of them is an alternative to something the engine already does or will do, and every one of them falls back to that something when its precondition fails. The precondition is always the same: a verified relationship with a built link, and both sides of the join reaching the operator by a path the planner can prove preserves row identity.

## 5.1 What "preserves row identity" means, because it is where the bugs will be

A link maps a `rid` of the child table to a `rid` of the parent table. It is therefore usable only where the operator knows, for each row in front of it, which row of the base table that row came from. A scan knows. A filter over a scan knows, because a filter is a selection and a selection is a list of positions. A projection knows. A join's output does not know for both of its inputs, an aggregate's output does not know at all, and a sort's output knows only if it carried the `rid` through.

So the plan carries a `rid` the way it carries any other column: as a `Sequence` vector body over the part's starting row id, which costs nothing, surviving filters as the selection is applied, and dropped the moment an operator cannot maintain it. The rule is mechanical and belongs in `crates/rudb-plan`: every node declares for each of its outputs whether that output still has a `rid` of some base table, and the link rewrite in document 06 fires only where the declaration says yes. `../planner/02-what-a-plan-is.md` is where this property gets written down beside the others.

This is the single most important paragraph in the document. Every incorrect answer this layer can produce is a `rid` used after the operator that invalidated it.

## 5.2 The link join

A join whose condition is `child.fk = parent.pk` for a relationship with a forward link, where the child side reaches the join with its `rid` intact.

Plan: scan the child, read the link column beside the data columns, and emit the parent's projected columns as gathers. There is no build side, there is no hash table, there is no probe, and there is no materialization of the parent at all.

The output's parent columns are a new vector body, `Gathered { source, indices }`, holding an `Arc` to the parent's column vector and the `rid`s to take from it. Document 08 specifies the body. Two consequences follow and they are the reason this is a body rather than an eager copy. A parent column that the query projects but never touches before the final output, which on TPC-H Q3 is `o_orderdate` and `o_shippriority`, both of which are only grouped on, is never read out of the parent's pages until the operator that actually needs its values asks. And a parent column used in a filter is filtered *in the gathered form*, meaning the kernel evaluates the predicate over the distinct parent rows that were actually reached rather than over one value per child row, which on a many-to-one join with a low distinct count is a large factor and is precisely the encoded-execution argument the engine already makes for dictionaries.

The three join kinds behave as follows. Inner drops child rows whose link is the *no parent* sentinel. Left keeps them and gathers null. Semi and anti are a test of the sentinel and never touch the parent at all, which makes them nearly free and which is worth stating because `EXISTS` over a foreign key is extremely common. Right and full need the parent rows that nothing pointed at, which is the backward direction, and they are handled by section 5.6 or by falling back to the hash join. Single needs a second-match check, which a many-to-one link gives by construction and a verified cardinality certifies, so `single` over a verified relationship is the only case in the engine where that check can be skipped rather than performed, and it must be skipped only when the verification in document 02 section 2.3 said *exactly one* or *at most one*.

Cost, per child row: one bit-packed read of the link, one bounds check, and one gather per projected parent column per row that survives to an operator that needs values. Against a hash join's per row cost, hash, probe, compare, gather, it removes the hash and the probe and the compare, and it removes the entire build.

## 5.3 Where the link join is not the right shape

When the join is selective on the parent side, scanning the whole child table to look up each row's parent is the wrong direction, and the right one is to reduce the child first. That is section 5.4, and in practice on TPC-H it is section 5.4 that does the work and section 5.2 that finishes it. Q3, Q5, Q7, Q8, Q9, Q10, Q12, Q14, Q18 and Q21 all filter a small table and join a large one, and a link join alone would read all of `lineitem` for every one of them.

## 5.4 Exact reduction, which is the claim this directory has to prove

The mechanism. A predicate on a parent table is evaluated over the parent's rows, producing a `Rids` bitmap over parent row ids, which the scan produces anyway, since it already knows which rows passed. Push that bitmap forward through the forward link: one sequential pass over the child's link column, one bit test per row, producing a `Rids` over the child. That bitmap is then a filter on the child's scan, evaluated before any of the child's data columns are decoded, and combined with the link's own zone maps so that a whole part whose parent `rid` range misses the bitmap entirely is skipped without being read.

What makes this different from a Bloom filter pushed to a scan is that it is exact. There are no false positives, so a row that survives the reduction is a row that really joins, and a reduction that runs on every edge of the join graph therefore removes every dangling tuple rather than most of them. That is the definition of full semi-join reduction, and full semi-join reduction over an acyclic query is what Yannakakis' algorithm needs for its `O(IN + OUT)` guarantee. Robust Predicate Transfer had to build a maximum spanning tree over the weighted join graph to approximately recover that guarantee from Bloom filters that do not give it; with exact bitmaps the guarantee is a property of the primitive.

The cost, stated so it can be argued with. A forward pass over the join tree is one sequential pass over one bit-packed column per edge, plus one bit test per row into a bitmap that may not fit in cache. A backward pass is the same in the other direction. TPC-H Q5 has six tables and five edges, so the reduction is ten passes over link columns before any join runs, against the five hash builds and five probes it replaces part of. The reduction is a win when it removes enough rows to pay for the passes, and it is a loss on a query where every row joins anyway. That is exactly the decision the Yannakakis+ and the ML-gated work in document 01 section 1.6 exist to make, and document 06 section 6.5 is rudb's version of it.

The honest risk, recorded here rather than in the open questions because it is the main one: a random bit test into an 18 MB bitmap is a last-level cache miss, and six hundred million of them is not free even though each is one instruction. The mitigations are that the tests happen in `rid` order when the child is clustered, which makes them sequential rather than random; that the link's zone maps skip parts wholesale; and that the sparse form of section 4.3 is used when the surviving parent set is small enough that a merge beats a probe. Whether those are enough is measured in document 09 section 9.4 and not asserted here.

## 5.5 Reduction over a chain, and the part skip

The bitmaps compose. `region → nation → customer → orders → lineitem` is four pushes, each one sequential, and the result is a bitmap over `lineitem` rows that can possibly contribute to Q5. Nothing between the first and the last is materialized. This is the transitive reach that Parachute buys by precomputing join-induced columns on the foreign-key table, obtained instead by composing links at query time, which costs passes where Parachute costs space. Which of those is right depends on how often the same chain recurs, which is a workload question, and document 11 keeps the precomputed variant open.

The part skip deserves its own sentence because it is where the asymptotics change. The forward link is a column, so it has zone maps: the minimum and maximum parent `rid` in each part of a thousand rows. If the reduced parent set has no `rid` in that range, the part cannot contribute and is never read. On a child table clustered by the join key this prunes almost perfectly, on TPC-H at SF100 a `region = 'EUROPE'` restriction reaches five nations, a fifth of `customer`, a fifth of `orders`, and if `lineitem` is clustered by `l_orderkey` then a fifth of `lineitem`'s parts contain nothing but non-matching rows and are skipped entirely. The zone map note in `crates/rudb-storage/src/zone.rs` already says the same thing about ClickBench and clustering: the maps are only as good as the sort order, and the sort order is an F2 item. This layer makes that item pay twice.

## 5.6 Backward traversal, and why it is usually a forward pass

A backward traversal answers "for each parent, its children". A query that needs one is usually computing an aggregate over the children per parent, the count of line items per order in Q13, the sum of quantity per order in Q18, and an aggregate per parent over children is the same thing as a group-by on the child keyed by the parent `rid`.

That rewrite is strictly better than the traversal. It is one sequential pass over the child with the existing grouping machinery, keyed on an integer that is already dense, which means the group-by hash table is replaced by a direct-addressed array of `parent_rows` slots with no hashing and no probing and no collisions at all. `crates/rudb-exec/src/group.rs` holds group state in flat vectors indexed by a slot; when the key is a `rid`, the slot *is* the key. Q18's `HAVING sum(l_quantity) > 300` over a group by `l_orderkey` becomes a single pass over `lineitem` accumulating into a 150-million-entry array, which is 1.2 GB at eight bytes and 600 MB at four, and is the one place in this design where a direct-addressed array's memory has to be weighed against a hash table's. The weighing is a cardinality comparison the planner can do.

The backward adjacency of document 03 section 3.5 therefore exists for the cases the rewrite does not cover: a right or full outer join, a query that needs the children of a *small selected set* of parents rather than of all of them, and the multiway intersect of section 5.7. The first two are where a CSR read of a few thousand short lists beats a pass over six hundred million rows by orders of magnitude, so it is not a marginal structure, but it is not the common one either.

## 5.7 Factorized expansion

A one-to-many join expands. Q9 joins `lineitem` to `orders` to `nation` and every `lineitem` row carries its order's and its nation's values, which in a flat representation means copying `o_orderdate` six hundred million times to group by the year of it.

The factorized form does not copy. An `Expanded { values, offsets }` vector body, per document 08, holds the parent's values once and an offset array saying which output rows share which parent value; it is the same shape as the dictionary body the engine already has, with the offsets playing the part of the codes and the sharing being positional rather than by value. Aggregations consume it directly: the sum of a value repeated `k` times is the value times `k`, and the kernels that already fold a run-length encoded column are the kernels that fold this. FFX reports a mean 2.08x from exactly this over flat execution and up to 9.39x on branched factorizations, in an engine of the same shape as rudb's.

Flattening is a real operation with a real cost and it has to have exactly one place it happens: an operator that cannot consume the expanded form calls `flatten`, which materializes. The list of operators that can consume it starts small, the arithmetic kernels, the comparison kernels, the sum, count, min and max aggregates, and grows by measurement. Every operator that has not been taught the form flattens, which is correct and slower, which is the same discipline `crates/rudb-vector` already applies to its other bodies.

## 5.8 Multiway intersect

For cyclic joins, where a single pair of tables cannot be joined without producing an intermediate larger than the final result. TPC-H has essentially none of these; Q21's self-join on `lineitem` with an existence and a non-existence condition is the closest it comes. JOB, CEB and any graph workload have many.

The design is Kùzu's ASP-Join expressed over rudb's structures: accumulate the candidate set for the first variable, semi-join it against each participating relationship's adjacency, then probe by intersecting several adjacency lists at once. With the `Rids` bitmaps of section 4.3 the intersection of several lists is a bitmap `AND`, which is where this layer makes worst-case optimal joins cheaper to implement than they usually are. Free Join's lesson applies: this should be one plan type that generalizes the binary case rather than a second execution path with a second optimizer, and the way to get that in rudb is for the multiway intersect to be an operator the ordinary planner can emit rather than a mode the query enters.

It is scheduled last in document 10 for the reason document 01 section 1.4 gives, which is that TPC-H is acyclic and the goal document names TPC-H first.

## 5.9 What stays

The hash join stays and is the general case: any equi-join with no verified relationship, no built link, or no surviving `rid`. `../engine/08-join.md` is its specification and nothing in this directory replaces a line of it. The nested loop stays for non-equi joins. IEJoin, merge join and the Bloom filter of `../engine/08-join.md` section 8.5 all stay, and the Bloom filter is what a reduction degrades into when the key is not a `rid`.

Two interactions are worth naming. The delete mask: a link may point at a deleted parent, and the gather then produces a row the mask removes, which is correct and wasteful; when the delete fraction of a parent exceeds a threshold the reduction bitmap is pre-intersected with the live mask once, which costs one `AND` over the parent's bitmap and saves a gather per child row. And spill: a reduction that cannot reserve its bitmap is skipped, not spilled, because a spilled bitmap is slower than the join it was trying to accelerate.
