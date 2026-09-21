//! The printer and the reader as a fixed point, over plans nobody wrote by hand.
//!
//! `roundtrip.rs` covers the shapes a real query produces, which is the set somebody thought of.
//! This covers the set nobody thought of. It builds a plan out of a seeded generator, prints it,
//! reads it back, prints that, and demands the two dumps are the same string. Two thousand seeds
//! run in half a second because a plan is three vectors and the printer never touches a disk.
//!
//! The generator is most of the test and so it is held to the same standard as the thing it tests.
//! Every plan it builds passes [`Plan::validate`] before it is printed, because a generator that
//! quietly builds malformed plans is asserting that the printer does something sensible with
//! garbage rather than that it is a fixed point. Every operator and every expression form has to
//! come out of it, and so does every piece of text the printer has to escape, both of which are
//! their own tests below. The failure mode of a generator is not that it breaks, it is that it
//! stops reaching something and nobody notices.
//!
//! A failure replays from the seed in the message and nothing else. `RUDB_PLAN_SEED` sets where
//! the run starts and `RUDB_PLAN_SEEDS` how many it does, so the same test is a fast pre merge
//! check by default and a long run on a machine that has the time for one.

use rudb_common::{Field, LogicalType, Value};
use rudb_plan::{
    Arm, Bound, BuildSide, ColumnBinding, CompareOp, ConjunctionOp, Expr, ExprRef, JoinKind, Node,
    NodeRef, Plan, SetOpKind, Slice, SortKey, StrRef, WindowBound, WindowExclude, WindowFrame,
    WindowUnit,
};

/// How many seeds the property runs over when nothing says otherwise.
const SEEDS: u64 = 2000;

/// How deep a generated plan goes, counted in operators from the root.
const PLAN_DEPTH: usize = 4;

/// How deep a generated expression goes.
const EXPR_DEPTH: usize = 4;

/// The seeds one run covers.
fn seeds() -> std::ops::RangeInclusive<u64> {
    let first = setting("RUDB_PLAN_SEED", 1);
    let count = setting("RUDB_PLAN_SEEDS", SEEDS).max(1);
    first..=first.saturating_add(count - 1)
}

fn setting(name: &str, fallback: u64) -> u64 {
    match std::env::var(name) {
        Ok(text) => text
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{name} is {text}, which is not a number")),
        Err(_) => fallback,
    }
}

#[test]
fn a_generated_plan_prints_and_reads_back_as_itself() {
    for seed in seeds() {
        let plan = Generator::new(seed).plan();
        plan.validate()
            .unwrap_or_else(|error| panic!("seed {seed} built a plan that is not valid: {error}"));
        let first = plan.to_string();
        let read = Plan::parse(&first).unwrap_or_else(|error| {
            panic!("seed {seed} printed a plan that cannot be read back:\n{first}\n{error}")
        });
        read.validate().unwrap_or_else(|error| {
            panic!("seed {seed} read back a plan that is not valid:\n{first}\n{error}")
        });
        assert_eq!(first, read.to_string(), "seed {seed} changed on the way through the reader");
    }
}

/// `Plan::operator` is what `EXPLAIN` puts on a line and the whole dump is what the round trip is
/// about, so the two printers agreeing is what keeps an `EXPLAIN` readable by the plan reader.
#[test]
fn one_operator_prints_the_same_alone_as_it_does_in_a_dump() {
    for seed in seeds() {
        let plan = Generator::new(seed).plan();
        let dump = plan.to_string();
        let mut lines = dump.lines();
        walk(&plan, plan.root(), &mut lines, seed);
        assert!(lines.next().is_none(), "seed {seed} printed a line for no operator");
    }
}

/// Parent before children, which is the order the dump is in, so the lines line up with the walk.
fn walk(plan: &Plan, node: NodeRef, lines: &mut std::str::Lines<'_>, seed: u64) {
    let line = lines.next().unwrap_or_else(|| panic!("seed {seed} ran out of lines"));
    assert_eq!(
        line.trim_start(),
        plan.operator(node),
        "seed {seed} prints node {node} differently on its own"
    );
    for child in plan.node(node).children().into_iter().flatten() {
        walk(plan, child, lines, seed);
    }
}

/// The generator's own test. A generated corpus is worth what it reaches and nothing more, so the
/// set of operators and expression forms it produces is asserted rather than assumed. Adding a
/// node to [`Node`] and not to the generator fails here rather than passing quietly.
#[test]
fn the_generator_reaches_every_operator_and_every_expression_form() {
    let mut operators: Vec<&'static str> = Vec::new();
    let mut forms: Vec<&'static str> = Vec::new();
    for seed in 1..=SEEDS {
        let plan = Generator::new(seed).plan();
        for node in 0..plan.node_count() {
            let keyword = plan.node(node as NodeRef).keyword();
            if !operators.contains(&keyword) {
                operators.push(keyword);
            }
        }
        for expr in 0..plan.expr_count() {
            let form = form_of(plan.expr(expr as ExprRef));
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
    }
    operators.sort_unstable();
    forms.sort_unstable();
    assert_eq!(
        operators,
        [
            "Aggregate",
            "CrossProduct",
            "DependentJoin",
            "Distinct",
            "Dummy",
            "Fetch",
            "Filter",
            "Get",
            "Join",
            "Limit",
            "Project",
            "SetOp",
            "Sort",
            "TableFetch",
            "TableFunction",
            "TopN",
            "Values",
            "Window",
        ],
        "the generator does not build every operator"
    );
    assert_eq!(
        forms,
        [
            "Aggregate",
            "Case",
            "Cast",
            "Column",
            "Compare",
            "Conjunction",
            "Constant",
            "Function",
            "Window",
        ],
        "the generator does not build every expression form"
    );
}

/// Whether a constant of this type can be written as anything other than a null.
///
/// The struct case is the reason this exists. A struct value takes its type from the values inside
/// it, so a struct with a null field is a struct whose type says `NULL` where a field type should
/// be, and the reader cannot make a value of that.
fn has_a_form(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Boolean
        | LogicalType::TinyInt
        | LogicalType::SmallInt
        | LogicalType::Integer
        | LogicalType::BigInt
        | LogicalType::HugeInt
        | LogicalType::UTinyInt
        | LogicalType::USmallInt
        | LogicalType::UInteger
        | LogicalType::UBigInt
        | LogicalType::UHugeInt
        | LogicalType::Float
        | LogicalType::Double
        | LogicalType::Decimal { .. }
        | LogicalType::Varchar
        | LogicalType::Blob
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::Interval
        | LogicalType::List(_) => true,
        LogicalType::Struct(fields) => fields.iter().all(|field| has_a_form(&field.ty)),
        _ => false,
    }
}

fn form_of(expr: &Expr) -> &'static str {
    match expr {
        Expr::Column(_) => "Column",
        Expr::Constant(_) => "Constant",
        Expr::Cast { .. } => "Cast",
        Expr::Compare { .. } => "Compare",
        Expr::Conjunction { .. } => "Conjunction",
        Expr::Function { .. } => "Function",
        Expr::Aggregate { .. } => "Aggregate",
        Expr::Window { .. } => "Window",
        Expr::Case { .. } => "Case",
    }
}

/// A xorshift, so a plan is a function of its seed on every host and in every run without the
/// workspace growing a dependency for it. The same generator the encoding tests use.
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        // Zero is the one state a xorshift cannot leave, and seed zero is the one a person types.
        Self(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A number below `bound`, which has to be positive.
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    /// A count between `low` and `high`, both included.
    fn count(&mut self, low: usize, high: usize) -> usize {
        low + self.below(high - low + 1)
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.next() % one_in == 0
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }

    /// One of `from`, borrowed, for the things a plan holds that are not `Copy`.
    fn choose<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// Names a plan can hold. The last three are here because they do not print as themselves: a name
/// with a space, a name with a quote in it and a name that is a keyword all have to come back
/// through the reader as the bytes they went in as.
const NAMES: [&str; 13] = [
    "a",
    "b",
    "x",
    "hits",
    "SearchPhrase",
    "count",
    "Search Phrase",
    "a\"b",
    "select",
    "",
    "a,b",
    "a(b)",
    "#0.1",
];

/// Scalar function names. `CAST` and `CASE` are here on purpose: they are the two words the reader
/// dispatches on, so a function that happens to be called one of them is the case the quoting rule
/// exists for.
const FUNCTIONS: [&str; 7] = ["+", "upper", "date_part", "list_value", "CAST", "CASE", "a\"b"];

/// Aggregate names, including the no argument one, which prints without a space where the
/// arguments would be.
const AGGREGATES: [&str; 5] = ["count_star", "count", "sum", "min", "string_agg"];

/// Strings a `VARCHAR` constant can hold. Every one of them is a thing the writer escapes: a
/// quote, a backslash, a control character that would otherwise split one operator over two lines,
/// a brace and a comma that would otherwise end an element of a braced list early, and text that
/// is not ASCII.
const TEXTS: [&str; 11] = [
    "",
    "ada",
    "it's",
    "back\\slash",
    "one\ntwo",
    "{a, b}",
    "ends'",
    "grüß ganz",
    "\r\ttabs",
    "\u{0}",
    "a::b",
];

/// The types a column can have, which is every type the printer has a form for, whether or not a
/// constant of it can be built. A column of a type with no value representation is a plan the
/// binder produces the moment the type exists, so the annotation has to survive the reader first.
static LEAVES: [LogicalType; 24] = [
    LogicalType::Boolean,
    LogicalType::TinyInt,
    LogicalType::SmallInt,
    LogicalType::Integer,
    LogicalType::BigInt,
    LogicalType::HugeInt,
    LogicalType::UTinyInt,
    LogicalType::USmallInt,
    LogicalType::UInteger,
    LogicalType::UBigInt,
    LogicalType::UHugeInt,
    LogicalType::Float,
    LogicalType::Double,
    LogicalType::Varchar,
    LogicalType::Blob,
    LogicalType::Bit,
    LogicalType::Uuid,
    LogicalType::Date,
    LogicalType::Time,
    LogicalType::TimeTz,
    LogicalType::Timestamp,
    LogicalType::TimestampS,
    LogicalType::TimestampMs,
    LogicalType::Interval,
];

struct Generator {
    plan: Plan,
    random: Random,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self { plan: Plan::new(), random: Random::new(seed) }
    }

    fn plan(mut self) -> Plan {
        let root = self.node(PLAN_DEPTH);
        self.plan.set_root(root);
        self.plan
    }

    // Names, types and values.

    fn name(&mut self) -> StrRef {
        let text = self.random.pick(&NAMES);
        self.plan.intern(text)
    }

    fn index(&mut self) -> u32 {
        self.random.below(4) as u32
    }

    /// A type, nested up to `depth` levels of list, array and struct.
    fn ty(&mut self, depth: usize) -> LogicalType {
        if depth == 0 || self.random.below(4) != 0 {
            return self.random.choose(&LEAVES).clone();
        }
        match self.random.below(4) {
            0 => LogicalType::List(Box::new(self.ty(depth - 1))),
            1 => LogicalType::Array(Box::new(self.ty(depth - 1)), self.random.count(1, 8) as u32),
            2 => {
                let scale = self.random.count(0, 6) as u8;
                let width = scale + self.random.count(1, 12) as u8;
                LogicalType::Decimal { width, scale }
            }
            _ => {
                let fields = (0..self.random.count(1, 3))
                    .map(|_| {
                        let name = self.random.pick(&NAMES);
                        Field::new(name, self.ty(depth - 1))
                    })
                    .collect();
                LogicalType::Struct(fields)
            }
        }
    }

    /// A value of exactly `ty`, so that the constant holding it and the type written beside it
    /// cannot disagree.
    ///
    /// A type with no value form comes back as a null, which is what the reader does with one too:
    /// `NULL::BIT` is the only `BIT` constant either side can write, so a round trip over it is a
    /// round trip rather than a hole. A struct is a null whenever any field of it is one of those,
    /// because a struct takes its type from what is inside it and a null field would type itself.
    fn value_of(&mut self, ty: &LogicalType) -> Value {
        let number = self.random.next();
        match ty {
            LogicalType::Boolean => Value::Boolean(number % 2 == 0),
            LogicalType::TinyInt => Value::TinyInt(number as i8),
            LogicalType::SmallInt => Value::SmallInt(number as i16),
            LogicalType::Integer => Value::Integer(number as i32),
            LogicalType::BigInt => Value::BigInt(number as i64),
            LogicalType::HugeInt => Value::HugeInt(i128::from(number as i64)),
            LogicalType::UTinyInt => Value::UTinyInt(number as u8),
            LogicalType::USmallInt => Value::USmallInt(number as u16),
            LogicalType::UInteger => Value::UInteger(number as u32),
            LogicalType::UBigInt => Value::UBigInt(number),
            LogicalType::UHugeInt => Value::UHugeInt(u128::from(number)),
            // The four that have no digits are in here rather than left out, because the shortest
            // text that reads back as the same bits is exactly what the printer claims to write and
            // these are the four where that claim is worth something.
            LogicalType::Float => {
                Value::Float(self.random.pick(&[f32::NAN, f32::INFINITY, -0.0, 1.5, number as f32]))
            }
            LogicalType::Double => Value::Double(self.random.pick(&[
                f64::NAN,
                f64::NEG_INFINITY,
                -0.0,
                0.1,
                number as f64,
            ])),
            LogicalType::Varchar => Value::Varchar(self.random.pick(&TEXTS).to_string()),
            LogicalType::Blob => {
                Value::Blob((0..self.random.below(5)).map(|at| (number >> at) as u8).collect())
            }
            LogicalType::Date => Value::Date(number as i32),
            LogicalType::Time => Value::Time(number as i64),
            LogicalType::Timestamp => Value::Timestamp(number as i64),
            LogicalType::Interval => Value::Interval {
                months: number as i32,
                days: (number >> 32) as i32,
                micros: number as i64,
            },
            LogicalType::Decimal { width, scale } => Value::Decimal {
                unscaled: i128::from(number as i64) % 10_i128.pow(u32::from(*width)),
                width: *width,
                scale: *scale,
            },
            LogicalType::List(element) => {
                let element = element.as_ref().clone();
                let values =
                    (0..self.random.below(4)).map(|_| self.value_of(&element)).collect::<Vec<_>>();
                Value::List { element, values }
            }
            LogicalType::Struct(fields) if fields.iter().all(|field| has_a_form(&field.ty)) => {
                let fields = fields.clone();
                let held = fields
                    .iter()
                    .map(|field| (field.name.clone(), self.value_of(&field.ty)))
                    .collect();
                Value::Struct(held)
            }
            // Bit, Uuid, the fixed length array and the time zone and sub second timestamps have
            // no value form, and a null is the one constant of them that can be written.
            _ => Value::Null,
        }
    }

    // Expressions.

    /// An expression of exactly `ty`.
    fn of_type(&mut self, ty: &LogicalType, depth: usize) -> ExprRef {
        let boolean = *ty == LogicalType::Boolean;
        let arms = if depth == 0 {
            2
        } else if boolean {
            6
        } else {
            4
        };
        match self.random.below(arms) {
            0 => {
                let binding = ColumnBinding::new(self.index(), self.random.below(6) as u32);
                self.plan.add_expr(Expr::Column(binding), ty.clone())
            }
            1 => {
                let value = if self.random.chance(3) { Value::Null } else { self.value_of(ty) };
                let held = self.plan.add_value(value);
                self.plan.add_expr(Expr::Constant(held), ty.clone())
            }
            2 => {
                let from = self.ty(1);
                let input = self.of_type(&from, depth - 1);
                let try_cast = self.random.chance(2);
                self.plan.add_expr(Expr::Cast { input, try_cast }, ty.clone())
            }
            3 => {
                let name = self.plan.intern(self.random.pick(&FUNCTIONS));
                let count = self.random.below(4);
                let args = self.exprs(count, depth - 1);
                self.plan.add_expr(Expr::Function { name, args }, ty.clone())
            }
            4 => {
                let op = self.random.pick(&[
                    CompareOp::Equal,
                    CompareOp::NotEqual,
                    CompareOp::Less,
                    CompareOp::LessOrEqual,
                    CompareOp::Greater,
                    CompareOp::GreaterOrEqual,
                    CompareOp::DistinctFrom,
                    CompareOp::NotDistinctFrom,
                ]);
                let operand = self.ty(1);
                let left = self.of_type(&operand, depth - 1);
                let right = self.of_type(&operand, depth - 1);
                self.plan.add_expr(Expr::Compare { op, left, right }, LogicalType::Boolean)
            }
            _ => {
                let op = self.random.pick(&[ConjunctionOp::And, ConjunctionOp::Or]);
                let count = self.random.count(2, 3);
                let children = self.booleans(count, depth - 1);
                self.plan.add_expr(Expr::Conjunction { op, children }, LogicalType::Boolean)
            }
        }
    }

    /// An expression of a type the caller does not care about.
    ///
    /// A `CASE` lives here rather than in [`Self::of_type`] because every arm of it and the `ELSE`
    /// have to be the same type as the whole thing, so it is cheapest to build where the type is
    /// chosen rather than where it is handed down.
    fn any(&mut self, depth: usize) -> ExprRef {
        let ty = self.ty(1);
        if depth > 0 && self.random.below(5) == 0 {
            let arms = (0..self.random.count(1, 3))
                .map(|_| {
                    let when = self.of_type(&LogicalType::Boolean, depth - 1);
                    let then = self.of_type(&ty, depth - 1);
                    Arm { when, then }
                })
                .collect::<Vec<_>>();
            let otherwise =
                if self.random.chance(2) { Some(self.of_type(&ty, depth - 1)) } else { None };
            let arms = self.plan.add_arms(&arms);
            return self.plan.add_expr(Expr::Case { arms, otherwise }, ty);
        }
        self.of_type(&ty, depth)
    }

    /// An aggregate, which is legal only in the aggregate list of an [`Node::Aggregate`] and so is
    /// built only there.
    fn aggregate(&mut self, depth: usize) -> ExprRef {
        let name = self.plan.intern(self.random.pick(&AGGREGATES));
        let count = self.random.below(3);
        let args = self.exprs(count, depth);
        let distinct = self.random.chance(3);
        let filter = if self.random.chance(3) {
            Some(self.of_type(&LogicalType::Boolean, depth))
        } else {
            None
        };
        let ty = self.ty(0);
        self.plan.add_expr(Expr::Aggregate { name, args, distinct, filter }, ty)
    }

    fn window(&mut self, depth: usize) -> ExprRef {
        let name = self.plan.intern(self.random.pick(&AGGREGATES));
        let count = self.random.below(3);
        let args = self.exprs(count, depth);
        let distinct = self.random.chance(3);
        let filter = if self.random.chance(3) {
            Some(self.of_type(&LogicalType::Boolean, depth))
        } else {
            None
        };
        let ignore_nulls = self.random.chance(3);
        let ty = self.ty(0);
        self.plan.add_expr(Expr::Window { name, args, distinct, filter, ignore_nulls }, ty)
    }

    fn exprs(&mut self, count: usize, depth: usize) -> Slice {
        let list = (0..count).map(|_| self.any(depth)).collect::<Vec<_>>();
        self.plan.add_expr_list(&list)
    }

    fn booleans(&mut self, count: usize, depth: usize) -> Slice {
        let list =
            (0..count).map(|_| self.of_type(&LogicalType::Boolean, depth)).collect::<Vec<_>>();
        self.plan.add_expr_list(&list)
    }

    fn names(&mut self, count: usize) -> Slice {
        let list = (0..count).map(|_| self.name()).collect::<Vec<_>>();
        self.plan.add_name_list(&list)
    }

    fn fields(&mut self, count: usize) -> Slice {
        let list = (0..count)
            .map(|_| {
                let name = self.random.pick(&NAMES);
                Field::new(name, self.ty(2))
            })
            .collect::<Vec<_>>();
        self.plan.add_fields(&list)
    }

    fn keys(&mut self, depth: usize) -> Slice {
        let list = (0..self.random.count(1, 3))
            .map(|_| SortKey {
                expr: self.any(depth),
                descending: self.random.chance(2),
                nulls_first: self.random.chance(2),
            })
            .collect::<Vec<_>>();
        self.plan.add_sort_keys(&list)
    }

    // Operators.

    /// A leaf, which is an operator with no input. The three that are not [`Node::Dummy`] are what
    /// rows come from, so a plan of any height has one of these under it.
    fn leaf(&mut self) -> Node {
        match self.random.below(4) {
            0 => Node::Dummy,
            1 => {
                let catalog = self.name();
                let schema = self.name();
                let table = self.name();
                let alias = self.name();
                let index = self.index();
                let count = self.random.count(1, 3);
                let columns = self.fields(count);
                Node::Get { catalog, schema, table, alias, index, columns }
            }
            2 => {
                let width = self.random.count(1, 3);
                let columns = self.fields(width);
                let rows =
                    (0..self.random.count(1, 3)).map(|_| self.exprs(width, 1)).collect::<Vec<_>>();
                let rows = self.plan.add_rows(&rows);
                let index = self.index();
                Node::Values { index, columns, rows }
            }
            _ => {
                let function = self.name();
                let count = self.random.below(3);
                let args = self.exprs(count, 1);
                let named = self.random.below(3);
                let options = self.names(named);
                let settings = self.exprs(named, 1);
                let count = self.random.count(1, 3);
                let columns = self.fields(count);
                let index = self.index();
                Node::TableFunction { index, function, args, options, settings, columns }
            }
        }
    }

    fn node(&mut self, depth: usize) -> NodeRef {
        if depth == 0 {
            let leaf = self.leaf();
            return self.plan.add_node(leaf);
        }
        let node = match self.random.below(15) {
            0 => {
                let input = self.node(depth - 1);
                let predicate = self.of_type(&LogicalType::Boolean, EXPR_DEPTH);
                Node::Filter { input, predicate }
            }
            1 => {
                let input = self.node(depth - 1);
                let count = self.random.count(1, 3);
                let exprs = self.exprs(count, EXPR_DEPTH);
                let names = self.names(count);
                let index = self.index();
                Node::Project { input, index, exprs, names }
            }
            2 => {
                let input = self.node(depth - 1);
                let count = self.random.below(3);
                let groups = self.exprs(count, EXPR_DEPTH);
                let list = (0..self.random.count(1, 2))
                    .map(|_| self.aggregate(EXPR_DEPTH - 1))
                    .collect::<Vec<_>>();
                let aggregates = self.plan.add_expr_list(&list);
                let index = self.index();
                Node::Aggregate { input, index, groups, aggregates }
            }
            3 => {
                let input = self.node(depth - 1);
                let keys = self.keys(EXPR_DEPTH);
                Node::Sort { input, keys }
            }
            14 => {
                let input = self.node(depth - 1);
                let count = self.random.below(3);
                let partition = self.exprs(count, EXPR_DEPTH);
                let order = self.keys(EXPR_DEPTH);
                let start = if self.random.chance(2) {
                    WindowBound::UnboundedPreceding
                } else {
                    let offset = self.of_type(&LogicalType::Integer, EXPR_DEPTH);
                    WindowBound::Preceding(offset)
                };
                let end = if self.random.chance(2) {
                    WindowBound::CurrentRow
                } else {
                    let offset = self.of_type(&LogicalType::Integer, EXPR_DEPTH);
                    WindowBound::Following(offset)
                };
                let frame = WindowFrame {
                    unit: self.random.pick(&[
                        WindowUnit::Rows,
                        WindowUnit::Range,
                        WindowUnit::Groups,
                    ]),
                    start,
                    end,
                    exclude: self.random.pick(&[
                        WindowExclude::NoOthers,
                        WindowExclude::CurrentRow,
                        WindowExclude::Group,
                        WindowExclude::Ties,
                    ]),
                };
                let list = (0..self.random.count(1, 2))
                    .map(|_| self.window(EXPR_DEPTH - 1))
                    .collect::<Vec<_>>();
                let expressions = self.plan.add_expr_list(&list);
                let index = self.index();
                Node::Window { input, index, partition, order, frame, expressions }
            }
            4 => {
                let input = self.node(depth - 1);
                let count = if self.random.chance(4) {
                    Bound::All
                } else {
                    Bound::Rows(self.random.next() % 50)
                };
                let offset = Bound::Rows(self.random.next() % 20);
                Node::Limit { input, count, offset }
            }
            5 => {
                let input = self.node(depth - 1);
                let keys = self.keys(EXPR_DEPTH);
                let count = self.random.next() % 50;
                let offset = self.random.next() % 20;
                Node::TopN { input, keys, count, offset }
            }
            6 => {
                let input = self.node(depth - 1);
                let path = self.value_of(&LogicalType::Varchar);
                let held = self.plan.add_value(path);
                let arg = self.plan.add_expr(Expr::Constant(held), LogicalType::Varchar);
                let args = self.plan.add_expr_list(&[arg]);
                let binding = ColumnBinding::new(self.index(), self.random.below(6) as u32);
                let row = self.plan.add_expr(Expr::Column(binding), LogicalType::BigInt);
                let count = self.random.count(1, 3);
                let columns = self.fields(count);
                let index = self.index();
                Node::Fetch { input, index, args, columns, row }
            }
            7 => {
                let input = self.node(depth - 1);
                let count = self.random.below(3);
                let on = self.exprs(count, EXPR_DEPTH);
                Node::Distinct { input, on }
            }
            8 => {
                let input = self.node(depth - 1);
                let catalog = self.plan.intern("memory");
                let schema = self.plan.intern("main");
                let table = self.plan.intern("t");
                let binding = ColumnBinding::new(self.index(), self.random.below(6) as u32);
                let row = self.plan.add_expr(Expr::Column(binding), LogicalType::BigInt);
                let count = self.random.count(1, 3);
                let columns = self.fields(count);
                let index = self.index();
                Node::TableFetch { input, index, catalog, schema, table, columns, row }
            }
            9 | 10 => {
                let left = self.node(depth - 1);
                let right = self.node(depth - 1);
                let kind = self.random.pick(&[
                    JoinKind::Inner,
                    JoinKind::Left,
                    JoinKind::Right,
                    JoinKind::Full,
                    JoinKind::Semi,
                    JoinKind::Anti,
                    JoinKind::Single,
                    JoinKind::Positional,
                ]);
                let count = self.random.below(3);
                let conditions = self.booleans(count, EXPR_DEPTH);
                let build = self.random.pick(&[BuildSide::Right, BuildSide::Left]);
                Node::Join { left, right, kind, conditions, build }
            }
            11 => {
                let left = self.node(depth - 1);
                let right = self.node(depth - 1);
                let kind = self.random.pick(&[
                    JoinKind::Inner,
                    JoinKind::Left,
                    JoinKind::Semi,
                    JoinKind::Anti,
                    JoinKind::Single,
                    JoinKind::Mark,
                ]);
                let count = self.random.below(3);
                let conditions = self.booleans(count, EXPR_DEPTH);
                Node::DependentJoin { left, right, kind, conditions }
            }
            12 => {
                let left = self.node(depth - 1);
                let right = self.node(depth - 1);
                Node::CrossProduct { left, right }
            }
            _ => {
                let left = self.node(depth - 1);
                let right = self.node(depth - 1);
                let kind =
                    self.random.pick(&[SetOpKind::Union, SetOpKind::Except, SetOpKind::Intersect]);
                let all = self.random.chance(2);
                let index = self.index();
                Node::SetOp { left, right, kind, all, index }
            }
        };
        self.plan.add_node(node)
    }
}

/// The other half of the generator's own test. An operator list says the generator still builds a
/// `Filter`; it does not say the `Filter` still has anything hard in it. These are the pieces of
/// text the printer has to escape or quote to stay readable, so a generator that stops producing
/// one of them has stopped testing the part of the reader that is worth testing.
#[test]
fn the_generated_dumps_hold_every_form_the_printer_has_to_escape() {
    let mut all = String::new();
    for seed in 1..=SEEDS {
        all.push_str(&Generator::new(seed).plan().to_string());
    }
    for probe in [
        // Types with a shape of their own, and the one that is only ever a null.
        "DECIMAL(",
        "STRUCT(",
        "[]",
        "TIME WITH TIME ZONE",
        "NULL::",
        // Numbers with no digits in them, which is what the shortest round tripping text is for.
        "NaN",
        "inf",
        "-0.0",
        // Text and bytes that would otherwise end something early or start a new line.
        "''",
        "\\x0a",
        "\\x00",
        "X'",
        "grüß",
        // Names that do not print as themselves.
        "\"Search Phrase\"",
        "\"a\"\"b\"",
        "\"\"",
        "\"a,b\"",
        "\"CAST\"",
        // Expression forms with a keyword in the middle rather than an operator.
        "CASE",
        "TRY_CAST",
        "DISTINCT ",
        "FILTER ",
        "IS DISTINCT FROM",
        "count_star()",
        // Operator arguments that are a word rather than a number.
        "ALL offset",
        "NULLS FIRST",
    ] {
        assert!(all.contains(probe), "no generated plan holds {probe}");
    }
}
