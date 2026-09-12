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
    /// Every argument has to be a string already and the result is fixed. `lower`, `length`, `LIKE`.
    ///
    /// The difference from [`Shape::FixedTo`] with a VARCHAR argument is the word already. DuckDB
    /// refuses `lower(123)`, `length(DATE '2020-01-01')` and `123 LIKE '1%'` with a binder error
    /// naming the overloads it does have, and it refuses a BLOB as well, so the rule is VARCHAR
    /// rather than anything a cast can reach. `||` is the one string function that really does take
    /// anything, since `1 || 'a'` is `1a` upstream, and it keeps [`Shape::FixedTo`] for that reason.
    Text(Fixed),
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
        shape: Shape::Text(returns),
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
        Shape::Text(result) => {
            for ty in arguments {
                // An untyped null is accepted the way it is everywhere else here. DuckDB answers
                // `length(NULL)` with NULL rather than refusing it, because a null has no type to
                // pick an overload with and every overload would return null anyway.
                if *ty != LogicalType::Varchar && *ty != LogicalType::Null {
                    return Err(no_match(entry.name, arguments));
                }
            }
            (vec![LogicalType::Varchar; arguments.len()], result.ty())
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
    };
    Ok(Resolved { name: entry.name, kind: entry.kind, arguments: cast_to, returns })
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
];

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
const ALIASES: &[(&str, &str)] = &[
    ("len", "length"),
    ("char_length", "length"),
    ("character_length", "length"),
    ("lcase", "lower"),
    ("ucase", "upper"),
    ("mean", "avg"),
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
