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

use rudb_catalog::{Catalog, same_name};
use rudb_common::{
    Error, FunctionRules, LogicalType, RegexRules, Result, SetFunctions, SqlState, Value,
};
use rudb_kernels::pgjson::JsonSet;
use rudb_kernels::pgregexp::{self, Function};
use rudb_parse::ast::LiteralKind;
use rudb_parse::{Ast, ast, deparse};
use rudb_pgtypes::keywords::quote_identifier;
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef};

use crate::advisory::{no_such_function, spelled_call};
use crate::binder::Binder;
use crate::scope::Scope;

/// How a call was written apart from its arguments, for the rules of `func_get_detail`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Written<'a> {
    /// The names of the last arguments, for a call with named arguments.
    pub(crate) names: &'a [&'a str],
    /// The call has `VARIADIC` before its last argument.
    pub(crate) variadic: bool,
}

/// The PostgreSQL type of a value of `ty` when that type binds back as `ty`, so that the rules for
/// a call see the type that the value has. A DECIMAL is a `numeric` with a typmod.
fn exact_oid(ty: &LogicalType) -> Option<rudb_pgtypes::Oid> {
    let oid = rudb_pgtypes::pg_type(ty).oid;
    let back = rudb_pgtypes::logical_type(oid)?;
    let exact = back == *ty || matches!(ty, LogicalType::Decimal { .. });
    exact.then_some(oid)
}

/// A function of `pg_proc` named `written` whose every form returns a set that a kernel here
/// gives as an array, such as `string_to_table`, which a select list and `FROM` unnest.
pub(crate) fn rows_function(written: &str) -> bool {
    let procs = rudb_pgtypes::procs(written);
    !procs.is_empty()
        && procs.iter().all(|proc| {
            proc.kind == b'f'
                && proc.retset
                && matches!(proc.lang, b'i' | b'c')
                && rudb_kernels::pgproc::rows_of(proc.src).is_some()
        })
}

/// A function of `pg_proc` named `written` whose every form has a kernel here, so that a call of
/// it in a PostgreSQL session is the kernel and not a macro of the pin with the same name.
pub(crate) fn kernel_function(written: &str) -> bool {
    let procs = rudb_pgtypes::procs(written);
    !procs.is_empty()
        && procs.iter().all(|proc| {
            proc.kind == b'f'
                && matches!(proc.lang, b'i' | b'c')
                && rudb_kernels::pgproc::has(proc.src)
        })
}

/// The list of one dimension inside a list of lists, or `ty` itself.
fn innermost_list(ty: &LogicalType) -> &LogicalType {
    match ty {
        LogicalType::List(element) if matches!(**element, LogicalType::List(_)) => {
            innermost_list(element)
        }
        ty => ty,
    }
}

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

/// The text of `expr` when it is a string literal.
fn string_literal(ast: &Ast, expr: ast::ExprRef) -> Option<&str> {
    match ast.expr(expr) {
        ast::Expr::Literal { kind: LiteralKind::String, text } => Some(ast.string(text)),
        _ => None,
    }
}

/// The name of the sequence in a default of the form `nextval('name')`.
fn nextval_of(default: &str) -> Option<String> {
    let inside = default.strip_prefix("nextval('")?.strip_suffix("')")?;
    Some(inside.replace("''", "'"))
}

/// A dotted name in text split into its parts, as PostgreSQL reads one: a part in quotes keeps its
/// case and a doubled quote in it is one quote, and the rest is folded to lower case.
fn folded_parts(text: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut rest = text.chars().peekable();
    let mut inside = false;
    while let Some(c) = rest.next() {
        let part = parts.last_mut().expect("one part at least");
        match c {
            '"' if inside && rest.peek() == Some(&'"') => {
                rest.next();
                part.push('"');
            }
            '"' => inside = !inside,
            '.' if !inside => parts.push(String::new()),
            c if inside => part.push(c),
            c => part.extend(c.to_lowercase()),
        }
    }
    parts
}

/// What `pg_get_serial_sequence(table, column)` gives: the name of the sequence that the default of
/// the column takes its values from, with its schema, when the table owns that sequence, and a
/// null when it does not. The table is read as PostgreSQL reads a name in text, folded to lower
/// case unless it is in quotes. The names are then found without regard to case, because a name
/// written with no quotes is not folded when a table is created here yet, and a table made as
/// `FooBar` is to be found as `foobar`.
fn serial_sequence_of(catalog: &Catalog, table: &str, column: &str) -> Result<Value> {
    let parts = folded_parts(table);
    let written = parts.join(".");
    let missing = || {
        Error::catalog(format!("Table with name {written} does not exist!"))
            .state(SqlState::UNDEFINED_TABLE)
            .pg(format!("relation \"{written}\" does not exist"))
            .unplaced()
    };
    let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
    let name = catalog.resolve(&parts).map_err(|_| missing())?;
    let held = catalog.table(&name).map_err(|_| missing())?;
    let Some(at) = held.columns().iter().position(|field| same_name(&field.name, column)) else {
        return Err(Error::binder(format!(
            "column \"{column}\" of relation \"{}\" does not exist",
            name.table
        ))
        .state(SqlState::UNDEFINED_COLUMN)
        .unplaced());
    };
    let Some(sequence) = held.default(at).and_then(nextval_of) else {
        return Ok(Value::Null);
    };
    let parts = rudb_parse::identifier_parts(&sequence);
    let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
    let Ok(sequence) = catalog.resolve_sequence(&parts) else {
        return Ok(Value::Null);
    };
    if catalog.sequence(&sequence)?.owner() != Some(&name) {
        return Ok(Value::Null);
    }
    Ok(Value::Varchar(format!(
        "{}.{}",
        quote_identifier(&sequence.schema),
        quote_identifier(&sequence.table)
    )))
}

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
    /// A call with one argument that names a type, such as `int4('5')` or `text(5)`, bound as the
    /// cast to the type, or `None` when it is a call of a function. This is the rule of
    /// `func_get_detail` of PostgreSQL. A function of `pg_proc` that takes exactly the type of
    /// the argument is called, and a function with the name of a type that gives the type is the
    /// function of the cast, so it binds as the cast. With no such function, a literal string or
    /// null is always cast, and another value is cast when the cast leaves the value as it is or
    /// goes through text, but not a row to a string type.
    pub(crate) fn function_style_cast(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        bound: &[ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        use rudb_pgtypes::{CoercionContext, CoercionPath, TypeInfo, oid};
        let ([argument], [input]) = (arguments, bound) else { return Ok(None) };
        let Some(target) = rudb_pgtypes::func_name_as_type(written) else { return Ok(None) };
        let ty = self.plan().expr_type(*input).clone();
        let spelled = rudb_pgtypes::format_type(target);
        if ty == LogicalType::Null
            || matches!(ast.expr(*argument), ast::Expr::Literal { kind: LiteralKind::String, .. })
        {
            return self.bind_cast(ast, *argument, &spelled, false, scope).map(Some);
        }
        let source = rudb_pgtypes::pg_type(&ty).oid;
        let exact = rudb_pgtypes::procs(written).iter().find(|proc| proc.args == [source]);
        let cast = match exact {
            Some(proc) => proc.result == target,
            None => {
                match rudb_pgtypes::find_coercion_pathway(source, target, CoercionContext::Explicit)
                {
                    CoercionPath::Relabel => true,
                    CoercionPath::ViaIo => {
                        let row = source == oid::RECORD
                            || TypeInfo::get(source).is_some_and(|info| info.kind == b'c');
                        let string =
                            TypeInfo::get(target).is_some_and(|info| info.category == b'S');
                        !(row && string)
                    }
                    _ => false,
                }
            }
        };
        if !cast {
            return Ok(None);
        }
        let (target, declared) = self.cast_target(&spelled)?;
        self.cast_bound(*input, &target, declared, false).map(Some)
    }

    /// A call written with named arguments or with `VARIADIC` in a PostgreSQL session, which
    /// only a function of `pg_proc` can take. The arguments by position come first and the named
    /// ones after them, in the order of the call, as `func_get_detail` reads them.
    pub(crate) fn pg_written_call(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        written: &str,
        positional: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<ExprRef> {
        let named = ast.named_args(call);
        let mut arguments = positional.to_vec();
        arguments.extend(named.iter().map(|target| target.expr));
        let names: Vec<&str> = named.iter().map(|target| ast.string(target.alias)).collect();
        let mut bound = Vec::with_capacity(arguments.len());
        for &argument in &arguments {
            bound.push(self.bind_expr(ast, argument, scope)?);
        }
        let untyped: Vec<bool> = arguments
            .iter()
            .map(|&argument| {
                matches!(ast.expr(argument), ast::Expr::Literal { kind: LiteralKind::String, .. })
            })
            .collect();
        let how = Written { names: &names, variadic: ast.variadic(call) };
        let found = self.pg_proc_call(ast, written, &arguments, &bound, &untyped, how, scope)?;
        found.ok_or_else(|| {
            let message = format!("named arguments and VARIADIC in a call of {written}");
            Error::not_implemented(format!("{message} are not supported"))
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .pg(format!("{message} are not supported"))
                .with_span(self.current_span)
        })
    }

    /// The call `written(arguments)` bound to the function of `pg_proc` that PostgreSQL finds for
    /// it, or `None` when the call takes another path here.
    ///
    /// The function is found by the rules of `func_get_detail` over the types of the arguments,
    /// with `unknown` for a string literal and a null, and over the names of the named arguments.
    /// The arguments go to the places that their names give, and an argument that the call does
    /// not give takes its default. A function in C with a kernel here is called as the kernel,
    /// with each argument cast to the declared type, and a function in SQL has its body bound in
    /// place of the call, with `$1` and the others the arguments. A call with named arguments or
    /// `VARIADIC` to a function with neither calls the function of the pin with the arguments in
    /// their places. No function, or more than one, is the error of PostgreSQL. A name that is
    /// also an aggregate or a window function, and an argument of a type that PostgreSQL does not
    /// have, take the path of the pin.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pg_proc_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        bound: &[ExprRef],
        untyped: &[bool],
        how: Written<'_>,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        use rudb_pgtypes::{Resolution, oid};
        let special = !how.names.is_empty() || how.variadic;
        let procs = rudb_pgtypes::procs(written);
        if (procs.is_empty() && !special) || procs.iter().any(|proc| proc.kind != b'f') {
            return Ok(None);
        }
        let mut types = Vec::with_capacity(bound.len());
        let mut oids = Vec::with_capacity(bound.len());
        for (at, (&argument, &untyped)) in bound.iter().zip(untyped).enumerate() {
            let ty = self.plan().expr_type(argument).clone();
            // An array of any number of dimensions has the type of its elements' array.
            let typed = match how.variadic && at + 1 == bound.len() {
                true => innermost_list(&ty),
                false => &ty,
            };
            let oid = match untyped || ty == LogicalType::Null {
                true => oid::UNKNOWN,
                false => match exact_oid(typed) {
                    Some(oid) => oid,
                    None => return Ok(None),
                },
            };
            types.push(ty);
            oids.push(oid);
        }
        let call = rudb_pgtypes::Call { args: &oids, names: how.names, variadic: how.variadic };
        let candidate = match rudb_pgtypes::resolve_call(written, call) {
            Resolution::Found(candidate) => candidate,
            Resolution::NotFound(failure) => {
                let call = spelled_call(written, &types, untyped, how.names);
                let message = format!("function {call} does not exist");
                let mut error =
                    Error::binder(message.clone()).state(SqlState::UNDEFINED_FUNCTION).pg(message);
                if let Some(detail) = failure.detail() {
                    error = error.detail(detail);
                }
                if let Some(hint) = failure.hint() {
                    error = error.hint(hint);
                }
                return Err(error.with_span(self.current_span));
            }
            Resolution::Ambiguous => {
                let call = spelled_call(written, &types, untyped, how.names);
                let message = format!("function {call} is not unique");
                return Err(Error::binder(message.clone())
                    .state(SqlState::AMBIGUOUS_FUNCTION)
                    .pg(message)
                    .detail("Could not choose a best candidate function.")
                    .hint("You might need to add explicit type casts.")
                    .with_span(self.current_span));
            }
        };
        let proc = candidate.proc;
        if candidate.variadic != 0 {
            // A kernel of a variadic `any` takes its values as text, by the output function of
            // the type of each value, as `format()` writes them.
            let fixed = bound.len() - candidate.variadic;
            if proc.variadic != oid::ANY
                || !how.names.is_empty()
                || !rudb_kernels::pgproc::has(proc.src)
                || !matches!(proc.lang, b'i' | b'c')
            {
                return Ok(None);
            }
            let Some(returns) = rudb_pgtypes::logical_type(proc.result) else { return Ok(None) };
            let mut args = Vec::with_capacity(fixed + 1);
            for (index, &oid) in candidate.args[..fixed].iter().enumerate() {
                let Some(ty) = rudb_pgtypes::logical_type(oid) else { return Ok(None) };
                args.push(self.argument_as(ast, arguments[index], bound[index], &ty)?);
            }
            let mut texts = Vec::with_capacity(candidate.variadic);
            for &value in &bound[fixed..] {
                texts.push(self.output_text(value)?);
            }
            let values = self.call("list_value", texts)?;
            args.push(values);
            return Ok(Some(self.pgproc_kernel(proc.src, &args, returns)));
        }
        // `VARIADIC` before the argument of a variadic `any` gives the values as one array.
        if how.variadic && proc.variadic == oid::ANY {
            let (Some(&last), Some(ty)) = (arguments.last(), types.last()) else {
                return Ok(None);
            };
            if !matches!(ty, LogicalType::List(_)) || untyped.last() == Some(&true) {
                return Err(Error::binder("VARIADIC argument must be an array")
                    .state(SqlState::DATATYPE_MISMATCH)
                    .pg("VARIADIC argument must be an array")
                    .with_span(ast.expr_span(last)));
            }
            return self.variadic_any_call(ast, written, arguments, bound, scope).map(Some);
        }
        let Some(returns) = rudb_pgtypes::logical_type(proc.result) else { return Ok(None) };
        // A function that returns a set is the unnest of the array of its kernel, so it binds
        // only as the argument of the unnest of a select list or of `FROM`.
        let rows = match proc.retset {
            false => None,
            true => match rudb_kernels::pgproc::rows_of(proc.src) {
                Some(_) if !self.in_unnest => return Err(self.misplaced_set_function()),
                Some(array) => Some(array),
                None => return Ok(None),
            },
        };
        let kernel = matches!(proc.lang, b'i' | b'c')
            && (rudb_kernels::pgproc::has(proc.src) || rows.is_some());
        let body = match proc.lang {
            b's' if proc.src != "see system_functions.sql" => {
                let src = proc.src;
                Some(match src.get(..7) {
                    Some(start) if start.eq_ignore_ascii_case("select ") => &src[7..],
                    _ => src,
                })
            }
            _ => None,
        };
        if !kernel && body.is_none() && !special {
            return Ok(None);
        }
        // A polymorphic argument of a body is the argument as the call gives it, which the body
        // casts as it needs.
        let mut declared = Vec::with_capacity(candidate.args.len());
        for &oid in &candidate.args {
            match rudb_pgtypes::logical_type(oid) {
                Some(ty) => declared.push(Some(ty)),
                None if body.is_some() && rudb_pgtypes::is_polymorphic(oid) => declared.push(None),
                None => return Ok(None),
            }
        }
        // Each argument in the place of its declared argument, and the defaults in the others.
        let mut placed: Vec<Option<ExprRef>> = vec![None; proc.args.len()];
        for (index, (&at, ty)) in candidate.order.iter().zip(&declared).enumerate() {
            let value = match (arguments.get(index), bound.get(index), ty) {
                (Some(&argument), Some(&input), Some(ty)) => {
                    self.argument_as(ast, argument, input, ty)?
                }
                (Some(_), Some(&input), None) => input,
                (_, _, None) => return Ok(None),
                (_, _, Some(ty)) => {
                    let default = candidate.default_of(at).ok_or_else(|| {
                        Error::internal(format!("argument {at} of {written} has no default"))
                    })?;
                    self.default_argument(default, candidate.args[index], ty)?
                }
            };
            placed[at] = Some(value);
        }
        let cast = placed
            .into_iter()
            .collect::<Option<Vec<ExprRef>>>()
            .ok_or_else(|| Error::internal(format!("a call of {written} with an empty place")))?;
        if let Some(body) = body {
            // A body that the parser or the binder here cannot take yet leaves the call to the
            // path of the pin.
            let outer = self.inlined.replace(cast);
            let inlined = self.bind_macro_body(written, body, scope);
            self.inlined = outer;
            return Ok(inlined.ok().map(|call| self.cast_to(call, &returns)));
        }
        if !kernel {
            return self.call(written, cast).map(Some);
        }
        if let Some(array) = rows {
            let list = LogicalType::List(Box::new(returns));
            return Ok(Some(self.pgproc_kernel(array, &cast, list)));
        }
        Ok(Some(self.pgproc_kernel(proc.src, &cast, returns)))
    }

    /// The call of the kernel of the C function `src` of `pg_proc`.
    fn pgproc_kernel(&mut self, src: &str, args: &[ExprRef], returns: LogicalType) -> ExprRef {
        let name = self.plan_mut().intern(&format!("{}{src}", rudb_kernels::pgproc::PREFIX));
        let args = self.plan_mut().add_expr_list(args);
        self.add_expr(Expr::Function { name, args }, returns)
    }

    /// The default `text` of an argument of the PostgreSQL type `oid`, read by the input function
    /// of the type as `proargdefaults` holds it.
    fn default_argument(
        &mut self,
        text: &str,
        oid: rudb_pgtypes::Oid,
        ty: &LogicalType,
    ) -> Result<ExprRef> {
        let session = self.session;
        let read = session.postgres().and_then(|postgres| postgres.input.as_ref());
        let value = match read.and_then(|input| input.read(oid, text)) {
            Some(value) => self.add_constant(value?),
            None => self.add_constant(Value::Varchar(text.into())),
        };
        Ok(self.cast_to(value, ty))
    }

    /// A call with `VARIADIC` before the array of a function whose variadic argument is `any`,
    /// which takes each element of the array as one of its values. A null array gives a null, as
    /// it does in PostgreSQL, and an array of more dimensions gives its elements in order.
    fn variadic_any_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        bound: &[ExprRef],
        scope: &Scope,
    ) -> Result<ExprRef> {
        let (Some(&array), Some((_, fixed))) = (bound.last(), bound.split_last()) else {
            return Err(Error::internal(format!("VARIADIC in a call of {written} with no array")));
        };
        let mut flat = "$1".to_string();
        for _ in 1..depth(self.plan().expr_type(array)) {
            flat = format!("flatten({flat})");
        }
        let named = |name: &str| same_name(written, name);
        // Each element is text by the output function of its type, as an argument of concat is.
        // That is the cast to text, but for a boolean, which the output writes as `t` or `f`.
        let element = match innermost_list(self.plan().expr_type(array)) {
            LogicalType::List(element) if **element == LogicalType::Boolean => {
                "CASE WHEN __rudb_e THEN 't' WHEN NOT __rudb_e THEN 'f' END"
            }
            _ => "CAST(__rudb_e AS VARCHAR)",
        };
        let texts = format!("list_transform({flat}, lambda __rudb_e: {element})");
        // `format()` takes the texts as they are, so the kernel sees the values of the array.
        if let [format] = fixed
            && named("format")
        {
            let outer = self.inlined.replace(vec![array]);
            let values = self.bind_macro_body(written, &texts, scope);
            self.inlined = outer;
            let format = self.cast_to(*format, &LogicalType::Varchar);
            return Ok(self.pgproc_kernel("text_format", &[format, values?], LogicalType::Varchar));
        }
        let text = match fixed {
            [] if named("concat") => format!("array_to_string({texts}, '')"),
            [_] if named("concat_ws") => {
                format!("array_to_string({texts}, CAST($2 AS VARCHAR))")
            }
            [] if named("num_nulls") || named("num_nonnulls") => {
                let nulls = format!("(len({flat}) - list_count({flat}))");
                let counted = match named("num_nulls") {
                    true => nulls,
                    false => format!("list_count({flat})"),
                };
                format!("CAST({counted} AS INTEGER)")
            }
            _ => {
                let message = format!("VARIADIC in a call of {written} is not supported");
                return Err(Error::not_implemented(message.clone())
                    .state(SqlState::FEATURE_NOT_SUPPORTED)
                    .pg(message)
                    .with_span(ast.expr_span(arguments[arguments.len() - 1])));
            }
        };
        let mut inlined = vec![array];
        inlined.extend(fixed);
        let outer = self.inlined.replace(inlined);
        let call = self.bind_macro_body(written, &text, scope);
        self.inlined = outer;
        call
    }

    /// The call `written(arguments)` bound as PostgreSQL binds it, or `None` when the name is not
    /// one of the functions that this module writes out.
    pub(crate) fn postgres_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let named = |name: &str| same_name(written, name);
        if let [document] = arguments
            && let Some(set) = JsonSet::of(written)
        {
            return self.json_set(ast, set, *document, scope).map(Some);
        }
        // A select list and `FROM` unnest the series, so a series anywhere else has no rows to give.
        if named("generate_series")
            && !self.in_unnest
            && self.semantics.set_functions() == SetFunctions::Postgres
        {
            return Err(self.misplaced_set_function());
        }
        if arguments.is_empty() && named("pg_client_encoding") {
            let setting = self.session.postgres().and_then(|pg| pg.settings.get("client_encoding"));
            let encoding =
                setting.as_deref().and_then(rudb_common::guc::encoding).unwrap_or("UTF8");
            let constant = self.add_constant(Value::Varchar(encoding.into()));
            return Ok(Some(constant));
        }
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
        // The sequence is found when the statement is bound, so the call has to give two string
        // literals, which is how the ORMs write it.
        if named("pg_get_serial_sequence")
            && let [table, column] = arguments
            && let (Some(table), Some(column)) =
                (string_literal(ast, *table), string_literal(ast, *column))
        {
            let value = serial_sequence_of(self.catalog(), table, column)?;
            let constant = self.add_constant(value);
            return Ok(Some(self.cast_to(constant, &LogicalType::Varchar)));
        }
        if let [text, type_name] = arguments
            && (named("pg_input_is_valid") || named("pg_input_error_info"))
        {
            let valid = named("pg_input_is_valid");
            return self.input_check(ast, valid, *text, *type_name, scope).map(Some);
        }
        if let [value, template] = arguments
            && let Some(call) = self.formatting_call(ast, written, *value, *template, scope)?
        {
            return Ok(Some(call));
        }
        if let Some(call) = self.position_call(ast, written, arguments, scope)? {
            return Ok(Some(call));
        }
        if let Some(call) = self.array_fill_call(ast, written, arguments, scope)? {
            return Ok(Some(call));
        }
        if let Some(call) = self.concat_call(ast, written, arguments, scope)? {
            return Ok(Some(call));
        }
        if let Some(call) = self.regexp_call(ast, written, arguments, scope)? {
            return Ok(Some(call));
        }
        if let Some(call) = self.similar_call(ast, written, arguments, scope)? {
            return Ok(Some(call));
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

    /// `pg_input_is_valid(text, type)` when `valid`, and `pg_input_error_info(text, type)`. The
    /// kernel reads the text with the input function of the type, which reads `DateStyle` and
    /// `IntervalStyle`, so they are given to it as two constants. A new value of a setting makes a
    /// new plan.
    fn input_check(
        &mut self,
        ast: &Ast,
        valid: bool,
        text: ast::ExprRef,
        type_name: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let text = self.bind_expr(ast, text, scope)?;
        let text = self.cast_to(text, &LogicalType::Varchar);
        let type_name = self.bind_expr(ast, type_name, scope)?;
        let type_name = self.cast_to(type_name, &LogicalType::Varchar);
        let setting = |name: &str, default: &str| {
            let value = self.session.postgres().and_then(|pg| pg.settings.get(name));
            Value::Varchar(value.unwrap_or_else(|| default.to_owned()))
        };
        let date_style = setting("DateStyle", "ISO, MDY");
        let interval_style = setting("IntervalStyle", "postgres");
        let date_style = self.add_constant(date_style);
        let interval_style = self.add_constant(interval_style);
        let (kernel, returns) = match valid {
            true => ("__rudb_pg_input_valid", LogicalType::Boolean),
            false => ("__rudb_pg_input_error", rudb_kernels::pginput::error_type()),
        };
        let name = self.plan_mut().intern(kernel);
        let args = self.plan_mut().add_expr_list(&[text, type_name, date_style, interval_style]);
        Ok(self.add_expr(Expr::Function { name, args }, returns))
    }

    /// A JSON set function, such as `json_each`, as the list of its rows. The unnest around the
    /// call gives the rows one at a time.
    fn json_set(
        &mut self,
        ast: &Ast,
        set: JsonSet,
        document: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let bound = self.bind_expr(ast, document, scope)?;
        let ty = self.plan().expr_type(bound).clone();
        let unknown = string_literal(ast, document).is_some();
        if !unknown && ty != LogicalType::Null && ty != set.document() {
            return Err(no_such_function(set.name(), &[ty], &[false]));
        }
        if !self.in_unnest {
            return Err(self.misplaced_set_function());
        }
        let document = self.cast_to(bound, &set.document());
        let called = self.add_constant(Value::Varchar(set.name().to_owned()));
        let name = self.plan_mut().intern(rudb_kernels::pgjson::KERNEL);
        let args = self.plan_mut().add_expr_list(&[document, called]);
        Ok(self.add_expr(Expr::Function { name, args }, LogicalType::List(Box::new(set.element()))))
    }

    /// `text ~ pattern` of a PostgreSQL session, and `text ~* pattern` when `insensitive`.
    pub(crate) fn pg_regex_match(
        &mut self,
        text: ExprRef,
        pattern: ExprRef,
        insensitive: bool,
    ) -> ExprRef {
        let letters = if insensitive { "i" } else { "" };
        let letters = self.add_constant(Value::Varchar(letters.to_owned()));
        self.regexp_kernel(Function::Match, vec![text, pattern, letters])
    }

    /// A regular expression function of a PostgreSQL session, such as `regexp_match` or
    /// `regexp_instr`, or `None` for any other call. The kernel takes every parameter, so the call
    /// gets the default of each one it leaves out. `regexp_matches` gives the list of its rows,
    /// which the unnest around the call gives one at a time.
    fn regexp_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let Some(named) = Function::named(written) else { return Ok(None) };
        let least = named.parameters().iter().filter(|parameter| parameter.is_required()).count();
        let most = match named {
            Function::Replace => Function::ReplaceAt.parameters().len(),
            // The third argument is the escape of a `SIMILAR` pattern.
            Function::Substring => 3,
            _ => named.parameters().len(),
        };
        if self.semantics.regex_rules() != RegexRules::Postgres
            || !(least..=most).contains(&arguments.len())
        {
            return Ok(None);
        }
        let mut bound = Vec::with_capacity(most);
        for &argument in arguments {
            bound.push(self.bind_expr(ast, argument, scope)?);
        }
        let unknown: Vec<bool> =
            arguments.iter().map(|&argument| string_literal(ast, argument).is_some()).collect();
        let types: Vec<LogicalType> =
            bound.iter().map(|&argument| self.plan().expr_type(argument).clone()).collect();
        let integer = |at: usize| {
            !unknown[at] && matches!(types[at], LogicalType::Integer | LogicalType::SmallInt)
        };
        let text = |at: usize| {
            unknown[at] || matches!(types[at], LogicalType::Varchar | LogicalType::Null)
        };
        let function = match named {
            // A string or a null as the fourth argument is the flags, and an integer is the start.
            Function::Replace if arguments.len() > 4 || (arguments.len() == 4 && integer(3)) => {
                Function::ReplaceAt
            }
            other => other,
        };
        // `substring(text similar pattern escape escape)`, which is `substring` from what
        // `similar_to_escape` makes of the pattern.
        if function == Function::Substring && arguments.len() == 3 {
            if !text(0) {
                return Err(no_such_function("substring", &types, &unknown));
            }
            let pattern = self.similar_escape(bound[1], bound[2]);
            return Ok(Some(self.regexp_kernel(function, vec![bound[0], pattern])));
        }
        let parameters = function.parameters();
        let fits = |(at, ty): (usize, &LogicalType)| match parameters[at].is_text() {
            true => matches!(ty, LogicalType::Varchar | LogicalType::Null),
            false => {
                unknown[at]
                    || matches!(
                        ty,
                        LogicalType::Integer | LogicalType::SmallInt | LogicalType::Null
                    )
            }
        };
        if !types.iter().enumerate().all(fits) {
            return Err(no_such_function(function.name(), &types, &unknown));
        }
        if function.is_set() && !self.in_unnest {
            return Err(self.misplaced_set_function());
        }
        for (at, parameter) in parameters.iter().enumerate().take(bound.len()) {
            if unknown[at] && !parameter.is_text() {
                bound[at] =
                    self.argument_as(ast, arguments[at], bound[at], &LogicalType::Integer)?;
            }
        }
        for parameter in &parameters[bound.len()..] {
            bound.push(self.add_constant(parameter.default_value()));
        }
        Ok(Some(self.regexp_kernel(function, bound)))
    }

    /// `substring(value from start [for length])` and `substr` of a PostgreSQL session, which keep
    /// a part of a `text` or a `bytea` value from a position, or `None` for any other call. A
    /// `substring` whose arguments after the first are all strings or nulls is the form with a
    /// regular expression, which [`Self::regexp_call`] binds. A string literal as the start or the
    /// length is an integer, and a `bigint` is not one.
    fn position_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let substring = same_name(written, "substring");
        if !(substring || same_name(written, "substr")) || !(2..=3).contains(&arguments.len()) {
            return Ok(None);
        }
        let unknown: Vec<bool> =
            arguments.iter().map(|&argument| string_literal(ast, argument).is_some()).collect();
        let mut bound = Vec::with_capacity(arguments.len());
        for &argument in arguments {
            bound.push(self.bind_expr(ast, argument, scope)?);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&argument| self.plan().expr_type(argument).clone()).collect();
        let text = |at: usize| {
            unknown[at] || matches!(types[at], LogicalType::Varchar | LogicalType::Null)
        };
        if substring && (1..arguments.len()).all(text) {
            return Ok(None);
        }
        let integer = |at: usize| {
            unknown[at]
                || matches!(
                    types[at],
                    LogicalType::Integer | LogicalType::SmallInt | LogicalType::Null
                )
        };
        let returns = match &types[0] {
            _ if unknown[0] => LogicalType::Varchar,
            LogicalType::Varchar | LogicalType::Null => LogicalType::Varchar,
            LogicalType::Blob => LogicalType::Blob,
            _ => return Err(no_such_function(written, &types, &unknown)),
        };
        if !(1..arguments.len()).all(integer) {
            return Err(no_such_function(written, &types, &unknown));
        }
        let mut cast = Vec::with_capacity(bound.len());
        for (at, argument) in bound.into_iter().enumerate() {
            let ty = if at == 0 { returns.clone() } else { LogicalType::Integer };
            cast.push(self.argument_as(ast, arguments[at], argument, &ty)?);
        }
        let name = self.plan_mut().intern(rudb_kernels::PG_SUBSTR);
        let args = self.plan_mut().add_expr_list(&cast);
        Ok(Some(self.add_expr(Expr::Function { name, args }, returns)))
    }

    /// `concat` or `concat_ws` with its values written by the output functions of their types, or
    /// `None` for any other call.
    fn concat_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let name = match () {
            () if same_name(written, "concat") => "concat",
            () if same_name(written, "concat_ws") => "concat_ws",
            () => return Ok(None),
        };
        if arguments.is_empty() {
            return Ok(None);
        }
        let mut texts = Vec::with_capacity(arguments.len());
        for &argument in arguments {
            let bound = self.bind_expr(ast, argument, scope)?;
            texts.push(self.output_text(bound)?);
        }
        self.call(name, texts).map(Some)
    }

    /// `array_fill(value, dimensions [, lower_bounds])` of a PostgreSQL session, or `None` for any
    /// other call. The result is an array of the type of the value, so a value of no type and a
    /// value that is an array are errors. The dimensions and the lower bounds are `int4[]`, and a
    /// string literal for one of them is read as an `int4[]`.
    fn array_fill_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        if !same_name(written, "array_fill") || !(2..=3).contains(&arguments.len()) {
            return Ok(None);
        }
        let unknown: Vec<bool> =
            arguments.iter().map(|&argument| string_literal(ast, argument).is_some()).collect();
        let mut bound = Vec::with_capacity(arguments.len());
        for &argument in arguments {
            bound.push(self.bind_expr(ast, argument, scope)?);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&argument| self.plan().expr_type(argument).clone()).collect();
        let integers = |at: usize| match &types[at] {
            LogicalType::List(element) => {
                matches!(**element, LogicalType::Integer | LogicalType::SmallInt)
            }
            ty => unknown[at] || *ty == LogicalType::Null,
        };
        if !(1..arguments.len()).all(integers) {
            return Err(no_such_function(written, &types, &unknown));
        }
        let element = types[0].clone();
        if unknown[0] || element == LogicalType::Null {
            let message = "could not determine polymorphic type because input has type unknown";
            return Err(Error::binder(message)
                .state(SqlState::DATATYPE_MISMATCH)
                .pg(message)
                .unplaced());
        }
        if matches!(element, LogicalType::List(_) | LogicalType::Array(..)) {
            let name = rudb_pgtypes::format_type(rudb_pgtypes::pg_type(&element).oid);
            let message = format!("could not find array type for data type {name}");
            return Err(Error::binder(message.clone())
                .state(SqlState::UNDEFINED_OBJECT)
                .pg(message)
                .unplaced());
        }
        let dimensions = LogicalType::List(Box::new(LogicalType::Integer));
        let mut cast = vec![bound[0]];
        for at in 1..bound.len() {
            cast.push(self.argument_as(ast, arguments[at], bound[at], &dimensions)?);
        }
        let name = self.plan_mut().intern(rudb_kernels::pgarray::ARRAY_FILL);
        let args = self.plan_mut().add_expr_list(&cast);
        let returns = LogicalType::List(Box::new(element));
        Ok(Some(self.add_expr(Expr::Function { name, args }, returns)))
    }

    /// The argument `bound`, written as `written`, as a `ty`. A string literal is read by the input
    /// function of the type, so `substr('abc', 'x')` is the error of the input of `integer`.
    fn argument_as(
        &mut self,
        ast: &Ast,
        written: ast::ExprRef,
        bound: ExprRef,
        ty: &LogicalType,
    ) -> Result<ExprRef> {
        let read = match self.read_literal(ast, written, rudb_pgtypes::pg_type(ty).oid) {
            Some(value) => value?,
            None => bound,
        };
        Ok(self.cast_to(read, ty))
    }

    /// `similar_to_escape(pattern [, escape])` of a PostgreSQL session, which the transform also
    /// writes for `SIMILAR TO`. The escape is a backslash where the call gives none.
    fn similar_call(
        &mut self,
        ast: &Ast,
        written: &str,
        arguments: &[ast::ExprRef],
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        if self.semantics.regex_rules() != RegexRules::Postgres
            || !same_name(written, "similar_to_escape")
            || !(1..=2).contains(&arguments.len())
        {
            return Ok(None);
        }
        let mut bound = Vec::with_capacity(2);
        for &argument in arguments {
            bound.push(self.bind_expr(ast, argument, scope)?);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&argument| self.plan().expr_type(argument).clone()).collect();
        if !types.iter().all(|ty| matches!(ty, LogicalType::Varchar | LogicalType::Null)) {
            let unknown: Vec<bool> =
                arguments.iter().map(|&argument| string_literal(ast, argument).is_some()).collect();
            return Err(no_such_function("similar_to_escape", &types, &unknown));
        }
        if bound.len() == 1 {
            bound.push(self.add_constant(Value::Varchar("\\".to_owned())));
        }
        Ok(Some(self.similar_escape(bound[0], bound[1])))
    }

    /// The call of the kernel of `similar_to_escape`.
    fn similar_escape(&mut self, pattern: ExprRef, escape: ExprRef) -> ExprRef {
        let arguments =
            [pattern, escape].map(|argument| self.cast_to(argument, &LogicalType::Varchar));
        let name = self.plan_mut().intern(pgregexp::SIMILAR_ESCAPE);
        let args = self.plan_mut().add_expr_list(&arguments);
        self.add_expr(Expr::Function { name, args }, LogicalType::Varchar)
    }

    /// The call of one of the kernels of `pgregexp`, with an argument for each parameter.
    fn regexp_kernel(&mut self, function: Function, arguments: Vec<ExprRef>) -> ExprRef {
        let parameters = function.parameters();
        let arguments: Vec<ExprRef> = arguments
            .into_iter()
            .zip(parameters)
            .map(|(argument, parameter)| {
                let ty =
                    if parameter.is_text() { LogicalType::Varchar } else { LogicalType::Integer };
                self.cast_to(argument, &ty)
            })
            .collect();
        let name = self.plan_mut().intern(function.kernel());
        let args = self.plan_mut().add_expr_list(&arguments);
        self.add_expr(Expr::Function { name, args }, function.returns())
    }

    /// The error of PostgreSQL for a set-returning function in a place that cannot give rows.
    fn misplaced_set_function(&self) -> Error {
        if self.in_aggregate {
            let message = "aggregate function calls cannot contain set-returning function calls";
            return Error::binder(message).state(SqlState::FEATURE_NOT_SUPPORTED).hint(
                "You might be able to move the set-returning function into a LATERAL FROM item.",
            );
        }
        let place = match self.clause {
            "WHERE clause" => "WHERE",
            "HAVING clause" => "HAVING",
            "LIMIT clause" => "LIMIT",
            "JOIN condition" => "JOIN conditions",
            other => other,
        };
        Error::binder(format!("set-returning functions are not allowed in {place}"))
            .state(SqlState::FEATURE_NOT_SUPPORTED)
    }

    /// `to_char` of a date and a time, `to_timestamp(text, text)` and `to_date(text, text)`, as
    /// the kernels that port `formatting.c`. `None` for any other call.
    ///
    /// PostgreSQL has `to_char` over `timestamp`, `timestamptz` and `interval`. A `date` reaches the
    /// `timestamptz` form by its implicit cast, and a `time` reaches the `interval` form, which the
    /// kernel takes as it is.
    fn formatting_call(
        &mut self,
        ast: &Ast,
        written: &str,
        value: ast::ExprRef,
        template: ast::ExprRef,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let kernel = match () {
            () if same_name(written, "to_char") => "__rudb_pg_to_char",
            () if same_name(written, "to_number") => "__rudb_pg_to_number",
            () if same_name(written, "to_timestamp") => "__rudb_pg_to_timestamp",
            () if same_name(written, "to_date") => "__rudb_pg_to_date",
            () => return Ok(None),
        };
        let value = self.bind_expr(ast, value, scope)?;
        let template = self.bind_expr(ast, template, scope)?;
        let text = |ty: &LogicalType| matches!(ty, LogicalType::Varchar | LogicalType::Null);
        if !text(self.plan().expr_type(template)) {
            return Ok(None);
        }
        let given = self.plan().expr_type(value).clone();
        let (value, returns) = match (kernel, &given) {
            (
                "__rudb_pg_to_char",
                LogicalType::Timestamp
                | LogicalType::TimestampTz
                | LogicalType::Interval
                | LogicalType::Time,
            ) => (value, LogicalType::Varchar),
            ("__rudb_pg_to_char", LogicalType::Date) => {
                (self.cast_to(value, &LogicalType::TimestampTz), LogicalType::Varchar)
            }
            (
                "__rudb_pg_to_char",
                LogicalType::Integer
                | LogicalType::BigInt
                | LogicalType::Numeric
                | LogicalType::Decimal { .. }
                | LogicalType::Float
                | LogicalType::Double,
            ) => (value, LogicalType::Varchar),
            // PostgreSQL has no `to_char(int2, text)` and resolves the call to `float8`.
            ("__rudb_pg_to_char", LogicalType::SmallInt) => {
                (self.cast_to(value, &LogicalType::Double), LogicalType::Varchar)
            }
            ("__rudb_pg_to_number", ty) if text(ty) => {
                (self.cast_to(value, &LogicalType::Varchar), LogicalType::Numeric)
            }
            ("__rudb_pg_to_timestamp", ty) if text(ty) => {
                (self.cast_to(value, &LogicalType::Varchar), LogicalType::TimestampTz)
            }
            ("__rudb_pg_to_date", ty) if text(ty) => {
                (self.cast_to(value, &LogicalType::Varchar), LogicalType::Date)
            }
            _ => return Ok(None),
        };
        let template = self.cast_to(template, &LogicalType::Varchar);
        let name = self.plan_mut().intern(kernel);
        let args = self.plan_mut().add_expr_list(&[value, template]);
        Ok(Some(self.add_expr(Expr::Function { name, args }, returns)))
    }

    /// The number of dimensions of the array that `argument` is, from its type.
    fn array_depth(&mut self, ast: &Ast, argument: ast::ExprRef, scope: &Scope) -> Result<usize> {
        let bound = self.bind_expr(ast, argument, scope)?;
        Ok(depth(self.plan().expr_type(bound)).max(1))
    }

    /// The call `written` resolved to `call`, cast to the type that PostgreSQL gives the result.
    pub(crate) fn postgres_result(&mut self, written: &str, call: ExprRef) -> ExprRef {
        let named = |names: &[&str]| names.iter().any(|name| same_name(written, name));
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

    /// Whether `function(arguments)` is the series over `int4` of PostgreSQL, which gives an
    /// `int4` column where the series here gives a BIGINT. A parameter of no type is typed as an
    /// `int4` here, as PostgreSQL types it.
    pub(crate) fn postgres_series(&mut self, function: &str, arguments: &mut [ExprRef]) -> bool {
        if self.semantics.function_rules() == FunctionRules::Pin
            || !same_name(function, "generate_series")
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

    /// A projection over the series `node` that casts its BIGINT column to an `int4`. The value is
    /// the first column, and the number of `WITH ORDINALITY` after it stays a BIGINT.
    pub(crate) fn integer_series(&mut self, node: NodeRef, mut scope: Scope) -> (NodeRef, Scope) {
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.columns.len());
        let mut names = Vec::with_capacity(scope.columns.len());
        for (at, column) in scope.columns.iter().enumerate() {
            let read = self.plan_mut().add_expr(Expr::Column(column.binding), column.ty.clone());
            exprs.push(match column.ty {
                LogicalType::BigInt if at == 0 => self.cast_to(read, &LogicalType::Integer),
                _ => read,
            });
            names.push(self.plan_mut().intern(&column.name));
        }
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
            if at == 0 && column.ty == LogicalType::BigInt {
                column.ty = LogicalType::Integer;
            }
        }
        let exprs = self.plan_mut().add_expr_list(&exprs);
        let names = self.plan_mut().add_name_list(&names);
        (self.add_node(Node::Project { input: node, index, exprs, names }), scope)
    }
}
