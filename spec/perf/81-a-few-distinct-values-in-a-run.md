# 81. A few distinct values in a run

## The problem

q16 counts the distinct suppliers of each `p_brand`, `p_type` and `p_size`. At SF1 that puts 118,274 rows into 18,314 groups, so a group sees about six suppliers. At eight threads the query cost 1069 M instructions against 793 M on one, three copies each.

A `COUNT(DISTINCT BIGINT)` keeps one set of values per group. The set held its first value inline and became a hashbrown table at the second, so nearly every q16 group took an allocation, grew and rehashed twice on the way to six values, and paid a hashed insert per row. The profile at eight threads put about a sixth of the query in that table's insert and rehash and in `malloc` and `free`. Eight threads pay it more than one, because each instance builds most of the groups for itself and the merge then moves every value into the kept group's set again.

## The change

The set has a third form between the one value and the table: a run of up to 16 values in a boxed array, searched from the front. The second value allocates the run once, and a value is a compare against the ones already there. The seventeenth value moves the run into a table sized for twice that, which is the table the set would have had anyway. The enum stays four words, so an aggregate with one group per row pays no more memory than it did.

The merge takes values out of a run the same way it takes them out of a table, so it gets the same saving.

## Results

server3, SF1 native, three copies of q16 in one process, `perf stat` against main after note 80:

| threads | instructions before | after |
|---|---|---|
| 1 | 793 M | 733 M |
| 8 | 1069 M | 930 M |

The answers to all 22 queries are the same as before at one thread and at eight. server3 was shared while this ran, so no times are given.

## What is left

Eight threads still cost 200 M more than one. The rest comes from the same place as note 80's sums: each instance makes nearly all of the 18,314 groups, and the merge makes them again. The fold also still hands each new value to the count's accumulator as a `Value`, one call per value.
