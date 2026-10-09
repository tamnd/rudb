//! PostgreSQL's ordered-set and hypothetical-set aggregates, `percentile_cont(0.5) WITHIN GROUP
//! (ORDER BY x)` and `rank(3) WITHIN GROUP (ORDER BY x)`, typed as `parse_func.c` types them.
//!
//! The PostgreSQL parser keeps a `WITHIN GROUP` call under the name it was written with, and the
//! call reaches this module and not the pin's quantiles. The arguments in front of `WITHIN GROUP`
//! are the direct arguments, which are one value for a group, and the `ORDER BY` names the values
//! the aggregate reads. A direct argument is bound inside the call, so an aggregate in it is
//! nested, and it may read only the grouped columns, which is checked here since it is computed
//! with the rows and not over the groups.
//!
//! The call is handed to the states of `rudb_kernels::pgorderedset` under the names of
//! PostgreSQL's final functions, made into an ordered name so that the sort keys come with them.
//! A percentile is its value and then its fraction, `mode` is its value, and a hypothetical-set
//! call is its keys and then its direct arguments, each cast to the type it shares with its key.

use rudb_common::{Error, LogicalType, Result, SqlState, StateKey};
use rudb_functions::{FunctionKind, kind_of, resolve};
use rudb_parse::ast::{self, Ast, Nulls};
use rudb_plan::ExprRef;

use crate::binder::Binder;
use crate::expr::{postgres_type_name, undefined_function};
use crate::scope::Scope;

/// What a PostgreSQL ordered-set aggregate is, by its name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// `percentile_cont` and `percentile_disc`, one fraction over one key.
    Percentile { continuous: bool },
    /// `mode`, no direct argument over one key.
    Mode,
    /// `rank`, `dense_rank`, `percent_rank` and `cume_dist`, one direct argument for each key.
    Hypothetical(&'static str, LogicalType),
}

impl Kind {
    fn named(name: &str) -> Option<Self> {
        let lowered = name.to_ascii_lowercase();
        Some(match lowered.as_str() {
            "percentile_cont" => Self::Percentile { continuous: true },
            "percentile_disc" => Self::Percentile { continuous: false },
            "mode" => Self::Mode,
            "rank" => Self::Hypothetical("hypothetical_rank_final", LogicalType::BigInt),
            "dense_rank" => {
                Self::Hypothetical("hypothetical_dense_rank_final", LogicalType::BigInt)
            }
            "percent_rank" => {
                Self::Hypothetical("hypothetical_percent_rank_final", LogicalType::Double)
            }
            "cume_dist" => Self::Hypothetical("hypothetical_cume_dist_final", LogicalType::Double),
            _ => return None,
        })
    }

    /// Whether a call of `arguments` arguments with no `WITHIN GROUP` is this aggregate's
    /// signature read whole, which PostgreSQL finds and then refuses for the missing clause.
    fn takes_whole(&self, arguments: usize) -> bool {
        match self {
            Self::Percentile { .. } => arguments == 2,
            Self::Mode => arguments == 1,
            Self::Hypothetical(..) => arguments > 0,
        }
    }
}

impl Binder<'_> {
    /// The error of PostgreSQL for an ordered-set aggregate called with no `WITHIN GROUP`, as
    /// `percentile_cont(p, p)` and `rank(1)` are, or `None` for any other call. `rank()` is the
    /// window function and not this, and its arguments say so.
    pub(crate) fn within_group_required(&self, written: &str, arguments: usize) -> Option<Error> {
        let kind = Kind::named(written)?;
        if !kind.takes_whole(arguments) {
            return None;
        }
        let message = format!("WITHIN GROUP is required for ordered-set aggregate {written}");
        Some(Error::binder(message).state(SqlState::WRONG_OBJECT_TYPE))
    }

    /// Binds `name(direct) WITHIN GROUP (ORDER BY sorted) FILTER (WHERE filter)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_within_group(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        written: &str,
        direct: &[ast::ExprRef],
        sorted: &[ast::OrderItem],
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        if self.trying {
            return Err(Error::binder("aggregates are not allowed inside the TRY expression"));
        }
        let frames = std::mem::take(&mut self.lambda_frames);
        let bound = self.bind_ordered_set(ast, call, written, direct, sorted, filter, scope);
        self.lambda_frames = frames;
        bound
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_ordered_set(
        &mut self,
        ast: &Ast,
        call: ast::ExprRef,
        written: &str,
        direct: &[ast::ExprRef],
        sorted: &[ast::OrderItem],
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<ExprRef> {
        self.aggregate_allowed()?;
        // PostgreSQL transforms every argument before it looks the function up, so a column that is
        // not there or an aggregate inside the call is refused first.
        let filter = self.aggregate_filter(ast, filter, scope)?;
        let mut written_args: Vec<ast::ExprRef> = direct.to_vec();
        written_args.extend(sorted.iter().map(|item| item.expr));
        self.in_aggregate = true;
        let bound: Result<Vec<ExprRef>> =
            written_args.iter().map(|&arg| self.bind_expr(ast, arg, scope)).collect();
        self.in_aggregate = false;
        let mut bound = bound?;
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan().expr_type(arg).clone()).collect();

        let Some(kind) = Kind::named(written) else {
            return Err(not_ordered_set(ast, call, written, &written_args, &types));
        };
        if !kind.takes_whole(written_args.len()) {
            return Err(undefined(ast, call, written, &written_args, &types));
        }
        if ast.within_group_over(call) {
            let message = format!("OVER is not supported for ordered-set aggregate {written}");
            return Err(Error::not_implemented(message.clone())
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .pg(message));
        }
        let wanted = match kind {
            Kind::Percentile { .. } => Some(1),
            Kind::Mode => Some(0),
            Kind::Hypothetical(..) => None,
        };
        let hint = match wanted {
            Some(wanted) if direct.len() != wanted => Some(format!(
                "There is an ordered-set aggregate {written}, but it requires {wanted} direct \
                 argument{}, not {}.",
                if wanted == 1 { "" } else { "s" },
                direct.len()
            )),
            None if direct.len() != sorted.len() => Some(format!(
                "To use the hypothetical-set aggregate {written}, the number of hypothetical \
                 direct arguments (here {}) must match the number of ordering columns (here {}).",
                direct.len(),
                sorted.len()
            )),
            _ => None,
        };
        if let Some(hint) = hint {
            return Err(missing(ast, call, written, &written_args, &types).hint(hint));
        }

        let keys = bound.split_off(direct.len());
        let mut order = Vec::with_capacity(sorted.len());
        for (column, (&key, &item)) in keys.iter().zip(sorted).enumerate() {
            let ty = self.plan().expr_type(key).clone();
            let item = self.sort_operators(ast, item, &ty)?;
            let descending = self.descending(item.order);
            let nulls_first = match item.nulls {
                Nulls::First => true,
                Nulls::Last => false,
                Nulls::Unstated => self.semantics.nulls_first(descending),
            };
            order.push(StateKey { descending, nulls_first, column });
        }

        let (inner, args, returns) = match kind {
            Kind::Percentile { continuous } => {
                let (value, fraction) = (keys[0], bound[0]);
                // `percentile_cont` is over a `double precision` or an `interval`, and
                // `percentile_disc` is over any type and answers that type.
                let value_type = match (continuous, self.plan().expr_type(value).clone()) {
                    (true, LogicalType::Interval) => LogicalType::Interval,
                    (true, number) if numeric(&number) => LogicalType::Double,
                    (true, _) => {
                        return Err(undefined(ast, call, written, &written_args, &types));
                    }
                    (false, any) => any,
                };
                let Some(depth) = fraction_depth(self.plan().expr_type(fraction)) else {
                    return Err(undefined(ast, call, written, &written_args, &types));
                };
                let value = self.cast_to(value, &value_type);
                let fraction = self.cast_to(fraction, &nested(LogicalType::Double, depth));
                let inner =
                    if continuous { "percentile_cont_final" } else { "percentile_disc_final" };
                (inner, vec![value, fraction], nested(value_type, depth))
            }
            Kind::Mode => {
                let returns = self.plan().expr_type(keys[0]).clone();
                ("mode_final", vec![keys[0]], returns)
            }
            Kind::Hypothetical(inner, returns) => {
                // Each direct argument takes the type it shares with its key, as
                // `unify_hypothetical_args` makes it, and the key is cast to it too.
                let mut args = keys.clone();
                args.extend_from_slice(&bound);
                for (at, item) in sorted.iter().enumerate() {
                    let mut pair = [keys[at], bound[at]];
                    self.common_type(
                        ast,
                        &[item.expr, direct[at]],
                        &mut pair,
                        Some("WITHIN GROUP"),
                    )?;
                    let ty = self.plan().expr_type(pair[0]).clone();
                    args[at] = pair[0];
                    args[keys.len() + at] = self.checked_cast_to(pair[1], &ty, false)?;
                }
                (inner, args, returns)
            }
        };

        // The direct arguments are computed with the rows, which is only one value for a group
        // when they read nothing but grouped columns.
        for &arg in &bound {
            self.over_aggregate(arg, scope).map_err(|error| match error.sqlstate() {
                Some(SqlState::GROUPING_ERROR) => error.detail(
                    "Direct arguments of an ordered-set aggregate must use only grouped columns.",
                ),
                _ => error,
            })?;
        }
        let name = rudb_kernels::ordered_name(inner, args.len(), &order);
        self.aggregate_call(&name, &args, false, filter, returns)
    }
}

/// Whether a value is one `percentile_cont` reads as a `double precision`.
fn numeric(ty: &LogicalType) -> bool {
    ty.is_numeric() || matches!(ty, LogicalType::Numeric | LogicalType::Null)
}

/// How many arrays deep a fraction is, or `None` for a fraction that is not a number.
fn fraction_depth(ty: &LogicalType) -> Option<usize> {
    match ty {
        LogicalType::List(element) => fraction_depth(element).map(|depth| depth + 1),
        LogicalType::Varchar => Some(0),
        number if numeric(number) => Some(0),
        _ => None,
    }
}

/// `ty` inside `depth` arrays.
fn nested(ty: LogicalType, depth: usize) -> LogicalType {
    (0..depth).fold(ty, |inner, _| LogicalType::List(Box::new(inner)))
}

/// PostgreSQL's error for an ordered-set aggregate it finds and then refuses for how its arguments
/// are split, which a hint explains in place of a detail.
fn missing(
    ast: &Ast,
    call: ast::ExprRef,
    written: &str,
    arguments: &[ast::ExprRef],
    types: &[LogicalType],
) -> Error {
    let spelled = types
        .iter()
        .zip(arguments)
        .map(|(ty, &arg)| postgres_type_name(ast, arg, ty))
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!("function {written}({spelled}) does not exist");
    Error::binder(message).state(SqlState::UNDEFINED_FUNCTION).with_span(ast.expr_span(call))
}

/// PostgreSQL's error for `WITHIN GROUP` on a function that is not an ordered-set aggregate.
fn not_ordered_set(
    ast: &Ast,
    call: ast::ExprRef,
    written: &str,
    arguments: &[ast::ExprRef],
    types: &[LogicalType],
) -> Error {
    let refused = |message: String| Error::binder(message).state(SqlState::WRONG_OBJECT_TYPE);
    // PostgreSQL looks the function up over the direct arguments and the keys together, and only
    // a function it finds is refused for being of the wrong kind.
    let kind = kind_of(written).filter(|_| resolve(written, types).is_ok());
    match kind {
        Some(FunctionKind::Aggregate) => refused(format!(
            "{written} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"
        )),
        Some(FunctionKind::Window) => {
            refused(format!("window function {written} cannot have WITHIN GROUP"))
        }
        Some(_) => {
            refused(format!("WITHIN GROUP specified, but {written} is not an aggregate function"))
        }
        _ => undefined(ast, call, written, arguments, types),
    }
}

/// PostgreSQL's error for a function it does not find over these arguments, with the detail of
/// `func_lookup_failure_details`.
fn undefined(
    ast: &Ast,
    call: ast::ExprRef,
    written: &str,
    arguments: &[ast::ExprRef],
    types: &[LogicalType],
) -> Error {
    let error = Error::binder(format!("No function matches the given name: {written}"))
        .with_span(ast.expr_span(call));
    undefined_function(ast, error, written, arguments, types)
}
