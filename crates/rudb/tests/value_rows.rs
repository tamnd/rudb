//! A filter on a coded text column reads the rows the file records for the values it keeps, and
//! answers what a scan that read the column whole does.
//!
//! The checkpoint records the rows of each value of every text column coded against a table wide
//! dictionary. A scan asks its filter of the dictionary once and reads only the rows of the values
//! that pass. The table in memory is read whole, so asking both the same question checks that no
//! row is lost, including the nulls, a filter that keeps a null, and a filter on two columns.

use rudb::Database;
use rudb_common::Value;

/// Four hundred thousand credits with a note drawn from a few hundred values, some of them common,
/// a quarter of them null, and a second text column beside it.
const TABLES: [&str; 2] = [
    "CREATE TABLE credit(c_id BIGINT, c_movie BIGINT, c_note VARCHAR, c_kind VARCHAR)",
    "INSERT INTO credit SELECT i, i * 7919 % 50000, CASE WHEN i % 4 = 1 THEN NULL WHEN i % 13 = 0 \
     THEN '(producer)' WHEN i % 17 = 0 THEN '(executive producer)' ELSE '(as ' || (i * 31 % 300)::VARCHAR \
     || ')' END, 'kind ' || (i % 7)::VARCHAR FROM range(400000) r(i)",
];

/// The same rows in memory and in a file.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("rudb-value-rows-{name}-{}.rudb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let memory = Database::new();
        for sql in TABLES {
            memory.execute(sql).expect("the memory tables are made");
        }
        let name = path.to_str().expect("a UTF-8 temporary path");
        {
            let writing = Database::open(name).expect("a file name starts a native database");
            for sql in TABLES {
                writing.execute(sql).expect("the file tables are made");
            }
            writing.execute("CHECKPOINT").expect("the file tables are committed");
            writing.execute("CHECKPOINT").expect("the sections are built");
        }
        let file = Database::open(name).expect("the written file opens again");
        Self { memory, file, path }
    }

    /// Asserts the file agrees with memory, on one thread and on four, and returns the rows.
    fn agree(&self, query: &str) -> Vec<Vec<Value>> {
        let wanted: Vec<Vec<Value>> =
            self.memory.query(query).expect("the memory tables answer").rows().collect();
        for threads in [1, 4] {
            self.file.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            let got: Vec<Vec<Value>> =
                self.file.query(query).expect("the file answers").rows().collect();
            assert_eq!(got, wanted, "{threads} threads: {query}");
        }
        wanted
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn a_filter_on_a_coded_column_keeps_the_rows_a_whole_read_keeps() {
    let pair = Pair::new("filters");
    let rows = pair.agree(
        "SELECT count(*), sum(c_id), sum(c_movie), min(c_note), max(c_note) FROM credit WHERE \
         c_note IN ('(producer)', '(executive producer)')",
    );
    assert_ne!(rows[0][0], Value::BigInt(0), "the filter keeps some rows");
    pair.agree("SELECT count(*), sum(c_id) FROM credit WHERE c_note = '(as 17)'");
    pair.agree("SELECT count(*), sum(c_id) FROM credit WHERE c_note LIKE '%(as 1_)%'");
    pair.agree("SELECT count(*), sum(c_id) FROM credit WHERE c_note = 'no such note'");
    pair.agree(
        "SELECT count(*), sum(c_id) FROM credit WHERE c_note LIKE '%producer%' AND c_note NOT \
         LIKE '%executive%'",
    );
    pair.agree(
        "SELECT count(*), sum(c_id) FROM credit WHERE c_note = '(producer)' AND c_kind = 'kind 3'",
    );
    pair.agree("SELECT count(*), sum(c_id) FROM credit WHERE c_note IS NULL OR c_note = '(as 5)'");
    pair.agree("SELECT count(*), sum(c_id) FROM credit WHERE coalesce(c_note, 'x') = 'x'");
    pair.agree(
        "SELECT c_id, c_movie, c_note FROM credit WHERE c_note = '(as 250)' ORDER BY c_id LIMIT 20",
    );
}
