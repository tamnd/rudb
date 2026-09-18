# The F series, re-ordered by what the measurement says

The milestone numbers do not change. F1 is still the data plane, F2 is still the storage format, F4 is still parallelism and F5 is still hash and aggregation. What changes is the order we build them in and which items inside each one are on the critical path.

The order they were written in was F1, F2, F3, F4, F5, and it was a sensible order for building an engine from the bottom up. It is the wrong order for closing a ten times gap, because it puts the largest multiplier fifth.

## The one number that decides the order

With no CPU reduction at all, today's 4.280 CPU seconds spread over sixteen effective cores is 268 ms, against DuckDB's 533. Parallelism alone does not get us to ten times, but it flips the sign. We would go from 9.25 times behind to about two times ahead on the same run, without making a single loop faster.

Nothing else on the list has that property. Every CPU reduction we can name is worth between two and eight times on one line item that is itself a fraction of the total. Parallelism is worth an order of magnitude on everything at once.

So parallelism goes first, and the work that unblocks it goes before that.

## The critical path

**Stage one, unblock the threads.** Three things stand between us and a parallel driver and all three are small.

1. A Parquet scan hands out one morsel and locks one reader, so sixteen threads would queue behind a mutex. A morsel becomes a row group and each one carries its own reader state.
2. A grouped aggregate refuses a second instance, so any pipeline with a group by in it stays serial whatever the driver does. That needs a combine per aggregate function, and it needs a group key worth combining before it needs the combine.
3. Nothing cuts a plan into pipelines. `adapt.rs` drives a pull tree and `run_serial` is not on the execution path at all. The plan has to become a list of pipelines with edges between them before a scheduler has anything to schedule.

**Stage two, the compact group key.** This is F5's first item and it is here rather than later for three reasons. It is 27.6 percent of the CPU. It is all of the peak memory. And it is what stage one's combine has to merge, so building the combine first means building it twice.

**Stage three, the scheduler.** One thread pool per database, morsels from a shared queue, parallelism per pipeline bounded by a floor so that a small query does not pay for threads it cannot use. This is where the 268 ms above gets collected.

**Stage four, radix partitioning.** Sixteen threads each with their own hash table is sixteen tables, so the naive version of stage three makes the memory number worse by an order of magnitude at the moment it makes the time number better. Partition by hash, do not replicate.

**Stage five, the storage format.** The largest CPU line and the only route to the memory target on q34 and q35. It is also the slowest milestone to build, which is the reason it is fifth rather than first, and the reason to start profiling the existing encoder in parallel with stage one rather than waiting.

**Stage six, the kernels.** What is left of F1 and the start of F7. Filter is 10.7 percent and the specialised form pairs are what take it down.

## What this means for each milestone

**F1, the data plane.** Cut it down. The items on the budget are the macro generated kernel specialisations for flat against flat, flat against constant and dictionary against constant, and the flatten and fallback counters, because the counters are the instrument that tells us which pair to write next. `Form::Nested` and expression fusion move to F7, where encoded execution lives and where they have a consumer. F1 closes when the specialisations and the counters land, and 0.4.0 goes out then.

**F2, the storage format.** Promote the two items that the budget depends on and let the rest follow. Lazy column metadata, because it is the per query floor. Column scoped global dictionaries, because they are the memory target on the two worst queries and four of the seven expensive ones. Per block statistics, because they are what lets a scan skip. The block, page and tile structure has to come with them. The encoder's own speed, which the F2 issue rightly calls the thing most likely to slip, is not on the critical path for the benchmark and should not be allowed to block the read path.

**F3, memory and larger than memory.** Mostly off the critical path, with one exception. q19 and q33 do not run at all today because the hash aggregate does not spill, so we publish 41 of 43 and the ratio has an asterisk on it. Spilling turns that into 43 of 43 and removes the asterisk, which is worth doing before any number goes on a board.

**F4, parallelism.** First, not fourth. The morsel queue, the pool, the per pipeline degree and the floor. The exchange node, the wait for graph and the NUMA hooks are the parts that can wait, because they are about being right later rather than being fast now.

**F5, hash, aggregation and top k.** Second, not fifth, and the order inside it is key encoding, then combine, then radix partitioning, then the table implementations. The four hash tables behind a seam are a sweep worth running once there is something to sweep, and they are not the thing that takes the first eight times.

**F6 to F11.** Unchanged and after. Nothing in ClickBench is a join, so F6 does not move the number this roadmap is about, and it should not be allowed to jump the queue on the strength of being interesting.

**The D series and the compatibility issues.** They stay open and they stay off the critical path. The exception is anything that stops a benchmark query from running, which today is nothing, since q19 and q33 are a spilling problem and not a dialect one.

## The first ten pull requests

Ordered so that each one is measurable on the 1k to 1m ladder the day it lands.

1. One morsel per row group in a Parquet scan, each with its own reader. No speed change on one thread. The check is that the scan still answers the same rows with the morsel count forced to one per row group.
2. ~~Restore order where a query asked for one and not where it did not, so that out of order morsels are a scheduling decision and not an answer change.~~ #490. `Sink::at` tells a sink which morsel it is being fed from, `root_in_order` holds chunks until the morsels in front of them are done, and the bound on what it holds never parks the instance whose chunks are next out.
3. ~~A normalized group key. One comparable byte string per row in an arena, with the table holding a hash and an offset.~~ Replaced, see below. What landed instead is taking `Value` out of the group table at the probe, the insert and the way out.
4. ~~Key cases chosen at plan time. A single fixed width key of 64 bits or less used directly, two packed into 128. Expect q17 and q32 to move.~~ Built as #491 and taken back out as #492, because it measured slower. See below.
5. ~~Combine for count, sum, min, max and average, and the aggregate stops refusing a second instance.~~ #493. `Accumulator::combine` is one match over the state enum and `Aggregate::merge` is the probe the fold already does, reusing the hash the incoming table stored. A `DISTINCT` call and a spilled instance are still refused, and item 8 is what fixes the second.
6. ~~Cut the plan into pipelines and run them through `run_serial`. Delete `adapt.rs`.~~ #497. `build` returns a `Query`, which is the pipelines and the order they run in, and `adapt.rs`, `operator.rs` and `cancel.rs` are gone. It cost 1.3 percent on query time and 1.9 percent on CPU rather than nothing, measured interleaved at 1m, because the result now sits in the root queue before the caller drains it and a breaker's counters read a clock on the way back out.
7. ~~One thread pool per database and a morsel scheduler that runs one pipeline on N threads, with the degree bounded below so a small query stays on one thread. This is the pull request that collects the order of magnitude.~~ #508. 1.62x on query time at 1m interleaved, for 1.38x the CPU and 1.47x the peak memory. The degree comes from `Pipeline::degree`, which is the pool ceiling capped by what `Source::morsels` counts, so a small query is serial because it has one morsel rather than because anything tested for small. Every sink was audited and the ones whose output order is their input order refuse a second instance, which is why the `COUNT(DISTINCT ...)` queries did not move. The 1m file has nine row groups, so nine was the ceiling that run actually reached out of thirty two.
7b. ~~Merge the distinct sets of two aggregate instances, so a `COUNT(DISTINCT ...)` stops holding its whole pipeline on one thread.~~ #516, closing #509. Not on the original list, and taken ahead of item 8 because the ladder after item 7 said it was the bigger of the two remaining buckets and the smaller of the two changes. 1.44x at ten million rows on its own, which is 2.65x counting from before item 7, and between 1.7 and 8.3 times on the nine queries it was about.
8. Radix partitioned aggregation as the default, so N threads is not N tables. #510, with #518 as the plumbing under it. After 7b it is the whole of the top of the list rather than a third of it: about 6.4 seconds of the 13.7 the ten million row suite now takes, against about 3.5 in the wide scans that are already going six to nine times. Two of the distinct queries are in it as well, because their sets are large enough that merging them is a real pass rather than a formality. A tree merge was tried first and taken back out, which is the section below.
9. Re-run the ladder at 1k, 10k, 100k, 1m and 10m, publish it in rudb-bench, and write down what each of the eight bought.
10. Lazy column metadata in the Parquet reader, which is the per query floor and the one F2 item that does not need the format to exist.

After ten, the remaining gap is the storage format and the filter kernels, in that order, and the budget in `02-budget.md` says what each has to deliver.

## The tree merge was built, measured and taken out

This is the fourth time the roadmap has had an item that looked right on paper and lost to the measurement, and the pattern in all four is the same: the arithmetic was done on the critical path and not on the total work.

The item was not on the list. It came out of the ladder after item 7, which said that a group by with several hundred thousand groups was getting between 1.1 and 1.7 times from thirty two threads where a scan was getting between 6 and 9.7. The reading was that the fold parallelises and the merge does not, since thirty two instances were merged one after another on the thread that started the query. The arithmetic said q34 at ten million rows takes 1.749s on thirty two threads against 2.652s on one, so about 1.42s of the threaded run is the serial merge, and pairing the instances off into five rounds instead of thirty one merges in a row should take that to about 0.3s.

It measured slower. #517 merged it and #518 took it out.

What the arithmetic missed is that merging two tables is not like adding two numbers. It costs about one probe per group on the smaller side, so the cost of a merge depends on how large the two tables are, and in a tree the same group is probed again at every level. Thirty two tables of g groups cost about 5 by 16g in a tree against 31g in a line. When the tables overlap heavily the union stops growing, every merge at every level costs about g, and the tree really does win on the critical path. When they do not overlap the union doubles at every level, the critical path is g plus 2g plus 4g plus 8g plus 16g, which is the same 31g the line had, and all that is left of the change is the extra work.

Count distinct over a high cardinality column is the case where the sets overlap least and q5 at ten million rows went from 211ms to 956ms. The queries where the keys do repeat, q34 and q36, went 1.15x and 1.19x. Over the whole suite it was 13.492s before against 13.778s after, hot CPU 83.2s against 95.0s, peak RSS 2.17 GiB against 2.29 GiB.

Two things came out of it worth keeping.

The first is the half of the change that stayed. An instance now combines on the thread that built its state rather than handing it back to the caller, so a merge overlaps the other instances' scans instead of waiting for all of them to join. That is neutral on its own and it is what a partitioned merge needs underneath it.

The second is the reason partitioning is the right answer and a cleverer merge order is not. Partition p of the kept table only ever takes partition p of an arriving one, so a group is probed exactly once however many instances there are. The total work is the 32g that the line does, the parallelism is the P that the tree wanted, and neither is bought at the other's expense. That is what the DuckDB writeup on their aggregate hash table describes and it is what #510 builds.

A rule to add to the two in `04-loop.md`. When an item claims a speedup from doing the same work in a different order, write down the total work in both orders before writing any code. If the total work goes up, the critical path has to go down by more than that, and for a merge it usually does not.

## What would make me change this plan

If stage three lands and the effective core count comes out below eight on the ClickBench shapes, the plan is wrong and the scheduler is the problem rather than the enabler, and the next thing to profile is the scheduler rather than the next line item.

If the compact group key lands and Aggregate does not fall by at least three times on one thread, then the cost is in the probe and not in the key, and the hash table implementations from F5 move ahead of the storage format.

This one fired. See the section below.

## What the profile changed, written down on the day it changed it

Pull request 3 above was going to be a normalized group key in an arena. It is not, and the reason is worth keeping because it is the second time the roadmap has aimed at the storage layout when the cost was somewhere else.

`table.rs` already stores group keys column at a time, with a hash per slot and a run of `i32` or `i64` or packed bytes per key column. That landed under #237 and the roadmap was written without checking it. So the arena was not the missing piece.

Callgrind on `GROUP BY URL` and `GROUP BY UserID` over the million row file said where the time actually is. Both profiles are dominated by `Value`. On `UserID`, `Aggregate::fold` calls `Vector::value_at` exactly once per input row at 131 instructions a call, with `drop_glue::<Value>` on the same call count behind it, and `Aggregate::finish` spends 14 percent of the query in `Vector::from_values`. Add the `Value::heap` charging and it is around 40 percent of the query in building and dropping tagged values around a table that already stores the keys in the right shape.

On `URL` the picture is different in a way that matters for what comes next. Snappy decompression is 20 percent, `memcpy` is 18 percent, and `core::str::converts::from_utf8` is 18 percent. The probe itself is cheap there, because the varchar path already compares through `bytes_at` and the profile shows the memcmp at 0.37 percent. So the string group by is not a grouping problem at all, it is a decode problem, and it belongs with the storage format in stage five rather than with the hash table.

Two things follow.

The first is what landed as pull request 3: `Vector::signed_at`, the integer sibling of the `text_at` and `bytes_at` that already exist for exactly this caller, and the three sites in the group table that stop crossing the `Value` boundary because of it. Small, contained, and aimed at a number the profile named.

The second is that `from_utf8` at 18 percent of a varchar group by is a separate finding and is not in the ten. `string.rs` documents the intent already: a scan over a page where the format guarantees UTF-8 wants to validate the page once instead of once per string, and the reader does not do it yet. That goes on the list for stage five, next to the decompression it sits beside.

The rule this keeps proving is the one in `04-loop.md`. Profile before writing, including when the plan was written two days ago by somebody who had just read the profile.

If lazy column metadata lands and the floor does not fall from 3.2 ms, then the floor is not metadata and I have misread the profile.

## The compact group key was built, measured and taken out

Pull request 4 above is done in the sense that it was written, gated, merged as #491 and reverted as #492 an hour later. It is worth keeping the numbers because the trigger two sections up fired exactly as written, and because this is now the third time the roadmap has aimed at a layout when the cost was somewhere else.

What was built was the thing the item asks for. One or two key columns whose physical type is a signed integer of 64 bits or less get a narrow key chosen when the aggregate is built. Each chunk's key columns are flattened once, with the type matched on once per column rather than once per row, into a run of words with a small present bitmap beside it, and the hash is mixed in the same pass so that the narrow table and the general one agree by construction. The probe after that is two array reads and a compare with no vector in it at all.

The 1m rung on gpc, run back to back against its own parent on the same machine, says it is slower on every query it touches.

| | 4ad5820 | 5b34268 |
| --- | --- | --- |
| q16, `GROUP BY UserID`, narrow | 62.088ms | 63.373ms |
| q31, `GROUP BY SearchEngineID, ClientIP`, narrow | 93.050ms | 94.443ms |
| q32, `GROUP BY WatchID, ClientIP`, narrow | 93.987ms | 96.877ms |
| q17, `GROUP BY UserID, SearchPhrase`, general | 132.740ms | 143.862ms |
| Aggregate over the suite | 1.030s, 58.5ns a row | 1.072s, 60.8ns a row |
| hot cpu | 3.950s | 4.160s |
| query time | 4.293s | 4.498s |
| peak RSS | 170.89 MiB | 172.14 MiB |

The premise was already false when the item was written, and it was false for a reason that is in this document twice already. #237 stores the group key a column at a time in a run of `i32` or `i64`, so the stored key was as narrow as the flat words are before any of this started, which is why the memory row does not move. #488 made the probe read the key where it lies through `Vector::signed_at` instead of through a `Value`, so the compare was already an integer out of a slice. All that was left to win was the per row match inside `signed_at`, and buying that costs a 16 KB write and a read back per chunk per key column, and a strided gather on the way out when the finished groups become vectors again. A key of two columns pays both twice.

The rows are split by whether the query could take the narrow path at all, because that is where the second finding is. q16, q31 and q32 all took it and all lost between one and a half and three percent, which is the flatten costing more than the dispatch it removes. q17 groups by `UserID` and `SearchPhrase`, and a varchar key does not fit the shape, so q17 ran the same general path it ran before and lost eight percent anyway. That is the cost of the enum that the shape choice put in `Table::holds` and `Table::insert`, which every probe step of every group by now goes through, landing in the middle of `Aggregate::fold`, which callgrind puts at 21.9 percent of q17 on its own. The queries that could not use the abstraction paid more for it than the queries that could saved.

So the item is closed as measured and wrong rather than left open, and what it bought is the answer to the question it was asked: the cost in a grouped aggregate is not the shape of the key.

The rule this adds to the one in `04-loop.md` is that a roadmap item written against a profile still has to be checked against the code before it is written, because two pull requests that landed since the profile had already taken the thing it was aiming at. Grep the tree for the problem before building the solution.

## The q17 profile after the revert

Taken on gpc at 6433d82 over the million row file, callgrind, `SELECT UserID, SearchPhrase, count(*) FROM hits GROUP BY UserID, SearchPhrase ORDER BY count(*) DESC LIMIT 10`. 1.55 billion instructions in total.

| share | where |
| --- | --- |
| 21.91% | `Aggregate::fold`, which is the probe and the accumulate inlined into one |
| 10.74% | `core::str::converts::from_utf8` |
| 6.81% | `memcpy` |
| 6.21% | `snappy::decompress_into` |
| 6.04% | `Aggregate::finish` |
| 5.07% | `vector::push_value` |
| 7.48% | `Vector::bytes_at`, in two copies |
| 3.42% | `Vector::signed_at` |
| 3.29% | `drop_glue::<Value>` |
| 3.16% | `memset` |
| 2.83% | `table::StringColumn::string` |
| 2.22% | `vector::string::StringColumn::push` |
| 2.02% | `Vector::from_values` |
| 2.00% | `LogicalType::eq` |
| 1.51% | `Aggregate::fresh` |
| 1.33% | `Value::heap` |

Three things to take from it.

`Aggregate::fold` at 21.9 percent with the probe inlined into it is the number the narrow key was aimed at and did not move. `signed_at` and `bytes_at` between them are another 10.9 percent and they are the reads inside the compare, so the key handling really is about a third of this query. The part of that which is the shape of the key is the 3.42 percent in `signed_at`, and the experiment says the flatten costs more than that.

`push_value`, `from_values`, `drop_glue::<Value>`, `StringColumn::string` and `Value::heap` add up to 15.5 percent and they are all in `finish` and `fresh`, which is the way out of the table and the initial state of an accumulator. #488 took `Value` out of the probe and the insert. It did not take it out of the accumulator, and the accumulator is now the larger half.

`LogicalType::eq` at 2 percent is a type comparison running per row somewhere it should not be. That is a small and self contained thing to find and it is worth a look before anything larger.

`from_utf8` at 10.74 percent is the same finding as before, the varchar page being validated once per string instead of once per page, and it is still filed against stage five.

## The UTF-8 ASCII fast path was built, measured and thrown away

The `from_utf8` line above is 10.7 percent of q17, 12.1 percent of q34 and 17.8 percent of q40, and it has been on this list since the first audit. The obvious change is to check `is_ascii` first and skip the validator when the bytes are plain, since almost every ClickBench string is a URL. It was built on branch f701 against the string column's `push_in_place`, gated green and measured on gpc.

Callgrind at a million rows on q34 says the change did what it said. `from_utf8` fell from 328 million instructions to 209 million and `push_in_place` inclusive fell from 267.7 million to 217.6 million, which is about two percent of the query. Its own self cost rose from 48.0 million to 116.7 million, which is the inlined `is_ascii` doing the work the validator used to do, so the saving is the difference between a full UTF-8 walk and a byte scan rather than the whole line. 149,915 strings of about a million still took the slow path, so the URL column is not all ASCII and the fast path does not always fire.

The wall clock at ten million rows over 32 threads was a wash on q34, q28 and q24, inside the noise on every pass. Worse, the total instruction count for the identical main binary varied between 2.47 billion and 2.71 billion across runs, so at this resolution the totals cannot carry a two percent claim either.

The branch was deleted and the finding kept. Two rules come out of it.

An instruction count win under callgrind does not imply a wall clock win when the query is bound by something else. q34 is bound by the sixteen shared mutexes of the grouped aggregate, which is root cause four in `07-the-root-causes.md`, and shaving two percent off the work each thread does between lock acquisitions moves nothing.

The line is still worth attacking, but structurally rather than with a fast path. The scan should not be validating at all, because the read path validates the same bytes again when a string is handed out, and a Parquet BLOB column that is not valid UTF-8 is readable in DuckDB and is an error here. That is a change to the contract rather than a branch inside the loop, and it is filed that way in `07-the-root-causes.md` as root cause two.
