# 159. Codes widened a block at a time

## What was slow

Note 157 made the native reader widen the codes of a bit packed page to eight or sixteen bits when they are a few bits short of either, so that a filter compares them where they lie. The dates of TPC-H are twelve bits and `l_quantity` is thirteen, so every page of those columns is widened as it is decoded. `Vector::on_lanes` did that a code at a time: it read each code with `code_at` and wrote it with `write_code`, which reads the word the code shares with its neighbours, ors the code in and stores it back. A page is decoded once and then held, so a query run again over the same process does not pay this, but the first run of every query does, and for a query over lineitem's dates that was more than a third of its instructions.

## The change

When the codes start on a block of 64, `on_lanes` unpacks 64 of them at once with `unpack_block_at`, the same AVX2 shuffle, shift and mask an aggregate uses to read packed codes, and lays them down a whole output word at a time, four codes of sixteen bits or eight of eight bits ored together in registers and stored once. The rows past the last whole block, and a vector cut partway into a block, still go a code at a time. The test that widened codes read back the same values now also cuts at the first row and at row 128, which go through the blocks.

## Measured

At SF1 on server2, one thread, each query run once in a fresh process, which is when pages are decoded, against main at #2770, with this change built on #2769. The figures are the instructions of the whole process.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q01 | 1017 | 762 |
| q03 | 590 | 303 |
| q06 | 719 | 463 |
| q12 | 889 | 505 |
| q14 | 264 | 136 |
| q19 | 386 | 381 |
| q21 | 962 | 579 |

Run again in the same process, with its pages already decoded, each of q21, q06, q01, q12, q14 and q03 takes the same instructions as on main, as it should, since nothing after decoding changed. q19 reads few widened columns and barely moves. The answers to all 22 queries are the same bytes as before at one thread and at six.
