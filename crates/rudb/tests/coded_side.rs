//! A join whose gathered side holds codes into a table wide dictionary answers what one over the
//! strings does.
//!
//! A stored table keeps a column of few distinct strings as codes into one dictionary for the whole
//! table, and a scan hands those codes up. The side a join gathers now keeps them as codes rather
//! than laying the strings out, so whatever reads the joined rows reads codes. The table in memory
//! holds plain strings, so asking both the same question checks that grouping, sorting, filtering
//! and joining on the coded columns all come out the same, nulls included.

use rudb::Database;
use rudb_common::Value;

/// Parts with a brand that is sometimes null and a type, and supplies that name every part.
const TABLES: [&str; 4] = [
    "CREATE TABLE part(p_partkey BIGINT, p_brand VARCHAR, p_type VARCHAR, p_size INTEGER)",
    "INSERT INTO part SELECT i, CASE WHEN i % 17 = 3 THEN NULL ELSE 'Brand#' || (i * 7 % 25)::VARCHAR \
     END, CASE i % 4 WHEN 0 THEN 'MEDIUM POLISHED TIN' WHEN 1 THEN 'SMALL BRUSHED COPPER' WHEN 2 \
     THEN 'LARGE PLATED NICKEL' ELSE 'ECONOMY ANODIZED STEEL' END, (i % 50)::INTEGER FROM \
     range(20000) r(i)",
    "CREATE TABLE partsupp(ps_partkey BIGINT, ps_suppkey BIGINT, ps_brand VARCHAR)",
    "INSERT INTO partsupp SELECT i % 20000, i * 13 % 1000, 'Brand#' || (i % 30)::VARCHAR FROM \
     range(80000) r(i)",
];

/// The same rows in memory and in a file.
struct Pair {
    memory: Database,
    file: Database,
    path: std::path::PathBuf,
}

impl Pair {
    /// The pair for the test called `name`, each in a file of its own since the tests run at once.
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir()
            .join(format!("rudb-coded-side-{name}-{}.rudb", std::process::id()));
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
        // Otherwise this is a test of strings joined to strings, which is not what it is for.
        for column in ["p_brand", "p_type"] {
            let coded = file
                .query(&format!(
                    "SELECT bool_and(compression LIKE 'TABLE DICT%') FROM pragma_storage_info('part') \
                     WHERE column_name = '{column}'"
                ))
                .expect("the storage is described");
            assert_eq!(coded.rows().next(), Some(vec![Value::Boolean(true)]), "{column}");
        }
        Self { memory, file, path }
    }

    /// Asserts the file agrees with memory on every row, on one thread and on four, and returns
    /// how many rows there were.
    fn agree(&self, query: &str) -> usize {
        let wanted: Vec<Vec<Value>> =
            self.memory.query(query).expect("the memory tables answer").rows().collect();
        for threads in [1, 4] {
            self.file.execute(&format!("SET threads = {threads}")).expect("sets the threads");
            let got: Vec<Vec<Value>> =
                self.file.query(query).expect("the file answers").rows().collect();
            assert_eq!(got, wanted, "{threads} threads: {query}");
        }
        wanted.len()
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn a_grouping_on_the_coded_strings_of_the_gathered_side_is_the_grouping_on_the_strings() {
    let pair = Pair::new("grouping");
    let rows = pair.agree(
        "SELECT p_brand, p_type, p_size, count(DISTINCT ps_suppkey) AS n FROM partsupp, part WHERE \
         p_partkey = ps_partkey AND p_brand <> 'Brand#4' AND p_type NOT LIKE 'MEDIUM POLISHED%' \
         AND p_size IN (1, 4, 9, 16, 25, 36, 49) GROUP BY p_brand, p_type, p_size ORDER BY n DESC, \
         p_brand NULLS FIRST, p_type, p_size",
    );
    assert!(rows > 8, "only {rows} groups");
    assert!(
        pair.agree(
            "SELECT p_brand, count(*) FROM partsupp JOIN part ON p_partkey = ps_partkey \
         WHERE p_brand IS NULL OR p_brand < 'Brand#2' GROUP BY p_brand ORDER BY p_brand NULLS LAST"
        ) > 3
    );
}

#[test]
fn coded_strings_of_the_gathered_side_read_back_filtered_and_sorted_as_the_strings() {
    let pair = Pair::new("sorted");
    let rows = pair.agree(
        "SELECT ps_partkey, ps_suppkey, upper(p_type), p_brand || '!' FROM partsupp JOIN part ON \
         p_partkey = ps_partkey WHERE p_type LIKE '%PLATED%' AND ps_suppkey < 40 ORDER BY \
         ps_partkey, ps_suppkey",
    );
    assert!(rows > 100, "only {rows} rows");
}

#[test]
fn a_join_on_a_coded_string_matches_by_the_string_and_not_by_the_code() {
    let pair = Pair::new("joined");
    // The two brand columns are in two tables and so under two dictionaries, where one code can
    // stand for two different strings.
    let rows = pair.agree(
        "SELECT p_brand, count(*), min(ps_suppkey) FROM part JOIN partsupp ON p_brand = ps_brand \
         AND p_partkey = ps_partkey GROUP BY p_brand ORDER BY p_brand",
    );
    assert!(rows > 3, "only {rows} brands");
    assert!(
        pair.agree(
            "SELECT count(*) FROM part JOIN (SELECT DISTINCT ps_brand FROM partsupp) \
         ON p_brand = ps_brand"
        ) > 0
    );
}
