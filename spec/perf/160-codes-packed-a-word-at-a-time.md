# 160. Codes packed a word at a time

## What was slow

A strided page, which is how the native writer stores a column whose values step by a fixed amount, such as `l_quantity`, a stride of 100 over six bit codes, is decoded flat and then packed again, because a page is held in memory packed (note 107). Packing went through `pack`, which took each value's code as an `i128` difference checked into a `u64` and wrote it with `write_code`, which reads the word the code lands in, ors the code in and stores the word back, and does the same to the next word when the code spills into it. After that `on_lanes` widened the same codes to sixteen or eight bit lanes (note 157), so every code was laid down twice. In a profile of q06 run once in a fresh process with note 159 in, `pack` and `on_lanes` were each about a fifth of the instructions.

## The change

`lay_codes` builds each output word in a register: it ors the codes in as they come, stores the word once when it is full and starts the next with the bits of the code that did not fit. The code is the value less the base in the low 64 bits, which is the same number since it fits in the width. `pack` uses it for every layout, so a column packed when a file is written gets the same bits as before. `Vector::bit_packed_on_lanes` packs at the width of the lanes straight away when `on_lanes` would widen the result, and the native reader uses it for strided pages. The widened codes test now also checks that packing straight at the lanes reads back the same values as the flat vector.

## Measured

At SF1 on server2, one thread, each query run once in a fresh process, which is when pages are decoded. The figures are the instructions of the whole process, against main at #2773 built on #2769.

| query | main (M instructions) | this change (M instructions) |
| --- | --- | --- |
| q06 | 463 | 342 |
| q01 | 762 | 641 |
| q21 | 579 | 569 |
| q12 | 505 | 495 |
| q03 | 303 | 295 |
| q19 | 381 | 381 |

q06 and q01 read the most strided columns of lineitem. Run again in the same process q06 and q01 take the same instructions as on main. The answers to all 22 queries are the same bytes as before at one thread and at six.
