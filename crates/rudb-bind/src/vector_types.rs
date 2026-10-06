//! `test_vector_types()`, the table DuckDB's tests read when they want a column of one type in every
//! way the pin can lay a vector out: flat, constant, a dictionary over a flat one and a sequence.
//!
//! Only the types of the arguments matter, never their values, and each one becomes a column named
//! `test_vector`, `test_vector2` and so on. The values are the ones `test_all_types()` has for that
//! type, its least, its greatest and a null, and a list or a struct or a map is built out of the
//! values of what is inside it. A type with no column in `test_all_types()` is refused, and so is a
//! `DECIMAL` or an `ENUM` other than the one that table has, because the pin finds a value by type
//! and then insists on the exact type.
//!
//! rudb does not keep the layouts apart, so what is left of them is the rows each one gives. The
//! flat part is the three values, the constant part is the first value three times, the dictionary
//! part is the second and the third again, and the sequence part is the three values once more
//! except that an integer counts 3, 5 and 7. The pin leaves the sequence part out when a column is a
//! map, so those calls give eight rows and not eleven. `all_flat` only changes the layout, so it is
//! checked and then has nothing to do.
//!
//! The pin fails on the sequence of an unsigned integer, because its sequence vector has no
//! unsigned case even though the function asks for one. This gives the 3, 5 and 7 the function
//! means to give, which tamnd/duckdb#44 describes.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_parse::ast;
use rudb_parse::{Ast, NONE, parse_ast_with_case};
use rudb_plan::{ColumnBinding, Expr, Node, NodeRef};

use crate::all_types::all_types_columns;
use crate::binder::Binder;
use crate::scope::{Scope, Visible};

/// The three values of a column or of a part of one, each written as SQL.
type Three = [String; 3];

/// The `test_all_types()` columns with their values written out, as [`all_types_columns`] gives
/// them.
type Columns = [(&'static str, &'static str, String, String)];

/// The one enum `test_all_types()` has a value of for every type, which is `small_enum`.
const SMALL_ENUM: &str = "ENUM('DUCK_DUCK_ENUM', 'GOOSE')";

impl Binder<'_> {
    /// The rows of a call to `test_vector_types`, or `None` if the call is to something else.
    pub(crate) fn vector_types(
        &mut self,
        ast: &Ast,
        called: &str,
        args: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<Option<(NodeRef, Scope)>> {
        if !called.eq_ignore_ascii_case("test_vector_types") {
            return Ok(None);
        }
        let written = ast.target_list(args).to_vec();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let types = self.vector_arguments(ast, &written);
        self.clause = previous;
        let types = types?;
        let text = vector_types_text(&types)?;
        let parsed = parse_ast_with_case(&text, self.semantics.identifier_case())?;
        let [ast::Statement::Query(query)] = parsed.statements[..] else {
            return Err(Error::internal("the text of test_vector_types is not one query"));
        };
        let outer = self.pinned_span.replace(self.current_span);
        let bound = self.bind_query(&parsed, query);
        self.pinned_span = outer;
        let (input, below) = bound?;
        // The text gives an empty list or a null map whatever type comes out of it, so every column
        // is cast to the type it was asked for, which is a cast to the same type for most of them.
        let label = if alias == NONE { "test_vector_types" } else { ast.string(alias) };
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(below.len());
        let mut names = Vec::with_capacity(below.len());
        let mut scope = Scope::empty();
        for ((at, column), ty) in below.columns.iter().enumerate().zip(&types) {
            let read = self.add_expr(Expr::Column(column.binding), column.ty.clone());
            exprs.push(self.cast_to(read, ty));
            names.push(self.plan_mut().intern(&column.name));
            scope.push(Visible {
                table: label.to_string(),
                binding: ColumnBinding::new(index, at as u32),
                ty: ty.clone(),
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

    /// The types of the positional arguments, with `all_flat` checked and set aside, or the pin's
    /// refusal of an argument.
    fn vector_arguments(&mut self, ast: &Ast, written: &[ast::Target]) -> Result<Vec<LogicalType>> {
        let empty = Scope::empty();
        let mut types = Vec::with_capacity(written.len());
        for argument in written {
            let named = argument.alias != NONE;
            if named {
                let name = ast.string(argument.alias);
                if !name.eq_ignore_ascii_case("all_flat") {
                    return Err(Error::binder(format!(
                        "Invalid named parameter \"{name}\" for function test_vector_types\n\
                         Candidates:\n    all_flat BOOLEAN\n"
                    )));
                }
            }
            let mut bound = self.bind_expr(ast, argument.expr, &empty)?;
            if named {
                bound = self.cast_to(bound, &LogicalType::Boolean);
            }
            let ty = self.plan().expr_type(bound).clone();
            let Some(value) = crate::fold::value_of(self.plan(), bound)? else {
                return Err(Error::binder("Table function cannot contain subqueries"));
            };
            if named {
                if !matches!(value, Value::Boolean(_)) {
                    return Err(Error::invalid_input("Cannot use NULL as argument for all_flat"));
                }
                continue;
            }
            if holds(&ty, &|inner| matches!(inner, LogicalType::Variant)) {
                return Err(Error::not_implemented("Unimplemented type for test_vector_types"));
            }
            types.push(ty);
        }
        if types.is_empty() {
            return Err(Error::binder(
                "No function matches the given name and argument types 'test_vector_types()'. You \
                 might need to add explicit type casts.\n\tCandidate functions:\n\t\
                 \"test_vector_types\"(ANY, all_flat : BOOLEAN)\n",
            ));
        }
        Ok(types)
    }
}

/// Whether `ty` is or has inside it a type that `found` picks out.
fn holds(ty: &LogicalType, found: &dyn Fn(&LogicalType) -> bool) -> bool {
    found(ty)
        || match ty {
            LogicalType::List(inner) | LogicalType::Array(inner, _) => holds(inner, found),
            LogicalType::Map(key, value) => holds(key, found) || holds(value, found),
            LogicalType::Struct(fields) | LogicalType::Union(fields) => {
                fields.iter().any(|field| holds(&field.ty, found))
            }
            _ => false,
        }
}

/// The name of the column at `at`, which the pin numbers from the second one on.
fn column_name(at: usize) -> String {
    if at == 0 { "test_vector".to_string() } else { format!("test_vector{}", at + 1) }
}

/// The query under `test_vector_types()` for columns of `types`.
fn vector_types_text(types: &[LogicalType]) -> Result<String> {
    for ty in types {
        // The pin builds a union as the struct it is stored as, and the check of the result is
        // what turns it away.
        if matches!(ty, LogicalType::Union(_)) {
            return Err(Error::conversion(
                "One or more rows in the produced UNION have validity set for more than 1 member",
            ));
        }
        if holds(ty, &|inner| matches!(inner, LogicalType::Union(_))) {
            return Err(Error::conversion(
                "One or more of the tags do not point to a valid union member",
            ));
        }
    }
    let leaves = all_types_columns();
    let flat = types.iter().map(|ty| flat_values(&leaves, ty)).collect::<Result<Vec<_>>>()?;
    let sequence = if types.iter().any(|ty| matches!(ty, LogicalType::Map(..))) {
        Vec::new()
    } else {
        types.iter().map(|ty| sequence_values(&leaves, ty)).collect::<Result<Vec<_>>>()?
    };
    // The flat rows, the constant ones, which are the first value three times, and the dictionary
    // ones, which are the second and the third.
    let mut rows: Vec<Vec<&str>> = [0, 1, 2, 0, 0, 0, 1, 2]
        .into_iter()
        .map(|at| flat.iter().map(|values| values[at].as_str()).collect())
        .collect();
    if !sequence.is_empty() {
        rows.extend((0..3).map(|at| sequence.iter().map(|values| values[at].as_str()).collect()));
    }
    let selects: Vec<String> = rows
        .iter()
        .map(|row| {
            let items: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(at, value)| format!("{value} AS \"{}\"", column_name(at)))
                .collect();
            format!("SELECT {}", items.join(", "))
        })
        .collect();
    Ok(selects.join(" UNION ALL "))
}

/// The flat values of `ty`: for a list the first two values of what is in it, an empty list and the
/// third, for a map the first entry, a null and the second, and for a struct the values of each
/// field side by side.
fn flat_values(leaves: &Columns, ty: &LogicalType) -> Result<Three> {
    match ty {
        LogicalType::List(inner) => {
            let values = flat_values(leaves, inner)?;
            Ok([
                format!("[{}, {}]", values[0], values[1]),
                "[]".to_string(),
                format!("[{}]", values[2]),
            ])
        }
        LogicalType::Map(key, value) => {
            let (keys, values) = (flat_values(leaves, key)?, flat_values(leaves, value)?);
            Ok([
                format!("MAP {{{}: {}}}", keys[0], values[0]),
                "NULL".to_string(),
                format!("MAP {{{}: {}}}", keys[1], values[1]),
            ])
        }
        LogicalType::Struct(fields) if !fields.is_empty() => {
            let values = fields
                .iter()
                .map(|field| flat_values(leaves, &field.ty))
                .collect::<Result<Vec<_>>>()?;
            Ok(std::array::from_fn(|at| struct_text(fields, &values, at)))
        }
        _ => leaf_values(leaves, ty),
    }
}

/// The sequence values of `ty`, which are the flat ones except that an integer counts 3, 5 and 7,
/// a list is the first two values of what is in it, an empty list and the third, and so is a map
/// inside something else.
fn sequence_values(leaves: &Columns, ty: &LogicalType) -> Result<Three> {
    match ty {
        LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt => Ok(["3", "5", "7"].map(|count| format!("CAST({count} AS {ty})"))),
        LogicalType::List(inner) => {
            let values = sequence_values(leaves, inner)?;
            Ok([
                format!("[{}, {}]", values[0], values[1]),
                "[]".to_string(),
                format!("[{}]", values[2]),
            ])
        }
        LogicalType::Map(key, value) => {
            let (keys, values) = (sequence_values(leaves, key)?, sequence_values(leaves, value)?);
            Ok([
                format!("MAP {{{}: {}, {}: {}}}", keys[0], values[0], keys[1], values[1]),
                "MAP {}".to_string(),
                format!("MAP {{{}: {}}}", keys[2], values[2]),
            ])
        }
        LogicalType::Struct(fields) if !fields.is_empty() => {
            let values = fields
                .iter()
                .map(|field| sequence_values(leaves, &field.ty))
                .collect::<Result<Vec<_>>>()?;
            Ok(std::array::from_fn(|at| struct_text(fields, &values, at)))
        }
        _ => leaf_values(leaves, ty),
    }
}

/// The struct made of value `at` of each field, written as a `row` when the fields have no names.
fn struct_text(fields: &[Field], values: &[Three], at: usize) -> String {
    if Field::unnamed(fields) {
        let items: Vec<&str> = values.iter().map(|value| value[at].as_str()).collect();
        return format!("row({})", items.join(", "));
    }
    let items: Vec<String> = fields
        .iter()
        .zip(values)
        .map(|(field, value)| format!("'{}': {}", field.name.replace('\'', "''"), value[at]))
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// The least value of `ty`, its greatest and a null, from the `test_all_types()` column of exactly
/// that type, or the pin's refusal when there is none.
fn leaf_values(leaves: &Columns, ty: &LogicalType) -> Result<Three> {
    let small_enum = ["DUCK_DUCK_ENUM", "GOOSE"];
    let name = match ty {
        LogicalType::Boolean => "bool",
        LogicalType::TinyInt => "tinyint",
        LogicalType::SmallInt => "smallint",
        LogicalType::Integer => "int",
        LogicalType::BigInt => "bigint",
        LogicalType::HugeInt => "hugeint",
        LogicalType::UHugeInt => "uhugeint",
        LogicalType::UTinyInt => "utinyint",
        LogicalType::USmallInt => "usmallint",
        LogicalType::UInteger => "uint",
        LogicalType::UBigInt => "ubigint",
        LogicalType::BigNum => "bignum",
        LogicalType::Date => "date",
        LogicalType::Time => "time",
        LogicalType::Timestamp => "timestamp",
        LogicalType::TimestampS => "timestamp_s",
        LogicalType::TimestampMs => "timestamp_ms",
        LogicalType::TimestampNs => "timestamp_ns",
        LogicalType::TimeTz => "time_tz",
        LogicalType::TimestampTz => "timestamp_tz",
        LogicalType::TimestampTzNs => "timestamp_tz_ns",
        LogicalType::Float => "float",
        LogicalType::Double => "double",
        LogicalType::Decimal { width: 4, scale: 1 } => "dec_4_1",
        LogicalType::Uuid => "uuid",
        LogicalType::Interval => "interval",
        LogicalType::Varchar => "varchar",
        LogicalType::Blob => "blob",
        LogicalType::Bit => "bit",
        LogicalType::Enum(labels) if labels.iter().eq(small_enum.iter()) => "small_enum",
        LogicalType::Array(inner, 3) if **inner == LogicalType::Integer => "fixed_int_array",
        LogicalType::TimeNs => "time_ns",
        LogicalType::Null => {
            return Err(Error::not_implemented(
                "Unimplemented type for test_vector_types \"NULL\"",
            ));
        }
        _ => {
            return Err(Error::not_implemented(format!(
                "Unimplemented type for test_vector_types {ty}"
            )));
        }
    };
    let Some((_, written, least, most)) = leaves.iter().find(|(column, ..)| *column == name) else {
        return Err(Error::internal(format!("test_all_types has no column {name}")));
    };
    let written = if written.is_empty() { SMALL_ENUM } else { written };
    Ok([
        format!("CAST({least} AS {written})"),
        format!("CAST({most} AS {written})"),
        format!("CAST(NULL AS {written})"),
    ])
}
