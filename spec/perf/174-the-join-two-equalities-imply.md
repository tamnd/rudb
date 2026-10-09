# 174. The join two equalities imply

## What was slow

q05 asks for lines whose customer and supplier are in the same nation of Asia. It says `c_nationkey = s_nationkey` and `s_nationkey = n_nationkey`, and never `c_nationkey = n_nationkey`. The exhaustive join order search from #2888 only knew the conditions as written, so customer could reach the filtered nation only through supplier, and customer meets supplier on the nation alone, which is a hundred and fifty thousand rows times ten thousand over twenty five. The search never took that join, and customer came in last, as a build side of all 150,000 customers on two keys. Orders were read for the year at 227,597 rows and lineitem through them at 910,519, where the plan before #2888 restricted customer to Asia first and read 46,008 orders and 184,082 lines.

## The change

The join order pass now puts the columns its equalities compare into classes, and for a class whose columns are each in a different table it offers the search an equality between every two of them that no condition compares directly. Between two sets of tables the search tests one condition of each class, since the others follow from it. The greedy order, which the pass falls back to past the search's budget, still sees only the conditions as written.

A class gets its implied edges only when two of its tables meet on conditions that multiply, by the same arithmetic the search scores a pair with. Without that rule q09 went from 288 to 492 million instructions. Its part key and supplier key each join lineitem to a key, so the implied edges added nothing the conditions did not reach, but they let part, partsupp and supplier be joined among themselves first. The rows said that was cheaper, and it ran twice as long, because the plan without them reaches partsupp and orders through stored links from the 319,404 lines the green parts keep, and dense lookups do the rest, where the other plan probes a hash table on two keys.

Two smaller fixes came out of looking at that plan, and both stay because they are right on their own. A join the search builds lists the condition with the most distinct values first, since the first key is the one a hash join hands the scan under its driving side. And the check that decides whether a build side's parents are few enough to read through the adjacency counts the parents the bitmap over the key values found rather than the build side's rows, since a side can hold one parent many times.

## Measured

At SF1 on server2 against main at #2899, one thread.

| query | main | this change |
| --- | --- | --- |
| q05 instructions | 380.5M | 176.7M |
| q05 cycles | 279M | 133M |
| q05 cycles, DuckDB | 318M | 318M |

The q05 plan is customer joined to the five nations of Asia on the implied edge, 30,183 rows, then orders at 46,008, lineitem at 184,082, and supplier last on both of its keys at 7,243. No other query moves by more than 0.3 percent in instructions, and the answers to all 22 queries are the same bytes as before at one thread and at six.
