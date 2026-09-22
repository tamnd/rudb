//! A string column read back out of a file, used as a key.
//!
//! A `VARCHAR` that is still in memory is a run of bytes with the lengths beside it. The same column
//! read back out of a native file is a dictionary of the distinct values with a code per row, and
//! the nulls are in the validity beside the codes rather than being values of their own. Both forms
//! stand for the same column and everything above them is supposed to answer the same, which means
//! the form is invisible right up until one of the paths has no arm for it.
//!
//! That is what #1265 was. The group key column borrows the dictionary rather than copying it, which
//! is what makes grouping a file-backed string cheap, and a borrowed run had nowhere to put a row
//! that did not belong to the dictionary it was reading. A null was such a row, so a correlated
//! subquery over a nullable string read back from a file raised an internal error naming the value's
//! type, where the same query over the same rows in the process that created the table answered.
//!
//! So every test here writes a file, drops the database, opens the file again, and asks the question
//! in the second process's worth of state. A test that only builds the table in memory passes
//! whatever the answer is.

use rudb::Database;
use rudb_common::Value;

/// A file holding a database the statements were run against, and the path it is at.
///
/// Opened, written to and dropped, so what comes back is a committed file rather than a handle.
fn written(tag: &str, statements: &[&str]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("rudb-reread-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let database =
        Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("a native database");
    for sql in statements {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    drop(database);
    path
}

/// The database in a file that was already written, opened again.
fn reopened(path: &std::path::Path) -> Database {
    Database::open(path.to_str().expect("a UTF-8 temporary path")).expect("the file opens")
}

/// A database with the statements run against no file at all, which is the answer to compare with.
fn in_memory(statements: &[&str]) -> Database {
    let database = Database::new();
    for sql in statements {
        database.execute(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

/// Every row of the answer as one string per row, columns joined by a bar, which is enough to
/// compare two engines' worth of the same query without caring what the types printed as.
fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    (0..result.len())
        .map(|row| {
            (0..result.names().len())
                .map(|column| format!("{:?}", result.value_at(row, column)))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// The statements every test below writes, holding a null, an empty string, a repeat and a value
/// too long to sit inline, so the dictionary has more than one entry and the validity has more than
/// one answer.
const SETUP: &[&str] = &[
    "CREATE TABLE o (k INTEGER, t VARCHAR)",
    "CREATE TABLE i (k INTEGER, w INTEGER)",
    "INSERT INTO o VALUES (1, 'a'), (2, NULL), (3, 'a'), (4, ''), (5, NULL), (6, 'a string past \
     the sixteen bytes a view holds inline')",
    "INSERT INTO i VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)",
];

/// The reported case, which is the narrowest shape that reaches it: the outer column has to be a
/// string, it has to hold a null, and the correlation has to be the thing that turns it into a
/// domain key. An `INTEGER` column with a null in it was always fine and so was a plain `GROUP BY`
/// over the same reloaded column.
#[test]
fn a_nullable_string_read_back_from_a_file_can_be_a_domain_key() {
    let path = written("domain", SETUP);
    let database = reopened(&path);
    let sql = "SELECT k, (SELECT o.t FROM i WHERE i.k = o.k GROUP BY i.k) AS c FROM o ORDER BY k";
    assert_eq!(rows(&database, sql), rows(&in_memory(SETUP), sql));
    let result = database.query(sql).expect("the correlated subquery answers");
    assert_eq!(result.value_at(0, 1), Value::Varchar("a".into()));
    assert_eq!(result.value_at(1, 1), Value::Null);
}

/// The same column grouped every other way a query can group it, since the arm that was missing is
/// under all of them and the correlated case is only where it was first noticed.
#[test]
fn a_string_read_back_from_a_file_groups_the_same_as_one_that_never_left_memory() {
    let path = written("grouping", SETUP);
    let database = reopened(&path);
    let memory = in_memory(SETUP);
    for sql in [
        "SELECT t, count(*) FROM o GROUP BY t ORDER BY t",
        "SELECT t, count(*) FROM o GROUP BY t HAVING count(*) > 1 ORDER BY t",
        "SELECT count(DISTINCT t) FROM o",
        "SELECT t FROM o GROUP BY t ORDER BY t",
        "SELECT k, (SELECT count(*) FROM i WHERE i.w > 0 GROUP BY o.t) AS c FROM o ORDER BY k",
        "SELECT k, (SELECT sum(i.w) FROM i WHERE i.k = o.k GROUP BY o.t) AS c FROM o ORDER BY k",
        "SELECT o.t, sum(i.w) FROM o JOIN i ON i.k = o.k GROUP BY o.t ORDER BY o.t",
    ] {
        assert_eq!(rows(&database, sql), rows(&memory, sql), "{sql}");
    }
}

/// The two rows that look alike to a form which keeps the nulls anywhere but the validity. A null
/// slot in a borrowed run stores a code nobody reads, and the empty string stores a real one, so a
/// path that told them apart by their bytes would put both in one group.
#[test]
fn a_null_and_an_empty_string_read_back_from_a_file_are_not_the_same_group() {
    let path = written("empty", SETUP);
    let database = reopened(&path);
    let counted = database
        .query("SELECT t, count(*) FROM o GROUP BY t ORDER BY t NULLS LAST")
        .expect("the grouped count answers");
    // Four groups: the two 'a' rows, the long one, the empty string, and the two nulls together.
    assert_eq!(counted.len(), 4);
    assert_eq!(counted.value_at(0, 0), Value::Varchar(String::new()));
    assert_eq!(counted.value_at(0, 1), Value::BigInt(1));
    assert_eq!(counted.value_at(3, 0), Value::Null);
    assert_eq!(counted.value_at(3, 1), Value::BigInt(2));
}

/// And the same over more rows than one chunk holds, because a run is started again per chunk and
/// the row that ends one is the row of the chunk after it.
#[test]
fn a_string_read_back_from_a_file_groups_the_same_across_many_chunks() {
    let path = written(
        "chunks",
        &["CREATE TABLE m AS SELECT r AS k, CASE WHEN r % 7 = 0 THEN NULL ELSE 'v' || (r % 5) \
             END AS t FROM range(20000) tt(r)"],
    );
    let database = reopened(&path);
    let sql = "SELECT t, count(*) FROM m GROUP BY t ORDER BY t NULLS LAST";
    let counted = database.query(sql).expect("the grouped count answers");
    // Five values and the null, and every seventh row of the twenty thousand is the null.
    assert_eq!(counted.len(), 6);
    assert_eq!(counted.value_at(5, 0), Value::Null);
    assert_eq!(counted.value_at(5, 1), Value::BigInt(20_000 / 7 + 1));
    let correlated = "SELECT count(*) FROM (SELECT k, (SELECT m.t FROM range(3) x(a) WHERE a = 1 \
                      GROUP BY a) AS c FROM m) q";
    assert_eq!(
        database.value(correlated).expect("the correlated subquery answers"),
        Value::BigInt(20_000)
    );
}
