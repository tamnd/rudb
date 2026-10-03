//! Binding struct literals and picking a field out of a struct.
//!
//! `{'a': 1, 'b': 'x'}` is `struct_pack(a := 1, b := 'x')` on the pin, a call whose type is a
//! STRUCT named by the call itself and not by the types of its arguments, so it is settled here and
//! not in the signature table, the same way `list_aggr` is. The plan records a call to
//! `struct_pack` whose type carries the names, and the kernel reads them off that type.
//!
//! `s.a`, `s['a']` and `struct_extract(s, 'a')` are one call too. The key has to be a constant,
//! since which field it names decides the type of the answer, so the binder checks it against the
//! struct's fields and the kernel finds the same field again by name.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::fold;

/// The name the plan records for a struct literal, which is the one the kernel dispatches on.
pub(crate) const STRUCT_PACK: &str = "struct_pack";

/// The name the plan records for picking one field out of a struct.
pub(crate) const STRUCT_EXTRACT: &str = "struct_extract";

/// The name the plan records for a union of one member.
pub(crate) const UNION_VALUE: &str = "union_value";

/// The name the plan records for the member a union row holds.
const UNION_TAG: &str = "union_tag";

/// The name the plan records for picking one member out of a union.
const UNION_EXTRACT: &str = "union_extract";

/// The call that picks a field out of any struct by its place, named or not.
const STRUCT_EXTRACT_AT: &str = "struct_extract_at";

impl Binder<'_> {
    /// A struct built from bound values and the names they were written with.
    ///
    /// A name written twice is refused whatever its case, since a field is found without case on
    /// the pin and two of them could not be told apart. The sentence is the pin's, which names the
    /// second of the two.
    pub(crate) fn pack_struct(&mut self, names: &[String], values: &[ExprRef]) -> Result<ExprRef> {
        for (at, name) in names.iter().enumerate() {
            if !name.is_empty()
                && names[..at].iter().any(|earlier| earlier.eq_ignore_ascii_case(name))
            {
                return Err(Error::binder(format!(
                    "Duplicate named argument \"{name}\" in function call to '\"struct_pack\"'"
                )));
            }
        }
        if values.is_empty() {
            return Ok(self.add_constant(Value::Struct(Vec::new())));
        }
        let fields = names
            .iter()
            .zip(values)
            .map(|(name, &value)| Field::new(name.clone(), self.plan().expr_type(value).clone()))
            .collect();
        let args = self.plan_mut().add_expr_list(values);
        let recorded = self.plan_mut().intern(STRUCT_PACK);
        Ok(self.add_expr(Expr::Function { name: recorded, args }, LogicalType::Struct(fields)))
    }

    /// A bound call that picks a field out of a struct, or `None` when the call is not one.
    ///
    /// `struct_extract` is always one, and a subscript is one when what it subscripts is a struct,
    /// which is how `s['a']` and `s.a` reach the same place.
    pub(crate) fn struct_field(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        if rudb_catalog::same_name(written, STRUCT_EXTRACT_AT) {
            return self.extract_at(bound).map(Some);
        }
        let extract = rudb_catalog::same_name(written, STRUCT_EXTRACT);
        let subscript = ["array_extract", "list_extract", "list_element"]
            .iter()
            .any(|name| rudb_catalog::same_name(written, name));
        let &[input, key] = bound else {
            return Ok(None);
        };
        let LogicalType::Struct(fields) = self.plan().expr_type(input).clone() else {
            return Ok(None);
        };
        if !extract && !subscript {
            return Ok(None);
        }
        let at = match fold::value_of(self.plan(), key) {
            Ok(Some(Value::Varchar(name))) => self.field_named(&fields, &name)?,
            Ok(Some(value)) if value.logical_type().is_integer() && Field::unnamed(&fields) => {
                let index = value.as_i64().unwrap_or(0);
                if index < 1 || index > fields.len() as i64 {
                    return Err(out_of_range(index, fields.len()));
                }
                index as usize - 1
            }
            Ok(Some(value)) if value.logical_type().is_integer() => {
                return Err(Error::binder(
                    "struct_extract with an integer key can only be used on unnamed structs, use \
                     a string key instead",
                ));
            }
            _ => {
                return Err(Error::binder(
                    "Key name for struct_extract needs to be a constant string",
                ));
            }
        };
        // The plan records the field by its place, counted from one, so the kernel does not look
        // a name up again and an unnamed struct is picked from the same way.
        let key = self.add_constant(Value::BigInt(at as i64 + 1));
        let args = self.plan_mut().add_expr_list(&[input, key]);
        let recorded = self.plan_mut().intern(STRUCT_EXTRACT);
        Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, fields[at].ty.clone())))
    }

    /// `j.a` and `j[1]` on a JSON value, which the pin binds as `json_extract` with the key made
    /// into a path, or `None` when the call is not one of those two.
    ///
    /// A constant field name becomes `$."a"`, so a name is only ever a key and never reads as a path
    /// of its own. A constant subscript that casts to a UINTEGER becomes `$[1]`, and any other string
    /// is a key the same way a field name is. Anything else, a negative index or a key that is not a
    /// constant, is passed as it was written, which is how `j[-1]` counts from the end.
    pub(crate) fn json_field(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        let &[input, key] = bound else {
            return Ok(None);
        };
        if *self.plan().expr_type(input) != LogicalType::Json {
            return Ok(None);
        }
        let element = rudb_catalog::same_name(written, "array_extract");
        if !element && !rudb_catalog::same_name(written, STRUCT_EXTRACT) {
            return Ok(None);
        }
        let mut key = key;
        if let Expr::Constant(held) = *self.plan().expr(key) {
            let value = self.plan().value(held).clone();
            let index = rudb_kernels::cast::cast_value(&value, &LogicalType::UInteger, true);
            let path = match (&value, index) {
                (Value::Null, _) => None,
                (_, Ok(Value::UInteger(index))) if element => Some(format!("$[{index}]")),
                (Value::Varchar(text), _) => Some(format!("$.\"{text}\"")),
                (value, _) if !element => Some(format!("$.\"{value}\"")),
                _ => None,
            };
            if let Some(path) = path {
                key = self.add_constant(Value::Varchar(path));
            }
        }
        self.call("json_extract", vec![input, key]).map(Some)
    }

    /// `struct_extract_at(s, i)`, the field at place `i` counted from one, in a named struct as well
    /// as an unnamed one.
    ///
    /// The place decides the type of the answer, so it has to be known before the query runs. A
    /// string is read as a number the way the pin casts a literal to its BIGINT, and a null place or
    /// a null struct is the untyped null.
    fn extract_at(&mut self, bound: &[ExprRef]) -> Result<ExprRef> {
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let fields = match types.as_slice() {
            [LogicalType::Struct(fields), index] if takes_place(index) => Some(fields.clone()),
            [LogicalType::Null, index] if takes_place(index) => None,
            _ => return Err(mismatch(STRUCT_EXTRACT_AT, &types)),
        };
        let Ok(Some(index)) = fold::value_of(self.plan(), bound[1]) else {
            return Err(Error::binder(
                "The \"index\" argument in function \"struct_extract_at\" must be a constant \
                 expression",
            ));
        };
        let (Some(fields), false) = (fields, index.is_null()) else {
            return Ok(self.add_constant(Value::Null));
        };
        let index = match index {
            Value::Varchar(text) => text.trim().parse::<i64>().map_err(|_| {
                Error::invalid_input(format!("Could not convert string '{text}' to INT64"))
            })?,
            other => other.as_i64().unwrap_or(0),
        };
        if index < 1 || index > fields.len() as i64 {
            return Err(out_of_range(index, fields.len()));
        }
        let key = self.add_constant(Value::BigInt(index));
        let args = self.plan_mut().add_expr_list(&[bound[0], key]);
        let recorded = self.plan_mut().intern(STRUCT_EXTRACT);
        let returns = fields[index as usize - 1].ty.clone();
        Ok(self.add_expr(Expr::Function { name: recorded, args }, returns))
    }

    /// `union_tag(u)`, `union_extract(u, 'k')` and `u.k`, or `None` when the call is not one of
    /// those on a union.
    ///
    /// The tag answers an enum of the member names, and a member answers its own type, so both are
    /// typed here. The member is found without case and recorded by its place counted from one, the
    /// way a struct field is, and a row holding another member answers null for it.
    pub(crate) fn union_call(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        let tag = rudb_catalog::same_name(written, UNION_TAG);
        let extract = rudb_catalog::same_name(written, UNION_EXTRACT);
        if !tag && !extract && !rudb_catalog::same_name(written, STRUCT_EXTRACT) {
            return Ok(None);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let members = match types.first() {
            Some(LogicalType::Union(members)) => members.clone(),
            _ if tag || extract => return Err(mismatch(&written.to_ascii_lowercase(), &types)),
            _ => return Ok(None),
        };
        if tag {
            let [input] = bound[..] else {
                return Err(mismatch(UNION_TAG, &types));
            };
            let labels: Vec<String> = members.iter().map(|member| member.name.clone()).collect();
            let args = self.plan_mut().add_expr_list(&[input]);
            let recorded = self.plan_mut().intern(UNION_TAG);
            let returns = LogicalType::Enum(labels.into());
            return Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, returns)));
        }
        let (&[input, key], Some(LogicalType::Varchar | LogicalType::Null)) =
            (bound, types.get(1))
        else {
            return Err(mismatch(UNION_EXTRACT, &types));
        };
        let at = match fold::value_of(self.plan(), key) {
            Ok(Some(Value::Null)) => return Ok(Some(self.add_constant(Value::Null))),
            Ok(Some(Value::Varchar(name))) => {
                members.iter().position(|member| member.name.eq_ignore_ascii_case(&name)).ok_or_else(
                    || {
                        let entries: Vec<String> =
                            members.iter().map(|member| format!("\"{}\"", member.name)).collect();
                        Error::binder(format!(
                            "Could not find key \"{name}\" in union\nCandidate Entries: {}",
                            entries.join(", ")
                        ))
                    },
                )?
            }
            _ => return Err(Error::binder("Key name for union_extract needs to be a constant string")),
        };
        let key = self.add_constant(Value::BigInt(at as i64 + 1));
        let args = self.plan_mut().add_expr_list(&[input, key]);
        let recorded = self.plan_mut().intern(UNION_EXTRACT);
        Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, members[at].ty.clone())))
    }

    /// `union_value(k := v)`, a union of the one member it names, holding the value even when the
    /// value is a null.
    pub(crate) fn union_value(&mut self, names: &[String], values: &[ExprRef]) -> Result<ExprRef> {
        let ([name], &[value]) = (names, values) else {
            return Err(Error::binder("union_value takes exactly one argument"));
        };
        let ty = self.plan().expr_type(value).clone();
        let args = self.plan_mut().add_expr_list(&[value]);
        let recorded = self.plan_mut().intern(UNION_VALUE);
        let returns = LogicalType::Union(vec![Field::new(name.clone(), ty)]);
        Ok(self.add_expr(Expr::Function { name: recorded, args }, returns))
    }

    /// Where the field of that name is, found without case, or the pin's refusal naming them all.
    fn field_named(&self, fields: &[Field], name: &str) -> Result<usize> {
        fields.iter().position(|field| field.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
            let entries: Vec<String> =
                fields.iter().map(|field| format!("\"{}\"", field.name)).collect();
            Error::binder(format!(
                "Could not find key \"{name}\" in struct\n\nCandidate Entries: {}",
                entries.join(", ")
            ))
        })
    }

    /// `struct_insert`, `struct_update`, `struct_concat`, `struct_keys`, `struct_values`,
    /// `struct_contains` and `struct_position` with their spellings `struct_has` and
    /// `struct_indexof`, and `list_zip` with its spelling `array_zip`, or `None` for any other call.
    ///
    /// Each answers a type made out of the fields of its arguments, so the type is worked out here
    /// and the kernel in `rudb_kernels::structs` puts the values where the type says they go.
    pub(crate) fn struct_call(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        let name = written.to_ascii_lowercase();
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let fields_of = |at: usize| match types.get(at) {
            Some(LogicalType::Struct(fields)) => Some(fields.clone()),
            _ => None,
        };
        // An alias is refused in its own name but recorded under the name the kernel answers to.
        let mut recorded: &str = &name;
        let returns = match name.as_str() {
            "struct_insert" | "struct_update" => {
                let Some(mut fields) = fields_of(0) else {
                    return Ok(None);
                };
                let Some(added) = fields_of(1).filter(|_| bound.len() == 2) else {
                    if bound.len() == 1 && name == "struct_insert" {
                        return Err(Error::invalid_input("Can't insert nothing into a STRUCT"));
                    }
                    return Err(Error::binder(format!(
                        "Need named argument for struct {}, e.g., a := b",
                        &name[7..]
                    )));
                };
                for field in added {
                    let same =
                        fields.iter().position(|one| one.name.eq_ignore_ascii_case(&field.name));
                    match same {
                        Some(_) if name == "struct_insert" => {
                            return Err(Error::binder(format!(
                                "Duplicate struct entry name \"\"{}\"\"",
                                field.name
                            )));
                        }
                        Some(at) => fields[at] = field,
                        None => fields.push(field),
                    }
                }
                LogicalType::Struct(fields)
            }
            "struct_concat" => {
                let mut fields: Vec<Field> = Vec::new();
                let mut unnamed = None;
                for (at, ty) in types.iter().enumerate() {
                    let LogicalType::Struct(held) = ty else {
                        return Err(Error::invalid_input(format!(
                            "struct_concat: Argument at position \"{}\" is not a STRUCT",
                            at + 1
                        )));
                    };
                    let this = Field::unnamed(held);
                    if *unnamed.get_or_insert(this) != this {
                        return Err(Error::invalid_input(
                            "struct_concat: Cannot mix named and unnamed STRUCTs",
                        ));
                    }
                    for field in held {
                        if !this
                            && fields.iter().any(|one| one.name.eq_ignore_ascii_case(&field.name))
                        {
                            return Err(Error::invalid_input(format!(
                                "struct_concat: Arguments contain duplicate STRUCT entry \"{}\"",
                                field.name
                            )));
                        }
                        fields.push(field.clone());
                    }
                }
                if fields.is_empty() {
                    return Ok(None);
                }
                LogicalType::Struct(fields)
            }
            "struct_keys" | "struct_values" => {
                let Some(fields) = fields_of(0).filter(|_| bound.len() == 1) else {
                    return Ok(None);
                };
                if name == "struct_keys" {
                    if Field::unnamed(&fields) {
                        return Err(Error::invalid_input(
                            "struct_keys() expects a STRUCT argument",
                        ));
                    }
                    LogicalType::list(LogicalType::Varchar)
                } else {
                    let unnamed = fields.iter().map(|field| Field::new("", field.ty.clone()));
                    LogicalType::Struct(unnamed.collect())
                }
            }
            "struct_contains" | "struct_has" | "struct_position" | "struct_indexof" => {
                let position = matches!(name.as_str(), "struct_position" | "struct_indexof");
                match types.as_slice() {
                    // The pin folds `struct_contains(NULL, x)` to a BOOLEAN null before binding, and
                    // binds `struct_position(NULL, x)` to the untyped null, since only the second
                    // one looks at a null argument itself.
                    [LogicalType::Null, _] if position => {
                        return Ok(Some(self.add_constant(Value::Null)));
                    }
                    [LogicalType::Null, _] => {}
                    [LogicalType::Struct(fields), _] if fields.is_empty() => {}
                    [LogicalType::Struct(fields), _] if !Field::unnamed(fields) => {
                        return Err(Error::binder(format!(
                            "\"{name}\" can only be used on unnamed structs"
                        )));
                    }
                    [LogicalType::Struct(_), _] => {}
                    _ => return Err(mismatch(&name, &types)),
                }
                recorded = if position { "struct_position" } else { "struct_contains" };
                if position { LogicalType::Integer } else { LogicalType::Boolean }
            }
            "list_zip" | "array_zip" => {
                let Some(last) = types.last() else {
                    return Err(Error::binder(format!("Provide at least one argument to {name}")));
                };
                // A trailing BOOLEAN is the flag that cuts every row to its shortest list, and it is
                // only a flag in the last place.
                let lists = types.len() - usize::from(*last == LogicalType::Boolean);
                if lists == 0 {
                    return Err(Error::binder(format!(
                        "Provide at least one list argument to {name}"
                    )));
                }
                let mut fields = Vec::with_capacity(lists);
                for ty in &types[..lists] {
                    fields.push(Field::new(
                        "",
                        match ty {
                            LogicalType::List(element) => (**element).clone(),
                            LogicalType::Null => LogicalType::Null,
                            _ => return Err(Error::binder("Parameter type needs to be List")),
                        },
                    ));
                }
                recorded = "list_zip";
                LogicalType::list(LogicalType::Struct(fields))
            }
            _ => return Ok(None),
        };
        let args = self.plan_mut().add_expr_list(bound);
        let recorded = self.plan_mut().intern(recorded);
        Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, returns)))
    }
}

/// Whether a place of this type is one `struct_extract_at` takes, which is what the pin casts to a
/// BIGINT without being asked: the narrower integers, a string and the untyped null.
fn takes_place(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Null
            | LogicalType::Varchar
            | LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
    )
}

/// The pin's refusal of a place outside the struct, which names `struct_extract` whichever of the
/// two calls it came from.
fn out_of_range(index: i64, fields: usize) -> Error {
    Error::binder(format!(
        "Key index {index} for struct_extract out of range - expected an index between 1 and \
         {fields}"
    ))
}

/// The pin's refusal of a struct call that none of its overloads takes, naming what it was given.
fn mismatch(name: &str, types: &[LogicalType]) -> Error {
    let spelled: Vec<String> = types.iter().map(ToString::to_string).collect();
    rudb_functions::named_mismatch(name, &spelled, false)
}
