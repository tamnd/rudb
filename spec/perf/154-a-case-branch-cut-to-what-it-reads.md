# 154. A CASE branch cut to what it reads

## What was slow

A `CASE` runs each branch over the rows that branch claimed, and to do that it cut the whole chunk down to those rows, every column of it. TPC-H q14 sums `CASE WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END` over a chunk of six columns. The `WHEN` reads one of them and the `THEN` reads two, so most of each cut was a copy of columns nobody read.

The `ELSE 0` was worse. It reads no column at all, yet it still cut the chunk, ran a literal over the cut, and then the assembly flattened that constant and wrote the same zero once for each row it claimed.

## The change

A prepared `CASE` now keeps, for each `WHEN`, each `THEN` and the `ELSE`, the columns of the chunk that expression reads. A cut copies only those columns down to the rows and puts a null constant of the right type in the place of every other column, so the expression prepared against the whole chunk still finds its columns where it expects them. A branch that is a single literal is not cut or run at all: it becomes one constant vector placed over the rows it claimed.

The assembly, given a constant piece, lays the value once and points every row it claims at that one slot, rather than flattening the constant first. A piece with no nulls also skips the null check for each row. A test places constant pieces beside one that is not, for a number, a string longer than a view, a typed null and a constant of no rows, and checks the result against the general path.

## Measured

At SF1 on server2, one thread, against main at #2749.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q14 | 58 | 44 |
| q12 | 105 | 93 |
| q08 | 66 | 65 |
| q01 | 185 | 185 |

q14 and q12 each sum a `CASE` with a literal `ELSE`. q08 sums one too, but over a few thousand rows, so it moves little. The answers to all 22 queries are the same bytes as before at one thread and at six.
