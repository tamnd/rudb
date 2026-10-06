# 142. A statement tokenized once, and symbols looked up once

## What was slow

Planning q02 cost about nine million instructions, measured as the difference between running its `EXPLAIN` a few times and many times. The query itself is 26 million, so a third of what q02 costs went into getting from text to a plan. Two pieces of that had nothing to do with planning.

1. The shell asks `rudb::is_complete` after every line whether the statement typed so far is finished, and that tokenizes the whole statement each time. q02 is 46 lines, so its first line was tokenized 46 times, its second 45 times and so on, before the statement ran and was tokenized again to be split and parsed.
2. The matcher compares a grammar symbol such as `(` or `,` to a token by comparing the token's text with the symbol's text, which is a call into memcmp. The same token is tried against many symbols as the matcher works through the alternatives of a rule, so every one of those tries paid for the call.

## The change

1. A line can only finish a statement if it has a semicolon in it or closes a block comment. Only a semicolon ends a statement, and a line without one can end it anyway only by closing a block comment that came after one. Any other line leaves the last token something other than a semicolon, so the shell does not ask. The first line of a statement is always checked, because a line of nothing but a comment is complete on its own.
2. The matcher looks up which symbol each token spells once, when it starts, and keeps the answer beside the token. Matching a symbol is then an index compare, the same as matching a keyword already was.

## Measured

At SF1 on server2, one thread, against main at #2687.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| `EXPLAIN` q02 | 9 | 7 |
| q02 | 26 | 24 |
| q11 | 27 | 26 |
| q16 | 134 | 133 |

q06, q17 and q22 run within one million instructions of before. The answers to all 22 queries are the same bytes as before at one thread and at six.
