//! The PostgreSQL shims the pin defines as macros, `pg_typeof`, the `has_*_privilege` pairs, the
//! `pg_*_is_visible` family and the constants, with `days_in_month` and the two halves of
//! `md5_number`.
//!
//! Every expected answer here was taken from the pinned duckdb binary and not from rudb.

use rudb::Database;

fn rows(database: &Database, sql: &str) -> Vec<String> {
    let result = database.query(sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    result
        .rows()
        .map(|row| row.iter().map(ToString::to_string).collect::<Vec<_>>().join("|"))
        .collect()
}

fn answered(sql: &str) -> String {
    rows(&Database::new(), sql).join("\n")
}

fn refused(sql: &str) -> String {
    Database::new().query(sql).unwrap_err().to_string()
}

#[test]
fn the_privilege_and_visibility_shims_answer_true() {
    let cases = [
        (
            "SELECT has_table_privilege('t', 'select'), has_table_privilege('u', 't', 'select'), has_column_privilege(1,2,3,4), pg_table_is_visible(1), pg_has_role('r', 'x')",
            "true|true|true|true|true",
        ),
        (
            "SELECT has_any_column_privilege('t', 'p'), has_database_privilege('d', 'p'), has_schema_privilege('u', 's', 'p'), has_sequence_privilege('s', 'p'), has_function_privilege('f', 'p')",
            "true|true|true|true|true",
        ),
        (
            "SELECT pg_type_is_visible(1), pg_function_is_visible(1), pg_operator_is_visible(1), pg_opfamily_is_visible(1), pg_ts_dict_is_visible(1)",
            "true|true|true|true|true",
        ),
        (
            "SELECT pg_catalog.has_table_privilege('t', 'select'), pg_catalog.pg_typeof(1)",
            "true|integer",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn the_constant_shims_answer_their_constant() {
    let cases = [
        (
            "SELECT inet_client_addr(), inet_server_port(), typeof(inet_client_addr()), col_description(1, 2), obj_description(1, 'x'), shobj_description(1,'y')",
            "NULL|NULL|\"NULL\"|NULL|NULL|NULL",
        ),
        (
            "SELECT pg_is_other_temp_schema(1), pg_my_temp_schema(), typeof(pg_my_temp_schema()), pg_get_expr('abc', 1)",
            "false|0|INTEGER|abc",
        ),
        (
            "SELECT pg_conf_load_time() = current_timestamp, typeof(pg_postmaster_start_time())",
            "true|TIMESTAMP WITH TIME ZONE",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn pg_typeof_is_the_lower_case_type() {
    assert_eq!(
        answered(
            "SELECT pg_typeof(1), pg_typeof('a'), pg_typeof([1]), pg_typeof(NULL), typeof(pg_typeof(1))"
        ),
        "integer|varchar|integer[]|\"null\"|VARCHAR"
    );
    assert_eq!(
        rows(&Database::new(), "SELECT pg_typeof(x) FROM (VALUES (1), (2)) t(x)"),
        ["integer", "integer"]
    );
}

#[test]
fn days_in_month_and_the_md5_halves_expand_the_way_the_pin_expands_them() {
    let cases = [
        (
            "SELECT days_in_month(DATE '2024-02-10'), days_in_month(TIMESTAMP '2023-02-01 10:00'), days_in_month(NULL), typeof(days_in_month(DATE '2024-02-10'))",
            "29|28|NULL|BIGINT",
        ),
        (
            "SELECT md5_number_lower('abc'), md5_number_upper('abc'), typeof(md5_number_lower('abc'))",
            "8250560606382298838|12704604231530709392|UBIGINT",
        ),
        (
            "SELECT last_day(NULL), typeof(last_day(NULL)), dayname(NULL), typeof(dayname(NULL)), monthname(NULL)",
            "NULL|DATE|NULL|VARCHAR|NULL",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
    assert_eq!(
        rows(
            &Database::new(),
            "SELECT days_in_month(d) FROM (VALUES (DATE '2023-01-05'), (DATE '2023-04-01'), (NULL)) t(d)"
        ),
        ["31", "30", "NULL"]
    );
}

#[test]
fn a_shim_called_with_the_wrong_arguments_is_refused_in_the_pins_words() {
    let cases = [
        (
            "SELECT has_table_privilege(1)",
            "Binder Error: Macro has_table_privilege() does not support the supplied arguments. You might need to add explicit type casts.\nCandidate macros:\n\thas_table_privilege(table, privilege)\n\thas_table_privilege(user, table, privilege)",
        ),
        (
            "SELECT pg_typeof()",
            "Binder Error: Macro pg_typeof() does not support the supplied arguments. You might need to add explicit type casts.\nCandidate macros:\n\tpg_typeof(expression)",
        ),
        (
            "SELECT days_in_month(1)",
            "Binder Error: No function matches the given name and argument types 'last_day(INTEGER_LITERAL)'. You might need to add explicit type casts.\n\tCandidate functions:\n\tlast_day(col0 DATE) -> DATE\n\tlast_day(col0 TIMESTAMP) -> DATE\n\tlast_day(col0 TIMESTAMP WITH TIME ZONE) -> DATE\n",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}
