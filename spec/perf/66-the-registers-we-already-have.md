# The registers we already have

Every note before this one is a change to the engine. This one is a compiler flag, and it is worth more than all of them put together.

Chasing q01's profile past the fold, `Prepared::run_step` was 9.2 percent of the query and its hot loops disassembled to `paddq`, `psubq`, `pmuludq` and `movdqu`, stepping four `i64` an iteration. Those are SSE2 instructions on 128 bit `xmm` registers. server2 is an AMD EPYC with AVX2. The 14.4 percent of the query sitting on a single `pmuludq` is a 64 bit multiply emulated out of 32 bit partial products, which is how LLVM lowers an `i64` multiply when it has nothing wider to use, and that multiply is q01's `l_extendedprice * (1 - l_discount) * (1 + l_tax)`.

The whole engine was compiled for baseline x86-64, which is SSE2 and nothing after it. There was no `target-cpu` or `target-feature` anywhere: not in `.cargo/config.toml`, not in the workspace manifest, not in a crate manifest. `spec/07-execution.md` section 7.3 asked for the opposite, runtime dispatch through `is_x86_feature_detected!` with AVX-512, AVX2, SSE4.2 and NEON behind it, and there were zero uses of that macro in the workspace. The section was specified and never built, and in the meantime nothing was using anything.

## What it is worth

Two binaries from the same commit, differing only in `RUSTFLAGS`, on server2 at SF1, one thread, three rounds, median, with the `SELECT 1` baseline of 38.9 M subtracted. All 22 answers unchanged.

| query | baseline | x86-64-v3 | ratio |
| --- | --- | --- | --- |
| q01 | 1619.4M | 1210.8M | 0.748x |
| q02 | 150.8M | 135.2M | 0.897x |
| q03 | 648.0M | 576.1M | 0.889x |
| q04 | 415.4M | 369.3M | 0.889x |
| q05 | 821.6M | 730.7M | 0.889x |
| q06 | 297.5M | 212.0M | 0.713x |
| q07 | 676.7M | 595.4M | 0.880x |
| q08 | 533.8M | 470.8M | 0.882x |
| q09 | 2070.7M | 1829.3M | 0.883x |
| q10 | 889.7M | 805.5M | 0.905x |
| q11 | 105.4M | 97.2M | 0.922x |
| q12 | 672.7M | 566.8M | 0.843x |
| q13 | 1060.2M | 1016.6M | 0.959x |
| q14 | 278.1M | 221.3M | 0.796x |
| q15 | 243.7M | 187.5M | 0.769x |
| q16 | 307.5M | 281.9M | 0.917x |
| q17 | 494.6M | 434.7M | 0.879x |
| q18 | 938.0M | 889.6M | 0.948x |
| q19 | 506.8M | 390.8M | 0.771x |
| q20 | 566.7M | 525.3M | 0.927x |
| q21 | 1512.2M | 1340.8M | 0.887x |
| q22 | 313.5M | 300.0M | 0.957x |
| suite | 15.12 G | 13.19 G | 0.872x |

Every query wins. The suite retires 12.8 percent fewer instructions and q01 25.2 percent fewer, and the worst query in the table still wins 4 percent. For scale, three changes merged on the day this was found were 0.965x, flat, and 0.991x on q01, and each of them took a profile, a rewrite and a measurement. This is a line in a config file, it is worth thirty times any of them, and it is worth it on all twenty two queries rather than on the one somebody was looking at.

An earlier run of the same comparison included `target-cpu=native`, which lost to `x86-64-v3` on every single query on this box. That is worth recording because it is the opposite of what anyone would guess. Nothing in the win depends on a feature only server2 has, so there is no argument here for a per host build.

## Why the baseline and not dispatch

`spec/19-open-questions.md` Q7 has the long version. The short version is that dispatch was the default answer when the prize was assumed to be a few kernels, and an indirect call per kernel is cheap against that. The prize turned out to be the arithmetic the whole engine is built out of, which no dispatch table reaches, because nobody is going to write two versions of every decimal multiply and every offset calculation in the tree. A raised baseline gets all of it with no call and no lost inlining.

What it costs is a floor. `x86-64-v3` is AVX2, BMI2 and FMA, so Haswell on Intel and Zen on AMD, which is hardware from 2013 and 2017, and an older host gets an illegal instruction rather than a slow query. For something people link into their own programs that floor is part of the product, so it is written down where somebody installing will read it, in `README.md`, along with the `RUSTFLAGS` that gives the portable build back.

Dispatch keeps the job a baseline cannot do, which is AVX-512 and specifically `VBMI2` for bit unpacking. That hardware is not general enough to compile for, bit unpacking is hot enough to pay for a call, and it is one kernel family rather than the whole engine.

Two things checked rather than assumed, because FMA is the part of `v3` that could quietly change an answer. LLVM did not contract a single float expression: the v3 binary has four fused multiply add instructions against the baseline's two, and the two it gained are the workspace's only two explicit `f64::mul_add` calls, both in `compact.rs`, which are a single rounding by definition in either build and so cannot differ. And the binary really is built the way this note claims, which is 96,562 references to a `%ymm` register against 61 in the baseline build.

## The instrument

This showed up in instructions retired, the counter this directory leads with, and it showed up enormously, and nobody saw it for the whole life of the project. The reason is structural and worth keeping: every measurement in `spec/perf` is a ratio between two binaries built the same way, so a constant factor sitting under both of them cancels out of all of them. A ratio cannot see what it divides by. The engine had never been compiled for anything past 2003, and no number in this directory was capable of saying so.

## Where the flag lives

`.cargo/config.toml`, under `[target.'cfg(target_arch = "x86_64")']`, so an x86-64 target nobody has added yet is covered and the two arm targets in the release matrix are not.

The trap, which is the only hard part of this change: a `RUSTFLAGS` in the environment replaces the rustflags from a config file rather than adding to them. This repository sets one in four places, and a config only change would have shipped AVX2 while every test, gate and benchmark ran on SSE2, which is the one shape of mistake that leaves a green gate over an engine nobody runs. So CI writes the flag alongside its `-D warnings`, with the arm row of the test matrix leaving it out, and the gate passes its deny through `cargo --config` instead of the variable, because Cargo joins the rustflags of every `target.<cfg>` table that matches and `cfg(all())` matches everything. Verified on Rust 1.85, which is the MSRV, and on 1.98.
