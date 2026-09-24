# 63. The link header alone

## The problem

With `graph_sections` on, a query that joins nothing still cost more than with it off. Instruction counts on one thread at SF1 showed every one of the 22 TPC-H queries, q01 and q06 included, paying about the same amount extra with the setting on. `SELECT 1` on the SF1 file went from 9.6 million instructions and 7 ms of CPU to 126 million and 122 ms.

The cost is paid once per process, before the first query plans. The planner is handed every declared relationship with whether it is verified, meaning every child row found a parent. That is two numbers in the link's header, and it was found by reading the link whole: every extent read, checksummed and copied into a payload, and then copied again into a `Link`. The keyed TPC-H schema declares ten relationships, and the links out of `lineitem` alone are tens of megabytes at SF1. A profile of `SELECT 1` was page faults, `memmove` and the checksum, nearly all under `stored_link`.

A long running server pays this once for each change to the catalog, which is small. The shell pays it on every run, which is how the benchmark runs rudb and how most people run it, and it is more than most TPC-H queries take at SF1.

## The change

`Link::counts` reads the three counts off the front of a payload, with the same form and layout checks `Link::read` makes. `Reader::payload_head` reads the first bytes of a section's first extent, checking the extent table but not the extent. `stored_link_counts` puts them together with the same binding check `stored_link` makes, so a link built against another parent, another column or an older generation of the parent is refused the same way. The planner's question now reads about a hundred bytes a relationship.

The header is not checksummed, since the checksum covers the whole extent and checking it would mean reading the whole extent again. That is safe because the header only decides what to plan. A plan that reads a link loads it through `stored_link`, which checks everything and refuses to run the join if the check fails, so a torn header can cost a query an error but not a wrong answer. That is the same thing any other torn page costs.

## Results

On server3 at SF1, `perf stat` on one process:

| | instructions | CPU |
|---|---|---|
| `SELECT 1`, setting off | 9.6 M | 6.7 ms |
| `SELECT 1`, setting on, before | 126.1 M | 121.8 ms |
| `SELECT 1`, setting on, after | 10.4 M | 6.4 ms |

All 22 answers with the setting on match main with it off. The binding test in `rudb-native` checks that the header and the whole link report the same counts and are refused for the same wrong parent name, parent column and child column.

## What is left

With the fixed cost gone, the setting changes three queries by more than noise, in instructions on one thread: q09 and q13 are faster with it on, and q12 is much slower, because its link join costs more than the hash join it replaces. q12 has to be fixed or kept off the link before `graph_sections` can be on by default.
