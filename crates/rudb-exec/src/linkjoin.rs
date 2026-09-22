//! The join that reads a stored link instead of building a hash table.
//!
//! spec/graph/05-execution.md section 5.2, in one sentence: scan the child, read the link column
//! beside the data columns, and emit the parent's projected columns as gathers. There is no build
//! side, there is no hash table, there is no probe, and there is no materialisation of the parent
//! at all.
//!
//! That is the whole operator. Per child row it is one read out of a bit packed column, one bounds
//! check, and a `u32` written into a buffer. What a hash join spends on the same row is a hash, a
//! probe, a compare and a gather, on top of a build that read the parent side into a table first.
//!
//! # Why the parent columns come out as gathers and not as rows
//!
//! Because most of them are never read. A `Gathered` vector is an `Arc` to the parent's column and
//! the row ids to take from it, so a parent column the query projects but never touches before the
//! final output costs one pointer per chunk rather than one value per child row. On TPC-H Q3 that
//! is `o_orderdate` and `o_shippriority`, both of which are only grouped on.
//!
//! The other half is section 8.2's dispatch rule. A kernel handed a gathered column can fold over
//! the distinct parent rows that were actually reached rather than over one value per child row,
//! which on a many to one join is the same argument the engine already makes for dictionaries. The
//! operator does not have to do anything to get that: it hands out the form and the kernels read
//! it.
//!
//! # The four kinds
//!
//! Inner drops the child rows whose link is the *no parent* sentinel. Left keeps them and gathers
//! null, which is what the sentinel already reads as, so left is the kind that does no selection at
//! all. Semi and anti are a test of the sentinel and never touch the parent, which makes `EXISTS`
//! over a foreign key nearly free and is worth stating because it is extremely common.
//!
//! Right and full need the parent rows that nothing pointed at, which is the backward direction and
//! is not this operator. `Plan::check` refuses them, so this never sees one.
//!
//! # What makes this safe to run at all
//!
//! Two checks, neither of them optional, both of them section 3.1's rule that a graph section
//! changes the time and never the answer.
//!
//! The link is taken from the file only when the file is the whole table and the generation stamp
//! on the section matches the parent it was built against. [`rudb_catalog::Rows::stored`] is the
//! first of those and [`rudb_native::graph::stored_link`] is the second.
//!
//! And every row id this operator writes is checked against the length of the column it points
//! into, by [`Vector::gathered`], on every chunk. That check is the only thing between a link built
//! against a table that has since been rewritten and a read of whatever happens to be at that
//! offset.

use std::sync::{Arc, Mutex};

use rudb_catalog::Parent;
use rudb_common::{Cancel, Error, LogicalType, Memory, Reservation, Result, Session};
use rudb_graph::Link;
use rudb_pipeline::{Compaction, Gauge, Lease, Progress, Stream, narrow};
use rudb_plan::{ExprRef, JoinKind, Plan};
use rudb_seam::{Context, SeamId, Settings};
use rudb_vector::{Chunk, Data, NO_ROW, Selection, Vector};

use crate::prepared::{Prepared, Scratch};
use crate::register::compaction;
use crate::schema::Schema;

/// A join answered by one forward link rather than by a hash table.
#[derive(Debug)]
pub(crate) struct LinkJoin {
    /// Inner, left, semi or anti. Nothing else reaches here.
    kind: JoinKind,
    /// The forward link, read out of the child table's file once when the operator was built.
    link: Arc<Link>,
    /// The parent's columns, held whole and read at most once each.
    ///
    /// Shared with nothing, but behind an `Arc` because every gathered vector this operator hands
    /// out holds one of its columns for as long as something upstream is still reading it, which
    /// outlives the chunk and may outlive the pipeline.
    parent: Arc<Parent>,
    /// Which stored column of the parent each projected parent column is, with its type, in the
    /// order this operator produces them.
    ///
    /// Empty for a semi or an anti join, which is not a special case here but the reason those two
    /// never touch the parent: the loop below is the same loop and the list it walks is empty.
    projected: Vec<(usize, LogicalType)>,
    /// The child column holding the row id, which the plan named.
    rid: Prepared,
    schema: Schema,
    compaction: &'static dyn Compaction,
    /// What the parent's columns are charged, held for as long as they are readable.
    held: Mutex<Reservation>,
    cancel: Cancel,
}

/// Everything one instance of a link join mutates.
#[derive(Debug)]
pub(crate) struct Linking {
    scratch: Scratch,
    /// The parent row id of each child row in the chunk, with [`NO_ROW`] where there is none.
    ///
    /// One buffer per instance, reused per chunk, and handed to every projected parent column of
    /// that chunk at once. Eight gathered columns off one chunk are eight pointers and one buffer.
    rids: Vec<u32>,
    gauge: Gauge,
}

impl LinkJoin {
    /// Applies the session semantics to the row id expression's prepared casts.
    #[must_use]
    pub(crate) fn in_session(mut self, session: &Session) -> Self {
        self.rid = self.rid.in_session(session);
        self
    }

    /// An operator gathering `projected` out of `parent` through `link`.
    ///
    /// # Errors
    ///
    /// If the row id expression does not resolve against the child's schema, if it is not
    /// `BIGINT`, or if the session has pinned the compaction seam to something that cannot run over
    /// these columns.
    #[expect(clippy::too_many_arguments, reason = "an operator's inputs, none of them a group")]
    pub(crate) fn new(
        plan: &Plan,
        kind: JoinKind,
        link: Arc<Link>,
        parent: Arc<Parent>,
        projected: Vec<(usize, LogicalType)>,
        rid: ExprRef,
        child: &Schema,
        gathered: &Schema,
        seams: &Settings,
        memory: &Memory,
        cancel: Cancel,
    ) -> Result<Self> {
        if !matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti) {
            return Err(Error::internal("a link join was built for a kind a link cannot answer"));
        }
        let types = child.types();
        let context = Context::new(SeamId::ChunkCompaction, seams).with_types(&types);
        let compaction = compaction().choose(&context)?.strategy();
        let rid = Prepared::one(plan, rid, child)?;
        Ok(Self {
            kind,
            link,
            parent,
            projected,
            rid,
            // The child's columns and then the parent's, which is the order a join binds its
            // output in and the order the gathers are pushed on below.
            schema: Schema::concat(child, gathered),
            compaction,
            held: Mutex::new(memory.reservation()),
            cancel,
        })
    }

    /// What this operator produces.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Fills `rids` with one parent row id per row of the chunk.
    ///
    /// The sentinel covers both halves of *no parent*: a child whose key is null and a child whose
    /// key matched nothing. Section 2.4 keeps the two apart where an anti join needs them apart, by
    /// reading the child's own validity, and neither of the kinds here asks that question.
    fn resolve(&self, chunk: &Chunk, local: &mut Linking) -> Result<()> {
        let rows = chunk.len();
        let ids = self.rid.evaluate_one(chunk, &mut local.scratch)?.flatten()?;
        let held: &[i64] = match ids.data() {
            Some(Data::Int64(values)) if !ids.validity().has_nulls(rows) => values.as_slice(),
            // A row id is produced by a scan as a sequence over the part's first row, so a null one
            // is not a row whose identity is unknown but a plan that named the wrong column.
            _ => return Err(Error::internal("a link join was handed invalid child row ids")),
        };
        let held = held.get(..rows).ok_or_else(|| {
            Error::internal("a link join was handed fewer row ids than the chunk has rows")
        })?;
        local.rids.clear();
        local.rids.reserve(rows);
        for &id in held {
            let child = u64::try_from(id)
                .map_err(|_| Error::internal("a link join was handed a negative child row id"))?;
            // `forward` answers `None` for a child past the end of the link as well as for one with
            // no parent, which is the same answer for the same reason: section 3.1 says the answer
            // to a section that does not cover a row is no section, and no section says nothing
            // about that row. A child past the end can only be a row appended since the link was
            // built, and `Rows::stored` already refused a table that has any.
            local.rids.push(match self.link.forward(child) {
                Some(parent) => {
                    u32::try_from(parent).ok().filter(|&rid| rid != NO_ROW).ok_or_else(|| {
                        Error::internal("a link answered a parent row id a gather cannot hold")
                    })?
                }
                None => NO_ROW,
            });
        }
        Ok(())
    }

    /// Puts the gathered parent columns beside the child's.
    ///
    /// Nothing is read out of the parent here. What is built is one `Arc` per projected column over
    /// the ids that were just resolved, and the values are read by whichever operator above first
    /// asks for them, which for a column that is only grouped on is never.
    fn gather(&self, chunk: &mut Chunk, rids: &[u32]) -> Result<()> {
        if self.projected.is_empty() {
            return Ok(());
        }
        let rows = chunk.len();
        let ids = Arc::new(rids.to_vec());
        let mut columns: Vec<Vector> = chunk.columns().to_vec();
        for (column, ty) in &self.projected {
            let source = self.parent.column(*column, ty)?.ok_or_else(|| {
                Error::out_of_memory(
                    "a link join could not hold the parent columns it gathers from".to_string(),
                )
            })?;
            columns.push(Vector::gathered(source, Arc::clone(&ids))?);
        }
        *chunk = Chunk::with_rows(columns, rows)?;
        Ok(())
    }

    /// Reads the parent's projected columns, so that no instance is the one that pays for it.
    ///
    /// The same argument [`crate::join::Probe::prepare`] makes, with a smaller thing being read: an
    /// instance that found the column missing would read it behind a lock while the rest of the
    /// lease slept on it.
    ///
    /// A column that does not fit is an error here rather than a fall back, and this is the one
    /// place in the graph layer where that is the right answer. A hash join over the same parent
    /// materialises the same columns and then builds a table over them, so it costs strictly more
    /// than this does. A parent whose projection will not fit here is a parent whose hash join
    /// would not have fit either, and reporting it is what the hash join would have done.
    fn read_parent(&self) -> Result<()> {
        for (column, ty) in &self.projected {
            self.cancel.check()?;
            if self.parent.column(*column, ty)?.is_none() {
                return Err(Error::out_of_memory(
                    "a link join could not hold the parent columns it gathers from".to_string(),
                ));
            }
        }
        let footprint = u64::try_from(self.parent.footprint()).unwrap_or(u64::MAX);
        let mut held = self.held.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let already = held.bytes();
        if footprint > already {
            held.grow(footprint - already)?;
        }
        Ok(())
    }
}

impl Stream for LinkJoin {
    type Local = Linking;

    fn local(&self) -> Linking {
        Linking { scratch: self.rid.scratch(), rids: Vec::new(), gauge: Gauge::new(1) }
    }

    fn prepare(&self, _threads: &Lease<'_>) -> Result<()> {
        self.read_parent()
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Linking) -> Result<Progress> {
        self.cancel.check()?;
        let rows = chunk.len();
        if rows == 0 {
            // Of this operator's width and not of its input's, because an empty chunk still says
            // how many columns the operator above is about to be handed.
            *chunk = Chunk::empty(&self.schema.types());
            return Ok(Progress::More);
        }
        self.resolve(chunk, local)?;
        match self.kind {
            // Neither of these looks at the parent. The whole operator is the sentinel test, which
            // is why section 5.2 calls them nearly free.
            JoinKind::Semi | JoinKind::Anti => {
                let hit = self.kind == JoinKind::Semi;
                let kept =
                    Selection::from_predicate(rows, |row| (local.rids[row] != NO_ROW) == hit);
                if kept.len() != rows {
                    narrow(self.compaction, chunk, &kept, &mut local.gauge)?;
                }
                Ok(Progress::More)
            }
            JoinKind::Inner => {
                let kept = Selection::from_predicate(rows, |row| local.rids[row] != NO_ROW);
                if kept.len() != rows {
                    // The ids move with the rows, because what comes out of `narrow` is as long
                    // as the selection was and row `i` of it is row `kept[i]` of what went in.
                    local.rids = kept.iter().map(|row| local.rids[row]).collect();
                    narrow(self.compaction, chunk, &kept, &mut local.gauge)?;
                }
                let rids = std::mem::take(&mut local.rids);
                let result = self.gather(chunk, &rids);
                local.rids = rids;
                result?;
                Ok(Progress::More)
            }
            // The kind that does no selection at all. A child row with no parent keeps its place
            // and the sentinel in its id is what makes the gathered value null, so there is nothing
            // here to pad and nothing to drop.
            JoinKind::Left => {
                let rids = std::mem::take(&mut local.rids);
                let result = self.gather(chunk, &rids);
                local.rids = rids;
                result?;
                Ok(Progress::More)
            }
            JoinKind::Right
            | JoinKind::Full
            | JoinKind::Mark
            | JoinKind::Single
            | JoinKind::Positional => {
                Err(Error::internal("a link join was asked for a kind a link cannot answer"))
            }
        }
    }

    /// One pass over the rows, which is the claim section 5.2 makes.
    ///
    /// The link read is one bit packed load and one bounds check per row, which is about what a
    /// comparison against a literal costs, and the gather is not done here at all: it is a pointer
    /// per column per chunk and the reading of it is charged to whoever reads it.
    fn weight(&self) -> usize {
        1
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_catalog::{Parent, Rows};
    use rudb_common::{Cancel, Field, LogicalType, Memory, Session, Value};
    use rudb_graph::{Link, NO_PARENT};
    use rudb_pipeline::Stream;
    use rudb_plan::{ColumnBinding, Expr, JoinKind, Plan};
    use rudb_seam::Settings;
    use rudb_storage::MemoryTable;
    use rudb_vector::{Chunk, Vector};

    use super::LinkJoin;
    use crate::schema::Schema;

    /// A parent of `rows` rows whose one column is its own row number, so that a gathered value
    /// says which parent row it came from and a wrong link shows up as a wrong number.
    fn parent(rows: i32) -> Arc<Parent> {
        let mut table = MemoryTable::new(vec![LogicalType::Integer]);
        let held: Vec<Value> = (0..rows).map(Value::Integer).collect();
        let column = Vector::from_values(LogicalType::Integer, &held).expect("a column");
        table.append(Chunk::new(vec![column]).expect("a chunk")).expect("appended");
        Arc::new(Parent::new(Rows::Memory(table), 64 * 1024 * 1024))
    }

    /// The child's schema: one data column and the row id beside it, which is the shape section 5.1
    /// describes.
    fn child_schema() -> Schema {
        Schema::numbered(
            vec![
                Field::new("l_price", LogicalType::Integer),
                Field::new("file_row_number", LogicalType::BigInt),
            ],
            0,
        )
    }

    /// The parent's projected columns, which a semi or an anti join has none of.
    fn gathered_schema(parent_columns: bool) -> Schema {
        let fields = if parent_columns {
            vec![Field::new("o_key", LogicalType::Integer)]
        } else {
            Vec::new()
        };
        Schema::numbered(fields, 1)
    }

    /// A chunk of `parents.len()` child rows, numbered from zero, with a data column beside them.
    fn child(rows: usize) -> Chunk {
        let prices: Vec<Value> = (0..rows)
            .map(|row| Value::Integer(100 + i32::try_from(row).expect("a small row count")))
            .collect();
        let prices = Vector::from_values(LogicalType::Integer, &prices).expect("prices");
        let rids = Vector::sequence(0, 1, rows);
        Chunk::new(vec![prices, rids]).expect("a child chunk")
    }

    /// A link over `parents`, where an entry of `None` is the no parent sentinel.
    fn link(parents: &[Option<u64>]) -> Arc<Link> {
        let held: Vec<u64> = parents.iter().map(|parent| parent.unwrap_or(NO_PARENT)).collect();
        let highest = held.iter().filter(|&&p| p != NO_PARENT).max().copied().unwrap_or(0);
        Arc::new(Link::build(&held, highest + 1).expect("a link"))
    }

    /// Builds the operator for one kind over one link, gathering the parent's only column for the
    /// kinds that gather anything.
    fn operator(kind: JoinKind, parents: &[Option<u64>], rows: i32) -> LinkJoin {
        let mut plan = Plan::new();
        let rid = plan.add_expr(Expr::Column(ColumnBinding::new(0, 1)), LogicalType::BigInt);
        let gathers = !matches!(kind, JoinKind::Semi | JoinKind::Anti);
        let projected = if gathers { vec![(0, LogicalType::Integer)] } else { Vec::new() };
        LinkJoin::new(
            &plan,
            kind,
            link(parents),
            parent(rows),
            projected,
            rid,
            &child_schema(),
            &gathered_schema(gathers),
            &Settings::default(),
            &Memory::unlimited(),
            Cancel::new(),
        )
        .expect("the operator is buildable")
        .in_session(&Session::default())
    }

    /// Pushes one chunk through and hands back what came out, as values.
    fn run(operator: &LinkJoin, mut chunk: Chunk) -> Vec<Vec<Value>> {
        let mut local = operator.local();
        operator.push(&mut chunk, &mut local).expect("the push");
        let chunk = chunk.flatten().expect("flattened");
        (0..chunk.len()).map(|row| chunk.row(row).collect()).collect()
    }

    /// The thing the operator exists for. Every child row is answered by its parent's column,
    /// read through the link and never through a hash table.
    #[test]
    fn an_inner_link_join_puts_each_childs_parent_beside_it() {
        let operator = operator(JoinKind::Inner, &[Some(2), Some(0), Some(2), Some(1)], 3);
        let rows = run(&operator, child(4));
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(100), Value::BigInt(0), Value::Integer(2)],
                vec![Value::Integer(101), Value::BigInt(1), Value::Integer(0)],
                vec![Value::Integer(102), Value::BigInt(2), Value::Integer(2)],
                vec![Value::Integer(103), Value::BigInt(3), Value::Integer(1)],
            ]
        );
    }

    /// Inner drops the sentinel rows, and the ids move with the rows rather than staying where
    /// they were. A remap that forgot to move them would put the second surviving row's parent
    /// beside the first.
    #[test]
    fn an_inner_link_join_drops_the_children_with_no_parent() {
        let operator = operator(JoinKind::Inner, &[None, Some(1), None, Some(0)], 2);
        let rows = run(&operator, child(4));
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(101), Value::BigInt(1), Value::Integer(1)],
                vec![Value::Integer(103), Value::BigInt(3), Value::Integer(0)],
            ]
        );
    }

    /// Left keeps them and gathers null, and it does so without a mask and without padding: the
    /// sentinel in the id is what the null is.
    #[test]
    fn a_left_link_join_keeps_the_children_with_no_parent_and_gathers_null() {
        let operator = operator(JoinKind::Left, &[None, Some(1), None, Some(0)], 2);
        let rows = run(&operator, child(4));
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(100), Value::BigInt(0), Value::Null],
                vec![Value::Integer(101), Value::BigInt(1), Value::Integer(1)],
                vec![Value::Integer(102), Value::BigInt(2), Value::Null],
                vec![Value::Integer(103), Value::BigInt(3), Value::Integer(0)],
            ]
        );
    }

    /// A semi join is the sentinel test and nothing else. The parent is never read, which is what
    /// the empty projection here is asserting: an operator that needed the parent would not be
    /// buildable without one.
    #[test]
    fn a_semi_link_join_keeps_the_children_that_have_a_parent_and_reads_nothing() {
        let operator = operator(JoinKind::Semi, &[None, Some(1), None, Some(0)], 2);
        let rows = run(&operator, child(4));
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(101), Value::BigInt(1)],
                vec![Value::Integer(103), Value::BigInt(3)],
            ]
        );
    }

    /// And anti is the other half of the same test.
    #[test]
    fn an_anti_link_join_keeps_the_children_that_have_none() {
        let operator = operator(JoinKind::Anti, &[None, Some(1), None, Some(0)], 2);
        let rows = run(&operator, child(4));
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(100), Value::BigInt(0)],
                vec![Value::Integer(102), Value::BigInt(2)],
            ]
        );
    }

    /// A gather is a pointer and not a copy, which is the reason the body exists. Eight columns
    /// off one parent are one parent between them, and one column off it is not a second copy of
    /// the parent either.
    #[test]
    fn the_parent_is_pointed_at_rather_than_copied() {
        let operator = operator(JoinKind::Inner, &[Some(0); 8], 4);
        let mut chunk = child(8);
        let mut local = operator.local();
        operator.push(&mut chunk, &mut local).expect("the push");
        let gathered = chunk.column(2).expect("the parent column");
        let (source, rids) = gathered.gathered_parts().expect("it is a gather");
        assert_eq!(source.len(), 4, "the source is the whole parent column");
        assert_eq!(rids.len(), 8, "one id per child row");
        assert!(
            gathered.footprint() < source.footprint() + 8 * 4 + 64,
            "the parent was copied rather than pointed at"
        );
    }

    /// A row id the plan named that is not a row id is a plan that is wrong, and it is caught on
    /// the chunk rather than producing a plausible row.
    #[test]
    fn a_child_row_id_that_is_not_a_row_id_is_refused() {
        let operator = operator(JoinKind::Inner, &[Some(0), Some(0)], 2);
        let prices =
            Vector::from_values(LogicalType::Integer, &[Value::Integer(1), Value::Integer(2)])
                .expect("prices");
        let rids = Vector::from_values(LogicalType::BigInt, &[Value::BigInt(-1), Value::BigInt(0)])
            .expect("ids");
        let mut chunk = Chunk::new(vec![prices, rids]).expect("a chunk");
        let mut local = operator.local();
        let message = operator.push(&mut chunk, &mut local).unwrap_err().to_string();
        assert!(message.contains("negative child row id"), "unhelpful message: {message}");
    }

    /// A child past the end of the link reads as no parent rather than as an error, which is
    /// section 3.1's rule: a section that does not cover a row says nothing about that row.
    #[test]
    fn a_child_past_the_end_of_the_link_has_no_parent() {
        let operator = operator(JoinKind::Left, &[Some(0), Some(0)], 2);
        let rows = run(&operator, child(4));
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[2][2], Value::Null, "a child the link does not cover has no parent");
        assert_eq!(rows[3][2], Value::Null);
    }
}
