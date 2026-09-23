use rudb::Database;
use rudb_common::Value;

#[test]
fn a_native_row_count_reuses_its_plan_until_the_table_or_settings_change() {
    let path = std::env::temp_dir().join(format!(
        "rudb-count-plan-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("open the file");
    database.execute("CREATE TABLE hits (i INTEGER)").expect("create the table");
    database.execute("INSERT INTO hits VALUES (1), (2)").expect("insert rows");
    database.close().expect("persist the native table");

    let database = Database::open(name).expect("reopen the native table");
    let sql = "SELECT COUNT(*) FROM hits";
    let first = database.execute(sql).expect("first count");
    assert_eq!(first.value_at(0, 0), Value::BigInt(2));
    assert!(first.metrics().expect("metrics").timing.bind_ns > 0);

    let second = database.execute(sql).expect("repeated count");
    assert_eq!(second.value_at(0, 0), Value::BigInt(2));
    assert_eq!(second.metrics().expect("metrics").timing.bind_ns, 0);

    let nonzero = "SELECT COUNT(*) FROM hits WHERE i <> 0";
    let first_nonzero = database.execute(nonzero).expect("first filtered count");
    assert_eq!(first_nonzero.value_at(0, 0), Value::BigInt(2));
    assert!(first_nonzero.metrics().expect("metrics").timing.bind_ns > 0);
    let repeated_nonzero = database.execute(nonzero).expect("repeated filtered count");
    assert_eq!(repeated_nonzero.value_at(0, 0), Value::BigInt(2));
    assert_eq!(repeated_nonzero.metrics().expect("metrics").timing.bind_ns, 0);

    let filtered = "SELECT COUNT(*) FROM hits WHERE i > 1";
    for _ in 0..2 {
        let answer = database.execute(filtered).expect("filtered count");
        assert_eq!(answer.value_at(0, 0), Value::BigInt(1));
        assert!(answer.metrics().expect("metrics").timing.bind_ns > 0);
    }

    database.execute("SET default_order = 'DESC'").expect("change a setting");
    let after_setting = database.query(nonzero).expect("count after setting");
    assert_eq!(after_setting.value_at(0, 0), Value::BigInt(2));
    assert!(after_setting.metrics().expect("metrics").timing.bind_ns > 0);

    database.execute("INSERT INTO hits VALUES (3)").expect("grow the table");
    let after_insert = database.execute(sql).expect("count after insert");
    assert_eq!(after_insert.value_at(0, 0), Value::BigInt(3));
    assert!(after_insert.metrics().expect("metrics").timing.bind_ns > 0);
    let grown_again = database.execute(sql).expect("count the grown table again");
    assert_eq!(grown_again.value_at(0, 0), Value::BigInt(3));
    assert!(grown_again.metrics().expect("metrics").timing.bind_ns > 0);
    let grown_nonzero = database.execute(nonzero).expect("count nonzero values in the grown table");
    assert_eq!(grown_nonzero.value_at(0, 0), Value::BigInt(3));
    assert!(grown_nonzero.metrics().expect("metrics").timing.bind_ns > 0);
    database.close().expect("close the file");
    std::fs::remove_file(path).expect("remove the fixture");
}

#[test]
fn a_native_three_aggregate_summary_reuses_its_plan_until_the_table_changes() {
    let path = std::env::temp_dir().join(format!(
        "rudb-summary-plan-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("open the file");
    database.execute("CREATE TABLE hits (i INTEGER, j INTEGER)").expect("create the table");
    database.execute("INSERT INTO hits VALUES (1, 10), (2, 20)").expect("insert rows");
    database.close().expect("persist the native table");

    let database = Database::open(name).expect("reopen the native table");
    let sql = "SELECT SUM(i), COUNT(*), AVG(j) FROM hits";
    let first = database.execute(sql).expect("first summary");
    assert_eq!(first.value_at(0, 1), Value::BigInt(2));
    assert!(first.metrics().expect("metrics").timing.bind_ns > 0);
    let repeated = database.execute(sql).expect("repeated summary");
    assert_eq!(repeated.value_at(0, 0), first.value_at(0, 0));
    assert_eq!(repeated.value_at(0, 1), first.value_at(0, 1));
    assert_eq!(repeated.value_at(0, 2), first.value_at(0, 2));
    assert_eq!(repeated.metrics().expect("metrics").timing.bind_ns, 0);

    let derived = "SELECT SUM(i + 1), COUNT(*), AVG(j) FROM hits";
    for _ in 0..2 {
        let answer = database.execute(derived).expect("derived aggregate");
        assert!(answer.metrics().expect("metrics").timing.bind_ns > 0);
    }

    database.execute("SET default_order = 'DESC'").expect("change a setting");
    let after_setting = database.execute(sql).expect("summary after setting");
    assert!(after_setting.metrics().expect("metrics").timing.bind_ns > 0);

    database.execute("INSERT INTO hits VALUES (3, NULL)").expect("grow the table");
    let after_insert = database.execute(sql).expect("summary after insert");
    assert_eq!(after_insert.value_at(0, 1), Value::BigInt(3));
    assert_eq!(after_insert.value_at(0, 2), first.value_at(0, 2));
    assert_ne!(after_insert.value_at(0, 0), first.value_at(0, 0));
    assert!(after_insert.metrics().expect("metrics").timing.bind_ns > 0);
    database.close().expect("close the file");
    std::fs::remove_file(path).expect("remove the fixture");
}

#[test]
fn a_native_average_reuses_its_plan_until_the_table_changes() {
    let path = std::env::temp_dir().join(format!(
        "rudb-average-plan-{}-{}.rdb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock advances")
            .as_nanos()
    ));
    let name = path.to_str().expect("a UTF-8 temporary path");
    let database = Database::open(name).expect("open the file");
    database.execute("CREATE TABLE hits (i INTEGER)").expect("create the table");
    database.execute("INSERT INTO hits VALUES (1), (2), (NULL)").expect("insert rows");
    database.close().expect("persist the native table");

    let database = Database::open(name).expect("reopen the native table");
    let sql = "SELECT AVG(i) FROM hits";
    let first = database.execute(sql).expect("first average");
    assert_eq!(first.value_at(0, 0), Value::Double(1.5));
    assert!(first.metrics().expect("metrics").timing.bind_ns > 0);
    let repeated = database.execute(sql).expect("repeated average");
    assert_eq!(repeated.value_at(0, 0), first.value_at(0, 0));
    assert_eq!(repeated.metrics().expect("metrics").timing.bind_ns, 0);

    let derived = "SELECT AVG(i + 1) FROM hits";
    for _ in 0..2 {
        let answer = database.execute(derived).expect("derived average");
        assert!(answer.metrics().expect("metrics").timing.bind_ns > 0);
    }

    database.execute("SET default_order = 'DESC'").expect("change a setting");
    let after_setting = database.execute(sql).expect("average after setting");
    assert_eq!(after_setting.value_at(0, 0), first.value_at(0, 0));
    assert!(after_setting.metrics().expect("metrics").timing.bind_ns > 0);

    database.execute("INSERT INTO hits VALUES (5)").expect("grow the table");
    let after_insert = database.execute(sql).expect("average after insert");
    assert_ne!(after_insert.value_at(0, 0), first.value_at(0, 0));
    assert!(after_insert.metrics().expect("metrics").timing.bind_ns > 0);
    database.close().expect("close the file");
    std::fs::remove_file(path).expect("remove the fixture");
}
