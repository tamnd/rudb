//! A `LIKE` a compressed text page answers on its codes keeps the rows one over the strings does.
//!
//! A scan whose filter is the only reader of a string column asks the column's pages which rows
//! pass, and a compressed page answers without its strings being decompressed. The table in memory
//! holds plain strings and always searches them, so asking both the same question checks the answer
//! on the codes, nulls, negation and the columns read beside it included.

use rudb::Database;
use rudb_common::Value;

/// Comments long and varied enough to be compressed, some null and some holding the words.
const TABLES: [&str; 2] = [
    "CREATE TABLE orders(o_orderkey BIGINT, o_custkey BIGINT, o_comment VARCHAR)",
    "INSERT INTO orders SELECT i, i % 997, CASE WHEN i % 23 = 5 THEN NULL ELSE CASE i % 7 WHEN 0 \
     THEN 'furiously special ideas sleep' WHEN 1 THEN 'blithely special foxes nag requests' WHEN 2 \
     THEN 'requests are special' WHEN 3 THEN 'quick specialrequests haggle' WHEN 4 THEN 'ironic \
     packages wake é' ELSE 'pending deposits ü detect' END || ' ' || (i * 7919 % 100003)::VARCHAR \
     || ' carefully final accounts' END FROM range(60000) r(i)",
];

/// The same rows in memory and in a file.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("rudb-like-on-codes-{name}-{}.rudb", std::process::id()));
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
fn a_count_under_like_and_not_like_is_the_count_over_the_strings() {
    let pair = Pair::new("count");
    for pattern in ["%special%requests%", "%special%", "%é%", "%%ü%%", "%nothing here%", "%1%2%3%"]
    {
        for op in ["LIKE", "NOT LIKE"] {
            let rows = pair
                .agree(&format!("SELECT count(*) FROM orders WHERE o_comment {op} '{pattern}'"));
            assert_eq!(rows.len(), 1);
        }
    }
    // The nulls are in neither answer.
    let count = |op: &str| {
        pair.agree(&format!(
            "SELECT count(*) FROM orders WHERE o_comment {op} '%special%requests%'"
        ))[0][0]
            .clone()
    };
    let (Value::BigInt(like), Value::BigInt(unlike)) = (count("LIKE"), count("NOT LIKE")) else {
        panic!("counts")
    };
    assert_eq!(like + unlike, 60000 - 2609);
}

#[test]
fn the_columns_read_beside_the_filtered_one_are_the_kept_rows() {
    let pair = Pair::new("beside");
    let rows = pair.agree(
        "SELECT o_custkey, count(o_orderkey), sum(o_orderkey) FROM orders WHERE o_comment NOT LIKE \
         '%special%requests%' GROUP BY o_custkey ORDER BY o_custkey",
    );
    assert!(rows.len() > 900, "only {} groups", rows.len());
    pair.agree(
        "SELECT o_orderkey, o_custkey FROM orders WHERE o_comment LIKE '%special%requests%' ORDER BY \
         o_orderkey LIMIT 50",
    );
    // Read above the filter too, which leaves the strings to be read the usual way.
    pair.agree(
        "SELECT o_orderkey, o_comment FROM orders WHERE o_comment LIKE '%quick%' ORDER BY o_orderkey \
         LIMIT 20",
    );
}

#[test]
fn the_filtered_side_of_a_left_join_keeps_the_rows_the_strings_keep() {
    let pair = Pair::new("joined");
    let rows = pair.agree(
        "SELECT c_count, count(*) AS custdist FROM (SELECT c, count(o_orderkey) AS c_count FROM \
         range(1000) t(c) LEFT JOIN orders ON c = o_custkey AND o_comment NOT LIKE \
         '%special%requests%' GROUP BY c) GROUP BY c_count ORDER BY custdist DESC, c_count DESC",
    );
    assert!(rows.len() > 3, "only {} rows", rows.len());
}
