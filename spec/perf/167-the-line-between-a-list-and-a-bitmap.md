# 167. The line between a list and a bitmap

## What was slow

A set of row ids is a sorted list below one member in `SPARSE_RATIO` rows and a bitmap above it. Section 4.3 put the line at one in a thousand, on the side of the bitmap because a scan tests a set in row order. Most of what is done with a set is not a test, though. A push writes it, a scan counts the parts it touches and the members in each part, and a reduced scan takes the members of each part it reads, and each of those is a member at a time for a list and a word at a time for a bitmap. At one part in a thousand, which is q17's brand and container, and one `partsupp` row in 268, which is what q02 keeps through `part`, the bitmap was written, counted and walked a word at a time for a few hundred members.

Note 166 tried the line at one in sixty four, where a list stops being the smaller of the two, and it put more on q20 and q08 than it took off q17. q20 keeps one part in 94, and as a list its members were counted into parts with a division each and laid out from the bitmap the push wrote, which a word at a time costs less for a set that dense. The filter of a reduced scan also tested each row it kept against the set, which for a list is a search a row.

## The change

The line is at one member in 256 rows, a few times sixty four, since a member costs a few times what a word does. The filter of a reduced scan walks a list's members in the part alongside the rows it kept, a list's parts are counted with a compare a member and a division a part, and a bitmap is laid out as a list in one allocation of the right length.

## Measured

At SF1 on server2, one thread, thousands of instructions a run, from a query run five times in one process less once, against the build of #2869. The other queries move by less than 0.3 percent either way.

| query | main | this change |
| --- | --- | --- |
| q02 | 23,621 | 23,305 |
| q08 | 65,058 | 64,614 |
| q17 | 80,609 | 77,623 |
| q19 | 89,774 | 88,720 |
| q20 | 143,495 | 143,531 |
| all 22 | 2,415,895 | 2,410,665 |

The answers to all 22 queries are the same bytes as before at one thread and at six.
