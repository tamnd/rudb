# 166. The next child in the same word

## What was slow

A scan that a reduction or a filter left with a few rows of a child table asks the link for the parent of each one, through `Link::forward_each`. For a monotone link that was a walk of the bitmap a word at a time from the last child answered, up to a thousand children, and a `select1` and a `rank0` past that. Every child paid a select in its word, about forty instructions of byte sums and a table load, even when it was the next row of the same parent, which is most of them, since a filter on a parent keeps all of its children. On a link of 200,000 parents of four children each with one parent in 268 held, the shape of the `partsupp` rows that q02 keeps through `part`, this came to 142 instructions a child.

The push of a parent set down a monotone link had two smaller costs. A set of few parents read on through every parent between two held ones, and the held parents came out of the chained iterator over the set's three forms, which the compiler stopped inlining once the walk grew.

## The change

`forward_each` keeps the bit of the last child it answered. A child a few past it is the same number of one bits further on, and when those are still in the word it is found there, with a trailing zero count for the very next child and a select in the word for one further on. A child past the word searches the count of ones before each word, stepping eight words from the last one and then galloping, where it used to walk.

The push finds a held parent more than 1,024 parents on with a select rather than reading on, writes the children it keeps as a list when the set's share of the parent would make them a list, and walks the held parents with one loop over the form the set is in. A push over a link of another form lays a sparse set out as a bitmap first, since it tests every child it reaches. The keys a link hands a scan are collected into a vector of the right length rather than grown a push at a time.

Moving the line between a list and a bitmap from one member in a thousand to one in sixty four was tried with this. It took 2.7 million instructions off q17 and put 2.8 million on q20 and 1.7 million on q08, whose sets between the two lines are tested a row at a time further on, so the line stays where it was.

## Measured

At SF1 on server2, one thread, thousands of instructions a run, from a query run five times in one process less once, against the build of #2860. The cycles on server2 move by more than these differences while it is loaded, so the table is instructions, which are the same from run to run. The micro query is `SELECT sum(ps_supplycost), count(*) FROM part, partsupp WHERE p_partkey = ps_partkey AND p_size = 15 AND p_type LIKE '%BRASS'`.

| query | main | this change |
| --- | --- | --- |
| micro query | 3,763 | 3,410 |
| q02 | 24,019 | 23,621 |
| q03 | 71,383 | 69,535 |
| q05 | 95,101 | 93,416 |
| q09 | 352,978 | 348,392 |
| q10 | 143,916 | 131,508 |
| q12 | 81,808 | 81,269 |
| q20 | 145,113 | 143,495 |
| all 22 | 2,439,522 | 2,415,989 |

No query went up by more than q16's 0.15 percent. On the link above, `forward_each` went from 142 instructions a child to 103. The answers to all 22 queries are the same bytes as before at one thread and at six.
