//! A delete from a table that lives in a file marks its rows gone beside the file rather than
//! reading every row it keeps into memory, and every query over the table still answers as if
//! the rows had been taken out.
//!
//! Each test runs the same statements on a file and on a database in memory, which keeps its rows
//! the ordinary way, and compares the answers.

use std::path::{Path, PathBuf};

use rudb::Database;
use rudb_catalog::table::Rows;

fn path(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-gone-{tag}-{}.rudb", std::process::id()));
    remove(&path);
    path
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = std::fs::remove_dir_all(PathBuf::from(wal));
}

fn open(path: &Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 path")).expect("the database opens")
}

const LOAD: &str = "CREATE TABLE t AS SELECT range AS id, (range % 7)::INTEGER AS k, \
                    'name ' || (range % 13) AS s, CASE WHEN range % 11 = 0 THEN NULL ELSE range * 3 END AS v \
                    FROM range(100000)";

/// Queries that between them reach a scan, a filter a part's bounds can answer, a group on a
/// string column, the null count, a sum, a late fetch of rows by number, and a lookup of one row.
const QUERIES: &[&str] = &[
    "SELECT count(*) FROM t",
    "SELECT count(v), sum(v), min(id), max(id) FROM t",
    "SELECT count(*) FROM t WHERE v IS NULL",
    "SELECT s, count(*), sum(id) FROM t GROUP BY s ORDER BY s",
    "SELECT k, count(DISTINCT s) FROM t GROUP BY k ORDER BY k",
    "SELECT count(DISTINCT k), count(DISTINCT id) FROM t",
    "SELECT * FROM t WHERE id BETWEEN 50000 AND 50020 ORDER BY id",
    "SELECT * FROM t ORDER BY v DESC NULLS LAST, id LIMIT 7",
    "SELECT * FROM t WHERE s = 'name 3' ORDER BY id LIMIT 5",
    "SELECT * FROM t WHERE s LIKE '%e 1%' ORDER BY id DESC LIMIT 5",
    "SELECT id FROM t WHERE id = 99999",
    "SELECT * FROM t ORDER BY id LIMIT 3 OFFSET 777",
];

fn answers(db: &Database) -> Vec<String> {
    QUERIES
        .iter()
        .map(|sql| {
            let result = db.query(sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
            let rows = result.rows().map(|row| format!("{row:?}")).collect::<Vec<_>>();
            format!("{sql}\n{}", rows.join("\n"))
        })
        .collect()
}

fn same(file: &Database, memory: &Database, when: &str) {
    let (got, want) = (answers(file), answers(memory));
    for (got, want) in got.iter().zip(&want) {
        assert_eq!(got, want, "{when}");
    }
}

fn both(file: &Database, memory: &Database, sql: &str) {
    file.execute(sql).unwrap_or_else(|error| panic!("{sql} on the file: {error}"));
    memory.execute(sql).unwrap_or_else(|error| panic!("{sql} in memory: {error}"));
}

fn masked(db: &Database) -> bool {
    db.with_catalog(|catalog| {
        let name = catalog.resolve(&["t"]).expect("resolves");
        matches!(catalog.table(&name).expect("the table is there").rows(), Rows::Masked(..))
    })
}

/// The sum and non-null count of `v` as the table answers them without reading its rows, which
/// for a table with rows gone is the file's sum less the one written down with those rows.
fn summed(db: &Database) -> Option<(i128, u64)> {
    db.with_catalog(|catalog| {
        let name = catalog.resolve(&["t"]).expect("resolves");
        catalog.table(&name).expect("the table is there").rows().exact_sum(3).expect("sums")
    })
}

fn loaded(tag: &str) -> (PathBuf, Database, Database) {
    let path = path(tag);
    let file = open(&path);
    let memory = Database::new();
    both(&file, &memory, LOAD);
    file.execute("CHECKPOINT").expect("checkpoints");
    (path, file, memory)
}

#[test]
fn a_delete_from_a_file_marks_the_rows_and_reads_as_if_they_were_gone() {
    let (path, file, memory) = loaded("reads");
    both(&file, &memory, "DELETE FROM t WHERE id % 3 = 0");
    assert!(masked(&file), "the delete read the table into memory");
    same(&file, &memory, "after one delete");
    both(&file, &memory, "DELETE FROM t WHERE id BETWEEN 20000 AND 60000 OR s = 'name 4'");
    assert!(masked(&file), "the second delete read the table into memory");
    same(&file, &memory, "after two deletes");
    both(&file, &memory, "DELETE FROM t WHERE id = 99999");
    same(&file, &memory, "after the last row went");
    drop(file);
    remove(&path);
}

#[test]
fn rows_gone_from_a_file_stay_gone_through_a_checkpoint_and_a_reopen() {
    let (path, file, memory) = loaded("reopen");
    both(&file, &memory, "DELETE FROM t WHERE k = 2 OR id < 100");
    file.execute("CHECKPOINT").expect("checkpoints");
    same(&file, &memory, "after the checkpoint");
    drop(file);
    let file = open(&path);
    same(&file, &memory, "after the reopen");
    drop(file);
    remove(&path);
}

#[test]
fn a_checkpoint_after_a_delete_writes_the_rows_gone_and_not_the_rows_kept() {
    let (path, file, memory) = loaded("marks");
    let before = std::fs::metadata(&path).expect("the file is there").len();
    both(&file, &memory, "DELETE FROM t WHERE id % 10 = 3 OR id BETWEEN 40000 AND 41000");
    file.execute("CHECKPOINT").expect("checkpoints");
    let grown = std::fs::metadata(&path).expect("the file is there").len() - before;
    // A bit for each row of every part that lost one, which is every part here, and a catalog.
    assert!(grown < before / 20, "the checkpoint wrote {grown} bytes over a file of {before}");
    assert!(masked(&file), "the checkpoint read the table back into memory");
    same(&file, &memory, "after the checkpoint");
    drop(file);
    let file = open(&path);
    assert!(masked(&file), "the file did not say which rows were gone");
    same(&file, &memory, "after the reopen");
    let want = memory.query("SELECT sum(v)::HUGEINT, count(v) FROM t").expect("sums");
    let want = format!("{:?}", want.rows().next().expect("one row"));
    let got = summed(&file).expect("the sum of what is left is known without a scan");
    assert_eq!(
        want,
        format!("{:?}", [rudb::Value::HugeInt(got.0), rudb::Value::BigInt(got.1 as i64)])
    );
    both(&file, &memory, "DELETE FROM t WHERE k = 5");
    file.execute("CHECKPOINT").expect("checkpoints");
    drop(file);
    let file = open(&path);
    assert!(masked(&file), "the second delete was not written down beside the file");
    same(&file, &memory, "after a second delete and a reopen");
    assert!(summed(&file).is_some(), "the second record did not count what its rows held");
    both(&file, &memory, "INSERT INTO t VALUES (300000, 1, 'late', 1)");
    file.execute("CHECKPOINT").expect("checkpoints");
    drop(file);
    let file = open(&path);
    same(&file, &memory, "after an insert into a table with rows gone and a reopen");
    drop(file);
    remove(&path);
}

#[test]
fn rows_gone_from_a_file_come_back_when_the_transaction_rolls_back() {
    let (path, file, memory) = loaded("rollback");
    let connection = file.connect();
    connection.execute("BEGIN").expect("begins");
    connection.execute("DELETE FROM t WHERE k = 1").expect("deletes");
    assert_eq!(
        connection.value("SELECT count(*) FROM t WHERE k = 1").expect("counts").to_string(),
        "0"
    );
    connection.execute("ROLLBACK").expect("rolls back");
    same(&file, &memory, "after the rollback");
    drop(connection);
    drop(file);
    remove(&path);
}

#[test]
fn a_table_with_rows_gone_takes_inserts_updates_and_more_deletes() {
    let (path, file, memory) = loaded("writes");
    both(&file, &memory, "DELETE FROM t WHERE id % 5 = 1");
    both(&file, &memory, "INSERT INTO t VALUES (200000, 3, 'new', 9), (200001, 4, NULL, NULL)");
    same(&file, &memory, "after an insert");
    both(&file, &memory, "DELETE FROM t WHERE id % 5 = 2");
    both(&file, &memory, "UPDATE t SET v = v + 1 WHERE k = 6");
    same(&file, &memory, "after an update");
    file.execute("CHECKPOINT").expect("checkpoints");
    both(&file, &memory, "DELETE FROM t WHERE id % 5 = 3");
    both(&file, &memory, "UPDATE t SET s = 'changed' WHERE id % 1000 = 4");
    same(&file, &memory, "after an update of a table with rows gone");
    file.execute("CHECKPOINT").expect("checkpoints");
    drop(file);
    let file = open(&path);
    same(&file, &memory, "after the reopen");
    both(&file, &memory, "DELETE FROM t");
    same(&file, &memory, "after every row went");
    drop(file);
    remove(&path);
}

#[test]
fn a_delete_that_returns_its_rows_or_is_logged_still_marks_them() {
    let (path, file, memory) = loaded("returning");
    let got = file.execute("DELETE FROM t WHERE id IN (5, 6, 7) RETURNING id, s").expect("deletes");
    let got = got.rows().map(|row| format!("{row:?}")).collect::<Vec<_>>();
    let want =
        memory.execute("DELETE FROM t WHERE id IN (5, 6, 7) RETURNING id, s").expect("deletes");
    let want = want.rows().map(|row| format!("{row:?}")).collect::<Vec<_>>();
    assert_eq!(got, want);
    assert!(masked(&file), "the delete read the table into memory");
    same(&file, &memory, "after a delete with a returning list");
    drop(file);
    // The log has the delete and the file does not, so this is replay putting it back.
    let file = open(&path);
    same(&file, &memory, "after the log was replayed");
    drop(file);
    remove(&path);
}
