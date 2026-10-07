# 158. A LIKE prefix as one number

## What was slow

A `LIKE` pattern that is a prefix and a `%` is decided on the view of each string: the length and the first four bytes, which every view holds whether the string sits in the view or in the arena, and only a row they agree with goes on to read the rest from the arena. The four bytes were compared as `view.prefix()[..head] == prefix[..head]`, where `head` is the prefix length capped at four. That is a slice of a length known only at run time, and Rust compares such a slice with a call to `memcmp`, so the call the view was meant to save was still made once per row. TPC-H q20 asks `p_name LIKE 'forest%'` of the two hundred thousand parts, and in a profile of its instructions the `LIKE` kernel and `memcmp` together came to about a sixth of the query.

## The change

`like_run` builds the wanted four bytes and a mask of the bytes that count once per call, as two `u32`. A row then reads its view's four bytes as one `u32`, masks them and compares the result with the wanted number, which is an and and a compare with no call. A prefix longer than four bytes still reads the arena for a row whose first four bytes agree, as before. The existing `like` tests cover prefixes shorter than, equal to and longer than four bytes.

## Measured

At SF1 on server2, one thread, against main at #2757.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q20 | 104 | 93 |
| q14 | 44 | 44 |
| q16 | 112 | 112 |
| q13 | 190 | 190 |

q14, q16 and q13 also run a `LIKE` and do not move. The answers to all 22 queries are the same bytes as before at one thread and at six.
