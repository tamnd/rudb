# Pushdown

The two passes that are worth more than everything else in this folder put together, and the two where a wrong answer is easiest to produce. They get one document because they are built together, they share the table set analysis, and the correctness conditions rhyme.

## 04.1 Expression simplification first

Pass 1 in document 03, and it is here because both pushdowns are much weaker without it.

**Constant folding.** `DATE '1998-12-01' - INTERVAL '90 days'` is evaluated once at plan time instead of once per row. TPC-H Q1 has exactly that expression in its predicate over six million rows. The constant analysis from document 02 says what is foldable and the volatile function list says what is not.

**Comparison normalization.** `5 < x` becomes `x > 5`. One shape for every later pass to match on, and it is what lets the scan recognize a predicate as a zone-map check.

**Connective flattening.** A left-deep chain of `AND` becomes one *n*-ary conjunction. This is what lets predicate pushdown split a predicate into conjuncts with one pass over a flat list rather than a recursive descent through a chain.

**Boolean identities and null propagation.** `x AND true`, `x OR false`, `NOT NOT x`. An expression provably null in a filter position eliminates the branch, because `WHERE` keeps rows where the predicate is true and a null predicate drops the row. That last clause is a rule `Node::Filter`'s own doc comment already states and it is the difference between `WHERE` and `CHECK`.

**Range collapsing.** `x > 5 AND x > 8` becomes `x > 8`. `x > 5 AND x < 3` becomes false, which pass 12 then turns into an empty node. A comparison against a value outside the column's type range collapses to a constant.

**`IN` normalization.** `x IN (1)` becomes `x = 1`. `x IN (1, 2, 3)` on a small list stays an `IN`, because document 09 pushes an `IN` list to a scan and an `OR` chain it cannot. Above a threshold, `IN` against a constant list becomes a join against a materialized list, which is document 07's problem to order. Birler and Neumann's *On the Vexing Difficulty of Evaluating IN Predicates* (CIDR 2026) is a whole paper about how badly this case is handled by nearly everyone and it is worth reading before writing this part, because generated SQL is full of it.

Applied to a fixed point with a round bound. Cheap, unglamorous, and everything after it is written against the normalized shape.

## 04.2 Projection pushdown

**The pass that is already half built.** `crates/rudb-exec/src/source.rs` resolves the plan's projection by name against the stored table and reads only those positions. Its own doc comment says it was written that way in anticipation of this pass. `crates/rudb-bind/src/binder.rs` line 831 hands it every column of the table. The gap between those two facts is one pass.

**The algorithm.** One top-down walk computing the required column set at each node, then one bottom-up rebuild narrowing every node to it.

- The root requires its output columns.
- A `Filter` requires what its parent requires, plus what its predicate reads.
- A `Project` requires what its expressions read, for those outputs its parent requires. An output nobody requires is dropped, and dropping it can drop what it read.
- An `Aggregate` requires its group expressions plus the arguments of the aggregates its parent requires.
- A `Join` requires what its parent requires plus what its conditions read, split by which side provides each.
- A `Sort` requires what its parent requires plus its keys. Note that a sort key is not an output, so a `SELECT a FROM t ORDER BY b` reads two columns and returns one, and a pass that forgets the key produces a plan that cannot sort.
- A `Get` is narrowed to what its parent requires, in the table's own column order.
- A `SetOp` requires the union of what both sides must provide, positionally, because a set operation's columns are matched by position and not by name.

**Then re-bind.** Narrowing a `Get` changes the positions of every column above it. Rather than remapping every `ColumnBinding` by hand, hand the plan back to the binder, which resolves by name and by table index and comes out right. This is exactly what firepanda's `prune.mojo` does and the reason is the same: remapping positions by hand is a class of bug that does not have to exist.

**The value.** `spec/09-optimizer.md` section 9.2 prices it as the difference between 20 GB and 200 MB on ClickBench, which is the 105-column `hits` table against a median query that reads three columns. That number is about the scan layer, which does not exist yet, so today the pass is worth the difference between copying 105 columns per chunk and copying three, which is real and is not an order of magnitude. The number becomes the spec's number at 2e. **Ship the pass now anyway**, because the scan layer is being written against a plan shape and it should be written against the narrow one.

**The correctness condition.** There is essentially one and it is easy: never drop a column something above reads. The schema-preservation check in document 03 catches every violation at the root, and the per-node required set is what catches it in the middle. This is the safest pass in the folder, which is another reason it is first.

**One subtlety worth naming.** A `Project` whose expression has a side effect or can raise cannot be dropped merely because nobody reads its output. `SELECT 1 FROM t WHERE ...` next to `SELECT 1/0 FROM t WHERE false` is the shape, and DuckDB's behaviour on it comes from the corpus. Default to keeping an expression that can raise, which costs a column and is never wrong.

## 04.3 Filter pushdown

A filter moves toward the scan until it cannot go further, so rows are eliminated before they are joined, aggregated or projected. This is the pass that fixes the corpus timeout: a ten thousand against fifty thousand join with a predicate that never moved below it is five hundred million comparisons, and the same join with the predicate applied first is a fraction of that even before the join becomes a hash join.

**Split at `AND` first.** A filter holds one expression and that expression usually wants to end up in several places. `l_quantity < 30 AND p_size <= 15` over a join has one half belonging on each side. So the first thing the pass does is split every predicate into conjuncts and treat them separately, and whatever cannot move is reassembled as one filter where it stopped.

Split only at `AND`. An `OR` cannot be split because neither side has to hold.

**What each node passes through.**

| node | passes a conjunct when |
|---|---|
| `Sort` | always. Sorting changes the order of rows and not which rows there are. |
| `Limit` | never. Which rows a limit keeps depends on which rows arrive. |
| `Project` | it reads only columns the projection passes through unchanged under the same name. A computed output is not a column of the input and a renamed one is a different name below. |
| `Aggregate` | it reads only group keys. An aggregate output never qualifies, which is the difference between `WHERE` and `HAVING`. |
| `Distinct` with no keys | always. Deduplicating and then filtering is the same set as filtering and then deduplicating. |
| `Distinct` with keys | only when it reads the keys. A keyed distinct keeps one row per key and does not promise which one, so filtering afterwards can empty a group that filtering beforehand would have kept a different row of. |
| `SetOp` | always, into both sides, positionally. |
| `CrossProduct` | into the side that provides every column it reads. |
| `Join` | see below, and this is where it gets hard. |

**Joins, by kind.** This is the table that every database has had a bug in.

- **Inner.** Push into either side that provides every column the conjunct reads. A conjunct reading both sides stays as a join condition, and if it is an equality it becomes a join key rather than a residual, which is what makes it a hash join instead of a nested loop.
- **Left.** Push into the left side freely. **Do not push into the right side.** The right side is null-producing, and a predicate applied before the padding sees different rows than one applied after. This is the classic wrong answer. A conjunct *above* the join that reads the right side and is null rejecting on it converts the left join to an inner join first, and then the inner rule applies. That conversion is where the null-rejecting analysis from document 02 earns itself.
- **Right.** The mirror of left.
- **Full.** Push into neither side. A conjunct above it that is null rejecting on one side converts it to the other one-sided kind; null rejecting on both converts it to inner.
- **Semi.** Push into the left freely. Pushing into the right is safe only for a conjunct that reads only the right, and it is already a filter in effect, so the gain is small and the argument is longer than the gain. Do it, but do it after the inner and left cases are landed and tested.
- **Anti.** Push into the left freely. **Do not push into the right, ever.** Removing a row from an anti join's right side adds rows to its output. This is the rule that is opposite to intuition and it is the second classic wrong answer.
- **Single.** Push into the left freely. Not into the right, for the left-join reason: a `Single` join pads with nulls.
- **Positional.** Push into neither side, because the *n*th row of each side is the semantics and filtering changes which row is *n*th.

**The rebuild.** `rudb-plan` hands out node indices in creation order, so an input always sits below the node that reads it, and moving a filter down makes new parents for old children. So the pass walks bottom up and writes a new node list, which restores the invariant by construction and drops anything the root no longer reaches. It returns the new root; the caller's old indices mean nothing afterwards. Expressions are not rebuilt, so every `ExprRef` stays valid; positions are, so the plan goes back to the binder afterwards, exactly as projection pushdown does.

## 04.4 Transitive predicates

The part that is easy to miss and is worth the most on a star schema.

If the plan has `a.x = b.x` as a join condition and a filter `a.x > 5`, then `b.x > 5` holds and the user never wrote it. Derive it, push it, and the build side of that join shrinks before it is built.

**The algorithm.** Build equivalence classes over column references from the equality join conditions and from equality predicates. For each class, collect every predicate on any member, and for each member, add the predicates it does not already have. Then push everything.

**The conditions.** Only from equality conditions, only when both sides are plain column references, and only through join kinds where the equivalence actually holds. It holds for inner and for the preserved side of an outer join; it does not hold across the null-producing side, because a padded row has null there and null is not equal to anything, so a derived predicate would drop rows the outer join is supposed to keep.

**It has to be idempotent against itself.** Deriving `b.x > 5` from `a.x > 5` and then deriving `a.x > 5` back from it is how this pass loops forever. Track which predicates were derived and never derive from a derived one.

**Why it is here rather than in document 08.** Document 08's predicate transfer is the same idea at runtime with approximate filters over the whole join graph, and this is the exact version at plan time over one equivalence class. They should be built as one thing with two entry points, and the constant-predicate case is the one that is free and always right.

## 04.5 What this is worth, and what is known

The firepanda folder has hand-measured numbers for the dataframe versions of these passes and they are the best evidence available today, because rudb has not run a suite yet. Quoted as what they are, which is a different engine on a different language on one machine:

- Projection pushdown was most of the distance from 6.671 seconds to about 2 seconds across the 22 TPC-H queries at SF1. The single largest pass by a wide margin.
- Predicate pushdown took q19 from 83 to 70 milliseconds and q21 from 285 to 169.
- Together with join reordering by hand, the whole set went from 6.671 to about 1.4 seconds, a factor of 4.8, with one kernel changed in that time.

What rudb has of its own is the corpus timeout: four files killed at ten seconds, all four joins of ten thousand against fifty thousand rows. That is not a number these passes improve by a percentage. It is a number they change the shape of.

**The prediction to record now, so it can be wrong later.** On TPC-H SF10 through rudb's own harness, filter and projection pushdown together should move total CPU seconds by more than a factor of two, and should move Q19 and Q21 by more than the rest. If they do not, the reason is that the tier-0 operators are slow enough that the row count is not the binding constraint, which would be worth knowing and would say to finish 2f before finishing this.

## What we should take from this document

Projection pushdown is the first pull request. It changes no operator, the scan is already written for it, and the pass is one walk down and one rebuild up followed by a re-bind.

Filter pushdown splits at `AND` only, and its whole risk is the join kind table. Inner and left first, anti never into the right, full into neither, and the null-rejecting conversion of an outer join to an inner one is what makes the rest of the table reachable.

Both passes rebuild the node list bottom up rather than moving nodes in place, and both hand the plan back to the binder rather than remapping positions.

Transitive predicates are built with the equality classes that document 08 later needs, they are the free and exact case of predicate transfer, and they need a derived-predicate marker or they loop.

The numbers quoted for what this is worth are from a different engine. The number that is rudb's own is four corpus files timing out at ten seconds on a join of ten thousand rows, which is what the absence of this document looks like from outside.
