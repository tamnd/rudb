# 94. A link key taken from the parent's key map

## The problem

After #2299 the `lineitem` scan of q03 reads its columns at the rows the orders hand it through the link and the ship date keeps, a few dozen rows a part. One of those columns is the order key, which `lineitem` keeps as runs of deltas because it is stored in order key order. Reading a run length page at a few rows still walks the ends of every run in the part, so the order key cost about a tenth of the query no matter how few rows were wanted.

## The change

A row that a link points at a parent holds the key that parent holds, since that is how the link was made. So when a join has already read the link and the parent's key map to reduce the scan, `Scan::read_reduced` in `rudb-exec` now takes the link column at its rows from those two instead of reading it: the link gives each row's parent and the key map gives the parent's key. `KeyMap::keys_at` answers a list of parent rows for the identity and dense forms, using a new `Rank::word_holding` to find the word that holds a row's bit, and the sorted and permuted forms leave the column to be read as before. A row with no parent, a link or key map not read yet, and a column that is not a plain integer all read the column the way they did before.

## Results

Measured on server3 at SF1, one run in a fresh process, threads 1, millions of instructions, against main at #2346 built from the same base:

| query | before | after |
|---|---|---|
| q03 | 347 | 299 |
| q04 | 227 | 235 |
| q05 | 418 | 421 |
| q08 | 361 | 334 |
| q10 | 277 | 282 |
| q20 | 433 | 429 |

No other query moved by more than 1 M. Every answer is the same at 1 and 8 threads. q04 and q10 now take the order key from the map at rows where reading it was about as cheap, and that is left for later.
