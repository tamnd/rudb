//! The in-repo micro-benchmark table.
//!
//! This measures the part of the engine that exists. Today that is the front end: text in, tokens,
//! a parse tree, an AST out. It is deliberately not the same thing as `tamnd/rudb-bench`, which
//! measures whole queries against whole engines and is the only place a number anybody quotes ever
//! comes from. This one answers a smaller question that comes up on a Tuesday, which is whether the
//! change you are about to push made the parser slower.
//!
//! Two rules from `spec/15-rudb-bench.md` are the reason this file is longer than a loop and a
//! `println!`.
//!
//! Rule two says the median of at least five runs with the interquartile range, never a minimum.
//! The sampling that satisfies it lives in [`crate::timing`], which is where the kernel table gets
//! it from too, so that the two tables are produced by the same apparatus rather than by two
//! apparatuses that used to agree.
//!
//! Rule ten says a micro-benchmark number never appears without the end-to-end number it is
//! supposed to explain. rudb runs a query now, but nothing here times one, so there is still no
//! query time to put underneath this table, and the honest thing is to say so in the output every
//! time rather than to let a parser number stand on its own and start sounding like a result. The
//! query times live in `tamnd/rudb-bench` against whole engines, which is where they belong. The
//! end-to-end column here is the whole front end, and it is measured rather than summed from the
//! three stage columns, so the stages are an explanation of a number rather than its definition.

use std::hint::black_box;
use std::path::Path;

use rudb_parse::generated::rules::PROGRAM;
use rudb_parse::{Token, Tree, parse_ast, parse_tokens, tokenize, transform};

use crate::timing::{Number, build_line, rebuild, shared_caveats, show, time};

/// The workload.
///
/// This is not the parser's test corpus and it must not become it. The corpus grows every time
/// somebody covers another piece of the grammar, which is exactly what a corpus is for and exactly
/// what makes it useless as a workload: a total that gets bigger because a test was added looks
/// identical to a total that got bigger because the parser got slower. So this list is its own
/// thing, and it changes only in a commit that changes it on purpose, with the numbers from before
/// and after in the pull request, because the two totals are not comparable across such a commit.
///
/// Every case has to reach an AST. The transformer answers every rule in the table and one of its
/// answers is a Not implemented error, which is a fast path that does none of the work the transform
/// column claims to be timing. A window function or a `CREATE TABLE` in here today would put a small
/// number in that column and read as the transformer being quick. There is a test for it, which is
/// also what will notice when M1 makes one of them eligible.
///
/// The shapes are chosen to have different costs rather than to be representative of anything. A
/// literal is the floor, the ClickBench query is what the project is actually pointed at, and the
/// nested parentheses are there because a chain of unary precedence rules is where a PEG matcher
/// with a bad FIRST filter falls apart.
const WORKLOAD: &[(&str, &str)] = &[
    ("select literal", "SELECT 1"),
    ("arithmetic", "SELECT 1 + 2 * 3 - 4 / 5 % 6 + 7 * 8 - 9"),
    ("filter and project", "SELECT a, b, c FROM t WHERE a = 1 AND b > 2 OR NOT c"),
    ("group by", "SELECT count(*), sum(x), avg(y) FROM t GROUP BY a, b HAVING count(*) > 1"),
    ("three way join", "SELECT * FROM a LEFT JOIN b USING (id) INNER JOIN c ON c.id = a.id"),
    (
        "order by and limit",
        "SELECT a, b FROM t ORDER BY a ASC, b DESC NULLS LAST LIMIT 10 OFFSET 5",
    ),
    ("set operation", "SELECT a FROM t UNION ALL SELECT b FROM u EXCEPT SELECT c FROM v"),
    ("derived table", "SELECT s.x FROM (SELECT y AS x FROM u WHERE u.k = 1) s WHERE s.x > 1"),
    ("case expression", "SELECT CASE WHEN a THEN 1 WHEN b THEN 2 WHEN c THEN 3 ELSE 4 END FROM t"),
    ("casts and literals", "SELECT CAST(a AS INTEGER), b::VARCHAR, 1.5, 'text', TRUE, NULL FROM t"),
    ("nested parentheses", "SELECT (((((((((a + 1))))))))) FROM t"),
    (
        "clickbench q13",
        "SELECT \"SearchPhrase\", count(*) AS c FROM hits WHERE \"SearchPhrase\" <> '' GROUP BY \"SearchPhrase\" ORDER BY c DESC LIMIT 10",
    ),
];

/// One row of the table.
struct Row {
    name: &'static str,
    bytes: usize,
    tokens: usize,
    steps: u64,
    tokenize: Number,
    matching: Number,
    transform: Number,
    total: Number,
}

/// Produce the table.
///
/// A debug build of this would be measuring the borrow checker's leftovers and not the parser, and
/// a number from one would be wrong by a factor that changes with every edit, so it re-runs itself
/// under the `bench` profile and measures there.
pub(crate) fn run(root: &Path) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return rebuild(root, "bench", &[]);
    }
    report();
    Ok(())
}

/// Measure every case and print the table.
fn report() {
    let rows: Vec<Row> = WORKLOAD.iter().map(|&(name, sql)| measure(name, sql)).collect();

    println!("rudb front end, text to AST, on a frozen workload of {} statements", rows.len());
    println!("{}", build_line());
    println!();
    println!(
        "{:<20}  {:>5}  {:>6}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>5}",
        "case", "bytes", "tokens", "steps", "tokenize", "match", "transform", "total", "IQR"
    );
    for row in &rows {
        println!(
            "{:<20}  {:>5}  {:>6}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}  {:>4.1}%",
            row.name,
            row.bytes,
            row.tokens,
            row.steps,
            show(row.tokenize.median),
            show(row.matching.median),
            show(row.transform.median),
            show(row.total.median),
            row.total.relative() * 100.0
        );
    }

    let bytes: usize = rows.iter().map(|r| r.bytes).sum();
    let tokens: usize = rows.iter().map(|r| r.tokens).sum();
    let steps: u64 = rows.iter().map(|r| r.steps).sum();
    let total: f64 = rows.iter().map(|r| r.total.median).sum();
    println!();
    println!(
        "{:<20}  {:>5}  {:>6}  {:>7}  {:>9}  {:>9}  {:>9}  {:>9}",
        "whole workload",
        bytes,
        tokens,
        steps,
        show(rows.iter().map(|r| r.tokenize.median).sum()),
        show(rows.iter().map(|r| r.matching.median).sum()),
        show(rows.iter().map(|r| r.transform.median).sum()),
        show(total)
    );
    let megabytes_per_second = bytes as f64 / total * 1_000.0;
    let per_token = total / tokens as f64;
    println!();
    println!(
        "{megabytes_per_second:.1} MB/s of SQL text over the whole workload, {per_token:.0}ns per token"
    );

    println!();
    for line in &caveats() {
        println!("{line}");
    }
}

/// The things that have to be read with the table and not after it.
///
/// Written as a list that is printed every time rather than as a paragraph in a document, for the
/// same reason `rudb-bench` prints the reasons a result may not be published under every result:
/// a caveat that lives somewhere else is a caveat nobody reads.
fn caveats() -> Vec<String> {
    let mut lines = vec!["Read this with the following, and not on its own:".to_string()];
    lines.extend(shared_caveats());
    lines.extend([
        "  rule ten: a micro number never appears without the end-to-end number it explains,"
            .to_string(),
        "    and nothing here times a query. So the total column here is the front end and"
            .to_string(),
        "    not a query time, and a win in it is worth nothing until there is a query time"
            .to_string(),
        "    underneath it.".to_string(),
        "  the three stage columns are measured separately and do not have to add up to the"
            .to_string(),
        "    total, which is measured too. The gap is what the measurement itself costs."
            .to_string(),
        "  peak resident memory is not here. A parse builds an arena and frees it, and the"
            .to_string(),
        "    steps and tokens columns describe that better than a high water mark does."
            .to_string(),
        "  whole queries against whole engines are tamnd/rudb-bench, and that is where any"
            .to_string(),
        "    number anybody quotes comes from.".to_string(),
    ]);
    lines
}

/// Measure one case.
///
/// The three stages are timed on inputs that were produced outside the timed region, so the match
/// column is the matcher and not the matcher plus a tokenizer, and the transform column is the
/// transformer and not the whole front end. The total is timed on the text, which is the thing a
/// caller actually has.
fn measure(name: &'static str, sql: &'static str) -> Row {
    let tokens = tokenize(sql).unwrap_or_else(|e| panic!("{name} does not tokenize: {e}"));
    let tree = parse_tokens(sql, &tokens, PROGRAM, true)
        .unwrap_or_else(|e| panic!("{name} does not parse: {e}"));
    transform(sql, &tokens, &tree).unwrap_or_else(|e| panic!("{name} does not transform: {e}"));

    Row {
        name,
        bytes: sql.len(),
        tokens: tokens.len(),
        steps: tree.steps(),
        tokenize: time(|| {
            drop(black_box(tokenize(black_box(sql))));
        }),
        matching: time(|| {
            drop(black_box(matched(sql, &tokens)));
        }),
        transform: time(|| {
            drop(black_box(transform(sql, &tokens, &tree)));
        }),
        total: time(|| {
            drop(black_box(parse_ast(black_box(sql))));
        }),
    }
}

/// The match stage on its own, named so the closure above stays one line.
fn matched(sql: &str, tokens: &[Token]) -> Tree {
    parse_tokens(black_box(sql), black_box(tokens), PROGRAM, true).expect("parses")
}

#[cfg(test)]
mod tests {
    use super::{WORKLOAD, caveats};

    #[test]
    fn every_case_in_the_workload_reaches_an_ast() {
        // Not a parse check. The transformer answers every rule, and one of its answers is a Not
        // implemented error, which is a fast path that does none of the work the table claims to be
        // timing. A workload case that stops at the parse tree would put a small number in the
        // transform column and look like the transformer was quick.
        let failed: Vec<String> = WORKLOAD
            .iter()
            .filter_map(|&(name, sql)| {
                rudb_parse::parse_ast(sql).err().map(|e| format!("{name}: {e}"))
            })
            .collect();
        assert!(failed.is_empty(), "the benchmark workload does not reach an AST:\n{failed:#?}");
    }

    #[test]
    fn the_workload_is_not_the_test_corpus() {
        // Not a style point. A workload that grows with coverage cannot be compared against itself
        // from last month, because a total that went up because a test was added is indis-
        // tinguishable from a total that went up because the parser got slower.
        assert!(WORKLOAD.len() < 20, "the workload has started collecting cases");
    }

    #[test]
    fn the_case_names_fit_the_column() {
        for &(name, _) in WORKLOAD {
            assert!(name.len() <= 20, "{name} overflows the case column and breaks the table");
        }
    }

    #[test]
    fn rule_ten_is_printed_every_time_and_not_remembered() {
        let text = caveats().join("\n");
        assert!(text.contains("rule ten"), "the table can be read as a result without this");
        assert!(text.contains("rule seven"));
        assert!(text.contains("rule two"));
    }
}
