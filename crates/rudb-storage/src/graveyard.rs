//! The graveyard of a frozen stripe, `engine-v4/14-reads-stay-fast.md` section 14.8.
//!
//! A stripe is frozen as of F, the clock its rows are final at, and a frozen stripe has no
//! `created` stamps and no undo chains to tell a reader from before F which rows were not there
//! yet or what they held. Waiting for every such reader to end before swapping the frozen stripe
//! in means a ten-second query keeps ten seconds of hot stripes in memory. So the freezer swaps it
//! in at once and keeps beside it the rows a statement still running sees differently from F: the
//! ones inserted after its snapshot, the ones deleted after it, and the ones written after it,
//! with the values it sees. That is the graveyard of Alhomssi and Leis (Scalable and Robust
//! Snapshot Isolation for High-Performance Storage Engines, PVLDB 16, 2023): rows deleted but still
//! visible to an old snapshot leave the main structure for a side one, so the main one does not
//! have to keep them.
//!
//! Only the snapshots held when the stripe is swapped in can read it below F, since a statement
//! that begins later takes a snapshot at or past F. So the graveyard answers for those snapshots,
//! each slot's rows as each of them sees it, and is dropped once the table's horizon passes F, see
//! [`crate::stripes::StripeDirectory::bury`]. A snapshot the collector killed may be left without
//! an answer, which it finds out about when its statement ends.

use crate::deletes::{PART_ROWS, PART_WORDS};
use crate::hot::HotStripe;

/// A value of a row in the graveyard: a fixed-width value as a hot stripe holds it, or the bytes
/// of a text, which the arena of the hot stripe they came from no longer holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// A fixed-width value, zero extended.
    Fixed(u128),
    /// The bytes of a text or a blob.
    Text(Box<[u8]>),
}

/// What a snapshot sees of a slot of the graveyard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// No row: inserted after it, deleted at or before it, or a hole.
    Absent,
    /// The row as the frozen stripe holds it.
    Frozen,
    /// The row with these values, one a column, `None` for null.
    Values(Box<[Option<Cell>]>),
}

/// A slot some held snapshot sees differently from F.
#[derive(Debug)]
struct Ghost {
    slot: u32,
    /// Whether the frozen stripe has it deleted or as a hole, in the base of its delete vector.
    dead: bool,
    /// What each snapshot of [`Graveyard::snapshots`] sees.
    seen: Box<[Seen]>,
}

/// The rows of a frozen stripe as the snapshots before its F see them, where that differs from F.
#[derive(Debug)]
pub struct Graveyard {
    /// F.
    at: u64,
    /// The snapshots below F held when it was made, in order.
    snapshots: Box<[u64]>,
    /// By slot.
    ghosts: Box<[Ghost]>,
    bytes: usize,
}

impl Graveyard {
    /// The graveyard of `hot`, whose first `rows` slots are final as of `at`, for the statements
    /// at `snapshots`, or `None` when none of them is before `at` or none sees a row differently.
    pub(crate) fn dig(hot: &HotStripe, rows: u32, at: u64, snapshots: &[u64]) -> Option<Self> {
        let mut snapshots = snapshots.iter().copied().filter(|&s| s < at).collect::<Vec<_>>();
        snapshots.sort_unstable();
        snapshots.dedup();
        let oldest = *snapshots.first()?;
        let columns = (0..hot.width()).collect::<Vec<_>>();
        let (mut now, mut then) = (vec![None; columns.len()], vec![None; columns.len()]);
        let mut ghosts = Vec::new();
        let mut bytes = 0;
        for slot in 0..rows {
            if !hot.changed_after(slot, oldest) {
                continue;
            }
            let dead = !hot.there(slot, at);
            for (value, &column) in now.iter_mut().zip(&columns) {
                *value = hot.read(slot, column);
            }
            let seen = snapshots
                .iter()
                .map(|&snapshot| {
                    if !hot.there(slot, snapshot) {
                        return Seen::Absent;
                    }
                    hot.read_as_of(slot, &columns, snapshot, 0, &mut then);
                    if !dead && then == now {
                        return Seen::Frozen;
                    }
                    let values = then
                        .iter()
                        .zip(&columns)
                        .map(|(&value, &column)| {
                            if !hot.is_text(column) {
                                return value.map(Cell::Fixed);
                            }
                            let mut text = Vec::new();
                            hot.text(value, &mut text).then(|| Cell::Text(text.into()))
                        })
                        .collect::<Box<[_]>>();
                    bytes += values.iter().map(cell_bytes).sum::<usize>();
                    Seen::Values(values)
                })
                .collect::<Box<[_]>>();
            let same = |seen: &Seen| match seen {
                Seen::Absent => dead,
                Seen::Frozen => !dead,
                Seen::Values(_) => false,
            };
            if !seen.iter().all(same) {
                bytes += size_of::<Ghost>() + seen.len() * size_of::<Seen>();
                ghosts.push(Ghost { slot, dead, seen });
            }
        }
        (!ghosts.is_empty()).then(|| Self {
            at,
            bytes: bytes + snapshots.len() * 8,
            snapshots: snapshots.into(),
            ghosts: ghosts.into(),
        })
    }

    /// F, the checkpoint of the frozen stripe, at or past which it has nothing to say.
    #[must_use]
    pub fn at(&self) -> u64 {
        self.at
    }

    /// The snapshots it answers for, in order.
    #[must_use]
    pub fn snapshots(&self) -> &[u64] {
        &self.snapshots
    }

    /// Slots it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ghosts.len()
    }

    /// Whether it holds no slot, which [`Self::dig`] never makes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ghosts.is_empty()
    }

    /// Bytes it holds, about.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// What a reader at `snapshot` sees of `slot` when that is not the row the frozen stripe
    /// holds: `Some(None)` for no row and `Some(Some(values))` for these values. `None` means
    /// the frozen stripe answers, under its delete vector.
    #[must_use]
    pub fn seen(&self, slot: u32, snapshot: u64) -> Option<Option<&[Option<Cell>]>> {
        let at = self.index(snapshot)?;
        let ghost =
            &self.ghosts[self.ghosts.binary_search_by_key(&slot, |ghost| ghost.slot).ok()?];
        match &ghost.seen[at] {
            Seen::Absent => Some(None),
            Seen::Frozen => None,
            Seen::Values(values) => Some(Some(values)),
        }
    }

    /// Sets the bits of `mask`, the slots of `part` a reader at `snapshot` sees by the frozen
    /// stripe's delete vector, to what it sees in fact: a slot of a row inserted after `snapshot`
    /// is cleared, and one of a row deleted after it is set. A row the frozen stripe still holds
    /// keeps its bit, which a delete taken after the freeze began may have cleared.
    pub(crate) fn patch(&self, part: u32, snapshot: u64, mask: &mut [u64; PART_WORDS]) {
        let Some(at) = self.index(snapshot) else { return };
        let start = part * PART_ROWS;
        let first = self.ghosts.partition_point(|ghost| ghost.slot < start);
        for ghost in self.ghosts[first..].iter().take_while(|ghost| ghost.slot < start + PART_ROWS)
        {
            let bit = (ghost.slot - start) as usize;
            let (word, bit) = (&mut mask[bit / 64], 1 << (bit % 64));
            match ghost.seen[at] {
                Seen::Absent => *word &= !bit,
                _ if ghost.dead => *word |= bit,
                _ => {}
            }
        }
    }

    /// The place in [`Self::snapshots`] of the newest one at or before `snapshot`, which is
    /// `snapshot` itself for every statement that can read below F, or the oldest for a killed
    /// one older than all. `None` at or past F.
    fn index(&self, snapshot: u64) -> Option<usize> {
        (snapshot < self.at)
            .then(|| self.snapshots.partition_point(|&held| held <= snapshot).saturating_sub(1))
    }
}

fn cell_bytes(cell: &Option<Cell>) -> usize {
    size_of::<Option<Cell>>()
        + match cell {
            Some(Cell::Text(text)) => text.len(),
            _ => 0,
        }
}
