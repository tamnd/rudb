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

    let filtered = "SELECT COUNT(*) FROM hits WHERE i > 1";
    for _ in 0..2 {
        let answer = database.execute(filtered).expect("filtered count");
        assert_eq!(answer.value_at(0, 0), Value::BigInt(1));
        assert!(answer.metrics().expect("metrics").timing.bind_ns > 0);
    }

    database.execute("SET default_order = 'DESC'").expect("change a setting");
    let after_setting = database.query(sql).expect("count after setting");
    assert_eq!(after_setting.value_at(0, 0), Value::BigInt(2));
    assert!(after_setting.metrics().expect("metrics").timing.bind_ns > 0);

    database.execute("INSERT INTO hits VALUES (3)").expect("grow the table");
    let after_insert = database.execute(sql).expect("count after insert");
    assert_eq!(after_insert.value_at(0, 0), Value::BigInt(3));
    assert!(after_insert.metrics().expect("metrics").timing.bind_ns > 0);
    let grown_again = database.execute(sql).expect("count the grown table again");
    assert_eq!(grown_again.value_at(0, 0), Value::BigInt(3));
    assert!(grown_again.metrics().expect("metrics").timing.bind_ns > 0);
    database.close().expect("close the file");
    std::fs::remove_file(path).expect("remove the fixture");
}
