# The goal: four axes, and the honest version of 10x

The stated goal is 10x faster than DuckDB and 10x less resource, on every benchmark. That sentence is not achievable as literally written and this document says exactly which parts of it are, which parts are not, and what has to be true for the achievable parts to close. If this document is wrong, the project is a waste of five years, so it is written to be argued with.

Everything here rests on document 03, which is the measurement. Read them together.

## 2.1 The starting position, stated bluntly

On ClickBench at `c6a.4xlarge`, summing the best hot run of each of 43 queries, from the public result JSONs recomputed locally on 10 September 2026:

| System | Hot total | Cold total | Load | On disk |
|---|---|---|---|---|
| Umbra | 8.10 s | 49.79 s | 164 s | 8.30 GB |
| ClickHouse | 18.07 s | 110.97 s | 219 s | 9.42 GB |
| DuckDB | 26.25 s | 115.19 s | 126 s | 20.46 GB |
| DataFusion | 45.57 s | 182.91 s | 10 s | 14.78 GB |
| Polars | 45.35 s | 179.30 s | 10 s | 14.78 GB |
| Velox | 81.51 s | 182.49 s | 11 s | 14.78 GB |

Two facts in that table should reset anyone's priors before they start.

**Ten times faster than DuckDB is 2.63 seconds, and the fastest CPU engine in the world on this workload is at 8.10.** So the target is 3.1x past Umbra, a compiled-query engine from Neumann's group at TUM that has been under development for seven years and represents the current ceiling. It is not "beat DuckDB." It is "beat the state of the art by a factor of three."

**The Rust and Arrow ecosystem is currently 1.7x behind DuckDB, not ahead of it.** DataFusion at 45.57 and Polars at 45.35 are the best that stack currently does on this board. Writing the engine in Rust buys nothing on the scoreboard by itself. The first 1.7x is catch-up.

## 2.2 The thesis

The order of magnitude is in the physical layout of the data, not in the execution of the operators, and a general-purpose engine captures it by choosing and revising layout at runtime rather than fixing it at design time.

This is not an opinion. It is the ablation from [Bespoke OLAP](https://arxiv.org/abs/2603.02001) (PVLDB 19(11), 2026), described in document 01.1. Their synthesized engine, restricted to a flat columnar layout and allowed to specialize only its code, got 1.26x on TPC-H and 0.57x on CEB. Allowed to specialize the layout, it got 12.35x and 51.40x. Query compilation, operator fusion, restrict hints, unrolling, branchless predicates and software prefetch, which is the entire content of the usual "we made it fast" story, is priced by that ablation at about 26 percent.

The corollary for this project is uncomfortable and important. **Building a very good vectorized execution engine in Rust and being pleased with it gets us to roughly DuckDB, not past it.** Every hour spent on kernel micro-optimization before the storage layer is right is an hour spent on the 26 percent.

The three concrete mechanisms this thesis cashes out to, developed in documents 05, 06 and 09:

**Global dictionaries as a storage decision, not a user annotation.** ClickHouse has `LowCardinality(String)` and it is a schema decision the user makes. A column like `URL` in ClickBench hits is not low cardinality in the ClickHouse sense, but it has enormous structural redundancy and a bounded distinct count, and if the distinct values live in one global dictionary rather than per-row-group dictionaries, then `GROUP BY URL` becomes `GROUP BY u32` and never touches a byte of string data. Section 2.4 shows what that alone is worth.

**Encodings that execution runs on directly rather than decoding first.** A frame-of-reference column is filtered by transforming the predicate, not the data. A run-length column is summed by arithmetic on the runs. A dictionary column is grouped on its codes. Document 06.7 is the full matrix of which operators can run on which encodings.

**Multi-column compression.** [FastLanes](https://www.vldb.org/pvldb/vol18/p4629-afroozeh.pdf) introduced the mechanism, which exploits correlation between columns and is the thing columnar formats have historically been unable to do. In ClickBench hits, `URL` and `Referer` and `Title` are heavily correlated, `URLHash` is a function of `URL`, `EventDate` is a truncation of `EventTime`, and the resolution and window dimension columns move together. This is where the resource axis lives, and it is open question one in document 19 because nobody has published what it does to this specific dataset.

## 2.3 Axis 1: compatibility

**The claim.** rudb is a drop-in replacement for DuckDB v2.0 on three surfaces, measured continuously rather than asserted.

**Storage format.** rudb opens, queries, writes and checkpoints DuckDB v2.0 storage-format files. The conformance test is a round trip in both directions: take a DuckDB-written file, read it in rudb, write it back, and require that DuckDB reads the result and produces identical query answers for the whole corpus; and the reverse. Document 12.2. This is a reimplementation against the format, and the format is only partly documented, so section 12.6 is honest about the reverse-engineering required and about the fact that DuckDB Labs is under no obligation to keep it stable.

**Extension C ABI.** An extension binary compiled against DuckDB v2.0's stable C API loads into rudb and runs. This became a well posed problem only with v2.0, which defines the API in a versioned YAML specification with per-symbol lifecycle and stability tags. Before that the API was a hand-maintained function-pointer struct that changed shape every release. The test is that we take the actual published community extension binaries and load them. Document 12.3.

**SQL dialect.** rudb passes the DuckDB SQL logic test corpus at 99 percent or better with every failure enumerated and categorized in a checked-in file, not hidden. The number is produced by `rudb-compat` on every commit. Document 14.

**What this axis explicitly does not claim.** Bug-for-bug compatibility on undefined behaviour, identical error message text, identical `EXPLAIN` output, or identical floating point results where DuckDB's result depends on aggregation order. Document 14.10 is the ledger of permitted divergences and requires each one to carry a reproduction and a written argument for why it is not matched.

## 2.4 Axis 2: aggregate performance

**The claim.** 10x on total ClickBench hot runtime and on the ClickBench Combined metric against DuckDB, same instance, in the untuned load-and-go category. 10x on TPC-H SF100 total. 5x or better on TPC-DS, JOB and CEB.

Here is the arithmetic that says whether 10x is reachable, worked from the measured board rather than from ambition.

**Scenario one: match Umbra on every query.** Total 8.10 s, which is 3.2x over DuckDB. So a perfect implementation of the current state of the art gets us a third of the way and no further. This is the single most important number in the specification.

**Scenario two: 10x over DuckDB on every query where DuckDB is more than 2.5x off Umbra, and Umbra parity elsewhere.** That is 35 of the 43 queries and 79.4 percent of DuckDB's time. Total 5.03 s, which is 5.2x. Still not there.

So 10x requires beating Umbra substantially on the queries where Umbra itself is slow, and Umbra's time is extremely concentrated. Seven queries account for 70.4 percent of its 8.10 seconds:

| Q | Umbra | DuckDB | Shape | Where the 3x comes from |
|---|---|---|---|---|
| 28 | 1.393 | 6.478 | `REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', ...)` | specialize the pattern to a scanner, not a regex VM |
| 32 | 1.323 | 2.035 | `GROUP BY WatchID, ClientIP ORDER BY c DESC LIMIT 10` | top-k without materializing 100M groups |
| 18 | 0.846 | 1.650 | `GROUP BY UserID, minute(EventTime), SearchPhrase` | dictionary codes, dense minute key |
| 34 | 0.732 | 2.198 | `GROUP BY 1, URL ORDER BY c DESC LIMIT 10` | global dictionary makes this an integer group-by |
| 33 | 0.730 | 2.054 | `GROUP BY URL ORDER BY c DESC LIMIT 10` | same |
| 16 | 0.368 | 0.892 | `GROUP BY UserID, SearchPhrase` | same |
| 13 | 0.309 | 0.781 | `COUNT(DISTINCT UserID) GROUP BY SearchPhrase` | exact distinct by partitioned sort, not hash sets |

That table is the whole project on one page. There are exactly three technical problems in it.

**Problem one: high-cardinality grouping where the query only wants the top of the distribution.** Q32, Q33, Q34, Q15, Q16, Q17 and Q18 all end in `ORDER BY count DESC LIMIT 10` and all of them currently build a hash table with tens of millions of entries in order to answer a question about ten of them. Q32 groups by `WatchID, ClientIP` where `WatchID` is near-unique, so nearly every group has count 1 and the entire table is thrown away. The general technique is a two-pass heavy-hitter algorithm: pass one runs a bounded-memory frequency sketch (Space-Saving or Misra-Gries) that provably contains every key with frequency above a threshold in a fixed small table, pass two verifies the candidates exactly. It is exact, not approximate, because pass two counts the candidates precisely and the sketch's guarantee bounds what can be missed. This is a real, publishable execution technique, it is general (any `GROUP BY ... ORDER BY agg LIMIT k` where the aggregate is monotone), and it turns an O(distinct) memory problem into an O(k) one. Document 07.5.

**Problem two: strings as group keys.** Q33 and Q34 group 100 million URLs; Q16, Q17, Q18 group `SearchPhrase`; Q13 groups it too. If `URL` is stored under a global dictionary, the group-by key is a `u32` code, the hash table is dense and small, and the string bytes are never touched until the final ten rows are materialized. This is not a new idea in isolation, it is what ClickHouse's `LowCardinality` does when a user asks for it, and it is what DuckDB's dictionary vectors do within a single vector. Making it a global, automatic, cross-row-group property is the storage-layout specialization the thesis is about. Document 06.4 covers how the dictionary is built incrementally without a blocking global pass, which is the hard part.

**Problem three: string transformation at scale.** Q28 is 17 percent of Umbra's total and 25 percent of DuckDB's. The regex is a URL authority extraction and it does not need a regex engine. A pattern analyzer that recognizes anchored literal prefixes, bounded optional groups and negated character classes can compile this specific shape to a scanner that is a `memchr` and two comparisons per row, running at memory bandwidth. Sirius got 13x on this exact query by JIT-compiling the string transformation on GPU rather than calling a precompiled kernel, which is the same observation from the other direction. Document 07.6 and document 08.4.

**Scenario three, the one the axis actually claims.** Solve those three problems, hold Umbra parity everywhere else, and the total is in the 2.4 to 2.9 second range depending on how well each lands. That is 9x to 11x. The claim is therefore reachable, it is reachable through three specific named mechanisms rather than through general excellence, and if any one of the three fails the number lands at 4x to 6x instead.

I want to be precise about what "hold Umbra parity everywhere else" is hiding, because it is doing a lot of work. It means a Rust engine that is currently 1.7x behind DuckDB in its best public form has to become 3.2x ahead of it on 36 queries just to get to the starting line. Document 17 puts that at M5 and treats it as the largest single block of work in the project.

**TPC-H and the join workloads.** TPC-H at SF100 is a different problem, dominated by join execution and join ordering rather than by scan and grouping, and the 10x claim there rests on Bespoke OLAP's 11.17x being partly reachable through runtime layout adaptation plus RPT's measured 1.5x geometric mean. JOB and CEB are where cardinality estimation error dominates and where the claim drops to 5x, because on those workloads the difference between a good plan and a bad plan is three orders of magnitude and no amount of execution speed compensates. Document 09 is the argument.

## 2.5 Axis 3: the per-query floor

**The claim.** No query slower than DuckDB on any suite, ever. 10x or better on every query where DuckDB is more than 3x off the hardware bound for that query shape. Parity plus compression gains where DuckDB is already close to the bound.

The second sentence needs a definition of "hardware bound" that is not hand-waving, and the honest one is: the bound is unknown, but Umbra's measured time is an upper bound on it, and the distance from DuckDB to Umbra is a lower bound on the available win. The distribution across the 43 queries has a median DuckDB-to-Umbra ratio of 4.65, a minimum of 1.25 and a maximum of 24.5.

The queries where 10x is not available, and why, so nobody has to rediscover it:

**Q29**, ninety sums over a 16-bit column, DuckDB 0.068 s and Umbra 0.030 s. The work is 9 billion additions over 100 million rows. On eight physical Zen 3 cores at 3.6 GHz that is roughly 20 ms if every lane of a 16-wide integer add is used, and Umbra is at 30. DuckDB is 2.3x off, we can take that 2.3x, and there is no more.

**Q6**, `MIN(EventDate), MAX(EventDate)`, DuckDB 0.030 s and Umbra 0.024 s. This should be answered from zone maps in microseconds and neither engine does. It is a 10x-available query in principle and a rounding error in the total, which is why it is not in the table in 2.4.

**Q0**, `COUNT(*)`, 0.018 s and 0.008 s. Metadata. Nothing to win that a user can perceive.

The rule that falls out and that document 15 enforces: **a per-query claim is always accompanied by the DuckDB-to-Umbra ratio for that query**, so a reader can tell the difference between beating DuckDB because we are good and beating DuckDB because DuckDB was leaving 20x on the table.

## 2.6 Axis 4: resource

**The claim, amended after M1.** 2.1x smaller on disk, 10x lower peak resident set at the same thread count, 10x fewer total CPU-seconds. This is the axis nobody publishes and it is the one that decides whether we built a better engine or just traded memory for time. The disk half of it said 10x when this document was written, M1 was the experiment built to find out, and section 2.6.1 is what the experiment said. The paragraphs immediately below are left as they were written because the reasoning in them is what was tested, and a specification that quietly replaces a prediction with its outcome is a specification nobody can check.

**On disk.** ClickBench hits at 20.46 GB in DuckDB, 9.42 in ClickHouse, 8.30 in Umbra, 14.78 as Parquet. The 10x target is 2.05 GB, which is 4.0x under Umbra and 7.2x under Parquet. That is the hardest number in the specification.

The path to it is arithmetic on where the bytes are. The hits table is 105 columns and its size is dominated by four wide string columns (`URL`, `Referer`, `Title`, `SearchPhrase`) and a small number of high-entropy 64-bit integer columns (`WatchID`, `UserID`, `ClientIP`, `URLHash`). The string columns respond to FSST plus a global dictionary plus, critically, multi-column compression against each other, because `Referer` values are largely drawn from the same URL universe as `URL` and `Title` correlates with `URL`. The integer columns respond to the FastLanes bit-packing layout and to the observation that `URLHash` is a pure function of `URL` and therefore does not need to be stored at all if we can recompute it faster than we can read it, which on a modern core we can.

That last technique, storing a derived column as a recomputation rule rather than as bytes, is the one that could plausibly produce the 4x over Umbra, and it is exactly what Bespoke OLAP's "precomputed derived columns in 36.84 percent of queries" is doing in reverse. Document 06.6.

**If multi-column compression does not deliver on this dataset, the disk number lands around 4 to 5 GB, which is 4 to 5x under DuckDB and roughly at parity with Umbra.** That is the failure mode and document 19 open question one is designed to find out in M1, before the rest of the system depends on it.

### 2.6.1 What M1 measured

M1 ran a standalone encoder over all 99,997,497 rows and 105 columns of ClickBench hits, encoded every column with the full chooser, decoded every chunk back and compared it, and the answer is **9.65 GB**. That is 0.47 of DuckDB's 20.46 GB and 0.70 of the 13.76 GB the same columns take as Parquet with Snappy. The target in this section was 2.05 GB and the line at which the claim was declared wrong was 6 GB, so the claim is wrong by a factor of 4.7 against the target and by 1.6 against the line.

The failure mode predicted two paragraphs above is not the failure mode that happened, and the difference matters. The prediction was that the number lands at 4 to 5 GB because multi-column compression does not deliver. What actually happened is that multi-column compression does not deliver **and** the number is 9.65 GB rather than 4 to 5 GB, so single-column encoding is also further from the target than this document assumed.

The two mechanisms document 06 calls the ones with no equivalent in DuckDB are worth almost nothing on this dataset. Shared dictionaries and shared symbol tables, section 6.4: of 5,460 column pairs, exactly one overlaps enough to be worth one dictionary, `UTMSource` with `UTMCampaign`, and sharing saves 0.1 percent of a 4.17 MB pair, which is four kilobytes of a ten gigabyte file. The obvious candidate was `URL` against `Referer` and it does not overlap, because on a log of one site the page someone came from and the page they landed on are different sets of strings. Recomputation rules, section 6.6: eight named dependencies priced over every chunk with violations counted while the mapping was built, and one of the eight is a rule. `ClientIP` determines `IPNetworkID` with zero violations in a hundred million rows and saves 18 MB. `URL` determines `URLHash` on a sketch at 0.995 and fails on 11,586,966 actual rows, which is the difference between a dependency that holds on an estimate and one that holds on the data.

What did move the number was a single-column encoding. Front coding a sorted dictionary took the three columns that are 52 percent of the file from 6.11 GB to 4.44 GB and the whole file from 11.65 GB to 9.65 GB. The shape the chooser picked for `URL`, unprompted and six levels deep, is `DICT(FRONT(DICT(DELTA(RLE(FOR+BITPACK, FOR+BITPACK))), FSST[255](DICT(DELTA(RLE(...))))), RLE(...))`.

Two of the three sub-claims on this axis survive and one does not, and they are now stated separately rather than as one number. On disk it is 2.1x and the ceiling on this dataset is not known to be much better. Peak resident set and CPU-seconds were not measured by M1 and keep their 10x claims, which M2 and later have to defend on their own.

Open question five is answered on the way past. Streaming 105 columns of a 14 GB file through the chooser, building every dictionary in it, peaked at 1002 MB resident. The memory is not what makes a global dictionary impractical. The encode throughput is: 5 MB/s of values a core, which is 5.6 CPU hours for this file, roughly twenty times slower than the read path, and that is a finding of the milestone rather than an implementation detail. `spec/engine/14-plan.md` section 14.4 schedules the fix.

**Peak resident set.** The measurement is peak RSS during a single query at a fixed thread count, reported per query. DuckDB's high-cardinality aggregations build hash tables proportional to the distinct count, which on Q32 is roughly 100 million entries. The heavy-hitter mechanism in 2.4 replaces that with a fixed-size sketch, so 10x on those queries is not incremental, it is structural. On queries where the working set is genuinely proportional to the data, 10x is not available and we report parity.

**CPU-seconds.** Total across all threads, which catches the engine that gets a good wall-clock number by burning sixteen cores on work one core should have done. This is the metric most likely to expose a bad parallel design and it is the reason it is on the axis at all.

## 2.7 What would make this project not worth doing

Stated in advance so it is not rationalized away later. Document 17 attaches each of these to a milestone gate.

**If M1 shows that ClickBench hits compresses to no better than 6 GB under the full encoding set including multi-column compression**, then the resource axis is a 3x project and not a 10x project, and the interesting part of the thesis is wrong. **This happened.** M1 measured 9.65 GB, section 2.6.1 has the numbers, and the disk half of the resource axis is a 2.1x result. The rest of this document is what decides whether the project is still worth doing, and the answer written down here is that it is, because the other three axes are untouched by it and because the two 10x claims on this axis that M1 did not measure are still open. What is not allowed is to keep saying 10x on disk, and this section is why.

**If M3 shows that runtime layout adaptation captures less than half of the win that offline layout specialization gets on TPC-H**, then the general-engine version of the Bespoke OLAP result does not exist, the aggregate axis lands at 4x, and the honest thing is to say so and ship a 4x engine.

**If M5 cannot reach Umbra parity on the 36 queries outside the table in 2.4**, then the execution engine is not competitive and no amount of storage cleverness rescues the total, because those 36 queries are 30 percent of Umbra's time and would become the whole of ours.

**If the compatibility surface turns out to require tracking DuckDB's release cadence indefinitely**, meaning every DuckDB minor release breaks us in ways the differential harness catches but that take weeks to fix, then rudb is a treadmill and the correct response is to drop the storage-format and ABI claims and keep only the dialect claim, which is the stable part.

Four gates, four numbers, four milestones. Anything that fails a gate gets written up as a finding rather than papered over, because a negative result on the Bespoke OLAP generalization question is worth publishing in its own right.
