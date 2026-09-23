# Arithmetic in one loop

Notes written on 23 September 2026, after [note 24](24-adds-that-wait.md) took q01's grouped sums from 15.0 G to 13.5 G cycles and left the arithmetic in front of them as the next largest piece.

## The question

The charge in q01 is `l_extendedprice * (1 - l_discount) * (1 + l_tax)`. The prepared expression ran it as five steps, and each step read its inputs as whole vectors of 8192 rows and wrote a whole vector out. That is four intermediate vectors of 64 KB each per chunk, every one written to memory and read back by the next step, and every step checked every row for overflow on its own. The question was whether a chain of decimal arithmetic can be run as one loop that keeps its intermediates in a few small buffers and proves once, before it starts, that nothing can overflow.

## What changed

A chain of `+`, `-` and `*` over decimal columns of width 18 or less, with the constants and the same-scale or widening casts between them, is now compiled into a short program over i64 buffers (`crates/rudb-exec/src/fused.rs`). The prepared expression holds it as one step, with the ordinary steps built beside it as the fallback.

When a chunk comes in, the step looks at what it knows about each column's range without reading a row. A packed column carries its base and the largest code its width can hold. A flat column's range is one pass over its values. A dictionary's range is the range of its values. It then runs interval arithmetic in i128 over the program, and if every node stays inside the width its type allows, no row can overflow and the loop needs no check. If the proof fails, or a column has nulls, the chunk goes through the ordinary steps, which raise the same error they always did. The tests check that the message is the same either way.

The loop itself walks the chunk 256 rows at a time. For each block it fills one buffer per column, unpacking packed codes in bulk and gathering through a dictionary's codes when there is one, runs the program over the buffers with wrapping arithmetic, and appends the root to the output. At 256 rows a buffer is 2 KB, so every intermediate stays in L1.

The second change is in the filter. When a filter keeps some rows of a chunk, a packed column is now compacted at once by unpacking only the kept positions in bulk (`Vector::unpacked_at`), the same way a stable dictionary already was. Before, the packed column was left behind a selection and every reader after it paid for reading a code at a time through the selection.

## What was tried and dropped

A fixed array on the stack for the unpacked codes of each block was cleared on every block, and that clear cost more than the loop saved. One buffer allocated per chunk and reused replaced it.

Finding a flat column's range per block, inside the copy loop, instead of once per chunk before it, saved nothing measurable in cycles and cost 0.37 G more instructions over ten q01 runs, so it was reverted.

## Numbers

Ten q01 runs on one thread on server3, against the main this branch started from:

| | cycles | instructions |
|---|---|---|
| main | 13.8 G to 13.9 G | 35.41 G |
| this change | 13.2 G to 13.8 G | 32.22 G |

server3 was under a load of about 20 while these ran, so the cycles move by about 5 percent from run to run. The instructions fell 9 percent. The cycles fell by less, because each 64 KB column of 8192 rows still lives in L2 and not in L1, and the loads from it are the part of q01 this change does not touch.

The same ten runs on gpc, a 13900K that was idle, as wall time over five runs each:

| | mean | best |
|---|---|---|
| main | 1.362 s | 1.323 s |
| this change | 1.324 s | 1.291 s |
| DuckDB | 0.743 s | 0.695 s |

The 22 TPC-H queries on gpc against the native file, best of three, all threads, whole process time including start up:

| | total |
|---|---|
| main | 593 ms |
| this change | 583 ms |
| DuckDB | 1289 ms |

Most of the gain in the suite is q14, which went from 28 ms to 16 ms over six runs each, and q12, from 22 ms to 19 ms. q14 filters lineitem down to one month and then sums `l_extendedprice * (1 - l_discount)`, so it uses both halves of this change, and the two were not measured apart. Every one of the 22 answers is the same as main's.

## What this leaves

On one thread q01 is still 1.8 times DuckDB's time. With the arithmetic in one loop, the profile is now led by the grouped sums and means reading their argument vectors, the gather in the filter and the copies of the two flag columns. Two of the columns, price and discount, are read by both the discounted sum and the charge, and each fused chain unpacks them again. The next step is to run every aggregate argument of a query from one block loop, so each column is unpacked once per block and the sums read the values while they are still in L1, rather than handing an 8192 row vector from one operator to the next.
