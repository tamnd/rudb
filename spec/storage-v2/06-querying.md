# What the format gives the engine

A format that decodes faithfully and hands the engine a column of values has given the engine nothing it did not already have from Parquet. The reason to have a format at all is the things in this document, and they all come from invariants 5 and 6: equality and order are needed on a key rather than on its bytes, and the work is over a handful of distinct values repeated many times.

## The four promises

**One. A code is comparable across the whole table.** Every block of a dictionary column uses the same dictionary, so code 5 in block 1 and code 5 in block 700 are the same value. This is the promise Parquet cannot make, because its dictionary is per column chunk, and it is the one that changes what execution can do rather than how fast it does it.

**Two. Equal codes mean equal values and different codes mean different values.** The dictionary is deduplicated at write time, so the engine can use a code as a key without ever looking at what it stands for.

**Three. `rank[code]` orders codes the way the values order.** One array lookup, into an array that is four bytes per distinct value and is cached for the life of the process.

**Four. The directory answers `count`, `min`, `max`, `sum`, nulls and a distinct estimate per block per column without reading the block.**

## What those buy, on the queries in document 01

### Grouping a string column becomes grouping a four byte integer

`GROUP BY URL` in q34 and q35, `GROUP BY SearchPhrase` in q13, q14, q22 and q23, `GROUP BY Title` in q38, `GROUP BY Referer` inside q40's `CASE`. Ten queries.

Today each of those hashes a string per row, compares strings on collision, and stores a string in the hash table's key. With promise one the key is a `u32`, the hash is a mix of four bytes, the comparison is an integer compare, and the table's key column is four bytes wide instead of sixteen for a pointer and a length. The strings are touched exactly once at the end, for the ten groups that survive `ORDER BY c DESC LIMIT 10`.

This is also what makes the two phase radix partitioned aggregate of F5 work well here, because radix partitioning wants a key whose bits are cheap to look at, and a code is.

### A predicate runs once per distinct value

`URL LIKE '%google%'` in q21, q22 and q24. `Title LIKE '%Google%'` and `URL NOT LIKE '%.google.%'` in q23. `SearchPhrase <> ''` in ten queries. `MobilePhoneModel <> ''` in two.

A page of `URL` in hits holds roughly twenty thousand distinct values across a hundred thousand rows, and the whole column holds far fewer distinct values than rows. Running the `LIKE` over the dictionary rather than the rows is the ratio of distinct to rows, and on this data that is between five and a hundred to one depending on the column. Note 11 measured DuckDB doing this per page and found it to be the largest of its five advantages; doing it per table rather than per page is strictly better, because the dictionary is smaller relative to the rows it covers the more rows it covers.

`SearchPhrase <> ''` is the extreme case. It is one comparison against one dictionary entry and the answer for every row is a single bit lookup.

### An expensive function runs once per distinct value

q29 is `REGEXP_REPLACE(Referer, '^https?://(?:www\.)?([^/]+)/.*$', '\1')` as a group key, and it is 386 nanoseconds a row in the current profile, which makes it one of the most expensive queries in the suite. `Referer` has far fewer distinct values than rows. Running the regex over the dictionary produces a new dictionary of hostnames, and the group key is then a code into that, so the regex runs once per distinct referer instead of once per row.

q28's `AVG(length(URL))` is the same shape. `length` runs once per distinct URL, and the average is the length array weighted by the per code counts.

This generalises to any pure scalar function of a dictionary column, which is a large family, and the rule is that the result is itself a dictionary column over the same code space.

### TPC-H's decimal arithmetic runs once per distinct multiplier

`sum(l_extendedprice * (1 - l_discount))` appears in nine of the twenty two queries. `l_discount` has eleven distinct values. `l_tax` has nine.

So `1 - l_discount` is eleven subtractions rather than six million, and for q1, which groups by `l_returnflag` and `l_linestatus` into four groups, `sum(l_extendedprice * (1 - l_discount))` can be computed as a sum of `l_extendedprice` per combination of group and discount code, which is forty four accumulators, followed by forty four multiplies at the end. That replaces six million decimal multiplies with six million adds and forty four multiplies, and a decimal multiply is several times an add.

The format's contribution here is only that `l_discount` arrives as a code into an eleven entry dictionary rather than as a decimal per row. The rewrite is the optimiser's, and it is the sort of rewrite that is impossible to see when the column arrives as values.

### Whole blocks are answered from the directory

q1 `SELECT COUNT(*)`, q7 `SELECT MIN(EventDate), MAX(EventDate)`, and the `SUM(AdvEngineID)` and `COUNT(*)` parts of q3. Those read the directory and stop.

More usefully, the same field answers the unfiltered part of every aggregate over a block the filter did not touch, so a query with a filter on one column and a `sum` on another only computes the `sum` for blocks the filter actually split.

### Ordering works on codes

`ORDER BY SearchPhrase` in q26. `ORDER BY s_name`, `ORDER BY n_name`, `ORDER BY p_brand, p_type, p_size` in TPC-H. With promise three those sort four byte integers through `rank` rather than comparing strings, and for a top k of ten that is the difference between ten string comparisons per row and ten integer comparisons per row.

The range predicates get it too. A `BETWEEN` on a dictionary column becomes a range on `rank`, which the block's minimum and maximum rank in the directory can prune against, which is how a string column gets zone maps that are worth anything.

### Counting distinct is counting set bits

`COUNT(DISTINCT UserID)` and `COUNT(DISTINCT SearchPhrase)` in eight queries. For a dictionary column the exact answer is the population of a bitmap over the code space, which for a column with ten million distinct values is 1.25 MB of bitmap and one pass. Per group it is a bitmap per group, which is only affordable for a small number of groups, so the sketch stays for the rest. But q6, `SELECT COUNT(DISTINCT SearchPhrase)`, becomes exact and cheap, and the per group cases in q9 through q14 get a much better starting point than hashing strings.

## What the format does not fix

Three ClickBench queries are point lookups against an unindexed high cardinality column: `UserID = 435090932899640449` in q20, `RefererHash = ...` in q41, `URLHash = ...` in q42. Zone maps do not prune a random high cardinality integer against an unsorted table, so all three read the column. An index would fix them and an index is not this format.

`GROUP BY UserID` in q16, `GROUP BY UserID, SearchPhrase` in q17, q18 and q19, and `GROUP BY WatchID, ClientIP` in q32 and q33 are the genuinely hard aggregations, because the keys are high cardinality integers with no dictionary worth having. Those are F5's problem and the format's only contribution is not making them worse.

The six queries that group by an expression are helped only when the expression is a pure function of one dictionary column, which covers q19, q29, q35 and q43 but not q36, whose key is four separate arithmetic derivations of `ClientIP`, a column with no dictionary.

## The rule this implies for the vector layer

Everything above depends on the scan handing execution a form rather than a copy, and on every operator between the scan and the answer preserving that form until something genuinely needs the values. That is what `Form` and the `flatten` counter in F1 are for, and it is why `xtask lint flatten` exists: a `flatten` in the middle of a plan throws away every promise in this document silently, and the only defence is that each one is a reviewed line with a written reason.
