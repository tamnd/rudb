# Filters that mark

Notes written on 24 September 2026, after [note 25](25-arithmetic-in-one-loop.md) and the packed reads that followed it left the filter's compaction as about a fifth of q01's instructions.

## The question

q01 filters lineitem on `l_shipdate <= date '1998-09-02'`, which keeps about 98 percent of the rows, and then groups what is left by two flags and sums and averages four columns. The filter is pushed into the scan, and when it kept fewer rows than a chunk held it cut every column the query reads down to the kept rows before handing the chunk up. For q01 that means copying seven columns of nearly 8192 rows each to drop about 150 of them. The question was whether a filter that keeps most of a chunk can tell the operator above it which rows it kept and leave the columns as they are.

## What changed

A chunk can now carry a selection of the rows a filter kept without being cut to them (`Chunk::marked` in `crates/rudb-vector/src/chunk.rs`). The builder only lets a filter mark when the operator directly above it is a grouped aggregate with plain, non distinct calls and nothing volatile in its keys or calls (`marks_through` in `crates/rudb-exec/src/build.rs`), since that is the one reader that knows what to do with it. The filter then marks when it keeps at least three quarters of the rows and cuts the chunk as before when it keeps fewer, because below that the copy is small and every reader after it gains from the shorter columns. This is the same rule for the filter operator and for a filter pushed into a native scan, and the scan only marks when no other filter or runtime filter runs with it.

The grouped aggregate already had a way to skip a row. A call with a `FILTER` clause gives the rows it drops a slot that points at no group, and every sum, mean and count kernel steps over such slots. A marked chunk now uses the same path. The aggregate cuts only its key columns to the kept rows, so no group is ever opened for a row the filter dropped, finds the slot of each kept row, and then spreads those slots back out over the whole chunk with the dropped rows pointing at no group (`spread_slots` in `crates/rudb-exec/src/group.rs`). The arguments are read over every row, straight from the unpacked columns, with no gather.

Any path in the aggregate that cannot work this way, such as a distinct count, a single group or the paths that close groups as they go, settles the chunk first, which is the old cut, so nothing changes for them. An argument that fails on a row the filter dropped, for example a cast that does not fit, would have raised an error the old way that it must not raise, so when evaluating the arguments over the whole chunk fails the aggregate settles the chunk and evaluates again over the kept rows only, where the error is raised only if a kept row causes it. Cutting a chunk that is still marked is an internal error, so an operator that forgets to settle is caught rather than reading dropped rows.

## Numbers

Ten q01 runs on one thread on server3, which was under a load of about 40 on 8 cores, three runs each:

| | cycles | instructions |
|---|---|---|
| main | 10.86 G to 11.24 G | 29.23 G |
| this change | 9.87 G to 10.09 G | 26.12 G |

Instructions fell 10.6 percent and cycles about 10 percent. DuckDB on the same file and one thread takes about 6.8 G cycles and 11.4 G instructions for the ten runs.

The 22 TPC-H queries on server3 against the native file, best of three, instructions over all threads:

| | q01 | total |
|---|---|---|
| main | 3.020 G | 28.824 G |
| this change | 2.709 G | 28.551 G |

The other 21 queries moved by less than one percent either way, which is the noise of counting over all threads, as none of them has a filter that keeps most rows directly under a grouping. Every one of the 22 answers is the same as main's.

## What this leaves

q01 is now led by the sums and means reading their arguments. `sum(l_extendedprice)` and `avg(l_extendedprice)` read the same column and each keeps its own total, and the same holds for quantity and discount, so the next step is to let calls over the same argument share one total. After that the local totals can be kept in i64 rather than i128 when the range of the column proves they fit, which halves the width of the lane each row adds into.
