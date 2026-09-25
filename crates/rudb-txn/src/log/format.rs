//! The bytes of a segment header and of a record, `09-the-log.md` sections 9.3 and 9.4.

use super::crc::{crc32c, extend};

/// The first eight bytes of every segment.
pub const SEGMENT_MAGIC: [u8; 8] = *b"RUDBLOG1";

/// The segment format this build writes and the only one it reads.
pub const SEGMENT_VERSION: u32 = 1;

/// How many bytes of a segment its header takes. Records start here.
pub const SEGMENT_HEADER: usize = 4096;

/// How many bytes a segment is unless a test asks for less.
pub const SEGMENT_BYTES: u64 = 64 << 20;

/// How many bytes every record's header takes.
pub const RECORD_HEADER: usize = 20;

/// Records start on this boundary, and the space up to it after a payload is zeros.
pub const RECORD_ALIGN: usize = 8;

/// The record is one of a transaction's that went to the lane before its Commit did.
pub const SPILLED: u16 = 1;

/// A Delete's slots are `(first, last)` pairs rather than single slots.
pub const RANGE: u16 = 1 << 1;

/// The payload is LZ4 compressed.
pub const LZ4: u16 = 1 << 2;

/// What a record holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A run of consecutive new rows of one stripe.
    Insert,
    /// New values for rows of one stripe that had the same columns changed.
    Update,
    /// The operand of a commutative change to one column.
    Delta,
    /// Rows of one stripe that were deleted.
    Delete,
    /// The end of a transaction's block.
    Commit,
    /// A bulk load published into a live database.
    Attach,
    /// A catalog change.
    Ddl,
    /// A checkpoint round that finished.
    Checkpoint,
}

impl Kind {
    /// The byte this kind is written as.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Insert => 1,
            Self::Update => 2,
            Self::Delta => 3,
            Self::Delete => 4,
            Self::Commit => 5,
            Self::Attach => 6,
            Self::Ddl => 7,
            Self::Checkpoint => 8,
        }
    }

    /// The kind written as `byte`, or `None` for a byte no kind is written as, zero included,
    /// which is what the preallocated end of a segment reads as.
    #[must_use]
    pub const fn of(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Insert,
            2 => Self::Update,
            3 => Self::Delta,
            4 => Self::Delete,
            5 => Self::Commit,
            6 => Self::Attach,
            7 => Self::Ddl,
            8 => Self::Checkpoint,
            _ => return None,
        })
    }
}

/// The seed every checksum in segment `sequence` starts from.
///
/// A recycled segment still holds the records of the segment it used to be, and they are well
/// formed. Seeding with the sequence number makes them fail the check under the new one, so replay
/// stops where the new records stop.
#[must_use]
pub const fn seed(sequence: u64) -> u32 {
    (sequence as u32) ^ ((sequence >> 32) as u32)
}

/// The fixed fields of a segment's first 4 KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// The lane the segment belongs to.
    pub lane: u8,
    /// Its place in the lane, which is also what its checksums are seeded with.
    pub sequence: u64,
    /// The database it belongs to, so a segment copied in from another database is not replayed.
    pub database: u64,
}

impl SegmentHeader {
    /// The header as it is written: the fields, a checksum of them, and zeros to 4 KiB.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![0_u8; SEGMENT_HEADER];
        bytes[0..8].copy_from_slice(&SEGMENT_MAGIC);
        bytes[8..12].copy_from_slice(&SEGMENT_VERSION.to_le_bytes());
        bytes[12] = self.lane;
        bytes[16..24].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.database.to_le_bytes());
        let crc = crc32c(&bytes[0..40]);
        bytes[40..44].copy_from_slice(&crc.to_le_bytes());
        bytes
    }

    /// The header at the start of `bytes`, or `None` when it is not one this build wrote: a short
    /// read, another magic or version, a checksum that fails, or reserved bytes that are not zero.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.get(..SEGMENT_HEADER)?;
        let (word, half) = (|at| word_at(bytes, at), |at| half_at(bytes, at));
        let sound = bytes[0..8] == SEGMENT_MAGIC
            && half(8) == SEGMENT_VERSION
            && bytes[13..16] == [0; 3]
            && word(32) == 0
            && half(40) == crc32c(&bytes[0..40])
            && bytes[44..].iter().all(|&b| b == 0);
        sound.then(|| Self { lane: bytes[12], sequence: word(16), database: word(24) })
    }
}

/// The fields of a record's header apart from its length and checksum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// What the payload holds.
    pub kind: Kind,
    /// The lane the record was written to.
    pub lane: u8,
    /// [`SPILLED`], [`RANGE`] and [`LZ4`].
    pub flags: u16,
    /// The order across lanes: the commit timestamp for a record written at commit, the
    /// transaction id for a spilled one, and the snapshot for a Checkpoint.
    pub gsn: u64,
}

/// How many bytes a record with a payload of `len` takes in the lane, padding included.
#[must_use]
pub const fn record_bytes(len: usize) -> usize {
    (RECORD_HEADER + len).next_multiple_of(RECORD_ALIGN)
}

/// Appends a record to `out`: its header, the payload, and zeros to the next 8 byte boundary.
///
/// `seed` is the segment's, from [`seed`], so the record only verifies in the segment it was
/// written for.
///
/// # Panics
///
/// If the payload is 4 GiB or longer, which no record is: a lane's segment is far smaller, and a
/// transaction spills long before its block gets near one.
pub fn encode_record(out: &mut Vec<u8>, header: &RecordHeader, payload: &[u8], seed: u32) {
    let len = u32::try_from(payload.len()).expect("a record's payload is under 4 GiB");
    let start = out.len();
    out.extend_from_slice(&len.to_le_bytes());
    out.push(header.kind.byte());
    out.push(header.lane);
    out.extend_from_slice(&header.flags.to_le_bytes());
    out.extend_from_slice(&header.gsn.to_le_bytes());
    let crc = extend(extend(seed, &out[start..start + 16]), payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(start + record_bytes(payload.len()), 0);
}

/// The record at the start of `bytes` and how many bytes it takes, or `None` when there is no
/// sound record there: too few bytes, a kind no record has, padding that is not zero, or a
/// checksum that fails under `seed`. Replay stops at the first `None`.
#[must_use]
pub fn decode_record(bytes: &[u8], seed: u32) -> Option<(RecordHeader, &[u8], usize)> {
    let head = bytes.get(..RECORD_HEADER)?;
    let len = half_at(head, 0) as usize;
    let kind = Kind::of(head[4])?;
    let flags = u16::from_le_bytes([head[6], head[7]]);
    let gsn = word_at(head, 8);
    let crc = half_at(head, 16);
    let total = record_bytes(len);
    let payload = bytes.get(RECORD_HEADER..RECORD_HEADER + len)?;
    let padding = bytes.get(RECORD_HEADER + len..total)?;
    if padding.iter().any(|&b| b != 0) || extend(extend(seed, &head[0..16]), payload) != crc {
        return None;
    }
    Some((RecordHeader { kind, lane: head[5], flags, gsn }, payload, total))
}

/// The little endian `u64` at `at`, which the caller has checked is in `bytes`.
fn word_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// The little endian `u32` at `at`, which the caller has checked is in `bytes`.
fn half_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

/// The payload of a Commit record, which ends a transaction's block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// The transaction.
    pub txn: u64,
    /// Its commit timestamp, which is also the record's `gsn`.
    pub commit_ts: u64,
    /// The newest commit it depends on.
    pub dep_ts: u64,
    /// How many records the block has before this one.
    pub records: u32,
    /// How many bytes they take in the lane, headers and padding included.
    pub bytes: u64,
}

impl Commit {
    /// How many bytes the payload is.
    pub const LEN: usize = 36;

    /// The payload as it is written.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0_u8; Self::LEN];
        out[0..8].copy_from_slice(&self.txn.to_le_bytes());
        out[8..16].copy_from_slice(&self.commit_ts.to_le_bytes());
        out[16..24].copy_from_slice(&self.dep_ts.to_le_bytes());
        out[24..28].copy_from_slice(&self.records.to_le_bytes());
        out[28..36].copy_from_slice(&self.bytes.to_le_bytes());
        out
    }

    /// The payload read back, or `None` if it is not 36 bytes.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN {
            return None;
        }
        let word = |at| word_at(bytes, at);
        Some(Self {
            txn: word(0),
            commit_ts: word(8),
            dep_ts: word(16),
            records: half_at(bytes, 24),
            bytes: word(28),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Commit, Kind, RECORD_HEADER, RecordHeader, SEGMENT_HEADER, SegmentHeader, decode_record,
        encode_record, record_bytes, seed,
    };

    #[test]
    fn a_segment_header_reads_back_and_a_damaged_one_does_not() {
        let header = SegmentHeader { lane: 3, sequence: 0x1_0000_0007, database: 0xDEAD_BEEF };
        let bytes = header.encode();
        assert_eq!(bytes.len(), SEGMENT_HEADER);
        assert_eq!(SegmentHeader::decode(&bytes), Some(header));
        for at in [0, 9, 12, 16, 30, 40, 100, 4095] {
            let mut damaged = bytes.clone();
            damaged[at] ^= 1;
            assert_eq!(SegmentHeader::decode(&damaged), None, "byte {at}");
        }
        assert_eq!(SegmentHeader::decode(&bytes[..100]), None);
        assert_eq!(SegmentHeader::decode(&[0; SEGMENT_HEADER]), None);
    }

    #[test]
    fn a_record_reads_back_padded_to_eight_bytes() {
        let header = RecordHeader { kind: Kind::Update, lane: 1, flags: 0, gsn: 42 };
        for len in [0, 1, 3, 4, 12, 13, 100] {
            let payload: Vec<u8> = (0..len).map(|n| n as u8 ^ 0x5A).collect();
            let mut out = vec![9, 9, 9];
            encode_record(&mut out, &header, &payload, seed(7));
            assert_eq!(out.len(), 3 + record_bytes(len));
            assert_eq!((out.len() - 3) % 8, 0);
            let (read, body, total) = decode_record(&out[3..], seed(7)).expect("a record");
            assert_eq!((read, body, total), (header, payload.as_slice(), out.len() - 3));
        }
    }

    #[test]
    fn a_record_does_not_verify_in_another_segment_or_damaged() {
        let header = RecordHeader { kind: Kind::Insert, lane: 0, flags: 0, gsn: 1 };
        let mut out = Vec::new();
        encode_record(&mut out, &header, b"hello, log", seed(5));
        assert!(decode_record(&out, seed(5)).is_some());
        assert!(decode_record(&out, seed(6)).is_none(), "a recycled segment's old record");
        for at in 0..out.len() {
            let mut damaged = out.clone();
            damaged[at] ^= 0x10;
            assert!(decode_record(&damaged, seed(5)).is_none(), "byte {at}");
        }
        assert!(decode_record(&out[..RECORD_HEADER + 3], seed(5)).is_none(), "cut short");
        assert!(decode_record(&[0; 64], seed(0)).is_none(), "the zeros past the last record");
    }

    #[test]
    fn a_commit_is_thirty_six_bytes_and_fifty_six_as_a_record() {
        let commit = Commit { txn: 1, commit_ts: 2, dep_ts: 3, records: 4, bytes: 5 };
        assert_eq!(Commit::decode(&commit.encode()), Some(commit));
        assert_eq!(record_bytes(Commit::LEN), 56);
        assert_eq!(Commit::decode(&[0; 35]), None);
    }

    #[test]
    fn every_kind_is_its_own_byte() {
        for byte in 0..=255 {
            if let Some(kind) = Kind::of(byte) {
                assert_eq!(kind.byte(), byte);
            }
        }
        assert_eq!(Kind::of(0), None);
    }
}
