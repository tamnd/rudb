# Columns the filter read last

Notes written on 24 September 2026, after #1686, on where q01 still does more work than DuckDB.

## The question

q01 was 2.52 G instructions against DuckDB's 2.21 G. Taking it apart one piece at a time put the grouping and the sums close to DuckDB and the scan well behind it:

| query over lineitem | rudb | DuckDB |
|---|---|---|
| nothing, the process alone | 0.02 G | 0.06 G |
| the q01 date filter and a count | 0.30 G | 0.22 G |
| the filter and four ungrouped sums | 0.94 G | 0.76 G |
| the filter and a count grouped by the two flags | 0.76 G | 0.85 G |

A profile of the filter and the count had most of its samples in `Chunk::select` unpacking a packed column at every kept row. The only column in that query is the ship date, and after the filter has read it nothing does. Narrowing a chunk wraps most columns in the selection, which costs nothing, but a bit packed column or one with a stable dictionary is unpacked there. That was decided in [note 16](16-a-key-that-is-not-an-integer.md) because every later reader would otherwise unpack it again. That reasoning is right for a column somebody reads, and pure cost for one nobody does. q01 keeps 98 percent of lineitem, so it unpacked six million dates a query and then threw them away.

## What changed

A scan with a pushed filter now works out, when it is built, which of its columns only that filter reads. It walks up the plan from the filter, noting what each operator reads, until it reaches an aggregate or a projection. Those two make their own columns, so nothing above them can see the scan's. Filters, joins, cross products, sorts and plain limits pass their input's columns on, so what they read is all they read. Anything else stops the walk and nothing is left out: a set operation or a `DISTINCT` compares whole rows, a dependent join lets its other side read this one, and the top of the plan hands every column to the caller without an expression that says so.

When the filter narrows a chunk, those columns are taken out first and come back as null constants of the kept length. A column is only taken out if narrowing would unpack it and no runtime filter from a join reads it next. The first version also left out string columns, which narrowing only wraps, and q13 got five percent more expensive, because the order comment it filters on came back as a constant the join then had to handle.

## What it did

All 22 answers are the same bytes as before. The machine was busy while this was measured and the thread pool's spinning moved every count by a few percent between runs, so these are on one thread, where the count moves less:

| query | before | after |
|---|---|---|
| q01 | 2.588 G | 2.409 G |
| q09 | 2.477 G | 2.311 G |
| q21 | 2.244 G | 2.209 G |
| q12 | 1.20 G to 1.27 G | 1.24 G to 1.27 G |

At the default thread count the filter and the count went from 0.30 G to 0.15 G against DuckDB's 0.24 G, and the filter and the four sums from 0.94 G to 0.79 G. q12 moved inside its own noise. Its filter keeps one row in two hundred, so there were few rows to unpack in the first place.
