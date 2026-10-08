//! The built-in function that a call names, as `func_get_detail` of
//! `src/backend/parser/parse_func.c` finds it for a call with its arguments by position or by
//! name.
//!
//! The candidates are the rows of `pg_proc` with the name that take the arguments, as
//! `FuncnameGetCandidates` of `src/backend/catalog/namespace.c` lists them: a variadic function
//! takes its fixed arguments and any number of values after them, a function with defaults takes
//! fewer arguments, and a call with named arguments takes the functions whose argument names
//! match, as `MatchNamedCall` says. An exact match wins. Otherwise the candidates are the ones
//! that every argument casts to with no cast written, and `func_select_candidate` chooses one of
//! them by the number of exact matches, the preferred types and the categories of the arguments
//! of type `unknown`.
//!
//! A candidate with a polymorphic argument takes the call when the actual types agree, as
//! `check_generic_type_consistency` of `src/backend/parser/parse_coerce.c` says. The actual type
//! of the result is for the caller to find.

use crate::coerce::{
    CoercionContext, CoercionPath, can_coerce_implicitly, common_type, find_coercion_pathway,
    is_preferred,
};
use crate::oid;
use crate::procs::{Proc, procs};
use crate::types::{Oid, TypeInfo};

/// A call as `func_get_detail` reads it.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    /// The types of the arguments in the order of the call. An argument of type `unknown` is a
    /// string literal or a null with no type.
    pub args: &'a [Oid],
    /// The names of the last arguments, for a call with named arguments.
    pub names: &'a [&'a str],
    /// The call was written with `VARIADIC` before its last argument, so a variadic function
    /// takes that argument as its array and not as one of its values.
    pub variadic: bool,
}

/// A function that a call can call, with the types that it takes in the order of the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub proc: &'static Proc,
    /// The types of the arguments: the declared types, with the element type of a variadic
    /// argument once for each value that it takes. The types of the defaults are at the end.
    pub args: Vec<Oid>,
    /// For each of `args`, the declared argument of the function that it is. A call by position
    /// takes the arguments in order, and a call with named arguments takes them in the order of
    /// the call with the defaults that it does not give after them, in their order.
    pub order: Vec<usize>,
    /// The number of values that the variadic argument takes, or 0.
    pub variadic: usize,
    /// The number of arguments that take their default values.
    pub defaults: usize,
    /// Another function has the same arguments after the variadic and default rules, so the
    /// call is not unique.
    ambiguous: bool,
}

impl Candidate {
    /// The default of the declared argument `at`, as the text that the input function of its
    /// type reads.
    pub fn default_of(&self, at: usize) -> Option<&'static str> {
        let first = self.proc.args.len() - self.proc.defaults.len();
        at.checked_sub(first).and_then(|index| self.proc.defaults.get(index).copied())
    }
}

/// Why no function takes a call, which selects the detail of the error as
/// `func_lookup_failure_details` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No built-in function has the name.
    Name,
    /// No function with the name takes the number of arguments.
    Count,
    /// No function with the name has the names of the named arguments.
    Names,
    /// The closest match has an argument by position and by name.
    Duplicate,
    /// The closest match does not get each argument that has no default.
    Missing,
    /// The named arguments match a variadic function, which a call with named arguments can call
    /// only with `VARIADIC` before its array.
    Variadic,
    /// No function with the name takes the types of the arguments.
    Types,
}

impl Failure {
    /// The detail of `42883` for this failure, if it has one.
    pub fn detail(self) -> Option<&'static str> {
        Some(match self {
            Self::Name => "There is no function of that name.",
            Self::Count => "No function of that name accepts the given number of arguments.",
            Self::Names => "No function of that name accepts the given argument names.",
            Self::Duplicate => {
                "In the closest available match, an argument was specified both positionally and \
                 by name."
            }
            Self::Missing => {
                "In the closest available match, not all required arguments were supplied."
            }
            Self::Variadic => return None,
            Self::Types => "No function of that name accepts the given argument types.",
        })
    }

    /// The hint of `42883` for this failure, if it has one.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Self::Variadic => Some(
                "This call would be correct if the variadic array were labeled VARIADIC and \
                 placed last.",
            ),
            Self::Types => Some("You might need to add explicit type casts."),
            _ => None,
        }
    }
}

/// What [`resolve_function`] finds for a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The one function that the call calls.
    Found(Candidate),
    /// No function takes the call.
    NotFound(Failure),
    /// More than one function takes the call and the rules cannot choose one, which is
    /// `42725`.
    Ambiguous,
}

/// The function that a call of `name` with arguments of the types `args` by position calls.
pub fn resolve_function(name: &str, args: &[Oid]) -> Resolution {
    resolve_call(name, Call { args, names: &[], variadic: false })
}

/// The function that `call` of `name` calls.
pub fn resolve_call(name: &str, call: Call<'_>) -> Resolution {
    let all = procs(name);
    if all.is_empty() {
        return Resolution::NotFound(Failure::Name);
    }
    let args = call.args;
    let (candidates, seen) = candidates(all, call);
    if candidates.is_empty() {
        return Resolution::NotFound(seen.failure(!call.names.is_empty()));
    }
    let count = args.len();
    let exact = candidates.iter().find(|candidate| count == 0 || candidate.args[..count] == *args);
    let best = match exact {
        Some(exact) => exact.clone(),
        None => {
            let mut matching: Vec<Candidate> = candidates
                .iter()
                .filter(|candidate| can_coerce(args, &candidate.args[..count]))
                .cloned()
                .collect();
            match matching.len() {
                0 => return Resolution::NotFound(Failure::Types),
                1 => matching.remove(0),
                _ => match select_candidate(args, matching) {
                    Some(best) => best,
                    None => return Resolution::Ambiguous,
                },
            }
        }
    };
    if best.ambiguous {
        return Resolution::Ambiguous;
    }
    // `VARIADIC` with named arguments needs the last argument to be the variadic one.
    if !call.names.is_empty() && call.variadic && best.order.get(count - 1) != Some(&(count - 1)) {
        return Resolution::NotFound(Failure::Types);
    }
    Resolution::Found(best)
}

/// What the search for candidates saw, as the flags `FGC_ARGCOUNT_MATCH` and the others of
/// `FuncnameGetCandidates`.
#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    /// A function takes the number of arguments.
    count: bool,
    /// A function has every name of the call.
    names: bool,
    /// A function has every name of the call and none of them is an argument by position.
    apart: bool,
    /// A function has every name, none by position, and gets each argument with no default.
    all: bool,
    /// A function takes the named arguments.
    valid: bool,
    /// A variadic function takes the named arguments, which a call without `VARIADIC` cannot do.
    variadic: bool,
}

impl Seen {
    /// The failure to report when no function is a candidate.
    fn failure(self, named: bool) -> Failure {
        if !self.count {
            return Failure::Count;
        }
        if named && !self.valid {
            return match (self.names, self.apart, self.all) {
                (false, _, _) => Failure::Names,
                (true, false, _) => Failure::Duplicate,
                (true, true, false) => Failure::Missing,
                (true, true, true) => Failure::Types,
            };
        }
        match self.variadic {
            true => Failure::Variadic,
            false => Failure::Types,
        }
    }
}

/// The functions that `call` can call, as `FuncnameGetCandidates` lists them. Two functions with
/// the same arguments after the variadic and default rules are one candidate: a function that is
/// not variadic wins over one that is, and otherwise the candidate is ambiguous.
fn candidates(all: &'static [Proc], call: Call<'_>) -> (Vec<Candidate>, Seen) {
    let count = call.args.len();
    let mut seen = Seen::default();
    let mut found: Vec<Candidate> = Vec::new();
    let mut special = false;
    for proc in all {
        let declared = proc.args.len();
        let defaulted = declared > count;
        if defaulted && count + proc.defaults.len() < declared {
            continue;
        }
        let (variadic, order) = if call.names.is_empty() {
            let variadic = !call.variadic && declared <= count && proc.variadic != 0;
            if declared != count && !variadic && !defaulted {
                continue;
            }
            seen.count = true;
            (variadic, (0..declared.max(count)).collect())
        } else {
            if declared != count && !defaulted {
                continue;
            }
            seen.count = true;
            let Some(order) = match_named_call(proc, count, call.names, &mut seen) else {
                continue;
            };
            if proc.variadic != 0 && !call.variadic {
                seen.variadic = true;
                continue;
            }
            (false, order)
        };
        special |= variadic || defaulted || !call.names.is_empty();
        let args = match variadic {
            true => {
                let mut args = proc.args[..declared - 1].to_vec();
                args.resize(count, proc.variadic);
                args
            }
            false => order.iter().map(|&at: &usize| proc.args[at]).collect(),
        };
        let candidate = Candidate {
            proc,
            variadic: if variadic { count + 1 - declared } else { 0 },
            defaults: if defaulted { declared - count } else { 0 },
            order: match variadic {
                true => (0..count).map(|at| at.min(declared - 1)).collect(),
                false => order,
            },
            args,
            ambiguous: false,
        };
        // The rows of one name with the same arguments differ only by the special rules, since
        // every built-in function is in `pg_catalog`.
        let compared = candidate.args.len() - candidate.defaults;
        let same = special
            .then(|| {
                found.iter().position(|held| {
                    held.args.len() - held.defaults == compared
                        && held.args[..compared] == candidate.args[..compared]
                })
            })
            .flatten();
        match same {
            None => found.push(candidate),
            Some(at) => match (variadic, found[at].variadic > 0) {
                (true, false) => {}
                (false, true) => {
                    found.remove(at);
                    found.push(candidate);
                }
                _ => found[at].ambiguous = true,
            },
        }
    }
    (found, seen)
}

/// `MatchNamedCall`: the declared argument of each argument of a call with `count` arguments
/// whose last ones have the names `names`, followed by the arguments with defaults that the call
/// does not give, or `None` when the names do not fit the function.
fn match_named_call(
    proc: &Proc,
    count: usize,
    names: &[&str],
    seen: &mut Seen,
) -> Option<Vec<usize>> {
    let declared = proc.args.len();
    let positional = count - names.len();
    let mut given = vec![false; declared];
    let mut order: Vec<usize> = (0..positional).collect();
    given[..positional].fill(true);
    let mut apart = true;
    for name in names {
        let at = proc.names.iter().position(|held| !held.is_empty() && held == name)?;
        if at >= declared {
            return None;
        }
        apart &= !given[at];
        given[at] = true;
        order.push(at);
    }
    seen.names = true;
    if !apart {
        return None;
    }
    seen.apart = true;
    let first_default = declared - proc.defaults.len();
    for (at, &held) in given.iter().enumerate().skip(positional) {
        if held {
            continue;
        }
        if at < first_default {
            return None;
        }
        order.push(at);
    }
    seen.all = true;
    seen.valid = true;
    Some(order)
}

/// Whether each argument casts to the type of the candidate with no cast written, as
/// `can_coerce_type` with `COERCION_IMPLICIT` says.
fn can_coerce(inputs: &[Oid], targets: &[Oid]) -> bool {
    let mut generic = false;
    for (&input, &target) in inputs.iter().zip(targets) {
        if input == target {
            continue;
        }
        if input == oid::INTERNAL || target == oid::INTERNAL {
            return false;
        }
        if target == oid::ANY {
            continue;
        }
        if is_polymorphic(target) {
            generic = true;
            continue;
        }
        if input == oid::UNKNOWN
            || find_coercion_pathway(input, target, CoercionContext::Implicit) != CoercionPath::None
        {
            continue;
        }
        let composite = |oid: Oid| TypeInfo::get(oid).is_some_and(|info| info.kind == b'c');
        let composite_array = |oid: Oid| element_type(oid).is_some_and(composite);
        if (input == oid::RECORD && composite(target))
            || (target == oid::RECORD && composite(input))
            || (target == oid::RECORD_ARRAY && composite_array(input))
        {
            continue;
        }
        return false;
    }
    !generic || generic_types_agree(inputs, targets)
}

/// The rows of `pg_range`: a range type, the type of its bounds and its multirange type.
pub(crate) const RANGES: [(Oid, Oid, Oid); 6] = [
    (oid::INT4RANGE, oid::INT4, oid::INT4MULTIRANGE),
    (oid::NUMRANGE, oid::NUMERIC, oid::NUMMULTIRANGE),
    (oid::TSRANGE, oid::TIMESTAMP, oid::TSMULTIRANGE),
    (oid::TSTZRANGE, oid::TIMESTAMPTZ, oid::TSTZMULTIRANGE),
    (oid::DATERANGE, oid::DATE, oid::DATEMULTIRANGE),
    (oid::INT8RANGE, oid::INT8, oid::INT8MULTIRANGE),
];

/// `get_element_type`: the element type of an array type.
pub(crate) fn element_type(array: Oid) -> Option<Oid> {
    TypeInfo::get(array)
        .filter(|info| info.category == b'A' && info.elem != 0)
        .map(|info| info.elem)
}

/// `get_range_subtype`: the type of the bounds of a range type.
pub(crate) fn range_subtype(range: Oid) -> Option<Oid> {
    RANGES.iter().find(|row| row.0 == range).map(|row| row.1)
}

/// `get_multirange_range`: the range type of a multirange type.
pub(crate) fn multirange_range(multirange: Oid) -> Option<Oid> {
    RANGES.iter().find(|row| row.2 == multirange).map(|row| row.0)
}

/// `check_generic_type_consistency`: whether the actual types of the polymorphic arguments agree.
/// An argument of type `unknown` agrees with any type. No built-in type is an enum.
fn generic_types_agree(inputs: &[Oid], targets: &[Oid]) -> bool {
    // One actual type for each family, which every argument of the family must have.
    fn hold(slot: &mut Option<Oid>, actual: Oid) -> bool {
        match *slot {
            Some(held) => held == actual,
            None => {
                *slot = Some(actual);
                true
            }
        }
    }
    let mut element = None;
    let mut array = None;
    let mut range = None;
    let mut multirange = None;
    let mut compatible_range = None;
    let mut compatible_multirange = None;
    let mut compatible = Vec::new();
    let (mut nonarray, mut anyenum, mut compatible_nonarray) = (false, false, false);
    for (&actual, &declared) in inputs.iter().zip(targets) {
        nonarray |= declared == oid::ANYNONARRAY;
        anyenum |= declared == oid::ANYENUM;
        compatible_nonarray |= declared == oid::ANYCOMPATIBLENONARRAY;
        if actual == oid::UNKNOWN {
            continue;
        }
        let agrees = match declared {
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM => hold(&mut element, actual),
            oid::ANYARRAY => hold(&mut array, actual),
            oid::ANYRANGE => hold(&mut range, actual),
            oid::ANYMULTIRANGE => hold(&mut multirange, actual),
            oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => {
                compatible.push(actual);
                true
            }
            oid::ANYCOMPATIBLEARRAY => match element_type(actual) {
                Some(element) => {
                    compatible.push(element);
                    true
                }
                None => false,
            },
            oid::ANYCOMPATIBLERANGE => match compatible_range {
                Some(held) => held == actual,
                None => match range_subtype(actual) {
                    Some(subtype) => {
                        compatible_range = Some(actual);
                        compatible.push(subtype);
                        true
                    }
                    None => false,
                },
            },
            oid::ANYCOMPATIBLEMULTIRANGE => match compatible_multirange {
                Some((held, _)) => held == actual,
                None => match multirange_range(actual) {
                    Some(range) => {
                        compatible_multirange = Some((actual, range));
                        true
                    }
                    None => false,
                },
            },
            _ => true,
        };
        if !agrees {
            return false;
        }
    }
    // An `anyarray` of type `anyarray` agrees for now, as PostgreSQL allows it.
    if let Some(array) = array.filter(|&array| array != oid::ANYARRAY) {
        let Some(from_array) = element_type(array) else { return false };
        if !hold(&mut element, from_array) {
            return false;
        }
    }
    if let Some(multirange) = multirange {
        let Some(from_multirange) = multirange_range(multirange) else { return false };
        if !hold(&mut range, from_multirange) {
            return false;
        }
    }
    if let Some(range) = range {
        let Some(subtype) = range_subtype(range) else { return false };
        if !hold(&mut element, subtype) {
            return false;
        }
    }
    let is_array = |oid: Option<Oid>| oid.is_some_and(|oid| element_type(oid).is_some());
    if nonarray && is_array(element) {
        return false;
    }
    if anyenum {
        return false;
    }
    let mut subtype = compatible_range.and_then(range_subtype);
    if let Some((_, range)) = compatible_multirange {
        match compatible_range {
            Some(held) if held != range => return false,
            Some(_) => {}
            None => {
                let Some(from_range) = range_subtype(range) else { return false };
                subtype = Some(from_range);
                compatible.push(from_range);
            }
        }
    }
    if !compatible.is_empty() {
        let known: Vec<Option<Oid>> = compatible.iter().copied().map(Some).collect();
        let Ok(common) = common_type(&known) else { return false };
        if !compatible
            .iter()
            .all(|&actual| actual == common || can_coerce_implicitly(actual, common))
        {
            return false;
        }
        if compatible_nonarray && element_type(common).is_some() {
            return false;
        }
        if subtype.is_some_and(|subtype| subtype != common) {
            return false;
        }
    }
    true
}

/// Whether the type is one of the polymorphic pseudo-types, whose actual type comes from the
/// arguments.
pub fn is_polymorphic(oid: Oid) -> bool {
    matches!(
        oid,
        oid::ANYELEMENT
            | oid::ANYARRAY
            | oid::ANYNONARRAY
            | oid::ANYENUM
            | oid::ANYRANGE
            | oid::ANYMULTIRANGE
            | oid::ANYCOMPATIBLE
            | oid::ANYCOMPATIBLEARRAY
            | oid::ANYCOMPATIBLENONARRAY
            | oid::ANYCOMPATIBLERANGE
            | oid::ANYCOMPATIBLEMULTIRANGE
    )
}

/// `typcategory` and `typispreferred`, with the category `\0` of `TYPCATEGORY_INVALID` for a type
/// that is not built in.
fn category(oid: Oid) -> (u8, bool) {
    match TypeInfo::get(oid) {
        Some(info) => (info.category, is_preferred(oid)),
        None => (0, false),
    }
}

/// `IsPreferredType`: the type is preferred in the category, or preferred at all when the
/// category is not known.
fn preferred_in(slot: u8, oid: Oid) -> bool {
    let (category, preferred) = category(oid);
    (slot == category || slot == 0) && preferred
}

/// The candidates with the most positions for which `matches` holds, which is all of them when
/// none has one.
fn most(candidates: Vec<Candidate>, matches: impl Fn(&Candidate) -> usize) -> Vec<Candidate> {
    let best = candidates.iter().map(&matches).max().unwrap_or(0);
    candidates.into_iter().filter(|candidate| matches(candidate) == best).collect()
}

/// `func_select_candidate`: the one of more than one candidate that the heuristics choose, or
/// `None` when they cannot choose.
fn select_candidate(args: &[Oid], candidates: Vec<Candidate>) -> Option<Candidate> {
    let known = |at: usize| args[at] != oid::UNKNOWN;
    let positions = 0..args.len();
    let exact = |candidate: &Candidate| {
        positions.clone().filter(|&at| known(at) && candidate.args[at] == args[at]).count()
    };
    let mut candidates = most(candidates, exact);
    if candidates.len() == 1 {
        return candidates.pop();
    }
    let slots: Vec<u8> = args.iter().map(|&arg| category(arg).0).collect();
    let near = |candidate: &Candidate| {
        positions
            .clone()
            .filter(|&at| {
                known(at)
                    && (candidate.args[at] == args[at]
                        || preferred_in(slots[at], candidate.args[at]))
            })
            .count()
    };
    candidates = most(candidates, near);
    if candidates.len() == 1 {
        return candidates.pop();
    }
    let unknowns = positions.clone().filter(|&at| !known(at)).count();
    if unknowns == 0 {
        return None;
    }
    // The category of each unknown argument: `S` when a candidate takes a string type there, the
    // one category of all the candidates when they agree, and otherwise none.
    let mut resolved = Vec::new();
    for at in positions.clone().filter(|&at| !known(at)) {
        let mut slot: Option<(u8, bool)> = None;
        let mut conflict = false;
        for candidate in &candidates {
            let (category, preferred) = category(candidate.args[at]);
            slot = match slot {
                None => Some((category, preferred)),
                Some((held, any)) if held == category => Some((held, any || preferred)),
                Some(_) if category == b'S' => Some((category, preferred)),
                Some(held) => {
                    conflict = true;
                    Some(held)
                }
            };
        }
        match slot {
            Some((category, preferred)) if !conflict || category == b'S' => {
                resolved.push((at, category, preferred));
            }
            _ => {
                resolved.clear();
                break;
            }
        }
    }
    if !resolved.is_empty() {
        let kept: Vec<Candidate> = candidates
            .iter()
            .filter(|candidate| {
                resolved.iter().all(|&(at, slot, any)| {
                    let (category, preferred) = category(candidate.args[at]);
                    category == slot && (!any || preferred)
                })
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            candidates = kept;
        }
        if candidates.len() == 1 {
            return candidates.pop();
        }
    }
    // The last rule: when the known arguments all have one type, the unknown ones are taken to
    // have it too.
    if unknowns < args.len() {
        let mut types = args.iter().copied().filter(|&arg| arg != oid::UNKNOWN);
        let first = types.next()?;
        if types.all(|arg| arg == first) {
            let assumed = vec![first; args.len()];
            let mut unique = None;
            for candidate in &candidates {
                if can_coerce(&assumed, &candidate.args[..args.len()]) {
                    if unique.is_some() {
                        return None;
                    }
                    unique = Some(candidate.clone());
                }
            }
            return unique;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(name: &str, args: &[Oid]) -> (Oid, Vec<Oid>) {
        match resolve_function(name, args) {
            Resolution::Found(candidate) => (candidate.proc.result, candidate.proc.args.to_vec()),
            other => panic!("{name}{args:?} is {other:?}"),
        }
    }

    #[test]
    fn a_call_finds_the_function_that_postgresql_finds() {
        use oid::*;
        // An exact match, and an integer that goes to the preferred type of its category.
        assert_eq!(found("abs", &[INT2]), (INT2, vec![INT2]));
        assert_eq!(found("sqrt", &[INT4]), (FLOAT8, vec![FLOAT8]));
        assert_eq!(found("sqrt", &[NUMERIC]), (NUMERIC, vec![NUMERIC]));
        // An unknown argument takes the preferred type of the one category of the candidates.
        assert_eq!(found("abs", &[UNKNOWN]), (FLOAT8, vec![FLOAT8]));
        // Only `numeric` has a scale.
        assert_eq!(found("round", &[INT4, INT4]), (NUMERIC, vec![NUMERIC, INT4]));
        assert_eq!(found("round", &[INT4]), (FLOAT8, vec![FLOAT8]));
        assert_eq!(found("mod", &[INT2, INT4]), (INT4, vec![INT4, INT4]));
        assert_eq!(found("upper", &[UNKNOWN]), (TEXT, vec![TEXT]));
        // A variadic function takes the values after its fixed arguments.
        assert!(matches!(
            resolve_function("concat", &[INT4, UNKNOWN, TEXT]),
            Resolution::Found(Candidate { variadic: 3, .. })
        ));
        assert!(matches!(
            resolve_function("make_interval", &[INT4]),
            Resolution::Found(Candidate { defaults: 6, .. })
        ));
        assert_eq!(resolve_function("nosuch", &[]), Resolution::NotFound(Failure::Name));
        assert_eq!(resolve_function("abs", &[INT4, INT4]), Resolution::NotFound(Failure::Count));
        assert_eq!(
            resolve_function("round", &[FLOAT8, INT4]),
            Resolution::NotFound(Failure::Types)
        );
        assert_eq!(resolve_function("upper", &[INT4]), Resolution::NotFound(Failure::Types));
        // The polymorphic arguments must agree.
        assert_eq!(
            found("width_bucket", &[INT4, INT4_ARRAY]),
            (INT4, vec![ANYCOMPATIBLE, ANYCOMPATIBLEARRAY])
        );
        assert_eq!(
            found("array_append", &[INT4_ARRAY, UNKNOWN]).1,
            vec![ANYCOMPATIBLEARRAY, ANYCOMPATIBLE]
        );
        assert_eq!(
            resolve_function("array_append", &[INT4_ARRAY, TEXT]),
            Resolution::NotFound(Failure::Types)
        );
        assert_eq!(found("lower", &[INT4RANGE]).1, vec![ANYRANGE]);
    }

    fn named(name: &str, args: &[Oid], names: &[&str], variadic: bool) -> Resolution {
        resolve_call(name, Call { args, names, variadic })
    }

    #[test]
    fn named_arguments_and_variadic_find_the_function_that_postgresql_finds() {
        use oid::*;
        // `make_interval(1, days => 3)` gives `years` by position and `days` by name, and the
        // other five take their defaults in their order.
        let Resolution::Found(interval) = named("make_interval", &[INT4, INT4], &["days"], false)
        else {
            panic!("make_interval(integer, days => integer) is not found");
        };
        assert_eq!(interval.order, [0, 3, 1, 2, 4, 5, 6]);
        assert_eq!((interval.defaults, interval.default_of(6)), (5, Some("0.0")));
        assert_eq!(interval.args[..2], [INT4, INT4]);
        for (args, names, failure) in [
            (&[INT4][..], &["nope"][..], Failure::Names),
            (&[INT4, INT4], &["years"], Failure::Duplicate),
            (&[UNKNOWN, UNKNOWN], &["path", "vars"], Failure::Missing),
        ] {
            let function = match failure {
                Failure::Missing => "jsonb_path_query_first",
                _ => "make_interval",
            };
            assert_eq!(named(function, args, names, false), Resolution::NotFound(failure));
        }
        assert_eq!(
            named("make_date", &[INT4; 4], &["year", "month", "day", "x"], false),
            Resolution::NotFound(Failure::Count)
        );
        // A variadic function takes named arguments only with `VARIADIC` before its array.
        let path = ["from_json", "path_elems"];
        assert_eq!(
            named("jsonb_extract_path_text", &[UNKNOWN, TEXT_ARRAY], &path, false),
            Resolution::NotFound(Failure::Variadic)
        );
        assert!(matches!(
            named("jsonb_extract_path_text", &[UNKNOWN, TEXT_ARRAY], &path, true),
            Resolution::Found(_)
        ));
        // With `VARIADIC` the array is the argument of the variadic function as it is declared.
        assert!(matches!(
            named("concat", &[INT4_ARRAY], &[], true),
            Resolution::Found(Candidate { variadic: 0, .. })
        ));
        assert_eq!(
            named("concat", &[UNKNOWN, TEXT_ARRAY], &[], true),
            Resolution::NotFound(Failure::Count)
        );
        assert_eq!(named("concat", &[INT4], &["x"], false), Resolution::NotFound(Failure::Names));
    }
}
