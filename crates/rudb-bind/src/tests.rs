//! What the binder produces, checked against the plan's textual form.
//!
//! The assertions are on the printed plan rather than on the arenas, because the printed plan is
//! what a person reads when a query does the wrong thing, and a test that reads what a person
//! reads fails in a way that says what went wrong.

use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Field, LogicalType, Session, Span};

use crate::{bind_sql, bind_sql_with};

/// A catalog with two tables in it, which is enough for a join and for every name rule.
fn catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog
        .create_table(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("url", LogicalType::Varchar),
                Field::new("counter", LogicalType::Integer),
            ],
        )
        .expect("a table nothing else has created");
    catalog
        .create_table(
            QualifiedName::new("memory", "main", "visits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("duration", LogicalType::Integer),
            ],
        )
        .expect("a table nothing else has created");
    catalog
}

/// The printed plan for a query that is expected to bind.
fn plan(query: &str) -> String {
    bind_sql(query, &catalog())
        .unwrap_or_else(|error| panic!("{query} should bind: {error}"))
        .to_string()
}

/// The message for a query that is expected not to bind.
fn failure(query: &str) -> String {
    match bind_sql(query, &catalog()) {
        Ok(plan) => panic!("{query} should not bind, it produced\n{plan}"),
        Err(error) => error.message().to_string(),
    }
}

#[test]
fn a_scan_projects_what_the_query_asked_for() {
    assert_eq!(
        plan("SELECT url FROM hits"),
        "Project #1 [#0.1::VARCHAR AS url]\n  \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn a_star_expands_in_the_order_the_table_has() {
    assert_eq!(
        plan("SELECT * FROM hits"),
        "Project #1 [#0.0::BIGINT AS UserID, #0.1::VARCHAR AS url, #0.2::INTEGER AS counter]\n  \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn a_query_with_no_from_clause_sits_on_one_row() {
    assert_eq!(plan("SELECT 1"), "Project #0 [1::INTEGER AS \"1\"]\n  Dummy\n");
}

#[test]
fn a_where_clause_becomes_a_filter_under_the_projection() {
    assert_eq!(
        plan("SELECT url FROM hits WHERE counter > 5"),
        "Project #1 [#0.1::VARCHAR AS url]\n  \
         Filter (#0.2::INTEGER > 5::INTEGER)::BOOLEAN\n    \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn a_comparison_brings_both_sides_to_the_type_they_meet_at() {
    // The literal is an INTEGER and the column is a BIGINT, so the literal is the one that moves.
    let text = plan("SELECT url FROM hits WHERE UserID = 7");
    assert!(text.contains("(#0.0::BIGINT = CAST(7::INTEGER)::BIGINT)::BOOLEAN"), "{text}");
}

#[test]
fn an_alias_names_the_output_column_and_the_expression_names_it_otherwise() {
    let text = plan("SELECT counter + 1 AS bumped, counter * 2 FROM hits");
    assert!(text.contains("AS bumped"), "{text}");
    assert!(text.contains("AS \"(counter * 2)\""), "{text}");
}

#[test]
fn an_and_of_three_things_is_one_flat_conjunction() {
    let text = plan("SELECT url FROM hits WHERE counter > 1 AND counter < 9 AND url = 'a'");
    assert_eq!(
        text.matches(" AND ").count(),
        2,
        "one conjunction of three, not two of two: {text}"
    );
}

#[test]
fn between_becomes_the_pair_of_comparisons_it_means() {
    let text = plan("SELECT url FROM hits WHERE counter BETWEEN 1 AND 9");
    assert!(text.contains("(#0.2::INTEGER >= 1::INTEGER)"), "{text}");
    assert!(text.contains("(#0.2::INTEGER <= 9::INTEGER)"), "{text}");
    let negated = plan("SELECT url FROM hits WHERE counter NOT BETWEEN 1 AND 9");
    assert!(negated.contains(" OR "), "{negated}");
}

#[test]
fn in_becomes_a_disjunction_of_equalities() {
    let text = plan("SELECT url FROM hits WHERE counter IN (1, 2, 3)");
    assert_eq!(text.matches(" OR ").count(), 2, "{text}");
    let negated = plan("SELECT url FROM hits WHERE counter NOT IN (1, 2)");
    assert!(negated.contains(" AND "), "{negated}");
    assert!(negated.contains("<>"), "{negated}");
}

#[test]
fn is_null_is_the_null_safe_comparison_against_a_null() {
    let text = plan("SELECT url FROM hits WHERE url IS NULL");
    assert!(text.contains("IS NOT DISTINCT FROM"), "{text}");
    let negated = plan("SELECT url FROM hits WHERE url IS NOT NULL");
    assert!(negated.contains("IS DISTINCT FROM"), "{negated}");
}

#[test]
fn a_simple_case_is_bound_as_the_searched_one_it_means() {
    let text = plan("SELECT CASE counter WHEN 1 THEN 'one' ELSE 'many' END AS which FROM hits");
    assert!(text.contains("CASE WHEN"), "{text}");
    assert!(text.contains("(#0.2::INTEGER = 1::INTEGER)"), "{text}");
}

#[test]
fn a_group_by_puts_the_groups_first_and_the_aggregates_after() {
    assert_eq!(
        plan("SELECT url, count(*) FROM hits GROUP BY url"),
        "Project #2 [#1.0::VARCHAR AS url, #1.1::BIGINT AS \"count_star()\"]\n  \
         Aggregate #1 groups=[#0.1::VARCHAR] aggregates=[count_star()::BIGINT]\n    \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn an_aggregate_with_no_group_by_still_aggregates() {
    let text = plan("SELECT sum(counter) FROM hits");
    assert!(
        text.contains("Aggregate #1 groups=[] aggregates=[sum(#0.2::INTEGER)::HUGEINT]"),
        "{text}"
    );
}

#[test]
fn the_same_aggregate_written_twice_is_computed_once() {
    let text = plan("SELECT sum(counter), sum(counter) + 1 FROM hits");
    assert_eq!(text.matches("sum(#").count(), 1, "{text}");
}

#[test]
fn a_column_that_is_neither_grouped_nor_aggregated_is_refused() {
    let message = failure("SELECT url, count(*) FROM hits GROUP BY counter");
    assert!(message.contains("must appear in the GROUP BY clause"), "{message}");
    assert!(message.contains("url"), "the message should name the column: {message}");
}

#[test]
fn a_grouped_expression_is_recognised_wherever_it_is_written_again() {
    let text = plan("SELECT counter + 1, count(*) FROM hits GROUP BY counter + 1");
    assert!(text.contains("groups=[\"+\"(#0.2::INTEGER, 1::INTEGER)::INTEGER]"), "{text}");
    assert!(text.contains("[#1.0::INTEGER AS \"(counter + 1)\""), "{text}");
}

/// `typeof` is answered here, so what reaches the plan is the name of the type. Per #229.
#[test]
fn typeof_is_the_name_of_a_type_rather_than_a_call() {
    let text = plan("SELECT typeof(counter) FROM hits");
    assert!(text.contains("'INTEGER'::VARCHAR AS \"typeof(counter)\""), "{text}");
    // The only `typeof` left in the plan is the column's name, which is the call as it was written.
    assert!(!text.contains("typeof(#"), "nothing is left for the executor to do: {text}");
    // The argument is still an expression where it was written, so it follows the rules about what
    // can be selected next to an aggregate. Upstream refuses this one for the same reason.
    let message = failure("SELECT typeof(url), count(*) FROM hits");
    assert!(message.contains("must appear in the GROUP BY clause"), "{message}");
    let text = plan("SELECT typeof(url) FROM hits GROUP BY url");
    assert!(text.contains("'VARCHAR'::VARCHAR"), "{text}");
}

#[test]
fn group_by_can_name_a_target_by_position_or_by_alias() {
    let by_position = plan("SELECT url, count(*) FROM hits GROUP BY 1");
    let by_alias = plan("SELECT url AS u, count(*) FROM hits GROUP BY u");
    assert!(by_position.contains("groups=[#0.1::VARCHAR]"), "{by_position}");
    assert!(by_alias.contains("groups=[#0.1::VARCHAR]"), "{by_alias}");
}

#[test]
fn group_by_all_groups_everything_that_is_not_an_aggregate() {
    let text = plan("SELECT url, counter, count(*) FROM hits GROUP BY ALL");
    assert!(text.contains("groups=[#0.1::VARCHAR, #0.2::INTEGER]"), "{text}");
}

#[test]
fn having_filters_above_the_aggregate_and_where_filters_below_it() {
    let text = plan("SELECT url FROM hits WHERE counter > 1 GROUP BY url HAVING count(*) > 2");
    let filter_above = text.find("Filter (#1.1").expect("the HAVING filter");
    let aggregate = text.find("Aggregate").expect("the aggregate");
    let filter_below = text.find("Filter (#0.2").expect("the WHERE filter");
    assert!(filter_above < aggregate && aggregate < filter_below, "{text}");
}

/// A `HAVING` that compares a group against a total over the whole table, which is TPC-H q11.
///
/// The query it reads is one row and has nothing to do with the groups, so its join goes on top of
/// the aggregate. Underneath it the column would be on every input row and the grouping rule would
/// ask for it in the GROUP BY, which is what this used to say.
#[test]
fn a_having_can_compare_a_group_against_a_query_of_its_own() {
    let text = plan(
        "SELECT url, count(*) FROM hits GROUP BY url HAVING count(*) > (SELECT count(*) / 100 \
         FROM hits)",
    );
    let filter = text.find("Filter").expect("the HAVING filter");
    let join = text.find("Join").expect("the join that brings the total in");
    let aggregate = text.find("Aggregate").expect("the aggregate");
    assert!(filter < join && join < aggregate, "{text}");
}

#[test]
fn an_aggregate_in_a_where_clause_says_where_it_cannot_go() {
    let message = failure("SELECT url FROM hits WHERE count(*) > 1");
    assert!(message.contains("WHERE clause"), "{message}");
}

#[test]
fn an_order_by_sorts_the_projection_and_takes_the_defaults_sql_gives_it() {
    let text = plan("SELECT url FROM hits ORDER BY url");
    assert!(text.contains("Sort [#1.0::VARCHAR ASC NULLS LAST]"), "{text}");
    let descending = plan("SELECT url FROM hits ORDER BY url DESC");
    assert!(descending.contains("DESC NULLS LAST"), "{descending}");
}

#[test]
fn an_order_by_on_something_not_selected_projects_it_and_then_drops_it() {
    let text = plan("SELECT url FROM hits ORDER BY counter");
    assert!(text.contains("#1 [#0.1::VARCHAR AS url, #0.2::INTEGER AS counter]"), "{text}");
    assert!(text.starts_with("Project #2 [#1.0::VARCHAR AS url]\n"), "{text}");
}

#[test]
fn an_order_by_position_names_the_output_column() {
    let text = plan("SELECT url, counter FROM hits ORDER BY 2 DESC");
    assert!(text.contains("Sort [#1.1::INTEGER DESC"), "{text}");
    let out_of_range = failure("SELECT url FROM hits ORDER BY 4");
    assert!(out_of_range.contains("out of range"), "{out_of_range}");
}

#[test]
fn a_limit_and_an_offset_are_constants_by_the_time_they_are_here() {
    let text = plan("SELECT url FROM hits LIMIT 10 OFFSET 5");
    assert!(text.contains("Limit 10 offset 5"), "{text}");
    let offset_only = plan("SELECT url FROM hits OFFSET 5");
    assert!(offset_only.contains("Limit ALL offset 5"), "{offset_only}");
}

#[test]
fn distinct_sits_above_the_projection() {
    let text = plan("SELECT DISTINCT url FROM hits");
    assert!(text.starts_with("Distinct on=[]\n  Project"), "{text}");
}

#[test]
fn distinct_cannot_order_by_something_it_does_not_select() {
    let message = failure("SELECT DISTINCT url FROM hits ORDER BY counter");
    assert!(message.contains("must appear in the select list"), "{message}");
}

#[test]
fn two_tables_in_a_from_clause_are_a_cross_product() {
    let text = plan("SELECT hits.url, visits.duration FROM hits, visits");
    assert!(text.contains("CrossProduct"), "{text}");
}

#[test]
fn a_join_condition_binds_against_both_sides() {
    let text = plan("SELECT url FROM hits JOIN visits ON hits.UserID = visits.UserID");
    assert!(text.contains("Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]"), "{text}");
}

#[test]
fn using_makes_the_equality_and_leaves_one_copy_of_the_column() {
    let text = plan("SELECT UserID, url, duration FROM hits JOIN visits USING (UserID)");
    assert!(text.contains("on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]"), "{text}");
    assert!(text.contains("[#0.0::BIGINT AS UserID"), "{text}");
}

#[test]
fn a_using_column_named_twice_is_still_one_column() {
    // `USING (UserID, UserID)` is legal and the reference binary treats it as `USING (UserID)`.
    // Taking the name twice dropped the right side's copy twice, which took `duration` out of the
    // answer here and panicked outright when the copy was the last column in the scope. The fuzz
    // target in `fuzz/fuzz_targets/bind.rs` found it.
    let text = plan("SELECT * FROM hits JOIN visits USING (UserID, UserID)");
    assert!(text.contains("AS UserID"), "{text}");
    assert!(text.contains("AS duration"), "{text}");
    assert_eq!(text.matches("on=[").count(), 1, "{text}");
}

#[test]
fn natural_joins_on_whatever_both_sides_call_the_same_thing() {
    let text = plan("SELECT url FROM hits NATURAL JOIN visits");
    assert!(text.contains("Join INNER on=[(#0.0::BIGINT = #1.0::BIGINT)::BOOLEAN]"), "{text}");
}

#[test]
fn an_alias_replaces_the_table_name_rather_than_adding_to_it() {
    let text = plan("SELECT h.url FROM hits AS h");
    assert!(text.contains("AS h #0"), "{text}");
    let message = failure("SELECT hits.url FROM hits AS h");
    assert!(message.contains("Referenced table \"hits\" not found"), "{message}");
}

#[test]
fn a_subquery_in_the_from_clause_is_bound_and_then_named() {
    let text = plan("SELECT sub.url FROM (SELECT url FROM hits) AS sub");
    assert!(text.contains("Project #2 [#1.0::VARCHAR AS url]"), "{text}");
}

#[test]
fn a_non_recursive_cte_is_an_inlined_subquery() {
    let printed = plan("WITH chosen AS (SELECT UserID, url FROM hits) SELECT url FROM chosen");
    assert!(printed.contains("Get memory.main.hits"), "{printed}");
    assert_eq!(printed.matches("Project").count(), 2, "{printed}");
}

#[test]
fn later_ctes_can_read_earlier_ones_and_rename_their_columns() {
    let printed = plan(
        "WITH first AS (SELECT counter FROM hits), second(n) AS NOT MATERIALIZED (SELECT counter + 1 FROM first) SELECT n FROM second",
    );
    assert!(printed.contains("Get memory.main.hits"), "{printed}");
    assert!(printed.contains("AS n"), "{printed}");
}

#[test]
fn a_materialized_cte_is_computed_once_and_read_where_it_is_named() {
    let printed = plan(
        "WITH chosen AS MATERIALIZED (SELECT UserID, url FROM hits) SELECT a.url FROM chosen AS a, chosen AS b",
    );
    assert_eq!(printed.matches("MaterializedCte chosen @0").count(), 1, "{printed}");
    assert_eq!(printed.matches("Get memory.main.hits").count(), 1, "{printed}");
    assert_eq!(printed.matches("CteScan chosen @0").count(), 2, "{printed}");
    // Two reads of one thing held once, so the two have table indexes of their own and the columns
    // of each are that read's columns rather than a second name for the same ones.
    assert!(printed.contains("CteScan chosen @0 #3 [UserID::BIGINT, url::VARCHAR]"), "{printed}");
    assert!(printed.contains("CteScan chosen @0 #4 [UserID::BIGINT, url::VARCHAR]"), "{printed}");
}

#[test]
fn a_materialized_cte_holds_the_columns_its_column_list_named() {
    let printed = plan(
        "WITH chosen(who, where_) AS MATERIALIZED (SELECT UserID, url FROM hits) SELECT where_ FROM chosen",
    );
    assert!(
        printed.contains("MaterializedCte chosen @0 [who::BIGINT, where_::VARCHAR]"),
        "{printed}"
    );
    assert!(printed.contains("AS where_"), "{printed}");
    // More names than the definition has columns is not an error, which is the pinned build going
    // its own way and is written out on `Scope::rename_prefix`.
    let printed = plan(
        "WITH chosen(a, b, c, d) AS MATERIALIZED (SELECT UserID FROM hits) SELECT a FROM chosen",
    );
    assert!(printed.contains("MaterializedCte chosen @0 [a::BIGINT]"), "{printed}");
}

#[test]
fn a_materialized_cte_inside_another_one_is_held_separately() {
    let printed = plan(
        "WITH outer_ AS MATERIALIZED (WITH inner_ AS MATERIALIZED (SELECT counter FROM hits) SELECT counter FROM inner_) SELECT counter FROM outer_",
    );
    assert!(printed.contains("MaterializedCte outer_ @1"), "{printed}");
    assert!(printed.contains("MaterializedCte inner_ @0"), "{printed}");
    assert!(printed.contains("CteScan inner_ @0"), "{printed}");
    assert!(printed.contains("CteScan outer_ @1"), "{printed}");
}

#[test]
fn a_materialized_cte_can_be_read_by_a_later_one() {
    let printed = plan(
        "WITH first AS MATERIALIZED (SELECT counter FROM hits), second AS MATERIALIZED (SELECT counter + 1 AS n FROM first) SELECT n FROM second",
    );
    assert!(printed.contains("MaterializedCte first @0"), "{printed}");
    assert!(printed.contains("MaterializedCte second @1 [n::INTEGER]"), "{printed}");
    assert!(printed.contains("CteScan first @0"), "{printed}");
}

#[test]
fn a_union_lines_the_two_sides_up_and_sorts_above_both() {
    let text = plan("SELECT counter FROM hits UNION SELECT duration FROM visits ORDER BY 1");
    assert!(text.contains("SetOp UNION DISTINCT"), "{text}");
    assert!(text.starts_with("Sort [#4.0::INTEGER ASC"), "{text}");
    let all = plan("SELECT counter FROM hits UNION ALL SELECT duration FROM visits");
    assert!(all.contains("SetOp UNION ALL"), "{all}");
}

#[test]
fn a_union_of_different_widths_says_so() {
    let message = failure("SELECT url FROM hits UNION SELECT UserID, duration FROM visits");
    assert!(message.contains("same number of result columns"), "{message}");
}

#[test]
fn a_union_of_two_types_casts_the_narrower_side() {
    let text = plan("SELECT UserID FROM hits UNION ALL SELECT duration FROM visits");
    assert!(text.contains("CAST(#3.0::INTEGER)::BIGINT"), "{text}");
}

#[test]
fn a_name_that_is_not_a_table_is_reported_the_way_duckdb_reports_it() {
    let message = failure("SELECT * FROM nope");
    assert!(message.contains("Table with name nope does not exist!"), "{message}");
}

#[test]
fn a_name_that_is_not_a_function_is_reported_the_way_duckdb_reports_it() {
    let message = failure("SELECT nope(url) FROM hits");
    assert!(message.contains("Scalar Function with name nope does not exist!"), "{message}");
}

#[test]
fn arithmetic_on_a_string_is_refused_before_anything_runs() {
    let message = failure("SELECT url + 1 FROM hits");
    assert!(message.contains("No function matches the given name"), "{message}");
}

#[test]
fn a_qualified_table_name_resolves_and_prints_all_three_parts() {
    let text = plan("SELECT url FROM memory.main.hits");
    assert!(text.contains("Get memory.main.hits AS hits"), "{text}");
}

#[test]
fn identifiers_match_without_regard_to_case_and_keep_the_case_they_were_created_with() {
    let text = plan("SELECT USERID FROM HITS");
    assert!(text.contains("AS UserID"), "{text}");
    assert!(text.contains("Get memory.main.hits AS hits"), "{text}");
}

#[test]
fn what_is_not_bound_yet_says_what_was_written_rather_than_producing_a_wrong_plan() {
    for query in
        ["SELECT counter ** 2 FROM hits", "SELECT url FROM hits LIMIT 10 PERCENT OFFSET (SELECT 3)"]
    {
        let message = failure(query);
        assert!(!message.is_empty(), "{query} should say what it cannot do");
    }
}

/// A row count the binder cannot work out is joined in under the limit and read off a column.
///
/// The join is a `SINGLE` one because a subquery standing where a value should be is at most one
/// row, and the projection over the top drops the column that join added, so the query still
/// answers the columns it asked for.
#[test]
fn a_row_count_the_binder_cannot_work_out_is_read_off_a_column() {
    let text = plan("SELECT url FROM hits LIMIT (SELECT 3)");
    assert!(text.contains("Limit #2.0::INTEGER offset 0"), "{text}");
    assert!(text.contains("Join SINGLE"), "{text}");
    assert!(text.starts_with("Project #3 [#1.0::VARCHAR AS url]"), "{text}");
}

/// A limit written as a share of the input binds to a node of its own.
///
/// It is a separate node rather than a field on the ordinary limit because it cannot emit anything
/// until it has counted the input, and the plan is where that difference is said.
#[test]
fn a_percentage_limit_binds_to_a_node_of_its_own() {
    let text = plan("SELECT url FROM hits LIMIT 10 PERCENT OFFSET 5");
    assert!(text.starts_with("LimitPercent 10% offset 5"), "{text}");
    assert_eq!(
        failure("SELECT url FROM hits LIMIT 101 PERCENT"),
        "Limit percent out of range, should be between 0% and 100%"
    );
}

/// `DESCRIBE` is answered while it is bound, so what comes out is a `VALUES` and nothing else.
///
/// The query it describes is bound, because that is the only way to learn the names and the types,
/// and then it is dropped. Nothing reads a row of it. A plan that still had a `Get` under here
/// would be a plan that opens the table to answer a question about the table's shape, which on a
/// hundred million row file is the difference between instant and a minute.
#[test]
fn describe_is_answered_at_bind_time_and_comes_out_as_rows() {
    let text = plan("DESCRIBE SELECT url, counter FROM hits");
    assert!(text.starts_with("Values"), "{text}");
    assert!(!text.contains("Get"), "the described query is bound and then thrown away: {text}");
    assert!(text.contains("'url'") && text.contains("'VARCHAR'"), "{text}");
    assert!(text.contains("'counter'") && text.contains("'INTEGER'"), "{text}");
    // The six columns are an interface: `rudb-compat` reads the types of every result it compares
    // out of `SELECT column_name, column_type FROM (DESCRIBE ...)`, so the names and the order of
    // them are what another program depends on rather than what a person happens to see.
    assert_eq!(
        plan("SELECT * FROM (DESCRIBE SELECT 1 AS a)").lines().next().map(|line| {
            line.split_once('[')
                .map_or(String::new(), |(_, rest)| rest.trim_end_matches(']').into())
        }),
        Some(
            "#1.0::VARCHAR AS column_name, #1.1::VARCHAR AS column_type, #1.2::VARCHAR AS null, \
             #1.3::VARCHAR AS key, #1.4::VARCHAR AS default, #1.5::VARCHAR AS extra"
                .to_string()
        )
    );
}

/// `NO` survives a column being passed through and does not survive anything being done to it.
///
/// That is the reference binary's rule and not an approximation of it. A projection that hands a
/// column straight on cannot introduce a null, and one that computes anything at all can, so the
/// scope carries the flag and the projection copies it only for a bare column reference.
#[test]
fn describe_says_no_for_a_column_that_refuses_nulls_until_something_is_done_to_it() {
    let mut catalog = catalog();
    catalog
        .create_table(
            QualifiedName::new("memory", "main", "strict"),
            vec![Field::required("a", LogicalType::Integer), Field::new("b", LogicalType::Varchar)],
        )
        .expect("a table nothing else has created");
    let says = |query: &str| {
        bind_sql(query, &catalog).unwrap_or_else(|error| panic!("{query}: {error}")).to_string()
    };
    assert!(says("DESCRIBE strict").contains("'NO'"));
    assert!(says("DESCRIBE SELECT * FROM strict").contains("'NO'"));
    assert!(says("DESCRIBE SELECT a FROM strict").contains("'NO'"));
    assert!(!says("DESCRIBE SELECT a + 1 AS c FROM strict").contains("'NO'"));
    // A set operation takes nulls if either side does, whichever side the `NOT NULL` was on.
    assert!(
        !says("DESCRIBE SELECT a FROM strict UNION ALL SELECT counter FROM hits").contains("'NO'")
    );
}

// The statements that are not queries. What is worth asserting here is the resolution: which table
// the name landed on, which column each value goes into, and which type each one arrives as. What
// happens to the catalog afterwards is `rudb`'s test to write, because the binder never touches it.

use crate::{Bound, bind_statement_sql};

/// One statement, bound against the two table catalog.
fn bound(sql: &str) -> Bound {
    bind_statement_sql(sql, &catalog()).unwrap_or_else(|error| panic!("{sql} should bind: {error}"))
}

/// The message for a statement that is expected not to bind.
fn statement_failure(sql: &str) -> String {
    match bind_statement_sql(sql, &catalog()) {
        Ok(_) => panic!("{sql} should not bind"),
        Err(error) => error.message().to_string(),
    }
}

#[test]
fn a_set_arrives_with_its_value_already_a_value() {
    let Bound::Setting(setting) = bound("SET memory_limit = '1GB'") else { panic!("a setting") };
    assert_eq!(setting.name, "memory_limit");
    assert_eq!(setting.scope, rudb_parse::ast::Scope::Unwritten);
    assert_eq!(setting.value, Some(rudb_common::Value::Varchar("1GB".to_string())));
    let Bound::Setting(setting) = bound("SET LOCAL threads = 4") else { panic!("a setting") };
    assert_eq!(setting.scope, rudb_parse::ast::Scope::Local);
    assert_eq!(setting.value, Some(rudb_common::Value::Integer(4)));
    // A reset is the same statement with nothing on the right of it.
    let Bound::Setting(setting) = bound("RESET memory_limit") else { panic!("a setting") };
    assert_eq!(setting.name, "memory_limit");
    assert_eq!(setting.value, None);
}

#[test]
fn a_setting_name_is_not_looked_up_and_a_bare_word_on_the_right_is() {
    // The binder has no idea what settings exist, so a name that is not one gets through here and
    // is refused by the engine. A bare word as the value is a column reference and there is nothing
    // in scope for it to be, which is better than guessing that an unquoted word meant itself.
    let Bound::Setting(setting) = bound("SET nothing_of_the_sort = 1") else { panic!("a setting") };
    assert_eq!(setting.name, "nothing_of_the_sort");
    assert!(
        statement_failure("SET disabled_optimizers = expression_rewriter")
            .contains("expression_rewriter")
    );
}

#[test]
fn a_create_table_resolves_its_name_and_its_types_before_anything_is_created() {
    let Bound::CreateTable(create) = bound("CREATE TABLE s (a DECIMAL(18, 3), b VARCHAR)") else {
        panic!("a create table");
    };
    assert_eq!(create.name, QualifiedName::new("memory", "main", "s"));
    assert_eq!(
        create.columns,
        vec![
            Field::new("a", LogicalType::decimal(18, 3).unwrap()),
            Field::new("b", LogicalType::Varchar),
        ]
    );
    assert!(create.source.is_none());
}

#[test]
fn a_create_table_as_takes_the_query_s_types_and_the_statement_s_names() {
    let Bound::CreateTable(create) = bound("CREATE TABLE s (id) AS SELECT UserID FROM hits") else {
        panic!("a create table");
    };
    assert_eq!(create.columns, vec![Field::new("id", LogicalType::BigInt)]);
    let source = create.source.expect("a create table as has a query");
    assert!(source.to_string().contains("Get"), "{source}");
}

#[test]
fn a_create_table_that_cannot_work_says_so_before_it_is_run() {
    assert_eq!(
        statement_failure("CREATE TABLE s (a INTEGER, A VARCHAR)"),
        "Column with name A already exists!"
    );
    assert_eq!(
        statement_failure("CREATE TABLE s (a, b) AS SELECT UserID FROM hits"),
        "Target table has more colum names than query result."
    );
    assert!(statement_failure("CREATE TABLE s (a NOSUCHTYPE)").contains("NOSUCHTYPE"));
}

#[test]
fn an_insert_projects_the_source_into_the_target_s_shape() {
    let Bound::Insert(insert) = bound("INSERT INTO visits (duration) VALUES (1)") else {
        panic!("an insert");
    };
    assert_eq!(insert.name, QualifiedName::new("memory", "main", "visits"));
    // Two columns out, in the table's order, with the unnamed one a null of the table's own type
    // rather than an untyped null, so the append never has to ask what type the hole is.
    let printed = insert.source.to_string();
    assert!(printed.contains("NULL::BIGINT AS UserID"), "{printed}");
    assert!(printed.contains("AS duration"), "{printed}");

    // And the cast when the source type is not the column's. The literal is an `INTEGER` and
    // `UserID` is a `BIGINT`, so the widening is in the plan and not in the append.
    let Bound::Insert(insert) = bound("INSERT INTO visits (UserID) VALUES (1)") else {
        panic!("an insert");
    };
    let printed = insert.source.to_string();
    assert!(printed.contains("::BIGINT AS UserID"), "{printed}");
}

#[test]
fn an_insert_checks_the_width_and_the_column_names_against_the_table() {
    assert!(statement_failure("INSERT INTO visits VALUES (1)").contains("2 columns"));
    assert!(statement_failure("INSERT INTO visits (nope) VALUES (1)").contains("nope"));
    assert!(
        statement_failure("INSERT INTO visits (duration, duration) VALUES (1, 2)")
            .contains("twice")
    );
    assert!(statement_failure("INSERT INTO nope VALUES (1)").contains("nope"));
}

#[test]
fn a_drop_of_a_name_that_is_not_there_depends_on_if_exists() {
    let Bound::DropTable(drop) = bound("DROP TABLE hits, visits") else { panic!("a drop") };
    assert_eq!(drop.names.len(), 2);
    // With `IF EXISTS` the name that does not resolve is left out rather than kept and forgiven
    // later, so what comes back is a list of drops that all succeed.
    let Bound::DropTable(drop) = bound("DROP TABLE IF EXISTS hits, nope") else { panic!("a drop") };
    assert_eq!(drop.names, vec![QualifiedName::new("memory", "main", "hits")]);
    assert!(statement_failure("DROP TABLE nope").contains("nope"));
}

#[test]
fn values_binds_to_a_values_node_with_the_types_the_rows_agree_on() {
    let Bound::Query(plan) = bound("VALUES (1, 'a'), (2, 'b')") else { panic!("a query") };
    let printed = plan.to_string();
    assert!(printed.starts_with("Values"), "{printed}");
    assert!(printed.contains("col0"), "{printed}");
    assert!(statement_failure("VALUES (1), (2, 3)").contains("same length"));
    assert!(statement_failure("VALUES (1), ('a')").contains("Cannot combine"));
}

#[test]
fn a_table_function_binds_to_its_own_node_and_not_to_a_scan() {
    let printed = plan("SELECT * FROM range(3)");
    assert!(printed.contains("TableFunction range"), "{printed}");
    // The argument is in the plan as an expression and the rows are not, which is the whole reason
    // this is not a Values. A three million row range should be three numbers in a plan dump.
    assert!(printed.contains("args="), "{printed}");
    assert!(!printed.contains("Get"), "{printed}");
}

#[test]
fn an_argument_is_cast_to_the_type_the_function_takes() {
    // `range` takes BIGINT and the literal is an INTEGER, so the cast is written into the plan
    // here rather than decided by the operator at run time.
    let printed = plan("SELECT * FROM range(3)");
    assert!(printed.contains("CAST"), "{printed}");
}

#[test]
fn a_table_function_can_be_aliased_the_same_ways_a_table_can() {
    assert!(plan("SELECT i FROM range(3) t(i)").contains("AS i"));
    assert!(plan("SELECT t.range FROM range(3) t").contains("AS range"));
    // More aliases than columns is the one rule here that does not match DuckDB. DuckDB takes
    // `range(3) t(i, j)` and ignores the second name, and this rejects it, because the check is
    // the same one a table alias goes through and loosening it for one source would be loosening
    // it for all of them. It is written down here rather than left to be found later.
    assert!(failure("SELECT i FROM range(3) t(i, j)").contains("2 columns specified"));
}

#[test]
fn a_name_that_is_not_a_table_function_does_not_fall_through_to_the_table_lookup() {
    // `hits` is a real table in this catalog, so `hits(1)` finding it would be the worst version
    // of this bug rather than the most obvious one.
    assert!(failure("SELECT * FROM hits(1)").contains("hits"));
    assert!(failure("SELECT * FROM nowhere.range(3)").contains("nowhere"));
    assert!(failure("SELECT * FROM range()").contains("range"));
}

#[test]
fn a_setting_is_folded_to_a_constant_of_the_type_the_setting_holds() {
    let mut session = Session::new();
    session.set("threads", "8");
    session.set("memory_limit", "1.0 GiB");
    let printed = bind_sql_with("SELECT current_setting('threads')", &catalog(), &session)
        .expect("a setting reads")
        .to_string();
    // The call is gone by the time there is a plan, which is what upstream's EXPLAIN shows too.
    assert!(printed.contains("8::BIGINT"), "{printed}");
    let text = bind_sql_with("SELECT current_setting('memory_limit')", &catalog(), &session)
        .expect("a setting reads")
        .to_string();
    assert!(text.contains("'1.0 GiB'::VARCHAR"), "{text}");
}

#[test]
fn a_binder_with_no_database_behind_it_has_no_settings_to_read() {
    // `bind_sql` passes an empty session, so every name is unrecognized rather than answered with
    // a default this crate would have had to invent.
    let message = failure("SELECT current_setting('threads')");
    assert!(message.starts_with("unrecognized configuration parameter \"threads\""), "{message}");
}

#[test]
fn bound_expressions_keep_the_ast_source_ranges() {
    let sql = "SELECT 1 + 22";
    let plan = bind_sql(sql, &catalog()).expect("the expression binds");
    let spans: Vec<Span> = (0..plan.expr_count() as u32).map(|expr| plan.expr_span(expr)).collect();
    assert!(spans.contains(&Span::new(7, 8)), "the first literal is missing from {spans:?}");
    assert!(spans.contains(&Span::new(11, 13)), "the second literal is missing from {spans:?}");
    assert!(spans.contains(&Span::new(7, 13)), "the addition is missing from {spans:?}");
    assert_eq!(plan.node_span(plan.root()), Span::new(0, sql.len() as u32));
}

#[test]
fn a_binder_error_keeps_the_smallest_expression_range() {
    let sql = "SELECT missing + 1 FROM hits";
    let error = bind_sql(sql, &catalog()).expect_err("the column is not in scope");
    assert_eq!(error.span(), Some(Span::new(7, 14)));
}

#[test]
fn a_correlated_scalar_subquery_records_a_dependent_join() {
    let printed = plan(
        "SELECT h.url, (SELECT v.duration FROM visits v WHERE v.UserID = h.UserID) FROM hits h",
    );
    assert!(printed.contains("DependentJoin SINGLE"), "{printed}");
    assert!(printed.contains("#1.0::BIGINT = #0.0::BIGINT"), "{printed}");
}

#[test]
fn an_inner_column_shadows_an_outer_column() {
    let printed = plan("SELECT (SELECT UserID FROM visits) FROM hits");
    assert!(!printed.contains("DependentJoin"), "{printed}");
    assert!(printed.contains("Join SINGLE"), "{printed}");
}

#[test]
fn a_correlated_exists_subquery_records_a_dependent_join() {
    let printed = plan(
        "SELECT h.url FROM hits h WHERE EXISTS (SELECT 1 FROM visits v WHERE v.UserID = h.UserID)",
    );
    assert!(printed.contains("DependentJoin SINGLE"), "{printed}");
}

#[test]
fn a_window_is_an_operator_and_the_target_reads_its_column() {
    // The call is not in the projection. The operator computes it and the projection reads column
    // zero of the operator, which is the same shape the aggregate path produces and for the same
    // reason: everything above the operator is arithmetic over a column it already has.
    assert_eq!(
        plan("SELECT sum(counter) OVER () FROM hits"),
        "Project #2 [#1.0::HUGEINT AS \"sum(counter) OVER ()\"]\n  \
         Window #1 partition=[] order=[] \
         frame=RANGE UNBOUNDED PRECEDING TO CURRENT ROW EXCLUDE NO OTHERS \
         expressions=[sum(#0.2::INTEGER)::HUGEINT]\n    \
         Get memory.main.hits AS hits #0 [UserID::BIGINT, url::VARCHAR, counter::INTEGER]\n"
    );
}

#[test]
fn the_default_frame_is_the_one_the_standard_gives_a_window_with_no_frame_written() {
    // Written out because it is the answer to a question people get wrong. A window with an order
    // and no frame is a running total, not a total, and the frame above says so: it runs from the
    // start of the partition to the current row.
    let printed = plan("SELECT sum(counter) OVER (PARTITION BY url ORDER BY counter) FROM hits");
    assert!(
        printed.contains(
            "Window #1 partition=[#0.1::VARCHAR] order=[#0.2::INTEGER ASC NULLS LAST] \
             frame=RANGE UNBOUNDED PRECEDING TO CURRENT ROW EXCLUDE NO OTHERS"
        ),
        "{printed}"
    );
}

#[test]
fn two_calls_that_agree_on_everything_are_one_operator_and_one_column() {
    // The same rule two identical aggregates over one grouping get. The sum is computed once and
    // both targets read it, so the arithmetic in the first target is over the same column the
    // second target is.
    let printed = plan("SELECT sum(counter) OVER () + 1, sum(counter) OVER () FROM hits");
    assert_eq!(printed.matches("Window #1").count(), 1, "{printed}");
    assert!(printed.contains("expressions=[sum(#0.2::INTEGER)::HUGEINT]"), "{printed}");
    assert_eq!(printed.matches("#1.0::HUGEINT").count(), 2, "{printed}");
}

#[test]
fn two_calls_that_disagree_are_two_operators_stacked_in_the_order_they_were_written() {
    // They disagree on the order, so they cannot share a sort, so they cannot share an operator.
    // The first one written ends up at the bottom, which is the order somebody reading the plan
    // next to the query expects to find them in.
    let printed = plan("SELECT sum(counter) OVER (), count(*) OVER (ORDER BY counter) FROM hits");
    let first = printed.find("Window #2").expect("the second run");
    let second = printed.find("Window #1").expect("the first run");
    assert!(first < second, "the run written first should be the deeper one\n{printed}");
    assert!(printed[first..].contains("expressions=[count_star()::BIGINT]"), "{printed}");
}

#[test]
fn a_star_inside_a_window_is_count_star_and_nothing_else_takes_one() {
    let printed = plan("SELECT count(*) OVER () FROM hits");
    assert!(printed.contains("expressions=[count_star()::BIGINT]"), "{printed}");
    assert_eq!(failure("SELECT sum(*) OVER () FROM hits"), "* is not allowed in sum()");
}

/// `count()` written with nothing in it is the same function as `count(*)`, inside an `OVER` and
/// out of one. Every other aggregate given no arguments is still an arity mistake and says so.
#[test]
fn count_with_no_arguments_is_the_same_function_as_count_with_a_star() {
    let printed = plan("SELECT count() FROM hits");
    assert!(printed.contains("aggregates=[count_star()::BIGINT]"), "{printed}");
    let windowed = plan("SELECT count() OVER () FROM hits");
    assert!(windowed.contains("expressions=[count_star()::BIGINT]"), "{windowed}");
    assert!(failure("SELECT sum() FROM hits").starts_with("No function matches"));
}

#[test]
fn the_window_runs_after_the_grouping_and_after_the_having() {
    // Measured on the pinned binary rather than reasoned about. The window totals one group here,
    // the one that survived the filter, which is only true if the operator sits above the filter.
    // Putting it below would total both groups and the query would answer a different number.
    let printed = plan(
        "SELECT url, sum(count(counter)) OVER () FROM hits GROUP BY url HAVING count(counter) > 1",
    );
    let aggregate = printed.find("Aggregate #1").expect("the grouping");
    let filter = printed.find("Filter").expect("the having");
    let window = printed.find("Window #2").expect("the window");
    assert!(window < filter && filter < aggregate, "{printed}");
    // And it reads the aggregate's output rather than the table's, which is the whole point of
    // being allowed to write a window over a grouped block at all.
    assert!(printed.contains("expressions=[sum(#1.1::BIGINT)::HUGEINT]"), "{printed}");
}

#[test]
fn the_grouping_rule_applies_inside_the_over_as_well_as_to_the_arguments() {
    // A window does not exempt anything from the grouping rule. The column has to be grouped or
    // aggregated wherever it appears, and that includes the partition keys and the order keys,
    // which is the part that is easy to leave out.
    for query in [
        "SELECT sum(counter) OVER () FROM hits GROUP BY url",
        "SELECT count(*) OVER (PARTITION BY counter) FROM hits GROUP BY url",
        "SELECT count(*) OVER (ORDER BY counter) FROM hits GROUP BY url",
    ] {
        assert_eq!(
            failure(query),
            "column \"counter\" must appear in the GROUP BY clause or must be part of an \
             aggregate function",
            "{query}"
        );
    }
    let printed =
        plan("SELECT count(*) OVER (PARTITION BY url ORDER BY url) FROM hits GROUP BY url");
    assert!(printed.contains("partition=[#1.0::VARCHAR]"), "{printed}");
}

#[test]
fn a_window_belongs_in_the_select_or_the_order_by_and_nowhere_else() {
    assert_eq!(
        failure("SELECT counter FROM hits WHERE sum(counter) OVER () > 1"),
        "WHERE clause cannot contain window functions!"
    );
    assert_eq!(
        failure("SELECT counter FROM hits GROUP BY counter HAVING sum(counter) OVER () > 1"),
        "HAVING clause cannot contain window functions!"
    );
    assert_eq!(
        failure("SELECT counter FROM hits GROUP BY sum(counter) OVER ()"),
        "GROUP BY clause cannot contain window functions!"
    );
    // A join condition says the `WHERE` sentence, which is upstream's wording and not a shortcut
    // taken here. The pinned binary refuses `ON sum(a.i) OVER () = b.i` with those exact words.
    assert_eq!(
        failure("SELECT h.url FROM hits h JOIN visits v ON sum(h.counter) OVER () = v.UserID"),
        "WHERE clause cannot contain window functions!"
    );
    // And the two places it belongs both work. A window written only in the `ORDER BY` still has
    // to be computed, so it goes through the hidden target the sort keys already use and the
    // operator lands under the sort rather than over it.
    let printed = plan("SELECT counter FROM hits ORDER BY sum(counter) OVER ()");
    let sort = printed.find("Sort").expect("the sort");
    let window = printed.find("Window").expect("the window");
    assert!(sort < window, "{printed}");
}

#[test]
fn a_window_and_an_aggregate_cannot_be_written_inside_each_other() {
    assert_eq!(
        failure("SELECT sum(sum(counter) OVER ()) FROM hits"),
        "aggregate function calls cannot contain window function calls"
    );
    assert_eq!(
        failure("SELECT sum(sum(counter) OVER ()) OVER () FROM hits"),
        "window function calls cannot be nested"
    );
}

#[test]
fn the_name_inside_an_over_has_to_be_one_that_can_be_a_window() {
    // Two different refusals for two different situations, which is what the pinned binary does
    // and is worth keeping apart. A name it knows as a scalar and a name it does not know at all
    // each get their own sentence there.
    assert_eq!(
        failure("SELECT abs(counter) OVER () FROM hits"),
        "abs is not an aggregate function"
    );
    assert_eq!(
        failure("SELECT nosuchwindow(counter) OVER () FROM hits"),
        "Aggregate Function with name nosuchwindow does not exist!"
    );
}

#[test]
fn fill_asks_the_query_for_one_sort_key_and_a_type_it_can_subtract() {
    // Every one of these sentences came off the pinned binary, including the order they come in.
    // The argument is checked before the sort key even when both are wrong, an `OVER` with no
    // order at all is refused with the same words two sort keys are, and the two lists of types
    // are not the same list: a TIMETZ can be ordered by and cannot be filled, and an INTERVAL is
    // neither. The doubled quotes in the DISTINCT sentence are upstream's too.
    assert_eq!(
        failure("SELECT fill(counter) OVER () FROM hits"),
        "FILL functions must have only one ORDER BY expression"
    );
    assert_eq!(
        failure("SELECT fill(counter) OVER (ORDER BY counter, url) FROM hits"),
        "FILL functions must have only one ORDER BY expression"
    );
    assert_eq!(
        failure("SELECT fill(url) OVER (ORDER BY counter) FROM hits"),
        "FILL argument must support subtraction"
    );
    assert_eq!(
        failure("SELECT fill(counter) OVER (ORDER BY url) FROM hits"),
        "FILL ordering must support subtraction"
    );
    assert_eq!(
        failure("SELECT fill(url) OVER (ORDER BY counter, url) FROM hits"),
        "FILL argument must support subtraction"
    );
    assert_eq!(
        failure("SELECT fill(DISTINCT counter) OVER (ORDER BY counter) FROM hits"),
        "DISTINCT is not implemented for the window function \"\"fill\"\""
    );
    assert_eq!(
        failure("SELECT fill(counter IGNORE NULLS) OVER (ORDER BY counter) FROM hits"),
        "RESPECT/IGNORE NULLS is not supported for the window function \"fill\""
    );
}

#[test]
fn fill_binds_with_the_type_it_was_given_and_nothing_is_cast_on_the_way_in() {
    // The pin prints the row as `fill(col0 ANY) -> ANY`, so the declaration says nothing and the
    // argument says everything. A DECIMAL stays the DECIMAL it arrived as, width and scale and all.
    let counted = plan("SELECT fill(counter) OVER (ORDER BY counter) FROM hits");
    assert!(counted.contains("expressions=[fill(#0.2::INTEGER)::INTEGER]"), "{counted}");
    let scaled = plan("SELECT fill(counter * 1.5) OVER (ORDER BY counter) FROM hits");
    assert!(scaled.contains("::DECIMAL(12,1))::DECIMAL(12,1)]"), "{scaled}");
}

#[test]
fn a_value_window_binds_with_the_first_arguments_type_and_a_count_that_is_a_bigint() {
    // Five names and one shape. What is worth checking is the part the shape decides rather than
    // the part the executor does: the answer is the value's own type, the count is cast to a
    // BIGINT, and the default is cast to the value's type rather than left where it started.
    let read = plan("SELECT first_value(url) OVER (ORDER BY counter) FROM hits");
    assert!(read.contains("expressions=[first_value(#0.1::VARCHAR)::VARCHAR]"), "{read}");
    let shifted = plan("SELECT lag(counter, 2) OVER (ORDER BY counter) FROM hits");
    assert!(
        shifted.contains("expressions=[lag(#0.2::INTEGER, CAST(2::INTEGER)::BIGINT)::INTEGER]"),
        "{shifted}"
    );
    let defaulted = plan("SELECT lag(counter, 1, 0.5) OVER (ORDER BY counter) FROM hits");
    assert!(defaulted.contains("CAST(0.5::DECIMAL(2,1))::INTEGER"), "{defaulted}");
    let nth = plan("SELECT nth_value(url, 3) OVER (ORDER BY counter) FROM hits");
    assert!(nth.contains("nth_value(#0.1::VARCHAR, CAST(3::INTEGER)::BIGINT)::VARCHAR"), "{nth}");
}

#[test]
fn a_ranking_window_binds_to_a_window_operator_and_the_name_says_what_it_reads() {
    // The ranking windows go through the same table the aggregates do, so what is worth checking
    // here is that they reach the operator at all and come back with the type the pin gives them.
    let counted = plan("SELECT row_number() OVER (ORDER BY counter) FROM hits");
    assert!(counted.contains("expressions=[row_number()::BIGINT]"), "{counted}");
    let divided = plan("SELECT percent_rank() OVER (ORDER BY counter) FROM hits");
    assert!(divided.contains("expressions=[percent_rank()::DOUBLE]"), "{divided}");
    let cut = plan("SELECT ntile(4) OVER (ORDER BY counter) FROM hits");
    assert!(cut.contains("expressions=[ntile(CAST(4::INTEGER)::BIGINT)::BIGINT]"), "{cut}");
}

#[test]
fn a_filter_is_a_condition_over_the_rows_and_reaches_the_call_as_one_more_input() {
    // The predicate is cast to BOOLEAN the way a WHERE is, and it lands on the call rather than on
    // the scan, because it decides which rows this one aggregate reads and not which rows the
    // query has.
    let summed = plan("SELECT sum(counter) FILTER (WHERE counter > 1) FROM hits");
    assert!(
        summed.contains(
            "aggregates=[sum(#0.2::INTEGER FILTER (#0.2::INTEGER > 1::INTEGER)::BOOLEAN)::HUGEINT]"
        ),
        "{summed}"
    );
    let counted = plan("SELECT count(*) FILTER (WHERE counter > 1) FROM hits");
    assert!(
        counted.contains(
            "aggregates=[count_star(FILTER (#0.2::INTEGER > 1::INTEGER)::BOOLEAN)::BIGINT]"
        ),
        "{counted}"
    );
    let cast = plan("SELECT sum(counter) FILTER (WHERE counter) FROM hits");
    assert!(cast.contains("FILTER CAST(#0.2::INTEGER)::BOOLEAN)"), "{cast}");
    let windowed = plan("SELECT sum(counter) FILTER (WHERE counter > 1) OVER () FROM hits");
    assert!(
        windowed.contains(
            "expressions=[sum(#0.2::INTEGER FILTER (#0.2::INTEGER > 1::INTEGER)::BOOLEAN)::HUGEINT"
        ),
        "{windowed}"
    );
}

#[test]
fn a_filter_is_refused_where_there_is_nothing_for_it_to_keep_or_drop() {
    // Upstream's three sentences. A scalar function is not a call that reads several rows, so none
    // of the three modifiers mean anything on one, and the one sentence names all three whichever
    // one was written. A ranking window reads no values at all, so a predicate over the values has
    // nothing to work on, and that one is refused with the doubled quotes DISTINCT is refused with.
    assert_eq!(
        failure("SELECT abs(counter) FILTER (WHERE counter > 1) FROM hits"),
        "Function \"abs\" is a Scalar Function. \"DISTINCT\", \"FILTER\", and \"ORDER BY\" are \
         only applicable to window and aggregate functions."
    );
    assert_eq!(
        failure("SELECT abs(DISTINCT counter) FROM hits"),
        "Function \"abs\" is a Scalar Function. \"DISTINCT\", \"FILTER\", and \"ORDER BY\" are \
         only applicable to window and aggregate functions."
    );
    assert_eq!(
        failure("SELECT row_number() FILTER (WHERE counter > 1) OVER () FROM hits"),
        "FILTER is not implemented for the window function \"\"row_number\"\""
    );
    assert_eq!(
        failure("SELECT lag(counter) FILTER (WHERE counter > 1) OVER (ORDER BY counter) FROM hits"),
        "FILTER is not implemented for the window function \"\"lag\"\""
    );
    // A name nobody has is still a name nobody has. The sentence above comes after the catalog has
    // been asked and not before it, so the answer here is the catalog's and not that one.
    assert!(
        failure("SELECT nosuch(counter) FILTER (WHERE counter > 1) FROM hits")
            .contains("nosuch does not exist"),
        "an unknown name should reach the catalog first"
    );
}

#[test]
fn what_a_filter_may_contain_depends_on_what_the_call_it_hangs_off_is() {
    // An aggregate's filter is bound as if it were inside the call, so an aggregate in it is
    // refused on its own terms and a window in it is refused the way a window inside an aggregate
    // is. A window's filter is bound inside the window instead, so a window in it is a nested
    // window while an aggregate in it is an ordinary aggregate over the same rows.
    assert_eq!(
        failure("SELECT sum(counter) FILTER (WHERE sum(counter) > 1) FROM hits"),
        "aggregate functions are not allowed in FILTER"
    );
    assert_eq!(
        failure("SELECT sum(counter) FILTER (WHERE row_number() OVER () > 1) FROM hits"),
        "aggregate function calls cannot contain window function calls"
    );
    assert_eq!(
        failure("SELECT sum(counter) FILTER (WHERE row_number() OVER () > 1) OVER () FROM hits"),
        "window function calls cannot be nested"
    );
    let inner = plan(
        "SELECT sum(counter) FILTER (WHERE sum(counter) > 1) OVER () FROM hits GROUP BY counter",
    );
    assert!(inner.contains("Window"), "{inner}");
}

#[test]
fn a_ranking_window_is_only_a_window_and_says_so_where_it_cannot_be_one() {
    // Upstream's two sentences. A name that is only ever a window is not a function call on its
    // own, and a DISTINCT in front of one has nothing to collapse because the call reads no values.
    assert_eq!(failure("SELECT row_number() FROM hits"), "Window functions are not supported here");
    assert_eq!(
        failure("SELECT row_number(DISTINCT) OVER () FROM hits"),
        "DISTINCT is not implemented for the window function \"\"row_number\"\""
    );
}

#[test]
fn a_range_frame_with_an_offset_needs_exactly_one_thing_to_measure_the_offset_from() {
    // A `RANGE` offset is a distance from the current row's key, so there has to be one key for it
    // to be a distance from. `ROWS` counts rows instead and does not care.
    assert_eq!(
        failure("SELECT sum(counter) OVER (RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM hits"),
        "RANGE frames must have only one ORDER BY expression"
    );
    assert_eq!(
        failure(
            "SELECT sum(counter) OVER (ORDER BY url, counter \
             RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM hits"
        ),
        "RANGE frames must have only one ORDER BY expression"
    );
    let printed =
        plan("SELECT sum(counter) OVER (ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM hits");
    assert!(printed.contains("frame=ROWS 1::INTEGER PRECEDING TO CURRENT ROW"), "{printed}");
    let printed = plan(
        "SELECT sum(counter) OVER (ORDER BY counter \
         RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM hits",
    );
    assert!(printed.contains("frame=RANGE 1::INTEGER PRECEDING TO CURRENT ROW"), "{printed}");
}

#[test]
fn a_window_in_a_subquery_stays_in_the_block_that_wrote_it() {
    // A block can be bound inside another one without a subquery expression in between, so the
    // collected runs have to be put aside for the duration. If they were not, the inner window
    // would come out attached to the outer block and the plan would be wrong in a way that only
    // shows up on a query with a window on both sides.
    let printed = plan("SELECT sum(counter) OVER () FROM (SELECT counter FROM hits) t");
    assert_eq!(printed.matches("Window").count(), 1, "{printed}");
}
