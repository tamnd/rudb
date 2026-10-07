//! The functions of a PostgreSQL session that are written another way here.
//!
//! PostgreSQL has functions that the engine does not have under the same name, such as
//! `cardinality` and `num_nulls`. A call to one of them is written out as SQL that the engine has
//! and is bound where the call was, the way a built-in macro is.
//!
//! PostgreSQL also has functions that the engine has with a wider result. `length` is an `int4`
//! there and a BIGINT here, and `sum` of an `int4` is an `int8` there and a HUGEINT here. A call to
//! one of them is bound as usual and then cast to the type that PostgreSQL gives it, so a client
//! that reads the type from the RowDescription gets the type it expects.

use rudb_common::{LogicalType, Result, Value};
use rudb_parse::{Ast, ast, deparse};
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef};

use crate::binder::Binder;
use crate::scope::Scope;

/// The functions whose result is an `int4` in PostgreSQL and a BIGINT here.
const INTEGER_RESULTS: &[&str] = &[
    "array_length",
    "ascii",
    "bit_length",
    "char_length",
    "character_length",
    "length",
    "octet_length",
    "position",
    "strpos",
];

/// Whether `ty` fits in an `int4`.
fn narrow(ty: &LogicalType) -> bool {
    matches!(ty, LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer)
}

/// Whether `field` is a string literal that names the seconds field of `date_part`.
fn seconds(field: &str) -> bool {
    let name = field.strip_prefix('\'').and_then(|name| name.strip_suffix('\''));
    name.is_some_and(|name| {
        ["second", "seconds", "sec", "secs", "s"].iter().any(|held| name.eq_ignore_ascii_case(held))
    })
}

/// How many arrays are nested in `ty`, which is the number of dimensions of a PostgreSQL array.
fn depth(ty: &LogicalType) -> usize {
    match ty {
        LogicalType::List(inner) | LogicalType::Array(inner, _) => 1 + depth(inner),
        _ => 0,
    }
}

impl Binder<'_> {
    /// The call `written(arguments)` bound as PostgreSQL binds it, or `None` when the name is not
    /// one of the functions that this module writes out.
    pub(crate) fn postgres_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let named = |name: &str| rudb_catalog::same_name(written, name);
        if arguments.is_empty() && named("pg_backend_pid") {
            let backend = self.session.postgres().map_or(0, |postgres| postgres.backend);
            return Ok(Some(self.add_constant(Value::Integer(backend))));
        }
        // `now()` gives the start of the transaction, `statement_timestamp()` the start of this
        // statement and `clock_timestamp()` the clock at each row.
        if arguments.is_empty() && named("statement_timestamp") {
            let started = self.session.statement_start().unwrap_or_else(crate::context::micros_now);
            return Ok(Some(self.add_constant(Value::TimestampTz(started))));
        }
        if arguments.is_empty() && named("clock_timestamp") {
            let args = self.plan_mut().add_expr_list(&[]);
            let name = self.plan_mut().intern("clock_timestamp");
            return Ok(Some(
                self.add_expr(Expr::Function { name, args }, LogicalType::TimestampTz),
            ));
        }
        let texts: Vec<String> =
            arguments.iter().map(|&argument| deparse::expression(ast, argument)).collect();
        let text = match texts.as_slice() {
            [_, ..] if named("num_nulls") || named("num_nonnulls") => {
                let test = if named("num_nulls") { "IS NULL" } else { "IS NOT NULL" };
                let each: Vec<String> = texts
                    .iter()
                    .map(|text| format!("CAST((({text}) {test}) AS INTEGER)"))
                    .collect();
                format!("CAST(({}) AS INTEGER)", each.join(" + "))
            }
            // The bounds can be in either order. A value below the first bound is in bucket 0 and
            // a value at or past the second bound is in the bucket after the last.
            [value, low, high, count] if named("width_bucket") => {
                let value = format!("CAST(({value}) AS DOUBLE)");
                format!(
                    "CAST(CASE WHEN (({low}) < ({high})) THEN (CASE WHEN ({value} < ({low})) THEN \
                     (0) WHEN ({value} >= ({high})) THEN ((({count}) + 1)) ELSE ((floor(((({value} \
                     - ({low})) * ({count})) / (({high}) - ({low})))) + 1)) END) ELSE (CASE WHEN \
                     ({value} > ({low})) THEN (0) WHEN ({value} <= ({high})) THEN ((({count}) + \
                     1)) ELSE ((floor((((({low}) - {value}) * ({count})) / (({low}) - ({high})))) \
                     + 1)) END) END AS INTEGER)"
                )
            }
            // The fill is a space when the call does not give one.
            [string, length] if named("lpad") || named("rpad") => {
                format!("{written}(({string}), ({length}), ' ')")
            }
            [string, pattern] if named("regexp_count") => {
                format!("CAST(len(regexp_extract_all(({string}), ({pattern}))) AS INTEGER)")
            }
            // The first match starts after the text that is left when the first match and all
            // that follows it are taken away.
            [string, pattern] if named("regexp_instr") => format!(
                "CAST(CASE WHEN ((({string}) IS NULL) OR (({pattern}) IS NULL)) THEN (NULL) WHEN \
                 regexp_matches(({string}), ({pattern})) THEN ((length(regexp_replace(({string}), \
                 (('(?:' || ({pattern})) || ')(?s:.*)'), '')) + 1)) ELSE 0 END AS INTEGER)"
            ),
            [array] if named("cardinality") => {
                let depth = self.array_depth(ast, arguments[0], scope)?;
                let mut flat = format!("({array})");
                for _ in 1..depth {
                    flat = format!("flatten({flat})");
                }
                format!("CAST(len({flat}) AS INTEGER)")
            }
            [array] if named("array_ndims") => {
                let depth = self.array_depth(ast, arguments[0], scope)?;
                format!("CAST(CASE WHEN (len(({array})) > 0) THEN ({depth}) END AS INTEGER)")
            }
            // An array here starts at 1 in every dimension, and each element of a dimension is as
            // long as the first one, so the length of dimension `n` is the length of the element
            // that `n - 1` subscripts of 1 reach.
            [array, dimension]
                if named("array_length") || named("array_lower") || named("array_upper") =>
            {
                let depth = self.array_depth(ast, arguments[0], scope)?;
                let bound = match named("array_lower") {
                    true => "1".to_string(),
                    false => {
                        let arms: Vec<String> = (1..=depth)
                            .map(|at| {
                                let reached = format!("({array}){}", "[1]".repeat(at - 1));
                                format!("WHEN {at} THEN len({reached})")
                            })
                            .collect();
                        format!("CASE ({dimension}) {} END", arms.join(" "))
                    }
                };
                format!(
                    "CAST(CASE WHEN ((len(({array})) > 0) AND (({dimension}) BETWEEN 1 AND \
                     {depth})) THEN ({bound}) END AS INTEGER)"
                )
            }
            // The seconds field of PostgreSQL has the fraction of the second in it, and the pin
            // has that in its microseconds field.
            [field, moment] if named("date_part") && seconds(field) => {
                format!("(CAST(date_part('microsecond', ({moment})) AS DOUBLE) / 1000000)")
            }
            _ => return Ok(None),
        };
        self.bind_macro_body(written, &text, scope).map(Some)
    }

    /// The number of dimensions of the array that `argument` is, from its type.
    fn array_depth(&mut self, ast: &Ast, argument: ast::ExprRef, scope: &Scope) -> Result<usize> {
        let bound = self.bind_expr(ast, argument, scope)?;
        Ok(depth(self.plan().expr_type(bound)).max(1))
    }

    /// The call `written` resolved to `call`, cast to the type that PostgreSQL gives the result.
    pub(crate) fn postgres_result(&mut self, written: &str, call: ExprRef) -> ExprRef {
        let named =
            |names: &[&str]| names.iter().any(|name| rudb_catalog::same_name(written, name));
        let ty = self.plan().expr_type(call).clone();
        if ty == LogicalType::BigInt && named(INTEGER_RESULTS) {
            return self.cast_to(call, &LogicalType::Integer);
        }
        // `date_part` is a `float8` in PostgreSQL, where it is a BIGINT here for most fields.
        if ty == LogicalType::BigInt && named(&["date_part"]) {
            return self.cast_to(call, &LogicalType::Double);
        }
        call
    }

    /// The call `written(types)` resolved to `call`, cast to the type of the overload that
    /// PostgreSQL chooses for these types.
    ///
    /// `gcd` and `lcm` have overloads over `int4`, `int8` and `numeric` there and only over a
    /// BIGINT and a HUGEINT here, so the types have to be the ones that were given and not the
    /// ones that the call cast them to. `sign` is a TINYINT here and has overloads over `float8`
    /// and `numeric` there, and an integer goes to `float8`.
    pub(crate) fn postgres_narrowed(
        &mut self,
        written: &str,
        types: &[LogicalType],
        call: ExprRef,
    ) -> ExprRef {
        if rudb_catalog::same_name(written, "sign") && types.len() == 1 {
            let ty = match types[0] {
                LogicalType::Decimal { .. } | LogicalType::Numeric => LogicalType::Numeric,
                _ => LogicalType::Double,
            };
            if *self.plan().expr_type(call) != ty {
                return self.cast_to(call, &ty);
            }
            return call;
        }
        let named = ["gcd", "lcm"].iter().any(|name| rudb_catalog::same_name(written, name));
        match named && !types.is_empty() && types.iter().all(narrow) {
            true if *self.plan().expr_type(call) == LogicalType::BigInt => {
                self.cast_to(call, &LogicalType::Integer)
            }
            _ => call,
        }
    }

    /// Casts the value of a call of `round`, `trunc`, `ceil` or `floor` to the overload that
    /// PostgreSQL chooses for it.
    ///
    /// PostgreSQL has these over `float8` and `numeric` and not over an integer. An integer goes
    /// to `float8` when the call has one argument, and to `numeric` when it has a scale, since
    /// only the `numeric` overload takes one. A parameter of no type with a scale is a `numeric`
    /// for the same reason.
    pub(crate) fn postgres_rounding(&mut self, written: &str, arguments: &mut [ExprRef]) {
        let named =
            |names: &[&str]| names.iter().any(|name| rudb_catalog::same_name(written, name));
        let ty = match arguments.len() {
            1 if named(&["round", "trunc", "ceil", "ceiling", "floor"]) => LogicalType::Double,
            2 if named(&["round", "trunc"]) => LogicalType::Numeric,
            _ => return,
        };
        let value = arguments[0];
        let given = self.plan().expr_type(value);
        let placeholder = *given == LogicalType::Null && self.is_placeholder(value);
        if given.is_integer() || (placeholder && ty == LogicalType::Numeric) {
            arguments[0] = self.cast_to(value, &ty);
        }
    }

    /// Whether `function(arguments)` is the series over `int4` of PostgreSQL, which gives an
    /// `int4` column where the series here gives a BIGINT. A parameter of no type is typed as an
    /// `int4` here, as PostgreSQL types it.
    pub(crate) fn postgres_series(&mut self, function: &str, arguments: &mut [ExprRef]) -> bool {
        if self.session.postgres().is_none()
            || !rudb_catalog::same_name(function, "generate_series")
            || arguments.is_empty()
        {
            return false;
        }
        let integers = arguments.iter().all(|&argument| {
            narrow(self.plan().expr_type(argument)) || self.is_placeholder(argument)
        });
        if integers {
            for argument in arguments.iter_mut() {
                if self.is_placeholder(*argument) {
                    *argument = self.cast_to(*argument, &LogicalType::Integer);
                }
            }
        }
        integers
    }

    /// A projection over the series `node` that casts its BIGINT column to an `int4`.
    pub(crate) fn integer_series(&mut self, node: NodeRef, mut scope: Scope) -> (NodeRef, Scope) {
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.columns.len());
        let mut names = Vec::with_capacity(scope.columns.len());
        for column in &scope.columns {
            let read = self.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
            exprs.push(match column.ty {
                LogicalType::BigInt => self.cast_to(read, &LogicalType::Integer),
                _ => read,
            });
            names.push(self.plan_mut().intern(&column.name));
        }
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
            if column.ty == LogicalType::BigInt {
                column.ty = LogicalType::Integer;
            }
        }
        let exprs = self.plan_mut().add_expr_list(&exprs);
        let names = self.plan_mut().add_name_list(&names);
        (self.add_node(Node::Project { input: node, index, exprs, names }), scope)
    }
}
