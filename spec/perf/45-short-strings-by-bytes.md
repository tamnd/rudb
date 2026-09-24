# Short strings cut and looked up by their bytes

TPC-H q22 takes the first two characters of every customer's phone number and asks whether they are one of seven country codes, once in the query and once in the subquery that finds the average balance. Both steps did far more work than the question needs.

## What it cost

At one scale factor on one thread q22 took 0.42 G instructions against DuckDB's 0.34 G. A profile of it split like this:

1. A sixth went to looking the two characters up in a hash set of `String`, which ran the standard library's keyed hash over each two byte value.
2. A tenth went to checking each phone number was valid UTF-8, once before cutting it and once more before looking it up, though a value read out of a string column is always text already.
3. The cut itself checked the whole fifteen byte phone number was ASCII before taking two bytes of it.

## The change

`substring` with a start of one or more and a length of zero or more now cuts the bytes directly. A character starts at every byte that is not a UTF-8 continuation byte, so the two ends of the cut are found by counting those, and only as far as the end of the cut. For a phone number that means reading two bytes.

A list of strings in an `IN` is now held as bytes and compared as bytes, with no check for text. As with the whole numbers, a list of eight entries or fewer is kept as a list rather than a hash set. If every entry of such a list fits in eight bytes, each entry is kept as one word plus its length, so a row costs a load and a compare per entry and no call to compare bytes. Comparing the bytes through `memcmp` seven times a row was a third of what was left after the hash went.

## Results

Instructions at one thread, before and after, with DuckDB after that:

| query | before | after | DuckDB |
|---|---|---|---|
| q22 | 0.416 G | 0.338 G | 0.335 G |

Wall time for q22 went from 20.0 ms to 15.6 ms on one thread and from 6.9 ms to 6.0 ms at default threads. No other TPC-H query changed, and every answer is the same as before. What is left is decompressing the phone numbers and the anti join to orders.
