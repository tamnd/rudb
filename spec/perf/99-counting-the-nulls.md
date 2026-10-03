# 99. Counting the nulls

## The problem

TPC-H q13 counts `o_orderkey` for each customer over the 1.48 million orders whose comment passes the `NOT LIKE`. The aggregate goes through the ranged count of `group_ranged`, since `o_custkey` is a dense range from 1 to 150,000, and keeps one array of row counts and one array of value counts, each 1.2 MB of `i64`. A profile of q13 at one thread put 35 percent of its cycles in the aggregate's sink, and an annotation of it put almost all of that on two lines, the `incq` into the row counts and the `incq` into the value counts. Each is an add at a random place in an array bigger than the second level cache, so a row costs two cache misses, one in each array.

`o_orderkey` has no null in it. The value count is the row count in every place, and the second pass was paying a cache miss a row to write it down a second time.

## The change

A call that counts values keeps the nulls it saw in its group instead of its values, and the count it answers is the rows less the nulls. A chunk whose argument has no null adds nothing to that array, and the array is not even made until the first null comes, so a column with none costs no array, no reservation and no pass. A chunk with nulls in it adds one where the argument is null, which is the same pass the value count made before. Two instances that combine add their null arrays when both have one and take the one that exists when only one does.

A `SUM` keeps its own count of values beside the total, because a group of nulls has to answer null, and it goes the same way, so the ranged sums of q10, q11 and q15 drop the same pass when their argument has no null.

## Results

Pending the release build, posted on the pull request.
