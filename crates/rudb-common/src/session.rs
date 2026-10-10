//! What a session has set, for the tables and functions that read a setting back.
//!
//! Here for the layer rule and not because a setting is a kind of value, which is the same reason
//! [`crate::Cancel`] is here. The thing that fills this in is the embedding API at rank 13, which is
//! the only place that knows what `SET memory_limit` left behind, and the thing that reads it is the
//! executor at rank 12, where `duckdb_settings()` is built. No two crates in between can see each
//! other, so the only place both of them can see is the bottom.
//!
//! Strings on both sides, rather than a value per setting. A setting is written as text by `SET`,
//! read back as text by `current_setting()` and printed as text by `duckdb_settings()`, and the one
//! place the type matters is the `input_type` column, which is a fact about the setting rather than
//! about the session. Holding a `Value` here would mean the rendering happened twice, once for each
//! reader, and the two would eventually disagree about how many decimal places a memory limit has.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::Rules;
use crate::types::LogicalType;
use crate::tzdb::{Release, Zone};
use crate::value::Value;

/// The settings a session has, by name.
///
/// Every setting the engine has, not only the ones somebody changed. A reader of this is answering
/// "what is it now", so a name that is missing means the engine does not have that setting rather
/// than that it is at its default.
///
/// The names and values are shared between copies. The database builds a session once and hands a
/// copy to every statement, and with a couple of hundred settings a deep copy of the map was most
/// of what a `SELECT 1` cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    values: Arc<BTreeMap<String, String>>,
    time_zone: SessionTimeZone,
    semantics: Semantics,
    rules: Rules,
    links: String,
    seams: String,
    variables: Variables,
    postgres: Postgreses,
    begun: Begun,
    transaction: Transaction,
    prepared: Prepareds,
    /// The names of the settings this connection set for itself, under every spelling, which
    /// `duckdb_settings()` reports as `LOCAL` whatever their usual scope is.
    local: Arc<Vec<String>>,
}

/// When the open transaction began and when the statement arrived, in microseconds since the
/// epoch, or none.
///
/// Every copy compares equal, so a plan cached against a session is not lost at each statement.
/// That is safe because a plan that reads the instant is never cached, see `simple_cacheable`.
#[derive(Debug, Clone, Copy, Default)]
struct Begun {
    transaction: Option<i64>,
    statement: Option<i64>,
}

impl PartialEq for Begun {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Begun {}

/// What a PostgreSQL session gives the engine to read: its parameters and the text of `version()`.
///
/// The server owns the parameters and changes them. The engine gets a new copy each time they
/// change, which is cheap because the parameters hold only what the session touched.
#[derive(Debug, Clone)]
pub struct Postgres {
    /// The parameters of the session.
    pub settings: crate::guc::Settings,
    /// What `version()` returns.
    pub version: String,
    /// The input functions of the types, which read the text of a string literal in a cast.
    pub input: Option<Arc<dyn LiteralInput>>,
    /// The backend number of the session, which holds its advisory locks in [`crate::advisory`].
    pub backend: i32,
    /// The OID of the database of the session, which is part of the key of an advisory lock.
    pub database: u32,
}

/// Reads the text of a literal such as `'infinity'::date` with the input function of the type, as
/// PostgreSQL does, and not with the cast of the engine.
pub trait LiteralInput: Send + Sync + std::fmt::Debug {
    /// The value of `text` as the PostgreSQL type `oid`, or `None` when the type has no input
    /// function here.
    fn read(&self, oid: u32, text: &str) -> Option<crate::Result<Value>>;
}

/// The PostgreSQL session, if there is one, compared by identity. A new copy is a new value, so a
/// cached plan that read the old copy is not used again.
#[derive(Debug, Clone, Default)]
struct Postgreses(Option<Arc<Postgres>>);

impl PartialEq for Postgreses {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(one), Some(other)) => Arc::ptr_eq(one, other),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for Postgreses {}

/// The number of the transaction a statement runs in, which `txid_current()` answers with.
///
/// Every copy compares equal, because the number changes with every statement outside a block and
/// a plan kept for a statement's text would otherwise never be used twice. No kept plan reads it:
/// `txid_current()` is a call, and a statement with a call in it is not kept.
#[derive(Debug, Clone, Copy, Default)]
struct Transaction(u64);

impl PartialEq for Transaction {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Transaction {}

/// One statement `PREPARE` named, the way `duckdb_prepared_statements()` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStatement {
    /// The name as it was written.
    pub name: String,
    /// The statement written back out, which is how the pin prints it rather than how it was typed.
    pub statement: String,
    /// How many parameters it takes. The pin lists each one as `UNKNOWN`, whatever it is used as.
    pub parameters: usize,
    /// The types of the columns it answers, `BIGINT` alone for a write with no `RETURNING`. `None`
    /// where the pin plans the statement again at every `EXECUTE` and so has no types to give.
    pub results: Option<Vec<LogicalType>>,
}

/// The statements a connection holds by name, in the order of their names in lower case.
///
/// Shared, so reading the session once per statement copies a pointer and not the list. Two are
/// equal when they hold the same statements, so the session a plan was kept against stops matching
/// once a statement is prepared or deallocated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Prepareds(Arc<[PreparedStatement]>);

/// One value `SET VARIABLE` left behind, with the type it was computed at.
///
/// The type is held beside the value rather than read off it, because a variable set to a query
/// that found no row is a null that is still an INTEGER, and `typeof(getvariable('a'))` says so on
/// the pin.
#[derive(Debug, Clone, PartialEq)]
pub struct Variable {
    /// The name as it was written, which is what `duckdb_variables()` lists.
    pub name: String,
    /// What the expression came to.
    pub value: Value,
    /// The type the expression had.
    pub ty: LogicalType,
}

/// The variables of a session, in the order they were first set.
///
/// Shared between copies for the reason the settings are, and compared by what they hold so that a
/// session is still something a cached plan can be checked against. A value is compared with its
/// own equality, so a variable holding a NaN makes two sessions differ, which costs a cached plan
/// and nothing else.
#[derive(Debug, Clone, Default)]
struct Variables(Arc<Vec<Variable>>);

impl PartialEq for Variables {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0) || self.0 == other.0
    }
}

impl Eq for Variables {}

/// The meaning-changing session choices consumed while a query is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Semantics {
    aggregate_types: AggregateTypes,
    cast_input: CastInput,
    number_casts: NumberCasts,
    cast_output: CastOutput,
    condition_types: ConditionTypes,
    common_types: CommonTypes,
    table_names: TableNames,
    distinct_order: DistinctOrder,
    count_types: CountTypes,
    query_columns: QueryColumns,
    conflict_arbiter: ConflictArbiter,
    explain_output: ExplainOutput,
    maintenance: Maintenance,
    character_types: CharacterTypes,
    column_names: ColumnNames,
    default_descending: bool,
    default_null_order: DefaultNullOrder,
    disable_timestamptz_casts: bool,
    error_texts: ErrorTexts,
    errors_as_json: bool,
    float_range: FloatRange,
    function_rules: FunctionRules,
    from_functions: FromFunctions,
    integer_division: bool,
    ieee_floating_point_ops: bool,
    identifier_case: IdentifierCase,
    identifier_compare: IdentifierCompare,
    insert_columns: InsertColumns,
    join_columns: JoinColumns,
    null_on_division_by_zero: bool,
    number_literals: NumberLiterals,
    operator_rules: OperatorRules,
    order_by_non_integer_literal: bool,
    pivot_limit: u64,
    plan_errors: PlanErrors,
    recursive_union: RecursiveUnion,
    regex_match_full: bool,
    regex_rules: RegexRules,
    row_fields: RowFields,
    row_nulls: RowNulls,
    scalar_subquery_error_on_multiple_rows: bool,
    sequence_owners: SequenceOwners,
    set_functions: SetFunctions,
    show_behavior: ShowBehavior,
    sort_operators: SortOperators,
    tie_order: TieOrder,
    window_order: WindowOrder,
    empty_targets: EmptyTargets,
    unread_queries: UnreadQueries,
    row_comparisons: RowComparisons,
    subscripts: Subscripts,
    collations: Collations,
    single_arrow_lambdas: bool,
    type_names: TypeNames,
    unknown_types: UnknownTypes,
    values_names: ValuesNames,
    warnings_as_errors: bool,
}

impl Default for Semantics {
    fn default() -> Self {
        Self {
            aggregate_types: AggregateTypes::Pin,
            cast_input: CastInput::Pin,
            number_casts: NumberCasts::Pin,
            cast_output: CastOutput::Pin,
            condition_types: ConditionTypes::Pin,
            common_types: CommonTypes::Pin,
            table_names: TableNames::Pin,
            distinct_order: DistinctOrder::Pin,
            count_types: CountTypes::Pin,
            query_columns: QueryColumns::Pin,
            conflict_arbiter: ConflictArbiter::Pin,
            explain_output: ExplainOutput::Pin,
            maintenance: Maintenance::Pin,
            from_functions: FromFunctions::Pin,
            float_range: FloatRange::Pin,
            row_fields: RowFields::Pin,
            row_nulls: RowNulls::Pin,
            character_types: CharacterTypes::Pin,
            column_names: ColumnNames::Pin,
            default_descending: false,
            default_null_order: DefaultNullOrder::default(),
            disable_timestamptz_casts: false,
            error_texts: ErrorTexts::Pin,
            errors_as_json: false,
            function_rules: FunctionRules::Pin,
            integer_division: false,
            ieee_floating_point_ops: true,
            identifier_case: IdentifierCase::Preserve,
            identifier_compare: IdentifierCompare::CaseInsensitive,
            insert_columns: InsertColumns::Exact,
            join_columns: JoinColumns::InPlace,
            null_on_division_by_zero: false,
            number_literals: NumberLiterals::Pin,
            operator_rules: OperatorRules::Pin,
            order_by_non_integer_literal: false,
            pivot_limit: 100_000,
            plan_errors: PlanErrors::Pin,
            recursive_union: RecursiveUnion::Pin,
            regex_match_full: false,
            regex_rules: RegexRules::Pin,
            scalar_subquery_error_on_multiple_rows: true,
            sequence_owners: SequenceOwners::Table,
            set_functions: SetFunctions::Pin,
            sort_operators: SortOperators::Pin,
            tie_order: TieOrder::Pin,
            window_order: WindowOrder::Pin,
            empty_targets: EmptyTargets::Pin,
            unread_queries: UnreadQueries::Pin,
            row_comparisons: RowComparisons::Pin,
            show_behavior: ShowBehavior::Auto,
            subscripts: Subscripts::Pin,
            collations: Collations::Pin,
            single_arrow_lambdas: false,
            type_names: TypeNames::Pin,
            unknown_types: UnknownTypes::Pin,
            values_names: ValuesNames::FromZero,
            warnings_as_errors: false,
        }
    }
}

impl Semantics {
    /// Whether errors are returned as structured JSON.
    #[must_use]
    pub fn errors_as_json(self) -> bool {
        self.errors_as_json
    }
    /// How unquoted identifiers are folded while a statement is parsed.
    #[must_use]
    pub fn identifier_case(self) -> IdentifierCase {
        self.identifier_case
    }
    /// How two identifiers are compared after the parser.
    #[must_use]
    pub fn identifier_compare(self) -> IdentifierCompare {
        self.identifier_compare
    }
    /// How the values of an `INSERT` are matched to the columns of the table.
    #[must_use]
    pub fn insert_columns(self) -> InsertColumns {
        self.insert_columns
    }
    /// Where the columns that a `USING` or `NATURAL` join merges go, and what they are.
    #[must_use]
    pub fn join_columns(self) -> JoinColumns {
        self.join_columns
    }
    /// The result types of `sum` and `avg`.
    #[must_use]
    pub fn aggregate_types(self) -> AggregateTypes {
        self.aggregate_types
    }
    /// The names of the columns of a `VALUES` list.
    #[must_use]
    pub fn values_names(self) -> ValuesNames {
        self.values_names
    }
    /// The rules of `char(n)` and `varchar(n)`.
    #[must_use]
    pub fn character_types(self) -> CharacterTypes {
        self.character_types
    }
    /// The name of a result column that has no alias.
    #[must_use]
    pub fn column_names(self) -> ColumnNames {
        self.column_names
    }
    /// The type of a number literal.
    #[must_use]
    pub fn number_literals(self) -> NumberLiterals {
        self.number_literals
    }
    /// The type names that a column definition can use.
    #[must_use]
    pub fn type_names(self) -> TypeNames {
        self.type_names
    }
    /// The rules of the operators that are different between the dialects.
    #[must_use]
    pub fn operator_rules(self) -> OperatorRules {
        self.operator_rules
    }
    /// How a parameter or a literal of no type gets its type.
    #[must_use]
    pub fn unknown_types(self) -> UnknownTypes {
        self.unknown_types
    }
    /// The words and the SQLSTATE of an error that the dialects report differently.
    #[must_use]
    pub fn error_texts(self) -> ErrorTexts {
        self.error_texts
    }
    /// The rules of the functions that are different between the dialects.
    #[must_use]
    pub fn function_rules(self) -> FunctionRules {
        self.function_rules
    }
    /// When an error of a constant part of a query is raised.
    #[must_use]
    pub fn plan_errors(self) -> PlanErrors {
        self.plan_errors
    }
    /// How a recursive `UNION` without `ALL` finds the rows it has made.
    #[must_use]
    pub fn recursive_union(self) -> RecursiveUnion {
        self.recursive_union
    }
    /// How an explicit cast reads a string.
    #[must_use]
    pub fn cast_input(self) -> CastInput {
        self.cast_input
    }
    /// How a cast between two number types checks the range of its value.
    #[must_use]
    pub fn number_casts(self) -> NumberCasts {
        self.number_casts
    }
    /// How a cast to text writes a value.
    #[must_use]
    pub fn cast_output(self) -> CastOutput {
        self.cast_output
    }
    /// What type a condition takes.
    #[must_use]
    pub fn condition_types(self) -> ConditionTypes {
        self.condition_types
    }
    /// What one type the values of a `CASE`, a `COALESCE` or an `ARRAY` take.
    #[must_use]
    pub fn common_types(self) -> CommonTypes {
        self.common_types
    }
    /// Whether two items of one `FROM` can have the same name.
    #[must_use]
    pub fn table_names(self) -> TableNames {
        self.table_names
    }
    /// What a `SELECT DISTINCT` can sort on.
    #[must_use]
    pub fn distinct_order(self) -> DistinctOrder {
        self.distinct_order
    }
    /// What type the count of a `LIMIT` and an `OFFSET` takes.
    #[must_use]
    pub fn count_types(self) -> CountTypes {
        self.count_types
    }
    /// What a table or a view made of a query does with two columns of one name.
    #[must_use]
    pub fn query_columns(self) -> QueryColumns {
        self.query_columns
    }
    /// Which key an `ON CONFLICT` can name and when a target that matches no key is refused.
    #[must_use]
    pub fn conflict_arbiter(self) -> ConflictArbiter {
        self.conflict_arbiter
    }
    /// Which options `VACUUM` and `ANALYZE` take, and what they do with a view and in a
    /// transaction block.
    #[must_use]
    pub fn maintenance(self) -> Maintenance {
        self.maintenance
    }
    /// Which options `EXPLAIN` takes and what it prints.
    #[must_use]
    pub fn explain_output(self) -> ExplainOutput {
        self.explain_output
    }
    /// What a float operator gives for a result past the range of its type.
    #[must_use]
    pub fn float_range(self) -> FloatRange {
        self.float_range
    }
    /// What a function in `FROM` that is not a table function is.
    #[must_use]
    pub fn from_functions(self) -> FromFunctions {
        self.from_functions
    }
    /// What the fields of a row value are named, and how a field is found by its name.
    #[must_use]
    pub fn row_fields(self) -> RowFields {
        self.row_fields
    }
    /// When a row value `IS NULL` and when it `IS NOT NULL`.
    #[must_use]
    pub fn row_nulls(self) -> RowNulls {
        self.row_nulls
    }
    /// What the name after `OWNED BY` of a sequence names.
    #[must_use]
    pub fn sequence_owners(self) -> SequenceOwners {
        self.sequence_owners
    }
    /// Which set returning functions a select list can call.
    #[must_use]
    pub fn set_functions(self) -> SetFunctions {
        self.set_functions
    }
    /// Which types a sort, a grouping, a `DISTINCT` and a set operation can take.
    #[must_use]
    pub fn sort_operators(self) -> SortOperators {
        self.sort_operators
    }
    /// What order a sort leaves the rows in whose keys tie.
    #[must_use]
    pub fn tie_order(self) -> TieOrder {
        self.tie_order
    }
    /// What order the windows of one query are computed in.
    #[must_use]
    pub fn window_order(self) -> WindowOrder {
        self.window_order
    }
    /// Whether a `SELECT` can have no targets.
    #[must_use]
    pub fn empty_targets(self) -> EmptyTargets {
        self.empty_targets
    }
    /// Whether a scalar query whose value nothing reads is run.
    #[must_use]
    pub fn unread_queries(self) -> UnreadQueries {
        self.unread_queries
    }
    /// How a row written out is compared with another one, or with a query of several columns.
    #[must_use]
    pub fn row_comparisons(self) -> RowComparisons {
        self.row_comparisons
    }
    /// How a subscript and a slice of a list read the list.
    #[must_use]
    pub fn subscripts(self) -> Subscripts {
        self.subscripts
    }
    /// What `COLLATE` names and how the collation of an expression comes from its inputs.
    #[must_use]
    pub fn collations(self) -> Collations {
        self.collations
    }
    /// Whether casts from local timestamps to zoned timestamps are refused.
    #[must_use]
    pub fn disable_timestamptz_casts(self) -> bool {
        self.disable_timestamptz_casts
    }

    /// Whether floating division and remainder use IEEE answers for zero divisors.
    #[must_use]
    pub fn ieee_floating_point_ops(self) -> bool {
        self.ieee_floating_point_ops
    }

    /// Whether an order item with no direction is descending.
    #[must_use]
    pub fn default_descending(self) -> bool {
        self.default_descending
    }

    /// Whether nulls precede values for an unstated placement in this direction.
    #[must_use]
    pub fn nulls_first(self, descending: bool) -> bool {
        match self.default_null_order {
            DefaultNullOrder::First => true,
            DefaultNullOrder::Last => false,
            DefaultNullOrder::Sqlite => !descending,
            DefaultNullOrder::Postgres => descending,
        }
    }

    /// Whether `/` is bound as the integer division operator.
    #[must_use]
    pub fn integer_division(self) -> bool {
        self.integer_division
    }

    /// Whether a division that would raise on a zero divisor yields null instead.
    #[must_use]
    pub fn null_on_division_by_zero(self) -> bool {
        self.null_on_division_by_zero
    }

    /// Whether a constant non-integer expression is accepted as a sort key.
    #[must_use]
    pub fn order_by_non_integer_literal(self) -> bool {
        self.order_by_non_integer_literal
    }

    /// The most columns a pivot may make.
    #[must_use]
    pub fn pivot_limit(self) -> u64 {
        self.pivot_limit
    }

    /// Whether regex match operators require the entire string to match.
    #[must_use]
    pub fn regex_match_full(self) -> bool {
        self.regex_match_full
    }

    /// Which syntax and which matching rules a regular expression has.
    #[must_use]
    pub fn regex_rules(self) -> RegexRules {
        self.regex_rules
    }

    /// Whether a lambda may be written with the deprecated arrow, `x -> x + 1`, which is what
    /// `lambda_syntax = 'ENABLE_SINGLE_ARROW'` turns on.
    #[must_use]
    pub fn single_arrow_lambdas(self) -> bool {
        self.single_arrow_lambdas
    }

    /// Whether a scalar query producing several rows raises an error.
    #[must_use]
    pub fn scalar_subquery_error_on_multiple_rows(self) -> bool {
        self.scalar_subquery_error_on_multiple_rows
    }

    /// How a bare name following `SHOW` is resolved.
    #[must_use]
    pub fn show_behavior(self) -> ShowBehavior {
        self.show_behavior
    }

    /// Whether warnings are promoted to errors.
    #[must_use]
    pub fn warnings_as_errors(self) -> bool {
        self.warnings_as_errors
    }
}

/// How a session folds identifiers that were not quoted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdentifierCase {
    /// Keep the spelling in the statement.
    #[default]
    Preserve,
    /// Fold ASCII letters to lowercase.
    Lower,
    /// Fold ASCII letters to uppercase.
    Upper,
}

/// How two identifiers are compared after the parser, when a name is looked up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdentifierCompare {
    /// ASCII letters compare without regard to case, as in DuckDB. `"A"` and `"a"` are one name.
    #[default]
    CaseInsensitive,
    /// The bytes compare, as in PostgreSQL. The parser folds each unquoted name, so `A` and `a`
    /// are one name, and `"A"` and `"a"` are two.
    Exact,
}

impl IdentifierCompare {
    /// Whether two identifiers name the same object.
    #[must_use]
    pub fn same(self, left: &str, right: &str) -> bool {
        match self {
            Self::CaseInsensitive => left.eq_ignore_ascii_case(right),
            Self::Exact => left == right,
        }
    }

    /// The place of the one name in `names` that is the same as `written`, or `None` when no name
    /// or more than one name is.
    #[must_use]
    pub fn find<'a>(
        self,
        names: impl IntoIterator<Item = &'a str>,
        written: &str,
    ) -> Option<usize> {
        let mut found = names.into_iter().enumerate().filter(|(_, name)| self.same(name, written));
        let (at, _) = found.next()?;
        found.next().is_none().then_some(at)
    }
}

/// How the values of an `INSERT` are matched to the columns of the table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InsertColumns {
    /// Each row has one value for each column that the statement writes, as in DuckDB.
    #[default]
    Exact,
    /// As in PostgreSQL: with no column list, a row can have fewer values than the table has
    /// columns. The values go to the leading columns, and the other columns take their defaults.
    /// The errors are the ones of PostgreSQL.
    Leading,
}

/// Where the columns that a `USING` or `NATURAL` join merges go, and what they are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoinColumns {
    /// As in DuckDB: a merged column has the place of its left copy. An inner or a left join reads
    /// the left copy at its own type. A name that `USING` gives two times is one name.
    #[default]
    InPlace,
    /// As in PostgreSQL: the merged columns come first, in the order of `USING`, or in the order
    /// of the left side for `NATURAL`. Each merged column has the common type of its two copies
    /// for each kind of join. A name that `USING` gives two times is an error.
    MergedFirst,
}

/// The result types of `sum` and `avg`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AggregateTypes {
    /// As in DuckDB: `sum` of an integer is a `HUGEINT`, `sum` of a `FLOAT` is a `DOUBLE`, and
    /// `avg` of an integer or a decimal is a `DOUBLE`.
    #[default]
    Pin,
    /// As in PostgreSQL: `sum` of an `int2` or an `int4` is an `int8`, `sum` of a `float4` is a
    /// `float4`, and `avg` of an integer or a `numeric` is a `numeric`.
    Postgres,
}

/// The rules of `char(n)` and `varchar(n)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CharacterTypes {
    /// As in DuckDB: the length is not checked, and a `char(n)` value has no padding.
    #[default]
    Pin,
    /// As in PostgreSQL: a store refuses a value that is too long with `22001`, an explicit cast
    /// cuts it, and a comparison with a `char(n)` value ignores the trailing spaces.
    Postgres,
}

/// The name of a result column that has no alias.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColumnNames {
    /// As in DuckDB: the text of the expression, so `SELECT 1` has a column `1`.
    #[default]
    Pin,
    /// As in PostgreSQL: the name that `FigureColname` gives, such as the name of a column or of
    /// a function, the name of the type of a cast, or `?column?`.
    Postgres,
}

/// The type of a number literal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NumberLiterals {
    /// As in DuckDB: a number with an exponent is a `DOUBLE`, an integer past `BIGINT` is a
    /// `HUGEINT` or a `BIGNUM`, and an integer literal takes the integer type that it meets in an
    /// operator when its value fits.
    #[default]
    Pin,
    /// As in PostgreSQL: a number with an exponent, an integer past `bigint` and a decimal past 38
    /// digits are a `numeric`, and an integer literal keeps its own type.
    Postgres,
}

/// The rules of the operators that are different between the dialects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OperatorRules {
    /// The operators of DuckDB.
    #[default]
    Pin,
    /// The operators of PostgreSQL where they are different: `date - date` is an `int4`, `/`
    /// divides an `interval`, a division of two exact numbers where one is not an integer is a
    /// `numeric` division, and a string literal joined to a `bytea` is a `bytea`.
    Postgres,
}

/// How a parameter or a literal of no type gets its type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnknownTypes {
    /// As in DuckDB: a parameter takes the type that the function resolution of rudb gives it.
    #[default]
    Pin,
    /// As in PostgreSQL: a parameter of no type takes the type of the other operand, of the
    /// elements of an array, of the column that an `INSERT` writes or the `bigint` of a `LIMIT`.
    /// It prefers `text` to a `bytea`, and it is a `text` as a result column or as the argument of
    /// `min` or `max`. A null parameter has the type that the client declared for it.
    Postgres,
}

/// The words and the SQLSTATE of an error that the dialects report differently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ErrorTexts {
    /// The text of DuckDB.
    #[default]
    Pin,
    /// The text and the SQLSTATE of PostgreSQL, such as `there is no parameter $1` with `42P02`
    /// and `LIMIT must not be negative` with `2201W`.
    Postgres,
}

/// The rules of the functions that are different between the dialects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FunctionRules {
    /// The functions of DuckDB.
    #[default]
    Pin,
    /// The functions of PostgreSQL where they are different: `every`, `pg_typeof` and a
    /// `current_setting` that reads the settings of the session are there, the result types are
    /// the ones of PostgreSQL, such as an `int4` for `generate_series` over `int4` and a `float8`
    /// for `date_part`, and an advisory lock function gives `void`.
    Postgres,
}

/// When an error of a constant part of a query is raised.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlanErrors {
    /// As in DuckDB: the error comes when the query reads the value, so a part that no row
    /// reaches raises nothing.
    #[default]
    Pin,
    /// As in PostgreSQL: the planner folds each call whose arguments are all constants, and an
    /// error there fails the statement before it makes a row.
    Postgres,
}

/// How a recursive `UNION` without `ALL` finds the rows it has made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RecursiveUnion {
    /// As in DuckDB: a column of each type can take part, because the engine compares the values
    /// it has kept.
    #[default]
    Pin,
    /// As in PostgreSQL: the rows go in a hash table, so a column whose type does not hash, such
    /// as `bit varying`, is `0A000 could not implement recursive UNION`.
    Postgres,
}

/// How an explicit cast reads a string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CastInput {
    /// As in DuckDB: the cast of the engine reads the string, so `'1.5'::text::int` is 2.
    #[default]
    Pin,
    /// As in PostgreSQL: the input function of the type reads the string, with its rules and its
    /// errors, so `'1.5'::text::int` is `22P02 invalid input syntax for type integer`.
    Postgres,
}

/// How a cast between two number types checks the range of its value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NumberCasts {
    /// As in DuckDB: the cast of the engine checks it, so `70000::int2` is a conversion error, and
    /// a double too big for a `FLOAT` becomes infinity.
    #[default]
    Pin,
    /// As in PostgreSQL: the cast function of the type pair checks it, so `70000::int2` is `22003
    /// smallint out of range`, and `1e300::float8::float4` is `22003 value out of range: overflow`.
    Postgres,
}

/// How a cast to text writes a value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CastOutput {
    /// As in DuckDB: the cast of the engine writes it, so `array[1, 2]::text` is `[1, 2]` and
    /// `'NaN'::double::text` is `nan`.
    #[default]
    Pin,
    /// As in PostgreSQL: the output function of the type writes it, so `array[1, 2]::text` is
    /// `{1,2}` and `'NaN'::float8::text` is `NaN`.
    Postgres,
}

/// What type a condition takes, in `WHERE`, `HAVING`, `JOIN ... ON`, `CASE WHEN`, `AND`, `OR`,
/// `NOT` and `IS TRUE`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConditionTypes {
    /// As in DuckDB: a number or a string is cast to a boolean, so `WHERE 1` keeps each row.
    #[default]
    Pin,
    /// As in PostgreSQL: a boolean, a NULL, or a string literal that the input function of
    /// `boolean` reads. Any other type is `42804 argument of WHERE must be type boolean, not type
    /// integer`.
    Postgres,
}

/// What one type the values take that must have one type: the results of a `CASE`, the values
/// of a `COALESCE`, a `GREATEST`, a `LEAST`, an `ARRAY` and an `IN` list, and the columns of a
/// `VALUES` and of a `UNION`, an `INTERSECT` and an `EXCEPT`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommonTypes {
    /// As in DuckDB: the values meet at the type that holds all of them, and a string literal
    /// among numbers is cast as a string.
    #[default]
    Pin,
    /// As in PostgreSQL: the type that `select_common_type` picks from the category and the
    /// preferred type of each value. A string literal and a NULL take that type, and a string
    /// literal is read with the input function of the type. Two types in different categories are
    /// `42804 COALESCE types integer and text cannot be matched`.
    Postgres,
}

/// Whether two items of one `FROM` can have the same name, which is the alias or else the name of
/// the table, the `WITH` query or the function.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TableNames {
    /// As in DuckDB: they can, and only a reference through the name, such as `t.a` or `*`, is
    /// an error.
    #[default]
    Pin,
    /// As in PostgreSQL: two items of one `FROM`, or the two sides of a join, cannot, and the
    /// query is `42712 table name "t" specified more than once`. Two tables with no alias are
    /// the exception when they are two different tables, such as `s.t` and `r.t`.
    Postgres,
}

/// What a `SELECT DISTINCT` can sort on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DistinctOrder {
    /// As in DuckDB: anything. A plain `DISTINCT` that sorts on a column it does not select is a
    /// `DISTINCT ON` the columns that it selects, and a `DISTINCT ON` sorts on any columns.
    #[default]
    Pin,
    /// As in PostgreSQL: a plain `DISTINCT` sorts only on the columns that it selects, and the
    /// `ORDER BY` of a `DISTINCT ON` starts with the expressions of the `DISTINCT ON`. Each other
    /// query is `42P10`.
    Postgres,
}

/// What a table or a view made of a query does with two columns of one name, in `CREATE TABLE
/// AS` and `CREATE VIEW`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueryColumns {
    /// As in DuckDB: when the statement has no column list, the second column takes the name with
    /// `_1`, so `CREATE TABLE t AS SELECT 1 AS x, 2 AS x` has the columns `x` and `x_1`.
    #[default]
    Pin,
    /// As in PostgreSQL: the statement is `42701 column "x" specified more than once`, after the
    /// column list renames the first columns.
    Postgres,
}

/// What a float operator gives for a result past the range of its type.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FloatRange {
    /// As in DuckDB: the result of IEEE 754, an infinity or a zero.
    #[default]
    Pin,
    /// As in PostgreSQL: `+`, `-`, `*` and `/` of `float4` and `float8` check the result as
    /// `float.h` does. An infinity from operands that are not infinite is `22003 value out of
    /// range: overflow`, a zero from a product or a quotient of operands that are not zero is
    /// `22003 value out of range: underflow`, and a zero divisor is `22012 division by zero`.
    Postgres,
}

/// What a function in `FROM` that is not a table function is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FromFunctions {
    /// As in DuckDB: only a table function can be in `FROM`.
    #[default]
    Pin,
    /// As in PostgreSQL: any function can be in `FROM`. A function that gives one value is a
    /// relation of one row, and a function that gives a row has a column for each field.
    Postgres,
}

/// What the fields of a row value are named, and how a field is found by its name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RowFields {
    /// As in DuckDB: the fields of `row(...)` have no names, and a name is found without case.
    #[default]
    Pin,
    /// As in PostgreSQL: the fields of `row(...)` are `f1`, `f2` and so on, a name must be the
    /// same, and a name that no field has is `42703`.
    Postgres,
}

/// When a row value `IS NULL` and when it `IS NOT NULL`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RowNulls {
    /// As in DuckDB: the test is about the row itself, so `row(1, null) IS NOT NULL` is true.
    #[default]
    Pin,
    /// As in PostgreSQL and the SQL standard: a row `IS NULL` when each of its fields is null and
    /// `IS NOT NULL` when none of them is, so `row(1, null)` is neither. The test does not look
    /// inside a field that is a row itself.
    Postgres,
}

/// Which options `EXPLAIN` takes and what it prints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExplainOutput {
    /// As in DuckDB: the options `ANALYZE`, `LOGICAL`, `STATISTICS` and `CODEGEN`, and the two
    /// columns `explain_key` and `explain_value`.
    #[default]
    Pin,
    /// As in PostgreSQL: the options of `ExplainQuery`, checked after the query is bound, and one
    /// column `QUERY PLAN` in the format the options ask for, with the node names of PostgreSQL.
    Postgres,
}

/// Which key an `ON CONFLICT` can name, and when a target that matches no key is refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConflictArbiter {
    /// As in DuckDB: a target that matches no key is refused before the `DO UPDATE` is read, and
    /// an `ON CONFLICT` with no target is refused when the table has no key.
    #[default]
    Pin,
    /// As in PostgreSQL: the `DO UPDATE` is read first, so an error in it comes first. A target
    /// that matches no key is then `42P10`. An `ON CONFLICT DO NOTHING` with no target is allowed
    /// when the table has no key, and it skips no row.
    Postgres,
}

/// Which options `VACUUM` and `ANALYZE` take, and what they do with a view and in a transaction
/// block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Maintenance {
    /// As in DuckDB: `ANALYZE` and `VACUUM ANALYZE` only. `FULL`, `FREEZE` and `VERBOSE` are not
    /// implemented, a view is an error, and both run in a transaction block.
    #[default]
    Pin,
    /// As in PostgreSQL: the options of `ExecVacuum`, a column list only with `ANALYZE`, a view is
    /// skipped with a warning, and `VACUUM` does not run in a transaction block.
    Postgres,
}

/// What type the count of a `LIMIT` and an `OFFSET` takes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CountTypes {
    /// As in DuckDB: any value that the engine casts to `BIGINT`, so `LIMIT true` is one row.
    #[default]
    Pin,
    /// As in PostgreSQL: a `bigint`, or a type with an implicit or an assignment cast to
    /// `bigint`. A string literal is read with the input function of `bigint`. Any other type is
    /// `42804 argument of LIMIT must be type bigint, not type boolean`.
    Postgres,
}

/// Which syntax and which matching rules a regular expression has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RegexRules {
    /// As in DuckDB: the syntax of RE2, and the first match that the leftmost alternative gives.
    #[default]
    Pin,
    /// As in PostgreSQL: the syntax of Spencer's engine, and the longest match from the leftmost
    /// start. `regexp_match` gives a `text[]` and `regexp_matches` gives a set of them.
    Postgres,
}

/// What the name after `OWNED BY` of a sequence names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SequenceOwners {
    /// As in DuckDB: the name is a table, with its schema in front if it has one.
    #[default]
    Table,
    /// As in PostgreSQL: the last part of the name is a column of the table.
    Column,
}

/// Which set returning functions a select list can call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SetFunctions {
    /// As in DuckDB: only `unnest`.
    #[default]
    Pin,
    /// As in PostgreSQL: also `generate_series`, which gives one row for each value.
    Postgres,
}

/// Which types a sort, a grouping, a `DISTINCT` and a set operation can take.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortOperators {
    /// As in DuckDB: each type, because the engine compares any two values of one type.
    #[default]
    Pin,
    /// As in PostgreSQL: a key compares with the equality operator of its type and sorts with
    /// the ordering operator, which come from the default btree or hash operator class. A type
    /// with none, such as `json`, is `42883 could not identify an equality operator for type
    /// json`.
    Postgres,
}

/// What order a sort leaves the rows in whose keys tie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TieOrder {
    /// As in DuckDB: the rows that tie stay in the order they arrived in, whatever the number of
    /// threads.
    #[default]
    Pin,
    /// As in PostgreSQL: the rows that tie are in the order its in-memory sort leaves them in,
    /// which is a quicksort, or a radix sort when the first key compares as an integer. Neither
    /// is stable, so a window function that numbers the rows or reads a neighbor sees that order.
    Postgres,
}

/// What order the windows of one query are computed in. Each window sorts the rows the window
/// before it made, so the last one decides the order of a query with no `ORDER BY`, and the one
/// before it decides the order of the rows that tie in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WindowOrder {
    /// As in DuckDB: in the order they were written in.
    #[default]
    Pin,
    /// As in PostgreSQL: in the order `select_active_windows` puts them in, which compares the
    /// sort keys of the windows. See `rudb_bind::windoworder`.
    Postgres,
}

/// Whether a `SELECT` can have no targets, as in `SELECT FROM t`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmptyTargets {
    /// As in DuckDB: a `SELECT` needs at least one target.
    #[default]
    Pin,
    /// As in PostgreSQL: a `SELECT` with no targets gives rows with no columns, one for each row
    /// of its `FROM`.
    Postgres,
}

/// Whether a scalar query whose value nothing reads is run, as in
/// `SELECT a FROM (SELECT a, (SELECT b FROM t) AS c FROM u) s`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UnreadQueries {
    /// As in DuckDB: the query is run, so a query that gives more than one row is still an error.
    #[default]
    Pin,
    /// As in PostgreSQL: the query is not run. PostgreSQL removes the outputs of a subquery in
    /// `FROM` that nothing reads in `remove_unused_subquery_outputs`, and a scalar query there
    /// goes with its output.
    Postgres,
}

/// How a row written out is compared with another one, as in `ROW(a, b) < ROW(c, d)`, or with a
/// query of several columns, as in `ROW(a, b) = (SELECT c, d FROM t)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RowComparisons {
    /// As in DuckDB: the two rows are two struct values, compared as values, and a query has to
    /// give one column.
    #[default]
    Pin,
    /// As in PostgreSQL: the rows are compared a pair of columns at a time, as
    /// `make_row_comparison_op` in `parse_expr.c` does. `=` is true when each pair is equal, `<>`
    /// when one pair differs, and an ordered comparison is decided by the first pair that is not
    /// equal, so a null there makes it null. The columns of the query are the second row.
    Postgres,
}

/// How a subscript and a slice of a list read the list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Subscripts {
    /// As in DuckDB: a subscript of any integer type, where a negative index counts from the end.
    #[default]
    Pin,
    /// As in PostgreSQL: the subscript is coerced to `integer`, and an index outside the array gives
    /// a null, or for a slice, only the part of the array that is inside the bounds.
    Postgres,
}

/// What `COLLATE` names and how the collation of an expression comes from its inputs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Collations {
    /// As in DuckDB.
    #[default]
    Pin,
    /// As in PostgreSQL: `COLLATE` names a collation of `pg_collation` and is refused on a type
    /// that has no collation, and two different collations written with `COLLATE` cannot meet in
    /// one operator or function. A function that maps the case of text, and `ILIKE`, which matches
    /// the lower case of both sides, map it by the collation of the call.
    Postgres,
}

/// The type names that a column definition can use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TypeNames {
    /// The type names of DuckDB.
    #[default]
    Pin,
    /// The type names of PostgreSQL too, such as `bpchar` and `serial`, with the declared type
    /// that a client sees for each column.
    Postgres,
}

/// The names of the columns of a `VALUES` list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ValuesNames {
    /// `col0`, `col1` and on, as in DuckDB.
    #[default]
    FromZero,
    /// `column1`, `column2` and on, as in PostgreSQL.
    FromOne,
}

/// How `SHOW name` chooses between a setting and a table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShowBehavior {
    /// Prefer a table when one exists, then fall back to a setting.
    #[default]
    Auto,
    /// Always read a setting.
    Setting,
    /// Always describe a table.
    Table,
}

/// How an unstated `NULLS FIRST` or `NULLS LAST` is resolved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DefaultNullOrder {
    /// Nulls precede values in both directions.
    First,
    /// Nulls follow values in both directions, which is DuckDB's default.
    #[default]
    Last,
    /// Nulls are low, as in SQLite and MySQL.
    Sqlite,
    /// Nulls are high, as in PostgreSQL.
    Postgres,
}

/// A parsed session time zone, cheap enough to carry beside a prepared expression.
///
/// A zone of a DuckDB session is a zone of the IANA release of DuckDB's ICU, and a zone of a
/// PostgreSQL session is what the `TimeZone` setting of PostgreSQL names, from the release of the
/// pin of PostgreSQL. Both read the same way, as `localtime.c` of the pin reads a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimeZone(Zone);

impl Default for SessionTimeZone {
    fn default() -> Self {
        Self(Zone::utc(Release::Icu))
    }
}

impl SessionTimeZone {
    /// The zone a name spells in the release of DuckDB's ICU, with case ignored the way the pin's
    /// ICU lookup ignores it, or `None` for a name that the release does not know.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        Zone::file(Release::Icu, name).map(Self)
    }

    /// The zone of a value of the `TimeZone` parameter of PostgreSQL, which a cast between a
    /// `timestamptz` and a type without a zone reads in a PostgreSQL session in place of the zone
    /// of the database.
    ///
    /// A fixed offset, such as `-3` or `INTERVAL '+05:30'`, is the POSIX zone that PostgreSQL
    /// makes of it. `None` for a value that is not a zone, which the check of the parameter does
    /// not let through.
    #[must_use]
    pub fn of_postgres(value: &str) -> Option<Self> {
        crate::guc::zone(value).ok().map(Self)
    }

    /// The zone and its rules.
    #[must_use]
    pub fn zone(self) -> Zone {
        self.0
    }

    /// Whether the zone is UTC at all instants, where every wall clock is its instant and nothing
    /// needs moving.
    #[must_use]
    pub fn is_utc(self) -> bool {
        self.fixed_offset() == Some(0)
    }

    /// The canonical name of the zone: the IANA name, or the POSIX string in upper case.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// The offset in seconds east of UTC of a zone with one offset at all instants, as
    /// `pg_get_timezone_offset` finds it.
    #[must_use]
    pub fn fixed_offset(self) -> Option<i32> {
        self.0.state().fixed_offset()
    }

    /// The abbreviation of the zone at an instant expressed as Unix microseconds, such as `CEST`,
    /// or the offset as the data writes it for a zone that has no abbreviation, such as `-03`.
    #[must_use]
    pub fn abbreviation_at(self, micros: i64) -> &'static str {
        self.0.state().at(micros.div_euclid(1_000_000)).abbrev
    }

    /// The UTC offset in seconds at an instant expressed as Unix microseconds.
    #[must_use]
    pub fn offset_seconds_at(self, micros: i64) -> i32 {
        self.0.state().at(micros.div_euclid(1_000_000)).offset
    }

    /// The UTC offset in seconds right now, which is the offset the pin puts on a time of day that
    /// came without one, since a time of day has no date to look an offset up at.
    #[must_use]
    pub fn offset_seconds_now(self) -> i32 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX));
        self.offset_seconds_at(now)
    }

    /// The wall clock this zone reads at an instant, both as Unix microseconds, or `None` when the
    /// reading is past the end of the `i64`.
    #[must_use]
    pub fn local_of_instant(self, micros: i64) -> Option<i64> {
        micros.checked_add(i64::from(self.offset_seconds_at(micros)) * 1_000_000)
    }

    /// The instant a wall clock reading in this zone names, both as Unix microseconds.
    ///
    /// A reading the clocks skipped over is read with the offset from before the jump, so 02:30 on
    /// the morning New York moves to summer time is 03:30 summer time. A reading the clocks passed
    /// twice is the second of the two. That is `DetermineTimeZoneOffset` of PostgreSQL, and it is
    /// also what an ICU calendar answers with its defaults when its fields are set. `None` when
    /// the instant is past the end of the `i64`.
    #[must_use]
    pub fn instant_of_local(self, micros: i64) -> Option<i64> {
        let offset = self.0.state().local_offset(micros.div_euclid(1_000_000));
        micros.checked_sub(i64::from(offset) * 1_000_000)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self {
            values: Arc::new(BTreeMap::new()),
            time_zone: SessionTimeZone::default(),
            semantics: Semantics::default(),
            rules: Rules::new(),
            links: String::new(),
            seams: String::new(),
            variables: Variables::default(),
            postgres: Postgreses::default(),
            begun: Begun::default(),
            transaction: Transaction::default(),
            prepared: Prepareds::default(),
            local: Arc::new(Vec::new()),
        }
    }
}

impl Session {
    /// A session that knows nothing, which is what a caller with no database behind it has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what one setting is now.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        Arc::make_mut(&mut self.values).insert(name.to_string(), value.into());
    }

    /// Sets the time zone after it has been validated by the setting layer.
    pub fn set_time_zone(&mut self, name: &str) {
        self.time_zone = SessionTimeZone::named(name).unwrap_or_default();
    }

    /// The canonical IANA name of the session time zone.
    #[must_use]
    pub fn time_zone(&self) -> &str {
        self.time_zone.name()
    }

    /// The parsed zone used by prepared expressions.
    #[must_use]
    pub fn session_time_zone(&self) -> SessionTimeZone {
        self.time_zone
    }

    /// Sets the direction used by an order item that names none.
    pub fn set_default_descending(&mut self, descending: bool) {
        self.semantics.default_descending = descending;
    }

    /// Sets how an order item with no null placement is resolved.
    pub fn set_default_null_order(&mut self, order: DefaultNullOrder) {
        self.semantics.default_null_order = order;
    }

    /// Sets whether `/` is bound as the integer division operator.
    pub fn set_integer_division(&mut self, enabled: bool) {
        self.semantics.integer_division = enabled;
    }

    /// Sets whether floating division and remainder use IEEE answers for zero divisors.
    pub fn set_ieee_floating_point_ops(&mut self, enabled: bool) {
        self.semantics.ieee_floating_point_ops = enabled;
    }

    /// Sets how unquoted identifiers are folded while a statement is parsed.
    pub fn set_identifier_case(&mut self, case: IdentifierCase) {
        self.semantics.identifier_case = case;
    }

    /// Sets whether division errors caused by a zero divisor become nulls.
    pub fn set_null_on_division_by_zero(&mut self, enabled: bool) {
        self.semantics.null_on_division_by_zero = enabled;
    }

    /// Sets whether a constant non-integer expression is accepted as a sort key.
    pub fn set_order_by_non_integer_literal(&mut self, enabled: bool) {
        self.semantics.order_by_non_integer_literal = enabled;
    }

    /// Sets the most columns a pivot may make.
    pub fn set_pivot_limit(&mut self, limit: u64) {
        self.semantics.pivot_limit = limit;
    }

    /// Sets whether casts from local timestamps to zoned timestamps are refused.
    pub fn set_disable_timestamptz_casts(&mut self, enabled: bool) {
        self.semantics.disable_timestamptz_casts = enabled;
    }

    /// Sets whether errors are returned as structured JSON.
    pub fn set_errors_as_json(&mut self, enabled: bool) {
        self.semantics.errors_as_json = enabled;
    }

    /// Sets whether regex match operators require the entire string to match.
    pub fn set_regex_match_full(&mut self, enabled: bool) {
        self.semantics.regex_match_full = enabled;
    }

    /// Sets whether a lambda may be written with the deprecated arrow.
    pub fn set_single_arrow_lambdas(&mut self, enabled: bool) {
        self.semantics.single_arrow_lambdas = enabled;
    }

    /// Sets whether scalar queries may choose one row from several.
    pub fn set_scalar_subquery_error_on_multiple_rows(&mut self, enabled: bool) {
        self.semantics.scalar_subquery_error_on_multiple_rows = enabled;
    }

    /// Sets how `SHOW name` resolves its name.
    pub fn set_show_behavior(&mut self, behavior: ShowBehavior) {
        self.semantics.show_behavior = behavior;
    }

    /// Sets whether warnings are promoted to errors.
    pub fn set_warnings_as_errors(&mut self, enabled: bool) {
        self.semantics.warnings_as_errors = enabled;
    }

    /// The meaning-changing choices the binder resolves into the plan.
    #[must_use]
    pub fn semantics(&self) -> Semantics {
        self.semantics
    }

    /// Records which optimization rules the session has turned off.
    pub fn set_rules(&mut self, rules: Rules) {
        self.rules = rules;
    }

    /// Which optimization rules may fire for this statement.
    ///
    /// Read by whatever is about to apply one, which is why it rides on the session rather than
    /// being reached for through the database: the rank that sets it and the ranks that obey it
    /// cannot see each other.
    #[must_use]
    pub fn rules(&self) -> Rules {
        self.rules
    }

    /// Records the relationships `SET graph_links` declared, as they were written.
    pub fn set_links(&mut self, links: impl Into<String>) {
        self.links = links.into();
    }

    /// The relationships declared for this session, as they were written, empty for none.
    ///
    /// The text and not the parsed form, because the parser is in `rudb-graph` and a session is
    /// read by ranks below that one. Whoever needs a relationship is above it and parses this
    /// itself, and the text is what `SET` already validated, so the parse there cannot fail.
    #[must_use]
    pub fn links(&self) -> &str {
        &self.links
    }

    /// Records which seams the session has pinned, as a hint body.
    ///
    /// The text and not the parsed form, for the reason [`Session::links`] carries text: the pins
    /// live in `rudb-seam`, which is above this crate, and a session is read from below it. A hint
    /// body rather than a format of its own because `rudb_seam::Settings` already writes one and
    /// already parses one, so the spelling of a pin cannot come to mean two things.
    pub fn set_seams(&mut self, seams: impl Into<String>) {
        self.seams = seams.into();
    }

    /// The seams this session has pinned, as a hint body, empty for none.
    ///
    /// Empty is not the same as unknown. A session with nothing pinned has every seam at its
    /// default, which is what an unpinned seam reads back as, so a reader of this parses it and asks
    /// it rather than treating empty as an absence of an answer.
    #[must_use]
    pub fn seams(&self) -> &str {
        &self.seams
    }

    /// Records the variables `SET VARIABLE` has left, in the order they were first set.
    pub fn set_variables(&mut self, variables: Vec<Variable>) {
        self.variables = Variables(Arc::new(variables));
    }

    /// The variable of that name, which is matched without regard to case the way the pin matches
    /// it, so `SET VARIABLE A = 1` is read back by `getvariable('a')`.
    #[must_use]
    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.0.iter().find(|held| held.name.eq_ignore_ascii_case(name))
    }

    /// Every variable, in the order they were first set.
    pub fn variables(&self) -> impl Iterator<Item = &Variable> {
        self.variables.0.iter()
    }

    /// Records the PostgreSQL session that runs the statements, or none.
    ///
    /// A PostgreSQL session also takes the rules of PostgreSQL for division: `/` of two integers
    /// is an integer, and a zero divisor is an error for every type. It takes the rule of
    /// PostgreSQL for the values of an `INSERT`, and it compares identifiers byte for byte. Nulls
    /// are high, so an unstated order puts them last when ascending and first when descending. Its
    /// `TimeZone` is the zone of the session, see [`SessionTimeZone::of_postgres`].
    pub fn set_postgres(&mut self, postgres: Option<Arc<Postgres>>) {
        if let Some(postgres) = &postgres
            && let Some(zone) = postgres.settings.get("TimeZone")
            && let Some(zone) = SessionTimeZone::of_postgres(&zone)
        {
            self.time_zone = zone;
        }
        if postgres.is_some() {
            self.semantics.integer_division = true;
            self.semantics.ieee_floating_point_ops = false;
            self.semantics.null_on_division_by_zero = false;
            self.semantics.insert_columns = InsertColumns::Leading;
            self.semantics.identifier_compare = IdentifierCompare::Exact;
            self.semantics.join_columns = JoinColumns::MergedFirst;
            self.semantics.aggregate_types = AggregateTypes::Postgres;
            self.semantics.values_names = ValuesNames::FromOne;
            self.semantics.character_types = CharacterTypes::Postgres;
            self.semantics.column_names = ColumnNames::Postgres;
            self.semantics.number_literals = NumberLiterals::Postgres;
            self.semantics.type_names = TypeNames::Postgres;
            self.semantics.operator_rules = OperatorRules::Postgres;
            self.semantics.unknown_types = UnknownTypes::Postgres;
            self.semantics.error_texts = ErrorTexts::Postgres;
            self.semantics.function_rules = FunctionRules::Postgres;
            self.semantics.plan_errors = PlanErrors::Postgres;
            self.semantics.recursive_union = RecursiveUnion::Postgres;
            self.semantics.cast_input = CastInput::Postgres;
            self.semantics.number_casts = NumberCasts::Postgres;
            self.semantics.cast_output = CastOutput::Postgres;
            self.semantics.condition_types = ConditionTypes::Postgres;
            self.semantics.common_types = CommonTypes::Postgres;
            self.semantics.table_names = TableNames::Postgres;
            self.semantics.distinct_order = DistinctOrder::Postgres;
            self.semantics.count_types = CountTypes::Postgres;
            self.semantics.query_columns = QueryColumns::Postgres;
            self.semantics.conflict_arbiter = ConflictArbiter::Postgres;
            self.semantics.explain_output = ExplainOutput::Postgres;
            self.semantics.maintenance = Maintenance::Postgres;
            self.semantics.from_functions = FromFunctions::Postgres;
            self.semantics.float_range = FloatRange::Postgres;
            self.semantics.row_fields = RowFields::Postgres;
            self.semantics.row_nulls = RowNulls::Postgres;
            self.semantics.sequence_owners = SequenceOwners::Column;
            self.semantics.set_functions = SetFunctions::Postgres;
            self.semantics.sort_operators = SortOperators::Postgres;
            self.semantics.tie_order = TieOrder::Postgres;
            self.semantics.window_order = WindowOrder::Postgres;
            self.semantics.empty_targets = EmptyTargets::Postgres;
            self.semantics.unread_queries = UnreadQueries::Postgres;
            self.semantics.row_comparisons = RowComparisons::Postgres;
            self.semantics.subscripts = Subscripts::Postgres;
            self.semantics.collations = Collations::Postgres;
            self.semantics.regex_rules = RegexRules::Postgres;
            self.semantics.default_null_order = DefaultNullOrder::Postgres;
        }
        self.postgres = Postgreses(postgres);
    }

    /// Records when the open transaction began, in microseconds since the epoch, or none.
    pub fn set_begun(&mut self, begun: Option<i64>) {
        self.begun.transaction = begun;
    }

    /// Records when the client sent the statement, in microseconds since the epoch, or none.
    pub fn set_statement_start(&mut self, start: Option<i64>) {
        self.begun.statement = start;
    }

    /// When the client sent the statement, which `statement_timestamp()` gives. A server records
    /// it when it reads the message, so all the statements of one simple query have the same one,
    /// as in PostgreSQL.
    #[must_use]
    pub fn statement_start(&self) -> Option<i64> {
        self.begun.statement
    }

    /// When the open transaction began. `now()` and `current_timestamp` give this instant in each
    /// statement of the transaction, as in PostgreSQL and DuckDB. Outside a transaction each
    /// statement is one, and they give the instant the statement started.
    #[must_use]
    pub fn begun(&self) -> Option<i64> {
        self.begun.transaction
    }

    /// The PostgreSQL session that runs the statements. `current_setting()`, `version()` and the
    /// user functions read it when it is there.
    #[must_use]
    pub fn postgres(&self) -> Option<&Postgres> {
        self.postgres.0.as_deref()
    }

    /// Records the number of the transaction the statement runs in.
    pub fn set_transaction(&mut self, number: u64) {
        self.transaction = Transaction(number);
    }

    /// Records the statements the connection holds by name.
    pub fn set_prepared(&mut self, statements: Arc<[PreparedStatement]>) {
        self.prepared = Prepareds(statements);
    }

    /// The statements the connection holds by name, which `duckdb_prepared_statements()` lists.
    #[must_use]
    pub fn prepared(&self) -> &[PreparedStatement] {
        &self.prepared.0
    }

    /// The number of the transaction the statement runs in, which is the same for every statement
    /// of a block and different for every transaction, and zero for a session no database made.
    #[must_use]
    pub fn transaction(&self) -> u64 {
        self.transaction.0
    }

    /// Whether this name is the one the relationship declarations are written under.
    ///
    /// Both spellings, for the reason [`crate::clustering::is_clustering_setting`] takes both. The
    /// caller decides first that no DuckDB setting is called this.
    #[must_use]
    pub fn is_links_setting(name: &str) -> bool {
        name.eq_ignore_ascii_case("graph_links") || name.eq_ignore_ascii_case("graph.links")
    }

    /// Whether the bundled time-zone database knows this name.
    #[must_use]
    pub fn knows_time_zone(name: &str) -> bool {
        SessionTimeZone::named(name).is_some()
    }

    /// The UTC offset in seconds at an instant expressed as Unix microseconds.
    #[must_use]
    pub fn offset_seconds_at(&self, micros: i64) -> i32 {
        self.session_time_zone().offset_seconds_at(micros)
    }

    /// A UTC instant shifted to the wall clock of this session.
    #[must_use]
    pub fn local_micros(&self, micros: i64) -> i64 {
        micros.saturating_add(i64::from(self.offset_seconds_at(micros)) * 1_000_000)
    }

    /// What that setting is now, and `None` for a name this session has no answer for.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Records the settings this connection set for itself, under every spelling.
    pub fn set_local(&mut self, names: Vec<String>) {
        self.local = Arc::new(names);
    }

    /// Whether this connection set the setting with this name for itself.
    #[must_use]
    pub fn is_local(&self, name: &str) -> bool {
        self.local.iter().any(|held| held == name)
    }

    /// Every setting and its value, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Session, SessionTimeZone};

    #[test]
    fn a_time_zone_of_postgresql_is_a_named_zone_or_a_fixed_offset() {
        let zone = SessionTimeZone::of_postgres("America/New_York").unwrap();
        assert_eq!(zone.name(), "America/New_York");
        // 2024-07-01 12:00 UTC, in summer time.
        let noon = 1_719_835_200_000_000;
        assert_eq!(zone.offset_seconds_at(noon), -4 * 3600);
        let zone = SessionTimeZone::of_postgres("interval '+05:30'").unwrap();
        assert_eq!(zone.name(), "<+05:30>-05:30");
        assert_eq!(zone.offset_seconds_at(noon), 19_800);
        assert_eq!(zone.local_of_instant(noon), Some(noon + 19_800_000_000));
        assert_eq!(zone.instant_of_local(noon + 19_800_000_000), Some(noon));
        assert!(!zone.is_utc());
        let zone = SessionTimeZone::of_postgres("-3").unwrap();
        assert_eq!(zone.offset_seconds_at(noon), -3 * 3600);
        assert_eq!(zone.abbreviation_at(noon), "-03");
        assert_eq!(zone.fixed_offset(), Some(-3 * 3600));
        let zone = SessionTimeZone::of_postgres("XYZ+3").unwrap();
        assert_eq!(zone.abbreviation_at(noon), "XYZ");
        let paris = SessionTimeZone::of_postgres("Europe/Paris").unwrap();
        assert_eq!(paris.abbreviation_at(noon), "CEST");
        assert_eq!(paris.abbreviation_at(noon - 183 * 86_400_000_000), "CET");
        assert_eq!(paris.fixed_offset(), None);
        let sao_paulo = SessionTimeZone::of_postgres("America/Sao_Paulo").unwrap();
        assert_eq!(sao_paulo.abbreviation_at(noon), "-03");
        assert!(SessionTimeZone::of_postgres("0").unwrap().is_utc());
        assert_eq!(SessionTimeZone::of_postgres("Mars/Olympus"), None);
    }

    #[test]
    fn a_session_hands_back_what_was_put_in_and_says_nothing_about_a_name_it_has_not_got() {
        let mut session = Session::new();
        assert!(session.is_empty());
        session.set("threads", "8");
        session.set("memory_limit", "1.0 GiB");
        assert_eq!(session.get("threads"), Some("8"));
        assert_eq!(session.get("nothing_called_this"), None);
        // Name order, because the one reader of this is a catalog table that comes out sorted and
        // sorting it twice would be sorting it once too many.
        let pairs: Vec<(&str, &str)> = session.iter().collect();
        assert_eq!(pairs, [("memory_limit", "1.0 GiB"), ("threads", "8")]);
    }
}
