# C. Join processing and join-heavy workloads (JOB-centric), state of 2026

Research notes for the rudb compiled-execution-engine spec. Target: roughly 10x DuckDB on JOB (113 queries, IMDB),
with CEB, TPC-H and TPC-DS as secondary targets. Compiled 2026-09-24.

Conventions:
- Every number carries its source URL. "[snippet]" means the fact was seen only in a search-result snippet and the
  primary PDF was not read.
- "[fig]" means the number was read off a rendered figure, not from text.
- "[unit?]" flags a number whose units look inconsistent in the source.
- Nothing here is extrapolated. Where no published number exists, these notes say "no published number found".

---------------------------------------------------------------------------------------------------------------------

## 0. TL;DR

- JOB is a join-ordering and intermediate-result benchmark. It is not a scan-bandwidth benchmark. All 113 queries
  are alpha-acyclic, select-project-join (SPJ) blocks wrapped in MIN(...), with 3-16 joins (average 8). Runtime is
  dominated by a handful of queries whose chosen join order builds big intermediates over cast_info, movie_info and
  movie_keyword, and by string predicates (LIKE, IN-lists, disjunctions) evaluated on the large fact-like tables.
- The biggest 2024-2026 lever is semijoin/Bloom-filter reduction applied before joining: Predicate Transfer (PT),
  Robust PT (RPT), RPT+, Parachute, SQL Server bitmaps, Yannakakis+ and SYA. These give 1.2-1.6x geomean on JOB over
  DuckDB, and they collapse the spread between best and worst join orders from roughly 100x down to about 1.2-1.6x.
- The biggest hash-table lever is the Umbra/CedarDB "unchained" table: a directory with 16-bit Bloom tags in the
  pointer bits plus a dense adjacency array. It is about 2x faster than open addressing on probe-heavy workloads.
- Worst-case-optimal joins (WCOJ) and Free Join help cyclic and graph queries. They are neutral to harmful on JOB:
  in Umbra, pure WCOJ is about 25x slower on JOB.
- String predicates matter a great deal on JOB. Compiled LIKE (segment split plus SIMD, Boyer-Moore or Two-Way) and
  the German-string 12-byte prefix fast path are the known state of the art. 2026 work adds FSST-domain LIKE
  (2.5-17x) and Aho-Corasick wildcard joins.
- Absolute published JOB totals are scarce. DuckDB 1.3.2 single-threaded on a Xeon E-2236 takes 55.3 s for all 113
  queries (AQP paper) [fig]. DuckDB multi-threaded is about 30 s (arXiv 2311.17293) [snippet]. No public
  whole-benchmark JOB total was found for Umbra, CedarDB or Hyper.

---------------------------------------------------------------------------------------------------------------------

## 1. The Join Order Benchmark itself

### 1.1 Origin papers
- Leis, Gubichev, Mirchev, Boncz, Kemper, Neumann. "How Good Are Query Optimizers, Really?" PVLDB 9(3):204-215,
  2015. https://www.vldb.org/pvldb/vol9/p204-leis.pdf
- Leis, Radke, Gubichev, Mirchev, Boncz, Kemper, Neumann. "Query optimization through the looking glass, and what
  we found running the Join Order Benchmark." VLDBJ 27(5):643-668, 2018.
  https://link.springer.com/article/10.1007/s00778-017-0480-7
- Query text: https://github.com/gregrahn/join-order-benchmark. CedarDB also ships a JOB dataset page:
  https://cedardb.com/docs/example_datasets/job/

### 1.2 Data
- IMDB snapshot from May 2013. It is 3.6 GB as CSV and has 21 tables (Leis 2015/2018).
- The schema is highly correlated and skewed, with non-uniform distributions. The authors state that this is
  intentional and makes cardinality estimation much harder than on synthetic data.
- The largest relation is cast_info, with about 36M tuples (as cited in the TreeTracker paper, arXiv 2403.01631).
  Other large relations are movie_info, movie_keyword, movie_companies, name, title and char_name; their exact
  counts were not transcribed here.
- Load cost for reference: IMDb loads into DuckDB in 91.67 s (Parachute paper, arXiv 2506.13670).

### 1.3 Queries
- 33 query structures, each with 2-6 variants, give 113 queries (Leis 2015).
- Joins per query: 3-16, average 8 (Leis 2015). The CedarDB docs say 4-17 tables per query
  (https://cedardb.com/docs/example_datasets/job/) [snippet].
- CEB's Table 1 describes JOB as 113 queries, 70K sub-plans, 31 templates, 5-16 joins and 88 distinct optimal plans
  (Negi et al., arXiv 2101.04964) [snippet].
- Each query is a single SPJ block. Projections are wrapped in MIN(...) so that output transfer does not dominate
  (Leis 2018, footnote 4). The consequence is that output is one row, so the whole cost is the join pipeline plus
  scans and filters.
- All queries are alpha-acyclic (Birler et al. PVLDB 2024, https://db.in.tum.de/people/sites/birler/papers/diamond.pdf).
  Across JOB, TPC-H and TPC-DS, 94% of queries are acyclic (Zhao et al. SIGMOD 2025, arXiv 2502.15181).
- Predicates include LIKE with leading and inner wildcards, IN-lists, disjunctions, IS NULL / IS NOT NULL, and
  range predicates on production_year. Examples from the VLDBJ appendix:
  - 9b, 15a, 15b, 19b, 22a: `mc.note LIKE '%(200%)%'`
  - 15a: `mi.info LIKE 'USA:% 200%'`
  - 15c: `(mi.info LIKE 'USA:% 199%' OR mi.info LIKE 'USA:% 200%')`
  - 19a/19c: `(mi.info LIKE 'Japan:%200%' OR mi.info LIKE 'USA:%200%')`
  - 19b: `mi.info LIKE 'Japan:%2007%' OR ... 'USA:%2008%'`
  (lookingglass appendix, https://link.springer.com/article/10.1007/s00778-017-0480-7)
- Many predicates therefore sit on movie_info.info and movie_companies.note. These are large text columns on large
  tables, and they are evaluated before or while joining.

### 1.4 What the original study found (Leis 2015/2018)
- For PostgreSQL, the share of estimates off by 10x or more is 16% for 1-join subexpressions, 32% for 2-join and
  52% for 3-join. Error grows exponentially with join count.
- DBMS A and HyPer estimate complex base-table predicates such as LIKE substring search well because they use
  sampling. HyPer uses a random sample for base-table selectivity (lookingglass section 3).
- PostgreSQL disasters came from risky nested-loop joins picked on underestimates. Rerunning without non-index
  nested-loop joins removed all timeouts even with PostgreSQL's own estimates. A second failure was hash-table
  build-size underestimation, fixed by runtime resizing in PostgreSQL 9.5.
- In main memory with indexes cached, hash join beats index-nested-loop join by at most 5x in PostgreSQL and 2x in
  HyPer. The main-memory setting is "much more forgiving".
- Cost model: a simple C_mm model (tuples produced) is enough. Tuning the PostgreSQL model for main memory gives
  41% faster runtimes; even simple C_mm is 34% faster than the default (lookingglass section 5).
- The implication for rudb is that JOB mostly punishes (a) large intermediates from underestimated joins, and
  (b) plans with no robustness when estimates are wrong. Engine speed multiplies whatever the plan gives you.

### 1.5 Is JOB "really" about join ordering? (counterpoints)
- Simpli-Squared (arXiv, simpli.txt): with simple PK/FK-aware heuristics, PostgreSQL JOB drops to 350 s with tuning
  and 178 s with PK+FK indexes; one outlier query took 7690 s. The authors question whether accurate cardinality
  estimation is fundamental for JOB. JOB-light is described there as 70 queries, at most 6 tables, mostly star
  joins, with no LIKE and no disjunctions.
- RPT (SIGMOD 2025) shows that with full semijoin reduction, the join order barely matters (section 3.2).

### 1.6 Related benchmarks
- **JOB-light:** 70 queries, up to 6 relations (mostly star), 696 sub-queries, numeric and categorical filters only
  (Han et al. PVLDB 15(4) 2022, https://www.vldb.org/pvldb/vol15/p752-zhu.pdf; simpli.txt).
- **CEB (Cardinality Estimation Benchmark):** Negi et al., "Flow-Loss", PVLDB 14(11) 2021,
  https://arxiv.org/pdf/2101.04964. IMDb part: 13,644 queries, 3.5M sub-plans, 15 templates (the repo says 16),
  5-15 joins, about 2,200 distinct optimal plans [snippet]. Parachute counts 13,646 queries. Repo:
  https://github.com/learnedsystems/CEB. It contains RANGE, IN and LIKE predicates.
- **STATS / STATS-CEB:** Han et al. PVLDB 15(4) 2022. 8 tables, 34 columns, over 1M rows (Stack Exchange); 70 join
  templates and 146 queries (2,603 sub-queries); acyclic star/chain only, up to 7 relations; true cardinalities from
  200 to 2e10; the full workload takes about 10 h on PostgreSQL [snippet].
  https://github.com/Nathaniel-Han/End-to-End-CardEst-Benchmark
- **DSB:** Ding, Chaudhuri, Gehrke, Narasayya. PVLDB 14(13):3376-3388, 2021.
  https://www.vldb.org/pvldb/vol14/p3376-ding.pdf. TPC-DS-derived with skew and correlations, many-to-many joins,
  inequality joins and cyclic joins. It produces 6.1x more distinct plans per template on SQL Server 2019 than
  TPC-DS [snippet].
- **SQLStorm:** Schmidt, Leis, Boncz, Neumann (PVLDB 2025). LLM-generated, 18K+ queries on StackOverflow data at
  1/12/220 GB. Umbra is 4.11x faster than DuckDB (median) on SQLStorm-1 and 7.93x on SQLStorm-220, and up to 1,000x
  on some queries. PostgreSQL is typically 84x slower than Umbra but beats it on 108 of 17,531 queries (sqlstorm.txt,
  section 5).
- **JOB-Complex** (arXiv 2507.07471) is a harder JOB variant for traditional and learned optimizers [snippet only;
  not read].

---------------------------------------------------------------------------------------------------------------------

## 2. Published JOB runtimes (absolute numbers)

JOB totals are rarely published. Most papers report relative speedups. Everything found is listed below.

| System / config | JOB total | Hardware | Source |
|---|---|---|---|
| DuckDB 1.3.2, 1 thread | 55.3 s (113 q) [fig] | Xeon E-2236, HDD | AQP arXiv 2511.16455, Fig 7 |
| DuckDB 1.3.2 + AQP, 1 thread | 50.6 s [fig] | same | same |
| DuckDB 0.10.1, 1 thread | 123.9 s [fig] | same | same |
| DuckDB 0.10.1 + AQP | 115.1 s [fig] | same | same |
| DuckDB 1.3.2, common 63 queries | 31.6 s [fig] | same | same |
| PostgreSQL 12.3, same 63 queries | 173.0 s; AQP 86.0 s (2.01x) [fig] | same | same |
| DuckDB (version per paper), multithreaded | 30.38 s; 37.89 s w/o CE | n/a | arXiv 2311.17293 [snippet] |
| MonetDB | 114.46 s; 88.79 s w/o CE | n/a | arXiv 2311.17293 [snippet] |
| HEAVY.AI | 37/113 timeouts (120 s), 4744 s counted | n/a | arXiv 2311.17293 [snippet] |
| PostgreSQL tuned (Simpli2) | 350 s; 178 s w/ PK+FK idx | n/a | simpli.txt |
| DuckDB (SYA paper) | avg 0.828 s/query | see paper | arXiv 2411.04042 |
| DataFusion (SYA paper baseline) | avg 0.518 s/query | see paper | arXiv 2411.04042 |
| ADOPT (WCOJ + RL) | 45 s total (table layout garbled; other systems' JOB cells uncertain) | JVM | arXiv 2307.16540 Table 1 |

Per-query anchor points:
- JOB 8d on DuckDB 0.10.1 takes 2.7 s, dropping to 0.8 s with true cardinalities injected (AQP paper) [fig].
- JOB 13a: DuckDB over 10 s vs Free Join about 1 s (Free Join, SIGMOD 2023, https://arxiv.org/pdf/2301.10841).
- JOB 2a: the worst join order produces 179x more intermediate tuples than the best; with RPT the ratio drops to
  1.2x (arXiv 2502.15181).
- JOB 6a: semijoin reduction shrinks cast_info from 36M to 486 tuples (TreeTracker, arXiv 2403.01631).

Umbra, CedarDB and Hyper:
- No whole-benchmark JOB total was found in any paper, blog or doc. TUM papers report JOB only relative to
  baseline Umbra.
  - Diamond: the ht optimisation gives a "noticeable" JOB gain; WCOJ is about 25x slower.
  - WCOJ hybrid (Freitag 2020): no slowdown on JOB.
- Indirect Umbra-vs-DuckDB ratios on related workloads:
  - TPC-H SF100 on 2x EPYC 7713: Umbra is 2x faster than Hyper and 6x faster than DuckDB v0.10.1; DuckDB could not
    finish SF1000 within 24 h (Birler et al. DaMoN 2024, https://db.in.tum.de/~birler/papers/hashtable.pdf).
  - SQLStorm: Umbra is 4.11x to 7.93x faster than DuckDB (median).
  - CEB: Bespoke-OLAP (arXiv 2603.02001) reports 0.4 s vs DuckDB 19.5 s, and 9.56x faster than Umbra
    single-threaded, which implies Umbra about 3.8 s [snippet; derived].
  - CE graph benchmark: diamond-hardened Umbra is about 100x faster than DuckDB 0.9.2.
- Takeaway: the 10x-over-DuckDB target on JOB is roughly where Umbra-class systems sit on analytic workloads
  (6x TPC-H SF100 vs DuckDB 0.10; 4-8x median on SQLStorm). The target is plausible but needs plan robustness plus
  engine speed. The engine alone probably does not reach it.
- DataFusion has JOB/imdb in `bench.sh` but publishes no numbers. It uses a syntactic, non-cost-based join order,
  so it is fragile on JOB.
- A GitHub project (StavrosGous) claims 59x on JOB [snippet; unverified].

---------------------------------------------------------------------------------------------------------------------

## 3. Robust join processing: semijoin reduction, Bloom transfer, Yannakakis variants

### 3.1 Predicate Transfer (PT), CIDR 2024
- Yang, Zhao, Yu, Koutris. "Predicate Transfer: Efficient Pre-Filtering on Multi-Join Queries". CIDR 2024.
  https://www.cidrdb.org/cidr2024/papers/p22-yang.pdf [URL not re-verified]
- Idea: generalise Yannakakis semijoin passes with Bloom filters. A forward pass and a backward pass over a join
  graph pre-filter every base table before any join runs.
- TPC-H SF1 results: 3.3x over BloomJoin, 4.1x over no-PT, 4.2x over exact Yannakakis; up to 61x on some queries
  (pt.txt).

### 3.2 Robust Predicate Transfer (RPT), SIGMOD 2025
- Zhao, Yang, Koutris, Yu et al. "Debunking the Myth of Join Ordering: Toward Robust SQL Analytics". SIGMOD 2025.
  https://arxiv.org/abs/2502.15181. Code: https://github.com/zzjjyyy/PredTransDuckDB
- Built on DuckDB 0.9.2. Hardware: 2x Xeon 8474C, 512 GB.
- Algorithm: LargestRoot builds a transfer spanning tree rooted at the largest relation (maximum spanning tree by
  size), then runs a forward and a backward Bloom pass. SafeSubjoin restricts join orders after the transfer.
  - For alpha-acyclic queries, any join order produced by SafeSubjoin is guaranteed to never produce dangling
    intermediates beyond BF false positives.
- Robustness factor (RF, worst/best over random orders), avg/max:
  - Left-deep: JOB 30.4/371 -> 1.2/1.6; TPC-H 2.7/9.3 -> 1.3/1.5; TPC-DS 7.2/224 -> 1.1/1.5.
  - Bushy: JOB 120/1747 -> 1.6/7.7 (max at 17e); TPC-H 5.1/13.7 -> 1.8/3.0; TPC-DS 35.0/1226 -> 1.8/4.2.
- Speed vs DuckDB with its own optimizer: TPC-H 1.53x, JOB 1.46x, TPC-DS 1.56x, DSB 1.54x. On-disk: 1.3x / 1.5x.
- BF operations take 28% / 12% / 46% of time on TPC-H / JOB / TPC-DS.
  - BF: Arrow-style blocked BF, 2% FPR, AVX2. BF probes are 2-7x faster than hash-table probes.
- Bushy plans gain only 6% (TPC-H) and 11% (JOB) over left-deep once RPT is on.
- Cyclic queries in the suites: TPC-H Q5; TPC-DS 19, 24, 46, 64, 68, 72, 85. These need special handling because
  the spanning tree does not cover all edges.
- The largest JOB query (29) has 17 joins.

### 3.3 RPT+ (PVLDB 19(6), 2026)
- Qiao, Boncz, Zhang. "RPT+: ..." PVLDB 19(6):1278-1290, 2026. doi 10.14778/3797919.3797934.
  https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt+-vldb26.pdf
  Code: https://github.com/embryo-labs/dynamic-predicate-transfer. DuckDB extension:
  https://github.com/YimingQiao/bloom
- Implemented in DuckDB v1.3.0. Hardware: 2x Xeon Platinum 8474C at 2.1 GHz (48 cores), 512 GB DDR5-4800,
  8 threads, GCC 12.2 -O3.
- Geomean speedup over DuckDB: JOB 1.47x, SQLStorm 1.28x, TPC-H SF100 1.17x, Appian 1.01x.
- Robustness:
  - On SQLStorm, plain RPT regresses (below 0.9x) on at least 28% of queries; RPT+ does so on 2.1%.
  - RPT+ reaches up to 500x.
  - SQLStorm run: 18,251 queries, 13,308 finish.
- Mechanisms:
  1. **Asymmetric transfer plans.** Transfer only in directions that pay off, instead of the full symmetric
     forward/backward plan.
  2. **Cascade filter:** min-max check, then BF, then exact hash probe.
     - Min-max goes before the BF because it is nearly free.
     - DuckDB v1.3.0 by default already transfers min-max from build to probe side.
  3. **Cache-sectorized Bloom filter (CSBF):**
     - 64-byte blocks with 32-bit sectors, 20 bits/key, 7 hashes.
     - 2.48 cycles/tuple, FPR 6.1e-5, uses 32-bit gathers (VPGATHERDD).
  4. **Accuracy matters beyond FP work.** False positives inflate downstream min-max ranges; expected range is about
     n - 2/p. A more accurate BF gave 4x on JOB 07c.
  5. **Dynamic pipelines.** Sample gamma = 100K tuples. Keep a filter only if selectivity is below
     tau_sel = 0.35 and progress is below tau_prog = 0.6. Memory budget M_avail = 64 GB. Stop threshold
     tau_stop = 0.9.
- Overhead cases (Fig 1): JOB 10a, JOB 24a, SQLStorm 19785. Regressions on JOB templates 8, 10 and 24 appear when
  the transfer breaks row-group (zone-map) skipping.
- Lesson for a compiled engine: filters must be adaptive (sampled, then kept or dropped), cheap per tuple (about
  2-3 cycles), and ordered cheapest-first. Transfer must not destroy scan-side zone-map skipping.

### 3.4 Parachute (PVLDB 18, 2025)
- Stoian et al. arXiv 2506.13670. Precomputes "parachute" columns: compact per-tuple join-key signatures
  (8/16-bit) stored with the fact table so the probe side can be filtered at scan time without a build.
- Setup: DuckDB v1.2, single thread, Xeon Gold 5318Y.
- JOB:
  - 1.54x over DuckDB and 1.24x over DuckDB+PSF (probe-side filter) with 14.35% extra space.
  - 1.47x / 1.18x with 7.94% extra space.
- CEB (13,646 queries): 1.56x / 1.33x with 9.82% space.
- Dangling tuples left: Parachute 2.79%, PSF 6.63%, RPT 0.29%.
- On DuckDB v0.9: RPT 1.48x, Parachute 1.81x (16-bit).
- Cost: load time rises 3.9x (IMDb load 91.67 s baseline).
- Their PSF baseline:
  - 8 KiB L1-resident BF, k = 2, about 5000 keys, about 2% FPR.
  - Discarded if more than 34% of bits are set.
  - Disabled if it filters under 60% after 4000 rows.
  - Gives 1.26x over DuckDB on JOB.
- DuckDB 1.2's own SIP: min/max transfer, plus an IN-list when the build side has 50 or fewer distinct keys.

### 3.5 SQL Server "bitmap filters as Yannakakis" (CIDR 2026)
- Zhao et al. CIDR 2026. https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf
- Runtime chooses between a bitmap and a bit-vector from hash-build statistics. The bitmap carries min/max for
  columnstore rowgroup elimination.
- The pull-based cascade of bitmaps equals Yannakakis's bottom-up pass. There is no top-down pass.
- Cascades optimizer costing carries a "bitmap context". Bitmap selectivity is discretised into buckets:
  75%, 10%, 1%, 0.1%.
- Setup: TPC-H SF100, Azure 8-core Xeon 8370C, 64 GB, SQL Server 2025.
- Results:
  - Up to 3.47x; per-query values include 3.47, 1.91, 1.92, 2.33, 3.00, 2.86.
  - 7 queries exceed 2x. 12 acyclic queries get instance-optimal plans.
  - The Q12 bitmap rejects 80%.
- Lesson: production evidence that optimizer-aware bitmap placement (not just runtime SIP) is shippable.

### 3.6 Lookahead Information Passing (LIP), VLDB 2017
- Zhu, Potti, Saurabh, Patel. "Looking Ahead Makes Query Plans Robust". PVLDB 10(8), 2017.
  https://www.vldb.org/pvldb/vol10/p889-zhu.pdf [URL not re-verified]. Implemented in Quickstep on SSB SF100.
- For star joins: push all dimension BFs to the fact scan and reorder filters adaptively by observed selectivity.
- Results:
  - One query's plan-to-plan spread went from 2.1-58 s to 1.3-7.4 s.
  - Geomean 4.0x; 5-10x on Q4.3.

### 3.7 Yannakakis+ (SIGMOD 2025)
- arXiv 2504.03279 (Wang, Hu, Dai, Yi et al., HKUST).
- A query-rewriting Yannakakis with an optimised semijoin plan that emits SQL, so it runs on any engine.
- Overall: 160 of 162 queries faster, average 2.41x.
- JOB on DuckDB 1.0 with 72 threads: max 14.84x, mean 1.42x. On PostgreSQL 16.2: 12.31x / 1.40x.
- Table 2 lists "DuckDB native Max 933.73, Mean 53.02" labelled seconds [unit?]. This is likely ms and should not
  be used as an absolute JOB number.

### 3.8 Shredded Yannakakis (SYA) (PVLDB 18(8), 2025)
- Bekkers, Neven, Vansummeren, Wang. arXiv 2411.04042. Implemented in DataFusion.
- Uses nested/factorised semijoin representations so dangling-free evaluation costs no more than binary joins in
  practice.
- Overall: 1,849 queries, 85.3% improve, up to 62.5x.
- JOB:
  - Max slowdown 1.3x (89 ms); max speedup 6.4x (2.2 s).
  - Matches or beats binary-join DataFusion on 94.6% of JOB queries.
- STATS-CEB: up to 24x (143 queries).
- Notes that classic full Yannakakis is about 5x slower on JOB than binary joins, which is why naive Yannakakis
  lost historically.
- Baselines: DuckDB avg 0.828 s vs DataFusion avg 0.518 s per JOB query in their setting.

### 3.9 TreeTracker Join (TTJ), TODS 2025
- arXiv 2403.01631. Java, single core, Ryzen 5900X.
- A hash-join variant that detects dangling tuples during probing and removes them in place (backjumping), with no
  separate semijoin pass.
- Vs hash join on JOB: avg 1.11x, max 12.6x (16b), min 0.9x (11b).
- Vs Yannakakis on JOB: avg 1.60x, max 9.2x, min 0.2x.
- JOB 6a: semijoin reduction shrinks cast_info from 36M to 486 tuples.

### 3.10 Diamond-hardened joins (PVLDB 17(11), 2024)
- Birler, Kemper, Neumann. "Robust Join Processing with Diamond Hardened Joins". PVLDB 17(11):3215-3228.
  https://db.in.tum.de/people/sites/birler/papers/diamond.pdf
- Splits a hash join into **Lookup** (find the key's match group) and **Expand** (enumerate matches). Expand can be
  deferred past later lookups, so intermediates are not blown up by n:m fan-outs ("diamond problem"). Expand3 is a
  ternary operator for diamond patterns.
- Code sketch from the paper: `iterator = ht.find(k)  # Lookup ... do: # Expand`.
- Setup: Umbra on Ryzen 9 5950X, 16 cores, 64 GB.
- JOB (all alpha-acyclic):
  - Only the "ht" change (hash table with dense collision lists that return the match group contiguously) gives
    noticeable gains. The other techniques are neutral.
  - Pure WCOJ is about 25x slower on JOB and 12x slower on TPC-H SF10. The ht change gives about 5% on TPC-H.
- CE graph benchmark:
  - About 100x faster than DuckDB 0.9.2, 230x faster than WCOJ, 2.4x faster than baseline Umbra.
  - dblp_cyclic_q9_06 goes from 2 s to 4 ms.
- Microbenchmark: L&E is 730x faster than ht and 15x faster than WCOJ (acyclic diamond).
- Lesson: make the hash table return a contiguous match range (not a chain walk) so Expand can be lazy. This is
  the structural prerequisite for factorised or late expansion.

### 3.11 Worst-case-optimal joins in a relational engine (Umbra, PVLDB 13, 2020)
- Freitag, Bandle, Schmidt, Kemper, Neumann. "Adopting Worst-Case Optimal Joins in Relational Database Systems".
  PVLDB 13(11):1891-1904. https://doi.org/10.14778/3407790.3407797
- Hash tries with lazy child expansion. A hybrid optimiser only uses WCOJ where binary joins would grow
  intermediates.
- No slowdown on TPC-H or JOB. Only 5 false negatives out of 923 joins (queries 8c, 16b).
- Hardware: 28 cores, 2x Xeon E5-2680 v4.
- ADOPT (arXiv 2307.16540) adds adaptive RL attribute orders for WCOJ. JOB total 45 s in their JVM setup (Table 1;
  layout of competitor cells garbled in extraction).

### 3.12 Free Join (SIGMOD 2023)
- Wang, Willsey, Suciu. arXiv 2301.10841. Rust; COLT (column-oriented lazy trie). Unifies binary join plans and
  Generic Join.
- JOB (excluding 5 empty-result queries, single-thread M1 MacBook Air):
  - Geomean 2.94x over DuckDB binary join, up to 19.36x, min 0.85x.
  - 9.61x over Generic Join.
- 13a: DuckDB over 10 s vs Free Join about 1 s.
- Note: the 2.94x is against DuckDB circa 2023 (about v0.6-0.7); DuckDB's JOB time roughly halved from 0.10 to 1.3
  per the AQP data.

### 3.13 SplitJoin (arXiv 2510.25684, Oct 2025)
- He, Zhao, Frisk, Yang, Kristensen, Koutris, Yu. Heavy/light key split as a first-class operator; per-split join
  orders; front-end on DuckDB and Umbra.
- DuckDB, social-network cyclic queries:
  - Completes 43 queries vs 29 natively.
  - 2.1x faster on average, up to 13.6x. Intermediates 7.9x smaller on average, up to 74x.
- Umbra: 45 vs 35 queries completed, 1.3x avg (up to 6.1x), 1.2x smaller intermediates.
- The DuckDB library page states 66 vs 48, 1.8x, 4.5x (https://duckdb.org/library/splitjoin/) [snippet]; this
  differs from arXiv v1, perhaps a later version.
- Relevance to JOB: low (acyclic, FK-mostly). High for graph-like or cyclic user queries.

### 3.14 Factorised execution
- FFX (arXiv 2609.09002, Sep 2026): "packed factorized vectors" keep full vectorisation.
  - 102-105x over DuckDB and Kuzu on many-to-many analytic queries (single-threaded).
  - Up to 108.4x fewer operator function calls.
  - Packed vs unpacked: 4.94x average.
- Relevance: JOB's n:m joins (cast_info, movie_keyword, movie_info fan-out per title) plus MIN aggregates are
  exactly where factorisation plus aggregate pushdown avoids expansion. MIN is idempotent, so duplicates from
  fan-out never need to be materialised.

### 3.15 DuckDB's own state (2025-2026)
- v1.2 / v1.3: build-to-probe min-max transfer, plus IN-list if 50 or fewer distinct keys (Parachute,
  RPT+ papers).
- v1.5.x (2026) ships Bloom filters in selective hash joins (duckdb/duckdb#19502), pushed into the probe-side scan
  as an optional filter [snippet].
  - Fixes: "Defer Bloom Filter Pushdown until it's done" #22218 (v1.5.3); "Disable bloom filter pushdown through
    casts" #21792 [snippet].
  - A correctness bug with spilled builds is reported in unreleased code (issue #25702) [snippet].
  - https://github.com/duckdb/duckdb/releases
- A 2025 community discussion proposed linear probing for DuckDB's join hash table and found that BF overhead
  could be a net loss on some workloads (discussion #18983) [snippet].
- Implication: the "10x DuckDB" baseline will move. RPT-style gains (about 1.5x on JOB) are being absorbed into
  DuckDB itself.

---------------------------------------------------------------------------------------------------------------------

## 4. Hash-table design for joins

### 4.1 Unchained hash table (Umbra/CedarDB), DaMoN 2024
- Birler, Schmidt, Fent, Neumann. "Simple, Efficient, and Robust Hash Tables for Join Processing". DaMoN 2024.
  https://db.in.tum.de/~birler/papers/hashtable.pdf. Blog: https://cedardb.com/blog/simple_efficient_hash_tables/
- Layout:
  - Directory of 64-bit slots. The lower 48 bits are a pointer into a dense tuple array; the upper 16 bits are a
    per-slot Bloom filter.
  - Build partitions tuples by hash, then lays them out contiguously so all tuples for a directory slot are
    adjacent (an "adjacency array"). A probe does one directory load, one Bloom check in the same word, then a
    linear scan of a contiguous range.
- Bloom tag: each key sets 4 of the 16 bits, taken from a 2,048-entry precomputed tag table (4 KB, L1-resident).
  FPR is about 1/169 at load factor 0.65 (1/168 measured).
- Hashing:
  - CRC32 plus multiply; two CRC32 calls for 64-bit keys.
  - The tag bits come from the hash bits not used for the slot index.
- Build:
  - Thread-local partitioning of the build side, then bump allocation per partition.
  - 1 GB huge pages instead of software write-combining. No atomics in the final build (partitions owned per
    thread).
- Probe: relies on out-of-order execution rather than explicit prefetch. Tight compiled loops issue enough
  independent loads.
- Results:
  - About 2x vs Robin Hood open addressing on join-heavy workloads; up to 20x on graph queries.
  - Validated on 10,312 queries (TPC-H SF1/10, TPC-DS SF1/10, JOB, LDBC, CE).
  - Tiny queries can be up to 30% slower (the fixed partitioning cost).
- Cross-system (TPC-H SF100, 2x EPYC 7713): Umbra is 2x faster than Hyper and 6x faster than DuckDB v0.10.1.
- Why it wins on JOB-like work:
  (1) Most probes miss. After selective dimension filters, the Bloom tag rejects them without touching tuple
      memory.
  (2) n:m matches are contiguous, which enables diamond-style lazy Expand (section 3.10).
  (3) There is no per-bucket chain pointer chasing.

### 4.2 Partitioned vs non-partitioned (Bandle, Giceva, Neumann, SIGMOD 2021)
- "To Partition, or Not to Partition, That is the Join Question in a Real System". SIGMOD 2021, pp. 168-180,
  doi 10.1145/3448016.3452831. https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf.
  Honorable mention.
- Integrated a radix-partitioned join into Umbra, with SW prefetching, SW write-combining and non-temporal stores
  on both sides. A Bloom-filter semijoin reducer was needed to make radix join competitive for selective queries.
- Conclusion: in a code-generating DBMS, radix join rarely justifies the extra code and optimiser complexity on
  real workloads. Use a non-partitioned hash join and add a Bloom reducer [snippet via Semantic Scholar summary].
- The 2024 unchained design is the practical synthesis: partition the build (cheap, cache-friendly) but not the
  probe.

### 4.3 Chaining vs open addressing (2025-2026 discussion)
- DuckDB's join HT uses chaining with salted pointers. A 2025 proposal explored linear probing
  (duckdb#18983) [snippet].
- Birler 2024: open addressing (Robin Hood) loses about 2x to unchained on joins. The dense-array layout plus
  in-pointer Bloom is the key, not chaining per se.
- For aggregation HTs (a different trade-off), see "Global Hash Tables Strike Back!" arXiv 2505.04153 [not read].

### 4.4 Prefetching and interleaving
- Psaropoulos, Legler, May, Ailamaki. "Interleaving with Coroutines". PVLDB 11(2), 2017.
  https://doi.org/10.14778/3149193.3149202
- Speedups for hash-probe-like index lookups (integer keys):
  - Group prefetching (GP): 2.7-3.7x.
  - Coroutines (CORO): 2.0-2.4x.
  - AMAC: 1.8-2.3x.
- Best interleave group: GP about 10; AMAC and CORO 5-6.
- Implications:
  - With a vectorised or morsel batch in a compiled engine, GP is simplest: hash a batch, prefetch the
    directory, then probe.
  - Birler 2024 argues OoO execution suffices when the Bloom tag lives in the directory word. Explicit prefetch
    mainly matters when the directory exceeds LLC and the probe loop has long dependency chains (e.g., multi-join
    pipelines).
  - A compiled tuple-at-a-time pipeline cannot group-prefetch without restructuring into batches ("relaxed
    operator fusion"-style stage buffers).

### 4.5 Bloom-filter engineering (for transfer and PSF)
- RPT+ CSBF: 64 B block, 32-bit sectors, 20 bits/key, k = 7, 2.48 cycles/tuple, FPR 6.1e-5, AVX2 gathers.
- RPT: Arrow blocked BF, 2% FPR, AVX2; BF probe is 2-7x cheaper than a hash probe.
- Parachute PSF: 8 KiB, k = 2, 2% FPR, with adaptive discard rules (over 34% bits set; under 60% filtered after
  4000 rows).
- Unchained: 16-bit in-pointer tag with 4 set bits, FPR about 0.6% at LF 0.65, and no separate memory access.
- Design rule from RPT+: when filters feed zone-map / min-max pruning downstream, accuracy (low FPR) is worth extra
  bits per key.

### 4.6 SIMD probe
- The RPT+ CSBF uses VPGATHERDD for 8 lanes of 32-bit words.
- Unchained avoids SIMD in the probe. The tag check is a scalar AND/compare on an already-loaded word.
- No 2025-2026 JOB-specific result shows SIMD hash probing beating the unchained scalar-plus-OoO design in a real
  system (none found).

---------------------------------------------------------------------------------------------------------------------

## 5. Query structure: late materialisation, groupjoin, unnesting, mark join, IN

### 5.1 Late materialisation / MIN-only outputs in JOB
- JOB outputs are MIN(col) over the full join. An engine can:
  (a) carry only join keys and row ids through joins, fetching payload columns once at the end (late
      materialisation);
  (b) push MIN below n:m fan-out, because MIN is duplicate-insensitive;
  (c) use factorised or diamond-style lazy expansion so fan-out is never enumerated when only MIN is needed.
- There is no published JOB number isolating late materialisation. FFX (section 3.14) and diamond L&E
  (section 3.10) are the nearest evidence.

### 5.2 Groupjoin
- Fent & Neumann. "A Practical Approach to Groupjoin and Nested Aggregates". PVLDB 14(11):2383-2396, 2021.
  https://vldb.org/pvldb/vol14/p2383-fent.pdf
- Groupjoin fuses a join and the following GROUP BY on the same key. It occurs in about 1/8 of TPC-H and TPC-DS
  queries. Unnesting also creates it.
- The paper covers estimation, planning and contention-free parallel execution [snippet].
- Relevance to JOB: low (no GROUP BY). High for TPC-H (Q13 and others) and TPC-DS.

### 5.3 Eager aggregation
- The classical rule: push a (partial) aggregate below a join when it is a key of the join or duplicate-insensitive.
  For JOB, MIN over fan-out qualifies.
- No 2025-2026 paper with JOB numbers was found (none found).

### 5.4 Unnesting
- Neumann. "Improving Unnesting of Complex Queries". BTW 2025, pp. 25-47. doi 10.18420/BTW2025-01 [snippet; not
  read]. Follows Neumann & Kemper "Unnesting Arbitrary Queries" (BTW 2015), which introduced dependent-join
  elimination and relies on the **mark join**.
- JOB has no subqueries. The relevance is to SQLStorm, TPC-DS and TPC-H (Q2, Q4, Q17, Q20, Q21, Q22).

### 5.5 IN predicates and mark joins (Birler & Neumann, CIDR 2026)
- "On the Vexing Difficulty of Evaluating IN Predicates". CIDR 2026.
  https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf
- IN / NOT IN with nullable attributes: no subquadratic algorithm exists in general unless SAT improves (reduction
  from orthogonal vectors). Common cases can be done in linear time.
- Correctness survey (IN/NOT IN with nullable attributes):
  - Wrong results: ClickHouse 25.8, Hyper 9.1.0, **DuckDB 1.3.0**, Snowflake 9.21.0, Redshift 1.0.118447,
    Firebolt Core 4.23.5.
  - No multi-attribute IN support (otherwise correct): SQL Server 2022, BigQuery 2025-07-22.
  - Correct but sometimes quadratic: Materialize, Databricks, CedarDB v2025-07-23, SQLite, DB2, MariaDB, Oracle
    23c, PostgreSQL 17, TimescaleDB, YugabyteDB.
- Umbra 25.12 with their right/left mark-join algorithms is correct and linear in the common case (one nullable
  attribute). Setup: Ryzen 9 5950X, one core per container.
  - Only Umbra ran the correlated NOT IN query in linear time.
  - PostgreSQL, DBMS Y and Umbra ran the decorrelated version in linear time.
- Implication for rudb (DuckDB-compatible):
  - The DuckDB compatibility target is itself wrong on NULL-ful NOT IN. The spec must decide whether to be
    bug-compatible or SQL-correct.
  - The engine needs a mark-join operator (tri-state match: true, false, null) in the IR.

---------------------------------------------------------------------------------------------------------------------

## 6. Cardinality-estimation robustness (brief)

- The JOB failure mode is underestimation of multi-join results, where PostgreSQL error grows exponentially with
  join count (16%, 32%, 52% of estimates off by 10x or more at 1, 2, 3 joins; Leis 2015).
- **LpBound** (Zhang, Mayer, Abo Khamis, Olteanu, Suciu; arXiv 2502.05912, SIGMOD 2025):
  - A pessimistic upper bound from l_p-norms of degree sequences, solved as an LP with Shannon inequalities.
  - Estimation takes a couple of ms and a few MB of stats. It is orders of magnitude more accurate than
    PostgreSQL 13.14 / DuckDB estimators on JOB, STATS and subgraph workloads.
  - Feeding LpBound estimates into PostgreSQL for the 20 longest queries gave faster plans, with runtime
    improvements up to 3000 s. The LPflow variant stays under 70 ms per JOB query.
  - Predicate-aware stats improve error up to 50% (JOB-light), 65% (JOB-range) and 10% (STATS).
  - Observations on DuckDB's estimator: fixed 0.2 selectivity for range predicates; HLL-based domain sizes; can
    overestimate by more than 12 orders of magnitude on cyclic self-joins; ignores GROUP BY.
- **xBound** (Stoian, Bang, Zhao, Camacho-Rodriguez, Tian, Kipf; arXiv 2601.13117, 2026):
  - Provable lower bounds to fix underestimation.
  - DuckDB v1.4, PostgreSQL 18 and Fabric DW all overwhelmingly underestimate on JOB-light and StackOverflow-CEB.
  - Reduces underestimate Q-error by 8.38x (DuckDB) and 2.30x (PostgreSQL) on SO-CEB. JOB-light: only DuckDB's
    median improves, by 2.45x.
- **AQP / re-optimisation** (arXiv 2511.16455): mid-query re-planning on DuckDB gives 123.9 -> 115.1 s (0.10.1)
  and 55.3 -> 50.6 s (1.3.2) on JOB. Modest gains, because DuckDB's plans are already reasonable.
- Robust execution (RPT/RPT+/SYA/TTJ/diamond) reduces how much estimates matter: RPT's RF on JOB drops from 30.4
  to 1.2 average. For a new engine, robust execution is higher leverage than a better estimator. Keep the
  estimator decent (sampling for LIKE selectivity, HyPer-style) and bound-aware (LpBound or xBound style) to avoid
  catastrophes.

---------------------------------------------------------------------------------------------------------------------

## 7. String predicates (LIKE '%...%' and friends)

### 7.1 Why it matters on JOB
- Many JOB templates filter movie_info.info, movie_companies.note, title.title, name.name, char_name.name and
  keyword.keyword with LIKE '%x%' or multi-segment patterns ('%(200%)%', 'USA:% 200%'), often OR-ed (section 1.3).
- These predicates run over millions of rows before the join can shrink anything. When PT/RPT removes join
  overhead, scan-plus-LIKE becomes a larger share of the remaining time.

### 7.2 Compiled LIKE, Umbra (Riedl, Fent, Bandle, Neumann, ADMS 2023)
- "Exploiting Code Generation for Efficient LIKE Pattern Matching". ADMS@VLDB 2023, CEUR 3462.
  https://db.in.tum.de/~riedl/papers/like-codegen.pdf
- Generates pattern-specific code: a generalised SSE (PCMPESTRI-style packed compare) search for longer patterns,
  and KMP with an LPS table precomputed at code-generation time.
- Up to 2.5x faster LIKE; patterns tested up to about 300 characters [snippet].

### 7.3 "Teach Your DBMS to LIKE Strings" (arXiv 2608.23307, PVLDB 20(1), 2026)
- Authors: Nguyen, Ginter, Duc-Tam Nguyen, Neumann, Leis. Umbra implementation. Hardware: Ryzen 9 9950X.
- **Wildcard filter (compiled):**
  - Split the pattern on %. Each segment is matched with SIMD vector primitives (short), Boyer-Moore (medium) or
    Two-Way (long). Search parameters (skip tables) are precomputed at compile time.
  - Underscores: leading and trailing _ are consumed as fixed-length skips. The longest underscore-free part
    anchors the search; on a mismatch, retry at the next occurrence.
  - Fast path: with Umbra's German-string layout (12-byte inline prefix), literal-prefix patterns ('ab%') are
    checked directly on the inline prefix without dereferencing.
- **Wildcard join** (column LIKE column-of-patterns): an Aho-Corasick automaton over pattern literals replaces
  nested loops. It uses a UTF-8-aware trie with compact internal nodes.
- Results:
  - Wildcard join up to 30.6x over DuckDB v1.4.4 (e.g., 4.59 vs 0.15 queries/s) and 114.75x over baseline Umbra
    (0.04 q/s).
  - One join query is 81.3x over DuckDB; baseline Umbra times out beyond 180 s.
  - Compiled filter: 13.3x over DuckDB and 14.3x over baseline Umbra on a filter stress query (Hacker News
    benchmark). Both baselines are slowed by interpreted pattern matching.
  - Better LIKE selectivity estimation also changed join orders (their "Query 2", where DuckDB won through a
    better join order).

### 7.4 Compression-aware LIKE (FSST domain), DaMoN 2026
- Pop, Riedl, Neumann (per citation [55] in arXiv 2608.23307). "Compression-Aware LIKE: Matching Patterns in the
  FSST Domain". DaMoN 2026, doi 10.1145/3789237.3809128.
- Compiles the pattern into automata over FSST symbol codes, with separate automata for prefix, suffix and
  substring parts. Adds early rejection and SIMD self-loop traversal. Interpreted and compiled variants.
- 2.5-17x over decompress-then-match on TPC-H, StackOverflow and IMDB [snippet].

### 7.5 FSST basics (PVLDB 13(11), 2020)
- Boncz, Neumann, Leis. https://www.vldb.org/pvldb/vol13/p2649-boncz.pdf
- A static 255-symbol table (1-8 byte symbols) gives random access to individual strings. Equality on compressed
  strings works directly if both sides share a symbol table (compress the constant). LIKE on compressed strings
  was left as future work (now addressed by section 7.4).
- About 2x compression on text; LZ4-comparable speed.
- In an engine, the TPC-H overhead is at most 3% with string columns compressed, and Q19 gets 30% faster. The
  worst case (padded string join keys) costs 14%, mostly from memcpy into the hash table (484 ms uncompressed vs
  554 ms FSST).

### 7.6 Dictionary-encoded LIKE
- Standard technique: evaluate the predicate once per distinct dictionary entry, producing a bitmap or selection
  over codes, then filter codes. The cost is proportional to dictionary size, not row count.
- No 2025-2026 paper with JOB-specific numbers was found (none found). Applicability depends on storage: IMDB
  columns like movie_info.info have high cardinality, so dictionary gains are limited there. keyword.keyword,
  company_type.kind, info_type.info and kind_type.kind are low-cardinality and benefit.

### 7.7 SIMD substring search
- Short-needle SIMD search (compare the first and last needle bytes across a 16/32-byte window, then verify) is the
  primitive used for short segments in section 7.3.
- Long needles use Two-Way, which is O(n) worst case and O(1) extra space.

---------------------------------------------------------------------------------------------------------------------

## 8. Cross-cutting observations

1. **JOB speedups compose from three independent sources:**
   - (a) Plan robustness: semijoin/Bloom transfer, about 1.5x geomean over DuckDB, with 10-15x on individual
     queries (Yann+ max 14.84x, TTJ max 12.6x, Free Join max 19.36x).
   - (b) Engine efficiency: compiled pipelines plus an unchained HT. The Umbra class is about 2x vs open
     addressing, and about 6x vs DuckDB 0.10 on TPC-H SF100.
   - (c) String predicate speed: compiled LIKE, 2.5-13x on predicate-bound work.
   None of the papers combines all three and reports a JOB total.
2. **Geomean vs total.** JOB totals are dominated by a few slow queries (e.g., 8d, 13a, 16b, 17e, 33-ish
   templates). Robust techniques pay off most on these, so total-time speedups exceed geomean speedups.
3. **Single-thread vs multi-thread.** Most JOB papers run DuckDB single-threaded (AQP, Parachute, TTJ, Free
   Join). RPT+ uses 8 threads and Yann+ 72. JOB queries are small (sub-second on DuckDB), so parallel
   scalability is limited by per-query setup cost. Compile latency matters as much as execution.
4. **Compile time.** JOB queries on DuckDB take tens to hundreds of ms each (DuckDB avg 0.828 s in SYA's setup,
   and 55.3 s / 113 = about 0.49 s single-threaded in AQP). A compiled engine must keep compile latency well below
   this, i.e., an Umbra-style fast backend or adaptive interpretation. (Covered in other notes.)
5. **Regression risk.** Every robust technique has a regression tail:
   - RPT: below 0.9x on at least 28% of SQLStorm queries.
   - SYA: max slowdown 1.3x.
   - TTJ: min 0.9x vs HJ and 0.2x vs YA.
   - Free Join: min 0.85x.
   - Unchained HT: up to 30% slower on tiny queries.
   Adaptivity (sampling, early disable) is mandatory.

---------------------------------------------------------------------------------------------------------------------

## 9. Source index (primary PDFs read)

- Leis 2015 PVLDB 9(3): https://www.vldb.org/pvldb/vol9/p204-leis.pdf
- Leis 2018 VLDBJ: https://link.springer.com/article/10.1007/s00778-017-0480-7
- PT CIDR 2024: https://www.cidrdb.org/cidr2024/papers/p22-yang.pdf [URL not re-verified; PDF read from local copy]
- RPT SIGMOD 2025: https://arxiv.org/abs/2502.15181
- RPT+ PVLDB 19(6) 2026: https://people.iiis.tsinghua.edu.cn/~huanchen/publications/rpt+-vldb26.pdf
- Parachute PVLDB 18: https://arxiv.org/abs/2506.13670
- SQL Server bitmaps CIDR 2026: https://www.vldb.org/cidrdb/papers/2026/p29-zhao.pdf
- LIP PVLDB 10(8): https://www.vldb.org/pvldb/vol10/p889-zhu.pdf [URL not re-verified]
- Yannakakis+ SIGMOD 2025: https://arxiv.org/abs/2504.03279
- SYA PVLDB 18(8): https://arxiv.org/abs/2411.04042
- TreeTracker TODS 2025: https://arxiv.org/abs/2403.01631
- Diamond PVLDB 17(11): https://db.in.tum.de/people/sites/birler/papers/diamond.pdf
- WCOJ Umbra PVLDB 13(11):1891-1904: https://doi.org/10.14778/3407790.3407797
- Free Join SIGMOD 2023: https://arxiv.org/abs/2301.10841
- SplitJoin: https://arxiv.org/abs/2510.25684
- ADOPT: https://arxiv.org/abs/2307.16540
- FFX: https://arxiv.org/abs/2609.09002
- Unchained HT DaMoN 2024: https://db.in.tum.de/~birler/papers/hashtable.pdf
- Bandle SIGMOD 2021: https://db.in.tum.de/~bandle/papers/bandle-partitionVsNonPartition.pdf
- Interleaving with coroutines PVLDB 11(2):230: https://doi.org/10.14778/3149193.3149202
- Groupjoin PVLDB 14: https://vldb.org/pvldb/vol14/p2383-fent.pdf
- IN predicates CIDR 2026: https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf
- LpBound: https://arxiv.org/abs/2502.05912
- xBound: https://arxiv.org/abs/2601.13117
- AQP on DuckDB: https://arxiv.org/abs/2511.16455
- Simpli-Squared (JOB PostgreSQL heuristics): simpli.txt (arXiv, read locally)
- SQLStorm PVLDB 2025: https://doi.org/10.14778/3749646.3749683
- LIKE 2026: https://arxiv.org/abs/2608.23307
- LIKE codegen ADMS 2023: https://db.in.tum.de/~riedl/papers/like-codegen.pdf
- FSST PVLDB 13: https://www.vldb.org/pvldb/vol13/p2649-boncz.pdf
- FSST-domain LIKE DaMoN 2026: https://doi.org/10.1145/3789237.3809128 [snippet]
- CEB / Flow-Loss: https://arxiv.org/abs/2101.04964 [snippet]
- STATS-CEB: https://www.vldb.org/pvldb/vol15/p752-zhu.pdf [snippet]
- DSB: https://www.vldb.org/pvldb/vol14/p3376-ding.pdf [snippet]

---------------------------------------------------------------------------------------------------------------------

## 10. Implications for a compiled engine targeting JOB

1. **Budget realistically.** 10x DuckDB on JOB means about 5.5 s single-threaded total if DuckDB 1.3.2 takes
   55.3 s (AQP, Xeon E-2236). DuckDB itself keeps improving (0.10.1 -> 1.3.2 halved JOB time; v1.5 adds join Bloom
   filters), so benchmark against the current DuckDB release, not the one in the papers.
2. **Make semijoin reduction a first-class plan stage, not a bolt-on.** Implement RPT-style LargestRoot transfer
   (forward and backward) over the join tree for alpha-acyclic queries, which is all of JOB. The evidence is
   1.46-1.54x geomean and RF 30.4 -> 1.2 on JOB (RPT, RPT+, Parachute).
3. **Use RPT+'s asymmetric and adaptive policy, not symmetric full transfer.** Sample about 100K tuples, keep a
   filter only if it passes about 35% or fewer, and drop it if it is not paying (Parachute: under 60% filtered
   after 4000 rows). Plain RPT regresses on at least 28% of SQLStorm queries; RPT+ on 2.1%.
4. **Cascade cheapest-first:** zone-map / min-max, then BF, then exact HT probe. Transfer must never disable
   row-group skipping; that caused RPT+ regressions on JOB templates 8, 10 and 24.
5. **Use a cache-sectorized BF with low FPR** (about 20 bits/key, k = 7, 64 B blocks, around 2.5 cycles/tuple).
   Accuracy pays twice, because BF false positives widen downstream min-max ranges (4x on JOB 07c in RPT+).
6. **Use the unchained hash table as the default join HT:**
   - Directory word = 48-bit pointer plus 16-bit Bloom tag (4-of-16 bits from a 2,048-entry table).
   - Partitioned build into a dense adjacency array, CRC32-based hashing, huge pages, no atomics.
   - Expect about 2x over open addressing on join-heavy probes. Avoid radix-partitioning the probe side (Bandle
     2021).
7. **Keep a fast path for tiny builds.** Unchained is up to 30% slower on tiny queries. Use a direct-mapped or
   IN-list/perfect-hash path when the build has 50 or fewer distinct keys (DuckDB 1.2's IN-list SIP), and for
   dimension tables such as kind_type, info_type, company_type and role_type (at most a few hundred rows).
8. **Expose Lookup and Expand as separate IR operators** (diamond hardening). The HT must return a contiguous match
   range. This allows deferring n:m fan-out (cast_info, movie_keyword, movie_info per title) past subsequent
   filtering lookups, and it is the prerequisite for factorised or MIN-pushdown evaluation.
9. **Exploit MIN(...)-only outputs.** MIN is duplicate-insensitive, so aggregate below n:m joins (eager
   aggregation) or keep fan-out factorised and never enumerate it. Carry only keys and row ids through the join
   pipeline and fetch payload strings at the end (late materialisation). No paper isolates this on JOB, but FFX
   shows 100x-class wins on n:m analytics.
10. **Do not ship WCOJ as a default on JOB.** Umbra measured pure WCOJ as about 25x slower on JOB. If WCOJ, Free
    Join or SplitJoin is added for cyclic or graph queries, gate it with a Freitag-style hybrid optimiser that only
    fires when binary joins would grow intermediates (5 false negatives out of 923 joins, zero JOB slowdown).
11. **Batch the probe loop to allow group prefetch when the directory exceeds LLC.** Psaropoulos found GP (group
    about 10) gives 2.7-3.7x, above CORO and AMAC. For LLC-resident directories, rely on OoO and the in-word Bloom
    tag (Birler 2024). The code generator should emit both variants and pick by build size at runtime.
12. **Compile LIKE.** Split on %; match segments with SIMD (short), Boyer-Moore (medium) or Two-Way (long); precompute
    skip tables at compile time; handle _ by fixed skips; add a German-string 12-byte inline-prefix fast path. The
    reported gains are 13.3x over DuckDB on filter-bound work and 2.5x over interpreted LIKE in Umbra.
13. **Fuse OR-ed LIKEs over one column** (e.g., JOB 15c, 19a: 'USA:% 199%' OR 'USA:% 200%') into one pass sharing
    the common prefix 'USA:'. Use multi-pattern Aho-Corasick for pattern lists and wildcard joins (up to 30.6x over
    DuckDB v1.4.4).
14. **Evaluate string predicates on dictionaries or compressed codes where possible.** Per-dictionary-entry
    evaluation suits low-cardinality JOB columns. FSST equality works directly on codes, and FSST-domain LIKE
    automata give 2.5-17x over decompress-then-match (DaMoN 2026). Decompress late: FSST TPC-H overhead is at
    most 3%.
15. **Order predicates within a scan adaptively** (LIP-style): cheap numeric predicates (production_year ranges,
    IS NOT NULL) first, LIKE last, and transferred BFs positioned by measured selectivity per cost. LIP cut plan
    spread from 2.1-58 s to 1.3-7.4 s.
16. **Plan with a robust-by-construction cost model.** C_mm (tuples produced) suffices (Leis 2018). Prefer hash
    joins; avoid non-index nested loops on estimates (the PostgreSQL JOB timeouts). With RPT, bushy plans gain only
    6-11% over left-deep, so a left-deep DP plus transfer is adequate for JOB.
17. **Estimate LIKE selectivity by sampling** (HyPer-style), not with fixed constants. DuckDB uses a fixed 0.2 for
    ranges (LpBound paper), and LIKE-selectivity errors flipped join orders in the 2026 LIKE paper. Add a
    pessimistic or lower-bound sanity check (LpBound / xBound) to catch catastrophic misestimates in multi-join
    subtrees.
18. **Mark join in the IR from day one.** IN / NOT IN / EXISTS decorrelation needs it for linear-time evaluation
    (Birler & Neumann CIDR 2026). DuckDB 1.3.0 returns wrong results on NULL-ful NOT IN, so decide explicitly
    between DuckDB bug-compatibility and SQL-correct semantics.
19. **Add groupjoin** (Fent & Neumann 2021) for TPC-H and TPC-DS (about 1/8 of queries) and for unnested
    subqueries. It is not needed for JOB but cheap once the HT supports in-place aggregates.
20. **Measure per-query and tail, not just total.** Report geomean, total and max-regression on JOB, CEB (13.6K
    queries, the same IMDB data, which stresses robustness across 15-16 templates), JOB-light and STATS-CEB.
    Every published robust technique has a regression tail (RPT+ 2.1%, SYA 1.3x max, TTJ 0.9x), and a 10x claim
    must survive CEB's breadth.
21. **Keep compile latency small relative to JOB query time** (DuckDB's typical per-query time is about 0.5 s
    single-threaded). Transfer-filter construction, sampling decisions and HT-variant selection should be runtime
    parameters of pre-compiled pipeline code, not triggers for recompilation.
22. **Validate against the moving target.** DuckDB v1.5.x now pushes join Bloom filters into probe scans and
    already has min-max SIP. Expect DuckDB to absorb about 1.2-1.5x of the RPT gains. The residual 10x must come
    mainly from engine efficiency (compiled pipelines, unchained HT, compiled LIKE, late materialisation) on top of
    robust plans.
