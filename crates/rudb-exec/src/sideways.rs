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
use rudb_common::{Result, SessionTimeZone};
use rudb_graph::{KeyMap, Link, Pushed, Rids};
use rudb_metrics::Reduced;
use rudb_plan::{ColumnBinding, Expr, ExprRef, Node, NodeRef, Plan};
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
    /// What the exact reduction came to, for the scan to report, including one that stopped early
    /// and so left `rows` empty.
    reduced: Option<Reduced>,
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
#[derive(Debug)]
pub(crate) struct Exact {
    /// Which parent row holds a key.
    keys: KeyMap,
    /// Which parent row every driving row points at, when the link is in the file. Without it the
    /// build side's keys become a [`Domain`] instead.
    link: Option<Link>,
}

impl Exact {
    /// The key map of the parent and the link from the driving table to it, if there is one.
    pub(crate) fn new(keys: KeyMap, link: Option<Link>) -> Self {
        Self { keys, link }
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

    /// The build side's keys as a bitmap the scan of `index` tests its rows against, and which of
    /// its columns holds the key. See [`Domain`].
    pub(crate) fn domain(&self, index: u32) -> Option<(usize, &Domain)> {
        let binding = self.binding.get()?;
        if binding.table != index {
            return None;
        }
        Some((binding.column as usize, self.found.get()?.domain.as_ref()?))
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
            _ => return None,
        }
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
pub(crate) fn found(keyed: &Keyed<'_>, exact: Option<&Exact>, chunks: &[Chunk]) -> Result<Found> {
    let (plan, exprs, schema, time_zone) = keyed.parts();
    let pushed = exact.map(|exact| reduce(keyed, exact, chunks)).transpose()?.flatten();
    let mut reduced = pushed.as_ref().map(|pushed| Reduced {
        kept: pushed.rids.len(),
        rows: pushed.rids.rows(),
        stopped: pushed.stopped,
        by_key: false,
    });
    let mut domain = None;
    if let Some((bitmap, held)) =
        exact.map(|exact| domain_of(keyed, exact, chunks)).transpose()?.flatten()
    {
        let parents = exact.map_or(0, |exact| exact.keys.len());
        reduced = Some(Reduced { kept: held, rows: parents, stopped: false, by_key: true });
        // A side that holds every parent key removes only the rows whose key no parent holds, and
        // the join drops those as cheaply, so testing every row would buy nothing.
        domain = (held < parents).then_some(bitmap);
    }
    // The exact rows answer everything the filter would, with no false positives, so a side that
    // has them does not pay for building the filter too. Nor does a side whose reduction stopped
    // early, because it stopped on finding that the first third of the driving table all matches,
    // and a filter over the same keys would pass the same rows at the price of a hash each.
    let stopped = pushed.as_ref().is_some_and(|pushed| pushed.stopped);
    let exact = pushed.filter(|pushed| !pushed.stopped).map(|pushed| pushed.rids);
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    let mut extremes = Extremes::default();
    let settled = exact.is_some() || stopped || reduced.is_some_and(|reduced| reduced.by_key);
    let mut filter = if settled { None } else { Blocked::sized(rows, BUDGET) };
    let mut hashes = Vec::new();
    for chunk in chunks {
        let keys = evaluate_all_in_time_zone(plan, &exprs, schema, chunk, time_zone)?;
        let Some(keys) = keys.first() else { continue };
        extremes.widen(keys);
        let Some(filter) = filter.as_mut() else { continue };
        hash(std::slice::from_ref(keys), chunk.len(), &mut hashes, Across::TwoInputs);
        // A null key matches nothing under the rule this is armed for, so it is left out here and a
        // driving row holding one is dropped by the filter it is missing from. That is the same
        // answer the hash table gives and it is arrived at a scan earlier.
        let nullable = has_nulls(keys, chunk.len());
        for (row, &word) in hashes.iter().enumerate() {
            if nullable && keys.is_null_at(row) {
                continue;
            }
            filter.add(word);
        }
    }
    Ok(Found { range: extremes.into_range(), filter, rows: exact, domain, reduced })
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
fn reduce(keyed: &Keyed<'_>, exact: &Exact, chunks: &[Chunk]) -> Result<Option<Pushed>> {
    let Some(link) = exact.link.as_ref() else { return Ok(None) };
    let (plan, exprs, schema, time_zone) = keyed.parts();
    let parents = link.parents();
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
            let Some(rid) = exact.keys.lookup(key)? else { return Ok(None) };
            let Some(word) = usize::try_from(rid / 64).ok().and_then(|at| words.get_mut(at)) else {
                return Ok(None);
            };
            *word |= 1 << (rid % 64);
        }
    }
    let held = Rids::from_words(parents, words)?;
    Ok(Some(held.forward_or_stop(link)?))
}

/// The build side's keys as a [`Domain`] over the parent's key range, and how many of them it holds.
///
/// Only for a join armed with a key map and no link, and only when the map is one of the two forms
/// with a compact range. `None` when a key does not read as an integer or falls outside the range,
/// which is a key no parent holds and should not happen, and then the join gets the filter. The
/// count is of distinct keys, which is of parents, because a bit is set rather than added to.
fn domain_of(keyed: &Keyed<'_>, exact: &Exact, chunks: &[Chunk]) -> Result<Option<(Domain, u64)>> {
    if exact.link.is_some() {
        return Ok(None);
    }
    let Some((base, range)) = exact.keys.span() else { return Ok(None) };
    let Ok(len) = usize::try_from(range.div_ceil(64)) else { return Ok(None) };
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
        Self { range, filter, rows: None, domain: None, reduced: None }
    }

    /// The same, with an exact set of driving rows.
    #[cfg(test)]
    pub(crate) fn exactly(range: Option<(Bound, Bound)>, rows: Rids) -> Self {
        Self { range, filter: None, rows: Some(rows), domain: None, reduced: None }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::bounds::{Bound, Op};
    use rudb_common::{Field, LogicalType, SessionTimeZone, Value};
    use rudb_plan::{ColumnBinding, Expr, ExprRef, Plan};
    use rudb_storage::Blocked;
    use rudb_vector::{Chunk, Vector};

    use rudb_graph::{KeyMap, Link};

    use super::{Across, Exact, Extremes, Found, Keyed, Schema, Sideways, beneath, found, hash};

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

        let found = found(&keyed, None, &[chunk(&[Some(5), Some(9)]), chunk(&[Some(2)])])
            .expect("a column of integers");

        assert_eq!(found.range, Some((Bound::Int(2), Bound::Int(9))));
        let filter = found.filter.expect("a filter over three keys");
        assert_eq!(through(&filter, &[Some(5), Some(9), Some(2)]), [true, true, true]);
    }

    /// The property the whole thing rests on: a filter says no about a key that is in it never, at
    /// any size. This one is forced small enough that its false positive rate is high, which is
    /// what makes a false negative show up rather than hide.
    #[test]
    fn no_key_that_went_in_is_ever_turned_away() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let keys: Vec<Option<i32>> = (0..4_000).map(|value| Some(value * 7 + 11)).collect();

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
        let keys: Vec<Option<i32>> = (0..4_000).map(|value| Some(value * 7 + 11)).collect();
        let absent: Vec<Option<i32>> =
            (0..4_000).map(|value| Some(value * 7 + 1_000_000)).collect();

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

        let found = found(&keyed, None, &[chunk(&[Some(3), None, Some(4)])]).expect("integers");

        assert_eq!(found.range, Some((Bound::Int(3), Bound::Int(4))));
        assert_eq!(through(&found.filter.expect("a filter"), &[None]), [false]);
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

    /// Section 5.4 on the smallest case that shows it. Parents keyed 100 upwards, children pointing
    /// at them in order, and a build side holding two of the keys: the rows kept are exactly the
    /// children of those two parents, and no filter is built beside them.
    #[test]
    fn an_exact_side_keeps_the_children_of_the_parents_it_holds_and_no_others() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let parent_keys: Vec<Option<i128>> = (0..50).map(|rid| Some(100 + rid)).collect();
        let parents_of: Vec<u64> = (0..5_000).map(|child| child / 100).collect();
        let exact = Exact::new(
            KeyMap::build(&parent_keys).expect("unique keys"),
            Some(Link::build(&parents_of, 50).expect("every parent exists")),
        );

        let found = found(&keyed, Some(&exact), &[chunk(&[Some(103), None]), chunk(&[Some(140)])])
            .expect("integers");

        assert!(found.filter.is_none(), "the exact rows make the filter redundant");
        let rows = found.rows.expect("an exact side");
        let kept: Vec<u64> = rows.iter().collect();
        let expected: Vec<u64> = (300..400).chain(4_000..4_100).collect();
        assert_eq!(kept, expected);
        assert_eq!(found.range, Some((Bound::Int(103), Bound::Int(140))));
    }

    /// A key the map has never heard of cannot come from the parent, so something upstream is not
    /// what the builder checked for. The join falls back to the filter rather than trusting the set.
    #[test]
    fn a_key_the_parent_does_not_hold_falls_back_to_the_filter() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let exact = Exact::new(
            KeyMap::build(&[Some(1), Some(2)]).expect("unique keys"),
            Some(Link::build(&[0, 1, 1], 2).expect("both parents exist")),
        );

        let found = found(&keyed, Some(&exact), &[chunk(&[Some(1), Some(9)])]).expect("integers");

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

    /// A side that holds every parent key would drop only rows no parent holds, which the join drops
    /// as cheaply, so it reports what it found and hands the scan nothing to test.
    #[test]
    fn a_side_that_holds_every_parent_key_tests_nothing() {
        let mut plan = Plan::new();
        let (expr, schema) = key(&mut plan);
        let keyed = Keyed::new(&plan, expr, schema, SessionTimeZone::default());
        let exact = Exact::new(KeyMap::build(&[Some(1), Some(2)]).expect("unique keys"), None);

        let found = found(&keyed, Some(&exact), &[chunk(&[Some(2), Some(1)])]).expect("integers");

        assert!(found.domain.is_none() && found.filter.is_none());
        assert_eq!(found.reduced.map(|reduced| reduced.kept), Some(2));
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
