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

/// Whether a name is a scalar function, an aggregate or a window.
///
/// The binder needs to ask before it knows which slot the call goes in, since an aggregate is only
/// legal in an aggregate list and the error for one in the wrong place should say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    /// One row in, one row out.
    Scalar,
    /// Many rows in, one row out.
    Aggregate,
    /// Many rows in, one row out per row, and the answer comes from where the row sits.
    ///
    /// Every aggregate is also usable as a window, so this is not the kind of everything that can
    /// go inside an `OVER`. It is the kind of the names that can go nowhere else, which is why a
    /// call to one without an `OVER` is an error rather than a call.
    Window,
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
    /// Slash promotes integers and decimals to double but preserves two floats as float.
    Slashed,
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
    /// Every argument widens to the one type they all reach, no narrower than a floor, and the
    /// result is fixed. `age(x, y)`.
    ///
    /// The difference from [`Shape::Widened`] is which way the arguments are allowed to pull. There
    /// the floor is the answer and an argument wider than it is refused, which is right for a name
    /// whose one overload reads a fixed type. Here the arguments meet each other first, so
    /// `age(DATE, DATE)` reads two timestamps because the floor says so and `age(now(), now())`
    /// reads two zoned ones because the arguments say so. Upstream has a row per combination and
    /// this is the one rule they follow.
    WidenedTogether(Fixed, Fixed),
    /// The arguments are whatever they are and the result is fixed. `count(x)` over anything.
    AnyTo(Fixed),
    /// The first `n` arguments are cast to one fixed type, the rest are left alone, and the result
    /// is fixed. `date_part('minute', x)` reads a part of whatever `x` is, and
    /// `regexp_extract(s, p, 2)` takes two strings and then a number that has to stay one.
    LeadingFixedTo(usize, Fixed, Fixed),
    /// The first argument is cast to one fixed type, the rest are left alone, and the result is the
    /// last argument's own type. `date_trunc('month', x)` gives back whatever kind of date `x` was.
    LeadingFixedToLast(Fixed),
    /// Every argument promotes and an integer result widens to the accumulator. `sum`.
    Accumulated,
    /// Every argument promotes to one integer type and the result is that type. `bit_and`.
    ///
    /// [`Shape::Promoted`] with everything but the integers taken away, which is the pin's list of
    /// overloads: `bit_and(1.5)` and `bit_and(true)` are refused there rather than cast. A null
    /// argument is a BIGINT, since that is the type the pin answers `bit_or(NULL)` with.
    Bitwise,
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
    /// Two arguments joined end to end, which means two strings or two lists. `||`.
    ///
    /// One operator with two readings and the arguments pick which. Two lists concatenate into a
    /// list of the type their elements promote to, so `[1, 2] || [3.5]` is a list of decimals, and
    /// anything else is the string reading that [`Shape::FixedTo`] gave this row before lists could
    /// be concatenated at all, which is what keeps `1 || 'a'` answering `1a`.
    ///
    /// A list on one side and something that is not a list on the other is neither reading and is
    /// refused. It has to be, because the string reading would answer it: `[1, 2] || 3` would be
    /// `[1, 2]3`, which is not a wrong list so much as a list printed by accident. Upstream refuses
    /// it too and this is its sentence.
    ///
    /// An untyped null on one side takes the other side's reading, since a null belongs to every
    /// type and the answer is null whichever way it went.
    Concatenated,
    /// Every argument is a list and they are joined end to end. `list_concat`.
    ///
    /// The variadic sibling of [`Shape::Concatenated`] with the string reading taken away, so
    /// `list_concat('a', 'b')` is refused where `'a' || 'b'` is a string. The elements of every
    /// argument promote to one type the way they do for a list literal.
    ///
    /// It differs from the operator on nulls, which is not a detail: `list_concat([1], NULL)` is
    /// `[1]` upstream and `[1] || NULL` is null. A null argument here is a list with nothing in it
    /// and is skipped, which is the same rule `concat` follows over strings.
    ListConcatenated,
    /// One string or one list, and how long it is. `length`.
    ///
    /// Characters for a string and elements for a list, and the two are separate overloads on the
    /// pin that happen to share a name. A list is counted at its top level only, so
    /// `length([[1], [2]])` is 2, and a null element is an element, so `length([1, NULL])` is 2.
    /// Anything else is refused rather than turned into a string first, which is what this row
    /// used to do: `length([1, 2, 3])` counted the nine characters of `[1, 2, 3]`.
    Counted,
    /// A list, and a dimension to count it along. `array_length`.
    ///
    /// The list half of [`Shape::Counted`] with no string reading, so `array_length('abc')` is
    /// refused, and a second argument that is a whole number and is not cast to one, the way a
    /// slice's step is not. Only the first dimension is implemented upstream and asking for any
    /// other is a runtime error there, so it is one here too, in the same words.
    ListCounted,
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
    /// One string naming a setting, and the result is whatever type that setting holds.
    ///
    /// `current_setting` and nothing else. It is a shape rather than a fixed pair because the pin
    /// declares the return as `ANY` and then works it out from the name that was passed, which is
    /// why `typeof(current_setting('threads'))` is BIGINT there and
    /// `typeof(current_setting('memory_limit'))` is VARCHAR. Both come from one overload.
    ///
    /// Resolving this is an error and that is the point of it. The binder folds the call to the
    /// setting's value before it asks this table anything, so the only way a call arrives here is
    /// the way the fold cannot happen, which is an argument that is not a constant, and that is the
    /// case the pin refuses in the same words.
    Setting,
    /// The first argument is the value and its type is the answer, the second is a row count, and
    /// the third is another value of the first's type. The five windows that read a row rather than
    /// aggregate one.
    ///
    /// The pin prints the widest of them as `lag(col T, "offset" BIGINT := 1, "default" ANY := NULL)
    /// -> T`, and `nth_value(col0 ANY, col1 BIGINT) -> ANY` and `first_value(col0 ANY) -> ANY` are
    /// that rule with the tail cut off, so one shape covers all five. The count is a BIGINT and the
    /// default is cast to the column's type rather than left alone, which is why `lag(i, 1, 0.5)`
    /// over an INTEGER column answers 1 and `lag(k, 1, 'z')` over one raises a conversion error at
    /// run time rather than a binder error at bind time. The cast is legal and it is the value that
    /// will not go through it.
    ///
    /// The spelling is carried because the pin does not use one for all five. `duckdb_functions()`
    /// there says `[T, BIGINT, ANY] -> T` for `lag` and `lead` and `[ANY] -> ANY` for the other
    /// three, which is the same rule written two ways, and a table that prints what the pin prints
    /// has to know which way each row goes.
    ValueThenCountThenValue(Spelled),
    /// One argument, nothing is cast, and the result is that argument's own type. `fill`.
    ///
    /// The pin prints it as `fill(col0 ANY) -> ANY` and `typeof(fill(x) OVER (ORDER BY k))` there
    /// gives back whatever `x` was, INTEGER for an INTEGER column and DECIMAL(10,2) for a decimal
    /// one, so the declaration says nothing and the argument says everything. The rule that the
    /// argument has to be a type arithmetic can reach is the binder's rather than this table's,
    /// since it is the sort key and not just the argument that has to satisfy it.
    AsGiven,
    /// Every argument promotes to one type and the result is a list of that type. `list_value`.
    ///
    /// The one shape whose result is not a type any of the arguments had, which is why it cannot be
    /// [`Shape::Promoted`] with a wrapper bolted on at the call site. A list literal is a call to
    /// this by the time the binder is done with it, so the rule that decides what `[a, b]` holds is
    /// the rule that decides what `list_value(a, b)` holds, written once.
    ///
    /// No arguments at all is `"NULL"[]`, which is the pin's answer for `[]` and is a list whose
    /// element type is the untyped null rather than a guess at what somebody meant to put in it.
    Listed,
    /// No arguments at all and a fixed result. `now()` and `current_schema()`.
    ///
    /// The session context functions, which are the ones whose answer comes from the connection
    /// rather than from anything written in the query. The binder folds every one of them into a
    /// constant before this table is asked, the same way it folds `typeof`, so what these rows are
    /// for is the two questions the fold does not answer: which names exist, which is what
    /// `duckdb_functions()` reports, and what `now(1)` says, which is the arity error rather than a
    /// missing function.
    Constant(Fixed),
    /// A list and a value its elements are compared with, and a fixed result. `list_position` and
    /// `list_contains`.
    ///
    /// The pin declares these `(T[], T)`, so the element type and the value meet at one type the way
    /// the two sides of `=` do. Two that will not meet are refused with the pin's sentence about the
    /// type variable, which names the two readings of `T` it could not reconcile.
    ListSearched(Fixed),
    /// Two lists whose elements meet at one type, and either a list of that type back, which is
    /// `list_intersect`, or a fixed result, which is `list_has_any` and `list_has_all`.
    ListsMet(Option<Fixed>),
    /// One list, and a list of the same type back. `list_distinct` and `list_reverse`.
    ListKept,
    /// One list of anything, and a fixed result. `list_unique`.
    ListTo(Fixed),
    /// A list, and a second list of a fixed element type that picks from it. `list_where` takes a
    /// list of booleans and `list_select` a list of whole numbers.
    ///
    /// The second list is not cast, so `list_select([1], [1.5])` is refused as it is on the pin and
    /// not rounded to the first element.
    ListPicked(Fixed),
    /// A list of lists, and the list of their elements. `flatten`.
    Flattened,
    /// A list, the length it should have, and what to pad it with. `list_resize`.
    ///
    /// The length is cast to UBIGINT, which is where the pin sends it, so a negative length is that
    /// cast's out of range error. The padding is cast to the element type.
    Resized,
    /// A list and up to two strings saying how to order it. `list_sort` and `list_reverse_sort`.
    ///
    /// The strings are not cast, so `list_sort([1], 1)` is refused as it is on the pin, and the
    /// binder holds them to constants.
    Sorted,
    /// A start, a stop and a step, all BIGINT, or two moments and an INTERVAL, answering with the
    /// list of values from the start toward the stop. `range` and `generate_series`.
    ///
    /// Only the integer types that widen to BIGINT without loss are taken, so a HUGEINT or a
    /// decimal is refused as it is on the pin. A date goes in as a timestamp, and a zoned moment on
    /// either side makes the list zoned.
    Ranged,
    /// A list and the same two strings, answering with the places `list_sort` would put each
    /// element at rather than the elements. `list_grade_up`.
    Graded,
}

/// How a shape names the argument whose type the call decides, and the result that follows it.
///
/// `T` says the two are the same type and `ANY` says the name does not commit to one. They mean the
/// same thing to the binder, since both resolve from what was passed, and they are different words
/// in the table `duckdb_functions()` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spelled {
    Same,
    Any,
}

impl Spelled {
    const fn name(self) -> &'static str {
        match self {
            Self::Same => SAME,
            Self::Any => ANY,
        }
    }
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
    UBigInt,
    Double,
    Varchar,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
    Interval,
}

impl Fixed {
    fn ty(self) -> LogicalType {
        match self {
            Self::Boolean => LogicalType::Boolean,
            Self::Integer => LogicalType::Integer,
            Self::BigInt => LogicalType::BigInt,
            Self::UBigInt => LogicalType::UBigInt,
            Self::Double => LogicalType::Double,
            Self::Varchar => LogicalType::Varchar,
            Self::Date => LogicalType::Date,
            Self::Time => LogicalType::Time,
            Self::TimeTz => LogicalType::TimeTz,
            Self::Timestamp => LogicalType::Timestamp,
            Self::TimestampTz => LogicalType::TimestampTz,
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
    number("/", Arity::exactly(2), Shape::Slashed),
    number("//", Arity::exactly(2), Shape::Divided),
    number("abs", Arity::exactly(1), Shape::Promoted),
    // Strings.
    // `||` takes anything and turns it into a string, which is why everything under it is a `Text`
    // and this is not. `1 || 'a'` is `1a` upstream, and two lists are the second reading of the same
    // operator, which is what [`Shape::Concatenated`] is for.
    Entry {
        name: "||",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::Concatenated,
        numeric_only: false,
    },
    text("lower", Arity::exactly(1), Fixed::Varchar),
    text("upper", Arity::exactly(1), Fixed::Varchar),
    Entry {
        name: "length",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::Counted,
        numeric_only: false,
    },
    Entry {
        name: "array_length",
        kind: FunctionKind::Scalar,
        arity: Arity::between(1, 2),
        shape: Shape::ListCounted,
        numeric_only: false,
    },
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
    // `contains` is also the list search when its first argument is a list, and [`resolve`] sends
    // it to `list_contains` in that case. This row is the string overload.
    text("contains", Arity::exactly(2), Fixed::Boolean),
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
    // The answer is a double here and a bigint by the time the binder is finished with it, for
    // every part but the two that carry a fraction. See `narrowed_part` in `rudb-bind`, which is
    // where the value of the first argument gets to decide the type of the call.
    Entry {
        name: "date_part",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::LeadingFixedTo(1, Fixed::Varchar, Fixed::Double),
        numeric_only: false,
    },
    Entry {
        name: "date_trunc",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(2),
        shape: Shape::LeadingFixedToLast(Fixed::Varchar),
        numeric_only: false,
    },
    // The gap between two moments counted in calendar fields. Upstream has a one argument form as
    // well, which measures from today, and it is not here because there is no clock in the engine
    // yet and a function that invents one would be worse than a function that is missing.
    //
    // Widening rather than casting is the whole overload: a DATE widens to a TIMESTAMP and upstream
    // accepts `age(DATE, DATE)`, while a TIME and an INTERVAL do not widen anywhere and upstream
    // refuses both of those with a binder error rather than reading them as moments.
    Entry {
        name: "age",
        kind: FunctionKind::Scalar,
        arity: Arity::one_of(&[1, 2]),
        shape: Shape::WidenedTogether(Fixed::Timestamp, Fixed::Interval),
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
    // Building a list. `[a, b]` is `list_value(a, b)` by the time the binder is done with it, which
    // is what DuckDB's own transformer writes as well, so this is the row behind the bracket as much
    // as behind the written name. `list_pack` is the same function under another name and is an
    // alias below, which is what the pin's `alias_of` column says about it.
    Entry {
        name: "list_value",
        kind: FunctionKind::Scalar,
        arity: Arity::at_least(0),
        shape: Shape::Listed,
        numeric_only: false,
    },
    // Joining lists end to end. The pin prints one overload, `list_concat([ANY[]...]) -> ANY[]`,
    // and answers to four names for it, three of which are aliases below. It is the named form of
    // `||` over two lists and is not quite the same function, because a null argument is skipped
    // here and answers null there.
    //
    // It starts at one argument and not at none, which is a divergence and a deliberate one. The pin
    // answers `list_concat()` with the empty string, of type VARCHAR, from a function that declares
    // it returns `ANY[]`. That is tamnd/duckdb#11 and this refuses the call rather than copying it.
    Entry {
        name: "list_concat",
        kind: FunctionKind::Scalar,
        arity: Arity::at_least(1),
        shape: Shape::ListConcatenated,
        numeric_only: false,
    },
    // Looking inside a list. Every row here is one overload on the pin, and all but `list_unique`,
    // `list_resize` and `flatten` are declared over `T`, which is what makes a list of numbers and a
    // string needle a binder error rather than a comparison that is never true.
    list_row("list_position", 2, Shape::ListSearched(Fixed::Integer)),
    list_row("list_contains", 2, Shape::ListSearched(Fixed::Boolean)),
    list_row("list_has_any", 2, Shape::ListsMet(Some(Fixed::Boolean))),
    list_row("list_has_all", 2, Shape::ListsMet(Some(Fixed::Boolean))),
    list_row("list_intersect", 2, Shape::ListsMet(None)),
    list_row("list_distinct", 1, Shape::ListKept),
    list_row("list_reverse", 1, Shape::ListKept),
    list_row("list_unique", 1, Shape::ListTo(Fixed::UBigInt)),
    list_row("list_where", 2, Shape::ListPicked(Fixed::Boolean)),
    list_row("list_select", 2, Shape::ListPicked(Fixed::BigInt)),
    list_row("flatten", 1, Shape::Flattened),
    Entry {
        name: "list_sort",
        kind: FunctionKind::Scalar,
        arity: Arity::between(1, 3),
        shape: Shape::Sorted,
        numeric_only: false,
    },
    Entry {
        name: "list_reverse_sort",
        kind: FunctionKind::Scalar,
        arity: Arity::between(1, 2),
        shape: Shape::Sorted,
        numeric_only: false,
    },
    Entry {
        name: "list_grade_up",
        kind: FunctionKind::Scalar,
        arity: Arity::between(1, 3),
        shape: Shape::Graded,
        numeric_only: false,
    },
    // `range` and `generate_series` as scalars, which answer with the whole series as one list.
    // The table functions of the same names are a different table and are not affected.
    Entry {
        name: "range",
        kind: FunctionKind::Scalar,
        arity: Arity::between(1, 3),
        shape: Shape::Ranged,
        numeric_only: false,
    },
    Entry {
        name: "generate_series",
        kind: FunctionKind::Scalar,
        arity: Arity::between(1, 3),
        shape: Shape::Ranged,
        numeric_only: false,
    },
    Entry {
        name: "list_resize",
        kind: FunctionKind::Scalar,
        arity: Arity::between(2, 3),
        shape: Shape::Resized,
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
    // The value of a setting, as a value rather than as a row of `duckdb_settings()`. This is the
    // second function the binder folds and it folds for the same reason `typeof` does: the answer
    // is settled once the name is known and nothing about it changes per row. Upstream folds it too
    // and an `EXPLAIN` of a query that calls it shows the literal, which is what makes an `ANY`
    // return type resolve to something a plan can carry.
    Entry {
        name: "current_setting",
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(1),
        shape: Shape::Setting,
        numeric_only: false,
    },
    // Session context. Fourteen names for eight answers, which is the SQL standard's spellings and
    // Postgres's spellings and DuckDB's own sitting on top of each other. The binder folds every one
    // of them, so these rows exist to be listed by `duckdb_settings()`'s neighbour
    // `duckdb_functions()` and to give `now(1)` the arity error the pin gives it.
    //
    // Four of these are macro rows upstream rather than scalar rows, which is `current_user`,
    // `session_user`, `user` and `current_catalog`, and this table has no macros so they are scalars
    // here. The difference shows up in the `function_type` column of `duckdb_functions()` and
    // nowhere else, since the parenthesized call binds on both engines and answers the same.
    session("now", Fixed::TimestampTz),
    session("get_current_timestamp", Fixed::TimestampTz),
    session("transaction_timestamp", Fixed::TimestampTz),
    session("current_localtimestamp", Fixed::Timestamp),
    session("get_current_time", Fixed::TimeTz),
    session("current_localtime", Fixed::Time),
    session("current_date", Fixed::Date),
    session("today", Fixed::Date),
    session("current_schema", Fixed::Varchar),
    session("current_database", Fixed::Varchar),
    session("current_catalog", Fixed::Varchar),
    session("current_user", Fixed::Varchar),
    session("session_user", Fixed::Varchar),
    session("user", Fixed::Varchar),
    // Aggregates.
    aggregate("count_star", Arity::exactly(0), Shape::AnyTo(Fixed::BigInt), false),
    aggregate("count", Arity::exactly(1), Shape::AnyTo(Fixed::BigInt), false),
    aggregate("sum", Arity::exactly(1), Shape::Accumulated, true),
    aggregate("avg", Arity::exactly(1), Shape::PromotedTo(Fixed::Double), true),
    aggregate("min", Arity::exactly(1), Shape::Promoted, false),
    aggregate("max", Arity::exactly(1), Shape::Promoted, false),
    aggregate("list", Arity::exactly(1), Shape::Listed, false),
    aggregate("first", Arity::exactly(1), Shape::AsGiven, false),
    aggregate("last", Arity::exactly(1), Shape::AsGiven, false),
    aggregate("any_value", Arity::exactly(1), Shape::AsGiven, false),
    aggregate("bool_and", Arity::exactly(1), Shape::Widened(Fixed::Boolean, Fixed::Boolean), false),
    aggregate("bool_or", Arity::exactly(1), Shape::Widened(Fixed::Boolean, Fixed::Boolean), false),
    aggregate("bit_and", Arity::exactly(1), Shape::Bitwise, false),
    aggregate("bit_or", Arity::exactly(1), Shape::Bitwise, false),
    aggregate("bit_xor", Arity::exactly(1), Shape::Bitwise, false),
    aggregate("product", Arity::exactly(1), Shape::FixedTo(Fixed::Double, Fixed::Double), false),
    aggregate("var_samp", Arity::exactly(1), Shape::FixedTo(Fixed::Double, Fixed::Double), false),
    aggregate("var_pop", Arity::exactly(1), Shape::FixedTo(Fixed::Double, Fixed::Double), false),
    aggregate(
        "stddev_samp",
        Arity::exactly(1),
        Shape::FixedTo(Fixed::Double, Fixed::Double),
        false,
    ),
    aggregate("stddev_pop", Arity::exactly(1), Shape::FixedTo(Fixed::Double, Fixed::Double), false),
    aggregate(
        "string_agg",
        Arity::between(1, 2),
        Shape::FixedTo(Fixed::Varchar, Fixed::Varchar),
        false,
    ),
    // The ranking windows, which answer from where the row sits in its partition rather than from
    // anything in it. Six names and seven rows, since `rank_dense` is an alias upstream reports
    // with `dense_rank` in its `alias_of`. The three that count rows are BIGINT and the two that
    // divide one count by another are DOUBLE, which was read off the pin with `typeof` rather than
    // assumed, and `ntile` takes the one argument the family has and takes it as a BIGINT.
    ranking("cume_dist", Arity::exactly(0), Shape::AnyTo(Fixed::Double)),
    ranking("dense_rank", Arity::exactly(0), Shape::AnyTo(Fixed::BigInt)),
    ranking("ntile", Arity::exactly(1), Shape::FixedTo(Fixed::BigInt, Fixed::BigInt)),
    ranking("percent_rank", Arity::exactly(0), Shape::AnyTo(Fixed::Double)),
    ranking("rank", Arity::exactly(0), Shape::AnyTo(Fixed::BigInt)),
    ranking("row_number", Arity::exactly(0), Shape::AnyTo(Fixed::BigInt)),
    // The windows that read a row rather than count one. Five names, one shape, and the answer is
    // the first argument's own type in every case, which was read off the pin with `typeof` the way
    // the ranking types were. `lag` and `lead` take an optional count and an optional default, and
    // `nth_value` takes a count it requires.
    value_window("first_value", Arity::exactly(1), Spelled::Any),
    value_window("lag", Arity::between(1, 3), Spelled::Same),
    value_window("last_value", Arity::exactly(1), Spelled::Any),
    value_window("lead", Arity::between(1, 3), Spelled::Same),
    value_window("nth_value", Arity::exactly(2), Spelled::Any),
    // The thirteenth window name, which reads neither a position nor a row. It fills the gaps in a
    // column by interpolating between the values on either side of each one, so the answer is the
    // argument's own type and there is nothing else to declare.
    Entry {
        name: "fill",
        kind: FunctionKind::Window,
        arity: Arity::exactly(1),
        shape: Shape::AsGiven,
        numeric_only: false,
    },
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

/// A scalar over lists that takes exactly `count` arguments.
const fn list_row(name: &'static str, count: usize, shape: Shape) -> Entry {
    Entry {
        name,
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(count),
        shape,
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

/// A session context function, which takes nothing and answers about the connection.
const fn session(name: &'static str, returns: Fixed) -> Entry {
    Entry {
        name,
        kind: FunctionKind::Scalar,
        arity: Arity::exactly(0),
        shape: Shape::Constant(returns),
        numeric_only: false,
    }
}

const fn aggregate(name: &'static str, arity: Arity, shape: Shape, numeric_only: bool) -> Entry {
    Entry { name, kind: FunctionKind::Aggregate, arity, shape, numeric_only }
}

/// A window that reads where the row sits rather than what is in it.
const fn ranking(name: &'static str, arity: Arity, shape: Shape) -> Entry {
    Entry { name, kind: FunctionKind::Window, arity, shape, numeric_only: false }
}

/// A window that reads a row of the partition rather than aggregating one.
const fn value_window(name: &'static str, arity: Arity, spelled: Spelled) -> Entry {
    Entry {
        name,
        kind: FunctionKind::Window,
        arity,
        shape: Shape::ValueThenCountThenValue(spelled),
        numeric_only: false,
    }
}

/// Whether a name is a function at all, and which kind.
///
/// The binder asks this before it knows what to do with a call, since `count(x)` in a projection
/// has to become an error naming the aggregate rather than a lookup failure naming the name.
#[must_use]
pub fn kind_of(name: &str) -> Option<FunctionKind> {
    find(name).map(|entry| entry.kind)
}

/// What `date_part` answers with when the specifier is known at binding time.
///
/// `epoch` counts seconds and `julian` counts days, and both of them carry a fraction, so those two
/// are doubles and every other part is a whole number. A specifier that names no part at all is a
/// double as well, since the call is going to fail anyway and the sentence about it belongs to the
/// one place that knows every spelling.
///
/// This is the only place a call's type comes from the value of an argument rather than the type of
/// one, and it is upstream's rule rather than an optimization: the declared overload there is a
/// double and the binder narrows it, which is why `date_part(p, ts)` over a column of specifiers is
/// a double even when every row of it says `year`.
#[must_use]
pub fn part_type(spelling: &str) -> LogicalType {
    let fraction = ["epoch", "julian", "jd"];
    if fraction.iter().any(|name| name.eq_ignore_ascii_case(spelling)) {
        LogicalType::Double
    } else {
        LogicalType::BigInt
    }
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
    if entry.name == "contains"
        && matches!(arguments.first(), Some(LogicalType::List(_) | LogicalType::Array(..)))
    {
        return resolve("list_contains", arguments);
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
        Shape::Slashed => {
            let common = promote_all(name, arguments)?;
            let returns =
                if common == LogicalType::Float { LogicalType::Float } else { LogicalType::Double };
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
        Shape::WidenedTogether(floor, result) => {
            // A null has nothing to pull with, so it is left out of the meeting and then cast to
            // whatever the rest of them settled on, which is the floor when they were all nulls.
            let mut wanted = floor.ty();
            for ty in arguments {
                if *ty == LogicalType::Null {
                    continue;
                }
                match ty.promote(&wanted) {
                    Some(met) => wanted = met,
                    None => return Err(no_match(entry.name, arguments)),
                }
            }
            (vec![wanted.clone(); arguments.len()], result.ty())
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
        Shape::Bitwise => {
            // A null literal promotes to INTEGER on its own, and the pin's answer is BIGINT.
            let common = if arguments.iter().all(|ty| *ty == LogicalType::Null) {
                LogicalType::BigInt
            } else {
                promote_all(name, arguments)?
            };
            if !common.is_integer() {
                return Err(no_match(entry.name, arguments));
            }
            (vec![common.clone(); arguments.len()], common)
        }
        Shape::Listed => {
            let element = list_element(arguments)?;
            (vec![element.clone(); arguments.len()], LogicalType::list(element))
        }
        Shape::Concatenated => {
            let left = &arguments[0];
            let right = &arguments[1];
            match (left, right) {
                (LogicalType::List(_), _) | (_, LogicalType::List(_)) => {
                    // A null on one side is not a list and is not the string reading either, so it
                    // takes the reading the other side is already in and the answer is a null of
                    // that side's type.
                    let wanted = match (left, right) {
                        (LogicalType::Null, ty) | (ty, LogicalType::Null) => ty.clone(),
                        (LogicalType::List(_), LogicalType::List(_)) => {
                            left.promote(right).ok_or_else(|| {
                                Error::binder(format!(
                                    "Cannot concatenate lists of types {left} and {right} - an explicit cast is required"
                                ))
                            })?
                        }
                        _ => {
                            return Err(Error::binder(format!(
                                "Cannot concatenate types {left} and {right} - an explicit cast is required"
                            )));
                        }
                    };
                    (vec![wanted.clone(), wanted.clone()], wanted)
                }
                _ => (vec![LogicalType::Varchar; 2], LogicalType::Varchar),
            }
        }
        Shape::ListConcatenated => {
            let mut wanted = LogicalType::Null;
            for ty in arguments {
                // A null is every type's, so it is left out of the meeting rather than dragging the
                // answer down to the untyped null, and the cast below turns it into a null list of
                // whatever the rest of them settled on.
                if *ty == LogicalType::Null {
                    continue;
                }
                if !matches!(ty, LogicalType::List(_)) {
                    return Err(no_match(entry.name, arguments));
                }
                // An argument of the wrong shape is the candidate block above and an argument whose
                // elements will not meet the rest is the operator's sentence, which is the pin's
                // split too: `list_concat([1], 2)` is a candidate block and
                // `list_concat([1], ['a'])` is the sentence.
                let met = wanted.promote(ty).ok_or_else(|| {
                    Error::binder(format!(
                        "Cannot concatenate lists of types {wanted} and {ty} - an explicit cast is required"
                    ))
                })?;
                wanted = met;
            }
            if wanted == LogicalType::Null {
                wanted = LogicalType::list(LogicalType::Null);
            }
            (vec![wanted.clone(); arguments.len()], wanted)
        }
        Shape::Counted => {
            // A null goes to the string overload, which is the one the pin lists first, and it is
            // a null answer whichever overload it went to.
            let taken = match &arguments[0] {
                LogicalType::Varchar | LogicalType::Null => LogicalType::Varchar,
                list @ (LogicalType::List(_) | LogicalType::Array(_, _)) => list.clone(),
                _ => return Err(no_match(entry.name, arguments)),
            };
            (vec![taken], LogicalType::BigInt)
        }
        Shape::ListCounted => {
            let target = &arguments[0];
            if !matches!(
                target,
                LogicalType::List(_) | LogicalType::Array(_, _) | LogicalType::Null
            ) {
                return Err(no_match(entry.name, arguments));
            }
            if let Some(dimension) = arguments.get(1) {
                if !dimension.is_integer() && *dimension != LogicalType::Null {
                    return Err(no_match(entry.name, arguments));
                }
            }
            let mut cast_to = vec![LogicalType::BigInt; arguments.len()];
            cast_to[0] = target.clone();
            (cast_to, LogicalType::BigInt)
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
        Shape::ValueThenCountThenValue(_) => {
            let first = arguments[0].clone();
            let mut cast_to = vec![first.clone(); arguments.len()];
            if let Some(count) = cast_to.get_mut(1) {
                *count = LogicalType::BigInt;
            }
            (cast_to, first)
        }
        Shape::AsGiven => (vec![arguments[0].clone()], arguments[0].clone()),
        Shape::PromotedToFirst => {
            let common = promote_all(name, arguments)?;
            // An untyped null keeps nothing to hand back, so it takes the promoted type the way
            // every other shape here does. Upstream says NULL for `typeof(nullif(NULL, NULL))`
            // because it has a type for a null literal and this engine does not, which is #244.
            let first = &arguments[0];
            let returns = if *first == LogicalType::Null { common.clone() } else { first.clone() };
            (vec![common; arguments.len()], returns)
        }
        // Reaching here means the binder could not fold the call, and the only reason it cannot is
        // an argument that is not a constant. The pin says exactly this and names the parameter.
        Shape::Setting => {
            return Err(Error::binder(format!(
                "The \"setting_name\" argument in function \"{}\" must be a constant expression",
                entry.name
            )));
        }
        // The arity check above already refused every call but the one with no arguments, so there
        // is nothing to cast and nothing left to decide.
        Shape::Constant(fixed) => (Vec::new(), fixed.ty()),
        Shape::ListSearched(to) => {
            let Some(element) = element_of_list(&arguments[0]) else {
                return Err(no_match(entry.name, arguments));
            };
            let element = deduce(entry, &element, &arguments[1])?;
            (vec![LogicalType::list(element.clone()), element], to.ty())
        }
        Shape::ListsMet(to) => {
            let (Some(left), Some(right)) =
                (element_of_list(&arguments[0]), element_of_list(&arguments[1]))
            else {
                return Err(no_match(entry.name, arguments));
            };
            let list = LogicalType::list(deduce(entry, &left, &right)?);
            let returns = to.map_or_else(|| list.clone(), Fixed::ty);
            (vec![list.clone(), list], returns)
        }
        Shape::ListKept => {
            let Some(element) = element_of_list(&arguments[0]) else {
                // `list_reverse` is a macro over a slice on the pin, so what it says about a value
                // that is not a list is what the slice says.
                if entry.name == "list_reverse" {
                    return Err(match arguments[0] {
                        LogicalType::Varchar => Error::not_implemented(STEPPED_STRING),
                        _ => Error::binder("ARRAY_SLICE can only operate on LISTs and VARCHARs"),
                    });
                }
                return Err(no_match(entry.name, arguments));
            };
            listed_or_null(&arguments[0], element, Vec::new())
        }
        Shape::ListTo(to) => {
            let Some(element) = element_of_list(&arguments[0]) else {
                return Err(no_match(entry.name, arguments));
            };
            let taken = match arguments[0] {
                LogicalType::Null => LogicalType::Null,
                _ => LogicalType::list(element),
            };
            (vec![taken], to.ty())
        }
        Shape::ListPicked(by) => {
            let picks = match &arguments[1] {
                LogicalType::Null => true,
                LogicalType::List(inner) => match by {
                    Fixed::Boolean => matches!(**inner, LogicalType::Boolean | LogicalType::Null),
                    _ => inner.is_integer() || **inner == LogicalType::Null,
                },
                _ => false,
            };
            let Some(element) = element_of_list(&arguments[0]).filter(|_| picks) else {
                return Err(no_match(entry.name, arguments));
            };
            listed_or_null(&arguments[0], element, vec![LogicalType::list(by.ty())])
        }
        Shape::Flattened => {
            let element = match &arguments[0] {
                LogicalType::Null => None,
                LogicalType::List(inner) | LogicalType::Array(inner, _) => match &**inner {
                    LogicalType::Null => Some(LogicalType::Null),
                    LogicalType::List(element) | LogicalType::Array(element, _) => {
                        Some((**element).clone())
                    }
                    _ => return Err(no_match(entry.name, arguments)),
                },
                _ => return Err(no_match(entry.name, arguments)),
            };
            match element {
                None => (vec![LogicalType::Null], LogicalType::Null),
                Some(element) => {
                    let list = LogicalType::list(element.clone());
                    (vec![LogicalType::list(list.clone())], list)
                }
            }
        }
        Shape::Resized => {
            let Some(element) = element_of_list(&arguments[0]) else {
                return Err(no_match(entry.name, arguments));
            };
            let mut rest = vec![LogicalType::UBigInt];
            if arguments.len() == 3 {
                rest.push(match element {
                    LogicalType::Null => arguments[2].clone(),
                    ref element => element.clone(),
                });
            }
            listed_or_null(&arguments[0], element, rest)
        }
        Shape::Sorted => {
            let spelled = arguments[1..]
                .iter()
                .all(|ty| matches!(ty, LogicalType::Varchar | LogicalType::Null));
            let Some(element) = element_of_list(&arguments[0]).filter(|_| spelled) else {
                return Err(no_match(entry.name, arguments));
            };
            listed_or_null(&arguments[0], element, arguments[1..].to_vec())
        }
        Shape::Ranged => {
            let moment = |ty: &LogicalType| {
                matches!(
                    ty,
                    LogicalType::Date
                        | LogicalType::Timestamp
                        | LogicalType::TimestampTz
                        | LogicalType::Null
                )
            };
            let whole = |ty: &LogicalType| {
                matches!(
                    ty,
                    LogicalType::TinyInt
                        | LogicalType::SmallInt
                        | LogicalType::Integer
                        | LogicalType::BigInt
                        | LogicalType::UTinyInt
                        | LogicalType::USmallInt
                        | LogicalType::UInteger
                        | LogicalType::Null
                )
            };
            match arguments {
                [start, stop, LogicalType::Interval] if moment(start) && moment(stop) => {
                    let zoned = [start, stop].contains(&&LogicalType::TimestampTz);
                    let when =
                        if zoned { LogicalType::TimestampTz } else { LogicalType::Timestamp };
                    (
                        vec![when.clone(), when.clone(), LogicalType::Interval],
                        LogicalType::list(when),
                    )
                }
                _ if arguments.iter().all(whole) => (
                    vec![LogicalType::BigInt; arguments.len()],
                    LogicalType::list(LogicalType::BigInt),
                ),
                _ => return Err(no_match(entry.name, arguments)),
            }
        }
        Shape::Graded => {
            let spelled = arguments[1..]
                .iter()
                .all(|ty| matches!(ty, LogicalType::Varchar | LogicalType::Null));
            let Some(element) = element_of_list(&arguments[0]).filter(|_| spelled) else {
                return Err(no_match(entry.name, arguments));
            };
            let (cast_to, returns) =
                listed_or_null(&arguments[0], element, arguments[1..].to_vec());
            let returns = match returns {
                LogicalType::Null => LogicalType::Null,
                _ => LogicalType::list(LogicalType::BigInt),
            };
            (cast_to, returns)
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
        BigInt, Date, Double, HugeInt, Integer, Interval, Null, SmallInt, Time, TimeTz, Timestamp,
        TimestampTz, TinyInt, UBigInt, UHugeInt, USmallInt, UTinyInt,
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
        // A zoned value keeps its zone through all of this, which is upstream's answer for every one
        // of these rather than something read off the unzoned rows above. The mixed subtraction is
        // the one that has a cast in it: a plain timestamp or a date next to a zoned one becomes
        // zoned first, and then the two of them are two of the same kind.
        ("+" | "-", [TimestampTz, Interval]) | ("+", [Interval, TimestampTz]) => kept(TimestampTz),
        ("+" | "-", [TimeTz, Interval]) | ("+", [Interval, TimeTz]) => kept(TimeTz),
        ("-", [TimestampTz, TimestampTz]) => kept(Interval),
        ("-", [TimestampTz, Timestamp | Date] | [Timestamp | Date, TimestampTz]) => {
            Some((vec![TimestampTz, TimestampTz], Interval))
        }
        ("+", [Date, TimeTz] | [TimeTz, Date]) => kept(TimestampTz),
        ("+" | "-", [TimestampTz, Null]) | ("+", [Null, TimestampTz]) => kept(TimestampTz),
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
    // The session context functions, which all print the same way because they all take nothing.
    // The four spelled as macros upstream are not here on purpose: the pin answers those with
    // "Macro current_user() does not support the supplied arguments" and a `Candidate macros:` block
    // under it, and rudb has no macros to say that about, so a block naming candidate functions
    // would be a second thing wrong rather than the sentence with nothing under it.
    ("now", &["now() -> TIMESTAMP WITH TIME ZONE"]),
    ("get_current_timestamp", &["get_current_timestamp() -> TIMESTAMP WITH TIME ZONE"]),
    ("transaction_timestamp", &["transaction_timestamp() -> TIMESTAMP WITH TIME ZONE"]),
    ("current_localtimestamp", &["current_localtimestamp() -> TIMESTAMP"]),
    ("get_current_time", &["get_current_time() -> TIME WITH TIME ZONE"]),
    ("current_localtime", &["current_localtime() -> TIME"]),
    ("current_date", &["current_date() -> DATE"]),
    ("today", &["today() -> DATE"]),
    ("current_schema", &["current_schema() -> VARCHAR"]),
    ("current_database", &["current_database() -> VARCHAR"]),
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
    (
        "array_length",
        &["array_length(col0 ANY[]) -> BIGINT", "array_length(col0 ANY[], col1 BIGINT) -> BIGINT"],
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
    // The other variadic, and the one the pin prints with no leading parameter at all, which is why
    // the zero argument call binds there. See the entry in [`TABLE`].
    ("list_concat", &["list_concat([ANY[]...]) -> ANY[]"]),
    ("list_position", &["list_position(col0 T[], col1 T) -> INTEGER"]),
    ("list_contains", &["list_contains(col0 T[], col1 T) -> BOOLEAN"]),
    ("list_has_any", &["list_has_any(col0 T[], col1 T[]) -> BOOLEAN"]),
    ("list_has_all", &["list_has_all(col0 T[], col1 T[]) -> BOOLEAN"]),
    ("list_intersect", &["list_intersect(col0 T[], col1 T[]) -> T[]"]),
    ("list_distinct", &["list_distinct(col0 T[]) -> T[]"]),
    ("list_unique", &["list_unique(col0 ANY[]) -> UBIGINT"]),
    ("list_where", &["list_where(col0 T[], col1 BOOLEAN[]) -> T[]"]),
    ("list_select", &["list_select(col0 T[], col1 BIGINT[]) -> T[]"]),
    ("flatten", &["flatten(col0 T[][]) -> T[]"]),
    (
        "list_sort",
        &[
            "list_sort(list ANY[]) -> ANY[]",
            "list_sort(list ANY[], sort_order VARCHAR) -> ANY[]",
            "list_sort(list ANY[], sort_order VARCHAR, null_order VARCHAR) -> ANY[]",
        ],
    ),
    (
        "list_reverse_sort",
        &[
            "list_reverse_sort(list ANY[]) -> ANY[]",
            "list_reverse_sort(list ANY[], null_order VARCHAR) -> ANY[]",
        ],
    ),
    (
        "range",
        &[
            "\"range\"(col0 BIGINT) -> BIGINT[]",
            "\"range\"(col0 BIGINT, col1 BIGINT) -> BIGINT[]",
            "\"range\"(col0 BIGINT, col1 BIGINT, col2 BIGINT) -> BIGINT[]",
            "\"range\"(col0 TIMESTAMP, col1 TIMESTAMP, col2 INTERVAL) -> TIMESTAMP[]",
            "\"range\"(col0 TIMESTAMP WITH TIME ZONE, col1 TIMESTAMP WITH TIME ZONE, col2 \
             INTERVAL) -> TIMESTAMP WITH TIME ZONE[]",
        ],
    ),
    (
        "generate_series",
        &[
            "generate_series(col0 BIGINT) -> BIGINT[]",
            "generate_series(col0 BIGINT, col1 BIGINT) -> BIGINT[]",
            "generate_series(col0 BIGINT, col1 BIGINT, col2 BIGINT) -> BIGINT[]",
            "generate_series(col0 TIMESTAMP, col1 TIMESTAMP, col2 INTERVAL) -> TIMESTAMP[]",
            "generate_series(col0 TIMESTAMP WITH TIME ZONE, col1 TIMESTAMP WITH TIME ZONE, \
             col2 INTERVAL) -> TIMESTAMP WITH TIME ZONE[]",
        ],
    ),
    (
        "list_grade_up",
        &[
            "list_grade_up(list ANY[]) -> ANY[]",
            "list_grade_up(list ANY[], sort_order VARCHAR) -> ANY[]",
            "list_grade_up(list ANY[], sort_order VARCHAR, null_order VARCHAR) -> ANY[]",
        ],
    ),
    (
        "list_resize",
        &[
            "list_resize(col0 ANY[], col1 ANY) -> ANY[]",
            "list_resize(col0 ANY[], col1 ANY, col2 ANY) -> ANY[]",
        ],
    ),
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
    (
        "contains",
        &[
            "contains(col0 VARCHAR, col1 VARCHAR) -> BOOLEAN",
            "contains(col0 T[], col1 T) -> BOOLEAN",
            "contains(col0 MAP(K, V), col1 K) -> BOOLEAN",
            "contains(col0 TUPLE, col1 ANY) -> BOOLEAN",
        ],
    ),
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
    ("current_setting", &["current_setting(setting_name VARCHAR) -> ANY"]),
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
/// What the pin says about a slice with a step over a string, which is what `list_reverse` is on the
/// pin, unbalanced parenthesis and all.
const STEPPED_STRING: &str = "Slice with steps has not been implemented for string types, you can \
     consider rewriting your query as follows:\n SELECT array_to_string((str_split(string, \
     '')[begin:end:step], '');";

/// The element type of a list argument, or of an array, which is cast to a list, or the untyped
/// null for a null argument, and `None` for anything else.
fn element_of_list(ty: &LogicalType) -> Option<LogicalType> {
    match ty {
        LogicalType::Null => Some(LogicalType::Null),
        LogicalType::List(element) | LogicalType::Array(element, _) => Some((**element).clone()),
        _ => None,
    }
}

/// The one type two readings of `T` meet at, or the pin's sentence for two that do not. Only the
/// two argument shapes ask this.
fn deduce(entry: &Entry, first: &LogicalType, second: &LogicalType) -> Result<LogicalType> {
    first.promote(second).ok_or_else(|| {
        let (arguments, returns) = entry.shape.declared(2);
        Error::binder(format!(
            "Cannot deduce template type 'T' in function: '{}({}) -> {returns}'\nType 'T' was \
             inferred to be:\n - '{first}', from first occurrence\n - '{second}', which is \
             incompatible with previously inferred type!",
            entry.name,
            arguments.join(", ")
        ))
    })
}

/// The cast list and result for a function that answers in the type of its list argument: a list
/// of `element` when the argument is one, and the untyped null when the argument is a null.
fn listed_or_null(
    first: &LogicalType,
    element: LogicalType,
    mut rest: Vec<LogicalType>,
) -> (Vec<LogicalType>, LogicalType) {
    let (taken, returns) = match first {
        LogicalType::Null => (LogicalType::Null, LogicalType::Null),
        _ => {
            let list = LogicalType::list(element);
            (list.clone(), list)
        }
    };
    rest.insert(0, taken);
    (rest, returns)
}

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
///
/// A decimal keeps its scale and takes the widest width, for the integer's reason: a column of
/// `DECIMAL(3,1)` that holds 99.9 a hundred times sums to 9990.0, which does not fit in three
/// digits, and the pin answers `DECIMAL(38,1)` whatever the column's width was.
fn accumulator(ty: &LogicalType) -> LogicalType {
    match ty {
        _ if ty.is_integer() => LogicalType::HugeInt,
        LogicalType::Float => LogicalType::Double,
        LogicalType::Decimal { scale, .. } => {
            LogicalType::Decimal { width: MAX_DECIMAL_WIDTH, scale: *scale }
        }
        _ => ty.clone(),
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

/// What the elements of a list written out in a query meet at.
///
/// Promotion is the same rule [`promote_all`] uses and the two differences are both about the
/// untyped null. A list of nothing but nulls stays a list of the null type, because `typeof([NULL])`
/// on the pin is `"NULL"[]` rather than `INTEGER[]`, and a list of no items at all is the same thing
/// for the same reason. Everywhere else an untyped null has to land on a type the executor can hold
/// a vector of, and here the vector is the list rather than the element.
///
/// The message is the one the binder printed for a list that would not reconcile before a list was a
/// call, so the bracket and the written name say the same sentence. It is not the pin's sentence:
/// the pin reports a template type it could not deduce and names the literal types it inferred along
/// the way, which needs a notion of a literal type that rudb does not have. See #1338.
fn list_element(arguments: &[LogicalType]) -> Result<LogicalType> {
    let mut element = LogicalType::Null;
    for ty in arguments {
        element = element.promote(ty).ok_or_else(|| {
            Error::binder(format!("Cannot mix values of type {element} and type {ty} in a list"))
        })?;
    }
    Ok(element)
}

fn find(name: &str) -> Option<&'static Entry> {
    let name = canonical(name);
    TABLE.iter().find(|entry| entry.name.eq_ignore_ascii_case(name))
}

/// One overload of one function, as `duckdb_functions()` reports it.
///
/// An overload here is a name and an argument count, because that is what an entry in this crate's
/// table has one of each. Upstream has an overload per pair of argument types instead and so reports
/// 44 rows for `+`, and `types` is where the difference shows up. See [`function_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionRow {
    /// The name as it was written, which is the alias for an alias.
    pub name: &'static str,
    /// Scalar or aggregate.
    pub kind: FunctionKind,
    /// The name this one resolves to, and `None` for a name that is its own.
    pub alias_of: Option<&'static str>,
    /// One per argument, in order.
    pub types: Vec<&'static str>,
    /// What the call produces.
    pub returns: &'static str,
    /// The type of the trailing variadic argument, for the names that take one.
    pub varargs: Option<&'static str>,
}

/// Every name in the table and every argument count it takes, for `duckdb_functions()`.
///
/// The types here are declared types and not resolved ones, which is the whole difference between
/// this table and upstream's. The table in this module resolves by shape: `+` is one entry saying
/// both arguments promote and the result is what they promote to, where upstream carries an entry
/// per pair of numeric types because it carries an implementation per pair. So upstream reports 44
/// rows for `+` naming concrete types and this reports two, one per arity, with the type variable.
///
/// `T` is upstream's own spelling for an argument whose type the call decides, which it uses for
/// `list_extract` and `lag` and the rest of the generic functions, and it means the same thing here:
/// every argument spelled `T` in one row is the same type as every other. `ANY` is the weaker one
/// and means the argument is not constrained and not tied to the others, which is what `count(x)`
/// takes. A return of `ANY` means the type is decided by the arguments in a way a name cannot say,
/// which is where `sum` is, since it promotes and then widens an integer to the accumulator.
///
/// Rows come out in the order the table is written in, which is by family. The caller sorts.
///
/// [`resolve`]: crate::signature::resolve
#[must_use]
pub fn function_rows() -> Vec<FunctionRow> {
    let mut rows = Vec::new();
    for entry in TABLE {
        for count in entry.arity.every_count() {
            let (types, returns) = entry.shape.declared(count);
            rows.push(FunctionRow {
                name: entry.name,
                kind: entry.kind,
                alias_of: None,
                types,
                returns,
                varargs: entry.arity.open().then(|| entry.shape.declared(1).0[0]),
            });
        }
    }
    // An alias is a row of its own with the same shape, because a client reading this table to find
    // out whether `len` works wants a row for `len`. Upstream does the same and fills `alias_of`
    // with the name it resolves to, which is how this crate's list was read off in the first place.
    for (alias, real) in ALIASES {
        let mut aliased: Vec<FunctionRow> = rows
            .iter()
            .filter(|row| row.name == *real)
            .map(|row| FunctionRow { name: alias, alias_of: Some(real), ..row.clone() })
            .collect();
        rows.append(&mut aliased);
    }
    rows
}

impl Arity {
    /// Every argument count this accepts, with an open end reported as its shortest form.
    ///
    /// An open end is `concat` and friends, which take any number, and the row for one says so in
    /// `varargs` rather than by having a row per count up to some number nobody picked.
    ///
    /// An open end that starts at nothing is two rows rather than one, which is the pin's row set
    /// for `list_value` and is not an exception to the rule above. A row of no arguments names no
    /// type, so it cannot say what the result is made of, and the second row is where the type
    /// variable is introduced. `concat` starts at one and needs no such row.
    fn every_count(self) -> Vec<usize> {
        match self {
            Self::Exactly(count) => vec![count],
            Self::Between(least, Some(most)) => (least..=most).collect(),
            Self::Between(0, None) => vec![0, 1],
            Self::Between(least, None) => vec![least],
            Self::OneOf(counts) => counts.to_vec(),
        }
    }

    /// Whether the count has no upper end.
    const fn open(self) -> bool {
        matches!(self, Self::Between(_, None))
    }
}

impl Fixed {
    /// The name this type goes by in a catalog table, which is the name a cast spells.
    const fn name(self) -> &'static str {
        match self {
            Self::Boolean => "BOOLEAN",
            Self::Integer => "INTEGER",
            Self::BigInt => "BIGINT",
            Self::UBigInt => "UBIGINT",
            Self::Double => "DOUBLE",
            Self::Varchar => "VARCHAR",
            Self::Date => "DATE",
            Self::Time => "TIME",
            Self::TimeTz => "TIME WITH TIME ZONE",
            Self::Timestamp => "TIMESTAMP",
            Self::TimestampTz => "TIMESTAMP WITH TIME ZONE",
            Self::Interval => "INTERVAL",
        }
    }
}

/// The type variable, for an argument whose type the call decides and that every other argument
/// spelled the same way has to agree with.
const SAME: &str = "T";

/// An argument that is not constrained and is not tied to the others, or a result that the
/// arguments decide in a way no name can say.
const ANY: &str = "ANY";

/// A list of the type variable, which is what a list constructor gives back.
const SAME_LIST: &str = "T[]";

/// A list of the untyped null, which is what a list constructor with nothing in it gives back.
const NULL_LIST: &str = "\"NULL\"[]";

/// A list whose element type the call decides and that is not tied to the other arguments.
const ANY_LIST: &str = "ANY[]";

impl Shape {
    /// What the arguments and the result are declared to be, at this argument count.
    ///
    /// Not what a call resolves to. A shape that promotes says `T` here and works out the real type
    /// in [`resolve`] from what was passed, and a shape that widens a decimal says `ANY` for the
    /// result because the width is not in the name.
    fn declared(self, count: usize) -> (Vec<&'static str>, &'static str) {
        let all = |name: &'static str| vec![name; count];
        let leading = |taken: usize, first: &'static str, rest: &'static str| {
            (0..count).map(|at| if at < taken { first } else { rest }).collect::<Vec<_>>()
        };
        match self {
            // Promoting says `T` and the result is that same `T`, exactly.
            Self::Promoted | Self::PromotedToFirst | Self::Bitwise => (all(SAME), SAME),
            // Promoting and then moving: a decimal product is as wide as both operands, a decimal
            // quotient is a double, a decimal sum gains a carry digit and an integer sum widens to
            // the accumulator. The arguments still meet at one type and the result is no longer it.
            Self::Multiplied
            | Self::Divided
            | Self::Slashed
            | Self::PromotedWithCarry
            | Self::Accumulated => (all(SAME), ANY),
            Self::PromotedTo(fixed) => (all(SAME), fixed.name()),
            // The floor is what a shape that widens is declared as, which is the overload upstream
            // lists first and the one a call with nothing to say about its arguments lands on.
            Self::FixedTo(from, to)
            | Self::Exact(from, to)
            | Self::Widened(from, to)
            | Self::WidenedTogether(from, to) => (all(from.name()), to.name()),
            Self::AnyTo(fixed) => (all(ANY), fixed.name()),
            Self::LeadingFixedTo(taken, first, to) => {
                (leading(taken, first.name(), ANY), to.name())
            }
            Self::LeadingFixedToLast(first) => (leading(1, first.name(), SAME), SAME),
            // A subscript takes a string or a list and a whole number, and the whole number is not
            // cast to one, which is why it is spelled out rather than left as `ANY`.
            Self::Extracted => (leading(1, SAME, "BIGINT"), ANY),
            Self::Sliced => (leading(1, SAME, "BIGINT"), SAME),
            Self::TextThenIndex(taken, to) => {
                (leading(taken, Fixed::Varchar.name(), "BIGINT"), to.name())
            }
            // The value, then a row count, then another value of the first one's type. The third
            // one is `ANY` and not the spelling of the first, which is the pin's row for `lag` and
            // is where the declaration stops being the rule: the binder casts the default to the
            // column's type whatever the table says here.
            Self::ValueThenCountThenValue(spelled) => {
                let names = (0..count)
                    .map(|at| match at {
                        0 => spelled.name(),
                        1 => "BIGINT",
                        _ => ANY,
                    })
                    .collect();
                (names, spelled.name())
            }
            // One `ANY` in and one `ANY` out, which is the pin's row for `fill` and is the whole of
            // what it declares.
            Self::AsGiven => (all(ANY), ANY),
            // One overload with an `ANY` return, which is the pin's row for it. The name decides
            // the type and a name is not something a signature can hold.
            Self::Setting => (all(Fixed::Varchar.name()), ANY),
            // A list of what the arguments meet at, and a list of the null type when there are no
            // arguments to meet. Both rows are the pin's, which carries the two of them for this
            // name and nothing in between.
            Self::Listed => (all(SAME), if count == 0 { NULL_LIST } else { SAME_LIST }),
            // The string reading's row, which is the one the pin lists first for this name. The list
            // reading gets no row of its own because an entry here is a name and an argument count,
            // and this name at two arguments is already spoken for.
            Self::Concatenated => (all(Fixed::Varchar.name()), Fixed::Varchar.name()),
            // The pin's row, which is one variadic overload over lists of anything. The element type
            // is not `T` because the arguments do not have to agree on it, they promote to it.
            Self::ListConcatenated => (all(ANY_LIST), ANY_LIST),
            // The string row, which is the one the pin lists first for `length`. The list row is
            // the same name at the same count and an entry here has only one row per count.
            Self::Counted => (all(Fixed::Varchar.name()), Fixed::BigInt.name()),
            // Both of the pin's rows, the list alone and the list with a dimension.
            Self::ListCounted => (leading(1, ANY_LIST, "BIGINT"), Fixed::BigInt.name()),
            // No arguments, so `all` is empty whatever it is handed and only the result is named.
            Self::Constant(fixed) => (Vec::new(), fixed.name()),
            Self::ListSearched(to) => (leading(1, SAME_LIST, SAME), to.name()),
            Self::ListsMet(to) => (all(SAME_LIST), to.map_or(SAME_LIST, Fixed::name)),
            Self::ListKept => (all(SAME_LIST), SAME_LIST),
            Self::ListTo(to) => (all(ANY_LIST), to.name()),
            Self::ListPicked(by) => {
                let picks = match by {
                    Fixed::Boolean => "BOOLEAN[]",
                    _ => "BIGINT[]",
                };
                (leading(1, SAME_LIST, picks), SAME_LIST)
            }
            Self::Flattened => (all("T[][]"), SAME_LIST),
            Self::Resized => (leading(1, ANY_LIST, ANY), ANY_LIST),
            Self::Sorted => (leading(1, ANY_LIST, Fixed::Varchar.name()), ANY_LIST),
            Self::Graded => (leading(1, ANY_LIST, Fixed::Varchar.name()), ANY_LIST),
            Self::Ranged => (all("BIGINT"), "BIGINT[]"),
        }
    }
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
    ("array_agg", "list"),
    ("arbitrary", "first"),
    ("stddev", "stddev_samp"),
    ("variance", "var_samp"),
    ("group_concat", "string_agg"),
    ("listagg", "string_agg"),
    ("list_extract", "array_extract"),
    ("list_element", "array_extract"),
    ("list_slice", "array_slice"),
    ("list_pack", "list_value"),
    ("list_cat", "list_concat"),
    ("array_concat", "list_concat"),
    ("array_cat", "list_concat"),
    ("list_indexof", "list_position"),
    ("array_position", "list_position"),
    ("array_indexof", "list_position"),
    ("list_has", "list_contains"),
    ("array_contains", "list_contains"),
    ("array_has", "list_contains"),
    ("array_has_any", "list_has_any"),
    ("array_has_all", "list_has_all"),
    ("array_intersect", "list_intersect"),
    ("array_distinct", "list_distinct"),
    ("array_reverse", "list_reverse"),
    ("array_unique", "list_unique"),
    ("array_where", "list_where"),
    ("array_select", "list_select"),
    ("array_resize", "list_resize"),
    ("array_sort", "list_sort"),
    ("array_reverse_sort", "list_reverse_sort"),
    ("array_grade_up", "list_grade_up"),
    ("grade_up", "list_grade_up"),
    ("rank_dense", "dense_rank"),
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
    /// A list is counted now and anything that is neither a list nor a string is still refused.
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

    /// `date_part` says double whatever it reads and `date_trunc` hands back the type it was given,
    /// which is two answers that one shape cannot give and is why there are two new ones. A double
    /// rather than a bigint because that is upstream's declared overload, and the narrowing to a
    /// bigint happens in the binder, where the specifier can be looked at.
    #[test]
    fn a_date_function_fixes_the_part_and_leaves_the_date_alone() {
        let part = resolve("date_part", &[LogicalType::Varchar, LogicalType::Timestamp])
            .expect("a part of a timestamp");
        assert_eq!(part.returns, LogicalType::Double);
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
            // The one row that is meant not to resolve, because the binder answers the call before
            // it gets here and the only way here is the case upstream refuses. It has a test of its
            // own below rather than an exception with nothing behind it.
            if entry.shape == Shape::Setting {
                continue;
            }
            for count in entry.arity.counts() {
                // A shape that names the type it wants is asked for it, since `chr` wants an
                // INTEGER and refuses a string the way upstream does.
                let ty = match (entry.numeric_only, entry.shape) {
                    (
                        _,
                        Shape::Exact(argument, _)
                        | Shape::Widened(argument, _)
                        | Shape::WidenedTogether(argument, _),
                    ) => argument.ty(),
                    // Only lists go in, so it is asked with a list of the same string the rest are.
                    (_, Shape::ListConcatenated | Shape::ListCounted) => {
                        LogicalType::list(LogicalType::Varchar)
                    }
                    // The bit aggregates take whole numbers and nothing else.
                    (true, _) | (_, Shape::Bitwise) => LogicalType::Integer,
                    (false, _) => LogicalType::Varchar,
                };
                let mut arguments = vec![ty; count];
                // A subscript and a substring are the shapes whose arguments are not all alike. The
                // leading ones are the string or the list and everything after them is a whole
                // number, so a row of strings is not a call either one accepts and not a call worth
                // asserting it accepts.
                let leading = match entry.shape {
                    Shape::Extracted | Shape::Sliced | Shape::ListCounted | Shape::Resized => 1,
                    Shape::TextThenIndex(leading, _) => leading,
                    _ => count,
                };
                for bound in arguments.iter_mut().skip(leading) {
                    *bound = LogicalType::BigInt;
                }
                // The list functions each want their own mix of lists and values.
                let strings = || LogicalType::list(LogicalType::Varchar);
                match entry.shape {
                    Shape::ListSearched(_) => arguments = vec![strings(), LogicalType::Varchar],
                    Shape::ListsMet(_) | Shape::ListKept | Shape::ListTo(_) => {
                        arguments = vec![strings(); count];
                    }
                    Shape::ListPicked(by) => {
                        arguments = vec![strings(), LogicalType::list(by.ty())]
                    }
                    Shape::Flattened => arguments = vec![LogicalType::list(strings())],
                    Shape::Resized => arguments[0] = strings(),
                    Shape::Ranged => arguments = vec![LogicalType::BigInt; count],
                    Shape::Sorted | Shape::Graded => {
                        arguments = vec![LogicalType::Varchar; count];
                        arguments[0] = strings();
                    }
                    _ => {}
                }
                resolve(entry.name, &arguments).unwrap_or_else(|error| {
                    panic!("{} does not resolve at {count} arguments: {error}", entry.name)
                });
            }
        }
    }

    /// The three answers the pin gives a call to `current_setting`, read off `v2.0.0-dev84237`.
    ///
    /// The right number of arguments and a name the binder could not fold is the constant
    /// expression sentence, and a wrong number is the ordinary arity error with the one overload
    /// listed under it. The folded case is not here because it never reaches this table.
    #[test]
    fn a_setting_read_from_a_column_is_refused_in_the_pins_words() {
        let error = resolve("current_setting", &[LogicalType::Varchar]).expect_err("is refused");
        assert_eq!(
            error.to_string(),
            "Binder Error: The \"setting_name\" argument in function \"current_setting\" must be a constant expression"
        );
        let none = resolve("current_setting", &[]).expect_err("takes one argument");
        assert_eq!(
            none.to_string(),
            "Binder Error: No function matches the given name and argument types 'current_setting()'. \
             You might need to add explicit type casts.\n\tCandidate functions:\n\tcurrent_setting(setting_name VARCHAR) -> ANY\n"
        );
    }

    /// One row with an `ANY` return, which is what the pin's `duckdb_functions()` says about it.
    #[test]
    fn a_setting_is_declared_over_a_string_and_returns_anything() {
        let row = function_rows()
            .into_iter()
            .find(|row| row.name == "current_setting")
            .expect("a row for it");
        assert_eq!(row.types, ["VARCHAR"]);
        assert_eq!(row.returns, "ANY");
        assert_eq!(row.varargs, None);
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
