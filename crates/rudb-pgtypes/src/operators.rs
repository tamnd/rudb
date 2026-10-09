//! The built-in operators of PostgreSQL, as `pg_operator` has them, for the rules that find the
//! operator that an expression names.

use crate::generated::operators::OPERATORS;
use crate::procs::{Proc, procs};
use crate::types::{Oid, TypeInfo};

/// A row of `pg_operator`, with the columns that the rules for an operator read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operator {
    pub oid: Oid,
    pub name: &'static str,
    /// `oprkind`: `b` for an operator between two values and `l` for a prefix operator.
    pub kind: u8,
    /// The types of the operands: `oprleft` and `oprright`, or only `oprright` for a prefix
    /// operator.
    pub args: &'static [Oid],
    /// `oprresult`.
    pub result: Oid,
    /// `oprcode`: the name of the function that the operator calls.
    pub code: &'static str,
    /// `oprcanmerge`: the operator is the equality of a btree operator family, so a merge join
    /// and a sort can use it.
    pub merges: bool,
    /// `oprcanhash`: the operator is the equality of a hash operator family, so a hash join and a
    /// hash table can use it.
    pub hashes: bool,
}

/// The flags of a row of the generated table: neither, `oprcanmerge`, `oprcanhash`, or both.
pub(crate) const N: u8 = 0;
pub(crate) const M: u8 = 1;
pub(crate) const H: u8 = 2;
pub(crate) const MH: u8 = M | H;

/// The row constructor of the generated table, short so that each row fits on one line.
pub(crate) const fn o(
    oid: Oid,
    name: &'static str,
    kind: u8,
    args: &'static [Oid],
    result: Oid,
    code: &'static str,
    flags: u8,
) -> Operator {
    Operator { oid, name, kind, args, result, code, merges: flags & M != 0, hashes: flags & H != 0 }
}

impl Operator {
    /// The function of `pg_proc` that the operator calls. The generator checks that there is
    /// one.
    pub fn proc(&self) -> Option<&'static Proc> {
        procs(self.code).iter().find(|proc| proc.args == self.args)
    }
}

/// The built-in operators with this `oprname` and `oprkind`, in the order of the OIDs.
pub fn operators(name: &str, kind: u8) -> impl Iterator<Item = &'static Operator> {
    let start = OPERATORS.partition_point(|operator| operator.name < name);
    let end = start + OPERATORS[start..].partition_point(|operator| operator.name == name);
    OPERATORS[start..end].iter().filter(move |operator| operator.kind == kind)
}

/// The built-in operators that call the function `proc`, in the order of the name and the OID.
pub fn operators_of(proc: &Proc) -> impl Iterator<Item = &'static Operator> {
    let (name, args) = (proc.name, proc.args);
    OPERATORS.iter().filter(move |operator| operator.code == name && operator.args == args)
}

/// Whether a hash table can hold the values of the type, as `op_hashjoinable` finds for the
/// equality that sorts and groups them. This is false when the type has an equality of its own that
/// a btree can use but a hash cannot, such as `bit` and `money`, and for an array of such a type,
/// whose equality hashes only when the equality of the element hashes. A type with no equality of
/// its own, such as `varchar`, reads as the type that it can be read as without a change, so it is
/// taken as hashable here.
pub fn hashable(oid: Oid) -> bool {
    if let Some(info) = TypeInfo::get(oid).filter(|info| info.is_array()) {
        return hashable(info.elem);
    }
    let own = |operator: &&Operator| operator.args == [oid, oid] && operator.merges;
    operators("=", b'b').find(own).is_none_or(|equality| equality.hashes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oid;

    #[test]
    fn an_operator_has_its_operands_and_its_function() {
        let plus: Vec<_> =
            operators("+", b'b').filter(|o| o.args == [oid::INT4, oid::INT4]).collect();
        assert_eq!(plus.len(), 1);
        assert_eq!(plus[0].proc().map(|proc| proc.src), Some("int4pl"));
        let proc = plus[0].proc().unwrap();
        assert_eq!(operators_of(proc).map(|o| o.oid).collect::<Vec<_>>(), [plus[0].oid]);
        let negate: Vec<_> = operators("-", b'l').map(|o| o.args.len()).collect();
        assert!(!negate.is_empty() && negate.iter().all(|&count| count == 1));
        assert!(OPERATORS.iter().all(|operator| operator.proc().is_some()));
        assert_eq!(operators("nosuchop", b'b').count(), 0);
    }

    #[test]
    fn a_type_is_hashable_when_its_equality_hashes() {
        for hashes in [oid::INT4, oid::TEXT, oid::VARCHAR, oid::NUMERIC, oid::INT4_ARRAY] {
            assert!(hashable(hashes), "{hashes}");
        }
        for sorts_only in [oid::BIT, oid::VARBIT, oid::MONEY, oid::VARBIT_ARRAY] {
            assert!(!hashable(sorts_only), "{sorts_only}");
        }
    }
}
