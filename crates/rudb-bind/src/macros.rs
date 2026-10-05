//! The pin's built-in scalar macros that have no function of the same name here.
//!
//! The pin defines about seventy of its names as macros rather than functions, `fmod`, `ago`,
//! `geomean` and `assert_true` among them, and `duckdb_functions()` lists each one with its
//! parameters and its body. The bodies below are copied from that listing word for word. A call is
//! expanded the way the pin expands it: every argument is written back out as SQL, put in the body
//! in place of its parameter, and the text that makes is parsed and bound where the call was. So
//! `fmod(a, 2)` binds exactly as `(a - (2 * floor((a / 2))))` would, types, errors and all.
//!
//! The macros that already have a function or an expansion of their own here are not in the table.
//! The `list_` aggregates are `crate::listaggr`, `list_append` and its five relatives are in
//! `crate::expr`, and `if` and `nullif` bind as the CASE they stand for. The `json` macro is a
//! function here.
//!
//! The pin keeps the PostgreSQL shims, `pg_typeof`, the `has_*_privilege` pairs and the
//! `pg_*_is_visible` family among them, in `pg_catalog` and the rest in `main`. Both are on the
//! search path, so a bare call finds either, and the table does not tell them apart.
//! `pg_get_viewdef` and `pg_get_constraintdef` wait for arguments that are bound where the call is,
//! since their bodies are queries of a table whose columns would capture an argument written as
//! `view_oid`, where the pin reads the caller's column.

use std::cell::Cell;

use rudb_catalog::{Catalog, Overload, Parameter, QualifiedName};
use rudb_common::{Error, Result, Session};
use rudb_parse::NONE;
use rudb_parse::{Ast, Kind, ast, deparse, parse_ast_with_case, quoted, tokenize};
use rudb_plan::{ExprRef, NodeRef};

use rudb_functions::{FunctionKind, kind_of};

use crate::binder::{Binder, WindowCall};
use crate::parameters::Parameters;
use crate::scope::Scope;
use crate::statement::{Bound, MacroChange};

/// One overload of a built-in macro, as `duckdb_functions()` lists it.
struct Macro {
    name: &'static str,
    parameters: &'static [&'static str],
    body: &'static str,
}

const fn define(
    name: &'static str,
    parameters: &'static [&'static str],
    body: &'static str,
) -> Macro {
    Macro { name, parameters, body }
}

const MACROS: &[Macro] = &[
    define("ago", &["i"], "(current_timestamp - CAST(i AS INTERVAL))"),
    define("array_pop_back", &["arr"], "arr[:(len(arr) - 1)]"),
    define("array_pop_front", &["arr"], "arr[2:]"),
    define(
        "array_to_string",
        &["arr", "sep"],
        "CASE  WHEN ((len(CAST(arr AS VARCHAR[])) = 0)) THEN ('') ELSE \
         list_aggr(CAST(arr AS VARCHAR[]), 'string_agg', sep) END",
    ),
    // The pin gives `sep` a default of `','` here, and a default is written into the body.
    define(
        "array_to_string_comma_default",
        &["arr"],
        "CASE  WHEN ((len(CAST(arr AS VARCHAR[])) = 0)) THEN ('') ELSE \
         list_aggr(CAST(arr AS VARCHAR[]), 'string_agg', ',') END",
    ),
    define(
        "assert_true",
        &["condition"],
        "CASE  WHEN (condition) THEN (NULL) ELSE \"error\"('Assertion failed') END",
    ),
    define(
        "assert_true",
        &["condition", "message"],
        "CASE  WHEN (condition) THEN (NULL) ELSE \
         \"error\"(COALESCE(('Assertion: ' || message), 'Assertion failed')) END",
    ),
    define("col_description", &["table_oid", "column_number"], "NULL"),
    define("current_role", &[], "'duckdb'"),
    define("date_add", &["date", "interval"], "(date + \"interval\")"),
    define("days_in_month", &["date"], "\"day\"(last_day(date))"),
    define("fdiv", &["x", "y"], "floor((x / y))"),
    define("fmod", &["x", "y"], "(x - (y * floor((x / y))))"),
    define(
        "generate_subscripts",
        &["arr", "dim"],
        "unnest(generate_series(1, array_length(arr, dim)))",
    ),
    define("geomean", &["x"], "exp(avg(ln(x)))"),
    define("geometric_mean", &["x"], "geomean(x)"),
    define("has_any_column_privilege", &["table", "privilege"], "true"),
    define("has_any_column_privilege", &["user", "table", "privilege"], "true"),
    define("has_column_privilege", &["table", "column", "privilege"], "true"),
    define("has_column_privilege", &["user", "table", "column", "privilege"], "true"),
    define("has_database_privilege", &["database", "privilege"], "true"),
    define("has_database_privilege", &["user", "database", "privilege"], "true"),
    define("has_foreign_data_wrapper_privilege", &["fdw", "privilege"], "true"),
    define("has_foreign_data_wrapper_privilege", &["user", "fdw", "privilege"], "true"),
    define("has_function_privilege", &["function", "privilege"], "true"),
    define("has_function_privilege", &["user", "function", "privilege"], "true"),
    define("has_language_privilege", &["language", "privilege"], "true"),
    define("has_language_privilege", &["user", "language", "privilege"], "true"),
    define("has_schema_privilege", &["schema", "privilege"], "true"),
    define("has_schema_privilege", &["user", "schema", "privilege"], "true"),
    define("has_sequence_privilege", &["sequence", "privilege"], "true"),
    define("has_sequence_privilege", &["user", "sequence", "privilege"], "true"),
    define("has_server_privilege", &["server", "privilege"], "true"),
    define("has_server_privilege", &["user", "server", "privilege"], "true"),
    define("has_table_privilege", &["table", "privilege"], "true"),
    define("has_table_privilege", &["user", "table", "privilege"], "true"),
    define("has_tablespace_privilege", &["tablespace", "privilege"], "true"),
    define("has_tablespace_privilege", &["user", "tablespace", "privilege"], "true"),
    define("inet_client_addr", &[], "NULL"),
    define("inet_client_port", &[], "NULL"),
    define("inet_server_addr", &[], "NULL"),
    define("inet_server_port", &[], "NULL"),
    define(
        "json_group_array",
        &["x"],
        "CAST((('[' || string_agg(CASE  WHEN ((x IS NULL)) THEN (CAST('null' AS \"JSON\")) \
         ELSE to_json(x) END, ',')) || ']') AS \"JSON\")",
    ),
    define(
        "json_group_object",
        &["n", "v"],
        "CAST((('{' || string_agg(((CASE  WHEN ((n IS NULL)) THEN \
         (\"error\"('json_group_object key cannot be NULL')) ELSE to_json(CAST(n AS VARCHAR)) \
         END || ':') || CASE  WHEN ((v IS NULL)) THEN (CAST('null' AS \"JSON\")) ELSE to_json(v) \
         END), ',')) || '}') AS \"JSON\")",
    ),
    define("json_group_structure", &["x"], "(json_structure(json_group_array(x)) -> 0)"),
    define(
        "md5_number_lower",
        &["param"],
        "CAST(CAST(CAST(CAST(md5_number(param) AS BIT) AS VARCHAR)[:64] AS BIT) AS uint64)",
    ),
    define(
        "md5_number_upper",
        &["param"],
        "CAST(CAST(CAST(CAST(md5_number(param) AS BIT) AS VARCHAR)[65:] AS BIT) AS uint64)",
    ),
    define("obj_description", &["object_oid", "catalog_name"], "NULL"),
    define("pg_collation_is_visible", &["collation_oid"], "true"),
    define("pg_conf_load_time", &[], "current_timestamp"),
    define("pg_conversion_is_visible", &["conversion_oid"], "true"),
    define("pg_function_is_visible", &["function_oid"], "true"),
    define("pg_get_expr", &["pg_node_tree", "relation_oid"], "pg_node_tree"),
    define("pg_has_role", &["role", "privilege"], "true"),
    define("pg_has_role", &["user", "role", "privilege"], "true"),
    define("pg_is_other_temp_schema", &["schema_id"], "false"),
    define("pg_my_temp_schema", &[], "0"),
    define("pg_opclass_is_visible", &["opclass_oid"], "true"),
    define("pg_operator_is_visible", &["operator_oid"], "true"),
    define("pg_opfamily_is_visible", &["opclass_oid"], "true"),
    define("pg_postmaster_start_time", &[], "current_timestamp"),
    define("pg_table_is_visible", &["table_oid"], "true"),
    define("pg_ts_config_is_visible", &["config_oid"], "true"),
    define("pg_ts_dict_is_visible", &["dict_oid"], "true"),
    define("pg_ts_parser_is_visible", &["parser_oid"], "true"),
    define("pg_ts_template_is_visible", &["template_oid"], "true"),
    define("pg_type_is_visible", &["type_oid"], "true"),
    define("pg_typeof", &["expression"], "lower(typeof(expression))"),
    define(
        "regexp_split_to_table",
        &["text", "pattern"],
        "unnest(string_split_regex(\"text\", pattern))",
    ),
    define("shobj_description", &["object_oid", "catalog_name"], "NULL"),
    define(
        "split_part",
        &["string", "delimiter", "position"],
        "\"if\"(((string IS NOT NULL) AND (\"delimiter\" IS NOT NULL) AND (\"position\" IS NOT \
         NULL)), COALESCE(string_split(string, \"delimiter\")[\"position\"], ''), NULL)",
    ),
    define("wavg", &["value", "weight"], "weighted_avg(\"value\", weight)"),
    define(
        "weighted_avg",
        &["value", "weight"],
        "(sum((\"value\" * weight)) / sum(CASE  WHEN ((\"value\" IS NOT NULL)) THEN (weight) \
         ELSE 0 END))",
    ),
];

/// The macros whose body aggregates, which make the select block they are in aggregate the way a
/// call to an aggregate does.
const AGGREGATING: &[&str] = &[
    "geomean",
    "geometric_mean",
    "json_group_array",
    "json_group_object",
    "json_group_structure",
    "wavg",
    "weighted_avg",
];

/// Whether a call to `name` aggregates, which has to be known before anything in its block binds.
pub(crate) fn aggregates(name: &str) -> bool {
    AGGREGATING.iter().any(|held| rudb_catalog::same_name(name, held))
}

/// Whether `name` is one of the macros in the table, which a call written with `DISTINCT`, a
/// `FILTER` or an `ORDER BY` is refused for.
pub(crate) fn is_macro(name: &str) -> bool {
    MACROS.iter().any(|held| rudb_catalog::same_name(name, held.name))
}

/// The overload of `written` that takes `count` arguments, or `None` if `written` is not a macro.
fn chosen(written: &str, count: usize) -> Result<Option<&'static Macro>> {
    let overloads: Vec<&'static Macro> =
        MACROS.iter().filter(|held| rudb_catalog::same_name(written, held.name)).collect();
    let Some(first) = overloads.first() else {
        return Ok(None);
    };
    match overloads.iter().find(|held| held.parameters.len() == count) {
        Some(&chosen) => Ok(Some(chosen)),
        None => {
            let candidates: Vec<String> = overloads
                .iter()
                .map(|held| format!("\t{}({})", first.name, held.parameters.join(", ")))
                .collect();
            Err(Error::binder(format!(
                "Macro {}() does not support the supplied arguments. You might need to add \
                 explicit type casts.\nCandidate macros:\n{}",
                first.name,
                candidates.join("\n")
            )))
        }
    }
}

impl Binder<'_> {
    /// The expansion of a call to a built-in macro, or `None` if `written` is not one.
    ///
    /// The arguments are not bound first. They are bound inside the body, where the body puts
    /// them, which is what lets `geomean(x)` put `x` inside an aggregate.
    pub(crate) fn builtin_macro(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let Some(chosen) = chosen(written, arguments.len())? else {
            return Ok(None);
        };
        let texts: Vec<String> =
            arguments.iter().map(|&argument| deparse::expression(ast, argument)).collect();
        let text = substitute(chosen.body, chosen.parameters, &texts)?;
        self.bind_macro_body(chosen.name, &text, scope).map(Some)
    }

    /// The expansion of a call to a built-in macro written with a window, or `None` if the name is
    /// not one.
    ///
    /// The pin pushes the window down onto the one aggregate in the body, so that
    /// `json_group_array(v) OVER (ORDER BY v)` is the `string_agg` in its body over that window,
    /// and refuses a body with no aggregate or more than one, which a macro that calls another
    /// macro is. The `DISTINCT` and the `FILTER` go with the window, and so does `IGNORE NULLS`,
    /// which the aggregate then refuses. An `ORDER BY` inside the brackets is dropped, the way the
    /// pin drops it.
    pub(crate) fn builtin_window_macro(
        &mut self,
        ast: &Ast,
        call: &WindowCall<'_>,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let Some(chosen) = chosen(call.name, call.args.len())? else {
            return Ok(None);
        };
        let body = chosen.body;
        let tokens = tokenize(body)?;
        let calls: Vec<usize> = (0..tokens.len())
            .filter(|&at| {
                let word = tokens[at].text(body);
                let word =
                    word.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(word);
                tokens.get(at + 1).is_some_and(|next| next.text(body) == "(")
                    && kind_of(word) == Some(FunctionKind::Aggregate)
            })
            .collect();
        let [aggregate] = calls[..] else {
            return Err(Error::binder(
                "Window function macro bodies must contain exactly one aggregate function",
            ));
        };
        let open = tokens[aggregate + 1].end as usize;
        let mut depth = 0usize;
        let close = tokens[aggregate + 1..]
            .iter()
            .find(|token| {
                match token.text(body) {
                    "(" => depth += 1,
                    ")" => depth -= 1,
                    _ => {}
                }
                depth == 0
            })
            .map(|token| token.start as usize)
            .ok_or_else(|| Error::internal(format!("the body of {}", chosen.name)))?;
        let texts: Vec<String> =
            call.args.iter().map(|&argument| deparse::expression(ast, argument)).collect();
        let parameters = chosen.parameters;
        let text = format!(
            "{}{}{}{}){} {}{}",
            substitute(&body[..open], parameters, &texts)?,
            if call.distinct { "DISTINCT " } else { "" },
            substitute(&body[open..close], parameters, &texts)?,
            if call.ignore_nulls { " IGNORE NULLS" } else { "" },
            deparse::filtered(ast, call.filter),
            deparse::over(ast, call.spec),
            substitute(&body[close + 1..], parameters, &texts)?,
        );
        self.bind_macro_body(chosen.name, &text, scope).map(Some)
    }

    /// The text a macro's body came to once its arguments were put in, bound where the call was.
    fn bind_macro_body(&mut self, name: &str, text: &str, scope: &Scope) -> Result<ExprRef> {
        let body =
            parse_ast_with_case(&format!("SELECT {text}"), self.semantics.identifier_case())?;
        let expr = match body.statements.first() {
            Some(&ast::Statement::Query(query)) => match body.query(query).body {
                ast::QueryBody::Select(select) => {
                    body.target_list(body.select(select).targets).first().map(|target| target.expr)
                }
                _ => None,
            },
            _ => None,
        };
        let expr = expr.ok_or_else(|| Error::internal(format!("the body of {name}")))?;
        // Everything the body binds to is placed where the call was written, since the body's own
        // text is not anything the user wrote and a caret into it would point at the wrong words.
        let outer = self.pinned_span.replace(self.current_span);
        let bound = self.bind_expr(&body, expr, scope);
        self.pinned_span = outer;
        bound
    }
}

thread_local! {
    /// How many calls to a user's macro deep the binder is on this thread.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// How deep calls to a user's macro can go. A macro that calls itself, which a macro named after
/// the function it wraps does, would otherwise go on until the stack ran out. The pin stops it with
/// its limit on how deep an expression goes, and this stops it sooner, since every level here is a
/// parse and a bind, but with the pin's sentence.
const MAX_DEPTH: usize = 100;

/// One level deeper into calls to a user's macro, giving back the depth to put back after, or the
/// pin's refusal when that is too deep.
fn deeper() -> Result<usize> {
    let depth = DEPTH.get();
    if depth >= MAX_DEPTH {
        return Err(Error::binder(
            "Max expression depth limit of 1000 exceeded. Use \"SET max_expression_depth TO x\" \
             to increase the maximum expression depth.",
        ));
    }
    DEPTH.set(depth + 1);
    Ok(depth)
}

impl Binder<'_> {
    /// Whether an expression has an aggregate in it, counting a call to a user's macro whose body
    /// has one.
    pub(crate) fn aggregates(&self, ast: &Ast, expr: ast::ExprRef) -> bool {
        let catalog = self.catalog();
        crate::expr::aggregating(ast, expr, &|name| catalog.aggregating_macro(name))
    }

    /// The expansion of a call to a scalar macro a user made, or `None` if the name is not one.
    ///
    /// The overload is the first whose parameters the arguments fit: positional ones in order,
    /// named ones by name, and a default for every parameter neither gave. A typed parameter takes
    /// an argument of that type and no other. The body is then expanded the way a built-in macro's
    /// is, with the text of each argument in place of its parameter.
    pub(crate) fn user_macro(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        name: ast::Slice,
        args: ast::Slice,
        modified: bool,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let parts: Vec<&str> = ast.name(name).collect();
        let Some(found) = self.catalog().resolve_macro(&parts, None) else {
            return Ok(None);
        };
        let called = found.name.table.clone();
        if found.table {
            return Err(Error::binder(format!(
                "Function \"{called}\" is a table function but it was used as a scalar function. \
                 This function has to be called in a FROM clause (similar to a table)."
            )));
        }
        if let Some((_, said)) = ast.misnamed(call) {
            let written = parts.last().copied().unwrap_or_default();
            return Err(Error::binder(format!("Macro \"{written}\"() {said}")));
        }
        let overloads = found.overloads.clone();
        if modified {
            return Err(Error::invalid_input(format!(
                "Function \"{called}\" is a Macro Function. \"DISTINCT\", \"FILTER\", and \
                 \"ORDER BY\" are only applicable to window and aggregate functions."
            )));
        }
        let (positional, named) = ast.written_args(call, args);
        let text = self.expanded(ast, &called, &overloads, positional, named, scope)?;
        let depth = deeper()?;
        let bound = self.bind_macro_body(&called, &text, scope);
        DEPTH.set(depth);
        bound.map(Some)
    }

    /// The rows of a call to a table macro a user made, or `None` if the name is not one.
    ///
    /// The body, with the arguments in it, is bound the way a subquery written there would be, and
    /// its rows are named after the macro unless the call has an alias.
    pub(crate) fn table_macro(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        args: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<Option<(NodeRef, Scope)>> {
        let parts: Vec<&str> = ast.name(name).collect();
        let Some(found) = self.catalog().resolve_macro(&parts, Some(true)) else {
            return Ok(None);
        };
        let called = found.name.table.clone();
        let overloads = found.overloads.clone();
        let written = ast.target_list(args);
        let positional: Vec<ast::ExprRef> = written
            .iter()
            .filter(|target| target.alias == NONE)
            .map(|target| target.expr)
            .collect();
        let named: Vec<ast::Target> =
            written.iter().filter(|target| target.alias != NONE).copied().collect();
        let first_named = written.iter().position(|target| target.alias != NONE);
        if first_named.is_some_and(|first| written[first..].iter().any(|t| t.alias == NONE)) {
            return Err(Error::binder(format!(
                "Macro \"{}\"() has positional argument following named argument",
                parts.last().copied().unwrap_or_default()
            )));
        }
        for (index, target) in named.iter().enumerate() {
            let text = ast.string(target.alias);
            if named[..index]
                .iter()
                .any(|before| ast.string(before.alias).eq_ignore_ascii_case(text))
            {
                return Err(Error::binder(format!(
                    "Macro \"{}\"() has named argument repeated '\"{text}\"'",
                    parts.last().copied().unwrap_or_default()
                )));
            }
        }
        let empty = Scope::empty();
        let text = self.expanded(ast, &called, &overloads, &positional, &named, &empty)?;
        let parsed = parse_ast_with_case(&text, self.semantics.identifier_case())?;
        let Some(&ast::Statement::Query(query)) = parsed.statements.first() else {
            return Err(Error::internal(format!("the body of {called}")));
        };
        let depth = deeper()?;
        let bound = self.bind_query(&parsed, query);
        DEPTH.set(depth);
        let (node, mut scope) = bound?;
        let label = if alias == NONE { called } else { ast.string(alias).to_string() };
        scope.relabel(&label);
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok(Some((node, scope)))
    }

    /// The body of the first overload the arguments fit with the arguments in it, or the pin's
    /// refusal naming every overload if they fit none.
    fn expanded(
        &mut self,
        ast: &Ast,
        called: &str,
        overloads: &[Overload],
        positional: &[ast::ExprRef],
        named: &[ast::Target],
        scope: &Scope,
    ) -> Result<String> {
        let starred =
            positional.iter().any(|&argument| matches!(ast.expr(argument), ast::Expr::Star { .. }));
        if !starred {
            for overload in overloads {
                if let Some(texts) = self.fits(ast, overload, positional, named, scope)? {
                    let names: Vec<&str> = overload
                        .parameters
                        .iter()
                        .map(|parameter| parameter.name.as_str())
                        .collect();
                    return substitute(&overload.body, &names, &texts);
                }
            }
        }
        let candidates: Vec<String> =
            overloads.iter().map(|overload| format!("\t{}", overload.signature(called))).collect();
        Err(Error::binder(format!(
            "Macro {called}() does not support the supplied arguments. You might need to add \
             explicit type casts.\nCandidate macros:\n{}",
            candidates.join("\n")
        )))
    }

    /// The text of the argument each parameter of an overload gets, or `None` if the call does not
    /// fit it.
    fn fits(
        &mut self,
        ast: &Ast,
        overload: &Overload,
        positional: &[ast::ExprRef],
        named: &[ast::Target],
        scope: &Scope,
    ) -> Result<Option<Vec<String>>> {
        let parameters = &overload.parameters;
        if positional.len() > parameters.len() {
            return Ok(None);
        }
        let mut given: Vec<Option<ast::ExprRef>> = vec![None; parameters.len()];
        for (slot, &argument) in positional.iter().enumerate() {
            given[slot] = Some(argument);
        }
        for target in named {
            let written = ast.string(target.alias);
            let Some(slot) = parameters
                .iter()
                .position(|parameter| rudb_catalog::same_name(&parameter.name, written))
            else {
                return Ok(None);
            };
            if given[slot].is_some() {
                return Ok(None);
            }
            given[slot] = Some(target.expr);
        }
        let mut texts = Vec::with_capacity(parameters.len());
        for (parameter, argument) in parameters.iter().zip(given) {
            let Some(argument) = argument else {
                match &parameter.default {
                    Some(default) => texts.push(default.clone()),
                    None => return Ok(None),
                }
                continue;
            };
            if let Some(ty) = &parameter.ty {
                let wanted = rudb_common::LogicalType::parse(ty)?;
                let bound = self.bind_expr(ast, argument, scope)?;
                if *self.plan().expr_type(bound) != wanted {
                    return Ok(None);
                }
            }
            texts.push(deparse::expression(ast, argument));
        }
        Ok(Some(texts))
    }
}

/// Binds `CREATE MACRO` or `DROP MACRO`.
///
/// A body is checked here the way the pin checks it: bound with every parameter standing for a
/// null of its type, so that a column it names which is not a parameter, a function that is not
/// there, or a star is refused now rather than at the first call. Anything else that goes wrong is
/// left for the call, since a null is not what a call will put there and an error about what it
/// can do is not an error about the macro.
pub(crate) fn statement(
    ast: &Ast,
    catalog: &Catalog,
    given: &Parameters,
    session: &Session,
    index: ast::MacroRef,
) -> Result<Bound> {
    let written = ast.macro_def(index);
    let parts: Vec<&str> = ast.name(written.name).collect();
    if written.drop {
        let found = catalog.resolve_macro(&parts, written.table);
        if found.is_none() && !written.quiet {
            return Err(Error::catalog(format!(
                "{} with name {} does not exist!",
                rudb_catalog::macros::kind(written.table == Some(true)),
                parts.last().copied().unwrap_or_default()
            )));
        }
        return Ok(Bound::Macro(MacroChange {
            name: found.map(|held| held.name.clone()),
            table: found.is_some_and(|held| held.table),
            made: None,
            or_replace: false,
            if_not_exists: false,
        }));
    }
    let name = if written.temporary {
        catalog.resolve_for_create_temporary(&parts)?
    } else {
        catalog.resolve_for_create(&parts)?
    };
    let table = written.overloads.first().is_some_and(|overload| overload.table);
    let case = session.semantics().identifier_case();
    let mut overloads = Vec::with_capacity(written.overloads.len());
    for overload in &written.overloads {
        let parameters: Vec<Parameter> = overload
            .parameters
            .iter()
            .map(|(name, ty, default)| Parameter {
                name: name.clone(),
                ty: ty.clone(),
                default: default.clone(),
            })
            .collect();
        for parameter in &parameters {
            let Some(default) = &parameter.default else {
                continue;
            };
            let checked =
                parse_ast_with_case(&format!("SELECT {default}"), case).and_then(|default| {
                    crate::statement::bind_one(&default, catalog, &given.uncaught(), session, false)
                });
            if let Err(error) = checked
                && error.to_string().contains("Referenced column")
            {
                return Err(Error::binder(format!(
                    "Default value for parameter \"{}\" cannot contain column names",
                    parameter.name
                )));
            }
        }
        let aggregating = if overload.table {
            read_tables(catalog, given, session, &overload.body, &parameters)?;
            false
        } else {
            checked(catalog, given, session, &overload.body, &parameters)?
        };
        overloads.push(Overload { parameters, body: overload.body.clone(), aggregating });
    }
    if written.or_replace {
        let mut seen = Vec::new();
        let calls_itself =
            overloads.iter().any(|overload| depends(catalog, &overload.body, &name, &mut seen));
        if calls_itself {
            return Err(Error::catalog("CREATE OR REPLACE is not allowed to depend on itself"));
        }
    }
    Ok(Bound::Macro(MacroChange {
        name: Some(name.clone()),
        table,
        made: Some(rudb_catalog::Macro { name, table, overloads, oid: 0 }),
        or_replace: written.or_replace,
        if_not_exists: written.quiet,
    }))
}

/// A macro read back out of a database file, made from the statement the file kept and put in
/// `database` without the checks `CREATE MACRO` runs, which were run when it was made.
/// `aggregating` says for each overload whether its body aggregates, which the file keeps beside
/// the statement because the macros a body calls may be read back after it.
///
/// # Errors
///
/// If the statement is not one `CREATE MACRO`.
pub fn kept_macro(sql: &str, database: &str, aggregating: &[bool]) -> Result<rudb_catalog::Macro> {
    let parsed = rudb_parse::parse_ast(sql)?;
    let Some(&ast::Statement::Macro(index)) = parsed.statements.first() else {
        return Err(Error::internal(format!("a kept macro that is not one: {sql}")));
    };
    let written = parsed.macro_def(index);
    let bare = parsed.name(written.name).last().unwrap_or_default().to_string();
    let table = written.overloads.first().is_some_and(|overload| overload.table);
    let overloads = written
        .overloads
        .iter()
        .enumerate()
        .map(|(at, overload)| Overload {
            parameters: overload
                .parameters
                .iter()
                .map(|(name, ty, default)| Parameter {
                    name: name.clone(),
                    ty: ty.clone(),
                    default: default.clone(),
                })
                .collect(),
            body: overload.body.clone(),
            aggregating: aggregating.get(at).copied().unwrap_or(false),
        })
        .collect();
    let name = QualifiedName::new(database, rudb_catalog::DEFAULT_SCHEMA, bare);
    Ok(rudb_catalog::Macro { name, table, overloads, oid: 0 })
}

/// Binds a table macro's body with a null in place of each parameter, which the pin does too, so
/// that a body reading a table that is not there is refused when it is made.
fn read_tables(
    catalog: &Catalog,
    given: &Parameters,
    session: &Session,
    body: &str,
    parameters: &[Parameter],
) -> Result<()> {
    let names: Vec<&str> = parameters.iter().map(|parameter| parameter.name.as_str()).collect();
    let nulls = vec!["NULL".to_string(); names.len()];
    let text = substitute(body, &names, &nulls)?;
    let Ok(parsed) = parse_ast_with_case(&text, session.semantics().identifier_case()) else {
        return Ok(());
    };
    let Err(error) =
        crate::statement::bind_one(&parsed, catalog, &given.uncaught(), session, false)
    else {
        return Ok(());
    };
    let message = error.to_string();
    let missing = message
        .strip_prefix("Catalog Error: Table with name ")
        .and_then(|rest| rest.split_once(" does not exist"))
        .map(|(name, _)| name.to_ascii_lowercase());
    if missing.is_some_and(|name| body.to_ascii_lowercase().contains(&name)) {
        return Err(error);
    }
    Ok(())
}

/// Binds a scalar macro's body with a null in place of each parameter, and answers whether it has
/// an aggregate in it.
fn checked(
    catalog: &Catalog,
    given: &Parameters,
    session: &Session,
    body: &str,
    parameters: &[Parameter],
) -> Result<bool> {
    let names: Vec<&str> = parameters.iter().map(|parameter| parameter.name.as_str()).collect();
    let nulls: Vec<String> = parameters
        .iter()
        .map(|parameter| match &parameter.ty {
            Some(ty) => format!("NULL::{ty}"),
            None => "NULL".to_string(),
        })
        .collect();
    let text = substitute(body, &names, &nulls)?;
    let parsed =
        parse_ast_with_case(&format!("SELECT {text}"), session.semantics().identifier_case())?;
    let expr = match parsed.statements.first() {
        Some(&ast::Statement::Query(query)) => match parsed.query(query).body {
            ast::QueryBody::Select(select) => {
                parsed.target_list(parsed.select(select).targets).first().map(|target| target.expr)
            }
            _ => None,
        },
        _ => None,
    };
    let Some(expr) = expr else {
        return Ok(false);
    };
    if matches!(parsed.expr(expr), ast::Expr::Star { .. }) {
        return Err(Error::binder("STAR expression is not supported here"));
    }
    if let Err(error) =
        crate::statement::bind_one(&parsed, catalog, &given.uncaught(), session, false)
    {
        // A missing function counts only when the body names it, since `x.a.b` binds to
        // `struct_extract` over a `NULL`, which has no field to take.
        let message = error.to_string();
        let missing = message
            .split_once("with name ")
            .and_then(|(_, rest)| rest.split_once(" does not exist"))
            .map(|(name, _)| name.trim_matches('"').to_ascii_lowercase());
        let written = missing.is_none_or(|name| body.to_ascii_lowercase().contains(&name));
        if message.contains("Referenced column")
            || (message.starts_with("Catalog Error") && written)
            || message.contains("Conflicting column names")
            || message.contains("Window functions are not supported here")
        {
            return Err(error);
        }
    }
    // A parameter is a column to the pin while the body is bound, so a query in the body that
    // reads a table with a column of that name finds it twice. Left as a name, a parameter that
    // binds at all has been found in the body's own tables.
    if body.to_ascii_lowercase().contains("select") {
        for (index, name) in names.iter().enumerate() {
            let mut values = nulls.clone();
            values[index] = quoted(name);
            let text = substitute(body, &names, &values)?;
            if text == substitute(body, &names, &nulls)? {
                continue;
            }
            let parsed = parse_ast_with_case(
                &format!("SELECT {text}"),
                session.semantics().identifier_case(),
            )?;
            let bound =
                crate::statement::bind_one(&parsed, catalog, &given.uncaught(), session, false);
            if bound.is_ok() {
                return Err(Error::binder(format!("Conflicting column names for column {name}!")));
            }
        }
    }
    Ok(crate::expr::aggregating(&parsed, expr, &|name| catalog.aggregating_macro(name)))
}

/// Whether a body calls the macro of that name, itself or through the macros it calls.
fn depends(catalog: &Catalog, body: &str, name: &QualifiedName, seen: &mut Vec<String>) -> bool {
    let Ok(tokens) = tokenize(body) else {
        return false;
    };
    for (at, token) in tokens.iter().enumerate() {
        if !tokens.get(at + 1).is_some_and(|next| next.text(body) == "(") {
            continue;
        }
        let text = token.text(body);
        let word = text.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(text);
        let Some(found) = catalog.resolve_macro(&[word], None) else {
            continue;
        };
        if found.name == *name {
            return true;
        }
        let key = found.name.to_string();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        if found.overloads.iter().any(|overload| depends(catalog, &overload.body, name, seen)) {
            return true;
        }
    }
    false
}

/// The body with every mention of a parameter replaced by its argument, in brackets.
///
/// A mention is a bare or quoted word that spells the parameter and is not a function's name or a
/// named argument's, which rules out the `"day"` in `"day"(last_day(date))` and would rule out the
/// `"key"` before a `:=`.
fn substitute(body: &str, parameters: &[&str], arguments: &[String]) -> Result<String> {
    let tokens = tokenize(body)?;
    let mut out = String::with_capacity(body.len());
    let mut copied = 0;
    for (at, token) in tokens.iter().enumerate() {
        if !(token.kind.is_identifier() || token.kind == Kind::Keyword) {
            continue;
        }
        let text = token.text(body);
        let word = text.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')).unwrap_or(text);
        let Some(position) = parameters.iter().position(|name| name.eq_ignore_ascii_case(word))
        else {
            continue;
        };
        let next = tokens.get(at + 1).map(|next| next.text(body));
        if matches!(next, Some("(" | ":=")) {
            continue;
        }
        // The `a` in `t.a` is a column of `t` and the one in `x AS a` is a new name, and neither
        // is the parameter.
        let previous = at.checked_sub(1).map(|before| tokens[before].text(body));
        if previous.is_some_and(|before| before == "." || before.eq_ignore_ascii_case("AS")) {
            continue;
        }
        out.push_str(&body[copied..token.start as usize]);
        out.push('(');
        out.push_str(&arguments[position]);
        out.push(')');
        copied = token.end as usize;
    }
    out.push_str(&body[copied..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parameter_is_replaced_where_it_is_a_value_and_nowhere_else() {
        let one = |body: &str, parameters: &[&str], arguments: &[&str]| {
            let arguments: Vec<String> = arguments.iter().map(ToString::to_string).collect();
            substitute(body, parameters, &arguments).unwrap()
        };
        assert_eq!(
            one("(x - (y * floor((x / y))))", &["x", "y"], &["a", "2"]),
            "((a) - ((2) * floor(((a) / (2)))))"
        );
        assert_eq!(one("\"day\"(last_day(date))", &["date"], &["d"]), "\"day\"(last_day((d)))");
        assert_eq!(one("(date + \"interval\")", &["date", "interval"], &["d", "i"]), "((d) + (i))");
        assert_eq!(
            one("struct_pack(\"key\" := \"key\")", &["key"], &["k"]),
            "struct_pack(\"key\" := (k))"
        );
    }
}
