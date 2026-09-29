//! `finalize` and `combine`, the two calls that read an exported aggregate state.
//!
//! `sum(x) EXPORT_STATE` answers with the running state of the sum rather than the sum, typed as
//! an `AGGREGATE_STATE` that remembers the call it came from. `finalize` turns such a state into
//! the answer the aggregate would have given and `combine` folds two of them into one. Both are
//! settled here because their types come from the state's type: `finalize` answers whatever the
//! aggregate answers, and `combine` only takes two states of the same call over the same types.

use rudb_common::{Error, LogicalType, Result, StateKey, Value};
use rudb_functions::{FunctionKind, kind_of, resolve};
use rudb_plan::{Expr, ExprRef};

use crate::binder::Binder;
use crate::fold;

/// The name `to_aggregate_state` is stored under, which the kernel answers with its input.
const TO_STATE: &str = "to_aggregate_state";

impl Binder<'_> {
    /// `finalize(state)` or `combine(state, state)`, or `None` for any other call.
    pub(crate) fn state_call(
        &mut self,
        written: &str,
        bound: &[ExprRef],
    ) -> Result<Option<ExprRef>> {
        let name = written.to_ascii_lowercase();
        if name == TO_STATE && (3..=5).contains(&bound.len()) {
            return self.state_from(bound).map(Some);
        }
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
                // An ordered state holds its keys too, and the pin tells only the arguments apart
                // in its message, so two states that sort differently read the same there.
                if left.arguments != right.arguments
                    || left.order != right.order
                    || left.layout != right.layout
                {
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

impl Binder<'_> {
    /// `to_aggregate_state(data, name, signature, constants, order)`, which types `data` as the state
    /// of the call of `name` over the types in `signature`, bound to the constants in `constants`
    /// and sorted by `order` when it is an ordered one. The data has to be in the state's layout,
    /// and for an ordered state it is the rows, with `order` naming the column each key sorts on.
    fn state_from(&mut self, bound: &[ExprRef]) -> Result<ExprRef> {
        let mut given = Vec::with_capacity(bound.len() - 1);
        for &arg in &bound[1..] {
            match fold::value_of(self.plan(), arg)? {
                Some(value) => given.push(value),
                None => {
                    return Err(Error::binder(format!(
                        "{TO_STATE}: the aggregate name, signature and constant parameters must \
                         be constant"
                    )));
                }
            }
        }
        let Value::Varchar(name) = &given[0] else {
            return Err(Error::binder(format!("{TO_STATE}: the aggregate name must be a string")));
        };
        let name = name.to_ascii_lowercase();
        if !matches!(kind_of(&name), Some(FunctionKind::Aggregate)) {
            return Err(Error::catalog(format!(
                "Aggregate Function with name {name} does not exist!"
            )));
        }
        let Value::List { values: signature, .. } = &given[1] else {
            return Err(Error::binder(format!(
                "{TO_STATE}: the signature must be a list of types"
            )));
        };
        let mut types = Vec::with_capacity(signature.len());
        for entry in signature {
            types.push(match entry {
                Value::Null => {
                    return Err(Error::binder(format!(
                        "{TO_STATE}: the signature cannot contain NULL values"
                    )));
                }
                Value::Varchar(text) => crate::statement::read_type(self.catalog(), text)?,
                _ => {
                    return Err(Error::binder(format!(
                        "{TO_STATE}: the signature must be a list of types"
                    )));
                }
            });
        }
        let constants = self.state_constants(given.get(2), &types)?;
        let resolved = resolve(&name, &types)?;
        let layout = rudb_kernels::state_layout(resolved.name, &types, &resolved.returns)
            .map_err(|error| converted(resolved.name, error))?;
        let data = bound[0];
        let (data, layout, order) = match given.get(3) {
            Some(order) => {
                let ty = self.plan().expr_type(data).clone();
                let LogicalType::List(element) = &ty else { return Err(unbuffered()) };
                let LogicalType::Struct(fields) = element.as_ref() else {
                    return Err(unbuffered());
                };
                let order = self.state_order(order, fields.len())?;
                if order.is_empty() {
                    return Err(Error::binder(format!(
                        "{TO_STATE}: an ordered aggregate state must have at least one ORDER BY key"
                    )));
                }
                (data, ty, order)
            }
            None => (self.checked_cast_to(data, &layout, false)?, layout, Vec::new()),
        };
        let ty = LogicalType::aggregate_state(
            resolved.name,
            types,
            resolved.returns,
            layout,
            constants,
            order,
        );
        let args = self.plan_mut().add_expr_list(&[data]);
        let recorded = self.plan_mut().intern(TO_STATE);
        Ok(self.add_expr(Expr::Function { name: recorded, args }, ty))
    }

    /// The constants of a `to_aggregate_state`, one per argument and a null for an argument that
    /// is not bound to one, each cast to its argument's type.
    fn state_constants(
        &mut self,
        given: Option<&Value>,
        types: &[LogicalType],
    ) -> Result<Vec<Option<Value>>> {
        let values = match given {
            None | Some(Value::Null) => return Ok(vec![None; types.len()]),
            Some(Value::List { values, .. }) => values,
            Some(_) => {
                return Err(Error::binder(format!(
                    "{TO_STATE}: the constant parameters must be a list with one entry per \
                     argument (use NULL for arguments that are not bound to a constant), e.g. \
                     [NULL, '|']"
                )));
            }
        };
        if values.len() != types.len() {
            return Err(Error::binder(format!(
                "{TO_STATE}: the constant parameters list has {} entries but the aggregate has {} \
                 arguments - it must have exactly one entry per argument (use NULL for arguments \
                 that are not bound to a constant)",
                values.len(),
                types.len()
            )));
        }
        let mut constants = Vec::with_capacity(values.len());
        for (value, ty) in values.iter().zip(types) {
            if value.is_null() {
                constants.push(None);
                continue;
            }
            let constant = self.add_constant(value.clone());
            let cast = self.checked_cast_to(constant, ty, false)?;
            constants.push(fold::value_of(self.plan(), cast)?);
        }
        Ok(constants)
    }

    /// The keys of a `to_aggregate_state` over buffered rows, each a struct of the column it sorts
    /// on and its modifiers the way `create_sort_key` spells them, `DESC NULLS LAST`.
    fn state_order(&self, given: &Value, columns: usize) -> Result<Vec<StateKey>> {
        let entries = match given {
            Value::Null => return Ok(Vec::new()),
            Value::List { values, .. } => values,
            _ => {
                return Err(Error::binder(format!(
                    "{TO_STATE}: the ORDER BY argument must be a list of {{column, order}} \
                     structs, e.g. [{{'column': 1, 'order': 'DESC NULLS LAST'}}]"
                )));
            }
        };
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            let Value::Struct(fields) = entry else {
                return Err(Error::binder(format!(
                    "{TO_STATE}: each ORDER BY entry must be a {{column, order}} struct"
                )));
            };
            let field =
                |name: &str| fields.iter().find(|(held, _)| held == name).map(|(_, value)| value);
            let (Some(column), Some(Value::Varchar(order))) = (field("column"), field("order"))
            else {
                return Err(Error::binder(format!(
                    "{TO_STATE}: each ORDER BY entry must have a non-NULL 'column' and 'order'"
                )));
            };
            let Some(column) = column.as_i64() else {
                return Err(Error::binder(format!(
                    "{TO_STATE}: each ORDER BY entry must have a non-NULL 'column' and 'order'"
                )));
            };
            let column = usize::try_from(column).unwrap_or(usize::MAX);
            if column >= columns {
                return Err(Error::binder(format!(
                    "{TO_STATE}: ORDER BY column {column} is out of range (the state has {columns} \
                     columns)"
                )));
            }
            let (descending, nulls_first) = modifiers(order)?;
            keys.push(StateKey { descending, nulls_first, column });
        }
        Ok(keys)
    }
}

/// The direction and the place of nulls a sort modifier names, which has to say both.
fn modifiers(text: &str) -> Result<(bool, bool)> {
    let words: Vec<String> = text.split_whitespace().map(str::to_ascii_uppercase).collect();
    let descending = match words.first().map(String::as_str) {
        Some("ASC") => false,
        Some("DESC") => true,
        _ => {
            return Err(Error::binder(
                "create_sort_key modifier must start with either ASC or DESC",
            ));
        }
    };
    match &words[1..] {
        [nulls, place] if nulls == "NULLS" && place == "FIRST" => Ok((descending, true)),
        [nulls, place] if nulls == "NULLS" && place == "LAST" => Ok((descending, false)),
        _ => Err(Error::binder(
            "create_sort_key modifier must end with either NULLS FIRST or NULLS LAST",
        )),
    }
}

/// The pin's refusal of an ordered state that is not a list of rows.
fn unbuffered() -> Error {
    Error::binder(format!(
        "{TO_STATE}: an ordered aggregate state value must be a LIST of STRUCTs (the buffer of \
         values), e.g. [{{'v0': ...}}, ...]"
    ))
}

/// The refusal of an aggregate with no state, in the words the pin uses for `to_aggregate_state`,
/// which differ from the ones it uses for `EXPORT_STATE`.
fn converted(name: &str, error: Error) -> Error {
    if error.message().contains("does not have a state type callback") {
        return Error::binder(format!(
            "Aggregate function \"{name}\" does not have a state type callback defined - cannot \
             convert to its state"
        ));
    }
    error
}

/// Refuses a `combine_aggr` over anything but a state.
pub(crate) fn merged_argument(ty: &LogicalType) -> Result<()> {
    match ty {
        LogicalType::AggregateState(_) => Ok(()),
        other => Err(Error::binder(format!(
            "Can only \"combine_aggr\" {}, not AGGREGATE_STATE",
            shown(other)
        ))),
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
