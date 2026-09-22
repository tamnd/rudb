//! Choosing the join that reads a link over the join that builds a hash table.
//!
//! spec/graph/06-the-optimizer.md section 6.4. Both joins answer the same question and the plan has
//! to pick one, and the thing being picked between is not two algorithms so much as two places in
//! the memory hierarchy: a hash join reads the parent into a table and probes it, which is fast
//! while that table is cache resident and a cache miss per row once it is not, and a link join
//! reads one row id per child row and gathers, which costs the miss whatever the parent's size is
//! and never pays for the build.
//!
//! So the rule is about where the parent sits:
//!
//! - The parent fits in the last level cache after projection: hash join. Twenty five nations are
//!   not worth a link.
//! - The parent does not fit and the child is clustered by the parent's row id, so the gathers walk
//!   forwards: link join.
//! - The parent does not fit and the child is not clustered: the default is the link join when the
//!   projected parent is narrow, because what a link join reads per row is one row id and what a
//!   hash join reads is a key, a bucket and a row.
//!
//! A semi or an anti join skips all of it, because it never touches the parent at all. A link join
//! answering one is a sentinel test per child row against a structure that is already in the file,
//! and there is no parent side to size.
//!
//! # What this pass will not do
//!
//! It will not read a link for a join whose parent is anything but a bare scan. A gather by row id
//! reads the parent's stored rows, so a filter or a projection between the scan and the join is a
//! restriction the gather would ignore, which is a wrong answer rather than a slow one. That is not
//! a gap so much as the shape of the direction: a join that is selective on the parent side is what
//! section 5.4's reduction is for, and the reduction is the next milestone.
//!
//! It will also not read a link through a projection on the child side, because the row id is a
//! column and a projection that was not asked to carry it does not. Widening a projection is one
//! more rewrite than this pass needs for the queries it was written for, where the projections sit
//! above the joins rather than between them, and doing it here rather than when there is a plan
//! that needs it would be a rewrite nothing tests.
//!
//! And it will not fire where the extra column would be seen. A link join carries the child's row
//! id in its output, one column wider than the join it replaces, which is invisible to an operator
//! that reads its input by binding and very visible to one that reads it by position or by all of
//! it. So the rewrite asks what is above the join first: somewhere between it and the root there
//! has to be an operator that says what its own columns are, and nothing on the way up may be a set
//! operation, a distinct, or the definition of a common table expression.
//!
//! # Why the relationships come in rather than being read
//!
//! The optimizer cannot see the catalog, by design: what it gets is a set of facts read once per
//! statement, and [`Context`] is the seam. A relationship is one more fact, and it has to be one
//! rather than a declaration because a declaration is what an author believes. So a [`Linked`]
//! carries both: what somebody declared, and whether the child's file holds a link for it, which is
//! only true when the build verified the parent side was unique. A rewrite reads the second one and
//! nothing here trusts a `SET`.
//!
//! # Why the reasons are a type rather than an early return
//!
//! Section 6.7 asks the plan output to say, per join, which algorithm was chosen and why the other
//! one was not, and gives the argument: this layer's failure mode is silence. A hash join that is
//! slow is visibly a hash join, and a link that went unread because a projection two nodes up
//! dropped the row id looks from the outside exactly like a link that was never built.
//!
//! So one walk hands back a [`Why`] for every join it looks at, the rewrite acts on the ones
//! that say yes, and [`why`] is what `EXPLAIN` asks afterwards. One implementation and two callers,
//! because two walks that both worked out why a join is a hash join would agree on the day they
//! were written and disagree some time after, and the one that would be wrong is the one somebody
//! reads when they are trying to find out why a query is slow.

use rudb_common::{Field, LogicalType, Result};
use rudb_functions::FILE_ROW_NUMBER;
use rudb_plan::{
    Carried, ColumnBinding, CompareOp, Expr, JoinKind, Node, NodeRef, Plan, Slice, rids_of,
};

use crate::estimate;
use crate::pass::{Context, Pass};

/// How much of the parent has to fit for a hash table over it to stay cache resident.
///
/// One number for a machine's last level cache, which is not a number a plan can read: the pass
/// runs on whatever the query is running on and there is no portable way to ask. Eight megabytes is
/// the common server figure and is deliberately on the generous side, because being wrong towards
/// the hash join is being wrong about a dimension table and being wrong the other way is a gather
/// per row over something that was going to be in cache.
///
/// Owed a setting and a measurement, both of them document 09 section 9.3.
pub const CACHE_BYTES: u64 = 8 * 1024 * 1024;

/// How wide the parent's projection may be before a hash join is worth its build.
///
/// The crossover of the third bullet of section 6.4 and the one this pass guesses at. Under this,
/// what the link join reads per child row is smaller than what the probe reads; over it, the gather
/// is moving enough bytes that the build pays for itself.
///
/// Owed a setting and a measurement, both of them document 09 section 9.3.
pub const NARROW_BYTES: usize = 32;

/// The two numbers section 6.4 is decided by, as a statement has left them.
///
/// One type rather than two arguments because they are read together, changed together and are the
/// same kind of thing: a guess about a machine that a measurement is supposed to replace. The
/// defaults are [`CACHE_BYTES`] and [`NARROW_BYTES`], and `RESET` puts them back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    /// How much of the parent has to fit for its hash table to stay cache resident.
    pub cache_bytes: u64,
    /// How wide the parent's projection may be before a hash join is worth its build.
    pub narrow_bytes: usize,
}

impl Default for Sizes {
    fn default() -> Self {
        Self { cache_bytes: CACHE_BYTES, narrow_bytes: NARROW_BYTES }
    }
}

/// A relationship somebody declared, and whether the child's file holds a link for it.
///
/// The two are separate on purpose. A declaration is what an author believes and a link is what the
/// build found, and only the second one licenses a rewrite: a link is written once the build has
/// read the parent side and found it unique, so a plan that reads one may rely on a child row
/// having at most one parent. What the declaration alone is good for is the plan output, because
/// section 6.7's complaint is that a relationship nobody built and a relationship nobody declared
/// look identical from the outside, and a reader who declared one wants to be told which it was.
///
/// What the build found comes in two parts, which `../stats/07-graph-statistics.md` section 7.3
/// calls the uniqueness and totality certificates. They are separate because a rewrite that reads a
/// link needs only the first and a rewrite that deletes an operator needs both, and a relationship
/// can have the first without the second: a link is written for a child column some of whose rows
/// match nothing, and it says so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    /// The many side's table, as the catalog holds it.
    pub child: String,
    /// The many side's key column.
    pub child_column: String,
    /// The one side's table.
    pub parent: String,
    /// The one side's key column.
    pub parent_column: String,
    /// Whether the child's file holds a readable forward link for it.
    ///
    /// This is also the uniqueness certificate of `../stats/07-graph-statistics.md` section 7.3,
    /// because a link only exists over a parent key the build read and found distinct. The two are
    /// one field rather than two because nothing can make them disagree: there is no way to hold a
    /// link and not hold the certificate that licensed writing it.
    pub built: bool,
    /// The totality certificate: every child row found a parent, the unmatched count is zero.
    ///
    /// Separate from [`Self::built`] because a link exists for a relationship that is partial and
    /// says so, where a key that repeats gets no link at all. False is the safe answer and is what
    /// a relationship with no link gets, since totality is a fact about the children and the thing
    /// that counted them is the link.
    pub total: bool,
}

impl Linked {
    /// A relationship the file holds a link for, which is the one a rewrite may read.
    pub fn built(
        child: impl Into<String>,
        child_column: impl Into<String>,
        parent: impl Into<String>,
        parent_column: impl Into<String>,
    ) -> Self {
        Self { built: true, ..Self::declared(child, child_column, parent, parent_column) }
    }

    /// A relationship the file holds a link for whose every child row found a parent.
    ///
    /// Both certificates of section 7.3, which together are a foreign key that was verified rather
    /// than declared, and which are what license removing an operator rather than accelerating one.
    pub fn verified(
        child: impl Into<String>,
        child_column: impl Into<String>,
        parent: impl Into<String>,
        parent_column: impl Into<String>,
    ) -> Self {
        Self { total: true, ..Self::built(child, child_column, parent, parent_column) }
    }

    /// A relationship somebody declared and no file holds a link for.
    pub fn declared(
        child: impl Into<String>,
        child_column: impl Into<String>,
        parent: impl Into<String>,
        parent_column: impl Into<String>,
    ) -> Self {
        Self {
            child: child.into(),
            child_column: child_column.into(),
            parent: parent.into(),
            parent_column: parent_column.into(),
            built: false,
            total: false,
        }
    }

    /// Whether this is the relationship between those two columns.
    ///
    /// Names are matched without regard to case, the same way the catalog resolves one.
    #[must_use]
    fn between(&self, child: (&str, &str), parent: (&str, &str)) -> bool {
        self.child.eq_ignore_ascii_case(child.0)
            && self.child_column.eq_ignore_ascii_case(child.1)
            && self.parent.eq_ignore_ascii_case(parent.0)
            && self.parent_column.eq_ignore_ascii_case(parent.1)
    }
}

/// What was decided about one join, in the words section 6.7 asks `EXPLAIN` to print.
///
/// Section 6.7's argument for having this at all is that the failure mode of this layer is silence.
/// A hash join that is slow is visibly a hash join, and a link that went unread because a projection
/// two nodes up dropped the row id looks exactly like a link that was never built. So every reason
/// this pass can decline for is a variant here rather than an early return with nothing said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// A forward link answers neither a right nor a full join, because both want the parent rows
    /// nothing pointed at and a link is only ever read from the child.
    Kind,
    /// Not one equality over two plain columns, so there is no single column a link is indexed by.
    Key,
    /// Nobody declared a relationship between the two columns the join equates.
    None,
    /// Declared, and the child's file holds no link for it.
    NotBuilt,
    /// The parent side is not a stored table read whole, so a gather by row id would read rows the
    /// plan had already restricted.
    ParentNotStored,
    /// The child's scan is behind something that moves a row, so its row id no longer names a row.
    ChildNotStored,
    /// Built, and the rows arriving at the join are no longer rows of the child table.
    RowIdGone,
    /// The row id the link join carries would reach somewhere that counts its input's columns.
    ColumnWouldShow,
    /// The parent fits in cache after projection, so the build costs less than the gathers.
    Fits {
        /// The rows the parent was estimated at.
        rows: u64,
        /// What those rows take after projection.
        bytes: u64,
    },
    /// The parent does not fit, and its projection is wide enough that the build pays for itself.
    Wide {
        /// The projected parent's width in bytes.
        width: usize,
        /// The width the build starts paying for itself at.
        narrow: usize,
    },
    /// Chosen: the parent does not fit and its projection is narrow.
    Narrow {
        /// The projected parent's width in bytes.
        width: usize,
        /// The width the build starts paying for itself at.
        narrow: usize,
    },
    /// Chosen: a semi or an anti join is a sentinel test and never reads the parent at all.
    NeverRead,
}

impl Why {
    /// Whether this is a reason to read the link rather than a reason not to.
    #[must_use]
    pub const fn chosen(self) -> bool {
        matches!(self, Self::Narrow { .. } | Self::NeverRead)
    }

    /// How far the pass got before this was the answer.
    ///
    /// An inner join is tried both ways round and only one of the two can be the relationship, so
    /// one side always fails at the first question. Reporting that one would tell a reader the join
    /// has no relationship when it has one and something later declined it, which is the sentence
    /// section 6.7 exists to prevent. The further of the two is the one that says something.
    const fn rank(self) -> u8 {
        match self {
            Self::Kind | Self::Key | Self::None => 0,
            Self::ParentNotStored | Self::ChildNotStored => 1,
            Self::NotBuilt => 2,
            Self::RowIdGone => 3,
            Self::ColumnWouldShow => 4,
            Self::Fits { .. } | Self::Wide { .. } => 5,
            Self::Narrow { .. } | Self::NeverRead => 6,
        }
    }

    /// The further on of two answers about the same join.
    fn or(self, other: Self) -> Self {
        if other.rank() > self.rank() { other } else { self }
    }
}

impl std::fmt::Display for Why {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Kind => write!(out, "a forward link does not answer a right or a full join"),
            Self::Key => write!(out, "the join is not one equality over two columns"),
            Self::None => write!(out, "no relationship is declared between those two columns"),
            Self::NotBuilt => {
                write!(out, "the relationship is declared and its link is not in the file")
            }
            Self::ParentNotStored => {
                write!(out, "the parent side is not a stored table read whole")
            }
            Self::ChildNotStored => write!(out, "the child side is not a stored table"),
            Self::RowIdGone => write!(
                out,
                "the link is in the file and the rows here are no longer rows of the child table"
            ),
            Self::ColumnWouldShow => {
                write!(out, "the row id would reach an operator that counts its input's columns")
            }
            Self::Fits { rows, bytes } => write!(
                out,
                "the parent is {rows} rows and {bytes} bytes projected, which fits in cache"
            ),
            Self::Wide { width, narrow } => write!(
                out,
                "the parent does not fit in cache and its projection is {width} bytes, \
                 which is not under {narrow}"
            ),
            Self::Narrow { width, narrow } => write!(
                out,
                "the parent does not fit in cache and its projection is {width} bytes, \
                 which is under {narrow}"
            ),
            Self::NeverRead => write!(out, "a semi or an anti join never reads the parent"),
        }
    }
}

/// What the planner decided about one join, for whoever asks after the fact.
///
/// `None` for anything that is not a join, which is most of a plan. Read on the finished plan, so a
/// join that is still a [`Node::Join`] is one this pass declined and a [`Node::LinkJoin`] is one it
/// took. Both answers come out of the same code the decision was made with, because two walks that
/// agreed on the day they were written are two walks that disagree afterwards, and the one that
/// would be wrong is the one somebody reads when a query is slow.
#[must_use]
pub fn why(plan: &Plan, at: NodeRef, context: &Context) -> Option<Why> {
    match *plan.node(at) {
        Node::Join { .. } => {
            let carried = rids_of(plan);
            let consumers = consumers(plan);
            Some(decided(plan, at, &carried, &consumers, context).0)
        }
        // Already taken, so the question is which bullet of section 6.4 took it. A semi or an anti
        // is the bullet that does not size anything, and everything else got here through the width.
        Node::LinkJoin { parent, kind, .. } => Some(match kind {
            JoinKind::Semi | JoinKind::Anti => Why::NeverRead,
            _ => match *plan.node(parent) {
                Node::Get { columns, .. } => {
                    let sizes = context.sizes();
                    Why::Narrow { width: width(plan, columns), narrow: sizes.narrow_bytes }
                }
                _ => Why::ParentNotStored,
            },
        }),
        _ => None,
    }
}

/// Replaces a hash join with a link join where the file already answers it.
#[derive(Debug)]
pub struct LinkJoinRewrite;

impl Pass for LinkJoinRewrite {
    fn name(&self) -> &'static str {
        "link_join"
    }

    fn run(&self, plan: &mut Plan, context: &Context) -> Result<()> {
        if context.links().is_empty() {
            return Ok(());
        }
        // One walk before anything is rewritten, and it stays true across the rewrites: what a node
        // carries is a question about the operators under it, and nothing below a join changes
        // here. A node the walk never reached carries nothing, so an orphan left behind by an
        // earlier pass answers no to the check below and is never rewritten.
        let carried = rids_of(plan);
        let consumers = consumers(plan);
        for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
            rewrite(plan, node, &carried, &consumers, context);
        }
        Ok(())
    }
}

/// One join, rewritten if the file answers it and section 6.4 says it should be.
fn rewrite(
    plan: &mut Plan,
    at: NodeRef,
    carried: &[Carried],
    consumers: &[Option<NodeRef>],
    context: &Context,
) {
    let (why, taken) = decided(plan, at, carried, consumers, context);
    if !why.chosen() {
        return;
    }
    let Some(taken) = taken else { return };
    let Some(binding) = number(plan, taken.scan) else {
        return;
    };
    let rid = plan.add_expr(Expr::Column(binding), LogicalType::BigInt);
    let Node::Join { kind, conditions, .. } = *plan.node(at) else { return };
    *plan.node_mut(at) =
        Node::LinkJoin { child: taken.child, parent: taken.parent, kind, conditions, rid };
}

/// Which two inputs a link would be read between, once it is worth reading one.
#[derive(Debug, Clone, Copy)]
struct Taken {
    child: NodeRef,
    parent: NodeRef,
    /// The child's scan, which is the node the row id column is added to.
    scan: NodeRef,
}

/// Section 6.4 over one join, without changing anything.
///
/// The pair rather than an answer alone, because the rewrite needs the sides it settled on and the
/// plan output needs the sentence, and working either of them out twice is how the two come to
/// disagree.
fn decided(
    plan: &Plan,
    at: NodeRef,
    carried: &[Carried],
    consumers: &[Option<NodeRef>],
    context: &Context,
) -> (Why, Option<Taken>) {
    let Node::Join { left, right, kind, conditions, .. } = *plan.node(at) else {
        return (Why::Key, None);
    };
    // Which input is the many side is not a free choice. An inner join is symmetric and may be read
    // either way round. A left join keeps the rows of its left input, and the link join keeps the
    // rows of its child, so the child has to be the left one. A semi or an anti join decides about
    // the rows of its left input for the same reason.
    let sides: &[(NodeRef, NodeRef)] = match kind {
        JoinKind::Inner => &[(left, right), (right, left)],
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti => &[(left, right)],
        _ => return (Why::Kind, None),
    };
    let Some(keys) = equated_pair(plan, conditions) else {
        return (Why::Key, None);
    };
    let mut worst = Why::None;
    for &(child, parent) in sides {
        let found = match matched(plan, child, parent, keys, carried, context) {
            Ok(found) => found,
            Err(why) => {
                worst = worst.or(why);
                continue;
            }
        };
        let why = worth_it(plan, parent, kind, &found, context);
        // Asked after the rest of it on purpose. What is above a join is the same whichever way
        // round its sides are read, so asking first would report a shape complaint for a join that
        // has no relationship at all and bury the reason that was actually in the way.
        let why =
            if why.chosen() && !absorbed(plan, consumers, at) { Why::ColumnWouldShow } else { why };
        worst = worst.or(why);
        if why.chosen() {
            return (why, Some(Taken { child, parent, scan: found.scan }));
        }
    }
    (worst, None)
}

/// Which node reads each node's output, or nothing for the root and for anything orphaned.
///
/// A node is read once. That is a property of how this plan is built rather than of plans in
/// general, and where it does not hold the second reader overwrites the first, which is a node this
/// pass then declines for whichever answer it ended up with. Declining is the safe direction.
fn consumers(plan: &Plan) -> Vec<Option<NodeRef>> {
    let mut consumers = vec![None; plan.node_count()];
    for node in 0..u32::try_from(plan.node_count()).unwrap_or(u32::MAX) {
        for child in plan.node(node).children().into_iter().flatten() {
            if let Some(slot) = consumers.get_mut(child as usize) {
                *slot = Some(node);
            }
        }
    }
    consumers
}

/// Whether one more column out of `at` is absorbed before it reaches the answer.
///
/// Walks up until something states its own columns, which is what a projection and an aggregate
/// both do: above one of those the extra column is gone whatever the input was. The walk fails at
/// the root, because the root's width is the answer's width, and at any operator that reads all of
/// its input's columns or matches them up by position, because for one of those a column nobody
/// asked for is a different answer rather than a wider one.
fn absorbed(plan: &Plan, consumers: &[Option<NodeRef>], at: NodeRef) -> bool {
    let mut at = at;
    // The plan is a tree, so this terminates, and the bound is the belt to that brace.
    for _ in 0..plan.node_count() {
        let Some(above) = consumers.get(at as usize).copied().flatten() else {
            return false;
        };
        match plan.node(above) {
            Node::Project { .. } | Node::Aggregate { .. } => return true,
            // Carries its input's columns through and adds nothing that depends on how many there
            // were, so the question is the same one asked one level higher.
            Node::Filter { .. }
            | Node::Sort { .. }
            | Node::Limit { .. }
            | Node::LimitPercent { .. }
            | Node::TopN { .. }
            | Node::Window { .. }
            | Node::Join { .. }
            | Node::LinkJoin { .. }
            | Node::CrossProduct { .. }
            | Node::DependentJoin { .. } => at = above,
            _ => return false,
        }
    }
    false
}

/// What a join has to be for a link to answer it.
struct Match {
    /// The child's scan, which is the node the row id column is added to.
    scan: NodeRef,
    /// The parent's projected columns, which is what the gather costs per row.
    projected: Slice,
}

/// Whether this pairing of the two inputs is a relationship the file holds a link for.
///
/// The questions are asked in the order a reader wants them answered rather than in the order that
/// is cheapest. Whether the row id survived is only interesting once there is a relationship for it
/// to have survived for, so the relationship is looked up first even though it is the dearer of the
/// two, and a join over two unrelated columns is never told that its row id went missing.
fn matched(
    plan: &Plan,
    child: NodeRef,
    parent: NodeRef,
    keys: [ColumnBinding; 2],
    carried: &[Carried],
    context: &Context,
) -> std::result::Result<Match, Why> {
    // A gather by row id reads the parent's stored rows, so the parent has to be the stored table
    // and nothing else. See the note at the top about what a filter here would mean.
    let Node::Get { table: parent_name, index: parent_index, columns: projected, .. } =
        *plan.node(parent)
    else {
        return Err(Why::ParentNotStored);
    };
    let [child_key, parent_key] =
        match (keys[0].table == parent_index, keys[1].table == parent_index) {
            (false, true) => [keys[0], keys[1]],
            (true, false) => [keys[1], keys[0]],
            _ => return Err(Why::None),
        };
    let scan = scan_under(plan, child, child_key.table).ok_or(Why::ChildNotStored)?;
    let Node::Get { table: child_name, columns: child_columns, .. } = *plan.node(scan) else {
        return Err(Why::ChildNotStored);
    };
    let child_column =
        plan.field_list(child_columns).get(child_key.column as usize).ok_or(Why::None)?;
    let parent_column =
        plan.field_list(projected).get(parent_key.column as usize).ok_or(Why::None)?;
    let relationship = (
        (plan.string(child_name), child_column.name.as_str()),
        (plan.string(parent_name), parent_column.name.as_str()),
    );
    let declared = context
        .links()
        .iter()
        .find(|link| link.between(relationship.0, relationship.1))
        .ok_or(Why::None)?;
    if !declared.built {
        return Err(Why::NotBuilt);
    }
    // Section 5.1's rule, and the only thing between this pass and a wrong answer. A link is
    // indexed by a row of the child table, so it may only be read where every row reaching the join
    // is still a row of that table.
    if !carried.get(child as usize).is_some_and(|rids| rids.has(child_key.table)) {
        return Err(Why::RowIdGone);
    }
    Ok(Match { scan, projected })
}

/// Section 6.4, which is the whole of the choice.
fn worth_it(plan: &Plan, parent: NodeRef, kind: JoinKind, found: &Match, context: &Context) -> Why {
    // Never touches the parent, so none of the rest of it applies. A semi join over a relationship
    // the file has verified is a sentinel test per child row, and there is no size at which a hash
    // table beats that.
    if matches!(kind, JoinKind::Semi | JoinKind::Anti) {
        return Why::NeverRead;
    }
    let width = width(plan, found.projected);
    let sizes = context.sizes();
    let bytes = |rows: u64| rows.saturating_mul(u64::try_from(width).unwrap_or(u64::MAX));
    // A parent nobody has counted is planned as one that does not fit, which is the answer that
    // stays right as a table grows: a hash join over a parent this pass declined is the plan that
    // ran before there were links at all.
    if let Some(rows) = estimate::rows(plan, parent, context.facts()) {
        if bytes(rows) <= sizes.cache_bytes {
            return Why::Fits { rows, bytes: bytes(rows) };
        }
    }
    // The second bullet of section 6.4, which is the one this pass cannot ask yet: whether the
    // child is stored in the parent's row id order is a property of how the file was written and
    // nothing records it. A clustered child is the case the link join wins by the most, so what
    // this costs is some of the win rather than any of the correctness, and until it is recorded
    // every parent that does not fit is decided by the width below.
    if width < sizes.narrow_bytes {
        Why::Narrow { width, narrow: sizes.narrow_bytes }
    } else {
        Why::Wide { width, narrow: sizes.narrow_bytes }
    }
}

/// What one row of a projection takes, which is what a gather moves per child row.
fn width(plan: &Plan, columns: Slice) -> usize {
    plan.field_list(columns).iter().map(|field| field.ty.physical().size()).sum()
}

/// The two columns one equality holds equal, when that is what the conditions are.
///
/// One condition and no more, because more than one equality is a composite key and a link is built
/// over a single column. Two columns and nothing computed, because a link is indexed by a column.
fn equated_pair(plan: &Plan, conditions: Slice) -> Option<[ColumnBinding; 2]> {
    let [condition] = plan.expr_list(conditions) else {
        return None;
    };
    let Expr::Compare { op: CompareOp::Equal, left, right } = *plan.expr(*condition) else {
        return None;
    };
    match (plan.expr(left), plan.expr(right)) {
        (&Expr::Column(left), &Expr::Column(right)) => Some([left, right]),
        _ => None,
    }
}

/// The scan of `index` under `at`, through the operators that leave a row where it was.
///
/// A filter only, which is the shape a pushed down predicate leaves. A projection would be the
/// second half of the note at the top of this module.
fn scan_under(plan: &Plan, at: NodeRef, index: u32) -> Option<NodeRef> {
    match *plan.node(at) {
        Node::Get { index: found, .. } if found == index => Some(at),
        Node::Filter { input, .. } => scan_under(plan, input, index),
        _ => None,
    }
}

/// Turns the row id on for a scan and hands back the column it now produces.
///
/// A scan that already produces one is read rather than widened, because two columns of that name
/// would be two spellings of the same thing and the reader answers the last of them.
fn number(plan: &mut Plan, scan: NodeRef) -> Option<ColumnBinding> {
    let Node::Get { index, columns, .. } = *plan.node(scan) else {
        return None;
    };
    let mut fields = plan.field_list(columns).to_vec();
    if let Some(at) = fields.iter().position(|field| field.name == FILE_ROW_NUMBER) {
        return Some(ColumnBinding::new(index, u32::try_from(at).ok()?));
    }
    let at = u32::try_from(fields.len()).ok()?;
    fields.push(Field::required(FILE_ROW_NUMBER.to_string(), LogicalType::BigInt));
    let widened = plan.add_fields(&fields);
    match plan.node_mut(scan) {
        Node::Get { columns, .. } => *columns = widened,
        _ => return None,
    }
    Some(ColumnBinding::new(index, at))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_plan::{Node, Plan};

    use super::{LinkJoinRewrite, Linked, Why};
    use crate::estimate::Facts;
    use crate::pass::{Context, Pass};

    /// The relationship every test here declares, which is TPC-H's largest one.
    fn declared() -> Arc<Vec<Linked>> {
        Arc::new(vec![Linked::built("lineitem", "l_orderkey", "orders", "o_orderkey")])
    }

    /// A context that knows the relationship and how large the two tables are.
    fn context(parent_rows: u64) -> Context {
        let mut facts = Facts::new();
        facts.record("memory", "main", "lineitem", 6_000_000);
        facts.record("memory", "main", "orders", parent_rows);
        let mut context = Context::new();
        context.measure(Arc::new(facts));
        context.relate(declared());
        context
    }

    /// The join of the two, under a projection so that the extra column has somewhere to go.
    ///
    /// The parent is projected wide enough that section 6.4's width rule does not decline it, and
    /// the caller says how many rows it has, which is the other half of the rule.
    fn joined(kind: &str) -> Plan {
        let text = format!(
            "Project #2 [#0.0::BIGINT AS k]\n  \
             Join {kind} on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n    \
             Get memory.main.lineitem AS lineitem #0 [l_orderkey::BIGINT]\n    \
             Get memory.main.orders AS orders #1 [o_orderkey::BIGINT]\n"
        );
        Plan::parse(&text).unwrap_or_else(|error| panic!("{text} did not parse: {error}"))
    }

    /// Runs the pass and hands back what the plan reads as.
    fn rewritten(plan: &mut Plan, context: &Context) -> String {
        LinkJoinRewrite.run(plan, context).expect("the pass does not fail");
        plan.to_string()
    }

    #[test]
    fn a_join_over_a_declared_relationship_with_a_parent_too_large_to_cache_reads_the_link() {
        let mut plan = joined("INNER");
        let text = rewritten(&mut plan, &context(1_500_000));
        assert!(text.contains("LinkJoin"), "the join was not rewritten:\n{text}");
        assert!(text.contains("rid="), "the rewrite left no row id behind:\n{text}");
        assert!(
            text.contains("file_row_number"),
            "the child scan was not asked for a row id:\n{text}"
        );
    }

    #[test]
    fn a_parent_small_enough_to_sit_in_cache_keeps_its_hash_join() {
        let mut plan = joined("INNER");
        let text = rewritten(&mut plan, &context(25));
        assert!(!text.contains("LinkJoin"), "twenty five rows were worth a link:\n{text}");
    }

    #[test]
    fn a_semi_join_reads_the_link_however_small_the_parent_is() {
        let mut plan = joined("SEMI");
        let text = rewritten(&mut plan, &context(25));
        assert!(text.contains("LinkJoin"), "a semi join sized its parent:\n{text}");
    }

    #[test]
    fn an_anti_join_reads_the_link_too() {
        let mut plan = joined("ANTI");
        let text = rewritten(&mut plan, &context(25));
        assert!(text.contains("LinkJoin"), "an anti join sized its parent:\n{text}");
    }

    #[test]
    fn a_relationship_nobody_declared_is_left_alone() {
        let mut plan = joined("INNER");
        let mut context = context(1_500_000);
        context.relate(Arc::default());
        let text = rewritten(&mut plan, &context);
        assert!(!text.contains("LinkJoin"), "an undeclared join was rewritten:\n{text}");
    }

    #[test]
    fn the_relationship_has_to_be_the_way_round_it_was_declared() {
        let mut plan = joined("INNER");
        let mut context = context(1_500_000);
        context.relate(Arc::new(vec![Linked::built(
            "orders",
            "o_orderkey",
            "lineitem",
            "l_orderkey",
        )]));
        let text = rewritten(&mut plan, &context);
        assert!(text.contains("LinkJoin"), "the pass refused to swap the sides:\n{text}");
        let found = (0..u32::try_from(plan.node_count()).expect("a small plan"))
            .find_map(|node| match *plan.node(node) {
                Node::LinkJoin { child, .. } => Some(child),
                _ => None,
            })
            .expect("the join is still a join of some kind");
        let Node::Get { table, .. } = *plan.node(found) else {
            panic!("the child is not a scan");
        };
        assert_eq!(
            plan.string(table),
            "orders",
            "the child is not the table the relationship names as the child"
        );
    }

    #[test]
    fn a_left_join_may_only_read_the_link_in_the_direction_that_keeps_its_rows() {
        let mut plan = joined("LEFT");
        let mut context = context(1_500_000);
        context.relate(Arc::new(vec![Linked::built(
            "orders",
            "o_orderkey",
            "lineitem",
            "l_orderkey",
        )]));
        let text = rewritten(&mut plan, &context);
        assert!(
            !text.contains("LinkJoin"),
            "a left join swapped the side whose rows it keeps:\n{text}"
        );
    }

    #[test]
    fn a_join_at_the_root_is_left_alone_because_the_row_id_would_be_in_the_answer() {
        let text = "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n  \
             Get memory.main.lineitem AS lineitem #0 [l_orderkey::BIGINT]\n  \
             Get memory.main.orders AS orders #1 [o_orderkey::BIGINT]\n";
        let mut plan = Plan::parse(text).expect("the plan parses");
        let text = rewritten(&mut plan, &context(1_500_000));
        assert!(!text.contains("LinkJoin"), "the answer grew a column:\n{text}");
    }

    #[test]
    fn a_full_or_right_join_is_not_something_a_forward_link_answers() {
        for kind in ["FULL", "RIGHT"] {
            let mut plan = joined(kind);
            let text = rewritten(&mut plan, &context(1_500_000));
            assert!(!text.contains("LinkJoin"), "a {kind} join was rewritten:\n{text}");
        }
    }

    #[test]
    fn running_the_pass_twice_gives_the_same_plan() {
        let mut plan = joined("INNER");
        let context = context(1_500_000);
        let once = rewritten(&mut plan, &context);
        let twice = rewritten(&mut plan, &context);
        assert_eq!(once, twice, "the pass does not settle");
    }

    /// What the pass says about the one join of a plan, before and after it runs.
    fn about(plan: &Plan, context: &Context) -> Why {
        (0..u32::try_from(plan.node_count()).expect("a small plan"))
            .find_map(|node| super::why(plan, node, context))
            .expect("the plan has a join in it")
    }

    #[test]
    fn a_join_that_read_the_link_says_which_bullet_chose_it() {
        let mut plan = joined("INNER");
        let context = context(1_500_000);
        rewritten(&mut plan, &context);
        let why = about(&plan, &context);
        assert!(why.chosen(), "{why}");
        assert_eq!(
            why.to_string(),
            "the parent does not fit in cache and its projection is 8 bytes, which is under 32"
        );
    }

    #[test]
    fn a_semi_join_says_it_never_read_the_parent_rather_than_quoting_a_width() {
        let mut plan = joined("SEMI");
        let context = context(25);
        rewritten(&mut plan, &context);
        let why = about(&plan, &context);
        assert!(why.chosen(), "{why}");
        assert_eq!(why.to_string(), "a semi or an anti join never reads the parent");
    }

    #[test]
    fn a_parent_that_fits_says_how_large_it_was_rather_than_only_that_it_fitted() {
        // The two numbers are the whole point. A reader who is told a parent fits and not what it
        // was measured at cannot tell a small table from a setting somebody turned up.
        let mut plan = joined("INNER");
        let context = context(25);
        rewritten(&mut plan, &context);
        let why = about(&plan, &context);
        assert!(!why.chosen(), "{why}");
        assert_eq!(
            why.to_string(),
            "the parent is 25 rows and 200 bytes projected, which fits in cache"
        );
    }

    /// The plan the pass has finished with, told about those relationships and nothing else.
    fn over(kind: &str, links: Vec<Linked>) -> (Plan, Context) {
        let mut plan = joined(kind);
        let mut context = context(1_500_000);
        context.relate(Arc::new(links));
        rewritten(&mut plan, &context);
        (plan, context)
    }

    #[test]
    fn a_relationship_nobody_built_says_the_link_is_missing_rather_than_the_relationship() {
        // Half of the pair section 6.7 exists for. This sentence sends a reader to the checkpoint
        // and the one in the test below sends them to the setting, and before this they read the
        // same.
        let (plan, context) =
            over("INNER", vec![Linked::declared("lineitem", "l_orderkey", "orders", "o_orderkey")]);
        assert_eq!(
            about(&plan, &context).to_string(),
            "the relationship is declared and its link is not in the file"
        );
    }

    #[test]
    fn a_relationship_over_other_columns_says_there_is_none_over_these_ones() {
        let (plan, context) =
            over("INNER", vec![Linked::built("orders", "o_totalprice", "nation", "n_name")]);
        assert_eq!(
            about(&plan, &context).to_string(),
            "no relationship is declared between those two columns"
        );
    }

    #[test]
    fn a_join_whose_row_id_would_reach_the_answer_says_that_and_not_that_there_is_no_link() {
        // The failure mode section 6.7 names outright: without this sentence, a join declined for
        // the shape of the plan above it reads exactly like a join over a relationship nobody has.
        let text = "Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]\n  \
             Get memory.main.lineitem AS lineitem #0 [l_orderkey::BIGINT]\n  \
             Get memory.main.orders AS orders #1 [o_orderkey::BIGINT]\n";
        let mut plan = Plan::parse(text).expect("the plan parses");
        let context = context(1_500_000);
        rewritten(&mut plan, &context);
        assert_eq!(
            about(&plan, &context).to_string(),
            "the row id would reach an operator that counts its input's columns"
        );
    }

    #[test]
    fn a_right_join_says_the_link_points_the_other_way() {
        let mut plan = joined("RIGHT");
        let context = context(1_500_000);
        rewritten(&mut plan, &context);
        assert_eq!(
            about(&plan, &context).to_string(),
            "a forward link does not answer a right or a full join"
        );
    }

    #[test]
    fn the_three_constructors_are_three_steps_up_the_same_ladder() {
        // What a rewrite is allowed to do grows with the certificates, so the three have to be
        // ordered rather than merely different. A declaration licenses nothing, a link licenses
        // reading it in place of a hash table, and a link over a total relationship licenses
        // deleting the join. The ladder is what stops a rewrite that wanted the second one from
        // firing on the first.
        let declared = Linked::declared("lineitem", "l_orderkey", "orders", "o_orderkey");
        assert!(!declared.built, "nobody built it");
        assert!(!declared.total, "and nothing counted the children");

        let built = Linked::built("lineitem", "l_orderkey", "orders", "o_orderkey");
        assert!(built.built, "the parent side was read and found unique");
        assert!(!built.total, "which says nothing about the children");

        let verified = Linked::verified("lineitem", "l_orderkey", "orders", "o_orderkey");
        assert!(verified.built && verified.total, "both certificates of section 7.3");

        // The four names are the same in all three, which is what lets a pass match on the columns
        // and then read the certificates rather than the other way round.
        for link in [&declared, &built, &verified] {
            assert!(link.between(("lineitem", "l_orderkey"), ("orders", "o_orderkey")));
        }
    }
}
