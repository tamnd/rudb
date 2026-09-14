//! Build a well formed AST out of fuzzer bytes and push it through the binder and the optimizer.
//!
//! The other target starts at raw bytes, so almost everything it produces dies in the tokenizer or
//! in the rule table and the budget that reaches the binder is what is left over. This one starts
//! where that one gives up. The input is not a query, it is the shape of one, so every input that
//! is long enough to build anything at all arrives at the binder as a tree the transformer could
//! have produced.
//!
//! What it does not do is derive `Arbitrary` on the AST node types, which is the obvious reading of
//! "structure aware" and is wrong here. The AST is arenas of `u32` indices, and the invariant that
//! matters is not which variant a node is, it is that every index in it points at a node that
//! exists. A derived `Ast` would be arenas of random indices, the first thing to walk it would
//! panic on an index out of range, and every crash it reported would be a bug in the generator.
//! So the generator builds bottom up out of an `Unstructured` instead: children exist before the
//! node that names them, and a reference to something that is not there cannot be spelled.
//!
//! The oracle is better here than in the other target too. A panic still counts, and so does a hang
//! and so does memory growth, but the optimizer already checks itself when debug assertions are on,
//! which is the state `cargo fuzz` builds in: every pass has to leave the plan valid, none of them
//! may change how many columns a query returns, and running the whole sequence twice has to change
//! nothing. So this is not only a crash hunt. It is the idempotence and column count properties of
//! twenty odd rewrites, over trees that nobody wrote by hand.

#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use rudb_bind::{Bound, bind_statement};
use rudb_catalog::{Catalog, QualifiedName};
use rudb_common::{Field, LogicalType};
use rudb_parse::ast::{
    self, Ast, CaseArm, ColumnDef, Distinct, Expr, ExprRef, LiteralKind, Nulls, Order, OrderItem,
    Query, QueryBody, QueryRef, Select, SelectRef, Slice, Source, SourceRef, StrRef, Target,
};
use rudb_parse::matcher::NONE;

/// The catalog every input binds against.
///
/// The same two tables the binder's own tests use. A generated name that matches one of these is
/// the interesting case, because a query that resolves is a query that reaches type checking,
/// overload resolution and then the whole optimizer, while a query naming a table that is not there
/// stops at the first error. The name pools below are weighted towards these for that reason.
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

fuzz_target!(|generated: Generated| {
    let ast = generated.0;
    let Ok(bound) = bind_statement(&ast, &catalog()) else {
        // A statement that does not bind is the ordinary outcome and says nothing. The binder
        // refusing what it cannot do is the front end working, not failing.
        return;
    };
    let mut plan = match bound {
        Bound::Query(plan) | Bound::Explain { plan, .. } => plan,
        _ => return,
    };
    // A plan the binder produced has to satisfy the same invariant a plan the optimizer produced
    // does. The optimizer checks itself at the end of the sequence, so without this line a binder
    // that emitted a malformed plan would be reported against whichever pass ran last.
    plan.validate().expect("the binder produced a plan that does not validate");
    // Every self check in the optimizer is behind `debug_assertions`, which `cargo fuzz` leaves on:
    // the plan still validates, the query still returns the same number of columns, and a second
    // run of the sequence changes nothing. Anything this reports is a bug in a pass rather than in
    // the query, which is what its own doc comment says and why the error is not swallowed.
    rudb_opt::optimize(&mut plan).expect("the optimizer reported a bug in a pass");
});

/// An AST the generator below built, which is always one the transformer could have produced.
#[derive(Debug)]
struct Generated(Ast);

impl<'a> Arbitrary<'a> for Generated {
    fn arbitrary(source: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        let mut builder = Builder { ast: Ast::default(), source, budget: BUDGET };
        let statement = builder.statement(DEPTH)?;
        let mut ast = builder.ast;
        ast.statements.push(statement);
        Ok(Self(ast))
    }
}

/// How deep a query may nest, counting a subquery in an expression as a level.
const DEPTH: u32 = 3;

/// How many arena nodes one input may build.
///
/// A bound rather than a guess. Without it the generator will happily turn a long input into a
/// query of tens of thousands of nodes, which binds slowly, optimizes more slowly, and then reports
/// a libFuzzer timeout that is a large input rather than a hang. The interesting shapes are small.
const BUDGET: u32 = 300;

/// Names the catalog has, and a few it does not.
const NAMES: [&str; 10] =
    ["hits", "visits", "UserID", "url", "counter", "duration", "t", "x", "main", "nosuch"];

/// Functions, aggregates and one name nothing resolves.
const FUNCTIONS: [&str; 12] = [
    "count", "sum", "min", "max", "avg", "upper", "length", "abs", "coalesce", "nullif",
    "date_part", "nosuch",
];

/// Types as they are written, which is how the AST keeps them, plus one that is not a type.
const TYPES: [&str; 11] = [
    "INTEGER",
    "BIGINT",
    "VARCHAR",
    "DOUBLE",
    "BOOLEAN",
    "DECIMAL(10,2)",
    "DATE",
    "TIMESTAMP",
    "BLOB",
    "INTEGER[]",
    "not a type",
];

/// Numbers written as text, since that is what a literal is at this stage.
///
/// The ones that matter are at the ends: a value that no integer type holds, one that no double
/// holds, and the negative zero that compares equal to zero and does not print like it.
const NUMBERS: [&str; 10] = [
    "0",
    "1",
    "-1",
    "2147483647",
    "9223372036854775807",
    "999999999999999999999999999999",
    "1e400",
    "-0.0",
    "0.1",
    "1.5e-320",
];

/// Strings, holding the things that have ever broken a printer.
const TEXTS: [&str; 7] = ["", "a", "it's", "one\ntwo", "\u{0}", "grüß", "%_"];

impl Builder<'_, '_> {
    /// One statement, which is what the fuzz target binds.
    fn statement(&mut self, depth: u32) -> arbitrary::Result<ast::Statement> {
        Ok(match self.pick(7)? {
            0 | 1 | 2 => ast::Statement::Query(self.query(depth)?),
            3 => {
                let query = self.query(depth)?;
                let analyze = self.flag()?;
                ast::Statement::Explain { query, analyze }
            }
            4 => {
                let name = self.name()?;
                let columns = self.column_defs()?;
                let query = if self.flag()? { self.query(depth)? } else { NONE };
                let create = ast::CreateTable {
                    name,
                    columns,
                    query,
                    if_not_exists: self.flag()?,
                    or_replace: self.flag()?,
                    temporary: self.flag()?,
                };
                self.ast.create_tables.push(create);
                ast::Statement::CreateTable(self.ast.create_tables.len() as u32 - 1)
            }
            5 => {
                let name = self.name()?;
                let columns = self.parts()?;
                let source = self.query(depth)?;
                self.ast.inserts.push(ast::Insert { name, columns, source });
                ast::Statement::Insert(self.ast.inserts.len() as u32 - 1)
            }
            _ => {
                let names = self.name_lists()?;
                let if_exists = self.flag()?;
                let view = self.flag()?;
                self.ast.drop_tables.push(ast::DropTable { names, if_exists, view });
                ast::Statement::DropTable(self.ast.drop_tables.len() as u32 - 1)
            }
        })
    }

    /// A query, which is a body plus the modifiers that apply to what the body produced.
    fn query(&mut self, depth: u32) -> arbitrary::Result<QueryRef> {
        let body = self.body(depth)?;
        let order_by = if self.chance(3)? { self.order_items(depth)? } else { Slice::default() };
        let limit = self.optional_expr(depth)?;
        let offset = self.optional_expr(depth)?;
        let query = Query {
            body,
            order_by,
            order_by_all: self.chance(8)?,
            limit,
            limit_percent: self.chance(8)?,
            offset,
        };
        self.ast.queries.push(query);
        Ok(self.ast.queries.len() as u32 - 1)
    }

    /// What produces the rows.
    fn body(&mut self, depth: u32) -> arbitrary::Result<QueryBody> {
        if depth == 0 || self.spent() {
            return Ok(QueryBody::Select(self.select(0)?));
        }
        Ok(match self.pick(8)? {
            0..=4 => QueryBody::Select(self.select(depth)?),
            5 | 6 => {
                let left = self.query(depth - 1)?;
                let right = self.query(depth - 1)?;
                QueryBody::SetOp {
                    op: [ast::SetOp::Union, ast::SetOp::Except, ast::SetOp::Intersect]
                        [self.pick(3)? as usize],
                    quantifier: [
                        ast::Quantifier::Unstated,
                        ast::Quantifier::All,
                        ast::Quantifier::Distinct,
                    ][self.pick(3)? as usize],
                    by_name: self.chance(4)?,
                    left,
                    right,
                }
            }
            _ => QueryBody::Values(self.rows(depth)?),
        })
    }

    /// One select block.
    fn select(&mut self, depth: u32) -> arbitrary::Result<SelectRef> {
        let distinct = match self.pick(6)? {
            0..=3 => Distinct::No,
            4 => Distinct::Yes,
            _ => Distinct::On(self.expr_list(depth)?),
        };
        let from = self.sources(depth)?;
        let targets = self.targets(depth)?;
        let filter = self.optional_expr(depth)?;
        let group_by = if self.chance(3)? { self.expr_list(depth)? } else { Slice::default() };
        let having = self.optional_expr(depth)?;
        let select = Select {
            distinct,
            targets,
            from,
            filter,
            group_by,
            group_by_all: self.chance(8)?,
            having,
        };
        self.ast.selects.push(select);
        Ok(self.ast.selects.len() as u32 - 1)
    }

    /// The `FROM` list, which is several entries when the query wrote a cross product.
    fn sources(&mut self, depth: u32) -> arbitrary::Result<Slice> {
        let count = self.count(0, 2)?;
        let mut refs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            refs.push(self.source(depth)?);
        }
        Ok(extend(&mut self.ast.source_lists, refs))
    }

    /// One `FROM` entry, which is a tree because joins nest.
    fn source(&mut self, depth: u32) -> arbitrary::Result<SourceRef> {
        let source = if depth == 0 || self.spent() {
            let name = self.name()?;
            let alias = self.optional_string()?;
            let columns = self.parts()?;
            Source::Table { name, alias, columns }
        } else {
            match self.pick(8)? {
                0..=3 => {
                    let name = self.name()?;
                    let alias = self.optional_string()?;
                    let columns = self.parts()?;
                    Source::Table { name, alias, columns }
                }
                4 => {
                    let query = self.query(depth - 1)?;
                    let alias = self.optional_string()?;
                    let columns = self.parts()?;
                    Source::Subquery { query, alias, columns }
                }
                5 => {
                    let name = self.name()?;
                    let args = self.targets(depth - 1)?;
                    let alias = self.optional_string()?;
                    let columns = self.parts()?;
                    Source::Function { name, args, alias, columns }
                }
                6 => {
                    let rows = self.rows(depth - 1)?;
                    let alias = self.optional_string()?;
                    let columns = self.parts()?;
                    Source::Values { rows, alias, columns }
                }
                _ => {
                    let left = self.source(depth - 1)?;
                    let right = self.source(depth - 1)?;
                    let on = self.optional_expr(depth - 1)?;
                    let using = self.parts()?;
                    Source::Join {
                        left,
                        right,
                        kind: [
                            ast::JoinKind::Inner,
                            ast::JoinKind::Left,
                            ast::JoinKind::Right,
                            ast::JoinKind::Full,
                            ast::JoinKind::Semi,
                            ast::JoinKind::Anti,
                            ast::JoinKind::Cross,
                            ast::JoinKind::Positional,
                        ][self.pick(8)? as usize],
                        natural: self.chance(6)?,
                        on,
                        using,
                    }
                }
            }
        };
        self.ast.sources.push(source);
        Ok(self.ast.sources.len() as u32 - 1)
    }

    /// The target list, which is what a select returns.
    fn targets(&mut self, depth: u32) -> arbitrary::Result<Slice> {
        let count = self.count(1, 3)?;
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let expr = self.expr(depth)?;
            let alias = self.optional_string()?;
            items.push(Target { expr, alias });
        }
        Ok(extend(&mut self.ast.targets, items))
    }

    /// An `ORDER BY` list.
    fn order_items(&mut self, depth: u32) -> arbitrary::Result<Slice> {
        let count = self.count(1, 2)?;
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let expr = self.expr(depth)?;
            items.push(OrderItem {
                expr,
                order: [Order::Unstated, Order::Ascending, Order::Descending]
                    [self.pick(3)? as usize],
                nulls: [Nulls::Unstated, Nulls::First, Nulls::Last][self.pick(3)? as usize],
            });
        }
        Ok(extend(&mut self.ast.order_items, items))
    }

    /// The rows of a `VALUES`, each of which is a run of expressions.
    ///
    /// Every row is the same width, because a `VALUES` whose rows disagree is a parse error rather
    /// than something the transformer can produce, and generating one would be asking the binder
    /// about a tree that cannot arrive.
    fn rows(&mut self, depth: u32) -> arbitrary::Result<Slice> {
        let height = self.count(1, 3)?;
        let width = self.count(1, 3)?;
        let mut rows = Vec::with_capacity(height as usize);
        for _ in 0..height {
            let mut refs = Vec::with_capacity(width as usize);
            for _ in 0..width {
                refs.push(self.expr(depth.saturating_sub(1))?);
            }
            rows.push(extend(&mut self.ast.expr_lists, refs));
        }
        Ok(extend(&mut self.ast.rows, rows))
    }

    /// Column definitions for a `CREATE TABLE`.
    fn column_defs(&mut self) -> arbitrary::Result<Slice> {
        let count = self.count(0, 3)?;
        let mut items = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let name = self.string()?;
            let ty = if self.chance(8)? { NONE } else { self.ty()? };
            items.push(ColumnDef { name, ty, not_null: self.chance(4)? });
        }
        Ok(extend(&mut self.ast.column_defs, items))
    }

    /// One expression.
    fn expr(&mut self, depth: u32) -> arbitrary::Result<ExprRef> {
        let expr = if depth == 0 || self.spent() { self.leaf()? } else { self.branch(depth)? };
        self.ast.exprs.push(expr);
        self.budget = self.budget.saturating_sub(1);
        Ok(self.ast.exprs.len() as u32 - 1)
    }

    /// An expression with nothing under it.
    fn leaf(&mut self) -> arbitrary::Result<Expr> {
        Ok(match self.pick(6)? {
            0 | 1 | 2 => Expr::Column { name: self.name()? },
            3 | 4 => {
                let kind = [
                    LiteralKind::Number,
                    LiteralKind::String,
                    LiteralKind::Blob,
                    LiteralKind::Null,
                    LiteralKind::True,
                    LiteralKind::False,
                ][self.pick(6)? as usize];
                let text = match kind {
                    LiteralKind::Number => {
                        let number = self.choose(&NUMBERS)?;
                        self.intern(number)
                    }
                    LiteralKind::String => {
                        let text = self.choose(&TEXTS)?;
                        self.intern(text)
                    }
                    LiteralKind::Blob => self.intern("\\x00\\x41"),
                    _ => NONE,
                };
                Expr::Literal { kind, text }
            }
            _ => Expr::Parameter { name: self.string()? },
        })
    }

    /// An expression with something under it.
    fn branch(&mut self, depth: u32) -> arbitrary::Result<Expr> {
        Ok(match self.pick(12)? {
            0 => Expr::Star { qualifier: self.parts()?, replacements: Slice::default() },
            1 | 2 => {
                let operand = self.expr(depth - 1)?;
                Expr::Unary { op: self.unary()?, operand }
            }
            3 | 4 | 5 => {
                let left = self.expr(depth - 1)?;
                let right = self.expr(depth - 1)?;
                Expr::Binary { op: self.binary()?, left, right }
            }
            6 => {
                let name = self.function()?;
                let args = self.expr_list(depth - 1)?;
                Expr::Function { name, args, distinct: self.chance(4)? }
            }
            7 => {
                let operand = self.expr(depth - 1)?;
                let ty = self.ty()?;
                Expr::Cast { operand, ty, try_cast: self.flag()? }
            }
            8 => {
                let operand = if self.chance(2)? { self.expr(depth - 1)? } else { NONE };
                let count = self.count(1, 2)?;
                let mut arms = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let when = self.expr(depth - 1)?;
                    let then = self.expr(depth - 1)?;
                    arms.push(CaseArm { when, then });
                }
                let arms = extend(&mut self.ast.case_arms, arms);
                let otherwise = self.optional_expr(depth - 1)?;
                Expr::Case { operand, arms, otherwise }
            }
            9 => {
                let operand = self.expr(depth - 1)?;
                let low = self.expr(depth - 1)?;
                let high = self.expr(depth - 1)?;
                Expr::Between { operand, low, high, negated: self.flag()? }
            }
            10 => {
                let operand = self.expr(depth - 1)?;
                let list = self.expr_list(depth - 1)?;
                Expr::In { operand, list, negated: self.flag()? }
            }
            _ => match self.pick(3)? {
                0 => Expr::List { items: self.expr_list(depth - 1)? },
                1 => Expr::Row { items: self.expr_list(depth - 1)? },
                _ => Expr::Subquery { query: self.query(depth - 1)? },
            },
        })
    }

    /// A run of expressions, which several forms hold.
    fn expr_list(&mut self, depth: u32) -> arbitrary::Result<Slice> {
        let count = self.count(0, 3)?;
        let mut refs = Vec::with_capacity(count as usize);
        for _ in 0..count {
            refs.push(self.expr(depth)?);
        }
        Ok(extend(&mut self.ast.expr_lists, refs))
    }

    /// An expression that is often absent, which is how the AST spells an optional clause.
    fn optional_expr(&mut self, depth: u32) -> arbitrary::Result<ExprRef> {
        if self.chance(3)? { self.expr(depth.saturating_sub(1)) } else { Ok(NONE) }
    }

    /// A prefix or postfix operator.
    fn unary(&mut self) -> arbitrary::Result<ast::UnaryOp> {
        use ast::UnaryOp::{
            BitNot, Factorial, IsFalse, IsNotFalse, IsNotNull, IsNotTrue, IsNotUnknown, IsNull,
            IsTrue, IsUnknown, Negate, Not, Plus,
        };
        Ok([
            Not,
            Negate,
            Plus,
            BitNot,
            Factorial,
            IsNull,
            IsNotNull,
            IsTrue,
            IsNotTrue,
            IsFalse,
            IsNotFalse,
            IsUnknown,
            IsNotUnknown,
        ][self.pick(13)? as usize])
    }

    /// An infix operator, which is most of what a predicate is made of.
    fn binary(&mut self) -> arbitrary::Result<ast::BinaryOp> {
        use ast::BinaryOp::{
            Add, And, AtTimeZone, BitAnd, BitOr, Collate, Concat, Contains, Divide, Eq, Glob, Gt,
            GtEq, ILike, IntegerDivide, IsDistinctFrom, IsNotDistinctFrom, Like, Lt, LtEq, Modulo,
            Multiply, Named, NotEq, NotILike, NotLike, Or, Overlaps, Power, Regex, ShiftLeft,
            ShiftRight, SimilarTo, StartsWith, Subtract,
        };
        let choice = self.pick(35)?;
        const OPERATORS: [ast::BinaryOp; 34] = [
            Or,
            And,
            Eq,
            NotEq,
            Lt,
            Gt,
            LtEq,
            GtEq,
            IsDistinctFrom,
            IsNotDistinctFrom,
            Add,
            Subtract,
            Multiply,
            Divide,
            IntegerDivide,
            Modulo,
            Power,
            BitAnd,
            BitOr,
            ShiftLeft,
            ShiftRight,
            Concat,
            Like,
            NotLike,
            ILike,
            NotILike,
            Glob,
            SimilarTo,
            Regex,
            Collate,
            AtTimeZone,
            Contains,
            StartsWith,
            Overlaps,
        ];
        // The last alternative is the open door in the dialect: any run of operator characters that
        // is not already a token binds as a function of that name, so it is a name and not a
        // variant and it cannot go in the table above.
        match OPERATORS.get(choice as usize) {
            Some(&op) => Ok(op),
            None => Ok(Named(self.intern("<=>"))),
        }
    }

    /// A qualified name, as a run of parts with the outermost first.
    fn name(&mut self) -> arbitrary::Result<Slice> {
        let count = self.count(1, 3)?;
        let mut parts = Vec::with_capacity(count as usize);
        for _ in 0..count {
            parts.push(self.string()?);
        }
        Ok(extend(&mut self.ast.parts, parts))
    }

    /// A function name, which is a qualified name that usually resolves.
    fn function(&mut self) -> arbitrary::Result<Slice> {
        let chosen = self.choose(&FUNCTIONS)?;
        let name = self.intern(chosen);
        Ok(extend(&mut self.ast.parts, vec![name]))
    }

    /// A bare run of names, which is what a column alias list and a `USING` clause are.
    fn parts(&mut self) -> arbitrary::Result<Slice> {
        let count = self.count(0, 2)?;
        let mut parts = Vec::with_capacity(count as usize);
        for _ in 0..count {
            parts.push(self.string()?);
        }
        Ok(extend(&mut self.ast.parts, parts))
    }

    /// A run of names, each of which is itself a run of parts, which is what `DROP` takes.
    fn name_lists(&mut self) -> arbitrary::Result<Slice> {
        let count = self.count(1, 2)?;
        let mut names = Vec::with_capacity(count as usize);
        for _ in 0..count {
            names.push(self.name()?);
        }
        Ok(extend(&mut self.ast.name_lists, names))
    }

    /// One interned name.
    fn string(&mut self) -> arbitrary::Result<StrRef> {
        let chosen = self.choose(&NAMES)?;
        Ok(self.intern(chosen))
    }

    /// One interned name, or nothing, which is how an alias is spelled.
    fn optional_string(&mut self) -> arbitrary::Result<StrRef> {
        if self.chance(3)? { self.string() } else { Ok(NONE) }
    }

    /// A type as it was written, which is what the AST keeps and the binder resolves.
    fn ty(&mut self) -> arbitrary::Result<StrRef> {
        let chosen = self.choose(&TYPES)?;
        Ok(self.intern(chosen))
    }
}

/// The arena being built, and the bytes it is being built out of.
struct Builder<'a, 'u> {
    /// What is being built.
    ast: Ast,
    /// The fuzzer's input, read as choices rather than as text.
    source: &'u mut Unstructured<'a>,
    /// How many more expressions may be built before every branch takes its leaf arm.
    budget: u32,
}

impl Builder<'_, '_> {
    /// One of `n` alternatives.
    ///
    /// Running out of input is not an error here, it is the end of the tree: `int_in_range` yields
    /// the first alternative once the bytes are gone, and every list in this file is ordered so that
    /// the first alternative is the smallest one. That is what makes a short input a small query
    /// rather than a rejected one.
    fn pick(&mut self, alternatives: u32) -> arbitrary::Result<u32> {
        self.source.int_in_range(0..=alternatives - 1)
    }

    /// A count between two bounds, inclusive.
    fn count(&mut self, low: u32, high: u32) -> arbitrary::Result<u32> {
        if self.spent() {
            return Ok(low);
        }
        self.source.int_in_range(low..=high)
    }

    /// True one time in `odds`.
    fn chance(&mut self, odds: u32) -> arbitrary::Result<bool> {
        Ok(self.pick(odds)? == 0)
    }

    /// True or false.
    fn flag(&mut self) -> arbitrary::Result<bool> {
        self.source.arbitrary()
    }

    /// One entry of a fixed pool.
    fn choose<'p>(&mut self, pool: &'p [&'p str]) -> arbitrary::Result<&'p str> {
        let index = self.pick(pool.len() as u32)?;
        Ok(pool[index as usize])
    }

    /// Interns a string, reusing the slot when the text is already there.
    ///
    /// The transformer interns, so an AST with the same name in two slots is one the transformer
    /// would not have produced, and a binder that compared by slot rather than by text would pass
    /// here and fail on real input.
    fn intern(&mut self, text: &str) -> StrRef {
        if let Some(found) = self.ast.strings.iter().position(|held| held == text) {
            return found as u32;
        }
        self.ast.strings.push(text.to_string());
        self.ast.strings.len() as u32 - 1
    }

    /// Whether the node budget is gone, which makes every remaining branch take its leaf arm.
    fn spent(&self) -> bool {
        self.budget == 0
    }
}

/// Pushes a run onto a backing vector and returns the slice that names it.
///
/// Everything goes through here rather than pushing as it goes, because a run has to be contiguous
/// and the items of a run are built by code that pushes onto the same vector. A nested list built
/// in place would interleave with the one holding it and both slices would name the wrong items.
fn extend<T>(backing: &mut Vec<T>, items: Vec<T>) -> Slice {
    let start = backing.len() as u32;
    let len = items.len() as u32;
    backing.extend(items);
    Slice { start, len }
}
