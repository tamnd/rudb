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
use crate::rids::{Rids, ones_from, past_zeros};
use crate::tail::Tail;

/// The payload layout version.
const LAYOUT: u8 = 1;

/// A member more than this many parents past the last one is found with a select rather than by
/// reading on through the starts to it.
///
/// Reading on costs a word of the starts for every sixty four bits passed, and each parent passed
/// is a zero and its children ones. On the IMDb load's cast_info, about fourteen children a movie,
/// a thousand parents is a few hundred words, which is about what one select costs.
const FAR: u64 = 1024;

/// Child rows a bucket of [`Adjacency::push`] covers, which is eight kilobytes of its bitmap.
///
/// A row within a bucket fits a `u16`, so this is at most its range.
const BUCKET_ROWS: usize = 1 << 16;

/// How many lists [`Adjacency::push`] has asked memory for before it reads the first of them.
///
/// Enough to cover a trip to memory with the work of reading the lists in between, which is a few
/// dozen nanoseconds a list.
const AHEAD: usize = 16;

/// The fewest children [`Adjacency::push_spans`] hands a thread of its own, so that what a thread
/// costs to start, tens of microseconds, is a small part of what it does.
const SHARED_ROWS: u64 = 1 << 16;

/// How many bits of a parent `rid` [`Adjacency::build`] deals the children by, which is about a
/// thousand ranges, each sorted on its own with eight kilobytes of counters on the IMDb `name`.
const RANGE_BITS: u32 = 10;

/// Bytes of fixed header at the front of a backward adjacency payload.
///
/// `children`, `parents`, `edges`, then the width, the layout and padding to an eight byte boundary.
pub const HEADER_BYTES: usize = 32;

/// Where the lists of some parents lie among an adjacency's child rows, which [`Adjacency::spans`]
/// finds and [`Adjacency::push_spans`] reads.
#[derive(Debug, Clone)]
pub struct Spans {
    /// The lists that hold a child, in rising order of parent.
    lists: Vec<std::ops::Range<usize>>,
    /// The child rows the lists hold between them.
    rows: u64,
    /// Rows in the child table of the adjacency the lists are in.
    children: u64,
}

impl Spans {
    /// How many child rows the lists hold, which is what [`Adjacency::reached`] counts.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }
}

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
    /// A counting sort, a radix at a time. Counting every parent's children and then placing each
    /// child at its parent's slot is two passes of writes to wherever the parent's counter lands,
    /// and on the IMDb `cast_info`, 36 million children against 4 million people, every one of
    /// them was a trip to memory: the adjacencies were 15 of the 91 seconds of CPU the checkpoint
    /// took. So the children are first dealt into ranges of parents, about a thousand of them,
    /// each of which is written in order, and then each range is sorted by parent on its own,
    /// where its counters and its children both fit in cache. Dealing keeps row order within a
    /// range, so each list still comes out ascending, and the ranges are dealt into the slice the
    /// lists end up in, so the build holds no more than the direct sort did.
    ///
    /// # Errors
    ///
    /// If a parent `rid` is not [`NO_PARENT`] and is not below `parents`, for the reason
    /// [`crate::Link::build`] refuses the same thing.
    pub fn build(parents_of: &[Rid], parents: u64) -> Result<Self> {
        let parent_rows = usize::try_from(parents)
            .map_err(|_| malformed("a parent table larger than fits in memory"))?;
        let children = count(parents_of.len());
        let width = width_for(children);
        // A child is dealt with its parent's place within its range above its own row, so the two
        // have to fit a word between them, which they do below four trillion child rows.
        let shift = (usize::BITS - parent_rows.leading_zeros()).saturating_sub(RANGE_BITS);
        if width + shift as usize >= 64 {
            return Self::build_direct(parents_of, parents);
        }
        let ranges = (parent_rows >> shift) + 1;
        let mut sizes = vec![0_usize; ranges + 1];
        for &parent in parents_of {
            if parent == NO_PARENT {
                continue;
            }
            if parent >= parents {
                return Err(malformed(format!(
                    "a child points at parent {parent} of a table with {parents} rows"
                )));
            }
            sizes[(parent >> shift) as usize + 1] += 1;
        }
        for at in 1..sizes.len() {
            sizes[at] += sizes[at - 1];
        }
        let edges = sizes[ranges];
        let within = (1_u64 << shift) - 1;
        let mut grouped = vec![0_u64; edges];
        let mut dealt = sizes[..ranges].to_vec();
        for (child, &parent) in parents_of.iter().enumerate() {
            if parent == NO_PARENT {
                continue;
            }
            let slot = &mut dealt[(parent >> shift) as usize];
            grouped[*slot] = (parent & within) << width | count(child);
            *slot += 1;
        }
        drop(dealt);
        let len = edges + parent_rows;
        let mut words = vec![0_u64; len.div_ceil(64)];
        let row = u64::MAX >> (64 - width);
        let mut held = Vec::new();
        let mut starts = vec![0_usize; (1 << shift) + 1];
        for range in 0..ranges {
            let (low, high) = (sizes[range], sizes[range + 1]);
            let first = range << shift;
            let span = (1_usize << shift).min(parent_rows.saturating_sub(first));
            let starts = &mut starts[..=span];
            starts.fill(0);
            held.clear();
            held.extend_from_slice(&grouped[low..high]);
            for &pair in &held {
                starts[(pair >> width) as usize + 1] += 1;
            }
            for at in 1..starts.len() {
                starts[at] += starts[at - 1];
            }
            for (at, pair) in starts.windows(2).enumerate() {
                // Parent `p`'s ones start after its predecessors' children and their `p` zeros.
                let parent = first + at;
                for one in low + pair[0] + parent..low + pair[1] + parent {
                    words[one / 64] |= 1 << (one % 64);
                }
            }
            for &pair in &held {
                let slot = &mut starts[(pair >> width) as usize];
                grouped[low + *slot] = pair & row;
                *slot += 1;
            }
        }
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

    /// The same as [`Self::build`], one counter per parent and no ranges, for a child table too
    /// large to deal a row and a place in a word.
    fn build_direct(parents_of: &[Rid], parents: u64) -> Result<Self> {
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

    /// How many child rows point at each parent, in parent order, saturating at `u32::MAX`.
    ///
    /// One walk over the starts, where each parent is a run of ones followed by a zero, so a word
    /// answers for every parent that ends in it with a `trailing_ones` each. This is what
    /// [`crate::Degrees::with_counts`] wants, and it is a few milliseconds for the four million
    /// people of the IMDb `cast_info` where counting the same thing from the children was a
    /// scattered increment for each of 36 million of them.
    #[must_use]
    pub fn degrees(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(usize::try_from(self.parents).unwrap_or(0));
        let len = self.starts.len();
        let mut run = 0_usize;
        for (index, &word) in self.starts.words().iter().enumerate() {
            let mut word = word;
            let mut left = len.saturating_sub(index * 64).min(64);
            while left > 0 {
                let ones = (word.trailing_ones() as usize).min(left);
                run += ones;
                if ones == left {
                    break;
                }
                out.push(u32::try_from(run).unwrap_or(u32::MAX));
                run = 0;
                word = word.checked_shr(u32::try_from(ones + 1).unwrap_or(64)).unwrap_or(0);
                left -= ones + 1;
            }
        }
        out
    }

    /// How many child rows point at `parent`, read off the starts without reading any of them.
    ///
    /// # Errors
    ///
    /// If `parent` is past the end.
    pub fn degree(&self, parent: Rid) -> Result<u64> {
        let list = self.list(parent).ok_or_else(|| {
            malformed(format!("parent {parent} of {} is past the end", self.parents))
        })?;
        Ok(count(list.len()))
    }

    /// How many children the parents of `held` have between them, without reading a child row.
    ///
    /// A walk over the starts, for a caller deciding whether [`Self::push`] is worth its gathers.
    ///
    /// # Errors
    ///
    /// If `held` is not a set over the parent table.
    pub fn reached(&self, held: &Rids) -> Result<u64> {
        let mut reached = 0;
        self.lists(held, |list| {
            reached += count(list.len());
            Ok(())
        })?;
        Ok(reached)
    }

    /// Where the lists of the parents of `held` lie, and how many child rows they hold, from one
    /// walk over the starts, or `None` once they hold more than `most`.
    ///
    /// For a caller that counts the rows before it decides to read them, and then reads them with
    /// [`Self::push_spans`]. Counting with [`Self::reached`] and reading with [`Self::push`] walked
    /// the starts twice, and the walk is most of what a count costs. On JOB 13a at one thread the
    /// walk was 6.6 percent of the query and the iteration over the held parents feeding it
    /// another 3.6.
    ///
    /// `most` is where the caller would turn the rows down. The walk stops there, because a set
    /// that is turned down needs no more of a count, and a range kept for each list of a set of
    /// millions of parents was a write of tens of megabytes for nothing: JOB 6f was a fifth slower
    /// for it.
    ///
    /// # Errors
    ///
    /// If `held` is not a set over the parent table.
    pub fn spans(&self, held: &Rids, most: u64) -> Result<Option<Spans>> {
        let mut spans = Spans { lists: Vec::new(), rows: 0, children: self.children };
        let mut over = false;
        let walked = self.lists(held, |list| {
            spans.rows += count(list.len());
            if spans.rows > most {
                // Only to stop the walk, and dropped below.
                over = true;
                return Err(Error::internal("past the rows a caller reads"));
            }
            if !list.is_empty() {
                spans.lists.push(list);
            }
            Ok(())
        });
        if over {
            return Ok(None);
        }
        walked?;
        Ok(Some(spans))
    }

    /// The child rows that point into `held`, which is a set over the parent table.
    ///
    /// The lists of the members, set as bits of one bitmap over the children, which the set then
    /// settles into whichever form its size calls for. A bit a child costs no comparison, where
    /// sorting the lists together cost a log a child, and the result is the exact set a forward
    /// push of `held` through the link would give.
    ///
    /// The bits are not set as the lists are read. A child table in another order than its parent
    /// puts each child of a list anywhere in a bitmap of megabytes, and setting its bit there is a
    /// trip to memory. So the children are first dealt into one bucket per `BUCKET_ROWS` of
    /// them, which writes to the ends of a few hundred lists that stay in the cache, and then each
    /// bucket sets its bits in a stretch of the bitmap small enough to stay in the cache while it
    /// does.
    ///
    /// # Errors
    ///
    /// If `held` is not a set over the parent table.
    pub fn push(&self, held: &Rids) -> Result<Rids> {
        self.dealt(|deal| {
            // A list is asked for from memory when the walk reaches it and read [`AHEAD`] lists
            // later. The lists of a sparse set of parents are a cache line or two each, megabytes
            // apart, and read where the walk found them every one was a wait on memory. On JOB 17f
            // that was most of the push, which was a sixth of the query. Which order the lists are
            // dealt in does not matter, because each child is a bit.
            let mut waiting: [std::ops::Range<usize>; AHEAD] = std::array::from_fn(|_| 0..0);
            let mut next = 0;
            self.lists(held, |list| {
                self.ask(&list);
                let due = std::mem::replace(&mut waiting[next], list);
                next = (next + 1) % AHEAD;
                deal(due)
            })?;
            for list in waiting {
                deal(list)?;
            }
            Ok(())
        })
    }

    /// [`Self::push`] of the parents [`Self::spans`] found the lists of, on up to `threads`
    /// threads.
    ///
    /// The push runs after the relation before has finished and before any worker of the scan
    /// starts, on the query's thread, so it is time every worker waits for. On JOB 13a at one
    /// thread it was a third of the query. With [`SHARED_ROWS`] children or more for each, the
    /// lists are cut into a share a thread, each thread deals its share into buckets of its own,
    /// and then each thread sets the bits of a stretch of buckets from all of them, which is the
    /// same two steps the push takes on one thread and touches no bit two threads could both set.
    ///
    /// # Errors
    ///
    /// If `spans` was found over an adjacency with another number of children.
    pub fn push_spans(&self, spans: &Spans, threads: usize) -> Result<Rids> {
        if spans.children != self.children {
            return Err(Error::internal(format!(
                "lists over {} children pushed through an adjacency over {}",
                spans.children, self.children
            )));
        }
        let lists = spans.lists.as_slice();
        let shares = usize::try_from(spans.rows / SHARED_ROWS)
            .unwrap_or(usize::MAX)
            .clamp(1, threads.max(1));
        if shares == 1 {
            return self.dealt(|deal| self.read_lists(lists, deal));
        }
        let per = spans.rows.div_ceil(count(shares));
        let mut groups: Vec<&[std::ops::Range<usize>]> = Vec::with_capacity(shares);
        let (mut start, mut held) = (0, 0);
        for (at, list) in lists.iter().enumerate() {
            held += count(list.len());
            if held >= per && groups.len() + 1 < shares {
                groups.push(&lists[start..=at]);
                (start, held) = (at + 1, 0);
            }
        }
        groups.push(&lists[start..]);
        let dealt = std::thread::scope(|scope| {
            let handles: Vec<_> = groups
                .iter()
                .map(|&group| {
                    scope.spawn(move || self.buckets(|deal| self.read_lists(group, deal)))
                })
                .collect();
            handles.into_iter().map(joined).collect::<Result<Vec<_>>>()
        })?;
        let mut words = vec![0_u64; self.words()];
        let stretch = words.len().div_ceil(BUCKET_ROWS / 64).div_ceil(shares) * (BUCKET_ROWS / 64);
        std::thread::scope(|scope| {
            let dealt = &dealt;
            let handles: Vec<_> = words
                .chunks_mut(stretch.max(1))
                .enumerate()
                .map(|(at, words)| {
                    let first = at * stretch / (BUCKET_ROWS / 64);
                    scope.spawn(move || set_bits(words, first, dealt))
                })
                .collect();
            handles.into_iter().map(joined).collect::<Result<Vec<()>>>()
        })?;
        Rids::from_words(self.children, words)
    }

    /// Hands `deal` each of `lists`, asking memory for each [`AHEAD`] lists before it is read.
    fn read_lists(
        &self,
        lists: &[std::ops::Range<usize>],
        deal: &mut dyn FnMut(std::ops::Range<usize>) -> Result<()>,
    ) -> Result<()> {
        for list in lists.iter().take(AHEAD) {
            self.ask(list);
        }
        for (at, list) in lists.iter().enumerate() {
            if let Some(ahead) = lists.get(at + AHEAD) {
                self.ask(ahead);
            }
            deal(list.clone())?;
        }
        Ok(())
    }

    /// The child rows of the lists `walk` hands its dealer, as a set over the children.
    fn dealt(
        &self,
        walk: impl FnOnce(&mut dyn FnMut(std::ops::Range<usize>) -> Result<()>) -> Result<()>,
    ) -> Result<Rids> {
        let buckets = self.buckets(walk)?;
        let mut words = vec![0_u64; self.words()];
        set_bits(&mut words, 0, std::slice::from_ref(&buckets))?;
        Rids::from_words(self.children, words)
    }

    /// The child rows of the lists `walk` hands its dealer, dealt into one bucket per
    /// [`BUCKET_ROWS`] of them as their place in the bucket.
    fn buckets(
        &self,
        walk: impl FnOnce(&mut dyn FnMut(std::ops::Range<usize>) -> Result<()>) -> Result<()>,
    ) -> Result<Vec<Vec<u16>>> {
        let mut buckets: Vec<Vec<u16>> = vec![Vec::new(); self.words().div_ceil(BUCKET_ROWS / 64)];
        let mut deal = |list: std::ops::Range<usize>| -> Result<()> {
            for at in list {
                let child = bitpack::tail_at(&self.rows, self.width, at)?;
                let bucket = buckets
                    .get_mut(usize::try_from(child).unwrap_or(usize::MAX) / BUCKET_ROWS)
                    .ok_or_else(|| malformed(format!("child {child} past the end")))?;
                #[expect(clippy::cast_possible_truncation, reason = "the row within its bucket")]
                bucket.push((child % BUCKET_ROWS as u64) as u16);
            }
            Ok(())
        };
        walk(&mut deal)?;
        Ok(buckets)
    }

    /// Words in a bitmap over the children.
    fn words(&self) -> usize {
        usize::try_from(self.children.div_ceil(64)).unwrap_or(0)
    }

    /// Asks for the cache lines the packed rows of `list` start and end in, ahead of reading them.
    ///
    /// Most lists are one or two lines. One longer than that is read in order, and the hardware
    /// sees that and fetches what is between for itself.
    fn ask(&self, list: &std::ops::Range<usize>) {
        let rows: &[u8] = &self.rows;
        let first = list.start.saturating_mul(self.width) / 8;
        let end = list.end.saturating_mul(self.width).div_ceil(8).min(rows.len());
        if first < end {
            prefetch(&rows[first]);
            prefetch(&rows[end - 1]);
        }
    }

    /// Hands `each` the list of every member of `held`, in rising order of parent.
    ///
    /// The members come in order, so the next list starts a few zeros on from where the last one
    /// ended, and the walk reads on through the starts to it a word at a time. Two selects a member
    /// was a binary search and a scan each. With 36 million children in no order against 2.5
    /// million parents, the shape of cast_info against title, and one parent in a hundred held,
    /// the selects and the scattered bits made the push four times slower than this walk and the
    /// buckets. A member far past the last one is still found with a select, because reading on to
    /// it would be a pass over the words between.
    fn lists(
        &self,
        held: &Rids,
        mut each: impl FnMut(std::ops::Range<usize>) -> Result<()>,
    ) -> Result<()> {
        if held.rows() != self.parents {
            return Err(Error::internal(format!(
                "a set over {} rows pushed through an adjacency over {} parents",
                held.rows(),
                self.parents
            )));
        }
        let bits = self.starts.words();
        let len = self.starts.len();
        // `at` is where the list of parent `parent` starts among the bits, which is past `parent`
        // zeros, so the children listed before it are `at - parent`.
        let (mut parent, mut at) = (0_u64, 0_usize);
        held.try_for_each(|member| {
            if member > parent {
                let found = if member - parent > FAR {
                    self.starts.select0(member - 1).map(|zero| zero + 1)
                } else {
                    past_zeros(bits, at, member - parent)
                };
                at = found.ok_or_else(|| {
                    malformed(format!("parent {member} of {} is past the end", self.parents))
                })?;
                parent = member;
            }
            let run = ones_from(bits, at, len);
            let from = at - usize::try_from(parent).unwrap_or(usize::MAX);
            each(from..from + run)?;
            // The zero after the list, which moves on to the next parent.
            at += run + 1;
            parent += 1;
            Ok(())
        })
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
    /// rows rather than copying them out of it. See `Tail` for why.
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

/// Asks for the cache line that holds `byte`, without waiting for it.
#[inline]
fn prefetch(byte: &u8) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch is a hint. It reads nothing into the program and does not fault whatever
    // the address, and this one is of a byte the caller holds a reference to anyway.
    unsafe {
        std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(
            std::ptr::from_ref(byte).cast::<i8>(),
        );
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = byte;
}
fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb backward adjacency: {}", message.into()))
}

/// Sets in `words`, which begins at bucket `first`, the bits of the rows each of `dealt` holds in
/// the buckets it covers.
fn set_bits(words: &mut [u64], first: usize, dealt: &[Vec<Vec<u16>>]) -> Result<()> {
    for (at, words) in words.chunks_mut(BUCKET_ROWS / 64).enumerate() {
        for buckets in dealt {
            let Some(bucket) = buckets.get(first + at) else { continue };
            for &row in bucket {
                let row = usize::from(row);
                *words.get_mut(row / 64).ok_or_else(|| malformed("a child past the end"))? |=
                    1 << (row % 64);
            }
        }
    }
    Ok(())
}

/// What a thread of [`Adjacency::push_spans`] came to, or an error if it panicked.
fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, Result<T>>) -> Result<T> {
    handle.join().map_err(|_| Error::internal("a thread of a push panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Degrees;
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
    fn dealing_by_ranges_builds_the_bytes_the_direct_sort_builds() {
        let (scattered, fifty) = scattered();
        let mut seed = 5_u64;
        let mut random = |parents: u64, rows: usize| {
            (0..rows)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    if seed >> 60 == 0 { NO_PARENT } else { (seed >> 20) % parents }
                })
                .collect::<Vec<Rid>>()
        };
        let cases = vec![
            (scattered, fifty),
            (random(1, 500), 1),
            (random(1023, 5_000), 1024),
            (random(1024, 5_000), 1024),
            (random(4096, 70_000), 4096),
            (random(100_003, 300_000), 100_003),
            ((0..20_000).map(|child| child / 3).collect(), 7_000),
            (vec![NO_PARENT; 100], 0),
            (vec![NO_PARENT; 100], 10),
            (Vec::new(), 10),
        ];
        for (parents_of, parents) in cases {
            let dealt = Adjacency::build(&parents_of, parents).expect("dealt");
            let direct = Adjacency::build_direct(&parents_of, parents).expect("direct");
            let (mut left, mut right) = (Vec::new(), Vec::new());
            dealt.write(&mut left).expect("write");
            direct.write(&mut right).expect("write");
            assert_eq!(left, right, "{} children of {parents} parents", parents_of.len());
        }
        let mut past = random(1000, 2000);
        past[1500] = 1000;
        assert!(Adjacency::build(&past, 1000).is_err(), "a parent past the end");
    }

    #[test]
    fn the_degrees_read_off_the_lists_measure_what_counting_the_children_measures() {
        let (scattered, fifty) = scattered();
        let mut seed = 11_u64;
        let mut random = |parents: u64, rows: usize| {
            (0..rows)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    if seed >> 60 == 0 { NO_PARENT } else { (seed >> 20) % parents }
                })
                .collect::<Vec<Rid>>()
        };
        let cases = vec![
            (scattered, fifty),
            (random(1, 500), 1),
            (random(1024, 5_000), 1024),
            (random(100_003, 300_000), 100_003),
            ((0..20_000).map(|child| child / 200).collect(), 130),
            ((0..20_000).map(|child| u64::from(child % 7 == 0) * 3).collect(), 5),
            (vec![NO_PARENT; 100], 10),
            (Vec::new(), 10),
        ];
        for (parents_of, parents) in cases {
            let adjacency = Adjacency::build(&parents_of, parents).expect("build");
            let degrees = adjacency.degrees();
            assert_eq!(degrees.len() as u64, parents, "one degree a parent");
            for (parent, degree) in degrees.iter().enumerate() {
                let listed = adjacency.degree(parent as u64).expect("degree");
                assert_eq!(u64::from(*degree), listed, "parent {parent}");
            }
            assert_eq!(
                Degrees::with_counts(&parents_of, &degrees, true),
                Degrees::of(&parents_of, parents, true),
                "{} children of {parents} parents",
                parents_of.len()
            );
        }
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
        assert_eq!(
            adjacency.reached(&held).expect("reached"),
            pushed.len(),
            "counted without reading a row"
        );
        let wrong = Rids::from_sorted(parents + 1, vec![0]).expect("wrong");
        assert!(adjacency.push(&wrong).is_err(), "a set over another table");
    }

    #[test]
    fn a_push_finds_near_and_far_members_alike() {
        // Enough parents that the gaps between members are both under and over FAR, and parents
        // with no children among them, so that both ways of finding a list are taken.
        let parents = 20 * FAR;
        let parents_of: Vec<Rid> = (0..60_000_u64)
            .map(
                |child| {
                    if child % 31 == 0 { NO_PARENT } else { (child * 7919 + 5) % (parents - 7) }
                },
            )
            .collect();
        let adjacency = Adjacency::build(&parents_of, parents).expect("build");
        let members: Vec<Rid> =
            [0, 1, 2, FAR, FAR + 1, 3 * FAR + 5, 3 * FAR + 900, 19 * FAR, parents - 1]
                .into_iter()
                .collect();
        let held = Rids::from_sorted(parents, members.clone()).expect("held");
        let mut expected = Vec::new();
        for &member in &members {
            adjacency.children_of(member, &mut expected).expect("list");
        }
        expected.sort_unstable();
        let pushed = adjacency.push(&held).expect("push");
        assert_eq!(pushed.iter().collect::<Vec<_>>(), expected);
        assert_eq!(adjacency.reached(&held).expect("reached"), count(expected.len()));
        let rows = count(expected.len());
        let spans = adjacency.spans(&held, rows).expect("spans").expect("not past the most");
        assert_eq!(spans.rows(), rows);
        let read = adjacency.push_spans(&spans, 1).expect("push the spans");
        assert_eq!(read.iter().collect::<Vec<_>>(), expected, "the same rows as the push");
        if rows > 0 {
            assert!(adjacency.spans(&held, rows - 1).expect("spans").is_none(), "one past it");
        }
    }

    #[test]
    fn a_push_of_more_members_than_it_reads_ahead_misses_none_of_them() {
        // Every third parent, so the lists asked for ahead fill the window many times over, and a
        // count that is not a multiple of the window, so the drain at the end has some left.
        let (parents_of, parents) = scattered();
        let adjacency = Adjacency::build(&parents_of, parents).expect("build");
        let members: Vec<Rid> = (0..parents).step_by(3).collect();
        assert!(members.len() > AHEAD && !members.len().is_multiple_of(AHEAD));
        let held = Rids::from_sorted(parents, members.clone()).expect("held");
        let mut expected = Vec::new();
        for &member in &members {
            adjacency.children_of(member, &mut expected).expect("list");
        }
        expected.sort_unstable();
        let pushed = adjacency.push(&held).expect("push");
        assert_eq!(pushed.iter().collect::<Vec<_>>(), expected);
        let spans = adjacency.spans(&held, u64::MAX).expect("spans").expect("no most");
        let read = adjacency.push_spans(&spans, 1).expect("push the spans");
        assert_eq!(read.iter().collect::<Vec<_>>(), expected, "the same rows as the push");
        let other = Adjacency::build(&[0, 1], 2).expect("another adjacency");
        assert!(other.push_spans(&spans, 1).is_err(), "lists of another adjacency");
    }

    #[test]
    fn a_push_shared_over_threads_finds_the_rows_one_thread_finds() {
        // Children in no order over several buckets, and enough of them that the lists are cut
        // into a share for each of three threads, with a stretch of buckets each to set.
        let mut seed = 11_u64;
        let parents = 50_000;
        let parents_of: Vec<Rid> = (0..8 * BUCKET_ROWS)
            .map(|_| {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (seed >> 20) % parents
            })
            .collect();
        let adjacency = Adjacency::build(&parents_of, parents).expect("build");
        let held = Rids::from_sorted(parents, (0..parents).step_by(2).collect()).expect("held");
        let spans = adjacency.spans(&held, u64::MAX).expect("spans").expect("no most");
        assert!(spans.rows() >= 3 * SHARED_ROWS, "enough rows for three threads");
        let alone = adjacency.push_spans(&spans, 1).expect("one thread");
        for threads in [2, 3, 8] {
            let shared = adjacency.push_spans(&spans, threads).expect("shared");
            assert_eq!(shared, alone, "{threads} threads");
        }
        assert_eq!(alone, adjacency.push(&held).expect("push"));
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
