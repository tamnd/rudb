//! A scan a pass reads again under an index of its own keeps what the file said about it.
//!
//! The pass behind `min(c.name)` over a join narrows the scan of `c` to the columns the join needs
//! and the row's place in its file, and gives the narrowed scan a fresh index. The estimates are
//! looked up by index, so before the synopsis was carried over an equality on a skewed column was
//! charged the row count over the distinct count. JOB 33a has 84,843 `[us]` companies charged at
//! 1,093, which put them first in the join order.

use rudb::Database;

#[test]
fn an_equality_on_a_narrowed_scan_is_estimated_from_the_synopsis() {
    let path =
        std::env::temp_dir().join(format!("rudb-narrowed-estimate-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    {
        let writing = Database::open(name).expect("a file name starts a native database");
        for sql in [
            "CREATE TABLE company(id INTEGER, country VARCHAR, name VARCHAR)",
            // Eight thousand of the twenty thousand companies are `us` and the rest are spread over
            // a hundred and twenty other codes, so the distinct count says about 165.
            "INSERT INTO company SELECT i, CASE WHEN i % 5 < 2 THEN 'us' \
             ELSE 'c' || (i % 200)::VARCHAR END, 'name ' || i::VARCHAR FROM range(20000) r(i)",
            "CREATE TABLE placed(company INTEGER, movie INTEGER)",
            "INSERT INTO placed SELECT i * 7 % 20000, i FROM range(100000) r(i)",
        ] {
            writing.execute(sql).expect("the table is made");
        }
        writing.execute("CHECKPOINT").expect("the tables are committed");
    }
    let file = Database::open(name).expect("the written file opens again");
    let explained: String = file
        .query(
            "EXPLAIN SELECT min(c.name), min(p.movie) FROM company c, placed p \
             WHERE c.country = 'us' AND p.company = c.id",
        )
        .expect("the plan is explained")
        .rows()
        .flatten()
        .map(|value| format!("{value}\n"))
        .collect();
    let _ = std::fs::remove_file(&path);
    let line = explained
        .lines()
        .find(|line| line.contains("'us'"))
        .unwrap_or_else(|| panic!("no filter on the country in {explained}"));
    assert!(line.contains("~8000 rows estimated from frequency synopsis"), "{explained}");
}
