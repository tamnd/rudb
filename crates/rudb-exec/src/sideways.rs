//! What a hash join tells the scan under its driving side, once its build side is in.
//!
//! By the time a join has gathered one side it knows something about the other side's key that no
//! statistic could have told the planner: the exact set of keys it will ever match. A driving row
//! outside that set matches nothing, so a scan that drops it does the join's work earlier and over
//! less.
//!
//! Two tiers of the same fact, in the order they cost anything.
//!
//! The range, which is the smallest and the largest key. A whole stored chunk outside it is a chunk
//! the scan never reads, never decompresses and never decodes. That is the tier
//! `spec/planner/09-runtime-filters-and-adaptivity.md` section 09.2 calls always on, and it is the
//! paragraph of `spec/engine/08-join.md` section 8.5 about composing with zone maps. It costs two
//! comparisons per chunk against numbers that are already in memory and it carries no bytes of its
//! own.
//!
//! The filter, which is the keys themselves to the precision ten bits each buys. It answers about a
//! row rather than a chunk, so it is what is left for a fact table whose join key is spread over its
//! whole domain, where the range covers every chunk and rules out nothing. That is section 8.5's
//! first paragraph and the largest win it names. It costs a hash of the key column per chunk on both
//! sides and one cache line touched per row, and its bytes are proportional to the build side's
//! rows rather than to the driving side's.
//!
//! # Why it is a handoff rather than an argument
//!
//! The two sides of a join are two pipelines with an edge between them, and this crosses that edge
//! in the same direction the rows do. The build pipeline finishes before the driving one starts,
//! which is what the edge means, so by the time the driving side is asked how to divide its work
//! both tiers are known. What carries them is one shared object: the join arms it while the query is
//! being built, the sink at the end of the build side fills it as that side finishes, and the scan
//! reads it when it is asked for its morsels. Nothing locks, because each of the three steps happens
//! strictly after the one before it.
//!
//! # What it refuses, and why each refusal is a wrong answer avoided
//!
//! A scan that drops rows is only allowed where the join was going to drop them anyway.
//!
//! **The kind.** An inner join and a semi join throw away a driving row that matches nothing, so
//! dropping it earlier is the same answer. A left, an anti and a single join all answer with that
//! row, so dropping it is a row missing from the result. Only the first two arm this.
//!
//! **The null rule.** `NULL = NULL` is null, so a driving row whose key is null matches nothing and
//! dropping it is dropping a row that was going to go. `IS NOT DISTINCT FROM` is the other rule for
//! the same value and under it two nulls match, so neither a range, which is about order, nor a
//! filter, which was built without the nulls in it, is allowed to decide anything. Only `=` arms
//! this.
//!
//! **The shape below the join.** Both tiers are facts about one column of one stored table, so what
//! consumes them has to be the scan of that table with nothing between the two that decides which
//! rows survive by counting rather than by value. A `LIMIT` over a `SORT` is the case that says so:
//! filtering the scan under it changes which rows reach the limit, which changes the answer even
//! though every row removed would have failed the join. [`crate::build`] walks down through a filter
//! and a projection and stops at anything else.
//!
//! Anything this cannot arm is a query that runs exactly as it did before, because a scan handed
//! nothing asks nothing and reads everything.
//!
//! # The column the join names is not the column the scan produces
//!
//! A projection binds its output against a table index of its own, so the driving column a join
//! knows about is `#1.0` where the scan under it produces `#0.0`, and a scan asked about a binding
//! into some other table answers nothing. That is one node between the two and it is there in every
//! plan that projects, which is every plan over a view and every plan pushdown has been through, so
//! taking the join's binding as it stands is a runtime filter that is built, handed over and never
//! read. [`beneath`] is the walk that turns the one into the other, down the same two nodes the
//! builder allows and through nothing else, and a projection that computes its column rather than
//! passing one through ends the walk with nothing, because a fact about a value says nothing about
//! what an expression over it produces.

use std::sync::{Arc, OnceLock};

use rudb_common::bounds::{Bound, Op};
use rudb_common::{LogicalType, Result, SessionTimeZone};
use rudb_graph::link::Form;
use rudb_graph::{Adjacency, KeyMap, Link, PART_ROWS, Pushed, Rids};
use rudb_metrics::Reduced;
use rudb_plan::{BuildSide, ColumnBinding, Expr, ExprRef, JoinKind, Node, NodeRef, Plan};
use rudb_storage::{Blocked, Range};
use rudb_vector::{Chunk, Vector};

use crate::expr::evaluate_all_in_time_zone;
use crate::lookup::has_nulls;
use crate::schema::Schema;
use crate::table::{Across, hash};

/// The most a runtime filter may take, which is thirty two megabytes.
///
/// Ten bits a key is twenty six million keys inside that, and a build side larger than that is one
/// where the driving side is unlikely to be the one worth reading less of. Past it the range is
/// still handed over and the filter is not, which is the same answer for fewer bytes.
const BUDGET: usize = 32 << 20;

/// The bits of a bitmap small enough to take whatever it costs a key, which is one megabyte, inside
/// the second level cache of any core this is built for. See [`dense`].
const SMALL: u64 = 1 << 23;

/// The edge one join's runtime filter crosses, shared between the join, its build side's sink and
/// one scan.
///
/// Every field is written once and read afterwards, in the order the fields are declared, which is
/// the order the three steps happen in. A [`Sideways`] that was never armed answers nothing to
/// everything, which is what a join that cannot use one leaves behind.
#[derive(Debug, Default)]
pub(crate) struct Sideways<'a> {
    /// How to read the build side's key out of its chunks. Written by the join while the query is
    /// being built, and read by the sink at the end of the build side.
    keyed: OnceLock<Keyed<'a>>,
    /// The column of the driving side the key is compared against, which is the column this is
    /// about. Written at the same moment as `keyed` and read by the scan.
    binding: OnceLock<ColumnBinding>,
    /// The stored structures that turn the build side's keys into exact driving rows, when the join
    /// is over a relationship with a link in the file. Written with `keyed` and read by the sink.
    exact: OnceLock<Exact>,
    /// What the build side turned out to hold. Written by the sink when the build side finishes and
    /// read by the scan when it is asked for its morsels.
    found: OnceLock<Found>,
    /// Whether a join above wants the build side's keys as a bitmap even where the scan has
    /// something better. Written while the query is being built. See [`Sideways::kept`].
    wanted: OnceLock<()>,
    /// Whether the scan this is about reads it as one of several, below another join's handoff
    /// that is its own. Such a scan cannot place exact rows from this one, see [`Found::spare`], so
    /// the build side does not make them. Written when the scan is built.
    aside: OnceLock<()>,
    /// Whether a scan took this as its own, which is the scan that places the exact rows. Written
    /// when the scan is built. See [`Sideways::settles`].
    owned: OnceLock<()>,
}

/// What one side of a join holds, as much of it as was worth keeping.
///
/// Both halves are optional and they are optional separately. A side with no keyed row at all has
/// neither. A side whose key column has no ordered bound, which is a type no [`Bound`] compares,
/// has no range and may still have a filter. A side too large for [`BUDGET`] has a range and no
/// filter.
#[derive(Debug, Default)]
pub(crate) struct Found {
    /// The smallest and the largest key, both ends or neither.
    range: Option<(Bound, Bound)>,
    /// The keys themselves, to the precision ten bits each buys.
    filter: Option<Blocked>,
    /// The driving rows that can match, exactly, when the join was armed with an [`Exact`].
    ///
    /// When this is there the filter is not, because it answers the same question with no false
    /// positives and a bit test in place of a hash.
    rows: Option<Rids>,
    /// The build side's keys as a bitmap over the parent's key range, when the join had a key map
    /// and no link. Like `rows`, it takes the place of the filter.
    domain: Option<Domain>,
    /// The same bitmap kept beside `rows`, for a scan that is reduced by another join's rows and
    /// so reads this join's answer as one of several. Such a scan cannot place a second set of
    /// rows, because the rows it tests have already been narrowed, and without this it would get
    /// nothing from the join at all, since `rows` takes the place of the filter.
    spare: Option<Domain>,
    /// The build side's keys as a bitmap over their own range, made only for a join above that
    /// asked for it and only when `domain` is not there. The scan never reads it.
    held: Option<Domain>,
    /// What the exact reduction came to, for the scan to report, including one that stopped early
    /// and so left `rows` empty.
    reduced: Option<Reduced>,
    /// The build side's keys themselves, when they are integers and few enough to keep. See
    /// [`Keys`].
    keys: Option<Keys>,
}

/// The most keys a build side keeps as a sorted list.
///
/// A part covers a few thousand keys of a table stored in key order, so past a few thousand keys
/// spread over a few million there is rarely a part with none of them in it, and the sort is paid
/// for nothing. TPC-H q4 is the case: the orders of one quarter are fifty seven thousand keys that
/// land in every part of `lineitem`.
const KEYS: usize = 1 << 12;

/// The build side's keys, sorted and each once, for the scan to rule out parts with.
///
/// The range says a part is worth reading when any of its keys could fall between the smallest and
/// the largest the build side holds. That is the right question for a filter on the parent that
/// keeps one stretch of it, and the wrong one for a filter that keeps keys scattered over the whole
/// of it. TPC-H q18 is the case: the orders over three hundred in quantity are fifty seven keys
/// spread over six million, so the range covers all of `lineitem` and `orders` and both scans read
/// every part, and the filter or the bitmap drops all but a handful of rows only after each part was
/// decoded. Both tables are stored in order key order, so a part covers a few thousand keys and
/// almost none of them holds one of the fifty seven. Asking for the first key at or past the part's
/// smallest and checking it against the part's largest is a binary search a part, and it rules out
/// the part before a byte of it is read.
///
/// Kept beside the filter or a bitmap, which both answer about a row and say nothing about a part,
/// and not beside the exact rows, which already skip every part they hold nothing in. Only up to
/// [`KEYS`].
#[derive(Debug)]
pub(crate) struct Keys {
    keys: Vec<i64>,
}

impl Keys {
    /// Whether no key falls inside a part whose column spans `range`.
    ///
    /// `false` for a range with an open end or one that is not an integer, which keeps the part.
    pub(crate) fn misses(&self, range: &Range) -> bool {
        let (Some(Bound::Int(low)), Some(Bound::Int(high))) = (&range.low, &range.high) else {
            return false;
        };
        let at = self.keys.partition_point(|&key| i128::from(key) < *low);
        self.keys.get(at).is_none_or(|&key| i128::from(key) > *high)
    }
}

/// The keys a build side holds, as one bit per key value over the range the parent's keys span.
///
/// The half of section 5.4 that needs no link. A key map in the identity or the dense form says the
/// parent's keys fill most of a range, so a bitmap over that range costs at most eight bits a parent
/// row, and a driving row is tested against it with a subtraction and one bit. That is exact for
/// the same reason the link is: a key the build side holds has its bit set and a key it does not
/// hold has not. It is how `lineitem` gets reduced against a filtered `part` on Q9, Q14 and Q19,
/// where the link between the two is over the budget and so is not in the file.
///
/// What it gives up next to the link is the part skip, because all a part says about its keys is
/// their range, and the range of a child column that is not in its parent's order is the whole
/// table.
#[derive(Debug)]
pub(crate) struct Domain {
    /// The parent's smallest key, which is bit zero.
    base: i128,
    /// How many key values from `base` the bitmap covers.
    range: u64,
    words: Vec<u64>,
}

impl Domain {
    /// Whether the build side holds `key`.
    fn holds(&self, key: i64) -> bool {
        let Ok(offset) = u64::try_from(i128::from(key) - self.base) else { return false };
        offset < self.range && self.words[(offset / 64) as usize] >> (offset % 64) & 1 == 1
    }

    /// Which of the first `rows` rows of `keys` hold a key the build side holds.
    ///
    /// A null key is not one, because a null matches nothing under the rule this is armed for.
    /// `block` is the caller's to keep between chunks so that the widened keys are not allocated a
    /// chunk at a time.
    pub(crate) fn keep(&self, keys: &Vector, rows: usize, block: &mut Vec<i64>) -> Vec<u32> {
        let mut kept = Vec::with_capacity(rows);
        if keys.signed_block(block) && block.len() >= rows {
            let none_null = keys.none_null();
            if let (Ok(base), true, true) =
                (i64::try_from(self.base), none_null, self.range < 1 << 62)
            {
                // A key under the base wraps round to an offset far past the range, so one compare
                // covers both ends, and the row is written whether it is kept or not so that the loop
                // has no branch to mispredict on a bitmap that keeps about half.
                // A key past the range is moved onto the bit at the range itself, which
                // [`words_for`] leaves room for and nothing sets, so the range test is a
                // conditional move and not a branch that half the keys of a filtered parent take.
                kept.resize(rows, 0);
                let mut at = 0;
                for (row, &key) in block[..rows].iter().enumerate() {
                    let offset = (key.wrapping_sub(base) as u64).min(self.range);
                    let hit = self.words[(offset / 64) as usize] >> (offset % 64) & 1 == 1;
                    kept[at] = row as u32;
                    at += usize::from(hit);
                }
                kept.truncate(at);
                return kept;
            }
            for (row, &key) in block[..rows].iter().enumerate() {
                if self.holds(key) && (none_null || !keys.is_null_at(row)) {
                    kept.push(row as u32);
                }
            }
            return kept;
        }
        for row in 0..rows {
            let key = keys.signed_at(row).and_then(|key| i64::try_from(key).ok());
            if key.is_some_and(|key| self.holds(key)) {
                kept.push(row as u32);
            }
        }
        kept
    }
}

/// What turns a build side's keys into the set of driving rows that can match them, exactly.
///
/// spec/graph/05-execution.md section 5.4. The build side of a join over a relationship is some
/// subset of the parent's rows, and the key map says which row holds each key, so the keys become a
/// set of parent rows. The link says which parent every child row points at, so pushing that set
/// through it gives the child rows that have a partner on the build side and no others. That is the
/// runtime filter with the false positives taken out: a Bloom filter keeps a row the join will drop
/// about one time in a hundred and costs a hash per row, and this keeps none and costs a bit test.
///
/// It is only made where the key the build side is hashed on is the parent's stored key column and
/// the driving column is the child's stored link column, both read as they are with nothing
/// computed over them. [`crate::build`] checks that, and the link it takes has already been checked
/// against the parent's generation by [`rudb_native::graph::stored_link`].
///
/// The key map and the link are read out of the file the first time a build side asks for them
/// rather than when the join is built. Reading one is a checksum and a decode of the whole section,
/// the link of `lineitem` to `orders` is six million rows of it, and a join whose build side turns
/// out to hold every parent never asks. See [`found_for`].
#[derive(Debug)]
pub(crate) struct Exact {
    /// Rows the parent has, which is known before anything is read.
    parents: u64,
    /// Rows the driving table has, when it is one file and so could have a link.
    children: Option<u64>,
    /// Which parent row holds a key, once read. `None` inside is a map that did not read.
    keys: OnceLock<Option<KeyMap>>,
    /// Which parent row every driving row points at, when the link is in the file. Without it the
    /// build side's keys become a [`Domain`] instead.
    link: OnceLock<Option<Link>>,
    /// The children of every parent row, when the file holds the backward adjacency. With it a
    /// build side that holds few parents becomes the driving rows by reading their lists.
    adjacency: OnceLock<Option<Adjacency>>,
    /// The form the link takes, read off its head without reading the link.
    form: OnceLock<Option<Form>>,
    /// Where the three are read from, or nothing when they were handed over already read.
    stored: Option<Stored>,
}

/// The files a join's [`Exact`] reads its key map and its link out of.
#[derive(Debug)]
pub(crate) struct Stored {
    /// The parent's file and the column its key map is over.
    pub(crate) parent: rudb_native::Reader,
    pub(crate) column: usize,
    /// The driving table's file and the edge its link is stored under, when it is one file.
    pub(crate) child: Option<(rudb_native::Reader, rudb_native::graph::Edge)>,
}

impl Exact {
    /// The key map of the parent and the link from the driving table to it, if there is one.
    #[cfg(test)]
    pub(crate) fn new(keys: KeyMap, link: Option<Link>) -> Self {
        Self {
            parents: keys.len(),
            children: link.as_ref().map(Link::children),
            keys: OnceLock::from(Some(keys)),
            link: OnceLock::from(link),
            adjacency: OnceLock::from(None),
            form: OnceLock::new(),
            stored: None,
        }
    }

    /// The same, read out of the files when a build side first asks.
    pub(crate) fn stored(stored: Stored) -> Self {
        Self {
            parents: stored.parent.table().rows() as u64,
            children: stored.child.as_ref().map(|(child, _)| child.table().rows() as u64),
            keys: OnceLock::new(),
            link: OnceLock::new(),
            adjacency: OnceLock::new(),
            form: OnceLock::new(),
            stored: Some(stored),
        }
    }

    /// Whether a push of `held` of the parents could skip half the driving table's parts.
    ///
    /// Taken as if the parents held were spread evenly over the parent table. A part of the driving
    /// table points at about `parents * PART_ROWS / children` parents, and it can be skipped only
    /// when none of them is held, which for a spread set is `(1 - held / parents)` to that power.
    /// On TPC-H q03 the orders a join keeps are one in ten and a part of `lineitem` points at about
    /// two hundred and fifty of them, so no part could be skipped and the push was not worth the
    /// lookups that find it out. A set that is not spread but gathered in one range of keys is
    /// skipped by the range the scan is also handed, which is the bitmap's too. `true` when the
    /// driving table is not one file, where there is no link and [`reduce`] finds that out first.
    fn might_skip(&self, held: u64) -> bool {
        let Some(children) = self.children.filter(|&children| children > 0) else { return true };
        if self.parents == 0 {
            return false;
        }
        let per_part = self.parents as f64 * PART_ROWS as f64 / children as f64;
        let missed = (1.0 - held as f64 / self.parents as f64).powf(per_part.max(1.0));
        missed >= 0.5
    }

    fn keys(&self) -> Option<&KeyMap> {
        self.keys
            .get_or_init(|| {
                let stored = self.stored.as_ref()?;
                rudb_native::graph::key_map(&stored.parent, stored.column)
            })
            .as_ref()
    }

    /// The same, with the adjacency handed over already built.
    #[cfg(test)]
    pub(crate) fn adjacent(keys: KeyMap, adjacency: Adjacency) -> Self {
        Self {
            parents: keys.len(),
            children: Some(adjacency.children()),
            keys: OnceLock::from(Some(keys)),
            link: OnceLock::from(None),
            adjacency: OnceLock::from(Some(adjacency)),
            form: OnceLock::new(),
            stored: None,
        }
    }

    fn adjacency(&self) -> Option<&Adjacency> {
        self.adjacency
            .get_or_init(|| {
                let stored = self.stored.as_ref()?;
                let (child, edge) = stored.child.as_ref()?;
                rudb_native::graph::stored_adjacency(child, &stored.parent, edge)
            })
            .as_ref()
    }

    /// Whether the link is in the monotone form, which [`reduce`] can push for what the set holds.
    fn monotone(&self) -> bool {
        let form = self.form.get_or_init(|| {
            let Some(stored) = self.stored.as_ref() else { return self.link().map(Link::form) };
            let (child, edge) = stored.child.as_ref()?;
            rudb_native::graph::stored_link_counts(child, &stored.parent, edge)
                .map(|counts| counts.form)
        });
        *form == Some(Form::Monotone)
    }

    fn link(&self) -> Option<&Link> {
        self.link
            .get_or_init(|| {
                let stored = self.stored.as_ref()?;
                let (child, edge) = stored.child.as_ref()?;
                rudb_native::graph::stored_link(child, &stored.parent, edge)
            })
            .as_ref()
    }
}

/// How to read one key column out of a chunk of the build side.
///
/// The expression rather than a column number, because the binder writes `p.k::INTEGER = b.k` as a
/// cast around one operand and the value that goes in the table is the cast one. This is the same
/// expression the hash table is built on, evaluated the same way.
#[derive(Debug)]
pub(crate) struct Keyed<'a> {
    plan: &'a Plan,
    expr: ExprRef,
    schema: Schema,
    time_zone: SessionTimeZone,
}

impl<'a> Keyed<'a> {
    /// The key expression `expr` over rows shaped like `schema`.
    pub(crate) fn new(
        plan: &'a Plan,
        expr: ExprRef,
        schema: Schema,
        time_zone: SessionTimeZone,
    ) -> Self {
        Self { plan, expr, schema, time_zone }
    }

    /// The three things the evaluator wants, so that the caller cannot put them in the wrong order.
    pub(crate) fn parts(&self) -> (&'a Plan, [ExprRef; 1], &Schema, SessionTimeZone) {
        (self.plan, [self.expr], &self.schema, self.time_zone)
    }
}

impl<'a> Sideways<'a> {
    /// A handoff nobody has armed, which is what a join that cannot use one leaves behind.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Says how the build side's key is read, for the sink that is about to walk it.
    ///
    /// Called at most once, while the query is being built and before anything runs. A second call
    /// is ignored rather than refused, because the only caller is the one place in [`crate::build`]
    /// that makes one of these and a second arming would be a bug there rather than in a query.
    pub(crate) fn keying(&self, keyed: Keyed<'a>) {
        let _ = self.keyed.set(keyed);
    }

    /// Says which driving column the range is going to be about, for the scan that reads it.
    ///
    /// Separate from [`Sideways::keying`] because the two are read by different operators and a scan
    /// that knows the column has no use for the expression that produced the range.
    pub(crate) fn about(&self, binding: ColumnBinding) {
        let _ = self.binding.set(binding);
    }

    /// How to read the build side's key, for the sink that is about to walk it.
    pub(crate) fn keyed(&self) -> Option<&Keyed<'a>> {
        self.keyed.get()
    }

    /// Says the build side's keys can be turned into exact driving rows, and with what.
    ///
    /// Called at most once, next to [`Sideways::keying`], and only for a join over a relationship
    /// whose link the file holds.
    pub(crate) fn exactly(&self, exact: Exact) {
        let _ = self.exact.set(exact);
    }

    /// What turns the keys into rows, for the sink, when the join was armed with it.
    pub(crate) fn exact(&self) -> Option<&Exact> {
        self.exact.get()
    }

    /// The driving column this is about, in the scan's own naming, once the join has armed it.
    pub(crate) fn binding(&self) -> Option<ColumnBinding> {
        self.binding.get().copied()
    }

    /// Asks for the build side's keys as a bitmap, for a join above that narrows its own table by
    /// them. Called while the query is being built.
    pub(crate) fn wanted(&self) {
        let _ = self.wanted.set(());
    }

    /// Says the scan reads this handoff as one of several. See [`Sideways::aside`].
    pub(crate) fn set_aside(&self) {
        let _ = self.aside.set(());
    }

    /// Whether exact rows from this handoff would be placed by the scan. See [`Sideways::aside`].
    pub(crate) fn placed(&self) -> bool {
        self.aside.get().is_none()
    }

    /// Says a scan took this as its own, so the exact rows it makes are the rows that scan reads.
    pub(crate) fn own(&self) {
        let _ = self.owned.set(());
    }

    /// Whether every row the scan hands up is one of the exact rows, once the build side has
    /// finished.
    ///
    /// True only when a scan took this as its own and the build side made exact rows for it. Every
    /// row that scan reads is then a row whose key one of the build side's rows holds, and the
    /// filters and projections between it and the join can drop such a row but cannot make one.
    /// That is what lets the join above answer without looking anything up. See
    /// [`crate::join::Probe::settled_by`].
    pub(crate) fn settles(&self) -> bool {
        self.owned.get().is_some()
            && self.placed()
            && self.binding().is_some_and(|binding| self.rows(binding.table).is_some())
    }

    /// Whether a join above asked for [`Sideways::kept`].
    pub(crate) fn is_wanted(&self) -> bool {
        self.wanted.get().is_some()
    }

    /// The build side's keys as a bitmap, once the build side has finished and made one.
    pub(crate) fn kept(&self) -> Option<&Domain> {
        let found = self.found.get()?;
        found.domain.as_ref().or(found.held.as_ref())
    }

    /// Records what the build side held. Called once, when the build side's pipeline finishes.
    pub(crate) fn found(&self, found: Found) {
        let _ = self.found.set(found);
    }

    /// The tests a scan of `index` should add to the ones the plan already gave it.
    ///
    /// Empty unless this was armed, the build side has finished, it found a range, and the column
    /// the range is about is one of this scan's. The positions are positions in the scan's
    /// projection, which is what a binding into a scan's own table index already is.
    pub(crate) fn tests(&self, index: u32) -> Vec<(usize, Op, Bound)> {
        let (Some(binding), Some(Some((low, high)))) =
            (self.binding.get(), self.found.get().map(|found| &found.range))
        else {
            return Vec::new();
        };
        if binding.table != index {
            return Vec::new();
        }
        let column = binding.column as usize;
        vec![(column, Op::GreaterOrEqual, low.clone()), (column, Op::LessOrEqual, high.clone())]
    }

    /// The filter a scan of `index` should put its rows through, and which of its columns.
    ///
    /// `None` on everything the tests above answer nothing about, and also on a build side that was
    /// too large for one. The position is the same projection position, because a row is dropped by
    /// reading the column the scan has just produced rather than by reading the table.
    pub(crate) fn sifting(&self, index: u32) -> Option<(usize, &Blocked)> {
        let binding = self.binding.get()?;
        if binding.table != index {
            return None;
        }
        Some((binding.column as usize, self.found.get()?.filter.as_ref()?))
    }

    /// The exact set of rows a scan of `index` should keep, by their position in the table.
    ///
    /// `None` on everything the tests above answer nothing about, and on a join that was not armed
    /// with an [`Exact`] or whose keys could not all be read as integers.
    pub(crate) fn rows(&self, index: u32) -> Option<&Rids> {
        if self.binding.get()?.table != index {
            return None;
        }
        self.found.get()?.rows.as_ref()
    }

    /// What the exact reduction came to for a scan of `index`, for `EXPLAIN ANALYZE` to show.
    ///
    /// Asked once by the scan rather than kept up to date, because it is settled when the build side
    /// finishes and nothing changes it after.
    pub(crate) fn reduction(&self, index: u32) -> Option<Reduced> {
        if self.binding.get()?.table != index {
            return None;
        }
        self.found.get()?.reduced
    }

    /// The build side's keys as a sorted list the scan of `index` rules out parts with, and which of
    /// its columns holds the key. See [`Keys`].
    pub(crate) fn keys(&self, index: u32) -> Option<(usize, &Keys)> {
        let binding = self.binding.get()?;
        if binding.table != index {
            return None;
        }
        Some((binding.column as usize, self.found.get()?.keys.as_ref()?))
    }

    /// The build side's keys as a bitmap the scan of `index` tests its rows against, and which of
    /// its columns holds the key. See [`Domain`].
    pub(crate) fn domain(&self, index: u32) -> Option<(usize, &Domain)> {
        let binding = self.binding.get()?;
        if binding.table != index {
            return None;
        }
        Some((binding.column as usize, self.found.get()?.domain.as_ref()?))
    }

    /// The bitmap kept beside the exact rows, for a scan that cannot place them. See
    /// [`Found::spare`].
    pub(crate) fn spare(&self, index: u32) -> Option<(usize, &Domain)> {
        let binding = self.binding.get()?;
        if binding.table != index {
            return None;
        }
        Some((binding.column as usize, self.found.get()?.spare.as_ref()?))
    }
}

/// The same column as `binding`, named the way the scan at the bottom of `node` names it.
///
/// The join knows its driving column as the projection above the scan binds it, and the scan knows
/// its own columns, so somebody has to walk between the two. This does, down the two nodes
/// [`crate::build`] lets a runtime filter through, and it is the same walk for the same reason: a
/// filter keeps rows and renames nothing, so the binding goes through it untouched, and a projection
/// rebinds, so the binding becomes whatever the expression in that position is.
///
/// `None` unless the walk ends at a scan of the table the binding is about by then. A projection
/// whose column at that position is an expression rather than a column ends it, because a set of
/// values says nothing about what an expression over them produces, and so does a node the builder
/// would have refused anyway, which is here as well so that the two cannot drift apart.
pub(crate) fn beneath(plan: &Plan, node: NodeRef, binding: ColumnBinding) -> Option<ColumnBinding> {
    let mut at = node;
    let mut binding = binding;
    loop {
        match *plan.node(at) {
            Node::Get { index, .. } | Node::TableFunction { index, .. } => {
                return (binding.table == index).then_some(binding);
            }
            Node::Filter { input, .. } => at = input,
            Node::Project { input, index, exprs, .. } => {
                if binding.table == index {
                    let exprs = plan.expr_list(exprs);
                    let at = exprs.get(binding.column as usize)?;
                    let Expr::Column(inner) = *plan.expr(*at) else { return None };
                    binding = inner;
                }
                at = input;
            }
            ref node @ Node::Join { .. } => at = through(node)?,
            _ => return None,
        }
    }
}

/// The input of a join that a filter on one of its driving columns may go down into.
///
/// A join above this one drops a row whose key it has no match for. When this join is an inner
/// join or a semi join, every row it produces carries its driving row's columns unchanged, so a
/// driving row the join above would drop can be dropped before this join instead, and the answer
/// is the same with less work in between. That is the whole of the argument, and it is why the
/// driving side and not the gathered one: a gathered row is read once per match and dropping it
/// early says nothing about the rows it would have paired with.
///
/// Anything else lets nothing through. An outer join answers for a driving row with no match, a
/// mark join answers for every driving row, and a semi join turned around gathers its subject and
/// streams the other side past, so its driving rows are not the ones it produces.
///
/// On TPC-H q05 this is what lets the supplier side reach the lineitem scan. The join on the
/// order key is the one nearest the scan, and without this the filter from the supplier join
/// above it stopped there, so nine hundred thousand rows paid for the probe of orders to find
/// out that all but seven thousand had the wrong supplier.
pub(crate) fn through(node: &Node) -> Option<NodeRef> {
    let Node::Join { left, right, kind, build, .. } = *node else { return None };
    match (kind, build) {
        (JoinKind::Inner, BuildSide::Left) => Some(right),
        (JoinKind::Inner | JoinKind::Semi, BuildSide::Right) => Some(left),
        _ => None,
    }
}

/// What the build side holds, read off the chunks it was gathered into.
///
/// One pass over one column of the smaller side of the join, at the moment that side is complete.
/// At the moment rather than as the chunks arrive, because the filter has to be sized before the
/// first key goes into it and the row count is exact only once the side has finished. Two passes
/// would be the alternative and the second of them is this one.
///
/// The key is the expression rather than a column number, because the binder writes `p.k::INTEGER =
/// b.k` as a cast around one operand and the value that goes into the hash table is the cast one.
/// The hash is the same hash the hash table takes, over the value rather than over a dictionary
/// code, which is what lets the scan on the other side hash its raw column and get the same word.
///
/// # Errors
///
/// Whatever evaluating the key expression raises, which is what the hash table build would have
/// raised over the same rows a moment later.
///
/// With `wanted`, the keys as a bitmap as well when nothing else made one, for a join above that
/// asked. See [`Sideways::wanted`].
///
/// With `placed` false, no exact rows, because the scan reads this handoff as one of several and
/// places only the rows of its own. The bitmap over the key values is made as it would be.
pub(crate) fn found_for(
    keyed: &Keyed<'_>,
    exact: Option<&Exact>,
    chunks: &[Chunk],
    wanted: bool,
    placed: bool,
) -> Result<Found> {
    let (plan, exprs, schema, time_zone) = keyed.parts();
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    // A build side with as many rows as the parent has keys is every parent or close to it, and
    // then the exact set removes about nothing and finding that out costs a lookup in the key map
    // for each of its rows. On TPC-H q09 that is the join with all 1.5 million orders, which cost
    // 0.56 G instructions to learn that it held every one of them. It is left to the bitmap or the
    // filter, which measure what they keep and stop paying when it is everything.
    let exact = exact.filter(|exact| (rows as u64) < exact.parents);
    // The bitmap over the key values first, because it is one bit a build row and the key map
    // alone, and it counts the parents the side holds. That count says whether a push could skip
    // enough of the driving table to pay for the lookups and the link, see [`Exact::might_skip`],
    // and only then are those read.
    let by_key = exact.map(|exact| domain_of(keyed, exact, chunks)).transpose()?.flatten();
    let listed = match exact.filter(|_| placed) {
        Some(exact) => listed(keyed, exact, chunks)?,
        None => None,
    };
    // A monotone link is pushed whatever it could skip, because the push walks from one held
    // parent to the next and costs about what the set holds, and the exact rows it makes spare the
    // scan a bit test on every row of every part. On TPC-H q04 that took the query from 0.281 G to
    // 0.106 G instructions on the file clustered by date, where the orders of one quarter are spread
    // over too many parts of `lineitem` for the push to skip one.
    let trying = exact.filter(|exact| {
        placed
            && listed.is_none()
            && (exact.monotone() || by_key.as_ref().is_none_or(|(_, held)| exact.might_skip(*held)))
    });
    let pushing = match listed {
        Some(listed) => Some(Pushing::Done(listed)),
        None => trying.map(|exact| reduce(keyed, exact, chunks)).transpose()?.flatten(),
    };
    let pushed = match pushing {
        Some(Pushing::Done(pushed)) => Some(pushed),
        _ => None,
    };
    let mut reduced = pushed.as_ref().map(|pushed| Reduced {
        kept: pushed.rids.len(),
        rows: pushed.rids.rows(),
        stopped: pushed.stopped,
        by_key: false,
    });
    let mut domain = None;
    let mut spare = None;
    if let Some((bitmap, held)) = by_key {
        let parents = exact.and_then(Exact::keys).map_or(0, KeyMap::len);
        // A side that holds every parent key removes only the rows whose key no parent holds, and
        // the join drops those as cheaply, so testing every row would buy nothing.
        let bitmap = (held < parents).then_some(bitmap);
        if pushed.is_none() {
            reduced = Some(Reduced { kept: held, rows: parents, stopped: false, by_key: true });
            domain = bitmap;
        } else {
            spare = bitmap;
        }
    }
    // The exact rows answer everything the filter would, with no false positives, so a side that
    // has them does not pay for building the filter too. Nor does a side whose reduction stopped
    // early, because it stopped on finding that the first third of the driving table all matches,
    // and a filter over the same keys would pass the same rows at the price of a hash each.
    let stopped = pushed.as_ref().is_some_and(|pushed| pushed.stopped);
    let exact = pushed.filter(|pushed| !pushed.stopped).map(|pushed| pushed.rids);
    let mut extremes = Extremes::default();
    let settled = exact.is_some() || stopped || reduced.is_some_and(|reduced| reduced.by_key);
    let mut keyed = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let keys = evaluate_all_in_time_zone(plan, &exprs, schema, chunk, time_zone)?;
        keyed.push((keys.into_iter().next(), chunk.len()));
    }
    if !settled && domain.is_none() {
        domain = dense(&keyed, rows);
    }
    // After the exact rows settled the scan's question, which leaves the join above that asked with
    // nothing, and the bitmap is one pass over a side that is usually the small one.
    let held = if wanted && domain.is_none() { dense(&keyed, rows) } else { None };
    let settled = settled || domain.is_some();
    let mut filter = if settled { None } else { Blocked::sized(rows, BUDGET) };
    let keys = if exact.is_none() && !stopped && rows <= KEYS { sorted(&keyed) } else { None };
    let mut hashes = Vec::new();
    for (keys, len) in &keyed {
        let (Some(keys), len) = (keys, *len) else { continue };
        extremes.widen(keys);
        let Some(filter) = filter.as_mut() else { continue };
        hash(std::slice::from_ref(keys), len, &mut hashes, Across::TwoInputs);
        // A null key matches nothing under the rule this is armed for, so it is left out here and a
        // driving row holding one is dropped by the filter it is missing from. That is the same
        // answer the hash table gives and it is arrived at a scan earlier.
        let nullable = has_nulls(keys, len);
        for (row, &word) in hashes.iter().enumerate() {
            if nullable && keys.is_null_at(row) {
                continue;
            }
            filter.add(word);
        }
    }
    let spare = spare.filter(|_| exact.is_some());
    Ok(Found {
        range: extremes.into_range(),
        filter,
        rows: exact,
        domain,
        spare,
        held,
        reduced,
        keys,
    })
}

/// The build side's keys as a [`Domain`] over their own range, when that range is small enough.
///
/// The filter hashes every driving row and still keeps about one in a hundred that the join will
/// drop. When the key is an integer and the keys the build side holds sit close together, one bit
/// per value between the smallest and the largest is exact, costs no hash, and is often smaller
/// than the filter. TPC-H is made of such keys: the orders of one year are two hundred and
/// twenty seven thousand keys spread over six million values, which is twenty six bits a key
/// against the filter's ten, and the suppliers of one region are two thousand over ten thousand.
///
/// Sixty four bits a key is the most this takes, and never more than the filter's own budget, past
/// which the filter is the smaller of the two and a bit test that misses the cache is no cheaper
/// than a filter lookup that does too. A bitmap of [`SMALL`] bits or fewer is taken however few keys
/// it holds, because it sits in the second level cache of any core this runs on, and there a bit
/// test is far cheaper than the hash and the four lane probe the filter takes, which counted out
/// at about sixty instructions a row. TPC-H q17 is the first case: two hundred and four parts over
/// two hundred thousand keys is a thousand bits a key and 25 KB, and the filter it made instead
/// cost a hash for each of six million rows and let through twelve times the rows that matched.
/// q4 is the second: the orders of one quarter are fifty seven thousand keys over six million
/// values, a hundred bits a key and 750 KB, and the bitmap took q4 from 0.756 G to 0.474 G
/// instructions on one thread. Only the four signed integer types of sixty four bits or
/// fewer, because the scan reads the driving column through the same widening and the join has
/// already made the two sides one type. `None` for anything else, and the filter is built instead.
fn dense(keyed: &[(Option<Vector>, usize)], rows: usize) -> Option<Domain> {
    let mut block = Vec::new();
    let mut low = i64::MAX;
    let mut high = i64::MIN;
    for (keys, len) in keyed {
        let Some(keys) = keys else { continue };
        if !integer(keys.logical_type()) || !keys.signed_block(&mut block) || block.len() < *len {
            return None;
        }
        let nullable = has_nulls(keys, *len);
        for (row, &key) in block[..*len].iter().enumerate() {
            if nullable && keys.is_null_at(row) {
                continue;
            }
            low = low.min(key);
            high = high.max(key);
        }
    }
    if low > high {
        return None;
    }
    let range = u64::try_from(i128::from(high) - i128::from(low) + 1).ok()?;
    let bytes = usize::try_from(range.div_ceil(8)).ok()?;
    if (range > (rows as u64).saturating_mul(64) && range > SMALL) || bytes > BUDGET {
        return None;
    }
    let mut words = vec![0_u64; words_for(range)?];
    for (keys, len) in keyed {
        let Some(keys) = keys else { continue };
        if !keys.signed_block(&mut block) {
            return None;
        }
        let nullable = has_nulls(keys, *len);
        for (row, &key) in block[..*len].iter().enumerate() {
            if nullable && keys.is_null_at(row) {
                continue;
            }
            let offset = key.wrapping_sub(low) as u64;
            words[(offset / 64) as usize] |= 1 << (offset % 64);
        }
    }
    Some(Domain { base: i128::from(low), range, words })
}

/// How many words a [`Domain`] over `range` keys takes, or `None` for more than memory holds.
///
/// One more bit than the range, always zero, so that [`Domain::keep`] can send every key outside
/// the range to it rather than branch around the read.
fn words_for(range: u64) -> Option<usize> {
    usize::try_from(range / 64 + 1).ok()
}

/// The build side's keys sorted and each once, or `None` when one is not an integer [`dense`] reads.
fn sorted(keyed: &[(Option<Vector>, usize)]) -> Option<Keys> {
    let mut block = Vec::new();
    let mut keys = Vec::new();
    for (column, len) in keyed {
        let Some(column) = column else { continue };
        if !integer(column.logical_type()) || !column.signed_block(&mut block) || block.len() < *len
        {
            return None;
        }
        let nullable = has_nulls(column, *len);
        for (row, &key) in block[..*len].iter().enumerate() {
            if !(nullable && column.is_null_at(row)) {
                keys.push(key);
            }
        }
    }
    keys.sort_unstable();
    keys.dedup();
    Some(Keys { keys })
}

/// The four signed integer types a key is read through a widening for, on both sides of the join.
fn integer(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer | LogicalType::BigInt
    )
}

/// The driving rows whose link points at a parent row the build side holds.
///
/// One lookup per build row to make a set of parent rows, then one push of that set through the
/// link, which skips every part of the driving table whose parents all fall outside it. The key
/// expression is evaluated here a second time rather than once for both, because it is a column
/// read and a side that is armed with this is the side the join was going to hash anyway.
///
/// `None` when a key does not read as an integer or is not in the key map. Neither should happen,
/// because the build side is a subset of the parent's rows and the map is over all of them. If one
/// does, the join gets the filter instead, which is slower and is never wrong. The push stops early
/// when the first third of the driving table all matches, see [`Rids::forward_or_stop`].
///
/// Declined when the push would decode half the driving table's parts or more and the parent's key
/// map spans a compact range, because then [`domain_of`] makes the same exact set as a bitmap over
/// the key values. What a push buys over that bitmap is the parts it skips, since a part the push
/// decodes costs the link and a bit test for every row and the scan then decodes it anyway to test
/// the key. TPC-H is the case where it does not pay: the orders of one quarter are spread over the
/// whole of `lineitem`, the push skipped no part of it, and with the push q04 took 0.94 G
/// instructions against 0.52 G with the bitmap. A monotone link is never declined, because its
/// push walks from one held parent to the next and costs about what the set holds, see
/// `Rids::forward`, and the exact rows it makes are cheaper for the scan than the bitmap.
fn reduce(keyed: &Keyed<'_>, exact: &Exact, chunks: &[Chunk]) -> Result<Option<Pushing>> {
    let Some(map) = exact.keys() else { return Ok(None) };
    let Some(link) = exact.link() else { return Ok(None) };
    let Some(held) = held_parents(keyed, map, link.parents(), chunks)? else { return Ok(None) };
    if map.span().is_some() && link.form() != Form::Monotone {
        let (reached, parts) = held.reach(link)?;
        if reached.saturating_mul(2) >= parts {
            return Ok(Some(Pushing::Declined));
        }
    }
    Ok(Some(Pushing::Done(held.forward_or_stop(link)?)))
}

/// The driving rows whose key the build side holds, read off the backward adjacency, when the
/// parents it holds have few children between them.
///
/// spec/graph/03-the-file-format.md section 3.5, for the case section 5.3 of the execution document
/// gives it: the children of a small selected set of parents. Without it the scan tests every
/// driving row against the build side's keys, and on TPC-H q17 that is six million tests on each of
/// two scans of `lineitem` to keep 6,088. With it the lists of the 204 parts are read and the scan
/// decodes those rows and no others, see `Scan::read_reduced`.
///
/// `None` when there is no adjacency, when a key is not in the key map, or when the parents'
/// children are more than one driving row in [`LISTED`]. Past that the scan reads most parts whole
/// anyway and the bitmap over the key values tests their rows for less than the lists cost.
fn listed(keyed: &Keyed<'_>, exact: &Exact, chunks: &[Chunk]) -> Result<Option<Pushed>> {
    let rows: u64 = chunks.iter().map(|chunk| chunk.len() as u64).sum();
    let Some(children) = exact.children.filter(|&children| children > 0) else { return Ok(None) };
    // A build row is at most one parent, and a parent has children / parents of them on average,
    // so a side holding more than a LISTED'th of the parents is expected to reach more than a
    // LISTED'th of the children and is not worth reading the adjacency for.
    if rows.saturating_mul(LISTED) >= exact.parents.min(children) {
        return Ok(None);
    }
    let Some(adjacency) = exact.adjacency() else { return Ok(None) };
    let Some(map) = exact.keys() else { return Ok(None) };
    let Some(held) = held_parents(keyed, map, adjacency.parents(), chunks)? else {
        return Ok(None);
    };
    if adjacency.reached(&held).saturating_mul(LISTED) >= children {
        return Ok(None);
    }
    let rids = adjacency.push(&held)?;
    let parts = children.div_ceil(PART_ROWS as u64);
    let mut touched = 0_u64;
    let mut last = None;
    for rid in rids.iter() {
        let part = rid / PART_ROWS as u64;
        if last != Some(part) {
            touched += 1;
            last = Some(part);
        }
    }
    Ok(Some(Pushed { rids, parts, skipped: parts - touched, stopped: false }))
}

/// A build side is read through the adjacency when its parents' children are fewer than one
/// driving row in this many. See [`listed`].
const LISTED: u64 = 8;

/// The parent rows whose keys the build side holds, as a set over the parent table.
///
/// `None` when a key does not read as an integer or is not in the key map, which should not happen
/// because the build side is a subset of the parent's rows.
fn held_parents(
    keyed: &Keyed<'_>,
    map: &KeyMap,
    parents: u64,
    chunks: &[Chunk],
) -> Result<Option<Rids>> {
    let (plan, exprs, schema, time_zone) = keyed.parts();
    let mut words = vec![0_u64; usize::try_from(parents.div_ceil(64)).unwrap_or(usize::MAX)];
    for chunk in chunks {
        let keys = evaluate_all_in_time_zone(plan, &exprs, schema, chunk, time_zone)?;
        let Some(keys) = keys.first() else { continue };
        let nullable = has_nulls(keys, chunk.len());
        for row in 0..chunk.len() {
            // A null key matches nothing under the rule this is armed for, the same as in the filter.
            if nullable && keys.is_null_at(row) {
                continue;
            }
            let Some(key) = keys.signed_at(row) else { return Ok(None) };
            let Some(rid) = map.lookup(key)? else { return Ok(None) };
            let Some(word) = usize::try_from(rid / 64).ok().and_then(|at| words.get_mut(at)) else {
                return Ok(None);
            };
            *word |= 1 << (rid % 64);
        }
    }
    Ok(Some(Rids::from_words(parents, words)?))
}

/// What [`reduce`] made of a join armed with a link.
enum Pushing {
    /// The set pushed through the link.
    Done(Pushed),
    /// Not pushed, because the key map's bitmap is the same set for less. See [`reduce`].
    Declined,
}

/// The build side's keys as a [`Domain`] over the parent's key range, and how many of them it holds.
///
/// Made for every join armed with a key map, before any push, and used unless a push is made. Only
/// when the map is one of the two forms with a compact range. `None` when a key does not read as an
/// integer or falls outside the range, which is a key no parent holds and should not happen, and
/// then the join gets the filter. The count is of distinct keys, which is of parents, because a bit
/// is set rather than added to.
fn domain_of(keyed: &Keyed<'_>, exact: &Exact, chunks: &[Chunk]) -> Result<Option<(Domain, u64)>> {
    let Some((base, range)) = exact.keys().and_then(KeyMap::span) else { return Ok(None) };
    let Some(len) = words_for(range) else { return Ok(None) };
    let (plan, exprs, schema, time_zone) = keyed.parts();
    let mut words = vec![0_u64; len];
    for chunk in chunks {
        let keys = evaluate_all_in_time_zone(plan, &exprs, schema, chunk, time_zone)?;
        let Some(keys) = keys.first() else { continue };
        let nullable = has_nulls(keys, chunk.len());
        for row in 0..chunk.len() {
            if nullable && keys.is_null_at(row) {
                continue;
            }
            let Some(key) = keys.signed_at(row) else { return Ok(None) };
            let Some(offset) = key.checked_sub(base).and_then(|at| u64::try_from(at).ok()) else {
                return Ok(None);
            };
            if offset >= range {
                return Ok(None);
            }
            words[(offset / 64) as usize] |= 1 << (offset % 64);
        }
    }
    let held = words.iter().map(|word| u64::from(word.count_ones())).sum();
    Ok(Some((Domain { base, range, words }, held)))
}

/// The smallest and largest key one side of a join holds, widened a chunk at a time.
///
/// The cheap half of [`found`], and the half that survives a column the filter could not be sized
/// for. Two comparisons a chunk against numbers already in a register.
#[derive(Debug, Default, Clone)]
pub(crate) struct Extremes {
    low: Option<Bound>,
    high: Option<Bound>,
}

impl Extremes {
    /// Widens this to cover one more chunk's worth of keys.
    ///
    /// A column with no ordered bound in it, which is a column of all nulls or of a type no bound
    /// compares with, widens this by nothing. That is right for the nulls, because a null key
    /// matches nothing under the rule this is armed for, and right for the type, because a column
    /// this cannot summarize leaves the range as it was and the range is only ever used to exclude.
    pub(crate) fn widen(&mut self, keys: &Vector) {
        let range = Range::of(keys);
        if let Some(low) = range.low {
            self.low = Some(match self.low.take() {
                Some(held) => held.smaller(low),
                None => low,
            });
        }
        if let Some(high) = range.high {
            self.high = Some(match self.high.take() {
                Some(held) => held.larger(high),
                None => high,
            });
        }
    }

    /// Both ends, or nothing when either end is missing.
    ///
    /// Both or neither, because a range with one open end excludes nothing on that side and a caller
    /// that had to check would be a caller that could forget.
    pub(crate) fn into_range(self) -> Option<(Bound, Bound)> {
        Some((self.low?, self.high?))
    }
}

impl Found {
    /// A build side that turned out to hold this, for the tests that stand in for one.
    #[cfg(test)]
    pub(crate) fn of(range: Option<(Bound, Bound)>, filter: Option<Blocked>) -> Self {
        Self { range, filter, ..Self::default() }
    }

    /// The same, with the keys as a sorted list beside the range.
    #[cfg(test)]
    pub(crate) fn listing(range: Option<(Bound, Bound)>, mut keys: Vec<i64>) -> Self {
        keys.sort_unstable();
        Self { keys: Some(Keys { keys }), ..Self::of(range, None) }
    }

    /// The same, with an exact set of driving rows.
    #[cfg(test)]
    pub(crate) fn exactly(range: Option<(Bound, Bound)>, rows: Rids) -> Self {
        Self { range, filter: None, rows: Some(rows), ..Self::default() }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::{Bound, Op};
    use rudb_common::{Field, LogicalType, SessionTimeZone, Value};
    use rudb_plan::{ColumnBinding, Expr, ExprRef, Plan};
    use rudb_storage::Blocked;
    use rudb_vector::{Chunk, Vector};

    use rudb_graph::{Adjacency, KeyMap, Link};

    use super::{
        Across, Exact, Extremes, Found, Keyed, SMALL, Schema, Sideways, beneath, found_for, hash,
    };

    fn found(
        keyed: &Keyed<'_>,
        exact: Option<&Exact>,
        chunks: &[Chunk],
    ) -> rudb_common::Result<Found> {
        found_for(keyed, exact, chunks, false, true)
    }

    fn column(values: &[Option<i32>]) -> Vector {
        let values: Vec<Value> =
            values.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
        Vector::from_values(LogicalType::Integer, &values).expect("a column of integers")
    }

    #[test]
    fn the_range_of_several_chunks_covers_every_one_of_them() {
        let mut extremes = Extremes::default();
        extremes.widen(&column(&[Some(5), Some(9)]));
        extremes.widen(&column(&[Some(2), Some(7)]));
        assert_eq!(extremes.into_range(), Some((Bound::Int(2), Bound::Int(9))));
    }

    /// A null is not a key under the rule this filter is armed for, so it widens nothing.
    #[test]
    fn a_column_of_nulls_widens_nothing() {
        let mut extremes = Extremes::default();
        extremes.widen(&column(&[Some(4)]));
        extremes.widen(&column(&[None, None]));
        assert_eq!(extremes.into_range(), Some((Bound::Int(4), Bound::Int(4))));
    }

    /// A build side with no keys at all leaves no range, which a scan reads as nothing to add.
    #[test]
    fn nothing_seen_is_no_range() {
        assert_eq!(Extremes::default().into_range(), None);
    }

    /// The scan asks by table index, so a range about another table's column is not this scan's.
    #[test]
    fn a_scan_is_told_only_about_its_own_column() {
        let sideways = Sideways::new();
        sideways.found(Found::of(Some((Bound::Int(1), Bound::Int(4))), None));
        // Armed without a key expression, which the scan does not read.
        sideways.about(ColumnBinding::new(7, 2));

        assert!(sideways.tests(8).is_empty(), "another table's scan");
        assert_eq!(
            sideways.tests(7),
            vec![(2, Op::GreaterOrEqual, Bound::Int(1)), (2, Op::LessOrEqual, Bound::Int(4)),]
        );
    }

    /// A join that never armed one, and a build side that finished with no range, both answer
    /// nothing, which is a scan that reads everything exactly as it did before.
    #[test]
    fn an_unarmed_handoff_and_an_empty_build_side_both_say_nothing() {
        let unarmed = Sideways::new();
        assert!(unarmed.tests(1).is_empty());
        assert!(unarmed.sifting(1).is_none());

        let empty = Sideways::new();
        empty.about(ColumnBinding::new(1, 0));
        empty.found(Found::of(None, None));
        assert!(empty.tests(1).is_empty());
        assert!(empty.sifting(1).is_none());
    }

    /// One chunk of one integer column, which is the shape a build side of one key column has.
    fn chunk(values: &[Option<i32>]) -> Chunk {
        Chunk::new(vec![column(values)]).expect("one column is one length")
    }

    /// A side of that column, cut into the chunks it would have arrived in.
    fn chunks(values: &[Option<i32>]) -> Vec<Chunk> {
        values.chunks(512).map(chunk).collect()
    }

    /// A key that is the only column of the side, which is what a `dim.k` in a join condition is.
    fn key(plan: &mut Plan) -> (ExprRef, Schema) {
        let expr = plan.add_expr(Expr::Column(ColumnBinding::new(1, 0)), LogicalType::Integer);
        let schema = Schema::numbered(vec![Field::new("k", LogicalType::Integer)], 1);
        (expr, schema)
    }

    /// Whether the filter would let a row holding each of `values` through, hashed the way the scan
    /// on the other side hashes the column it has just read.
    fn through(filter: &Blocked, values: &[Option<i32>]) -> Vec<bool> {
        let probe = column(values);
        let mut hashes = Vec::new();
        hash(std::slice::from_ref(&probe), values.len(), &mut hashes, Across::TwoInputs);
        hashes.iter().map(|&word| filter.holds(word)).collect()
    }

    #[test]
    fn a_build_side_is_read_for_both_its_range_and_its_keys() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, None, &[chunk(&[Some(5), Some(90_000_000)]), chunk(&[Some(2)])])
            .expect("a column of integers");

        assert_eq!(found.range, Some((Bound::Int(2), Bound::Int(90_000_000))));
        let filter = found.filter.expect("a filter over three keys");
        assert_eq!(through(&filter, &[Some(5), Some(90_000_000), Some(2)]), [true, true, true]);
    }

    /// The property the whole thing rests on: a filter says no about a key that is in it never, at
    /// any size. This one is forced small enough that its false positive rate is high, which is
    /// what makes a false negative show up rather than hide.
    #[test]
    fn no_key_that_went_in_is_ever_turned_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let keys: Vec<Option<i32>> = (0..4_000).map(|value| Some(value * 10_000 + 11)).collect();

        let found = found(&keyed, None, &chunks(&keys)).expect("a column of integers");

        let filter = found.filter.expect("a filter over four thousand keys");
        assert!(through(&filter, &keys).into_iter().all(|held| held), "a key it was given");
    }

    /// And the point of it: a key that never went in is usually turned away. Usually rather than
    /// always, because that is what a filter this size promises, and the join behind it is what
    /// makes the difference correct rather than merely rare.
    #[test]
    fn a_key_the_build_side_never_held_is_nearly_always_turned_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let keys: Vec<Option<i32>> = (0..4_000).map(|value| Some(value * 10_000 + 11)).collect();
        let absent: Vec<Option<i32>> =
            (0..4_000).map(|value| Some(value * 7 + 100_000_000)).collect();

        let found = found(&keyed, None, &chunks(&keys)).expect("a column of integers");

        let filter = found.filter.expect("a filter over four thousand keys");
        let through = through(&filter, &absent).into_iter().filter(|&held| held).count();
        assert!(through < absent.len() / 10, "{through} of {} got through", absent.len());
    }

    /// A null key matches nothing under the rule this is armed for, so it is not a key the filter
    /// holds, and a driving row holding one is dropped by a filter it was never put in.
    #[test]
    fn a_null_is_not_a_key_the_filter_holds() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found =
            found(&keyed, None, &[chunk(&[Some(3), None, Some(40_000_000)])]).expect("integers");

        assert_eq!(found.range, Some((Bound::Int(3), Bound::Int(40_000_000))));
        assert_eq!(through(&found.filter.expect("a filter"), &[None]), [false]);
    }

    /// Keys that sit close together become a bitmap over their own range in place of the filter.
    /// It keeps exactly the driving rows whose key the side holds and drops a null, a key under the
    /// smallest, one over the largest and one between them that the side does not hold.
    #[test]
    fn keys_close_together_are_kept_as_a_bitmap_instead_of_a_filter() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, None, &[chunk(&[Some(-4), None, Some(60)]), chunk(&[Some(7)])])
            .expect("integers");

        assert!(found.filter.is_none(), "the bitmap is exact, so no filter is built beside it");
        assert_eq!(found.range, Some((Bound::Int(-4), Bound::Int(60))));
        let domain = found.domain.expect("a bitmap over sixty five values");
        let driving = column(&[Some(7), Some(8), None, Some(-4), Some(-5), Some(61), Some(60)]);
        let kept = domain.keep(&driving, driving.len(), &mut Vec::new());
        assert_eq!(kept, [0, 3, 6]);
        let whole = column(&[Some(60), Some(1), Some(7), Some(i32::MIN), Some(i32::MAX)]);
        assert_eq!(domain.keep(&whole, whole.len(), &mut Vec::new()), [0, 2]);
    }

    /// A range that ends on a word boundary still has the bit past its end to send a miss to, so a
    /// key one past the largest, far past it or under the smallest is dropped and not read out of
    /// the next word or out of bounds.
    #[test]
    fn a_bitmap_whose_range_fills_its_words_drops_every_key_outside_it() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, None, &[chunk(&[Some(0), Some(63), Some(64), Some(127)])])
            .expect("integers");

        let domain = found.domain.expect("a bitmap over a hundred and twenty eight values");
        assert_eq!(domain.range, 128);
        let driving = column(&[
            Some(127),
            Some(128),
            Some(129),
            Some(191),
            Some(192),
            Some(-1),
            Some(i32::MAX),
            Some(64),
        ]);
        assert_eq!(domain.keep(&driving, driving.len(), &mut Vec::new()), [0, 7]);
    }

    /// Past sixty four bits a key the bitmap is bigger than the filter it would replace, once it is
    /// too big to sit in the second level cache.
    #[test]
    fn keys_spread_wide_still_get_a_filter() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let wide = i32::try_from(SMALL).expect("small");

        let found = found(&keyed, None, &[chunk(&[Some(0), Some(wide)])]).expect("integers");

        assert!(found.domain.is_none() && found.filter.is_some());
    }

    /// A few keys over a range that fits the second level cache are a bitmap however far apart they
    /// are, which is TPC-H q17's two hundred and four parts over two hundred thousand keys and q4's
    /// orders of one quarter over six million.
    #[test]
    fn a_few_keys_over_a_small_range_are_a_bitmap_however_far_apart() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let edge = i32::try_from(SMALL).expect("small") - 1;

        let found = found(&keyed, None, &[chunk(&[Some(0), Some(edge)])]).expect("integers");

        assert!(found.filter.is_none(), "the bitmap takes the filter's place");
        let domain = found.domain.expect("two keys over the cache sized range");
        let probe = column(&[Some(0), Some(1), Some(edge), Some(edge + 1)]);
        assert_eq!(domain.keep(&probe, 4, &mut Vec::new()), [0, 2]);
    }

    /// A side that gathered nothing leaves a filter that holds nothing, which is a scan that drops
    /// every row it reads, which is the right answer for a join whose other side is empty.
    #[test]
    fn an_empty_build_side_turns_every_driving_row_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());

        let found = found(&keyed, None, &[]).expect("nothing to read");

        assert_eq!(found.range, None);
        assert_eq!(through(&found.filter.expect("a filter of no keys"), &[Some(1)]), [false]);
    }

    /// Section 5.4 on the smallest case that shows it. Parents keyed 100 upwards, a thousand children
    /// pointing at each in order, so a part of the driving table is about one parent's, and a build
    /// side holding two of the keys: the rows kept are exactly the children of those two parents,
    /// and no filter is built beside them.
    #[test]
    fn an_exact_side_keeps_the_children_of_the_parents_it_holds_and_no_others() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..50).map(|rid| Some(100 + rid)).collect();
        let parents_of: Vec<u64> = (0..50_000).map(|child| child / 1_000).collect();
        let exact = Exact::new(
            KeyMap::build(&parent_keys).expect("unique keys"),
            Some(Link::build(&parents_of, 50).expect("every parent exists")),
        );

        let found = found(&keyed, Some(&exact), &[chunk(&[Some(103), None]), chunk(&[Some(140)])])
            .expect("integers");

        assert!(found.filter.is_none(), "the exact rows make the filter redundant");
        let rows = found.rows.expect("an exact side");
        let kept: Vec<u64> = rows.iter().collect();
        let expected: Vec<u64> = (3_000..4_000).chain(40_000..41_000).collect();
        assert_eq!(kept, expected);
        assert_eq!(found.range, Some((Bound::Int(103), Bound::Int(140))));
    }

    /// The same answer read from the other end. Children scattered over the parents, so the key
    /// test would have to visit every child row, and the adjacency lists the ones two parents own.
    #[test]
    fn an_adjacency_lists_the_children_of_the_parents_a_side_holds() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..50).map(|rid| Some(100 + rid)).collect();
        let parents_of: Vec<u64> = (0..50_000).map(|child| child % 50).collect();
        let exact = Exact::adjacent(
            KeyMap::build(&parent_keys).expect("unique keys"),
            Adjacency::build(&parents_of, 50).expect("every parent exists"),
        );

        let found = found(&keyed, Some(&exact), &[chunk(&[Some(103), None]), chunk(&[Some(140)])])
            .expect("integers");

        let rows = found.rows.expect("an exact side");
        let kept: Vec<u64> = rows.iter().collect();
        let expected: Vec<u64> =
            (0..50_000).filter(|child| child % 50 == 3 || child % 50 == 40).collect();
        assert_eq!(kept, expected);
    }

    /// The exact rows answer the scan, which leaves no bitmap behind for a join above that narrows
    /// its own table by these keys. Asked for, the side makes one anyway, and it holds exactly the
    /// keys the side holds.
    #[test]
    fn an_exact_side_asked_for_its_keys_holds_them_as_a_bitmap_as_well() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..50).map(|rid| Some(100 + rid)).collect();
        let parents_of: Vec<u64> = (0..50_000).map(|child| child / 1_000).collect();
        let exact = Exact::new(
            KeyMap::build(&parent_keys).expect("unique keys"),
            Some(Link::build(&parents_of, 50).expect("every parent exists")),
        );
        let side = [chunk(&[Some(103), None]), chunk(&[Some(140)])];

        let unasked = found(&keyed, Some(&exact), &side).expect("integers");
        assert!(unasked.rows.is_some() && unasked.domain.is_none() && unasked.held.is_none());

        let asked = found_for(&keyed, Some(&exact), &side, true, true).expect("integers");
        assert!(asked.rows.is_some(), "the scan is still answered by the exact rows");
        let held = asked.held.expect("a bitmap for the join above");
        let kept: Vec<i64> = (90..160).filter(|&key| held.holds(key)).collect();
        assert_eq!(kept, [103, 140]);
    }

    /// A filter below leaves the key column as codes into the run the scan read, which is the form
    /// the build side of a join over a filtered table arrives in, and the bitmap is made from it all
    /// the same.
    #[test]
    fn a_side_whose_keys_are_a_dictionary_still_makes_its_bitmap() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let run = column(&[Some(10), Some(11), Some(12), Some(13), Some(14)]);
        let codes = Vector::dictionary(vec![4, 1, 4], run).expect("codes inside the run");
        let side = [Chunk::new(vec![codes]).expect("one column")];

        let found = found(&keyed, None, &side).expect("integers");

        let domain = found.domain.expect("a bitmap over the keys the codes name");
        let kept: Vec<i64> = (0..20).filter(|&key| domain.holds(key)).collect();
        assert_eq!(kept, [11, 14]);
    }

    /// A key the map has never heard of cannot come from the parent, so something upstream is not
    /// what the builder checked for. The join falls back to the filter rather than trusting the set.
    #[test]
    fn a_key_the_parent_does_not_hold_falls_back_to_the_filter() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let exact = Exact::new(
            KeyMap::build(&[Some(1), Some(2), Some(3), Some(4)]).expect("unique keys"),
            Some(Link::build(&[0, 1, 1, 2, 3], 4).expect("every parent exists")),
        );

        let found =
            found(&keyed, Some(&exact), &[chunk(&[Some(1), Some(9_000_000)])]).expect("integers");

        assert!(found.rows.is_none());
        assert!(found.filter.is_some(), "the filter is what the join gets instead");
    }

    /// With no link in the file the keys become a bitmap over the parent's key range. It keeps the
    /// driving rows whose key the build side holds and drops a null, a key outside the range and a
    /// key inside it the side does not hold, and it says how many parent keys it kept.
    #[test]
    fn a_side_with_no_link_keeps_the_keys_it_holds_as_a_bitmap() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..200).map(|rid| Some(100 + rid)).collect();
        let exact = Exact::new(KeyMap::build(&parent_keys).expect("unique keys"), None);

        let found = found(&keyed, Some(&exact), &[chunk(&[Some(103), None]), chunk(&[Some(299)])])
            .expect("integers");

        assert!(found.filter.is_none() && found.rows.is_none(), "the bitmap is the whole answer");
        let reduced = found.reduced.expect("a reduction to report");
        assert_eq!((reduced.kept, reduced.rows, reduced.by_key), (2, 200, true));
        let domain = found.domain.expect("a bitmap");
        let driving = chunk(&[Some(103), Some(104), None, Some(299), Some(300), Some(99)]);
        let mut block = Vec::new();
        assert_eq!(domain.keep(&driving.columns()[0], driving.len(), &mut block), [0, 3]);
    }

    /// A side with as many rows as the parent has keys holds about every parent, so the key map is
    /// never read for it. It is handed what a join with no key map gets, which here is the bitmap
    /// the scan measures and stops testing once it keeps everything.
    #[test]
    fn a_side_as_long_as_the_parent_does_not_read_the_key_map() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let exact = Exact::new(KeyMap::build(&[Some(1), Some(2)]).expect("unique keys"), None);
        let side = [chunk(&[Some(2), Some(1)])];

        let armed = found(&keyed, Some(&exact), &side).expect("integers");

        assert!(armed.reduced.is_none() && armed.rows.is_none(), "the key map is not asked");
        let alone = found(&keyed, None, &side).expect("integers");
        assert_eq!(armed.domain.is_some(), alone.domain.is_some());
        assert!(exact.keys.get().is_some(), "handed over already read, so nothing was loaded");
    }

    /// A thousand parents with a hundred children each, the children dealt round the parents so
    /// that a part of the driving table points at all of them and the link is packed. A side that
    /// holds every fifth parent leaves no part with none of them, so a push would skip nothing and
    /// the bitmap over the keys is what the scan gets.
    #[test]
    fn a_packed_link_is_pushed_only_where_it_could_skip_parts() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..1_000).map(|rid| Some(100 + rid)).collect();
        let parents_of: Vec<u64> = (0..100_000).map(|child| child % 1_000).collect();
        let link = Link::build(&parents_of, 1_000).expect("every parent exists");
        assert_eq!(link.form(), super::Form::Packed);
        let exact = Exact::new(KeyMap::build(&parent_keys).expect("unique keys"), Some(link));

        let spread: Vec<Option<i32>> = (0..1_000).step_by(5).map(|rid| Some(100 + rid)).collect();
        let wide = found(&keyed, Some(&exact), &[chunk(&spread)]).expect("integers");
        assert!(wide.rows.is_none(), "no push");
        assert!(wide.domain.is_some() && wide.filter.is_none(), "the bitmap is the answer");
        let reduced = wide.reduced.expect("a reduction to report");
        assert_eq!((reduced.kept, reduced.rows, reduced.by_key), (200, 1_000, true));
    }

    /// The same parents with their children in parent order, which is the monotone form. The push
    /// costs what the side holds, so it is made however spread the side is, and the bitmap is kept
    /// beside the rows for a scan that reads this join as one of several.
    #[test]
    fn a_monotone_link_is_pushed_however_spread_the_side_is() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..1_000).map(|rid| Some(100 + rid)).collect();
        let parents_of: Vec<u64> = (0..100_000).map(|child| child / 100).collect();
        let exact = Exact::new(
            KeyMap::build(&parent_keys).expect("unique keys"),
            Some(Link::build(&parents_of, 1_000).expect("every parent exists")),
        );

        let spread: Vec<Option<i32>> = (0..1_000).step_by(5).map(|rid| Some(100 + rid)).collect();
        let wide = found(&keyed, Some(&exact), &[chunk(&spread)]).expect("integers");
        let rows = wide.rows.expect("a push");
        let expected: Vec<u64> = (0..100_000).filter(|child| child / 100 % 5 == 0).collect();
        assert_eq!(rows.iter().collect::<Vec<u64>>(), expected);
        assert!(wide.domain.is_none() && wide.spare.is_some());

        let near =
            found(&keyed, Some(&exact), &[chunk(&[Some(600), Some(601)])]).expect("integers");
        let rows = near.rows.expect("a push");
        assert_eq!(rows.iter().collect::<Vec<u64>>(), (50_000..50_200).collect::<Vec<u64>>());
        let reduced = near.reduced.expect("a reduction to report");
        assert!(!reduced.by_key);

        let aside =
            found_for(&keyed, Some(&exact), &[chunk(&spread)], false, false).expect("integers");
        assert!(aside.rows.is_none(), "a scan that cannot place rows is not given any");
        assert!(aside.domain.is_some(), "and gets the bitmap in their place");
    }

    /// A driving side written as plan text, which is how every other operator test in this crate
    /// builds one.
    fn driving(text: &str) -> Plan {
        Plan::parse(text).expect("the plan text round trips")
    }

    /// The case that is in every plan over a view: the join names the projection's column and the
    /// scan under it names its own, and without the walk between them the filter is built, handed
    /// over and read by nobody.
    #[test]
    fn a_projection_between_the_join_and_the_scan_renames_the_column_the_filter_is_about() {
        let plan = driving(
            "Project #1 [#0.1::INTEGER AS k]\n  \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [a::INTEGER, k::INTEGER]",
        );

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(1, 0)),
            Some(ColumnBinding::new(0, 1)),
            "the scan's own name for the projection's column"
        );
    }

    /// A filter keeps rows and renames nothing, so the binding goes through it as it stands, and
    /// this is the shape a join over a filtered fact table drives with.
    #[test]
    fn a_filter_between_the_two_leaves_the_binding_alone() {
        let plan = driving(
            "Project #1 [#0.0::INTEGER AS k]\n  \
             Filter (#0.0::INTEGER > 3::INTEGER)::BOOLEAN\n    \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]",
        );

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(1, 0)),
            Some(ColumnBinding::new(0, 0))
        );
    }

    /// A projection that computes its column ends the walk, because a set of values says nothing
    /// about what an expression over them produces, and a scan dropping rows on that would be rows
    /// missing from the answer.
    #[test]
    fn a_computed_column_is_not_a_column_the_filter_can_be_about() {
        let plan = driving(
            "Project #1 [(#0.0::INTEGER > 3::INTEGER)::BOOLEAN AS k]\n  \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]",
        );

        assert_eq!(beneath(&plan, plan.root(), ColumnBinding::new(1, 0)), None);
    }

    /// And the walk has to end at the scan the binding is about by then, so a driving side with a
    /// node in the way, or one about another table's column, arms nothing.
    #[test]
    fn a_walk_that_does_not_reach_the_scan_it_is_about_arms_nothing() {
        let plan = driving(
            "Limit 5 offset 0\n  \
             TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]",
        );

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(0, 0)),
            None,
            "a node in the way"
        );

        let plan = driving("TableFunction read_parquet args=['f'::VARCHAR] #0 [k::INTEGER]");

        assert_eq!(
            beneath(&plan, plan.root(), ColumnBinding::new(3, 0)),
            None,
            "another table's column"
        );
    }
}
