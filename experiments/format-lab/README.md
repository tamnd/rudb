# format-lab

The measuring instrument for M1, the format experiment.
It reads a real Parquet file, runs every column through the rudb encoding chooser, and prints what happened.

This is not part of rudb.
It is a separate workspace with its own lock file, it is not in `cargo xtask ci`, and it is not published.
It exists because the milestone asks nine questions that can only be answered on real files, and answering them needs a Parquet reader, and the published workspace has no external dependencies at all and is going to keep it that way.
Linking arrow-rs here keeps that promise intact.
When the report is written this crate has done its job.

## Running it

```
cd experiments/format-lab
cargo run --release -- stats /path/to/hits.parquet
```

A full pass over ClickBench `hits` is 100 million rows and 105 columns and takes a while.
For a first look, read a slice of it:

```
cargo run --release -- stats hits.parquet --rows 5_000_000 --markdown
```

The pairwise questions are a separate command, because they are about two columns at once and the answers do not fit in the same table:

```
cargo run --release -- pairs hits.parquet --markdown
```

`pairs` hashes every column once a row and combines the hashes, so all 5,460 pairs of a 105 column table cost a multiply and a compare each per row rather than a pass over the file each.
It prints which column determines which other column, which is what section 6.6 needs before it can drop a column and recompute it, and how much two string columns overlap, which is what section 6.4 needs before it can put them in one dictionary.

`--help` lists the rest.
The ones that matter are `--rows` to cut the run short, `--columns` to look at one column, `--threads` to match the machine, and `--no-verify` to skip the decode pass when you only want sizes.

## What it prints

One row per column: the estimated distinct count from a bottom-k sketch, the null count, the size Parquet stored that column in, the size the rudb chooser produced, the ratio between them, bytes per row, and which shape won how many chunks.
Then totals, encode and decode throughput, and the peak resident size of the process.

The ratio is against Parquet's own compressed column size, not against the raw values, because that is the comparison the milestone is about.
The `values as read` line is the raw size and it is there to make the throughput numbers mean something, not to be quoted as a compression ratio.

## Things to know before believing a number

The chunk is 122,880 rows, which is DuckDB's row group, so that sizes here can be put next to sizes DuckDB produces without an argument about the unit.

Nulls are stored as the empty string or as zero rather than in a validity bitmap.
The storage layer will not do that.
For a size experiment it is the right call, it keeps the value counts aligned across columns and it costs a run of zeros in whatever encoding wins, but a column that is mostly null is being flattered.

Floating point columns are skipped.
There is no float encoding yet and section 6.2 of the spec is deliberate about leaving it until the integer and string paths are settled.

Every fixed width type is read as an `i64`, including booleans, dates and timestamps.
A `u64` above `i64::MAX` and a decimal that does not fit are counted and reported, and any column with a nonzero count there has a size that is not the size of the real column.

When `--rows` cuts the run short, the Parquet side of the comparison is scaled by the fraction of rows read.
That is exact only if the rows read look like the rest of the file, which for `hits`, a log sorted by time, they do not entirely.
Full runs are the ones to quote.
