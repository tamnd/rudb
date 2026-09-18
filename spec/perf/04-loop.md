# The development loop

Run the small ladder on every change, because it takes four minutes and it is the only thing that tells you whether the change did what you thought.

## The command

On `gamingpc-wsl`, which is the only machine in the fleet that can run this honestly:

    export PATH=$HOME/.cargo/bin:$PATH
    export TMPDIR=$HOME/rudb-tmp
    export RUDB_BENCH_RUDB=$HOME/benchrun/rudb-f0/target/release/rudb
    cd ~/benchrun/rudb-bench
    for rows in 1k 10k 100k 1m; do
      ./target/release/rudb-bench run clickbench --engines rudb,duckdb --rows $rows --runs 5
    done

Four sizes, 43 queries, five hot runs each, both engines, about four minutes end to end. The local Mac cannot be used for any of this and never could.

## What to read out of it

Four rows and one block, in this order.

**`query time`** is what each engine says the queries cost it, and it is the row the public board publishes and the row the ratio is taken on. Use it for the headline.

**`hot cpu`** is the row that cannot be bought with hardware. A change that improves query time and leaves CPU alone bought its win with cores. A change that improves CPU is real work. Report both, always, and if only one can be quoted, quote CPU.

**`peak RSS`** is set by the single worst query and by nothing else, so it moves in steps rather than smoothly, and a change that halves the memory of thirty nine queries and leaves q34 alone does not move it at all.

**`vs duckdb on 41 shared`** is the number the goal is about.

**`where rudb spent it, by kind of operator`** is the block at the bottom, and it is the work list. If a change was meant to move FileScan and Aggregate moved instead, something else happened.

## Why the ladder and not one size

The four sizes say different things and all four are needed.

At 1k and 10k almost nothing is being measured except what it costs to start, and we win both by a wide margin. That lead is real and worth protecting, since the per query floor is one of the four axes, but it is not evidence about the engine.

At 100k we use four times less CPU than DuckDB and are 1.7 times behind on wall clock, which is the clearest single picture of the parallelism gap that exists anywhere in the data.

At 1m the CPU curves cross and we start to lose on both axes, which is where the per row costs become the story.

Reading any one of the four alone gets you a wrong conclusion, and reading 1m alone gets you the specific wrong conclusion that our per row cost is worse than DuckDB's as a general fact, when it is better at 100k and worse at 1m.

## Before anything gets claimed

The ladder is a development number and the harness says so in its own output. Two things have to happen before a number leaves the repository:

1. Run it at 10m. Every win at 1k and 10k evaporates there and some wins at 1m do too, so 10m is where a change proves it scales rather than that it moved a constant.
2. Run all 43 queries. Today we run 41 and skip q19 and q33 because the hash aggregate does not spill, and every ratio we quote carries that asterisk until F3 removes it.

## The rule that keeps this honest

DuckDB loads the file into its own format first and is timed on that. We read the Parquet where it lies and decode it inside every query. The harness prints both facts in the `storage` and `load` rows and it is right to, because it is the largest single structural difference between the two columns and it is 52.9 percent of our CPU.

Until F2 exists, every comparison we publish has to carry both rows. After F2 exists, the native format column becomes the headline and the Parquet column stays as a second row, because reading somebody else's file quickly is a real thing to be good at and dropping the measurement the moment it stops flattering us is not.
