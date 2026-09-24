# FSST at one store a symbol

After #1721, FSST decompression of the order comment was a fifth of TPC-H q13. The comment is stored FSST compressed, and the `NOT LIKE '%special%requests%'` filter needs the text of every one of the 1.5 million orders, so the decompression itself cannot be skipped. What could go was how it wrote its output.

## What it was doing

A native chunk of FSST strings was decoded one string at a time through `SymbolTable::decompress`. That appends each symbol to a `Vec` by extending it by eight bytes and truncating it back to the symbol's length, so every code paid a capacity check, a length update and a truncate. About six instructions went into each byte it produced.

`SymbolTable::decompress_at` already did it the other way, one eight byte store into room made ahead of time and a step of the cursor, and the replay of matched chunks used it. The plain FSST chunk did not.

## What it does now

The chunk decoder walks the runs and decompresses each one with `decompress_at`. A run of `n` codes writes at most `n` symbols of at most eight bytes, the last store included, so `8n` bytes of room past where the run starts is enough. The room is made by doubling the buffer, so the zeroes written to make it add up to at most twice the size of the decompressed chunk, and the buffer is cut back to what was written at the end.

## Results

Instructions per run, one thread, SF1, against DuckDB 1.5.

| query | before | after | DuckDB |
|---|---|---|---|
| q13 | 2.105 G | 1.857 G | 1.578 G |
| q20 | 0.787 G | 0.732 G | 0.822 G |
| q22 | 0.457 G | 0.424 G | 0.346 G |
| q10 | 1.078 G | 1.053 G | 1.189 G |

At the default thread count q13 went from 2.40 G to 2.16 G, under DuckDB's 2.34 G. The other queries moved less than one percent apart from q18, whose change came from #1718 merging in between. All 22 answers are the same.
