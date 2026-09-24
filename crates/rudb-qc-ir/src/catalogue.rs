//! The runtime functions an `rtcall` may reach, per `spec/compiler/06-qir.md` section 6.5.
//!
//! The catalogue is static data so that every tier agrees on it without a registry: an `rtcall`
//! operand is an index into [`CATALOGUE`], the interpreter dispatches on the same index, and
//! `direct` puts the same index into its veneer table. `rudb-qc-rt` implements each entry, and
//! a test there checks that it implements all of them with these signatures.

use crate::Ty;

/// One runtime function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proxy {
    /// The name the text form uses after `@`.
    pub name: &'static str,
    /// The argument types.
    pub args: &'static [Ty],
    /// The result type, `void` for none.
    pub ret: Ty,
    /// The function returns a status first, and the call site returns it when it is not `Ok`.
    pub mayfail: bool,
    /// No side effects, so equal calls may be merged.
    pub pure: bool,
    /// Writes state shared beyond the morsel, which matters to rule V9.
    pub effect: bool,
}

macro_rules! proxies {
    ($($name:literal ($($arg:ident),*) -> $ret:ident $(, $attr:ident)*;)*) => {
        /// Every runtime function, in id order. Ids are stable within a build, and the code cache
        /// keys on the build.
        pub const CATALOGUE: &[Proxy] = &[$(
            Proxy {
                name: $name,
                args: &[$(Ty::$arg),*],
                ret: Ty::$ret,
                mayfail: proxies!(@has mayfail $($attr)*),
                pure: proxies!(@has pure $($attr)*),
                effect: proxies!(@has effect $($attr)*),
            },
        )*];
    };
    (@has $want:ident) => { false };
    (@has mayfail mayfail $($rest:ident)*) => { true };
    (@has pure pure $($rest:ident)*) => { true };
    (@has effect effect $($rest:ident)*) => { true };
    (@has $want:ident $other:ident $($rest:ident)*) => { proxies!(@has $want $($rest)*) };
}

proxies! {
    // Strings.
    "str_promote" (Ptr, Str16) -> Str16, mayfail;
    "str_eq" (Str16, Str16) -> I1, pure;
    "str_cmp" (Str16, Str16) -> I32, pure;
    "str_hash" (Str16, I64) -> I64, pure;
    "str_like" (Ptr, Str16) -> I1, pure;
    "str_ilike" (Ptr, Str16) -> I1, pure;
    "str_contains" (Ptr, Str16) -> I1, pure;
    "str_regex" (Ptr, Str16) -> I1, pure;
    "str_regex_replace" (Ptr, Ptr, Str16) -> Str16, mayfail;
    "str_lower" (Ptr, Str16) -> Str16, mayfail;
    "str_upper" (Ptr, Str16) -> Str16, mayfail;
    "str_substr" (Ptr, Str16, I64, I64) -> Str16, mayfail;
    "str_length" (Str16) -> I64, pure;
    "str_concat" (Ptr, Str16, Str16) -> Str16, mayfail;
    // Hash tables and aggregation state.
    "ht_grow" (Ptr, Ptr) -> Void, mayfail, effect;
    "ht_insert" (Ptr, Ptr, I64) -> Ptr, mayfail, effect;
    "agg_flush" (Ptr, Ptr) -> Void, mayfail, effect;
    "agg_distinct" (Ptr, Ptr, I64, Str16) -> Void, mayfail, effect;
    "sink_row" (Ptr, Ptr) -> Ptr, mayfail, effect;
    // Decimals and dates.
    "i128_div" (I128, I128) -> I128, mayfail, pure;
    "date_trunc_minute" (I64) -> I64, pure;
    "date_extract_minute" (I64) -> I64, pure;
    "date_extract_year" (I32) -> I64, pure;
    "date_trunc_month" (I32) -> I32, pure;
}

/// The id of the runtime function with this name.
#[must_use]
pub fn proxy(name: &str) -> Option<u32> {
    CATALOGUE.iter().position(|p| p.name == name).map(|i| i as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_attributes_parse() {
        for (i, p) in CATALOGUE.iter().enumerate() {
            assert_eq!(proxy(p.name), Some(i as u32), "{} is listed twice", p.name);
        }
        let grow = CATALOGUE[proxy("ht_grow").unwrap() as usize];
        assert!(grow.mayfail && grow.effect && !grow.pure);
        let eq = CATALOGUE[proxy("str_eq").unwrap() as usize];
        assert!(eq.pure && !eq.mayfail && !eq.effect);
    }
}
