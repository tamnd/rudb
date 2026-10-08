//! The built-in collations of PostgreSQL, as `pg_collation` has them, for the `COLLATE` clause.

use crate::generated::collations::COLLATIONS;
use crate::types::Oid;

/// A row of `pg_collation`, with the columns that the rules for a collation read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collation {
    pub oid: Oid,
    pub name: &'static str,
    /// `collprovider`: `d` for the default collation of the database, `c` for the C library, `b`
    /// for the builtin provider and `i` for ICU.
    pub provider: u8,
    /// `collencoding`: the encoding the collation works with, or -1 for every encoding.
    pub encoding: i32,
    /// `colllocale`, or `collcollate` for a collation of the C library. Empty for the default
    /// collation.
    pub locale: &'static str,
}

/// The built-in collation with the name `name`, which is case sensitive as a name in
/// `pg_collation` is.
pub fn collation(name: &str) -> Option<&'static Collation> {
    let found = COLLATIONS.binary_search_by(|collation| collation.name.cmp(name)).ok()?;
    Some(&COLLATIONS[found])
}

/// The built-in collation with the OID `oid`.
pub fn collation_by_oid(oid: Oid) -> Option<&'static Collation> {
    COLLATIONS.iter().find(|collation| collation.oid == oid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_collations_are_found_by_their_exact_names() {
        assert_eq!(collation("C").map(|c| c.oid), Some(950));
        assert_eq!(collation("default").map(|c| (c.provider, c.encoding)), Some((b'd', -1)));
        assert_eq!(collation("pg_unicode_fast").map(|c| c.locale), Some("PG_UNICODE_FAST"));
        assert_eq!(collation("ucs_basic").map(|c| (c.provider, c.locale)), Some((b'b', "C")));
        assert_eq!(collation("c"), None);
        assert_eq!(collation_by_oid(951).map(|c| c.name), Some("POSIX"));
    }
}
