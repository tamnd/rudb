//! A sort on string keys, with each string written into the key as its rank among the others.
//!
//! A string orders by its bytes, so where it falls among the strings of its key is all a sort needs
//! of it, and a key list of a count, two strings and a size fits the fixed width byte key that way.
//! These hold the order that comes out to the order the rows should be in, worked out here from the
//! rows as they were scanned: bytes compared for the strings, the direction and the null placement
//! each key asked for, and a tie left in the order the rows arrived.

use std::cmp::Ordering;

use rudb::{Database, Value};

/// Strings that share prefixes, differ only in length, are empty, are not ASCII, and are null, beside
/// an integer with few values so that most rows tie on it and the strings decide.
fn database(threads: usize) -> Database {
    let database = Database::new();
    // Made on one thread so that the table is stored in the order of `id`, which is the order a
    // scan reads it in and so the order a tie is left in however many threads sort it.
    database.execute("SET threads = 1").expect("sets the thread count");
    database
        .execute(
            "CREATE TABLE t AS SELECT i AS id, (i * 7) % 5 AS n, CASE WHEN i % 11 = 0 THEN NULL \
             WHEN i % 13 = 0 THEN '' ELSE 'Brand#' || ((i * 31) % 17)::VARCHAR END AS brand, CASE \
             WHEN i % 9 = 0 THEN NULL WHEN i % 7 = 0 THEN 'caf\u{e9} ' || (i % 3)::VARCHAR ELSE \
             'MEDIUM ' || ((i * 13) % 23)::VARCHAR END AS kind, ((i * 3) % 50)::INTEGER AS size \
             FROM range(12000) r(i)",
        )
        .expect("the table");
    database.execute(&format!("SET threads = {threads}")).expect("sets the thread count");
    database
}

/// Where two values fall under one key, `descending` and with nulls first or last.
fn rank(left: &Value, right: &Value, descending: bool, nulls_first: bool) -> Ordering {
    let ordering = match (left, right) {
        (Value::Null, Value::Null) => return Ordering::Equal,
        (Value::Null, _) => return if nulls_first { Ordering::Less } else { Ordering::Greater },
        (_, Value::Null) => return if nulls_first { Ordering::Greater } else { Ordering::Less },
        (Value::Varchar(left), Value::Varchar(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Value::BigInt(left), Value::BigInt(right)) => left.cmp(right),
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (left, right) => panic!("no order written here for {left:?} and {right:?}"),
    };
    if descending { ordering.reverse() } else { ordering }
}

/// The rows of `columns` from `t` sorted on `keys`, a column and a direction and a null placement
/// each, once by the database and once here, where a tie keeps the order of the scan.
fn both(
    database: &Database,
    columns: &str,
    keys: &[(usize, bool, bool)],
) -> (Vec<String>, Vec<String>) {
    let order: Vec<String> = keys
        .iter()
        .map(|&(at, descending, nulls_first)| {
            let direction = if descending { "DESC" } else { "ASC" };
            let nulls = if nulls_first { "FIRST" } else { "LAST" };
            format!("{} {direction} NULLS {nulls}", at + 1)
        })
        .collect();
    let sql = format!("SELECT {columns} FROM t ORDER BY {}", order.join(", "));
    let sorted: Vec<String> =
        database.query(&sql).expect("the sort ran").rows().map(|row| format!("{row:?}")).collect();
    let mut rows: Vec<Vec<Value>> = database
        .query(&format!("SELECT {columns} FROM t ORDER BY id"))
        .expect("the scan ran")
        .rows()
        .collect();
    rows.sort_by(|left, right| {
        keys.iter()
            .map(|&(at, descending, nulls_first)| {
                rank(&left[at], &right[at], descending, nulls_first)
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    });
    (sorted, rows.iter().map(|row| format!("{row:?}")).collect())
}

#[test]
fn string_keys_sort_by_their_bytes_whichever_way_each_one_goes() {
    for threads in [1, 4] {
        let database = database(threads);
        // The first list is the shape of TPC-H q16, a count, two strings and a size, which is the
        // widest a ranked list gets. The id is not a key, since it would not fit beside them, so a
        // tie is left in the order the rows arrived, which is the order of the scan on any number
        // of threads.
        for keys in [
            vec![(1, true, false), (2, false, false), (3, false, false), (4, false, false)],
            vec![(2, true, true), (3, false, false)],
            vec![(3, false, true), (2, true, false)],
        ] {
            let (sorted, expected) = both(&database, "id, n, brand, kind, size", &keys);
            assert_eq!(sorted.len(), 12000);
            assert!(sorted == expected, "{threads} threads, keys {keys:?}");
        }
    }
}

/// On one thread a tie on every key is left in the order the rows were scanned, the same as the
/// sort that held its keys as values left it.
#[test]
fn rows_that_tie_on_every_string_key_stay_in_the_order_they_arrived() {
    let database = database(1);
    let (sorted, expected) =
        both(&database, "id, brand, kind", &[(1, false, false), (2, true, true)]);
    assert!(sorted == expected, "the ties moved");
}

/// A key list too wide for the ranks to fit, three wide integers and two strings, still sorts, by
/// the path that holds the keys as values.
#[test]
fn a_key_list_too_wide_to_rank_still_sorts() {
    let database = database(1);
    let (sorted, expected) = both(
        &database,
        "id, n::BIGINT, size::BIGINT, (id % 3)::BIGINT, brand, kind",
        &[
            (1, false, false),
            (2, false, false),
            (3, false, false),
            (4, false, false),
            (5, false, false),
        ],
    );
    assert!(sorted == expected, "the wide key list sorted wrong");
}
