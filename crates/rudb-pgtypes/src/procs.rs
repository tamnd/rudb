//! The built-in functions of PostgreSQL, as `pg_proc` has them, for the rules that find the
//! function a call names.

use crate::generated::procs::PROCS;
use crate::types::{Oid, TypeInfo};

/// A row of `pg_proc`, with the columns that the rules for a call read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proc {
    pub oid: Oid,
    pub name: &'static str,
    /// `proargtypes`: the types of the input arguments. The last one of a variadic function is
    /// the array of `variadic`, or `any`.
    pub args: &'static [Oid],
    /// `prorettype`.
    pub result: Oid,
    /// `provariadic`: the element type of the variadic argument, or 0.
    pub variadic: Oid,
    /// `prokind`: `f` for a function, `a` for an aggregate, `w` for a window function and `p`
    /// for a procedure.
    pub kind: u8,
    /// `proisstrict`: the result is null when an argument is.
    pub strict: bool,
    /// `proretset`: the function gives a set of rows.
    pub retset: bool,
    /// `provolatile`: `i`, `s` or `v`.
    pub volatility: u8,
    /// The names of the input arguments, with an empty name for an argument with none, or no
    /// names at all.
    pub names: &'static [&'static str],
    /// `proargdefaults`: the defaults of the last input arguments, as text.
    pub defaults: &'static [&'static str],
}

/// The row constructor of the generated table, short so that each row fits on one line.
#[allow(clippy::too_many_arguments)]
pub(crate) const fn p(
    oid: Oid,
    name: &'static str,
    args: &'static [Oid],
    result: Oid,
    variadic: Oid,
    kind: u8,
    strict: bool,
    retset: bool,
    volatility: u8,
    names: &'static [&'static str],
    defaults: &'static [&'static str],
) -> Proc {
    Proc { oid, name, args, result, variadic, kind, strict, retset, volatility, names, defaults }
}

impl Proc {
    /// Whether a call with `count` arguments by position can call this function, as
    /// `FuncnameGetCandidates` decides it: a variadic function takes its fixed arguments and any
    /// number of values after them, and a function with defaults takes fewer arguments.
    pub fn takes(&self, count: usize) -> bool {
        let declared = self.args.len();
        let variadic = self.variadic != 0 && declared <= count;
        let defaulted = declared > count && count + self.defaults.len() >= declared;
        declared == count || variadic || defaulted
    }
}

/// The built-in functions with this `proname`, in the order of the OIDs.
pub fn procs(name: &str) -> &'static [Proc] {
    let start = PROCS.partition_point(|proc| proc.name < name);
    let end = start + PROCS[start..].partition_point(|proc| proc.name == name);
    &PROCS[start..end]
}

/// The type that a call of a function with the name of a type converts its argument to, as
/// `FuncNameAsType` of PostgreSQL finds it: a built-in type with this `typname` that is not the
/// row type of a table.
pub fn func_name_as_type(name: &str) -> Option<Oid> {
    TypeInfo::by_name(name).filter(|info| info.kind != b'c').map(|info| info.oid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid;

    #[test]
    fn the_functions_of_a_name_and_the_arguments_they_take() {
        let abs: Vec<_> = procs("abs").iter().map(|proc| proc.args).collect();
        assert!(abs.contains(&&[oid::INT4][..]) && abs.len() == 6);
        assert!(procs("nosuchfn").is_empty() && procs("").is_empty());
        let interval = &procs("make_interval")[0];
        assert_eq!(interval.names[3], "days");
        assert!(interval.takes(0) && interval.takes(7) && !interval.takes(8));
        let concat = &procs("concat")[0];
        assert!(concat.takes(1) && concat.takes(5) && !concat.takes(0));
        assert!(procs("abs").iter().all(|proc| !proc.takes(2)));
        assert_eq!(func_name_as_type("int4"), Some(oid::INT4));
        assert_eq!(func_name_as_type("pg_class"), None);
        assert_eq!(func_name_as_type("integer"), None);
    }
}
