# LIKE answered on compressed codes

TPC-H q13 keeps the orders whose comment does not hold `special` followed later by `requests`. The comment is read by that filter and by nothing else, so the scan already drops the column right after the filter runs. Before this change the scan still had to produce the strings for the filter to search, and that was most of what the orders scan did.

## What the scan cost

A bare `SELECT count(*) FROM orders WHERE o_comment NOT LIKE '%special%requests%'` took 1.13 G instructions at one scale factor on one thread, against DuckDB's 1.03 G. A profile split it like this:

1. A third went to decompressing the comments, which the file keeps FSST compressed, into one buffer.
2. A fifth went to the substring search for `special`, and a few percent more to the compare after it.
3. The rest went to laying the strings out as a column and checking they were valid UTF-8, which nothing after the filter ever looked at.

## The change

A pattern that is some pieces between `%` signs, with no `_`, matches a string exactly when the pieces appear in it in order. Finding each piece as early as possible is never worse than finding it later, so the question is one walk over the bytes through an automaton with a state for every byte of every piece and a last state for having found them all. It is the usual table for finding one string in another, built for each piece, with each piece handing over to the next when it completes. `rudb_encoding::sequence` builds it.

A compressed string is a list of codes, each standing for up to eight bytes, and the bytes a code stands for are the same for the whole chunk. So where a code takes each state only needs working out once per chunk, and after that a string is one table lookup per code. The lookups are filled in the first time a state meets a code, so a chunk pays only for the pairs its strings reach, which for most strings is the first state and the codes that do not start a piece. An escape is followed by one raw byte, and that byte goes through the byte table.

The native reader answers the question for one part of a compressed text column and hands back the rows that pass, with nulls in neither answer. The scan asks for it when the whole pushed filter is such a `LIKE` or `NOT LIKE`, the filter is the only reader of the column, and no join filter reads it either. The other columns are read as usual and narrowed to those rows, and the string column comes back as nulls, which is what it became after the filter before. Any part the pages cannot answer, such as one not compressed or a table in memory, is read the usual way.

## Results

Instructions at one thread, before and after, with DuckDB after that:

| query | before | after | DuckDB |
|---|---|---|---|
| q13 | 1.605 G | 1.248 G | 1.559 G |
| `NOT LIKE` count on orders | 1.130 G | 0.544 G | 1.027 G |
| q09 | 1.876 G | 1.808 G | 1.796 G |

At default threads q13 went from 43.5 ms to 35.6 ms and the bare count from 25.1 ms to 13.6 ms. Every TPC-H answer is the same as before at one thread and at default threads.
