//! rudb's abstract syntax tree.
//!
//! The parse tree the matcher produces is DuckDB's grammar, faithfully. That is the point of it and
//! it is also why nothing downstream should read it: a bump of the vendored grammar is allowed to
//! rename `BetweenInLikeExpression`, and if the binder is matching on that name then the bump is a
//! rewrite. This module is the boundary. It is ours, it changes when we decide it changes, and
//! `transform` is the one place that knows both shapes.
//!
//! Everything is an arena with `u32` indices, per `spec/04-architecture.md` section 4.5. There is
//! no `Box` and no `Vec` inside a node. A list of children is a [`Slice`] into a side vector, which
//! means a node is a fixed size, the whole tree is a handful of allocations, and walking it is a
//! sequential read rather than a pointer chase per node. It also means an `Ast` is `Clone` and
//! `Send` without any thought, and that a subtree can be addressed by a `u32` in a plan or an
//! error without borrowing anything.
//!
//! The one cost is that you cannot hold a reference to a node and index the arena at the same time,
//! so the code reads a node out by value first. Nodes are small and `Copy`, so that is a register
//! move.

use crate::matcher::NONE;

/// A run of items in one of the side vectors.
///
/// Empty is `len == 0`, and `start` is then meaningless rather than wrong. There is no `Option`
/// wrapper because an absent list and an empty list are the same thing everywhere this is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Slice {
    /// The first item.
    pub start: u32,
    /// How many items.
    pub len: u32,
}

impl Slice {
    /// Whether the run is empty.
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// The run as a range, for indexing the backing vector.
    pub const fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// An index into `Ast::strings`.
pub type StrRef = u32;
/// An index into `Ast::exprs`.
pub type ExprRef = u32;
/// An index into `Ast::sources`.
pub type SourceRef = u32;
/// An index into `Ast::queries`.
pub type QueryRef = u32;
/// An index into `Ast::selects`.
pub type SelectRef = u32;
/// An index into `Ast::create_tables`.
pub type CreateTableRef = u32;
/// An index into `Ast::create_views`.
pub type CreateViewRef = u32;
/// An index into `Ast::drop_tables`.
pub type DropTableRef = u32;
/// An index into `Ast::inserts`.
pub type InsertRef = u32;
/// An index into `Ast::settings`.
pub type SettingRef = u32;

/// One statement.
///
/// Seven of the twenty seven the grammar reaches. The rest are a transform error naming the rule
/// rather than a variant that nothing fills in, so that adding one is a compile error somewhere
/// useful rather than a silent `todo!()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Statement {
    /// A query, meaning a `SELECT` or a set operation over two of them.
    Query(QueryRef),
    /// `CREATE TABLE`.
    CreateTable(CreateTableRef),
    /// `CREATE VIEW`.
    CreateView(CreateViewRef),
    /// `DROP TABLE` or `DROP VIEW`, which are one rule in the grammar and one statement here.
    DropTable(DropTableRef),
    /// `INSERT INTO`.
    Insert(InsertRef),
    /// `SET name = value`.
    Set(SettingRef),
    /// `RESET name`, which is the same shape with nothing on the right of it.
    Reset(SettingRef),
}

/// `SET name = value` and `RESET name`.
///
/// One struct for the two, because `RESET name` is `SET name` with no value and giving it its own
/// arena would mean two of everything to say the same thing twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// The setting name, as written.
    pub name: StrRef,
    /// The scope word, if one was written.
    pub scope: Scope,
    /// The value, or `NONE` for a `RESET`.
    ///
    /// An expression rather than text. `SET memory_limit = '1GB'` writes a string and `SET threads
    /// = 4` writes a number, and what a setting does with either is the setting's business.
    pub value: ExprRef,
}

/// Which copy of a setting a statement means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// No scope word, which every setting reads as the one it has.
    #[default]
    Unwritten,
    /// `GLOBAL`.
    Global,
    /// `SESSION`.
    Session,
    /// `LOCAL`.
    Local,
}

impl Scope {
    /// The word that was written, for the sentence an error prints.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Unwritten => "",
            Self::Global => "GLOBAL",
            Self::Session => "SESSION",
            Self::Local => "LOCAL",
        }
    }
}

/// `CREATE TABLE name (columns)` or `CREATE TABLE name AS query`.
///
/// Exactly one of `columns` and `query` says what the table is. A column list is the ordinary form
/// and `query` is `CREATE TABLE AS`, where the columns come from what the query produced and the
/// only thing the syntax contributes is optionally renaming them, which is `columns` with the types
/// left as `NONE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTable {
    /// The table name, as a run of [`Slice`] parts, outermost first.
    pub name: Slice,
    /// The column definitions, as a run of [`ColumnDef`].
    pub columns: Slice,
    /// The `AS` query, or `NONE`.
    pub query: QueryRef,
    /// Whether `IF NOT EXISTS` was written.
    pub if_not_exists: bool,
    /// Whether `OR REPLACE` was written.
    pub or_replace: bool,
    /// Whether `TEMP` or `TEMPORARY` was written.
    pub temporary: bool,
}

/// One column of a `CREATE TABLE`.
///
/// The type is the text as written rather than a resolved type, because resolving a type is the
/// binder's job and this crate is syntax. `VARCHAR(10)` and `STRUCT(a INTEGER)` reach the binder
/// as themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDef {
    /// The column name.
    pub name: StrRef,
    /// The type as written, or `NONE` when the definition had none, which only `CREATE TABLE AS`
    /// allows.
    pub ty: StrRef,
    /// Whether `NOT NULL` was written.
    pub not_null: bool,
}

/// `CREATE VIEW name (columns) AS query`.
///
/// The body is kept twice over, as a bound reference into this same arena and as the text that was
/// written. Both are needed and they are needed for different things. The reference is what binds
/// the body at creation, which is where a view over a table that is not there is refused. The text
/// is what the catalog keeps, because a view is bound again at every reference rather than frozen
/// at creation: a view over `SELECT * FROM t` follows `t` when a column is added to it, which was
/// measured, and the only way to follow it is to have the query to bind again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateView {
    /// The view name, as a run of [`Slice`] parts, outermost first.
    pub name: Slice,
    /// The column aliases, as a run of parts, empty when the statement wrote no list.
    pub columns: Slice,
    /// The body.
    pub query: QueryRef,
    /// The body as it was written, which is what the catalog keeps.
    pub sql: StrRef,
    /// Whether `IF NOT EXISTS` was written.
    pub if_not_exists: bool,
    /// Whether `OR REPLACE` was written.
    pub or_replace: bool,
    /// Whether `TEMP` or `TEMPORARY` was written.
    pub temporary: bool,
}

/// `DROP TABLE a, b` or `DROP VIEW a, b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTable {
    /// The names, as a run of [`Slice`] into `Ast::name_lists`, each of which is a run of parts.
    pub names: Slice,
    /// Whether `IF EXISTS` was written.
    pub if_exists: bool,
    /// Whether `VIEW` was written where `TABLE` could have been. Dropping one as the other is an
    /// error rather than a synonym, so which word was written has to survive the transform.
    pub view: bool,
}

/// `INSERT INTO name (columns) query`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insert {
    /// The table name, as a run of parts, outermost first.
    pub name: Slice,
    /// The column list, as a run of parts, empty when the statement did not write one.
    pub columns: Slice,
    /// What produces the rows, which is a `VALUES` clause or any other query.
    pub source: QueryRef,
}

/// A query: a body, plus the modifiers that apply to whatever the body produced.
///
/// The split is the grammar's, not an invention. `SelectStatementInternal <- WithClause?
/// SelectSetOpChain ResultModifiers?` puts `ORDER BY` and `LIMIT` outside the set operator chain,
/// which is the only place they can go and be right: `a UNION b ORDER BY x` sorts the union and not
/// the second half of it. Hanging them off `Select` instead would have made that unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query {
    /// What produces the rows.
    pub body: QueryBody,
    /// The `ORDER BY` list, as a run of [`OrderItem`].
    pub order_by: Slice,
    /// Whether the clause was `ORDER BY ALL`.
    pub order_by_all: bool,
    /// The `LIMIT` expression, or `NONE`.
    pub limit: ExprRef,
    /// Whether the limit was a percentage rather than a row count.
    pub limit_percent: bool,
    /// The `OFFSET` expression, or `NONE`.
    pub offset: ExprRef,
}

impl Query {
    /// A query with no modifiers on it.
    pub const fn bare(body: QueryBody) -> Self {
        Self {
            body,
            order_by: Slice { start: 0, len: 0 },
            order_by_all: false,
            limit: NONE,
            limit_percent: false,
            offset: NONE,
        }
    }
}

/// What produces the rows of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryBody {
    /// One `SELECT ... FROM ... WHERE ...` block.
    Select(SelectRef),
    /// `UNION`, `EXCEPT` or `INTERSECT` over two queries.
    SetOp {
        /// Which operator.
        op: SetOp,
        /// Whether duplicates survive.
        quantifier: Quantifier,
        /// Whether the columns are matched up by name rather than by position.
        by_name: bool,
        /// The query on the left.
        left: QueryRef,
        /// The query on the right.
        right: QueryRef,
    },
    /// `VALUES (1, 'a'), (2, 'b')`, as a run of [`Slice`] in `Ast::rows`.
    ///
    /// A row count and a column count and nothing else, so it is a query body rather than a
    /// statement of its own. That is also what makes `INSERT INTO t VALUES (1)` and
    /// `INSERT INTO t SELECT 1` the same shape by the time anything downstream sees them, which is
    /// the reason the insert walker does not have two arms.
    Values(Slice),
    /// `DESCRIBE SELECT ...`, `DESCRIBE t` and `DESCRIBE 'file.parquet'`.
    ///
    /// A query body rather than a statement, because that is where the grammar puts it:
    /// `SelectStatementType <- ... / DescribeStatement / ...`, so `FROM (DESCRIBE SELECT 1)` is a
    /// subquery over one and needs no rule of its own. The two spellings that name something
    /// instead of writing a query arrive here as `DESCRIBE SELECT * FROM that`, which is not a
    /// shortcut: on the reference binary `DESCRIBE t` and `DESCRIBE SELECT * FROM t` produce the
    /// same six columns and the same rows, down to the primary key and the default.
    Describe(QueryRef),
}

/// Which set operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`.
    Union,
    /// `EXCEPT`.
    Except,
    /// `INTERSECT`.
    Intersect,
}

/// Whether a set operator or an aggregate keeps duplicates.
///
/// `Unstated` is not the same as `All` even though the two agree for `UNION`, because they disagree
/// for `INTERSECT` in some dialects and because an error message that says what was written is
/// better than one that says what it was taken to mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantifier {
    /// Neither word was written.
    Unstated,
    /// `ALL`.
    All,
    /// `DISTINCT`.
    Distinct,
}

/// What the `DISTINCT` clause of a select said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distinct {
    /// No clause, or the no-op `SELECT ALL`.
    No,
    /// `SELECT DISTINCT`.
    Yes,
    /// `SELECT DISTINCT ON (a, b)`, holding the expressions in the parentheses.
    On(Slice),
}

/// One select block.
///
/// Every optional expression is `NONE` when it is absent rather than an `Option<u32>`, which keeps
/// the struct at forty bytes and keeps the absent case spelled the same way it is spelled in the
/// parse tree arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Select {
    /// The `DISTINCT` clause.
    pub distinct: Distinct,
    /// The target list, as a run of [`Target`].
    pub targets: Slice,
    /// The `FROM` list, as a run of [`SourceRef`]. Several entries mean a cross product.
    pub from: Slice,
    /// The `WHERE` expression, or `NONE`.
    pub filter: ExprRef,
    /// The `GROUP BY` list, as a run of [`ExprRef`].
    pub group_by: Slice,
    /// Whether the clause was `GROUP BY ALL`.
    pub group_by_all: bool,
    /// The `HAVING` expression, or `NONE`.
    pub having: ExprRef,
}

impl Select {
    /// An empty select, which is what the transformer fills in from.
    pub const fn empty() -> Self {
        Self {
            distinct: Distinct::No,
            targets: Slice { start: 0, len: 0 },
            from: Slice { start: 0, len: 0 },
            filter: NONE,
            group_by: Slice { start: 0, len: 0 },
            group_by_all: false,
            having: NONE,
        }
    }
}

/// One entry of a target list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// What is being selected.
    pub expr: ExprRef,
    /// The alias, or `NONE`. The binder invents one when there is none, because what it invents
    /// depends on the expression and that is a binder question rather than a parser question.
    pub alias: StrRef,
}

/// One entry of an order by list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderItem {
    /// What to sort on.
    pub expr: ExprRef,
    /// The direction.
    pub order: Order,
    /// Where nulls go.
    pub nulls: Nulls,
}

/// Sort direction, with the unwritten case kept apart from the default it resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// Nothing was written.
    Unstated,
    /// `ASC` or `ASCENDING`.
    Ascending,
    /// `DESC` or `DESCENDING`.
    Descending,
}

/// Null placement in a sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nulls {
    /// Nothing was written, so the session default applies.
    Unstated,
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// One entry in a `FROM` clause, which is a tree because joins nest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A named table, possibly qualified by schema and catalog.
    Table {
        /// The name, as a run of [`StrRef`] in `Ast::parts`, outermost first.
        name: Slice,
        /// The alias, or `NONE`.
        alias: StrRef,
        /// Column aliases from `AS t(a, b)`, as a run of [`StrRef`].
        columns: Slice,
    },
    /// A parenthesised query in the `FROM` clause.
    Subquery {
        /// The query.
        query: QueryRef,
        /// The alias, or `NONE`.
        alias: StrRef,
        /// Column aliases, as a run of [`StrRef`].
        columns: Slice,
    },
    /// A function call where a table goes, such as `range(10)`.
    ///
    /// Held with the name as a qualified run rather than a single string, because `main.range(10)`
    /// is legal and a function in a schema that does not exist has to say so rather than being
    /// looked up unqualified and found.
    Function {
        /// The name, as a run of [`StrRef`] in `Ast::parts`, outermost first.
        name: Slice,
        /// The arguments, as a run of [`Target`] where the alias is the parameter name and is
        /// `NONE` for a positional one.
        args: Slice,
        /// The alias, or `NONE`.
        alias: StrRef,
        /// Column aliases from `AS t(a, b)`, as a run of [`StrRef`].
        columns: Slice,
    },
    /// A `VALUES` in the `FROM` clause.
    Values {
        /// The rows, as a run of [`Slice`] in `Ast::rows`.
        rows: Slice,
        /// The alias, or `NONE`.
        alias: StrRef,
        /// Column aliases, as a run of [`StrRef`].
        columns: Slice,
    },
    /// Two sources joined.
    Join {
        /// The left side.
        left: SourceRef,
        /// The right side.
        right: SourceRef,
        /// Which join.
        kind: JoinKind,
        /// Whether it was written `NATURAL`.
        natural: bool,
        /// The `ON` expression, or `NONE`.
        on: ExprRef,
        /// The `USING` column list, as a run of [`StrRef`].
        using: Slice,
    },
}

/// Which join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `[INNER] JOIN`.
    Inner,
    /// `LEFT [OUTER] JOIN`.
    Left,
    /// `RIGHT [OUTER] JOIN`.
    Right,
    /// `FULL [OUTER] JOIN`.
    Full,
    /// `SEMI JOIN`.
    Semi,
    /// `ANTI JOIN`.
    Anti,
    /// `CROSS JOIN`.
    Cross,
    /// `POSITIONAL JOIN`, which is DuckDB's own and pairs rows by ordinal.
    Positional,
}

/// One expression.
///
/// Twenty four bytes, which is the widest variant rounded up. The precedence chain in the grammar
/// does not survive into here: twenty levels of `X <- Y Tail*` become one [`Expr::Binary`] tree,
/// because the levels exist to make the grammar unambiguous and mean nothing afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expr {
    /// `*`, or `t.*` with a qualifier.
    Star {
        /// The qualifier, as a run of [`StrRef`], empty for a bare star.
        qualifier: Slice,
        /// `REPLACE (expression AS column)`, as a run of [`Target`] where the alias is the column
        /// being replaced, empty for a star with no replace list.
        ///
        /// A [`Target`] rather than a type of its own because a replacement is an expression and a
        /// name, which is exactly what a target is, and because that puts it in the arena every
        /// other expression and name pair already lives in.
        replacements: Slice,
    },
    /// A column reference, qualified or not.
    Column {
        /// The name, as a run of [`StrRef`], outermost first, so `s.t.a` is three parts.
        name: Slice,
    },
    /// A literal, kept as the text that was written.
    Literal {
        /// Which kind.
        kind: LiteralKind,
        /// The text, with quotes stripped and escapes resolved for a string, `NONE` for a keyword
        /// literal like `NULL` where the kind already says everything.
        text: StrRef,
    },
    /// A prefix or postfix operator.
    Unary {
        /// Which operator.
        op: UnaryOp,
        /// What it applies to.
        operand: ExprRef,
    },
    /// An infix operator.
    Binary {
        /// Which operator.
        op: BinaryOp,
        /// The left operand.
        left: ExprRef,
        /// The right operand.
        right: ExprRef,
    },
    /// A function call.
    Function {
        /// The name, as a run of [`StrRef`], so `main.count` is two parts.
        name: Slice,
        /// The arguments, as a run of [`ExprRef`].
        args: Slice,
        /// Whether the call said `DISTINCT`.
        distinct: bool,
    },
    /// `CAST(x AS t)` or `TRY_CAST(x AS t)`.
    Cast {
        /// What is being cast.
        operand: ExprRef,
        /// The target type, as the text it was written with. Parsing it is `rudb-common`'s job and
        /// doing it here would put the type system in the parser.
        ty: StrRef,
        /// Whether a failure yields null rather than an error.
        try_cast: bool,
    },
    /// `CASE`, searched or simple.
    Case {
        /// The operand of a simple `CASE x WHEN`, or `NONE` for a searched one.
        operand: ExprRef,
        /// The arms, as a run of [`CaseArm`].
        arms: Slice,
        /// The `ELSE`, or `NONE`.
        otherwise: ExprRef,
    },
    /// `x BETWEEN a AND b`.
    Between {
        /// What is being tested.
        operand: ExprRef,
        /// The lower bound.
        low: ExprRef,
        /// The upper bound.
        high: ExprRef,
        /// Whether it was written `NOT BETWEEN`.
        negated: bool,
    },
    /// `x IN (a, b, c)`.
    In {
        /// What is being tested.
        operand: ExprRef,
        /// The list, as a run of [`ExprRef`].
        list: Slice,
        /// Whether it was written `NOT IN`.
        negated: bool,
    },
    /// A prepared statement parameter, written `?`, `?1`, `$1` or `$name`.
    Parameter {
        /// The identifier, which is the number for a positional one and the word for a named one.
        /// A bare `?` is numbered by where it was written, so the identifier is there either way.
        name: StrRef,
    },
    /// A bracketed list of expressions, `[a, b, c]`, which is a LIST value.
    List {
        /// The items, as a run of [`ExprRef`], in the order they were written.
        items: Slice,
    },
    /// A parenthesised list of more than one expression, which is a row value.
    Row {
        /// The items, as a run of [`ExprRef`].
        items: Slice,
    },
    /// A scalar subquery, `(SELECT ...)` where an expression is expected.
    Subquery {
        /// The query.
        query: QueryRef,
    },
}

/// One `WHEN a THEN b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaseArm {
    /// The `WHEN`.
    pub when: ExprRef,
    /// The `THEN`.
    pub then: ExprRef,
}

/// Which literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralKind {
    /// A number, kept as text because the width it wants depends on where it lands.
    Number,
    /// A string.
    String,
    /// A blob, kept as the text a blob prints as, which is the text a cast reads it back from.
    Blob,
    /// `NULL`.
    Null,
    /// `TRUE`.
    True,
    /// `FALSE`.
    False,
}

/// A prefix or postfix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `NOT x`.
    Not,
    /// `-x`.
    Negate,
    /// `+x`, which is a no-op that still has to survive to the binder so that `+'a'` errors.
    Plus,
    /// `~x`.
    BitNot,
    /// `x!`.
    Factorial,
    /// `x IS NULL` or `x ISNULL`.
    IsNull,
    /// `x IS NOT NULL` or `x NOTNULL`.
    IsNotNull,
    /// `x IS TRUE`.
    IsTrue,
    /// `x IS NOT TRUE`.
    IsNotTrue,
    /// `x IS FALSE`.
    IsFalse,
    /// `x IS NOT FALSE`.
    IsNotFalse,
    /// `x IS UNKNOWN`.
    IsUnknown,
    /// `x IS NOT UNKNOWN`.
    IsNotUnknown,
}

/// An infix operator.
///
/// The list is the dialect and not a general idea of what operators are. `Named` is the one open
/// door, because `OperatorLiteral` in the grammar takes any run of operator characters that is not
/// already a token, and rejecting that here would reject SQL DuckDB accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR`.
    Or,
    /// `AND`.
    And,
    /// `=` or `==`.
    Eq,
    /// `!=` or `<>`.
    NotEq,
    /// `<`.
    Lt,
    /// `>`.
    Gt,
    /// `<=`.
    LtEq,
    /// `>=`.
    GtEq,
    /// `IS DISTINCT FROM`.
    IsDistinctFrom,
    /// `IS NOT DISTINCT FROM`.
    IsNotDistinctFrom,
    /// `+`.
    Add,
    /// `-`.
    Subtract,
    /// `*`.
    Multiply,
    /// `/`.
    Divide,
    /// `//`, integer division.
    IntegerDivide,
    /// `%`.
    Modulo,
    /// `^` or `**`.
    Power,
    /// `&`.
    BitAnd,
    /// `|`.
    BitOr,
    /// `<<`.
    ShiftLeft,
    /// `>>`.
    ShiftRight,
    /// `||`.
    Concat,
    /// `LIKE` or `~~`.
    Like,
    /// `NOT LIKE` or `!~~`.
    NotLike,
    /// `ILIKE` or `~~*`.
    ILike,
    /// `NOT ILIKE` or `!~~*`.
    NotILike,
    /// `GLOB` or `~~~`.
    Glob,
    /// `SIMILAR TO`.
    SimilarTo,
    /// `!~`, which the grammar calls the not-similar-to operator.
    NotSimilarTo,
    /// `~`, a regex match.
    Regex,
    /// `~*`, a case insensitive regex match.
    RegexInsensitive,
    /// `!~*`, a negated case insensitive regex match.
    NotRegexInsensitive,
    /// `COLLATE`.
    Collate,
    /// `AT TIME ZONE`.
    AtTimeZone,
    /// `->`.
    Arrow,
    /// `->>`.
    LongArrow,
    /// `@>`, contains.
    Contains,
    /// `<@`, contained by.
    ContainedBy,
    /// `&&`, overlaps.
    Overlaps,
    /// `^@`, starts with.
    StartsWith,
    /// `<<=`, an inet operator.
    InetContainedByOrEq,
    /// `>>=`, an inet operator.
    InetContainsOrEq,
    /// An operator the dialect does not name, which DuckDB resolves as a binary function of that
    /// name. `a <=> b` is the shape.
    Named(StrRef),
}

/// A parsed statement or script, with every arena it points into.
///
/// Cheap to clone, cheap to send, and self contained: no index in here refers to anything outside
/// it, and nothing in here borrows the query text. The text is copied into `strings` on the way in,
/// which costs one allocation per distinct identifier and buys an `Ast` that outlives the string it
/// came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ast {
    /// The statements in the script, in order.
    pub statements: Vec<Statement>,
    /// The query arena.
    pub queries: Vec<Query>,
    /// The select arena.
    pub selects: Vec<Select>,
    /// The expression arena.
    pub exprs: Vec<Expr>,
    /// The from-item arena.
    pub sources: Vec<Source>,
    /// Interned text. Identifiers keep the case they were written in, because DuckDB does not fold
    /// it at any point, including for quoted identifiers.
    pub strings: Vec<String>,
    /// Backing store for every [`Slice`] of names.
    pub parts: Vec<StrRef>,
    /// Backing store for every [`Slice`] of expressions.
    pub expr_lists: Vec<ExprRef>,
    /// Backing store for every [`Slice`] of from items.
    pub source_lists: Vec<SourceRef>,
    /// Backing store for every [`Slice`] of target list entries.
    pub targets: Vec<Target>,
    /// Backing store for every [`Slice`] of order by entries.
    pub order_items: Vec<OrderItem>,
    /// Backing store for every [`Slice`] of case arms.
    pub case_arms: Vec<CaseArm>,
    /// The `CREATE TABLE` arena.
    pub create_tables: Vec<CreateTable>,
    /// The `CREATE VIEW` arena.
    pub create_views: Vec<CreateView>,
    /// The `DROP TABLE` arena.
    pub drop_tables: Vec<DropTable>,
    /// The `INSERT` arena.
    pub inserts: Vec<Insert>,
    /// The `SET` and `RESET` arena.
    pub settings: Vec<Setting>,
    /// Backing store for every [`Slice`] of column definitions.
    pub column_defs: Vec<ColumnDef>,
    /// Backing store for every [`Slice`] of names, which is a name list rather than a name.
    pub name_lists: Vec<Slice>,
    /// Backing store for the rows of a `VALUES`, each of which is a run of expressions.
    pub rows: Vec<Slice>,
}

impl Ast {
    /// The text behind a [`StrRef`], or the empty string for `NONE`.
    pub fn string(&self, index: StrRef) -> &str {
        if index == NONE { "" } else { &self.strings[index as usize] }
    }

    /// Every parameter identifier the statement uses, once each, in the order they were written.
    ///
    /// The arena is built as the walk goes, so its order is the written order, and a parameter used
    /// twice is one identifier here because it is one value to provide.
    pub fn parameters(&self) -> Vec<&str> {
        let mut found: Vec<&str> = Vec::new();
        for expr in &self.exprs {
            if let Expr::Parameter { name } = *expr {
                let name = self.string(name);
                if !found.contains(&name) {
                    found.push(name);
                }
            }
        }
        found
    }

    /// The parts of a name, outermost first.
    pub fn name(&self, slice: Slice) -> impl Iterator<Item = &str> {
        self.parts[slice.range()].iter().map(|&part| self.string(part))
    }

    /// A name written back out with dots between the parts, for error messages and tests.
    pub fn name_text(&self, slice: Slice) -> String {
        self.name(slice).collect::<Vec<_>>().join(".")
    }

    /// One expression.
    pub fn expr(&self, index: ExprRef) -> Expr {
        self.exprs[index as usize]
    }

    /// One from item.
    pub fn source(&self, index: SourceRef) -> Source {
        self.sources[index as usize]
    }

    /// One query.
    pub fn query(&self, index: QueryRef) -> Query {
        self.queries[index as usize]
    }

    /// One select block.
    pub fn select(&self, index: SelectRef) -> Select {
        self.selects[index as usize]
    }

    /// The expressions of a list.
    pub fn expr_list(&self, slice: Slice) -> &[ExprRef] {
        &self.expr_lists[slice.range()]
    }

    /// The from items of a list.
    pub fn source_list(&self, slice: Slice) -> &[SourceRef] {
        &self.source_lists[slice.range()]
    }

    /// The entries of a target list.
    pub fn target_list(&self, slice: Slice) -> &[Target] {
        &self.targets[slice.range()]
    }

    /// The entries of an order by list.
    pub fn order_list(&self, slice: Slice) -> &[OrderItem] {
        &self.order_items[slice.range()]
    }

    /// The arms of a case.
    pub fn arm_list(&self, slice: Slice) -> &[CaseArm] {
        &self.case_arms[slice.range()]
    }

    /// One `CREATE TABLE`.
    pub fn create_table(&self, index: CreateTableRef) -> CreateTable {
        self.create_tables[index as usize]
    }

    /// One `CREATE VIEW`.
    pub fn create_view(&self, index: CreateViewRef) -> CreateView {
        self.create_views[index as usize]
    }

    /// One `DROP TABLE`.
    pub fn drop_table(&self, index: DropTableRef) -> DropTable {
        self.drop_tables[index as usize]
    }

    /// One `INSERT`.
    pub fn insert(&self, index: InsertRef) -> Insert {
        self.inserts[index as usize]
    }

    /// One `SET` or `RESET`.
    pub fn setting(&self, index: SettingRef) -> Setting {
        self.settings[index as usize]
    }

    /// The column definitions of a `CREATE TABLE`.
    pub fn column_defs(&self, slice: Slice) -> &[ColumnDef] {
        &self.column_defs[slice.range()]
    }

    /// The names of a name list, each of which is itself a run of parts.
    pub fn name_list(&self, slice: Slice) -> &[Slice] {
        &self.name_lists[slice.range()]
    }

    /// The rows of a `VALUES`, each of which is itself a run of expressions.
    pub fn rows(&self, slice: Slice) -> &[Slice] {
        &self.rows[slice.range()]
    }

    /// How many nodes the whole tree is, across every arena.
    ///
    /// The number to watch when the transformer changes. A parse tree of five thousand nodes that
    /// becomes an AST of thirty is the twenty precedence levels being thrown away, which is the
    /// whole reason this module exists.
    pub fn node_count(&self) -> usize {
        self.queries.len() + self.selects.len() + self.exprs.len() + self.sources.len()
    }
}
