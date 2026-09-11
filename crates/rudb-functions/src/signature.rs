//! What a function call resolves to.
//!
//! This is the smallest thing the binder cannot be written without: given a name and the types of
//! the arguments, which function is that and what does it return. It is not the function library.
//! There is no implementation attached to any of these yet, no volatility, no statistics and no
//! vectorized kernel, and all of that is what this crate grows into.
//!
//! The set here is what M0 reaches, which is the operators the transformer emits plus the five
//! aggregates a first query needs. A name that is not in it produces DuckDB's own error text rather
//! than a Rust panic or a silent pass through, because a function that binds and then does nothing
//! is a wrong answer and a function that does not bind is a message.
//!
//! Overload resolution here is by shape rather than by an exact signature match. `+` does not have
//! one entry per pair of numeric types, it has one entry that says both arguments promote and the
//! result is what they promote to. DuckDB's own table is closer to the former and it needs to be,
//! because it carries an implementation per pair. Ours does not carry one yet, and inventing 169
//! rows before there is a kernel behind any of them would be inventing the wrong 169 rows.

use rudb_common::{Error, LogicalType, Result};

/// Whether a name is a scalar function or an aggregate.
///
/// The binder needs to ask before it knows which slot the call goes in, since an aggregate is only
/// legal in an aggregate list and the error for one in the wrong place should say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    /// One row in, one row out.
    Scalar,
    /// Many rows in, one row out.
    Aggregate,
}

/// A resolved call.
///
/// `arguments` is what the arguments have to be cast to and not what they were, so the binder can
/// insert the casts without redoing the resolution. It is the same length as what was passed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The function's own name, which is what the plan records.
    pub name: &'static str,
    /// Scalar or aggregate.
    pub kind: FunctionKind,
    /// What each argument has to be cast to.
    pub arguments: Vec<LogicalType>,
    /// What the call produces.
    pub returns: LogicalType,
}

/// How the argument types decide the return type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Every argument promotes to one type and the result is that type. `+` and `min`.
    Promoted,
    /// Every argument promotes to one type and the result is fixed. `=` over anything is boolean.
    PromotedTo(Fixed),
    /// Every argument is cast to one fixed type and the result is another. `||` over strings.
    FixedTo(Fixed, Fixed),
    /// The arguments are whatever they are and the result is fixed. `count(x)` over anything.
    AnyTo(Fixed),
    /// The first `n` arguments are cast to one fixed type, the rest are left alone, and the result
    /// is fixed. `date_part('minute', x)` is a bigint whatever `x` is, and
    /// `regexp_extract(s, p, 2)` takes two strings and then a number that has to stay one.
    LeadingFixedTo(usize, Fixed, Fixed),
    /// The first argument is cast to one fixed type, the rest are left alone, and the result is the
    /// last argument's own type. `date_trunc('month', x)` gives back whatever kind of date `x` was.
    LeadingFixedToLast(Fixed),
    /// Every argument promotes and an integer result widens to the accumulator. `sum`.
    Accumulated,
}

/// The return types a signature can name outright.
///
/// A small enum rather than a `LogicalType` so that the table stays a `const` and there is no
/// allocation behind a lookup that happens once per expression in every query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fixed {
    Boolean,
    Integer,
    BigInt,
    Double,
    Varchar,
    Date,
    Timestamp,
}

impl Fixed {
    fn ty(self) -> LogicalType {
        match self {
            Self::Boolean => LogicalType::Boolean,
            Self::Integer => LogicalType::Integer,
            Self::BigInt => LogicalType::BigInt,
            Self::Double => LogicalType::Double,
            Self::Varchar => LogicalType::Varchar,
            Self::Date => LogicalType::Date,
            Self::Timestamp => LogicalType::Timestamp,
        }
    }
}

/// How many arguments a function takes.
///
/// A range rather than a count because `-` is both the negation and the subtraction, and one name
/// with two arities is much less trouble than two names that the transformer would have to tell
/// apart before the binder ever sees the call.
///
/// `OneOf` is the range with a hole in it. `make_date` takes one argument or three and not two, and
/// a range that accepted two would bind a call DuckDB refuses and then have nothing to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    Exactly(usize),
    Between(usize, Option<usize>),
    OneOf(&'static [usize]),
}

impl Arity {
    const fn exactly(count: usize) -> Self {
        Self::Exactly(count)
    }

    const fn between(least: usize, most: usize) -> Self {
        Self::Between(least, Some(most))
    }

    const fn at_least(least: usize) -> Self {
        Self::Between(least, None)
    }

    const fn one_of(counts: &'static [usize]) -> Self {
        Self::OneOf(counts)
    }

    fn accepts(self, count: usize) -> bool {
        match self {
            Self::Exactly(wanted) => count == wanted,
            Self::Between(least, most) => count >= least && most.is_none_or(|most| count <= most),
            Self::OneOf(counts) => counts.contains(&count),
        }
    }

    /// Every count this accepts, with an open end stopped one past where it starts, for the tests
    /// that hold each row of the table to its own shape at each count it claims to take.
    #[cfg(test)]
    fn counts(self) -> Vec<usize> {
        match self {
            Self::Exactly(count) => vec![count],
            Self::Between(least, most) => (least..=most.unwrap_or(least + 1)).collect(),
            Self::OneOf(counts) => counts.to_vec(),
        }
    }

    #[cfg(test)]
    fn least(self) -> usize {
        match self {
            Self::Exactly(count) | Self::Between(count, _) => count,
            Self::OneOf(counts) => counts.iter().copied().min().unwrap_or(0),
        }
    }
}

struct Entry {
    name: &'static str,
    kind: FunctionKind,
    arity: Arity,
    shape: Shape,
    /// Whether every argument has to be a number, which is the only argument constraint M0 needs.
    numeric_only: bool,
}

/// The whole table.
///
/// One row per name. There are no overloads by argument type in here yet, because every name below
/// has exactly one shape, and a second row for a name would need a rule for which one wins that is
/// worth writing when there is a name that needs it.
const TABLE: &[Entry] = &[
    // Arithmetic. The result is what the operands promote to, so `INTEGER + BIGINT` is a `BIGINT`
    // and the executor never has to widen mid expression. `+` and `-` take one argument as well as
    // two, because the unary forms are the same function and DuckDB names them the same way.
    number("+", Arity::between(1, 2), Shape::Promoted),
    number("-", Arity::between(1, 2), Shape::Promoted),
    number("*", Arity::exactly(2), Shape::Promoted),
    number("%", Arity::exactly(2), Shape::Promoted),
    // `/` is the exception and it is DuckDB's exception too: `7 / 2` is 3.5 and not 3, so the
    // result is a double whatever went in, and `//` is the operator that keeps the integer.
    number("/", Arity::exactly(2), Shape::PromotedTo(Fixed::Double)),
    number("//", Arity::exactly(2), Shape::Promoted),
    number("abs", Arity::exactly(1), Shape::Promoted),
    // Strings.
    text("||", Arity::exactly(2), Fixed::Varchar),
    text("lower", Arity::exactly(1), Fixed::Varchar),
    text("upper", Arity::exactly(1), Fixed::Varchar),
    text("length", Arity::exactly(1), Fixed::BigInt),
    // Pattern matching. The transformer emits the operator spellings, so those are the names, and
    // `LIKE` is one of them rather than a keyword the binder has to know about separately.
    text("~~", Arity::exactly(2), Fixed::Boolean),
    text("!~~", Arity::exactly(2), Fixed::Boolean),
    text("~~*", Arity::exactly(2), Fixed::Boolean),
    text("!~~*", Arity::exactly(2), Fixed::Boolean),
    // Logic. `AND` and `OR` are conjunctions in the plan rather than calls, so only `NOT` is here.
    Entry {
        name: "not",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::FixedTo(Fixed::Boolean, Fixed::Boolean),
        numeric_only: false,
    },
    // `coalesce` promotes across every argument, which is exactly what `Shape::Promoted` says, and
    // it is the one scalar here that takes a variable number of them.
    Entry {
        name: "coalesce",
        kind: FunctionKind::Scalar,
        arity: Arity::at_least(1),
        shape: Shape::Promoted,
        numeric_only: false,
    },
    // Dates and times. `EXTRACT(minute FROM x)` is spelled `date_part('minute', x)` by the time it
    // gets here, because that is what DuckDB's own parser does with it, so there is one entry for
    // the two spellings. The part is a string and the thing it is a part of is left alone, which is
    // what the two leading shapes are for: there is nothing to promote a timestamp towards.
    Entry {
        name: "date_part",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::LeadingFixedTo(1, Fixed::Varchar, Fixed::BigInt),
        numeric_only: false,
    },
    Entry {
        name: "date_trunc",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::LeadingFixedToLast(Fixed::Varchar),
        numeric_only: false,
    },
    // The two that turn a number into a date and a timestamp, which is how every ClickBench entry
    // on the board reads that data: the Parquet stores four of its columns as integers and every
    // query in the set treats them as dates and times. DuckDB's own entry wraps them in exactly
    // these two calls, so these are what let that entry run here unmodified.
    //
    // One argument is days since the epoch and three are a year, a month and a day. Upstream reads
    // the single one as an INTEGER and the triple as three BIGINTs, and both are INTEGER here,
    // because the column this is called on is an INTEGER and a widening pass over a hundred million
    // values to reach a function that immediately narrows again is a pass nobody asked for. The
    // difference shows on a year that does not fit in an INTEGER, where upstream converts and then
    // complains about the destination and this complains about the cast.
    Entry {
        name: "make_date",
        kind: FunctionKind::Scalar,
        arity: Arity::one_of(&[1, 3]),
        shape: Shape::FixedTo(Fixed::Integer, Fixed::Date),
        numeric_only: true,
    },
    // Milliseconds since the epoch. Upstream also has seven overloads that read a date or a time
    // and give the milliseconds back, which this table has no way to say yet because it is one row
    // per name and those pick by argument type. `epoch_ms` of a timestamp is the missing half.
    Entry {
        name: "epoch_ms",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::FixedTo(Fixed::BigInt, Fixed::Timestamp),
        numeric_only: true,
    },
    // Regular expressions. The pattern is a string like the text is, so three of the four are the
    // plain string shape. `regexp_extract` is not, because its third argument is the group number
    // and casting that to a string and reading it back would be a way to accept `'two'`.
    text("regexp_replace", Arity::between(3, 4), Fixed::Varchar),
    text("regexp_matches", Arity::between(2, 3), Fixed::Boolean),
    text("regexp_full_match", Arity::between(2, 3), Fixed::Boolean),
    Entry {
        name: "regexp_extract",
        kind: FunctionKind::Scalar,
        arity: Arity::between(2, 4),
        shape: Shape::LeadingFixedTo(2, Fixed::Varchar, Fixed::Varchar),
        numeric_only: false,
    },
    // Aggregates.
    aggregate("count_star", Arity::exactly(0), Shape::AnyTo(Fixed::BigInt), false),
    aggregate("count", Arity::exactly(1), Shape::AnyTo(Fixed::BigInt), false),
    aggregate("sum", Arity::exactly(1), Shape::Accumulated, true),
    aggregate("avg", Arity::exactly(1), Shape::PromotedTo(Fixed::Double), true),
    aggregate("min", Arity::exactly(1), Shape::Promoted, false),
    aggregate("max", Arity::exactly(1), Shape::Promoted, false),
];

/// A scalar that takes numbers.
const fn number(name: &'static str, arity: Arity, shape: Shape) -> Entry {
    Entry { name, kind: FunctionKind::Scalar, arity, shape, numeric_only: true }
}

/// A scalar that takes strings and returns `returns`.
const fn text(name: &'static str, arity: Arity, returns: Fixed) -> Entry {
    Entry {
        name,
        kind: FunctionKind::Scalar,
        arity,
        shape: Shape::FixedTo(Fixed::Varchar, returns),
        numeric_only: false,
    }
}

const fn aggregate(name: &'static str, arity: Arity, shape: Shape, numeric_only: bool) -> Entry {
    Entry { name, kind: FunctionKind::Aggregate, arity, shape, numeric_only }
}

/// Whether a name is a function at all, and which kind.
///
/// The binder asks this before it knows what to do with a call, since `count(x)` in a projection
/// has to become an error naming the aggregate rather than a lookup failure naming the name.
#[must_use]
pub fn kind_of(name: &str) -> Option<FunctionKind> {
    find(name).map(|entry| entry.kind)
}

/// Resolves a call.
///
/// # Errors
///
/// If there is no function of that name, if the argument count is wrong, if an argument is not a
/// number where the function needs one, or if the arguments have no type in common. The messages
/// are DuckDB's, since a great deal of code in the wild asserts on them.
pub fn resolve(name: &str, arguments: &[LogicalType]) -> Result<Resolved> {
    let entry = find(name).ok_or_else(|| {
        Error::catalog(format!("Scalar Function with name {name} does not exist!"))
    })?;
    if !entry.arity.accepts(arguments.len()) {
        return Err(Error::binder(format!(
            "No function matches the given name and argument types '{name}({})'. You might need to add explicit type casts.",
            arguments.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
        )));
    }
    if entry.numeric_only {
        for ty in arguments {
            // A null literal has no type yet and every function accepts one, since the alternative
            // is that `sum(NULL)` fails to bind rather than returning null.
            if !ty.is_numeric() && *ty != LogicalType::Null {
                return Err(Error::binder(format!(
                    "No function matches the given name and argument types '{name}({ty})'. You might need to add explicit type casts."
                )));
            }
        }
    }
    let (cast_to, returns) = match entry.shape {
        Shape::Promoted => {
            let common = promote_all(name, arguments)?;
            (vec![common.clone(); arguments.len()], common)
        }
        Shape::PromotedTo(fixed) => {
            let common = promote_all(name, arguments)?;
            (vec![common; arguments.len()], fixed.ty())
        }
        Shape::FixedTo(argument, result) => (vec![argument.ty(); arguments.len()], result.ty()),
        Shape::AnyTo(result) => (arguments.to_vec(), result.ty()),
        Shape::LeadingFixedTo(count, first, result) => {
            (leading(count, first, arguments), result.ty())
        }
        Shape::LeadingFixedToLast(first) => {
            // A null literal has no type and DuckDB refuses `date_trunc('month', NULL)` outright,
            // because it cannot tell the date overload from the interval one. Refusing needs a
            // table with both overloads in it to refuse from, which this is not yet, so the answer
            // is the widest of the candidates rather than a message about a choice nobody made.
            let last = match arguments.last() {
                Some(LogicalType::Null) | None => LogicalType::Timestamp,
                Some(ty) => ty.clone(),
            };
            (leading(1, first, arguments), last)
        }
        Shape::Accumulated => {
            let common = promote_all(name, arguments)?;
            let returns = accumulator(&common);
            (vec![common; arguments.len()], returns)
        }
    };
    Ok(Resolved { name: entry.name, kind: entry.kind, arguments: cast_to, returns })
}

/// The cast list for a shape that fixes the leading arguments and leaves the others as they are.
fn leading(count: usize, first: Fixed, arguments: &[LogicalType]) -> Vec<LogicalType> {
    let mut cast_to = arguments.to_vec();
    for head in cast_to.iter_mut().take(count) {
        *head = first.ty();
    }
    cast_to
}

/// What a sum of this type accumulates into.
///
/// Summing a column of `INTEGER` overflows an `INTEGER` after 2^31 of them and there is no useful
/// error to raise at that point, so the accumulator is the widest integer there is and the answer
/// is right. A float sums into a double for the same reason and a double stays a double, since
/// there is nothing wider to go to.
fn accumulator(ty: &LogicalType) -> LogicalType {
    if ty.is_integer() {
        LogicalType::HugeInt
    } else if *ty == LogicalType::Float {
        LogicalType::Double
    } else {
        ty.clone()
    }
}

fn promote_all(name: &str, arguments: &[LogicalType]) -> Result<LogicalType> {
    // Only reachable for a signature whose arity allows no arguments and whose shape promotes,
    // which is a combination the table does not contain and which the test below holds it to.
    let mut common = match arguments.first() {
        Some(first) => first.clone(),
        None => {
            return Err(Error::internal(format!("{name} promotes over no arguments")));
        }
    };
    for ty in &arguments[1..] {
        common = common.promote(ty).ok_or_else(|| {
            Error::binder(format!(
                "No function matches the given name and argument types '{name}({})'. You might need to add explicit type casts.",
                arguments.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            ))
        })?;
    }
    // Every argument was a null literal, which has no type. Untyped null is not a type an executor
    // can hold a vector of, so it becomes an integer, which is what DuckDB does with `SELECT NULL`.
    if common == LogicalType::Null {
        common = LogicalType::Integer;
    }
    Ok(common)
}

fn find(name: &str) -> Option<&'static Entry> {
    TABLE.iter().find(|entry| entry.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_returns_what_its_operands_promote_to() {
        let resolved = resolve("+", &[LogicalType::Integer, LogicalType::BigInt])
            .expect("an integer and a bigint add");
        assert_eq!(resolved.returns, LogicalType::BigInt);
        assert_eq!(resolved.arguments, vec![LogicalType::BigInt, LogicalType::BigInt]);
    }

    /// The one arithmetic result that is not the promotion, and it is DuckDB's rule rather than an
    /// invention: `7 / 2` is 3.5 and `7 // 2` is 3.
    #[test]
    fn division_gives_a_double_and_integer_division_does_not() {
        let divide = resolve("/", &[LogicalType::Integer, LogicalType::Integer]).expect("divides");
        assert_eq!(divide.returns, LogicalType::Double);
        let integer =
            resolve("//", &[LogicalType::Integer, LogicalType::Integer]).expect("divides");
        assert_eq!(integer.returns, LogicalType::Integer);
    }

    #[test]
    fn a_sum_accumulates_wider_than_it_reads() {
        assert_eq!(
            resolve("sum", &[LogicalType::Integer]).expect("sums").returns,
            LogicalType::HugeInt
        );
        assert_eq!(
            resolve("sum", &[LogicalType::Double]).expect("sums").returns,
            LogicalType::Double
        );
        assert_eq!(
            resolve("sum", &[LogicalType::Float]).expect("sums").returns,
            LogicalType::Double
        );
    }

    #[test]
    fn count_takes_anything_and_returns_a_bigint() {
        let counted = resolve("count", &[LogicalType::Varchar]).expect("counts strings");
        assert_eq!(counted.returns, LogicalType::BigInt);
        assert_eq!(counted.arguments, vec![LogicalType::Varchar], "count does not cast its input");
        assert_eq!(resolve("count_star", &[]).expect("counts rows").returns, LogicalType::BigInt);
    }

    /// `date_part` says bigint whatever it reads and `date_trunc` hands back the type it was given,
    /// which is two answers that one shape cannot give and is why there are two new ones.
    #[test]
    fn a_date_function_fixes_the_part_and_leaves_the_date_alone() {
        let part = resolve("date_part", &[LogicalType::Varchar, LogicalType::Timestamp])
            .expect("a part of a timestamp");
        assert_eq!(part.returns, LogicalType::BigInt);
        assert_eq!(part.arguments, vec![LogicalType::Varchar, LogicalType::Timestamp]);
        let truncated = resolve("date_trunc", &[LogicalType::Varchar, LogicalType::Date])
            .expect("a truncated date");
        assert_eq!(truncated.returns, LogicalType::Date);
        assert_eq!(truncated.arguments, vec![LogicalType::Varchar, LogicalType::Date]);
    }

    /// The two constructors, and the arity with a hole in it. Two arguments is not a `make_date`
    /// upstream has and it is not one here either.
    #[test]
    fn a_date_is_made_from_one_number_or_from_three_and_never_from_two() {
        let day = resolve("make_date", &[LogicalType::Integer]).expect("days since the epoch");
        assert_eq!(day.returns, LogicalType::Date);
        assert_eq!(day.arguments, vec![LogicalType::Integer]);
        let civil =
            resolve("make_date", &vec![LogicalType::BigInt; 3]).expect("a year, a month and a day");
        assert_eq!(civil.returns, LogicalType::Date);
        assert_eq!(civil.arguments, vec![LogicalType::Integer; 3]);
        let error = resolve("make_date", &vec![LogicalType::Integer; 2]).unwrap_err();
        assert_eq!(
            error.message(),
            "No function matches the given name and argument types 'make_date(INTEGER, INTEGER)'. You might need to add explicit type casts."
        );
    }

    #[test]
    fn milliseconds_since_the_epoch_are_a_timestamp() {
        let stamp = resolve("epoch_ms", &[LogicalType::Integer]).expect("a timestamp");
        assert_eq!(stamp.returns, LogicalType::Timestamp);
        assert_eq!(stamp.arguments, vec![LogicalType::BigInt], "the argument widens to read it");
        let error = resolve("epoch_ms", &[LogicalType::Varchar]).unwrap_err();
        assert!(error.message().contains("'epoch_ms(VARCHAR)'"), "{error}");
    }

    /// The part is cast rather than checked, so a part that arrives as something other than a
    /// string is a string by the time the kernel sees it.
    #[test]
    fn the_part_of_a_date_function_is_cast_to_a_string() {
        let resolved = resolve("date_part", &[LogicalType::Integer, LogicalType::Date])
            .expect("the part is cast rather than refused");
        assert_eq!(resolved.arguments, vec![LogicalType::Varchar, LogicalType::Date]);
    }

    /// The group number of an extraction has to arrive as a number, since the kernel tells the
    /// option string from the group by the type rather than by the position.
    #[test]
    fn an_extraction_casts_the_text_and_the_pattern_and_leaves_the_group_alone() {
        let resolved = resolve(
            "regexp_extract",
            &[LogicalType::Varchar, LogicalType::Varchar, LogicalType::Integer],
        )
        .expect("an extraction");
        assert_eq!(resolved.returns, LogicalType::Varchar);
        assert_eq!(
            resolved.arguments,
            vec![LogicalType::Varchar, LogicalType::Varchar, LogicalType::Integer]
        );
        let matched = resolve("regexp_matches", &[LogicalType::Varchar, LogicalType::Varchar])
            .expect("a match");
        assert_eq!(matched.returns, LogicalType::Boolean);
    }

    #[test]
    fn a_name_that_is_not_a_function_says_so_the_way_duckdb_does() {
        let error = resolve("nope", &[]).expect_err("there is no function called nope");
        assert_eq!(
            error.to_string(),
            "Catalog Error: Scalar Function with name nope does not exist!"
        );
    }

    #[test]
    fn the_wrong_number_of_arguments_is_caught() {
        let error = resolve("abs", &[LogicalType::Integer, LogicalType::Integer])
            .expect_err("abs takes one");
        assert!(error.message().contains("No function matches"), "{error}");
    }

    #[test]
    fn arithmetic_on_a_string_is_refused() {
        let error =
            resolve("*", &[LogicalType::Varchar, LogicalType::Integer]).expect_err("no multiply");
        assert!(error.message().contains("No function matches"), "{error}");
    }

    #[test]
    fn a_call_over_nothing_but_nulls_lands_on_a_type_an_executor_can_hold() {
        let resolved =
            resolve("+", &[LogicalType::Null, LogicalType::Null]).expect("null plus null");
        assert_eq!(resolved.returns, LogicalType::Integer);
    }

    #[test]
    fn an_aggregate_is_known_to_be_one() {
        assert_eq!(kind_of("sum"), Some(FunctionKind::Aggregate));
        assert_eq!(kind_of("SUM"), Some(FunctionKind::Aggregate), "names are case insensitive");
        assert_eq!(kind_of("abs"), Some(FunctionKind::Scalar));
        assert_eq!(kind_of("nope"), None);
    }

    /// Two rows for one name would need a rule for which one wins, and there is no such rule yet,
    /// so the table having none is worth asserting rather than remembering.
    #[test]
    fn no_name_appears_twice() {
        let mut names: Vec<&str> = TABLE.iter().map(|entry| entry.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "a name is in the table twice");
    }

    #[test]
    fn every_entry_resolves_at_every_count_it_accepts() {
        for entry in TABLE {
            for count in entry.arity.counts() {
                let ty =
                    if entry.numeric_only { LogicalType::Integer } else { LogicalType::Varchar };
                let arguments = vec![ty; count];
                resolve(entry.name, &arguments).unwrap_or_else(|error| {
                    panic!("{} does not resolve at {count} arguments: {error}", entry.name)
                });
            }
        }
    }

    /// A signature that promotes over its arguments and accepts none of them would reach the
    /// internal error in `promote_all`, which is a message no user should ever see.
    #[test]
    fn nothing_that_promotes_accepts_no_arguments() {
        for entry in TABLE {
            let promotes =
                matches!(entry.shape, Shape::Promoted | Shape::PromotedTo(_) | Shape::Accumulated);
            assert!(
                !(promotes && entry.arity.least() == 0),
                "{} promotes over its arguments and takes none",
                entry.name
            );
        }
    }

    /// `-` is the negation and the subtraction under one name, which is the reason arity is a
    /// range, so it is worth holding to.
    #[test]
    fn minus_is_both_the_negation_and_the_subtraction() {
        assert_eq!(
            resolve("-", &[LogicalType::Integer]).expect("negates").returns,
            LogicalType::Integer
        );
        assert_eq!(
            resolve("-", &[LogicalType::Integer, LogicalType::BigInt]).expect("subtracts").returns,
            LogicalType::BigInt
        );
        assert!(
            resolve("-", &vec![LogicalType::Integer; 3]).is_err(),
            "three is not an arity minus has"
        );
    }
}
