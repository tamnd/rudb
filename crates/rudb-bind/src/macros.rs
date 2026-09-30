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
//! `crate::expr`, and `if` and `nullif` bind as the CASE they stand for. The json macros wait for a
//! JSON type.
//!
//! The pin keeps the PostgreSQL shims, `pg_typeof`, the `has_*_privilege` pairs and the
//! `pg_*_is_visible` family among them, in `pg_catalog` and the rest in `main`. Both are on the
//! search path, so a bare call finds either, and the table does not tell them apart.
//! `pg_get_viewdef` and `pg_get_constraintdef` wait for arguments that are bound where the call is,
//! since their bodies are queries of a table whose columns would capture an argument written as
//! `view_oid`, where the pin reads the caller's column.

use rudb_common::{Error, Result};
use rudb_parse::{Ast, Kind, ast, deparse, parse_ast_with_case, tokenize};
use rudb_plan::ExprRef;

use crate::binder::Binder;
use crate::scope::Scope;

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
const AGGREGATING: &[&str] = &["geomean", "geometric_mean", "wavg", "weighted_avg"];

/// Whether a call to `name` aggregates, which has to be known before anything in its block binds.
pub(crate) fn aggregates(name: &str) -> bool {
    AGGREGATING.iter().any(|held| rudb_catalog::same_name(name, held))
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
        let overloads: Vec<&Macro> =
            MACROS.iter().filter(|held| rudb_catalog::same_name(written, held.name)).collect();
        let Some(first) = overloads.first() else {
            return Ok(None);
        };
        let Some(chosen) = overloads.iter().find(|held| held.parameters.len() == arguments.len())
        else {
            let candidates: Vec<String> = overloads
                .iter()
                .map(|held| format!("\t{}({})", first.name, held.parameters.join(", ")))
                .collect();
            return Err(Error::binder(format!(
                "Macro {}() does not support the supplied arguments. You might need to add \
                 explicit type casts.\nCandidate macros:\n{}",
                first.name,
                candidates.join("\n")
            )));
        };
        let texts: Vec<String> =
            arguments.iter().map(|&argument| deparse::expression(ast, argument)).collect();
        let text = substitute(chosen.body, chosen.parameters, &texts)?;
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
        let expr = expr.ok_or_else(|| Error::internal(format!("the body of {}", chosen.name)))?;
        // Everything the body binds to is placed where the call was written, since the body's own
        // text is not anything the user wrote and a caret into it would point at the wrong words.
        let outer = self.pinned_span.replace(self.current_span);
        let bound = self.bind_expr(&body, expr, scope);
        self.pinned_span = outer;
        bound.map(Some)
    }
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
