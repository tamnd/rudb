# Testing and correctness

Document 14 tests compatibility, meaning agreement with DuckDB. This document tests correctness, meaning agreement with what is true. They are different and neither substitutes for the other: where DuckDB has a bug, the compatibility suite rewards reproducing it, and where DuckDB and `rudb` share a misunderstanding, the compatibility suite is silent.

A database that is fast and wrong is worthless, and every optimization in documents 06 through 09 adds a path on which it could be wrong. The specialized-versus-general kernel pairs from document 6.7, the four execution tiers from document 08, the adaptive switches from document 7.9, and the optimizer rewrites from document 9.2 each multiply the number of ways the same query can be executed. The strategy below is built around that: the number of execution paths is the problem, so the testing apparatus is organized around proving they agree.

## 16.1 Layers

**Unit tests** for every module, in the module, standard Rust practice, no comment needed.

**Property tests** for anything with an algebraic law. Encode then decode is the identity, for every encoding on every input. Parse then print then parse is the identity, for the SQL AST, for the logical plan, for the expression IR, for the physical plan. Serialize then deserialize is the identity for every catalog object. These are cheap to write, they run in seconds, and they catch the class of bug that is hardest to find by reading.

**Integration tests** as `sqllogictest` files, our own plus DuckDB's imported corpus.

**Differential tests**, which are the heart of the strategy and get their own sections below.

**Fuzz tests**, continuous, coverage-guided.

**Crash tests**, simulated, deterministic.

## 16.2 The specialization equivalence problem

Document 6.7 says every operator has a general path over any physical form plus zero or more specialized paths, and that adding a specialization is a pure performance change. That sentence is a testable claim and it is tested directly.

**Every query in the corpus runs twice: once with all specializations enabled, once with all specializations disabled.** Results must be bit-identical. This is a single session setting and it turns the entire specialization design into something that is verified rather than trusted.

**Then it runs with each specialization individually disabled**, which is more expensive and runs nightly rather than per commit, and which is what turns a failure from "some specialization is wrong" into "this one is".

**The kernel generator's output is tested against the generic implementation exhaustively for small types.** For an 8-bit input, all 256 values. For a 16-bit input, all 65,536. For wider types, a property test with edge-case-weighted generation. A generated kernel that disagrees with the generic one on any input is a build failure, and because the kernels are generated the tests are generated too, so coverage does not depend on anyone remembering to add a case.

## 16.3 The tier equivalence problem

Document 8.1 says tier 0 is the correctness reference and every other tier is validated against it.

**Every query in the corpus runs at every tier and the results must be bit-identical.** Not approximately equal. Bit-identical, including floating point, which is why document 8.4 forbids reassociation and unrequested fused multiply-add in generated code.

**The expression IR has its own fuzzer** that generates random typed IR programs, evaluates them on all tiers, and compares. This finds things a SQL-level fuzzer does not, because the SQL front end cannot express every IR program and the IR is where the three backends actually have to agree.

## 16.4 Fuzzing

**Query fuzzing** via grammar-based generation, shared with document 14.3.

**Data fuzzing.** Random data covering every type, every nesting depth, every encoding, and specifically data constructed to be adversarial for the encoders: values that straddle a bit-width boundary, dictionaries that grow past a code width mid-load, RLE runs of exactly the vector length, strings that FSST cannot compress, floats that ALP must send to the exception path, and nested structures at the depth limit.

**File fuzzing.** Take a valid database file, mutate bytes, open it. Every possible outcome except a crash or a hang is acceptable: a clean error is fine, refusing to open is fine, silently wrong data is not, and a segmentation fault is a security bug. Format parsers are where memory-safety bugs live in every C and C++ database, and this is a place where Rust removes the whole class, but only for the safe code. Every `unsafe` block in the parsing path is a place where it does not, which is why section 16.7 exists.

**API fuzzing.** Random sequences of C API calls including invalid ones, out-of-order ones, use-after-free attempts and double frees, under AddressSanitizer. The C API is an unsafe boundary by construction and a caller will do all of these things.

**Continuous, not periodic.** Dedicated machines, persistent corpus, automatic filing with automatic reduction. A fuzzer that runs for an hour before a release finds a small fraction of what one that runs continuously finds.

**Any panic is a failure**, including one that would be caught. A panic in a worker thread is a bug in this codebase's terms, per document 4.9.

## 16.5 Crash consistency

**Deterministic simulation with an intercepting filesystem layer.** Every write, fsync, rename and truncate goes through a shim that records it and can be told to fail at a chosen point, and to reorder writes that were not separated by an fsync.

**The test enumerates failure points.** Run a workload, record the sequence of I/O operations, then for each point in that sequence rerun with a failure injected there and verify that the resulting database opens and contains exactly a prefix of committed transactions. This is exhaustive over failure points for a given workload rather than random, which is what makes it able to prove something rather than merely fail to find a bug.

**Reordering is tested, not just truncation.** A crash after two writes with no fsync between them can leave either, both or neither. Testing only the truncation case misses the bugs that actually happen on real hardware.

**The specific properties asserted**, per document 11.5: no committed transaction is lost, no uncommitted transaction is visible, no block is both free and referenced, no metadata references a block that was never written, and the checksums validate.

**This is scheduled at M6 and not after 1.0**, because retrofitting the I/O interception layer into a codebase that has been calling `File::write` directly for two years is a much larger job than building against the shim from the start.

## 16.6 Memory and resource testing

**Every allocation failure point is tested**, by an allocator shim that fails the nth allocation for every n over a workload. A database whose out-of-memory path is untested has an out-of-memory path that does not work, and the entire value of document 4.9's fallible allocation discipline is realized only if it is exercised.

**Spilling is tested by running the standard suites at a memory limit low enough to force it**, at several limits, with the results compared against the unspilled run. TPC-H SF1000 on a small machine is the standard configuration for this and it should be in the nightly.

**Leak detection** under sanitizers, plus an assertion at shutdown that the buffer manager has zero pins outstanding, which catches the leaked-pin class that document 5.5 says Rust's RAII prevents by construction and that this assertion verifies it actually did.

## 16.7 Unsafe code

**Every `unsafe` block has a comment stating the invariant that makes it sound**, and CI rejects one that does not. This is a lint, not a convention.

**`unsafe` is confined to a small number of named modules**: SIMD intrinsics, the FFI boundary, the buffer manager's page mapping, and specific hot-path bounds-check elisions where the bound was already checked. The rest of the codebase is `#![forbid(unsafe_code)]`, enforced per crate in document 18.

**Miri runs over the whole test suite** on the scalar code path, which is why the scalar fallback in document 7.3 is not vestigial. It is slow and it runs nightly.

**Every hot-path bounds-check elision is justified by a measurement recorded in its comment.** An `unsafe` block that was added because someone assumed it would be faster is a defect, whether or not it is sound.

## 16.8 Concurrency testing

**Loom or a similar model checker for the small concurrent data structures**: the buffer manager's latch protocol, the global hash table's insert path, the work-stealing deque. These are exactly the code where a rare interleaving produces a bug that appears once a month in production and cannot be reproduced.

**ThreadSanitizer over the full suite** nightly. Rust's type system prevents data races in safe code, which means every race that does exist is in one of the `unsafe` modules from 16.7, which is a very useful narrowing but not an absence.

**Stress tests with many concurrent readers plus a writer**, running for hours, asserting snapshot isolation properties throughout.

## 16.9 What good looks like

A specific and uncomfortable standard: **at 1.0, no known wrong-answer bug, and a fuzzer that has run for a month without finding a new one.** Not "no wrong answers found", which is a statement about how hard anyone looked.

**Every bug found gets a regression test before it gets a fix**, no exceptions.

**Coverage is measured and published**, with the understanding that line coverage is a weak signal and its main use is finding entire modules nobody tested rather than proving that anything is correct.

**The correctness apparatus is built before the thing it tests wherever the order is a choice.** The differential harness precedes the optimizer passes, the I/O shim precedes the storage engine's write path, and the tier comparison precedes the second tier. Every one of those orderings costs time up front and every one of them is cheaper than the alternative.
