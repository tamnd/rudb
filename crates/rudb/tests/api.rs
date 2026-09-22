//! The embedding API: opening a database, connecting to it and sharing it between threads.
//!
//! These are tests about the shape of the API rather than about what a query answers. What they are
//! guarding is that one database can be reached from more than one place at once, because that is
//! the thing a program embedding a database needs and the thing that is painful to add afterwards.

use std::thread;

use rudb::{Config, Database};
use rudb_common::{Clustering, Field, LogicalType, Value, Width};

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

/// A load into a database that already has a table goes to the file as it runs.
///
/// The first table into an empty file has always streamed: the rows go from the source into the
/// writer and the table is never held in memory. The second one did not, so loading a table needed
/// room for the whole of it and a `CHECKPOINT` afterwards to get it out of memory again.
///
/// What this can check cheaply is the second half. A table that streamed is in the file the moment
/// the insert returns, so a process that opens the database and never says `CHECKPOINT` still finds
/// it there.
#[test]
fn a_load_beside_a_committed_table_reaches_the_file_without_a_checkpoint() {
    let path = std::env::temp_dir().join(format!("rudb-api-stream-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let first = Database::open(&name).expect("a file name starts a native database");
    first.execute("CREATE TABLE a (x INTEGER)").expect("creates");
    first.execute("INSERT INTO a SELECT * FROM range(100)").expect("inserts");
    drop(first);

    let second = Database::open(&name).expect("the native database reopens");
    second.execute("CREATE TABLE b (y INTEGER, s VARCHAR)").expect("creates the second");
    second
        .execute("INSERT INTO b SELECT i, 'row' || i FROM range(0, 100) t(i)")
        .expect("inserts into the second");
    drop(second);

    let third = Database::open(&name).expect("the native database reopens again");
    assert_eq!(
        third.value("SELECT count(*) FROM b").expect("the second table is in the file"),
        Value::BigInt(100)
    );
    assert_eq!(
        third.value("SELECT s FROM b WHERE y = 7").expect("reads"),
        Value::Varchar("row7".into())
    );
    assert_eq!(
        third.value("SELECT sum(x) FROM a").expect("the first is still there"),
        Value::HugeInt(4950)
    );
    drop(third);
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// `CREATE TABLE AS SELECT` writes the file as it runs, the same as an insert into a fresh table.
///
/// It used to run the whole query into a result, move the result into an in-memory table and wait
/// for a `CHECKPOINT` to reach the file, which meant the statement needed room for the answer twice
/// over and none of it was charged against the memory limit. Both halves are checked here, the
/// first table into an empty file and a second one beside a committed table, because those are the
/// two ways the sink can be reached and the second one goes through the append path.
#[test]
fn a_create_table_as_select_reaches_the_file_without_a_checkpoint() {
    let path = std::env::temp_dir().join(format!("rudb-api-ctas-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let first = Database::open(&name).expect("a file name starts a native database");
    first
        .execute("CREATE TABLE a AS SELECT i AS x, 'row' || i AS s FROM range(0, 100) t(i)")
        .expect("creates and fills");
    drop(first);

    let second = Database::open(&name).expect("the native database reopens");
    assert_eq!(
        second.value("SELECT count(*) FROM a").expect("the first table is in the file"),
        Value::BigInt(100)
    );
    second
        .execute("CREATE TABLE b AS SELECT i AS y FROM range(0, 50) t(i)")
        .expect("creates the second beside the first");
    drop(second);

    let third = Database::open(&name).expect("the native database reopens again");
    assert_eq!(
        third.value("SELECT count(*) FROM b").expect("the second table is in the file"),
        Value::BigInt(50)
    );
    assert_eq!(
        third.value("SELECT s FROM a WHERE x = 7").expect("reads"),
        Value::Varchar("row7".into())
    );
    assert_eq!(
        third.value("SELECT sum(y) FROM b").expect("the second table reads back"),
        Value::HugeInt(1225)
    );
    drop(third);
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// A `CREATE TABLE AS SELECT` that fails leaves no table behind.
///
/// The entry is made after the query rather than before it, so there is no window where the catalog
/// names a table the file does not have. The query below binds and then fails the cast at run time,
/// which is the shape that would have left an empty table behind under the other order.
#[test]
fn a_create_table_as_select_that_fails_leaves_no_table() {
    let path = std::env::temp_dir().join(format!("rudb-api-ctas-err-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database
        .execute("CREATE TABLE t AS SELECT CAST('x' || i AS INTEGER) AS c FROM range(0, 100) r(i)")
        .expect_err("x1 is not an integer");
    database.execute("SELECT count(*) FROM t").expect_err("and the table is not in the catalog");
    database
        .execute("CREATE TABLE t AS SELECT i AS x FROM range(0, 10) r(i)")
        .expect("the name is free");
    assert_eq!(database.value("SELECT count(*) FROM t").expect("reads"), Value::BigInt(10));
    drop(database);
    std::fs::remove_file(path).expect("removes the temporary database");
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

/// The last table can go too, and what is left is a database with nothing in it.
///
/// A file that has to hold at least one table is a file that cannot record the drop, and what that
/// looked like was an error out of the checkpoint and the table still there at the next open, which
/// is a statement that succeeded being undone by the write that was supposed to publish it. DuckDB
/// drops it, keeps the file and opens it again as a database with no tables, and this is that.
#[test]
fn the_last_table_can_be_dropped_and_the_file_says_so() {
    let path = std::env::temp_dir().join(format!("rudb-api-last-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database.execute("CREATE TABLE t AS SELECT 1 AS x").expect("creates");
    database.execute("CHECKPOINT").expect("commits");
    database.execute("DROP TABLE t").expect("drops the only table");
    database.execute("CHECKPOINT").expect("commits a database with nothing in it");
    database.execute("CHECKPOINT").expect("and a second one has nothing to do");
    drop(database);

    assert!(path.exists(), "the file is still there, the way DuckDB leaves it");
    let reopened = Database::open(&name).expect("a database with no tables opens");
    assert!(
        reopened.execute("SELECT * FROM t").is_err(),
        "the dropped table does not come back at the next open"
    );
    // And it is a database rather than a headstone, so the next table goes into it as usual.
    reopened.execute("CREATE TABLE u AS SELECT 2 AS x").expect("creates");
    reopened.execute("CHECKPOINT").expect("commits");
    drop(reopened);

    let again = Database::open(&name).expect("the native database reopens");
    assert_eq!(again.value("SELECT sum(x) FROM u").expect("the new table"), Value::HugeInt(2));
    drop(again);
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// A table created in one run is there in the next one without anybody saying `CHECKPOINT`.
///
/// What a program embedding a database expects of it, and what DuckDB does. Without this every
/// session that forgot the checkpoint threw its work away at the end, which is a database that
/// silently is not one.
#[test]
fn a_database_on_a_file_is_written_when_the_last_handle_goes_away() {
    let path = std::env::temp_dir().join(format!("rudb-api-close-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    // Two tables and then the rows, which is the shape that keeps them in memory until the write
    // below. One table and a load goes straight to the file as it runs, and a test over that would
    // pass whether the database wrote anything on the way out or not.
    database.execute("CREATE TABLE t (x INTEGER)").expect("creates");
    database.execute("CREATE TABLE u (y INTEGER)").expect("creates the second");
    // A second handle and a connection, because the write is the last one going away and not the
    // first. A database written when the first handle drops is one that loses everything the
    // handles still open do after it.
    let second = database.clone();
    let connection = database.connect();
    drop(database);
    connection.execute("INSERT INTO t VALUES (7), (5)").expect("inserts");
    assert!(!path.exists(), "nothing is on the disk yet, because nobody said CHECKPOINT");
    drop(connection);
    assert!(!path.exists(), "a handle going away while others are open writes nothing");
    drop(second);
    assert!(path.exists(), "the last handle going away is what writes the file");

    let reopened = Database::open(&name).expect("the native database reopens");
    assert_eq!(
        reopened.value("SELECT sum(x) FROM t").expect("both rows are in the file"),
        Value::HugeInt(12)
    );
    reopened.execute("SELECT * FROM u").expect("the empty table was written too");
    drop(reopened);
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// `close` is the same write with the error handed back, and it is safe to call and then drop.
#[test]
fn close_writes_the_file_and_says_whether_it_worked() {
    let path = std::env::temp_dir().join(format!("rudb-api-explicit-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database.execute("CREATE TABLE t (x INTEGER)").expect("creates");
    database.execute("CREATE TABLE u (y INTEGER)").expect("creates the second");
    database.execute("INSERT INTO t VALUES (3)").expect("inserts");
    assert!(!path.exists(), "nothing is on the disk until the close below");
    database.close().expect("the file is written and nothing went wrong");
    assert!(path.exists(), "the file is there as soon as close returns");

    let reopened = Database::open(&name).expect("the native database reopens");
    assert_eq!(reopened.value("SELECT sum(x) FROM t").expect("the row"), Value::HugeInt(3));
    // Closing a database that is already on the disk is the checkpoint that finds nothing to do.
    reopened.close().expect("closing a second time writes nothing and works");
    std::fs::remove_file(path).expect("removes the temporary database");
}

/// A read only database does not write its file, on the way out or on a `CHECKPOINT`.
///
/// `CHECKPOINT` succeeding and writing nothing is what the pinned DuckDB does, measured. What it
/// also does and this does not yet is refuse the statements by name, which is #1225, so the create
/// below is expected to succeed here and to be gone at the next open rather than to be refused.
#[test]
fn a_read_only_database_leaves_the_file_alone() {
    let path = std::env::temp_dir().join(format!("rudb-api-frozen-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database.execute("CREATE TABLE t AS SELECT 1 AS x").expect("creates");
    database.close().expect("writes the file");
    let written = std::fs::metadata(&path).expect("the file is there").len();

    let frozen = Database::open_with(&name, Config::default().with_read_only(true))
        .expect("the file opens read only");
    assert!(frozen.config().read_only(), "the setting is what it was opened with");
    // A create with rows behind it, which is the statement that writes the file as it runs on a
    // database that is allowed to. Here it has to take the path that keeps the rows in memory.
    frozen.execute("CREATE TABLE u AS SELECT 2 AS y").expect("the statement is not refused yet");
    assert_eq!(frozen.value("SELECT sum(y) FROM u").expect("the rows"), Value::HugeInt(2));
    frozen.execute("CHECKPOINT").expect("a checkpoint on a read only database does nothing");
    frozen.close().expect("closing writes nothing");
    assert_eq!(std::fs::metadata(&path).expect("the file").len(), written, "the file is untouched");

    let reopened = Database::open(&name).expect("the native database reopens");
    assert_eq!(
        reopened.value("SELECT sum(x) FROM t").expect("the file is what it was"),
        Value::HugeInt(1)
    );
    assert!(
        reopened.execute("SELECT * FROM u").is_err(),
        "the table written under it is not there"
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

#[test]
fn a_declared_row_order_survives_a_checkpoint_and_a_reopen() {
    // The declaration is the only thing about a table that a rewrite destroys without anybody
    // noticing. The per fragment ranges prune on whatever order the rows arrived in, so a table
    // loaded sorted prunes today, and the same table after a checkpoint that did not know to keep
    // the order stops pruning and no output says why. This is the record that stops that.
    let path = std::env::temp_dir().join(format!("rudb-api-cluster-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let columns = [
        Field::new("l_orderkey", LogicalType::BigInt),
        Field::new("l_linenumber", LogicalType::Integer),
        Field::new("l_shipdate", LogicalType::Date),
    ];
    let stage_zero = Clustering::new(vec![2, 0, 1], Width::Month, &columns).expect("valid");

    let database = Database::open(&name).expect("a file name starts a native database");
    database
        .execute("CREATE TABLE lineitem (l_orderkey BIGINT, l_linenumber INTEGER, l_shipdate DATE)")
        .expect("creates");
    database
        .execute("INSERT INTO lineitem VALUES (1, 1, DATE '1995-09-02'), (2, 1, DATE '1995-09-03')")
        .expect("inserts");
    database.execute("CREATE TABLE nation (n_nationkey INTEGER)").expect("creates");
    database.execute("INSERT INTO nation VALUES (1)").expect("inserts");
    database.with_catalog_mut(|catalog| {
        let table = catalog.resolve(&["lineitem"]).expect("resolves");
        catalog
            .table_mut(&table)
            .expect("the table is there")
            .cluster_by(Some(stage_zero.clone()))
            .expect("the columns are the table's");
    });
    database.execute("CHECKPOINT").expect("commits");
    drop(database);

    let reopened = Database::open(&name).expect("the native database reopens");
    reopened.with_catalog(|catalog| {
        let table = catalog.resolve(&["lineitem"]).expect("resolves");
        assert_eq!(
            catalog.table(&table).expect("the table is there").clustering(),
            Some(&stage_zero),
            "the order the table was declared with came back out of the file"
        );
        let plain = catalog.resolve(&["nation"]).expect("resolves");
        assert_eq!(
            catalog.table(&plain).expect("the table is there").clustering(),
            None,
            "and a table nobody declared one for did not pick one up"
        );
    });
    assert_eq!(
        reopened.value("SELECT sum(l_orderkey) FROM lineitem").expect("reads"),
        Value::HugeInt(3)
    );
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn declaring_an_order_after_a_checkpoint_gets_the_file_rewritten() {
    // Everything is already in the file, so the checkpoint's own test for having nothing to do
    // says there is nothing to do, and the declaration would go nowhere. It has to ask whether the
    // file agrees about the order as well as about which tables there are.
    let path = std::env::temp_dir().join(format!("rudb-api-recluster-{}.rudb", std::process::id()));
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let columns = [Field::new("a", LogicalType::Integer), Field::new("d", LogicalType::Date)];
    let asked = Clustering::new(vec![1], Width::Year, &columns).expect("valid");

    let database = Database::open(&name).expect("a file name starts a native database");
    database.execute("CREATE TABLE t (a INTEGER, d DATE)").expect("creates");
    database.execute("INSERT INTO t VALUES (1, DATE '2020-01-01')").expect("inserts");
    database.execute("CHECKPOINT").expect("commits once, with no declaration");
    database.with_catalog_mut(|catalog| {
        let table = catalog.resolve(&["t"]).expect("resolves");
        catalog.table_mut(&table).expect("there").cluster_by(Some(asked.clone())).expect("valid");
    });
    database.execute("CHECKPOINT").expect("commits again, for the declaration alone");
    drop(database);

    let reopened = Database::open(&name).expect("the native database reopens");
    reopened.with_catalog(|catalog| {
        let table = catalog.resolve(&["t"]).expect("resolves");
        assert_eq!(catalog.table(&table).expect("there").clustering(), Some(&asked));
    });
    std::fs::remove_file(path).expect("removes the temporary database");
}
