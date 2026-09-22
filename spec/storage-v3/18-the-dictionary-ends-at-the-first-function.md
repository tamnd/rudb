# The dictionary ends at the first function

## Why this document exists

Document 16 established that the operator line printed one clock and this series read it as another, and document 17 added that the host cannot be trusted for wall time. What neither of them did was say where the suite's CPU goes, because until the clock was named there was no column that could be summed. This document sums it. It is the first breakdown in this series whose parts add up to the whole, and the thing it finds was not on anybody's list.

Everything below comes from `rudb --metrics` under `PRAGMA enable_profiling`, against the 10,000,000 row native database, with `operator.cpu_ns` rather than `operator.wall_ns`. That distinction is the whole reason the numbers close.

## The suite adds up

One pass of the 43 queries, every operator's CPU summed by kind:

| | CPU | share |
| --- | ---: | ---: |
| statement | 57.63 s | |
| pipelines | 57.61 s | 100% of statement |
| operators | 56.72 s | 98% of statement |
| driving the pipelines | 0.89 s | 2% of pipeline CPU |

| kind | CPU | share of statement |
| --- | ---: | ---: |
| Aggregate | 35.04 s | 60.8% |
| Scan | 11.11 s | 19.3% |
| Filter | 10.18 s | 17.7% |
| TopN | 0.36 s | 0.6% |
| Project | 0.02 s | 0.0% |
| TableFetch, Sort, Values, Limit | under 0.01 s | 0.0% |

Three operator kinds are 97.8% of the engine. The accounting closing to within 2% is worth stating on its own, because `crates/rudb-metrics/src/driver.rs` predicts that driving a pipeline can be a third of the execution on a query that moves ten thousand chunks, and at ClickBench chunk sizes it is 2%. That module's cross check is close to an identity here, exactly as its documentation says it should be.

One correction to how that first table should be read, since this series has now misread a column three times. `Aggregate` is not a synonym for grouping. When a query groups by an expression, the planner folds the expression into the aggregate rather than giving it a `Project` row, so the scalar work is charged to `Aggregate`. The 60.8% is grouping and the functions evaluated in group expressions together, and separating them is the rest of this document.

## One query is a third of the suite

| query | statement | Aggregate | Scan | Filter | rows into Aggregate | groups out |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Q29 | 20.16 s | 19.26 s | 0.59 s | 0.22 s | 8,682,923 | 15 |
| Q23 | 4.72 s | 0.19 s | 0.75 s | 3.76 s | 1,091 | 10 |
| Q17 | 4.36 s | 3.54 s | 0.77 s | 0.00 s | 10,000,000 | 640 |
| Q21 | 4.15 s | 0.01 s | 0.30 s | 3.80 s | 646 | 1 |
| Q33 | 3.02 s | 2.30 s | 0.67 s | 0.00 s | 10,000,000 | 640 |
| Q19 | 2.55 s | 1.94 s | 0.49 s | 0.00 s | 10,000,000 | 640 |

Q29 is 35% of the suite's CPU by itself and 55% of every Aggregate second in it. The top twelve queries are 85%. Document 16 said that any statement about this suite that is not a statement about its largest eight is a statement about the last quarter of it, and the operator numbers make that sharper: any statement about this suite that is not a statement about Q29 is a statement about two thirds of it.

Q29 groups by a `REGEXP_REPLACE` over `Referer` that extracts a hostname. At 2,218 nanoseconds per input row it is two orders of magnitude off what reading the column costs, so the rest of this document is about which two of the several things it does are responsible.

## What the cost is not

Six probes over the same 8,682,923 rows, each forcing its result to be consumed so the projection is not pruned away:

| expression | statement CPU |
| --- | ---: |
| `STRLEN(Referer)`, the baseline | 0.74 s |
| `UPPER(Referer)` | 14.75 s |
| `SUBSTRING(Referer, 1, 5)` | 15.72 s |
| `SUBSTRING(Referer, 1, 40)` | 20.40 s |
| `SUBSTRING(Referer, 1, 200)` | 30.30 s |
| `REGEXP_REPLACE(Referer, '^.*$', 'x')` | 41.25 s |

Reading the column and measuring it costs 0.74 seconds. Every expression that produces a string column costs between twenty and fifty times that, and the pattern across them rules out the two explanations that come to mind first.

It is not the size of the output. The regular expression in the last row returns one byte per row and is the slowest of the six, and `SUBSTRING(Referer, 1, 200)` returns whole strings and is slower than `SUBSTRING(Referer, 1, 5)` returning five characters of them. It is not the regular expression engine either, because `UPPER` and `SUBSTRING` do not have one and sit in the same range. What `SUBSTRING` does show is that its cost tracks the character count it was asked for rather than the bytes it returned, which is a per character loop over UTF-8 rather than a byte range taken whole, and that is a cost shared by every function here that walks its input.

## The dictionary ends at the first function

The measurement that matters is not any of those. It is what happens to the operator above them.

`Referer` has 2,719,020 distinct values in those 8,682,923 rows. Grouping on it directly, and grouping on the regular expression's output, with every operator's CPU shown, two passes each on a host whose load average read between 24 and 27 on eight cores throughout:

| group key | Scan | Project | Aggregate | statement |
| --- | ---: | ---: | ---: | ---: |
| `Referer`, a stored column | 0.38 s, 0.29 s | 0.02 s, 0.01 s | 0.28 s, 0.29 s | 0.78 s, 0.66 s |
| `REGEXP_REPLACE(Referer, ...)` | 0.53 s, 0.65 s | 6.28 s, 2.99 s | 4.41 s, 4.56 s | 11.29 s, 8.26 s |

Grouping 8,682,923 rows into 2,719,020 groups costs 0.28 seconds. That is document 09's stable global string codes working precisely as document 09 specifies: the group key is a dictionary code, so the hash table is a hash table of integers and the strings are never touched. High cardinality grouping on a stored string column is not a weakness of this engine. It is one of its best results, and no document in this series had measured it.

Put one function in front of that column and the statement costs twelve to fourteen times as much, and the increase lands in two places rather than one. `Project` goes from hundredths of a second to three or six, which is the function. `Aggregate` goes from 0.28 seconds to 4.5, which is nothing to do with the function: it is the same grouping as the row above, over the same number of rows, done on raw strings because the expression's output arrived without a dictionary. The scalar layer consumed a dictionary encoded column and produced a plain one, and the aggregate above it lost a sixteenfold advantage it had already earned.

## What the code already guessed, and the half it missed

`crates/rudb-kernels/src/regexp.rs` anticipates part of this in its own module documentation. It notes that a dictionary still runs the machine once per row rather than once per distinct value, calls that the obvious next thing to do, and then declines to do it on the grounds that it is worth a number before it is worth writing, because `Referer` may have too many distinct values for the dictionary form to pay.

That is the right discipline and the number is now available: 2,719,020 distinct in 8,682,923 is 3.19 to one, which is a real saving and a modest one, and on its own it would not obviously justify the work. The reason to do it anyway is the half the module did not consider, which is that the saving in the kernel is the smaller half. Applying the function per distinct value and emitting a dictionary column with the same codes does 3.19 times less matching, and it also hands the operator above an integer key instead of a string one, which the table above prices at 0.28 seconds against 4.5. The kernel's own estimate of its leverage was low because leverage from a vectorized kernel does not all appear inside the kernel.

This generalizes past Q29 and past regular expressions. It is a property of the seam between the scalar layer and the vector layer: a function whose output depends only on its input is constant on a dictionary entry, so for any such function over a dictionary column there is an output dictionary waiting to be built, and every operator downstream that can use a code rather than a value keeps its fast path. `GROUP BY` on an expression over a string column is the case ClickBench happens to contain. `DISTINCT`, joins, and sorts on the same shape are the same case.

## What it does not do is reach the target

The project's stated target is a tenfold margin, and the same breakdown that finds the lever above also says plainly that this class of work cannot deliver it.

The suite costs 57.63 seconds of CPU. A tenth of that is 5.76 seconds. The `Scan` operators alone cost 11.11 seconds, which is 19.3% of the suite and very nearly twice the entire budget a tenfold improvement would allow. Setting every operator above the scans to zero, on all 43 queries at once, leaves 5.19 times better and not ten, and that figure is not an estimate: it is one division, and it reproduces from a different direction the 5.16 to 6.46 range document 16 arrived at by summing scan shares across three passes.

So the ceiling is not a matter of how well the aggregate, the filter and the scalar layer are written. Even perfect, they leave a factor of two between the result and the goal, and the missing factor has to come out of the scan. Reaching the target requires reading less: fewer bytes off the device, fewer rows materialized, or an answer computed before the query arrived. That is a statement about storage layout and precomputation rather than about execution, and it is the first time in this series that it has been an arithmetic conclusion rather than an opinion.

What the dictionary finding is worth is the largest single item measured so far inside the execution budget, on the query that is a third of the suite. It is worth doing on those grounds. It is not worth presenting as progress toward ten.

## What this document does not claim

It claims nothing from wall time, and it should not be read as claiming precision from CPU time either. The load average on this host ran between 24 and 27 on eight cores while the last table was taken, and CPU time is not immune to that: threads sharing a core retire fewer instructions per cycle, so contention inflates CPU as well as wall, and the same probe measured 7.17 seconds in one run and 4.48 in another. Every conclusion here rests on a ratio between two measurements taken next to each other, and the ratios it rests on are twelvefold and sixteenfold rather than tens of percent.

It also does not claim the operator attribution is exact. `Aggregate` absorbs the scalar work folded into group expressions, which is why the section above had to take Q29 apart by rewriting it rather than by reading its plan, and the rewrite moves work between `Project` and `Aggregate` without changing the total. The figures that survive that are the statement totals and the comparison between two queries of the same shape.
