//! Bound expressions.
//!
//! The taxonomy is DuckDB's, minus the classes M0 cannot produce. A comparison is not a function
//! and a conjunction is not a comparison, because the optimizer's rewrites in
//! `spec/09-optimizer.md` section 9.2 are written against exactly those shapes: comparison
//! normalization needs to enumerate comparisons, filter pushdown needs to split conjunctions, and
//! predicate transfer in 9.5 needs to find equijoin comparisons without pattern matching on a
//! function called `=`. Arithmetic is a function, because nothing in the optimizer treats `+`
//! differently from `abs`.

use crate::{ExprRef, Slice, StrRef, ValueRef};

/// Which column, by identity rather than by name.
///
/// The binder gives every operator that introduces columns a table index, and a column is that
/// index plus a position. Two columns called `id` from two sides of a join are two bindings and
/// there is no ambiguity to resolve, which is the entire reason the bound plan has no names in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnBinding {
    /// The index of the operator that produces the column.
    pub table: u32,
    /// The position of the column in that operator's output.
    pub column: u32,
}

impl ColumnBinding {
    /// A binding to the column at `column` of the operator numbered `table`.
    #[must_use]
    pub fn new(table: u32, column: u32) -> Self {
        Self { table, column }
    }
}

/// One bound expression.
///
/// The type of an expression is not in here. It lives in a parallel vector in [`Plan`], indexed by
/// the same [`ExprRef`], because a [`LogicalType`] owns a `Vec` for its nested cases and putting
/// one inside every variant would make the common variants three times larger for the benefit of
/// the rare ones.
///
/// [`Plan`]: crate::Plan
/// [`LogicalType`]: rudb_common::LogicalType
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A reference to a column of some operator's output.
    Column(ColumnBinding),
    /// A literal, folded constant, or bound parameter value.
    Constant(ValueRef),
    /// A cast to the expression's own type.
    ///
    /// The target is the type stored for this expression, not a second copy of it, so there is no
    /// way for a cast to disagree with its own result type.
    Cast {
        /// What is being cast.
        input: ExprRef,
        /// Whether a failed cast yields null instead of raising.
        try_cast: bool,
    },
    /// A binary comparison.
    Compare {
        /// Which comparison.
        op: CompareOp,
        /// Left operand.
        left: ExprRef,
        /// Right operand.
        right: ExprRef,
    },
    /// An `AND` or `OR` over two or more operands.
    ///
    /// Flat rather than binary, because filter pushdown splits a conjunction into its parts and a
    /// right-leaning tree of two-argument `AND`s makes that a recursion instead of a loop.
    Conjunction {
        /// Which connective.
        op: ConjunctionOp,
        /// Two or more operands, into the expression list pool.
        children: Slice,
    },
    /// A scalar function, already resolved to one overload by the binder.
    ///
    /// The name is the resolved function's name and not the name the user wrote, so `a + b` is a
    /// call to `+` and an alias in the catalog has already been followed.
    Function {
        /// The resolved function name.
        name: StrRef,
        /// The arguments, into the expression list pool.
        args: Slice,
    },
    /// An aggregate function.
    ///
    /// An aggregate appears only as a direct element of [`Node::Aggregate`]'s aggregate list.
    /// Anything downstream that wants the result refers to it with a [`ColumnBinding`] into the
    /// aggregate's table index, which is why that node has one. [`Plan::validate`] checks this,
    /// and the textual form relies on it: an aggregate and a scalar function print the same way,
    /// and it is the slot they are printed in that says which is which.
    ///
    /// [`Node::Aggregate`]: crate::Node::Aggregate
    /// [`Plan::validate`]: crate::Plan::validate
    Aggregate {
        /// The resolved aggregate name.
        name: StrRef,
        /// The arguments, into the expression list pool.
        args: Slice,
        /// Whether duplicate input rows are collapsed before aggregating.
        distinct: bool,
        /// The `FILTER (WHERE ...)` predicate, if there is one.
        filter: Option<ExprRef>,
    },
    /// A searched `CASE`.
    ///
    /// There is no simple `CASE` here. `CASE x WHEN 1 THEN ...` is rewritten to the searched form
    /// by the binder, because two representations of one thing is two code paths in every pass
    /// that touches either.
    Case {
        /// The `WHEN`/`THEN` pairs, in order, into the arm pool.
        arms: Slice,
        /// The `ELSE`, if there is one. Absent means null.
        otherwise: Option<ExprRef>,
    },
}

/// One `WHEN`/`THEN` pair of a [`Expr::Case`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm {
    /// The condition.
    pub when: ExprRef,
    /// The result if the condition is true.
    pub then: ExprRef,
}

/// One key of a [`Node::Sort`](crate::Node::Sort).
///
/// Both flags are always set to something concrete. SQL's defaults are a parser concern, and a
/// bound plan that still says "unstated" is a plan whose output order depends on who reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    /// What to sort on.
    pub expr: ExprRef,
    /// Descending rather than ascending.
    pub descending: bool,
    /// Nulls before non-nulls rather than after.
    pub nulls_first: bool,
}

/// Which comparison a [`Expr::Compare`] performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompareOp {
    /// `=`, null in either operand yields null.
    Equal,
    /// `<>`.
    NotEqual,
    /// `<`.
    Less,
    /// `<=`.
    LessOrEqual,
    /// `>`.
    Greater,
    /// `>=`.
    GreaterOrEqual,
    /// `IS DISTINCT FROM`, which is total: two nulls are not distinct.
    DistinctFrom,
    /// `IS NOT DISTINCT FROM`, the null-safe equality.
    NotDistinctFrom,
}

impl CompareOp {
    /// The spelling used in the textual form.
    #[must_use]
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "<>",
            Self::Less => "<",
            Self::LessOrEqual => "<=",
            Self::Greater => ">",
            Self::GreaterOrEqual => ">=",
            Self::DistinctFrom => "IS DISTINCT FROM",
            Self::NotDistinctFrom => "IS NOT DISTINCT FROM",
        }
    }

    /// Every comparison, in the order the reader tries them.
    ///
    /// A spelling that is a prefix of another has to come after it, or `<` matches the front of
    /// `<=` and the reader produces the wrong operator on text that was perfectly well formed.
    /// There is a test below that holds this list to that.
    pub(crate) const SPELLINGS: [Self; 8] = [
        Self::NotDistinctFrom,
        Self::DistinctFrom,
        Self::NotEqual,
        Self::LessOrEqual,
        Self::GreaterOrEqual,
        Self::Equal,
        Self::Less,
        Self::Greater,
    ];

    /// The comparison that holds exactly when this one does with the operands swapped.
    ///
    /// Used by comparison normalization, which wants the constant on one fixed side.
    #[must_use]
    pub fn flip(self) -> Self {
        match self {
            Self::Less => Self::Greater,
            Self::LessOrEqual => Self::GreaterOrEqual,
            Self::Greater => Self::Less,
            Self::GreaterOrEqual => Self::LessOrEqual,
            other => other,
        }
    }
}

/// Which connective a [`Expr::Conjunction`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConjunctionOp {
    /// `AND`.
    And,
    /// `OR`.
    Or,
}

impl ConjunctionOp {
    /// The spelling used in the textual form.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flipping_a_comparison_twice_is_the_comparison() {
        for op in CompareOp::SPELLINGS {
            assert_eq!(op.flip().flip(), op, "{} does not flip back", op.symbol());
        }
    }

    #[test]
    fn the_equalities_are_their_own_flip() {
        for op in [CompareOp::Equal, CompareOp::NotEqual, CompareOp::NotDistinctFrom] {
            assert_eq!(op.flip(), op, "{} should not care about operand order", op.symbol());
        }
    }

    #[test]
    fn every_comparison_has_exactly_one_spelling() {
        let mut seen: Vec<&str> = CompareOp::SPELLINGS.iter().map(|op| op.symbol()).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "two comparisons print the same way");
    }

    /// The reader matches spellings in `SPELLINGS` order and stops at the first hit, so a spelling
    /// that is a prefix of a later one would never be reached. Without this, moving `Less` up the
    /// list would make every `<=` in every dump read back as `<` and the round trip would fail
    /// somewhere far away from the edit that caused it.
    #[test]
    fn no_spelling_is_reachable_only_after_a_prefix_of_it() {
        for (index, op) in CompareOp::SPELLINGS.iter().enumerate() {
            for earlier in &CompareOp::SPELLINGS[..index] {
                assert!(
                    !op.symbol().starts_with(earlier.symbol()),
                    "{} is tried after {}, which is a prefix of it",
                    op.symbol(),
                    earlier.symbol()
                );
            }
        }
    }
}
