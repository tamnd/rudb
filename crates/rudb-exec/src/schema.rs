//! What an operator produces: names, types, and the bindings downstream expressions use.

use rudb_common::{Error, Field, LogicalType, Result};
use rudb_plan::ColumnBinding;

/// One operator's output columns.
///
/// The fields and the bindings are parallel and always the same length, which [`Schema::new`]
/// checks. They are two vectors rather than a vector of pairs because a caller almost always wants
/// one of them whole: the result set wants the names and the types, and the expression evaluator
/// wants to search the bindings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    fields: Vec<Field>,
    bindings: Vec<ColumnBinding>,
}

impl Schema {
    /// A schema of `fields`, bound at `bindings`.
    ///
    /// # Errors
    ///
    /// If the two are not the same length, which would make a binding resolve to the wrong column
    /// or to no column at all.
    pub fn new(fields: Vec<Field>, bindings: Vec<ColumnBinding>) -> Result<Self> {
        if fields.len() != bindings.len() {
            return Err(Error::internal(format!(
                "a schema of {} fields and {} bindings",
                fields.len(),
                bindings.len()
            )));
        }
        Ok(Self { fields, bindings })
    }

    /// A schema of `fields` numbered from zero against `table`.
    ///
    /// The common case, since an operator that introduces columns numbers them in the order it
    /// produces them and the binder does the same.
    ///
    /// # Panics
    ///
    /// If there are more than `u32::MAX` fields, which is the bound a binding's position already
    /// carries.
    #[must_use]
    pub fn numbered(fields: Vec<Field>, table: u32) -> Self {
        let bindings = (0..fields.len())
            .map(|at| {
                ColumnBinding::new(
                    table,
                    u32::try_from(at).expect("a schema this wide cannot be built"),
                )
            })
            .collect();
        Self { fields, bindings }
    }

    /// A schema with no columns, which is what [`Node::Dummy`](rudb_plan::Node::Dummy) produces.
    #[must_use]
    pub fn empty() -> Self {
        Self { fields: Vec::new(), bindings: Vec::new() }
    }

    /// The fields, in output order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// The bindings, in output order.
    #[must_use]
    pub fn bindings(&self) -> &[ColumnBinding] {
        &self.bindings
    }

    /// How many columns.
    #[must_use]
    pub fn width(&self) -> usize {
        self.fields.len()
    }

    /// Whether there are no columns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The column types, in order.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        self.fields.iter().map(|field| field.ty.clone()).collect()
    }

    /// The column names, in order.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.fields.iter().map(|field| field.name.clone()).collect()
    }

    /// Where a binding sits in the output.
    ///
    /// A linear scan. An operator's schema is as wide as the query is, which is tens of columns on
    /// the widest ClickBench query, and a hash map of tens of entries rebuilt per operator costs
    /// more than the scans it saves.
    #[must_use]
    pub fn position_of(&self, binding: ColumnBinding) -> Option<usize> {
        self.bindings.iter().position(|held| *held == binding)
    }

    /// The left schema's columns followed by the right schema's, which is what a join produces.
    #[must_use]
    pub fn concat(left: &Self, right: &Self) -> Self {
        let mut fields = left.fields.clone();
        fields.extend(right.fields.iter().cloned());
        let mut bindings = left.bindings.clone();
        bindings.extend(right.bindings.iter().copied());
        Self { fields, bindings }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two() -> Schema {
        Schema::numbered(
            vec![Field::new("a", LogicalType::Integer), Field::new("b", LogicalType::Varchar)],
            7,
        )
    }

    #[test]
    fn a_numbered_schema_binds_its_columns_in_order() {
        let schema = two();
        assert_eq!(schema.position_of(ColumnBinding::new(7, 0)), Some(0));
        assert_eq!(schema.position_of(ColumnBinding::new(7, 1)), Some(1));
        assert_eq!(schema.position_of(ColumnBinding::new(7, 2)), None);
        assert_eq!(schema.position_of(ColumnBinding::new(6, 0)), None);
    }

    #[test]
    fn a_schema_reports_its_names_and_types_in_output_order() {
        let schema = two();
        assert_eq!(schema.names(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(schema.types(), vec![LogicalType::Integer, LogicalType::Varchar]);
        assert_eq!(schema.width(), 2);
    }

    /// Two tables that both number their columns from zero is the ordinary case, and it is the case
    /// a join would get wrong if the position were the whole of the answer.
    #[test]
    fn concatenating_keeps_both_sides_distinguishable() {
        let left = Schema::numbered(vec![Field::new("id", LogicalType::Integer)], 0);
        let right = Schema::numbered(vec![Field::new("id", LogicalType::Integer)], 1);
        let joined = Schema::concat(&left, &right);
        assert_eq!(joined.width(), 2);
        assert_eq!(joined.position_of(ColumnBinding::new(0, 0)), Some(0));
        assert_eq!(joined.position_of(ColumnBinding::new(1, 0)), Some(1));
    }

    #[test]
    fn a_schema_whose_halves_disagree_is_caught() {
        let error = Schema::new(vec![Field::new("a", LogicalType::Integer)], Vec::new())
            .expect_err("one field and no bindings");
        assert!(error.message().contains("1 fields and 0 bindings"), "{error}");
    }
}
