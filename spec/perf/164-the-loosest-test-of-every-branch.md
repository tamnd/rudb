# 164. The loosest test every branch of a disjunction makes of a column

## The problem

A disjunction over two tables tells the filter pass two things already. `shared` lifts a test that every branch makes word for word, and `narrowed` gives each table the disjunction of what each branch says about it. Neither takes a branch apart. TPC-H q19 names a brand, four containers and a range of sizes in each of its three branches, so part gets a disjunction of three conjunctions and the filter walks it branch by branch over all 200,000 parts to keep 485. Selection::without and Selection::complement, which thread an OR, came to about 21M instructions a run between them, and range comparisons over packed integers another 13M, out of about 95M for the query.

## The change

For each column that every branch tests against constants, the pass now also states the loosest such test beside the disjunction. Equalities, including an IN list as the binder writes it, are gathered into one list across the branches. Of the bounds on one side, each branch's tightest is what the branch promises, and the loosest of those over the branches is stated. On q19 part gets `p_brand IN` three brands, `p_container IN` twelve containers and `p_size <= 15`, and lineitem gets `l_quantity >= 1` and `l_quantity <= 30`. Each is one membership test over dictionary codes or one comparison of packed integers, and together they leave about one part in a hundred for the disjunction. A bound at the top of a filter is also one a scan can skip a block with, which a bound inside an OR never is. As with `narrowed`, this applies only to a disjunction over more than one table, and volatile tests are left out. `stretch` (#2765) states the one range that a column's ranges cover inside the disjunction `narrowed` gives one table, when the ranges meet with no gap. On q19 it states the same two `l_quantity` bounds, which are stated once, since each is checked against what the list already has. What this adds over it is the lists of equalities, which a range does not cover, and the bounds of a disjunction whose ranges leave a gap.

## Results

Measured against main at 86fee134, which has #2756 and #2765, in user instructions a run at SF1 on one thread on server3, with every answer the same at one and four threads on both the clustered and the base database.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q19 | 249 | 240 |
| q07 | 35 | 35 |
| q12 | 65 | 65 |
| q22 | 48 | 48 |
| q01 | 131 | 131 |
| q06 | 17 | 17 |

Against an older main, before #2756 made each operand of an `OR` cheaper and #2765 stated the `l_quantity` range, the same change took q19 from 174M to 146M. What is left of that is the part side: three brands, twelve containers and a bound on the size, which no other rule states. The 249M of main at 86fee134 is mostly `Vector::on_lanes` from #2760, about 63M a run, which #2773 and the PRs after it are taking down.
