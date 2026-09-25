//! The log anchor, the `RUDBWL1` catalog extension of `engine-v4/03-the-shape.md` section 3.4.
//!
//! A file says how much of its log it already holds. Every commit at or below the durable cut is in
//! the file's pages, and replay starts at each lane's segment and offset and skips any Commit at or
//! below the cut. That is what makes the order of a checkpoint safe: the file is published first
//! and the segments behind it are recycled after, and a crash between the two leaves segments whose
//! commits the anchor already says are in.
//!
//! The void list is the one thing recovery writes here (`12-recovery.md` section 12.7): timestamps
//! above the cut that were lost with a torn tail, which a later replay must never apply. It is
//! written and read now and stays empty until recovery has lanes that can tear.

use rudb_common::Result;

use super::{Cursor, invalid, put_u32, put_u64};

/// The magic the anchor block starts with, after the device card.
pub(crate) const LOG_ANCHOR: &[u8; 8] = b"RUDBWL1\0";

/// The one layout of the block, which a later one would change.
const VERSION: u8 = 1;

/// The most lanes a log has, `09-the-log.md` section 9.4.
const MAX_LANES: usize = 64;

/// The most void timestamps an anchor keeps.
const MAX_VOIDS: usize = 1 << 16;

/// Where replay of one lane starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LaneStart {
    /// The segment's sequence.
    pub sequence: u64,
    /// The byte offset in it.
    pub offset: u64,
}

/// How much of the log a file holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogAnchor {
    /// The id every segment header of this database's log carries.
    pub database: u64,
    /// The durable cut: every commit at or below it is in the file.
    pub durable: u64,
    /// Where replay starts, one entry per lane.
    pub lanes: Vec<LaneStart>,
    /// Timestamps above the cut that were lost and must never be replayed, in order.
    pub voids: Vec<u64>,
}

impl LogAnchor {
    /// Whether a commit at `ts` is one replay applies: above the cut and not void.
    #[must_use]
    pub fn replays(&self, ts: u64) -> bool {
        ts > self.durable && self.voids.binary_search(&ts).is_err()
    }

    /// Appends the block, magic first.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.lanes.len() > MAX_LANES || self.voids.len() > MAX_VOIDS {
            return Err(invalid("log anchor holds more lanes or voids than it can"));
        }
        out.extend_from_slice(LOG_ANCHOR);
        out.push(VERSION);
        put_u64(out, self.database);
        put_u64(out, self.durable);
        out.push(self.lanes.len() as u8);
        for lane in &self.lanes {
            put_u64(out, lane.sequence);
            put_u64(out, lane.offset);
        }
        put_u32(out, self.voids.len() as u32);
        for &ts in &self.voids {
            put_u64(out, ts);
        }
        Ok(())
    }

    /// Reads the block after its magic.
    pub(crate) fn decode(cur: &mut Cursor<'_>) -> Result<Self> {
        if cur.u8()? != VERSION {
            return Err(invalid("log anchor version differs"));
        }
        let database = cur.u64()?;
        let durable = cur.u64()?;
        let count = cur.u8()? as usize;
        if count > MAX_LANES {
            return Err(invalid("log anchor names more lanes than a log has"));
        }
        let mut lanes = Vec::with_capacity(count);
        for _ in 0..count {
            lanes.push(LaneStart { sequence: cur.u64()?, offset: cur.u64()? });
        }
        let count = cur.u32()? as usize;
        if count > MAX_VOIDS {
            return Err(invalid("log anchor holds more voids than it can"));
        }
        let mut voids = Vec::with_capacity(count);
        for _ in 0..count {
            voids.push(cur.u64()?);
        }
        if voids.windows(2).any(|pair| pair[0] >= pair[1])
            || voids.first().is_some_and(|&ts| ts <= durable)
        {
            return Err(invalid("log anchor voids are out of order or under the cut"));
        }
        Ok(Self { database, durable, lanes, voids })
    }
}
