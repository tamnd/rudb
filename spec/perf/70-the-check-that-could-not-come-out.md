# The check that could not come out

[`69-one-loop-for-five-totals.md`](69-one-loop-for-five-totals.md) ends by naming what is left in the row loop it had just written: an overflow check after every add, five of them a row on q01 out of the thirteen instructions a row costs. It says the check cannot fire, because q01's columns are `DECIMAL(15, 2)` and a chunk holds a few thousand rows, and it says the fix is to carry the width in from the logical type and is worth another five instructions a row. This note is what happened when that was tried. The bound is real and it is cheap to work out. It does not hold on the one pass in either benchmark that would spend it, and the change measures 1.000x over both suites, so it is not in the tree.

## The bound

A total of `rows` values of a type is held by an `i64` if the largest magnitude the type has times `rows` is. That is one multiplication per call per chunk, asked once in the same place that already asks every other question about a call, and the answer is a `bool` the walk carries. A `TINYINT` is under 128, an `INTEGER` is under two to the thirty one, a `DECIMAL(w, s)` is under ten to the `w`, and a `BIGINT` or a `HUGEINT` has no bound worth having because two values of one can leave an `i64` between them.

That much was right. What was wrong was the arithmetic note 69 did in its head. q01 sums two columns the query computes rather than reads, and the engine declares them the way the multiplication rule says to:

```
SELECT typeof(l_extendedprice * (1 - l_discount)),
       typeof(l_extendedprice * (1 - l_discount) * (1 + l_tax)) FROM lineitem LIMIT 1;

DECIMAL(18,4)   DECIMAL(18,6)
```

Ten to the eighteen times nine is just inside an `i64` and ten to the eighteen times ten is outside it, so a chunk of a few thousand rows of either of those columns has no bound at all. The declared width is the rule's worst case and not the data's: `l_extendedprice` never exceeds seven figures, so the real values are around ten to the eleven and nine of them would be nowhere near anything. Nothing in the vector says so. The type is what the walk has to go on and the type says eighteen digits.

The pass is all or nothing because one row loop serves every call on it, so q01's three stored `DECIMAL(15, 2)` columns, which are bounded with room to spare, are on a pass with two that are not, and the pass adds the checked way. Splitting it would cost a second walk of the runs, and a second walk of the runs is the thing [`57-no-branch-a-row-in-the-cut.md`](57-no-branch-a-row-in-the-cut.md) and the two notes after it spent their effort removing.

## What it measured

Flat. TPC-H at SF1 on one thread is 12.13 G either side with no query moving by more than 0.1 percent, and ClickBench over a million rows is 5.69 G either side with all 43 answers unchanged and nothing above 1.001x. The baseline is main at `ef3b60b0` and both sides are the same build of everything else.

## What it cost to write the branch four ways

This is the part worth keeping. The change is one `bool` and two forms of one add, and there are four ways to write that, three of which cost more than the check does.

| | q01 | |
| --- | --- | --- |
| the walk as note 69 left it | 1120.6 M | |
| the row loop in a function of its own taking a `bool`, `#[inline]` | 1326.6 M | 1.184x |
| the same, `#[inline(always)]`, with the run passed by reference | 1245.9 M | 1.112x |
| the two loops written out in the walk itself | 1235.7 M | 1.103x |
| the bound a constant, so one loop is compiled and the other is not | 1120.8 M | 1.000x |

The first two rows are a total behind a reference. The walk's whole point is that the group's totals live in registers for the length of a run, which is what makes the add `add mem,reg` and not a load, an add and a store. Handing them to a function as `&mut [i64; W]` gives that up, and forcing the inline does not give it back: the compiler had already decided where those totals live. That is 125 M instructions on q01 for a function boundary in a loop that runs six million times.

The third row is the surprise. The two loops written out one after the other in the walk, the totals a local array again, still costs 115 M. Both nests are in the one function whether or not a given chunk runs them, and two nests to unroll is past what the compiler will do, so the loop over the `W` calls goes back to being a loop with a counter and a bound. That is note 69's win undone and then some, for a change meant to save 30 M.

The fourth row is a `const BOUNDED: bool` on the walk, which doubles the instantiations, eight widths to sixteen for each of thirteen layouts, and compiles exactly one of the two nests in each. It reads 1.000x, which is what a mechanism that never fires should read, and is the only one of the four that is a fair test of the idea at all. The other three were measuring how the branch was written.

## The other thing this turned up

The shared run walk is reached by q01 and by nothing else in either suite. `update_shared_runs` needs two calls that can share a layout, and before that it needs the group count to be at most 256 with at least four rows a group, because past that the locals cost more to clear than the rows they save. q01 groups by `l_returnflag` and `l_linestatus` and has four groups. Every grouped ClickBench query has far more, and the flat ClickBench numbers above are not the bound failing there, they are the walk never running there.

So this kernel is a q01 kernel, and what is left in it is 77 M of q01's 1120 M, which is the whole row loop and not just the check. Note 69's win was real and worth having. A third note about the same thirteen instructions is not the best use of the next day, and [`68-the-counter-that-was-not-load-immune.md`](68-the-counter-that-was-not-load-immune.md)'s profile of q01 says where to go instead: `unpack_block` is 9.8 percent of the query and is reached from five different call sites, so it is the same size of prize with most of the suite behind it rather than one query.

## If someone comes back to this

There is a version of the bound that would hold. Sum a run into a total that starts at zero rather than at the group's running total, and fold it into the group's cell with one checked add per call per run at the end. Then the row loop needs only the longest run times the magnitude to fit, not the whole chunk, and q01's runs of one group average 2.81 rows. That turns five checks a row into five checks a run, which is about two thirds of them, worth something like 19 M on q01. It needs a run longer than the magnitude allows to be walked in pieces, and it adds a load and a store per call per run to pay for the ones it removes, so the arithmetic is closer than it looks. Measure it before writing it, which is what this note did not do.

The one thing that did land from all of this is a test. `a_column_at_the_widest_its_type_holds_totals_the_way_the_scatter_does` puts every row of a chunk at the largest value its type holds, every row in one group, and checks the shared pass against the scatter, and then does the same with a `BIGINT` column of `i64::MAX` where each value fits and the total does not. That second miss is one of the two `many_runs` documents and nothing had been covering it.
