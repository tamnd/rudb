//! Binding `list_aggr`, which runs an aggregate over the elements of each list.
//!
//! `list_aggr([1, 2, 3], 'sum')` is a scalar call whose second argument names an aggregate, so the
//! call's type is not the declared `ANY` of its one overload but whatever that aggregate returns
//! over the element type. That depends on the value of an argument and not only on its type, which
//! is why the binder settles it here instead of the signature table. The name has to be a constant
//! for the same reason, and the pin refuses one that is not.
//!
//! The aggregate is resolved the way `sum(x)` would be, over the element type and the arguments
//! after the name, and the list is cast to the element type the aggregate asked for. The executor
//! then folds each list through the same accumulator a `GROUP BY` uses, so an aggregate rudb can
//! run over rows is one it can run over a list, and one it cannot is refused in both places.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_functions::{FunctionKind, kind_of, resolve};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::fold;

/// Every name `list_aggr` answers to.
const NAMES: &[&str] =
    &["list_aggr", "list_aggregate", "array_aggr", "array_aggregate", "aggregate"];

/// The name the plan records, which is the one the executor dispatches on.
pub(crate) const LIST_AGGR: &str = "list_aggr";

/// The aggregates the pin has a `list_` macro for, each of which is `list_aggr(l, 'name')`.
///
/// Read off `duckdb_functions()` where the macro definition calls `list_aggr`. A name here that
/// rudb has no aggregate for yet is still expanded, so it is refused as a missing aggregate and not
/// as a missing function, which is what the pin would say if it lacked one too.
const MACROS: &[&str] = &[
    "any_value",
    "approx_count_distinct",
    "avg",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bool_and",
    "bool_or",
    "count",
    "entropy",
    "first",
    "histogram",
    "kurtosis",
    "kurtosis_pop",
    "last",
    "mad",
    "max",
    "median",
    "min",
    "mode",
    "product",
    "sem",
    "skewness",
    "stddev_pop",
    "stddev_samp",
    "string_agg",
    "sum",
    "var_pop",
    "var_samp",
];

impl Binder<'_> {
    /// A bound call to `list_aggr` under any of its names, or `None` for any other name.
    ///
    /// A null list is a null answer before the aggregate is looked at, so `list_aggr(NULL, 'nope')`
    /// is null on the pin and not a missing function. An aggregate that does not accept the
    /// element type, or the arguments after the name, is refused with the aggregate's own sentence
    /// under a line that says it was the aggregate that did not match, which is how the pin puts it.
    pub(crate) fn list_aggregate(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        if let Some(aggregate) = macro_aggregate(written) {
            if bound.len() != 1 {
                let written = written.to_lowercase();
                return Err(Error::binder(format!(
                    "Macro {written}() does not support the supplied arguments. You might need to \
                     add explicit type casts.\nCandidate macros:\n\t{written}(l)"
                )));
            }
            let name = self.add_constant(Value::Varchar(aggregate.to_string()));
            return self.list_aggregate(LIST_AGGR, &[bound[0], name]);
        }
        if !NAMES.iter().any(|name| rudb_catalog::same_name(written, name)) {
            return Ok(None);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let listed = matches!(
            types.first(),
            Some(LogicalType::List(_) | LogicalType::Array(..) | LogicalType::Null)
        );
        let named = matches!(types.get(1), Some(LogicalType::Varchar | LogicalType::Null));
        if !listed || !named {
            return Err(no_match(written, &types));
        }
        let mut list = bound[0];
        let element = match &types[0] {
            LogicalType::List(element) => (**element).clone(),
            LogicalType::Array(element, _) => {
                let element = (**element).clone();
                list = self.cast_to(list, &LogicalType::List(Box::new(element.clone())));
                element
            }
            _ => return Ok(Some(self.add_constant(Value::Null))),
        };
        let name = match fold::value_of(self.plan(), bound[1]) {
            Ok(Some(Value::Varchar(name))) => name,
            Ok(Some(Value::Null)) => "NULL".to_string(),
            _ => {
                return Err(Error::binder(format!(
                    "The \"col1\" argument in function \"{}\" must be a constant expression",
                    written.to_lowercase()
                )));
            }
        };
        match kind_of(&name) {
            Some(FunctionKind::Aggregate) => {}
            Some(_) => return Err(Error::catalog(format!("{name} is not an aggregate function"))),
            None => {
                return Err(Error::catalog(format!(
                    "Aggregate Function with name {name} does not exist!"
                )));
            }
        }
        let mut arguments = vec![element.clone()];
        arguments.extend(types[2..].iter().cloned());
        let resolved = resolve(&name, &arguments)
            .map_err(|error| Error::binder(format!("No matching aggregate function\n{error}")))?;
        if resolved.arguments[0] != element {
            let wanted = LogicalType::List(Box::new(resolved.arguments[0].clone()));
            list = self.checked_cast_to(list, &wanted, false)?;
        }
        let mut args = vec![list, self.add_constant(Value::Varchar(resolved.name.to_string()))];
        for (&extra, wanted) in bound[2..].iter().zip(&resolved.arguments[1..]) {
            args.push(self.checked_cast_to(extra, wanted, false)?);
        }
        let args = self.plan_mut().add_expr_list(&args);
        let recorded = self.plan_mut().intern(LIST_AGGR);
        Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, resolved.returns)))
    }
}

/// The aggregate a `list_` macro stands for, or `None` for any other name.
fn macro_aggregate(written: &str) -> Option<&'static str> {
    let head = written.get(..5)?;
    if !head.eq_ignore_ascii_case("list_") {
        return None;
    }
    MACROS.iter().copied().find(|name| written[5..].eq_ignore_ascii_case(name))
}

/// The pin's refusal of a call whose list or name argument is the wrong type.
fn no_match(written: &str, types: &[LogicalType]) -> Error {
    let written = written.to_lowercase();
    let types: Vec<String> = types.iter().map(ToString::to_string).collect();
    Error::binder(format!(
        "No function matches the given name and argument types '{written}({})'. You might need to \
         add explicit type casts.\n\tCandidate functions:\n\t{written}(col0 ANY[], col1 VARCHAR, \
         [ANY...]) -> ANY\n",
        types.join(", ")
    ))
}
