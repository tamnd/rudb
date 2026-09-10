# The grammar

Document 04 originally said the parser is hand-written recursive descent and that we start where DuckDB ended up, minus the generator. The second half of that sentence was wrong, and this document is the correction. Where DuckDB ended up is the generator. Their v2.0 parser is driven by a declarative PEG grammar that ships in the repository as sixty one kilobytes of text under the MIT license, and that file set is the definition of the dialect this project claims to be compatible with. We vendor it.

This is the cheapest document in the specification. Everything else here is work that has to happen either way. This is the one piece of leverage that exists only because of what DuckDB shipped in August 2026, and taking it changes compatibility from a chase into a property that is maintained by a script.

## 20.1 What v2.0 changed

Until v2.0 the DuckDB parser was a bison grammar forked from PostgreSQL. There was no artifact in it worth vendoring: a `.y` file with C++ actions interleaved is a description of DuckDB's parse tree types, not a description of the language, and lifting the language back out of it means reading every action. That is why document 04's original position was reasonable when it was written.

v2.0 replaced it with a PEG parser whose grammar lives in `src/parser/peg/grammar/` as plain text with no actions in it at all. The rule bodies are pure syntax. The semantic layer is a separate transformer keyed on rule name. That separation is what makes the grammar liftable, and it was done for DuckDB's own reasons rather than for ours: they wanted a runtime-extensible grammar so an extension can add syntax, which is exactly what document 12.4 needs us to support too.

**The move matters more than the file format does, and this is the part that is easy to miss.** A PEG grammar for DuckDB SQL in this shape has existed since at least v1.4.0, but it was not the parser's. It shipped at `extension/autocomplete/grammar/`, and it drove the autocomplete extension. Checked at the tags: there is no `src/parser/peg/` directory at all in v1.4.0, v1.5.0 or v1.5.5. So for three releases that grammar was a second, independent, descriptive model of the language sitting next to the bison parser that actually decided what compiled, and nothing forced the two to agree. Vendoring it then would have bought a file that was approximately DuckDB's syntax and drifted silently wherever it was not, which is worse than useless for a compatibility claim because it looks authoritative.

In v2.0 that same grammar became the parser. It is now on the path every query takes, which means any divergence between it and the language is a bug that breaks DuckDB itself, not a stale autocomplete suggestion. That is the change that makes this file worth pinning, and it is why this document is dated now rather than a year ago.

The three consequences for this project, in order of how much they matter.

**The syntactic surface stops being something we transcribe.** A hand-written parser for this dialect means reading 1,086 rules and writing each one out in Rust, then reading the diff of those rules on every upstream release and writing that out too. Each transcription is an opportunity to produce a syntax error on valid DuckDB SQL, which is the single failure mode document 00 promises the project does not have. Vendoring removes the opportunity rather than reducing it.

**Extension grammars come for free.** DuckDB v2.0 lets an extension register grammar changes that are applied to the parsed grammar before it is compiled. Because we hold the same rule graph in the same shape, an extension's grammar change is a graph edit here too, rather than a feature we would have to invent an equivalent of.

**The bump becomes a diff a human reads.** Section 20.9 is the procedure. The interesting property is that `git diff` on the vendored directory after a fetch is a complete and exact statement of what changed syntactically in an upstream release, in a form small enough to read in one sitting.

## 20.2 The artifact, measured

All counts are from the vendored tree at `v2.0-cyanoptera`, commit `cc7e7bac7fcb6e0994359965a87ac4f6a96f2e17`, retrieved 10 September 2026. They are recomputed rather than copied from anywhere, and `crates/rudb-parse/grammar/VENDOR` records a SHA-256 for every file. Counting is per file rather than over the concatenation, because three of the forty files have no trailing newline and concatenating them merges a rule with the one after it, which is worth knowing before the generator reads them the same way.

| | |
|---|---|
| `.gram` files, all under `statements/` | 40 |
| total bytes | 61,190 |
| lines | 1,421, of which 1,222 are neither blank nor comment |
| rule definitions | 1,086, plus `%whitespace`, which is the runtime's rather than a rule |
| keyword lists | 5, in `keywords/` |
| reserved keywords | 75 |
| unreserved keywords | 339 |
| column name keywords | 54 |
| function name keywords | 29 |
| type name keywords | 31 |
| distinct keywords across all five | 498, so 30 words are in more than one class |

Sixty one kilobytes is the entire syntax of the dialect that won on PostgreSQL compatibility. For scale in the other direction, upstream's transformer, which is the layer after the parser and the one we still write ourselves, is 46 files and 2,650,053 bytes, most of it generated serialization and copy boilerplate. The grammar is about two per cent of the front end by size and it is the two per cent that defines what compiles.

The upstream tokenizer, `src/parser/peg/tokenizer/base_tokenizer.cpp`, is 613 lines and is not in the grammar. That file is section 20.7 and it is the real risk in this plan.

## 20.3 What we take, exactly

**Vendored verbatim, byte for byte.** `src/parser/peg/grammar/statements/*.gram` and `src/parser/peg/grammar/keywords/*.list`, plus upstream's `LICENSE` as `LICENSE.duckdb`. These land in `crates/rudb-parse/grammar/` under a `VENDOR` file recording the upstream URL, the ref, the resolved commit SHA, the retrieval date, and a SHA-256 per file.

**Derived and vendored alongside**, both with their own checksums, both covered in section 20.4: `memoized_rules.list` and `matcher_overrides.list`.

**Read as reference, not vendored.** The rest of `scripts/parser/grammar_types.yml`, which maps rule names to the C++ node types upstream's transformer produces. It names 110 result types. It is a C++ artifact and useless to us directly, and it is the best available index of which rules a transformer actually has to do something with as opposed to descending through, which is how document 05's equivalent orders the work.

**Not taken.** The generator, the matcher, the tokenizer, the transformer, the binder. All C++, all ours to write in Rust. `scripts/parser/inline_grammar.py` is read once for its behaviour, because it is the definition of how the keyword lists and the `.gram` files become one grammar, and then discarded.

**The vendored directory is never edited.** Not for a fix, not for a workaround, not to add a rule we would like to have. The moment one local edit exists, compatible by construction becomes compatible except for the edits, and within a year nobody can say what they were. `cargo xtask vendor-grammar` is the only thing that writes there and `cargo xtask grammar` recomputes every checksum and fails if anything moved. That check is in `cargo xtask ci` and therefore in the required CI job, which is what turns the rule from a request into a fact. If upstream's grammar is wrong, the fix goes upstream and we pin the next commit.

## 20.4 The two lists that are not in the grammar

Two facts about how the grammar is matched are not expressible in the grammar text, are load bearing, and would be invented differently by anyone reimplementing from the `.gram` files alone. Both are extracted mechanically by the vendoring task from the same commit as the grammar, so they can never drift out of step with it.

**The memoized rules**, 22 of them, out of `packrat_memoized_rules` in `grammar_types.yml`. Every one is on the expression precedence chain, from `Expression` down through `ComparisonExpression`, `AdditiveExpression` and `MultiplicativeExpression` to `FunctionExpression`, `ColumnReference`, `ColId` and `Identifier`. Full packrat memoizes every rule at every position and buys linear time at the cost of a table proportional to rules times positions, which for a 1,086 rule grammar over a hundred token query is a cost that exceeds what it saves on almost every real statement. DuckDB's answer is a short explicit list chosen with a profiler. We copy the list rather than choosing our own, and section 20.10 explains why that is a correctness decision and not a performance one.

**The matcher overrides**, 24 of them, out of the generated block in `src/parser/peg/compiled_grammar.cpp`. These are the rules whose bodies the matcher does not walk because it matches them itself against the token in hand. Skipping this is a correctness bug rather than a missed optimization: the grammar text says `OperatorLiteral <- Identifier`, so a matcher that believed the body would read a bare `+` as an identifier. The same goes for `NumberLiteral`, `StringLiteral` and the 21 name rules.

The 21 name rules split 13 to 8 between two matchers. `IdentifierMatcher` rejects a word that is a keyword unless the keyword's class is one the position tolerates. `ReservedIdentifierMatcher` drops that check entirely, and that is the whole of why `db.select` is legal after the dot while a bare `select` is not a column name.

Each override carries a third field, the suggestion state it was constructed with, and the suggestion is not autocomplete trivia. It is read twice. Once to pick which keyword class the position tolerates, where a type name position takes a type name keyword, the two function name positions take a scalar function name keyword, and everything else takes a column name keyword. And once to decide whether a single quoted string counts as a name in that position, which only a table name position allows, and which is the entire mechanism by which `FROM 'data.parquet'` parses with no rule mentioning it. Dropping the suggestion and keeping only the matcher class would quietly make all 13 identifier positions behave like a column name.

`EndOfInput` is installed by upstream outside the generated block and is deliberately not in our extracted list, because the extraction is defined as the contents of the marked block and a reader who adds one thing by hand will eventually add another.

## 20.5 The generator and the rule table

`cargo xtask gen-grammar` reads the vendored tree and writes `crates/rudb-parse/src/generated/`. The output is checked in. CI regenerates and fails on any diff, which is what actually enforces that nobody hand edits the generated file either.

Four decisions, written down because they will otherwise be relitigated.

**The output is data, not code.** A flat table of rule nodes covering sequence, ordered choice, repetition, optional, reference, keyword, literal and negative lookahead, which the matcher interprets. Not 1,086 generated Rust functions. Two reasons. Generated code at that scale is a compile time cost paid by everyone who builds the workspace whether or not they ever parse a query, and rudb already has 27 crates to keep honest on that axis. And a data table can be regenerated and diffed by a human, which is the property that makes a grammar bump reviewable.

**Parameterized rules are expanded at generation time.** The grammar has two macro forms, `List(D) <- D (',' D)* ','?` and `Parens(D) <- '(' D ')'`, and they are most of why 1,086 rules fit in 1,421 lines. Expanding `Parens(List(Expression))` into a concrete rule with a synthesized name costs a few hundred extra table entries and buys a matcher with no environment to thread through it. Note in passing that the trailing comma in `List` is where DuckDB's most used piece of friendly SQL comes from, and that it arrives for free.

**The keyword table is one sorted list with a class mask, not five tables.** The five classes are not disjoint, 30 of the 498 words are in more than one, so five tables means storing those words more than once and then deciding which answer wins. Worse, most words in a query are not keywords at all and the common case with five tables is five misses. One binary search over 498 words answers the whole question and the class is a bit mask, so asking whether a word is acceptable in a position is an `and`.

**The generated files are checked in.** A contributor with no network builds rudb. This is the same rule the vendored tree lives under and for the same reason.

The rule table also carries the first token filter, which is the single largest performance lever in the matcher and is described in section 20.8.

## 20.6 The matcher

A PEG interpreter over the rule table. Input is the token vector and a start rule, output is a parse tree in an arena or a failure with a position.

Nodes are fixed size and index linked, `(rule, token_start, token_end, first_child, next_sibling)`, allocated from one arena per parse. This is the same shape document 04 already specified for the AST and for the same reason: an ownership tree with `Box` per node is a pointer chase per node and a fight with the borrow checker that buys nothing.

The matcher knows nothing about SQL. Sequence matches children in order and fails as a unit. Ordered choice tries alternatives left to right, resets the token position on each failure, and takes the first success. There is no longest match, no ambiguity and no conflict, which is the whole reason the dialect stopped fighting its parser. Repetition is greedy with no backtracking into it, which is standard PEG and is a genuine semantic difference from a context free grammar that the grammar text is written to expect. Negative lookahead matches without consuming and appears in exactly one place, `PlainIdentifier <- !ReservedKeyword <[a-z_]i[a-z0-9_]i*>`. Positive lookahead is in the notation, is never used, and is rejected by upstream's own grammar reader, so we reject it too.

**It is an explicit stack machine and not a recursive function.** Upstream's is, and it is the difference between accepting five thousand nested parentheses and running out of native stack at around eight hundred rule frames. A parser that dies on deeply nested input is a denial of service in any front door that accepts untrusted SQL, and document 16's fuzzing surface includes exactly that. There is still a depth cap, because unbounded is not a policy, and past it the error is DuckDB's own `memory exhausted at or near` so that the text agrees even where the limit does not.

**It never reaches a character level node, and that is checked rather than assumed.** Character classes and captures appear only inside `%whitespace`, `NumberLiteral`, `StringLiteral`, `PlainIdentifier` and `QuotedIdentifier`. The first is never referenced because whitespace is applied between tokens rather than called, the next two are overridden, and the last two are reachable only through `Identifier`, which is overridden as well. So reaching one means the grammar has grown a shape the tokenizer does not cover, and the matcher stops loudly instead of guessing.

## 20.7 The tokenizer is the part that is still ours

The grammar guarantees less than it looks like it does, and this is the largest of the three gaps. String literals, dollar quoting, numeric literal forms, comments, identifier quoting, case folding and the operator rules all live in a hand-written C++ tokenizer that has no declarative artifact. We match it by behaviour, and the way we find out whether we have is a differential fuzzer against DuckDB's own tokenizer from week one. Six hundred lines of state machine is the entire distance between us and the compatibility claim on this axis, and it is the cheapest place in the project to buy confidence.

The details that have to be right, each of which is a silent compatibility bug if it is not. Every one of these is a fact about DuckDB rather than a design decision of ours.

**Identifiers fold to lower case and quoted identifiers do not.** `SELECT A` and `select a` are the same column and `"A"` is a different one. Standard SQL folds up and DuckDB folds down, which is observable in error messages and in catalog output.

**Keyword classification comes from the vendored `.list` files.** The longest keyword is short enough that the fold happens in a fixed stack buffer and a longer word skips the lookup entirely. A word in the reserved list cannot be a bare identifier; a word in one of the other four can be, in the positions the grammar allows. Putting a word in the wrong class produces precisely the failure this whole document exists to prevent.

**Two string literals separated by whitespace containing a newline are one literal.** `SELECT 'a' 'b'` is a syntax error, the same two literals with a newline between them is `'ab'`, a line comment in between preserves the join and a block comment breaks it. That is PostgreSQL's rule and DuckDB kept it, and nobody writes it down until a corpus file uses it.

**A decimal point means `DECIMAL`, not `DOUBLE`.** `1.1 + 2.2` is `DECIMAL(3,1)` and is exactly `3.3`, where an engine that read the literal as a double gets `3.3000000000000003`. An exponent overrides that and makes it `DOUBLE`. `1.` and `.5` are both valid and both `DECIMAL`. This is the tokenizer reaching into document 10's type rules and it is the single most visible wrong answer available on this path.

**There are no hex or binary literals.** `SELECT 0x1F` is the number `0` in a column named `x1F`. So is `0b101`. An implementation that helpfully accepts them produces a different answer rather than a different error.

**Underscore digit separators are accepted only between two digits.** `1_000` is a thousand, `SELECT 1_` is `1` aliased `_`, and `SELECT 1__0` is `1` aliased `__0`. The exponent has the same shape of rule, so `SELECT 1e` is `1` aliased `e` and an `e` with no digits after it has to be handed back rather than reported.

**Four string prefixes, `E`, `X`, `B` and `N` in either case, and only when the quote is the very next byte.** `SELECT e 'a'` is an identifier and then a string. Only `E` changes how the body is read, where a backslash swallows the byte after it including a quote.

**Parameters are `?`, `?1`, `$1` and `$name`, and there is no `:name` form.** `$` is also dollar quoting, and the disambiguation is that a dollar quote tag is a word that does not start with a digit.

**Operators are a maximal run of operator characters with three rules on top**, and this is the one place where copying PostgreSQL's answer is wrong. A dozen characters are always their own token and never join a run, including `-` and `#`, so `SELECT 1 =- 1` is `1 = -1`. Six sequences are checked before that, `->>`, `::`, `:=`, `->`, `**` and `//`, which is how `->` survives the rule that a minus never joins anything. And a run of more than one character gives back a trailing `+` unless the run also contains one of a specific set of punctuation characters, so `SELECT 1 =+ 1` is `1 = +1` while `SELECT 1 !=+ 1` goes looking for an operator named `!=+`.

**Comments are `--` to end of line and `/* */` with nesting.** The version that does not nest turns a commented out block containing a comment into a syntax error halfway down a file.

Nothing is decoded at this stage. A string keeps its quotes and its escapes, a number keeps its underscores, an identifier keeps its case. Decoding is the transformer's job, and leaving it there is what keeps a token small and the whole token vector in cache.

## 20.8 Performance, and why it is not the objection it looks like

The obvious objection to interpreting a rule table is that a hand-written recursive descent parser is faster, and it is. The objection does not survive contact with the numbers.

**On the benchmark axis it is not measurable.** Document 02's target is 2.63 seconds for the 43 ClickBench queries, which is 61 milliseconds a query. A parse at one hundred microseconds is 0.16 per cent of that. There is no benchmark in document 15 where parse time appears above the noise, and an engine that wins ClickBench by being fast at parsing has misunderstood the problem.

**On the latency axis it is real and it is bounded.** The embedded case is a REPL and a notebook where a person types `SELECT 1` and waits, and there the front end is the whole cost. The reference points, all measured by the `firepanda` project on an Apple M4 and cited here as theirs rather than ours: DuckDB 1.5.5 parses TPC-H q1 in about 156 microseconds and `SELECT 1` in about 6, both including its JSON serialization of the parse tree and therefore an upper bound. Their interpreted PEG matcher started at 380 microseconds and 20, and reached roughly a fifth of that with two changes, neither of which is exotic.

The first is a first token filter, worth about three times on its own. Every node in the generated table carries a 64 bit word saying which tokens it can start with, and the matcher tests the token in hand against that word before walking the node. The ordered choices in this grammar are long, fifty alternatives is ordinary, and the token in hand rules out nearly all of them; without the filter the matcher discovers that one recursion at a time. The words are FIRST sets computed at generation time to a fixed point, because the rule graph has cycles. Three rules keep the filter loose rather than tight: a node that can match the empty string gets every bit, a negative lookahead contributes nothing because it consumes nothing, and the overridden rules get their words from the matcher rather than from their bodies. Anything unclear resolves to every bit, because a word that is too generous costs one failed recursion and a word that is too tight rejects a valid query, and those two failures are not symmetric.

The second is memoizing successes as well as failures, worth about one and a half times on top. Failure memoization is what handles DuckDB's own documented pathology, which is a query with nineteen unmatched parentheses taking ten seconds unmemoized and one millisecond memoized. Success memoization is what handles `SELECT f(f(f(1)))`, where `TypeModifiers <- Parens(List(Expression)?)` means `f(x)` is attempted as a parameterized type before it is attempted as a call, so every nesting level is walked twice and twelve levels is a factor of four thousand. Failure memoization cannot see any of it, because both walks succeed.

The cost of memoizing successes is that a failed attempt must not truncate the node arena, since a memoized success is a subtree in it. So a failure unwinds only the pending stack and leaves its nodes where they are, unreachable from the tree and reachable from the memo table, and the arena ends up about half again the size of the tree it holds. One thing a memo hit must not do is hand back the node itself, because a node's `next_sibling` is written by whichever parent adopts it and the same subtree can be adopted more than once, so a hit copies the root and shares everything under it.

**Both of these have a test that is not a benchmark.** The filter is correctness critical in one direction: the generator computes the words and the matcher computes the token keys, they are two pieces of code, and a disagreement shows up as a valid query that no longer parses rather than as a crash. So the matcher has an unfiltered mode with the check compiled out, and the whole corpus goes through both and has to come back with the same tree node for node and the same error text word for word. The pathological shapes get their own suite from the first week, generated rather than collected: N unmatched parentheses, N nested calls, N nested `CASE`, N nested subqueries, long `IN` lists, long operator chains, wide select lists. Each has a wall clock ceiling, and every shape that could go exponential is also asked at n and at 2n, because an absolute ceiling on a shared runner is a blunt instrument while a shape that takes sixty four times as long for twice the size is exponential however slow the machine was.

**If the interpreter ever is the constraint, the answer is not a hand-written parser.** It is generating Rust from the same rule table, which is a change to one xtask and no change to the vendoring, the tokenizer, the transformer or the compatibility story. That is the option value the data table buys and it is why the table is the output rather than the code.

## 20.9 The bump procedure

Written down because it will be run by somebody who has not read this document.

1. `cargo xtask vendor-grammar <ref>` fetches the grammar at the ref, resolves it to a commit SHA, rewrites `VENDOR`, and refuses to write anything if the upstream layout has moved. With no argument it re-fetches the ref already recorded, which is how you find out whether upstream has moved without deciding to move with it.
2. `git diff crates/rudb-parse/grammar` is the complete syntactic change in that release. Read it. For a patch release it is tens of lines.
3. `cargo xtask gen-grammar`. The table diff should be proportionate to the grammar diff. If it is not, the generator has a bug and that is the finding.
4. Run the differential parse harness from document 14 against the new DuckDB build. Accept and reject must agree across the whole corpus. New syntax that we now parse and cannot yet transform shows up here as a refusal by name, never as a syntax error.
5. Every new rule with no transformer case gets a refusal naming it and an issue. That is the entire cost of falling behind on semantics and it is bounded and visible.

`v2.0-cyanoptera` is DuckDB's default branch rather than a tag, because 2.0 had not been released when this was written and the last tag was v1.5.5. A branch name pins nothing, so the pin is the commit SHA and the ref is recorded alongside it as the thing to re-resolve.

**How often this actually moves**, measured over the four releases the grammar has existed for. The first three are the autocomplete copy described in section 20.1, which is not the same artifact, but it is the same file set maintained by the same people and it is the only evidence available. Rule counts are per file, so a missing trailing newline does not merge two definitions, and the changed line count is `diff -r` over the statements directory in both directions.

| release | rules | lines | bytes | changed lines since previous |
|---|---|---|---|---|
| v1.4.0 | 525 | 782 | 32,514 | |
| v1.5.0 | 762 | 1,036 | 42,317 | 1,074 |
| v1.5.5 | 778 | 1,057 | 43,391 | 76 |
| v2.0 | 1,086 | 1,421 | 61,190 | 671 |

The shape is the useful part. Within a release series the grammar is nearly static: five patch releases moved it 76 lines, which is a diff read over coffee. Across a minor or major it is several hundred to a thousand lines, which is a day of reading and then however long the transformer work takes. The keyword lists move less than the rules do, 11 changed lines from v1.5.5 to v2.0 against 671 in the grammar, which matches the intuition that new syntax mostly reuses existing words.

So the recurring cost of staying current is a day for a patch series and a week for a major, and almost none of that week is the grammar. It is the transformer catching up with whatever the new rules mean.

CI runs step one once a week against the upstream default branch and opens an issue when the SHA moves. That mechanism is the difference between "100% compatible with DuckDB" being a maintained property and being a claim that was true on the day somebody wrote it down.

## 20.10 Where fidelity actually leaks

Three gaps, each a place a bug can hide behind the words "but we vendored the grammar."

**The tokenizer is not in the grammar.** Section 20.7 is the list and the differential fuzzer is the answer. This is the primary risk in the front end and it is worth restating that it is a hand-written component matched by behaviour, exactly like the one this whole approach was supposed to avoid, only much smaller.

**Ordered choice makes the matcher's semantics load bearing.** The same rule table walked with subtly different backtracking or memoization behaviour accepts a different language. This is not a hypothetical: memoizing a rule DuckDB does not memoize can change which alternative wins at a position, because a PEG memo entry makes an earlier attempt's outcome permanent. That is why section 20.4 copies the memoized list rather than choosing one, and why the differential harness compares parse trees on the corpus rather than only accept against reject.

**Some syntax is accepted by the grammar and rejected later, upstream too.** DuckDB's grammar is deliberately permissive in places and leaves the error to the binder. So accept and reject agreement is measured per stage, parser against parser and full pipeline against full pipeline, and reported separately, so that a binder difference is never counted as a grammar success.

## 20.11 What this does not buy

**It does not make the front end done.** The transformer from parse tree to rudb's AST is ours, it has to be total over the rule table, and document 10 remains the largest single body of compatibility work in the project. What vendoring changes is that the transformer's input is a known finite set of rule indices rather than whatever our own parser happened to produce, so a missing case is a compile error rather than a runtime fallthrough.

**It does not make the binder compatible.** Most of DuckDB's observable behaviour that is not in the grammar lives in binding: overload resolution, the implicit cast lattice, star expansion order, lateral column aliases, `USING` column merging. None of that is in a `.gram` file.

**It does not settle error recovery.** A PEG parse stops at the first failure and recovery would change which strings are accepted, which would put a hole in the compatibility claim. Document 04's requirement for multiple diagnostics is therefore a second entry point over the same rule table, explicitly not what the query path does, and it is a tooling feature rather than an engine feature.

**It does not remove the need to read DuckDB's source.** Sections 20.4 and 20.7 are both cases where the grammar text is not the whole answer and the C++ is what runs. The discipline is that every such case is extracted mechanically and recorded in the vendored tree with a checksum, rather than being learned once by one person and then living in their head.
