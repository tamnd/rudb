//! `list_distance`, `list_inner_product`, `list_negative_inner_product`, `list_cosine_similarity`,
//! `list_cosine_distance` and the names the pin keeps for them.
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

fn table(ty: &str, values: &str) -> Database {
    let database = Database::new();
    for sql in
        [format!("CREATE TABLE lists (l {ty}[])"), format!("INSERT INTO lists VALUES {values}")]
    {
        database.execute(&sql).unwrap_or_else(|error| panic!("{sql} failed: {error}"));
    }
    database
}

#[test]
fn two_lists_fold_into_the_number_the_pin_answers() {
    let cases = [
        (
            "SELECT list_distance([1,2,3], [4,5,6]), typeof(list_distance([1,2,3], [4,5,6]))",
            "5.196152422706632|DOUBLE",
        ),
        (
            "SELECT list_distance([1.0,2.0]::FLOAT[], [4,5]::FLOAT[]), typeof(list_distance([1.0,2.0]::FLOAT[], [4,5]::FLOAT[]))",
            "4.2426405|FLOAT",
        ),
        ("SELECT list_distance([1.0,2.0]::FLOAT[], [4,5]::DOUBLE[])", "4.242640687119285"),
        (
            "SELECT list_cosine_similarity([1,2,3], [4,5,6]), list_cosine_distance([1,2,3], [4,5,6])",
            "0.9746318461970762|0.025368153802923787",
        ),
        (
            "SELECT list_cosine_similarity([1,2,3]::FLOAT[], [4,5,6]::FLOAT[]), list_cosine_distance([1,2,3]::FLOAT[], [4,5,6]::FLOAT[])",
            "0.9746319|0.025368094",
        ),
        (
            "SELECT list_inner_product([1,2,3], [4,5,6]), list_dot_product([1,2,3], [4,5,6]), list_negative_inner_product([1,2,3], [4,5,6]), list_negative_dot_product([1,2,3], [4,5,6])",
            "32.0|32.0|-32.0|-32.0",
        ),
        (
            "SELECT list_distance([], []), list_inner_product([], []), list_cosine_similarity([], []) IS NULL, list_cosine_distance([], []) IS NULL",
            "0.0|0.0|true|true",
        ),
        // The pin clamps with std::max and std::min in an order that turns a NaN into -1.
        (
            "SELECT list_cosine_similarity([0,0], [1,2]), list_cosine_distance([0,0], [1,2]), list_cosine_similarity([1e200, 1e200], [1e200, 1e200])",
            "-1.0|2.0|-1.0",
        ),
        (
            "SELECT list_cosine_distance([3::FLOAT, 4::FLOAT], [4::FLOAT, 3::FLOAT]), list_inner_product([1e30::FLOAT], [1e30::FLOAT])",
            "0.04000002|inf",
        ),
        (
            "SELECT \"<->\"([1.0], [2.0]), list_distance(NULL, [1,2]), list_distance([1.0,NULL], NULL)",
            "1.0|NULL|NULL",
        ),
        (
            "SELECT typeof(list_distance([1,2]::BIGINT[], [3,4]::FLOAT[])), typeof(list_inner_product([NULL], [1.0::FLOAT])), typeof(list_inner_product([], [])), typeof(list_distance([1.5], [2]))",
            "FLOAT|FLOAT|DOUBLE|DOUBLE",
        ),
        (
            "SELECT typeof(list_distance(NULL, [1,2])), typeof(list_distance([1.0]::FLOAT[], NULL)), list_distance([1,2]::DECIMAL(4,1)[], [3,4]::DECIMAL(4,1)[])",
            "DOUBLE|FLOAT|2.8284271247461903",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(answered(sql), expected, "{sql}");
    }
}

#[test]
fn a_column_is_folded_row_by_row() {
    let values = "([1, 2, 3]), ([1, 2, 4]), ([7, 8, 9]), ([-1, -2, -3]), (NULL)";
    let floats = table("FLOAT", values);
    let sql = "SELECT list_distance(l, [1, 2, 3]), list_cosine_similarity(l, [1, 2, 3]), list_inner_product(l, [1,2,3]), typeof(list_distance(l, [1,2,3])) FROM lists";
    assert_eq!(
        rows(&floats, sql),
        [
            "0.0|1.0|14.0|FLOAT",
            "1.0|0.9914601|17.0|FLOAT",
            "10.392304|0.9594119|50.0|FLOAT",
            "7.483315|-1.0|-14.0|FLOAT",
            "NULL|NULL|NULL|FLOAT",
        ]
    );
    let doubles = table("DOUBLE", values);
    let sql = "SELECT list_distance(l, [1, 2, 3]), list_cosine_similarity(l, [1, 2, 3]), list_negative_dot_product(l, l), list_cosine_distance([1,2,3], l) FROM lists";
    assert_eq!(
        rows(&doubles, sql),
        [
            "0.0|1.0|-14.0|0.0",
            "1.0|0.9914601339836673|-21.0|0.00853986601633272",
            "10.392304845413264|0.9594119455666703|-194.0|0.04058805443332969",
            "7.483314773547883|-1.0|-14.0|2.0",
            "NULL|NULL|NULL|NULL",
        ]
    );
    let holed = table("DOUBLE", "([1, 2]), (NULL), ([NULL, 1.0])");
    let error = holed.query("SELECT list_distance(l, [1, 2]) FROM lists").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid Input Error: list_distance: left argument can not contain NULL values"
    );
    let short = table("DOUBLE", "([1, 2]), ([1])");
    let error = short.query("SELECT list_distance([1, 2], l) FROM lists").unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid Input Error: list_distance: list dimensions must be equal, got left length '2' and right length '1'"
    );
}

#[test]
fn what_the_pin_refuses_is_refused_in_its_words() {
    let cases = [
        (
            "SELECT list_distance([1,2], [1,2,3])",
            "Invalid Input Error: list_distance: list dimensions must be equal, got left length '2' and right length '3'",
        ),
        (
            "SELECT list_distance([1,NULL], [1,2])",
            "Invalid Input Error: list_distance: left argument can not contain NULL values",
        ),
        (
            "SELECT list_distance([1,2], [NULL,2])",
            "Invalid Input Error: list_distance: right argument can not contain NULL values",
        ),
        (
            "SELECT list_negative_dot_product([NULL], [1])",
            "Invalid Input Error: list_negative_dot_product: left argument can not contain NULL values",
        ),
        (
            "SELECT \"<->\"([1.0], [2.0, 3.0])",
            "Invalid Input Error: \"<->\": list dimensions must be equal, got left length '1' and right length '2'",
        ),
        (
            "SELECT list_distance(['a'], ['b'])",
            "Binder Error: No function matches the given name and argument types 'list_distance(VARCHAR[], VARCHAR[])'. You might need to add explicit type casts.\n\tCandidate functions:\n\tlist_distance(col0 FLOAT[], col1 FLOAT[]) -> FLOAT\n\tlist_distance(col0 DOUBLE[], col1 DOUBLE[]) -> DOUBLE\n",
        ),
        (
            "SELECT list_dot_product([1])",
            "Binder Error: No function matches the given name and argument types 'list_dot_product(INTEGER[])'. You might need to add explicit type casts.\n\tCandidate functions:\n\tlist_dot_product(col0 FLOAT[], col1 FLOAT[]) -> FLOAT\n\tlist_dot_product(col0 DOUBLE[], col1 DOUBLE[]) -> DOUBLE\n",
        ),
        (
            "SELECT typeof(list_distance([true], [1.0::FLOAT]))",
            "Binder Error: No function matches the given name and argument types 'list_distance(BOOLEAN[], FLOAT[])'. You might need to add explicit type casts.\n\tCandidate functions:\n\tlist_distance(col0 FLOAT[], col1 FLOAT[]) -> FLOAT\n\tlist_distance(col0 DOUBLE[], col1 DOUBLE[]) -> DOUBLE\n",
        ),
        (
            "SELECT list_distance(NULL, NULL)",
            "Binder Error: Could not choose a best candidate function for the function call \"list_distance(\"NULL\", \"NULL\")\". In order to select one, please add explicit type casts.\n\tCandidate functions:\n\tlist_distance(col0 DOUBLE[], col1 DOUBLE[]) -> DOUBLE\n\tlist_distance(col0 FLOAT[], col1 FLOAT[]) -> FLOAT\n",
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(refused(sql), expected, "{sql}");
    }
}
