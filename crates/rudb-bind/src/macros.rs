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
//! JSON type, `md5_number_lower` and `md5_number_upper` wait for BIT, and `days_in_month` waits for
//! `last_day`.

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
    define("current_role", &[], "'duckdb'"),
    define("date_add", &["date", "interval"], "(date + \"interval\")"),
    define("fdiv", &["x", "y"], "floor((x / y))"),
    define("fmod", &["x", "y"], "(x - (y * floor((x / y))))"),
    define("geomean", &["x"], "exp(avg(ln(x)))"),
    define(
        "generate_subscripts",
        &["arr", "dim"],
        "unnest(generate_series(1, array_length(arr, dim)))",
    ),
    define("geometric_mean", &["x"], "geomean(x)"),
    define(
        "regexp_split_to_table",
        &["text", "pattern"],
        "unnest(string_split_regex(\"text\", pattern))",
    ),
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
