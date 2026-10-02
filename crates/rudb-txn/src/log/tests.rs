//! The lane and replay together, on the simulated file system.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use rudb_io::{Crash, Filesystem, Op, OpenMode, SimFilesystem};

use super::{
    Block, CommitSync, Committed, Kind, Lane, Options, SEGMENT_HEADER, SegmentHeader, replay,
    segment_name, spares,
};

const DATABASE: u64 = 0x5EED;

fn dir() -> PathBuf {
    PathBuf::from("/db.wal")
}

/// Small segments so a few blocks cross one.
fn options(commit_sync: CommitSync) -> Options {
    Options {
        lane: 0,
        database: DATABASE,
        segment_bytes: 2 * SEGMENT_HEADER as u64,
        commit_sync,
        spare_ahead: false,
    }
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
        && read.records.iter().map(|r| r.payload.to_vec()).collect::<Vec<_>>() == expected
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
fn blocks_queued_before_a_wait_share_its_one_sync() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, Options { segment_bytes: 1 << 16, ..options(CommitSync::Full) });
    let before = lane.stats().syncs;
    let ends: Vec<u64> = (0..5).map(|n| lane.enqueue(&block(n)).expect("queued")).collect();
    assert_eq!(lane.durable(), 0, "nothing is written until somebody waits");
    lane.settle(ends[4], CommitSync::Full).expect("settles");
    assert_eq!(lane.stats().syncs - before, 1, "one sync covers every queued block");
    for &end in &ends {
        assert_eq!(lane.settle(end, CommitSync::Full).expect("already durable"), end);
    }
    assert_eq!(lane.stats().syncs - before, 1, "a block already durable waits for nothing");
    assert_eq!(txns(&sim), (0..5).collect::<Vec<_>>());
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

/// A lane with blocks `0..100` committed over several segments.
fn filled(sim: &SimFilesystem) -> Lane {
    let lane = open(sim, options(CommitSync::Full));
    for n in 0..100 {
        lane.commit(&block(n)).expect("commit");
    }
    lane
}

#[test]
fn a_checkpoint_retires_segments_the_lane_then_recycles() {
    let sim = SimFilesystem::new();
    let lane = filled(&sim);
    lane.retire(lane.position().0).expect("retire");
    // Only the segment being written is left, and two of the retired ones wait as spares.
    let kept = txns(&sim);
    assert!(!kept.is_empty() && kept.len() < 50, "{kept:?}");
    assert_eq!(spares(&sim, &dir(), 0).expect("spares").len(), 2);
    let before = lane.stats();
    for n in 100..200 {
        lane.commit(&block(n)).expect("commit");
    }
    let after = lane.stats();
    assert!(after.segments - before.segments > 2, "two hundred blocks cross segments");
    assert_eq!(after.recycled, 2, "the spares are used before a segment is filled with zeros");
    assert!(spares(&sim, &dir(), 0).expect("spares").is_empty());
    // The retired blocks are gone, and nothing the recycled segments held comes back.
    assert_eq!(txns(&sim), (kept[0]..200).collect::<Vec<_>>());
}

#[test]
fn a_crash_anywhere_in_retiring_and_recycling_leaves_a_log_that_replays_and_reopens() {
    let mut index = 0;
    loop {
        let sim = SimFilesystem::new();
        let lane = filled(&sim);
        // The blocks of the segment being written, which a retire keeps. The ones before it are
        // behind the checkpoint's cut, so a crash may leave any of them and replay skips them.
        let writing = lane.position().0;
        let cut = replay(&sim, &dir(), 0, DATABASE)
            .expect("replay")
            .blocks
            .iter()
            .find(|b| b.sequence == writing)
            .expect("a block in the open segment")
            .commit
            .txn;
        sim.clear_log();
        sim.fail_at(index);
        let mut acked = None;
        let finished = lane.retire(lane.position().0).is_ok()
            && (100..160).all(|n| {
                let ok = lane.commit(&block(n)).is_ok();
                if ok {
                    acked = Some(n);
                }
                ok
            });
        sim.clear_failure();
        if finished {
            assert!(index > 0, "the run did something");
            break;
        }
        drop(lane);
        let pending: Vec<u64> = sim.pending().into_iter().map(|(seq, _)| seq).collect();
        let mut crashes = vec![Crash::LosingUnsynced, Crash::KeepingEverything];
        if pending.len() <= 6 {
            for mask in 0..1_u32 << pending.len() {
                crashes.push(Crash::Keeping(
                    pending
                        .iter()
                        .enumerate()
                        .filter(|(bit, _)| mask & (1 << bit) != 0)
                        .map(|(_, &seq)| seq)
                        .collect(),
                ));
            }
        }
        for crash in &crashes {
            let after = sim.crash(crash);
            let read = txns(&after);
            let (behind, kept): (Vec<u64>, Vec<u64>) = read.iter().partition(|&&n| n < cut);
            assert!(behind.is_sorted(), "op {index}: {crash:?}");
            assert_eq!(kept, (cut..cut + kept.len() as u64).collect::<Vec<_>>(), "op {index}");
            assert!(kept.last() >= acked.as_ref().max(Some(&99)), "op {index}: {crash:?}");
            // The next run adopts whatever spares the crash left and goes on writing.
            let lane = open(&after, options(CommitSync::Full));
            lane.commit(&block(1000)).expect("a commit after the crash");
            let reread = txns(&after);
            assert_eq!(reread[..read.len()], read[..], "op {index}");
            assert_eq!(reread.last(), Some(&1000), "op {index}");
        }
        index += 1;
    }
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

/// Waits for the lane's thread to have made `count` spares in all, which on the simulated file
/// system takes well under a second.
fn made(lane: &Lane, count: u64) {
    let start = std::time::Instant::now();
    while lane.stats().made < count {
        assert!(start.elapsed().as_secs() < 20, "the lane made {} spares", lane.stats().made);
        thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn a_lane_makes_spares_ahead_and_starts_its_segments_from_them() {
    let sim = SimFilesystem::new();
    let options = Options { spare_ahead: true, ..options(CommitSync::Full) };
    let lane = open(&sim, options);
    made(&lane, 2);
    assert_eq!(spares(&sim, &dir(), 0).expect("spares").len(), 2);
    for n in 0..200 {
        lane.commit(&block(n)).expect("commit");
    }
    let stats = lane.stats();
    assert!(stats.segments > 3, "two hundred blocks cross segments, {stats:?}");
    assert!(stats.recycled >= 2, "the spares made ahead are used, {stats:?}");
    lane.stop();
    assert_eq!(txns(&sim), (0..200).collect::<Vec<_>>());
    // Every spare left is whole, and named apart from every segment that was ever written.
    let left = spares(&sim, &dir(), 0).expect("spares");
    assert!(left.len() <= 2, "{left:?}");
    for spare in &left {
        let size = sim.open(spare, OpenMode::Read).expect("a spare").len().expect("its size");
        assert_eq!(size, 2 * SEGMENT_HEADER as u64);
    }
}

#[test]
fn a_reopened_lane_takes_up_the_spares_it_made_and_names_new_ones_apart() {
    let sim = SimFilesystem::new();
    let options = Options { spare_ahead: true, ..options(CommitSync::Full) };
    let lane = open(&sim, options);
    made(&lane, 2);
    drop(lane);
    let before = spares(&sim, &dir(), 0).expect("spares");
    assert_eq!(before.len(), 2);
    let lane = open(&sim, options);
    // The reopened lane started its segment from one of them and makes one more.
    made(&lane, 1);
    lane.stop();
    assert_eq!(lane.stats().recycled, 1);
    let after = spares(&sim, &dir(), 0).expect("spares");
    assert_eq!(after.len(), 2);
    assert!(after.iter().filter(|spare| before.contains(spare)).count() == 1, "{after:?}");
}

#[test]
fn a_stopped_lane_creates_nothing_more() {
    let sim = SimFilesystem::new();
    let lane = open(&sim, Options { spare_ahead: true, ..options(CommitSync::Full) });
    lane.stop();
    let files = sim.read_dir(&dir()).expect("the directory");
    thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(sim.read_dir(&dir()).expect("the directory"), files);
}
