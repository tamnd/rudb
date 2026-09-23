//! A load crashed at every call it makes to the filesystem.
//!
//! The writer's file calls go through `rudb-io`, so a load can be run against `SimFilesystem`,
//! which records every write and sync and can be told to fail at any one of them. This runs one
//! small load to learn how many calls it makes, then runs it again once per call with the failure
//! placed there, and each time looks at what a crash at that point leaves on the disk.
//!
//! The requirement is the one the commit protocol promises. A file a crash left behind either has
//! no table in it, whether because it is not there, is short, or has no slot that checksums, or it
//! has the whole table with every row as it was written. There is no third answer where some of
//! the rows are there. And once `finish` has returned, the table is there whatever the crash kept.
//!
//! The reader is still on `std::fs`, so the image a crash leaves is copied out of the simulation
//! into a scratch file on the real disk and opened from there with the ordinary reader.

use std::path::{Path, PathBuf};

use rudb_common::{Field, LogicalType, Result, Value};
use rudb_io::{Crash, Op, SimFilesystem};
use rudb_native::{Reader, STRIPE_PARTS, Writer};
use rudb_vector::{Chunk, Vector};

/// Where the table lives in the simulation.
const PATH: &str = "/db/load.rudb";

/// Rows in one appended part.
const PART_ROWS: usize = 32;

/// Parts in the whole load: two full stripes and the start of a third, so the load flushes more
/// than once and the commit has several stripes behind it.
const PARTS: usize = STRIPE_PARTS * 2 + 3;

fn fields() -> Vec<Field> {
    vec![
        Field::required("id", LogicalType::BigInt),
        Field::new("name", LogicalType::Varchar),
        Field::new("score", LogicalType::Integer),
    ]
}

/// The value of every column of row `row`, which is what the load writes and the check expects.
fn row(row: usize) -> [Value; 3] {
    let name =
        if row % 11 == 0 { Value::Null } else { Value::Varchar(format!("name {}", row % 37)) };
    let score =
        if row % 13 == 0 { Value::Null } else { Value::Integer((row as i32 * 7) % 1000 - 500) };
    [Value::BigInt(row as i64), name, score]
}

fn part(index: usize) -> Chunk {
    let rows = (index * PART_ROWS..(index + 1) * PART_ROWS).map(row).collect::<Vec<_>>();
    let column = |at: usize, ty: LogicalType| {
        let values = rows.iter().map(|one| one[at].clone()).collect::<Vec<_>>();
        Vector::from_values(ty, &values).expect("a column of the part")
    };
    Chunk::new(vec![
        column(0, LogicalType::BigInt),
        column(1, LogicalType::Varchar),
        column(2, LogicalType::Integer),
    ])
    .expect("three columns of the same length")
}

/// Runs the whole load against `fs`, stopping at the first call that fails.
fn load(fs: &SimFilesystem, parts: &[Chunk]) -> Result<()> {
    let mut writer = Writer::create_in(fs, Path::new(PATH), "t", fields())?;
    for chunk in parts {
        writer.append(chunk)?;
    }
    writer.finish().map(|_| ())
}

/// What a crash left: no table, or the whole of it.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    Nothing,
    Complete,
}

/// Opens what `after` holds with the ordinary reader and says whether it is nothing or the whole
/// table, failing the test on anything in between.
fn found(after: &SimFilesystem, scratch: &Path) -> Found {
    let Some(bytes) = after.durable_contents(Path::new(PATH)) else {
        return Found::Nothing;
    };
    std::fs::write(scratch, &bytes).expect("the image goes to the scratch file");
    let Ok(reader) = Reader::open(scratch) else {
        return Found::Nothing;
    };
    let rows = PARTS * PART_ROWS;
    assert_eq!(reader.table().rows(), rows, "a table that opens has every row");
    let mut at = 0;
    for index in 0..reader.parts() {
        let chunk = reader.read(index, &[0, 1, 2]).expect("a committed part reads");
        for one in 0..chunk.len() {
            let want = row(at);
            for (column, value) in want.iter().enumerate() {
                assert_eq!(&chunk.value_at(one, column), value, "row {at} column {column}");
            }
            at += 1;
        }
    }
    assert_eq!(at, rows, "the parts hold every row once");
    Found::Complete
}

/// A few subsets of the unsynced writes: none, all, and some chosen to break a writer that gets
/// the order of a commit wrong.
///
/// Before the first sync of a commit nothing points at the pages, so no subset of them can make a
/// table appear, and the two interleavings are there to show that. The rest are aimed at the end of
/// the list, where the catalog and the slot are. The header with the last two and nothing between,
/// and everything but the second, are what a crash leaves when the slot and what it names land and
/// a page does not, which is the torn commit a missing sync would allow.
fn crashes(fs: &SimFilesystem) -> Vec<Crash> {
    let pending = fs.pending().into_iter().map(|(seq, _)| seq).collect::<Vec<_>>();
    let mut out = vec![Crash::LosingUnsynced, Crash::KeepingEverything];
    if pending.len() > 1 {
        let n = pending.len();
        out.push(Crash::Keeping(pending.iter().copied().step_by(2).collect()));
        out.push(Crash::Keeping(pending.iter().copied().skip(1).step_by(2).collect()));
        out.push(Crash::Keeping(pending[..n - 1].to_vec()));
        out.push(Crash::Keeping(
            pending.iter().copied().take(1).chain(pending[n - 2..].to_vec()).collect(),
        ));
        let mut all_but_second = pending.clone();
        all_but_second.remove(1);
        out.push(Crash::Keeping(all_but_second));
        out.push(Crash::Keeping(vec![pending[n - 1]]));
    }
    out
}

fn scratch(worker: usize) -> PathBuf {
    std::env::temp_dir().join(format!("rudb-native-crash-{}-{worker}.rudb", std::process::id()))
}

/// Fails the load at every call in `indices` and checks what each crash leaves, answering how
/// many of the crashes found the whole table.
fn run(indices: impl Iterator<Item = usize>, ops: &[Op], parts: &[Chunk], scratch: &Path) -> usize {
    let mut complete = 0;
    for index in indices {
        let fs = SimFilesystem::new();
        fs.fail_at(index);
        let outcome = load(&fs, parts);
        assert_eq!(fs.ops()[..=index], ops[..=index], "the load is the same up to the failure");
        // Nothing fails silently: a call the simulation refused has to reach the caller.
        assert!(outcome.is_err(), "the load finished although call {index} failed");
        for crash in crashes(&fs) {
            if found(&fs.crash(&crash), scratch) == Found::Complete {
                complete += 1;
                // The only point a table can be there without `finish` having returned is the
                // last sync, with the slot write it was making durable surviving the crash.
                assert_eq!(index, ops.len() - 1, "a table appeared at call {index}, {crash:?}");
            }
        }
    }
    complete
}

#[test]
fn a_load_crashed_at_any_call_leaves_no_table_or_the_whole_table() {
    let parts = (0..PARTS).map(part).collect::<Vec<_>>();

    // The run with nothing injected, which says how many calls there are to fail.
    let clean = SimFilesystem::new();
    load(&clean, &parts).expect("the load runs to the end with nothing injected");
    let ops = clean.ops();
    let syncs = ops.iter().filter(|op| matches!(op, Op::Sync { .. })).count();
    let writes = ops.iter().filter(|op| matches!(op, Op::Write { .. })).count();
    assert_eq!(syncs, 2, "a commit is two syncs: the pages and catalog, then the slot");
    assert!(writes > PARTS, "every part is its own write, and then the rest");
    assert!(clean.pending().is_empty(), "a finished load has nothing left unsynced");
    let path = scratch(0);
    assert_eq!(found(&clean.crash(&Crash::LosingUnsynced), &path), Found::Complete);
    let _ = std::fs::remove_file(&path);

    // Every crash point is a load of its own and they share nothing, so they are spread over the
    // machine's cores to keep the test quick. Each worker takes every nth point.
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(ops.len());
    let complete = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|worker| {
                let (ops, parts) = (&ops, &parts);
                scope.spawn(move || {
                    let path = scratch(worker);
                    let complete = run((worker..ops.len()).step_by(workers), ops, parts, &path);
                    let _ = std::fs::remove_file(path);
                    complete
                })
            })
            .collect::<Vec<_>>();
        handles.into_iter().map(|handle| handle.join().expect("a worker")).sum::<usize>()
    });
    assert!(complete > 0, "a crash on the last sync that keeps the slot write finds the table");
}
