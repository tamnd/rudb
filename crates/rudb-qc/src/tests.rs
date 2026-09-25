//! Every test runs a plan through both engines and compares the rows, which is the differential
//! check of `spec/compiler/16-testing.md` at its smallest.

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Cancel, Error, ErrorCode, Field, LogicalType, Memory, Session, Value};
use rudb_pipeline::Pool;
use rudb_plan::Plan;

use super::*;

/// `t` has repeats, nulls, a long string and more rows than one chunk holds, so a pipeline sees
/// more than one morsel.
fn catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let t = QualifiedName::new("memory", "main", "t");
    catalog
        .create_table(
            t.clone(),
            vec![Field::new("x", LogicalType::Integer), Field::new("s", LogicalType::Varchar)],
        )
        .expect("a fresh table");
    let words = ["a", "", "bb", "a string that is longer than twelve bytes", "c"];
    let rows: Vec<Vec<Value>> = (0..5000)
        .map(|i| {
            let x = if i % 7 == 3 { Value::Null } else { Value::Integer(i % 100 - 20) };
            let s = if i % 11 == 5 {
                Value::Null
            } else {
                Value::Varchar(words[i as usize % words.len()].to_string())
            };
            vec![x, s]
        })
        .collect();
    catalog
        .table_mut(&t)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&rows)
        .expect("rows of the table's own types");
    let big = QualifiedName::new("memory", "main", "big");
    catalog
        .create_table(big.clone(), vec![Field::new("x", LogicalType::Integer)])
        .expect("a fresh table");
    catalog
        .table_mut(&big)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&[vec![Value::Integer(i32::MAX)], vec![Value::Integer(1)]])
        .expect("two rows");
    let empty = QualifiedName::new("memory", "main", "empty");
    catalog
        .create_table(empty, vec![Field::new("x", LogicalType::Integer)])
        .expect("a fresh table");
    catalog
}

const SCAN: &str = "Get memory.main.t AS t #0 [x::INTEGER, s::VARCHAR]";

fn rows(chunks: &[Chunk]) -> Vec<Vec<Value>> {
    chunks.iter().flat_map(|c| (0..c.len()).map(|i| c.row(i).collect::<Vec<_>>())).collect()
}

/// The compiled engine's rows.
fn compiled(text: &str) -> Result<Vec<Vec<Value>>> {
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    let cancel = Cancel::new();
    let pool = Pool::default();
    let compiled = compile(&plan, &cancel).expect("the compiled engine takes it");
    let memory = Memory::unlimited();
    let seams = rudb_seam::Settings::new();
    let session = Session::new();
    let under = Under {
        catalog: &catalog,
        cancel: &cancel,
        memory: &memory,
        seams: &seams,
        session: &session,
        pool: &pool,
    };
    compiled.run(&plan, under).map(|a| rows(&a.chunks))
}

/// The first engine's rows and then the compiled engine's.
fn both(text: &str) -> (Vec<Vec<Value>>, Result<Vec<Vec<Value>>>) {
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    let first = rudb_exec::build(&plan, &catalog)
        .expect("the first engine builds")
        .collect(&Cancel::new(), &Pool::default())
        .expect("the first engine runs");
    (rows(&first), compiled(text))
}

/// Asserts the two engines give the same rows, in the same order when `ordered`.
fn same(text: &str, ordered: bool) {
    let (mut first, compiled) = both(text);
    let mut compiled = compiled.expect("the compiled engine runs");
    if !ordered {
        let key = |r: &Vec<Value>| format!("{r:?}");
        first.sort_by_key(key);
        compiled.sort_by_key(key);
    }
    assert!(!first.is_empty(), "a test that compares nothing");
    assert_eq!(first, compiled);
}

#[test]
fn a_filter_and_a_projection_over_a_scan_match_the_first_engine() {
    same(
        &format!(
            "Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y, #0.1::VARCHAR AS s]\n  Filter (#0.0::INTEGER > 3::INTEGER)::BOOLEAN\n    {SCAN}"
        ),
        true,
    );
}

#[test]
fn a_grouped_aggregate_over_strings_matches_the_first_engine() {
    same(
        &format!(
            "Aggregate #1 groups=[#0.1::VARCHAR] aggregates=[count_star()::BIGINT, count(#0.0::INTEGER)::BIGINT, min(#0.0::INTEGER)::INTEGER, max(#0.1::VARCHAR)::VARCHAR, sum(#0.0::INTEGER)::HUGEINT, avg(#0.0::INTEGER)::DOUBLE]\n  {SCAN}"
        ),
        false,
    );
}

#[test]
fn an_ungrouped_aggregate_matches_the_first_engine_on_rows_and_on_none() {
    let aggs = "aggregates=[count_star()::BIGINT, sum(#0.0::INTEGER)::HUGEINT, avg(#0.0::INTEGER)::DOUBLE, min(#0.0::INTEGER)::INTEGER]";
    same(&format!("Aggregate #1 groups=[] {aggs}\n  {SCAN}"), true);
    same(
        &format!("Aggregate #1 groups=[] {aggs}\n  Get memory.main.empty AS empty #0 [x::INTEGER]"),
        true,
    );
}

#[test]
fn a_count_distinct_matches_the_first_engine() {
    same(
        &format!(
            "Aggregate #1 groups=[#0.1::VARCHAR] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT]\n  {SCAN}"
        ),
        false,
    );
}

#[test]
fn a_top_n_over_groups_matches_the_first_engine_in_order() {
    same(
        &format!(
            "TopN 5 offset 1 [#1.1::BIGINT DESC NULLS LAST, #1.0::INTEGER ASC NULLS LAST]\n  Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n    {SCAN}"
        ),
        true,
    );
    same(
        &format!(
            "Limit 3 offset 2\n  Sort [#0.1::VARCHAR ASC NULLS FIRST, #0.0::INTEGER DESC NULLS FIRST]\n    {SCAN}"
        ),
        true,
    );
}

#[test]
fn an_overflow_is_the_error_the_first_engine_raises() {
    let text = "Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y]\n  Get memory.main.big AS big #0 [x::INTEGER]";
    let plan = Plan::parse(text).expect("a well formed plan");
    let first = rudb_exec::build(&plan, &catalog())
        .expect("the first engine builds")
        .collect(&Cancel::new(), &Pool::default())
        .expect_err("the add overflows");
    let error = compiled(text).expect_err("the add overflows");
    assert_eq!(error.code(), first.code(), "{error:?}");
    assert_eq!(error.code(), ErrorCode::OutOfRange, "{error:?}");
}

/// Asserts both engines fail on the plan with the same error.
fn fails_the_same(text: &str) -> Error {
    let plan = Plan::parse(text).expect("a well formed plan");
    let first = rudb_exec::build(&plan, &catalog())
        .expect("the first engine builds")
        .collect(&Cancel::new(), &Pool::default())
        .expect_err("the first engine fails");
    let error = compiled(text).expect_err("the compiled engine fails");
    assert_eq!(error.code(), first.code(), "{error:?} against {first:?}");
    assert_eq!(error.to_string(), first.to_string());
    error
}

#[test]
fn a_function_the_first_engine_does_not_have_fails_the_same_way_on_both() {
    // `md5` binds, since it is in the signature table, but no kernel computes it. The compiled
    // engine used to refuse it by name. It runs it through a `vcall` now, and the kernel's error is
    // the one the first engine raises.
    fails_the_same(&format!("Project #1 [md5(#0.1::VARCHAR)::VARCHAR AS h]\n  {SCAN}"));
}

#[test]
fn a_string_function_without_a_translator_matches_the_first_engine() {
    same(
        &format!(
            "Project #1 [replace(#0.1::VARCHAR, 'a'::VARCHAR, 'xyz'::VARCHAR)::VARCHAR AS r, left(#0.1::VARCHAR, 3::BIGINT)::VARCHAR AS l, #0.1::VARCHAR AS s]\n  {SCAN}"
        ),
        true,
    );
    same(
        &format!(
            "Aggregate #1 groups=[replace(#0.1::VARCHAR, 'a'::VARCHAR, 'xyz'::VARCHAR)::VARCHAR] aggregates=[count_star()::BIGINT]\n  {SCAN}"
        ),
        false,
    );
}

#[test]
fn a_function_over_a_null_argument_matches_the_first_engine() {
    // `concat` skips a null rather than answering null, so the kernel has to be told which
    // arguments are null rather than being skipped for them.
    same(
        &format!(
            "Project #1 [concat(#0.1::VARCHAR, '!'::VARCHAR)::VARCHAR AS c, concat(NULL::VARCHAR, #0.1::VARCHAR)::VARCHAR AS n, left(#0.1::VARCHAR, 1::BIGINT)::VARCHAR AS l]\n  {SCAN}"
        ),
        true,
    );
}

#[test]
fn a_function_over_numbers_matches_the_first_engine() {
    same(
        &format!(
            "Project #1 [abs(#0.0::INTEGER)::INTEGER AS a, \"%\"(#0.0::INTEGER, 7::INTEGER)::INTEGER AS m, \"//\"(#0.0::INTEGER, 3::INTEGER)::INTEGER AS d]\n  Filter (abs(#0.0::INTEGER)::INTEGER > 10::INTEGER)::BOOLEAN\n    {SCAN}"
        ),
        true,
    );
    same(
        &format!(
            "Aggregate #1 groups=[\"%\"(#0.0::INTEGER, 4::INTEGER)::INTEGER] aggregates=[sum(abs(#0.0::INTEGER)::INTEGER)::HUGEINT]\n  {SCAN}"
        ),
        false,
    );
}

#[test]
fn an_error_a_function_raises_is_the_error_the_first_engine_raises() {
    let error = fails_the_same(&format!(
        "Project #1 [\"//\"(#0.0::INTEGER, 0::INTEGER)::INTEGER AS d]\n  {SCAN}"
    ));
    assert_eq!(error.code(), ErrorCode::InvalidInput, "{error:?}");
    assert!(error.to_string().contains("(x // 0)"), "{error}");
}
