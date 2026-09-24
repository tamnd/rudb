# Correctness

How we know the compiled engine returns what DuckDB returns, on every tier, on every morsel, after every switch. This is the document that decides whether a faster number may be published at all.

The standard is set by document 00, settled decision 4: same rows, same types, same errors, same NULL behavior, checked differentially and never assumed. The compiled engine adds three kinds of failure that the first engine never had. A backend can miscompile. A tier switch can happen in the middle of a pipeline and lose or duplicate state. A code cache can keep serving a wrong answer long after the bug that produced it has been fixed. Each of these gets its own test, and each test runs on every commit.

The external evidence for taking this seriously is short and unpleasant. ClickHouse issue #118334 `[snippet]` (https://github.com/ClickHouse/ClickHouse/issues/118334) describes JIT-compiled `least`, `greatest`, `midpoint` and `bitShiftRight` on `Int128` using unsigned semantics. The results were correct for the first three executions and wrong from the fourth, when the expression crossed the compile threshold, and after that the wrong code was served from the compiled-expression cache. A separate fix in the same area stopped JIT compilation of range-checked conversions, which wrapped silently where the interpreter raised an error. Both bugs are invisible to a test that runs a query once. Photon's authors, who chose not to compile, wrote that "a majority of the work… was around adding tooling and observability" (https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf). A compiled engine needs more of that work, not less.

## 15.1 Three oracles, and what each one decides

**DuckDB decides what the right answer is.** For results, result types, error class and error presence, the pinned DuckDB release is the definition. That is the rule `rudb-compat` already enforces for the first engine. It renders values the way DuckDB's `result_helper.cpp` does and compares with DuckDB's sqllogictest rules. Nothing in this document changes it.

**The first engine is the second oracle.** It is faster to run than DuckDB inside the same process, it shares the frontend, and it has been through `rudb-compat` for longer. When the compiled engine and DuckDB disagree, the first engine says which side of the frontend boundary the bug is on. If the first engine agrees with the compiled engine, the bug is at or above A4, which the two engines share. If it agrees with DuckDB, the bug is in the compiler.

**`interp` is the tier reference.** Every backend must produce the same bits as `interp` for the same QIR function (document 03, section 3.8). `interp` is the simplest backend and the one whose semantics are written in Rust, so when `direct` and `interp` disagree, `interp` is presumed right. If `interp` itself is wrong, that shows up as disagreement with the first two oracles.

| oracle | decides | compared at | tolerance |
|---|---|---|---|
| DuckDB (pinned) | rows, types, errors, NULLs | rendered A9 | sqllogictest rules, float to DuckDB's printed precision, row order only under `ORDER BY` |
| first engine | which side of A4 the bug is on | rendered A9 | same as DuckDB |
| `interp` | whether a backend or switch broke it | raw A9 chunks and status codes | **bit-identical**, including float bits, NULL masks and error codes |

The third row is much stricter than the first two, and that is the point of it. A backend that turns `a*b+c` into a fused multiply-add passes against DuckDB whenever the last bit is lost in printing. It fails against `interp` on the first input where the bit differs. Tier-against-tier testing sees bugs that end-to-end testing can only see by luck.

## 15.2 The differential harness

**`rudb-compat` learns one new flag: `--engine vectorized|compiled|auto`.** It sets `SET engine = ...` on every connection it opens. Under `compiled`, a refusal is an error (document 03, section 3.4), and the harness records it as a *refusal*, not as a failure. Refusals are counted and published per corpus, and they never count as passes. This is new work in `rudb-compat/src/rudb.rs` and `src/suite.rs`.

**Every corpus the harness already runs is run again under the compiled engine.** That means the DuckDB sqllogictest corpus at v2.0-cyanoptera (4,106 files, 24,237 passing today on the first engine), `corpus/m0.sql`, `corpus/dialect.sql`, both ClickBench corpora, and the benchmark queries under `corpus/bench`. Two numbers are published per corpus:

- **the compiled pass count**, which must never be below the first engine's pass count on the same corpus, and
- **the compiled-only failures**, the set of cases that pass on the first engine and fail on the compiled one, which must be empty to merge.

The second number is the one that gates. The compiled engine inherits every first-engine failure, and fixing those is not its job. Adding a new one is a merge blocker.

**Root-causing reuses the existing machinery.** `scripts/rootcause` charges failures to the first failing statement per file. `src/bisect.rs` and `src/reduce.rs` shrink a failing case. `src/isolate.rs` separates a statement from its setup. The compiled engine adds one more axis to bisect along, tier and pass, which document 16 section 16.8 specifies.

## 15.3 Tier against tier

**Every query in every corpus runs once per tier, with the tier forced.** Three new debug settings exist in all builds, because a setting that only exists in debug builds tests a different binary from the one we ship:

```
SET qc_tier   = 'auto' | 'interp' | 'direct' | 'clif' | 'llvm';
SET qc_switch = 'off' | 'random:<seed>' | 'every:<n>';
SET qc_guard  = 'off' | 'fail:<seed>:<p>';
```

`qc_tier` pins every pipeline to one backend and turns off background recompilation. `clif` sits behind the non-default cargo feature `qc-clif`, and `llvm` behind its own feature. CI and every gate build with `qc-clif` on, so the tier matrix always includes `clif`. Asking for a tier that was not compiled in is an error, never a silent fallback. `qc_switch = 'random:<seed>'` makes the tiering policy (document 09) switch tiers at morsel boundaries at pseudo-random points drawn from the seed, including switching back down, which the production policy never does. `every:<n>` switches after every n morsels, cycling through the available backends. `qc_guard` is section 15.9.

**The comparison is bit-exact, single-threaded.** At `threads = 1` the morsel order is fixed, so A9 from any tier and any switch schedule must be bit-identical to A9 from `qc_tier = 'interp'`: the same chunks, rows, float bits, NULL masks, status codes and error-slot contents. At `threads > 1` the comparison falls back to a multiset of rows with bit-identical values, and floating-point aggregates are excluded from the bit rule, because the merge order of partial sums depends on scheduling in the first engine and in DuckDB as well. **The determinism rule holds per QIR function, and parallel float reduction order is not covered by it.** Any other doc that promises more has to say how it gets deterministic merge order.

**A switch test is only meaningful if it lands inside state.** A random switch is cheap to make useless: if every switch lands between pipelines, nothing is tested. The switch generator therefore biases towards the three places where state is live across morsels:

- inside a hash-table build before the table is finalized,
- inside an aggregation after the first partial group has been written, and
- inside a top-N heap.

It logs the switch points it actually hit, and a nightly run that did not hit all three kinds in some query is a harness failure.

**Each query runs five times per configuration, and the fifth run goes through the code cache.** This is the ClickHouse lesson. The first run compiles. Later runs reuse cached code and would expose a cache key that leaves out something the code depends on, such as a fact, a setting or a type width. The harness checks that runs two through five report `cache = hit` in `EXPLAIN ANALYZE` (document 16, section 16.6), so it is certain the cached path was taken.

The matrix per query:

| run | tier | switch | guard | threads | compares against |
|---|---|---|---|---|---|
| 1 | interp | off | off | 1 | first engine, DuckDB |
| 2 | direct | off | off | 1 | run 1, bit-exact |
| 3 | clif | off | off | 1 | run 1, bit-exact |
| 4 | auto | random | off | 1 | run 1, bit-exact |
| 5 | auto | random | fail | 1 | run 1, bit-exact |
| 6 | auto | off | off | all | run 1, multiset |
| 7 | llvm | off | off | 1 | run 1, bit-exact (nightly, feature builds) |

## 15.4 The floating-point rule, enforced rather than hoped for

**No backend may reassociate, contract, or change rounding.** Document 03 states the rule. The ways it gets broken in practice are specific, so each one has a specific check.

1. **Fused multiply-add.** `direct` has no FMA instruction in its encoder tables at all. `clif` never sees one, because QIR has no fused operation and Cranelift does not fuse on its own. `llvm` sets no fast-math flags, in particular not `contract`. **A test disassembles every function compiled in CI and fails on any FMA opcode**: `fmadd`, `fmsub`, `fnmadd` and `fnmsub` on AArch64, and the `vfmadd*` family on x86-64.
2. **Transcendentals and libm.** `exp`, `ln`, `pow`, `sin` and the rest are calls into `rudb-qc-rt` from every tier, including `interp`. Backend intrinsics such as `llvm.sin.f64` are forbidden, because they allow constant folding at a precision different from the runtime's.
3. **Constant folding.** QIR constant folding (document 06) calls the same Rust function `interp` uses for the operation. A fold that is implemented twice gets implemented differently eventually.
4. **Min, max and NaN.** x86 `minsd` and `maxsd` are not symmetric in NaN, and AArch64 `fmin` propagates NaN. DuckDB orders NaN above every value and treats NaN as equal to NaN. QIR's `fmin` and `fmax` carry DuckDB's semantics, and both native backends expand them to compare-and-select sequences. Document 12 owns the semantics. This document owns the test.
5. **Signed zero in keys.** `-0.0` and `0.0` must hash, group and join as equal, as they do in DuckDB. Normalization happens in the key encoder, and the property test in section 15.7 checks it.
6. **Decimal to float and float to decimal.** These are runtime calls with one implementation. They are never inlined arithmetic.

## 15.5 Fuzzing at the IR level

SQL-level fuzzing finds bugs slowly in a backend, because most SQL reduces to a handful of QIR shapes. **The backends are fuzzed directly with random QIR.**

**The generator lives in the separate `rudb/fuzz` workspace, which already exists and builds on nightly.** It is never an engine dependency. rudb's zero-outside-dependency rule (`rudb/spec/18-package-layout.md` section 18.5) allows fuzzing crates only there or as dev-dependencies. The generator uses the public QIR builder from `rudb-qc-ir`, so every module passes the verifier by construction. Its structure follows Cranelift's own `fuzzgen` approach and the structure-aware direction of CLIR (arXiv 2606.26977, `[snippet]`, title only). The generator:

- chooses a signature from the pipeline ABI, the only ABI generated code has;
- fills a state layout with typed slots;
- generates blocks with loops bounded by a counter, so every program terminates;
- draws instructions from the full QIR instruction set, weighted towards checked arithmetic, NULL-mask manipulation, comparisons, selects and `vcall`s into a stub runtime; and
- emits status returns on random paths, including the error slot.

**The oracle is `interp`, and the comparison covers the whole state.** Each generated function runs on every backend against the same input morsels (section 15.7 draws them from boundary values). After each run the harness compares the returned status, the error slot, every byte of the state block, and the output buffer. A difference is reduced by the QIR reducer from document 16 section 16.8 before it is filed.

**Round-trip is fuzzed at the same time.** Every generated module is printed, parsed back, and printed again, and the two texts must be identical. The parsed module must also run identically. That keeps the textual QIR honest, and document 16 depends on it for reproducers.

The fuzz targets run under `cargo fuzz` locally and in the nightly job, with a fixed time budget and a corpus that grows over time. Budgets are in section 15.12.

## 15.6 Fuzzing at the SQL level

The SQL fuzzers already exist in `rudb-compat` and are run against the first engine. **They are run under `--engine compiled`, with `qc_switch = 'random:<seed>'`, and nothing else about them changes.**

- **TLP** (`src/tlp.rs`, merged as the three-way split in rudb-compat PR #71 `[snippet]`) partitions a query on predicate p into the parts where p is true, where it is false and where it is NULL, and checks that the union of the three equals the unpartitioned query. Rigger and Su's OOPSLA 2020 paper reported 175 bugs, 77 of them logic bugs, about 61 of them in DuckDB (https://www.manuelrigger.at/preprints/TLP.pdf, `[snippet]`, the DuckDB count is approximate).
- **NoREC** (`src/norec.rs`) compares `SELECT * FROM t WHERE p` against `SELECT (p IS TRUE) FROM t` and counts the true rows. The two forms compile to different pipelines, one a filter and one a projection, so NoREC exercises the compiled filter path against the compiled expression path.
- **sqlsmith** (`src/sqlsmith.rs`) generates random queries, and the harness compares their results against DuckDB.

SQLancer's README counts over 400 bugs found across systems, and DuckDB runs SQLancer in its own CI (https://github.com/sqlancer/sqlancer, `[snippet]`). **These oracles are good at the bugs compilers make**: NULL handling in generated predicates, three-valued logic in `OR`, and constant folding that disagrees with runtime evaluation. They are poor at finding backend bugs, which section 15.5 is for.

## 15.7 Boundary values

Random inputs rarely hit the values that break generated code. **Every typed input slot in every fuzzer is drawn half the time from a fixed boundary table.** Design rule E27 lists the minimum.

| type | boundary values |
|---|---|
| INTEGER family | `MIN`, `MIN+1`, `-1`, `0`, `1`, `MAX-1`, `MAX`, and for division `MIN / -1` |
| HUGEINT, DECIMAL(38) | `±(10^38 - 1)`, `±1`, `0`, a value one past the scale |
| DOUBLE, FLOAT | `±0.0`, `±inf`, NaN, the smallest subnormal, `MAX`, values one ULP apart |
| VARCHAR | empty, 1 byte, 12 bytes, 13 bytes, 4 KiB, invalid-looking but valid UTF-8, embedded `\0`, `%` and `_` for LIKE |
| DATE, TIMESTAMP | epoch, leap day, `infinity`, `-infinity`, the largest representable |
| any | NULL, all-NULL morsel, zero-row morsel, one-row morsel, a morsel exactly 1,024 long and 1,025 long |

The 12-byte and 13-byte strings sit on either side of the inline string boundary. The 1,024 and 1,025 row morsels sit on either side of the batch boundary. Those are the two places compiled code switches representation.

**One property test is specific to compiled sort and join keys.** For random tuples a and b over any key type list, `compare(a, b)` in the first engine's comparator must have the same sign as `memcmp(encode(a), encode(b))` over the normalized-key encoding that compiled sorts and hash joins use (documents 10 and 11). This covers NULLS FIRST and LAST, DESC, collations, `-0.0`, and NaN. The test lives in `rudb-qc-rt` and runs in every commit gate.

## 15.8 The encoders against a disassembler

`direct` owns two instruction encoders, one for AArch64 and one for x86-64 (document 08). A wrong bit in an encoder is a miscompile that fires only for one register combination. **Every instruction form the encoders can emit is checked against an independent disassembler.** Under the zero-outside-dependency rule, the disassemblers are dev-dependencies of `rudb-qc-direct` or external tools invoked by the test. They are never linked into the engine.

- **x86-64:** iced-x86 `[GK]`. For every form in the encoder table, the test enumerates all register operands exhaustively, all addressing modes, and immediates from the boundary set plus random draws. It encodes each one, decodes it with iced-x86, and compares the mnemonic, operands and length.
- **AArch64:** the same procedure against a second decoder. LLVM's `llvm-mc --disassemble` is the reference in CI, and a Rust decoder is used locally where one is available `[GK]`.

**Decoding is not enough, so execution is also checked.** For each arithmetic form, a test builds a one-instruction function, runs it on the host over boundary inputs, and compares the result with the Rust semantics of the corresponding QIR operation. This covers flag-setting behavior, which is where overflow checks go wrong.

The encoder tests are exhaustive and take seconds. They run on every commit on both architectures: the M4 locally, the x86 gate box, and Graviton nightly (document 17, section 17.8).

## 15.9 Guards and deoptimization, forced

Generated code is specialized to facts: a dictionary size, a value range, a non-NULL column, a dense key (document 04). A guard checks each fact at the morsel boundary, and a failed guard deoptimizes that morsel to a less specialized version (document 09). In a correct plan the facts almost always hold, so the deoptimization path almost never runs. That makes it the least tested code in the engine.

**`SET qc_guard = 'fail:<seed>:<p>'` makes every guard fail with probability p**, independently per guard per morsel, drawn from the seed. The morsel must restart on the fallback code and produce bit-identical output. This is what checks document 03's claim that a morsel is restartable because generated code never allocates. Runs use p of 0.01, 0.5 and 1.0. At p = 1.0 every query runs entirely on fallback code, which also tests the fallback code on its own.

**The same mechanism forces the rare runtime branches**:

- `qc_guard` also fires hash-table growth and spill triggers where document 13 allows them, and
- a `qc_oom = 'fail:<seed>:<p>'` setting fails runtime allocations, to check that every failure becomes a status code and a clean error, never a crash or a partial result.

## 15.10 Memory safety

Generated code is unsafe code produced by a program. It needs two mechanisms: a check that runs in our own binaries, and a way to put generated code under the sanitizers.

**Checked QIR.** A QIR pass named `bounds`, enabled by `SET qc_checked = on` (on by default in debug builds and in the nightly corpus run), makes three kinds of checks:

- It rewrites every load and store to check the address against the region table of the pipeline state layout (document 05) and the morsel's column buffers.
- It checks every string view's length against its buffer.
- It checks every hash-table slot index against the table's capacity.

A failed check sets error code `QC_INTERNAL_BOUNDS` with the origin of the instruction (document 16, section 16.2) and returns. The pass runs before backend selection, so the same checks run on every tier.

**The C debug backend.** Document 03 section 3.9 allows a C emitter as a debug backend. This document places it: **a crate `rudb-qc-cdebug`, never linked into release builds**, that prints a QIR module as C11. The harness compiles that C with clang under AddressSanitizer and UndefinedBehaviorSanitizer, or ThreadSanitizer for the parallel matrix, and loads it through the same ABI.

- **Precedent.** Umbra kept a C backend for exactly this purpose, and its GCC backend is "only used for debugging" (https://vldb.org/pvldb/vol14/p3207-neumann.pdf).
- **Cost.** Compiling this way is slow, around the 46 s GCC needed for TPC-DS in the CGO 2024 comparison, so the backend only runs nightly.
- **Inputs.** It runs over the JOB, TPC-H SF0.1 and m0 corpora.
- **Scope.** The C backend follows the float rule of section 15.4 (`-ffp-contract=off`, no `-ffast-math`) and is included in the bit-exact comparison. That makes it a fifth tier and a useful second implementation of QIR semantics.

**The runtime is Rust and is checked as Rust.** `rudb-qc-rt` runs under Miri for its pure functions (key encoding, string kernels, decimal arithmetic) and under the sanitizers with the C backend in the nightly job `[GK: Miri does not execute JIT code, so it covers the runtime only]`.

## 15.11 Answer files

**Each benchmark suite has an answer file generated by the pinned DuckDB and checked into `rudb-bench/answers/`.** TPC-H SF1 already has one (`answers/tpch-sf1`). JOB and ClickBench are new.

- **JOB.** 113 answers, one per query, from the pinned DuckDB release on the IMDB snapshot described in document 17. Each JOB query returns one row of `MIN` values, so the answer file is small. Each answer is stored with a hash of the query text and the data snapshot, and a mismatch on either invalidates the answer rather than silently comparing against a stale one.
- **ClickBench.** 43 answers on the `hits` dataset. Queries whose results have ties under `LIMIT` are marked, and compared as sets over the tie group. `rudb-bench` already has `scripts/clickbench-audit.py` for this.
- **TPC-H.** The existing SF1 answers, plus SF10 generated the same way.

**Every timed benchmark run checks its answers before it reports a time.** `rudb-bench` already refuses to publish a timing for a wrong answer. The compiled engine inherits that rule and adds the tier to the record, so a run that was correct on `direct` and wrong after the background switch to `clif` is recorded as wrong.

## 15.12 CI tiers

| tier | when | budget | what runs |
|---|---|---|---|
| commit gate | every push, `scripts/gate` on the Linux box plus the local M4 | 10 min | encoder tests (both ISAs); key-encoding property test; QIR round-trip on the checked-in corpus; the tier matrix rows 1 to 5 on JOB (113), TPC-H SF0.01 (22), ClickBench on a 1M-row sample (43), `corpus/m0.sql`; compiled-only failures on the slt subset `rudb-compat` already gates |
| nightly | once a day, gamingpc-wsl and Graviton | 4 h | full slt corpus under `--engine compiled` and `auto`; full tier matrix including `llvm` and the C backend under sanitizers; TLP, NoREC and sqlsmith for 30 min each with random switches; QIR fuzzing for 30 min per backend; guard forcing at p ∈ {0.01, 0.5, 1.0}; answer checks on JOB, TPC-H SF1 and ClickBench |
| weekly | once a week | 24 h | CEB full set (13,644) against DuckDB; TPC-DS SF1 all 99; QIR fuzz corpus minimization; encoder execution tests over the widened immediate set |

**A red nightly blocks the next release, not the next merge.** A red commit gate blocks the merge. That is the same split `rudb` uses for the first engine, and it keeps the commit gate short enough that nobody routes around it.

## 15.13 The known-divergence registry

Some differences from DuckDB are deliberate or not worth fixing: an error message worded differently, a DuckDB bug we do not reproduce, or an unspecified row order. **They live in one file, `rudb-compat/corpus/divergences.toml`, and nowhere else.** An entry looks like this:

```toml
[[divergence]]
id        = "D-0007"
match     = { corpus = "slt", file = "test/sql/function/string/test_like.test", statement = 41 }
engines   = ["vectorized", "compiled"]
duckdb    = "v2.0-cyanoptera"
class     = "error-text"          # error-text | duckdb-bug | unspecified-order | float-print
reason    = "DuckDB's message quotes the pattern, ours quotes the escape character. Same error class."
review_by = "2026-12-31"
```

**Three rules keep the registry from becoming a place to hide bugs.**

1. **The compiled engine may not have a divergence that the first engine does not have.** Every entry lists `vectorized` among its engines, or it is rejected. Differences between the two engines of rudb are bugs, without exception.
2. **No entry may change a result value or its type.** The classes are error text, a DuckDB bug with an upstream link, unspecified order, and float printing beyond DuckDB's own precision. Wrong numbers are not a class.
3. **Every entry has a review date, and an expired entry fails the nightly.** The pinned DuckDB version moves, and some of its bugs get fixed.

The registry's size is published next to the refusal rate. A registry that grows faster than the pass count is a signal in its own right.

## 15.14 Correctness under concurrency and cancellation

The matrix in section 15.3 runs at one thread and at all threads. Two behaviors need more than that.

**Cancellation.** `SET qc_cancel = 'random:<seed>'` cancels the query at a random morsel boundary. It also cancels during a background compilation and during a `clif` install. After each cancellation the test checks four things:

- the query returns the cancellation error,
- no pipeline state is leaked (the runtime's allocation counter returns to its value before the query),
- the code cache holds no half-installed function, and
- the next query on the same connection is correct.

**Racing tier installs.** Background compilation finishes at an arbitrary time. The runtime swaps the function pointer between morsels, never within one (document 09). The ThreadSanitizer run over the C backend covers the state accesses. A dedicated stress test covers the swap itself: 64 threads run the same pipeline while the installer swaps between `direct` and `clif` versions on every morsel, and the output must stay bit-identical to `interp` under the multiset rule.

## 15.15 What counts as proof

A correctness claim is a sentence of the form "the compiled engine is correct on suite X". It may be made in a report or a release note when all of the following hold on the commit being claimed:

1. **Every query in X ran under `engine = 'compiled'` with zero refusals**, or the refusals are listed by reason and the claim says "on the accepted subset".
2. **The results matched the answer file**, generated by the named DuckDB version on the named data snapshot.
3. **The tier matrix of section 15.3 was clean on X**, including random switches and guard forcing, with the switch log showing that build, aggregate and top-N state were each hit.
4. **Each query ran at least five times, with the code cache hit on the later runs.**
5. **The registry entries that apply to X are listed in the claim.**

A performance claim in document 17 is not publishable without a correctness claim on the same commit for the same suite. The two are recorded together in `rudb-bench`, with no route to one that skips the other.

**Honest limits.** None of this proves the absence of miscompiles. The tier matrix only catches bugs in the parts of the input space the corpora reach. The fuzzers widen that space without closing it. Bit-exact comparison against `interp` means a bug shared by `interp` and every backend passes the tier test. Such a bug comes from a QIR translator, not a backend, and only DuckDB can catch it. This is why the two kinds of oracle stay separate and neither is dropped when the other goes green.

## What we should take from this document

The compiled engine gets three oracles. DuckDB defines the answer, the first engine locates a bug relative to the shared frontend, and `interp` defines what every backend must produce bit for bit. The strict one, the third, is where backend bugs are caught, and it is cheap because it compares raw chunks in one process.

Tier switching and caching are the new failure modes, and the ClickHouse `Int128` bug is both at once. Every query therefore runs on every tier, under random switches that are steered into live state, under forced guard failures, and five times so that the cache is on the path. A test that runs a query once proves nothing about a compiler.

The floating-point rule is enforced mechanically. No FMA opcode may appear in any compiled function, transcendentals go through one runtime, constants fold through one function, and parallel float aggregation order is explicitly outside the rule.

Backends are fuzzed at the QIR level against `interp` and checked against independent disassemblers. SQL is fuzzed with the TLP, NoREC and sqlsmith that `rudb-compat` already has. Memory safety comes from a checked-QIR pass on every tier and a C debug backend that puts generated code under the sanitizers every night.

Divergences from DuckDB live in one registry. The compiled engine may never have one the first engine lacks, and no divergence may change a value. A performance number without a correctness claim on the same commit does not get published.
