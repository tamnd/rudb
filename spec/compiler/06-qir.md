# QIR

Artifact A7. The intermediate representation that every translator writes and every backend reads. It is the one place where the compiled engine's semantics are written down in executable form: `interp`, `direct`, `clif` and `llvm` differ in speed, never in meaning.

## 6.1 What QIR is for, and what it is not

**QIR exists so that code generation and code emission are both linear passes over a flat buffer.** Every design decision below serves one of two budgets from document 02: QIR generation at 0.1 ms median and 0.5 ms maximum per JOB query, and the `direct` backend at 0.5 ms median and 2 ms maximum. The research is unanimous that the IR decides whether those budgets are reachable. Umbra's generator is more than 1000x faster than LB2's staged Scala, which needed 299 ms geomean just to produce code (TIDY, https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf). DirectEmit compiles all 6678 TPC-DS functions in 64 ms, about 10 µs per function, from an IR built for it (CGO24, https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf). TPDE's Cranelift port shows the flip side: 41% of its compile time goes to translating Wasm into CLIF before TPDE runs, so the IR a query engine generates must already be the backend's input (B §3.3, https://home.cit.tum.de/~engelke/pubs/2602-cgo1.pdf).

**QIR is not an optimizing IR.** There are no use lists, no alias analysis, no loop transformations, no instruction scheduling. Cranelift's own retrospective found that its e-graph mid-end buys about 2% faster code for 7-8% more compile time, and that most of the value is GVN, constant folding and LICM (https://cfallin.org/blog/2026/04/09/aegraph/). A query generator knows what is loop-invariant and what is constant when it emits the instruction, so it does those three things at append time and nothing afterwards. The optimizing tier is `clif` (and `llvm` if the feature is on). Whatever they do, they do on their own IR after translation.

**QIR is not a language anyone writes by hand.** It has a textual form (section 6.8) that prints and parses, because document 03 section 3.1 requires it of every artifact and because lowering tests are text-in, text-out. The text is a debugging surface, not an interface.

## 6.2 Design principles

Six decisions, each with the reason it was taken.

**1. A flat arena of 32-bit words with 4-byte references.** A function is a `Vec<u32>` of instructions laid out back to back, plus side tables. An instruction is one header word (opcode 8 bits, result type 5 bits, flags 5 bits, operand count 6 bits, reserved 8 bits) followed by its operand words. A value reference `Val(u32)` is a dense value number, and the value's type, storage class and name hint are columns indexed by it. Block parameters are values too, so every value has exactly one definition. The first draft made a `Val` the word offset of its defining instruction, as Umbra IR does, and C1 changed that: dense numbers are what the interpreter's register file and the liveness columns index by, and they let a block be appended to after a later block was started, which loop-invariant placement into a preheader needs. Each block keeps its own word arena, so the layout rule of principle 5 is a list of block ids and not a copy. This is Umbra IR's layout: variable-length instructions in one array, 4-byte offsets, no per-instruction allocation (TIDY; A §1.3). Rust makes it cheap to get wrong by reaching for `Vec<Inst>` with an enum of boxed operand lists, and that is forbidden: one `enum` with a `Vec` inside is one allocation per instruction, which is the whole generation budget.

**2. Constants are not instructions.** A `Val` with the top bit set is an index into the function's constant pool, where each entry is `(type, 128-bit payload)`. The pool is deduplicated at append through an open-addressing table keyed by `(type, bits)`. Constant folding happens at append: `add i32 3, 4` never enters the arena, the builder returns the pool entry for `7`. Folding respects overflow: a checked add of two constants that overflows is not folded, it is emitted, so the query fails at runtime with DuckDB's message and only if the row reaches it.

**3. SSA with block parameters, not phi nodes.** A block declares typed parameters. A branch passes arguments. Values defined in a dominating block may be used directly. The choice is made for the single-pass backend:

- Liveness is a backward scan over the arena. With block parameters, the only values that cross an edge are either parameters, which are defined at the block head, or dominating definitions, whose last use is a plain operand. There is no "use in a phi counts as a use at the end of the predecessor" rule to special-case. TPDE and Flying Start both do their liveness in one linear pass using Kohn's interval approach (B §3.1), and block parameters are the form in which that pass has the fewest cases.
- At each branch the backend emits a parallel move from arguments to parameter locations. That is the one place a single-pass allocator needs a move resolver, and it is visible at the branch instruction instead of being scattered across the successor's phi list.
- `clif` translation is one-to-one, since CLIF uses block parameters. `llvm` translation turns parameters into phis, which is mechanical. The interpreter treats a parameter as a register slot written by the branch.

The cost is that the builder must know at branch time what flows into the successor. Section 7.2 shows that structured control helpers (`if_else` returning values, `loop_with` carrying values) always know. QIR has no mutable variables and needs no SSA construction pass.

**4. Loops are declared, not discovered.** A block that is the target of a back edge carries the `loop` flag and its nesting depth, set by the builder's loop helper. TPDE computes loop nesting with the Wei et al. algorithm and Umbra uses Ramalingam's (B §3.1; E §1.4). We skip the analysis because the generator already knows, and the verifier checks the claim: every back edge targets a block flagged `loop`, every `loop` block is the target of at least one back edge, and the CFG is reducible.

**5. Blocks are laid out in reverse postorder at append.** The structured helpers emit the loop body immediately after the header and the exit after the body, and cold blocks (trap stubs, deopt exits, NULL slow paths) are appended to a separate cold list that is concatenated at the end. The backend never reorders blocks. Hot code is contiguous and cold code is out of line without a layout pass.

**6. One function per pipeline version.** A QIR module (A7) holds one function per pipeline and per specialization version (section 7.8), plus the module tables in section 6.9. The function signature is fixed by document 03: `extern "C" fn(state: *mut PipelineState, morsel: *const Morsel) -> Status`. Nothing in QIR can express a different entry signature, so every backend and every tier is interchangeable at a morsel boundary. EVOL's single function for a provably tiny OLTP plan (E §8.2) is a pipeline whose morsel is one row. Document 14 owns it and it uses the same signature.

## 6.3 Types

| Type | Width | Notes |
|---|---|---|
| `i1` | 1 bit | comparison results, validity bits; stored as a byte in memory |
| `i8`, `i16`, `i32`, `i64` | 8-64 | signedness is in the opcode, not the type, as in LLVM and CLIF |
| `i128` | 128 | DECIMAL(19..38), SUM accumulators, wide multiply results, UUID, HUGEINT |
| `f32`, `f64` | 32, 64 | IEEE; no fast-math flags exist |
| `ptr` | 64 | untyped address; the translator knows what it points to |
| `str16` | 128 | the 16-byte string header, see below |

**`i128` is a value type in every backend's interface, and each backend decides how to hold it.** `direct` holds it in two GPRs and emits add/adc pairs. `clif` uses its `i128`. `llvm` lowers it to two `i64` halves itself, because CGO24 found that 1328 of the 3876 FastISel fallbacks came from 128-bit types and that splitting them was about 7% faster (B §1.3). The type exists at the QIR level so that the translator never has to emit carry logic, and so that the rule "use `i128` only when the result type needs more than 18 digits" (section 7.7) is visible in the IR.

**`str16` is a first-class 128-bit value, not a pair.** The layout is DuckDB's and Umbra's: a u32 length, then either 12 inline bytes or a 4-byte prefix plus an 8-byte pointer (E §4.4, https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf). We considered representing it as `(i64, i64)` and rejected that for two reasons. First, the verifier needs the type to check storage-class rules: a string whose pointer is transient must not be stored into state that outlives the morsel. As a pair it is two anonymous integers. Second, the backends lower it differently and should be allowed to: `direct` keeps it in two GPRs, `clif` and `llvm` in two `i64`. A future AArch64 path could hold it in one NEON register for the equality fast path. The header operations in section 6.5 expose the halves cheaply, so treating it as one value costs nothing.

**Every `str16` value carries a storage class in its flags:** `persistent` (points into storage the snapshot pins), `transient` (points into a scan buffer valid for the current morsel), `temporary` (points into query-lifetime arena memory). Inline strings are trivially persistent. The class is a static property the translator knows. The verifier enforces it (rule V10). There is no runtime tag.

**There is no NULL type, no nullable type and no SQL type in QIR.** Section 6.7.

## 6.4 Arithmetic, comparison and hashing

Operands are values or constants. `ty` is the operand type. The result type is `ty` unless stated.

| Group | Instructions | Semantics |
|---|---|---|
| wrapping integer | `add sub mul neg`, `and or xor not`, `shl lshr ashr rotl rotr`, `clz ctz popcnt bswap` | two's complement, no checks; for hashing, addresses, loop counters, and arithmetic proven safe by a fact |
| checked, trap form | `sadd.t ssub.t smul.t sneg.t uadd.t usub.t umul.t ty a, b, !E` | on overflow, record error site `E` with operands `a, b` and return `Status::Error`; otherwise the result |
| checked, edge form | `sadd.ov ty a, b -> blk_ok, blk_ovf` (terminator) | result is a parameter of `blk_ok`; `blk_ovf` gets none |
| wide | `smulw i64 a, b -> i128`, `umulw` | full product, never overflows |
| division | `sdiv.t srem.t udiv.t urem.t ty a, b, !E` | traps on zero and on `MIN / -1`; the translator guards first when SQL says the result is NULL instead |
| decimal scale | `dup.t ty a, k, !E` | `a * 10^k`, checked, `k` a constant ≤ 38 |
| | `ddown ty a, k` | `a / 10^k` rounded half away from zero; `i64` lowered as a constant-divisor multiply, `i128` as an `rtcall` |
| conversion | `sext zext trunc ty a -> ty2`, `sitof uitof`, `ftosi.t ty a -> ty2, !E`, `fext ftrunc`, `bitcast` | `ftosi.t` traps on NaN and out of range |
| float | `fadd fsub fmul fdiv fneg fabs fmin.tot fmax.tot fsqrt` | IEEE round-to-nearest; `.tot` uses the total order below |
| compare | `icmp.{eq,ne,slt,sle,sgt,sge,ult,ule,ugt,uge} ty a, b -> i1` | |
| | `fcmp.{eq,lt,le}.tot ty a, b -> i1` | DuckDB's total order, NaN equal to itself and above every number [background; doc 12 pins it] |
| select | `select ty c, a, b` | branch-free choice; `c` is `i1` |
| hash | `crc32c i64 seed, x -> i64` | CRC-32C of the 8 bytes of `x` continuing from `seed`, zero-extended |

**Checked arithmetic is one instruction, not an add plus a flag test.** Umbra has both a branching form, `%c = checkedsadd i32 %a, %b %continue %overflow`, and a trapping form, `ssubtrap i32 %3, 53` (CGO24, E §1.3). QIR keeps both and makes the trap form the default. It is not a terminator, so a pipeline body with fifty checked operations is still one block, and the single-pass allocator sees one long block instead of fifty tiny ones. Each backend lowers it to its native shape: `adds w0, w1, w2; b.vs E3_stub` on AArch64, `add eax, ecx; jo E3_stub` on x86-64. The stubs are cold, one per error site, and each stores its operands into the per-thread error slot and returns. The edge form exists for the one case where overflow is not an error: a narrow accumulator speculated to fit, whose overflow edge deoptimizes (section 7.7).

**Division is never generated for decimals.** Document 03 section 3.6 puts decimal division in the runtime. `ddown` is the one exception, because rescaling by a constant power of ten is a multiply-shift for `i64` and appears in every decimal cast and comparison across scales.

**`crc32c` has exact semantics on every backend, and the hash function is fixed per process.** A hash table built by a pipeline on `direct` is probed by a pipeline that may be running on `interp` or `clif`, so all tiers must compute the same hash bit for bit. CRC-32C is available on every CPU the project targets: SSE4.2 on x86-64, the CRC extension on Armv8.1 and later [background]. The interpreter uses the same intrinsic through `core::arch`. The two-chain combine is plain QIR (section 7.6). CGO24's multiply-fold fallback for CPUs without CRC (E §1.2) is kept as a process-wide alternative hash, selected once at startup, never mixed.

**No fused multiply-add and no reassociation, anywhere.** Document 03 section 3.8. `fmul` followed by `fadd` must stay two roundings on every backend. `clif` does not fuse by default, `llvm` is built with contraction off, and the verifier has nothing to check because QIR has no instruction that fuses.

## 6.5 Memory, strings, control, calls

**Loads and stores carry their address mode.** `load.ty [base + idx*scale + disp]` and `store.ty [base + idx*scale + disp], v`, where `base` and `idx` are values, `scale` is 1, 2, 4, 8 or 16, and `disp` is a signed 32-bit constant. Every morsel column read has the shape `[col + row*4]` and every state field has the shape `[state + 72]`, so folding the address into the instruction removes an instruction per access and matches what Flying Start does with deferred address computation (E §3.4). Backends that cannot encode a mode directly split it. Flags on memory instructions: `.nt` (the address is known not to alias any store in the function, used by `clif` and `llvm`), `.a16` (16-byte aligned).

| Group | Instructions |
|---|---|
| memory | `load.ty`, `store.ty`, `load.bit [base + idx] -> i1` (bit `idx` of a bitmap), `memcpy.N dst, src` (N a constant ≤ 64), `memeq.N a, b -> i1`, `prefetch.{r,w} [addr]`, `cas.ty [addr], old, new -> i1`, `atomic.add.ty [addr], v -> ty` |
| string header | `str.len s -> i32`, `str.w0 s -> i64` (length and prefix), `str.w1 s -> i64` (second half), `str.ptr s -> ptr` (valid only when not inline), `str.inl s -> i1` (length ≤ 12), `str.mk w0, w1 -> str16`, `load.str [addr] -> str16`, `store.str [addr], s` |
| control (terminators) | `br blk(args)`, `brif c, blk1(args), blk2(args)`, `switch.ty x, [k0: blk0, ...], default(args)`, `ret status`, `trap !E` |
| calls | `rtcall @name(args) -> ty` with attributes, `vcall @k(bufs), n -> status` |
| engine | `guard c, !G`, `poll N`, `ctr.add #k, v` |

**String operations stop at the header.** Equality, ordering, prefix and `LIKE` are expanded by the translator into header comparisons plus an `rtcall` for the out-of-line bytes (section 7.9). The instructions give the translator the two 64-bit halves and the inline bit. `streq` is not an instruction because its fast path is two 64-bit compares and a branch. Every backend emits that well from the halves, and a monolithic instruction would force each backend to reimplement the slow path.

**`memcpy.N` and `memeq.N` exist because constant-size copies and compares are how tuples move.** DuckDB's sort work measured a static-size `memcmp` 25% faster on average below 16 bytes and a static `memcpy` 55% faster on one CPU (DSORT, https://duckdb.org/pdf/ICDE2023-kuiper-muehleisen-sorting.pdf). With `N` a constant, `direct` emits straight-line loads and stores. Anything longer or dynamic is an `rtcall`.

**`rtcall` targets the runtime library through a generated proxy table.** The table, built by a `build.rs` in `rudb-qc-rt`, records each function's symbol, QIR signature, and attributes. The translator names functions by proxy, never by address, so a module is position-independent until the backend links it. Attributes:

- `nothrow`: always set; runtime functions return status and never unwind across the boundary (E §5.2; RFC 2945).
- `mayfail`: the function returns `Status` as its first result, and the backend emits the check-and-return after it.
- `cold`: the call site sits in a cold block.
- `pure`: no side effects, eligible for CSE.
- `effect`: writes shared state, which matters for rule V9.

The calling convention is the platform C ABI. Document 08 covers the AArch64 ±128 MB `BL` range, which is why `direct` calls runtime functions through a veneer table placed in the code region.

**`vcall` is a stage boundary, not a per-tuple call.** `%st = vcall @k(%in0, %in1, %out), %n` calls the first engine's vectorized kernel `k` over `n ≤ 1024` elements. The inputs and output are scratch buffers declared in the pipeline state with the kernel's physical types and validity masks. The translator fills the buffers in one loop, emits the `vcall`, and reads the results in a second loop (section 7.10). The instruction itself is only the call. It is `mayfail` and `effect`-free. The proxy table resolves `k` from the first engine's function registry by `(function name, argument types)` at generation time.

**`guard c, !G` returns `Status::Deopt(G)` when `c` is false.** `G` indexes the module's guard table: which fact, which version to fall back to. The runtime reruns the morsel on the fallback version (section 7.7; document 09). A guard has no successor and is not a terminator, for the same reason as the trap form of checked arithmetic.

**`poll N` is the counted cancellation check.** It decrements a thread-local counter kept in `PipelineState`. Every `N`th execution it loads the query's cancel flag and returns `Status::Cancelled` if it is set. The generator must place `poll` on every back edge of a loop whose trip count is not bounded by the morsel: hash chain walks, nested-loop joins, string scans over unbounded values. Morsel-bounded loops need none, because the scheduler checks between morsels (MORSEL; E §5.3). Rule V11 checks this.

**`ctr.add #k, v` adds to counter `k` in the pipeline's counter block.** Counters feed selectivity measurement (section 7.5), the tiering extrapolation (document 09) and `EXPLAIN ANALYZE`. It is an instruction so that `direct` can keep a hot counter in a register across the batch loop and flush it at loop exit. A load-add-store per tuple is what a generic lowering would produce.

**Hash table probe and insert are expanded, not intrinsics.** This is the decision with the most alternatives, so here is the reasoning. An intrinsic `probe` would let each backend emit a hand-tuned loop, but it would bake one table layout into four backends. It would also hide the key comparison, which is the part that benefits most from specialization: a unique `i32` key compares in one instruction, a composite key with a string in twenty. And it would give the interpreter an opaque operation to reimplement. Expanded, the probe is 15 to 25 QIR instructions (the example in section 6.8). Every backend compiles it without knowing what a hash table is, and document 10 can change the layout by changing one generator. Umbra's IR has no hash table instructions either, and its probes are generated code (TIDY; document 03 section 3.6). Growth, resizing and spilling are `rtcall`s, and insert never needs them on the hot path because memory is reserved at plan time (document 03 section 3.7).

## 6.6 Instruction counts and what they cost

**Budgets.** These are this spec's targets, derived from document 02's per-query budgets, not measurements.

| Quantity | Target | Reasoning |
|---|---|---|
| bytes per instruction in the arena | ≤ 16 average | 4-byte header + 2.5 operands × 4; provenance is a separate 4-byte column |
| generation cost | ≤ 10 ns per instruction | 0.1 ms median QIR budget ÷ 10,000 instructions |
| `direct` cost | ≤ 50 ns per instruction | 0.5 ms budget; DirectEmit's ~10 µs per function (CGO24) is the existence proof |
| instructions per JOB query | ≤ 10,000 median | HyPer plans ran 300 to 19,000 LLVM instructions (ADAPT, https://db.in.tum.de/~leis/papers/adaptiveexecution.pdf); QIR is denser than LLVM IR because checks and address modes are folded |
| instructions per function | ≤ 20,000 hard cap | larger bodies are split (section 7.8) |
| verifier cost | ≤ 5 ns per instruction | on in debug builds and the differential harness, off on the release query path |

**Scaling must be linear.** Tidy Tuples' 2000-join stress query produced 108,000 Umbra IR instructions. LLVM took 150 s, FastISel 4 s and Flying Start under 0.04 s (E §3.4). EVOL reports a real customer query with 300,000 disjunctions (https://vldb.org/pvldb/vol14/p3207-neumann.pdf). Every QIR pass is linear in arena size. Section 7.11 carries the same rule to the generator.

## 6.7 The NULL model

**QIR has no NULLs. A nullable SQL value is two QIR values: the data and an `i1` validity.** The SQL-value layer of the generator (section 7.2) decides when the validity exists at all. It exists when the column is nullable, the expression can produce NULL, or an outer join introduced it. When a fact says a value is non-null, the validity is the constant `true`, and constant folding at append removes every AND, every select and every branch that would have tested it. This is Tidy Tuples' rule, where the null indicator of a NOT NULL value is a compile-time `false` (TIDY; E §4.3), moved one layer down so that the IR never pays for it.

Four consequences.

- **Loading validity is `load.bit`.** Storage validity masks are bitmaps. `load.bit [mask + row]` compiles to a shift and an AND on both ISAs.
- **Filters collapse NULL and FALSE.** A WHERE predicate only asks "is TRUE", so the translator emits `and valid, value` and branches once. Full three-valued logic, as a `(value, valid)` pair per result, is emitted only under `NOT`, `OR` of nullable operands, and in projected expressions (E §4.3).
- **Arithmetic on invalid lanes must not trap.** A checked add whose operand is NULL computes garbage and must not raise. The translator guards trapping instructions with the validity. It either branches around them, or, in the predicated form, substitutes a safe operand (`select valid, a, 0`) before the operation. Rule V12 checks this: every trapping instruction whose operand has a non-constant validity is dominated by a branch on that validity or takes a `select` on it.
- **Validity in state is explicit.** Aggregate state rows and hash table payloads store validity as bytes or bit fields at offsets the tuple layout fixed. QIR sees them as ordinary loads and stores.

## 6.8 Textual form

The printer and the parser live in `rudb-qc-ir`. The format is line-oriented, one instruction per line. Values are `%name`, where the name is a hint the builder was given and the printer disambiguates. Constants are written inline with the instruction's type. Blocks show their parameters. Trailing `;` comments hold provenance when printing with `-p`.

The example is the probe pipeline of JOB 1a, simplified to one join and one aggregate so it fits. The scan kernel has already applied `mc.note` predicates and handed a selection vector. The body tests the `company_type` semi-join as a bitmap over dense codes and probes the hash table built on `title.id`. For each match it updates `MIN(t.production_year)`. Facts used: `mc.movie_id` is non-null (exact), `mc.company_type_id` codes are dense in 1..4 (exact), `title.id` is unique (exact), `t.production_year` is nullable. The table layout (tagged directory words, entries with a next pointer, key and payload) is shown schematically and belongs to document 10.

```
module q1a  hash=crc32c  target=any
  guard !G0  fact=F9 "ht_t.size <= L2/2"  fallback=q1a.p3.staged
  error !E0  kind=cancel

func @q1a.p3.fused(ptr %st, ptr %m) -> i32  version=fused  plan=#14
block b0(ptr %st, ptr %m):
  %n     = load.i32 [%m + 8]                         ; batch length
  %sel   = load.ptr [%m + 16]                        ; u32 selection vector
  %c_mid = load.ptr [%m + 32]                        ; mc.movie_id, i32, non-null
  %c_ct  = load.ptr [%m + 40]                        ; mc.company_type_id, dense codes
  %ctbm  = load.i64 [%st + 64]                       ; semi-join bitmap over ct codes
  %dir   = load.ptr [%st + 72]                       ; ht_t directory
  %shift = load.i64 [%st + 80]                       ; 64 - log2(directory size)
  %mn0   = load.i32 [%st + 96]                       ; running MIN
  %mv0   = load.i8  [%st + 100]
  %mv0b  = trunc i8 %mv0 -> i1
  br b1(0, %mn0, %mv0b)
loop(1) b1(i32 %i, i32 %mn, i1 %mv):
  %done  = icmp.uge i32 %i, %n
  brif %done, b8(%mn, %mv), b2
block b2:
  %r     = load.i32 [%sel + %i*4]
  %ct    = load.i32 [%c_ct + %r*4]
  %ct64  = zext i32 %ct -> i64
  %bit   = lshr i64 %ctbm, %ct64
  %bit1  = trunc i64 %bit -> i1
  %inext = add i32 %i, 1
  brif %bit1, b3, b1(%inext, %mn, %mv)
block b3:
  %mid   = load.i32 [%c_mid + %r*4]
  %k     = zext i32 %mid -> i64
  %h1    = crc32c i64 6763793487589347598, %k
  %h2    = crc32c i64 4593845798347983834, %k
  %h2r   = rotr i64 %h2, 32
  %hx    = xor i64 %h1, %h2r
  %h     = mul i64 %hx, 11400714819323198485
  %slot  = lshr i64 %h, %shift
  %w     = load.i64 [%dir + %slot*8]
  %t4    = lshr i64 %h, 60
  %t48   = add i64 %t4, 48
  %tb    = shl i64 1, %t48                          ; tag bit in the pointer's top 16 bits
  %hit   = and i64 %w, %tb
  %miss  = icmp.eq i64 %hit, 0
  %e0    = and i64 %w, 281474976710655              ; low 48 bits: chain head
  brif %miss, b1(%inext, %mn, %mv), b4(%e0)
loop(2) b4(i64 %e):
  poll 1024
  %ep    = bitcast i64 %e -> ptr
  %key   = load.i32 [%ep + 8]
  %eq    = icmp.eq i32 %key, %mid
  brif %eq, b5(%ep), b6
block b6:
  %nx    = load.i64 [%ep + 0]
  %end   = icmp.eq i64 %nx, 0
  brif %end, b1(%inext, %mn, %mv), b4(%nx)
block b5(ptr %hitp):                                 ; unique key: first match ends the walk
  %py    = load.i32 [%hitp + 12]
  %pyv   = load.bit [%hitp + 128]                  ; bit 0 of the flags byte at offset 16
  %lt    = icmp.slt i32 %py, %mn
  %nmv   = not i1 %mv
  %bet   = or i1 %nmv, %lt
  %take  = and i1 %bet, %pyv
  %mn2   = select i32 %take, %py, %mn
  %mv2   = or i1 %mv, %pyv
  ctr.add #0, 1                                      ; probe matches
  br b1(%inext, %mn2, %mv2)
block b8(i32 %mnf, i1 %mvf):
  store.i32 [%st + 96], %mnf
  %mvb   = zext i1 %mvf -> i8
  store.i8 [%st + 100], %mvb
  ret 0
```

That is 55 instructions for a join probe with a semi-join, a tagged-directory early reject, a chain walk and a nullable MIN, with no NULL code for the non-null key. The staged version named in the guard table is the same logic split at the probe by a stage buffer and group prefetching (section 7.6). The runtime picks between the two once the build side's size is exact.

## 6.9 The module

A QIR module is the unit a backend compiles and the code cache stores. Next to the functions it holds:

- **State layout:** field offsets and sizes of `PipelineState` per pipeline, as document 05 declares them. The verifier checks every `[%st + disp]` access against it.
- **Error sites:** kind, SQL operator, SQL types of the operands, plan node. From the operands the error slot receives, the runtime builds the DuckDB message text, e.g. `Out of Range Error: Overflow in addition of INT32 (a + b)!` (E §4.1). The generated code carries a 32-bit id, never a string.
- **Guards:** fact id, fallback version, and whether the guard is morsel-invariant.
- **Counters:** meaning and plan node.
- **Proxies used, and vcall kernels used,** with signatures.
- **Constant blobs:** LIKE patterns, IN-list sets, dictionary bitmaps. They are copied into query state at pipeline start, never baked into code, so that the cache can share one compiled module across parameter values (E §8.3; document 14).

## 6.10 The verifier

`rudb-qc-ir::verify(&Module) -> Result<(), Vec<VerifyError>>`. It runs on every module in debug builds and in the differential harness of document 15, and after the parser in every textual test. It is linear. Rules:

| # | Rule |
|---|---|
| V1 | every operand is a constant or defined by an instruction in a block that dominates the use (dominators come from one RPO pass; the CFG is reducible) |
| V2 | operand and result types match the opcode's signature; block arguments match parameter types |
| V3 | every block ends in exactly one terminator; no terminator appears mid-block |
| V4 | back edges target only blocks flagged `loop`; loop depth flags are consistent with nesting |
| V5 | the entry block has the ABI parameters `(ptr, ptr)`, and every `ret` returns an `i32` status |
| V6 | loads and stores relative to `%st` fall inside the declared state layout, with matching width |
| V7 | every `!E`, `!G`, `#k`, `@proxy` and `@kernel` resolves in the module tables |
| V8 | constant-size ops have `N ≤ 64`; `dup`/`ddown` scale ≤ 38; `load.bit` index type is integer |
| V9 | a `guard` is not reachable from an `effect` instruction (a store to shared state, `cas`, `atomic`, `rtcall ... effect`) within the same invocation, unless the pipeline's sink is declared `morsel-local` (document 13 §13.4) |
| V10 | a `transient` `str16` is never stored into state that outlives the morsel; it must first pass through `rtcall @str_promote` |
| V11 | every loop whose header is not the morsel batch loop contains a `poll` on each back edge, unless flagged `bounded` with a constant trip count |
| V12 | trapping instructions on possibly-invalid operands are guarded by the validity (section 6.7) |
| V13 | no instruction's result is unused unless it has an effect or traps (checked after DCE) |

V9, V10, V11 and V12 are the engine's semantic invariants, the ones that turn "a backend miscompiled it" into "the generator produced an illegal module". Each has a test module in `rudb-qc-ir/tests/illegal/` that must be rejected.

## 6.11 Passes

**There is no general optimizer, and the list below is closed.** Adding a pass needs a measurement showing that it pays for itself on a suite, against the ≤ 10 ns per instruction budget.

**Constant folding and dedup, at append.** Section 6.2. Folding covers all pure integer and float operations, `select` with a constant condition, `icmp` of constants, and the algebraic identities that come up from NULL specialization: `and x, true`, `or x, false`, `select true, a, b`.

**CSE, at append, scoped.** Pure instructions are hash-consed in a table keyed by `(opcode, type, operands)`. The table is scoped by the structured control helpers: entering an `if_else` arm or a loop body pushes a scope, leaving pops it. A hit is therefore always a dominating definition, without a dominator tree. Loads are not CSE'd, except loads from `%st` and from the morsel header, which are immutable during an invocation and flagged `.inv`. This is where the expected wins come from: the same column loaded by two predicates, the same hash computed for a probe and a later insert, the same decimal rescale feeding two comparisons.

**Loop-invariant placement, at append.** The builder knows the current loop depth. An `.inv` load or a pure instruction whose operands are all defined outside the current loop is appended to the preheader instead of the body. The helper keeps a cursor into each open loop's preheader for this. It is LICM without analysis, and it is what makes `load.ptr [%m + 32]` appear once per morsel in the example.

**DCE, one backward pass.** Mark the operands of every effect, trap, guard, terminator and `ret`, sweep in reverse, and drop unmarked pure instructions by setting a tombstone bit in the header. Backends skip tombstones. Tidy Tuples reports DCE removing about 4% of code (B §1.1). It exists because translators emit loads and hashes speculatively and some turn out to be unused, and it is cheaper to remove them than to make every translator exact.

**Guard hoisting.** A guard whose condition depends only on `.inv` values (the morsel header, `PipelineState`) is moved to the entry block, and any guards sharing a fallback are merged into one. Most facts are morsel-invariant: the morsel's encoding, its zone map range, its null count. Hoisted, they cost one test per morsel instead of one per tuple, and V9 is trivially satisfied.

**Liveness and use counts, for `direct`.** One backward pass computes, for each value, its last use offset and its use count. A value live into a loop has its interval extended to the loop's end, which is Kohn's rule as TPDE implements it (B §3.1). The result is two `u32` columns next to the arena. `direct` reads them to free registers and to decide which values are worth a register at all. `interp` uses them to reuse register-file slots. `clif` and `llvm` ignore them.

## 6.12 Provenance

**Every instruction carries a 32-bit provenance id, in a column parallel to the arena.** The id indexes an interned table of `(plan node id, generator call site)`. Every builder method is `#[track_caller]` and records `std::panic::Location::caller()`, a `&'static Location` whose address is the interning key. The cost is one pointer hash per new site and one `u32` store per instruction. The payoff:

- The printer shows `; plan=#14 at rudb-qc-gen/src/join/probe.rs:212` on every line.
- `direct` emits address-range to provenance tables, which feed perf map files, jitdump records and per-operator cycle attribution in `EXPLAIN ANALYZE` (document 16). Tailored Profiling showed that per-operator attribution in fused code costs 44 + 6 + 6 lines in Umbra and about 2.8% at normal sampling rates (B §10.4).
- A wrong result minimized to one QIR instruction names the generator line that emitted it.

Photon rejected code generation largely because "a majority of the work in using a code generating runtime… was around adding tooling and observability" (A §4.2, https://people.eecs.berkeley.edu/~matei/papers/2022/sigmod_photon.pdf). Provenance from the first commit is the cheapest answer to that.

## 6.13 The crate

`rudb-qc-ir` contains the arena, the types, the builder core (typed wrappers are in `rudb-qc-gen`, section 7.2), the printer, the parser, the verifier and the passes of section 6.11. It depends on nothing else in the workspace. The parser exists for tests and for replaying a module captured from a failing query. Round-trip is a property test: `parse(print(m)) == m` for every module the corpus produces.

## What we should take from this document

QIR is designed for the two budgets, not for optimization. It is a flat arena of 32-bit words with 4-byte references. Constants are folded and deduplicated at append, and CSE, loop-invariant placement, DCE and guard hoisting are cheap linear passes with no analysis framework behind them. If QIR generation ever needs a dominator tree or a use list, a design rule has been broken.

The choices other documents depend on are these. SSA uses block parameters. Loops are declared by the builder. `i128` and `str16` are first-class value types, and `str16` carries a static storage class. Checked arithmetic defaults to a non-terminating trap form. The hash is CRC-32C, fixed per process. Hash table probes are expanded QIR, not intrinsics. `vcall` is a batch-level call over state buffers. `guard` returns a deopt status for the morsel.

NULL does not exist in QIR. Validity is an ordinary `i1` that constant folding deletes when a fact proves non-nullness, which is how the dominant non-null case pays nothing without a second code path in any backend.

The verifier is where the engine's invariants live. V9 (no deopt after effects), V10 (no transient strings in long-lived state), V11 (poll on unbounded loops) and V12 (no traps on invalid lanes) are all bugs no backend test would catch.

Provenance is in the IR from day one, because observability is the cost that stopped Photon, and one `u32` per instruction is the cheapest place to pay it.
