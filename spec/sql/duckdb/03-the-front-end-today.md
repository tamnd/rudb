# The front end as it stands

This is an inventory, not a design. The design is document 04 and it only makes sense after this, because most of the seam it needs is already there and the interesting question is which parts are not.

## 3.1 The six layers

Text arrives at `crates/rudb-parse/src/tokenize.rs`, 1017 lines, a hand written lexer matched to DuckDB's `base_tokenizer.cpp` by behaviour. Its own doc calls it the one part of the front end with no declarative artifact behind it. It produces twelve byte tokens with nothing decoded: quotes, escapes, digit separators and case are all left as written.

Tokens are matched against a grammar table by `crates/rudb-parse/src/matcher.rs`, 1015 lines. Its first two lines are worth quoting because they are the reason this folder is possible: "This is a PEG matcher and nothing more. It decides where every rule in the grammar started and stopped, and it does not know what any of them mean." It is an explicit stack machine over an arena of fixed size nodes, with success and failure memoization on the 22 rules `memoized_rules.list` names.

The table it walks is `crates/rudb-parse/src/generated/rules.rs`, 8964 lines holding 1088 rules, compiled by `cargo xtask gen-grammar` from `crates/rudb-parse/grammar/`, which is DuckDB's PEG grammar vendored byte for byte. `VENDOR` records the upstream, the ref `v2.0-cyanoptera`, the commit `cc7e7bac7fcb6e0994359965a87ac4f6a96f2e17`, the retrieval date and a sha256 per file, and `cargo xtask grammar` fails if anything under that directory was written by anything other than `cargo xtask vendor-grammar`.

The parse tree is turned into rudb's own AST by `crates/rudb-parse/src/transform.rs`, 3832 lines and the largest file in the repository. It is the only module that reads rule names out of the vendored grammar, deliberately, so that an upstream rename breaks one match arm and nothing else.

The AST is `crates/rudb-parse/src/ast.rs`, 955 lines, arena based with `u32` indices and no `Box` or `Vec` inside a node. `Statement` has 8 variants and `Expr` has 14.

The AST is resolved by `crates/rudb-bind`, 4339 lines across seven files, into `crates/rudb-plan`, whose `Node` has 14 variants and whose `Expr` has 8, with a printer and a parser that round trip to a fixed point that is a test rather than a claim.

## 3.2 Which of those already has no SQL in it

**The matcher has none.** It is a general PEG interpreter over a rule table. Feed it a rule table compiled from a Cypher grammar and it will match Cypher, and nothing in the file would need to change. This is the single largest piece of luck in the tree and it was not luck, it was the decision to keep the grammar as data.

**The generated rule table is SQL, but it is data.** A second grammar is a second table. They are both `&'static [Rule]` and the matcher takes the one it is given.

**The tokenizer is SQL.** It is matched to DuckDB's tokenizer specifically, and that is correct for the job it has. Cypher and KQL tokenize differently enough that this is a per language file rather than a parameter: KQL has `|` as a statement separator at the top level, a `datetime()` literal form and timespan literals like `5m`, and Cypher has backtick quoted identifiers and `$` parameters. The right answer is a tokenizer per language producing the same `Token` type, and `token.rs` is already only 149 lines of type definitions, so the type is not the problem.

**The transform is SQL by construction and by design.** It is the containment point. A second language gets a second transform module, not a branch inside this one.

**The AST is ours and is currently the SQL AST.** This is the seam that matters and document 04 is mostly about it.

**The binder is written in SQL's clause order** and its doc says so: FROM, WHERE, GROUP BY, HAVING, SELECT, DISTINCT, ORDER BY, LIMIT. That order is not a problem for another language, it is the answer to a problem another language does not have, since KQL and Cypher are written in the order they execute. What is a problem is that the binder's coercion rules, its null ordering default, its identifier comparison rule and its function catalog are all written into it rather than handed to it.

**The plan has no SQL in it already.** Fourteen node variants, all of them relational algebra names: `Get`, `Filter`, `Project`, `Aggregate`, `Sort`, `Limit`, `TopN`, `Distinct`, `Join`, `CrossProduct`, `SetOp`, `Values`, `Dummy`, `TableFunction`. No clause names appear as identifiers anywhere in the crate, only in doc comments explaining why an operator behaves as it does. `crates/rudb-plan/src/lib.rs` states that nothing in the crate looks anything up in a catalog and that every column reference is a `ColumnBinding` rather than a name. That is exactly the property document 05 needs and it already holds.

## 3.3 DuckDB's own seam, and why it is the one to copy

DuckDB shipped v2.0 on 20 August 2026 replacing the PostgreSQL derived bison grammar it had used since the start with a PEG parser, per its own announcement. The old path was `third_party/libpg_query` into `src/parser/transform/` producing `SQLStatement`, `TableRef` and `ParsedExpression`. The new path is `src/parser/peg/` into `src/parser/peg/transformer/` producing the same three classes. Everything from `Binder` down, `src/planner/`, `src/optimizer/`, `src/execution/`, did not move.

So DuckDB has now swapped its entire parser twice while keeping the AST fixed, and the second time it did it in a shipped release without changing the binder. That is not a theory about where the seam belongs, it is a load bearing beam somebody already replaced without the building falling down. Checked 14 September 2026 against the v2.0 announcement and the current `src/parser/peg/transformer/` directory listing.

The same release added two things in the same area. `GrammarExtension`, in `src/include/duckdb/parser/grammar_extension.hpp`, lets an extension register real PEG grammar rules and a transformer for them, gated behind the `active_grammar_extensions` setting, and DuckDB's own announcement demonstrates it with BigQuery style pipe syntax. `ParserExtension` gained a `parser_override` pointer that can replace statement production entirely, gated behind `allow_parser_override_extension`. Both of those settings are on the pinned binary and both are measured in section 2.6.

The rest of the field agrees on where the line goes. Calcite validates and lowers a dialect flavoured `SqlNode` into a neutral `RelNode`, and its roughly thirty `SqlDialect` subclasses are an output concept for regenerating SQL text, not an input one. DataFusion parses with `sqlparser-rs` and its `Dialect` trait, binds into `LogicalPlan`, and its own documentation says `LogicalPlan` can be built by the SQL planner, by the DataFrame API or programmatically by a custom query language. sqlglot, which only transpiles, puts the whole of its thirty plus dialects into a `Tokenizer`, `Parser` and `Generator` triple per dialect over a shared AST. Three projects, three languages, one answer.

## 3.4 What is not in the front end and should be

**A dialect registry.** `current_dialect`, `duckdb_dialects()`, `duckdb_grammar_extensions()` and `dialect_compatibility_mode` are all unanswered. Section 1.6 argues they are level two work.

**A session.** There is no `now()`, no `current_date`, no `current_timestamp`, no session time zone and no `current_setting()`. That is why the one argument form of `age` could not be shipped with the two argument form. It blocks a family rather than a function: every `current_*` name in the 62 PostgreSQL shims, the time zone half of the 84 date and time names, and the `TIMESTAMPTZ` casts.

**Error recovery.** A PEG parse stops at the first failure, which `spec/20-the-grammar.md` section 20.11 already says. That is fine for a database and not fine for the autocomplete extension, which is one of the six loaded by default.

**A statement that is more than one statement.** `crates/rudb-bind/src/statement.rs` line 158 refuses a script of more than one statement. The corpus is full of them and the harness works around it today.

## 3.5 The honest summary

The front end is in unusually good shape for a second language and unusually poor shape for the first one. The layering is right, the matcher is general, the plan is neutral, the grammar is data and the containment is real. What is thin is everything the layers contain: 7 statements of 36, 65 functions of 1159, no window, no `WITH`, no subquery, no session and no catalog tables.

That combination argues for one thing, which document 12 then spends itself on. Do not start the second language. Build the seam that a second language would need, use it for the one dialect we have, and let the fact that it costs almost nothing extra to make it a registry be the reason it is a registry. The alternative, which is to close the SQL gap first and factor later, is the alternative every project in section 3.3's last paragraph tried before it gave up and factored.
