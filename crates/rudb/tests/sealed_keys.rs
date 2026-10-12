//! A checkpoint seals the keys of a table with enough rows into a run in the file, and a lookup by
//! key on the table opened again reads the run rather than every key. Each check here runs the same
//! statements on a table in its file, checkpointed and opened again, and on one in memory, and
//! compares what lookups and short ranges by key answer, through deletes, inserts after them and
//! updates.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_common::Value;

const ROWS: i64 = 70_000;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-sealed-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
    path
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("opens")
}

/// The rows covered by each key run in the file at `path`, by table.
fn runs(path: &Path) -> Vec<(String, u64)> {
    let native = rudb_native::Catalog::open(path).expect("opens the file");
    let mut runs = Vec::new();
    for name in native.names().map(str::to_owned).collect::<Vec<_>>() {
        let reader = native.table(&name).expect("opens the table");
        for held in reader.table().sections() {
            if held.kind == *rudb_native::section::KEY_RUNS
                && held.usable(reader.table().generation())
            {
                let head = reader.payload_head(held, 8).expect("reads the head");
                let covered = u64::from_le_bytes(head[..8].try_into().expect("eight bytes"));
                runs.push((name.clone(), covered));
            }
        }
    }
    runs.sort();
    runs
}

fn all(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    db.execute(sql).expect(sql).rows().collect()
}

/// Runs a statement on both.
fn both(filed: &Database, memory: &Database, sql: &str) {
    let left = filed.execute(sql).map(|result| result.rows().collect::<Vec<_>>());
    let right = memory.execute(sql).map(|result| result.rows().collect::<Vec<_>>());
    match (left, right) {
        (Ok(left), Ok(right)) => assert_eq!(left, right, "{sql}"),
        (Err(left), Err(right)) => assert_eq!(left.to_string(), right.to_string(), "{sql}"),
        (left, right) => panic!("{sql}: the file said {left:?} and memory {right:?}"),
    }
}

/// Looks every key of `keys` up through `select` on both and checks they agree, and that the
/// short way agrees with the plan on the file.
fn agree(filed: &Database, memory: &Database, select: &str, keys: &[Vec<Value>]) -> usize {
    let short = filed.prepare(select).expect("prepares");
    let planned = filed.prepare(&format!("{select} LIMIT 1000")).expect("prepares");
    let other = memory.prepare(select).expect("prepares");
    let mut found = 0;
    for values in keys {
        let left = short.execute(values).expect(select).rows().collect::<Vec<_>>();
        let plan = planned.execute(values).expect(select).rows().collect::<Vec<_>>();
        let right = other.execute(values).expect(select).rows().collect::<Vec<_>>();
        assert_eq!(left, plan, "{select} {values:?}");
        assert_eq!(left, right, "{select} {values:?}");
        found += left.len();
    }
    found
}

fn ints(keys: impl IntoIterator<Item = i64>) -> Vec<Vec<Value>> {
    keys.into_iter().map(|key| vec![Value::Integer(key as i32)]).collect()
}

fn texts(keys: impl IntoIterator<Item = String>) -> Vec<Vec<Value>> {
    keys.into_iter().map(|key| vec![Value::Varchar(key)]).collect()
}

/// The keys every check looks up: some there, some never there, some taken out along the way.
fn sample() -> Vec<i64> {
    let mut keys = vec![-3, -1, 0, 1, 2, 3, 4, 5, 3 * ROWS - 3, 3 * ROWS, 3 * ROWS + 3];
    keys.extend((0..400).map(|at| (at * 7919) % (3 * ROWS + 30)));
    keys
}

fn check(filed: &Database, memory: &Database) {
    let keys = sample();
    assert!(agree(filed, memory, "SELECT * FROM t WHERE id = ?", &ints(keys.iter().copied())) > 0);
    let names = keys.iter().map(|key| format!("n{}", key / 3)).collect::<Vec<_>>();
    agree(filed, memory, "SELECT id, v FROM t WHERE name = ?", &texts(names));
    let pairs = keys
        .iter()
        .map(|key| {
            vec![Value::Integer((key % 50) as i32), Value::Varchar(format!("b{}", key / 50))]
        })
        .collect::<Vec<_>>();
    agree(filed, memory, "SELECT * FROM pairs WHERE a = ? AND b = ?", &pairs);
    for bound in [-1, 0, 299, 3001, 3 * ROWS - 10] {
        both(filed, memory, &format!("SELECT * FROM t WHERE id >= {bound} ORDER BY id LIMIT 7"));
        both(
            filed,
            memory,
            &format!("SELECT * FROM t WHERE id < {bound} ORDER BY id DESC LIMIT 7"),
        );
        let name = format!("'n{bound}'");
        both(filed, memory, &format!("SELECT * FROM t WHERE name >= {name} ORDER BY name LIMIT 7"));
    }
}

#[test]
fn a_checkpoint_seals_the_keys_and_a_lookup_reads_them() {
    let path = path("keys");
    let memory = Database::new();
    let mut filed = open(&path);
    let setup = [
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR UNIQUE, v BIGINT)".to_owned(),
        format!(
            "INSERT INTO t SELECT (i * 3)::INTEGER, CASE WHEN i % 1000 = 7 THEN NULL ELSE 'n' || i \
             END, i FROM range({ROWS}) r(i)"
        ),
        "CREATE TABLE pairs (a INTEGER, b VARCHAR, v BIGINT, PRIMARY KEY (a, b))".to_owned(),
        format!(
            "INSERT INTO pairs SELECT (i % 50)::INTEGER, 'b' || (i // 50), i FROM range({ROWS}) r(i)"
        ),
        // Too few rows for a run, so it is found the way it always was.
        "CREATE TABLE small (id INTEGER PRIMARY KEY, v INTEGER)".to_owned(),
        "INSERT INTO small SELECT i::INTEGER, i::INTEGER FROM range(100) r(i)".to_owned(),
    ];
    for sql in &setup {
        both(&filed, &memory, sql);
    }
    filed.execute("CHECKPOINT").expect("checkpoints");
    let sealed = runs(&path);
    let rows = ROWS as u64;
    assert_eq!(
        sealed,
        vec![("pairs".to_owned(), rows), ("t".to_owned(), rows), ("t".to_owned(), rows)]
    );
    drop(filed);
    filed = open(&path);
    check(&filed, &memory);
    both(&filed, &memory, "SELECT * FROM small WHERE id = 42");

    // Rows gone from the file: by key, by a range of keys, and a few at a time.
    for sql in [
        "DELETE FROM t WHERE id = 3",
        "DELETE FROM t WHERE id % 97 = 0",
        "DELETE FROM t WHERE name = 'n500'",
        "DELETE FROM pairs WHERE a = 7 AND b = 'b3'",
        "DELETE FROM pairs WHERE v % 101 = 0",
    ] {
        both(&filed, &memory, sql);
    }
    check(&filed, &memory);
    // Rows after them, one of them under a key taken out.
    for sql in [
        "INSERT INTO t VALUES (3, 'again', -1), (-3, 'n-1', -3)",
        "INSERT INTO t VALUES (97, 'n500', -97)",
        "INSERT INTO pairs VALUES (7, 'b3', -1), (-1, 'b-1', -2)",
    ] {
        both(&filed, &memory, sql);
    }
    check(&filed, &memory);
    // A key taken out after it was found, and found again.
    both(&filed, &memory, "DELETE FROM t WHERE id = 6");
    both(&filed, &memory, "INSERT INTO t VALUES (6, 'six', 6)");
    check(&filed, &memory);

    // The next checkpoint extends the file and keeps the runs, which still cover most of it.
    filed.execute("CHECKPOINT").expect("checkpoints");
    let kept = runs(&path);
    assert!(kept.len() == 3 && kept.iter().all(|(_, covered)| *covered >= rows), "{kept:?}");
    drop(filed);
    filed = open(&path);
    check(&filed, &memory);

    // An update of a column in no key. It reads every row of a table with keys and leaves the table
    // in memory, see `Table::takes_rows`, so it comes after the runs were kept.
    both(&filed, &memory, "UPDATE t SET v = v + 1 WHERE id = 6");
    both(&filed, &memory, "UPDATE t SET v = 0 WHERE id % 1001 = 0");
    check(&filed, &memory);

    // Rows past a quarter of the table, and the run is written again over all of them.
    // Keys no row before used, so it adds every one.
    let more = format!(
        "INSERT INTO t SELECT (i * 3 + 2)::INTEGER, 'm' || i, i FROM range({}) r(i)",
        ROWS / 2
    );
    filed.execute(&more).expect(&more);
    memory.execute(&more).expect(&more);
    both(&filed, &memory, "SELECT count(*) FROM t");
    check(&filed, &memory);
    filed.execute("CHECKPOINT").expect("checkpoints");
    let covered = runs(&path);
    let t = covered.iter().filter(|(name, _)| name == "t").map(|(_, rows)| *rows);
    assert!(t.clone().count() == 2 && t.clone().all(|covered| covered > rows), "{covered:?}");
    drop(filed);
    filed = open(&path);
    check(&filed, &memory);
    let moved = (0..200).map(|at| at * 3 + 2);
    agree(&filed, &memory, "SELECT * FROM t WHERE id = ?", &ints(moved));
    assert_eq!(all(&filed, "SELECT count(*) FROM t"), all(&memory, "SELECT count(*) FROM t"));
    drop(filed);
    let _ = std::fs::remove_file(&path);
}
