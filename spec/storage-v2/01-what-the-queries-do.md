# What the queries actually do

Everything in the rest of this directory is derived from this document, so this one is evidence and not design. The queries are the 43 of ClickBench over `hits` and the 22 of TPC-H, both taken from the suite definitions in tamnd/rudb-bench, which are the ones the project actually measures against. The classification below is a count over those 65 statements and not an impression of them.

## ClickBench, 43 queries over one table of 105 columns

### How many columns a query reads

| columns read | queries |
|---|---|
| 0 | q1 |
| 1 | q2, q4, q5, q6, q7, q8, q13, q16, q20, q21, q26, q29, q30, q34, q35, q36 |
| 2 | q3, q9, q11, q14, q15, q17, q18, q22, q25, q27, q28 |
| 3 | q12, q19 |
| 4 | q10, q23, q33 |
| 5 | q31, q32, q37, q38, q43 |
| 6 | q39, q41 |
| 7 | q42 |
| 8 | q40 |
| 105 | q24 |

The mean over the 42 queries that are not `SELECT *` is 2.6 columns. The maximum is 8. There are 105 columns in the table.

Only 25 distinct columns appear anywhere in the suite, so 80 of the table's columns are never read by any query except q24, and q24 reads them only to print ten rows.

### Which columns, and how often

| column | queries that read it | what it is |
|---|---|---|
| SearchPhrase | 14 | large string, mostly empty |
| UserID | 13 | 64 bit integer, high cardinality |
| URL | 9 | large string, high cardinality |
| IsRefresh | 10 | one bit in practice |
| CounterID | 8 | small integer, skewed |
| EventDate | 8 | date, and the table is clustered by it |
| ResolutionWidth | 6 | small integer, few distinct |
| AdvEngineID | 5 | small integer, about 19 distinct |
| EventTime | 5 | timestamp, and the table is clustered by it |
| DontCountHits | 4 | one bit |
| ClientIP | 4 | 32 bit integer, high cardinality |
| SearchEngineID | 3 | small integer |
| Title, Referer, WatchID, TraficSourceID, URLHash, RegionID | 2 each | |
| MobilePhone, MobilePhoneModel, RefererHash, IsLink, IsDownload, WindowClientWidth, WindowClientHeight | 1 each | |

### What the predicates look like

16 distinct columns carry a predicate anywhere in the suite. The shapes are these and there are only four of them.

| shape | where | what survives |
|---|---|---|
| equality on a small skewed integer | `CounterID = 62` in q37 to q43, seven queries | about one percent of rows |
| a range on the clustering column | `EventDate >= '2013-07-01' AND EventDate <= '2013-07-31'` in six of the same seven | every row in the table, see below |
| a flag equal to zero | `IsRefresh = 0`, `DontCountHits = 0`, `IsLink <> 0`, `IsDownload = 0` | most rows, cheaply |
| a string against a constant | `SearchPhrase <> ''` in ten queries, `URL LIKE '%google%'` in three, `Title LIKE '%Google%'` in one | a few percent |

Three exact point lookups round it out: `UserID = 435090932899640449` in q20, `RefererHash = ...` in q41 and `URLHash = ...` in q42. Those are needle in a haystack queries against columns with no index, and they read the whole column today.

That second row was written from the shape of the predicate rather than from the data and it is wrong. Measured, `hits` runs from 2013-07-02 to 2013-07-31 and nothing else, so the July month range covers the whole table and removes no rows at all. The clustering is real, at a median of two distinct dates in a block of 122,880 rows, and only q43 has a range narrow enough to use it, which is two days and prunes to 169 blocks of 814. The pruning in the other six comes entirely from `CounterID = 62`, which lands in 8 blocks of 814 because the median block holds exactly one distinct `CounterID`. Document 08 has the full table.

The seven queries q37 through q43 are the same predicate with the projection changed, and together they are the largest single block of the suite. They read five to eight columns, they keep well under one percent of the rows, and they return ten. A format that let them touch one percent of the bytes would take them out of the profile entirely.

### What comes out

32 of the 43 queries end in `ORDER BY something LIMIT 10` or `LIMIT 25`. The only unbounded sort is q8, which sorts about 19 groups. No query in the suite ever needs a full ordering of the table, and no query returns more than 25 rows except q24, which returns 10.

8 queries contain a `COUNT(DISTINCT)`, and in 5 of them it is per group.

### Group keys

| kind | queries | note |
|---|---|---|
| small integer domain | q8, q9, q10, q15, q28, q31, q40, q42 | tens to thousands of groups |
| large string | q13, q14, q22, q23, q34, q35, q37, q38, q39, q40 | millions of groups, and always a column a dictionary covers |
| high cardinality integer | q16, q17, q18, q19, q32, q33, q41 | the genuinely hard case |
| an expression over a column | q19 `extract(minute)`, q29 `REGEXP_REPLACE`, q35 a constant, q36 `ClientIP - n`, q43 `DATE_TRUNC` | six queries |

That last row is worth staring at. Almost a seventh of the suite groups by a function of a stored column rather than the column, so any format trick that only works when the group key is stored verbatim is worth six sevenths of what it looks like. But q35 and q36 are functions that are injective on the column's domain, which means the grouping could be done on the stored value and the function applied to the ten surviving groups. That is a plan rewrite rather than a format feature, and the format's job is only to make sure the stored form is still there to group on.

## TPC-H, 22 queries over 8 tables

A completely different shape and that is the point of including it.

### Joins

20 of the 22 have a join. Only q1 and q6 read one table. The join keys are without exception surrogate integers with referential integrity: `l_orderkey` into `orders`, `l_partkey` into `part`, `l_suppkey` into `supplier`, `o_custkey` into `customer`, `c_nationkey` and `s_nationkey` into `nation`, `n_regionkey` into `region`. Every one of those is a dense range starting at zero or one, with the single exception of `orderkey`, which is sparse by design.

One table is 85 percent of the bytes. At scale factor one, `lineitem` is 6,001,215 rows of a roughly one gigabyte dataset, and everything else is dimension sized. Every query that matters reads `lineitem` and joins it to something small.

### Predicates

12 of the 22 filter on a date range, and in every case it is `l_shipdate`, `o_orderdate` or `l_receiptdate`. `lineitem` is generated in `orderkey` order and the ship dates correlate with the order dates, so those ranges prune well against insertion order without anybody sorting anything.

The string predicates split cleanly in two. One group is equality or a small `IN` list against a column with a tiny fixed domain: `l_shipmode` has 7 values, `l_returnflag` 3, `l_linestatus` 2, `o_orderpriority` 5, `o_orderstatus` 3, `l_shipinstruct` 4, `n_name` 25, `r_name` 5, `p_brand` 25, `p_container` 40. The other group is a `LIKE` over real text: `p_type LIKE '%BRASS'`, `p_name LIKE '%green%'`, `o_comment NOT LIKE '%special%requests%'`, `s_comment LIKE '%Customer%Complaints%'`, `p_name LIKE 'forest%'`. Seven predicates in the whole suite are in the second group and the columns they run on are almost never read for anything else.

### The arithmetic

`l_extendedprice * (1 - l_discount)` appears in nine queries and is the hot inner loop of the benchmark. Now look at the domains: `l_discount` has 11 distinct values, `l_tax` has 9, `l_quantity` has 50. Of `lineitem`'s 16 columns, 8 have fewer than 51 distinct values. So the whole of `l_extendedprice * (1 - l_discount) * (1 + l_tax)` ranges over `l_extendedprice` times 99 possible multipliers, and an engine that knew that could compute 99 products per distinct price instead of one product per row, or more usefully compute the multiplier once per distinct discount and keep it.

### Group keys and output

Group keys are either tiny, four groups in q1 and 25 in q5 and 2 in q12, or one group per key of a table, which is q3 by orderkey, q10 by custkey, q11 and q16 by partkey, q15 by suppkey, q18 by orderkey. Output is bounded at 100 rows in five queries and unbounded but small in the rest. Nothing returns a large answer.

## The six invariants

Everything above collapses to six statements, and these are what the format is designed against.

**1. A query reads few columns out of many.** ClickBench: 2.6 of 105 on average. TPC-H: the widest query over `lineitem` reads 6 of its 16. The format has to make reading five columns out of a hundred and five cost five parts in a hundred and five, and that has to be true of the metadata as well as the data. Parquet gets the data half of this right and the metadata half wrong, and document 02 is mostly about the second half.

**2. A query keeps few rows out of many, and the predicate is on one or two columns.** Seven ClickBench queries keep under one percent. Twelve TPC-H queries keep a date range. The format has to let a reader decide not to read, from a summary, at a granularity fine enough to matter.

**3. The columns predicates run on and the columns that carry the bytes are different columns.** `CounterID`, `EventDate` and `IsRefresh` are the filter. `URL` and `Title` are the payload. `l_shipdate` is the filter and `l_extendedprice` and `l_comment` are the payload. The filter set is small, narrow and low cardinality. The payload set is wide, large and high cardinality. Storing them the same way and next to each other, which is what a row group does, means every filter read drags the payload's layout along with it.

**4. The answer is small.** 32 of 43 ClickBench queries return at most 25 rows. No TPC-H query returns more than 100. Nothing in either benchmark needs the table materialised, so a format optimised for handing back complete rows quickly is optimised for something nobody asked for.

**5. Grouping, joining and comparing need equality and order on a key, not the key's bytes.** Every ClickBench group by a string is a group by a column a dictionary covers. Every TPC-H join is on a dense surrogate integer. If the format can hand execution an integer code with the promise that code equality is value equality, then ten of the ClickBench queries stop touching strings at all until the last ten rows.

**6. The work is over a handful of distinct values repeated many times.** `l_discount` has 11 values across six million rows. `AdvEngineID` has 19 across a hundred million. A page of `URL` has twenty thousand distinct values across a hundred thousand rows. Every expensive thing in both benchmarks, a `LIKE`, a regex, a decimal multiply, a hash, is being done once per row when it could be done once per distinct value. This is the same observation as the aggregate hash table lesson in note 06 and the dictionary filter lesson in note 11, and it is the one that is worth the most.

## What follows

Invariant 1 says the metadata has to be columnar, which is document 02.

Invariants 2 and 3 say the summaries have to be per block and per column and stored apart from the data, and that the physical arrangement should follow how a column is used rather than which row it belongs to, which is also document 02.

Invariants 5 and 6 say the dictionary is the centre of the design rather than an encoding among others, which is document 03 for the bytes and document 06 for what execution gets.

Invariant 4 says none of this has to be fast at reconstructing rows, which is permission to make choices that a row store would refuse.
