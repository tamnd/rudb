# Tables the driving side narrows

Notes written on 23 September 2026, after #1588, while working on q09, the largest gap to DuckDB left in the TPC-H suite at SF1.

## The question

q09 took 3.01 G instructions over five runs against DuckDB's 2.07 G, with the same plan shape. The driving side is lineitem. It joins first to the green parts, about 5 percent of `part`, and then to `partsupp` on the part key and the supplier key. The join to `partsupp` built its hash table from all 800,000 rows, but every lineitem row that reached it had already been through the join to green parts on the same part key, so only about one `partsupp` row in twenty could ever match. The rest were hashed, laid into the table and then never found.

## What changed

A join whose driving key is a bare column now looks for a runtime filter from an inner or semi join below it on the same column, named the way the scan names it. That lower join has already dropped every driving row whose key is not in its build side, so a gathered row whose key is not there either can match nothing, and the upper join leaves it out of its table as if its key were a rejected null. Only the exact bitmap is used, because it answers for the integer value whatever the width of the column, and it is read with the same bit test the scan uses. A lower join whose table is not finished when the upper one builds leaves it alone, which is the old behavior.

The join to green parts is answered by the graph reduction's exact rows, so it had no bitmap to give. A runtime filter that a join above has asked for now makes the bitmap anyway, in the same pass over the build side. The bitmap also has to be read off a filtered build side, whose key column a filter leaves as codes into the run the scan read, and #1591 taught `Vector::signed_block` to read through those while this was being written.

This applies to the kinds that stream through a lookup and never hand out a gathered row nothing matched: inner, left, semi, anti and single. A mark join is left out, because it answers null or false for a null driving key by whether the gathered side has any rows, and it asks the table that. An outer join between the two joins can put that null there. The outer joins that keep the gathered side go through their own operator and are not touched.

## Measured

Five runs of q09 at SF1 against main after #1591, both built without the size setting #1589 put on the CLI crate, which is discussed on #1510.

| build | instructions | CPU over ten runs |
|---|---|---|
| main | 2.80 G | 289 ms |
| this change | 2.27 G | 226 ms |
| DuckDB | 2.14 G | 254 ms |

No other query moved by more than noise, and the answers of all 22 queries are the same bytes as main.

## What is left

q09 is now under DuckDB in CPU time, and 10 times is a long way off. Per row, the probe still compares keys that the table already knows are equal by hash, and the build still lays out every column of the gathered side before it knows which rows the table keeps. Laying out only the kept rows is the next step for this path, since a table that keeps one row in twenty still copies all twenty.
