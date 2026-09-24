# Two numbers a key

Notes written on 24 September 2026, on q21, the query where rudb still spent the most instructions over DuckDB after q01.

## The question

q21 keeps the late lines of an order when some other line of the same order came from another supplier and no other line of it was late. Both subqueries are on lineitem, keyed on the order and with a `<>` on the supplier:

```sql
EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey)
```

The plan gathers the 157 thousand late lines from Saudi suppliers, hashes them on the order key and drives the other 738 thousand lines of those orders through the table, setting a bit on every gathered line some driving line matches (`Marking` in `crates/rudb-exec/src/join.rs`). The `<>` is left over after the equality, so each driving line walked its order's chain, the pairs were gathered a batch at a time into a chunk, the comparison ran over the chunk and the flags were read back. On one thread the query with only the `EXISTS` took 1.54 G instructions against 0.49 G without it, and DuckDB's `EXISTS` costs it 0.37 G.

## What changed

Whether some driving line of an order has a supplier `d` with `g <> d` depends only on the smallest and the largest `d` of that order: a `d` other than `g` exists exactly when the two are not both `g`. The four orderings need only one of the two, since `g < d` holds for some `d` exactly when the largest `d` is above `g`. This is magic decorrelation turned into an aggregate, done inside the join where the table on the key is already built rather than as a separate group by in the plan.

So when the one condition left over after the equalities compares an integer or date column on the driving side with one of the same type on the gathered side, the marking join now keeps two numbers per key (`Extents` in `crates/rudb-exec/src/extents.rs`). A driving row costs its lookup and two compares, and no pair is ever made. Once the driving side is done, each gathered row is compared once against its key's range to set its bit. The ranges are shared by every thread and are only written when a value moves one, so the writes are rare after the first few lines of an order.

A null on either side never marks, which is what the comparison would have said. A driving column in a form with no run of integers, such as a gather through a link, is flattened a chunk at a time first. `crates/rudb/tests/extents.rs` checks all five comparisons, both kinds and both ways round against the answer worked out pair by pair, over keys that match nothing, keys that hold only one value and nulls in both the key and the compared column.

## Numbers

On server3 against the native file at SF1, instructions best of two, before and after:

| | main | this change |
|---|---|---|
| q21, all threads | 2.504 G | 2.228 G |
| only the `EXISTS` | 1.537 G | 1.377 G |
| only the `NOT EXISTS` | 1.516 G | 1.389 G |

DuckDB takes 1.830 G on q21. All 22 answers are the same as main's. Over five runs of q21 with server3 under a load of about 16, the wall time was 0.33 s to 0.41 s on main, 0.30 s to 0.43 s with this change and 0.32 s to 1.15 s for DuckDB, and the CPU time 0.84 s to 1.09 s, 0.68 s to 1.02 s and 0.84 s to 1.17 s.

## What this leaves

The join no longer spends anything on the comparison, and what it does spend is the table. Building the two tables of about 155 thousand lines and probing them is about 40 percent of q21 now, and the order keys of those lines are integers spread over six million values, about 38 values a line. That is too sparse for the direct index, which takes a key range of at most four values a line, so the join hashes. A bitmap over the range with a running count beside it would give each key its slot with a load and a popcount, with no hash, no stored key and no collisions, and that is the next step.
