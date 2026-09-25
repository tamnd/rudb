//! The leading groups of a one column count are read out of the column's frequency synopsis when
//! the synopsis can vouch for them, and out of the rows when it cannot. The same rows held in memory
//! are always grouped row by row, so asking both the same question checks the answer either way.

use rudb::Database;
use rudb_common::Value;

/// A user column with thirteen heavy values over a long tail and some nulls, an address column with
/// eleven heavy values, both weighted so that no two heavy values are held by the same number of
/// rows, and a flat table where every value is held by a handful of rows and the leading ten by
/// only a few more.
const TABLES: [&str; 4] = [
    "CREATE TABLE hits(u BIGINT, ip INTEGER)",
    "INSERT INTO hits SELECT CASE WHEN i % 97 = 0 THEN NULL WHEN i % 3 = 0 THEN floor((sqrt(8 * \
     (i // 3 % 91) + 1) - 1) / 2)::BIGINT * 1000 ELSE i * 7919 % 400000 END, CASE WHEN i % 5 = 0 \
     THEN (floor((sqrt(8 * (i // 5 % 66) + 1) - 1) / 2) - 5)::INTEGER ELSE (i * 31 % \
     300000)::INTEGER END FROM range(600000) r(i)",
    "CREATE TABLE flat(v BIGINT)",
    "INSERT INTO flat SELECT i % 150000 FROM range(600000) r(i) UNION ALL SELECT k * 7 FROM \
     range(10) r(k), range(12) s(j) WHERE j <= k",
];

struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("rudb-value-frequencies-{name}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let memory = Database::new();
        for sql in TABLES {
            memory.execute(sql).expect("the memory table is made");
        }
        let name = path.to_str().expect("a UTF-8 temporary path");
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            for sql in TABLES {
                writing.execute(sql).expect("the file table is made");
            }
            writing.execute("CHECKPOINT").expect("the file table is committed");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    fn rows(database: &Database, query: &str) -> Vec<Vec<Value>> {
        database.query(query).expect("the query answers").rows().collect()
    }

    fn agree(&self, query: &str) -> Vec<Vec<Value>> {
        let wanted = Self::rows(&self.memory, query);
        for threads in [1, 4] {
            self.file.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            assert_eq!(Self::rows(&self.file, query), wanted, "{threads} threads: {query}");
        }
        wanted
    }

    /// Whether the file answered without its scan reading a row.
    fn unread(&self, query: &str) -> bool {
        let plan = Self::rows(&self.file, &format!("EXPLAIN ANALYZE {query}"))
            .into_iter()
            .flatten()
            .map(|value| value.to_string())
            .collect::<String>();
        let scan = plan.lines().find(|line| line.trim_start().starts_with("Get ")).expect("a scan");
        scan.ends_with("[0 rows, 0s wall]")
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn the_leading_counts_of_one_column_come_out_of_its_synopsis() {
    let pair = Pair::new("leading");
    let query = "SELECT u, COUNT(*) AS c FROM hits GROUP BY u ORDER BY c DESC LIMIT 10";
    let rows = pair.agree(query);
    assert_eq!(rows.len(), 10);
    assert!(pair.unread(query));
    pair.agree("SELECT u, COUNT(*) AS c FROM hits GROUP BY u ORDER BY c DESC LIMIT 3 OFFSET 8");
    pair.agree("SELECT COUNT(*), u FROM hits GROUP BY u ORDER BY COUNT(*) DESC LIMIT 14");
    let query = "SELECT ip, ip - 1, COUNT(*) AS c FROM hits GROUP BY ip, ip - 1 ORDER BY c DESC \
                 LIMIT 11";
    assert_eq!(pair.agree(query).len(), 11);
    assert!(pair.unread(query));
}

#[test]
fn a_column_whose_synopsis_cannot_vouch_for_the_answer_is_grouped_row_by_row() {
    let pair = Pair::new("flat");
    // The tenth value of the flat table is held by five rows and the synopsis can only say that a
    // value it dropped is held by some handful, so the list alone could have missed a group.
    let query = "SELECT v, COUNT(*) AS c FROM flat GROUP BY v ORDER BY c DESC LIMIT 10";
    assert_eq!(pair.agree(query).len(), 10);
    assert!(!pair.unread(query));
    // More groups asked for than the synopsis lists, all of them past the heavy values.
    let query = "SELECT u, COUNT(*) AS c FROM hits GROUP BY u ORDER BY c DESC LIMIT 1 OFFSET 700";
    assert!(!pair.unread(query));
}

#[test]
fn rows_changed_after_the_synopsis_was_written_are_counted() {
    let pair = Pair::new("changed");
    for sql in ["DELETE FROM hits WHERE u = 0", "INSERT INTO hits VALUES (7, 1), (7, 1)"] {
        pair.memory.execute(sql).expect("the memory table changes");
        pair.file.execute(sql).expect("the file table changes");
    }
    pair.agree("SELECT u, COUNT(*) AS c FROM hits GROUP BY u ORDER BY c DESC LIMIT 10");
    pair.agree("SELECT ip, COUNT(*) AS c FROM hits GROUP BY ip ORDER BY c DESC LIMIT 10");
}
