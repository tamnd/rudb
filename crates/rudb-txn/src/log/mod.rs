//! The write-ahead log, `09-the-log.md`.
//!
//! Redo only and written at commit: a transaction's changes stay in its own memory until it
//! commits, and then go to a lane as one block ending in a Commit record. Nothing is undone at
//! recovery because nothing uncommitted was ever written.
//!
//! This is one lane, which recycles the segments a checkpoint retires. The spec's lanes and the
//! spill of a transaction too large to hold come after it, and each is a change to how blocks
//! reach a lane, not to the bytes a lane holds.

mod crc;
mod format;
mod lane;
mod replay;

pub use format::{
    Checkpointed, Commit, Kind, LZ4, RANGE, RECORD_ALIGN, RECORD_HEADER, RecordHeader,
    SEGMENT_BYTES, SEGMENT_HEADER, SEGMENT_MAGIC, SEGMENT_VERSION, SPILLED, SegmentHeader,
    decode_record, encode_record, record_bytes, seed,
};
pub use lane::{
    Block, CommitSync, Lane, Options, Stats, parse_segment_name, remove_spare, segment_name,
    segments, spares,
};
pub use replay::{Committed, Payload, Record, Replayed, replay};

#[cfg(test)]
mod tests;
