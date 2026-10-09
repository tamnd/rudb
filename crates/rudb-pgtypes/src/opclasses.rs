//! The operator classes of PostgreSQL, and the equality and ordering operators that a type has
//! through them. `GROUP BY`, `DISTINCT`, `ORDER BY` and a set operation read these, as
//! `get_sort_group_operators` reads them from `lookup_type_cache`.

use crate::coerce::{find_cast, is_preferred};
use crate::generated::opclasses::OPCLASSES;
use crate::oid;
use crate::types::{Oid, TypeInfo};

/// A row of `pg_opclass`: the access method, the input type, the name and whether the class is
/// the default class of the method for its input type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opclass {
    pub method: &'static str,
    pub input: Oid,
    pub name: &'static str,
    pub default: bool,
}

/// `GetDefaultOpClass`: the default class of an access method for a type. A class of the type
/// itself comes first. Else the one class whose input type the type is binary coercible to, or
/// the one such class whose input type is the preferred type of the category of the type.
pub fn default_opclass(oid: Oid, method: &str) -> Option<&'static Opclass> {
    let defaults =
        || OPCLASSES.iter().filter(move |class| class.method == method && class.default);
    if let Some(exact) = defaults().find(|class| class.input == oid) {
        return Some(exact);
    }
    let category = TypeInfo::get(oid).map(|info| info.category);
    let preferred = |input: Oid| {
        is_preferred(input) && TypeInfo::get(input).map(|info| info.category) == category
    };
    let compatible: Vec<&Opclass> =
        defaults().filter(|class| binary_coercible(oid, class.input)).collect();
    let mut best = compatible.iter().filter(|class| preferred(class.input));
    match (best.next(), best.next()) {
        (Some(class), None) => Some(class),
        (Some(_), Some(_)) => None,
        (None, _) => match compatible[..] {
            [class] => Some(class),
            _ => None,
        },
    }
}

/// `IsBinaryCoercible` for two built-in types: a value of `source` is a value of `target` with
/// no change. The polymorphic types take each type of their kind, `record` takes a composite
/// type, and a cast of `pg_cast` with the method `b` counts when it is implicit.
pub fn binary_coercible(source: Oid, target: Oid) -> bool {
    if source == target || matches!(target, oid::ANY | oid::ANYELEMENT | oid::ANYCOMPATIBLE) {
        return true;
    }
    let info = TypeInfo::get(source);
    let array = info.is_some_and(TypeInfo::is_array);
    let kind = info.map(|info| info.kind);
    let fits = match target {
        oid::ANYARRAY | oid::ANYCOMPATIBLEARRAY => array,
        oid::ANYNONARRAY | oid::ANYCOMPATIBLENONARRAY => !array,
        oid::ANYENUM => kind == Some(b'e'),
        oid::ANYRANGE | oid::ANYCOMPATIBLERANGE => kind == Some(b'r'),
        oid::ANYMULTIRANGE | oid::ANYCOMPATIBLEMULTIRANGE => kind == Some(b'm'),
        oid::RECORD => kind == Some(b'c'),
        oid::RECORD_ARRAY => info
            .filter(|info| info.is_array())
            .and_then(|info| TypeInfo::get(info.elem))
            .is_some_and(|elem| elem.kind == b'c' || elem.oid == oid::RECORD),
        _ => false,
    };
    fits || find_cast(source, target).is_some_and(|cast| {
        cast.method == b'b' && cast.context == crate::coerce::CoercionContext::Implicit
    })
}

/// Whether a type has an equality operator, as `lookup_type_cache` finds `eq_opr`: the operator
/// of the default btree class, else of the default hash class. An array has one when its element
/// type has one. A row of no declared type has one, because its fields are only known when it is
/// compared.
pub fn has_equality(oid: Oid) -> bool {
    if let Some(info) = TypeInfo::get(oid).filter(|info| info.is_array()) {
        return has_equality(info.elem);
    }
    oid == oid::RECORD
        || default_opclass(oid, "btree").is_some()
        || default_opclass(oid, "hash").is_some()
}

/// Whether a type has an ordering operator, as `lookup_type_cache` finds `lt_opr`: the operator
/// of the default btree class. An array has one when its element type has one.
pub fn has_ordering(oid: Oid) -> bool {
    if let Some(info) = TypeInfo::get(oid).filter(|info| info.is_array()) {
        return has_ordering(info.elem);
    }
    oid == oid::RECORD || default_opclass(oid, "btree").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_type_finds_its_default_class() {
        assert_eq!(default_opclass(oid::INT4, "btree").map(|class| class.name), Some("int4_ops"));
        // `varchar` is binary coercible to `text`, and `cidr` to `inet`.
        assert_eq!(default_opclass(oid::VARCHAR, "btree").map(|class| class.name), Some("text_ops"));
        assert_eq!(default_opclass(oid::CIDR, "hash").map(|class| class.name), Some("inet_ops"));
        assert_eq!(
            default_opclass(oid::INT4_ARRAY, "btree").map(|class| class.name),
            Some("array_ops")
        );
        assert_eq!(default_opclass(oid::JSON, "btree"), None);
        assert_eq!(default_opclass(oid::JSON, "hash"), None);
    }

    #[test]
    fn the_operators_of_a_type() {
        for oid in [oid::INT4, oid::TEXT, oid::VARCHAR, oid::JSONB, oid::BYTEA, oid::NUMERIC] {
            assert!(has_equality(oid) && has_ordering(oid), "{oid}");
        }
        // `xid` hashes and has no btree class, so it has an equality and no ordering.
        assert!(has_equality(oid::XID) && !has_ordering(oid::XID));
        for oid in [oid::JSON, oid::POINT, oid::BOX, oid::JSON_ARRAY] {
            assert!(!has_equality(oid) && !has_ordering(oid), "{oid}");
        }
        assert!(has_equality(oid::RECORD) && has_ordering(oid::RECORD));
    }
}
