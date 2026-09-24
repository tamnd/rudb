# One walk for both layouts

A grouped aggregate whose chunk comes in runs of one group folds every call that can share it in one walk of those runs, and calls over one physical layout shared a walk because the value loop is written once per layout. A column that points somewhere else is read out into a run of `i64` before it is folded, so those calls shared a walk of their own. q01 has three of each, its stored decimals packed and its computed ones flat, and it walked the runs twice.

Both halves are `i64`. `DECIMAL(15, 2)` is held in one and so are `DECIMAL(18, 4)` and `DECIMAL(18, 6)`, which is what q01's own arithmetic computes, and a column read out of a packed run is written into one. So the flat `i64` calls join the pass the read out ones take, and q01 walks its runs once.

What that saves is the walk and not the values. Counted out of the profile, the fold was spending about sixty nine instructions per run visited beyond what it spends per value, which over 2.13 million runs is most of what it spends at all: the slot read and checked, the locals for that slot indexed, the run's length added to the group's count, and per call a load out of the call list, a bounds check on the slice of values the run covers, and the total loaded and stored back. Halving the number of run visits takes that once instead of twice.

On server2 at SF1, one thread, three rounds, median, with the `SELECT 1` baseline subtracted, q01 went from 1774.0 M instructions to 1708.0 M and the suite from 16.79 G to 16.71 G. All 22 answers are unchanged and nothing moved the other way past 1.001x.

The per run cost is what is left to go at. A pass whose width is known at compile time could hold every call's total in a register across the run and its slice of values in another, which is the load, the bounds check and the store per call per run gone. The layout vote is what stands in the way of going further than that, since two calls over an `i32` and an `i64` column still take a walk each, and reading every layout out into `i64` up front would trade a copy per value for a walk per call.
