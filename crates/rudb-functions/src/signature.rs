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

use rudb_common::{Error, LogicalType, MAX_DECIMAL_WIDTH, Result};

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
    /// Every argument promotes to one type and the result is that type. `*` and `min`.
    Promoted,
    /// Two arguments, and a decimal product is as wide as both operands together. `*`.
    Multiplied,
    /// Every argument promotes to one type, and a decimal promotion becomes a double instead. `//`.
    ///
    /// `//` is integer division only when there are integers on both sides of it. Upstream answers
    /// `7.5 // 2.5` with the DOUBLE 3.0 and `7.9 // 1.0` with 7.9, so it does not truncate what it
    /// divides once a side is not an integer, and it is `/` under another spelling there. The one
    /// thing it does not do is go to a double the way `/` does whatever it was given, since
    /// `7 // 2` is 3 and an INTEGER on both engines, so it cannot share `/`'s shape. A FLOAT stays
    /// a FLOAT, which was measured, so this is a rule about decimals rather than about width.
    Divided,
    /// Every argument promotes and a decimal result gains a digit for the carry. `+` and `-`.
    ///
    /// Adding two `DECIMAL(18,0)` produces nineteen digits, so a rule that gives the sum eighteen
    /// of them is a rule that raises an overflow on the largest inputs it accepts. Only a decimal
    /// moves: an integer result is the promoted type, since promotion already went to a type that
    /// holds both, and the unary forms of the two operators do not widen because negating a number
    /// cannot carry.
    PromotedWithCarry,
    /// Every argument promotes to one type and the result is fixed. `=` over anything is boolean.
    PromotedTo(Fixed),
    /// Every argument is cast to one fixed type and the result is another. `||` over strings.
    FixedTo(Fixed, Fixed),
    /// Every argument has to be that type already and the result is fixed. `lower`, `length`,
    /// `LIKE`, `chr`.
    ///
    /// The difference from [`Shape::FixedTo`] is the word already. DuckDB refuses `lower(123)`,
    /// `length(DATE '2020-01-01')` and `123 LIKE '1%'` with a binder error naming the overloads it
    /// does have, and it refuses a BLOB as well, so the rule is VARCHAR rather than anything a cast
    /// can reach. `||` is the one string function that really does take anything, since `1 || 'a'`
    /// is `1a` upstream, and it keeps [`Shape::FixedTo`] for that reason.
    ///
    /// The argument type is part of the shape because the same rule holds away from strings.
    /// `chr(col0 INTEGER)` is the only overload upstream has and it refuses `chr(65.9)` and
    /// `chr(65::BIGINT)` rather than narrowing either of them.
    Exact(Fixed, Fixed),
    /// Every argument has to reach that type by widening and the result is fixed. `to_days`.
    ///
    /// Between [`Shape::Exact`] and [`Shape::FixedTo`], and it is where the interval constructors
    /// sit. `to_hours(25)` is an INTEGER reaching a BIGINT and upstream answers it, `to_seconds(1.5)`
    /// is a DECIMAL reaching a DOUBLE and upstream answers that too, and `to_days(1.7)` is a
    /// DECIMAL that would have to lose its fraction to reach an INTEGER, which upstream refuses with
    /// a binder error naming both overloads. So the question is whether promotion gets there and not
    /// whether the type is already right, and not whether a cast exists, since a cast exists for
    /// every one of the three.
    Widened(Fixed, Fixed),
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
    /// The first argument is a string or a list and the result is one piece of it. `array_extract`.
    ///
    /// The index is a BIGINT and nothing is cast to one, which is upstream's rule rather than an
    /// omission here: `[1, 2, 3][1.5]` is a binder error there listing the four overloads, so a
    /// decimal index is refused and not rounded. The bounds of a slice are the other way round,
    /// which is why that is a shape of its own and not this one with a longer arity.
    Extracted,
    /// The first argument is a string or a list, the rest are the bounds, and the result is the first
    /// argument's own type. `array_slice`.
    Sliced,
    /// The first `n` arguments have to be strings already, the rest are indexes, and the result is
    /// fixed. `substring(s, a, b)` and `overlay(s, r, a, b)`.
    ///
    /// An index is a BIGINT and nothing is cast to one, which is the same rule
    /// [`Shape::Extracted`] follows and is upstream's: `substring('abcdef', 2.5, 3)` is a binder
    /// error there listing the two overloads rather than a substring from the second character.
    TextThenIndex(usize, Fixed),
    /// Every argument promotes, and the result is the first argument's own type. `nullif`.
    ///
    /// The promotion is for the comparison and not for the answer, which is what makes this its own
    /// shape: `typeof(nullif(1, 2.5))` is INTEGER upstream and the comparison behind it is still
    /// `1 = 2.5`, so the two arguments have to meet somewhere and the answer has to come back from
    /// where it started. Comparing at the first argument's type instead would round the second one
    /// and answer `nullif(2, 2.5)` with null.
    PromotedToFirst,
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
    Interval,
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
            Self::Interval => LogicalType::Interval,
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
    // two, because the unary forms are the same function and DuckDB names them the same way, and
    // they are the two that carry: a sum of two decimals needs a digit the operands do not have.
    number("+", Arity::between(1, 2), Shape::PromotedWithCarry),
    number("-", Arity::between(1, 2), Shape::PromotedWithCarry),
    number("*", Arity::exactly(2), Shape::Multiplied),
    number("%", Arity::exactly(2), Shape::Promoted),
    // `/` is the exception and it is DuckDB's exception too: `7 / 2` is 3.5 and not 3, so the
    // result is a double whatever went in, and `//` is the operator that keeps the integer.
    number("/", Arity::exactly(2), Shape::PromotedTo(Fixed::Double)),
    number("//", Arity::exactly(2), Shape::Divided),
    number("abs", Arity::exactly(1), Shape::Promoted),
    // Strings.
    // `||` is the one that takes anything and turns it into a string, which is why it is a
    // `FixedTo` and everything under it is a `Text`. `1 || 'a'` is `1a` upstream.
    Entry {
        name: "||",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::FixedTo(Fixed::Varchar, Fixed::Varchar),
        numeric_only: false,
    },
    text("lower", Arity::exactly(1), Fixed::Varchar),
    text("upper", Arity::exactly(1), Fixed::Varchar),
    text("length", Arity::exactly(1), Fixed::BigInt),
    // `strlen` is bytes where `length` is characters, and it is a separate row rather than an alias
    // for that reason. `strlen('héllo')` is 6 upstream and `length('héllo')` is 5. It is here
    // because DuckDB's own ClickBench entry writes `AVG(STRLEN(URL))` in query 28, so a rudb
    // that has only `length` cannot run that board at all without the SQL being changed, and the
    // whole point of the comparison is that it is not changed.
    text("strlen", Arity::exactly(1), Fixed::BigInt),
    // The four SQL string functions that have a grammar rule of their own, plus the aliases upstream
    // answers the same call with. Each alias is a row rather than a pointer at one, because the
    // column a query gets back is named after the name that was written: `substr('abcdef', 2)` comes
    // back as `substr('abcdef', 2)` upstream and not as a substring of anything.
    Entry {
        name: "substring",
        kind: FunctionKind::Scalar,
        arity: Arity::one_of(&[2, 3]),
        shape: Shape::TextThenIndex(1, Fixed::Varchar),
        numeric_only: false,
    },
    Entry {
        name: "substr",
        kind: FunctionKind::Scalar,
        arity: Arity::one_of(&[2, 3]),
        shape: Shape::TextThenIndex(1, Fixed::Varchar),
        numeric_only: false,
    },
    Entry {
        name: "overlay",
        kind: FunctionKind::Scalar,
        arity: Arity::one_of(&[3, 4]),
        shape: Shape::TextThenIndex(2, Fixed::Varchar),
        numeric_only: false,
    },
    // `left` and `right` count characters and clamp, and a negative count is a count from the other
    // end rather than an error, so `left('abc', -1)` is `ab`. Both are declared
    // `(VARCHAR, BIGINT)` upstream and neither casts its count, which is what
    // [`Shape::TextThenIndex`] already says.
    Entry {
        name: "left",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::TextThenIndex(1, Fixed::Varchar),
        numeric_only: false,
    },
    Entry {
        name: "right",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::TextThenIndex(1, Fixed::Varchar),
        numeric_only: false,
    },
    text("replace", Arity::exactly(3), Fixed::Varchar),
    // `chr` is a code point and not a byte, so `chr(233)` is one character and not two bytes of
    // something else. Its one overload upstream takes an INTEGER and it narrows nothing to reach
    // it: `chr(65::BIGINT)` and `chr(65.9)` are both binder errors there.
    Entry {
        name: "chr",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::Exact(Fixed::Integer, Fixed::Varchar),
        numeric_only: false,
    },
    // `concat` takes anything, joins it and drops the nulls instead of propagating them, so
    // `concat('a', 1, NULL)` is `a1`. That last part is what makes it a third exception to the null
    // in null out rule, next to `coalesce` and `nullif`, and it is the only one of the three that is
    // an ordinary function rather than sugar for something else.
    Entry {
        name: "concat",
        kind: FunctionKind::Scalar,
        arity: Arity::at_least(1),
        shape: Shape::FixedTo(Fixed::Varchar, Fixed::Varchar),
        numeric_only: false,
    },
    text("position", Arity::exactly(2), Fixed::BigInt),
    text("strpos", Arity::exactly(2), Fixed::BigInt),
    text("instr", Arity::exactly(2), Fixed::BigInt),
    text("trim", Arity::between(1, 2), Fixed::Varchar),
    text("ltrim", Arity::between(1, 2), Fixed::Varchar),
    text("rtrim", Arity::between(1, 2), Fixed::Varchar),
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
    // `nullif(a, b)` is a macro upstream, `CASE WHEN a = b THEN NULL ELSE a END`, and it is a
    // function here because the column it produces is named after the call rather than after the
    // expansion. What that costs is the message for the wrong number of arguments: upstream's is a
    // binder error about a macro listing `"nullif"(a, b)` under `Candidate macros:`, and the one
    // below is the ordinary sentence about a function. Both refuse, and the reachable spelling of the
    // mistake is the quoted `"nullif"(1)`, since the grammar has NULLIF with exactly two arguments
    // and refuses any other count before the binder sees it.
    Entry {
        name: "nullif",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::PromotedToFirst,
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
    // The thirteen ways to build an interval out of a count of one unit, which is what
    // `INTERVAL 1 DAY` is once the transformer has rewritten it, and `to_days(1)` written out by
    // hand is the same call. Eleven of them count whole units and the two that can carry a fraction
    // take a DOUBLE, so `INTERVAL 2.7 SECOND` is two and seven tenths of a second while
    // `INTERVAL 1.5 DAY` is one day.
    //
    // Upstream declares the eight that land in months or days twice, once over an INTEGER and once
    // over a BIGINT, and only the first is here, for the reason the head of this table gives: one
    // row per name, and a second row needs a rule for which one wins. The rewrite always casts to
    // the width the row below wants, so the literal is unaffected and what is missing is a
    // handwritten `to_days(3::BIGINT)`, which is refused here and answered there. The three that
    // land in microseconds have the BIGINT overload and no INTEGER one, so those rows are exact.
    built("to_years", Fixed::Integer),
    built("to_months", Fixed::Integer),
    built("to_quarters", Fixed::Integer),
    built("to_decades", Fixed::Integer),
    built("to_centuries", Fixed::Integer),
    built("to_millennia", Fixed::Integer),
    built("to_days", Fixed::Integer),
    built("to_weeks", Fixed::Integer),
    built("to_hours", Fixed::BigInt),
    built("to_minutes", Fixed::BigInt),
    built("to_microseconds", Fixed::BigInt),
    built("to_seconds", Fixed::Double),
    built("to_milliseconds", Fixed::Double),
    // `trunc` is here because the interval rewrite writes it, and it is an ordinary function anybody
    // can write as well. Upstream has twenty six overloads and every one of them gives back the type
    // it was handed, which is what `Shape::Promoted` says over one argument. The exception is the
    // decimal, where upstream drops the scale and gives `DECIMAL(2,0)` for `trunc(1.7)` and this
    // keeps `DECIMAL(2,1)` holding 1.0, since no shape in this table drops a scale.
    number("trunc", Arity::exactly(1), Shape::Promoted),
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
    // Subscripting. A bracket is one of these two calls by the time the transformer is done with it,
    // `x[2]` being `array_extract(x, 2)` and `x[1:2]` being `array_slice(x, 1, 2)`, which is what
    // DuckDB's own transformer writes as well. Both take a string or a list and give back a piece of
    // the same thing, so neither one can name its return type here: it is read off the argument.
    Entry {
        name: "array_extract",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::Extracted,
        numeric_only: false,
    },
    // Three arguments is a range and four is a range with a step. There is no two argument form,
    // which is why a slice cannot share the row above: `array_slice([1, 2, 3], 1)` is an arity error
    // upstream rather than the whole list from the first element on.
    Entry {
        name: "array_slice",
        kind: FunctionKind::Scalar,
        arity: Arity::between(3, 4),
        shape: Shape::Sliced,
        numeric_only: false,
    },
    // The type of an expression, as a string. Nothing is cast and nothing runs: the binder folds
    // this to the name of the type it just decided, so the argument is only ever looked at and the
    // executor never sees the call.
    Entry {
        name: "typeof",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::AnyTo(Fixed::Varchar),
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
        shape: Shape::Exact(Fixed::Varchar, returns),
        numeric_only: false,
    }
}

/// An interval constructor, which takes one count of one unit and gives back an interval.
const fn built(name: &'static str, count: Fixed) -> Entry {
    Entry {
        name,
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::Widened(count, Fixed::Interval),
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
        return Err(no_match(entry.name, arguments));
    }
    if let Some((cast_to, returns)) = temporal(entry.name, arguments) {
        return Ok(Resolved { name: entry.name, kind: entry.kind, arguments: cast_to, returns });
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
        Shape::Multiplied => {
            let common = promote_all(name, arguments)?;
            match product(arguments)? {
                // Each side keeps its own scale and takes the answer's width, so the two runs are
                // the same physical type and the unscaled values multiply into the answer with no
                // rescaling anywhere. That is what the decimal loop in rudb-kernels expects.
                Some(LogicalType::Decimal { width, scale }) => {
                    let cast_to = arguments
                        .iter()
                        .map(|ty| match ty.decimal_shape() {
                            Some((_, held)) => LogicalType::Decimal { width, scale: held },
                            None => ty.clone(),
                        })
                        .collect();
                    (cast_to, LogicalType::Decimal { width, scale })
                }
                _ => (vec![common.clone(); arguments.len()], common),
            }
        }
        Shape::Divided => {
            let common = promote_all(name, arguments)?;
            // The cast goes with the answer rather than being left where promotion put it, because
            // a decimal run divided as a decimal and then widened to a double is not the same
            // number as the same pair of values divided as doubles.
            let returns = match common {
                LogicalType::Decimal { .. } => LogicalType::Double,
                other => other,
            };
            (vec![returns.clone(); arguments.len()], returns)
        }
        Shape::PromotedWithCarry => {
            let common = promote_all(name, arguments)?;
            // One argument is a negation or a unary plus, and neither one can carry. Negating the
            // smallest value of a type is the exception and it is not a signature's to take, since
            // the type of the answer depends on the value: the constant folder widens that one
            // value by a step, per #264, and the signature says the same thing here as upstream's
            // does.
            let returns = if arguments.len() > 1 { carrying(common) } else { common };
            (vec![returns.clone(); arguments.len()], returns)
        }
        Shape::PromotedTo(fixed) => {
            let common = promote_all(name, arguments)?;
            (vec![common; arguments.len()], fixed.ty())
        }
        Shape::FixedTo(argument, result) => (vec![argument.ty(); arguments.len()], result.ty()),
        Shape::Exact(argument, result) => {
            let wanted = argument.ty();
            for ty in arguments {
                // An untyped null is accepted the way it is everywhere else here. DuckDB answers
                // `length(NULL)` with NULL rather than refusing it, because a null has no type to
                // pick an overload with and every overload would return null anyway.
                if *ty != wanted && *ty != LogicalType::Null {
                    return Err(no_match(entry.name, arguments));
                }
            }
            (vec![wanted; arguments.len()], result.ty())
        }
        Shape::Widened(argument, result) => {
            let wanted = argument.ty();
            for ty in arguments {
                // A null is accepted here for the reason it is accepted above, and it is the only
                // type that does not have to promote anywhere, since it has nothing to promote.
                if *ty != LogicalType::Null && ty.promote(&wanted).as_ref() != Some(&wanted) {
                    return Err(no_match(entry.name, arguments));
                }
            }
            (vec![wanted; arguments.len()], result.ty())
        }
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
        Shape::Extracted => {
            let target = &arguments[0];
            let index = &arguments[1];
            let Some(element) = element_of(target) else {
                return Err(no_match(entry.name, arguments));
            };
            if !index.is_integer() && *index != LogicalType::Null {
                return Err(no_match(entry.name, arguments));
            }
            (vec![target.clone(), LogicalType::BigInt], element)
        }
        Shape::Sliced => {
            let target = &arguments[0];
            if element_of(target).is_none() {
                // Upstream's own sentence, shouted, and it is the same sentence whichever of the two
                // spellings the call was written with.
                return Err(Error::binder("ARRAY_SLICE can only operate on LISTs and VARCHARs"));
            }
            // A step is declared BIGINT and so it is not cast to one either, while the two bounds
            // are declared ANY and are: `array_slice([1, 2, 3], 1.5, 2)` is `[2]` upstream, rounded,
            // and `array_slice([1, 2, 3], 1, 2, 1.5)` is a binder error.
            if let Some(step) = arguments.get(3) {
                if !step.is_integer() && *step != LogicalType::Null {
                    return Err(no_match(entry.name, arguments));
                }
            }
            let mut cast_to = vec![LogicalType::BigInt; arguments.len()];
            cast_to[0] = target.clone();
            (cast_to, target.clone())
        }
        Shape::TextThenIndex(count, result) => {
            let (text, indexes) = arguments.split_at(count.min(arguments.len()));
            for ty in text {
                if *ty != LogicalType::Varchar && *ty != LogicalType::Null {
                    return Err(no_match(entry.name, arguments));
                }
            }
            for ty in indexes {
                if !ty.is_integer() && *ty != LogicalType::Null {
                    return Err(no_match(entry.name, arguments));
                }
            }
            let mut cast_to = vec![LogicalType::BigInt; arguments.len()];
            for slot in &mut cast_to[..text.len()] {
                *slot = LogicalType::Varchar;
            }
            (cast_to, result.ty())
        }
        Shape::PromotedToFirst => {
            let common = promote_all(name, arguments)?;
            // An untyped null keeps nothing to hand back, so it takes the promoted type the way
            // every other shape here does. Upstream says NULL for `typeof(nullif(NULL, NULL))`
            // because it has a type for a null literal and this engine does not, which is #244.
            let first = &arguments[0];
            let returns = if *first == LogicalType::Null { common.clone() } else { first.clone() };
            (vec![common; arguments.len()], returns)
        }
    };
    Ok(Resolved { name: entry.name, kind: entry.kind, arguments: cast_to, returns })
}

/// What the arithmetic operators return when a date, a time, a timestamp or an interval is one of
/// the arguments.
///
/// The one place in this file where the argument types pick the overload rather than the name
/// picking one shape. The table above says a name has exactly one shape and that a second row for
/// a name needs a rule for which one wins, and this is the rule: a date, a time, a timestamp or an
/// interval next to one of those, or next to a number, is temporal arithmetic, and everything else
/// is the numeric row. The answer is the types to cast the arguments to and the type that comes
/// back.
///
/// A date plus an interval is a timestamp and not a date, because the interval carries a time of
/// day. A time plus an interval is a time, since the months and the days have nowhere to go and it
/// wraps at midnight. Taking a date off an interval is not a thing on either engine, so only the
/// commuted addition is here.
///
/// Two intervals add and subtract field by field, and a number scales one, in either order for the
/// multiplication and with the interval on the left for the division.
///
/// The multiplication has two overloads of its own and the difference between them shows. A whole
/// number goes in as a `BIGINT` and multiplies the three fields as they are, and everything else
/// goes in as a `DOUBLE` and moves what is left over on a field down to the next one. `HUGEINT` and
/// `UBIGINT` take the double as well, since neither of them fits a `BIGINT` to begin with. Dividing
/// has only the double, which is why an integer count divided into an interval reports its division
/// by zero as `0.0`.
///
/// A plain number next to a date is a count of days and the answer stays a date, which is the one
/// shape here that does not become a timestamp. The count is an `INTEGER` and nothing wider, so a
/// `BIGINT` next to a date has no overload to reach at all, and taking a date off a number is not a
/// thing. One date taken off another is a count of days as a `BIGINT` and one timestamp taken off
/// another is an interval, and a date on either side of that subtraction becomes a timestamp first.
/// A date plus a time is the timestamp they name together, in either order, and taking a time off a
/// date is refused upstream.
///
/// An untyped null next to a date is the count of days and next to a timestamp is the interval,
/// which is measured rather than picked: `typeof(DATE '2020-01-01' + NULL)` is `DATE` and
/// `typeof(TIMESTAMP '2020-01-01' - NULL)` is `TIMESTAMP`. A null next to a time or next to an
/// interval is ambiguous upstream and refused, which we refuse too, with the wrong sentence for now
/// because the sentence for an ambiguous call is #395.
fn temporal(name: &str, arguments: &[LogicalType]) -> Option<(Vec<LogicalType>, LogicalType)> {
    use LogicalType::{
        BigInt, Date, Double, HugeInt, Integer, Interval, Null, SmallInt, Time, Timestamp, TinyInt,
        UBigInt, UHugeInt, USmallInt, UTinyInt,
    };
    let kept = |returns| Some((arguments.to_vec(), returns));
    // A null literal has no type yet, so it counts as the number and the cast to a double is what
    // turns the whole call into a null.
    let number = |ty: &LogicalType| ty.is_numeric() || *ty == Null;
    let counted = |ty: &LogicalType| ty.is_integer() && !matches!(ty, HugeInt | UHugeInt | UBigInt);
    // The days a date moves by are an `INTEGER`, so this is the set of types that widen into one.
    let days =
        |ty: &LogicalType| matches!(ty, TinyInt | SmallInt | Integer | UTinyInt | USmallInt | Null);
    match (name, arguments) {
        ("-", [Interval]) => kept(Interval),
        ("+" | "-", [Date | Timestamp, Interval]) | ("+", [Interval, Date | Timestamp]) => {
            kept(Timestamp)
        }
        ("+" | "-", [Time, Interval]) | ("+", [Interval, Time]) => kept(Time),
        ("+" | "-", [Interval, Interval]) => kept(Interval),
        ("-", [Date, Date]) => kept(BigInt),
        ("-", [Timestamp, Timestamp]) => kept(Interval),
        ("-", [Date, Timestamp] | [Timestamp, Date]) => {
            Some((vec![Timestamp, Timestamp], Interval))
        }
        ("+", [Date, Time] | [Time, Date]) => kept(Timestamp),
        ("+" | "-", [Date, count]) if days(count) => Some((vec![Date, Integer], Date)),
        ("+", [count, Date]) if days(count) => Some((vec![Integer, Date], Date)),
        ("+" | "-", [Timestamp, Null]) | ("+", [Null, Timestamp]) => kept(Timestamp),
        ("*", [Interval, count]) if counted(count) => Some((vec![Interval, BigInt], Interval)),
        ("*", [count, Interval]) if counted(count) => Some((vec![BigInt, Interval], Interval)),
        ("*" | "/", [Interval, scale]) if number(scale) => Some((vec![Interval, Double], Interval)),
        ("*", [scale, Interval]) if number(scale) => Some((vec![Double, Interval], Interval)),
        _ => None,
    }
}

/// The error for a call that names a real function and does not fit any of its overloads.
///
/// The sentence is DuckDB's, and so is the block under it when there is one. A message that says a
/// call does not match without saying what would match is a message that sends somebody to the
/// documentation, and the whole argument for copying the reference's errors is that a program
/// written against one engine should not have to be debugged differently against the other.
///
/// The trailing newline is the reference's too. Its message ends after the last candidate with a
/// line break, which is visible as the second blank line before the shell prints the offending SQL.
fn no_match(name: &str, arguments: &[LogicalType]) -> Error {
    let types = arguments.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    let mut message = format!(
        "No function matches the given name and argument types '{name}({types})'. You might need to add explicit type casts."
    );
    if let Some((_, overloads)) = CANDIDATES.iter().find(|(entry, _)| *entry == name) {
        message.push_str("\n\tCandidate functions:");
        for overload in *overloads {
            message.push_str("\n\t");
            message.push_str(overload);
        }
        message.push('\n');
    }
    Error::binder(message)
}

/// What the reference prints under `Candidate functions:`, per function, byte for byte.
///
/// Copied off the pinned binary rather than generated from [`TABLE`], because it is not derivable
/// from what rudb has. The parameters are called `col0` and `col1` for some functions and `string`,
/// `regex` and a quoted `"options"` for others, an operator is quoted where a plain name is not, and
/// `length` lists three overloads of which rudb has one. That last one is the argument for copying
/// rather than deriving: the list is what DuckDB accepts, rudb is meant to accept the same, and a
/// list that shrank to what is built today would have to be edited every time a gap closes.
///
/// A name missing from here gets the sentence with no block under it, which is what every function
/// outside the string family does today.
const CANDIDATES: &[(&str, &[&str])] = &[
    ("lower", &["lower(col0 VARCHAR) -> VARCHAR"]),
    ("upper", &["upper(col0 VARCHAR) -> VARCHAR"]),
    (
        "length",
        &[
            "length(col0 VARCHAR) -> BIGINT",
            "length(col0 BIT) -> BIGINT",
            "length(col0 ANY[]) -> BIGINT",
        ],
    ),
    ("strlen", &["strlen(col0 VARCHAR) -> BIGINT"]),
    ("chr", &["chr(col0 INTEGER) -> VARCHAR"]),
    ("left", &["\"left\"(col0 VARCHAR, col1 BIGINT) -> VARCHAR"]),
    ("right", &["\"right\"(col0 VARCHAR, col1 BIGINT) -> VARCHAR"]),
    ("replace", &["\"replace\"(col0 VARCHAR, col1 VARCHAR, col2 VARCHAR) -> VARCHAR"]),
    // The one overload upstream prints with a repeated parameter in it, which is how it writes a
    // variadic. Reachable with no arguments at all, since the grammar has nothing to say about the
    // count of an ordinary call.
    ("concat", &["concat(col0 ANY, [ANY...]) -> ANY"]),
    (
        "substring",
        &[
            "\"substring\"(col0 VARCHAR, col1 BIGINT, col2 BIGINT) -> VARCHAR",
            "\"substring\"(col0 VARCHAR, col1 BIGINT) -> VARCHAR",
        ],
    ),
    (
        "substr",
        &[
            "substr(col0 VARCHAR, col1 BIGINT, col2 BIGINT) -> VARCHAR",
            "substr(col0 VARCHAR, col1 BIGINT) -> VARCHAR",
        ],
    ),
    (
        "overlay",
        &[
            "\"overlay\"(col0 VARCHAR, col1 VARCHAR, col2 BIGINT) -> VARCHAR",
            "\"overlay\"(col0 VARCHAR, col1 VARCHAR, col2 BIGINT, col3 BIGINT) -> VARCHAR",
        ],
    ),
    ("position", &["\"position\"(col0 VARCHAR, col1 VARCHAR) -> BIGINT"]),
    ("strpos", &["strpos(col0 VARCHAR, col1 VARCHAR) -> BIGINT"]),
    ("instr", &["instr(col0 VARCHAR, col1 VARCHAR) -> BIGINT"]),
    (
        "trim",
        &["\"trim\"(col0 VARCHAR) -> VARCHAR", "\"trim\"(col0 VARCHAR, col1 VARCHAR) -> VARCHAR"],
    ),
    ("ltrim", &["ltrim(col0 VARCHAR) -> VARCHAR", "ltrim(col0 VARCHAR, col1 VARCHAR) -> VARCHAR"]),
    ("rtrim", &["rtrim(col0 VARCHAR) -> VARCHAR", "rtrim(col0 VARCHAR, col1 VARCHAR) -> VARCHAR"]),
    ("~~", &["\"~~\"(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN"]),
    ("!~~", &["\"!~~\"(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN"]),
    ("~~*", &["\"~~*\"(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN"]),
    ("!~~*", &["\"!~~*\"(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN"]),
    (
        "regexp_replace",
        &[
            "regexp_replace(string VARCHAR, regex VARCHAR, replacement VARCHAR) -> VARCHAR",
            "regexp_replace(string VARCHAR, regex VARCHAR, replacement VARCHAR, \"options\" VARCHAR) -> VARCHAR",
        ],
    ),
    (
        "regexp_matches",
        &[
            "regexp_matches(string VARCHAR, regex VARCHAR) -> BOOLEAN",
            "regexp_matches(string VARCHAR, regex VARCHAR, \"options\" VARCHAR) -> BOOLEAN",
        ],
    ),
    (
        "regexp_full_match",
        &[
            "regexp_full_match(string VARCHAR, regex VARCHAR) -> BOOLEAN",
            "regexp_full_match(string VARCHAR, regex VARCHAR, \"options\" VARCHAR) -> BOOLEAN",
        ],
    ),
    // Both overloads of each interval constructor, including the BIGINT one this engine does not
    // have a row for, because the list is what DuckDB accepts and somebody reading it is being told
    // what to write rather than what is built here.
    ("to_years", &["to_years(col0 INTEGER) -> INTERVAL", "to_years(col0 BIGINT) -> INTERVAL"]),
    ("to_months", &["to_months(col0 INTEGER) -> INTERVAL", "to_months(col0 BIGINT) -> INTERVAL"]),
    (
        "to_quarters",
        &["to_quarters(col0 INTEGER) -> INTERVAL", "to_quarters(col0 BIGINT) -> INTERVAL"],
    ),
    (
        "to_decades",
        &["to_decades(col0 INTEGER) -> INTERVAL", "to_decades(col0 BIGINT) -> INTERVAL"],
    ),
    (
        "to_centuries",
        &["to_centuries(col0 INTEGER) -> INTERVAL", "to_centuries(col0 BIGINT) -> INTERVAL"],
    ),
    (
        "to_millennia",
        &["to_millennia(col0 INTEGER) -> INTERVAL", "to_millennia(col0 BIGINT) -> INTERVAL"],
    ),
    ("to_days", &["to_days(col0 INTEGER) -> INTERVAL", "to_days(col0 BIGINT) -> INTERVAL"]),
    ("to_weeks", &["to_weeks(col0 INTEGER) -> INTERVAL", "to_weeks(col0 BIGINT) -> INTERVAL"]),
    // The five that have one overload each, which is why they are not in the pattern above. The
    // three that land in microseconds are declared over a BIGINT and never over an INTEGER, since
    // an hour of INTEGER hours does not fit the field anyway.
    ("to_hours", &["to_hours(col0 BIGINT) -> INTERVAL"]),
    ("to_minutes", &["to_minutes(col0 BIGINT) -> INTERVAL"]),
    ("to_microseconds", &["to_microseconds(col0 BIGINT) -> INTERVAL"]),
    ("to_seconds", &["to_seconds(col0 DOUBLE) -> INTERVAL"]),
    ("to_milliseconds", &["to_milliseconds(col0 DOUBLE) -> INTERVAL"]),
    // Four overloads of which this engine has two. The STRUCT one is `x.y`, which the transformer
    // writes as `struct_extract`, and a TUPLE is the positional half of the same idea.
    (
        "array_extract",
        &[
            "array_extract(\"array\" T[], \"index\" BIGINT) -> T",
            "array_extract(col0 VARCHAR, col1 BIGINT) -> VARCHAR",
            "array_extract(\"struct\" STRUCT, \"key\" VARCHAR) -> ANY",
            "array_extract(\"tuple\" TUPLE, \"index\" BIGINT) -> ANY",
        ],
    ),
    (
        "array_slice",
        &[
            "array_slice(col0 ANY, col1 ANY, col2 ANY) -> ANY",
            "array_slice(col0 ANY, col1 ANY, col2 ANY, col3 BIGINT) -> ANY",
        ],
    ),
    ("typeof", &["typeof(col0 ANY) -> VARCHAR"]),
];

/// What one element of a subscripted value is, or `None` for a value that cannot be subscripted.
///
/// A string is subscripted by character and a character is a string, so `'abcdef'[2]` is a VARCHAR
/// and not a type of its own. An untyped null takes the VARCHAR overload, which was measured:
/// `typeof(array_extract(NULL, 1))` is VARCHAR on the pinned binary while
/// `typeof(array_slice(NULL, 1, 2))` is NULL, so the null goes here and the slice keeps the type it
/// was handed.
///
/// A STRUCT is subscripted by name rather than by position and is not one of these. `x.y` is
/// `struct_extract(x, 'y')` by the time it leaves the transformer, which is a function this table
/// does not have yet, so that call fails with the name of the function it is missing.
fn element_of(ty: &LogicalType) -> Option<LogicalType> {
    match ty {
        LogicalType::Varchar | LogicalType::Null => Some(LogicalType::Varchar),
        LogicalType::List(element) | LogicalType::Array(element, _) => Some((**element).clone()),
        _ => None,
    }
}

/// The cast list for a shape that fixes the leading arguments and leaves the others as they are.
fn leading(count: usize, first: Fixed, arguments: &[LogicalType]) -> Vec<LogicalType> {
    let mut cast_to = arguments.to_vec();
    for head in cast_to.iter_mut().take(count) {
        *head = first.ty();
    }
    cast_to
}

/// The type of a decimal product, or `None` when no decimal is involved and promotion decides.
///
/// A product of `DECIMAL(a,b)` and `DECIMAL(c,d)` needs `a + c` digits with `b + d` after the
/// point, because the largest pair of inputs multiplies to exactly that, and an integer counts as
/// the decimal that holds it. The rest is where upstream stops widening, and both of the places it
/// stops were read off `v2.0.0-dev84237` across a grid of seventy two pairs rather than reasoned
/// about:
///
/// A product of two operands that each fit in sixty four bits is kept there when it can be. So
/// `DECIMAL(10,0) * DECIMAL(10,0)` is `DECIMAL(18,0)` rather than `DECIMAL(20,0)`, which is a type
/// that cannot hold every product of its own inputs and raises an overflow on the ones it cannot,
/// and `DECIMAL(18,17) * DECIMAL(10,0)` is `DECIMAL(18,17)`. It is kept there only while a digit is
/// left in front of the point, which is why `DECIMAL(10,9) * DECIMAL(10,9)` is `DECIMAL(20,18)` and
/// not `DECIMAL(18,18)`: at eighteen decimal places there is no room for the integer part, so the
/// answer moves to the wider representation instead.
///
/// Past that, the width stops at the widest decimal there is and the scale does not, because a
/// scale that had to shrink would be an answer with digits missing from the end of it rather than a
/// narrower one. A scale of more than thirty eight is refused at bind time with upstream's own
/// sentence, since there is no type to put the answer in.
fn product(arguments: &[LogicalType]) -> Result<Option<LogicalType>> {
    let mut decimals = false;
    let (mut width, mut scale, mut widest) = (0u8, 0u8, 0u8);
    for ty in arguments {
        decimals |= matches!(ty, LogicalType::Decimal { .. });
        let Some((one, held)) = ty.decimal_shape() else { return Ok(None) };
        width = width.saturating_add(one);
        scale = scale.saturating_add(held);
        widest = widest.max(one);
    }
    if !decimals {
        return Ok(None);
    }
    if scale > MAX_DECIMAL_WIDTH {
        return Err(Error::out_of_range(format!(
            "Needed scale {scale} to accurately represent the multiplication result, but this is out of range of the DECIMAL type. Max scale is {MAX_DECIMAL_WIDTH}; could not perform an accurate multiplication. Either add a cast to DOUBLE, or add an explicit cast to a decimal with a lower scale."
        )));
    }
    if widest <= WIDEST_SIXTY_FOUR_BIT
        && width > WIDEST_SIXTY_FOUR_BIT
        && scale < WIDEST_SIXTY_FOUR_BIT
    {
        width = WIDEST_SIXTY_FOUR_BIT;
    }
    Ok(Some(LogicalType::Decimal { width: width.min(MAX_DECIMAL_WIDTH), scale }))
}

/// The widest decimal that is still eight bytes a value, which is where a product stops widening.
const WIDEST_SIXTY_FOUR_BIT: u8 = 18;

/// The type an addition or a subtraction produces from what its operands promote to.
///
/// A decimal gains the one digit an addition can carry into and everything else is unchanged. At
/// the maximum width there is nowhere left to widen into, so the type stays where it is and the
/// overflow is raised on the row that overflows rather than on every query that could.
///
/// Measured on `v2.0.0-dev84237`, which is where each of these numbers comes from:
/// `DECIMAL(18,0) + DECIMAL(18,0)` is `DECIMAL(19,0)`, `DECIMAL(38,0) + DECIMAL(38,0)` is
/// `DECIMAL(38,0)`, `2.0 + 1::INTEGER` is `DECIMAL(12,1)` and `DECIMAL(18,0) - DECIMAL(4,2)` is
/// `DECIMAL(21,2)`. A modulo, a negation and `abs` do not widen and keep [`Shape::Promoted`] for
/// that reason, and a product widens by a rule of its own, which is [`product`].
fn carrying(common: LogicalType) -> LogicalType {
    match common {
        LogicalType::Decimal { width, scale } if width < MAX_DECIMAL_WIDTH => {
            LogicalType::Decimal { width: width + 1, scale }
        }
        other => other,
    }
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
    let name = canonical(name);
    TABLE.iter().find(|entry| entry.name.eq_ignore_ascii_case(name))
}

/// The name a function is in [`TABLE`] under, which is its own name unless it is an alias.
///
/// Aliases are resolved here rather than by a second row in the table, so that [`Resolved::name`]
/// is always the canonical name and the plan, the executor and every kernel below it see one name
/// per function. A kernel that had to know `len` is `length` would be a kernel with a second place
/// for the two to drift apart.
///
/// The list is DuckDB's, read off `duckdb_functions()` where `alias_of` is set, and it is only ever
/// as long as the table it points into. There is no point aliasing a name onto a function this
/// engine does not have yet, because the error would move from a missing function to a missing
/// function under a different name.
fn canonical(name: &str) -> &str {
    ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map_or(name, |(_, real)| *real)
}

/// Every other name DuckDB accepts for a function already in [`TABLE`].
///
/// `strlen` is deliberately not here. Upstream counts bytes with it and characters with `length`,
/// so it is a different function and it has a row of its own.
/// The three subscript spellings point the way the transformer writes them rather than the way
/// `duckdb_functions()` has them. Upstream is `array_slice` aliased onto `list_slice`, and
/// `array_extract` and `list_extract` are two functions there rather than one, differing in the
/// overloads they carry for a STRUCT and a TUPLE. Neither of those is here, so they are one function
/// here, and the name it is under is the one a bracket produces, which is what keeps the message a
/// bracket produces word for word the reference's.
///
/// What that costs is the same thing every row below costs: the message names the canonical spelling
/// and not the written one, so `list_slice(1, 2, 3)` says `array_slice` here where upstream says
/// `list_slice`, exactly as `len(1)` says `length`.
const ALIASES: &[(&str, &str)] = &[
    ("len", "length"),
    ("char_length", "length"),
    ("character_length", "length"),
    ("lcase", "lower"),
    ("ucase", "upper"),
    ("mean", "avg"),
    ("list_extract", "array_extract"),
    ("list_element", "array_extract"),
    ("list_slice", "array_slice"),
];

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

    /// Every decimal sum in here was read off `v2.0.0-dev84237` with `typeof`, per #243.
    ///
    /// The last one is the case the rule exists for. Two `DECIMAL(18,0)` hold numbers that add to
    /// nineteen digits, and a result type of eighteen means the largest pair of inputs the operator
    /// accepts is a pair it cannot answer.
    #[test]
    fn a_decimal_sum_is_a_digit_wider_than_what_its_operands_promote_to() {
        let decimal = |width, scale| LogicalType::Decimal { width, scale };
        let sum = |left: LogicalType, right: LogicalType| {
            resolve("+", &[left, right]).expect("adds").returns
        };
        assert_eq!(sum(decimal(18, 0), decimal(18, 0)), decimal(19, 0));
        assert_eq!(sum(decimal(2, 1), LogicalType::Integer), decimal(12, 1));
        assert_eq!(sum(decimal(18, 0), decimal(4, 2)), decimal(21, 2));
        assert_eq!(sum(decimal(4, 2), LogicalType::BigInt), decimal(22, 2));
        assert_eq!(sum(decimal(4, 2), LogicalType::UBigInt), decimal(23, 2));
        assert_eq!(sum(decimal(4, 2), LogicalType::HugeInt), decimal(38, 2));
        // Both sides are cast to the answer's type, because the kernel underneath adds two runs of
        // the same width and the carry digit can move the answer into a wider one.
        let resolved = resolve("-", &[decimal(18, 0), decimal(18, 0)]).expect("subtracts");
        assert_eq!(resolved.arguments, vec![decimal(19, 0), decimal(19, 0)]);
    }

    /// At the maximum width there is nowhere to carry into, so the type stops and the row raises.
    #[test]
    fn a_decimal_sum_at_the_widest_decimal_stays_there() {
        let widest = LogicalType::Decimal { width: MAX_DECIMAL_WIDTH, scale: 0 };
        let resolved = resolve("+", &[widest.clone(), widest.clone()]).expect("adds");
        assert_eq!(resolved.returns, widest);
    }

    /// Negation cannot carry, and neither can anything that is not an addition.
    ///
    /// `-1.50` is a `DECIMAL(4,2)` upstream and so is `abs(-1.50)`, and `5.50 % 3` is a
    /// `DECIMAL(12,2)`, which is the promotion with no digit added to it.
    #[test]
    fn nothing_but_a_two_sided_addition_gains_a_digit() {
        let decimal = |width, scale| LogicalType::Decimal { width, scale };
        assert_eq!(resolve("-", &[decimal(4, 2)]).expect("negates").returns, decimal(4, 2));
        assert_eq!(resolve("+", &[decimal(4, 2)]).expect("is unary plus").returns, decimal(4, 2));
        assert_eq!(resolve("abs", &[decimal(4, 2)]).expect("has a size").returns, decimal(4, 2));
        assert_eq!(
            resolve("%", &[decimal(4, 2), LogicalType::Integer]).expect("divides").returns,
            decimal(12, 2)
        );
    }

    /// Every product in here was read off `v2.0.0-dev84237` with `typeof`, per #243.
    ///
    /// The first three are the plain rule, the next two are the pair that stays in sixty four bits
    /// and the pair that does not because it has no digit left in front of the point, and the last
    /// is the width running into the widest decimal there is while the scale does not move.
    #[test]
    fn a_decimal_product_is_as_wide_as_both_of_its_operands_together() {
        let decimal = |width, scale| LogicalType::Decimal { width, scale };
        let times = |left: LogicalType, right: LogicalType| {
            resolve("*", &[left, right]).expect("multiplies").returns
        };
        assert_eq!(times(decimal(4, 2), decimal(4, 2)), decimal(8, 4));
        assert_eq!(times(decimal(4, 2), LogicalType::BigInt), decimal(23, 2));
        assert_eq!(times(decimal(18, 3), LogicalType::Integer), decimal(18, 3));
        assert_eq!(times(decimal(12, 6), decimal(12, 6)), decimal(18, 12));
        assert_eq!(times(decimal(10, 9), decimal(10, 9)), decimal(20, 18));
        assert_eq!(times(decimal(18, 17), decimal(18, 17)), decimal(36, 34));
        assert_eq!(times(decimal(20, 10), decimal(20, 10)), decimal(38, 20));
        // Nothing that is not a decimal goes near any of this.
        assert_eq!(times(LogicalType::Integer, LogicalType::Integer), LogicalType::Integer);
    }

    /// Each side takes the answer's width and keeps its own scale, which is what the kernel needs.
    ///
    /// The unscaled values then multiply into the answer with nothing rescaled on either side of
    /// the operator, which a cast of both sides to the answer's scale would not give.
    #[test]
    fn a_decimal_product_casts_its_operands_to_the_width_of_the_answer() {
        let decimal = |width, scale| LogicalType::Decimal { width, scale };
        let resolved = resolve("*", &[decimal(4, 2), LogicalType::BigInt]).expect("multiplies");
        assert_eq!(resolved.arguments, vec![decimal(23, 2), decimal(23, 0)]);
    }

    /// There is no type to put the answer in, so it is refused at bind time rather than truncated.
    #[test]
    fn a_product_that_needs_more_than_thirty_eight_decimal_places_is_refused() {
        let wide = LogicalType::Decimal { width: 30, scale: 30 };
        let error = resolve("*", &[wide.clone(), wide]).expect_err("has nowhere to put the scale");
        assert!(error.to_string().contains("Max scale is 38"), "{error}");
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

    /// `//` is integer division only when there are integers on both sides of it, which was
    /// measured: `7.5 // 2.5` is the DOUBLE 3.0 upstream and `7.5 // 2` is 3.75, so it neither
    /// stays a decimal nor truncates what it divided.
    #[test]
    fn integer_division_of_anything_but_integers_is_ordinary_division() {
        let decimal = LogicalType::Decimal { width: 4, scale: 2 };
        let divides = |left: LogicalType, right: LogicalType| {
            let resolved = resolve("//", &[left, right]).expect("divides");
            (resolved.arguments, resolved.returns)
        };
        let double = || (vec![LogicalType::Double; 2], LogicalType::Double);
        assert_eq!(divides(decimal.clone(), decimal.clone()), double());
        assert_eq!(divides(decimal.clone(), LogicalType::Integer), double());
        assert_eq!(divides(LogicalType::Integer, decimal), double());
        assert_eq!(divides(LogicalType::Double, LogicalType::Double), double());
        // A float stays a float, so this is a rule about decimals rather than about width.
        assert_eq!(
            divides(LogicalType::Float, LogicalType::Float),
            (vec![LogicalType::Float; 2], LogicalType::Float)
        );
        assert_eq!(
            divides(LogicalType::Integer, LogicalType::BigInt),
            (vec![LogicalType::BigInt; 2], LogicalType::BigInt)
        );
        assert_eq!(
            divides(LogicalType::HugeInt, LogicalType::HugeInt),
            (vec![LogicalType::HugeInt; 2], LogicalType::HugeInt)
        );
    }

    /// An alias has to come back under the real name, because the name on [`Resolved`] is what the
    /// plan interns and what every kernel below it matches on. SQL is case insensitive here, so the
    /// shouted spelling has to land in the same place.
    #[test]
    fn an_alias_resolves_to_the_function_it_is_an_alias_of() {
        for (alias, real) in ALIASES {
            assert_eq!(canonical(alias), *real);
            assert_eq!(canonical(&alias.to_uppercase()), *real);
        }
        let resolved = resolve("LEN", &[LogicalType::Varchar]).expect("len resolves");
        assert_eq!(resolved.name, "length");
        assert_eq!(resolved.returns, LogicalType::BigInt);
    }

    /// Every alias has to point at a row that exists, or the error a caller gets moves from a
    /// missing function to a missing function under another name, which is worse.
    #[test]
    fn every_alias_points_at_a_real_function() {
        for (alias, real) in ALIASES {
            assert!(
                TABLE.iter().any(|entry| entry.name == *real),
                "{alias} points at {real}, which is not in the table"
            );
        }
    }

    /// DuckDB refuses a string function anything that is not already a string, and the whole point
    /// of refusing is the message, so the message is what this checks.
    #[test]
    fn a_string_function_refuses_a_type_that_is_not_a_string() {
        let error = resolve("lower", &[LogicalType::Date]).expect_err("lower takes strings");
        assert_eq!(
            error.to_string(),
            "Binder Error: No function matches the given name and argument types 'lower(DATE)'. \
             You might need to add explicit type casts.\n\tCandidate functions:\n\tlower(col0 \
             VARCHAR) -> VARCHAR\n"
        );
        for name in ["upper", "length", "strlen"] {
            assert!(resolve(name, &[LogicalType::Integer]).is_err(), "{name} took an integer");
        }
        for name in ["~~", "!~~", "~~*", "!~~*"] {
            let types = [LogicalType::Integer, LogicalType::Varchar];
            assert!(resolve(name, &types).is_err(), "{name} took an integer");
        }
    }

    /// The wrong answer this shape was added for. `length([1,2,3])` used to cast the list to a
    /// string and count the nine characters of `[1, 2, 3]`, where DuckDB counts three elements.
    /// rudb has no list type in the executor yet, so refusing is the honest end of it for now.
    #[test]
    fn length_of_something_that_is_not_a_string_is_refused_rather_than_stringified() {
        let error = resolve("length", &[LogicalType::Blob]).expect_err("length takes strings");
        assert!(error.to_string().contains("length(col0 ANY[]) -> BIGINT"), "{error}");
    }

    /// `||` is the exception and it has to stay one. `1 || 'a'` is `1a` upstream.
    #[test]
    fn concatenation_still_takes_anything_and_makes_a_string_of_it() {
        let resolved = resolve("||", &[LogicalType::Integer, LogicalType::Varchar])
            .expect("concatenation takes anything");
        assert_eq!(resolved.returns, LogicalType::Varchar);
        assert_eq!(resolved.arguments, vec![LogicalType::Varchar, LogicalType::Varchar]);
    }

    /// A null literal has no type to pick an overload with, and DuckDB answers `length(NULL)` with
    /// NULL rather than refusing it.
    #[test]
    fn a_string_function_takes_an_untyped_null() {
        let resolved = resolve("length", &[LogicalType::Null]).expect("length of a null");
        assert_eq!(resolved.returns, LogicalType::BigInt);
        assert_eq!(resolved.arguments, vec![LogicalType::Varchar]);
    }

    /// A candidate block for a name nothing resolves to would be a message about a function that
    /// does not exist, which is worse than no block at all.
    #[test]
    fn every_name_with_candidates_is_a_function_this_engine_has() {
        for (name, overloads) in CANDIDATES {
            assert!(TABLE.iter().any(|entry| entry.name == *name), "{name} has no entry");
            assert!(!overloads.is_empty(), "{name} has an empty candidate list");
        }
    }

    /// `strlen` counts bytes and `length` counts characters, so it is a function and not an alias.
    /// This is the test that stops someone folding it into [`ALIASES`] to save a row.
    #[test]
    fn strlen_is_its_own_function_and_not_an_alias_of_length() {
        assert!(!ALIASES.iter().any(|(alias, _)| *alias == "strlen"));
        let resolved = resolve("strlen", &[LogicalType::Varchar]).expect("strlen resolves");
        assert_eq!(resolved.name, "strlen");
        assert_eq!(resolved.returns, LogicalType::BigInt);
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

    /// `nullif` compares at one type and answers at another, both read off the pinned binary with
    /// `typeof`. Per #306.
    #[test]
    fn nullif_answers_the_first_argument_and_compares_at_the_promotion() {
        let resolved =
            resolve("nullif", &[LogicalType::Integer, LogicalType::Decimal { width: 2, scale: 1 }])
                .expect("an integer and a decimal compare");
        assert_eq!(resolved.returns, LogicalType::Integer);
        let wide = LogicalType::Decimal { width: 11, scale: 1 };
        assert_eq!(resolved.arguments, vec![wide.clone(), wide]);
        let resolved = resolve("nullif", &[LogicalType::BigInt, LogicalType::SmallInt])
            .expect("two integers compare");
        assert_eq!(resolved.returns, LogicalType::BigInt);
        let resolved =
            resolve("nullif", &[LogicalType::Null, LogicalType::Null]).expect("two nulls compare");
        assert_eq!(resolved.returns, LogicalType::Integer, "there is nothing else to hand back");
        let error = resolve("nullif", &[LogicalType::Varchar, LogicalType::Integer])
            .expect_err("a string and a number have nothing in common here");
        assert!(error.message().contains("No function matches"), "{error}");
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
                // A shape that names the type it wants is asked for it, since `chr` wants an
                // INTEGER and refuses a string the way upstream does.
                let ty = match (entry.numeric_only, entry.shape) {
                    (_, Shape::Exact(argument, _) | Shape::Widened(argument, _)) => argument.ty(),
                    (true, _) => LogicalType::Integer,
                    (false, _) => LogicalType::Varchar,
                };
                let mut arguments = vec![ty; count];
                // A subscript and a substring are the shapes whose arguments are not all alike. The
                // leading ones are the string or the list and everything after them is a whole
                // number, so a row of strings is not a call either one accepts and not a call worth
                // asserting it accepts.
                let leading = match entry.shape {
                    Shape::Extracted | Shape::Sliced => 1,
                    Shape::TextThenIndex(leading, _) => leading,
                    _ => count,
                };
                for bound in arguments.iter_mut().skip(leading) {
                    *bound = LogicalType::BigInt;
                }
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
