# Expressions and semantics

Documents 04 through 11 are about speed. This one is about producing exactly the answer DuckDB produces while going fast. It covers the value model, NULLs, integer and decimal arithmetic, floats, strings, `LIKE`, casts and function calls. It closes with the table of places where a compiler typically drifts away from DuckDB, and the test that guards each one.

Thesis point 4 in document 00 is the contract: same rows, same types, same errors, same NULL behavior. A compiler breaks that contract in different places from a vectorized engine. It breaks it where it fuses, hoists, reorders or speculates, which are exactly the things it exists to do. So this document is organized around those four moves as much as around the types.

## 12.1 The compiled engine has no semantics of its own

**The oracle chain is DuckDB pin → first engine → compiled engine, and the compiled engine only ever answers to the first engine.** Every semantic decision is made once, in the shared frontend or in the first engine's function registry:

- which overload applies;
- what the result type of `DECIMAL(18,2) * DECIMAL(9,4)` is;
- whether rudb is bug-compatible with the pin on a `NOT IN` over NULLs.

The compiled engine implements the decision and never makes it. Where the first engine and the pin disagree, that is a `rudb-compat` issue against the first engine. It is never fixed in a translator.

**Types are fixed by A4, and the compiled engine never computes a result type.** Document 03 section 3.2 requires every expression in A4 to be typed and every implicit cast to be explicit. So the translators never infer anything:

- The decimal widening rules come from the binder.
- The rule that integer `/` returns DOUBLE comes from the binder (research-notes E section 4.1, https://rosettacode.org/wiki/Integer_overflow, https://github.com/duckdb/duckdb/issues/7094 [snippet]).
- The literal typing that makes two INT32 literals overflow in INT32 comes from the binder.
- A cast appears in A4 as a cast node, never implicitly.

A translator that finds a mismatch between operand types fails the A4 verifier. The query then goes to the first engine. The translator does not insert a cast.

**Semantics live in one layer of the translator.** Document 07 layers the generator as operators, then data structures, then tuples, then SQL values, following Tidy Tuples (https://db.in.tum.de/~kersten/Tidy%20Tuples%20and%20Flying%20Start%20Fast%20Compilation%20and%20Fast%20Execution%20of%20Relational%20Queries%20in%20Umbra.pdf). Only the SQL-value layer knows about NULLs, overflow, decimal scale, collation and error messages.

An operator translator that wants `a + b` asks the SQL-value layer for it and gets back a value. It never sees the overflow block. That is why a semantics fix is one change, not one per operator. It is also why this document can specify semantics without reference to joins or aggregation.

## 12.2 The value model

**A value in the translator is `SqlVal { val, null, ty }`.**

- `val` is one or two QIR registers.
- `null` is `Option<Reg>`: `None` when the value is proven non-NULL.
- `ty` is the SQL type from A4.

The QIR physical type follows from `ty`:

| SQL type | QIR physical | notes |
|---|---|---|
| BOOLEAN | `i1` in registers, `i8` in memory | a NULL boolean is a separate `i1` flag, never a third value in `val` |
| TINYINT, SMALLINT, INTEGER, BIGINT | `i8`, `i16`, `i32`, `i64` | signed |
| UTINYINT to UBIGINT | `i8` to `i64` | unsigned compare and overflow variants |
| HUGEINT, UHUGEINT | `i128` | two registers on both targets |
| FLOAT, DOUBLE | `f32`, `f64` | IEEE, no reassociation, no FMA (document 03 section 3.8) |
| DECIMAL(p,s) | `i16` / `i32` / `i64` / `i128` for p ≤ 4 / 9 / 18 / 38 | DuckDB's own widths [background]; the scale is a type attribute, never stored |
| DATE | `i32` days | |
| TIME, TIMESTAMP, TIMESTAMP_TZ | `i64` microseconds | the time zone is resolved at bind time or through a runtime call |
| INTERVAL | `{i32 months, i32 days, i64 micros}` | comparisons and arithmetic go through the runtime (section 12.8) |
| UUID | `i128` | DuckDB's flipped-sign-bit ordering [background], guarded by test |
| ENUM | `u8` / `u16` / `u32` code | compares on codes when the dictionary order is the enum order |
| VARCHAR, BLOB | `str`, the 16-byte header of section 12.5 | |
| LIST, STRUCT, MAP, UNION, ARRAY | opaque handle | every operation is a `vcall` until profiling says otherwise |

**Encoded values are a representation, not a type.** Document 04 section 4.6 propagates the representation of each column: decoded, dictionary code, FSST code or row id. A `SqlVal` whose representation is a dictionary code has the same `ty` as the decoded value. The SQL-value layer decides per operation whether it can run on the code:

- Equality against a constant becomes a code compare.
- A `LIKE` becomes a bitmap test (section 12.6).
- Anything else decodes first.

The decode is a load from the dictionary, which the scan hands up as a morsel-constant pointer.

## 12.3 NULLs

**Nullability is specialized, not tested.** A column whose row group has `NullCount = 0` produces `SqlVal { null: None }`, and every expression over it has no NULL code at all. A column that may contain NULLs produces a flag register from the validity bitmap. When the fact is not Exact, the per-morsel guard in document 04 section 4.7 picks between the two variants. The expectation, from the IMDb schema, is that most JOB join keys and filter columns are non-NULL, so most generated code never touches a validity bit.

**Three-valued logic, in the three places it is needed and nowhere else.**

- **Filters.** In a `WHERE` or `HAVING` conjunct, NULL and FALSE are the same thing: the row does not pass. The translator lowers a filter predicate to a single `i1` that is true only for TRUE. `a < b` on nullable inputs becomes `(a < b) & !na & !nb`, with no third state. This is the common case and costs one AND per nullable input.
- **Projections and `NOT`/`OR`/`AND` whose result is observed.** A boolean that is returned to the client, stored, grouped or passed to a function needs the full truth table. The translator carries `{val: i1, null: i1}` and applies Kleene logic: `x OR y` is TRUE if either is TRUE regardless of NULL, and NULL only if neither is TRUE and at least one is NULL. `NOT` of NULL is NULL. A filter under `NOT`, such as `WHERE NOT (a = 1 OR b = 2)`, needs the three-valued form inside the `NOT` and collapses only at the top.
- **Mark joins.** `IN` and `EXISTS` subqueries that survive as mark joins produce a tri-state match: true, false or null (research-notes C section 5.5, https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf). That paper found wrong results for NULL-ful `NOT IN` in DuckDB 1.3.0, among others. Per section 12.1, what rudb returns there is the first engine's decision. The compiled engine's mark-join translator implements the tri-state faithfully and lets the A4 plan decide how the mark is consumed.

**NULL-propagating operators compute unconditionally.** For `a + b` with nullable inputs, the translator emits the add on whatever bits are in the slots, plus `null = na | nb`. It does not branch. The value bits under a NULL are garbage, and garbage is harmless for every operation except the fallible ones. For those, the overflow flag is ANDed with `!null` before it can raise. The same holds in SIMD form: compute every lane, AND the validity masks, and mask the error lanes by validity before the deferred check (research-notes E section 4.1).

**Functions with explicit NULL behavior** (`COALESCE`, `IFNULL`, `IS [NOT] DISTINCT FROM`, `CASE`, `NULLIF`, aggregates that skip NULLs) are translated by the SQL-value layer from their definition in the function registry. `COALESCE(a, b)` becomes a select on `na`. It does not become a branch, unless `b` is fallible or expensive (section 12.4).

## 12.4 Integers and overflow

**Checked arithmetic is a QIR instruction, and the failure path is cold and shared.** Umbra IR writes `%c = checkedsadd i32 %a, %b %continue %overflow`, with a trapping form `ssubtrap` (https://home.cit.tum.de/~engelke/pubs/2403-cgo.pdf). QIR takes the same idea in the form document 06 defines. The failure successor goes to one cold block per (function, error kind, operator), which:

1. stores the operands and the instruction id into the error slot (document 13 section 13.5);
2. returns `Status::Error`.

On both targets the hot path is the arithmetic plus one conditional branch on the flags: `adds` then `b.vs` on AArch64, `add` then `jo` on x86-64.

```
; SQL: l_quantity * 100 + l_extendedprice   (INTEGER, BIGINT after the A4 casts)
%m  = mul.chk.s i64 %q, 100            -> ovf.mul.0
%s  = add.chk.s i64 %m, %p             -> ovf.add.1
...
ovf.mul.0 (cold):  raise Overflow, op=mul, ty=BIGINT, a=%q, b=100, inst=17
ovf.add.1 (cold):  raise Overflow, op=add, ty=BIGINT, a=%m, b=%p, inst=18
```

**The message is DuckDB's, byte for byte, and it is built by the runtime, never by generated code.** The template is `Out of Range Error: Overflow in addition of INT32 (a + b)!`, with the operand values in place of `a` and `b` (research-notes E section 4.1, https://rosettacode.org/wiki/Integer_overflow [snippet]). The cold block stores numbers, and `rudb-qc-rt` formats them with the same routine the first engine uses. Generated code never formats anything (document 03 section 3.6).

**Checks are elided only when a range proves them unnecessary.** The SQL-value layer carries a value range per register when one is known:

- a zone-map bound on the column;
- a constant;
- a dictionary-code domain;
- the result of a previous range-checked operation.

If the range of the result fits the type, the unchecked instruction is emitted. This is not an optimization the backend may perform on its own. The backend never sees SQL types.

**Deferred checks in batch and SIMD form.** Where the body processes a batch in lanes, for example a filter expression evaluated over the 1,024-row batch before the tuple loop:

1. The lanes compute wrapping.
2. The per-lane overflow bits are ORed into a mask.
3. The mask is ANDed with the selection and the validity.
4. The mask is tested once per batch.

A non-zero mask sends the batch to a scalar rescan. The rescan finds the failing row and produces the exact message with operand values. The rescan is part of the same generated function, reached only on the error path, so it costs code size and no time (research-notes E section 5.2, option C).

**Errors must never come from rows DuckDB would not evaluate.** This is the most important rule in the section, and it constrains three moves the compiler wants to make.

1. **Hoisting past a filter.** Computing `a * 1000000000` for all rows of a batch before a filter `a < 1000` has selected them would raise on rows DuckDB never evaluates. DuckDB evaluates later conjuncts only on rows that passed earlier ones. A hoisted fallible expression must therefore have its error mask ANDed with the selection that applies to it in the unhoisted program.
2. **Branch-free `CASE`.** `CASE WHEN x <> 0 THEN 100 / x END` must not raise on the rows where the branch is not taken. A translator that evaluates both arms as selects must mask each arm's errors by that arm's condition.
3. **Reordering conjuncts.** The permutable filters of document 10 (Menon et al., within 10% of optimal, https://www.vldb.org/pvldb/vol14/p101-menon.pdf) may reorder only infallible conjuncts. Comparisons, `LIKE`, `IS NULL`, `IN` over constants and code tests are infallible. A fallible conjunct, meaning any checked arithmetic or cast, is pinned after every conjunct that precedes it in A4 order.

DuckDB's own behavior here is not perfectly stable: whether GREATEST raises depends on rewrites (issue #12668 [snippet], research-notes E section 4.1). The engine's promise is stated in research-notes E section 5.2: it raises some error from the set the query could raise, and never an error on a row the reference would not have touched.

**Cases that are easy to get wrong, listed once.**

| expression | DuckDB behavior | why a compiler gets it wrong |
|---|---|---|
| `-x` for `x = INT_MIN` | overflow error | `neg` wraps silently on both ISAs |
| `abs(INT_MIN)` | overflow error | same |
| `INT_MIN // -1` | overflow error [background] | `sdiv` on AArch64 returns INT_MIN, while `idiv` on x86-64 traps with SIGFPE |
| `x % -1`, `x % 0` | 0, and NULL or error per pin [background] | `idiv` traps on x86-64 for INT_MIN % -1 |
| integer `/` | DOUBLE result | a translator that uses `sdiv` is wrong for every non-exact quotient |
| `x // 0` | per pin [background] | hardware trap on x86-64, zero on AArch64 |
| shifts `<<` by ≥ width | per pin [background] | x86-64 masks the count, AArch64 masks differently |
| i128 multiply | overflow error at 38 digits for DECIMAL, at 2^127 for HUGEINT | the cheap check is on 64-bit halves and misses carries |

**Integer division and modulo are never a bare machine instruction.** They lower to a QIR op whose semantics are defined on the ISA-independent result, with explicit tests for zero and for `(MIN, -1)` before the hardware instruction. The trap on x86-64 is a SIGFPE, and a signal in generated code is a crash. Document 13 section 13.5 installs no signal handler on purpose.

## 12.5 Decimals

**The binder fixes precision and scale, and the translator picks the width.** DuckDB widths are `i16`, `i32`, `i64` and `i128` for at most 4, 9, 18 and 38 digits [background]. Addition of two DECIMAL(18,0) gives DECIMAL(19,0), which crosses into `i128`, and results are capped at 38 digits (rudb PR #254 [snippet], research-notes E section 4.1).

**Arithmetic at a width boundary is the expensive case, so the translator avoids it by range.**

- **Addition and subtraction.** A4 aligns the scales with explicit casts, so these are integer adds with the checks of section 12.4. The check is against the physical width. Whether DuckDB also checks against the declared precision when the physical width has headroom is not known from the notes [background], so it is pinned by a differential test on max-precision values.
- **Multiplication.** An integer multiply of the unscaled values. The scale adds, and A4 has the result type. At `i128` the multiply-with-overflow is the costly path: four partial products and carry checks. The translator emits it only when the result type needs more than 18 digits and the operand ranges do not prove a 64-bit product fits (research-notes E section 4.1).
- **Division.** Always a runtime call (document 03 section 3.6). It involves rescaling, rounding and 128-bit division, none of which is worth generating.
- **Casts between scales.** Upscaling multiplies by a power of ten from a constant table, checked. Downscaling divides with DuckDB's rounding: half away from zero for decimal-to-decimal and decimal-to-integer [background]. The rounding is generated only for divisors that are powers of ten, as a multiply-high by a magic constant. Anything else calls the runtime.

**`SUM(DECIMAL)` accumulates in `i128` and checks once, at finalize.** An `i128` accumulator cannot overflow in any realistic row count for 18-digit inputs. Research-notes E section 4.1 recommends checking only at the final cast, which is a deferred check with no per-row cost beyond an add-with-carry.

The narrow-accumulator speculation in document 04 section 4.7 accumulates in `i64` when the zone maps prove that the sum over the morsel fits. Document 13 section 13.4 specifies how that guard stays restartable.

`AVG(DECIMAL)` is the `i128` sum plus a count, and the final division is a runtime call. Its result type and rounding come from the registry.

## 12.6 Strings

**The layout is rudb's existing 16-byte view, and the compiled engine uses it unchanged.** `StringView` in `rudb-vector/src/string.rs` is:

- a `u32` length followed by 12 payload bytes;
- a string of up to 12 bytes stored inline and zero padded;
- a longer string stored as a 4-byte prefix plus an 8-byte offset into the column's arena.

It has the shape of Umbra's German string (https://db.in.tum.de/~freitag/papers/p29-neumann-cidr20.pdf), Velox's StringView and Arrow's StringView (https://engineering.fb.com/2024/02/20/developer-tools/velox-apache-arrow-15-composable-data-management/). Two properties matter to the compiler. The zero padding makes inline equality a pair of 64-bit compares. The 8-byte offset is one load, not a buffer index plus an offset.

**The 8-byte field is a locator, and its meaning is a static storage class.** QIR's `str` type carries a storage class known at code generation time. It is never stored as tag bits in the value.

| class | locator means | lifetime | produced by |
|---|---|---|---|
| `Column` | offset into a column arena whose base is a morsel constant | while the morsel's row group is pinned | scans |
| `Const` | absolute pointer into the module's constant pool | the compiled module | literals, dictionary entries |
| `Persistent` | absolute pointer into a query-lifetime arena | the query | pipeline-breaker state: hash tables, aggregate keys, sort runs |
| `Transient` | absolute pointer into a per-worker scratch arena reset per batch | until the next batch | runtime string functions: `lower`, `substr`, `concat` |

This is Umbra's persistent, transient and temporary split, with a `Column` class added. The added class exists because rudb's views hold offsets, not pointers.

**Rule: a string crosses a pipeline breaker only as `Persistent` or `Const`.** When the tuples layer stores a `str` into a hash table, an aggregate state or a sort buffer, the translator emits a promotion:

- `Column` long strings are resolved to an absolute pointer, which is legal because the arena outlives the query.
- `Transient` long strings are copied by `rt_str_persist`.

Inline strings need neither. They are 16 bytes and self-contained, which is why every JOB key that fits in 12 bytes costs nothing at a breaker. Getting this wrong corrupts memory only on long strings, so the rule is enforced by the QIR verifier: a store of a non-persistent long-capable `str` into state fails verification. It is not left to translators.

**Equality and ordering run in the header, and memory is touched only when it must be.**

```
; a == b, both nullable-free, a: Column, b: Const "Japan"   (5 bytes, inline)
%lo  = load i64 [%a + 0]            ; length(4) | first 4 bytes
%hi  = load i64 [%a + 8]            ; bytes 4..11, zero padded
%e0  = icmp eq i64 %lo, 0x6170614A_00000005    ; folded from the constant
%e1  = icmp eq i64 %hi, 0x0000006E
%eq  = and i1 %e0, %e1
```

- **Against a constant of at most 12 bytes,** equality is these two compares with no branch on length.
- **Against a longer constant,** it is the first compare (length and prefix), then a `memcmp` of the rest against the constant pool. The `memcmp` is inline for up to 32 bytes and a runtime call beyond that.
- **Column against column,** such as a join key, uses the first compare. If that matches and the length is at most 12, it compares the second word. Otherwise it resolves both locators and calls a length-specialized compare.

**Ordering** compares the 4-byte prefix as a big-endian integer, a byte swap and then an unsigned compare. It falls through to `memcmp` only when the prefixes are equal. Comparing the little-endian prefix word directly is the classic bug, and section 12.9 lists its test.

**Collations other than binary are function calls.** DuckDB applies `NOCASE` and `NOACCENT` as expressions at bind time [background]. They arrive in A4 as function nodes and are `vcall`s unless profiling adds a translator. The compiled engine has no collation concept of its own.

**UTF-8 is assumed valid, as it is in the storage layer.** Byte-level operations are correct for equality, ordering (binary collation orders UTF-8 by code point) and `%`-only patterns, because UTF-8 is self-synchronizing. `_`, `length`, `substr` and `upper`/`lower` count code points. The inline translators for these handle ASCII with a fast check: all bytes of the 16-byte header, or the whole string, below 0x80. Otherwise they call the runtime.

## 12.7 LIKE and string predicates

**A LIKE pattern is compiled at code generation, and the pattern's shape is baked into the code.** For a prepared statement whose pattern is a parameter, the shape is part of the specialization key (document 14 section 14.3). A new value with the same shape reuses the code with the segment bytes loaded from state, while a new shape compiles again. The shapes:

| shape | JOB example | generated code |
|---|---|---|
| no wildcard | `k.keyword = 'character-name-in-title'` as LIKE | the equality of section 12.6 |
| `abc%` prefix ≤ 4 | `'USA:%'` | one masked compare on the prefix word; no dereference even for long strings |
| `abc%` prefix ≤ 12 | `'Japan:%'` | length test plus compare on the header; dereference only if the string is long and the prefix is longer than 4 |
| `%abc` suffix | `'%(voice)'` | length test, compare of the tail |
| `%abc%` contains | `'%(200%)%'` | segment search, below |
| multi-segment | `'USA:% 200%'` | anchored prefix, then an ordered segment search from the end of the previous match |
| with `_` | `'%(19__)%'` | fixed skips; code-point skips unless the value passes the ASCII check |
| `ESCAPE`, `SIMILAR TO`, `regexp_*` | | runtime call; regex is never generated (document 03 section 3.6) |
| `ILIKE` | | ASCII case folding inline when the pattern and the header are ASCII; the runtime otherwise |

**Segment search follows "Teach Your DBMS to LIKE Strings".** Split on `%` and match each segment with a SIMD primitive when it is short, Boyer-Moore when it is medium and Two-Way when it is long. Skip tables are built at code generation. Leading and trailing `_` become fixed skips, and prefix patterns are checked on the inline prefix. That work measured the compiled filter at 13.3x over DuckDB on a filter stress query (arXiv 2608.23307, research-notes C section 7.3).

The notes do not give its segment-length cutoffs. We start with these as tunables and fix them in C7 by measurement [derived]:

- up to 16 bytes: a SIMD first-and-last-byte search, generated inline;
- 17 to 64 bytes: Boyer-Moore-Horspool, as a runtime call with a table from the constant pool;
- over 64 bytes: Two-Way, as a runtime call.

The inline loop carries the counted cancellation check of document 13 section 13.6, because a single string can be megabytes long.

**OR-ed patterns are fused.** JOB 15c has `mi.info LIKE 'USA:% 199%' OR mi.info LIKE 'USA:% 200%'`, and 19a has `'Japan:%200%' OR 'USA:%200%'` (research-notes C section 1.3).

- The translator factors the common anchored prefix and tests it once. `'USA:'` is exactly the 4-byte prefix word, so the test costs one compare.
- It then searches for the set of next segments in one pass.
- For two or three segments, that pass is a SIMD search on the rarest shared byte (`' '`) followed by a compare against each candidate.
- Beyond a handful of patterns, it builds an Aho-Corasick automaton at code generation and calls the runtime matcher. The same paper's wildcard join uses Aho-Corasick for the column-of-patterns case at up to 30.6x over DuckDB v1.4.4.

**Encoded inputs change where the predicate runs, not what it means.**

- **Dictionary-coded input.** The predicate runs once per dictionary entry when the morsel starts, producing a bitmap over codes. The per-row test becomes a bit test. The cost is proportional to the dictionary size, which pays on low-cardinality columns such as `keyword.keyword`, `info_type.info` and `kind_type.kind`, not on `movie_info.info` (research-notes C section 7.6). The bitmap is cached per (dictionary, predicate) for the query, because JOB row groups share dictionaries when the storage layer allows it.
- **FSST-domain matching.** 2.5 to 17x over decompress-then-match, DaMoN 2026 (research-notes C section 7.4 [snippet]). It is a C7 item, used where the storage layer exposes FSST symbol tables as a fact.

Both are chosen by the physical plan with a guard on the per-row-group encoding (document 04 section 4.7). Both must produce the same bits as the value path, which is a tier-differential test in document 15.

## 12.8 Casts, functions and what goes through `vcall`

**Inline casts are the ones on the hot path of the benchmarks.**

- Integer widening, which is free.
- Narrowing, with a range check.
- Integer to decimal, and decimal to decimal (section 12.5).
- Integer and decimal to DOUBLE.
- DATE to TIMESTAMP.
- Numeric to BOOLEAN.

Everything that parses or formats text goes to the runtime: `VARCHAR` to number, number to `VARCHAR`, and date and time parsing. The same applies to anything involving a time zone, and to anything whose DuckDB definition includes rounding choices we have not reproduced inline and tested.

**Casts from float to integer or decimal are always a runtime call until C8.** DuckDB rounds to nearest [background] and raises on out-of-range values and NaN. Rust's `as` truncates toward zero and saturates. The hardware instructions differ between the two ISAs in how they handle NaN and out-of-range values. Two of the three are wrong, and the third needs care. An inline version lands only with a differential test over the full edge set in section 12.9.

**Floats.**

- **Evaluation.** Operations evaluate in IEEE order with no reassociation and no contraction into FMA (document 03 section 3.8). `SUM(DOUBLE)` is order-dependent, so the combine order of partial sums is fixed by morsel index (document 11) to keep tiers bit-identical.
- **Grouping and join keys.** Before hashing or equality, keys normalize `-0.0` to `0.0` and every NaN to one canonical NaN.
- **Comparison.** Ordering treats NaN as greater than every other value and equal to itself, which is DuckDB's total order [background]. A translator that emits a raw `fcmp` for `ORDER BY` or `MIN`/`MAX` gets NaN wrong. So the SQL-value layer lowers float comparisons to the total-order form: an integer compare on sign-adjusted bits.

**Everything else is `vcall`.** A `vcall` gathers the argument values of up to 1,024 selected tuples into a vector buffer and calls the first engine's kernel from the function registry. It then reads the results back (document 03 section 3.4, document 06). The kernel is the definition, so its semantics are the first engine's by construction. That makes `vcall` the safe default: a function moves to an inline translator only when profiling shows it on a hot path and a differential test covers its edge values.

Interval arithmetic, `date_trunc`, `strftime`, list and struct functions, and hashing functions other than the engine's internal hash all start as `vcall`.

**A `vcall` splits the tuple loop.** The body has to materialize arguments for a batch before the call and resume per tuple after it. Document 07 specifies the split. It is the same stage boundary as a staged probe (document 10), and it costs the same: one store and one load per argument and result per tuple.

## 12.9 Where compilers diverge from DuckDB, and the test that guards each

Every row is a test in `rudb-compat` or in the tier-differential suite of document 15. A translator that touches the behavior in a row does not merge without that test passing on every tier (interp, `direct`, `clif`, and `llvm` when enabled) and against the first engine.

| # | divergence | typical cause in a compiler | guarding test |
|---|---|---|---|
| 1 | error raised on a row DuckDB never evaluates | hoisting a fallible expression past a filter, branch-free `CASE`, conjunct reordering | fallible expressions under selective filters and in untaken `CASE` arms, over INT_MIN/INT_MAX columns; forced permutation of filter order per morsel |
| 2 | missing overflow error | range analysis wrong, or unchecked op on a narrowing path | expression fuzzer over edge values: INT_MIN, -1, 0, 1, INT_MAX, DECIMAL(38) max, and NULL |
| 3 | wrong error text or operand values | message built from the wrong instruction after a deferred check | overflow in batch/SIMD paths, compared to the first engine's message byte for byte |
| 4 | `INT_MIN // -1`, `x % -1`, `x // 0` | bare `sdiv`/`idiv` | explicit cases per type width on AArch64 and x86-64 CI |
| 5 | string ordering wrong for strings sharing a length but differing in the prefix | little-endian prefix compare | property test: `ORDER BY` over random bytes versus `memcmp` order, lengths 0 to 40 |
| 6 | equality wrong at the 12/13-byte boundary | non-zero padding, or length not in the first word | strings of 11, 12, 13 and 16 bytes with common prefixes, as keys, in `GROUP BY`, `JOIN` and `DISTINCT` |
| 7 | freed or overwritten string in results | `Transient` or `Column` long string stored past a breaker | long-string join and aggregation keys with memory poisoning of scratch arenas in debug builds |
| 8 | `_` matches a byte, not a character | byte-level matcher on non-ASCII values | `LIKE` with `_` over UTF-8 multi-byte data; `length` and `substr` on the same |
| 9 | `-0.0` and `0.0` in different groups; NaN groups split | hashing raw float bits | `GROUP BY` and `JOIN` on float columns with signed zeros and NaNs |
| 10 | NaN sorted or compared wrongly | raw `fcmp` | `ORDER BY`, `MIN`, `MAX` and `<` over NaN, ±inf, ±0 |
| 11 | float to int rounding or saturation | Rust `as`, hardware conversion | the cast matrix over .5 cases, ±2^31, ±2^63, NaN and inf |
| 12 | `SUM(DOUBLE)` differs between tiers | reassociation, FMA, or combine order | tier-differential with bit comparison; forced tier switch at each morsel |
| 13 | decimal rounding on downscale | truncation instead of half away from zero | casts of ±x.5 at each width boundary (4/9/18/38) |
| 14 | `NOT IN` with NULLs differs from the first engine | mark join simplified to anti join | the CIDR 2026 query set (https://www.vldb.org/cidrdb/papers/2026/p3-birler.pdf) run on both engines |
| 15 | NULL key matches in a join | NULL treated as a value in the key compare | NULL-bearing join keys with `=` and with `IS NOT DISTINCT FROM` |
| 16 | dictionary or FSST path differs from the value path | predicate on codes misses a collation or trailing-byte case | every string predicate run with the encoding guard forced both ways |
| 17 | 3VL collapsed under `NOT` | filter collapse applied below `NOT` | `WHERE NOT (a = 1 OR b = 2)` with NULLs; TLP partitioning from rudb-compat PR #71 (https://github.com/tamnd/rudb-compat/pull/71 [snippet]) |
| 18 | shift and bit operations on out-of-range counts | ISA count masking | shift counts -1, 0, width-1, width and 255 per type |

TLP (https://www.manuelrigger.at/preprints/TLP.pdf [snippet]) and NoREC are the general oracles for rows 1, 15 and 17. NoREC is especially apt for a compiler, because it compares filter code against projection code for the same predicate, and those are two different translator paths (research-notes E section 7.1).

## What we should take from this document

The compiled engine makes no semantic decisions. Types, casts and overloads are fixed in A4. Function definitions are the first engine's, reached through `vcall` until an inline translator has earned its place with a differential test. When the compiled engine and DuckDB disagree, the fix is either in the translator's implementation or in the first engine. It is never a new rule in the compiler.

The rule that costs the most engineering is that no error may come from a row DuckDB would not evaluate. Hoisting, branch-free `CASE`, SIMD evaluation and filter reordering all break it by default. The fix is uniform: mask error bits by the selection the unoptimized program would have had, and pin fallible conjuncts in their A4 order.

NULLs cost nothing where the data has none, because nullability is specialized per row group. Where NULLs exist, three-valued logic is carried only where it is observable: projections, `NOT`, and mark joins. Filters collapse to one bit.

Strings use rudb's existing 16-byte view unchanged. Its storage class is a static QIR property with four values, and it is promoted at every pipeline breaker under a verifier rule. Inline equality against a constant is two 64-bit compares, and ordering compares a byte-swapped prefix.

`LIKE` is compiled per pattern shape, OR-ed patterns are fused on shared prefixes, and dictionary-coded columns evaluate the pattern once per entry. That is C7's JOB work. Its cutoffs are measured there, not assumed here.

The divergence table in section 12.9 is the checklist. Every row has a test, every tier runs it, and a translator that touches the behavior of a row does not merge without it.
