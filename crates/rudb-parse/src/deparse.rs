//! An [`Ast`] written back out as SQL, the way DuckDB writes one.
//!
//! `duckdb_views().sql` is a deparse of the body rather than the text somebody typed, which was
//! measured: a view created with odd spacing, lower case type names and a comment in the middle
//! comes back normalised and without the comment. So the column needs a writer, and the writer has
//! to agree with the pin character for character or the column is a divergence on every view a
//! harness looks at.
//!
//! # This is not a pretty printer and it is not the printer in `transform`'s tests
//!
//! Two things are going on in the pin's output and only one of them is printing. `count(*)` comes
//! back as `count_star()`, `x IS TRUE` as `(CAST(x AS BOOLEAN) IS NOT DISTINCT FROM true)`,
//! `s LIKE 'a'` as `(s ~~ 'a')`, `[1, 2]` as `list_value(1, 2)`, `x IN (SELECT ...)` as
//! `(x = ANY(SELECT ...))` and a simple `CASE x WHEN 1` as a searched `CASE` with an `ELSE NULL`
//! nobody wrote. Those are rewrites DuckDB's transformer does on the way in, and what gets printed
//! is the rewritten tree. rudb's AST keeps the written form, deliberately, because an error message
//! should say what was written. So the rewrites happen here, at the point of printing, and every one
//! of them is a line in this file with the measurement it came from next to it.
//!
//! `transform`'s tests have a printer of their own and it stays. It answers a different question:
//! what shape did the transform produce. Printing `IS TRUE` as a cast and a distinct test would hide
//! exactly the bug those tests are there to catch.
//!
//! # The parentheses
//!
//! Every binary operation is parenthesised, whatever the precedence, so `x + y * 2 - 1` is
//! `((x + (y * 2)) - 1)`. Every unary one parenthesises its operand instead, so `-x` is `-(x)`. That
//! is upstream's rule and it is also the only rule that is safe without a precedence table, since a
//! printer that leaves parentheses out has to be right about precedence in both directions.
//!
//! A few of the quirks that follow from printing this way are upstream's rather than anybody's
//! design, and they are reproduced because the column is a comparison. `CASE` is followed by two
//! spaces, because the slot for the operand of a simple `CASE` is filled in unconditionally and a
//! searched one leaves it empty. A `FROM` list has a space before each comma. A chain of three set
//! operations loses the space before the second operator.
//!
//! # What does not agree yet
//!
//! Three things, and none of them is a printing question. Each one is a place where rudb's transform
//! threw away something the pin kept, so the answer is in `transform` and not here, and each has an
//! issue of its own. Two hundred and sixty two view bodies were measured against the pin and these
//! four lines are what is left over.
//!
//! `^` and `**` are one [`BinaryOp::Power`] here and two operators there, and the pin keeps whichever
//! was written all the way down to the function it resolves: `[1] ^ [2]` and `[1] ** [2]` fail with
//! different names in the message. A view written with one comes back with the other.
//!
//! `LIMIT ALL` is dropped by the transform, since it means no limit, and the pin writes it back as
//! `LIMIT NULL`.
//!
//! A subscript is rewritten by the transform into the call it stands for, so `[1, 2][1]` is
//! `array_extract(list_value(1, 2), 1)` here and `list_value(1, 2)[1]` there. The pin does the same
//! rewrite at bind time and prints the subscript, so this one is a matter of doing it later.
//!
//! # What the parser cannot reach yet
//!
//! A body the parser refuses never gets here, so none of the following is a divergence today. They
//! were measured anyway, at the same time as the rest, because the measurement is the expensive part
//! and whoever adds the syntax will need the answer. A window is printed with its clause spelled out
//! and a named one is inlined, so `OVER w` with `WINDOW w AS (ORDER BY x)` is `OVER (ORDER BY x)`. A
//! `FILTER` keeps its own parentheses and parenthesises the condition inside them. `EXISTS` is
//! written without a space before the parenthesis. `x IN (SELECT ...)` is `(x = ANY(SELECT ...))` and
//! `x > ALL (SELECT ...)` is `(NOT (x <= ANY(SELECT ...)))`. A `WITH` loses the space after the last
//! bracket, so it reads `WITH a AS (SELECT 1 AS n)SELECT n FROM a`, and a recursive one writes the
//! column list as ` (n)` with a space. `LATERAL` goes. `TABLESAMPLE 10 PERCENT` is `TABLESAMPLE
//! System(10.0 PERCENT)`. `CUBE (x, y)` and `ROLLUP (x, y)` are both written out as the
//! `GROUPING SETS` they stand for. `{'a': 1}` is `struct_pack(a := 1)` and `MAP {'a': 1}` is
//! `"map"(list_value('a'), list_value(1))`. A list comprehension is expanded into the three nested
//! lambdas it is made of.

use crate::ast::{
    Ast, BinaryOp, CaseArm, CreateViewRef, Distinct, Expr, ExprRef, JoinKind, LiteralKind, Nulls,
    Order, OrderItem, Quantifier, QueryBody, QueryRef, SelectRef, SetOp, Slice, Source, SourceRef,
    StrRef, Target, UnaryOp,
};
use crate::matcher::NONE;
use crate::tokenize::quoted;

/// A `CREATE VIEW` written back out, which is what `duckdb_views()` reports as `sql`.
///
/// The name loses its qualification, which was measured: `CREATE VIEW main.v AS ...` comes back as
/// `CREATE VIEW v AS ...`. So does `OR REPLACE` and so does `IF NOT EXISTS`, since what the column
/// answers is what this view is and not what the statement that made it asked for.
#[must_use]
pub fn create_view(ast: &Ast, index: CreateViewRef) -> String {
    let written = ast.create_view(index);
    let name = ast.name(written.name).last().unwrap_or_default();
    let temporary = if written.temporary { "TEMP " } else { "" };
    let mut out = format!("CREATE {temporary}VIEW {}", quoted(name));
    if !written.columns.is_empty() {
        // A space before the parenthesis, where `CREATE TABLE t(x INTEGER)` has none. Both were
        // measured and they really do differ.
        out += &format!(" ({})", names(ast, written.columns));
    }
    out + &format!(" AS {};", query(ast, written.query))
}

/// One query written back out.
#[must_use]
pub fn query(ast: &Ast, index: QueryRef) -> String {
    let held = ast.query(index);
    let mut out = match held.body {
        QueryBody::Select(select) => selection(ast, select),
        QueryBody::SetOp { op, quantifier, by_name, left, right } => {
            setop(ast, op, quantifier, by_name, left, right)
        }
        // A `VALUES` on its own becomes a select over it, named the way upstream names it. The name
        // is not a choice here: `CREATE VIEW v AS VALUES (1)` comes back with `AS valueslist` on it.
        QueryBody::Values(rows) => format!("SELECT * FROM ({}) AS valueslist", values(ast, rows)),
        QueryBody::Describe(inner) => format!("DESCRIBE ({})", query(ast, inner)),
        QueryBody::Show { name, .. } => format!("SHOW {}", ast.name_text(name)),
    };
    if held.order_by_all {
        // `ORDER BY ALL` is a star over the columns by the time it is printed.
        out += " ORDER BY COLUMNS(*)";
    } else if !held.order_by.is_empty() {
        let items: Vec<String> =
            ast.order_list(held.order_by).iter().map(|item| order(ast, item)).collect();
        out += &format!(" ORDER BY {}", items.join(", "));
    }
    if held.limit != NONE {
        // `LIMIT 10 PERCENT` comes back as `LIMIT (10) %`, which is upstream writing the percent
        // sign into the slot an operator goes in and getting the spacing wrong. Reproduced.
        if held.limit_percent {
            out += &format!(" LIMIT ({}) %", expr(ast, held.limit));
        } else {
            out += &format!(" LIMIT {}", expr(ast, held.limit));
        }
    }
    if held.offset != NONE {
        out += &format!(" OFFSET {}", expr(ast, held.offset));
    }
    out
}

/// A set operation, with the spacing bug upstream has in it.
///
/// Each side is wrapped in parentheses unless it is itself a set operation, in which case it is
/// written bare. The bare case also loses the space that would follow it, which is why
/// `a UNION b UNION c` comes back as `(a) UNION (b)UNION (c)` and not with a space there. That is
/// the pin's output and it is a comparison, so it is what this writes.
fn setop(
    ast: &Ast,
    op: SetOp,
    quantifier: Quantifier,
    by_name: bool,
    left: QueryRef,
    right: QueryRef,
) -> String {
    let word = match op {
        SetOp::Union => "UNION",
        SetOp::Except => "EXCEPT",
        SetOp::Intersect => "INTERSECT",
    };
    // `UNION DISTINCT` comes back as `UNION`, since distinct is what the operator does anyway.
    let all = if matches!(quantifier, Quantifier::All) { " ALL" } else { "" };
    let named = if by_name { " BY NAME" } else { "" };
    format!("{}{word}{all}{named} {}", branch(ast, left, true), branch(ast, right, false))
}

/// One side of a set operation, parenthesised unless it is a set operation itself.
fn branch(ast: &Ast, index: QueryRef, left: bool) -> String {
    let text = query(ast, index);
    if matches!(ast.query(index).body, QueryBody::SetOp { .. }) {
        return text;
    }
    if left { format!("({text}) ") } else { format!("({text})") }
}

/// One select block, without the modifiers that hang off the query around it.
fn selection(ast: &Ast, index: SelectRef) -> String {
    let held = ast.select(index);
    let mut out = "SELECT".to_string();
    match held.distinct {
        Distinct::No => {}
        Distinct::Yes => out += " DISTINCT",
        Distinct::On(list) => out += &format!(" DISTINCT ON ({})", exprs(ast, list)),
    }
    let targets: Vec<String> =
        ast.target_list(held.targets).iter().map(|target| aliased(ast, target)).collect();
    out += &format!(" {}", targets.join(", "));
    if !held.from.is_empty() {
        // A space before the comma, which is upstream's and was measured on `FROM t t1, t t2`.
        let sources: Vec<String> =
            ast.source_list(held.from).iter().map(|&index| source(ast, index)).collect();
        out += &format!(" FROM {}", sources.join(" , "));
    }
    if held.filter != NONE {
        out += &format!(" WHERE {}", expr(ast, held.filter));
    }
    if held.group_by_all {
        out += " GROUP BY ALL";
    } else if !held.group_by.is_empty() {
        out += &format!(" GROUP BY {}", exprs(ast, held.group_by));
    }
    if held.having != NONE {
        out += &format!(" HAVING {}", expr(ast, held.having));
    }
    out
}

/// One entry of a target list, with its alias if it was given one.
fn aliased(ast: &Ast, target: &Target) -> String {
    let written = expr(ast, target.expr);
    if target.alias == NONE {
        return written;
    }
    format!("{written} AS {}", quoted(ast.string(target.alias)))
}

/// One entry of an order by list.
fn order(ast: &Ast, item: &OrderItem) -> String {
    let mut out = expr(ast, item.expr);
    match item.order {
        Order::Unstated => {}
        Order::Ascending => out += " ASC",
        Order::Descending => out += " DESC",
    }
    match item.nulls {
        Nulls::Unstated => {}
        Nulls::First => out += " NULLS FIRST",
        Nulls::Last => out += " NULLS LAST",
    }
    out
}

/// One entry of a `FROM` clause.
fn source(ast: &Ast, index: SourceRef) -> String {
    match ast.source(index) {
        Source::Table { name, alias, columns } => label(ast, parts(ast, name), alias, columns),
        Source::Subquery { query: inner, alias, columns } => {
            label(ast, format!("({})", query(ast, inner)), alias, columns)
        }
        Source::Function { name, args, alias, columns, .. } => {
            let written: Vec<String> =
                ast.target_list(args).iter().map(|arg| argument(ast, arg)).collect();
            let call = format!("{}({})", parts(ast, name), written.join(", "));
            label(ast, call, alias, columns)
        }
        // A `VALUES` in a `FROM` clause is wrapped in a select of its own, named `valueslist`, and
        // then given whatever alias was written. Measured, including the name.
        Source::Values { rows, alias, columns } => {
            let inner = format!("(SELECT * FROM ({}) AS valueslist)", values(ast, rows));
            label(ast, inner, alias, columns)
        }
        Source::Join { left, right, kind, natural, on, using } => {
            let word = match kind {
                JoinKind::Inner => "INNER",
                JoinKind::Left => "LEFT",
                JoinKind::Right => "RIGHT",
                // `FULL OUTER JOIN` loses the `OUTER`, and a `NATURAL JOIN` gains an `INNER`.
                JoinKind::Full => "FULL",
                JoinKind::Semi => "SEMI",
                JoinKind::Anti => "ANTI",
                JoinKind::Cross => "CROSS",
                JoinKind::Positional => "POSITIONAL",
            };
            let natural = if natural { "NATURAL " } else { "" };
            let mut out =
                format!("({} {natural}{word} JOIN {}", source(ast, left), source(ast, right));
            if on != NONE {
                // A second pair of parentheses around a condition that has its own, so an equality
                // comes out as `ON ((a.x = b.y))`.
                out += &format!(" ON ({})", expr(ast, on));
            }
            if !using.is_empty() {
                out += &format!(" USING ({})", names(ast, using));
            }
            out + ")"
        }
    }
}

/// One argument of a table function, which is an expression or a name and an expression.
///
/// A named one is parenthesised and written with `=`, so `read_csv(f, header = true)` comes back as
/// `read_csv(f, ("header" = true))`. The name goes through the quoting rule like any other
/// identifier, which is why `header` gains quotes there.
fn argument(ast: &Ast, arg: &Target) -> String {
    if arg.alias == NONE {
        return expr(ast, arg.expr);
    }
    format!("({} = {})", quoted(ast.string(arg.alias)), expr(ast, arg.expr))
}

/// A from item with its alias and column list, if it was given either.
fn label(ast: &Ast, written: String, alias: StrRef, columns: Slice) -> String {
    let mut out = written;
    if alias != NONE {
        out += &format!(" AS {}", quoted(ast.string(alias)));
    }
    if !columns.is_empty() {
        out += &format!("({})", names(ast, columns));
    }
    out
}

/// The rows of a `VALUES`, with the keyword in front of them.
fn values(ast: &Ast, rows: Slice) -> String {
    let written: Vec<String> =
        ast.rows(rows).iter().map(|&row| format!("({})", exprs(ast, row))).collect();
    format!("VALUES {}", written.join(", "))
}

/// One expression.
fn expr(ast: &Ast, index: ExprRef) -> String {
    match ast.expr(index) {
        Expr::Star { qualifier, replacements } => star(ast, qualifier, replacements),
        Expr::Column { name } => parts(ast, name),
        Expr::Literal { kind, text } => literal(ast, kind, text),
        Expr::Unary { op, operand } => unary(ast, op, operand),
        Expr::Binary { op, left, right } => binary(ast, op, left, right),
        Expr::Function { name, args, distinct } => call(ast, name, args, distinct),
        Expr::Cast { operand, ty, try_cast } => {
            let word = if try_cast { "TRY_CAST" } else { "CAST" };
            format!("{word}({} AS {})", expr(ast, operand), typename(ast.string(ty)))
        }
        Expr::Case { operand, arms, otherwise } => case(ast, operand, arms, otherwise),
        Expr::Between { operand, low, high, negated } => {
            let written = format!(
                "({} BETWEEN {} AND {})",
                expr(ast, operand),
                expr(ast, low),
                expr(ast, high)
            );
            if negated { format!("(NOT {written})") } else { written }
        }
        Expr::In { operand, list, negated } => {
            let written = format!("({} IN ({}))", expr(ast, operand), exprs(ast, list));
            if negated { format!("(NOT {written})") } else { written }
        }
        Expr::InSubquery { operand, query: inner, negated } => {
            let any = format!("({} = ANY({}))", expr(ast, operand), query(ast, inner));
            if negated { format!("(NOT {any})") } else { any }
        }
        Expr::Parameter { name } => format!("${}", ast.string(name)),
        // A bracketed list is a call to `list_value`, including when it is empty.
        Expr::List { items } => format!("list_value({})", exprs(ast, items)),
        // And a parenthesised list is a call to `row`, which needs its quotes because it is a
        // keyword.
        Expr::Row { items } => format!("\"row\"({})", exprs(ast, items)),
        Expr::Subquery { query: inner } => format!("({})", query(ast, inner)),
        Expr::Exists { query: inner, negated } => {
            let exists = format!("EXISTS({})", query(ast, inner));
            if negated { format!("(NOT {exists})") } else { exists }
        }
    }
}

/// A star, with the qualifier and the replace list it may have been written with.
fn star(ast: &Ast, qualifier: Slice, replacements: Slice) -> String {
    let mut out =
        if qualifier.is_empty() { "*".to_string() } else { format!("{}.*", parts(ast, qualifier)) };
    if !replacements.is_empty() {
        let written: Vec<String> =
            ast.target_list(replacements).iter().map(|target| aliased(ast, target)).collect();
        out += &format!(" REPLACE ({})", written.join(", "));
    }
    out
}

/// One literal.
fn literal(ast: &Ast, kind: LiteralKind, text: StrRef) -> String {
    match kind {
        // `true` and `false` in lower case, which is the pin's spelling whichever way they were
        // written.
        LiteralKind::Null => "NULL".to_string(),
        LiteralKind::True => "true".to_string(),
        LiteralKind::False => "false".to_string(),
        LiteralKind::Number => number(ast.string(text)),
        LiteralKind::String => string(ast.string(text)),
        // A blob prints as a string of its escaped form cast to `BLOB`, so `X'ab'` comes back as
        // `'\xAB'::BLOB`.
        LiteralKind::Blob => format!("{}::BLOB", string(ast.string(text))),
    }
}

/// A numeric literal, written back as the value it was read as rather than as the text.
///
/// The value is what upstream prints, so the shape of the literal decides the shape of the answer.
/// A literal with an exponent in it is a DOUBLE and comes back in whatever form a double prints in.
/// One with a point in it is a DECIMAL of the width and scale that were written, so the digits after
/// the point survive exactly, trailing zeros and all, and only the digits in front of it are tidied.
/// One with neither is an integer. The underscores a long number can be written with are a way of
/// writing it and not part of it, so `1_000` is `1000` in all three.
fn number(written: &str) -> String {
    let text = written.replace('_', "");
    if text.contains(['e', 'E']) {
        return double(&text);
    }
    let Some((whole, fraction)) = text.split_once('.') else {
        return leading(&text).to_string();
    };
    // `1.` is a decimal of scale zero, which prints without the point, and `.5` keeps the empty
    // side it was written with rather than growing a zero. Both were measured.
    if fraction.is_empty() {
        return leading(whole).to_string();
    }
    format!("{}.{fraction}", if whole.is_empty() { "" } else { leading(whole) })
}

/// A run of digits with the zeros in front of it dropped, down to one digit.
fn leading(digits: &str) -> &str {
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() { &digits[digits.len().saturating_sub(1)..] } else { trimmed }
}

/// A double, in the form the formatting library upstream uses prints one in.
///
/// Plain digits while the decimal exponent is between minus four and fifteen, and the exponent form
/// outside that, with at least two digits of exponent and a sign that is written even when it is a
/// plus. So `1e3` is `1000.0`, `1e15` is `1000000000000000.0`, `1e16` is `1e+16`, `5e-4` is `0.0005`
/// and `5e-5` is `5e-05`. A plain one always has a point in it, which is what tells a double from an
/// integer when it is read back.
fn double(text: &str) -> String {
    let Ok(value) = text.parse::<f64>() else {
        return text.to_string();
    };
    // The shortest digits that read back as this value, which is what `{:e}` is, and the exponent
    // that goes with them. Rust writes that form as `2.5e-5`, so the exponent is the tail.
    let shortest = format!("{value:e}");
    let (mantissa, exponent) = shortest.split_once('e').unwrap_or((shortest.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if (-4..=15).contains(&exponent) {
        let plain = format!("{value}");
        return if plain.contains('.') { plain } else { plain + ".0" };
    }
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

/// A string literal, with the one character that has to be escaped escaped.
///
/// Only the quote. A newline written as `e'\n'` comes back as a real newline inside the quotes,
/// which was measured, so everything else goes out as the byte it is.
fn string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A prefix or postfix operator.
fn unary(ast: &Ast, op: UnaryOp, operand: ExprRef) -> String {
    // `-1` is a number and not a negation of one, so a minus in front of a numeric constant folds
    // into it and `- -3` folds twice and comes back as `3`. A plus does not fold, which is why
    // `+3` comes back as `+(3)`.
    if matches!(op, UnaryOp::Negate) {
        if let Some(number) = negated(ast, operand) {
            return number;
        }
    }
    let written = expr(ast, operand);
    match op {
        UnaryOp::Not => format!("(NOT {written})"),
        UnaryOp::Negate => format!("-({written})"),
        UnaryOp::Plus => format!("+({written})"),
        UnaryOp::BitNot => format!("~({written})"),
        // `x!` is a call to `factorial` by the time it is printed.
        UnaryOp::Factorial => format!("factorial({written})"),
        UnaryOp::IsNull => format!("({written} IS NULL)"),
        UnaryOp::IsNotNull => format!("({written} IS NOT NULL)"),
        // `IS UNKNOWN` is `IS NULL` and nothing else, so it prints as the thing it means.
        UnaryOp::IsUnknown => format!("({written} IS NULL)"),
        UnaryOp::IsNotUnknown => format!("({written} IS NOT NULL)"),
        // And the four tests against a boolean are a cast and a distinct test, which is what they
        // are defined to be: `x IS TRUE` is false rather than null for a null `x`, and a plain
        // `x = true` would not be.
        UnaryOp::IsTrue => distinct(&written, "true", true),
        UnaryOp::IsNotTrue => distinct(&written, "true", false),
        UnaryOp::IsFalse => distinct(&written, "false", true),
        UnaryOp::IsNotFalse => distinct(&written, "false", false),
    }
}

/// What `IS TRUE` and its three relatives are written as.
fn distinct(operand: &str, against: &str, same: bool) -> String {
    let word = if same { "IS NOT DISTINCT FROM" } else { "IS DISTINCT FROM" };
    format!("(CAST({operand} AS BOOLEAN) {word} {against})")
}

/// The text of a numeric constant with a minus applied to it, and `None` for anything else.
///
/// Recursive, because the fold happens on the way in and applies again to what it produced. A minus
/// in front of a minus in front of `3` is the constant `3`.
fn negated(ast: &Ast, index: ExprRef) -> Option<String> {
    match ast.expr(index) {
        Expr::Literal { kind: LiteralKind::Number, text } => {
            Some(format!("-{}", number(ast.string(text))))
        }
        Expr::Unary { op: UnaryOp::Negate, operand } => {
            let inner = negated(ast, operand)?;
            Some(inner.strip_prefix('-').unwrap_or(&inner).to_string())
        }
        _ => None,
    }
}

/// An infix operator, parenthesised.
fn binary(ast: &Ast, op: BinaryOp, left: ExprRef, right: ExprRef) -> String {
    let (left, right) = (expr(ast, left), expr(ast, right));
    // The three that are not written as an operator at all.
    match op {
        BinaryOp::SimilarTo => return format!("regexp_full_match({left}, {right})"),
        BinaryOp::NotSimilarTo => return format!("(NOT regexp_full_match({left}, {right}))"),
        // The arguments swap, so `ts AT TIME ZONE 'UTC'` is `timezone('UTC', ts)`.
        BinaryOp::AtTimeZone => return format!("timezone({right}, {left})"),
        // And the one that is written as an operator and is not parenthesised.
        BinaryOp::Collate => return format!("{left} COLLATE {right}"),
        _ => {}
    }
    let word = match op {
        BinaryOp::Or => "OR",
        BinaryOp::And => "AND",
        BinaryOp::Eq => "=",
        BinaryOp::NotEq => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Gt => ">",
        BinaryOp::LtEq => "<=",
        BinaryOp::GtEq => ">=",
        BinaryOp::IsDistinctFrom => "IS DISTINCT FROM",
        BinaryOp::IsNotDistinctFrom => "IS NOT DISTINCT FROM",
        BinaryOp::Add => "+",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::IntegerDivide => "//",
        BinaryOp::Modulo => "%",
        // Whichever of `^` and `**` was written is what the pin prints, and both arrive here as one
        // operator, so one spelling has to stand for both. See the module doc.
        BinaryOp::Power => "**",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::ShiftLeft => "<<",
        BinaryOp::ShiftRight => ">>",
        BinaryOp::Concat => "||",
        // The four pattern operators have a word spelling and a symbol spelling, and the symbol is
        // what comes back whichever was written.
        BinaryOp::Like => "~~",
        BinaryOp::NotLike => "!~~",
        BinaryOp::ILike => "~~*",
        BinaryOp::NotILike => "!~~*",
        BinaryOp::Glob => "~~~",
        BinaryOp::Regex => "~",
        BinaryOp::NotRegex => "!~",
        BinaryOp::RegexInsensitive => "~*",
        BinaryOp::NotRegexInsensitive => "!~*",
        BinaryOp::Arrow => "->",
        BinaryOp::LongArrow => "->>",
        BinaryOp::Contains => "@>",
        BinaryOp::ContainedBy => "<@",
        BinaryOp::Overlaps => "&&",
        BinaryOp::StartsWith => "^@",
        BinaryOp::InetContainedByOrEq => "<<=",
        BinaryOp::InetContainsOrEq => ">>=",
        BinaryOp::Named(name) => ast.string(name),
        BinaryOp::SimilarTo | BinaryOp::NotSimilarTo | BinaryOp::AtTimeZone | BinaryOp::Collate => {
            unreachable!("the four that return above")
        }
    };
    format!("({left} {word} {right})")
}

/// A function call.
fn call(ast: &Ast, name: Slice, args: Slice, distinct: bool) -> String {
    let written = parts(ast, name);
    let list = ast.expr_list(args);
    // `count(*)` is a different function from `count`, and the star is how it is spelled rather than
    // an argument it takes, so it prints under the name it really has.
    if list.len() == 1
        && matches!(ast.expr(list[0]), Expr::Star { qualifier, replacements }
            if qualifier.is_empty() && replacements.is_empty())
        && written.eq_ignore_ascii_case("count")
    {
        return "count_star()".to_string();
    }
    let word = if distinct { "DISTINCT " } else { "" };
    format!("{}({word}{})", operator(ast, name, &written), exprs(ast, args))
}

/// The name a call is written back under, which is the name it was written with for all but two.
///
/// `coalesce` and `ifnull` are grammar rules rather than function names, so they come back as the
/// one thing the rule stands for, upper case and unquoted. That holds for the one argument form as
/// well: `coalesce(x)` is `COALESCE(x)` and not `x`. No other name does this, which was measured,
/// and `nullif` is the one to check against because it looks like it should and does not.
fn operator(ast: &Ast, name: Slice, written: &str) -> String {
    let one = ast.name(name).next().unwrap_or_default();
    let alone = ast.name(name).count() == 1;
    if alone && (one.eq_ignore_ascii_case("coalesce") || one.eq_ignore_ascii_case("ifnull")) {
        return "COALESCE".to_string();
    }
    written.to_string()
}

/// A `CASE`, always searched and always with an `ELSE`.
///
/// A simple `CASE x WHEN 1 THEN 'a'` is rewritten into `CASE WHEN x = 1 THEN 'a' ELSE NULL END` on
/// the way in, so both forms print the same way. The two spaces after `CASE` are upstream leaving
/// the operand slot empty and writing the space around it anyway.
fn case(ast: &Ast, operand: ExprRef, arms: Slice, otherwise: ExprRef) -> String {
    let mut out = "CASE ".to_string();
    for arm in ast.arm_list(arms) {
        let when = when(ast, operand, arm);
        out += &format!(" WHEN ({when}) THEN ({})", expr(ast, arm.then));
    }
    let last = if otherwise == NONE { "NULL".to_string() } else { expr(ast, otherwise) };
    out + &format!(" ELSE {last} END")
}

/// The condition of one arm, which is the arm's own for a searched `CASE` and an equality for a
/// simple one.
fn when(ast: &Ast, operand: ExprRef, arm: &CaseArm) -> String {
    if operand == NONE {
        return expr(ast, arm.when);
    }
    format!("({} = {})", expr(ast, operand), expr(ast, arm.when))
}

/// A type as upstream writes one, which is two rules and not one.
///
/// A name the SQL standard spells is resolved and written back under the one name its type has, so
/// `int` is `INTEGER`, `numeric(5)` is `DECIMAL(5)`, `character varying` is `VARCHAR` and `real` is
/// `FLOAT`. Every other name is written back exactly as somebody typed it, case and all and without
/// quotes, so `text` stays `text`, `TEXT` stays `TEXT` and `int4` stays `int4`. All of that was
/// measured a name at a time, and the split is not arbitrary: the standard names are the ones the
/// grammar has rules for, and everything else is a name the parser hands to the catalog to look up
/// later, so the text is all it has.
///
/// This walks the text rather than going through the type system, because the type system throws
/// away what has to survive here. `DECIMAL(5)` and `DECIMAL` both become a width and a scale, and
/// `VARCHAR(10)` becomes `VARCHAR`, but upstream prints back the length that was written.
fn typename(text: &str) -> String {
    let text = text.trim();
    // A trailing `[]` or `[3]` is a list or an array of whatever is in front of it, and the element
    // is resolved the same way: `int[]` is `INTEGER[]` while `int4[]` stays `int4[]`.
    if let Some(open) = suffix(text) {
        return typename(&text[..open]) + &text[open..];
    }
    let (base, arguments) = arguments(text);
    let Some(name) = standard(base) else {
        let base = unquote(base);
        return match arguments {
            Some(arguments) => format!("{}({arguments})", catalogued(&base)),
            None => catalogued(&base),
        };
    };
    match (name, arguments) {
        // `STRUCT(a bool)` and `UNION(a int)` are a name and a type each, and the name keeps the
        // case it was written in while the type goes round again.
        ("STRUCT" | "UNION", Some(inside)) => {
            let written: Vec<String> = pieces(inside).iter().map(|piece| field(piece)).collect();
            format!("{name}({})", written.join(", "))
        }
        ("MAP", Some(inside)) => {
            let written: Vec<String> = pieces(inside).iter().map(|piece| typename(piece)).collect();
            format!("{name}({})", written.join(", "))
        }
        // The width and the scale of a decimal and the length of a string survive, because upstream
        // prints the modifiers it was given rather than the ones the type ended up with.
        ("DECIMAL" | "VARCHAR", Some(inside)) => {
            format!("{name}({})", pieces(inside).join(", "))
        }
        // And everything else drops them, because they chose the type rather than sitting on it.
        // `float(10)` is a `FLOAT` and there is nothing left of the ten.
        _ => name.to_string(),
    }
}

/// A type name the grammar has no rule for, which is a name for the catalog to look up later.
///
/// Written back as it stands, with the case it was written in and with no quotes, because all the
/// parser has is the text. `bool`, `TEXT`, `int4`, `timestamptz` and `Mixed` all come back exactly
/// as they went in, which was measured a name at a time.
///
/// `json` is the one exception in the whole list and it comes back quoted. That is not a rule about
/// json, it is what happens to a name the parser resolves on its own rather than leaving for the
/// catalog: the type it lands on carries the written name as its label, and a label is written back
/// through the identifier rule, which quotes a keyword. `json` is the only name that is both a
/// keyword and one of those, so it is the only one where the difference shows. The case that was
/// written survives it, so `JSON` is `"JSON"` and `json` is `"json"`.
fn catalogued(base: &str) -> String {
    if base.eq_ignore_ascii_case("json") { quoted(base) } else { base.to_string() }
}

/// A name with its quotes taken off, if it had any.
fn unquote(base: &str) -> String {
    match base.strip_prefix('"').and_then(|rest| rest.strip_suffix('"')) {
        Some(inside) => inside.replace("\"\"", "\""),
        None => base.to_string(),
    }
}

/// Where the trailing `[]` or `[3]` of a list or an array type starts, if there is one.
fn suffix(text: &str) -> Option<usize> {
    let rest = text.strip_suffix(']')?;
    let open = rest.rfind('[')?;
    rest[open + 1..].bytes().all(|byte| byte.is_ascii_digit()).then_some(open)
}

/// A type split into the name and whatever was in the parentheses after it.
fn arguments(text: &str) -> (&str, Option<&str>) {
    let Some(rest) = text.strip_suffix(')') else {
        return (text, None);
    };
    let mut depth = 0usize;
    for (at, byte) in rest.bytes().enumerate() {
        match byte {
            b'(' if depth == 0 => depth = 1,
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => continue,
        }
        if depth == 1 && byte == b'(' {
            return (rest[..at].trim(), Some(rest[at + 1..].trim()));
        }
    }
    (text, None)
}

/// The entries of an argument list, split on the commas that are not inside anything.
fn pieces(inside: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let (mut depth, mut quoted, mut start) = (0usize, false, 0usize);
    for (at, byte) in inside.bytes().enumerate() {
        match byte {
            b'"' => quoted = !quoted,
            b'(' | b'[' if !quoted => depth += 1,
            b')' | b']' if !quoted => depth = depth.saturating_sub(1),
            b',' if !quoted && depth == 0 => {
                found.push(inside[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    found.push(inside[start..].trim());
    found
}

/// One field of a `STRUCT` or a `UNION`, which is a name and then a type.
fn field(piece: &str) -> String {
    let mut quoting = false;
    for (at, byte) in piece.bytes().enumerate() {
        match byte {
            b'"' => quoting = !quoting,
            byte if byte.is_ascii_whitespace() && !quoting => {
                let name = piece[..at].trim();
                let name =
                    if name.starts_with('"') { quoted(&unquote(name)) } else { name.to_string() };
                return format!("{name} {}", typename(&piece[at + 1..]));
            }
            _ => {}
        }
    }
    piece.to_string()
}

/// The one name a type the SQL standard spells is written back under, and nothing for any other.
///
/// Several words for the ones the standard writes with several. The list is short because it is the
/// standard's list and not DuckDB's: `HUGEINT`, `TEXT`, `BLOB` and the rest of the names DuckDB adds
/// are not in here, and they are the ones that come back exactly as they were written.
fn standard(base: &str) -> Option<&'static str> {
    const NAMES: &[(&str, &str)] = &[
        ("BOOLEAN", "BOOLEAN"),
        ("INT", "INTEGER"),
        ("INTEGER", "INTEGER"),
        ("SMALLINT", "SMALLINT"),
        ("BIGINT", "BIGINT"),
        ("DEC", "DECIMAL"),
        ("DECIMAL", "DECIMAL"),
        ("NUMERIC", "DECIMAL"),
        ("REAL", "FLOAT"),
        ("FLOAT", "FLOAT"),
        ("DOUBLE PRECISION", "DOUBLE"),
        ("CHAR", "VARCHAR"),
        ("CHARACTER", "VARCHAR"),
        ("CHARACTER VARYING", "VARCHAR"),
        ("NATIONAL CHARACTER", "VARCHAR"),
        ("NATIONAL CHARACTER VARYING", "VARCHAR"),
        ("VARCHAR", "VARCHAR"),
        ("BIT", "BIT"),
        ("DATE", "DATE"),
        ("TIME", "TIME"),
        ("TIME WITH TIME ZONE", "TIME WITH TIME ZONE"),
        ("TIME WITHOUT TIME ZONE", "TIME"),
        ("TIMESTAMP", "TIMESTAMP"),
        ("TIMESTAMP WITH TIME ZONE", "TIMESTAMP WITH TIME ZONE"),
        ("TIMESTAMP WITHOUT TIME ZONE", "TIMESTAMP"),
        ("INTERVAL", "INTERVAL"),
        ("STRUCT", "STRUCT"),
        ("UNION", "UNION"),
        ("MAP", "MAP"),
    ];
    let written: Vec<&str> = base.split_whitespace().collect();
    let written = written.join(" ");
    NAMES
        .iter()
        .find(|(spelling, _)| spelling.eq_ignore_ascii_case(&written))
        .map(|(_, name)| *name)
}

/// A run of expressions, comma separated.
fn exprs(ast: &Ast, list: Slice) -> String {
    let written: Vec<String> = ast.expr_list(list).iter().map(|&item| expr(ast, item)).collect();
    written.join(", ")
}

/// A run of identifiers, comma separated, each quoted if it has to be.
fn names(ast: &Ast, list: Slice) -> String {
    ast.name(list).map(quoted).collect::<Vec<_>>().join(", ")
}

/// A dotted name, each part quoted if it has to be.
fn parts(ast: &Ast, list: Slice) -> String {
    ast.name(list).map(quoted).collect::<Vec<_>>().join(".")
}

#[cfg(test)]
mod tests {
    use super::create_view;
    use crate::ast::Statement;
    use crate::transform::parse_ast;

    /// The whole statement, deparsed.
    fn whole(sql: &str) -> String {
        let ast = parse_ast(sql).unwrap_or_else(|error| panic!("{sql} should parse: {error}"));
        let Statement::CreateView(index) = ast.statements[0] else {
            panic!("that was not a create view");
        };
        create_view(&ast, index)
    }

    /// Just the body, which is what most of these are about.
    fn body(query: &str) -> String {
        let written = whole(&format!("CREATE VIEW v AS {query}"));
        written
            .strip_prefix("CREATE VIEW v AS ")
            .and_then(|rest| rest.strip_suffix(';'))
            .expect("the statement wrapper is there")
            .to_string()
    }

    #[test]
    fn a_statement_loses_its_qualification_and_its_or_replace() {
        assert_eq!(whole("CREATE VIEW main.v AS SELECT 1"), "CREATE VIEW v AS SELECT 1;");
        assert_eq!(whole("CREATE OR REPLACE VIEW v AS SELECT 1"), "CREATE VIEW v AS SELECT 1;");
        assert_eq!(whole("CREATE VIEW IF NOT EXISTS v AS SELECT 1"), "CREATE VIEW v AS SELECT 1;");
        assert_eq!(whole("CREATE TEMP VIEW v AS SELECT 1"), "CREATE TEMP VIEW v AS SELECT 1;");
    }

    /// A space before the parenthesis here, and none in a `CREATE TABLE`. Both measured.
    #[test]
    fn an_alias_list_is_written_with_a_space_in_front_of_it() {
        assert_eq!(
            whole(r#"CREATE VIEW v ("Weird Name", "x y") AS SELECT 1, 2"#),
            r#"CREATE VIEW v ("Weird Name", "x y") AS SELECT 1, 2;"#
        );
    }

    #[test]
    fn comments_and_spacing_go_and_the_case_of_a_name_stays() {
        assert_eq!(
            whole("CREATE VIEW v AS SELECT  X /* a note */ FROM   T"),
            "CREATE VIEW v AS SELECT X FROM T;"
        );
    }

    #[test]
    fn every_binary_operation_is_parenthesised_and_every_unary_one_parenthesises_its_operand() {
        assert_eq!(body("SELECT x + y * 2 - 1 FROM t"), "SELECT ((x + (y * 2)) - 1) FROM t");
        assert_eq!(
            body("SELECT x > 1 AND y < 2 OR b FROM t"),
            "SELECT (((x > 1) AND (y < 2)) OR b) FROM t"
        );
        assert_eq!(body("SELECT NOT b FROM t"), "SELECT (NOT b) FROM t");
        assert_eq!(body("SELECT ~x FROM t"), "SELECT ~(x) FROM t");
        assert_eq!(body("SELECT +x FROM t"), "SELECT +(x) FROM t");
        assert_eq!(body("SELECT -x FROM t"), "SELECT -(x) FROM t");
    }

    /// A minus in front of a number is part of the number, and it folds as many times as it is
    /// written. A plus is not part of one and does not fold.
    #[test]
    fn a_minus_in_front_of_a_constant_folds_into_it() {
        assert_eq!(body("SELECT -1"), "SELECT -1");
        assert_eq!(body("SELECT - -3"), "SELECT 3");
        assert_eq!(body("SELECT +3"), "SELECT +(3)");
    }

    #[test]
    fn the_null_tests_and_the_boolean_tests() {
        assert_eq!(body("SELECT x IS NULL FROM t"), "SELECT (x IS NULL) FROM t");
        assert_eq!(body("SELECT x ISNULL FROM t"), "SELECT (x IS NULL) FROM t");
        assert_eq!(body("SELECT x NOTNULL FROM t"), "SELECT (x IS NOT NULL) FROM t");
        assert_eq!(
            body("SELECT b IS TRUE FROM t"),
            "SELECT (CAST(b AS BOOLEAN) IS NOT DISTINCT FROM true) FROM t"
        );
        assert_eq!(
            body("SELECT b IS NOT TRUE FROM t"),
            "SELECT (CAST(b AS BOOLEAN) IS DISTINCT FROM true) FROM t"
        );
        assert_eq!(
            body("SELECT b IS FALSE FROM t"),
            "SELECT (CAST(b AS BOOLEAN) IS NOT DISTINCT FROM false) FROM t"
        );
        assert_eq!(body("SELECT b IS UNKNOWN FROM t"), "SELECT (b IS NULL) FROM t");
        assert_eq!(body("SELECT b IS NOT UNKNOWN FROM t"), "SELECT (b IS NOT NULL) FROM t");
        assert_eq!(
            body("SELECT x IS DISTINCT FROM y FROM t"),
            "SELECT (x IS DISTINCT FROM y) FROM t"
        );
    }

    #[test]
    fn a_negated_between_or_in_is_a_not_around_the_plain_one() {
        assert_eq!(body("SELECT x BETWEEN 1 AND 10 FROM t"), "SELECT (x BETWEEN 1 AND 10) FROM t");
        assert_eq!(
            body("SELECT x NOT BETWEEN 1 AND 2 FROM t"),
            "SELECT (NOT (x BETWEEN 1 AND 2)) FROM t"
        );
        assert_eq!(body("SELECT x IN (1, 2, 3) FROM t"), "SELECT (x IN (1, 2, 3)) FROM t");
        assert_eq!(body("SELECT x NOT IN (1, 2) FROM t"), "SELECT (NOT (x IN (1, 2))) FROM t");
        assert_eq!(body("SELECT x IN (SELECT y FROM t)"), "SELECT (x = ANY(SELECT y FROM t))");
        assert_eq!(
            body("SELECT x NOT IN (SELECT y FROM t)"),
            "SELECT (NOT (x = ANY(SELECT y FROM t)))"
        );
    }

    /// The four pattern operators have a word spelling and a symbol spelling, and the symbol is
    /// what comes back either way.
    #[test]
    fn the_pattern_operators_come_back_as_symbols() {
        assert_eq!(body("SELECT s LIKE 'a' FROM t"), "SELECT (s ~~ 'a') FROM t");
        assert_eq!(body("SELECT s NOT LIKE 'a' FROM t"), "SELECT (s !~~ 'a') FROM t");
        assert_eq!(body("SELECT s ILIKE 'a' FROM t"), "SELECT (s ~~* 'a') FROM t");
        assert_eq!(body("SELECT s NOT ILIKE 'a' FROM t"), "SELECT (s !~~* 'a') FROM t");
        assert_eq!(body("SELECT s GLOB 'a' FROM t"), "SELECT (s ~~~ 'a') FROM t");
        assert_eq!(body("SELECT s !~ 'a' FROM t"), "SELECT (s !~ 'a') FROM t");
        assert_eq!(
            body("SELECT s NOT SIMILAR TO 'a' FROM t"),
            "SELECT (NOT regexp_full_match(s, 'a')) FROM t"
        );
    }

    #[test]
    fn collate_has_no_parentheses_and_the_rest_of_the_operators_keep_their_spelling() {
        assert_eq!(body("SELECT s COLLATE NOCASE FROM t"), "SELECT s COLLATE NOCASE FROM t");
        assert_eq!(body("SELECT x // y FROM t"), "SELECT (x // y) FROM t");
        assert_eq!(body("SELECT x || y FROM t"), "SELECT (x || y) FROM t");
        assert_eq!(body("SELECT x @> y FROM t"), "SELECT (x @> y) FROM t");
        assert_eq!(body("SELECT x <=> y FROM t"), "SELECT (x <=> y) FROM t");
    }

    /// Always searched, always with an `ELSE`, and two spaces after the keyword.
    #[test]
    fn a_case_is_written_the_long_way_round() {
        assert_eq!(
            body("SELECT CASE WHEN x > 0 THEN 'a' WHEN x < 0 THEN 'b' ELSE 'c' END FROM t"),
            "SELECT CASE  WHEN ((x > 0)) THEN ('a') WHEN ((x < 0)) THEN ('b') ELSE 'c' END FROM t"
        );
        assert_eq!(
            body("SELECT CASE x WHEN 1 THEN 'a' END FROM t"),
            "SELECT CASE  WHEN ((x = 1)) THEN ('a') ELSE NULL END FROM t"
        );
    }

    #[test]
    fn a_cast_writes_its_type_in_upper_case_with_a_space_after_the_comma() {
        assert_eq!(body("SELECT x::varchar FROM t"), "SELECT CAST(x AS VARCHAR) FROM t");
        assert_eq!(
            body("SELECT cast(x as decimal(4,1)) FROM t"),
            "SELECT CAST(x AS DECIMAL(4, 1)) FROM t"
        );
        assert_eq!(
            body("SELECT TRY_CAST(s AS INTEGER) FROM t"),
            "SELECT TRY_CAST(s AS INTEGER) FROM t"
        );
    }

    /// The names the grammar has a rule for, which come back under the one name the type has.
    #[test]
    fn a_standard_type_name_is_resolved_and_the_modifiers_it_was_written_with_survive() {
        let cast = |written: &str| body(&format!("SELECT CAST(x AS {written})"));
        assert_eq!(cast("int"), "SELECT CAST(x AS INTEGER)");
        assert_eq!(cast("numeric(5)"), "SELECT CAST(x AS DECIMAL(5))");
        assert_eq!(cast("decimal"), "SELECT CAST(x AS DECIMAL)");
        assert_eq!(cast("varchar(10)"), "SELECT CAST(x AS VARCHAR(10))");
        assert_eq!(cast("national character(2)"), "SELECT CAST(x AS VARCHAR(2))");
        // The argument of a float chose the type rather than sitting on it, so there is nothing
        // left of the ten by the time it is written back.
        assert_eq!(cast("float(10)"), "SELECT CAST(x AS FLOAT)");
        assert_eq!(cast("real"), "SELECT CAST(x AS FLOAT)");
        assert_eq!(cast("double precision"), "SELECT CAST(x AS DOUBLE)");
        assert_eq!(cast("time with time zone"), "SELECT CAST(x AS TIME WITH TIME ZONE)");
        assert_eq!(cast("int[]"), "SELECT CAST(x AS INTEGER[])");
        assert_eq!(cast("int[2][3]"), "SELECT CAST(x AS INTEGER[2][3])");
        assert_eq!(cast("map(int, varchar)"), "SELECT CAST(x AS MAP(INTEGER, VARCHAR))");
        assert_eq!(cast("union(a int)"), "SELECT CAST(x AS UNION(a INTEGER))");
    }

    /// A struct keeps the case of the field name and resolves the field type.
    #[test]
    fn a_struct_field_keeps_its_name_and_its_type_goes_round_again() {
        assert_eq!(body("SELECT CAST(x AS struct(a bool))"), "SELECT CAST(x AS STRUCT(a bool))");
        assert_eq!(
            body("SELECT CAST(x AS struct(\"A b\" int))"),
            "SELECT CAST(x AS STRUCT(\"A b\" INTEGER))"
        );
    }

    /// And every other name is the catalog's business, so the text is written back as it stands.
    #[test]
    fn a_type_name_the_grammar_has_no_rule_for_keeps_the_case_it_was_written_in() {
        let cast = |written: &str| body(&format!("SELECT CAST(x AS {written})"));
        assert_eq!(cast("text"), "SELECT CAST(x AS text)");
        assert_eq!(cast("TEXT"), "SELECT CAST(x AS TEXT)");
        assert_eq!(cast("DOUBLE"), "SELECT CAST(x AS DOUBLE)");
        assert_eq!(cast("bool"), "SELECT CAST(x AS bool)");
        assert_eq!(cast("\"bool\""), "SELECT CAST(x AS bool)");
        assert_eq!(cast("int4[]"), "SELECT CAST(x AS int4[])");
        assert_eq!(cast("TIMESTAMPTZ"), "SELECT CAST(x AS TIMESTAMPTZ)");
        // The one name that comes back quoted, for the reason written on `catalogued`.
        assert_eq!(cast("JSON"), "SELECT CAST(x AS \"JSON\")");
        assert_eq!(cast("json"), "SELECT CAST(x AS \"json\")");
        assert_eq!(cast("json[]"), "SELECT CAST(x AS \"json\"[])");
        assert_eq!(cast("struct(a json)"), "SELECT CAST(x AS STRUCT(a \"json\"))");
    }

    #[test]
    fn a_star_count_is_a_function_of_its_own_and_a_list_is_a_call() {
        assert_eq!(body("SELECT count(*) FROM t"), "SELECT count_star() FROM t");
        assert_eq!(body("SELECT count(DISTINCT x) FROM t"), "SELECT count(DISTINCT x) FROM t");
        assert_eq!(body("SELECT [1, 2, 3]"), "SELECT list_value(1, 2, 3)");
        assert_eq!(body("SELECT []"), "SELECT list_value()");
    }

    /// A function name goes through the same quoting rule an identifier does, so the ones that are
    /// keywords in a class come back quoted.
    #[test]
    fn a_function_name_is_quoted_when_it_is_a_keyword() {
        assert_eq!(body("SELECT nullif(x, 1) FROM t"), "SELECT \"nullif\"(x, 1) FROM t");
        assert_eq!(body("SELECT length(s) FROM t"), "SELECT length(s) FROM t");
    }

    #[test]
    fn the_literals() {
        assert_eq!(body("SELECT NULL, TRUE, FALSE"), "SELECT NULL, true, false");
        assert_eq!(body("SELECT 1.50, .5, 1_000"), "SELECT 1.50, .5, 1000");
        assert_eq!(body("SELECT 'it''s'"), "SELECT 'it''s'");
    }

    /// A number comes back as the value it was read as, which is three rules and not one.
    #[test]
    fn a_number_is_written_back_as_the_value_the_shape_of_it_made() {
        assert_eq!(body("SELECT 007, 1_000"), "SELECT 7, 1000");
        assert_eq!(body("SELECT 1.50, 00.5, 1., 0.0"), "SELECT 1.50, 0.5, 1, 0.0");
        assert_eq!(body("SELECT 1e3, 1.5e2, 1e-3, 5e-4"), "SELECT 1000.0, 150.0, 0.001, 0.0005");
        assert_eq!(body("SELECT 5e-5, 2.5e-5, 1e-10"), "SELECT 5e-05, 2.5e-05, 1e-10");
        assert_eq!(body("SELECT 1e15, 1e16, 1e100"), "SELECT 1000000000000000.0, 1e+16, 1e+100");
    }

    /// The part of an `EXTRACT` is a keyword and a keyword has one spelling.
    #[test]
    fn an_extract_is_a_date_part_call_and_the_keyword_it_named_has_one_spelling() {
        assert_eq!(body("SELECT extract(year FROM d)"), "SELECT date_part('YEAR', d)");
        assert_eq!(body("SELECT extract(years FROM d)"), "SELECT date_part('YEAR', d)");
        assert_eq!(body("SELECT extract(seconds FROM d)"), "SELECT date_part('SECOND', d)");
        // Two of the thirteen are written back plural, which is a list and not a rule.
        assert_eq!(
            body("SELECT extract(millisecond FROM d)"),
            "SELECT date_part('MILLISECONDS', d)"
        );
        assert_eq!(
            body("SELECT extract(microseconds FROM d)"),
            "SELECT date_part('MICROSECONDS', d)"
        );
        assert_eq!(body("SELECT extract(millennia FROM d)"), "SELECT date_part('MILLENNIUM', d)");
        // A word the grammar does not name as a keyword is an identifier and keeps its case.
        assert_eq!(body("SELECT extract(epoch FROM d)"), "SELECT date_part('epoch', d)");
        assert_eq!(body("SELECT extract(dow FROM d)"), "SELECT date_part('dow', d)");
    }

    /// The two spellings of the one operator, which is not a function name however much it looks it.
    #[test]
    fn coalesce_and_ifnull_are_one_operator_and_it_is_written_in_upper_case() {
        assert_eq!(body("SELECT coalesce(x, y)"), "SELECT COALESCE(x, y)");
        assert_eq!(body("SELECT IfNull(x, y)"), "SELECT COALESCE(x, y)");
        // Including the one argument form, which is not folded away.
        assert_eq!(body("SELECT coalesce(x)"), "SELECT COALESCE(x)");
        // And no other name does this, which `nullif` is the one to check against.
        assert_eq!(body("SELECT nullif(x, y)"), "SELECT \"nullif\"(x, y)");
        assert_eq!(body("SELECT greatest(x, y)"), "SELECT greatest(x, y)");
    }

    #[test]
    fn the_modifiers_hang_off_the_query_and_not_off_the_select() {
        assert_eq!(body("SELECT x FROM t LIMIT 5 OFFSET 2"), "SELECT x FROM t LIMIT 5 OFFSET 2");
        assert_eq!(body("SELECT x FROM t LIMIT 10 PERCENT"), "SELECT x FROM t LIMIT (10) %");
        assert_eq!(
            body("SELECT x FROM t ORDER BY x ASC, y NULLS LAST"),
            "SELECT x FROM t ORDER BY x ASC, y NULLS LAST"
        );
        assert_eq!(body("SELECT x FROM t ORDER BY ALL"), "SELECT x FROM t ORDER BY COLUMNS(*)");
        assert_eq!(body("SELECT x FROM t GROUP BY ALL"), "SELECT x FROM t GROUP BY ALL");
        assert_eq!(
            body("SELECT x FROM t GROUP BY x HAVING x > 0"),
            "SELECT x FROM t GROUP BY x HAVING (x > 0)"
        );
        assert_eq!(
            body("SELECT DISTINCT ON (x) x, y FROM t"),
            "SELECT DISTINCT ON (x) x, y FROM t"
        );
    }

    /// A branch that is a set operation of its own is written bare, and a bare left branch loses the
    /// space that would follow it. Upstream's, and reproduced because the column is a comparison.
    #[test]
    fn a_chain_of_set_operations_loses_a_space_in_the_middle() {
        assert_eq!(
            body("SELECT x FROM t UNION ALL SELECT y FROM t"),
            "(SELECT x FROM t) UNION ALL (SELECT y FROM t)"
        );
        assert_eq!(
            body("SELECT x FROM t UNION SELECT y FROM t UNION SELECT 1"),
            "(SELECT x FROM t) UNION (SELECT y FROM t)UNION (SELECT 1)"
        );
        assert_eq!(
            body("SELECT x FROM t UNION DISTINCT SELECT y FROM t"),
            "(SELECT x FROM t) UNION (SELECT y FROM t)"
        );
    }

    #[test]
    fn a_values_body_is_wrapped_in_a_select_that_names_it() {
        assert_eq!(
            body("VALUES (1, 'a'), (2, 'b')"),
            "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS valueslist"
        );
    }

    /// A space before each comma, which is upstream's and is not a typo here.
    #[test]
    fn a_from_list_has_a_space_before_the_comma() {
        assert_eq!(body("SELECT 1 FROM t AS t1, t AS t2"), "SELECT 1 FROM t AS t1 , t AS t2");
    }

    #[test]
    fn a_from_item_and_its_aliases() {
        assert_eq!(body("SELECT 1 FROM t AS r(n)"), "SELECT 1 FROM t AS r(n)");
        assert_eq!(body("SELECT 1 FROM main.t"), "SELECT 1 FROM main.t");
        assert_eq!(
            body("SELECT 1 FROM (SELECT x FROM t) AS sub"),
            "SELECT 1 FROM (SELECT x FROM t) AS sub"
        );
        assert_eq!(body("SELECT 1 FROM range(10)"), "SELECT 1 FROM \"range\"(10)");
    }

    /// Joins are parenthesised, `FULL OUTER` loses a word, `NATURAL` gains one, and an `ON` gets a
    /// second pair of parentheses on top of the ones the condition already has.
    #[test]
    fn a_join_is_parenthesised_and_so_is_its_condition_twice() {
        assert_eq!(
            body("SELECT 1 FROM t AS a JOIN t AS b ON a.x = b.y"),
            "SELECT 1 FROM (t AS a INNER JOIN t AS b ON ((a.x = b.y)))"
        );
        assert_eq!(
            body("SELECT 1 FROM t LEFT JOIN t AS u USING (x)"),
            "SELECT 1 FROM (t LEFT JOIN t AS u USING (x))"
        );
        assert_eq!(
            body("SELECT 1 FROM t CROSS JOIN t AS u"),
            "SELECT 1 FROM (t CROSS JOIN t AS u)"
        );
        assert_eq!(
            body("SELECT 1 FROM t FULL OUTER JOIN t AS u ON t.x = u.x"),
            "SELECT 1 FROM (t FULL JOIN t AS u ON ((t.x = u.x)))"
        );
        assert_eq!(
            body("SELECT 1 FROM t NATURAL JOIN t AS u"),
            "SELECT 1 FROM (t NATURAL INNER JOIN t AS u)"
        );
        assert_eq!(
            body("SELECT 1 FROM t POSITIONAL JOIN t AS u"),
            "SELECT 1 FROM (t POSITIONAL JOIN t AS u)"
        );
    }

    #[test]
    fn a_target_keeps_its_alias_and_a_star_keeps_its_replace_list() {
        assert_eq!(body("SELECT 1 + 2 AS \"quoted alias\""), "SELECT (1 + 2) AS \"quoted alias\"");
        assert_eq!(body("SELECT x AS \"select\" FROM t"), "SELECT x AS \"select\" FROM t");
        assert_eq!(body("SELECT t.* FROM t"), "SELECT t.* FROM t");
        assert_eq!(
            body("SELECT * REPLACE (x + 1 AS x) FROM t"),
            "SELECT * REPLACE ((x + 1) AS x) FROM t"
        );
    }

    #[test]
    fn a_describe_gets_parentheses_round_what_it_describes() {
        assert_eq!(body("DESCRIBE SELECT 1"), "DESCRIBE (SELECT 1)");
    }
}
