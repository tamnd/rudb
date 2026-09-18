# The pipeline program

Artifact 6. The physical plan says what to do and this says how, in a form small enough that a second implementation of the how is affordable.

## 8.1 Why there is an IR at all

The argument against one is good and should be stated first. rudb has working operators, they are fast, and an IR is a layer between a plan and code that already runs. Adding it buys nothing on day one and costs a printer, a parser, an interpreter and a migration.

The argument for it is document 01. `crates/rudb-exec/src/build.rs` is 1,658 lines, of which the central match over `Node` runs from line 1107 to roughly line 1520, and the rest is recognisers that run before it: a function that spots an aggregate under a project, one that answers a filter from metadata, one that spots an aggregate under a limit. The `Node::Filter` arm at line 1258 looks underneath itself for an `Node::Aggregate` and builds something different if it finds one. That is a peephole in a builder. It is there because there is no value to put it in.

The deeper cost is that every operator implements its own version of the same four things. Every one of `group.rs`, `join.rs`, `window.rs`, `topn.rs`, `sort.rs` and `setop.rs` evaluates expressions over a chunk, builds a key, maintains some state, and emits a chunk. They do it in six different ways, each tuned separately, each with its own null handling and its own selection vector handling. When somebody makes key building faster, they make it faster in one of six places.

A pipeline program is the shared vocabulary those six were going to converge on anyway. Document 02 section 2.4 is the literature: VOILA, Excalibur, the declarative sub-operator work and InkFuse all reach the same conclusion from different directions, which is that the set of primitive things a query engine does is small, maybe thirty of them, and that expressing operators as arrangements of those primitives is what makes both a vectorized back end and a compiled back end affordable from one source.

## 8.2 What a program is

A pipeline is a list of blocks and a set of state declarations. A query is a list of pipelines with a dependency order, because a pipeline that probes a hash table cannot start before the pipeline that built it finished.

```
pipeline p0  depends=[]
  state h : hashtable(keys=[u32], values=[i64 sum], shared, reserve=48MB)
  scan      t=hits parts=0..1431 out=[v0:codes32, v1:i32]
  compare   v1 lt const(20130701) -> s0
  select    s0
  hash      v0 -> v2
  insert    h keys=[v0] hashes=[v2] -> v3
  update    h.0 add v1 at v3
pipeline p1  depends=[p0]
  state h : (borrowed)
  drain     h out=[v0:codes32, v1:i64]
  decode    v0 dict(hits.UserID) -> v2
  emit      [v2, v1]
```

Three properties of that sketch matter.

**It prints and it parses**, like every other artifact in document 03 section 3.1. A lowering test is a physical plan in a text file and a program in a text file, with no data and no catalog anywhere in it.

**State is declared at the top of the pipeline, not created inside a block.** Section 8.4.

**The blocks say nothing about `GROUP BY`.** A grouped aggregate and a hash join build are the same four blocks with different state and different value types, which is the property that makes one compiler over the program tractable. If the block set had a `groupby` block it would be an operator list with extra steps, and there would be nothing to gain.

## 8.3 The block set

Closed and small. The target is thirty and the rule is that a new block needs an argument for why it is not an arrangement of existing ones.

**Data movement.** `scan` a part range, `decode` a column from an encoded representation to a wider one, `gather` values at positions, `emit` a chunk downstream, `buffer` a chunk into a materialization point, `drain` a state or buffer into chunks.

**Expression.** `compute` a pure expression tree over vectors, producing a vector. This is one block rather than thirty, because the expression language already exists in `rudb-plan` and `rudb-kernels` already has a dispatcher for it. Splitting arithmetic into blocks would double the block count and buy nothing that the compiler in document 10 cannot get from the expression tree directly.

**Selection.** `compare` produces a selection, `combine` intersects or unions two selections, `select` applies one to the live set, `compact` materialises the selection away when it gets dense enough to be worth it. The selection vector is explicit in the program rather than implicit in a chunk, because whether to compact is a real decision and document 10 section 10.4 makes it per chunk.

**Hashing and keys.** `hash` a set of vectors to a hash vector, `key` a set of vectors to a comparable row-wise key, `probe` a table with hashes producing match positions, `insert` into a table producing slot positions.

**State update.** `update` applies an aggregate step to a state column at given positions. `merge` combines two states of the same declaration. Every aggregate function is an `update` with a different step, and the step comes from `rudb-kernels/src/aggregate.rs`, which already has them.

**Control.** `loop` over a partition list, `branch` on a runtime condition, `call` another pipeline for a nested case. Control blocks are deliberately last and deliberately few, because the more control flow the program has the less the compiler can do with it, and because most control flow in a query engine is at the pipeline level rather than inside one.

`window.rs` at 1,251 lines and `setop.rs` at 197 are the two that will test whether the set is complete, and the honest expectation is that window needs one or two blocks of its own for frame boundary computation. That is fine. What is not fine is a block per window function.

## 8.4 State, and why it is declared rather than constructed

This is the part the sub-operator papers spend the least time on and the part that matters most for rudb, because state is where the memory goes and memory is half of the ten times less resource claim.

Every piece of state a pipeline uses is declared at the top with a name, a kind, its key and value types, whether it is shared across threads or per thread, and how much memory it reserves. The reservation comes from document 07 section 7.7 and it is a number the physical planner computed from a fact.

Three consequences.

**The memory footprint of a query is known before it runs**, by summing the declarations. That is the thing that makes an admission control decision possible, and it is not possible today because memory is allocated inside operators as they discover they need it.

**Per thread and shared are a declared property rather than a structural one.** `rudb-pipeline`'s `Sink` trait already has `type Local`, `local()` and `combine(&self, local: Self::Local)`, and `combine` taking the local state by value is a small, correct piece of design that makes a double merge impossible to write. A declared state maps onto that directly: a per thread declaration becomes the `Local` type, a shared declaration becomes a field on the sink, and `merge` blocks become the body of `combine`. None of the pipeline traits change.

**A state declaration is what a spill attaches to.** Document 09 section 9.5 keeps the spill decision at runtime, and what it decides about is a named declared thing with a known size and a known partitioning, rather than whatever an operator happens to be holding.

## 8.5 Two back ends over one program

The reason to have a program rather than better operators is that a program can have more than one implementation and the implementations can be checked against each other.

**The interpreter.** Walks the block list, calls a kernel per block, one chunk at a time. Every block's implementation is a function that already exists in `rudb-kernels` or is a small wrapper over one. This is what rudb does today with the intermediate value left out, so the interpreter should be no slower than the current operators once the block set is right, and if it is slower the block set is wrong.

**The compiler.** Takes a run of blocks with no materialization between them and generates one loop over rows. Document 10 owns the back end choice and the tiering policy. What matters here is the property the program has to have for a compiler to be possible at all, which is that a block's effect is a function of its inputs and its declared state and nothing else. No block may reach outside its declared inputs, and no block may depend on the identity of the operator it came from.

InkFuse's result, that incremental fusion gets most of compilation's benefit without compilation's latency, is why the interpreter is not a temporary scaffold. Both back ends are permanent, the interpreter is the reference, and a query that runs for three milliseconds should never wait on a compile.

**The differential test writes itself.** Same program, both back ends, same rows. Document 13 makes that a required test rather than an available one, and it is the single highest value test this architecture unlocks, because it catches the class of bug that a compiled path introduces and a vectorized path does not.

## 8.6 What happens to build.rs

Not deleted. Inverted.

Today `build(plan, node)` matches on a logical node and returns an operator. Tomorrow `lower(phys, node)` matches on a physical node and returns a list of blocks, and `build` becomes the thing that turns a program into a running pipeline, which is mechanical and has no query shape in it.

The match does not go away, and this is worth being clear about because it is the obvious objection. A match over node kinds has to exist somewhere and moving it does not reduce its size. What changes is what each arm contains. Today a `Node::Aggregate` arm decides the grouping strategy, the state layout, the key encoding and the parallelism, by reading the plan, because it is the last place that can. Tomorrow a `PhysAggregate` arm reads four fields that were already decided and emits the blocks they name. An arm that reads a field is a hundred times less likely to be edited than an arm that makes a decision, and edits per arm is the metric document 01 measured and document 13 keeps measuring.

The recognisers that run before the match are a separate story and they go somewhere specific. `certain_filter`, the metadata-only answer path, and the aggregate under a project or limit are all plan rewrites that landed in the builder because the builder is where somebody was standing. They go to document 05 section 5.6's three destinations by the same routing rule as the peepholes in `group.rs`.

## 8.7 Types

Four widths and a validity bit. The program's type system is not the SQL type system and it must not become one.

A block knows that it is operating on 8, 16, 32 or 64 bit values, on a variable length value, or on a fixed size struct of those. It does not know about `DECIMAL(18,4)` versus `BIGINT`, because those have the same representation and the same kernel, and a program that distinguishes them has one more path than it needs. Semantic types stay in `rudb-plan` and the physical planner, which is where a cast is inserted and where overflow behaviour is decided.

This is the part of the design with the most room to go wrong and the rule that keeps it honest is that a type in the program must change which machine instruction runs. If two types compile to the same loop they are one type.

## 8.8 What is not in a program

**No test on query shape.** Document 03 section 3.3's corollary applies here more than anywhere, because this is the last artifact above the loop. A program may branch on data. It may not branch on how many aggregate calls there were.

**No cost.** The decisions were made in artifact 5 and a program that carries a cost invites a second cost model.

**No catalog access.** A program refers to a part range and a dictionary handle, not to a table name it looks up. This is what lets a program be a value that survives being stored, shipped to another thread, and replayed in a test with no database open.

**No allocation outside a declaration.** A block that allocates is a block whose memory nobody counted.

## 8.9 Getting there without a rewrite

The migration is the reason to believe this is affordable and it has one property that makes it safe: **a program whose body is one block that calls an existing operator is a legal program.**

So the first version of every physical node lowers to a single opaque block wrapping the operator that runs it today. The plumbing lands, the printer and parser land, `EXPLAIN (PROGRAM)` prints one line per operator, and nothing is faster. Then operators come apart one at a time, in the order document 14 gives, which is aggregation first because that is where the lines and the commits are.

At every point in that sequence the engine is whole and the differential test from section 8.5 is running on whichever operators have been decomposed. There is no flag day and no branch that lives for a month.

## What we should take from this document

A pipeline program is a list of blocks and a set of declared states, printed and parsed like every other artifact, with a closed block set of about thirty and a type system of four widths.

State is declared rather than constructed, which is what makes the memory footprint of a query knowable before it runs and what gives a spill something named to attach to. It maps onto the existing `Sink::Local` and `combine` design without changing a trait.

Two back ends run the same program, the interpreter is permanent rather than scaffolding, and the differential test between them is the highest value test the architecture unlocks.

`build.rs`'s match does not disappear, it gets emptier: an arm that reads four decided fields instead of an arm that makes four decisions, and the migration starts with every operator wrapped in a single block so that nothing has to move at once.
