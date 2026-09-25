//! The lane and replay together, on the simulated file system.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use rudb_io::{Crash, Filesystem, Op, OpenMode, SimFilesystem};

use super::{
    Block, CommitSync, Committed, Kind, Lane, Options, SEGMENT_HEADER, SegmentHeader, replay,
    segment_name,
};

const DATABASE: u64 = 0x5EED;

fn dir() -> PathBuf {
    PathBuf::from("/db.wal")
}

/// Small segments so a few blocks cross one.
fn options(commit_sync: CommitSync) -> Options {
    Options { lane: 0, database: DATABASE, segment_bytes: 2 * SEGMENT_HEADER as u64, commit_sync }
}

fn open(sim: &SimFilesystem, options: Options) -> Lane {
    Lane::open(Arc::new(sim.clone()), &dir(), options).expect("the lane opens")
}

/// Block `n`: `n % 4` records with payloads that say which block they belong to.
fn block(n: u64) -> Block {
    let mut block = Block::new(n, 100 + n, 99 + n);
    for record in 0..n % 4 {
        let payload: Vec<u8> = (0..(n * 7 + record) % 90).map(|b| (b ^ n) as u8).collect();
        block.push(Kind::Insert, 0, &payload).expect("a record");
    }
    block
}

/// Whether a block read back is the one written as `block(n)`.
fn matches(read: &Committed, n: u64) -> bool {
    let written = block(n);
    let mut expected = Vec::new();
    for record in 0..n % 4 {
        expected.push((0..(n * 7 + record) % 90).map(|b| (b ^ n) as u8).collect::<Vec<u8>>());
    }
    read.commit.txn == n
        && read.commit.commit_ts == 100 + n
        && read.commit.dep_ts == 99 + n
        && read.records.len() == written.records()
        && read.records.iter().map(|r| r.payload.clone()).collect::<Vec<_>>() == expected
        && read.records.iter().all(|r| r.header.kind == Kind::Insert && r.header.gsn == 100 + n)
}

fn txns(sim: &SimFilesystem) -> Vec<u64> {
    let replayed = replay(sim, &dir(), 0, DATABASE).expect("replay");
    for read in &replayed.blocks {
        assert!(matches(read, read.commit.txn), "block {} reads back as written", read.commit.txn);
    }
    replayed.blocks.iter().map(|read| read.commit.txn).collect()
}

#[test]
fn committed_blocks_read_back_in_order() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    for n in 0..10 {
        lane.commit(&block(n)).expect("commit");
    }
    assert_eq!(txns(&sim), (0..10).collect::<Vec<_>>());
    let stats = lane.stats();
    assert_eq!(stats.commits, 10);
    assert_eq!(lane.durable(), stats.bytes);
}

#[test]
fn a_full_segment_is_synced_before_the_next_one_gets_a_record() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Os));
    for n in 0..60 {
        lane.commit(&block(n)).expect("commit");
    }
    lane.flush().expect("flush");
    assert!(lane.stats().segments > 2, "sixty blocks cross segments of 4 KiB");
    assert_eq!(txns(&sim), (0..60).collect::<Vec<_>>());

    // Every write to a segment's records comes after the last sync of the segment before it.
    let ops = sim.ops();
    let name = |sequence: u64| dir().join(segment_name(0, sequence));
    for sequence in 2..=lane.stats().segments {
        let first_record = ops.iter().position(|op| {
            matches!(op, Op::Write { path, offset, .. } if *path == name(sequence) && *offset >= SEGMENT_HEADER as u64 && !is_fill(op))
        });
        let last_write_before = ops
            .iter()
            .rposition(|op| matches!(op, Op::Write { path, .. } if *path == name(sequence - 1)));
        let last_sync_before = ops
            .iter()
            .rposition(|op| matches!(op, Op::Sync { path } if *path == name(sequence - 1)));
        if let Some(first) = first_record {
            assert!(last_sync_before.expect("synced") > last_write_before.expect("written"));
            assert!(last_sync_before.expect("synced") < first, "segment {sequence}");
        }
    }
}

/// Whether a write is part of creating the segment, which fills it with zeros a MiB at a time
/// and so writes everything past the header in one go at this size.
fn is_fill(op: &Op) -> bool {
    matches!(op, Op::Write { offset, len, .. } if *offset == SEGMENT_HEADER as u64 && *len == SEGMENT_HEADER)
}

#[test]
fn a_reopened_lane_writes_a_new_segment_and_replay_reads_both() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    for n in 0..3 {
        lane.commit(&block(n)).expect("commit");
    }
    drop(lane);
    let lane = open(&sim, options(CommitSync::Full));
    for n in 3..6 {
        lane.commit(&block(n)).expect("commit");
    }
    assert_eq!(txns(&sim), (0..6).collect::<Vec<_>>());
    assert!(sim.exists(&dir().join(segment_name(0, 2))));
}

#[test]
fn a_crash_keeps_every_acknowledged_block_and_no_partial_one() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Os));
    for n in 0..3 {
        lane.commit(&block(n)).expect("commit");
    }
    lane.flush().expect("flush");
    for n in 3..8 {
        lane.commit(&block(n)).expect("commit");
    }
    let pending: Vec<u64> = sim.pending().into_iter().map(|(seq, _)| seq).collect();
    assert!(pending.len() >= 2, "the blocks after the flush are written and not synced");
    for mask in 0..1_u32 << pending.len() {
        let kept: Vec<u64> = pending
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, &seq)| seq)
            .collect();
        let after = sim.crash(&Crash::Keeping(kept));
        let read = txns(&after);
        assert!(read.len() >= 3, "the flushed blocks survive every crash");
        assert_eq!(read, (0..read.len() as u64).collect::<Vec<_>>(), "mask {mask:b}: a prefix");
    }
    assert_eq!(txns(&sim.crash(&Crash::LosingUnsynced)), vec![0, 1, 2]);
    assert_eq!(txns(&sim.crash(&Crash::KeepingEverything)), (0..8).collect::<Vec<_>>());
}

#[test]
fn a_full_commit_survives_a_crash_right_after_it_returns() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    for n in 0..20 {
        lane.commit(&block(n)).expect("commit");
        let after = sim.crash(&Crash::LosingUnsynced);
        assert_eq!(txns(&after), (0..=n).collect::<Vec<_>>());
    }
}

#[test]
fn records_left_in_a_recycled_segment_do_not_replay() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    for n in 0..4 {
        lane.commit(&block(n)).expect("commit");
    }
    drop(lane);
    // Segment 2 is segment 1 renamed, with a sound header for its new sequence and the old
    // records still in it.
    let mut bytes = sim.contents(&dir().join(segment_name(0, 1))).expect("segment 1");
    bytes[..SEGMENT_HEADER]
        .copy_from_slice(&SegmentHeader { lane: 0, sequence: 2, database: DATABASE }.encode());
    let file = sim.open(&dir().join(segment_name(0, 2)), OpenMode::CreateNew).expect("create");
    file.write_at(0, &bytes).expect("write");
    let replayed = replay(&sim, &dir(), 0, DATABASE).expect("replay");
    assert_eq!(replayed.segments, 2);
    assert_eq!(replayed.blocks.iter().map(|b| b.commit.txn).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
}

#[test]
fn a_segment_of_another_database_is_refused() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    lane.commit(&block(1)).expect("commit");
    assert!(replay(&sim, &dir(), 0, DATABASE + 1).is_err());
    assert!(replay(&sim, &dir(), 1, DATABASE).expect("no segments of lane 1").blocks.is_empty());
    assert!(
        replay(&sim, Path::new("/elsewhere"), 0, DATABASE).expect("no directory").blocks.is_empty()
    );
}

#[test]
fn a_failed_write_stops_the_lane_for_good() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    lane.commit(&block(1)).expect("commit");
    sim.fail_at(sim.op_count());
    assert!(lane.commit(&block(2)).is_err());
    sim.clear_failure();
    assert!(lane.commit(&block(3)).is_err(), "a lane that failed does not come back");
    assert!(lane.flush().is_err());
}

#[test]
fn a_block_larger_than_a_segment_is_refused_and_an_empty_one_is_fine() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, options(CommitSync::Full));
    let mut large = Block::new(1, 2, 0);
    large.push(Kind::Insert, 0, &[1; SEGMENT_HEADER]).expect("a record");
    assert!(lane.commit(&large).is_err());
    assert!(Block::new(1, 2, 0).push(Kind::Commit, 0, &[]).is_err());
    lane.commit(&block(0)).expect("a block with only its Commit");
    assert_eq!(txns(&sim), vec![0]);
    assert!(
        Lane::open(
            Arc::new(sim.clone()),
            &dir(),
            Options { segment_bytes: 5000, ..options(CommitSync::Full) }
        )
        .is_err()
    );
}

#[test]
fn committers_on_many_threads_share_syncs() {
    let sim = SimFilesystem::new();
    let lane = Arc::new(
        Lane::open(
            Arc::new(sim.clone()),
            &dir(),
            Options { segment_bytes: 1 << 20, ..options(CommitSync::Full) },
        )
        .expect("the lane opens"),
    );
    let threads: Vec<_> = (0..8_u64)
        .map(|thread| {
            let lane = Arc::clone(&lane);
            thread::spawn(move || {
                for n in 0..50 {
                    lane.commit(&block(thread * 1000 + n)).expect("commit");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().expect("the committer finishes");
    }
    let mut read = txns(&sim.crash(&Crash::LosingUnsynced));
    read.sort_unstable();
    let mut expected: Vec<u64> = (0..8).flat_map(|t| (0..50).map(move |n| t * 1000 + n)).collect();
    expected.sort_unstable();
    assert_eq!(read, expected);
    let stats = lane.stats();
    assert_eq!(stats.commits, 400);
    assert!(stats.syncs <= 400);
}
