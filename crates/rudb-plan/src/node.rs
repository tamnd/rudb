//! Logical operators.
//!
//! One variant per operator, covering what the M0 binder can produce out of what the transformer
//! in `rudb-parse` can produce. That is a smaller set than DuckDB's and it is smaller on purpose:
//! an operator here that nothing constructs is an operator whose textual form, whose validation
//! and whose rewrite rules have never been run, and the first thing that happens when the binder
//! finally emits one is that all three turn out to be wrong.
//!
//! Every operator that introduces new columns carries a table index, which is the left half of a
//! [`ColumnBinding`](crate::ColumnBinding). [`Node::Filter`], [`Node::Sort`], [`Node::Limit`],
//! [`Node::TopN`], [`Node::Distinct`] and [`Node::Join`] do not have one, because they pass their
//! input's columns through unchanged and a binding that survives a filter should not have to be
//! rewritten by it.

use crate::{ExprRef, NodeRef, Slice, StrRef};

/// One logical operator.
///
/// Children are the inputs, in the order [`Node::children`] returns them, which is the order they
/// print in and the order the reader expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A base table scan.
    ///
    /// The projection is in `columns`, so a scan of two columns of a 105-column table is a two
    /// column scan in the plan and not a filter over a wide one. `spec/09-optimizer.md` section
    /// 9.2 calls projection pushdown the difference between 20 GB and 200 MB on ClickBench, and
    /// this is the field it pushes into.
    Get {
        /// The catalog name.
        catalog: StrRef,
        /// The schema name.
        schema: StrRef,
        /// The table name.
        table: StrRef,
        /// The alias the query used, which is what an error message should say.
        alias: StrRef,
        /// The table index that this scan's columns bind against.
        index: u32,
        /// The projected columns with their types, into the field pool.
        columns: Slice,
    },
    /// One row and no columns.
    ///
    /// What `SELECT 1` sits on top of. Not an empty result: an empty result produces no rows and
    /// `SELECT 1` produces one, and conflating them is how a scalar subquery starts returning
    /// nothing instead of null.
    Dummy,
    /// Literal rows.
    ///
    /// Every row has the same length as `columns`, which [`Plan::validate`](crate::Plan::validate)
    /// checks, because a ragged `VALUES` is a wrong answer rather than a crash.
    Values {
        /// The table index that these columns bind against.
        index: u32,
        /// The output columns with their types, into the field pool.
        columns: Slice,
        /// The rows, into the row pool, each row a slice of the expression list pool.
        rows: Slice,
    },
    /// A function call where a table goes, such as `range(10)`.
    ///
    /// The arguments are expressions rather than numbers, because `range(2 + 3)` is a legal call
    /// and folding it here would mean the plan could not be printed back as what was written. They
    /// cannot refer to a column: a table function that sees the row on its left is `LATERAL`, which
    /// is a different node and is not here yet.
    ///
    /// A separate node from [`Node::Values`] even though `range(3)` and `VALUES (0), (1), (2)`
    /// produce the same rows, because the one that produces three million rows should be three
    /// numbers in the plan rather than three million expressions in it.
    TableFunction {
        /// The table index that this call's columns bind against.
        index: u32,
        /// Which function, as its own canonical name.
        function: StrRef,
        /// The arguments, into the expression list pool.
        args: Slice,
        /// The names of the named parameters the call was written with, into the name pool.
        ///
        /// `read_csv('f.csv', delim=';')` keeps the `delim` here rather than only in whatever the
        /// binder made of it, because the executor opens the file a second time and has to open it
        /// the same way. A parameter the binder answers on its own, such as `binary_as_string`,
        /// is here too, so that a plan prints back as the call that was written.
        options: Slice,
        /// What each of those names was given, into the expression list pool and the same length.
        ///
        /// Constants, every one of them. The binder refuses anything else, because a parameter can
        /// decide what the columns are and the columns are settled there.
        settings: Slice,
        /// The produced columns with their types, into the field pool.
        columns: Slice,
    },
    /// A predicate over the input, keeping the rows where it is true.
    ///
    /// True, not "not false". A null predicate drops the row, which is SQL's rule and is the
    /// difference between `WHERE` and `CHECK`.
    Filter {
        /// The input.
        input: NodeRef,
        /// The predicate, which has to be `BOOLEAN`.
        predicate: ExprRef,
    },
    /// A projection, producing a new set of columns from the input's.
    Project {
        /// The input.
        input: NodeRef,
        /// The table index the produced columns bind against.
        index: u32,
        /// The expressions, into the expression list pool.
        exprs: Slice,
        /// One output name per expression, into the name list pool.
        ///
        /// Names are carried through the whole plan rather than attached at the root, because the
        /// thing a person reads a plan dump to answer is usually which column this is, and a dump
        /// with the names stripped out answers that with a number.
        names: Slice,
    },
    /// A grouped or ungrouped aggregation.
    ///
    /// The output is the group expressions followed by the aggregates, in that order, and that is
    /// what a binding into `index` means. An ungrouped aggregate has an empty `groups` and still
    /// produces exactly one row, including over an empty input.
    Aggregate {
        /// The input.
        input: NodeRef,
        /// The table index the produced columns bind against.
        index: u32,
        /// The group expressions, into the expression list pool.
        groups: Slice,
        /// The aggregate expressions, into the expression list pool. Every element is an
        /// [`Expr::Aggregate`](crate::Expr::Aggregate) and this is the only place one may appear.
        aggregates: Slice,
    },
    /// An ordering.
    Sort {
        /// The input.
        input: NodeRef,
        /// The keys in priority order, into the sort key pool.
        keys: Slice,
    },
    /// A row count limit and an offset.
    ///
    /// Both are constants. `LIMIT` over an expression is legal SQL and DuckDB evaluates it before
    /// the plan runs, so by the time it is here it is a number or the query did not bind.
    Limit {
        /// The input.
        input: NodeRef,
        /// How many rows to emit, or all of them.
        count: Option<u64>,
        /// How many rows to skip first.
        offset: u64,
    },
    /// A sort with a limit over it, which never holds more rows than the limit can emit.
    ///
    /// The same answer as a [`Node::Limit`] over a [`Node::Sort`] and a different amount of work.
    /// A sort has to see every row before it can emit the first one, so it holds the whole input;
    /// this holds the rows that could still come out and throws the rest away as it goes, which on
    /// `ORDER BY x LIMIT 10` over a hundred million rows is ten rows rather than a hundred million.
    ///
    /// `count` is not optional, because `LIMIT ALL` over a sort is a sort and there would be nothing
    /// to bound. The offset is part of the node rather than left above it, since the rows that are
    /// skipped still have to be found to be skipped, so what this has to keep is `count + offset`.
    TopN {
        /// The input.
        input: NodeRef,
        /// The keys in priority order, into the sort key pool.
        keys: Slice,
        /// How many rows to emit.
        count: u64,
        /// How many rows to skip first.
        offset: u64,
    },
    /// Duplicate elimination, over the whole row or over named expressions.
    Distinct {
        /// The input.
        input: NodeRef,
        /// The `DISTINCT ON` expressions, into the expression list pool. Empty means the whole
        /// row, which is plain `DISTINCT`.
        on: Slice,
    },
    /// A join with a condition.
    Join {
        /// The left input.
        left: NodeRef,
        /// The right input.
        right: NodeRef,
        /// Which join.
        kind: JoinKind,
        /// The conditions, into the expression list pool, combined with `AND`. Empty is a join
        /// with no condition, which for an inner join is a cross product and for an outer join
        /// is not.
        conditions: Slice,
    },
    /// An unconditional cross product.
    ///
    /// Separate from a [`Node::Join`] with no conditions because join ordering treats them
    /// differently: a cross product has no edge in the join graph and section 9.4's dynamic
    /// program enumerates connected subgraphs.
    CrossProduct {
        /// The left input.
        left: NodeRef,
        /// The right input.
        right: NodeRef,
    },
    /// `UNION`, `EXCEPT` or `INTERSECT`.
    SetOp {
        /// The left input.
        left: NodeRef,
        /// The right input.
        right: NodeRef,
        /// Which operation.
        kind: SetOpKind,
        /// Whether duplicates are kept.
        all: bool,
        /// The table index the produced columns bind against, since the output is neither side's
        /// columns.
        index: u32,
    },
}

impl Node {
    /// The keyword this operator prints as, which is also what the reader dispatches on.
    #[must_use]
    pub fn keyword(&self) -> &'static str {
        match self {
            Self::Get { .. } => "Get",
            Self::Dummy => "Dummy",
            Self::Values { .. } => "Values",
            Self::TableFunction { .. } => "TableFunction",
            Self::Filter { .. } => "Filter",
            Self::Project { .. } => "Project",
            Self::Aggregate { .. } => "Aggregate",
            Self::Sort { .. } => "Sort",
            Self::Limit { .. } => "Limit",
            Self::TopN { .. } => "TopN",
            Self::Distinct { .. } => "Distinct",
            Self::Join { .. } => "Join",
            Self::CrossProduct { .. } => "CrossProduct",
            Self::SetOp { .. } => "SetOp",
        }
    }

    /// The inputs, in printing order.
    ///
    /// Two slots rather than a `Vec`, because no logical operator in this set has three inputs and
    /// the printer walks this on every node of every dump. A caller wants
    /// `node.children().into_iter().flatten()`.
    #[must_use]
    pub fn children(&self) -> [Option<NodeRef>; 2] {
        match *self {
            Self::Get { .. } | Self::Dummy | Self::Values { .. } | Self::TableFunction { .. } => {
                [None, None]
            }
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::TopN { input, .. }
            | Self::Distinct { input, .. } => [Some(input), None],
            Self::Join { left, right, .. }
            | Self::CrossProduct { left, right }
            | Self::SetOp { left, right, .. } => [Some(left), Some(right)],
        }
    }

    /// How many inputs this operator takes.
    #[must_use]
    pub fn arity(&self) -> usize {
        self.children().into_iter().flatten().count()
    }

    /// The table index this operator introduces, if it introduces one.
    #[must_use]
    pub fn table_index(&self) -> Option<u32> {
        match *self {
            Self::Get { index, .. }
            | Self::Values { index, .. }
            | Self::TableFunction { index, .. }
            | Self::Project { index, .. }
            | Self::Aggregate { index, .. }
            | Self::SetOp { index, .. } => Some(index),
            _ => None,
        }
    }
}

/// Which join.
///
/// `Semi` and `Anti` are here because subquery unnesting produces them directly, per section 9.2,
/// and a semi join expressed as a join plus a distinct is a semi join the executor cannot
/// recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    /// Rows that match on both sides.
    Inner,
    /// Every left row, padded with nulls where the right does not match.
    Left,
    /// Every right row, padded with nulls where the left does not match.
    Right,
    /// Both of the above at once.
    Full,
    /// Left rows that have at least one match, each emitted once.
    Semi,
    /// Left rows that have no match.
    Anti,
    /// Left rows paired with their match, or with nulls, at most one right row each. What a
    /// correlated scalar subquery unnests to.
    Single,
    /// The nth left row with the nth right row, which is DuckDB's `POSITIONAL JOIN`.
    Positional,
}

impl JoinKind {
    /// The spelling used in the textual form.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Inner => "INNER",
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
            Self::Full => "FULL",
            Self::Semi => "SEMI",
            Self::Anti => "ANTI",
            Self::Single => "SINGLE",
            Self::Positional => "POSITIONAL",
        }
    }

    /// Every join kind, which is what the reader searches.
    pub(crate) const ALL: [Self; 8] = [
        Self::Inner,
        Self::Left,
        Self::Right,
        Self::Full,
        Self::Semi,
        Self::Anti,
        Self::Single,
        Self::Positional,
    ];
}

/// Which set operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SetOpKind {
    /// Rows from either side.
    Union,
    /// Rows from the left that are not on the right.
    Except,
    /// Rows on both sides.
    Intersect,
}

impl SetOpKind {
    /// The spelling used in the textual form.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Union => "UNION",
            Self::Except => "EXCEPT",
            Self::Intersect => "INTERSECT",
        }
    }

    /// Every set operation, which is what the reader searches.
    pub(crate) const ALL: [Self; 3] = [Self::Union, Self::Except, Self::Intersect];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Slice;

    /// Every node in one list, so that a variant added without a keyword, without a child slot or
    /// without an entry in the reader's dispatch table fails here rather than at the first dump
    /// that happens to contain one.
    fn one_of_each() -> Vec<Node> {
        vec![
            Node::Get {
                catalog: 0,
                schema: 0,
                table: 0,
                alias: 0,
                index: 0,
                columns: Slice::EMPTY,
            },
            Node::Dummy,
            Node::Values { index: 0, columns: Slice::EMPTY, rows: Slice::EMPTY },
            Node::TableFunction {
                index: 0,
                function: 0,
                args: Slice::EMPTY,
                options: Slice::EMPTY,
                settings: Slice::EMPTY,
                columns: Slice::EMPTY,
            },
            Node::Filter { input: 0, predicate: 0 },
            Node::Project { input: 0, index: 0, exprs: Slice::EMPTY, names: Slice::EMPTY },
            Node::Aggregate { input: 0, index: 0, groups: Slice::EMPTY, aggregates: Slice::EMPTY },
            Node::Sort { input: 0, keys: Slice::EMPTY },
            Node::Limit { input: 0, count: None, offset: 0 },
            Node::Distinct { input: 0, on: Slice::EMPTY },
            Node::Join { left: 0, right: 1, kind: JoinKind::Inner, conditions: Slice::EMPTY },
            Node::CrossProduct { left: 0, right: 1 },
            Node::SetOp { left: 0, right: 1, kind: SetOpKind::Union, all: true, index: 0 },
        ]
    }

    #[test]
    fn every_operator_has_its_own_keyword() {
        let mut keywords: Vec<&str> = one_of_each().iter().map(Node::keyword).collect();
        let count = keywords.len();
        keywords.sort_unstable();
        keywords.dedup();
        assert_eq!(keywords.len(), count, "two operators print the same keyword");
    }

    #[test]
    fn arity_agrees_with_the_child_slots() {
        for node in one_of_each() {
            let counted = node.children().into_iter().flatten().count();
            assert_eq!(node.arity(), counted, "{} disagrees with itself", node.keyword());
        }
    }

    /// A child slot that is `None` before a slot that is `Some` would make the printer emit the
    /// right input as the left one, and the reader would accept it.
    #[test]
    fn the_child_slots_are_filled_from_the_front() {
        for node in one_of_each() {
            let slots = node.children();
            assert!(
                !(slots[0].is_none() && slots[1].is_some()),
                "{} has a right input and no left one",
                node.keyword()
            );
        }
    }

    #[test]
    fn only_the_operators_that_introduce_columns_have_a_table_index() {
        for node in one_of_each() {
            let expected = matches!(
                node,
                Node::Get { .. }
                    | Node::Values { .. }
                    | Node::TableFunction { .. }
                    | Node::Project { .. }
                    | Node::Aggregate { .. }
                    | Node::SetOp { .. }
            );
            assert_eq!(
                node.table_index().is_some(),
                expected,
                "{} is on the wrong side of the table index rule",
                node.keyword()
            );
        }
    }

    #[test]
    fn every_join_kind_and_set_operation_is_in_the_list_the_reader_searches() {
        assert_eq!(JoinKind::ALL.len(), 8);
        assert_eq!(SetOpKind::ALL.len(), 3);
        let mut names: Vec<&str> = JoinKind::ALL.iter().map(|k| k.keyword()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), JoinKind::ALL.len(), "two join kinds print the same keyword");
    }
}
