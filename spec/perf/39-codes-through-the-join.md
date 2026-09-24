# Codes through the join

A native table keeps a column with few distinct strings as codes into one dictionary for the whole table, and a scan of it hands up those codes with the dictionary beside them. TPC-H `part` has `p_brand` (25 values), `p_type` (150), `p_container` and `p_mfgr` stored that way, and `nation` and `region` have their names. Most of the queries that read these columns read them on the gathered side of a hash join.

## Why it was slow

The gathered side of a join lays its chunks end to end into one vector per column, so that a probe can gather any row out of it. Any piece that was not flat was flattened first, because a probe reads rows by position and a dictionary piece used to mean a dictionary per chunk, whose codes do not line up from one chunk to the next. So the codes came out of the join as strings, and everything above the join paid for strings. On q16 the grouping on `p_brand`, `p_type` and `p_size` over 118,274 joined rows hashed and compared the bytes of both strings on every probe, and that was about a third of the query at one thread.

## What changed

When every piece of a gathered column holds codes into the same table wide dictionary, the codes are laid end to end instead and the column stays a dictionary over the shared values. That is four bytes a row rather than the string. A probe's gather of a stable dictionary already keeps it stable, so the joined rows carry the codes to whatever reads them, and the grouping, the sort and the comparison kernels already read stable codes: the grouping table holds a run of codes and compares a code to a code, and when the codes are the only key the hash is of the code too.

A column whose pieces point at two dictionaries, or where any piece is flat, is laid out as strings the way it was. A join key under two different dictionaries, such as `p_brand = ps_brand` across two tables, still compares the strings, because the lookup only compares codes when both sides point at the same dictionary.

## Results

Instructions per run, SF1, one thread, against DuckDB 1.5. The other queries moved by under 2 percent, none of them up, and all 22 answers are the same.

| query | before | after | DuckDB |
|---|---|---|---|
| q04 | 0.789 G | 0.752 G | 0.851 G |
| q09 | 2.015 G | 1.855 G | 1.772 G |
| q12 | 1.108 G | 1.085 G | 0.912 G |
| q16 | 0.432 G | 0.328 G | 0.328 G |

At default threads q16 went from 0.523 G to 0.402 G against DuckDB's 0.597 G, and q09 from 2.062 G to 1.909 G against DuckDB's 2.066 G.
