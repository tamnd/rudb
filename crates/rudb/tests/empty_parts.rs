//! A grouping fed by a scan that kept no rows of some parts answers what one over every row does.
//!
//! A selective filter on a column the file records the rows of each value for reads only those
//! rows, and a part holding none of them comes up as an empty chunk of constant vectors. A grouped
//! `COUNT(DISTINCT)` under a TopN on the count alone plans the shape of its codes from the first
//! chunk it sees, and an empty one in between used to read as a second code space and fail the
//! query.

use rudb::Database;
use rudb_common::Value;

/// Models set on the first and the last rows only, so the parts in between keep nothing. The noise
/// column makes the table large enough that the checkpoint records the rows of each model.
const TABLES: [&str; 2] = [
    "CREATE TABLE hits(model VARCHAR, user_id BIGINT, noise BIGINT)",
    "INSERT INTO hits SELECT CASE WHEN (i < 20000 OR i >= 380000) AND i % 5 = 0 THEN 'model' || \
     (i % 7)::VARCHAR ELSE '' END, i % 3001, (i * 2654435761) % 1000000007 FROM range(400000) r(i)",
];

#[test]
fn a_grouped_distinct_count_skips_the_parts_a_selective_filter_kept_nothing_of() {
    let path = std::env::temp_dir().join(format!("rudb-empty-parts-{}.rudb", std::process::id()));
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
    let query = "SELECT model, count(DISTINCT user_id) AS u FROM hits WHERE model <> '' GROUP BY \
                 model ORDER BY u DESC LIMIT 10";
    // Two models can tie on the count, so the rows are compared as a set.
    let sorted = |mut rows: Vec<Vec<Value>>| {
        rows.sort_by_key(|row| row[0].to_string());
        rows
    };
    let wanted = sorted(memory.query(query).expect("the memory table answers").rows().collect());
    assert_eq!(wanted.len(), 7);
    for threads in [1, 4] {
        file.execute(&format!("SET threads = {threads}")).expect("sets the threads");
        let got = sorted(file.query(query).expect("the file answers").rows().collect());
        assert_eq!(got, wanted, "{threads} threads");
    }
    drop(file);
    let _ = std::fs::remove_file(&path);
}
