# The surface, measured, and the gap

Everything in this document was counted on 14 September 2026 against `v2.0.0-dev84237`, `cc7e7bac7f`, on server2, and against rudb at `e0f32d2`. The statement that produced each DuckDB count is given so it can be rerun when the pin moves.

## 2.1 Functions, the headline

`SELECT count(*), count(DISTINCT function_name) FROM duckdb_functions()` gives 3245 rows over 1159 distinct names. Distinct `(function_name, parameter_types)` pairs are 3228, so the overload table is nearly a bijection with the rows. 665 rows carry a non empty `alias_of`.

By `function_type`, as rows and as distinct names:

| type | rows | names |
| --- | --- | --- |
| scalar | 1702 | 814 |
| aggregate | 1183 | 78 |
| table | 165 | 104 |
| macro | 140 | 121 |
| pragma | 38 | 38 |
| window | 13 | 13 |
| table_macro | 4 | 4 |

rudb's table is 65 entries in `crates/rudb-functions/src/signature.rs` lines 243 to 529: 59 scalar and 6 aggregate, plus 9 alias pairs in `ALIASES` at lines 1167 to 1198 and 5 table functions in `crates/rudb-functions/src/table.rs` lines 34 to 45, one of which, `rudb_strategies`, is not a DuckDB function at all. Zero window functions, zero macros, zero pragmas.

Unweighted, that is 65 of 1159, or 5.6 percent. That number is the one to publish and it is also the one that misleads most, for the reason in the next section.

## 2.2 The denominator is not what it looks like

Take the 931 distinct scalar and macro names together and sort them into families by prefix.

**282 of the 931 are collations.** `SELECT count(*) FROM (SELECT DISTINCT function_name f FROM duckdb_functions()) WHERE f LIKE 'collate\_%' ESCAPE '\'` is 141, and the same for `icu_collate_%` is another 141. They are one feature, a per locale collation registered once per locale by the ICU extension, and they are 30 percent of the scalar surface by name count. Implementing collation is one piece of work that moves the unweighted number by 24 points. Implementing it badly, by registering 282 names that all do nothing, moves it by 24 points and is a lie, which is why section 1.2's rule exists.

**62 are PostgreSQL compatibility shims.** `pg_typeof`, `pg_get_viewdef`, the eleven `has_*_privilege` functions, `current_schemas`, `inet_server_addr`, `obj_description` and the rest. Those exist so that a tool written against PostgreSQL connects and introspects. They are a family, most of them return a constant or read a catalog table, and none of them is hard once the catalog tables in section 2.5 exist.

The rest, 587 names, sort like this:

| family | names |
| --- | --- |
| list and array | 118 |
| date and time | 84 |
| string | 76 |
| operators spelled as punctuation | 67 |
| numeric and math | 56 |
| json | 36 |
| map | 14 |
| struct | 13 |
| variant | 13 |
| spatial, the `st_` prefix | 12 |
| regexp | 8 |
| bit and bitstring | 5 |
| enum | 5 |
| union | 3 |
| everything left over | 77 |

The classification is a prefix rule written for this document, so the boundaries are arguable at the edges, and the leftover 77 is where the arguing would happen. The shape is not arguable: eight families are two thirds of the real surface, and each of the eight is one body of machinery with a long tail of names hanging off it. `list_transform`, `list_filter` and `list_reduce` are the lambda machinery. The 118 list names are mostly not 118 problems, they are the `LIST` physical layout, a lambda binder, and then a week of writing the ones that do not fall out.

That is the central claim of this folder and it is the thing the last month of work got wrong. Shipping `age` took a day of measurement and a pull request and moved the unweighted number by 0.09 percent. Shipping the `LIST` type and its lambda binder moves it by ten points and unblocks `map`, `struct` and half of `json` as well.

## 2.3 Aggregates and windows

78 aggregate names over 1183 rows, against rudb's 6. The 78 are also families: nine `regr_*`, four quantile spellings, six `arg_min` and `arg_max` spellings that are aliases of two functions, `min_by` and `max_by`, the moment statistics, `bitstring_agg`, `histogram` and `approx_*`. The distance from 6 to 78 is smaller than it looks once the aggregate framework supports a state type larger than a scalar, which `sum` and `avg` do not need and `quantile`, `histogram` and `string_agg` all do.

`DISTINCT` and `FILTER (WHERE ...)` are already implemented, in `crates/rudb-bind/src/binder.rs` lines 424 to 428 and `crates/rudb-exec/src/group.rs`. `ORDER BY` inside an aggregate call is not, and there is no `WITHIN GROUP` anywhere in the AST, the plan or the executor.

13 window function names, none of them in rudb, and no `OVER` anywhere in the tree. `crates/rudb-bind/src/lib.rs` line 17 refuses window functions by name. In the upstream corpus this is one of the two largest single holes.

## 2.4 Types

`SELECT count(*) FROM duckdb_types()` is 313 rows, which collapse to 82 distinct `(type_name, logical_type, type_category)` triples. rudb's `LogicalType` in `crates/rudb-common/src/types.rs` has 33 variants.

What rudb has that matters: `DECIMAL` with width and scale, `LIST`, `ARRAY`, `STRUCT`, `MAP`, `UNION`, `BLOB`, `BIT`, `UUID`, `HUGEINT` and `UHUGEINT`, the four timestamp precisions and `TIMESTAMPTZ`.

What is missing from the enum entirely: `ENUM`, `VARINT` and its v2.0 spelling `BIGNUM`, `GEOMETRY`, `VARIANT`, `TIME_NS`, `TIMESTAMPTZ_NS`, `TUPLE` and `TYPE`. `JSON` is a `VARCHAR` with a tag upstream and rudb has no tag mechanism. `GEOMETRY` and `VARIANT` are parsed by the vendored grammar and reach no `LogicalType`, which means a query naming one gets a type error from a layer that should have given a not implemented error.

The composite types deserve separating from the scalar ones. `LIST`, `STRUCT`, `MAP`, `UNION` and `ARRAY` exist as `LogicalType` variants and there is no vector layout, no kernel and no cast arm behind any of them. A variant that the type system can name and the executor cannot hold is worse than a missing variant, because it turns a clear refusal into an internal error, and `crates/rudb-kernels/src/cast.rs` line 505 is where that shows up.

## 2.5 Statements

The `Statement` rule in `crates/rudb-parse/grammar/statements/common.gram` lists 36 alternatives. `crates/rudb-parse/src/transform.rs` has 7 arms for them, at lines 283 to 292: select, create, drop, insert, set, reset, explain. The other 29 reach `unsupported` at line 226 and answer `{text} is not supported yet, the grammar rule is {rule}`.

The `ast::Statement` enum has 8 variants because `CreateStatement` splits into table and view.

Ranked by how much of the upstream corpus each unlocks, rather than alphabetically, the missing ones that matter are: `UPDATE` and `DELETE`, `COPY`, `PREPARE` and `EXECUTE`, `ATTACH` and `DETACH`, the transaction statements, `PRAGMA` and `CALL`, and `ALTER`. `PRAGMA` is worth calling out because it is 38 of the 1159 function names as well as a statement, and because the corpus uses it constantly for setup rather than as a subject.

Inside `SELECT`, the three holes named by `crates/rudb-bind/src/lib.rs` line 17 are worth more than all 29 missing statements combined. `WITH` is 1244 records in the corpus and subquery expressions are 805. Window functions are the third.

## 2.6 Settings, keywords, introspection

`SELECT count(*) FROM duckdb_settings()` is 192. rudb has `crates/rudb/src/settings.rs`, which accepts `SET` and `RESET` for a constant value and refuses an expression, and whose own doc says `current_setting()` and `duckdb_settings()` are not there yet.

Settings are not a side surface. Nineteen of the 192 change the meaning of a query rather than its performance, and a compatibility suite that does not set them is testing one point in a space. The ones measured on the pinned binary and their defaults:

`preserve_identifier_case` is `preserve_case`, `default_null_order` is `NULLS_LAST`, `default_order` is `ASCENDING`, `integer_division` is `false`, `null_on_division_by_zero` is `false`, `ieee_floating_point_ops` is `true`, `scalar_subquery_error_on_multiple_rows` is `true`, `order_by_non_integer_literal` is `false`, `regex_match_operator_semantics` is `partial`, `show_behavior` is `AUTO`, `lambda_syntax` is `DEFAULT`, `disable_timestamptz_casts` is `false`, `default_collation` is empty, `errors_as_json` is `false`, `warnings_as_errors` is `false`, `storage_compatibility_version` is `latest`, `current_dialect` is `duckdb`, `dialect_compatibility_mode` is `NONE`, `allow_parser_override_extension` is `DEFAULT`.

Every one of those is a knob that a second dialect would want set differently, which is the argument of document 04 made by DuckDB rather than by us.

`SELECT count(*) FROM duckdb_keywords()` is 505: 75 reserved, 339 unreserved, 55 column name, 36 type function. rudb gets this one for free, because `crates/rudb-parse/src/generated/keywords.rs` is compiled from the same five vendored `.list` files, so the keyword classification is correct by construction and the only work is the table function that exposes it.

The introspection table functions are the surface a tool sees, and rudb answers none of them. The pinned binary has `duckdb_columns`, `duckdb_constraints`, `duckdb_databases`, `duckdb_dependencies`, `duckdb_extensions`, `duckdb_functions`, `duckdb_indexes`, `duckdb_keywords`, `duckdb_optimizers` at 44 rows, `duckdb_schemas`, `duckdb_secrets`, `duckdb_sequences`, `duckdb_settings`, `duckdb_tables`, `duckdb_types`, `duckdb_variables`, `duckdb_views`, `duckdb_dialects`, `duckdb_grammar_extensions` and about a dozen more, plus the whole `pragma_*` family and `information_schema`. `crates/rudb-catalog/src/lib.rs` has none of them and mentions `information_schema` only in a doc comment about name resolution.

This is the cheapest large win in the folder. The catalog data already exists in `crates/rudb-catalog`, the table function mechanism already exists in `crates/rudb-functions/src/table.rs`, and `rudb_strategies` already proves a table function can be backed by an in memory list of rows. Answering `duckdb_functions()` also has a property nothing else here has: it makes the harness in document 09 able to compute its own denominator from our side as well as DuckDB's, which is what turns the coverage number from a spreadsheet into a query.

## 2.7 Extensions

`duckdb_extensions()` is 31 rows and 6 are loaded in a default build: autocomplete, core_functions, icu, json, parquet, shell. Two of those define SQL surface. `core_functions` is where most of the 1159 live, so it is not optional and the split is invisible to a user. `icu` is where the 141 `icu_collate_*` names and the time zone support come from, which means `TIMESTAMPTZ` arithmetic and the session time zone are an extension boundary upstream and should be one here too.

## 2.8 The arithmetic, honestly

Unweighted function coverage today is 65 of 1159, 5.6 percent. Statement coverage is 7 of 36, 19 percent, and that overstates it because the 7 include the one that is most of the language and the binder refuses three of its major clauses. Upstream corpus pass rate at `995f0e8` is 21.4 percent of attempted records, with a third of the corpus never attempted.

The weighted number does not exist yet, because the real query corpus that supplies the weights does not exist yet. Building it is the first item in document 12, and until it exists every percentage in this folder is the unweighted one and says so.
