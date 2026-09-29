//! `make_type` and `get_type`, the two calls that answer a type as a value.
//!
//! Both are folded here, since the type they answer is settled before any row is read. `get_type`
//! answers the type its argument was bound to. `make_type` reads a type name and the parameters the
//! type takes, which are the child types of a list, a map or an array, the named fields of a
//! struct or a union, and the width and scale of a decimal, and it has to be given constants,
//! which the pin says too. Either answer is a `TYPE` constant holding the type's canonical text.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_parse::Ast;
use rudb_parse::ast;
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::fold;
use crate::scope::Scope;

/// One parameter of a `make_type` call, bound and folded, with the name it was passed under.
struct Parameter {
    name: Option<String>,
    ty: LogicalType,
    value: Value,
}

impl Binder<'_> {
    /// `make_type(name, ...)` or `get_type(x)`, folded to the type it answers, or `None` when the
    /// call does not have the arguments either takes, which the signature table then turns down.
    pub(crate) fn type_call(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        written: &str,
        args: ast::Slice,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        let arguments = ast.expr_list(args).to_vec();
        let named = ast.named_args(call).to_vec();
        if rudb_catalog::same_name(written, "get_type") {
            let [only] = arguments[..] else { return Ok(None) };
            if !named.is_empty() {
                return Ok(None);
            }
            let bound = self.bind_expr(ast, only, scope)?;
            self.over_aggregate(bound, scope)?;
            let ty = self.plan().expr_type(bound).clone();
            return Ok(Some(self.type_constant(&ty)));
        }
        let Some((&first, rest)) = arguments.split_first() else { return Ok(None) };
        let mut given = Vec::with_capacity(arguments.len() + named.len());
        let mut unnamed = rest.iter().map(|&arg| (None, arg)).collect::<Vec<_>>();
        unnamed.insert(0, (None, first));
        for target in &named {
            let name = ast.string(target.alias).to_string();
            if unnamed.iter().any(|(seen, _)| seen.as_deref() == Some(name.as_str())) {
                return Err(Error::binder(format!(
                    "Duplicate named argument \"{name}\" in function call to '\"make_type\"'"
                )));
            }
            unnamed.push((Some(name), target.expr));
        }
        for (name, arg) in unnamed {
            let bound = self.bind_expr(ast, arg, scope)?;
            let ty = self.plan().expr_type(bound).clone();
            let Some(value) = fold::value_of(self.plan(), bound)? else {
                return Err(Error::binder(
                    "make_type function arguments must be constant expressions",
                ));
            };
            given.push(Parameter { name, ty, value });
        }
        let head = given.remove(0);
        let name = match (&head.ty, &head.value) {
            (_, Value::Null) => return Ok(Some(self.type_constant(&LogicalType::Null))),
            (LogicalType::Varchar, Value::Varchar(name)) => name.clone(),
            _ => return Ok(None),
        };
        let ty = self.made_type(&name, &given)?;
        Ok(Some(self.type_constant(&ty)))
    }

    /// The type `name` names with these parameters, refused in the pin's words when it does not
    /// take them.
    fn made_type(&self, name: &str, given: &[Parameter]) -> Result<LogicalType> {
        let upper = name.to_ascii_uppercase();
        let types = || given.iter().map(parameter_type).collect::<Vec<_>>();
        let child = |at: usize| read_parameter(self, &given[at]);
        match upper.as_str() {
            "LIST" => match given {
                [] => Err(needs(name, "LIST(child TYPE)")),
                [one] if one.name.is_none() && one.ty == LogicalType::Type => {
                    Ok(LogicalType::list(child(0)?))
                }
                _ => Err(refuses(name, &types(), "LIST(child TYPE)")),
            },
            "MAP" => match given {
                [] => Err(needs(name, "MAP(key TYPE, value TYPE)")),
                [key, value]
                    if key.name.is_none()
                        && value.name.is_none()
                        && key.ty == LogicalType::Type
                        && value.ty == LogicalType::Type =>
                {
                    Ok(LogicalType::map(child(0)?, child(1)?))
                }
                _ => Err(refuses(name, &types(), "MAP(key TYPE, value TYPE)")),
            },
            "ARRAY" => match given {
                [] => Err(needs(name, "ARRAY(child TYPE, size BIGINT)")),
                [element, size] if element.ty == LogicalType::Type && size.ty.is_integer() => {
                    let length = size
                        .value
                        .as_i64()
                        .and_then(|length| u32::try_from(length).ok())
                        .filter(|&length| length > 0)
                        .ok_or_else(|| Error::binder("Array size must be greater than 0"))?;
                    Ok(LogicalType::array(child(0)?, length))
                }
                _ => Err(refuses(name, &types(), "ARRAY(child TYPE, size BIGINT)")),
            },
            "STRUCT" | "UNION" => {
                let mut fields = Vec::with_capacity(given.len());
                for (at, parameter) in given.iter().enumerate() {
                    let Some(field) = &parameter.name else {
                        return Err(Error::binder(format!(
                            "{upper} type arguments must have names"
                        )));
                    };
                    fields.push(Field::new(field.clone(), child(at)?));
                }
                Ok(if upper == "STRUCT" {
                    LogicalType::Struct(fields)
                } else {
                    LogicalType::Union(fields)
                })
            }
            "DECIMAL" | "NUMERIC" => decimal(name, given),
            // A length on a string parses and is dropped, as it is in a column definition.
            "VARCHAR" | "CHAR" | "BPCHAR" | "TEXT" | "STRING" | "NVARCHAR" if given.len() <= 1 => {
                Ok(LogicalType::Varchar)
            }
            _ if !given.is_empty() => {
                Err(Error::binder(format!("Type \"{name}\" does not take any type parameters")))
            }
            // A name only, so a type written with brackets or parameters is not one here.
            _ if name.contains(['[', '(']) => {
                Err(Error::catalog(format!("Type with name {name} does not exist!")))
            }
            _ => crate::statement::read_type(self.catalog(), name),
        }
    }

    /// A `TYPE` constant for `ty`, which holds its canonical text.
    fn type_constant(&mut self, ty: &LogicalType) -> ExprRef {
        let reference = self.plan_mut().add_value(Value::Varchar(ty.to_string()));
        self.add_expr(Expr::Constant(reference), LogicalType::Type)
    }
}

/// The type a parameter was bound as, which is how the pin lists them in its message.
fn parameter_type(parameter: &Parameter) -> LogicalType {
    parameter.ty.clone()
}

/// The type a `TYPE` parameter holds, read back out of its text.
fn read_parameter(binder: &Binder<'_>, parameter: &Parameter) -> Result<LogicalType> {
    match (&parameter.ty, &parameter.value) {
        (LogicalType::Type, Value::Varchar(text)) => {
            crate::statement::read_type(binder.catalog(), text)
        }
        (LogicalType::Type, Value::Null) => Ok(LogicalType::Null),
        (ty, _) => Err(Error::binder(format!("a type parameter of type {ty}"))),
    }
}

/// `make_type('DECIMAL', width, scale)`, which is `DECIMAL(18,3)` with no parameters and has a scale
/// of nothing when only the width is given.
fn decimal(name: &str, given: &[Parameter]) -> Result<LogicalType> {
    const CANDIDATES: &str = "decimal()\n\tdecimal(width UTINYINT, scale UTINYINT := 0)";
    if given.is_empty() {
        return Ok(LogicalType::Decimal { width: 18, scale: 3 });
    }
    let types: Vec<LogicalType> = given.iter().map(parameter_type).collect();
    if given.len() > 2 || given.iter().any(|p| !p.ty.is_integer() && p.ty != LogicalType::Null) {
        return Err(refuses(name, &types, CANDIDATES));
    }
    let mut read = [0_i64; 2];
    for (at, (parameter, what)) in given.iter().zip(["width", "scale"]).enumerate() {
        read[at] = parameter.value.as_i64().ok_or_else(|| {
            Error::binder(format!(
                "Type parameter \"{what}\" for type \"{}\" cannot be NULL",
                name.to_ascii_lowercase()
            ))
        })?;
    }
    let [width, scale] = read;
    let width = u8::try_from(width)
        .ok()
        .filter(|width| (1..=38).contains(width))
        .ok_or_else(|| Error::binder("DECIMAL type width must be between 1 and 38"))?;
    let scale = u8::try_from(scale)
        .ok()
        .filter(|&scale| scale <= width)
        .ok_or_else(|| Error::binder("DECIMAL type scale cannot be greater than width"))?;
    Ok(LogicalType::Decimal { width, scale })
}

/// The pin's refusal of a type that takes parameters and was given none.
fn needs(name: &str, candidates: &str) -> Error {
    Error::binder(format!(
        "Type \"{name}\" requires type parameters\n\tCandidate definitions:\n\t{candidates}"
    ))
}

/// The pin's refusal of parameters a type does not take.
fn refuses(name: &str, types: &[LogicalType], candidates: &str) -> Error {
    let listed: Vec<String> = types.iter().map(ToString::to_string).collect();
    Error::binder(format!(
        "Type \"{name}\" does not accept type parameters ({})\n\tCandidate \
         definitions:\n\t{candidates}",
        listed.join(", ")
    ))
}
