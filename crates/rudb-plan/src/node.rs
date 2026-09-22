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

/// How a window frame measures its bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowUnit {
    Rows,
    Range,
    Groups,
}

/// One end of a window frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowBound {
    UnboundedPreceding,
    Preceding(ExprRef),
    CurrentRow,
    Following(ExprRef),
    UnboundedFollowing,
}

/// Which peers a window frame removes after its bounds are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowExclude {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

/// The complete frame shared by a compatible run of window expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowFrame {
    pub unit: WindowUnit,
    pub start: WindowBound,
    pub end: WindowBound,
    pub exclude: WindowExclude,
}

/// One end of a [`Node::Limit`], which is a row count or an offset.
///
/// Nearly every limit written is a number, and a number is what the binder writes down when it can
/// work one out. What it cannot work out is a subquery, which has to run before there is a value,
/// and a call that answers differently every time it is made, such as `RANDOM()` or `nextval`. The
/// pin takes both of those and so does this, by evaluating the expression while the query runs
/// rather than while it is planned.
///
/// [`Bound::Read`] is how. The binder joins the query or the call in underneath as a single row,
/// which puts its one value in a column of every row the limit sees, and the limit reads that
/// column off the first chunk that reaches it and uses the number for the rest of the query. The
/// column is a column the query did not ask for, so the binder puts a projection over the limit
/// that drops it again.
///
/// A [`Bound::Read`] count is why the rewrites that need a number have to check: a limit over a
/// sort only becomes a [`Node::TopN`] when the count is known while the plan is built, and a limit
/// cannot move below the projection that produces the column it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    /// Every row, which is what leaving a `LIMIT` off means. Never an offset.
    All,
    /// A number the binder worked out.
    Rows(u64),
    /// A column of the input holding the number, the same in every row.
    Read(ExprRef),
}

impl Bound {
    /// The number, when it is one already.
    #[must_use]
    pub fn rows(self) -> Option<u64> {
        match self {
            Self::Rows(rows) => Some(rows),
            Self::All | Self::Read(_) => None,
        }
    }

    /// The column this reads, when it reads one.
    #[must_use]
    pub fn read(self) -> Option<ExprRef> {
        match self {
            Self::Read(expr) => Some(expr),
            Self::All | Self::Rows(_) => None,
        }
    }
}

/// The share a `LIMIT` written as a percentage names.
///
/// The two arms are the two things somebody can write. `LIMIT 30 PERCENT` and `LIMIT 30%` are a
/// number the binder works out, and the check that it is between nought and a hundred happens while
/// the query is bound. `LIMIT (SELECT 30)%` is a number nobody has before the query runs, so it
/// arrives as a column of the input exactly the way a [`Bound::Read`] count does, and the range
/// check moves to where the value turns up.
///
/// A share the query wrote as a subquery is always written with the sign rather than the word,
/// because the grammar refuses `PERCENT` after a closing bracket, in both engines. That is a rule
/// about spelling and not about what the node can hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Share {
    /// A percentage the binder worked out, from nought to a hundred.
    Percent(f64),
    /// A column of the input holding the percentage, the same in every row.
    Read(ExprRef),
}

impl Share {
    /// The percentage, when it is one already.
    #[must_use]
    pub fn percent(self) -> Option<f64> {
        match self {
            Self::Percent(percent) => Some(percent),
            Self::Read(_) => None,
        }
    }

    /// The column this reads, when it reads one.
    #[must_use]
    pub fn read(self) -> Option<ExprRef> {
        match self {
            Self::Read(expr) => Some(expr),
            Self::Percent(_) => None,
        }
    }
}

/// One logical operator.
///
/// Children are the inputs, in the order [`Node::children`] returns them, which is the order they
/// print in and the order the reader expects.
///
/// `PartialEq` and not `Eq`, because [`Node::LimitPercent`] holds a percentage as a `f64`.
/// [`Value`](rudb_common::Value) is the same shape for the same reason.
#[derive(Debug, Clone, PartialEq)]
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
    /// cannot refer to a column: a table function that sees the row on its left is `LATERAL`, and
    /// that is [`Node::LateralFunction`].
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
    /// A table function evaluated once per row of its input, which is what `LATERAL` means.
    ///
    /// `FROM t, range(t.n)` is this. A table function's arguments are what produce its rows rather
    /// than something read over rows that already exist, so there is nothing underneath one for a
    /// domain to be pushed into and nothing the rules in the unnesting pass can rewrite it into.
    /// This is the operator those rules stop at: the domain goes in on the left, the arguments read
    /// it, and the call is made once per row of it.
    ///
    /// The output is the input's columns followed by the function's, which is a cross product whose
    /// right side is allowed to change per left row. That is what lets the join putting the rows
    /// back beside their outer row sit above this and read the domain columns where it reads them
    /// everywhere else.
    ///
    /// Only the series family reaches here. A reader takes a file name, the binder settles the
    /// columns by opening the file, and a name that is not a constant is refused there, so a
    /// correlated `read_csv` never gets this far.
    LateralFunction {
        /// The rows the call is made against, one call per row.
        input: NodeRef,
        /// The table index that this call's columns bind against.
        index: u32,
        /// Which function, as its own canonical name.
        function: StrRef,
        /// The arguments, into the expression list pool, read against a row of `input`.
        args: Slice,
        /// The names of the named parameters the call was written with, into the name pool.
        options: Slice,
        /// What each of those names was given, into the expression list pool and the same length.
        settings: Slice,
        /// The produced columns with their types, into the field pool, not counting the input's.
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
    /// Window expressions that share one partition, ordering, and frame.
    Window {
        /// Rows over which the windows are evaluated.
        input: NodeRef,
        /// The table index of the appended window result columns.
        index: u32,
        /// Expressions that divide the input into independent partitions.
        partition: Slice,
        /// The ordering within each partition.
        order: Slice,
        /// The complete frame shared by this compatible expression run.
        frame: WindowFrame,
        /// Direct [`Expr::Window`](crate::Expr::Window) expressions appended to the input columns.
        expressions: Slice,
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
    /// Both are a [`Bound`], which is a number when the query said one and a column of the input
    /// when it wrote something the binder could not settle. See [`Bound`] for what puts the value
    /// in that column and who reads it.
    Limit {
        /// The input.
        input: NodeRef,
        /// How many rows to emit, or all of them.
        count: Bound,
        /// How many rows to skip first.
        offset: Bound,
    },
    /// A limit written as a share of the input rather than as a row count.
    ///
    /// `LIMIT 30 PERCENT` over ten rows is three rows, and it is a node of its own rather than a
    /// [`Node::Limit`] with another field for three reasons. The share is of the whole input, so
    /// this cannot emit anything until it has counted every row, where a plain limit hands each
    /// chunk on as it arrives and stops the scan early. The rewrites that fire on a plain limit are
    /// wrong here: a filter pushed under this one changes how many rows there are to take a share
    /// of, and the sort underneath it cannot become a top n because the count is not known until
    /// the sort has finished. And the pin builds a separate `Limit Percent` operator for it, which
    /// is the same split one layer down.
    ///
    /// A share the binder worked out is between nought and a hundred inclusive, checked while the
    /// query is bound, because that is where the pin refuses `LIMIT 101 PERCENT` too. The offset is
    /// applied after the share has been worked out, so `LIMIT 30 PERCENT OFFSET 2` over ten rows is
    /// three rows starting at the third.
    ///
    /// Both fields can be read off the rows instead of being a number written down here, for the
    /// same reason a plain limit's count can. `LIMIT (SELECT 30)% OFFSET (SELECT 2)` holds two
    /// numbers nobody has before the query runs, so each arrives as a column of the input and is
    /// read off the first chunk. See [`Share`] and [`Bound`].
    LimitPercent {
        /// The input.
        input: NodeRef,
        /// The share of the input to emit, from nought to a hundred.
        percent: Share,
        /// How many rows to skip first. Never [`Bound::All`], which is not an offset.
        offset: Bound,
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
    /// The columns of rows something below already picked out, read back from the file by ordinal.
    ///
    /// This is the top half of late materialisation. A `SELECT * FROM hits ORDER BY EventTime LIMIT
    /// 10` over a hundred and five columns needs one column to decide which ten rows win and all
    /// hundred and five of those ten rows afterwards, and a plan that carries the wide rows through
    /// the top N reads the whole file to throw almost all of it away. The rewrite in
    /// `rudb-opt`'s `late` module narrows the scan under the top N to the ordering columns plus the
    /// row's ordinal inside its file, and puts this above it to read the rest for the rows that
    /// survived.
    ///
    /// The ordinals come out of the input rather than being counted here, because the operator that
    /// counted them is the scan and everything between the scan and here may have dropped rows. The
    /// column that holds them is [`Self::Fetch::row`], and the scan produced it because the rewrite
    /// turned `file_row_number` on.
    ///
    /// The produced columns are the whole row and not only the deferred part, so the answer is one
    /// read of the file at the ordinals rather than a stitch of what was carried with what was
    /// fetched. That costs the ordering column a second read of a few pages and saves the plan above
    /// this from having any idea the rewrite happened.
    Fetch {
        /// The input, which carries each row's ordinal inside the file.
        input: NodeRef,
        /// The table index the produced columns bind against, which is the one the node this
        /// replaced produced, so that nothing above has to be rebound.
        index: u32,
        /// The file, into the expression list pool. One constant path, because a row ordinal only
        /// says which row when there is one file it could be in.
        args: Slice,
        /// The produced columns with their types, into the field pool.
        columns: Slice,
        /// The input column holding the ordinal, which has to be `BIGINT`.
        row: ExprRef,
    },
    /// Rows of a catalog table read back by their table-wide ordinal.
    TableFetch {
        input: NodeRef,
        index: u32,
        catalog: StrRef,
        schema: StrRef,
        table: StrRef,
        columns: Slice,
        row: ExprRef,
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
        /// Which input is gathered whole before the other one starts.
        ///
        /// The binder emits [`BuildSide::Right`] for everything, because at binding time there is
        /// nothing to choose with. `rudb_opt`'s `sides` pass overwrites it from an estimate, and
        /// the executor honours whatever it finds here.
        build: BuildSide,
    },
    /// A join whose right input can refer to columns produced by its left input.
    ///
    /// Binding emits this for a correlated subquery. The unnesting pass has to replace every one
    /// before execution, so the executor never evaluates the right input once per left row.
    DependentJoin {
        /// The outer input whose columns the right side may reference.
        left: NodeRef,
        /// The correlated input.
        right: NodeRef,
        /// Which result shape the subquery needs.
        kind: JoinKind,
        /// Conditions introduced while binding the subquery.
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
    /// A `WITH name AS MATERIALIZED (...)`, which is run once and read wherever it is named.
    ///
    /// The left input is the definition and the right input is the query that reads it. They are
    /// in that order because that is the order they run in: the definition is a pipeline breaker
    /// whichever operators are in it, since nothing above may start until the rows are all there.
    ///
    /// A plain `WITH` is not this. The reference binary inlines one at every use whatever its
    /// shape and however many times it is named, and the only decision left is whether the rows
    /// are needed at all, which is why an unused one is dropped rather than run for nothing.
    MaterializedCte {
        /// The query whose rows are held.
        definition: NodeRef,
        /// The query that reads them, which is where every [`Node::CteScan`] for this one is.
        body: NodeRef,
        /// The name it was written with, which is what the printer and an error message say.
        name: StrRef,
        /// Which materialisation this is, matching the `cte` of the scans that read it.
        ///
        /// A number of its own rather than the table index, because a scan binds against its own
        /// index and two scans of one materialisation have two of those.
        cte: u32,
        /// The held columns with their types, into the field pool.
        columns: Slice,
    },
    /// A read of a [`Node::MaterializedCte`] that has already run.
    ///
    /// A leaf, the same way a table scan is. What it reads was computed by a node above it rather
    /// than by a node under it, which is the one place in the plan where that is true, and it is
    /// why the materialisation holds its body as an input rather than sitting beside it.
    CteScan {
        /// The table index that this read's columns bind against.
        index: u32,
        /// Which materialisation it reads.
        cte: u32,
        /// The name it was written with.
        name: StrRef,
        /// The produced columns with their types, into the field pool.
        columns: Slice,
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
            Self::LateralFunction { .. } => "LateralFunction",
            Self::Filter { .. } => "Filter",
            Self::Project { .. } => "Project",
            Self::Aggregate { .. } => "Aggregate",
            Self::Window { .. } => "Window",
            Self::Sort { .. } => "Sort",
            Self::Limit { .. } => "Limit",
            Self::LimitPercent { .. } => "LimitPercent",
            Self::TopN { .. } => "TopN",
            Self::Fetch { .. } => "Fetch",
            Self::TableFetch { .. } => "TableFetch",
            Self::Distinct { .. } => "Distinct",
            Self::Join { .. } => "Join",
            Self::DependentJoin { .. } => "DependentJoin",
            Self::CrossProduct { .. } => "CrossProduct",
            Self::MaterializedCte { .. } => "MaterializedCte",
            Self::CteScan { .. } => "CteScan",
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
            Self::Get { .. }
            | Self::Dummy
            | Self::Values { .. }
            | Self::TableFunction { .. }
            | Self::CteScan { .. } => [None, None],
            Self::Filter { input, .. }
            | Self::Project { input, .. }
            | Self::Aggregate { input, .. }
            | Self::Window { input, .. }
            | Self::Sort { input, .. }
            | Self::Limit { input, .. }
            | Self::LimitPercent { input, .. }
            | Self::TopN { input, .. }
            | Self::Fetch { input, .. }
            | Self::TableFetch { input, .. }
            | Self::Distinct { input, .. }
            | Self::LateralFunction { input, .. } => [Some(input), None],
            Self::Join { left, right, .. }
            | Self::DependentJoin { left, right, .. }
            | Self::CrossProduct { left, right }
            | Self::SetOp { left, right, .. } => [Some(left), Some(right)],
            Self::MaterializedCte { definition, body, .. } => [Some(definition), Some(body)],
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
            | Self::LateralFunction { index, .. }
            | Self::Project { index, .. }
            | Self::Fetch { index, .. }
            | Self::TableFetch { index, .. }
            | Self::Aggregate { index, .. }
            | Self::Window { index, .. }
            | Self::CteScan { index, .. }
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
    /// Every left row plus a nullable boolean saying whether its condition matched the right side.
    /// A null means no row matched and at least one comparison was unknown.
    Mark,
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
            Self::Mark => "MARK",
            Self::Positional => "POSITIONAL",
        }
    }

    /// Every join kind, which is what the reader searches.
    pub(crate) const ALL: [Self; 9] = [
        Self::Inner,
        Self::Left,
        Self::Right,
        Self::Full,
        Self::Semi,
        Self::Anti,
        Self::Single,
        Self::Mark,
        Self::Positional,
    ];

    /// The same join with its two inputs the other way round, for the kinds where there is one.
    ///
    /// Swapping the inputs of a `LEFT` join makes a `RIGHT` join and the other way round, because
    /// the kind names a side. `INNER` and `FULL` name neither and are their own mirror. The rest
    /// return `None`: `SEMI`, `ANTI`, `SINGLE` and `MARK` produce the left side's rows, or a
    /// column about them, so their left input is not a side but the subject, and `POSITIONAL`
    /// pairs the nth with the nth, which no reordering of one input preserves.
    #[must_use]
    pub fn mirrored(self) -> Option<Self> {
        match self {
            Self::Inner => Some(Self::Inner),
            Self::Left => Some(Self::Right),
            Self::Right => Some(Self::Left),
            Self::Full => Some(Self::Full),
            Self::Semi | Self::Anti | Self::Single | Self::Mark | Self::Positional => None,
        }
    }
}

/// Which input of a join is gathered whole before the other one starts.
///
/// A join is two inputs and a dependency edge between them: one side is finished and held, and then
/// the other side's rows are matched against what was held. This says which side that is. It is
/// where the hash table goes when the hash join in #62 lands, and it is the side today's nested
/// loop turns into chunks and rescans once per row of the other one.
///
/// Which side that should be is not a property of the join and is not decided here. It is decided
/// by [`sides`](../../rudb_opt/sides/index.html) from a cardinality estimate, and the rule it uses
/// belongs to whichever operator is reading this, not to the flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BuildSide {
    /// The right input, which is what the binder emits and what every join did before this existed.
    #[default]
    Right,
    /// The left input, which means the executor swaps the two and puts the answer back in order.
    Left,
}

impl BuildSide {
    /// The spelling used in the textual form.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Right => "right",
            Self::Left => "left",
        }
    }

    /// Both sides, which is what the reader searches.
    pub(crate) const ALL: [Self; 2] = [Self::Right, Self::Left];
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
            Node::LateralFunction {
                input: 0,
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
            Node::Limit { input: 0, count: Bound::All, offset: Bound::Rows(0) },
            Node::LimitPercent { input: 0, percent: Share::Percent(50.0), offset: Bound::Rows(0) },
            Node::Distinct { input: 0, on: Slice::EMPTY },
            Node::Join {
                left: 0,
                right: 1,
                kind: JoinKind::Inner,
                conditions: Slice::EMPTY,
                build: BuildSide::default(),
            },
            Node::DependentJoin {
                left: 0,
                right: 1,
                kind: JoinKind::Single,
                conditions: Slice::EMPTY,
            },
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
                    | Node::LateralFunction { .. }
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
        assert_eq!(JoinKind::ALL.len(), 9);
        assert_eq!(SetOpKind::ALL.len(), 3);
        let mut names: Vec<&str> = JoinKind::ALL.iter().map(|k| k.keyword()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), JoinKind::ALL.len(), "two join kinds print the same keyword");
    }
}
