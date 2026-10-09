# 182. An opened key unpacks a block at a time

## What was wrong

A grouping on more than one key opens every integer key that is not flat into a flat run once per chunk, so the hash and the probe read it by index. `Vector::opened` does that, and it is meant to take the same paths `Vector::flatten` takes. The two each had their own copy of the chain, and only `flatten` learned the unpack of a whole packed column 64 codes at a time and the unpack through a selection. So a packed key went through the general copy, which lists every position, marks each one live, builds a validity from the marks and then reads each code on its own with 128 bit arithmetic.

## The change

`flatten` and `opened` now share one private `written_out`, which tries the dictionary decode, the runs, the whole block unpack and the unpack through a selection before it falls back to the general copy. `flatten` still counts itself against `Cause::Flatten` and `opened` still does not.

## Measured

Instructions per run at SF1 on server2, steady state at one thread, before and after.

| query | before | after |
|---|---|---|
| `part` grouped by `p_brand, p_size` | 20M | 6M |
| `part` grouped by `p_type, p_size` | 25M | 12M |
| `part` grouped by `p_brand, p_type, p_size` | 128M | 116M |

TPC-H q03, q10, q13, q16 and q18 do not move, since their keys come out of the scan flat or as dictionaries. The three key grouping is still twice what DuckDB runs, and what is left there is opening a group a row at a time, which is the next thing to look at.
