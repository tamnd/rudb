# The data model

Compact is one of the four things the user asked to be designed properly. This document is where compactness stops being an adjective and becomes a type.

## 1. The mistake we are not making

The standard in-memory model, Arrow's, and v1's `Vector`, is: a column is a flat buffer of fixed-width values, plus a validity bitmap, with a small set of escape hatches for constants and dictionaries. Encodings live on disk and are undone at the scan.

That model is why the Rust and Arrow ecosystem sits at 45 seconds on a board where DuckDB sits at 26. It is not a Rust problem. It is that every query pays to materialise data into a representation chosen for uniformity rather than for the query, and then all subsequent cleverness is spent making operations on that uniform representation fast.

The alternative is not exotic. It is what Umbra and ClickHouse already partly do and what [`02-research-2026.md`](02-research-2026.md) section 1.1 prices at an order of magnitude: keep the compact representation, and teach the operators to work on it.

## 2. Column and form

```rust pub struct Column {
    logical: LogicalType,
    len: u32,
    validity: Validity,
    form: Form,
}
```

`LogicalType` is DuckDB's type. It never changes as data moves through the engine. `Form` is how those values are physically represented right now, and it changes constantly.

```rust
#[non_exhaustive]
pub enum Form {
    /// One value per slot, natural width. The reference form.
    Flat(Buffer),

    /// One value, len times.
    Constant(Scalar),

    /// start + i * step. Free row numbers, free date ranges.
    Sequence { start: i128, step: i128 },

    /// codes[i] indexes values. `values` is itself a Column, and the
    /// invariant is that it is never a Dictionary. Depth is exactly one.
    Dictionary { codes: Buffer, values: Arc<Column>, source: DictSource },

    /// Values packed at `width` bits with `base` subtracted. FastLanes
    /// interleaving, so a compare is a compare and not a shuffle.
    BitPacked { data: Buffer, width: u8, base: i128 },

    /// Run-length. `ends` is exclusive prefix sums, so a binary search
    /// maps a row index to a run.
    Rle { values: Arc<Column>, ends: Buffer },

    /// FSST-compressed strings, symbol table shared at block scope.
    Fsst { data: Buffer, offsets: Buffer, symbols: Arc<SymbolTable> },

    /// 16-byte views: 4-byte length, then 12 bytes inline, or
    /// 4-byte prefix + 4-byte block + 4-byte offset.
    StringView { views: Buffer, blocks: Arc<StringBlocks> },

    /// Struct, list and map, as child columns plus structure.
    Nested(Box<Nested>),
}
```

`Validity` is `AllValid`, `AllInvalid`, or `Mask(Bitmap)`, unchanged from v1, that design is right and there is no reason to differ.

`#[non_exhaustive]` is load-bearing. `Form` is a seam (`vector.form` in [`04-modularity.md`](04-modularity.md)) and the variant list is expected to grow when somebody implements a new encoding from a paper. Every consumer must have a fallback arm, and that fallback arm is `flatten()`.

## 3. The one rule about flatten

```rust impl Column {
    /// Decode to `Form::Flat`. Costs a full materialisation.
    pub fn flatten(&self) -> Result<Column>;
}
```

Every call site is counted. The metrics document has a `decoded_bytes` and a `decode_sites` field, and `EXPLAIN ANALYZE` prints, per operator, how many rows it decoded and why, the `why` being the name of the kernel that had no encoded path.

This is the single most important instrument in the engine, because principle 4's failure mode is silent. An engine that decodes everything still returns the right answers; it just sits at 45 seconds. The counter is what turns "we should execute on encoded data" from an aspiration into a number that goes down.

v1 found the equivalent problem from the other end, `Vector::value_at` called per row from 31 sites across five kernel files, which it called the single most valuable thing found in six months of building. The lint that catches it is inherited verbatim: no `Scalar` may be constructed inside a loop over a column's length, checked by `xtask lint rowloop`.

## 4. Unified access, for the fallback only

Kernels that have no specialised path need one way to read any form. DuckDB's `UnifiedVectorFormat` is that mechanism and it works: a data pointer, a validity mask, and a selection, read as `data[sel[i]]`.

We keep it, with one change: it is explicitly the slow path, it is instrumented, and the instrumentation is on by default rather than behind a debug flag. `UnifiedAccess` is what a new kernel is written against on day one and what it graduates away from as its specialisations land.

ClickHouse's alternative, dispatch once on the concrete column type, then run a monomorphised loop, is what the specialisations do. The two are not in competition; they are the two ends of the ladder, and the whole design is about climbing it in the places that matter, measured by the counter in section 3.

## 5. The chunk

```rust pub struct Chunk {
    columns: Vec<Column>,
    len: u32,
    selection: Option<Selection>,
    /// Ownership of any block-scoped resources the columns point into:
    /// string blocks, symbol tables, dictionaries, buffer manager pins.
    holds: Holds,
}
```

Three decisions, all inherited from v1 because v1 got them right.

`Selection` is a `Vec<u32>` of surviving row indices, threaded rather than applied, so a filter is a write to a small integer array instead of a copy of every column.

`Holds` is a refcounted pin handle rather than a Rust lifetime parameter on `Column`. A lifetime would be more elegant and would make `Chunk` un-`Send`, and a `Chunk` that cannot cross a scheduler queue is not usable by [`09-parallel-and-distributed.md`](09-parallel-and-distributed.md). This is one of the places where the borrow checker's preferred answer is the wrong answer.

Chunk width is `122880` rows by default rather than v1's 1024. The reason is that 122,880 is 60 × 2048, DuckDB's row group size, and aligning the chunk to the storage block is what makes a whole-block encoded operation possible, a dictionary is block-scoped, an RLE run does not cross a block, a bit-packing base is per block. A chunk narrower than a block forces every encoded kernel to handle partial state. The inner loops inside a kernel still work in 2048-row tiles for cache reasons; that is a kernel implementation detail, not a chunk property. The width is a seam (`morsel.size`) and is swept at F4 over 2048, 8192, 32768, 122880 and 1048576.

## 6. Compactness, quantified

The four mechanisms, with what each is worth on `hits`, from M1's measurements in [`../02-the-goal.md`](../02-the-goal.md).

**Per-column encoding chosen by search.** M1 built this and it works: the chooser picked a six-level nest for `URL`. It took the file to 11.65 GB. This is `storage.encoder`.

**Front coding on sorted dictionaries.** Three columns, 52% of the file, from 6.11 GB to 4.44 GB, and the whole file from 11.65 GB to 9.65 GB. The single biggest lever M1 found.

**Global dictionaries.** Not yet built, and the one with the most leverage on execution rather than on size, because it turns a string group key into a `u32`. Section 7.

**Cross-column structure.** Measured and mostly absent: of 5,460 column pairs exactly one is worth a shared dictionary, and of eight named functional dependencies exactly one is a rule worth 18 MB. This is the mechanism [`../02-the-goal.md`](../02-the-goal.md) hoped for and M1 refuted, and the refutation is why the on-disk claim was amended from 10x to the measured 2.1x. It is recorded here so that nobody re-runs the experiment.

The honest position on compactness: 9.65 GB against DuckDB's 20.46 GB is 2.1x, Umbra is at 8.30 GB, and the target of 2.05 GB was missed by 4.7x. Compactness is not where the remaining wins are. What the compact form is *for* is section 7.

## 7. The global dictionary

The decision that makes the data model an execution decision rather than a storage decision.

A dictionary scoped to a block lets you store `URL` in 4.44 GB. A dictionary scoped to the whole column lets you *group by* `URL` without ever looking at a string. That is the difference between 52% of the file and ClickBench Q18, Q32, Q33 and Q34, four of the seven queries that make up 70.4% of Umbra's total.

The design:

A column may carry a **column-scoped dictionary**, built at write time, stored once, with a stable `u32` code space. Blocks store codes into it directly. `DictSource::Global(dict_id)` on a `Form::Dictionary` means the codes are comparable across blocks, across morsels and across threads, which is exactly the property a hash aggregate needs.

The cost is three things and all three are real. The dictionary must be built before the data can be written, which means the write path sees the column twice or holds the dictionary in memory, `hits` streaming 105 columns already peaked at 1002 MB resident and this makes it worse. The dictionary is a global object that must be resident during the query, which for high-cardinality columns is not small. And appends after the fact must either extend the code space or fall back to block scope, which makes it an *analytical* feature rather than a general one.

The resolution, and it is the honest one: a global dictionary is opt-in per column at load time, chosen by a heuristic with an explicit override, and columns that do not get one fall back to block scope with no loss of correctness. The heuristic is distinct-count from the KMV sketch against a size budget. `EXPLAIN` prints which columns have one. The measurement that decides whether the heuristic is right is F7's, not F2's.

The relationship to ordering is the second-order win and is worth stating: a dictionary whose codes are assigned in sorted order makes range predicates on codes exact, which means `WHERE URL LIKE 'http://x%'` becomes a code range comparison. Front coding already requires the dictionary to be sorted, so this costs nothing extra and M1 has already paid for it.

## 8. Form negotiation

The mechanism by which the encoded form survives past the scan.

At physical planning time, every operator declares two things: the set of forms it can consume per input column, and the set of forms it can produce per output column.

```rust pub trait FormAware {
    fn accepts(&self, input: usize) -> FormSet;
    fn produces(&self, input_forms: &[FormSet]) -> FormSet;
}
```

The planner propagates from the scan upward. Where a producer's form set and a consumer's accepted set intersect, nothing happens. Where they do not, the planner inserts an explicit `Decode` node, visible in `EXPLAIN`, counted in the metrics, and attributable to the operator that forced it.

The point is not that decoding is avoided. Most of it is not, especially early. The point is that every decode has a name and an owner, so the list of "operators that force a decode" is a work queue sorted by cost, which is what drives F7.

`FormSet` is a bitset over `Form` discriminants, so propagation is a fixed number of `u32` operations per column per plan node and costs nothing at planning time.

## 9. Nulls, and why they are not a form

Nulls stay in `Validity`, separate from `Form`, even though several encodings have a natural null representation, a reserved dictionary code, a sentinel in a bit-packed range.

The reason is that combining them doubles the number of cases every kernel must handle, and the measured benefit on `hits` is small because most of the columns that are heavily encoded are not heavily null. When a kernel wants the fused representation it can ask for it, `Validity::as_dictionary_code()` returns `Some(code)` when the planner arranged for one, but the general contract keeps them apart.

## 10. What this costs, and the F1 gate

The cost is the `Form` match in every kernel's entry, once per chunk, and the `FormSet` propagation at plan time. Both are noise.

The real cost is written code: every kernel that wants to be fast needs a specialisation per form combination that matters, and the combinatorics are only survivable because they are generated. One macro produces the flat×flat, flat×constant, and dictionary×constant specialisations for every binary operator, and everything else falls through to `UnifiedAccess` with the counter incrementing. v1 specified exactly this and it is right.

The F1 gate: CPU seconds down 5x against the F0 baseline on ClickBench Q1 through Q5 and TPC-H Q1 and Q6, with the same query run under `--set expr.eval=tree-walk --set kernel.compare=decoded-loop` for the switched-off number, both in the ledger.
