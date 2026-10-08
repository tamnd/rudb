//! The actual types of the polymorphic arguments and of the result of a call, as
//! `enforce_generic_type_consistency` of `src/backend/parser/parse_coerce.c` finds them.
//!
//! [`resolve_call`](crate::resolve_call) chooses a function when the actual types of its
//! polymorphic arguments agree. This module then gives each polymorphic argument and the result
//! its actual type, with the errors of PostgreSQL. The `anyelement` family takes the one type of
//! its arguments, and the `anycompatible` family takes the common type of its arguments, to which
//! each of them is cast. An argument of type `unknown` takes the type of its family.
//!
//! PostgreSQL leaves the declared type of a polymorphic argument of the `anyelement` family with a
//! known type as it is, because its value needs no cast. Here that argument gets its actual type,
//! so that the caller has a type for each argument. No type is a domain or an enum here, so the
//! rules for those are not needed.

use rudb_common::SqlState;

use crate::coerce::{can_coerce_implicitly, common_type};
use crate::error::TypeError;
use crate::oid;
use crate::resolve::{RANGES, element_type, multirange_range, range_subtype};
use crate::types::{Oid, TypeInfo, format_type};

/// The actual types of a call of a function with polymorphic types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generic {
    /// The type of each argument, with each polymorphic type replaced by its actual type.
    pub args: Vec<Oid>,
    /// The type of the result.
    pub result: Oid,
}

/// The actual types of the arguments and of the result of a call whose arguments have the types
/// `actual`, of a function with the argument types `declared` and the result type `result`.
pub fn enforce_generic_types(
    actual: &[Oid],
    declared: &[Oid],
    result: Oid,
) -> Result<Generic, TypeError> {
    let mut held = Held::new(result);
    for (&actual, &declared) in actual.iter().zip(declared) {
        held.take(actual, declared)?;
    }
    if held.family_one == 0 && !held.family_two {
        return Ok(Generic { args: declared.to_vec(), result });
    }
    if held.family_one > 0 {
        held.family_one_types(result)?;
    }
    if held.family_two {
        held.family_two_types()?;
    }
    let args = declared.iter().map(|&declared| held.actual(declared)).collect::<Result<_, _>>()?;
    Ok(Generic { args, result: held.actual(result)? })
}

/// What the arguments say about each polymorphic type.
#[derive(Default)]
struct Held {
    element: Option<Oid>,
    array: Option<Oid>,
    range: Option<Oid>,
    multirange: Option<Oid>,
    nonarray: bool,
    anyenum: bool,
    wants_multirange: bool,
    /// The number of arguments of the `anyelement` family, with the ones of type `unknown`.
    family_one: usize,
    family_two: bool,
    /// The types that the common type of the `anycompatible` family is chosen from.
    compatible_types: Vec<Oid>,
    compatible: Option<Oid>,
    compatible_array: Option<Oid>,
    compatible_range: Option<Oid>,
    compatible_subtype: Option<Oid>,
    compatible_multirange: Option<Oid>,
    compatible_multirange_range: Option<Oid>,
    compatible_nonarray: bool,
    wants_compatible_array: bool,
    wants_compatible_range: bool,
    wants_compatible_multirange: bool,
}

impl Held {
    fn new(result: Oid) -> Held {
        Held {
            nonarray: result == oid::ANYNONARRAY,
            anyenum: result == oid::ANYENUM,
            wants_multirange: result == oid::ANYMULTIRANGE,
            compatible_nonarray: result == oid::ANYCOMPATIBLENONARRAY,
            wants_compatible_array: result == oid::ANYCOMPATIBLEARRAY,
            wants_compatible_range: result == oid::ANYCOMPATIBLERANGE,
            wants_compatible_multirange: result == oid::ANYCOMPATIBLEMULTIRANGE,
            ..Held::default()
        }
    }

    /// Reads one argument of the actual type `actual` and the declared type `declared`.
    fn take(&mut self, actual: Oid, declared: Oid) -> Result<(), TypeError> {
        let unknown = actual == oid::UNKNOWN;
        match declared {
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM => {
                self.family_one += 1;
                self.nonarray |= declared == oid::ANYNONARRAY;
                self.anyenum |= declared == oid::ANYENUM;
                if !unknown {
                    hold(&mut self.element, actual, "anyelement")?;
                }
            }
            oid::ANYARRAY | oid::ANYRANGE | oid::ANYMULTIRANGE => {
                self.family_one += 1;
                self.wants_multirange |= declared == oid::ANYMULTIRANGE;
                let (slot, family) = match declared {
                    oid::ANYARRAY => (&mut self.array, "anyarray"),
                    oid::ANYRANGE => (&mut self.range, "anyrange"),
                    _ => (&mut self.multirange, "anymultirange"),
                };
                if !unknown {
                    hold(slot, actual, family)?;
                }
            }
            oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => {
                self.family_two = true;
                self.compatible_nonarray |= declared == oid::ANYCOMPATIBLENONARRAY;
                if !unknown {
                    self.compatible_types.push(actual);
                }
            }
            oid::ANYCOMPATIBLEARRAY => {
                self.family_two = true;
                self.wants_compatible_array = true;
                if !unknown {
                    let element = element_type(actual)
                        .ok_or_else(|| not_a("anycompatiblearray", "an array", actual))?;
                    self.compatible_types.push(element);
                }
            }
            oid::ANYCOMPATIBLERANGE => {
                self.family_two = true;
                self.wants_compatible_range = true;
                match self.compatible_range {
                    _ if unknown => {}
                    Some(held) if held != actual => {
                        return Err(not_alike("anycompatiblerange", held, actual));
                    }
                    Some(_) => {}
                    None => {
                        let subtype = range_subtype(actual)
                            .ok_or_else(|| not_a("anycompatiblerange", "a range type", actual))?;
                        self.compatible_range = Some(actual);
                        self.compatible_subtype = Some(subtype);
                        self.compatible_types.push(subtype);
                    }
                }
            }
            oid::ANYCOMPATIBLEMULTIRANGE => {
                self.family_two = true;
                self.wants_compatible_multirange = true;
                match self.compatible_multirange {
                    _ if unknown => {}
                    Some(held) if held != actual => {
                        return Err(not_alike("anycompatiblemultirange", held, actual));
                    }
                    Some(_) => {}
                    None => {
                        let range = multirange_range(actual).ok_or_else(|| {
                            not_a("anycompatiblemultirange", "a multirange type", actual)
                        })?;
                        self.compatible_multirange = Some(actual);
                        self.compatible_multirange_range = Some(range);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The element type of the `anyelement` family from its arrays and its ranges.
    fn family_one_types(&mut self, result: Oid) -> Result<(), TypeError> {
        if let Some(array) = self.array {
            let from_array = match array {
                // An actual `anyarray` comes from a column of `pg_statistic`, and says nothing of
                // its elements.
                oid::ANYARRAY => {
                    let infers = result != oid::ANYARRAY && family_one(result);
                    if self.family_one != 1 || infers {
                        return Err(mismatch(
                            "cannot determine element type of \"anyarray\" argument".into(),
                        ));
                    }
                    oid::ANYELEMENT
                }
                _ => element_type(array).ok_or_else(|| not_a("anyarray", "an array", array))?,
            };
            agree(&mut self.element, from_array, ("anyarray", array), "anyelement")?;
        }
        if let Some(multirange) = self.multirange {
            let range = multirange_range(multirange)
                .ok_or_else(|| not_a("anymultirange", "a multirange type", multirange))?;
            agree(&mut self.range, range, ("anymultirange", multirange), "anyrange")?;
        } else if self.wants_multirange {
            self.multirange = self.range.and_then(range_multirange);
        }
        if let Some(range) = self.range {
            let subtype =
                range_subtype(range).ok_or_else(|| not_a("anyrange", "a range type", range))?;
            agree(&mut self.element, subtype, ("anyrange", range), "anyelement")?;
        }
        // Only arguments of type `unknown` leave the element type open.
        let Some(element) = self.element else { return Err(undetermined(None)) };
        if self.nonarray && element_type(element).is_some() {
            let name = format_type(element);
            return Err(mismatch(format!("type matched to anynonarray is an array type: {name}")));
        }
        if self.anyenum {
            let name = format_type(element);
            return Err(mismatch(format!("type matched to anyenum is not an enum type: {name}")));
        }
        Ok(())
    }

    /// The common type of the `anycompatible` family, and its array and its ranges.
    fn family_two_types(&mut self) -> Result<(), TypeError> {
        if let Some(multirange) = self.compatible_multirange {
            match (self.compatible_range, self.compatible_multirange_range) {
                (Some(range), Some(from_multirange)) if range != from_multirange => {
                    return Err(not_consistent(
                        ("anycompatiblemultirange", multirange),
                        ("anycompatiblerange", range),
                    ));
                }
                (Some(_), _) => {}
                (None, from_multirange) => {
                    let subtype = from_multirange.and_then(range_subtype).ok_or_else(|| {
                        not_a("anycompatiblemultirange", "a multirange type", multirange)
                    })?;
                    self.compatible_range = from_multirange;
                    self.compatible_subtype = Some(subtype);
                    self.wants_compatible_range = true;
                    self.compatible_types.push(subtype);
                }
            }
        } else if self.wants_compatible_multirange {
            self.compatible_multirange = self.compatible_range.and_then(range_multirange);
        }
        if self.compatible_types.is_empty() {
            // Only arguments of type `unknown`, which are `text` as `select_common_type` makes
            // them. A range type cannot come from those.
            for (wants, family) in [
                (self.wants_compatible_range, "anycompatiblerange"),
                (self.wants_compatible_multirange, "anycompatiblemultirange"),
            ] {
                if wants {
                    return Err(undetermined(Some(family)));
                }
            }
            self.compatible = Some(oid::TEXT);
            self.compatible_array = Some(oid::TEXT_ARRAY);
            return Ok(());
        }
        let known: Vec<Option<Oid>> = self.compatible_types.iter().copied().map(Some).collect();
        let common = common_type(&known).map_err(|mismatched| {
            let (first, other) = (format_type(mismatched.first), format_type(mismatched.other));
            mismatch(format!("argument types {first} and {other} cannot be matched"))
        })?;
        if !self
            .compatible_types
            .iter()
            .all(|&ty| ty == common || can_coerce_implicitly(ty, common))
        {
            return Err(mismatch(
                "arguments of anycompatible family cannot be cast to a common type".into(),
            ));
        }
        if self.wants_compatible_array {
            self.compatible_array = Some(array_type(common)?);
        }
        for (wants, held, family) in [
            (self.wants_compatible_range, self.compatible_range, "anycompatiblerange"),
            (
                self.wants_compatible_multirange,
                self.compatible_multirange,
                "anycompatiblemultirange",
            ),
        ] {
            if !wants {
                continue;
            }
            let Some(held) = held else { return Err(undetermined(Some(family))) };
            if self.compatible_subtype != Some(common) {
                let (held, common) = (format_type(held), format_type(common));
                return Err(mismatch(format!(
                    "{family} type {held} does not match anycompatible type {common}"
                )));
            }
        }
        if self.compatible_nonarray && element_type(common).is_some() {
            let name = format_type(common);
            return Err(mismatch(format!(
                "type matched to anycompatiblenonarray is an array type: {name}"
            )));
        }
        self.compatible = Some(common);
        Ok(())
    }

    /// The actual type of the declared type `declared`.
    fn actual(&self, declared: Oid) -> Result<Oid, TypeError> {
        let found = match declared {
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM => self.element,
            oid::ANYARRAY => match (self.array, self.element) {
                (Some(array), _) => Some(array),
                (None, Some(element)) => Some(array_type(element)?),
                (None, None) => None,
            },
            oid::ANYRANGE => {
                return self.range.ok_or_else(|| undetermined(Some("anyrange")));
            }
            oid::ANYMULTIRANGE => {
                return self.multirange.ok_or_else(|| undetermined(Some("anymultirange")));
            }
            oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => self.compatible,
            oid::ANYCOMPATIBLEARRAY => self.compatible_array,
            oid::ANYCOMPATIBLERANGE => self.compatible_range,
            oid::ANYCOMPATIBLEMULTIRANGE => self.compatible_multirange,
            other => Some(other),
        };
        found.ok_or_else(|| undetermined(None))
    }
}

/// Whether the type is one of the `anyelement` family.
fn family_one(oid: Oid) -> bool {
    matches!(
        oid,
        oid::ANYELEMENT
            | oid::ANYARRAY
            | oid::ANYNONARRAY
            | oid::ANYENUM
            | oid::ANYRANGE
            | oid::ANYMULTIRANGE
    )
}

/// `get_array_type`, with the error of PostgreSQL for a type with no array type.
fn array_type(element: Oid) -> Result<Oid, TypeError> {
    TypeInfo::get(element).map(|info| info.array).filter(|&array| array != 0).ok_or_else(|| {
        let name = format_type(element);
        TypeError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("could not find array type for data type {name}"),
        )
    })
}

/// `get_range_multirange`: the multirange type of a range type.
fn range_multirange(range: Oid) -> Option<Oid> {
    RANGES.iter().find(|row| row.0 == range).map(|row| row.2)
}

/// Holds the actual type of one argument of a family, which must be the type of the others.
fn hold(slot: &mut Option<Oid>, actual: Oid, family: &str) -> Result<(), TypeError> {
    match *slot {
        Some(held) if held != actual => Err(not_alike(family, held, actual)),
        _ => {
            *slot = Some(actual);
            Ok(())
        }
    }
}

/// Holds the type `from` that the argument `of` gives for the family `family`, which must agree
/// with the type that the family holds.
fn agree(
    slot: &mut Option<Oid>,
    from: Oid,
    of: (&str, Oid),
    family: &str,
) -> Result<(), TypeError> {
    match *slot {
        Some(held) if held != from => Err(not_consistent(of, (family, held))),
        _ => {
            *slot = Some(from);
            Ok(())
        }
    }
}

fn mismatch(message: String) -> TypeError {
    TypeError::new(SqlState::DATATYPE_MISMATCH, message)
}

fn with_detail(mut error: TypeError, first: Oid, second: Oid) -> TypeError {
    error.detail = Some(format!("{} versus {}", format_type(first), format_type(second)));
    error
}

fn not_alike(family: &str, held: Oid, actual: Oid) -> TypeError {
    with_detail(
        mismatch(format!("arguments declared \"{family}\" are not all alike")),
        held,
        actual,
    )
}

fn not_consistent((declared, ty): (&str, Oid), (other, held): (&str, Oid)) -> TypeError {
    let message =
        format!("argument declared {declared} is not consistent with argument declared {other}");
    with_detail(mismatch(message), ty, held)
}

fn not_a(declared: &str, kind: &str, actual: Oid) -> TypeError {
    let name = format_type(actual);
    mismatch(format!("argument declared {declared} is not {kind} but type {name}"))
}

fn undetermined(family: Option<&str>) -> TypeError {
    mismatch(match family {
        Some(family) => {
            format!("could not determine polymorphic type {family} because input has type unknown")
        }
        None => "could not determine polymorphic type because input has type unknown".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(actual: &[Oid], declared: &[Oid], result: Oid) -> Result<(Vec<Oid>, Oid), String> {
        enforce_generic_types(actual, declared, result)
            .map(|generic| (generic.args, generic.result))
            .map_err(|error| error.message)
    }

    #[test]
    fn the_anyelement_family_takes_the_type_of_its_arguments() {
        use oid::{ANYARRAY, ANYELEMENT, INT4, INT4_ARRAY, UNKNOWN};
        let found = types(&[INT4_ARRAY, UNKNOWN], &[ANYARRAY, ANYELEMENT], ANYARRAY);
        assert_eq!(found, Ok((vec![INT4_ARRAY, INT4], INT4_ARRAY)));
        let found = types(&[UNKNOWN, INT4], &[ANYARRAY, ANYELEMENT], ANYELEMENT);
        assert_eq!(found, Ok((vec![INT4_ARRAY, INT4], INT4)));
        let found = types(&[UNKNOWN], &[ANYARRAY], INT4);
        assert_eq!(
            found,
            Err("could not determine polymorphic type because input has type unknown".into())
        );
        let error =
            enforce_generic_types(&[INT4, oid::TEXT], &[ANYELEMENT, ANYELEMENT], INT4).unwrap_err();
        assert_eq!(error.message, "arguments declared \"anyelement\" are not all alike");
        assert_eq!(error.detail.as_deref(), Some("integer versus text"));
    }

    #[test]
    fn the_anycompatible_family_takes_the_common_type_of_its_arguments() {
        use oid::{ANYCOMPATIBLE, ANYCOMPATIBLEARRAY, INT4, INT4_ARRAY, NUMERIC, UNKNOWN};
        let declared = [ANYCOMPATIBLEARRAY, ANYCOMPATIBLE];
        let found = types(&[INT4_ARRAY, NUMERIC], &declared, ANYCOMPATIBLEARRAY);
        assert_eq!(found, Ok((vec![oid::NUMERIC_ARRAY, NUMERIC], oid::NUMERIC_ARRAY)));
        let found = types(&[UNKNOWN, UNKNOWN], &declared, ANYCOMPATIBLEARRAY);
        assert_eq!(found, Ok((vec![oid::TEXT_ARRAY, oid::TEXT], oid::TEXT_ARRAY)));
        let found = types(&[INT4_ARRAY, oid::TEXT], &declared, ANYCOMPATIBLEARRAY);
        assert_eq!(found, Err("argument types integer and text cannot be matched".into()));
        let found = types(&[INT4], &[ANYCOMPATIBLEARRAY], INT4);
        assert_eq!(
            found,
            Err("argument declared anycompatiblearray is not an array but type integer".into())
        );
    }

    #[test]
    fn a_function_with_no_polymorphic_argument_keeps_its_types() {
        let found = types(&[oid::INT4], &[oid::INT4], oid::ANYARRAY);
        assert_eq!(found, Ok((vec![oid::INT4], oid::ANYARRAY)));
    }
}
