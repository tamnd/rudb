//! A scan whose join keeps a few rows in a thousand reads its other columns at those rows alone, and
//! answers what a scan that read them whole does.
//!
//! Once the bitmap a join over a small filtered side hands the scan is measured keeping few rows,
//! the scan reads the key first and the other columns only at the rows the bitmap kept, integers
//! unpacked a row at a time. The table in memory is read whole, so asking both the same question
//! checks the rows, the nulls and the values that come out of the sparse read.

use rudb::Database;
use rudb_common::Value;

/// A few hundred thousand facts over twenty thousand parts, so the bitmap is measured before most
/// of the facts are read, with prices, quantities that step by a hundred, a few distinct discounts,
/// sorted keys and some nulls.
const TABLES: [&str; 4] = [
    "CREATE TABLE part(p_partkey BIGINT, p_brand VARCHAR)",
    "INSERT INTO part SELECT i, 'Brand#' || (i % 97)::VARCHAR FROM range(20000) r(i)",
    "CREATE TABLE fact(f_orderkey BIGINT, f_partkey BIGINT, f_quantity DECIMAL(15,2), f_price \
     DECIMAL(15,2), f_discount DECIMAL(15,2), f_ship DATE, f_note INTEGER)",
    "INSERT INTO fact SELECT i // 4, i * 7919 % 20000, (i * 13 % 50 + 1)::DECIMAL(15,2), (i * \
     104729 % 9000000)::DECIMAL(15,2) / 100, (i % 11)::DECIMAL(15,2) / 100, DATE '1995-01-01' + \
     (i % 900)::INTEGER, CASE WHEN i % 29 = 3 THEN NULL ELSE (i * 31337 % 1000003)::INTEGER END FROM \
     range(400000) r(i)",
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
            .join(format!("rudb-sparse-rows-{name}-{}.rudb", std::process::id()));
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
fn the_columns_read_at_the_rows_a_tight_join_keeps_are_the_columns_read_whole() {
    let pair = Pair::new("sums");
    // One brand of ninety seven, so about one fact in a hundred.
    let rows = pair.agree(
        "SELECT count(*), sum(f_quantity), sum(f_price * (1 - f_discount)), min(f_ship), \
         max(f_ship), count(f_note), sum(f_note), sum(f_orderkey) FROM fact, part WHERE f_partkey \
         = p_partkey AND p_brand = 'Brand#23'",
    );
    assert!(matches!(rows[0][0], Value::BigInt(count) if count > 3000), "{rows:?}");
    // Twenty parts of twenty thousand, which is one fact in a thousand, row by row.
    pair.agree(
        "SELECT f_orderkey, f_partkey, f_quantity, f_price, f_discount, f_ship, f_note FROM fact \
         JOIN part ON f_partkey = p_partkey WHERE p_partkey % 1000 = 7 ORDER BY f_orderkey, \
         f_partkey, f_note NULLS FIRST",
    );
}

#[test]
fn a_tight_join_with_a_filter_of_its_own_keeps_the_rows_both_keep() {
    let pair = Pair::new("filtered");
    pair.agree(
        "SELECT p_brand, count(*), sum(f_price), avg(f_quantity) FROM fact JOIN part ON f_partkey = \
         p_partkey WHERE p_brand IN ('Brand#5', 'Brand#60') AND f_ship < DATE '1996-01-01' GROUP BY \
         p_brand ORDER BY p_brand",
    );
    // The q17 shape, where the subquery reads the same facts through the same join.
    pair.agree(
        "SELECT sum(f_price) / 7.0 FROM fact, part WHERE p_partkey = f_partkey AND p_brand = \
         'Brand#23' AND f_quantity < (SELECT 0.2 * avg(f_quantity) FROM fact WHERE f_partkey = \
         p_partkey)",
    );
}
