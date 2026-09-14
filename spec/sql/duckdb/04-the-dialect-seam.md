# The dialect seam

This is the one design decision in the folder that is expensive to change later. It is cheap now because, per document 03, most of it already holds by accident of the existing layering, and because the only thing that has to be built before any of the later languages exist is a registry with one entry in it.

## 4.1 What a dialect is

A dialect is four things bound together under a name:

1. a tokenizer, producing the shared `Token` type,
2. a grammar rule table, of the same `&'static [Rule]` shape the matcher already takes,
3. a transformer, the only part allowed to read rule names out of that grammar,
4. a semantics bundle, which is every question the binder currently answers with a constant.

The first three are per language and are not shared. The fourth is shared machinery with per dialect values, and it is the part that does not exist at all today.

Nothing below that is per dialect. The plan, the optimizer, the executor, the type system, the catalog and the storage format are one implementation and always will be. That sentence is the whole design and the rest of this document is the consequences of it.

## 4.2 Where the boundary goes, and why there are two of them

The tempting answer is one seam. There are two, and pretending otherwise is how projects end up with a Cypher parser that emits SQL text.

**Within the SQL family the seam is the AST.** DuckDB SQL, a Spark compatibility mode, pipe syntax and a PostgreSQL leaning mode are all the same language with different spellings and different defaults. They share `ast::Statement` and `ast::Expr`, they differ in grammar and in the semantics bundle. This is what DuckDB's own `dialect_compatibility_mode` is: the pinned binary accepts `SET dialect_compatibility_mode='spark'` and reports it back, which is a setting that changes function behaviour and coercion inside one parser, not a second parser. It is also what sqlglot does, which shares one AST across more than thirty dialects because they are all SQL.

**Outside the SQL family the seam is the bound plan.** A Cypher `MATCH` and a KQL `summarize ... by bin(...)` do not fit into `ast::Statement` and should not be made to. They get their own AST, their own binder, and they meet SQL at `rudb-plan`. This is what DataFusion says in its own documentation, that `LogicalPlan` can be produced by the SQL planner, by the DataFrame API, or programmatically by a custom query language, and it is the only arrangement under which a second language gets the optimizer and the executor for free without also getting SQL's parse tree shape for free.

`rudb-plan` is already fit for this and document 03 measured why: fourteen node variants that are all relational algebra names, no catalog lookups anywhere in the crate, and every column reference a `ColumnBinding` rather than a name. Document 05 is the list of what it still has to grow.

The failure mode of getting this wrong has a name and a citation. Apache AGE exposes Cypher as `cypher($$ ... $$)`, an opaque function returning an `agtype` column, which means the PostgreSQL planner sees a black box and cannot push a predicate into it or join across it. That is the seam placed at the function call boundary rather than at the plan, and it is cheap to ship and permanently slow. Checked 14 September 2026 against the AGE documentation.

## 4.3 The semantics bundle

Section 2.6 measured nineteen settings on the pinned binary that change what a query means rather than how fast it runs. Those are not nineteen special cases, they are the fields of one struct, and today every one of them is either a hardcoded constant in `crates/rudb-bind` or absent.

The bundle holds at least: identifier case handling and comparison, the null ordering default, the ascending or descending default, whether integer division truncates or promotes, whether division by zero errors or returns null, whether floating point follows IEEE or errors, whether a scalar subquery returning several rows errors, whether an integer literal in `ORDER BY` is a position or a value, whether a regex operator matches partially or fully, the lambda syntax, the default collation, whether errors render as JSON, whether warnings are errors, and the type coercion table.

The last of those is the biggest and the one that has to be a table rather than a function. Implicit coercion is where dialects actually differ. CockroachDB documents that its integer division returns a decimal rather than an integer and that float overflow returns infinity rather than erroring, both deliberate, both compatibility breaks with the engine it advertises wire compatibility with. pg_duckdb had to override the null ordering default because DuckDB's is not PostgreSQL's, and that override is in the shipping code rather than in a setting. Two projects, two ways of discovering that this belongs in a table.

The rule that keeps this honest: no code below the binder reads the bundle. The binder consumes it and emits a plan in which every one of those questions is already answered. A `Sort` node carries its null ordering explicitly rather than inheriting a default, and a `Cast` node is already in the plan rather than being implied by a coercion rule the executor would have to re-derive. Document 05 states this as the invariant it is.

## 4.4 The registry, and answering the three questions honestly

The pinned binary answers three things rudb does not. The design is to answer them with a real registry that has exactly one entry in it, not with three hardcoded strings.

`duckdb_dialects()` is a table function over the registry, one row per registered dialect, one column `dialect_name`, matching the measured shape. With one dialect registered it returns one row, `duckdb`, which is the true answer and also the correct answer.

`current_dialect` is a session setting whose setter looks the value up in the registry. An unknown value answers `Invalid Input Error: Dialect "cypher" is not installed`, which is the measured text and, more to the point, is the error of a registry rather than the error of a missing feature. Getting that exact string out of a lookup failure is worth more than it looks, because it means the day a second dialect is registered the setting works with no further change.

`dialect_compatibility_mode` is separate and is the intra SQL knob from section 4.2. The pinned binary accepts `spark`, reports it back, and answers an unrecognised value with `Not implemented Error: Enum value: unrecognized value "nope" for enum "DialectCompatibilityMode"` and a candidate list containing only `NONE`. So upstream's own enum is partly empty too. We match the observable behaviour, which means accepting what it accepts and producing that error shape for what it does not, and we do not invent members it does not have.

`duckdb_grammar_extensions()` returns the empty set with columns `name` and `description` until there is an extension mechanism, which is the honest answer and also the current upstream answer on a default build.

## 4.5 Grammar extensions against a vendored grammar

DuckDB v2.0 added `GrammarExtension`, which lets an extension contribute PEG rules and a transformer for them, gated behind `active_grammar_extensions`, and `ParserExtension` gained a `parser_override` pointer gated behind `allow_parser_override_extension`. Both settings are on the pinned binary.

This collides with something rudb does that upstream does not. The grammar under `crates/rudb-parse/grammar/` is vendored byte for byte with a sha256 per file in `VENDOR`, and `cargo xtask grammar` fails if anything other than `cargo xtask vendor-grammar` wrote there. That check is worth more than grammar extensibility and does not get weakened for it.

So an extension's rules are a separate source directory compiled to a separate rule table, and the tables are composed at registration rather than merged at generation. The vendored table stays a `&'static [Rule]` that nobody edits. Composition means the matcher takes a list of tables and resolves a rule name across them in registration order, which is a change to rule resolution in `matcher.rs` and not a change to what the matcher knows about SQL. `duckdb_grammar_extensions()` then reads the same registration list it composed from, which is the same discipline as 4.4: the introspection function reads the real structure instead of describing it.

A parser override, the thing that replaces statement production entirely, is how DuckPGQ did graph queries upstream. It is also the sharpest tool here and it stays behind the same setting name and the same default as upstream.

## 4.6 What is not parameterised

**The type system.** A dialect may not have a type the engine does not. Cypher's node and relationship values become types in `LogicalType` or they do not exist, and document 06 argues for which. The alternative, a per dialect value representation, is the AGE `agtype` outcome.

**The catalog and the storage format.** One catalog, one file format, all dialects. A graph is a pair of tables with an index, not a second store.

**The optimizer.** Per document 05, no pass may ask which language the query came from. A pass that needs to know has been given a plan that lost information, and the fix is in the plan, not in the pass.

**The error taxonomy.** Document 08 owns this. The kind before the colon is shared across dialects because it describes what the engine did, and the message after it may be per dialect because it describes what the user wrote.

## 4.7 The entry rule

ClickHouse shipped exactly this feature and then took it back. It had a `dialect` setting with `kusto` and `prql` alongside `clickhouse`, and in version 25.1 it marked both experimental after parser crash bugs, having shipped them without the fuzzing and conformance coverage the main parser gets. Checked 14 September 2026 against the ClickHouse 25.1 changelog and settings documentation.

That is the single most useful data point in this document, because it is the same design failing for a reason that has nothing to do with the design. The lesson is a rule, and it is the rule this folder ends on in document 13:

A dialect does not enter the registry until it has a corpus in the harness, a fuzz target, and a published pass rate, on the same terms as SQL. A dialect with a parser and no conformance suite is a liability wearing a feature's name, and it is worse than not shipping it, because a user who tries it and hits a crash has learned something about the whole engine rather than about one setting.
