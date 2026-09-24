//! A table's keys and foreign keys are in the file, so a reopened database refuses what it refused
//! before the checkpoint.
//!
//! The keys used to live only in the catalog in memory. A checkpoint wrote the rows and nothing
//! else, so the next open had tables with no primary key, took a repeated key without a word and let
//! a parent row go that children still pointed at.

use rudb::Database;

/// A file path of its own for each test, removed when the test is done.
struct File(std::path::PathBuf);

impl File {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("rudb-stored-keys-{name}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn open(&self) -> Database {
        Database::open(self.0.to_str().expect("a UTF-8 temporary path")).expect("the file opens")
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const PARENT: [&str; 2] = [
    "CREATE TABLE region(r_regionkey INTEGER PRIMARY KEY, r_name VARCHAR UNIQUE)",
    "INSERT INTO region VALUES (0, 'africa'), (1, 'america'), (2, 'asia')",
];

const CHILD: [&str; 2] = [
    "CREATE TABLE nation(n_nationkey INTEGER, n_code INTEGER, n_regionkey INTEGER REFERENCES \
     region(r_regionkey), PRIMARY KEY (n_nationkey, n_code))",
    "INSERT INTO nation VALUES (0, 7, 0), (1, 7, 1), (2, 7, 1), (3, 7, 2)",
];

/// Every refusal the keys make, asked of a database that has just been opened from the file.
fn refuses_what_the_keys_refuse(db: &Database) {
    let refused = |sql: &str| {
        let error = db.execute(sql).expect_err(sql);
        assert!(error.to_string().contains("onstraint"), "{sql}: {error}");
    };
    refused("INSERT INTO region VALUES (1, 'europe')");
    refused("INSERT INTO region VALUES (3, 'asia')");
    refused("INSERT INTO region VALUES (NULL, 'europe')");
    refused("INSERT INTO nation VALUES (3, 7, 1)");
    refused("INSERT INTO nation VALUES (4, 7, 9)");
    refused("DELETE FROM region WHERE r_regionkey = 1");
    // And what they allow still goes in.
    db.execute("INSERT INTO region VALUES (3, 'europe')").expect("a new key");
    db.execute("INSERT INTO nation VALUES (3, 8, 3)").expect("a new pair that finds its parent");
    db.execute("INSERT INTO nation VALUES (5, 7, NULL)").expect("a null points at nothing");
}

#[test]
fn keys_written_by_a_whole_file_checkpoint_come_back() {
    let file = File::new("whole");
    {
        let db = file.open();
        for sql in PARENT.iter().chain(&CHILD) {
            db.execute(sql).expect(sql);
        }
        db.execute("CHECKPOINT").expect("the checkpoint");
    }
    refuses_what_the_keys_refuse(&file.open());
}

#[test]
fn keys_written_by_an_appending_checkpoint_come_back() {
    let file = File::new("appended");
    {
        let db = file.open();
        for sql in PARENT {
            db.execute(sql).expect(sql);
        }
        db.execute("CHECKPOINT").expect("the first checkpoint");
    }
    {
        // The parent is in the file now, so this checkpoint carries it forward and appends the
        // child, which is the other of the two ways a table gets written.
        let db = file.open();
        for sql in CHILD {
            db.execute(sql).expect(sql);
        }
        db.execute("CHECKPOINT").expect("the second checkpoint");
    }
    refuses_what_the_keys_refuse(&file.open());
}

#[test]
fn a_table_with_no_keys_still_takes_anything() {
    let file = File::new("plain");
    {
        let db = file.open();
        db.execute("CREATE TABLE t(a INTEGER)").expect("create");
        db.execute("INSERT INTO t VALUES (1), (1)").expect("insert");
        db.execute("CHECKPOINT").expect("the checkpoint");
    }
    let db = file.open();
    db.execute("INSERT INTO t VALUES (1), (NULL)").expect("nothing to refuse");
    let count = db.query("SELECT count(*) FROM t").expect("count").rows().next().expect("a row");
    assert_eq!(count[0], rudb_common::Value::BigInt(4));
}
