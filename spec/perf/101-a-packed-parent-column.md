# 101. A packed parent column

## The problem

A link join gathers its parent's columns out of the whole column once a chunk reaches more than half of the parent's parts, and on TPC-H q09 every chunk does. The whole column was laid end to end flat, so `ps_supplycost` was 6.4 MB and `o_orderdate` 6 MB, and the 319,404 lineitem rows that survive the part filter reach into both at row ids in no order. Together they are more than the 8 MB last level cache of server2, so most of those gathered rows are a miss to memory. The two LinkJoin operators were 25 ms of cpu each at one thread, the largest operators of the query after the lineitem scan, and an instruction count does not show why, because a miss is one instruction.

## The change

The parts of the column are still decoded and laid end to end as before, and then a column with no nulls and a 16, 32 or 64 bit layout is bit packed over its own range with `Vector::bit_packed`, which keeps it flat when packing would not halve it. The gather already reads a packed column without unpacking it, one shift and mask a row when the rows are spread out and one unpack of the span when they are close together. `ps_supplycost` packs to 17 bits and `o_orderdate` to 12, so the two are about 4 MB between them and fit the cache.

The budget is still counted over the parts as they are decoded, flat, so a column that fits packed and not flat is refused as before. That is conservative and leaves the refusal rule alone.

## Results

Pending the release build, posted on the pull request.
