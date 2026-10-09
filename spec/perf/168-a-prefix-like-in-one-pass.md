# 168. A prefix LIKE in one pass

## What was slow

`p_name LIKE 'forest%'` is a prefix test, and a string view holds its length and its first four bytes inline, so most rows are answered without reading the arena at all. The loop that asked it went row by row: it read the view, compared the length, compared the first four bytes, and read the arena for the rest of the prefix, with a call and a few branches between each step. q20 asks this of all 200,000 parts twice at SF1, once for the parts the query keeps and once more for the copy of them that decorrelating the correlated sum puts under `lineitem`, and the loop was 13 percent of its profile. `StringView::prefix` was also not marked `#[inline]`, so the four bytes cost a call in the crate that asked for them.

## The change

When the column has no nulls, the length and the first four bytes of every row are tested in one pass with no branch in it, writing a bool a row. A second pass reads the arena only at the rows that passed, and only when the prefix is longer than four bytes. The negated form flips the answers at the end. A column with nulls takes the old loop. `StringView::prefix` is now `#[inline]`.

## Measured

At SF1 on server2, one thread, thousands of instructions a run, from a query run five times in one process less once, against the build of #2889. q22 moves too, through its tests of the first two characters of `c_phone`. No other query moves by more than 0.05 percent.

| query | main | this change |
| --- | --- | --- |
| q20 | 143,107 | 126,362 |
| q22 | 52,924 | 50,220 |
| all 22 | 2,678,335 | 2,658,885 |

The answers to all 22 queries are the same bytes as before at one thread and at six.
