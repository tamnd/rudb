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
fn a_file_name_says_what_is_missing_rather_than_quietly_being_a_memory_database() {
    // A program that opened a file and wrote to it would be told nothing until it came back looking
    // for its data, which is the worst moment to find out.
    let error = Database::open("/tmp/whatever.rudb").expect_err("there is no storage format yet");
    let message = error.message().to_string();
    assert!(message.contains("/tmp/whatever.rudb"), "{message}");
    assert!(message.contains("storage format"), "{message}");
    assert!(message.contains("issues/103"), "{message}");
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
