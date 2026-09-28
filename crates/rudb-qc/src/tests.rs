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
    // `clean` has no NULL and many chunks.
    let clean = QualifiedName::new("memory", "main", "clean");
    catalog
        .create_table(clean.clone(), vec![Field::new("x", LogicalType::Integer)])
        .expect("a fresh table");
    let rows: Vec<Vec<Value>> = (0..40_000).map(|i| vec![Value::Integer(i)]).collect();
    catalog
        .table_mut(&clean)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&rows)
        .expect("rows of the table's own types");
    // `runs` holds its keys in runs, some of them NULL, the way a file sorted on them does.
    let runs = QualifiedName::new("memory", "main", "runs");
    catalog
        .create_table(
            runs.clone(),
            vec![Field::new("k", LogicalType::BigInt), Field::new("j", LogicalType::Integer)],
        )
        .expect("a fresh table");
    let rows: Vec<Vec<Value>> = (0..20_000i64)
        .map(|i| {
            let k = if (i / 50) % 5 == 2 { Value::Null } else { Value::BigInt(i / 37 % 90) };
            let j =
                if (i / 60) % 7 == 4 { Value::Null } else { Value::Integer((i / 100 % 3) as i32) };
            vec![k, j]
        })
        .collect();
    catalog
        .table_mut(&runs)
        .expect("the table just created")
        .rows_mut()
        .append_rows(&rows)
        .expect("rows of the table's own types");
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

/// The compiled engine's rows, which every tier this build has must agree on to the bit, with
/// every function of the module compiled by the tiers that compile, again with the query moved
/// between the tiers at every morsel and at random ones, and with each chunk cut into morsels of an
/// odd size.
fn compiled(text: &str) -> Result<Vec<Vec<Value>>> {
    let mut runs = vec![
        Options { tier: Tier::Interp, ..Options::default() },
        Options::default(),
        Options { tier: Tier::Interp, morsel: 1000, ..Options::default() },
    ];
    for tier in [Tier::Clif, Tier::Direct].into_iter().filter(|t| t.built()) {
        for switch in [Switch::Off, Switch::Every(1), Switch::Random(7)] {
            runs.push(Options { tier, switch, ..Options::default() });
        }
        runs.push(Options { tier, morsel: 333, ..Options::default() });
    }
    let mut answers: Vec<(Options, Result<Vec<Vec<Value>>>)> = Vec::new();
    for options in runs {
        let answer = on(text, options);
        if let Some((first, earlier)) = answers.first() {
            let same = match (earlier, &answer) {
                (Ok(a), Ok(b)) => format!("{a:?}") == format!("{b:?}"),
                (Err(a), Err(b)) => a.to_string() == b.to_string() && a.code() == b.code(),
                _ => false,
            };
            assert!(same, "{options:?} gave {answer:?} and {first:?} gave {earlier:?}");
        }
        answers.push((options, answer));
    }
    answers.swap_remove(0).1
}

/// The compiled engine's rows on one tier.
fn on(text: &str, options: Options) -> Result<Vec<Vec<Value>>> {
    answer(text, options).map(|a| rows(&a.chunks))
}

/// The compiled engine's answer on one tier.
fn answer(text: &str, options: Options) -> Result<Answer> {
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    let cancel = Cancel::new();
    let pool = Pool::default();
    // The rows are read even where the table's statistics answer, so it is the code that is tested.
    let options = Options { rows: true, ..options };
    let compiled = compile_with(&plan, &cancel, options).expect("the compiled engine takes it");
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
    let answer = compiled.run(&plan, under)?;
    // A named tier compiles every function that ran, and `auto` leaves the small ones on
    // `interp`. The version of a body for morsels with no NULL is compiled only when one comes.
    let report = &answer.report;
    if matches!(options.tier, Tier::Clif | Tier::Direct) {
        assert!(report.native > 0 && report.fallbacks.is_empty(), "{report}");
    }
    Ok(answer)
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
fn auto_leaves_a_pipeline_over_one_morsel_on_interp_and_compiles_nothing() {
    let answer = answer(SCAN, Options::default()).expect("the scan runs");
    assert_eq!(answer.chunks.iter().map(Chunk::len).sum::<usize>(), 5000);
    let report = &answer.report;
    assert_eq!((report.small, report.native, report.bytes), (1, 0, 0), "{report}");
    assert_eq!(report.compile, std::time::Duration::ZERO, "{report}");
}

#[test]
fn a_named_tier_compiles_a_pipeline_when_it_starts_and_not_before() {
    for tier in [Tier::Clif, Tier::Direct].into_iter().filter(|t| t.built()) {
        let plan = Plan::parse(SCAN).expect("a well formed plan");
        let options = Options { tier, ..Options::default() };
        let compiled = compile_with(&plan, &Cancel::new(), options).expect("it is taken");
        assert_eq!(compiled.report().native, 0, "{tier}");
        let answer = answer(SCAN, options).expect("the scan runs");
        assert_eq!((answer.report.small, answer.report.native), (0, 1), "{tier}");
    }
}

#[test]
fn a_morsel_with_no_null_runs_the_version_that_checks_none_and_one_with_a_null_does_not() {
    let clean = "Aggregate #1 groups=[] aggregates=[count(#0.0::INTEGER)::BIGINT, sum(#0.0::INTEGER)::HUGEINT, min(#0.0::INTEGER)::INTEGER]\n  Get memory.main.big AS big #0 [x::INTEGER]";
    same(clean, true);
    let options = Options { tier: Tier::Interp, ..Options::default() };
    let report = answer(clean, options).expect("the aggregate runs").report;
    assert_eq!((report.nonull, report.deopts), (1, 0), "{report}");
    let report = answer(SCAN, options).expect("the scan runs").report;
    assert_eq!(report.nonull, 0, "{report}");
}

#[test]
fn a_guard_that_fails_in_a_body_sends_the_morsel_back_and_is_given_up_after_three() {
    let text = "Project #1 [\"+\"(#0.0::INTEGER, 1::INTEGER)::INTEGER AS y]\n  Get memory.main.clean AS clean #0 [x::INTEGER]";
    let (first, _) = both(text);
    let catalog = catalog();
    let plan = Plan::parse(text).expect("a well formed plan");
    let cancel = Cancel::new();
    let (pool, memory, seams, session) =
        (Pool::default(), Memory::unlimited(), rudb_seam::Settings::new(), Session::new());
    let under = Under {
        catalog: &catalog,
        cancel: &cancel,
        memory: &memory,
        seams: &seams,
        session: &session,
        pool: &pool,
    };
    for tier in [Tier::Interp, Tier::Clif, Tier::Direct].into_iter().filter(|t| t.built()) {
        let options = Options { tier, fresh: true, ..Options::default() };
        let mut compiled = compile_with(&plan, &cancel, options).expect("it is taken");
        let body = compiled.query.bodies[0].as_ref().expect("a pipeline");
        let nonull = body.nonull.clone().expect("a version with no NULL");
        let module = &compiled.query.module;
        let site = module.guards.iter().position(|g| g.fallback == body.func).expect("its guard");
        // The version with no NULL fails its guard before it does anything, on every morsel.
        let printed = rudb_qc_ir::print::print(module);
        let at = printed.find(&format!("func @{nonull} ")).expect("the version is printed");
        let entry = at + printed[at..].find("):\n").expect("an entry block") + 3;
        let changed =
            format!("{}  guard false, !G{site}\n{}", &printed[..entry], &printed[entry..]);
        compiled.query.module = rudb_qc_ir::parse(&changed).expect("the changed module parses");
        compiled.tiers = Tiers::new(&compiled.query.module, options);
        let answer = compiled.run(&plan, under).expect("the projection runs");
        assert_eq!(rows(&answer.chunks), first, "{tier}");
        let report = &answer.report;
        assert_eq!((report.deopts, report.nonull), (3, 0), "{tier}: {report}");
    }
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
fn a_grouped_aggregate_over_runs_of_one_key_matches_the_first_engine() {
    let aggs = "aggregates=[count_star()::BIGINT, count(#0.1::INTEGER)::BIGINT, sum(#0.1::INTEGER)::HUGEINT, min(#0.1::INTEGER)::INTEGER]";
    let scan = "Get memory.main.runs AS runs #0 [k::BIGINT, j::INTEGER]";
    same(&format!("Aggregate #1 groups=[#0.0::BIGINT] {aggs}\n  {scan}"), false);
    same(&format!("Aggregate #1 groups=[#0.0::BIGINT, #0.1::INTEGER] {aggs}\n  {scan}"), false);
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
    // Many groups have the same count, so the edge the top is cut at moves through ties.
    same(
        "TopN 7 offset 2 [#1.1::BIGINT ASC NULLS LAST, #1.0::BIGINT DESC NULLS LAST]\n  Aggregate #1 groups=[#0.0::BIGINT] aggregates=[count_star()::BIGINT]\n    Get memory.main.runs AS runs #0 [k::BIGINT, j::INTEGER]",
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
