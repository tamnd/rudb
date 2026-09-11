//! The Parquet reader under a filesystem that misbehaves.
//!
//! `SimFilesystem` in `rudb-io` can make any read fail, make any read come back short, and hand a
//! submitted batch back in an order nobody asked for. This is that machinery pointed at the reader,
//! which is the test gate `spec/engine/05-scan.md` asks for and the one M2d names.
//!
//! The requirement is not that every fault is survivable. A disk that returns an error in the
//! middle of a column chunk has ended that query and saying so is the right answer. The requirement
//! is the two failures that are not allowed:
//!
//! A wrong answer. A read that comes back short leaves whatever was in the buffer before, so a
//! decoder that trusts the length it asked for reads stale bytes and returns rows. They look like
//! rows. Nothing about them says they came from a disk that did not answer.
//!
//! A hang. The page walk is a loop that advances by however far the last header said, so a fault
//! that leaves it at the same byte twice is a loop that never ends, and a test that runs forever
//! looks the same from the outside as a test that is slow.
//!
//! So every fault below is asserted to produce either an error or the exact answer the same file
//! produces with no faults at all, and every read is bounded so a loop that stops advancing fails
//! rather than hangs.

use std::path::Path;

use rudb_common::Value;
use rudb_io::{Completions, Filesystem, OpenMode, RealFilesystem, SimFilesystem};
use rudb_parquet::Reader;

/// Where the fixture lives in the simulation.
const PATH: &str = "/data/file.parquet";

/// A simulated filesystem holding a copy of one of the committed fixtures.
///
/// The bytes come off the real disk once and go into the simulation, so what is being tested is the
/// reader against a hostile filesystem rather than the fixture against anything.
fn loaded(name: &str) -> SimFilesystem {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name);
    let bytes = read_whole(&path);
    let fs = SimFilesystem::new();
    fs.create_dir_all(Path::new("/data")).expect("the simulation makes a directory");
    let file = fs.open(Path::new(PATH), OpenMode::CreateNew).expect("the simulation makes a file");
    file.write_at(0, &bytes).expect("the write lands");
    file.sync().expect("the write is durable");
    drop(file);
    // Writing the fixture served no reads, so read zero of the run below is the reader's first one.
    fs.clear_read_faults();
    fs
}

/// The fixture's bytes, off the real disk.
fn read_whole(path: &Path) -> Vec<u8> {
    let fs = RealFilesystem::new();
    let file = fs.open(path, OpenMode::Read).expect("the fixture is committed");
    let len = usize::try_from(file.len().expect("the fixture has a size")).expect("it fits");
    let mut bytes = vec![0_u8; len];
    file.read_exact_at(0, &mut bytes).expect("the fixture reads");
    bytes
}

/// Every row of the file, or the error that stopped it.
///
/// The chunk count is bounded because this is the function a hang would hang in. The fixtures are
/// 4096 rows in chunks of at most 2048, so anything past a few dozen chunks is a loop that has
/// stopped making progress, and failing on it turns a hang into a failure with a message.
fn rows(fs: &SimFilesystem) -> rudb_common::Result<Vec<Vec<Value>>> {
    let file = fs.open(Path::new(PATH), OpenMode::Read)?;
    let mut reader = Reader::open(file)?;
    let mut out = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = reader.next_chunk()? {
        chunks += 1;
        assert!(
            chunks < 64,
            "the reader produced {chunks} chunks from a file that holds a handful"
        );
        for at in 0..chunk.len() {
            out.push(chunk.row(at).collect::<Vec<_>>());
        }
    }
    Ok(out)
}

/// The answer the file gives when nothing is wrong, and how many reads it took to get it.
fn baseline(name: &str) -> (Vec<Vec<Value>>, u64) {
    let fs = loaded(name);
    let rows = rows(&fs).expect("the fixture reads with no faults");
    (rows, fs.reads_served())
}

#[test]
fn a_read_that_fails_ends_the_query_and_does_not_answer_it_wrongly() {
    let (good, reads) = baseline("mixed.parquet");
    assert_eq!(good.len(), 4096);
    let mut broke = 0;
    for read in 0..reads {
        let fs = loaded("mixed.parquet");
        fs.fail_read_at(read);
        match rows(&fs) {
            // Surviving a failure is allowed. Answering differently because of one is not.
            Ok(rows) => assert_eq!(rows, good, "read {read} failed and the answer changed"),
            Err(_) => broke += 1,
        }
    }
    // If no injected failure ever reached the reader then this test is asserting nothing, which is
    // a way for a test to pass that is worse than failing.
    assert_eq!(broke, reads, "a failing read went unnoticed by the reader");
}

#[test]
fn a_read_that_comes_back_short_is_never_a_short_answer() {
    // The fault worth the most here. A short read is not an error at the filesystem, so nothing
    // below the reader will say anything about it, and the bytes past the end of what arrived are
    // whatever the buffer held. Those bytes decode. They decode into rows.
    let (good, reads) = baseline("mixed.parquet");
    for read in 0..reads {
        // A read that came back with nothing at all has to be noticed by whoever asked for it,
        // whichever read it was. This one is stated as an error rather than as an or, because a
        // reader that carries on from a buffer it never filled is the failure this file is about.
        let fs = loaded("mixed.parquet");
        fs.short_read_at(read, 0);
        rows(&fs).expect_err(&format!("read {read} returned no bytes and the reader carried on"));

        // The partial ones are an or. A cut longer than the read asked for is not a fault at all,
        // and a reader that recovers from a partial read by asking again is allowed, so what is
        // asserted is that the rows are either absent or right.
        for cut in [1_usize, 7, 64, 1000] {
            let fs = loaded("mixed.parquet");
            fs.short_read_at(read, cut);
            if let Ok(rows) = rows(&fs) {
                assert_eq!(
                    rows, good,
                    "read {read} came back with {cut} bytes and the rows changed"
                );
            }
        }
    }
}

#[test]
fn a_short_read_in_the_middle_of_a_column_chunk_is_caught() {
    // The specific case the guard in the page walk is for, stated as its own test so that removing
    // the guard fails something that names it. The footer reads are first, so a read partway
    // through the run is a column chunk, and half of one is a chunk that ends early.
    let (_, reads) = baseline("mixed.parquet");
    let fs = loaded("mixed.parquet");
    let last = reads - 1;
    fs.short_read_at(last, 200);
    let error = rows(&fs).expect_err("half a column chunk is not a column");
    assert!(!error.message().is_empty());
}

#[test]
fn the_delta_fixtures_survive_the_same_treatment() {
    // The encodings pyarrow wrote, since their decoders read lengths and bit widths out of the page
    // and then index with them, which is where a truncated page turns into a panic if nothing
    // checks.
    for name in ["delta.parquet", "lengths.parquet"] {
        let (good, reads) = baseline(name);
        for read in 0..reads {
            for cut in [0_usize, 3, 100] {
                let fs = loaded(name);
                fs.short_read_at(read, cut);
                if let Ok(rows) = rows(&fs) {
                    assert_eq!(rows, good, "{name}, read {read} cut to {cut}");
                }
            }
            let fs = loaded(name);
            fs.fail_read_at(read);
            if let Ok(rows) = rows(&fs) {
                assert_eq!(rows, good, "{name}, read {read} failed");
            }
        }
    }
}

#[test]
fn completions_arriving_out_of_order_do_not_change_the_answer() {
    // The reader is synchronous today and this still has to hold, because the submission interface
    // is what it moves onto and the order a batch comes back in is not the order it was asked in.
    // A test written now fails on the change that introduces the assumption rather than a release
    // later.
    let (good, _) = baseline("mixed.parquet");
    for order in [Completions::Reversed, Completions::Shuffled(7), Completions::Shuffled(99)] {
        let fs = loaded("mixed.parquet");
        fs.complete(order);
        assert_eq!(rows(&fs).expect("reordering is not a failure"), good, "{order:?}");
    }
}

#[test]
fn a_projection_that_reads_nothing_reads_nothing_even_when_every_read_fails() {
    // `SELECT count(*)` touches the footer and stops, so the row count survives a disk that cannot
    // serve another byte. This is worth holding onto: it is the cheapest query in the benchmark and
    // the one most likely to be asked of a file nobody can read all of.
    let fs = loaded("mixed.parquet");
    let file = fs.open(Path::new(PATH), OpenMode::Read).expect("the file opens");
    let mut reader = Reader::open(file).expect("the footer reads");
    reader.project(&[]).expect("no columns is a projection");
    let before = fs.reads_served();
    for read in before..before + 8 {
        fs.fail_read_at(read);
    }
    let mut rows = 0;
    while let Some(chunk) = reader.next_chunk().expect("counting rows reads no column data") {
        rows += chunk.len();
    }
    assert_eq!(rows, 4096);
    assert_eq!(reader.bytes_read(), 0, "counting rows read column bytes");
    assert_eq!(fs.reads_served(), before, "counting rows went back to the disk");
}
