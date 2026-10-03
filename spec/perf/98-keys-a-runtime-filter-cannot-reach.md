# 98. Keys a runtime filter cannot reach

## The problem

A hash join hands its build side's keys to the scan under its driving side, and the walk down to that scan goes through filters, projections and the driving side of each inner join below. A scan on the gathered side of a join further down is out of its reach.

TPC-H q05 joins the lineitem rows that came through customer and orders to the suppliers of Asia on two columns, `l_suppkey = s_suppkey` and `c_nationkey = s_nationkey`. The first reaches the lineitem scan. The second is about customer, which is the gathered side of its join with orders, so all 150,000 customers went into that join, 227,597 orders came out of it, the link to lineitem kept 910,519 rows, and the last join threw four fifths of what it was given away. In the steady state the lineitem scan was 45 ms of cpu and the join over it another 31 ms, against 74 ms for the whole query without the profile.

## The change

A pass after the build sides are chosen, `join_key_reach`, looks at each inner join for an equality whose driving column is read out of a scan no runtime filter reaches. It puts a semi join over that scan, against a copy of the side of the join holding the other column, so the scan keeps only the rows the join could match. The semi join is an ordinary hash join, so it hands its own keys to the scan under it and the scan reads them as a test per row.

It is the same answer because every operator between the join and the scan is an inner join, the left side of a semi join, a filter or a projection that passes the column through, so a scan row whose key the other side does not hold only reaches the join in rows that the join drops.

It refuses a key the runtime filter reaches, a side that restricts nothing, a copy whose tables hold more than a tenth of the rows the scan reads, and a scan that already has a semi join over it, which is also what keeps a second run of the pass from writing a second one.

## Results

Pending the release build, posted on the pull request.
