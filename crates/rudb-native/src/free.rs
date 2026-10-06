//! The free extent list, the `RUDBFX1` catalog extension of `engine-v4/10-freezing-and-checkpoint.md`
//! section 10.8, and the runs each table owns, which is how a commit knows what it can give back.
//!
//! A commit that stops naming bytes of the file gives them back here: the catalog before it, a
//! table directory it wrote again, the record of rows gone from a table that it wrote down again,
//! and every page of a table it wrote again from nothing. Each run is tagged with the generation
//! that first left it out, and a later commit writes into it rather than onto the end of the file
//! once nothing can still read it.
//!
//! Nothing can still read a run freed at generation g when two things hold. The slot that named
//! the generation before g is gone, which it is once g + 1 is committed, since that commit's slot
//! is written over it, so a commit at g + 2 or later may use the run. And nothing in this process
//! that read the file at a generation below g can still read the run, which every [`Hold`] keeps
//! track of. A [`super::Catalog`] holds everything its generation names, since it can open any
//! table of it later. A [`super::Reader`], and every page and dictionary it hands out, holds only
//! the runs its own table owns, since it read its directory and its gone rows when it was opened.
//! That second kind is what lets a database keep the reader of a table nothing changed for many
//! generations without keeping every other table's old pages from being written over.
//!
//! A reader in another process is not covered. Nothing keeps a second process off a file one
//! process writes, so the rule is the one the log already has: one process per database file.
//!
//! The tables are the hard part, because a table directory names its pages in a dozen places and
//! a walk over them that missed one would hand out a page that is still read. So the writer
//! records every run it places for a table, and the directory carries that list in its own block,
//! [`OWNED`]. What a table owns is then given back whole when the table is written again, and a
//! table from a file older than the block owns nothing anybody knows about and gives nothing back.
//! That leaks its pages until a `VACUUM`, which is the safe direction to be wrong in.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use rudb_common::Result;

use super::{Cursor, HEADER, invalid, put_u32, put_u64};

/// The magic the free list starts with, after the log anchor.
pub(crate) const FREE_EXTENTS: &[u8; 8] = b"RUDBFX1\0";

/// The magic of the directory block that lists the runs a table owns.
///
/// Same convention as the other trailing directory blocks: written only when the list is not
/// empty, and refused by a build that predates it with `directory extension magic differs`. That
/// is the answer wanted here, because a build that wrote this table again without giving its runs
/// back would leave a free list that no longer agrees with the file.
pub(crate) const OWNED: &[u8; 8] = b"RUDBOE1\0";

/// The most runs a free list keeps. Past it the smallest are left out, which leaks them and is
/// safe, since a run nobody lists is a run nobody writes into.
const MAX_FREE: usize = 1 << 16;

/// The most runs one table's list may hold, which a torn directory would otherwise turn into an
/// allocation of any size.
const MAX_OWNED: usize = 1 << 24;

/// A run of the file that no committed generation from `since` on reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Freed {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    /// The first generation that does not name the run.
    pub(crate) since: u64,
    /// The [`tag`] of the table whose pages the run held, or zero for a run any reader of the
    /// file might have read, such as a catalog or a directory.
    pub(crate) table: u64,
}

impl Freed {
    fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// Appends the free list, magic first, or nothing for an empty one.
pub(crate) fn encode(free: &[Freed], out: &mut Vec<u8>) -> Result<()> {
    if free.is_empty() {
        return Ok(());
    }
    out.extend_from_slice(FREE_EXTENTS);
    put_u32(out, u32::try_from(free.len()).map_err(|_| invalid("too many free runs"))?);
    for run in free {
        put_u64(out, run.offset);
        put_u64(out, run.length);
        put_u64(out, run.since);
        put_u64(out, run.table);
    }
    Ok(())
}

/// Reads a free list after its magic, refusing one whose runs are out of order, overlap or leave a
/// file `size` bytes long.
pub(crate) fn decode(cur: &mut Cursor<'_>, size: u64) -> Result<Vec<Freed>> {
    let count = cur.u32()? as usize;
    if count == 0 || count > MAX_FREE {
        return Err(invalid("free run count is outside its bound"));
    }
    let mut free = Vec::with_capacity(count);
    let mut after = HEADER;
    for _ in 0..count {
        let run =
            Freed { offset: cur.u64()?, length: cur.u64()?, since: cur.u64()?, table: cur.u64()? };
        let end =
            run.offset.checked_add(run.length).ok_or_else(|| invalid("free run overflows"))?;
        if run.length == 0 || run.since == 0 || run.offset < after || end > size {
            return Err(invalid("free runs overlap, are out of order or leave the file"));
        }
        after = end;
        free.push(run);
    }
    Ok(free)
}

/// Appends the runs a table owns, magic first, or nothing when it owns none anybody knows of.
pub(crate) fn encode_owned(owned: &[(u64, u64)], out: &mut Vec<u8>) -> Result<()> {
    if owned.is_empty() {
        return Ok(());
    }
    out.extend_from_slice(OWNED);
    put_u32(out, u32::try_from(owned.len()).map_err(|_| invalid("too many owned runs"))?);
    for &(offset, length) in owned {
        put_u64(out, offset);
        put_u64(out, length);
    }
    Ok(())
}

/// Reads the runs a table owns after their magic, with the same checks as [`decode`].
pub(crate) fn decode_owned(cur: &mut Cursor<'_>, size: u64) -> Result<Vec<(u64, u64)>> {
    let count = cur.u32()? as usize;
    if count == 0 || count > MAX_OWNED {
        return Err(invalid("owned run count is outside its bound"));
    }
    let mut owned = Vec::with_capacity(count.min(1 << 16));
    let mut after = HEADER;
    for _ in 0..count {
        let (offset, length) = (cur.u64()?, cur.u64()?);
        let end = offset.checked_add(length).ok_or_else(|| invalid("owned run overflows"))?;
        if length == 0 || offset < after || end > size {
            return Err(invalid("owned runs overlap, are out of order or leave the file"));
        }
        after = end;
        owned.push((offset, length));
    }
    Ok(owned)
}

/// Sorts runs and joins the ones that touch, so a table written straight onto the end of the
/// file owns one run however many pages it has.
pub(crate) fn coalesce(runs: &mut Vec<(u64, u64)>) {
    runs.retain(|&(_, length)| length > 0);
    runs.sort_unstable();
    let mut joined: Vec<(u64, u64)> = Vec::with_capacity(runs.len());
    for &(offset, length) in runs.iter() {
        match joined.last_mut() {
            Some((at, len)) if *at + *len == offset => *len += length,
            _ => joined.push((offset, length)),
        }
    }
    *runs = joined;
}

/// The pages a record of gone rows names, which are the rows an update wrote, see
/// [`super::GoneRows`]. A record without patches names none.
///
/// It reads only as far as it has to and checks nothing a decode would, because the record was
/// read and checked when the table was opened, and what this answers only decides what is given
/// back. A record it cannot follow gives back nothing but itself.
pub(crate) fn patch_pages(bytes: &[u8]) -> Result<Vec<(u64, u64)>> {
    let mut cur = Cursor::new(bytes);
    let words = |count: usize, width: usize| {
        count.checked_mul(width).ok_or_else(|| invalid("gone rows overflow"))
    };
    for _ in 0..cur.u32()? {
        cur.u32()?;
        let count = cur.u32()? as usize;
        cur.take(words(count, 8)?)?;
    }
    let sums = cur.u32()? as usize;
    // A flag, then the sum's two halves and the count.
    cur.take(words(sums, 1 + 8 + 8 + 8)?)?;
    let width = cur.u32()? as usize;
    cur.take(width)?;
    let mut pages = Vec::new();
    for _ in 0..cur.u32()? {
        cur.u32()?;
        let slots = cur.u32()? as usize;
        cur.take(words(slots, 4)?)?;
        for _ in 0..width {
            let offset = cur.u64()?;
            let length = cur.u32()?;
            cur.u64()?;
            pages.push((offset, u64::from(length)));
        }
    }
    Ok(pages)
}

/// Where one commit puts what it writes, and what it gives back.
///
/// It starts from the free list the committed generation holds and the end of the file. A write
/// goes into the smallest run that takes it and that nothing can still read, see the module, and
/// onto the end of the file when there is none. The commit's own catalog always goes on the end,
/// since it is what holds the list, and that keeps the list it writes the one it was built from.
#[derive(Debug)]
pub(crate) struct Space {
    /// Where the next write that no run takes goes.
    end: u64,
    /// Where the file ended when the commit started. Everything given back is under it.
    start: u64,
    /// Sorted by offset, none overlapping or touching.
    free: Vec<Freed>,
    /// The generation being written, which is what a run given back now is tagged with.
    generation: u64,
    /// What the holds of this process had open when the commit started: the generation, and the
    /// table tag, or zero for a whole catalog. A hold taken later read a generation no older than
    /// the committed one, which names nothing this commit could write into.
    holds: Vec<(u64, u64)>,
    /// The runs this commit took out of the list, so that a run given back twice is caught.
    taken: Vec<(u64, u64)>,
}

impl Space {
    /// The space of a commit at `generation` of the file at `path`, which ends at `end` and has
    /// `free` given back by the commits before.
    pub(crate) fn new(path: &Path, end: u64, free: Vec<Freed>, generation: u64) -> Self {
        let holds = holds(path);
        Self { end, start: end, free, generation, holds, taken: Vec::new() }
    }

    /// The space of a file being written from nothing, which has nothing to give.
    pub(crate) fn fresh(end: u64) -> Self {
        Self {
            end,
            start: end,
            free: Vec::new(),
            generation: 1,
            holds: Vec::new(),
            taken: Vec::new(),
        }
    }

    /// Whether this commit may write into `run`, see the module.
    fn usable(&self, run: &Freed) -> bool {
        run.since.saturating_add(2) <= self.generation
            && self.holds.iter().all(|&(generation, table)| {
                run.since <= generation || (table != 0 && run.table != 0 && table != run.table)
            })
    }

    /// Whether the run at `offset` is one the committed generation could name, rather than one
    /// this commit wrote.
    pub(crate) fn committed(&self, offset: u64) -> bool {
        offset < self.start
            && !self.taken.iter().any(|&(at, len)| at <= offset && offset < at + len)
    }

    /// Where the file ends, counting what this commit put on the end of it.
    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Finds a place for `length` bytes, and answers whether it is on the end of the file.
    pub(crate) fn take(&mut self, length: u64) -> Result<(u64, bool)> {
        if length > 0 {
            let fits = self
                .free
                .iter()
                .enumerate()
                .filter(|(_, run)| run.length >= length && self.usable(run))
                .min_by_key(|(_, run)| run.length)
                .map(|(at, _)| at);
            if let Some(at) = fits {
                let run = &mut self.free[at];
                let offset = run.offset;
                run.offset += length;
                run.length -= length;
                if run.length == 0 {
                    self.free.remove(at);
                }
                self.taken.push((offset, length));
                return Ok((offset, false));
            }
        }
        Ok((self.append(length)?, true))
    }

    /// Finds a place for `length` bytes on the end of the file whatever runs there are, which is
    /// where a catalog goes.
    pub(crate) fn append(&mut self, length: u64) -> Result<u64> {
        let offset = self.end;
        self.end =
            offset.checked_add(length).ok_or_else(|| invalid("native file length overflow"))?;
        Ok(offset)
    }

    /// Gives back `length` bytes at `offset`, which the generation before this one reads and this
    /// one does not, tagged with the [`tag`] of the table they were pages of, or zero.
    ///
    /// A run that is already free, or that this commit wrote, is a bug in whoever gave it, and it
    /// is dropped rather than listed: listing it would be handing out bytes something reads, and
    /// dropping it only leaks them. A debug build stops there instead, which is what the tests run.
    pub(crate) fn give(&mut self, offset: u64, length: u64, table: u64) {
        if length == 0 {
            return;
        }
        let end = offset.saturating_add(length);
        let clashes = offset < HEADER
            || end > self.start
            || self.free.iter().any(|run| offset < run.end() && run.offset < end)
            || self.taken.iter().any(|&(at, len)| offset < at + len && at < end);
        debug_assert!(!clashes, "{length} bytes at {offset} given back twice or never written");
        if clashes {
            return;
        }
        let at = self.free.partition_point(|run| run.offset < offset);
        self.free.insert(at, Freed { offset, length, since: self.generation, table });
        // Joined with the runs either side when they touch and were the same table's. The joined
        // run takes the later of the two generations, which can only make it wait longer than it
        // had to.
        let joins =
            |left: &Freed, right: &Freed| left.end() == right.offset && left.table == right.table;
        if at + 1 < self.free.len() && joins(&self.free[at], &self.free[at + 1]) {
            let next = self.free.remove(at + 1);
            self.free[at].length += next.length;
            self.free[at].since = self.free[at].since.max(next.since);
        }
        if at > 0 && joins(&self.free[at - 1], &self.free[at]) {
            let run = self.free.remove(at);
            self.free[at - 1].length += run.length;
            self.free[at - 1].since = self.free[at - 1].since.max(run.since);
        }
    }

    /// The free list the commit writes down.
    pub(crate) fn list(&self) -> Vec<Freed> {
        let mut free = self.free.clone();
        if free.len() > MAX_FREE {
            free.sort_unstable_by_key(|run| std::cmp::Reverse(run.length));
            free.truncate(MAX_FREE);
            free.sort_unstable_by_key(|run| run.offset);
        }
        free
    }
}

/// The tag a table's runs are given back under, which is never zero.
pub(crate) fn tag(name: &str) -> u64 {
    // FNV-1a, which is stable from build to build, since the tag is written down.
    let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    hash.max(1)
}

/// Something in this process that can still read runs of a file at the generation it was opened
/// at, for as long as the last clone of its `Arc` is alive.
#[derive(Debug)]
pub(crate) struct Hold;

/// Every hold of this process: the file, the generation, and the table tag or zero.
type Open = Vec<(PathBuf, u64, u64, Weak<Hold>)>;

static OPEN: Mutex<Open> = Mutex::new(Vec::new());

/// The name a file is kept under in [`OPEN`], which is the same for every path that reaches it.
pub(crate) fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A hold on the runs of the file kept under `key` that `generation` names, or only the ones the
/// table tagged `table` owns when that is not zero.
pub(crate) fn hold(key: &Path, generation: u64, table: u64) -> Arc<Hold> {
    let held = Arc::new(Hold);
    let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    open.retain(|(.., hold)| hold.strong_count() > 0);
    open.push((key.to_path_buf(), generation, table, Arc::downgrade(&held)));
    held
}

/// Every hold still alive on the file at `path`, as its generation and table tag.
fn holds(path: &Path) -> Vec<(u64, u64)> {
    let key = key(path);
    let mut open = OPEN.lock().unwrap_or_else(PoisonError::into_inner);
    open.retain(|(.., hold)| hold.strong_count() > 0);
    open.iter()
        .filter(|(held, ..)| *held == key)
        .map(|&(_, generation, table, _)| (generation, table))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(free: Vec<Freed>, generation: u64) -> Space {
        let end = 1000;
        Space { end, start: end, free, generation, holds: Vec::new(), taken: Vec::new() }
    }

    fn run(offset: u64, length: u64, since: u64, table: u64) -> Freed {
        Freed { offset, length, since, table }
    }

    #[test]
    fn a_run_waits_two_generations_and_the_smallest_that_fits_is_taken() {
        let free = vec![run(100, 50, 3, 0), run(200, 20, 3, 0), run(300, 80, 4, 0)];
        let mut early = space(free.clone(), 5);
        assert_eq!(early.take(10).unwrap(), (200, false));
        assert_eq!(early.take(60).unwrap(), (1000, true), "the run of 80 is too young");
        let mut late = space(free, 6);
        assert_eq!(late.take(60).unwrap(), (300, false));
        assert_eq!(late.take(10).unwrap(), (200, false));
        assert_eq!(late.list()[0], run(100, 50, 3, 0));
    }

    #[test]
    fn a_hold_keeps_what_it_can_read_and_nothing_else() {
        let free = vec![run(100, 50, 3, 0), run(200, 50, 3, 7), run(300, 50, 3, 9)];
        // A catalog at 2 can read every run freed at 3.
        let mut whole = space(free.clone(), 6);
        whole.holds = vec![(2, 0)];
        assert_eq!(whole.take(10).unwrap(), (1000, true));
        // A reader of table 7 at 2 keeps that table's run and the one any reader might read.
        let mut table = space(free.clone(), 6);
        table.holds = vec![(2, 7)];
        assert_eq!(table.take(50).unwrap(), (300, false));
        assert_eq!(table.take(50).unwrap(), (1000, true));
        // One that read 3 or later reads none of them.
        let mut later = space(free, 6);
        later.holds = vec![(3, 0), (5, 7)];
        assert_eq!(later.take(50).unwrap(), (100, false));
    }

    #[test]
    fn runs_given_back_join_their_neighbours_and_survive_the_catalog() {
        let mut space = space(vec![run(100, 50, 3, 0)], 7);
        space.give(150, 10, 0);
        space.give(90, 10, 0);
        space.give(400, 8, 5);
        space.give(408, 8, 6);
        let free = space.list();
        assert_eq!(free, vec![run(90, 70, 7, 0), run(400, 8, 7, 5), run(408, 8, 7, 6)]);
        let mut out = Vec::new();
        encode(&free, &mut out).unwrap();
        let mut cur = Cursor::new(&out[8..]);
        assert_eq!(decode(&mut cur, 1000).unwrap(), free);
        let mut cur = Cursor::new(&out[8..]);
        assert!(decode(&mut cur, 405).is_err(), "a run past the end of the file");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "given back twice")]
    fn a_run_given_back_twice_is_caught() {
        let mut space = space(vec![run(100, 50, 3, 0)], 7);
        space.give(120, 10, 0);
    }

    #[test]
    fn owned_runs_join_and_round_trip() {
        let mut runs = vec![(300, 10), (100, 50), (150, 20), (0, 0)];
        coalesce(&mut runs);
        assert_eq!(runs, vec![(100, 70), (300, 10)]);
        let mut out = Vec::new();
        encode_owned(&runs, &mut out).unwrap();
        let mut cur = Cursor::new(&out[8..]);
        assert_eq!(decode_owned(&mut cur, 400).unwrap(), runs);
    }
}
