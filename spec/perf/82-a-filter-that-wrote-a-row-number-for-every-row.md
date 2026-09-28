# A filter that wrote a row number for every row

[`76-a-unit-unpacked-where-the-chunk-holds-it.md`](76-a-unit-unpacked-where-the-chunk-holds-it.md) ended with the first honest scoreboard this milestone has had against DuckDB on both suites, and with the conclusion that TPC-H is where the work is: ClickBench needed another 3.55 G of the 5.70 G taken out to reach ten times, TPC-H another 9.74 G of 11.25 G. So the next target came from a profile of all twenty two TPC-H queries in one process on one thread rather than from a single query, and it was not in the decode or the aggregate or the join. It was the runtime filter a join hands down to the scan beneath it.

`rudb_exec::sideways::Domain` is that filter. It is a bit per key value over the range the build side covers, so it is exact, has no false positives, and replaces the hash filter entirely for a join whose keys sit close together, which on TPC-H is most of them. Section 5.4 of [`../graph/05-execution.md`](../graph/05-execution.md) has what it is for. Asking it about a chunk was `Domain::keep`, and in the suite profile that one function was 17.00 percent of every instruction the twenty two queries ran, three times the next item, with all of it reached through `Scan::sift_exact`.

Two things were wrong with it, and neither was the bit test.

The first is that it wrote a `u32` for every row it looked at. The loop was written branchless on purpose, because a filter that keeps about half its rows mispredicts on every other row if the store is behind a condition, so the row number went down unconditionally and only the cursor moved:

```rust
kept.resize(rows, 0);
let mut at = 0;
for (row, &key) in block[..rows].iter().enumerate() {
    let offset = (key.wrapping_sub(base) as u64).min(self.range);
    let hit = self.words[(offset / 64) as usize] >> (offset % 64) & 1 == 1;
    kept[at] = row as u32;
    at += usize::from(hit);
}
kept.truncate(at);
```

That is the right shape for a loop that has to produce row numbers, and the profile agreed with it: no branch in the loop mispredicted. It is the wrong shape for a loop whose caller asks how many rows survived before it asks which ones, and sometimes never asks which ones at all. In the annotation of that loop the one store is 33.64 percent of the function's samples and the bit test beside it 30.88, so the row numbers cost about as much as the work they came from:

```
   33.64 :   767dad: mov    %r11d,(%r12,%rax,4)
   30.88 :   767db1: bt     %r14,%rdi
```

The second is that the keys were copied before they were read. `Domain::keep` took its keys through `Vector::signed_block`, which widens any vector body into a `Vec<i64>` so that one loop can serve every integer width and every layout. For a `BIGINT` key column, which is what `l_orderkey` and `o_orderkey` and every other TPC-H key is, that widening is `extend_from_slice` over the whole chunk: a pass that reads the column and writes it again somewhere else so that the next pass can read it a second time. `Vector::signed_block` and what it called was about one percent of the suite, and two of the three callers passed `&mut Vec::new()`, so the buffer was allocated and thrown away per chunk as well.

## A bitmap the caller can count without reading

`Domain::kept` returns the answer as bits:

```rust
pub(crate) struct Kept {
    bits: Vec<u64>,
    count: usize,
}
```

The row loop is a fold into a register. Sixty four rows make one word, the word is pushed once, and the count comes out of `count_ones` on it rather than being carried a row at a time:

```rust
fn marked(rows: usize, mut word_of: impl FnMut(usize, usize) -> u64) -> Kept {
    let mut bits = Vec::with_capacity(rows / 64 + 1);
    let mut count = 0;
    let mut row = 0;
    while row < rows {
        let end = (row + 64).min(rows);
        let word = word_of(row, end);
        count += word.count_ones() as usize;
        bits.push(word);
        row = end;
    }
    Kept { bits, count }
}
```

All three callers, the two in `Scan` and the one in the join's own narrowing of a build side, already asked how many rows survived before doing anything with them. The join keeps a chunk whole when more than half its rows survive, because a row that cannot match does no harm in a hash table, and the scan reports the count to the thing that decides whether the filter is still worth running. So `count()` answers both of those from a popcount, and `indices()` walks the mask and reads out the rows that are set only when the caller has decided to filter. Reading them out of a mask is a `trailing_zeros` and a `bits &= bits - 1` per kept row, and it does not appear in the profile after the change at all, which is the whole point: the old loop paid for every row it looked at, and this pays for the rows that survived.

## Keys read where they lie

With the store gone, the widening was the rest of it, and neither of the two layouts a key column actually arrives in needs it.

A column already laid out as integers is read in place, a width at a time, with `i64::from` on the value rather than a pass over the chunk to widen it first. For `BIGINT` there is no conversion at all.

A packed column is read on its codes. A code is the value less the frame's base and an offset into the bitmap is the value less the domain's base, so the two differ by a constant that is worked out once per chunk and folded into the offset:

```rust
if let Some(packed) = keys.packed_parts()
    && let Ok(frame) = i64::try_from(packed.base())
{
    let shift = frame.wrapping_sub(base) as u64;
    let mut codes = [0_u64; 64];
    return marked(rows, |from, to| {
        let block = &mut codes[..to - from];
        packed.unpack(from, block);
        block.iter().enumerate().fold(0, |word, (bit, &code)| {
            word | u64::from(self.bit(code.wrapping_add(shift))) << bit
        })
    });
}
```

The sixty four codes go into sixty four words of stack that the next block writes over, so the packed form has no allocation for the keys either, and it gets `unpack_block` on the way, which is the vectorized unpack of a block of codes rather than a code at a time.

The range test is the same trick it already was, moved into one place. A key under the base wraps round to an offset far past the range, a key past the range is moved onto the bit at the range itself, and `words_for` leaves room for that bit and nothing ever sets it, so one unsigned compare covers both ends of the range and it is a conditional move rather than a branch:

```rust
fn bit(&self, offset: u64) -> bool {
    let offset = offset.min(self.range);
    self.words[(offset / 64) as usize] >> (offset % 64) & 1 == 1
}
```

Anything that is neither of those two layouts, and any column with nulls in it, still goes through the widening form, which now also builds its answer as bits. A dictionary key, a constant key and a sequence key all land there, and none of them is on a TPC-H join.

## What it was worth

Counted at ring 3, one thread, one query per process, three rounds, minimum of rounds, with `SELECT 1` subtracted, and with both binaries built one after the other out of the tree at `df81f6d9` with the diff applied and reverse applied:

| TPC-H SF1 | before | after | |
| --- | --- | --- | --- |
| q17 | 383.9 M | 349.1 M | 0.909x |
| q19 | 344.2 M | 326.8 M | 0.950x |
| q08 | 419.9 M | 399.7 M | 0.952x |
| q20 | 488.9 M | 468.4 M | 0.958x |
| q04 | 320.6 M | 308.2 M | 0.961x |
| q21 | 1179.5 M | 1147.7 M | 0.973x |
| q11 | 64.8 M | 63.2 M | 0.975x |
| q07 | 535.1 M | 522.0 M | 0.976x |
| q10 | 550.0 M | 537.6 M | 0.977x |
| q03 | 503.0 M | 493.1 M | 0.980x |
| suite | 10.57 G | 10.36 G | 0.980x |

Nothing in the suite reads above 1.000x except q14 at 1.001x, which is under the threshold the harness flags as moved, seventeen of the twenty two read below it, and all twenty two answers are unchanged. Four of the five that do not move are the four with no join to push a bitmap through: q01, q06, q13 and q15.

ClickBench is 1.000x on all forty three queries with all forty three answers unchanged, which is the expected reading rather than a disappointing one. There is one table in ClickBench and therefore no join, so nothing in it ever builds a `Domain` at all. It was run to show that.

Against DuckDB v2.0.0-dev at SF1 on the same footing, TPC-H is now 15.09 G against 10.36 G, which is 0.686x, where note 76 measured 0.747x. rudb wins twenty one of the twenty two and loses q02 at 1.009x. The heaviest queries are where the gap is thinnest: q09 is 0.878x and q21 0.865x, and those two are 2.4 G of the 10.4 G.

## Measured twice, and the first reading is superseded

There is a first pass of this measurement that read 0.964x, on the tree note 76 shipped, with q17 at 0.857x and the suite going 11.28 G to 10.88 G. It is not the number above and it is not wrong either. Notes 77 to 81 landed while this change was in flight and took about 0.7 G out of the before side, most of it out of q09, q10, q11, q13, q15, q16 and q21, and the saving this change is worth went from 0.40 G to 0.21 G along with it. A ratio against a tree that is no longer the tree is not worth publishing, so the whole pair was rebuilt and re-measured, and the table above is the second pass.

The share of the suite the change removes moved with it. `Domain::keep` was 17.14 percent of the older tree and `Domain::kept` 13.37 percent of it, which is 3.8 points; here it is 17.00 against 14.77, which is 2.2. The function costs about what it did, so what changed is how much of what it does the callers can now answer from a count, and the callers changed underneath: a join that has already matched its rows hands them on rather than looking each one up again, so more of the chunks that reach the filter are chunks something above does go on to cut.

server2 filled its disk, with 117 MB free of 193 GB and other work still writing to it, so both passes were built and measured on server3 instead. That is the kind of change that invalidates a comparison if it is not checked, so it was checked on the first pass, whose before binary is exactly the tree note 76 shipped: it reads 11.28 G on server3 against the 11.25 G note 76 recorded on server2, which is three tenths of a percent apart, and DuckDB on the same suite reads 15.16 G here against 15.06 G there, which is seven tenths. Both boxes are AVX2 without AVX-512 and both binaries are built for `x86-64-v3`, so the agreement is what it should be.

The ClickBench totals do not agree across the two boxes and should not be read beside each other. server3's readable copy of the one million row `hits` corpus was written by a different binary than server2's and chose different encodings, so the suite is 8.08 G here where server2 reads 5.70 G for the same queries on the same rows. That is a statement about two files, not about two trees, and the only number worth taking from the ClickBench run is the ratio.

## What is left of the filter

`Domain::kept` is 14.77 percent of the suite where `Domain::keep` was 17.00 percent, on two profiles taken with one command on one box. `Vector::signed_block` went from 1.00 percent of the suite to 0.38, which is the residue of the columns that still widen.

Here is the whole packed loop that is left, eleven instructions a row:

```
    0.00 :   768280: mov    0xb0(%rsp,%rax,8),%rcx
    0.41 :   768288: add    0x8(%rsp),%rcx
    0.68 :   76828d: cmp    %rcx,%r13
    0.00 :   768290: cmovb  %r13,%rcx
    0.82 :   768294: mov    %rcx,%rdi
    2.18 :   768297: shr    $0x6,%rdi
    5.59 :   76829b: cmp    %r15,%rdi
    0.82 :   76829e: jae    768e9e
    0.00 :   7682a4: shrx   %rcx,(%r14,%rdi,8),%rcx
   20.60 :   7682aa: and    $0x1,%ecx
    2.86 :   7682ad: shlx   %rax,%rcx,%rcx
    8.05 :   7682b2: or     %rcx,%rbp
    7.23 :   7682b5: inc    %rax
    0.55 :   7682b8: cmp    %rax,%rbx
    1.36 :   7682bb: jne    768280
```

The first two lines are the code out of the stack block and the frame's base folded into the domain's, the `cmp` and `cmovb` are the range test, and there is no key in memory and no allocation anywhere in it. The `and` carries the skid of the bitmap load in front of it, which is the cache miss the filter exists to pay.

The `cmp` at `76829b` and the `jae` under it are the bounds check on `self.words[(offset / 64) as usize]`, and they are there even though `offset` has just been clamped to `self.range` and `words_for` sized the vector at `range / 64 + 1`, so the index cannot be out of bounds. The compiler does not see that through a field of `self`. They are 6.4 percent of the function's samples in this copy of the loop and about the same in the flat copy beside it, so cutting the words to exactly that length once and handing the loop the slice is worth a build.

Past that, the filter is a bit per row over a bitmap that is too big for the second level cache on the wider joins, and what the profile calls a bit test is mostly a memory stall. Making it smaller, rather than making the loop around it shorter, is where the next real factor is.
