# 183. A selection lists its rows without a branch a row

## What was wrong

A filter leaves a mask, and anything that reads the rows it kept by position asks the selection for them as a list of `u32`. `Selection::indices` built that list by walking the set bits of every word and pushing each one, so it took a branch on whether a word was empty and a branch on every row it kept. Under a filter that keeps a few rows in a hundred most words hold none, one or two, and whether a word was empty was close to a coin toss. Under a filter that keeps half the rows the branch on every row was a coin toss too. On TPC-H q14 the listing was about 5 percent of the query, and under a filter that keeps half of `lineitem` it was a third of a `sum` over what was left.

## The change

`listed` now counts the rows first, makes room for them and 128 more, and writes every word into that room in one of four ways chosen by how many rows the word keeps, none of which branches on a row.

- A word with up to four rows writes four whatever it holds, and one with up to eight writes eight. The slots past its rows are written over by the next word.
- A word that drops eight rows or fewer is written as the runs between the rows it drops, each a fixed 64 wide copy cut back to its length.
- Anything between goes a byte at a time through a table of the rows each byte names, as one load, one add and one store of eight lanes, and moves on by the rows the byte has.

A new test lists masks of eleven densities from empty to full against a plain walk of the bits.

## Measured

Instructions and cycles per run at SF1 on server2, steady state at one thread, main at the same base before and the change after. The machine is shared, so cycles for the `sum` are the range of two runs each, and for q14 and q06 the median of six runs alternated between the two.

| query | instructions before | after | cycles before | after |
|---|---|---|---|---|
| `sum(l_extendedprice)` where `l_shipdate < DATE '1996-01-01'` | 132M | 103M | 63M to 70M | 54M to 58M |
| q14 | 42M | 44M | 46M | 46M |
| q06 | 43M | 45M | 45M | 43M |

The sparse queries run 2M more instructions, which are the stores a sparse word writes past its rows, and the cycles do not move with them, since what they replace was the branches that missed. q15 runs 54M instructions on both.
