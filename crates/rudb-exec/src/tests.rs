//! End to end tests for the operators, driven from the plan's textual form.
//!
//! Every plan in here is written as text and read back by [`Plan::parse`]. That is the payoff of
//! the round trip requirement in `spec/00-README.md`: an executor test says what it runs in the
//! same notation a plan dump uses, so a test that fails and a plan that was dumped from a real
//! query are the same thing and can be pasted into each other. Building the same plans through the
//! arena builders would be three times the lines and would test the builders rather than the
//! operators.
//!
//! There is no SQL here on purpose. The parser and the binder are below this crate in the layer
//! rule, and a test that went through them would fail here when they changed. The SQL level tests
//! live in the `rudb` crate, which is where a query is a string.

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Cancel, Field, LogicalType, Memory, Value};
use rudb_kernels::Accumulator;
use rudb_plan::Plan;

use crate::{build, build_with};

/// `t` has a repeated value, a null and rows that are not in order, because the interesting cases
/// in grouping, distinct and sorting are all about one of those three.
fn catalog() -> Catalog {
    let mut catalog = Catalog::new();
    let t = QualifiedName::new("memory", "main", "t");
    catalog
        .create_table(
            t.clone(),
            vec![Field::new("x", LogicalType::Integer), Field::new("s", LogicalType::Varchar)],
        )
        .expect("a fresh table");
    catalog
        .table_mut(&t)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&[
            vec![Value::Integer(3), Value::Varchar("a".to_string())],
            vec![Value::Integer(1), Value::Null],
            vec![Value::Integer(2), Value::Varchar("c".to_string())],
            vec![Value::Integer(1), Value::Varchar("a".to_string())],
        ])
        .expect("four rows of the table's own types");
    let empty = QualifiedName::new("memory", "main", "empty");
    catalog
        .create_table(empty, vec![Field::new("x", LogicalType::Integer)])
        .expect("a fresh table");
    let words = QualifiedName::new("memory", "main", "words");
    catalog
        .create_table(words.clone(), vec![Field::new("s", LogicalType::Varchar)])
        .expect("a fresh table");
    catalog
        .table_mut(&words)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&[
            vec![Value::Varchar("1".to_string())],
            vec![Value::Varchar("oops".to_string())],
            vec![Value::Varchar("2".to_string())],
        ])
        .expect("three rows");
    catalog
}

/// Runs a plan and returns its rows.
fn run(text: &str) -> Vec<Vec<Value>> {
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    plan.validate().expect("the plan holds together");
    let mut operator = build(&plan, &catalog).expect("the operators build");
    let mut rows = Vec::new();
    while let Some(chunk) = operator.next().expect("the query runs") {
        for row in 0..chunk.len() {
            rows.push(chunk.row(row).collect());
        }
    }
    rows
}

/// Runs a plan and returns the column names its root produces.
fn names(text: &str) -> Vec<String> {
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    let operator = build(&plan, &catalog).expect("the operators build");
    operator.schema().names()
}

/// Runs a plan that is expected to fail and returns the message.
fn failure(text: &str) -> String {
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    let mut operator = build(&plan, &catalog).expect("the operators build");
    loop {
        match operator.next() {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the query was expected to fail and did not"),
            Err(error) => return error.message().to_string(),
        }
    }
}

fn integer(value: i32) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Varchar(value.to_string())
}

const SCAN: &str = "Get memory.main.t AS t #0 [x::INTEGER, s::VARCHAR]";

#[test]
fn a_scan_produces_the_rows_that_were_put_in() {
    let rows = run(SCAN);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], vec![integer(3), text("a")]);
    assert_eq!(rows[1], vec![integer(1), Value::Null]);
}

/// The one query the whole milestone exists to run.
#[test]
fn a_filter_keeps_the_rows_where_the_predicate_is_true() {
    let rows = run(&format!("Filter (#0.0::INTEGER > 1::INTEGER)::BOOLEAN\n  {SCAN}"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], integer(3));
    assert_eq!(rows[1][0], integer(2));
}

/// A null predicate drops the row. `s <> 'a'` is null on the row where `s` is null, and a filter
/// that kept it would be the classic "not false" bug rather than "true".
#[test]
fn a_filter_drops_the_rows_it_cannot_decide() {
    let rows = run(&format!("Filter (#0.1::VARCHAR <> 'a'::VARCHAR)::BOOLEAN\n  {SCAN}"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], text("c"));
}

#[test]
fn a_projection_evaluates_its_expressions_and_names_them() {
    let plan =
        format!("Project #1 [\"+\"(#0.0::INTEGER, 10::INTEGER)::INTEGER AS bumped]\n  {SCAN}");
    assert_eq!(names(&plan), vec!["bumped".to_string()]);
    let rows = run(&plan);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], vec![integer(13)]);
    assert_eq!(rows[3], vec![integer(11)]);
}

#[test]
fn a_constant_query_needs_no_table() {
    let rows = run("Project #0 [1::INTEGER AS one]\n  Dummy");
    assert_eq!(rows, vec![vec![integer(1)]]);
}

#[test]
fn literal_rows_come_out_in_the_order_they_were_written() {
    let rows = run("Values #0 [a::INTEGER] rows=[[1::INTEGER], [2::INTEGER], [NULL::INTEGER]]");
    assert_eq!(rows, vec![vec![integer(1)], vec![integer(2)], vec![Value::Null]]);
}

/// An offset that lands in the middle of a chunk is the ordinary case, and rounding it to a chunk
/// boundary is a wrong answer rather than a slow one.
#[test]
fn a_limit_skips_and_then_counts() {
    let rows = run(&format!("Limit 2 offset 1\n  {SCAN}"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], integer(1));
    assert_eq!(rows[1][0], integer(2));
    let all = run(&format!("Limit ALL offset 3\n  {SCAN}"));
    assert_eq!(all.len(), 1);
    assert_eq!(all[0][0], integer(1));
}

#[test]
fn an_ungrouped_aggregate_over_an_empty_table_still_produces_a_row() {
    let rows = run(
        "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT, sum(#0.0::INTEGER)::HUGEINT]\n  Get memory.main.empty AS empty #0 [x::INTEGER]",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], vec![Value::BigInt(0), Value::Null]);
}

#[test]
fn a_grouped_aggregate_counts_and_sums_within_each_group() {
    let rows = run(&format!(
        "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n  {SCAN}"
    ));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![integer(3), Value::BigInt(1)]);
    assert_eq!(rows[1], vec![integer(1), Value::BigInt(2)]);
    assert_eq!(rows[2], vec![integer(2), Value::BigInt(1)]);
}

/// `count(s)` counts the rows where `s` is not null and `count(*)` counts them all, and the row
/// where `s` is null is what separates the two.
#[test]
fn count_of_a_column_skips_nulls_and_count_star_does_not() {
    let rows = run(&format!(
        "Aggregate #1 groups=[] aggregates=[count_star()::BIGINT, count(#0.1::VARCHAR)::BIGINT]\n  {SCAN}"
    ));
    assert_eq!(rows[0], vec![Value::BigInt(4), Value::BigInt(3)]);
}

/// Two nulls are one group. If the key compared with `=` then this would be two groups of one.
#[test]
fn nulls_group_together() {
    let rows = run(&format!(
        "Aggregate #1 groups=[#0.1::VARCHAR] aggregates=[count_star()::BIGINT]\n  {SCAN}"
    ));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![text("a"), Value::BigInt(2)]);
    assert_eq!(rows[1], vec![Value::Null, Value::BigInt(1)]);
}

#[test]
fn a_distinct_aggregate_counts_each_value_once() {
    let rows = run(&format!(
        "Aggregate #1 groups=[] aggregates=[count(DISTINCT #0.0::INTEGER)::BIGINT]\n  {SCAN}"
    ));
    assert_eq!(rows[0], vec![Value::BigInt(3)]);
}

#[test]
fn a_filtered_aggregate_only_sees_the_rows_it_asked_for() {
    let rows = run(&format!(
        "Aggregate #1 groups=[] aggregates=[count_star(FILTER (#0.0::INTEGER > 1::INTEGER)::BOOLEAN)::BIGINT]\n  {SCAN}"
    ));
    assert_eq!(rows[0], vec![Value::BigInt(2)]);
}

/// The direction and the null placement are independent. Reversing the whole comparison for a
/// descending sort would carry the nulls with it and put them at the wrong end.
#[test]
fn a_sort_puts_the_nulls_where_the_query_asked_and_not_where_the_direction_would() {
    let rows = run(&format!("Sort [#0.1::VARCHAR DESC NULLS LAST]\n  {SCAN}"));
    assert_eq!(rows[0][1], text("c"));
    assert_eq!(rows[1][1], text("a"));
    assert_eq!(rows[2][1], text("a"));
    assert_eq!(rows[3][1], Value::Null);
    let first = run(&format!("Sort [#0.1::VARCHAR DESC NULLS FIRST]\n  {SCAN}"));
    assert_eq!(first[0][1], Value::Null);
    assert_eq!(first[3][1], text("a"));
}

#[test]
fn a_sort_breaks_ties_with_the_next_key() {
    let rows = run(&format!(
        "Sort [#0.0::INTEGER ASC NULLS LAST, #0.1::VARCHAR DESC NULLS LAST]\n  {SCAN}"
    ));
    assert_eq!(rows[0], vec![integer(1), text("a")]);
    assert_eq!(rows[1], vec![integer(1), Value::Null]);
    assert_eq!(rows[2], vec![integer(2), text("c")]);
}

/// The same rows a sort with a limit over it produces, which is the whole promise of the operator.
#[test]
fn a_top_n_is_a_sort_with_a_limit_over_it() {
    let keys = "[#0.0::INTEGER ASC NULLS LAST, #0.1::VARCHAR DESC NULLS LAST]";
    let sorted = run(&format!("Limit 3 offset 0\n  Sort {keys}\n    {SCAN}"));
    assert_eq!(run(&format!("TopN 3 offset 0 {keys}\n  {SCAN}")), sorted);
    let skipped = run(&format!("Limit 2 offset 1\n  Sort {keys}\n    {SCAN}"));
    assert_eq!(run(&format!("TopN 2 offset 1 {keys}\n  {SCAN}")), skipped);
}

/// The rows that are skipped have to be found before there is anything to skip them from, so the
/// offset is part of what the operator holds rather than something left above it.
#[test]
fn a_top_n_counts_the_offset_into_what_it_keeps() {
    let rows = run(&format!("TopN 1 offset 2 [#0.0::INTEGER ASC NULLS LAST]\n  {SCAN}"));
    assert_eq!(rows, vec![vec![integer(2), text("c")]]);
}

/// Rows that tie on every key come out in the order they went in, on this and on the sort alike.
#[test]
fn a_top_n_keeps_the_input_order_of_rows_that_tie() {
    let rows = run(&format!("TopN 2 offset 0 [#0.0::INTEGER ASC NULLS LAST]\n  {SCAN}"));
    assert_eq!(rows, vec![vec![integer(1), Value::Null], vec![integer(1), text("a")]]);
}

#[test]
fn a_top_n_of_nothing_produces_nothing() {
    let rows = run(&format!("TopN 0 offset 0 [#0.0::INTEGER ASC NULLS LAST]\n  {SCAN}"));
    assert!(rows.is_empty());
}

#[test]
fn a_top_n_past_the_end_of_the_input_produces_what_there_is() {
    let rows = run(&format!("TopN 100 offset 0 [#0.0::INTEGER ASC NULLS LAST]\n  {SCAN}"));
    assert_eq!(rows.len(), 4);
    let past = run(&format!("TopN 100 offset 100 [#0.0::INTEGER ASC NULLS LAST]\n  {SCAN}"));
    assert!(past.is_empty());
}

#[test]
fn a_distinct_over_the_whole_row_keeps_the_first_of_each() {
    let rows = run(&format!("Distinct on=[]\n  Project #1 [#0.1::VARCHAR AS s]\n    {SCAN}"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![text("a")]);
    assert_eq!(rows[1], vec![Value::Null]);
    assert_eq!(rows[2], vec![text("c")]);
}

/// `DISTINCT ON` keeps the whole row and not the key, which is the difference between it and a
/// projection to the key followed by a plain `DISTINCT`.
#[test]
fn a_distinct_on_keeps_the_whole_first_row_of_each_key() {
    let rows = run(&format!("Distinct on=[#0.0::INTEGER]\n  {SCAN}"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![integer(3), text("a")]);
    assert_eq!(rows[1], vec![integer(1), Value::Null]);
}

#[test]
fn a_cross_product_produces_every_pair() {
    let rows = run(
        "CrossProduct\n  Values #0 [a::INTEGER] rows=[[1::INTEGER], [2::INTEGER]]\n  Values #1 [b::INTEGER] rows=[[10::INTEGER], [20::INTEGER], [30::INTEGER]]",
    );
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0], vec![integer(1), integer(10)]);
    assert_eq!(rows[3], vec![integer(2), integer(10)]);
}

const LEFT: &str = "Values #0 [a::INTEGER] rows=[[1::INTEGER], [2::INTEGER], [3::INTEGER]]";
const RIGHT: &str = "Values #1 [b::INTEGER] rows=[[2::INTEGER], [3::INTEGER], [3::INTEGER]]";
const ON: &str = "on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]";

#[test]
fn an_inner_join_emits_one_row_per_matching_pair() {
    let rows = run(&format!("Join INNER {ON}\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![integer(2), integer(2)]);
    assert_eq!(rows[1], vec![integer(3), integer(3)]);
    assert_eq!(rows[2], vec![integer(3), integer(3)]);
}

#[test]
fn a_left_join_pads_the_rows_with_no_match() {
    let rows = run(&format!("Join LEFT {ON}\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], vec![integer(1), Value::Null]);
}

#[test]
fn a_right_join_keeps_the_right_rows_nothing_matched() {
    let rows = run(&format!("Join RIGHT {ON}\n  {RIGHT}\n  {LEFT}"));
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[3], vec![Value::Null, integer(1)]);
}

#[test]
fn a_semi_join_emits_a_left_row_once_however_many_times_it_matched() {
    let rows = run(&format!("Join SEMI {ON}\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(rows, vec![vec![integer(2)], vec![integer(3)]]);
}

#[test]
fn an_anti_join_emits_the_left_rows_that_matched_nothing() {
    let rows = run(&format!("Join ANTI {ON}\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(rows, vec![vec![integer(1)]]);
}

/// A positional join does not stop at the shorter side, it pads it, which is DuckDB's answer and
/// not the one a zip would give.
#[test]
fn a_positional_join_pads_the_shorter_side() {
    let short = "Values #1 [b::INTEGER] rows=[[9::INTEGER]]";
    let rows = run(&format!("Join POSITIONAL on=[]\n  {LEFT}\n  {short}"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![integer(1), integer(9)]);
    assert_eq!(rows[1], vec![integer(2), Value::Null]);
}

#[test]
fn a_union_all_keeps_the_duplicates_and_a_union_does_not() {
    let all = run(&format!("SetOp UNION ALL #2\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(all.len(), 6);
    let distinct = run(&format!("SetOp UNION DISTINCT #2\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(distinct, vec![vec![integer(1)], vec![integer(2)], vec![integer(3)]]);
}

/// Three copies minus one copy is two copies. This is the rule that separates a multiset difference
/// from a set difference, and it is the one nobody notices is wrong until a query has duplicates.
#[test]
fn except_all_cancels_one_copy_at_a_time() {
    let many = "Values #0 [a::INTEGER] rows=[[1::INTEGER], [1::INTEGER], [1::INTEGER]]";
    let one = "Values #1 [b::INTEGER] rows=[[1::INTEGER]]";
    let rows = run(&format!("SetOp EXCEPT ALL #2\n  {many}\n  {one}"));
    assert_eq!(rows, vec![vec![integer(1)], vec![integer(1)]]);
    let distinct = run(&format!("SetOp EXCEPT DISTINCT #2\n  {many}\n  {one}"));
    assert!(distinct.is_empty(), "every copy is excluded when the operation is over sets");
}

#[test]
fn intersect_all_pairs_the_copies_off() {
    let rows = run(&format!("SetOp INTERSECT ALL #2\n  {LEFT}\n  {RIGHT}"));
    assert_eq!(rows, vec![vec![integer(2)], vec![integer(3)]]);
}

/// The reason `CASE` is evaluated through selections. `'oops'` does not cast to an integer, so an
/// evaluator that ran the `THEN` arm over the whole chunk and picked afterwards would fail this
/// query on a row the query was written to exclude.
#[test]
fn a_case_arm_is_never_evaluated_for_a_row_it_does_not_apply_to() {
    let rows = run(
        "Project #1 [CASE WHEN (#0.0::VARCHAR <> 'oops'::VARCHAR)::BOOLEAN THEN CAST(#0.0::VARCHAR)::INTEGER ELSE -1::INTEGER END::INTEGER AS n]\n  Get memory.main.words AS words #0 [s::VARCHAR]",
    );
    assert_eq!(rows, vec![vec![integer(1)], vec![integer(-1)], vec![integer(2)]]);
}

#[test]
fn a_case_with_no_arm_taken_and_no_else_is_null() {
    let rows =
        run("Project #1 [CASE WHEN FALSE::BOOLEAN THEN 1::INTEGER END::INTEGER AS n]\n  Dummy");
    assert_eq!(rows, vec![vec![Value::Null]]);
}

/// The error the executor reports is the one the user sees, so it is asserted on rather than left
/// to be whatever the first failing kernel happened to say.
#[test]
fn a_cast_that_cannot_succeed_says_what_it_could_not_convert() {
    let message = failure(
        "Project #1 [CAST(#0.0::VARCHAR)::INTEGER AS n]\n  Get memory.main.words AS words #0 [s::VARCHAR]",
    );
    assert!(message.contains("Could not convert"), "{message}");
    assert!(message.contains("oops"), "{message}");
}

/// A column that no operator in the tree produces is a bug in whoever built the plan, and the
/// message names the binding rather than a position in a chunk.
///
/// It is caught when the tree is built rather than on the first chunk, because a filter resolves
/// its predicate against the input's schema once and keeps the resolved positions. That is a
/// property worth asserting rather than an accident of where the code lives: a plan that cannot
/// resolve is broken before any data is read, and finding it on the first chunk means finding it
/// after a scan has opened files and a scheduler has handed out morsels.
#[test]
fn a_column_that_is_not_in_the_input_says_which_one() {
    let catalog = catalog();
    let plan = Plan::parse(&format!("Filter (#7.3::INTEGER > 1::INTEGER)::BOOLEAN\n  {SCAN}"))
        .expect("a well formed plan");
    let error = build(&plan, &catalog).expect_err("there is no table 7");
    assert!(error.message().contains("column #7.3"), "{error}");
}

#[test]
fn a_table_the_catalog_does_not_have_is_caught_when_the_tree_is_built() {
    let catalog = catalog();
    let plan = Plan::parse("Get memory.main.nope AS nope #0 [x::INTEGER]").expect("well formed");
    let error = build(&plan, &catalog).expect_err("there is no table called nope");
    assert!(error.message().contains("nope"), "{error}");
}

/// A pipeline deeper than one operator, which is what a real query is. The answer is the two rows
/// with the largest `x`, in descending order, which every one of the four operators has to agree
/// about for the result to come out right.
#[test]
fn a_whole_pipeline_runs_in_one_piece() {
    let rows = run(&format!(
        "Project #2 [#1.0::INTEGER AS x, #1.1::BIGINT AS n]\n  Limit 2 offset 0\n    Sort [#1.0::INTEGER DESC NULLS LAST]\n      Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n        Filter (#0.0::INTEGER > 0::INTEGER)::BOOLEAN\n          {SCAN}"
    ));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], vec![integer(3), Value::BigInt(1)]);
    assert_eq!(rows[1], vec![integer(2), Value::BigInt(1)]);
}

#[test]
fn a_cancelled_token_stops_the_tree_before_it_produces_a_chunk() {
    let catalog = catalog();
    let plan = Plan::parse(SCAN).expect("a well formed plan");
    let cancel = Cancel::new();
    let mut operator =
        build_with(&plan, &catalog, &cancel, &Memory::unlimited()).expect("the operators build");
    cancel.cancel();
    let error = operator.next().expect_err("it was cancelled");
    assert_eq!(error.code().duckdb_name(), "Interrupt Error");
}

#[test]
fn a_token_nothing_has_cancelled_leaves_the_answer_alone() {
    // The check is in the tree whether or not anybody is holding the other end of the token, so the
    // thing worth asserting is that it changes no answer.
    let catalog = catalog();
    let plan = Plan::parse(SCAN).expect("a well formed plan");
    let mut guarded = build_with(&plan, &catalog, &Cancel::new(), &Memory::unlimited())
        .expect("the operators build");
    let mut plain = build(&plan, &catalog).expect("the operators build");
    loop {
        let (left, right) = (guarded.next().expect("runs"), plain.next().expect("runs"));
        match (left, right) {
            (Some(left), Some(right)) => assert_eq!(left.len(), right.len()),
            (None, None) => break,
            _ => panic!("one of them finished and the other did not"),
        }
    }
}

#[test]
fn a_group_is_charged_for_the_room_it_takes_and_not_only_for_what_it_holds() {
    // #227. The hash table was charged for its entries and not for its capacity, and the per group
    // allocations around it were not charged at all, so the budget was spent at about two fifths of
    // the real footprint and a limit stopped nothing. The bound below is arithmetic anybody can
    // redo and it does not depend on the values: every group takes a slot in the table and a slot
    // per aggregate call beside it, whatever is in it.
    const GROUPS: i32 = 4096;
    let mut catalog = Catalog::new();
    let wide = QualifiedName::new("memory", "main", "wide");
    catalog
        .create_table(wide.clone(), vec![Field::new("x", LogicalType::Integer)])
        .expect("a fresh table");
    let rows: Vec<Vec<Value>> = (0..GROUPS).map(|at| vec![Value::Integer(at)]).collect();
    catalog
        .table_mut(&wide)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&rows)
        .expect("one row of the table's own type per group");

    let memory = Memory::unlimited();
    let plan = Plan::parse(
        "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n  \
         Get memory.main.wide AS wide #0 [x::INTEGER]\n",
    )
    .expect("a well formed plan");
    let mut operator =
        build_with(&plan, &catalog, &Cancel::new(), &memory).expect("the operators build");
    let mut seen = 0;
    while let Some(chunk) = operator.next().expect("the aggregate runs") {
        seen += chunk.len();
    }
    assert_eq!(seen, GROUPS as usize, "one group per distinct value");

    // What a group holds: the one copy of its key the table owns, by the footprint of the values in
    // it. Charging that and stopping there is what #227 was about, because it counts nothing for the
    // slot the group takes in a container or for the blocks the allocator hands out.
    let contents = size_of::<Value>();
    // The slot each group takes in the table and in the accumulators beside it. A floor rather than
    // the figure: it assumes both containers are exactly full, and neither is, and it counts nothing
    // for the control bytes or for the blocks the allocator hands out. So the charge has to clear it
    // by some margin and the old charge could not clear it at all.
    let slots = size_of::<(Vec<Value>, usize)>() + size_of::<Accumulator>();
    let groups = u64::try_from(GROUPS).expect("a small count");
    let contents = groups * u64::try_from(contents).expect("a small size");
    let floor = contents + groups * u64::try_from(slots).expect("a small size");
    assert!(
        memory.peak() > floor,
        "{} charged for {GROUPS} groups, against {contents} for their contents alone and {floor} \
         once every group is charged for the slot it takes in each container",
        memory.peak()
    );
}

#[test]
fn a_budget_too_small_for_the_rows_stops_the_operator_that_buffers_them() {
    // Every pipeline breaker in this crate charges what it holds, and the one thing a test here can
    // say that `crates/rudb` cannot is that it is the operator refusing rather than the result.
    let catalog = catalog();
    let plan = Plan::parse(&format!("Sort [#0.0::INTEGER ASC NULLS LAST]\n  {SCAN}"))
        .expect("a well formed plan");
    let memory = Memory::with_limit(1);
    let mut operator = build_with(&plan, &catalog, &Cancel::new(), &memory)
        .expect("the operators build, because nothing is held yet");
    let error = operator.next().expect_err("one byte is not enough for a row");
    assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
    drop(operator);
    assert_eq!(memory.used(), 0, "the failed operator gave everything back");
}

/// A table whose grouping does not fit in a small budget: `groups` distinct keys, each of them
/// twice, with a string beside the number.
///
/// The string is there so that a spilled key has a buffer in it. A key of nothing but fixed width
/// values is written and read back without the allocator being asked anything, and the path worth
/// testing is the one where it is.
fn crowd(groups: i32) -> Catalog {
    let mut catalog = Catalog::new();
    let name = QualifiedName::new("memory", "main", "crowd");
    catalog
        .create_table(
            name.clone(),
            vec![Field::new("x", LogicalType::Integer), Field::new("s", LogicalType::Varchar)],
        )
        .expect("a fresh table");
    let rows: Vec<Vec<Value>> = (0..groups * 2)
        .map(|at| {
            let key = at % groups;
            vec![Value::Integer(key), Value::Varchar(format!("key number {key}"))]
        })
        .collect();
    catalog
        .table_mut(&name)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&rows)
        .expect("two rows of the table's own types per group");
    catalog
}

/// Runs a plan under a given budget and hands back its rows in a settled order.
///
/// Sorted by the debug spelling of the row, which is not an order anybody would want to look at and
/// is the only one available here, because a `Value` is not ordered and the point is to compare two
/// runs of the same query rather than to read the answer.
fn under(catalog: &Catalog, text: &str, memory: &Memory) -> Vec<Vec<Value>> {
    let plan = Plan::parse(text).expect("a well formed plan");
    plan.validate().expect("the plan holds together");
    let mut operator =
        build_with(&plan, catalog, &Cancel::new(), memory).expect("the operators build");
    let mut rows: Vec<Vec<Value>> = Vec::new();
    while let Some(chunk) = operator.next().expect("the query runs") {
        for row in 0..chunk.len() {
            rows.push(chunk.row(row).collect());
        }
    }
    rows.sort_by_key(|row| format!("{row:?}"));
    rows
}

#[test]
fn a_group_by_that_outgrows_its_budget_spills_and_answers_anyway() {
    // #220. A table that cannot hold every group used to reach the allocator and die there, which
    // is what ten of the forty three ClickBench queries did. It now keeps the groups it has and
    // writes the rest of the rows to a file, so the same query answers over as many passes as the
    // budget needs.
    const GROUPS: i32 = 4096;
    const QUERY: &str = "Aggregate #1 groups=[#0.0::INTEGER, #0.1::VARCHAR] \
         aggregates=[count_star()::BIGINT, sum(#0.0::INTEGER)::HUGEINT]\n  \
         Get memory.main.crowd AS crowd #0 [x::INTEGER, s::VARCHAR]\n";
    let catalog = crowd(GROUPS);
    let open = Memory::unlimited();
    let want = under(&catalog, QUERY, &open);
    assert_eq!(want.len(), GROUPS as usize, "one row per group");

    // Three quarters of what the query took when nothing was stopping it. It has to be under the
    // whole, or the query never spills and this tests nothing, and it has to be over what the
    // answer costs, because the rows every pass finished are held until the last pass ends and no
    // amount of spilling makes an answer that does not fit fit. Three quarters is between the two
    // here, and the proof that it is under the whole is that the old code needed the whole and this
    // one gets an answer.
    let tight = Memory::with_limit(open.peak() / 4 * 3);
    let got = under(&catalog, QUERY, &tight);
    assert_eq!(got, want, "the same answer, over as many passes as the budget needed");
    assert_eq!(tight.used(), 0, "every pass gave back what it held");
}

#[test]
fn a_spilled_row_carries_its_distinct_argument_and_its_filter() {
    // The two columns a spilled row has that the group key does not. A `DISTINCT` needs the
    // argument itself, because the set that decides whether a value has been counted is rebuilt in
    // the pass that finishes the group, and a `FILTER` needs the answer the predicate already gave,
    // because the row it was evaluated against is not written out and cannot be evaluated again.
    const GROUPS: i32 = 4096;
    const QUERY: &str = "Aggregate #1 groups=[#0.0::INTEGER] \
         aggregates=[count(DISTINCT #0.1::VARCHAR)::BIGINT, \
         count_star(FILTER (#0.0::INTEGER > 1000::INTEGER)::BOOLEAN)::BIGINT]\n  \
         Get memory.main.crowd AS crowd #0 [x::INTEGER, s::VARCHAR]\n";
    let catalog = crowd(GROUPS);
    let open = Memory::unlimited();
    let want = under(&catalog, QUERY, &open);
    assert_eq!(want.len(), GROUPS as usize, "one row per group");

    let tight = Memory::with_limit(open.peak() / 4 * 3);
    let got = under(&catalog, QUERY, &tight);
    assert_eq!(got, want, "the same answer, over as many passes as the budget needed");
    assert_eq!(tight.used(), 0, "every pass gave back what it held");
}

#[test]
fn a_budget_too_small_for_one_group_says_so_rather_than_running_forever() {
    // The other end of the same change. Spilling turns a budget that is merely too small into more
    // passes, and there is a budget too small for even that, and the thing it must not do is loop
    // handing the same rows from one pass to the next.
    let catalog = crowd(64);
    let plan = Plan::parse(
        "Aggregate #1 groups=[#0.0::INTEGER] aggregates=[count_star()::BIGINT]\n  \
         Get memory.main.crowd AS crowd #0 [x::INTEGER, s::VARCHAR]\n",
    )
    .expect("a well formed plan");
    let memory = Memory::with_limit(1);
    let mut operator = build_with(&plan, &catalog, &Cancel::new(), &memory)
        .expect("the operators build, because nothing is held yet");
    let error = operator.next().expect_err("one byte is not enough for a group");
    assert_eq!(error.code().duckdb_name(), "Out of Memory Error");
    drop(operator);
    assert_eq!(memory.used(), 0, "the failed operator gave everything back");
}

#[test]
fn every_seam_has_a_row_in_the_strategies_table() {
    let rows = run("TableFunction rudb_strategies args=[] #0 [seam::VARCHAR, milestone::VARCHAR, \
         implementation::VARCHAR]");
    assert_eq!(rows.len(), rudb_seam::SeamId::ALL.len(), "one row per seam, registered or not");
    let seams: Vec<Value> =
        rudb_seam::SeamId::ALL.iter().map(|seam| text(seam.name())).collect::<Vec<_>>();
    let listed: Vec<Value> = rows.iter().map(|row| row[0].clone()).collect();
    assert_eq!(listed, seams, "in the order the design lists them");
}

#[test]
fn a_seam_with_no_registry_says_so_rather_than_being_left_out() {
    // Nothing is registered yet, which is the state F0 is meant to show honestly. Every row has a
    // seam, a milestone that owes it and a description, and nulls where an implementation would be.
    let rows = run("TableFunction rudb_strategies args=[] #0 [seam::VARCHAR, milestone::VARCHAR, \
         implementation::VARCHAR, is_reference::BOOLEAN]");
    for row in &rows {
        assert!(matches!(row[0], Value::Varchar(_)), "a seam name");
        assert!(matches!(row[1], Value::Varchar(_)), "the milestone that owes it");
        assert_eq!(row[2], Value::Null, "no implementation yet");
        assert_eq!(row[3], Value::Null, "so nothing is the reference either");
    }
}

#[test]
fn the_strategies_table_hands_back_the_columns_it_was_asked_for() {
    // Not its own first few. The binder projects every column in order today, so a subset only
    // arrives here once a pass trims the list, and an operator that ignored the list would answer
    // with the right column names over the wrong column values.
    let rows = run("TableFunction rudb_strategies args=[] #0 [milestone::VARCHAR, seam::VARCHAR]");
    let first = rows.first().expect("at least one seam");
    assert_eq!(first[0], text("F1"), "the milestone column, not the first column of the table");
    assert_eq!(first[1], text("vector.form"));
}

#[test]
fn the_ids_the_builder_tags_its_counters_with_are_the_ones_the_plan_says() {
    // The point of the numbering living in `rudb-plan` is that `EXPLAIN` can print an operator's id
    // without building the operator. That only holds if what gets built agrees, so this checks the
    // two against each other over a plan with a node of two inputs in it, which is the shape where
    // there are more operators than there are nodes.
    let text = concat!(
        "Join INNER on=[(#0.0::INTEGER = #1.0::INTEGER)::BOOLEAN]\n",
        "  Get memory.main.t AS t #0 [x::INTEGER]\n",
        "  Get memory.main.empty AS empty #1 [x::INTEGER]\n",
    );
    let plan = Plan::parse(text).expect("the plan parses");
    let shape = rudb_plan::Shape::of(&plan);
    let report = rudb_metrics::Report::new();
    let catalog = catalog();
    let mut root =
        crate::build_measured(&plan, &catalog, &Cancel::new(), &Memory::unlimited(), &report)
            .expect("the tree builds");
    while root.next().expect("the query runs").is_some() {}
    let mut document = rudb_metrics::Document::new(text);
    report.fill(&mut document);
    let ids: Vec<u32> = document.operators.iter().map(|operator| operator.id).collect();
    assert_eq!(ids, (0..shape.operators()).collect::<Vec<_>>(), "every id and no other");
    let join = document.operators.iter().find(|operator| operator.kind == "Join").expect("a join");
    assert_eq!(join.id, shape.operator(plan.root()));
    let gather =
        document.operators.iter().find(|operator| operator.kind == "Gather").expect("a gather");
    assert_eq!(gather.id, shape.gathered(plan.root()).expect("two inputs, two operators"));
}
