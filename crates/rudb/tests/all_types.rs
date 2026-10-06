//! `test_all_types()`, one column of every type with its least value, its greatest and a null.
//! Every expected answer here was taken from the pinned duckdb binary, v2.0.0-dev84237.

use rudb::Database;

/// Every row of `sql` as the shell writes it.
fn answered(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    let width = result.rows().next().map_or(0, |row| row.len());
    (0..result.len())
        .map(|row| {
            (0..width).map(|column| result.text_at(row, column)).collect::<Vec<_>>().join("|")
        })
        .collect()
}

fn refused(database: &Database, sql: &str, expected: &str) {
    let error = database.execute(sql).expect_err(sql);
    assert!(error.to_string().starts_with(expected), "{sql}: {error}");
}

#[test]
fn the_columns_are_the_pins_in_its_order_but_geometry() {
    let database = Database::new();
    let names = answered(
        &database,
        "SELECT string_agg(column_name, ',') FROM (DESCRIBE SELECT * FROM test_all_types())",
    );
    assert_eq!(
        names,
        ["bool,tinyint,smallint,int,bigint,hugeint,uhugeint,utinyint,usmallint,uint,ubigint,\
             bignum,date,time,timestamp,timestamp_s,timestamp_ms,timestamp_ns,time_tz,timestamp_tz,\
             timestamp_tz_ns,float,double,dec_4_1,dec_9_4,dec_18_6,dec38_10,uuid,interval,varchar,\
             blob,bit,small_enum,medium_enum,large_enum,int_array,double_array,date_array,\
             timestamp_array,timestamptz_array,varchar_array,nested_int_array,struct,empty_struct,\
             struct_of_arrays,array_of_structs,map,union,fixed_int_array,fixed_varchar_array,\
             fixed_nested_int_array,fixed_nested_varchar_array,fixed_struct_array,\
             struct_of_fixed_array,fixed_array_of_int_list,list_of_fixed_int_array,time_ns,tuple"]
    );
}

#[test]
fn each_column_holds_its_least_its_greatest_and_a_null() {
    let database = Database::new();
    for (column, expected) in [
        ("tinyint", ["-128|TINYINT", "127|TINYINT", "NULL|TINYINT"]),
        (
            "hugeint",
            [
                "-170141183460469231731687303715884105728|HUGEINT",
                "170141183460469231731687303715884105727|HUGEINT",
                "NULL|HUGEINT",
            ],
        ),
        ("date", ["5877642-06-25 (BC)|DATE", "5881580-07-10|DATE", "NULL|DATE"]),
        (
            "timestamp",
            [
                "290309-12-22 (BC) 00:00:00|TIMESTAMP",
                "294247-01-10 04:00:54.775806|TIMESTAMP",
                "NULL|TIMESTAMP",
            ],
        ),
        (
            "time_tz",
            [
                "00:00:00+15:59:59|TIME WITH TIME ZONE",
                "24:00:00-15:59:59|TIME WITH TIME ZONE",
                "NULL|TIME WITH TIME ZONE",
            ],
        ),
        (
            "interval",
            [
                "00:00:00|INTERVAL",
                "83 years 3 months 999 days 00:16:39.999999|INTERVAL",
                "NULL|INTERVAL",
            ],
        ),
        (
            "small_enum",
            [
                "DUCK_DUCK_ENUM|ENUM('DUCK_DUCK_ENUM', 'GOOSE')",
                "GOOSE|ENUM('DUCK_DUCK_ENUM', 'GOOSE')",
                "NULL|ENUM('DUCK_DUCK_ENUM', 'GOOSE')",
            ],
        ),
        (
            "varchar_array",
            ["[]|VARCHAR[]", "[🦆🦆🦆🦆🦆🦆, goose, NULL, '']|VARCHAR[]", "NULL|VARCHAR[]"],
        ),
        ("empty_struct", ["{}|STRUCT", "{}|STRUCT", "NULL|STRUCT"]),
        (
            "union",
            [
                "Frank|UNION(\"name\" VARCHAR, age SMALLINT)",
                "5|UNION(\"name\" VARCHAR, age SMALLINT)",
                "NULL|UNION(\"name\" VARCHAR, age SMALLINT)",
            ],
        ),
        (
            "fixed_nested_int_array",
            [
                "[[NULL, 2, 3], NULL, [NULL, 2, 3]]|INTEGER[3][3]",
                "[[4, 5, 6], [NULL, 2, 3], [4, 5, 6]]|INTEGER[3][3]",
                "NULL|INTEGER[3][3]",
            ],
        ),
        (
            "tuple",
            [
                "(NULL, NULL)|TUPLE(INTEGER, VARCHAR)",
                "(42, 🦆🦆🦆🦆🦆🦆)|TUPLE(INTEGER, VARCHAR)",
                "NULL|TUPLE(INTEGER, VARCHAR)",
            ],
        ),
    ] {
        let sql = format!("SELECT \"{column}\", typeof(\"{column}\") FROM test_all_types()");
        assert_eq!(answered(&database, &sql), expected, "{sql}");
    }
    let sql = "SELECT hex(varchar::BLOB) FROM test_all_types() WHERE varchar LIKE 'goo%'";
    assert_eq!(answered(&database, sql), ["676F6F007365"], "{sql}");
}

#[test]
fn the_arguments_are_the_two_named_flags() {
    let database = Database::new();
    refused(
        &database,
        "SELECT * FROM test_all_types(1)",
        "Binder Error: No function matches the given name and argument types \
         'test_all_types(INTEGER)'",
    );
    refused(
        &database,
        "SELECT * FROM test_all_types(foo := 1)",
        "Binder Error: Invalid named parameter \"foo\" for function test_all_types",
    );
    refused(
        &database,
        "SELECT * FROM test_all_types(use_large_enum := NULL)",
        "Invalid Input Error: Cannot use NULL as argument for use_large_enum",
    );
    refused(
        &database,
        "SELECT * FROM test_all_types(use_large_bignum := NULL)",
        "Invalid Input Error: Cannot use NULL as argument for use_large_bignum",
    );
    let sql = "SELECT count(*), max(large_enum) FROM test_all_types(use_large_enum := false)";
    assert_eq!(answered(&database, sql), ["3|enum_69999"], "{sql}");
}
