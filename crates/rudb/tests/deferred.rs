//! A scan that reads its string columns after its filters have run answers what one that reads
//! them first does.
//!
//! A scan of a stored table reads the columns its filters use, runs the filters, and then reads the
//! string columns nothing filters on only at the rows that were kept. The table in memory is read
//! the old way round, so asking both the same question checks the rows the second read picks are
//! the rows the first one kept, in the same order, nulls included.

use rudb::Database;
use rudb_common::Value;

/// Customers with a name, an address that is sometimes null and a segment, and orders that name
/// a quarter of them.
const TABLES: [&str; 4] = [
    "CREATE TABLE customer(c_custkey BIGINT, c_name VARCHAR, c_address VARCHAR, c_segment VARCHAR)",
    "INSERT INTO customer SELECT i, 'Customer#' || (i + 100000000)::VARCHAR, CASE WHEN i % 11 = 4 \
     THEN NULL ELSE 'street ' || (i * 37 % 1000)::VARCHAR || ' of the town ' || (i % 97)::VARCHAR \
     END, CASE i % 3 WHEN 0 THEN 'BUILDING' WHEN 1 THEN 'MACHINERY' ELSE 'HOUSEHOLD' END FROM \
     range(30000) r(i)",
    "CREATE TABLE orders(o_orderkey BIGINT, o_custkey BIGINT, o_total BIGINT)",
    "INSERT INTO orders SELECT i, i * 4 % 30000, i % 1000 FROM range(20000) r(i)",
];

/// The same rows in memory and in a file.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("rudb-deferred-{}.rudb", std::process::id()));
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

    /// Asserts the file agrees with memory on every row, and returns how many rows there were.
    fn agree(&self, query: &str) -> usize {
        let wanted = self.memory.query(query).expect("the memory tables answer");
        let got = self.file.query(query).expect("the file answers");
        let rows: Vec<Vec<Value>> = got.rows().collect();
        assert_eq!(rows, wanted.rows().collect::<Vec<_>>(), "{query}");
        rows.len()
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A join keeps a quarter of the customers, a pushed filter keeps a third, both at once keep a
/// twelfth, and a filter on a string column itself reads that column first. Each answers the same
/// from the file as from memory, the null addresses included.
#[test]
fn string_columns_read_after_the_filters_are_the_rows_the_filters_kept() {
    let pair = Pair::new();
    let joined = "SELECT c_custkey, c_name, c_address, c_segment, sum(o_total) FROM customer, \
                  orders WHERE c_custkey = o_custkey GROUP BY ALL ORDER BY c_custkey";
    assert_eq!(pair.agree(joined), 7_500);
    let pushed = "SELECT c_custkey, c_name, c_address FROM customer WHERE c_custkey % 3 = 1 ORDER \
                  BY c_custkey";
    assert_eq!(pair.agree(pushed), 10_000);
    let both = "SELECT c_custkey, c_name, c_address, count(*) FROM customer, orders WHERE \
                c_custkey = o_custkey AND c_custkey % 3 = 1 GROUP BY ALL ORDER BY c_custkey";
    assert_eq!(pair.agree(both), 2_500);
    let on_a_string = "SELECT c_custkey, c_name, c_address FROM customer, orders WHERE c_custkey = \
                       o_custkey AND c_segment = 'MACHINERY' ORDER BY c_custkey, o_orderkey";
    assert_eq!(pair.agree(on_a_string), 6_667);
    let nulls = "SELECT count(*), count(c_address) FROM customer, orders WHERE c_custkey = \
                 o_custkey";
    pair.agree(nulls);
    let everyone = "SELECT count(DISTINCT c_name) FROM customer WHERE c_custkey >= 0";
    pair.agree(everyone);
    let kept = pair.file.query(nulls).expect("the file answers");
    assert_eq!(kept.value_at(0, 0), Value::BigInt(20_000));
}
