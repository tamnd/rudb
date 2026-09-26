//! The backward adjacency: for each parent row, the child rows that point at it.
//!
//! spec/graph/03-the-file-format.md section 3.5. The forward link answers which parent a child has,
//! and a reduction that starts from a set of parents and asks it has to ask every child. That is a
//! pass over the whole child table to find the few rows that point into the set, and on TPC-H q17
//! it is six million tests to find 6,088 lines of 204 parts. This answers the other way round: the
//! children of those 204 parts are read off directly, and the scan decodes those rows and no others.
//!
//! Compressed sparse row in the ordinary sense. The child rows are grouped by parent, ascending
//! within each parent, and bit-packed to the width the largest child row takes. Where each
//! parent's list starts is the same bit vector the monotone link uses: a run of ones per parent,
//! one per child, then a zero, so that the list of parent `p` is `[cum(p - 1), cum(p))` with
//! `cum(p) = select0(p) - p`. A parent with no children costs one bit.
//!
//! A relationship whose link is monotone needs none of this, because its children are already
//! grouped by parent in the table itself and the link answers both directions. So one is built only
//! for a link in the packed form, and a child with no parent is left out of every list.
//!
//! Section 3.5 also allows a delta form for lists whose average length is above about eight. That
//! is not here yet: the flat form is what the budget is measured against first, and the delta form
//! arrives when a measurement says the bytes matter more than the simplicity.

use rudb_common::{Error, Result};
use rudb_encoding::bitpack;

use crate::bits::BitVector;
use crate::rid::{NO_PARENT, Rid};
use crate::rids::Rids;
use crate::tail::Tail;

/// The payload layout version.
const LAYOUT: u8 = 1;

/// Bytes of fixed header at the front of a backward adjacency payload.
///
/// `children`, `parents`, `edges`, then the width, the layout and padding to an eight byte boundary.
pub const HEADER_BYTES: usize = 32;

/// A relationship's parent to children map.
#[derive(Debug, Clone)]
pub struct Adjacency {
    /// Rows in the child table, which is what the child rows are counted against.
    children: u64,
    parents: u64,
    /// Children that point at a parent, which is how many child rows the lists hold between them.
    edges: u64,
    /// One run of ones per parent, one per child, each followed by a zero.
    starts: BitVector,
    /// The child rows, grouped by parent, `width` bits each.
    rows: Tail,
    width: usize,
}

impl Adjacency {
    /// Builds the adjacency from one parent `rid` per child, with [`NO_PARENT`] for the unmatched.
    ///
    /// Two passes over the slice, one to count each parent's children and one to place them, which
    /// is a counting sort. The children arrive in row order, so each list comes out ascending.
    ///
    /// # Errors
    ///
    /// If a parent `rid` is not [`NO_PARENT`] and is not below `parents`, for the reason
    /// [`crate::Link::build`] refuses the same thing.
    pub fn build(parents_of: &[Rid], parents: u64) -> Result<Self> {
        let parent_rows = usize::try_from(parents)
            .map_err(|_| malformed("a parent table larger than fits in memory"))?;
        let mut starts = vec![0_usize; parent_rows + 1];
        for &parent in parents_of {
            if parent == NO_PARENT {
                continue;
            }
            if parent >= parents {
                return Err(malformed(format!(
                    "a child points at parent {parent} of a table with {parents} rows"
                )));
            }
            starts[parent as usize + 1] += 1;
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        let edges = starts[parent_rows];
        let mut placed = starts.clone();
        let mut grouped = vec![0_u64; edges];
        for (child, &parent) in parents_of.iter().enumerate() {
            if parent == NO_PARENT {
                continue;
            }
            let slot = &mut placed[parent as usize];
            grouped[*slot] = count(child);
            *slot += 1;
        }
        let len = edges + parent_rows;
        let mut words = vec![0_u64; len.div_ceil(64)];
        for parent in 0..parent_rows {
            // Parent `p`'s ones start after its predecessors' children and their `p` zeros.
            for at in starts[parent] + parent..starts[parent + 1] + parent {
                words[at / 64] |= 1 << (at % 64);
            }
        }
        let children = count(parents_of.len());
        let width = width_for(children);
        let mut rows = Vec::with_capacity(bitpack::tail_len(edges, width));
        bitpack::pack_linear(&grouped, width, &mut rows)?;
        Ok(Self {
            children,
            parents,
            edges: count(edges),
            starts: BitVector::new(words, len)?,
            rows: rows.into(),
            width,
        })
    }

    /// Rows in the child table.
    #[must_use]
    pub fn children(&self) -> u64 {
        self.children
    }

    /// Rows in the parent table.
    #[must_use]
    pub fn parents(&self) -> u64 {
        self.parents
    }

    /// Children that point at a parent.
    #[must_use]
    pub fn edges(&self) -> u64 {
        self.edges
    }

    /// Bytes the body costs, not counting the header.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.starts.bytes() + self.rows.len()
    }

    /// Where the list of `parent` lies among the grouped child rows, or `None` past the end.
    fn list(&self, parent: Rid) -> Option<std::ops::Range<usize>> {
        if parent >= self.parents {
            return None;
        }
        let cum = |nth: Rid| -> Option<usize> {
            self.starts.select0(nth).map(|at| at - usize::try_from(nth).unwrap_or(usize::MAX))
        };
        let from = if parent == 0 { 0 } else { cum(parent - 1)? };
        Some(from..cum(parent)?)
    }

    /// The child rows that point at `parent`, ascending, appended to `out`.
    ///
    /// # Errors
    ///
    /// If `parent` is past the end, or the packed rows are shorter than the lists say.
    pub fn children_of(&self, parent: Rid, out: &mut Vec<Rid>) -> Result<()> {
        let list = self.list(parent).ok_or_else(|| {
            malformed(format!("parent {parent} of {} is past the end", self.parents))
        })?;
        for at in list {
            out.push(bitpack::tail_at(&self.rows, self.width, at)?);
        }
        Ok(())
    }

    /// How many children the parents of `held` have between them, without reading a child row.
    ///
    /// Two selects a member, for a caller deciding whether [`Self::push`] is worth its gathers.
    #[must_use]
    pub fn reached(&self, held: &Rids) -> u64 {
        held.iter().filter_map(|parent| self.list(parent)).map(|list| count(list.len())).sum()
    }

    /// The child rows that point into `held`, which is a set over the parent table.
    ///
    /// The lists of the members, set as bits of one bitmap over the children, which the set then
    /// settles into whichever form its size calls for. A bit a child costs no comparison, where
    /// sorting the lists together cost a log a child, and the result is the exact set a forward
    /// push of `held` through the link would give.
    ///
    /// # Errors
    ///
    /// If `held` is not a set over the parent table.
    pub fn push(&self, held: &Rids) -> Result<Rids> {
        if held.rows() != self.parents {
            return Err(Error::internal(format!(
                "a set over {} rows pushed through an adjacency over {} parents",
                held.rows(),
                self.parents
            )));
        }
        let mut words = vec![0_u64; usize::try_from(self.children.div_ceil(64)).unwrap_or(0)];
        for parent in held.iter() {
            let list = self.list(parent).ok_or_else(|| {
                malformed(format!("parent {parent} of {} is past the end", self.parents))
            })?;
            for at in list {
                let child = bitpack::tail_at(&self.rows, self.width, at)?;
                let word = words
                    .get_mut(usize::try_from(child / 64).unwrap_or(usize::MAX))
                    .ok_or_else(|| malformed(format!("child {child} past the end")))?;
                *word |= 1 << (child % 64);
            }
        }
        Rids::from_words(self.children, words)
    }

    /// Appends the header and the body.
    ///
    /// # Errors
    ///
    /// If the width does not fit the byte the layout gives it, which no width a `u64` takes can.
    pub fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&self.children.to_le_bytes());
        out.extend_from_slice(&self.parents.to_le_bytes());
        out.extend_from_slice(&self.edges.to_le_bytes());
        out.push(u8::try_from(self.width).map_err(|_| malformed("a width past a byte"))?);
        out.push(LAYOUT);
        out.extend_from_slice(&[0; 6]);
        self.starts.write(out);
        out.extend_from_slice(&self.rows);
        Ok(())
    }

    /// Reads an adjacency from exactly the bytes [`Adjacency::write`] produced.
    ///
    /// # Errors
    ///
    /// If the payload is shorter than its header, names a layout this build does not know, or holds
    /// a body that is not the size its header implies.
    pub fn read(bytes: &[u8]) -> Result<Self> {
        Self::read_from(bytes.to_vec(), 0)
    }

    /// [`Adjacency::read`] of the bytes of `payload` from `at` on, keeping `payload` for the child
    /// rows rather than copying them out of it. See [`Tail`] for why.
    ///
    /// # Errors
    ///
    /// As [`Adjacency::read`], or if `at` is past the end of `payload`.
    pub fn read_from(payload: Vec<u8>, at: usize) -> Result<Self> {
        let bytes =
            payload.get(at..).ok_or_else(|| malformed("a payload shorter than its header"))?;
        if bytes.len() < HEADER_BYTES {
            return Err(malformed("a payload shorter than its header"));
        }
        let children = number(&bytes[0..8])?;
        let parents = number(&bytes[8..16])?;
        let edges = number(&bytes[16..24])?;
        let width = bytes[24] as usize;
        if bytes[25] != LAYOUT {
            return Err(malformed(format!("layout {} is not one this build knows", bytes[25])));
        }
        if width != width_for(children) || edges > children {
            return Err(malformed("a header whose numbers do not agree"));
        }
        let len = usize::try_from(edges + parents)
            .map_err(|_| malformed("a list longer than fits in memory"))?;
        let edge_count =
            usize::try_from(edges).map_err(|_| malformed("more edges than fit in memory"))?;
        let rest = &bytes[HEADER_BYTES..];
        let split = BitVector::bytes_for(len);
        if rest.len() != split + bitpack::tail_len(edge_count, width) {
            return Err(malformed("a body that is not the size its header implies"));
        }
        let starts = BitVector::read(&rest[..split], len)?;
        if starts.ones() != edges {
            return Err(malformed("lists that do not hold the edges the header counts"));
        }
        let rows = Tail::of(payload, at + HEADER_BYTES + split)
            .ok_or_else(|| malformed("a body that is not the size its header implies"))?;
        Ok(Self { children, parents, edges, starts, rows, width })
    }
}

/// Bits a child row takes: enough for the last row of the child table.
fn width_for(children: u64) -> usize {
    (u64::BITS - children.saturating_sub(1).leading_zeros()).max(1) as usize
}

fn count(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

fn number(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| malformed("a header is torn"))?))
}

fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb backward adjacency: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::Link;

    /// A child table in no order against its parents, with a parent that has no children and a
    /// child that has no parent.
    fn scattered() -> (Vec<Rid>, u64) {
        let parents = 50;
        let parents_of = (0..3_000_u64)
            .map(|child| if child % 97 == 0 { NO_PARENT } else { (child * 7919 + 13) % 49 })
            .collect();
        (parents_of, parents)
    }

    #[test]
    fn each_parent_lists_exactly_the_children_that_point_at_it_in_row_order() {
        let (parents_of, parents) = scattered();
        let adjacency = Adjacency::build(&parents_of, parents).expect("build");
        for parent in 0..parents {
            let mut listed = Vec::new();
            adjacency.children_of(parent, &mut listed).expect("list");
            let expected: Vec<Rid> = (0..count(parents_of.len()))
                .filter(|&child| parents_of[child as usize] == parent)
                .collect();
            assert_eq!(listed, expected, "parent {parent}");
        }
        assert!(adjacency.children_of(parents, &mut Vec::new()).is_err(), "past the end");
    }

    #[test]
    fn a_push_is_the_set_a_forward_push_through_the_link_gives() {
        let (parents_of, parents) = scattered();
        let adjacency = Adjacency::build(&parents_of, parents).expect("build");
        let link = Link::build(&parents_of, parents).expect("link");
        let held = Rids::from_sorted(parents, vec![0, 3, 17, 48, 49]).expect("held");
        let pushed = adjacency.push(&held).expect("push");
        let forward = held.forward(&link).expect("forward").rids;
        assert_eq!(pushed.iter().collect::<Vec<_>>(), forward.iter().collect::<Vec<_>>());
        assert_eq!(adjacency.reached(&held), pushed.len(), "counted without reading a row");
        let wrong = Rids::from_sorted(parents + 1, vec![0]).expect("wrong");
        assert!(adjacency.push(&wrong).is_err(), "a set over another table");
    }

    #[test]
    fn it_reads_back_what_it_wrote_and_refuses_a_torn_body() {
        let (parents_of, parents) = scattered();
        let adjacency = Adjacency::build(&parents_of, parents).expect("build");
        let mut bytes = Vec::new();
        adjacency.write(&mut bytes).expect("write");
        assert_eq!(bytes.len(), HEADER_BYTES + adjacency.bytes());
        let read = Adjacency::read(&bytes).expect("read");
        assert_eq!(read.edges(), adjacency.edges());
        let held = Rids::from_sorted(parents, vec![5, 6, 7]).expect("held");
        assert_eq!(read.push(&held).expect("push"), adjacency.push(&held).expect("push"));
        assert!(Adjacency::read(&bytes[..bytes.len() - 1]).is_err(), "a short body");
        bytes[25] = 9;
        assert!(Adjacency::read(&bytes).is_err(), "a layout from elsewhere");
    }
}
