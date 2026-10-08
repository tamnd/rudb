//! The type that PostgreSQL gives to a list of values that must have one type, as the arms of a
//! `CASE` or the values of a `COALESCE`, an `ARRAY` or a column of `VALUES`.

use crate::generated::casts::{CASTS, PREFERRED};
use crate::oid;
use crate::types::{Oid, TypeInfo};

/// A row of `pg_cast`: a cast from `source` to `target`, the place it is allowed in and the way
/// it is done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cast {
    pub source: Oid,
    pub target: Oid,
    /// `castcontext`: the weakest place the cast is allowed in.
    pub context: CoercionContext,
    /// `castmethod`: `f` for a function, `i` for the output and then the input function, and `b`
    /// for no change to the value.
    pub method: u8,
}

/// The row constructor of the generated table, short so that each row fits on one line.
pub(crate) const fn c(source: Oid, target: Oid, context: u8, method: u8) -> Cast {
    let context = match context {
        b'i' => CoercionContext::Implicit,
        b'a' => CoercionContext::Assignment,
        _ => CoercionContext::Explicit,
    };
    Cast { source, target, context, method }
}

/// Where a value changes its type, from the place that allows the fewest casts to the place that
/// allows all of them: with no cast written, where the type of the place it goes to decides, as
/// in `INSERT`, and in a cast that is written. This is `CoercionContext` of PostgreSQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoercionContext {
    Implicit,
    Assignment,
    Explicit,
}

/// How a value of one type becomes a value of another, as `find_coercion_pathway` of PostgreSQL
/// finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoercionPath {
    /// No way that the context allows.
    None,
    /// The value stays as it is, as from `oid` to `int4` or from a type to itself.
    Relabel,
    /// The function of the cast.
    Function,
    /// Each element of an array becomes an element of the other array type.
    ArrayCoerce,
    /// The output function of the source and then the input function of the target.
    ViaIo,
}

/// The cast of `pg_cast` from `source` to `target`, if there is one.
pub fn find_cast(source: Oid, target: Oid) -> Option<&'static Cast> {
    let at = CASTS.binary_search_by_key(&(source, target), |cast| (cast.source, cast.target));
    at.ok().map(|at| &CASTS[at])
}

/// How a value of type `source` becomes a value of type `target` in `context`, for two built-in
/// types. This is `find_coercion_pathway` of PostgreSQL: a type to itself is a relabel, then the
/// cast of `pg_cast` decides when there is one. With no cast, an array becomes another array type
/// when its elements do, and the output and input functions turn any type into a string type
/// where a cast is not written, and a string type into any type where one is.
pub fn find_coercion_pathway(source: Oid, target: Oid, context: CoercionContext) -> CoercionPath {
    if source == target {
        return CoercionPath::Relabel;
    }
    if let Some(cast) = find_cast(source, target) {
        if context < cast.context {
            return CoercionPath::None;
        }
        return match cast.method {
            b'f' => CoercionPath::Function,
            b'i' => CoercionPath::ViaIo,
            _ => CoercionPath::Relabel,
        };
    }
    let (from, to) = (TypeInfo::get(source), TypeInfo::get(target));
    let element =
        |info: Option<&TypeInfo>| info.filter(|info| info.is_array()).map(|info| info.elem);
    if !matches!(target, oid::OIDVECTOR | oid::INT2VECTOR)
        && let (Some(to), Some(from)) = (element(to), element(from))
        && find_coercion_pathway(from, to, context) != CoercionPath::None
    {
        return CoercionPath::ArrayCoerce;
    }
    let string = |info: Option<&TypeInfo>| info.is_some_and(|info| info.category == b'S');
    if (context >= CoercionContext::Assignment && string(to))
        || (context == CoercionContext::Explicit && string(from))
    {
        return CoercionPath::ViaIo;
    }
    CoercionPath::None
}

/// Whether a value of type `from` becomes a value of type `to` with no cast written, as
/// `can_coerce_type` with `COERCION_IMPLICIT` says for two built-in types. An array becomes an
/// array of another type when its elements do.
pub fn can_coerce_implicitly(from: Oid, to: Oid) -> bool {
    find_coercion_pathway(from, to, CoercionContext::Implicit) != CoercionPath::None
}

/// Whether a value of type `from` becomes a value of type `to` where the type of the place it goes
/// to decides, as in `INSERT` and `LIMIT`. This is `can_coerce_type` with `COERCION_ASSIGNMENT` for
/// two built-in types: an implicit cast, an assignment cast, or the output function of `from` when
/// `to` is a string type. An array becomes an array of another type when its elements do.
pub fn can_coerce_assigned(from: Oid, to: Oid) -> bool {
    find_coercion_pathway(from, to, CoercionContext::Assignment) != CoercionPath::None
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
    Ok(held.unwrap_or(oid::TEXT))
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
