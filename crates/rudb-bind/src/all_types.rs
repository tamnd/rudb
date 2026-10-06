//! `test_all_types()`, the table DuckDB's own tests read when they want one column of every type
//! and, in each, the smallest value, the largest and a null.
//!
//! The pin builds its three rows out of `Value`s in C++. This builds them out of SQL, one `SELECT`
//! per row stacked with `UNION ALL`, so that every value goes through the same cast a user's
//! literal would and nothing here has to know how a type is laid out. The text is the pin's values
//! written the way the pin prints them, which is also what makes the table easy to check against
//! it: a value here that read back differently would be a cast that disagrees with the pin, and
//! that is worth knowing on its own.
//!
//! One of the pin's fifty nine columns is not here, `geometry`, because rudb has no such type yet.
//! It is the second to last, so every column but `tuple` keeps its place.
//!
//! The three enum columns are the exception to the text. Their types are hundreds of labels, or
//! seventy thousand with `use_large_enum := true`, and parsing a type that long costs more than the
//! whole rest of the table, so the text gives them as `VARCHAR` and a projection over it casts them
//! to types built here. The text itself is the same for every call, so it is parsed once per
//! process. `use_large_bignum := true` is turned away. The
//! pin makes `bignum` the largest a `BIGNUM` can hold, which is millions of digits, and that is a
//! value rudb's `BIGNUM` is not built to hold yet.

use std::sync::{Arc, Mutex};

use rudb_common::{Error, IdentifierCase, LogicalType, Result, Value};
use rudb_parse::ast;
use rudb_parse::{Ast, NONE, parse_ast_with_case};
use rudb_plan::{ColumnBinding, Expr, Node, NodeRef};

use crate::binder::Binder;
use crate::scope::{Scope, Visible};

/// The text of `test_all_types()` parsed, once for each way of folding identifiers it was asked for.
static PARSED: Mutex<Vec<(IdentifierCase, Arc<Ast>)>> = Mutex::new(Vec::new());

/// The duck the pin writes into its `VARCHAR` columns.
const DUCKS: &str = "'🦆🦆🦆🦆🦆🦆'";

/// The pin's `int_array` maximum, which a few of the other columns are built out of.
const INTS: &str = "[42, 999, NULL, NULL, -42]";

/// The pin's `varchar_array` maximum.
const VARCHARS: &str = "['🦆🦆🦆🦆🦆🦆', 'goose', NULL, '']";

/// The pin's `fixed_int_array` minimum and maximum.
const FIXED_INTS: (&str, &str) = ("[NULL, 2, 3]", "[4, 5, 6]");

/// The pin's `fixed_varchar_array` minimum and maximum.
const FIXED_VARCHARS: (&str, &str) = ("['a', NULL, 'c']", "['d', 'e', 'f']");

/// The pin's `struct` minimum and maximum.
const STRUCTS: (&str, &str) = ("{'a': NULL, 'b': NULL}", "{'a': 42, 'b': '🦆🦆🦆🦆🦆🦆'}");

impl Binder<'_> {
    /// The rows of a call to `test_all_types`, or `None` if the call is to something else.
    pub(crate) fn all_types(
        &mut self,
        ast: &Ast,
        called: &str,
        args: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<Option<(NodeRef, Scope)>> {
        if !called.eq_ignore_ascii_case("test_all_types") {
            return Ok(None);
        }
        let written = ast.target_list(args).to_vec();
        let empty = Scope::empty();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let mut large_enum = false;
        let mut failed = None;
        for argument in &written {
            match self.flag(ast, argument, &empty) {
                Ok((true, flag)) => large_enum = flag,
                Ok((false, true)) => {
                    failed = Some(Error::not_implemented(
                        "test_all_types with use_large_bignum, because a BIGNUM that large does \
                         not fit yet",
                    ));
                    break;
                }
                Ok((false, false)) => {}
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        self.clause = previous;
        if let Some(error) = failed {
            return Err(error);
        }
        let parsed = parsed(self.semantics.identifier_case())?;
        let [ast::Statement::Query(query)] = parsed.statements[..] else {
            return Err(Error::internal("the text of test_all_types is not one query"));
        };
        let outer = self.pinned_span.replace(self.current_span);
        let bound = self.bind_query(&parsed, query);
        self.pinned_span = outer;
        let (input, below) = bound?;
        let label = if alias == NONE { "test_all_types" } else { ast.string(alias) };
        let enums = enum_types(large_enum);
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(below.len());
        let mut names = Vec::with_capacity(below.len());
        let mut scope = Scope::empty();
        for (at, column) in below.columns.iter().enumerate() {
            let read = self.add_expr(Expr::Column(column.binding), column.ty.clone());
            let ty = enums
                .iter()
                .find(|(name, _)| *name == column.name)
                .map_or_else(|| column.ty.clone(), |(_, ty)| ty.clone());
            exprs.push(self.cast_to(read, &ty));
            names.push(self.plan_mut().intern(&column.name));
            scope.push(Visible {
                table: label.to_string(),
                binding: ColumnBinding::new(index, at as u32),
                ty,
                ..column.clone()
            });
        }
        let exprs = self.plan_mut().add_expr_list(&exprs);
        let names = self.plan_mut().add_name_list(&names);
        let node = self.add_node(Node::Project { input, index, exprs, names });
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, label)?;
        }
        Ok(Some((node, scope)))
    }

    /// Which of the two named parameters an argument sets, `true` for `use_large_enum`, and what it
    /// sets it to, or the pin's refusal of it.
    fn flag(&mut self, ast: &Ast, argument: &ast::Target, empty: &Scope) -> Result<(bool, bool)> {
        if argument.alias == NONE {
            let bound = self.bind_expr(ast, argument.expr, empty)?;
            let ty = self.plan().expr_type(bound).clone();
            return Err(Error::binder(format!(
                "No function matches the given name and argument types 'test_all_types({ty})'. \
                 You might need to add explicit type casts.\n\tCandidate functions:\n\t\
                 \"test_all_types\"(use_large_bignum : BOOLEAN, use_large_enum : BOOLEAN)\n"
            )));
        }
        let name = ast.string(argument.alias);
        let large_enum = if name.eq_ignore_ascii_case("use_large_enum") {
            true
        } else if name.eq_ignore_ascii_case("use_large_bignum") {
            false
        } else {
            return Err(Error::binder(format!(
                "Invalid named parameter \"{name}\" for function test_all_types\nCandidates:\n    \
                 use_large_bignum BOOLEAN\n    use_large_enum BOOLEAN\n"
            )));
        };
        let bound = self.bind_expr(ast, argument.expr, empty)?;
        let bound = self.cast_to(bound, &LogicalType::Boolean);
        let Some(value) = crate::fold::value_of(self.plan(), bound)? else {
            return Err(Error::binder("Table function cannot contain subqueries"));
        };
        let spelled = if large_enum { "use_large_enum" } else { "use_large_bignum" };
        match value {
            Value::Boolean(flag) => Ok((large_enum, flag)),
            _ => Err(Error::invalid_input(format!("Cannot use NULL as argument for {spelled}"))),
        }
    }
}

/// The text of `test_all_types()` parsed with `case`, from the cache when it has been before.
fn parsed(case: IdentifierCase) -> Result<Arc<Ast>> {
    let mut cache = PARSED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, parsed)) = cache.iter().find(|(folded, _)| *folded == case) {
        return Ok(Arc::clone(parsed));
    }
    let parsed = Arc::new(parse_ast_with_case(&all_types_text(), case)?);
    cache.push((case, Arc::clone(&parsed)));
    Ok(parsed)
}

/// The three enum columns and their types, with the long `large_enum` when `large_enum` is set.
fn enum_types(large_enum: bool) -> [(&'static str, LogicalType); 3] {
    let labels = |count: usize| (0..count).map(|label| format!("enum_{label}")).collect::<Vec<_>>();
    let large = if large_enum {
        labels(70_000)
    } else {
        vec!["enum_0".to_string(), "enum_69999".to_string()]
    };
    let small = vec!["DUCK_DUCK_ENUM".to_string(), "GOOSE".to_string()];
    [
        ("small_enum", LogicalType::Enum(small.into())),
        ("medium_enum", LogicalType::Enum(labels(300).into())),
        ("large_enum", LogicalType::Enum(large.into())),
    ]
}

/// The query under `test_all_types()`, with the enum columns as `VARCHAR`.
fn all_types_text() -> String {
    let columns = all_types_columns();
    let rows: Vec<String> = (0..3)
        .map(|row| {
            let values: Vec<String> = columns
                .iter()
                .map(|(name, ty, least, most)| {
                    // The null row takes its types from the two above it. An empty struct has no
                    // type that can be written, so its column is the value as it is, and so is an
                    // enum's, which the projection over this casts.
                    if row == 2 {
                        return format!("NULL AS \"{name}\"");
                    }
                    let value = [least, most][row];
                    if ty.is_empty() {
                        return format!("{value} AS \"{name}\"");
                    }
                    format!("CAST({value} AS {ty}) AS \"{name}\"")
                })
                .collect();
            format!("SELECT {}", values.join(", "))
        })
        .collect();
    rows.join(" UNION ALL ")
}

/// Every column of `test_all_types()`, in the pin's order: the name, the type, the least value and
/// the greatest, each written as SQL. The type is empty for the columns whose type is not written.
pub(crate) fn all_types_columns() -> Vec<(&'static str, &'static str, String, String)> {
    let int_lists = (
        format!("[[], {INTS}, NULL, [], {INTS}]"),
        format!("[[], {INTS}, []]"),
        format!("[{INTS}, [], {INTS}]"),
    );
    let (null_ints, ints) = FIXED_INTS;
    let (null_varchars, varchars) = FIXED_VARCHARS;
    let (null_struct, full_struct) = STRUCTS;
    // The largest `BIGNUM` without `use_large_bignum` is the largest `DOUBLE` written out as the
    // integer it is, which is what Rust prints for it with no places after the point.
    let bignum = format!("{:.0}", f64::MAX);
    let min_stamp = "'290309-12-22 (BC) 00:00:00'";
    vec![
        ("bool", "BOOLEAN", "false".into(), "true".into()),
        ("tinyint", "TINYINT", "-128".into(), "127".into()),
        ("smallint", "SMALLINT", "-32768".into(), "32767".into()),
        ("int", "INTEGER", "-2147483648".into(), "2147483647".into()),
        ("bigint", "BIGINT", "-9223372036854775808".into(), "9223372036854775807".into()),
        (
            "hugeint",
            "HUGEINT",
            "'-170141183460469231731687303715884105728'".into(),
            "'170141183460469231731687303715884105727'".into(),
        ),
        ("uhugeint", "UHUGEINT", "0".into(), "'340282366920938463463374607431768211455'".into()),
        ("utinyint", "UTINYINT", "0".into(), "255".into()),
        ("usmallint", "USMALLINT", "0".into(), "65535".into()),
        ("uint", "UINTEGER", "0".into(), "4294967295".into()),
        ("ubigint", "UBIGINT", "0".into(), "'18446744073709551615'".into()),
        ("bignum", "BIGNUM", format!("'-{bignum}'"), format!("'{bignum}'")),
        ("date", "DATE", "'5877642-06-25 (BC)'".into(), "'5881580-07-10'".into()),
        ("time", "TIME", "'00:00:00'".into(), "'24:00:00'".into()),
        ("timestamp", "TIMESTAMP", min_stamp.into(), "'294247-01-10 04:00:54.775806'".into()),
        ("timestamp_s", "TIMESTAMP_S", min_stamp.into(), "'294247-01-10 04:00:54'".into()),
        ("timestamp_ms", "TIMESTAMP_MS", min_stamp.into(), "'294247-01-10 04:00:54.775'".into()),
        (
            "timestamp_ns",
            "TIMESTAMP_NS",
            "'1677-09-22 00:00:00'".into(),
            "'2262-04-11 23:47:16.854775806'".into(),
        ),
        ("time_tz", "TIMETZ", "'00:00:00+15:59:59'".into(), "'24:00:00-15:59:59'".into()),
        (
            "timestamp_tz",
            "TIMESTAMPTZ",
            "'290309-12-22 (BC) 00:00:00+00'".into(),
            "'294247-01-10 04:00:54.775806+00'".into(),
        ),
        (
            "timestamp_tz_ns",
            "TIMESTAMPTZ_NS",
            "'1677-09-22 00:00:00+00'".into(),
            "'2262-04-11 23:47:16.854775806+00'".into(),
        ),
        ("float", "FLOAT", "'-3.4028235e+38'".into(), "'3.4028235e+38'".into()),
        (
            "double",
            "DOUBLE",
            "'-1.7976931348623157e+308'".into(),
            "'1.7976931348623157e+308'".into(),
        ),
        ("dec_4_1", "DECIMAL(4,1)", "'-999.9'".into(), "'999.9'".into()),
        ("dec_9_4", "DECIMAL(9,4)", "'-99999.9999'".into(), "'99999.9999'".into()),
        (
            "dec_18_6",
            "DECIMAL(18,6)",
            "'-999999999999.999999'".into(),
            "'999999999999.999999'".into(),
        ),
        (
            "dec38_10",
            "DECIMAL(38,10)",
            "'-9999999999999999999999999999.9999999999'".into(),
            "'9999999999999999999999999999.9999999999'".into(),
        ),
        (
            "uuid",
            "UUID",
            "'00000000-0000-0000-0000-000000000000'".into(),
            "'ffffffff-ffff-ffff-ffff-ffffffffffff'".into(),
        ),
        (
            "interval",
            "INTERVAL",
            "'00:00:00'".into(),
            "to_months(999) + to_days(999) + to_microseconds(999999999)".into(),
        ),
        ("varchar", "VARCHAR", DUCKS.into(), "'goo' || chr(0) || 'se'".into()),
        ("blob", "BLOB", "'thisisalongblob\\x00withnullbytes'".into(), "'\\x00\\x00\\x00a'".into()),
        ("bit", "BIT", "'0010001001011100010101011010111'".into(), "'10101'".into()),
        ("small_enum", "", "'DUCK_DUCK_ENUM'".into(), "'GOOSE'".into()),
        ("medium_enum", "", "'enum_0'".into(), "'enum_299'".into()),
        ("large_enum", "", "'enum_0'".into(), "'enum_69999'".into()),
        ("int_array", "INTEGER[]", "[]".into(), INTS.into()),
        (
            "double_array",
            "DOUBLE[]",
            "[]".into(),
            "[42.0, 'nan'::DOUBLE, 'inf'::DOUBLE, '-inf'::DOUBLE, NULL, -42.0]".into(),
        ),
        (
            "date_array",
            "DATE[]",
            "[]".into(),
            "['1970-01-01', 'infinity', '-infinity', NULL, '2022-05-12']".into(),
        ),
        (
            "timestamp_array",
            "TIMESTAMP[]",
            "[]".into(),
            "['1970-01-01 00:00:00', 'infinity', '-infinity', NULL, '2022-05-12 16:23:45']".into(),
        ),
        (
            "timestamptz_array",
            "TIMESTAMPTZ[]",
            "[]".into(),
            "['1970-01-01 00:00:00+00', 'infinity', '-infinity', NULL, '2022-05-12 16:23:45-07']"
                .into(),
        ),
        ("varchar_array", "VARCHAR[]", "[]".into(), VARCHARS.into()),
        ("nested_int_array", "INTEGER[][]", "[]".into(), int_lists.0),
        ("struct", "STRUCT(a INTEGER, b VARCHAR)", null_struct.into(), full_struct.into()),
        ("empty_struct", "", "{}".into(), "{}".into()),
        (
            "struct_of_arrays",
            "STRUCT(a INTEGER[], b VARCHAR[])",
            null_struct.into(),
            format!("{{'a': {INTS}, 'b': {VARCHARS}}}"),
        ),
        (
            "array_of_structs",
            "STRUCT(a INTEGER, b VARCHAR)[]",
            "[]".into(),
            format!("[{null_struct}, {full_struct}, NULL]"),
        ),
        (
            "map",
            "MAP(VARCHAR, VARCHAR)",
            "MAP {}".into(),
            format!("MAP {{'key1': {DUCKS}, 'key2': 'goose'}}"),
        ),
        (
            "union",
            "UNION(name VARCHAR, age SMALLINT)",
            "union_value(name := 'Frank')".into(),
            "union_value(age := 5::SMALLINT)".into(),
        ),
        ("fixed_int_array", "INTEGER[3]", null_ints.into(), ints.into()),
        ("fixed_varchar_array", "VARCHAR[3]", null_varchars.into(), varchars.into()),
        (
            "fixed_nested_int_array",
            "INTEGER[3][3]",
            format!("[{null_ints}, NULL, {null_ints}]"),
            format!("[{ints}, {null_ints}, {ints}]"),
        ),
        (
            "fixed_nested_varchar_array",
            "VARCHAR[3][3]",
            format!("[{null_varchars}, NULL, {null_varchars}]"),
            format!("[{varchars}, {null_varchars}, {varchars}]"),
        ),
        (
            "fixed_struct_array",
            "STRUCT(a INTEGER, b VARCHAR)[3]",
            format!("[{null_struct}, {full_struct}, {null_struct}]"),
            format!("[{full_struct}, {null_struct}, {full_struct}]"),
        ),
        (
            "struct_of_fixed_array",
            "STRUCT(a INTEGER[3], b VARCHAR[3])",
            format!("{{'a': {null_ints}, 'b': {null_varchars}}}"),
            format!("{{'a': {ints}, 'b': {varchars}}}"),
        ),
        ("fixed_array_of_int_list", "INTEGER[][3]", int_lists.1, int_lists.2),
        (
            "list_of_fixed_int_array",
            "INTEGER[3][]",
            format!("[{null_ints}, {ints}, {null_ints}]"),
            format!("[{ints}, {null_ints}, {ints}]"),
        ),
        ("time_ns", "TIME_NS", "'00:00:00'".into(), "'24:00:00'".into()),
        ("tuple", "TUPLE(INTEGER, VARCHAR)", "(NULL, NULL)".into(), format!("(42, {DUCKS})")),
    ]
}
