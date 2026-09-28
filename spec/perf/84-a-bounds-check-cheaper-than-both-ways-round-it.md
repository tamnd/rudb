# A bounds check cheaper than both ways round it

[`82-a-filter-that-wrote-a-row-number-for-every-row.md`](82-a-filter-that-wrote-a-row-number-for-every-row.md) left the runtime bitmap's row loop at eleven instructions a row and named the next thing to try. Two of the eleven are the bounds check on the read:

```rust
fn bit(&self, offset: u64) -> bool {
    let offset = offset.min(self.range);
    self.words[(offset / 64) as usize] >> (offset % 64) & 1 == 1
}
```

The index cannot be out of bounds. The line above it clamps the offset to `self.range` and `words_for` sized the vector at `range / 64 + 1`, so the largest index the loop can produce is the last word there is. The compiler emits the check anyway, and the note said that cutting the words to that length once and handing the loop the slice was worth a build.

It was worth a build. Two builds, in fact, one for each way of doing it, and neither is in the tree. This note is what they cost, because the next person to read the annotation will see the same two instructions and have the same idea.

## Cutting the slice does not fold the check

The first way is to keep the clamp and give the loop a slice whose length is the bound the clamp implies:

```rust
struct Bits<'a> {
    words: &'a [u64],
    range: u64,
}

fn bits(&self) -> Bits<'_> {
    Bits { words: &self.words[..=(self.range / 64) as usize], range: self.range }
}
```

For the check to fold the compiler has to see that `umin(offset, range) >> 6` is below `(range >> 6) + 1`. Both sides are `range`, the clamp is a `umin` against it and the length is a shift of it plus one, and one bounds check away from each other is as close as they ever get. It does not fold. The loop keeps the conditional move of the clamp and then compares against the length as before:

```
	cmp	x19, x9
	csel	x9, x19, x9, lo
	lsr	x10, x9, #6
	cmp	x10, x21
	b.ls	LBB2047_28
```

So that way is today's loop with a slice in front of it. It was read out of the generated code and not measured, because there is nothing in it to measure.

## Making the bounds check the range test is slower

The second way gives up the clamp. Nothing outside the range is ever set in the bitmap, so a key past the range reads a word that exists and finds a bit that is not set, and a key under the base wraps round to an offset past every word there is and finds no word at all. One unsigned compare against the length covers both ends of the range and the read:

```rust
fn at(self, offset: u64) -> bool {
    let word = self.words.get((offset / 64) as usize).copied().unwrap_or(0);
    word >> (offset % 64) & 1 == 1
}
```

That is a compare and a conditional move fewer than the clamp and the check together, it needs nothing of the compiler, and on x86-64 the whole function loses five of its seven bounds checks.

Counted at ring 3, one thread, one query per process, three rounds, minimum of rounds, `SELECT 1` subtracted, with both binaries built one after the other out of one tree with the diff applied and reverse applied, it is slower on every query in the suite:

| TPC-H SF1 | before | after | |
| --- | --- | --- | --- |
| q04 | 308.2 M | 312.2 M | 1.013x |
| q02 | 113.7 M | 114.8 M | 1.009x |
| q03 | 493.2 M | 497.4 M | 1.009x |
| q05 | 614.6 M | 619.3 M | 1.008x |
| q10 | 537.6 M | 541.8 M | 1.008x |
| q11 | 63.2 M | 63.8 M | 1.008x |
| q21 | 1147.8 M | 1157.1 M | 1.008x |
| suite | 10.36 G | 10.40 G | 1.004x |

All twenty two answers are unchanged and nothing reads below 1.000x. It was not run on ClickBench, because note 82 established that a suite with one table never builds one of these at all.

## What the two instructions were paying for

The read the check guards is one instruction:

```
	shrx   %rcx,(%r14,%rdi,8),%rcx
```

The word is loaded and shifted by the bit's offset in the same instruction, and the load is only allowed to be folded in because it always happens. As soon as the load can be skipped it has to stand on its own, the shift needs a second entry for the case where the word is a zero the loop made up, and the row loop comes out of the compiler in three blocks with an unconditional jump joining them. That jump is 13.43 percent of the function's samples, which is more than anything else in it except the shift the miss lands on:

```
   13.43 :   771ae5: jmp    771af2
    9.46 :   771a94: shrx   %rdx,%rdi,%rdx
```

The compare and the branch this was meant to remove are the cheapest pair in the loop. Both operands are already in registers, the branch is never taken, and a predicted not taken branch on a core wide enough to retire four instructions a cycle is close to free. What they were buying is a load folded into a shift, and losing the fold costs more than the pair. An instruction count says eleven against nine and is wrong about which is faster, which is the same lesson as note 72's and #1936's in a smaller space: a count is a proxy, and the place it breaks is where the instructions are not the same size.

## Where the loop's cost actually is

In the annotation of the shipped loop the `and` after the load carries 20.60 percent of the function on its own, and it carries it because a non-precise `instructions:u` sample skids onto the instruction after the load that missed. That is the cost, and it is not an instruction count at all.

The bitmap is as big as the spread of the build side's keys. At SF1 `orders` holds 1,500,000 rows whose `o_orderkey` is spread over 6,000,000 values, so a domain a join builds over it is 6,000,000 bits, which is 750 KB, and the loop walks it in whatever order `lineitem` arrives in. It does not fit the second level cache on this box, and no arrangement of eleven instructions around a miss to it is worth a percent of anything. The next thing to try is a smaller bitmap, or one read in an order that misses less, and this note is here to say that the instructions around the miss have been tried twice.
