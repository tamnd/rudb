//! A table that is already in the file, with rows appended to it since.
//!
//! Every table is a committed table the next time its file is opened, so an engine that can only
//! append to a table it built in this process can be written to exactly once in its life. That is
//! not a corner: it is the second run of every session anybody has, and it is what
//! `Rows::Grown` exists for. The file stays the file, the rows that arrived since are a table in
//! memory beside it, and the next checkpoint folds the two back into one generation.
//!
//! What these tests hold down is that both halves are read. A scan that only reads the file loses
//! the rows somebody just inserted, a scan that only reads memory loses everything that was there
//! before, and both of those look like a working database right up until somebody counts.

use rudb::Database;
use rudb_catalog::table::Rows;

/// A path nothing else in this file is using, with no database at it yet.
fn scratch(tag: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-grown-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

/// A file with `t` in it, written and committed by a database that is gone by the time this
/// returns, so that whoever opens it next is opening a table they did not build.
fn written(path: &std::path::Path) -> String {
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database.execute("CREATE TABLE t (a INTEGER, b VARCHAR)").expect("creates");
    database.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')").expect("inserts");
    database.execute("CHECKPOINT").expect("commits");
    name
}

#[test]
fn a_table_that_is_already_in_the_file_takes_an_insert() {
    // The report in #1228, at the size somebody hits it. Two runs of the same session, and the
    // second one puts a row into a table the first one wrote down.
    let path = scratch("insert");
    let name = written(&path);

    let second = Database::open(&name).expect("the written file opens");
    second.execute("INSERT INTO t VALUES (3, 'three')").expect("a committed table takes a row");
    assert_eq!(
        second.value("SELECT sum(a) FROM t").expect("reads").to_string(),
        "6",
        "the sum saw one half of the table and not the other"
    );
    assert_eq!(second.value("SELECT count(*) FROM t").expect("reads").to_string(), "3");
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_scan_over_a_grown_table_reads_the_file_and_the_rows_since() {
    // Count and sum go through the parts without caring much what is in them. These are the reads
    // that do care: a projection of one column, a filter that lands on each half in turn, and an
    // ordering that has to interleave the two rather than putting one after the other.
    let path = scratch("scan");
    let name = written(&path);

    let second = Database::open(&name).expect("the written file opens");
    second.execute("INSERT INTO t VALUES (0, 'zero'), (3, 'three')").expect("inserts");

    let rows = second.query("SELECT b FROM t ORDER BY a").expect("reads");
    let ordered: Vec<String> = rows.rows().map(|row| row[0].to_string()).collect();
    assert_eq!(ordered, ["zero", "one", "two", "three"], "the two halves did not interleave");

    assert_eq!(
        second.value("SELECT count(*) FROM t WHERE a < 2").expect("reads").to_string(),
        "2",
        "a filter that matches a row in each half found them"
    );
    assert_eq!(
        second.value("SELECT b FROM t WHERE a = 1").expect("reads").to_string(),
        "one",
        "a filter that matches only in the file found it"
    );
    assert_eq!(
        second.value("SELECT b FROM t WHERE a = 3").expect("reads").to_string(),
        "three",
        "a filter that matches only in the rows since found it"
    );
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_checkpoint_folds_a_grown_table_back_into_one_file() {
    // A grown table is a table the file and the catalog disagree about, so the checkpoint has to
    // write it again rather than deciding there is nothing to do. If it decides wrong, the rows
    // that were in memory are gone when the process ends and nothing says so.
    let path = scratch("fold");
    let name = written(&path);

    let second = Database::open(&name).expect("the written file opens");
    second.execute("INSERT INTO t VALUES (3, 'three')").expect("inserts");
    second.execute("CHECKPOINT").expect("commits the grown table");
    drop(second);

    let third = Database::open(&name).expect("the file opens a third time");
    assert_eq!(third.value("SELECT sum(a) FROM t").expect("reads").to_string(), "6");
    third.with_catalog(|catalog| {
        let table = catalog.resolve(&["t"]).expect("resolves");
        let rows = catalog.table(&table).expect("the table is there").rows();
        assert!(
            matches!(rows, Rows::Native(_)),
            "the folded table came back as something other than one file"
        );
    });
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_run_that_inserts_more_than_once_keeps_every_row() {
    // The corpus shape. A sqllogictest file creates a table and then inserts into it a statement at
    // a time, so the second insert of a run is going into a table that already has rows in memory
    // beside the file, and the third goes into the same one again.
    let path = scratch("many");
    let name = written(&path);

    let second = Database::open(&name).expect("the written file opens");
    for value in 3..=10 {
        second
            .execute(&format!("INSERT INTO t VALUES ({value}, 'more')"))
            .unwrap_or_else(|error| panic!("insert of {value} failed: {error}"));
    }
    assert_eq!(second.value("SELECT count(*) FROM t").expect("reads").to_string(), "10");
    assert_eq!(second.value("SELECT sum(a) FROM t").expect("reads").to_string(), "55");
    assert_eq!(second.value("SELECT max(a) FROM t").expect("reads").to_string(), "10");
    assert_eq!(second.value("SELECT min(a) FROM t").expect("reads").to_string(), "1");

    // And it survives being written down, which is the part a user notices.
    second.execute("CHECKPOINT").expect("commits");
    drop(second);
    let third = Database::open(&name).expect("the file opens again");
    assert_eq!(third.value("SELECT sum(a) FROM t").expect("reads").to_string(), "55");
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_string_group_over_a_grown_table_counts_the_rows_since() {
    // The file's string codes cover the file and not what arrived since, which has none. The
    // counts that group by code, one key or several, and the distinct count that counts codes, all
    // used to take the file's codes for the whole table and lose or double the rows since.
    let path = scratch("codes");
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let setup = [
        "CREATE TABLE t (u BIGINT, v BIGINT, s VARCHAR)",
        "INSERT INTO t SELECT i % 50, i % 7, 's' || (i % 300)::VARCHAR FROM range(200000) r(i)",
    ];
    let changes = [
        "INSERT INTO t SELECT 3, 1, 'new' || (i % 5)::VARCHAR FROM range(30000) r(i)",
        "INSERT INTO t SELECT 4, 2, 's1' FROM range(30000)",
    ];
    let memory = Database::new();
    for sql in setup.iter().chain(&changes) {
        memory.execute(sql).expect("the memory table is made");
    }
    {
        let first = Database::open(&name).expect("a file name starts a native database");
        for sql in setup {
            first.execute(sql).expect("the file table is made");
        }
        first.execute("CHECKPOINT").expect("commits");
    }
    let second = Database::open(&name).expect("the written file opens");
    for sql in changes {
        second.execute(sql).expect("a committed table takes rows");
    }
    for query in [
        "SELECT s, count(*) AS c FROM t GROUP BY s ORDER BY c DESC, s LIMIT 5",
        "SELECT u, s, count(*) AS c FROM t GROUP BY u, s ORDER BY c DESC, s LIMIT 3",
        "SELECT u, v, s, count(*) AS c FROM t GROUP BY u, v, s ORDER BY c DESC, s LIMIT 3",
        "SELECT count(*) FROM (SELECT s, count(*) FROM t GROUP BY s)",
        "SELECT count(DISTINCT s) FROM t",
    ] {
        let wanted = memory.query(query).expect("reads").rows().collect::<Vec<_>>();
        for threads in [1, 4] {
            second.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            let got = second.query(query).expect("reads").rows().collect::<Vec<_>>();
            assert_eq!(got, wanted, "{threads} threads: {query}");
        }
    }
    drop(second);
    std::fs::remove_file(path).expect("removes the temporary database");
}
