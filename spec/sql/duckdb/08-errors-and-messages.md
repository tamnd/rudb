# Errors as a compatibility surface

The last four differentials in this project ended at a missing block of error text rather than at a wrong answer. That is not a coincidence and it is not a sign the error surface is unimportant, it is a sign it is the surface nobody has counted.

## 8.1 What an error actually is on the pinned binary

Run `SELECT nosuchfn(1);` on `v2.0.0-dev84237` and standard error carries four distinct things, followed by exit code 1:

```
Catalog Error: Scalar Function with name nosuchfn does not exist!
Did you mean "nanosecond"?

LINE 1: SELECT nosuchfn(1);
               ^
```

A **kind**, `Catalog Error`, before the first colon. A **headline**, the sentence after it. A **suggestion**, its own line, absent for most errors. A **location block**, a blank line then `LINE n:` then the text then a caret in the column of the offending token.

Other measured shapes on the same binary: `Parser Error: syntax error at or near "+"`, with a location block and no suggestion. `Catalog Error: Table with name nosuchtable does not exist!` with `Did you mean "duckdb_tables"?`. `Binder Error: Referenced column "current_dialect" not found in FROM clause!`. `Invalid Input Error: Dialect "cypher" is not installed`, with no location block at all. `Not implemented Error: Enum value: unrecognized value "nope" for enum "DialectCompatibilityMode"`, followed by a candidate list.

Four parts, and they cost wildly different amounts to match. So they get compared at four levels rather than one, which is the whole design.

## 8.2 Five levels, published separately

**Level zero, it errors.** DuckDB refused the statement and so did we. rudb-compat already measures the inverse of this and calls it `MissedError`, a statement DuckDB refused and we accepted, which is the worst failure in the taxonomy because it is a wrong answer wearing a success.

**Level one, the kind matches.** `Catalog Error` against `Catalog Error`. This is what rudb-compat compares by default today and it is the right default.

**Level two, the headline matches.** The sentence after the colon, byte for byte, including the exclamation mark. This is what `--strict-messages` already raises the bar to.

**Level three, the suggestion matches.** Section 8.4 explains why this is its own level and not part of level two.

**Level four, the location block matches.** Same line number, same rendered text including any truncation, same caret column.

Publish five numbers. A single error compatibility percentage would average a cheap level against an expensive one and hide which is moving, for the same reason section 1.1 refuses to average the three level two denominators.

## 8.3 The kind taxonomy is DuckDB's, not ours

The kind before the colon is not a free choice. It comes from DuckDB's exception type enum and the set is fixed upstream: `Parser Error`, `Binder Error`, `Catalog Error`, `Conversion Error`, `Out of Range Error`, `Invalid Input Error`, `Not implemented Error`, `Constraint Error`, `IO Error`, `Internal Error` and the rest. Our error type's variants are that list. Any error we produce whose kind is not in the list is a bug by construction, and that is a test rather than a code review comment.

One thing changes in the harness as a result. `crates/rudb-compat/src/conform.rs` merges `Binder Error` and `Catalog Error` into one reason, on the argument that from the outside they are the same complaint. That argument is right for triage and wrong for the published number. Keep the merge as a triage bucket, in the reason list that groups failures for a human, and do not let it into the level one comparison, where the kinds are compared as written.

## 8.4 The suggestion is a reverse engineering problem

`nosuchfn` suggests `nanosecond`. That is a strange suggestion and it is informative: the candidate set being searched is not the set of scalar function names a user would expect, it includes date part specifiers, which means matching this output means matching the candidate set, the distance function, the threshold and the tie break, not just writing a Levenshtein.

So level three is separate and is explicitly optional for a long time. The right sequence is to get the presence or absence of a suggestion line right first, since a harness that expects three lines and gets two is a diff regardless of the content, and to treat the exact candidate as a later exercise with its own generated corpus: take every catalog name, mutate it in the ways a typo mutates a name, and record what the pinned binary suggests. That corpus is cheap to generate and is the only way this level ever gets to a high number.

## 8.5 The location block needs spans

`LINE 1: ...` with a caret under the token requires the byte offset of the offending token to survive from the tokenizer through the transform, the binder and the optimizer, because the errors that carry a location block include binder errors and runtime errors, not just parse errors. Section 5.2 lists source spans as the first plan addition for exactly this reason, and says that adding them after the optimizer exists is a rewrite of every pass.

The truncation rule matters too. The measured output shows `LINE 1: ...` with leading ellipsis when the offending token is far into a long statement, so the block has a window and a rule for centring it. That is small, mechanical and easy to get subtly wrong, which makes it a good candidate for a generated corpus rather than hand written cases.

## 8.6 Use the setting that already exists

`errors_as_json` is a setting on the pinned binary, defaulting to false. Turning it on gives a structured error instead of formatted text, which means the harness can compare fields rather than diff prose, and can tell the difference between a kind mismatch and a whitespace mismatch without a regex.

So the harness runs error comparisons in both modes. JSON mode gives level one, two and three cleanly and is what the published numbers are computed from. Text mode is what level four needs, because the caret and the line rendering only exist in the text form and a user only ever sees the text form.

This is the cheapest good idea in the document and it is available because somebody upstream had the same problem.

## 8.7 What is not matched

Internal errors and anything that looks like a crash. Where DuckDB raises `Internal Error`, matching the text is matching a bug, and section 1.5 already said we match behaviour and not bugs. We record it as a known difference.

Anything containing a file path, a temporary directory, a process identifier, a memory address or a duration. Those are excluded by name in the corpus, per section 1.5, not by a fuzzy filter.

## 8.8 Our own not implemented errors

`crates/rudb-parse/src/transform.rs` line 226 answers `{text} is not supported yet, the grammar rule is {rule}` for the 29 statements with no arm. That message is good, it names the grammar rule, and it must never be mistaken for a compatibility result.

The rule: an error we produce because we have not built something carries a kind that DuckDB never produces, so the harness counts it in its own bucket, which rudb-compat already has as `NotImplemented`. A not implemented error that reports itself as `Binder Error` would score as a level one pass against a real DuckDB binder error, and that is a number going up for no reason, which section 1.2 exists to prevent.

The distinction the user cares about is the same one. "You wrote something wrong" and "we have not built that yet" are different sentences and only one of them is their fault.

## 8.9 The number

Five percentages, over the same denominator: every record in the corpora where the pinned binary produced an error. Level zero is expected to reach a high number early and the other four are expected to be low for a long time. Publishing them separately from the beginning is what stops error compatibility from being the thing that is always about to be looked at.
