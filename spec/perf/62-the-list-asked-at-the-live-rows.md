# 62. The list asked at the live rows

A filter runs its conjuncts one after another and hands each one the rows the ones before it kept, so a later conjunct only has to look at the survivors. `select_in` is how an `IN` list takes part in that: it returns the kept rows directly, looking only at the rows still in play. It only knew whole numbers, though. A list of strings fell back to `in_set`, which builds a flag for every row of the chunk, and the filter then read those flags back at the live rows.

q12's `l_shipmode IN ('MAIL', 'SHIP')` is that case. It is a string column, it costs more than the date conjuncts beside it so the filter runs it last, and by then about one row in seven is left. It still built flags for all seven, and that was 7% of the query.

## What it does now

`select_in` has a second arm for a list of strings over a dictionary column. The dictionary answers the question once per code, the same two ways `in_set` does. A dictionary that came with its sorted order is searched once for each list entry, and no value is read. Any other dictionary goes through the per-code memo, which decides a code the first time a row asks about it. What changed is which rows ask: only the live ones, and what comes out is the kept rows, written without a branch on the answer, instead of a flag vector over the whole chunk.

A null in the column or in the list still goes to `in_set`, which knows the rule that a miss against a list holding a null is null. An error during a lookup does too, so `in_set` reports it the way it always did.

## Numbers

TPC-H SF1 on server3, instructions best of three, against the same tree without the change. All 22 answers match.

| query | before | after | DuckDB |
|---|---|---|---|
| q12 | 0.882 G | 0.818 G | 1.027 G |
| q19 | 0.604 G | 0.578 G | 1.130 G |
| all 22 | 17.644 G | 17.603 G | 26.151 G |

The server was under a load of about 45 during the run, and q21 came out 5% higher in the suite. A best of five of q21 alone gave 1.567 G before and 1.562 G after, so that was noise. q19 also filters on string `IN` lists, over `p_container` and `l_shipmode`.
