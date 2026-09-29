//! `finalize` and `combine`, the two calls that read an exported aggregate state.
//!
//! `sum(x) EXPORT_STATE` answers with the running state of the sum rather than the sum, typed as
//! an `AGGREGATE_STATE` that remembers the call it came from. `finalize` turns such a state into
//! the answer the aggregate would have given and `combine` folds two of them into one. Both are
//! settled here because their types come from the state's type: `finalize` answers whatever the
//! aggregate answers, and `combine` only takes two states of the same call over the same types.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;

impl Binder<'_> {
    /// `finalize(state)` or `combine(state, state)`, or `None` for any other call.
    pub(crate) fn state_call(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        let name = written.to_ascii_lowercase();
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();
        let mut bound = bound.to_vec();
        let (recorded, returns) = match (name.as_str(), types.as_slice()) {
            ("finalize", [LogicalType::AggregateState(state)]) => {
                // The constants the call was bound with ride along as arguments, since the state
                // written out does not hold them and finishing it can need them.
                if state.constants.iter().any(Option::is_some) {
                    for constant in &state.constants {
                        let value = constant.clone().unwrap_or(Value::Null);
                        bound.push(self.add_constant(value));
                    }
                }
                (rudb_kernels::finalize_name(state), state.returns.clone())
            }
            ("combine", [LogicalType::AggregateState(left), right]) => {
                let LogicalType::AggregateState(right) = right else {
                    return Err(different(right));
                };
                if left.function != right.function {
                    return Err(different(&LogicalType::AggregateState(right.clone())));
                }
                if left.constants != right.constants {
                    return Err(Error::binder(format!(
                        "Cannot COMBINE aggregate states of \"{}\" that were created with \
                         different parameters: [{}] <> [{}]",
                        left.function,
                        parameters(&left.arguments, &left.constants),
                        parameters(&right.arguments, &right.constants),
                    )));
                }
                if left.arguments != right.arguments {
                    return Err(Error::binder(format!(
                        "Cannot COMBINE aggregate states of \"{}\" that were created with \
                         different parameters: [{}] <> [{}]",
                        left.function,
                        listed(&left.arguments),
                        listed(&right.arguments),
                    )));
                }
                (name.clone(), types[0].clone())
            }
            ("finalize", [other]) | ("combine", [other, _]) => {
                return Err(Error::binder(format!(
                    "Can only \"{name}\" {}, not AGGREGATE_STATE",
                    shown(other)
                )));
            }
            _ => return Ok(None),
        };
        let args = self.plan_mut().add_expr_list(&bound);
        let recorded = self.plan_mut().intern(&recorded);
        Ok(Some(self.add_expr(Expr::Function { name: recorded, args }, returns)))
    }
}

/// A type the way the pin names it in these messages, where the null type is quoted.
fn shown(ty: &LogicalType) -> String {
    if *ty == LogicalType::Null { "\"NULL\"".to_string() } else { ty.to_string() }
}

fn listed(types: &[LogicalType]) -> String {
    types.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
}

/// The arguments of a call the way the pin lists them when two states' constants differ, each
/// with its type and the constant it was bound to.
fn parameters(types: &[LogicalType], constants: &[Option<Value>]) -> String {
    let shown = types.iter().enumerate().map(|(at, ty)| {
        let value = match constants.get(at) {
            Some(Some(value)) => value.to_string(),
            _ => "NULL".to_string(),
        };
        format!("{{'type': {ty}, 'value': {value}}}")
    });
    shown.collect::<Vec<_>>().join(", ")
}

fn different(right: &LogicalType) -> Error {
    Error::binder(format!(
        "Cannot COMBINE aggregate states from different functions, AGGREGATE_STATE <> {}",
        shown(right)
    ))
}
