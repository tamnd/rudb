//! `WITH ORDINALITY`, which gives a function in `FROM` one more column that numbers its rows from
//! 1. Both dialects have it, and both name the column `ordinality`.
//!
//! The number is made by the operator of the call, which knows the place of each row it makes. A
//! series knows it from the position of the value, and an unnest and a JSON walk count the rows
//! of each input row. A call to any other function is refused rather than numbered by a window,
//! because the order of the rows above a scan is not the order the call made them in.

use rudb_common::{Error, Field, LogicalType, Result};
use rudb_functions::TableFunction;
use rudb_plan::{ColumnBinding, Node, NodeRef};

use crate::binder::Binder;
use crate::scope::{Scope, Visible};

/// The name of the column of `WITH ORDINALITY`.
pub(crate) const ORDINALITY: &str = "ordinality";

impl Binder<'_> {
    /// Adds the column of `WITH ORDINALITY` to the call that `node` is, and to its scope.
    pub(crate) fn number_rows(&mut self, node: NodeRef, scope: &mut Scope) -> Result<()> {
        let binding = self.ordinality_column(node)?;
        let table = scope.columns.first().map(|column| column.table.clone()).unwrap_or_default();
        scope.push(Visible {
            table,
            name: ORDINALITY.to_owned(),
            binding,
            ty: LogicalType::BigInt,
            not_null: true,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        });
        Ok(())
    }

    /// Marks the call that `node` is as one that numbers its rows, and gives the binding of the
    /// number.
    pub(crate) fn ordinality_column(&mut self, node: NodeRef) -> Result<ColumnBinding> {
        let (Node::TableFunction { index, function, columns, .. }
        | Node::LateralFunction { index, function, columns, .. }) = *self.plan().node(node)
        else {
            return Err(Error::not_implemented(
                "WITH ORDINALITY for a call that is not a table function",
            ));
        };
        let name = self.plan().string(function).to_owned();
        let numbered = matches!(
            TableFunction::lookup(&name),
            Some(
                TableFunction::Range
                    | TableFunction::GenerateSeries
                    | TableFunction::Unnest
                    | TableFunction::JsonEach
                    | TableFunction::JsonTree
            )
        );
        if !numbered {
            return Err(unnumbered(&name));
        }
        let mut fields = self.plan().field_list(columns).to_vec();
        let at = u32::try_from(fields.len()).map_err(|_| Error::internal("a column count"))?;
        fields.push(Field::new(ORDINALITY, LogicalType::BigInt));
        let widened = self.plan_mut().add_fields(&fields);
        match self.plan_mut().node_mut(node) {
            Node::TableFunction { columns, ordinality, .. }
            | Node::LateralFunction { columns, ordinality, .. } => {
                *columns = widened;
                *ordinality = true;
            }
            _ => return Err(Error::internal("the call changed while it was numbered")),
        }
        Ok(ColumnBinding::new(index, at))
    }
}

/// The error for `WITH ORDINALITY` on a call whose rows are not numbered.
pub(crate) fn unnumbered(name: &str) -> Error {
    Error::not_implemented(format!("WITH ORDINALITY for {name}"))
}
