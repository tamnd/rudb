//! The load profile a bulk load leaves behind, read through `rudb_write_metrics()`.
//!
//! What these hold down is that the profile adds up to the load. Every row that went in is counted
//! once by the stage that converted it and once by the stage that wrote it, and the bytes the
//! stages say they wrote are the file, less the header and the commit slot that nothing charges. A
//! profile that disagreed with the file it describes would be a number nobody could act on.

use rudb::Database;
use rudb_common::Value;

/// A path nothing else is using, with no database at it yet.
fn scratch(tag: &str) -> std::path::PathBuf {
    let path =
        std::env::temp_dir().join(format!("rudb-loadprofile-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

fn number(value: &Value) -> i64 {
    value.to_string().parse().expect("an integer column")
}

/// The rows of the newest load into `target`, stage and the column asked for.
fn stages(database: &Database, target: &str, column: &str) -> Vec<(String, i64)> {
    let result = database
        .query(&format!(
            "SELECT stage, {column} FROM rudb_write_metrics() WHERE target = '{target}' \
             AND load = (SELECT max(load) FROM rudb_write_metrics() WHERE target = '{target}')"
        ))
        .expect("the load profile reads");
    result.rows().map(|row| (row[0].to_string(), number(&row[1]))).collect()
}

#[test]
fn a_create_table_as_leaves_a_profile_that_adds_up_to_the_file() {
    let path = scratch("ctas");
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database
        .execute(
            "CREATE TABLE profiled AS SELECT range AS a, 'v' || (range % 7) AS b \
             FROM range(300000)",
        )
        .expect("loads");

    let rows = stages(&database, "profiled", "rows");
    let named: Vec<&str> = rows.iter().map(|(stage, _)| stage.as_str()).collect();
    assert_eq!(named, ["convert", "page builder", "dictionary", "write", "publish", "total"]);
    let rows_of = |stage: &str| rows.iter().find(|(name, _)| name == stage).map(|(_, n)| *n);
    assert_eq!(rows_of("convert"), Some(300_000));
    assert_eq!(rows_of("page builder"), Some(300_000));
    assert_eq!(rows_of("write"), Some(300_000));
    assert_eq!(rows_of("total"), Some(300_000));

    let written = stages(&database, "profiled", "bytes_out");
    let total = written.iter().find(|(stage, _)| stage == "total").map(|(_, n)| *n).unwrap();
    let file = i64::try_from(std::fs::metadata(&path).expect("the file is there").len()).unwrap();
    assert!(total > 0 && total <= file, "{total} bytes charged to a file of {file}");
    assert!(file - total < 4096, "{} bytes of the file nobody charged", file - total);

    let running = database
        .value(
            "SELECT count(*) FROM rudb_write_metrics() WHERE target = 'profiled' AND NOT finished",
        )
        .expect("reads");
    assert_eq!(running.to_string(), "0");
    drop(database);
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_load_reports_what_it_held_on_its_total_row() {
    let path = scratch("memory");
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    database
        .execute(
            "CREATE TABLE remembered AS SELECT range AS a, 'v' || (range % 1000) AS b \
             FROM range(300000)",
        )
        .expect("loads");
    let result = database
        .query(
            "SELECT stage, accounted_peak, peak_rss FROM rudb_write_metrics() \
             WHERE target = 'remembered'",
        )
        .expect("the load profile reads");
    let mut totals = 0;
    for row in result.rows() {
        if row[0].to_string() != "total" {
            assert!(row[1].is_null() && row[2].is_null(), "a stage row has a peak: {row:?}");
            continue;
        }
        totals += 1;
        // At least the rows of one stripe's first column were held at once before it was written.
        let accounted = number(&row[1]);
        assert!(accounted >= 8 * 65_536, "{accounted} bytes accounted for 300,000 rows");
        if cfg!(target_os = "linux") {
            let resident = number(&row[2]);
            assert!(resident >= accounted, "{resident} resident under {accounted} accounted");
        }
    }
    assert_eq!(totals, 1);
    drop(database);
    std::fs::remove_file(path).expect("removes the temporary database");
}

#[test]
fn a_statement_that_never_loaded_leaves_no_profile() {
    let database = Database::new();
    database
        .execute("CREATE TABLE in_memory AS SELECT range AS a FROM range(10)")
        .expect("creates");
    let count = database
        .value("SELECT count(*) FROM rudb_write_metrics() WHERE target = 'in_memory'")
        .expect("reads");
    assert_eq!(count.to_string(), "0");
}

#[test]
fn a_load_counts_what_each_codec_it_offered_cost() {
    let path = scratch("codecs");
    let name = path.to_str().expect("a UTF-8 temporary path").to_owned();
    let database = Database::open(&name).expect("a file name starts a native database");
    let integers = |column: &str| -> i64 {
        let result = database
            .query(&format!(
                "SELECT sum({column}) FROM rudb_codec_metrics() WHERE family = 'integer'"
            ))
            .expect("the codec metrics read");
        number(&result.rows().next().expect("one row")[0])
    };
    let (offers, kept) = (integers("offers"), integers("kept"));
    database
        .execute(
            "CREATE TABLE coded AS SELECT range AS a, 'v' || (range % 7) AS b FROM range(300000)",
        )
        .expect("loads");
    // Other tests load in this process too, so the counts can only be said to have gone up.
    assert!(integers("offers") > offers);
    assert!(integers("kept") > kept);
    assert!(integers("kept") <= integers("offers"));

    let shares = database
        .query(
            "SELECT family, sum(share) FROM rudb_codec_metrics() GROUP BY family ORDER BY family",
        )
        .expect("the codec metrics read");
    for row in shares.rows() {
        let total: f64 = row[1].to_string().parse().expect("a share");
        assert!((total - 1.0).abs() < 1e-9, "{} shares add up to {total}", row[0]);
    }
    drop(database);
    let _ = std::fs::remove_file(&path);
}
