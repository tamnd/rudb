# Audit of rudb-bench#71

The report is `reports/rudb-duckdb-root-cause-20260914.md`, merged as tamnd/rudb-bench#71. It says the one million row ClickBench gap is mainly serial execution, a sequential Parquet reader and high cardinality aggregate state, and that the growth from 100k to 1m is linear rather than quadratic.

I checked every structural claim against the source at `cfdf975` and re-ran the ladder myself. The report is honest and the diagnosis is broadly right. Below is what holds, what is stated more strongly than the data supports, and what is missing.

## Claims that hold

**The engine runs on one thread.** Confirmed, and it is worse than the report says. `rudb-pipeline` has `run_serial` and nothing in the engine calls it. `crates/rudb-exec/src/lib.rs:73` says so in as many words: `adapt` is what drives the tree, it is a pull tree, and cutting the plan into pipelines is still ahead of us. So the first step of F4 is not swapping a driver, it is building the thing a driver would drive.

**A Parquet scan hands out exactly one morsel and locks one reader.** Confirmed at `crates/rudb-exec/src/source.rs:460`, which is `one: Handout::new(1)`, and at `source.rs:563`, where `read` takes a mutex over a single `FileReader` for the whole file list. Adding threads above this changes nothing. The morsel machinery itself is already shaped correctly, since `Handout` is an atomic and the other three sources hand out one morsel per chunk, so this is a `FileScan` problem and not a design problem.

**A grouped aggregate refuses a second instance.** Confirmed at `crates/rudb-exec/src/group.rs:109`, with a test at `group.rs:1420` that asserts the refusal. Combining two instances needs a serialize and a combine per aggregate function, which `spec/engine/07-aggregate.md` section 7.8 already lists as debt.

**Group keys are general values on the heap.** Confirmed at `crates/rudb-exec/src/key.rs:22`, where `Key` is a `Vec<Value>` used as the key of a `std::collections::HashMap`. Every probe of every row allocates a vector, and a text key allocates a string inside it. That is one allocation per row per group by, and it is also where our peak memory goes.

**The `threads` setting does nothing.** Confirmed at `crates/rudb/src/settings.rs:163`. It parses, it stores, it reads back, and no execution path consults it.

**Growth is linear.** Confirmed on my own run. Query time goes 542 ms at 100k to 4,568 ms at 1m, which is 8.4 times for ten times the rows. There is no quadratic term to find.

## Where the report overstates

The report says parallelism alone cannot get us to ten times because rudb already spends more total CPU than DuckDB. At one million rows that is true but the margin is small, 4.280 CPU seconds against 3.560. At one hundred thousand rows it is false and the sign is reversed: rudb uses 0.300 CPU seconds against DuckDB's 1.180, which is four times less.

The real shape is this. Our CPU per row per query goes from 73 ns at 100k to 104 ns at 1m, so it gets worse as cardinality grows, which is the hash tables falling out of cache. DuckDB's goes from 274 ns to 83 ns, so it gets better, which is its fixed per query cost being amortised over more rows. The two curves cross somewhere between 100k and 1m. A sentence that reads as a constant property of the engine is actually a statement about where the curves happened to cross on one sample size, and if we only ever quote the 1m number we will misjudge both engines.

The consequence for planning is real. DuckDB pays a fixed cost per query of roughly 12 ms at this scale, spinning up and coordinating its pool, and we pay roughly 3.2 ms. That is a lead we currently hold and every parallel scheduler design risks spending it.

## What the report leaves out

**The native storage format.** The report's dependency list has six items and none of them is F2. Meanwhile the harness says FileScan is 52.9 percent of our CPU across the suite, and the row underneath it says why: DuckDB loads once into its own format in 1.963 seconds and is then timed on that, while we decode Snappy Parquet inside every query and our load column is empty. This is the largest single line item on the board and the milestone that addresses it is not mentioned. Everything else on the report's list is correct and none of it touches the 52.9 percent.

**The per query floor.** Ten times below DuckDB's 533 ms over 41 queries is 1.3 ms per query. Our cheapest query today is `SELECT count(*)` at 3.2 ms, and it reads no column at all. So the target is below our floor and the floor is part of the work, not something that comes free once the heavy queries get faster. F2's lazy column metadata is the item that addresses it, since a file with 105 columns should not cost 3 ms to answer a question that touches none of them.

**The memory target is not an aggregate rewrite alone.** The report is right that peak RSS has to fall from 172 MiB to 31 MiB, and right that q34 and q35 are what set it. What it does not say is that a compact arena is not enough on its own. q34 groups one million rows by `URL`, which is about 600,000 distinct strings averaging sixty bytes, so the strings alone are 36 MB before any table structure. The only way that fits in 31 MiB is to never hold the string: group on a dictionary code that came out of storage already assigned. That is F2's column scoped global dictionary, which the F2 issue already names and already says is worth four of the seven expensive ClickBench queries.

**q24 is now a Fetch problem.** The report predates the late materialisation work landing in full. On today's run q24 is still our worst query at 528 ms against DuckDB's 60, and the suite wide Fetch line is 355 ms, which is 8 percent of everything and almost all of it is this one query. Late materialisation took q24 from about 1.03 seconds to 0.53 and was clearly worth it, but the cost moved rather than vanished, and reading ten scattered rows back out of 105 columns is now its own item.

## What I would change in the report's dependency list

The order in the report is row group morsels, then a pipeline runner, then aggregate combine, then compact keys, then predicate pushdown, then re-audit. Three edits:

1. Compact keys move above aggregate combine. Merging N tables of `Vec<Value>` is a worse thing to build than merging N arenas, and the memory target needs the arena whether or not there is a second thread.
2. F2 goes on the list, above predicate pushdown, because it is 52.9 percent of the CPU, it is what makes the comparison fair, and it is the only route to the memory target on q34 and q35.
3. Re-auditing is not step six, it is what happens after every step, at 1k to 1m, which takes four minutes.
