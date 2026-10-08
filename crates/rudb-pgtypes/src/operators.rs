//! The built-in operators of PostgreSQL, as `pg_operator` has them, for the rules that find the
//! operator that an expression names.

use crate::generated::operators::OPERATORS;
use crate::procs::{Proc, procs};
use crate::types::Oid;

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
}

/// The row constructor of the generated table, short so that each row fits on one line.
pub(crate) const fn o(
    oid: Oid,
    name: &'static str,
    kind: u8,
    args: &'static [Oid],
    result: Oid,
    code: &'static str,
) -> Operator {
    Operator { oid, name, kind, args, result, code }
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
        let negate: Vec<_> = operators("-", b'l').map(|o| o.args.len()).collect();
        assert!(!negate.is_empty() && negate.iter().all(|&count| count == 1));
        assert!(OPERATORS.iter().all(|operator| operator.proc().is_some()));
        assert_eq!(operators("nosuchop", b'b').count(), 0);
    }
}
