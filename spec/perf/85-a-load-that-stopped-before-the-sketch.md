# A load that stopped before the sketch

[`82-a-filter-that-wrote-a-row-number-for-every-row.md`](82-a-filter-that-wrote-a-row-number-for-every-row.md) and [`84-a-bounds-check-cheaper-than-both-ways-round-it.md`](84-a-bounds-check-cheaper-than-both-ways-round-it.md) left the runtime bitmap where it should be, and the next item on the TPC-H profile was `rudb_encoding::sequence::Coded::holds`, the walk behind `LIKE` over compressed text, at 4.78 percent of the suite and 56.11 percent of q13 on its own. q13 filters `o_comment NOT LIKE '%special%requests%'` over 1,500,000 rows, and 15,702 of those rows actually hold the two words, which is 1.05 percent. The sketch in front of the walk exists precisely so that the other 98.95 percent are never walked, and it was turning none of them away.

It was turning none of them away because the file did not have one.

## The file the benchmark measured

`grep -ac RUDBTG1` over the corpus finds nothing, and neither does `RUDBKM1`, `RUDBFL1`, `RUDBAJ1`, `RUDBGD1`, `RUDBSP1` or `RUDBRP1`. Asking the file to build them says why:

```
Invalid Input Error: invalid rudb native file: the file is format 29 and a graph section needs format 30, so it has to be written again
```

So every number this suite has produced was measured against a file with no text sketches, no key maps, no links, no degrees and no projections, and with whatever a build from before format 30 wrote for the two statistics sections. That is not a corpus anybody chose. It is the file the harness happens to leave behind, and the reason it leaves that file behind is one comment in `rudb-bench`:

```rust
// One process for every table, so the load is one open and one metrics document per
// statement rather than a process start per table. There is no CHECKPOINT after it the way
// DuckDB has one, because there is nothing to checkpoint: a native file is durable when the
// statement that wrote it returns, and the clock stops after that.
```

Every word of that is true about durability and none of it is true about sections. A native file is durable when the statement returns, and a native file that has never been checkpointed has no summary of any column it did not write one for inline, no sketch of any text column, and no graph section of any kind, because `CHECKPOINT` is where all of those are built. The DuckDB side of the same harness runs `CHECKPOINT` before it stops the clock. The rudb side has never run one, so the loader that was written to be fair to DuckDB on load time has been unfair to rudb on every query since.

## What the checkpoint is worth

One binary, one parquet export of one copy of the data, loaded twice: once and left alone, once and then `CHECKPOINT`. Counted at ring 3, one thread, one query per process, three rounds, minimum of rounds, `SELECT 1` subtracted:

| TPC-H SF1 | no checkpoint | checkpoint | |
| --- | --- | --- | --- |
| q13 | 824.9 M | 400.4 M | 0.485x |
| q09 | 1213.5 M | 1182.7 M | 0.975x |
| q16 | 241.9 M | 238.3 M | 0.985x |
| q04 | 306.7 M | 308.2 M | 1.005x |
| suite | 10.35 G | 9.89 G | 0.956x |

All twenty two answers are unchanged and nothing else moves by more than 0.3 percent. ClickBench over 999,975 rows of hits reads 4.50 G against 4.45 G, which is 0.989x, with all forty three answers unchanged:

| ClickBench | no checkpoint | checkpoint | |
| --- | --- | --- | --- |
| q23 | 292.8 M | 280.5 M | 0.958x |
| q26 | 47.4 M | 45.5 M | 0.959x |
| q37 | 9.7 M | 9.3 M | 0.962x |
| q21 | 360.8 M | 348.5 M | 0.966x |
| q24 | 447.5 M | 432.4 M | 0.966x |
| q15 | 120.8 M | 117.3 M | 0.971x |
| q22 | 408.1 M | 396.2 M | 0.971x |
| q38 | 9.6 M | 9.9 M | 1.031x |
| q13 | 32.3 M | 33.3 M | 1.030x |
| q35 | 64.7 M | 65.6 M | 1.014x |
| q14 | 76.0 M | 77.0 M | 1.013x |

q21 to q24 are the four that filter a long text column with `LIKE`, which is the sketch doing the one thing it is for. The five that read above 1.000x are all small in absolute terms, the largest of them 1.0 M on q13, and they are a plan reading a statistic it did not have before rather than the sketch costing anything.

## What it costs

The checkpoint is not free and the harness should charge it. Loading TPC-H SF1 from parquet is 61.57 G instructions and leaves a 265,431,877 byte file. The `CHECKPOINT` after it is 16.36 G instructions, which is 27 percent on top of the load, and the file comes out at 341,499,144 bytes, which is 29 percent larger. On ClickBench the checkpoint is 9.60 G instructions and the file goes from 212,903,588 bytes to 246,699,304, which is 16 percent. Most of that is the sketch itself, at eight bytes a row of every text column long enough to be worth one, and `grams::TEXT_GRAMS_SHARE` allows it half of the table's stored column bytes, which at SF1 is generous enough to admit `l_comment` at 48 MB beside `o_comment` at 12 MB.

Against DuckDB that changes which side is smaller. The same data in the DuckDB file this box compares against is 279,457,792 bytes. rudb without the checkpoint is 265 MB and wins on size; rudb with it is 341 MB and loses. On TPC-H a suite saving of 0.46 G a run against 16.36 G once is paid back after about thirty five runs, so for a board that loads once and queries forever the checkpoint is clearly worth it. On ClickBench it is 0.05 G a run against 9.60 G once, which is about a hundred and ninety runs, so there the sketch is close to free on queries and is mostly buying the four `LIKE` ones. For the half of this project's goal that is about resource rather than speed the eight bytes a row want a harder look than they have had. Note that nothing here is a choice a query can make: there is no setting that turns the sketch off at read time, so the only way to measure with it and without it is two files.

## Where q13 went

q13 is 400.4 M now and `Coded::holds` is 5.10 percent of it, which is 20 M against the 462 M it was. Nothing else in the walk changed, so that is the sketch turning away everything it can:

```
   10.72%  rudb_exec::group::Aggregate::sink
   10.02%  Vec<u32> from_iter in Reader::rows_holding
    9.49%  the closure in Reader::rows_holding
    7.21%  rudb_native::seeded_checksum
    6.50%  rudb_encoding::integer::decode_chunk
    5.62%  rudb_vector::Packed::values_at
    5.10%  rudb_encoding::sequence::Coded::holds
    2.99%  rudb_encoding::sequence::Coded::learn
```

The sketch lets through about four percent of rows against the one percent that match, so the walk still pays three times what a perfect filter would leave it, and the remaining 20 M is small enough that the loop it runs is no longer the thing to look at. `Coded::learn` at 2.99 percent is now most of the way to `holds` itself, and it is not walking anything: it is filling a fresh table of steps for each 1024 row chunk, because the walker is built per chunk and learns the same codes again every time. The two lines above them, at 19.5 percent between them, are the sketch's own test and the `Vec` of kept row numbers it writes, which is note 82's lesson in a second place: a filter that keeps 96 percent of 1,500,000 rows writes 1.44 M row numbers to say so.

## What this means for the numbers already published

Every note in this directory that measured a diff measured it with both sides on one corpus, so every ratio still says what it said. What moves is the baseline the ratios are against. Note 82's 10.36 G for the suite and 0.686x against DuckDB were both on the unsketched file, and the same tree on a checkpointed file is 9.89 G, which against the same 15.09 G of DuckDB is 0.655x. The corpus to measure against from here is one written by the build under test and then checkpointed, and the harness has to be the thing that does it rather than a step somebody remembers.
