//! A load into a table that declared its row order comes out of the file pruned.
//!
//! Stage 1 of `$HOME/notes/Spec/2140/tenx`, end to end and at a size small enough to run in a
//! test. The scan has always built a low and a high value per part out of whatever order the rows
//! arrived in, and has always ruled parts out with them, so the only reason TPC-H reads six
//! million rows to answer from thirty thousand is that the rows arrive in an order where every
//! part's range is the whole table's range. Declaring the order and sorting the load is the fix,
//! and there is no new pruning here, only rows put where the existing pruning can reach them.
//!
//! Both tests ask for the pruning and the answer together. The answer on its own would pass with
//! the declaration ignored, since sorting rows does not change what a query returns, and the
//! pruning on its own would pass on a predicate that matches nothing. The undeclared table beside
//! the declared one is what says the part count means anything: it is the same rows, the same
//! predicate and the same engine, and the order is the only thing that differs.

use rudb::Database;
use rudb_common::{Clustering, Field, LogicalType, Value, Width};

/// Two hundred thousand rows over six and a half years, in an order nothing can rule out.
///
/// The dates step by 7919 days modulo 2400, and 7919 is prime to 2400, so the sequence visits
/// every day in the range before it repeats one and any run of rows long enough to be a part
/// covers nearly the whole range. That is the property `dbgen` gives `l_shipdate` for free and the
/// one the declaration undoes. `l_orderkey` is scrambled the same way and for the same reason.
const ROWS: &str = "SELECT ((r * 7919) % 200000)::BIGINT AS l_orderkey, \
                    (r % 7 + 1)::INTEGER AS l_linenumber, \
                    (DATE '1992-01-01' + ((r * 7919) % 2400)::INTEGER)::DATE AS l_shipdate \
                    FROM range(200000) AS s(r)";

const SHAPE: &str = "(l_orderkey BIGINT, l_linenumber INTEGER, l_shipdate DATE)";

/// One calendar month of shipping dates, which is about one row in eighty of the table.
const MONTH: &str = "WHERE l_shipdate >= DATE '1995-09-01' AND l_shipdate < DATE '1995-10-01'";

/// The calendar quarter that month falls in, which is three times as many rows.
const QUARTER: &str = "WHERE l_shipdate >= DATE '1995-07-01' AND l_shipdate < DATE '1995-10-01'";

/// The three columns of the cut down lineitem these tests load.
fn fields() -> Vec<Field> {
    vec![
        Field::new("l_orderkey", LogicalType::BigInt),
        Field::new("l_linenumber", LogicalType::Integer),
        Field::new("l_shipdate", LogicalType::Date),
    ]
}

/// The stage 0 layout, which is `month(l_shipdate), l_orderkey, l_linenumber`.
fn stage_zero() -> Clustering {
    Clustering::new(vec![2, 0, 1], Width::Month, &fields()).expect("the stage 0 layout")
}

/// A database on disk holding the table, loaded and checkpointed, and the path to clean up.
///
/// Written and read by the same handle, which is enough here because the checkpoint is what moves
/// the rows into the file and the scan reads them from there afterwards. One thread throughout, so
/// that the order rows come back in is the order they are stored in rather than a race.
fn loaded(tag: &str, declare: Option<Clustering>) -> (Database, std::path::PathBuf) {
    let path =
        std::env::temp_dir().join(format!("rudb-clustered-{tag}-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("a file name starts a native database");
    database.execute("SET threads = 1").expect("sets the thread count");
    database.execute(&format!("CREATE TABLE lineitem {SHAPE}")).expect("creates");
    if let Some(clustering) = declare {
        database.with_catalog_mut(|catalog| {
            let table = catalog.resolve(&["lineitem"]).expect("resolves");
            catalog
                .table_mut(&table)
                .expect("the table is there")
                .cluster_by(Some(clustering))
                .expect("the columns are the table's");
        });
    }
    database.execute(&format!("INSERT INTO lineitem {ROWS}")).expect("loads");
    database.execute("CHECKPOINT").expect("commits");
    (database, path)
}

/// The scan line of an `EXPLAIN ANALYZE`, which is the line carrying the part counts.
fn scan_line(database: &Database, sql: &str) -> String {
    let result = database.query(&format!("EXPLAIN ANALYZE {sql}")).expect("the explain ran");
    let text = match result.value_at(0, 1) {
        Value::Varchar(text) => text,
        other => panic!("the plan came back as {other:?}"),
    };
    text.lines()
        .take_while(|line| !line.is_empty())
        .find(|line| line.contains("Get "))
        .unwrap_or_else(|| panic!("no scan on the tree:\n{text}"))
        .to_owned()
}

/// Every row of a result, as values.
fn rows(database: &Database, sql: &str) -> Vec<Vec<Value>> {
    let result = database.query(sql).expect("the query ran");
    (0..result.len())
        .map(|row| (0..result.width()).map(|column| result.value_at(row, column)).collect())
        .collect()
}

#[test]
fn a_load_into_a_declared_table_prunes_and_the_same_load_without_one_does_not() {
    let (sorted, sorted_path) = loaded("sorted", Some(stage_zero()));
    let (plain, plain_path) = loaded("plain", None);

    let query = format!("SELECT count(*), sum(l_orderkey) FROM lineitem {MONTH}");
    let wanted = rows(&plain, &query);
    assert_eq!(rows(&sorted, &query), wanted, "sorting the rows on the way in changed the answer");
    assert_ne!(
        wanted[0][0],
        Value::BigInt(0),
        "a predicate that matches nothing would prune everything and prove nothing"
    );

    let declared = scan_line(&sorted, &query);
    assert!(declared.contains("parts skipped"), "a declared order should prune: {declared}");

    let undeclared = scan_line(&plain, &query);
    assert!(
        !undeclared.contains("parts skipped"),
        "every part of the unsorted table spans nearly the whole range of dates, so nothing can be \
         ruled out, and a load that pruned without a declaration would mean this test is measuring \
         something other than the declaration: {undeclared}"
    );

    let _ = std::fs::remove_file(sorted_path);
    let _ = std::fs::remove_file(plain_path);
}

#[test]
fn the_rows_are_bucketed_by_the_declared_width_rather_than_sorted_on_the_column() {
    // The pruning above would come out of an ordinary sort on the date just as well, so it does
    // not say the width was honoured. What says it is the second key. Bucketed by month,
    // `l_orderkey` runs in order across the whole of a month and the dates inside that month are
    // in no order at all. Sorted on the date exactly, it would be the other way round. The first
    // of those is the key locality the joins want and it is the entire reason the width is part of
    // the declaration rather than the declaration being a plain column list.
    let (database, path) = loaded("width", Some(stage_zero()));

    // No ORDER BY on purpose: the answer is whatever order the rows are stored in, which is the
    // thing being asserted. The scan runs on one thread, set in `loaded`, so that order is stable.
    let month = rows(&database, &format!("SELECT l_shipdate, l_orderkey FROM lineitem {MONTH}"));
    assert!(month.len() > 1000, "a month should be thousands of rows, not {}", month.len());

    let keys: Vec<i64> = month
        .iter()
        .map(|row| match row[1] {
            Value::BigInt(key) => key,
            ref other => panic!("l_orderkey came back as {other:?}"),
        })
        .collect();
    assert!(
        keys.windows(2).all(|pair| pair[0] <= pair[1]),
        "the keys should run in order across the whole month"
    );
    let dates: Vec<i32> = month
        .iter()
        .map(|row| match row[0] {
            Value::Date(day) => day,
            ref other => panic!("l_shipdate came back as {other:?}"),
        })
        .collect();
    assert!(
        !dates.windows(2).all(|pair| pair[0] <= pair[1]),
        "the dates should not be in order inside the bucket, which is what makes the width a month \
         and not a day"
    );

    let _ = std::fs::remove_file(path);
}

/// A declaration that names no width buckets by the quarter, end to end and out of the file.
///
/// The width is the one thing about the layout that is a number rather than a shape, and
/// `14-the-partition-width.md` measured all four of them on SF1 in one sitting: 0.917 of the
/// unsorted file's instructions at a month against 0.898 at a quarter, because a narrow bucket
/// costs key locality and delta width and stops buying pruning above a quarter. So the width a
/// silent declaration gets is a quarter, and this asserts it the way the width is visible from
/// outside rather than by reading the constant back.
///
/// What makes it visible is the second key. Bucketed by quarter, `l_orderkey` runs in order across
/// a whole quarter. At the month this file used to default to, the same three months are three
/// runs of keys with a reset at each boundary, so the assertion below fails on a monthly default
/// and passes on a quarterly one.
#[test]
fn a_declaration_that_names_no_width_buckets_by_the_quarter() {
    let asked = Clustering::over(vec![2, 0, 1], &fields()).expect("a date leads the declaration");
    assert_eq!(asked.width(), Width::Quarter, "the width the suite measured at 0.898");
    let (database, path) = loaded("silent", Some(asked));

    let quarter =
        rows(&database, &format!("SELECT l_shipdate, l_orderkey FROM lineitem {QUARTER}"));
    assert!(quarter.len() > 3000, "a quarter should be thousands of rows, not {}", quarter.len());
    let keys: Vec<i64> = quarter
        .iter()
        .map(|row| match row[1] {
            Value::BigInt(key) => key,
            ref other => panic!("l_orderkey came back as {other:?}"),
        })
        .collect();
    assert!(
        keys.windows(2).all(|pair| pair[0] <= pair[1]),
        "the keys should run in order across the whole quarter, which they do not at a month"
    );

    let _ = std::fs::remove_file(path);
}

/// The declaration is reachable from SQL, and what it declares survives a reopen.
///
/// Everything above builds the declaration through `Table::cluster_by`, which is a Rust call, so
/// until this test the whole layout was invisible to anybody driving the engine through SQL. The
/// pin's grammar has no `CLUSTER BY` clause and the parser here is the pin's parser, so the
/// declaration arrives the way a relationship does, as a setting.
///
/// Three things are asserted and each one fails on its own kind of mistake. The setting reads back
/// as what was written, which a declaration that was applied and not recorded would fail. The file
/// prunes after a reopen, which a declaration that was recorded and not applied would fail, and
/// which is also what says the checkpoint wrote it down. And the answer is the one the undeclared
/// table gives, because a layout that changed an answer would be worse than no layout.
#[test]
fn a_clustering_declared_through_the_setting_reaches_the_file_and_survives_a_reopen() {
    let path =
        std::env::temp_dir().join(format!("rudb-clustered-setting-{}.rudb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let name = path.to_str().expect("a UTF-8 temporary path");
    let declaration = "lineitem(quarter(l_shipdate), l_orderkey, l_linenumber)";
    let query = format!("SELECT count(*), sum(l_orderkey) FROM lineitem {QUARTER}");

    let wanted = {
        let database = Database::open(name).expect("a file name starts a native database");
        database.execute("SET threads = 1").expect("sets the thread count");
        database.execute(&format!("CREATE TABLE lineitem {SHAPE}")).expect("creates");
        database
            .execute(&format!("SET cluster_by = '{declaration}'"))
            .expect("declares the order the rows are stored in");
        assert_eq!(
            database.setting("cluster_by").expect("a setting"),
            declaration,
            "the setting reads back as what was written"
        );
        database.execute(&format!("INSERT INTO lineitem {ROWS}")).expect("loads");
        database.execute("CHECKPOINT").expect("commits");
        rows(&database, &query)
    };

    // A second handle on the same file, which has none of the first one's session state. The only
    // thing it can know about the order is what the checkpoint wrote into the file.
    let reopened = Database::open(name).expect("the file opens again");
    reopened.execute("SET threads = 1").expect("sets the thread count");
    assert_eq!(rows(&reopened, &query), wanted, "the same rows out of the reopened file");
    let line = scan_line(&reopened, &query);
    assert!(line.contains("parts skipped"), "a declared order should prune: {line}");
    // Read back out of the file in the session that never set it, which is the whole argument for
    // the setting being a view of the catalog rather than something the session remembers.
    assert_eq!(reopened.setting("cluster_by").expect("a setting"), declaration);

    // A column the table does not have is refused with the table and the column named, and the
    // catalog is left alone, which is what says the resolve happens before anything is written.
    let complaint = reopened
        .execute("SET cluster_by = 'lineitem(l_shipdate, l_nosuchcolumn)'")
        .expect_err("no such column")
        .message()
        .to_string();
    assert!(complaint.contains("l_nosuchcolumn"), "{complaint}");
    assert!(scan_line(&reopened, &query).contains("parts skipped"), "the file still prunes");

    // A reset takes the declaration off the table it was on. Read through the catalog rather than
    // through a query, because the rows in the file are already in that order and a scan would go
    // on pruning whether the declaration were there or not, which is the whole difference between
    // what a table is declared to be and what it happens to be.
    assert!(
        reopened.with_catalog_mut(|catalog| {
            let table = catalog.resolve(&["lineitem"]).expect("resolves");
            catalog.table(&table).expect("the table is there").clustering().is_some()
        }),
        "the declaration is on the table before the reset"
    );
    reopened.execute("RESET cluster_by").expect("clears the declaration");
    assert_eq!(reopened.setting("cluster_by").expect("a setting"), "");
    assert!(
        reopened.with_catalog_mut(|catalog| {
            let table = catalog.resolve(&["lineitem"]).expect("resolves");
            catalog.table(&table).expect("the table is there").clustering().is_none()
        }),
        "and off it afterwards"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn a_load_into_a_table_that_declared_nothing_is_left_in_the_order_it_arrived() {
    // The sort is only ever added because a declaration asked for it, so the ordinary insert has
    // to come out the way it went in. A sort that got added regardless would pass both tests above
    // and would be a pass over every load in the engine that nobody asked for.
    let (database, path) = loaded("asis", None);
    let first = rows(&database, "SELECT l_orderkey FROM lineitem LIMIT 4");
    let wanted: Vec<Vec<Value>> =
        (0..4).map(|r: i64| vec![Value::BigInt((r * 7919) % 200_000)]).collect();
    assert_eq!(first, wanted, "an undeclared load was reordered");
    let _ = std::fs::remove_file(path);
}
