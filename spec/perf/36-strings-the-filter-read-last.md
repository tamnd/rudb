# Strings the filter read last

TPC-H q13 left joins customer to the orders whose comment does not match `%special%requests%` and counts orders per customer. The comment is read by the filter in the orders scan and by nothing after it. On one thread at SF1 rudb spent 2.34 G instructions on q13 against DuckDB's 1.59 G, and two of the places that cost were about that comment after the filter was done with it.

## What the profile said

The join builds on customer and probes with the 1.48 million orders the filter keeps. The probe gathers every column of the probe side into its output, and the comment was one of them, so every kept comment was copied out string by string into a new column that the aggregate above never read. `Vector::copied` and `copy_of` under the probe were about a twelfth of the samples.

Note 32 already made a scan hand up a column only its own filter reads as a null constant, but only for packed and dictionary columns. The note measured q13 five percent worse when the comment went the same way. The reason was in the gather: a constant fell through to the general copy, which builds a position list, walks it for nulls and makes a flag per row before it notices every row holds the same value. A null constant is now gathered as a null constant of the new length, and a constant that is not null is too when every position is inside it. With that the scan hands up the comment as a null constant, and the probe's gather of it is a few words.

The second cost was reading the comment pages at all. Every string of a native varchar page was checked for UTF-8 on its own as it was recorded, which is one call and its setup per forty odd bytes, and `utf8::valid` was another twelfth of q13. Text cut only where a character starts is text in every piece, so a page's strings are now checked in one pass over the run they sit in, plus a look at the byte after each cut to see that it does not continue a character. That refuses what the check per string refused, including a cut through the middle of a character, and a unit test holds it to that. Blob and bit pages are still not checked, since they never claimed to hold text.

## Results

Instructions per run, one thread, SF1, rudb before and after against DuckDB 1.5.

| query | before | after | DuckDB |
|---|---|---|---|
| q13 | 2.341 G | 2.113 G | 1.586 G |
| q20 | 0.805 G | 0.772 G | 0.802 G |
| q22 | 0.488 G | 0.460 G | 0.343 G |
| q10 | 1.090 G | 1.063 G | 1.183 G |

At the default thread count q13 went from 2.60 G to 2.37 G against DuckDB's 2.19 G. Every other query moved by less than one percent either way, and all 22 answers are the same as before.

## What is left on q13

FSST decompression of the comment is a fifth of the samples and the filter needs the text, so the next step there is matching the pattern against the compressed form or skipping pages the pattern cannot hold. The two aggregates, one grouping orders by customer and one grouping customers by their count, are most of the rest.
