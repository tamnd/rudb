//! The embedding API: opening a database, connecting to it and sharing it between threads.
//!
//! These are tests about the shape of the API rather than about what a query answers. What they are
//! guarding is that one database can be reached from more than one place at once, because that is
//! the thing a program embedding a database needs and the thing that is painful to add afterwards.

use std::thread;

use rudb::Database;
use rudb_common::{Field, LogicalType, Value};

#[test]
fn a_database_opens_by_name_and_two_spellings_mean_memory() {
    assert!(Database::open(":memory:").is_ok());
    assert!(Database::open("").is_ok(), "the empty name is the other way to say it");
}

#[test]
fn a_file_name_opens_a_native_database() {
    let path = std::env::temp_dir().join(format!("rudb-api-open-{}.rudb", std::process::id()));
    let database = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("a file name starts a native database");
    database.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    database.execute("INSERT INTO t VALUES (1), (2), (3)").expect("inserts");
    database.execute("CHECKPOINT").expect("commits");
    drop(database);
    let reopened = Database::open(path.to_str().expect("a UTF-8 temporary path"))
        .expect("the native database reopens");
    assert_eq!(reopened.value("SELECT sum(a) FROM t").expect("reads"), Value::HugeInt(6));
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn several_tables_go_into_one_file_and_come_back_out_of_it() {
    let path = std::env::temp_dir().join(format!("rudb-api-many-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database.execute("CREATE TABLE a (x INTEGER, s VARCHAR)").expect("creates");
    database.execute("INSERT INTO a VALUES (1, 'one'), (2, 'two')").expect("inserts");
    database.execute("CREATE TABLE b (y BIGINT)").expect("creates the second");
    database.execute("INSERT INTO b VALUES (10), (20), (30)").expect("inserts");
    database.execute("CHECKPOINT").expect("commits both tables at once");
    drop(database);

    let reopened = Database::open(&name).expect("the native database reopens");
    assert_eq!(reopened.value("SELECT sum(x) FROM a").expect("reads"), Value::HugeInt(3));
    assert_eq!(reopened.value("SELECT sum(y) FROM b").expect("reads"), Value::HugeInt(60));
    // Across the two, so that this is a database of two tables rather than two databases that
    // happen to share a file.
    assert_eq!(
        reopened
            .value("SELECT count(*) FROM a JOIN b ON b.y = a.x * 10")
            .expect("joins across the two"),
        Value::BigInt(2)
    );
    // A third table lands in the same file beside the two that are already committed.
    reopened.execute("CREATE TABLE c (z INTEGER)").expect("creates the third");
    reopened.execute("INSERT INTO c VALUES (5)").expect("inserts");
    reopened.execute("CHECKPOINT").expect("commits three");
    drop(reopened);

    let again = Database::open(&name).expect("the native database reopens again");
    assert_eq!(again.value("SELECT sum(z) FROM c").expect("reads"), Value::HugeInt(5));
    assert_eq!(
        again.value("SELECT sum(x) FROM a").expect("the first is still there"),
        Value::HugeInt(3)
    );
    // A checkpoint over a catalog where nothing changed does nothing, rather than rewriting the
    // file or refusing because the tables are already in it.
    again.execute("CHECKPOINT").expect("a second checkpoint is not an error");
    assert_eq!(again.value("SELECT sum(y) FROM b").expect("reads"), Value::HugeInt(60));
    drop(again);
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// A table already in the file is carried forward rather than written again.
///
/// The observable part of an append is what it does not do, so this measures it. A table is
/// committed, and then a one row table is added to a file holding a lot of rows and to a file
/// holding few. A checkpoint that rewrote the file would take time in proportion to what was
/// already in it; one that appends takes the same time either way.
///
/// The bound is loose on purpose, because a test that pins a ratio on a shared machine is a test
/// that fails for reasons that are nobody's fault. What a rewrite costs here is two orders of
/// magnitude, so ten times is a gap a rewrite cannot fit through and scheduling noise cannot open.
#[test]
fn a_committed_table_is_carried_forward_and_not_written_again() {
    fn add_one_row_to_a_file_of(rows: u64) -> std::time::Duration {
        let path = std::env::temp_dir()
            .join(format!("rudb-api-append-{rows}-{}.rudb", std::process::id()));
        let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
        let database = Database::open(&name).expect("a file name starts a native database");
        database
            .execute(&format!(
                "CREATE TABLE big AS SELECT i AS a, i * 2 AS b FROM range(0, {rows}) t(i)"
            ))
            .expect("creates");
        database.execute("CHECKPOINT").expect("commits");
        database.execute("CREATE TABLE one AS SELECT 1 AS a").expect("creates the second");
        let start = std::time::Instant::now();
        database.execute("CHECKPOINT").expect("commits the second");
        let taken = start.elapsed();
        // Both are there and both are right, so the cheap checkpoint is an append and not a write
        // that was skipped.
        assert_eq!(
            database.value("SELECT count(*) FROM big").expect("reads"),
            Value::BigInt(rows as i64)
        );
        assert_eq!(database.value("SELECT sum(a) FROM one").expect("reads"), Value::HugeInt(1));
        drop(database);
        let reopened = Database::open(&name).expect("the native database reopens");
        assert_eq!(
            reopened.value("SELECT count(*) FROM big").expect("reads after reopening"),
            Value::BigInt(rows as i64)
        );
        drop(reopened);
        std::fs::remove_file(path).expect("removes the temporary database");
        taken
    }

    let small = add_one_row_to_a_file_of(1_000);
    let large = add_one_row_to_a_file_of(2_000_000);
    assert!(
        large < small.max(std::time::Duration::from_millis(50)) * 10,
        "adding one row to a file of two million took {large:?} against {small:?} for a file of a \
         thousand, which is the whole file being written again"
    );
}

/// Several appends in a row, which is what alternating the two header slots is for.
///
/// One append writes the slot the committed generation did not use. The next one has to write the
/// first slot again, and a reader has to keep picking the higher generation rather than the lower
/// one that is still sitting in the header beside it. Five tables is enough to go round twice.
#[test]
fn a_file_takes_one_table_after_another_and_reads_back_every_one() {
    let path = std::env::temp_dir().join(format!("rudb-api-slots-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    for table in 1..=5 {
        let database = Database::open(&name).expect("the native database opens");
        database
            .execute(&format!(
                "CREATE TABLE t{table} AS SELECT i AS a, 'row' || i AS s FROM range(0, 100) x(i)"
            ))
            .expect("creates");
        database.execute("CHECKPOINT").expect("commits");
        drop(database);
    }
    let reopened = Database::open(&name).expect("the native database reopens");
    for table in 1..=5 {
        assert_eq!(
            reopened
                .value(&format!("SELECT count(*) FROM t{table}"))
                .unwrap_or_else(|error| panic!("t{table} reads back: {error}")),
            Value::BigInt(100)
        );
        // The strings too, because a global dictionary is built per table per generation and a
        // carried forward table's is the one the generation that wrote it left behind.
        assert_eq!(
            reopened
                .value(&format!("SELECT s FROM t{table} WHERE a = 7"))
                .expect("the varchar reads back"),
            Value::Varchar("row7".into())
        );
    }
    drop(reopened);
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// A dropped table leaves the file, which it can only do by the file being written again.
///
/// Every table left after a drop is still committed, so a checkpoint that asked only whether
/// anything had changed would find nothing and the dropped table would come back at the next open.
#[test]
fn a_dropped_table_is_gone_from_the_file_and_the_rest_are_not() {
    let path = std::env::temp_dir().join(format!("rudb-api-drop-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    for table in ["a", "b", "c"] {
        database
            .execute(&format!("CREATE TABLE {table} AS SELECT {} AS x", table.len()))
            .expect("creates");
        database.execute("CHECKPOINT").expect("commits");
    }
    database.execute("DROP TABLE b").expect("drops");
    database.execute("CHECKPOINT").expect("commits the drop");
    drop(database);

    let reopened = Database::open(&name).expect("the native database reopens");
    assert_eq!(
        reopened.value("SELECT sum(x) FROM a").expect("the first is there"),
        Value::HugeInt(1)
    );
    assert_eq!(
        reopened.value("SELECT sum(x) FROM c").expect("the third is there"),
        Value::HugeInt(1)
    );
    assert!(
        reopened.execute("SELECT * FROM b").is_err(),
        "the dropped table is not in the file the next process opens"
    );
    drop(reopened);
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn two_connections_are_two_views_of_one_database() {
    let database = Database::new();
    let writer = database.connect();
    let reader = database.connect();
    writer.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    writer.execute("INSERT INTO t VALUES (1), (2), (3)").expect("inserts");
    assert_eq!(reader.value("SELECT sum(a) FROM t").expect("reads"), Value::HugeInt(6));
}

#[test]
fn a_cloned_handle_is_the_same_database_and_not_a_copy_of_it() {
    let database = Database::new();
    let second = database.clone();
    database.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    second.execute("INSERT INTO t VALUES (7)").expect("inserts");
    assert_eq!(database.value("SELECT a FROM t").expect("reads"), Value::Integer(7));
}

#[test]
fn the_programmatic_write_path_and_sql_see_the_same_tables() {
    let database = Database::new();
    database.create_table("t", vec![Field::new("x", LogicalType::Integer)]).expect("creates");
    database.append("t", &[vec![Value::Integer(4)]]).expect("appends");
    let connection = database.connect();
    connection.execute("INSERT INTO t VALUES (5)").expect("inserts");
    assert_eq!(database.table_len("t").expect("counts"), 2);
    assert_eq!(connection.value("SELECT sum(x) FROM t").expect("reads"), Value::HugeInt(9));
}

#[test]
fn many_threads_read_one_database_at_once() {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    database.execute("INSERT INTO t SELECT * FROM range(1000)").expect("inserts");
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let connection = database.connect();
            thread::spawn(move || connection.value("SELECT count(*) FROM t").expect("reads"))
        })
        .collect();
    for reader in readers {
        assert_eq!(reader.join().expect("the thread finished"), Value::BigInt(1000));
    }
}

#[test]
fn a_writer_in_another_thread_is_visible_once_it_is_done() {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    let writer = database.connect();
    let worker = thread::spawn(move || {
        for row in 0..100 {
            writer.execute(&format!("INSERT INTO t VALUES ({row})")).expect("inserts");
        }
    });
    worker.join().expect("the thread finished");
    assert_eq!(database.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(100));
}

#[test]
fn the_catalog_is_readable_and_writable_for_the_call_and_no_longer() {
    let database = Database::new();
    database.execute("CREATE TABLE t (a INTEGER)").expect("creates");
    assert!(database.with_catalog(|catalog| catalog.tables().any(|t| t.name().table == "t")));
    database.with_catalog_mut(|catalog| {
        let name = catalog.resolve(&["t"]).expect("resolves");
        catalog.drop_table(&name).expect("drops");
    });
    assert!(database.table_names().is_empty());
}
