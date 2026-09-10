//! The scalar functions, which for M0 is arithmetic, the string functions and `LIKE`.
//!
//! One entry point rather than a function pointer per name, because the binder has already decided
//! which function this is and what its arguments were cast to, so all that is left is to do the
//! work. When the kernel generator in `spec/07-execution.md` section 7.3 arrives this becomes a
//! table lookup and the bodies below become the generated specializations, and the interface the
//! executor calls does not change.
//!
//! Null in, null out, for everything except `coalesce`. That rule is applied once here rather than
//! inside each function, which is the only way to be sure that a function added later does not
//! quietly forget it.
//!
//! Division by zero produces null rather than raising. That is DuckDB's behaviour and it is not
//! Postgres's, and it is one of the compatibility decisions that is worth a line of its own,
//! because a query that returns a row where another engine raises is a difference a user notices.

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_vector::Vector;

use crate::number::{approximate, digits, fit, integral, pow10, rescale};

/// Calls a scalar function on a batch.
///
/// `returns` is the type the binder resolved the call to, and it is passed in rather than derived
/// because deriving it would mean consulting the signature table from inside a kernel, and the
/// signature table lives seven ranks above this crate.
///
/// # Errors
///
/// If the arguments are not all the same length, if the function is not one of the ones written
/// here, or if the call fails at some row.
pub fn call(name: &str, args: &[Vector], returns: &LogicalType) -> Result<Vector> {
    let rows = args.first().map_or(0, Vector::len);
    for (at, arg) in args.iter().enumerate() {
        if arg.len() != rows {
            return Err(Error::internal(format!(
                "argument {at} of {name} is {} rows and argument 0 is {rows}",
                arg.len()
            )));
        }
    }
    let mut row = Vec::with_capacity(args.len());
    let mut values = Vec::with_capacity(rows);
    for index in 0..rows {
        row.clear();
        row.extend(args.iter().map(|arg| arg.value_at(index)));
        values.push(call_values(name, &row, returns)?);
    }
    Vector::from_values(returns.clone(), &values)
}

/// Calls a scalar function on one row.
///
/// # Errors
///
/// If the function is not one of the ones written here, or if the call fails.
pub fn call_values(name: &str, args: &[Value], returns: &LogicalType) -> Result<Value> {
    if name == "coalesce" {
        let found = args.iter().find(|value| !value.is_null());
        return Ok(found.cloned().unwrap_or(Value::Null));
    }
    if args.iter().any(Value::is_null) {
        return Ok(Value::Null);
    }
    match (name, args) {
        ("+", [only]) => Ok(only.clone()),
        ("-", [only]) => negate(only, returns),
        ("abs", [only]) => absolute(only, returns),
        ("not", [only]) => match only.as_bool() {
            Some(held) => Ok(Value::Boolean(!held)),
            None => Err(Error::internal(format!("not of a {}", only.logical_type()))),
        },
        ("+", [left, right]) => arithmetic(Op::Add, left, right, returns),
        ("-", [left, right]) => arithmetic(Op::Subtract, left, right, returns),
        ("*", [left, right]) => arithmetic(Op::Multiply, left, right, returns),
        ("%", [left, right]) => arithmetic(Op::Modulo, left, right, returns),
        ("//", [left, right]) => arithmetic(Op::Divide, left, right, returns),
        ("/", [left, right]) => divide(left, right),
        ("||", [left, right]) => Ok(Value::Varchar(format!("{left}{right}"))),
        ("lower", [only]) => Ok(Value::Varchar(only.to_string().to_lowercase())),
        ("upper", [only]) => Ok(Value::Varchar(only.to_string().to_uppercase())),
        ("length", [only]) => Ok(Value::BigInt(count_characters(only))),
        ("~~", [text, pattern]) => Ok(Value::Boolean(matches(text, pattern, false))),
        ("!~~", [text, pattern]) => Ok(Value::Boolean(!matches(text, pattern, false))),
        ("~~*", [text, pattern]) => Ok(Value::Boolean(matches(text, pattern, true))),
        ("!~~*", [text, pattern]) => Ok(Value::Boolean(!matches(text, pattern, true))),
        _ => Err(Error::not_implemented(format!(
            "the {name} function with {} arguments",
            args.len()
        ))),
    }
}

/// Which arithmetic, kept separate from the spelling so that the overflow message can name it the
/// way DuckDB names it.
#[derive(Debug, Clone, Copy)]
enum Op {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

impl Op {
    fn word(self) -> &'static str {
        match self {
            Self::Add => "addition",
            Self::Subtract => "subtraction",
            Self::Multiply => "multiplication",
            Self::Divide => "division",
            Self::Modulo => "modulo",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Subtract => "-",
            Self::Multiply => "*",
            Self::Divide => "//",
            Self::Modulo => "%",
        }
    }
}

fn overflow(op: Op, ty: &LogicalType, left: &Value, right: &Value) -> Error {
    Error::out_of_range(format!(
        "Overflow in {} of {ty} ({left} {} {right})!",
        op.word(),
        op.symbol()
    ))
}

fn arithmetic(op: Op, left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    match ty {
        LogicalType::Float | LogicalType::Double => float_arithmetic(op, left, right, ty),
        LogicalType::Decimal { width, scale } => {
            decimal_arithmetic(op, left, right, *width, *scale)
        }
        other if other.is_integer() => integer_arithmetic(op, left, right, ty),
        other => Err(Error::not_implemented(format!("{} on {other}", op.word()))),
    }
}

fn integer_arithmetic(op: Op, left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    let (a, b) = match (integral(left), integral(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "{} on {} and {}",
                op.word(),
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if matches!(op, Op::Divide | Op::Modulo) && b == 0 {
        return Ok(Value::Null);
    }
    let wide = match op {
        Op::Add => a.checked_add(b),
        Op::Subtract => a.checked_sub(b),
        Op::Multiply => a.checked_mul(b),
        Op::Divide => a.checked_div(b),
        Op::Modulo => a.checked_rem(b),
    };
    wide.and_then(|whole| fit(whole, ty)).ok_or_else(|| overflow(op, ty, left, right))
}

fn float_arithmetic(op: Op, left: &Value, right: &Value, ty: &LogicalType) -> Result<Value> {
    let (a, b) = match (approximate(left), approximate(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "{} on {} and {}",
                op.word(),
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if matches!(op, Op::Divide | Op::Modulo) && b == 0.0 {
        return Ok(Value::Null);
    }
    let result = match op {
        Op::Add => a + b,
        Op::Subtract => a - b,
        Op::Multiply => a * b,
        Op::Divide => (a / b).trunc(),
        Op::Modulo => a % b,
    };
    if matches!(ty, LogicalType::Float) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "arithmetic on a FLOAT column produces a FLOAT"
        )]
        return Ok(Value::Float(result as f32));
    }
    Ok(Value::Double(result))
}

fn decimal_arithmetic(op: Op, left: &Value, right: &Value, width: u8, scale: u8) -> Result<Value> {
    let ty = LogicalType::Decimal { width, scale };
    let (a, b) = match (unscaled_at(left, scale), unscaled_at(right, scale)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "{} on {} and {}",
                op.word(),
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if matches!(op, Op::Divide | Op::Modulo) && b == 0 {
        return Ok(Value::Null);
    }
    let unscaled = match op {
        Op::Add => a.checked_add(b),
        Op::Subtract => a.checked_sub(b),
        Op::Multiply => a.checked_mul(b).and_then(|wide| rescale(wide, scale * 2, scale)),
        Op::Modulo => a.checked_rem(b),
        Op::Divide => a.checked_div(b).and_then(|whole| whole.checked_mul(pow10(scale))),
    };
    let unscaled = unscaled.ok_or_else(|| overflow(op, &ty, left, right))?;
    if digits(unscaled) > width {
        return Err(overflow(op, &ty, left, right));
    }
    Ok(Value::Decimal { unscaled, width, scale })
}

/// A value as an unscaled integer at the given scale, for the decimal path.
fn unscaled_at(value: &Value, scale: u8) -> Option<i128> {
    match *value {
        Value::Decimal { unscaled, scale: held, .. } => rescale(unscaled, held, scale),
        _ => integral(value).and_then(|whole| whole.checked_mul(pow10(scale))),
    }
}

/// `/`, which the binder has already promoted both sides to `DOUBLE` for.
fn divide(left: &Value, right: &Value) -> Result<Value> {
    let (a, b) = match (approximate(left), approximate(right)) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(Error::not_implemented(format!(
                "division on {} and {}",
                left.logical_type(),
                right.logical_type()
            )));
        }
    };
    if b == 0.0 {
        return Ok(Value::Null);
    }
    Ok(Value::Double(a / b))
}

fn negate(value: &Value, ty: &LogicalType) -> Result<Value> {
    match value {
        Value::Float(v) => Ok(Value::Float(-v)),
        Value::Double(v) => Ok(Value::Double(-v)),
        Value::Decimal { unscaled, width, scale } => {
            Ok(Value::Decimal { unscaled: -unscaled, width: *width, scale: *scale })
        }
        _ => match integral(value) {
            Some(whole) => whole
                .checked_neg()
                .and_then(|negated| fit(negated, ty))
                .ok_or_else(|| overflow(Op::Subtract, ty, &Value::Integer(0), value)),
            None => Err(Error::not_implemented(format!("negating a {}", value.logical_type()))),
        },
    }
}

fn absolute(value: &Value, ty: &LogicalType) -> Result<Value> {
    match value {
        Value::Float(v) => Ok(Value::Float(v.abs())),
        Value::Double(v) => Ok(Value::Double(v.abs())),
        Value::Decimal { unscaled, width, scale } => {
            Ok(Value::Decimal { unscaled: unscaled.abs(), width: *width, scale: *scale })
        }
        _ => match integral(value) {
            Some(whole) => whole
                .checked_abs()
                .and_then(|positive| fit(positive, ty))
                .ok_or_else(|| overflow(Op::Subtract, ty, &Value::Integer(0), value)),
            None => Err(Error::not_implemented(format!("abs of a {}", value.logical_type()))),
        },
    }
}

/// `length`, which counts characters rather than bytes, the way DuckDB does.
fn count_characters(value: &Value) -> i64 {
    let text = match value.as_str() {
        Some(text) => text.chars().count(),
        None => value.to_string().chars().count(),
    };
    i64::try_from(text).unwrap_or(i64::MAX)
}

/// SQL `LIKE`, where `%` is any run and `_` is one character.
///
/// The loop is the standard one with a single backtracking point, which is linear on the patterns
/// that appear in practice and avoids the exponential blowup a naive recursion has on a pattern
/// like `%a%a%a%a%`. ClickBench query 21 is `LIKE '%google%'` over 100 million rows, so this is
/// somewhere the shape of the algorithm is going to matter.
fn matches(text: &Value, pattern: &Value, fold_case: bool) -> bool {
    let (text, pattern) = if fold_case {
        (text.to_string().to_lowercase(), pattern.to_string().to_lowercase())
    } else {
        (text.to_string(), pattern.to_string())
    };
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut at, mut against) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while at < text.len() {
        if against < pattern.len() && (pattern[against] == '_' || pattern[against] == text[at]) {
            at += 1;
            against += 1;
        } else if against < pattern.len() && pattern[against] == '%' {
            star = Some(against);
            resume = at;
            against += 1;
        } else if let Some(back) = star {
            against = back + 1;
            resume += 1;
            at = resume;
        } else {
            return false;
        }
    }
    while against < pattern.len() && pattern[against] == '%' {
        against += 1;
    }
    against == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn called(name: &str, args: &[Value], returns: &LogicalType) -> Value {
        call_values(name, args, returns).expect("this call is written")
    }

    #[test]
    fn null_in_is_null_out_for_everything_but_coalesce() {
        assert_eq!(
            called("+", &[Value::Integer(1), Value::Null], &LogicalType::Integer),
            Value::Null
        );
        assert_eq!(
            called("coalesce", &[Value::Null, Value::Integer(2)], &LogicalType::Integer),
            Value::Integer(2)
        );
        assert_eq!(
            called("coalesce", &[Value::Null, Value::Null], &LogicalType::Integer),
            Value::Null
        );
    }

    #[test]
    fn arithmetic_that_overflows_says_so_rather_than_wrapping() {
        let error =
            call_values("+", &[Value::Integer(i32::MAX), Value::Integer(1)], &LogicalType::Integer)
                .expect_err("2147483647 + 1 is not an integer");
        assert!(error.message().contains("Overflow in addition of INTEGER"), "{error}");
    }

    /// DuckDB returns null here where Postgres raises, and this is the line that records it.
    #[test]
    fn dividing_by_zero_is_null() {
        assert_eq!(
            called("/", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Double),
            Value::Null
        );
        assert_eq!(
            called("//", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Integer),
            Value::Null
        );
        assert_eq!(
            called("%", &[Value::Integer(1), Value::Integer(0)], &LogicalType::Integer),
            Value::Null
        );
    }

    #[test]
    fn a_division_is_a_double_even_when_both_sides_are_whole() {
        assert_eq!(
            called("/", &[Value::Integer(7), Value::Integer(2)], &LogicalType::Double),
            Value::Double(3.5)
        );
        assert_eq!(
            called("//", &[Value::Integer(7), Value::Integer(2)], &LogicalType::Integer),
            Value::Integer(3)
        );
    }

    #[test]
    fn decimals_add_at_their_own_scale_and_multiply_back_down_to_it() {
        let ty = LogicalType::decimal(10, 2).expect("a legal decimal");
        let two_fifty = Value::Decimal { unscaled: 250, width: 10, scale: 2 };
        let four = Value::Decimal { unscaled: 400, width: 10, scale: 2 };
        assert_eq!(
            called("+", &[two_fifty.clone(), four.clone()], &ty),
            Value::Decimal { unscaled: 650, width: 10, scale: 2 }
        );
        assert_eq!(
            called("*", &[two_fifty, four], &ty),
            Value::Decimal { unscaled: 1000, width: 10, scale: 2 }
        );
    }

    #[test]
    fn strings_join_and_fold() {
        assert_eq!(
            called(
                "||",
                &[Value::Varchar("ab".into()), Value::Varchar("cd".into())],
                &LogicalType::Varchar
            ),
            Value::Varchar("abcd".into())
        );
        assert_eq!(
            called("upper", &[Value::Varchar("aB".into())], &LogicalType::Varchar),
            Value::Varchar("AB".into())
        );
        assert_eq!(
            called("length", &[Value::Varchar("héllo".into())], &LogicalType::BigInt),
            Value::BigInt(5)
        );
    }

    #[test]
    fn like_matches_the_way_sql_says_it_does() {
        let text = Value::Varchar("google.com".into());
        for (pattern, expected) in [
            ("%google%", true),
            ("google%", true),
            ("%com", true),
            ("g_ogle.com", true),
            ("g__gle.com", true),
            ("goggle%", false),
            ("%GOOGLE%", false),
            ("google.com", true),
            ("%", true),
        ] {
            let held = called(
                "~~",
                &[text.clone(), Value::Varchar(pattern.into())],
                &LogicalType::Boolean,
            );
            assert_eq!(held, Value::Boolean(expected), "{pattern}");
        }
    }

    #[test]
    fn like_backtracks_rather_than_giving_up_at_the_first_star() {
        let text = Value::Varchar("aaaaaaab".into());
        let held = called("~~", &[text, Value::Varchar("%a%a%b".into())], &LogicalType::Boolean);
        assert_eq!(held, Value::Boolean(true));
    }

    #[test]
    fn the_case_folding_like_ignores_case_and_the_negated_ones_invert() {
        let text = Value::Varchar("Google".into());
        let pattern = Value::Varchar("%GOOGLE%".into());
        assert_eq!(
            called("~~*", &[text.clone(), pattern.clone()], &LogicalType::Boolean),
            Value::Boolean(true)
        );
        assert_eq!(called("!~~", &[text, pattern], &LogicalType::Boolean), Value::Boolean(true));
    }

    #[test]
    fn a_function_nobody_has_written_says_which_one() {
        let error = call_values("sqrt", &[Value::Double(4.0)], &LogicalType::Double)
            .expect_err("sqrt is not written yet");
        assert!(error.message().contains("the sqrt function"), "{error}");
    }

    #[test]
    fn a_batch_call_is_one_answer_per_row() {
        let left = Vector::from_values(
            LogicalType::Integer,
            &[Value::Integer(1), Value::Integer(2), Value::Null],
        )
        .expect("three rows");
        let right = Vector::constant(LogicalType::Integer, Value::Integer(10), 3);
        let sum = call("+", &[left, right], &LogicalType::Integer).expect("adds");
        assert_eq!(sum.value_at(0), Value::Integer(11));
        assert_eq!(sum.value_at(1), Value::Integer(12));
        assert_eq!(sum.value_at(2), Value::Null);
    }

    #[test]
    fn arguments_of_different_lengths_are_caught() {
        let left = Vector::constant(LogicalType::Integer, Value::Integer(1), 3);
        let right = Vector::constant(LogicalType::Integer, Value::Integer(1), 4);
        let error = call("+", &[left, right], &LogicalType::Integer).expect_err("ragged");
        assert!(error.message().contains("argument 1"), "{error}");
    }
}
