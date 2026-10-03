# 97. Keeping freed mappings

## The problem

Counting a warm run as the cost of the second run onward hid what a warm query really costs, because the second run in a process is the one that decodes and holds every part. Measured as the steady state, the eleventh run minus the third over eight, rudb at one thread on SF1 is 2 to 5 times DuckDB on most queries, and on several of them a large share of what is left is kernel time on every run. q11 spent 31 M cycles a run in the kernel against 31 M in the query, q18 94 M against 227 M, and q03, q09 and q10 a fifth or more.

A profile of the page faults put most of them in an aggregate's table and other big blocks being written for the first time. A block of 256 KiB or more is mapped from the system on its own, and was unmapped when it was freed. A warm statement frees its big blocks at the end and takes blocks the same size again at the start of the next, so every run faulted in and cleared all of them again.

Sending every block to mimalloc, as a test, took the faults away, q11 from 2772 a run to 11 and q18 from 6570 to 436, but that is what the mapped blocks were there to avoid, since it held a hundred megabytes more at peak on ClickBench.

## The change

A freed mapping is kept rather than unmapped, up to 64 MB and 16 ranges, and the next big block is cut out of it. What is kept is address ranges rather than blocks, because a big block is usually a vector that starts small and doubles, and kept whole the mapping it ended at was cut down to where it started on the next run, so every doubling after that faulted its new half in again. That first version only halved the faults. So a block takes the part of the shortest range that holds it and the rest stays kept, a block that grows takes the kept range right after it and grows where it is without a call to the kernel, a block that shrinks keeps its tail, and ranges freed next to each other join again. Only when nothing kept fits does a block go to `mremap` or a fresh mapping as before.

A zeroed block out of a kept range is cleared with a `memset` over the bytes the last block had, which costs a fraction of faulting them in. A block put together out of two kept ranges can span two of the kernel's mappings, which `mremap` refuses, so a failed resize falls back to a fresh mapping and a copy.

The bound is what keeping can add to a process's peak. A process that frees big blocks and never takes them again holds at most 64 MB of them.
