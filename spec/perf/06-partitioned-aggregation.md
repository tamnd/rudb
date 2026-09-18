# Three ways to partition a hash aggregate, and which one the numbers picked

Item 8 of the roadmap is #510, and three designs for it were built and measured. Writing down what each one cost, because the one that was fastest is not the one that landed and the reason is worth keeping.

## The three

**Merge partitioning**, which was #520. Every instance keeps one table over every group, the same as before. At combine, the arriving table is scattered into sixty four shared partitions by the top bits of each group's hash, so two instances can merge at the same time as long as they are in different partitions. The fold is untouched, which is why this was tried first: `Aggregate::fold` and `update_scattered` in rudb-kernels never see it.

**Fold partitioning into shared tables**, which is what #521 and #522 did and what is on main. There is no per instance table at all. A chunk is hashed once, split by the top four bits, and each part is gathered into its own vectors and folded into one of sixteen tables that every instance shares behind a mutex.

**Fold partitioning into local tables**, which is what DuckDB does and what nobody built. Every instance keeps sixteen tables of its own, so the fold takes no locks, and partition p of every instance is merged into partition p of the answer at the end.

## What they measured

Three revisions, gamingpc-wsl, thirty two threads, ClickBench, three runs per query, run ABCABC so no revision always has a warm machine. `cf2fd1e` is main before any of this.

At ten million rows:

| revision | query time | hot cpu | peak RSS |
| --- | --- | --- | --- |
| cf2fd1e | 13.390s / 13.449s | 84.300s / 83.950s | 2.18 GiB / 2.17 GiB |
| fold, shared (#521, #522) | 10.149s / 10.035s | 93.110s / 93.200s | 1.43 GiB / 1.42 GiB |
| merge (#520) | 9.433s / 9.484s | 91.950s / 92.130s | 2.41 GiB / 2.35 GiB |
| fold, shared, swept (#525) | 9.133s / 9.123s | 87.420s / 87.080s | 1.38 GiB / 1.43 GiB |

At one million rows:

| revision | query time | hot cpu | peak RSS |
| --- | --- | --- | --- |
| cf2fd1e | 1.993s / 2.112s | 5.730s / 5.990s | 281.95 MiB / 277.84 MiB |
| fold, shared (#521, #522) | 1.863s / 1.841s | 6.090s / 6.030s | 280.88 MiB / 281.77 MiB |
| merge (#520) | 1.976s / 2.177s | 6.250s / 7.230s | 281.84 MiB / 281.24 MiB |
| fold, shared, swept (#525) | 1.741s / 1.730s | 5.650s / 5.650s | 280.55 MiB / 283.60 MiB |

## Why the slower one won

Merge partitioning is 6.5 percent faster at ten million rows and uses 65 percent more memory. Both numbers come from the same fact: it still gives every instance a table over every group. Thirty two tables is thirty two copies of the group space, and the scatter then builds the partitions beside them, so the peak is worse than what it started from. It went the wrong way on memory to buy a little time, and the goal here is ten times faster and ten times less resource, not one of the two.

Fold partitioning holds one set of groups for the whole query. That is the 2.18 GiB to 1.43 GiB, and it is the single largest memory win the aggregate has had.

It also unblocks spilling under threads for free. The refusal in `Aggregate::merge` was there because a key could be in one instance's table and in another instance's spill file at once. A partition's spill file only ever holds keys that hash to that partition, so the key is either finished in the partition or absent from it, which is the invariant spilling rested on all along.

## What it cost, and the part that is still open

CPU went from 84s to 93s. Two causes. Gathering each partition's rows out of the chunk is a copy of every key, argument and filter column, which is one full extra copy of the input per chunk. And sixteen mutexes each held across a whole fold put a ceiling of sixteen on a thirty two thread machine, so half the threads are waiting rather than working.

The gather is the price of keeping the existing column kernels, and it is not obviously removable: `update_scattered` wants a dense array of slots, which is the same obstacle that made merge partitioning the easier thing to build in the first place. It is #527.

The lock ceiling was addressable without giving up the shared tables, and #525 did it. Every instance walked the partitions in the order zero to fifteen, so thirty two threads queued on partition zero, then queued on partition one behind whoever won the first. Starting each chunk one partition further along, and putting aside a partition that is already busy rather than waiting for it, turned that queue into a sweep. It gave back 1.07x on time and half the CPU regression, on no more memory.

Raising the partition count was the obvious thing to try alongside it and the answer was no. Sixty four partitions ran at the same speed as sixteen and peaked at 2.07 GiB against 1.43, because sixty four tables each round their own capacity up where sixteen do it four times less often. The count stays at sixteen.

What is left of the CPU regression is the gather, and it is a real piece of work rather than a knob.

## The rule this leaves

Measure memory in the same run as time, and refuse to read one without the other. #520 was measured first on time alone and looked like a clear win at 1.45x. It was a clear loss. The three way run took forty minutes and changed the decision completely.
