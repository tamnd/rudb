//! The type that PostgreSQL gives to a list of values that must have one type, as the arms of a
//! `CASE` or the values of a `COALESCE`, an `ARRAY` or a column of `VALUES`.

use crate::generated::casts::{ASSIGNMENT, IMPLICIT, PREFERRED};
use crate::types::{Oid, TypeInfo};

/// Whether a value of type `from` becomes a value of type `to` with no cast written, as
/// `can_coerce_type` with `COERCION_IMPLICIT` says for two built-in types. An array becomes an
/// array of another type when its elements do.
pub fn can_coerce_implicitly(from: Oid, to: Oid) -> bool {
    if from == to || IMPLICIT.binary_search(&(from, to)).is_ok() {
        return true;
    }
    match (TypeInfo::get(from), TypeInfo::get(to)) {
        (Some(from), Some(to)) if from.is_array() && to.is_array() => {
            can_coerce_implicitly(from.elem, to.elem)
        }
        _ => false,
    }
}

/// Whether a value of type `from` becomes a value of type `to` where the type of the place it goes
/// to decides, as in `INSERT` and `LIMIT`. This is `can_coerce_type` with `COERCION_ASSIGNMENT` for
/// two built-in types: an implicit cast, an assignment cast, or the output function of `from` when
/// `to` is a string type. An array becomes an array of another type when its elements do.
pub fn can_coerce_assigned(from: Oid, to: Oid) -> bool {
    if can_coerce_implicitly(from, to) || ASSIGNMENT.binary_search(&(from, to)).is_ok() {
        return true;
    }
    match (TypeInfo::get(from), TypeInfo::get(to)) {
        (Some(from), Some(to)) if from.is_array() && to.is_array() => {
            can_coerce_assigned(from.elem, to.elem)
        }
        (_, Some(to)) => to.category == b'S',
        _ => false,
    }
}

/// Whether the type has `typispreferred`, such as `text`, `float8` and `timestamptz`.
pub fn is_preferred(oid: Oid) -> bool {
    PREFERRED.binary_search(&oid).is_ok()
}

/// Two values whose types are in different categories, so that no one type holds both. `at` is
/// the place of the second value in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch {
    pub at: usize,
    pub first: Oid,
    pub other: Oid,
}

/// The type of a list of values, as `select_common_type` picks it. `None` is a value of type
/// `unknown`, such as a string literal or a NULL, which takes the type of the others.
///
/// The first value of a known type gives the category. A value of a type in another category is
/// a [`Mismatch`]. In the category, a later type replaces the type held when the held type is not
/// preferred and casts to the later type with no cast written, but the later type does not cast
/// back. So `integer` and `numeric` give `numeric` and `numeric` and `double precision` give
/// `double precision`. A list of values that are all `unknown` is `text`.
pub fn common_type(types: &[Option<Oid>]) -> Result<Oid, Mismatch> {
    let category = |oid: Oid| TypeInfo::get(oid).map_or(b'X', |info| info.category);
    let mut held: Option<Oid> = None;
    for (at, &ty) in types.iter().enumerate() {
        let Some(ty) = ty else { continue };
        match held {
            None => held = Some(ty),
            Some(first) if first == ty => {}
            Some(first) if category(first) != category(ty) => {
                return Err(Mismatch { at, first, other: ty });
            }
            Some(first) => {
                if !is_preferred(first)
                    && can_coerce_implicitly(first, ty)
                    && !can_coerce_implicitly(ty, first)
                {
                    held = Some(ty);
                }
            }
        }
    }
    Ok(held.unwrap_or(crate::oid::TEXT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid;

    #[test]
    fn an_assignment_takes_the_casts_of_assignment_and_the_output_to_a_string() {
        assert!(can_coerce_assigned(oid::INT4, oid::INT8));
        assert!(can_coerce_assigned(oid::NUMERIC, oid::INT8));
        assert!(can_coerce_assigned(oid::FLOAT8, oid::INT8));
        assert!(can_coerce_assigned(oid::BOOL, oid::TEXT));
        assert!(!can_coerce_assigned(oid::BOOL, oid::INT8));
        assert!(!can_coerce_assigned(oid::TEXT, oid::INT8));
        assert!(!can_coerce_assigned(oid::DATE, oid::INT8));
    }

    #[test]
    fn the_common_type_follows_the_category_and_the_preferred_type() {
        let common = |types: &[Option<Oid>]| common_type(types);
        assert_eq!(common(&[Some(oid::INT4), None]), Ok(oid::INT4));
        assert_eq!(common(&[None, None]), Ok(oid::TEXT));
        assert_eq!(common(&[Some(oid::INT4), Some(oid::NUMERIC)]), Ok(oid::NUMERIC));
        assert_eq!(common(&[Some(oid::NUMERIC), Some(oid::FLOAT4)]), Ok(oid::FLOAT4));
        assert_eq!(common(&[Some(oid::FLOAT8), Some(oid::INT8)]), Ok(oid::FLOAT8));
        assert_eq!(common(&[Some(oid::DATE), Some(oid::TIMESTAMPTZ)]), Ok(oid::TIMESTAMPTZ));
        assert_eq!(common(&[Some(oid::VARCHAR), Some(oid::TEXT)]), Ok(oid::VARCHAR));
        assert_eq!(common(&[Some(oid::TEXT), Some(oid::VARCHAR)]), Ok(oid::TEXT));
        assert_eq!(common(&[Some(oid::INT4_ARRAY), Some(oid::INT8_ARRAY)]), Ok(oid::INT8_ARRAY));
        assert_eq!(
            common(&[Some(oid::INT4), None, Some(oid::TEXT)]),
            Err(Mismatch { at: 2, first: oid::INT4, other: oid::TEXT })
        );
        assert!(can_coerce_implicitly(oid::INT2, oid::FLOAT8));
        assert!(!can_coerce_implicitly(oid::FLOAT8, oid::NUMERIC));
        assert!(is_preferred(oid::TIMESTAMPTZ) && !is_preferred(oid::INT4));
    }
}
