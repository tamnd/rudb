# Modularity: the engine as a laboratory

The request this document answers: design it so that a researcher can enhance one part, swap in a new paper, and test it easily.

That is a stronger requirement than "write clean code", and it is a different requirement from "make it extensible". Extensibility is about adding features. This is about replacing a mechanism with a different implementation of the same mechanism, and getting back a number that says which is better and by how much, on real data, without touching anything else.

## 1. The seam

A **seam** is a point in the engine where two or more published designs disagree about how to do the same thing.

A seam has six parts, and all six are mandatory. A trait with a narrow interface. A registry of named implementations. Exactly one implementation marked as the reference. At least two implementations in the tree. A policy that picks one. And a place in `EXPLAIN` where the choice is printed.

```rust
/// Everything a seam's implementations have in common. pub trait Strategy: Send + Sync + fmt::Debug {
    /// Stable, kebab-case, and part of the public settings surface forever.
    fn name(&self) -> &'static str;

    /// One line, shown by `rudb_strategies()`.
    fn describe(&self) -> &'static str;

    /// Where it came from, so `EXPLAIN` can cite it.
    fn provenance(&self) -> Provenance;

    /// Whether this implementation can handle the situation the planner is in.
    /// A strategy that says no is skipped; the reference always says yes.
    fn applicable(&self, context: &Context) -> bool;
}
```

`Provenance` is `Reference`, `Paper { title, venue, year }`, or `Ours`. It exists because the first question anyone asks about a number is what produced it, and the second is where that came from. `EXPLAIN` printing `hash.table = unchained (Birler et al., DaMoN 2024)` answers both without anybody opening a file.

## 2. The seams

This is the list. It is long because principle 2 says it should be, and it is finite because a seam is only created where there is a real disagreement in the literature or a real measurement to make.

| Seam | Reference implementation | Alternatives in tree | Milestone |
|---|---|---|---|
| `vector.form` | `flat` | `constant`, `dictionary`, `sequence`, `bitpacked`, `rle`, `fsst` | F1, F7 |
| `kernel.compare` | `decoded-loop` | `flat-specialised`, `encoded-direct`, `simd` | F1 |
| `kernel.filter` | `branchy` | `branchless`, `bitmask` | F1 |
| `expr.eval` | `tree-walk` | `program`, `fused` | F1 |
| `chunk.compaction` | `never` | `fixed-threshold`, `learned-gain` | F1 |
| `string.repr` | `heap-string` | `view-inline`, `view-block`, `dictionary-code` | F1 |
| `storage.encoder` | `plain` | `chooser-greedy`, `chooser-exhaustive`, `fastlanes` | F2 |
| `storage.dictionary` | `per-block` | `global`, `shared-across-columns` | F2 |
| `buffer.eviction` | `never-evict` | `clock`, `lru`, `sampled-predictive` | F3 |
| `spill.policy` | `fail` | `operator-chosen`, `manager-chosen` | F3 |
| `scheduler` | `single-thread` | `morsel-stealing`, `morsel-static` | F4 |
| `morsel.size` | `fixed-122880` | `fixed-N`, `adaptive` | F4 |
| `hash.function` | `siphash` | `multiply-shift`, `xxh3` | F5 |
| `hash.key` | `boxed-values` | `packed-fixed`, `inline-encoded`, `out-of-line` | F5 |
| `hash.table` | `std-hashmap` | `open-addressing-salt`, `unchained`, `linear-chained`, `global-concurrent` | F5, F6 |
| `agg.state` | `enum-accumulator` | `flat-bytes`, `columnar-state` | F5 |
| `agg.parallel` | `single` | `thread-local-merge`, `radix-partitioned`, `global-concurrent` | F5 |
| `topk` | `sort-then-limit` | `bounded-heap`, `heavy-hitter-two-pass` | F5 |
| `join.build` | `nested-loop` | `hash-build`, `partitioned-build` | F6 |
| `join.filter` | `none` | `bloom-probe-side`, `minmax-range`, `predicate-transfer` | F6 |
| `scan.materialisation` | `eager` | `lazy`, `lazy-with-cost` | F7 |
| `opt.join-order` | `as-written` | `greedy`, `dpccp`, `dphyp` | F8 |
| `opt.cardinality` | `fixed-guess` | `sketch-kmv`, `sketch-with-correlation` | F8 |
| `sort` | `comparator` | `normalised-radix`, `normalised-merge` | F9 |
| `window` | `per-partition-sort` | `segment-tree`, `streaming-frame` | F9 |
| `policy` | `default` | `pinned`, `adaptive-bandit` | F10 |
| `exchange.transport` | `in-process-channel` | `shared-memory`, `network` | F11 |

Twenty-seven seams. Each row of that table is a place where somebody can read a paper on Monday and have a number on Friday.

## 3. The granularity rule, which is the whole design

> **A seam is crossed once per chunk, never once per row.**

This is the rule that makes the cost of modularity bounded, and it is the rule that most modular engines get wrong. A `trait` whose method takes one value is an indirect call in a hot loop and costs an order of magnitude. A `trait` whose method takes a chunk is an indirect call every 122,880 rows and costs nothing measurable.

Concretely, every seam trait's methods take one of: a whole chunk, a whole column, a whole morsel, a whole partition, or a configuration decision made at plan time. None of them take a row, a value, or a single key.

```rust pub trait CompareKernel: Strategy {
    /// A whole column against a whole column, writing into a whole mask.
    fn compare(&self, op: CompareOp, left: &Column, right: &Column,
               sel: Option<&Selection>, out: &mut Mask) -> Result<()>;
}

pub trait HashTable: Strategy {
    /// A whole chunk of keys, producing a whole chunk of slots.
    fn insert(&mut self, keys: &KeyBlock, out: &mut [Slot]) -> Result<()>;
    fn probe(&self, keys: &KeyBlock, out: &mut MatchList) -> Result<()>;
}
```

Inside `compare`, the implementation is a monomorphised loop over primitives with no dynamic dispatch at all. The seam is at the door of the loop, not inside it.

There is one place where this rule is hard and it is worth naming: expression evaluation, where a tree of operators over a chunk naturally wants a call per node per chunk, and the interesting optimisation, fusion, is about *removing* the per-node boundary. [`10-expressions.md`](10-expressions.md) resolves it by making the seam the *compiler* rather than the *evaluator*: the strategy is asked once per query to turn an expression tree into a program, and the program it returns has no seams in it.

The rule is enforced by a lint. `xtask lint seams` walks every trait marked `#[seam]` and fails if any method's parameters are all scalar, and fails if any `#[seam]` trait object is called from inside a function that also contains a loop over a row count. It is a crude check and it catches the mistake that matters.

## 4. The registry

One registry per seam, built at process start, immutable afterwards.

```rust pub struct Registry<T: ?Sized + Strategy> {
    entries: Vec<Box<T>>,
    reference: usize,
    default: usize,
}
```

Registration is explicit and central, a single `register.rs` per crate listing everything, rather than by inventory or linker tricks. The reason is that the list is a public artefact: `rudb_strategies()` returns it, `EXPLAIN` names from it, the sweep enumerates it, and a mechanism that discovers implementations magically produces a different list on different platforms. A researcher adding an implementation adds one line to that file, and the fact that they had to is a feature.

Out-of-tree registration exists for the case where somebody does not want to fork: `rudb-extension` can register a strategy through the C ABI. It is not the main path, because a strategy behind an ABI cannot be inlined and its number is therefore not comparable to an in-tree one. That caveat is printed next to any measurement involving one.

## 5. Context: what the planner tells a strategy

`applicable()` and the policy both need to know the situation. `Context` is the situation, and its content is deliberately small and deliberately static, it is what the planner knows, not what the runtime discovers.

```rust pub struct Context<'a> {
    pub seam: SeamId,
    pub types: &'a [LogicalType],
    pub estimated_rows: Option<u64>,
    pub estimated_distinct: Option<u64>,
    pub forms: &'a [FormSet],       // what the input columns can arrive as
    pub memory_budget: u64,
    pub thread_count: usize,
    pub settings: &'a Settings,
}
```

The `forms` field is the one that makes principle 4 work: a hash table strategy can say "I am applicable only when the key column arrives as dictionary codes", and the planner will then prefer a plan that does not decode. That is the mechanism by which layout decisions propagate upward into operator choice instead of being erased at the scan.

## 6. The policy

The policy answers: given a seam and a context, which registered implementation runs.

```rust pub enum Policy {
    /// Always the reference. This is what the oracle runs under.
    Reference,
    /// Always this one, by name. Set by flag, session setting, or hint.
    Pinned(&'static str),
    /// A hand-written rule per seam, using `Context`. The default.
    Default,
    /// A contextual bandit over the applicable set. F10.
    Adaptive(BanditState),
}
```

Four things are true of every policy and are tested.

It never changes an answer. The oracle in [`15-testing.md`](15-testing.md) runs the corpus under `Reference` and under `Default` and diffs.

It is reproducible. A plan produced under `Adaptive` records the choices it made, and re-running that plan with those choices pinned reproduces the run. An adaptive engine whose runs cannot be replayed is an engine whose benchmark numbers cannot be investigated.

It is bounded. `Adaptive` explores with a decaying budget and hysteresis, and the exploration cost is a line in the metrics document.

It is visible. Every choice appears in `EXPLAIN`, with its provenance, and with a marker saying whether it came from the default rule, a pin, or the bandit.

The ordering matters and it is the reason F10 is cheap. `Default` is a hand-written rule, one per seam, perhaps twenty lines each, and it is what ships for most of the project's life. `Adaptive` is the same registry with a learner in front of it, and the paper that describes the learner, Piece of CAKE, lands as one file rather than as an architecture.

## 7. What "easy to test a new paper" means in practice

The workflow, end to end, for somebody who wants to try a hash table from a paper published next month.

```
1. crates/rudb-hash/src/tables/mine.rs — implement HashTable and Strategy.
2. crates/rudb-hash/src/register.rs — one line.
3. cargo test -p rudb --features oracle
        runs the whole corpus against your table, differentially, against
        std-hashmap. Any disagreement is your bug and the failing query is printed.
4. rudb-bench sweep --seam hash.table --suite clickbench --machine server3
        runs 43 queries once per registered table, holding all other seams fixed,
        and emits a table of CPU seconds, wall clock, peak RSS and bytes read
        per implementation per query, with the distribution and the publishability
        verdict attached.
5. rudb-bench ledger --seam hash.table
        appends to the committed ledger.
```

Step 3 is the one that costs a researcher nothing and would otherwise cost them a week. It works because the corpus is shared, because the reference is never deleted, and because the seam is narrow enough that "same inputs, same outputs" is checkable.

Step 4 is the one that makes the result publishable, because it holds everything else fixed. The commonest failure in this kind of comparison is that the new implementation is measured against a different build, a different machine or a different dataset; the sweep makes that impossible by construction, and `fleet.rs` refuses to mark a result publishable on a machine that is not the reporting machine.

There is a fifth step for the honest case where the new table is better on some queries and worse on others, which is the usual case. `rudb-bench sweep --pareto` reports the frontier and the per-query crossover, which is the input to writing the `Default` rule for that seam.

## 8. Module boundaries

Seams need the crate graph to agree with them. The rules, enforced by `xtask lint deps`:

A seam's trait lives in the lowest crate that needs it. Its implementations live in sibling modules under that crate, or in a dedicated crate when the implementations are large, `rudb-hash` holds every hash table, `rudb-sort` every sort.

No crate depends on a crate that holds implementations of a seam it does not use. The planner depends on the registries, not on the implementations.

No implementation depends on another implementation of the same seam. They may share helpers, which live one level up.

A crate that holds implementations exports no types of its own into the plan. If `rudb-hash` needs the planner to know something, it says it through `Context` and `applicable()`, not through a new plan node. This is the rule that keeps the number of seams from growing into a second architecture.

The full crate layout is [`17-code-layout.md`](17-code-layout.md).

## 9. Best practices, stated as rules rather than as virtues

These are the ones that specifically serve the researcher-swappability goal. General Rust hygiene is assumed and is already enforced by the workspace lints.

**Every public trait method documents its contract in terms the reference implementation satisfies.** Not "returns the matching rows" but "returns each matching pair exactly once, in build-side-then-probe-side order, with nulls excluded". An implementation cannot be checked against a contract that is not written.

**Every seam has a property test that expresses its contract, and the test is generic over the registry.** Adding an implementation adds it to the property test automatically. This is where most of the confidence comes from and it is about three hundred lines total.

**No seam implementation allocates outside the buffer manager.** Principle 5. It is also what makes a new implementation's memory behaviour measurable rather than invisible.

**Determinism is a property of a strategy, declared.** `fn deterministic(&self) -> Determinism` returns `Exact`, `PerThreadCount`, or `None`. Float sum under `radix-partitioned` is `PerThreadCount`; the oracle knows to compare within tolerance instead of exactly. Declaring it is what lets the test suite stay strict everywhere else.

**Errors carry the seam.** A failure inside `unchained` says so. A researcher debugging their own implementation should never have to guess whether the stack they are looking at is theirs.

**No feature flags for strategies.** Everything is compiled in and selected at runtime. Compile-time selection makes the sweep a build matrix and makes the ledger incomparable across rows. The binary is larger and nobody cares.

## 10. What this does not make modular

Worth stating, because a document like this invites the belief that everything is swappable.

The type system, the SQL dialect, the binder and the plan IR are not seams. They are the compatibility surface with DuckDB and they have one correct answer, which is DuckDB's answer. A researcher who wants to change the plan IR is proposing a different project.

The metrics schema is not a seam. It is versioned and it evolves, but there is one of it, because comparability across the whole ledger is the point.

The buffer manager's ownership of memory is not a seam. Its eviction policy is. The distinction is that the invariant, one manager, all allocations, is what every other seam's measurability depends on.
