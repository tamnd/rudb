# One loop for five totals

This is the loop [`57-no-branch-a-row-in-the-cut.md`](57-no-branch-a-row-in-the-cut.md) left behind. Note 57 cut a chunk into runs of one group without branching a row, and then handed those runs to a walk that visited each of them once per aggregate call. On q01 there are five calls over one layout and the runs average 2.81 rows, so the walk set up five loops, stepped five counters and tested five bounds to do fourteen adds.

Turning the two loops inside out is the whole change. The row loop is outside and the call loop is inside, so the counter is stepped once for all five calls and the adds between two steps of it are straight line code, because `W` is a constant and a loop over `W` calls is not a loop. Per row it went from twenty five instructions to thirteen.

```
add (%rbx,%r10,8),%r8        five adds, one per call, each into a register
jo                           the total held in that register for the whole run
add (%r14,%r10,8),%rsi
jo
add (%r15,%r10,8),%rcx
jo
add 0x0(%r13,%r10,8),%r12
jo
add 0x0(%rbp,%r10,8),%r9
jo
add $0x1,%r10                one step of the counter for all five
cmp %r10,%rax
jne
```

The bounds checks moved out with it. The old loop asked each call's column for the slice its run covered, which is one check a call a run, and then indexed inside that slice, which is free. The new one still asks each column for its slice, one check a call a run as before, and then reads all five slices at one row index, which the compiler drops the checks on because every slice was cut to the run's length and the loop's bound is that same length. So the row loop holds no check of anything. Getting that required saying the length once, as `end.checked_sub(from)`, rather than letting the subtraction happen in three places and hoping the compiler saw they agreed.

## What it measured

q01 at SF1 on one thread, on server2, against main:

| | before | after | |
| --- | --- | --- | --- |
| instructions | 1222.5 M | 1153.7 M | 0.944x |
| cycles | 642.4 M | 553.4 M | 0.861x |
| branches | 204.7 M | 181.1 M | 23.7 M fewer |
| branch misses | 4.11 M | 3.74 M | 0.37 M fewer |

Over the suite it is 0.994x with no other query moving at all, which is what a change to one kernel that only q01 reaches five calls wide should look like. The branch count is the clearest reading of what happened: 23.7 M fewer branches over 5.9 M rows is four fewer a row, which is the four loop tests that are gone.

Cycles moved further than instructions, 0.861x against 0.944x, and the instructions are only half the reason. IPC went from 1.90 to 2.09. A loop of 2.81 iterations spends most of itself in its own overhead, and five of them a run is five loop exits for the front end to predict where one would do.

## What is still there

The `jo` after every add. Each one is an overflow check on a total that cannot overflow, because q01's columns are `DECIMAL(15, 2)` and a chunk holds a few thousand rows, so ten to the fifteen times the chunk's rows is nowhere near what an `i64` holds. Proving that wants the logical type's precision carried into the walk, which is the same argument [`spec/perf/57`](57-no-branch-a-row-in-the-cut.md)'s sibling made about decimal chains, and it is worth another five instructions a row here. It is not done yet because it needs the bound plumbed from `shareable` through to `many_runs` and a second instantiation of the walk, and this change is worth landing on its own first.

That paragraph is wrong about q01 and [`70-the-check-that-could-not-come-out.md`](70-the-check-that-could-not-come-out.md) is the correction. Three of the five columns on q01's pass are `DECIMAL(15, 2)` and the other two are the chains the query computes, which the multiplication rule declares `DECIMAL(18, 4)` and `DECIMAL(18, 6)`, and ten to the eighteen times any row count above nine leaves an `i64`. One row loop serves every call on the pass, so the two unbounded columns decide it for all five. The bound was built and measured and reads 1.000x over both suites.

The wide fallback, `walk_any`, is untouched and still visits a run once per call. A query with nine folding aggregates over one layout in one `GROUP BY` goes there and none of the 22 does.
