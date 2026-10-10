//! From an `Ast` to a `Plan`.
//!
//! The binder walks the written query once, in the order the operators end up in rather than the
//! order the clauses are written in, which is `FROM`, `WHERE`, `GROUP BY`, `HAVING`, `SELECT`,
//! `DISTINCT`, `ORDER BY`, `LIMIT`. That order is not a stylistic choice: it is the reason `WHERE`
//! cannot see an output alias and `HAVING` cannot see a column that was not grouped, and doing it
//! in any other order means special casing both of those instead of getting them for free.
//!
//! Two things leave here settled that nothing downstream reconsiders. Every column is a table index
//! and a position rather than a name, so the optimizer never has to ask which `id` a name meant.
//! And every expression has a type, with the casts that make the types line up already written into
//! the plan as [`Expr::Cast`] nodes, so an executor never has to decide what a comparison between
//! an `INTEGER` and a `BIGINT` does.

use std::sync::Arc;

use rudb_catalog::{Catalog, DETACHED, Entry, FileStamp, QualifiedName, same_name};
use rudb_common::bounds::Zones;
use rudb_common::{
    AggregateTypes, Collations, CommonTypes, ConditionTypes, DeclaredType, DistinctOrder,
    EmptyTargets, Error, ErrorTexts, Field, FunctionRules, JoinColumns, LogicalType, Origin,
    RecursiveUnion, Result, Semantics, Session, ShowBehavior, SortOperators, Span, SqlState, Stat,
    StateKey, TableNames, UnknownTypes, Value, ValuesNames, WindowOrder,
};
use rudb_functions::{
    Columns, FILE_ROW_NUMBER, Footers, FunctionKind, Given, Resolved, TYPES_SET, TableFunction,
    content_files, csv_fields, csv_given, files, is_file, is_pattern, json_text, kind_of,
    parquet_footers, parquet_outline, resolve, resolve_pragma, resolve_table,
};
use rudb_kernels::json::scan;
use rudb_kernels::{percentage, row_count};
use rudb_parse::ast::{self, Ast, Distinct, LiteralKind, Nulls, Order, Quantifier, SetOp};
use rudb_parse::{NONE, identifier_parts, parse_ast_with_case};
use rudb_plan::{
    Bound, BuildSide, ColumnBinding, ConjunctionOp, Expr, ExprRef, JoinKind, Node, NodeRef, Plan,
    SetOpKind, Share, SortKey, WindowBound, WindowExclude, WindowFrame, WindowUnit,
};

use crate::expr::{describe, postgres_oid, written_oid};
use crate::fold;
use crate::ordinality::unnumbered;
use crate::overcall;
use crate::parameters::{Parameters, Written};
use crate::scope::{Joined, Scope, Visible};

/// The PostgreSQL type `name`, which `current_user` and the other session names have.
const NAME: DeclaredType = DeclaredType { oid: rudb_pgtypes::oid::NAME, typmod: -1 };
const VOID: DeclaredType = DeclaredType { oid: rudb_pgtypes::oid::VOID, typmod: -1 };
/// The name a table's row number answers to.
pub(crate) const ROWID: &str = "rowid";

/// Binds a parsed statement against a catalog.
///
/// # Errors
///
/// If the script does not hold exactly one statement, if a name does not resolve, if a type does
/// not work out, or if the query uses something M0 does not bind yet.
pub fn bind(ast: &Ast, catalog: &Catalog) -> Result<Plan> {
    bind_with(ast, catalog, &Parameters::new(), &Session::new())
}

/// Binds a parsed query against a catalog, with values for its parameters and its settings.
///
/// The session is what `current_setting()` reads, and a caller with no database behind it passes an
/// empty one, which makes every setting name unrecognized rather than making up an answer.
///
/// # Errors
///
/// Everything [`bind`] reports, plus an error for a parameter that was given no value.
pub fn bind_with(
    ast: &Ast,
    catalog: &Catalog,
    parameters: &Parameters,
    session: &Session,
) -> Result<Plan> {
    let query = match ast.statements.as_slice() {
        [ast::Statement::Query(query)] => *query,
        [] => return Err(Error::binder("no statement to bind")),
        // One statement that is not a query is its own answer. Reporting it as a script of several
        // reads as a count being wrong, and the count is right.
        [_] => return Err(Error::not_implemented("a statement that is not a query")),
        _ => return Err(Error::not_implemented("a script of more than one statement")),
    };
    let mut binder = Binder::with(catalog, parameters, session);
    let (root, _) = binder.bind_query(ast, query)?;
    let mut plan = binder.into_plan();
    plan.set_root(root);
    plan.validate()?;
    Ok(plan)
}

/// Parses and binds one query, which is the whole front end in one call.
///
/// # Errors
///
/// Anything the parser or the binder reports.
pub fn bind_sql(query: &str, catalog: &Catalog) -> Result<Plan> {
    bind_sql_with(query, catalog, &Session::new())
}

/// Parses and binds one query, with the settings a call to `current_setting()` reads.
///
/// # Errors
///
/// Anything the parser or the binder reports.
pub fn bind_sql_with(query: &str, catalog: &Catalog, session: &Session) -> Result<Plan> {
    let ast = parse_ast_with_case(query, session.semantics().identifier_case())?;
    bind_with(&ast, catalog, &Parameters::new(), session)
}

/// What an aggregating select block has decided so far.
#[derive(Debug)]
pub(crate) struct Aggregation {
    /// The table index the aggregate's output binds against.
    pub(crate) index: u32,
    /// The group expressions, over the input, which are the first output columns.
    pub(crate) groups: Vec<ExprRef>,
    /// The aggregate calls found so far, which follow the groups in the output.
    pub(crate) aggregates: Vec<ExprRef>,
}

/// The clause a select block's aliases are being read from, which decides how they are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AliasClause {
    /// One that may not read them, such as a join condition or the `GROUP BY`, which follows the
    /// aliases its own way.
    None,
    /// The select list, where an alias is read once the target it names has been bound.
    Select,
    /// `WHERE`, where a column of the `FROM` wins over an alias of the same name.
    Where,
    /// `HAVING`, where an alias wins over a column that is not grouped.
    Having,
    /// `QUALIFY`, which reads them as `WHERE` does.
    Qualify,
}

/// The aliases of a select block's targets, which a clause of the block may name in place of the
/// expression they were written for.
///
/// The pin binds the expression again where the alias is named rather than reading the target's
/// column, so `SELECT x + 1 AS y FROM t WHERE y > 1` filters on `x + 1`, and that is what happens
/// here. An alias is not followed while it is being bound, so `SELECT y + 1 AS y FROM t WHERE y >
/// 1` over a table without a `y` is the column missing and not a loop.
#[derive(Debug, Clone)]
pub(crate) struct Aliases {
    /// Each alias, the expression it was written for and the position of its target.
    entries: Vec<(String, ast::ExprRef, usize)>,
    /// The clause being bound.
    pub(crate) clause: AliasClause,
    /// How many targets of the select list are bound, while the select list is.
    pub(crate) defined: usize,
    /// The aliases whose expressions are being bound.
    visiting: Vec<usize>,
}

impl Aliases {
    /// The aliases a select block writes, of which the last of a name is the one that counts.
    fn of(ast: &Ast, select: &ast::Select) -> Self {
        let entries = ast
            .target_list(select.targets)
            .iter()
            .enumerate()
            .filter(|(_, target)| {
                target.alias != NONE && !crate::columns::has_star(ast, target.expr)
            })
            .map(|(at, target)| (ast.string(target.alias).to_string(), target.expr, at))
            .collect();
        Self { entries, clause: AliasClause::None, defined: 0, visiting: Vec::new() }
    }

    /// The entry for `name` that the clause being bound may follow.
    pub(crate) fn find(&self, name: &str) -> Option<(usize, ast::ExprRef, usize)> {
        if self.clause == AliasClause::None {
            return None;
        }
        let at = self.entries.iter().rposition(|(alias, ..)| same_name(alias, name))?;
        if self.visiting.contains(&at) {
            return None;
        }
        let (_, expr, target) = self.entries[at];
        Some((at, expr, target))
    }
}

/// One run of window calls that agree on where the rows come from and in what order.
///
/// The run is the unit the plan has an operator for, so two calls that write the same partition,
/// the same order and the same frame are one operator and one sort, and a third that writes a
/// different order is a second operator stacked on the first. Nothing here merges runs that only
/// look compatible, because a window is evaluated over the rows the operator below it produced and
/// deciding two runs are the same is the optimizer's job rather than the binder's.
#[derive(Debug)]
pub(crate) struct WindowRun {
    /// The table index the run's result columns bind against.
    index: u32,
    /// What divides the input into independent partitions.
    pub(crate) partition: Vec<ExprRef>,
    /// The order within a partition.
    pub(crate) order: Vec<SortKey>,
    /// The frame every call in the run shares.
    frame: WindowFrame,
    /// The calls, in the order their columns are appended.
    calls: Vec<ExprRef>,
    /// The window the run was written as, a window of the `WINDOW` clause when one of its calls
    /// named one. See [`crate::windoworder`].
    pub(crate) spec: ast::WindowRef,
    /// Whether `spec` is a window of the `WINDOW` clause.
    pub(crate) named: bool,
}

/// One aggregate call as it was written, before any of it has been bound.
#[derive(Clone, Copy)]
pub(crate) struct AggregateCall<'a> {
    /// The function name, as written and not yet resolved.
    name: &'a str,
    /// The arguments.
    args: &'a [ast::ExprRef],
    /// Whether `DISTINCT` was written inside the parens.
    distinct: bool,
    /// The `FILTER (WHERE ...)` predicate, or `NONE`.
    filter: ast::ExprRef,
    /// The `ORDER BY` written inside the parens.
    sorted: &'a [ast::OrderItem],
}

/// One window call as it was written, before any of it has been bound.
///
/// These six travel together from the parser all the way to the run they end up filed under, and
/// carrying them as one thing keeps the call that binds them readable.
pub(crate) struct WindowCall<'a> {
    /// The call, which errors are placed at.
    pub(crate) call: ast::ExprRef,
    /// The function name, as written and not yet resolved.
    pub(crate) name: &'a str,
    /// The arguments, which may include a star that only `count` is allowed to be given.
    pub(crate) args: &'a [ast::ExprRef],
    /// Whether `DISTINCT` was written inside the parens.
    pub(crate) distinct: bool,
    /// The `FILTER (WHERE ...)` predicate, which is written before the `OVER`, or `NONE`.
    pub(crate) filter: ast::ExprRef,
    /// Whether `IGNORE NULLS` was written inside the parens, which is where DuckDB puts it.
    pub(crate) ignore_nulls: bool,
    /// The `ORDER BY` written inside the parens, which says what order the call reads the rows of
    /// its frame in and is a different clause from the one in the `OVER`.
    pub(crate) order: ast::Slice,
    /// The `OVER`, which the parser has already resolved against any `WINDOW` clause.
    pub(crate) spec: ast::WindowRef,
}

/// Everything inside one window call once it is bound, which is what decides its run.
struct WindowParts {
    /// The arguments, before the casts the resolved signature asks for.
    args: Vec<ExprRef>,
    /// What divides the input into independent partitions.
    partition: Vec<ExprRef>,
    /// The order within a partition.
    order: Vec<SortKey>,
    /// The order the call reads the rows of its frame in, which is the `ORDER BY` written inside
    /// the brackets rather than the one in the `OVER` and is empty far more often than not.
    inner: Vec<SortKey>,
    /// The frame, with both ends and the exclusion.
    frame: WindowFrame,
}

/// What opening the files behind a table function call said about them.
///
/// The answers travel together because they come out of the same footer. A Parquet file states its
/// columns, its row count and its statistics in the same few kilobytes at the end of it, so a
/// binder that has read one has read all of them, and splitting them into four arguments would
/// mean four ways to forget one.
#[derive(Debug)]
struct Read {
    /// The columns the call produces, in the order the file stores them.
    fields: Vec<Field>,
    /// How many rows all of the files hold, where anybody counted.
    rows: Stat<u64>,
    /// How many distinct values a column holds, by name, for the columns anybody counted.
    distincts: Vec<(String, Stat<u64>)>,
    /// The bounds the files keep per part of themselves, where anything can answer for them.
    zones: Option<Arc<dyn Zones>>,
}

impl Read {
    /// Columns that came from somewhere other than a file, so nothing counted anything.
    fn uncounted(fields: Vec<Field>) -> Self {
        Self { fields, rows: Stat::Unknown, distincts: Vec::new(), zones: None }
    }
}

/// A materialised `WITH` definition that has been bound and can be read by name.
#[derive(Debug)]
struct Materialized {
    /// Which written definition this is, as an index into `Ast::ctes`.
    written: u32,
    /// The number the plan uses to pair a read with what it reads.
    cte: u32,
    /// The name it was written with, which is the table name a read is reachable through.
    name: String,
    /// What it produces, in order, under the declared names when a column list was written.
    fields: Vec<Field>,
    /// The number a `recurring.` read of it is paired with, which is `cte` for a definition that
    /// does not read itself and so has no such read.
    recurring: u32,
    /// What a `recurring.` read of it sees, which is `fields` except while the recursive side of
    /// a definition with `USING KEY` aggregates is bound, when the rows a round reads are not yet
    /// the rows of the table.
    finished: Vec<Field>,
    /// The table index of the columns of the definition, which a read takes its collations from.
    /// For a recursive definition it is the query that starts it.
    output: Option<u32>,
}

/// An aggregate `USING KEY` names, bound against one side of a recursive definition.
struct Fold {
    /// The column its answer lands in.
    into: u32,
    /// The call as written, which is bound again for the other side.
    call: ast::ExprRef,
    /// The aggregate it runs, as the executor knows it.
    name: String,
    /// What it answers.
    ty: LogicalType,
    /// Its arguments, over the side it was bound against.
    args: Vec<ExprRef>,
}

#[derive(Debug)]
pub(crate) struct PendingSubquery {
    pub(crate) node: NodeRef,
    pub(crate) kind: JoinKind,
    pub(crate) conditions: Vec<ExprRef>,
    pub(crate) dependent: bool,
    /// The outer columns the query's body read, which is what `dependent` counts.
    ///
    /// Kept rather than reduced to the flag because a join's `ON` has to decide which of its two
    /// inputs the query is joined into, and the answer is the side those columns come from. A
    /// query that reads neither side can go on either.
    pub(crate) reads: Vec<ColumnBinding>,
    /// The table index this query's join adds to the rows it is joined into.
    ///
    /// Kept so that a `HAVING` which reads one of these can say which columns came from a query
    /// joined above the grouping rather than from the table underneath it. Those columns are not
    /// the table's and the grouping rule has nothing to say about them.
    pub(crate) index: u32,
    /// Whether the query was written inside an aggregate call's argument or its `FILTER`.
    ///
    /// One written there is read once per row going into the aggregate, so it has to be joined in
    /// underneath the grouping however uncorrelated it is. Every other query a grouped block writes
    /// is one row for the whole block and is lifted over the grouping instead, which is what
    /// [`Binder::lift_over_aggregate`] decides.
    pub(crate) inside_aggregate: bool,
}

/// Which input of a join a query written in that join's `ON` is joined into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// The expressions of a select list, the name of each and the table column of each.
type Targets = (Vec<ExprRef>, Vec<String>, Vec<Option<Origin>>);

/// The state one binding run carries.
#[derive(Debug)]
pub(crate) struct Binder<'a> {
    catalog: &'a Catalog,
    /// What the parameters were given, empty for a statement that is not prepared.
    pub(crate) parameters: &'a Parameters,
    /// What the settings are now, which is what `current_setting()` folds to.
    pub(crate) session: &'a Session,
    /// Meaning-changing choices copied once and resolved into the plan above execution.
    pub(crate) semantics: Semantics,
    plan: Plan,
    next_index: u32,
    /// Source range inherited by plan objects built for the current AST expression or query.
    pub(crate) current_span: Span,
    /// The span every expression is placed at while a built-in macro's body is bound, which is the
    /// span of the call. See `crate::macros`.
    pub(crate) pinned_span: Option<Span>,
    /// The collations the statement wrote with `COLLATE`. See `crate::collate`.
    pub(crate) collated: crate::collate::Collated,
    /// The collations of DuckDB the statement met.
    pub(crate) pin_collated: crate::collation::PinCollated,
    /// The arguments of a function in SQL of `pg_proc` while its body is bound in place of the
    /// call, which `$1` and the others name there. See `crate::pgcalls`.
    pub(crate) inlined: Option<Vec<ExprRef>>,
    /// Set while a select block aggregates, which changes what a bare column means.
    pub(crate) aggregation: Option<Aggregation>,
    /// A grouped block may need stored column order to close groups while it scans. Other queries
    /// leave the summaries in the file instead of reading every column's section while binding.
    want_ascending: bool,
    /// Whether this binds the query of an `ON CONFLICT DO UPDATE`, whose `excluded` reads the new
    /// rows rather than the table.
    pub(crate) upsert: bool,
    /// Whether the next table bound has no `rowid`, which is the table an `INSERT` or an `UPDATE`
    /// returns its rows from. Only that one, so a query in its `RETURNING` reads its own.
    pub(crate) unnumbered: bool,
    /// Whether the statement last asked about writes `count(t.*)`, kept against where its syntax
    /// tree is and how many expressions it holds so that every source of it asks only once.
    pub(crate) counted: Option<((usize, usize), bool)>,
    /// The type and the default of each column an `INSERT` writes, handed to the `VALUES` right
    /// under it so that a `DEFAULT` item there can be the default of the column it lands in.
    pub(crate) insert_defaults: Option<Vec<(LogicalType, Option<String>)>>,
    /// The PostgreSQL type of each column an `INSERT` writes, in a PostgreSQL session, so that a
    /// string literal in the `VALUES` right under it is read by the input function of that type.
    pub(crate) insert_inputs: Option<Vec<u32>>,
    /// The columns a `COPY t FROM` loads, in the order the file holds them, handed to the
    /// `read_csv` the statement was rewritten into so that the file is read as the table's types
    /// under the table's names rather than as whatever the sniffer guessed.
    pub(crate) copy_into: Option<Vec<Field>>,
    /// Whether a `DEFAULT` binds as a null that the statement replaces afterwards, which is what an
    /// `UPDATE` does with `SET c = DEFAULT`.
    pub(crate) default_as_null: bool,
    /// Whether the select list of the next block keeps a parameter of no type as it is, so that
    /// the `INSERT` or the `UPDATE` above it gives the parameter the type of its column. In other
    /// blocks PostgreSQL makes such a column a `text`.
    pub(crate) unknowns_kept: bool,
    /// Set while an aggregate's own arguments are being bound, so nesting is caught.
    pub(crate) in_aggregate: bool,
    /// Whether an aggregate of `USING KEY` is being bound, where one inside another is refused in
    /// other words.
    pub(crate) folding: bool,
    /// Set while an aggregate's `FILTER` is being bound, which is refused its own aggregate.
    pub(crate) in_filter: bool,
    /// The window runs this select block has collected, in the order they were first written.
    pub(crate) windows: Vec<WindowRun>,
    /// The select block's own aliases while a clause that may read them is bound.
    pub(crate) aliases: Option<Aliases>,
    /// The `unnest` calls this select block has written, in the order they were written.
    pub(crate) unnests: Vec<crate::unnest::UnnestCall>,
    /// The table index the block's `unnest` calls produce their columns under, once there is one.
    pub(crate) unnest_index: Option<u32>,
    /// Whether an `unnest` may be written where the binder is, which is the select list and the
    /// `ORDER BY` of a select block.
    pub(crate) unnest_here: bool,
    /// Set while an `unnest` call's own argument is being bound, so nesting is caught.
    pub(crate) in_unnest: bool,
    /// Set while a select target that is an `unnest` call and nothing more is being bound, which is
    /// the one place an `unnest` of a struct may be written.
    pub(crate) unnest_root: bool,
    /// The struct such a target left to be taken apart into columns.
    pub(crate) unnest_struct: Option<crate::unnest::UnnestStruct>,
    /// Set while the block's `GROUP BY` is being bound, where an `unnest` runs under the grouping.
    /// It is `Some(true)` for `GROUP BY ALL`, which is not allowed to group on one.
    pub(crate) unnest_grouping: Option<bool>,
    /// The `unnest` calls the block's `GROUP BY` wrote, so the same call in the select list reads
    /// the grouped column rather than taking the list apart a second time.
    pub(crate) grouped_unnests: Vec<crate::unnest::GroupedUnnest>,
    /// The sequences a `nextval`, `currval` or `setval` named, which a table's default depends on.
    pub(crate) sequences: Vec<QualifiedName>,
    /// Set while a window call's own arguments and keys are being bound, so nesting is caught.
    pub(crate) in_window: bool,
    /// Uncorrelated scalar queries waiting to be joined into the select block that uses them.
    pub(crate) scalar_subqueries: Vec<PendingSubquery>,
    /// Table indices of the queries this block will join in above its grouping, not below it.
    ///
    /// Only ever set while a `HAVING` is being rewritten over the aggregate. A column from one of
    /// these is not a column of the grouped table, so the rule about grouping every column does not
    /// reach it, and the join that produces it goes on top of the `Aggregate` rather than under it.
    pub(crate) joined_above: Vec<u32>,
    pub(crate) outer_scopes: Vec<Scope>,
    /// The nulls that stand for parameters of no known type while a statement is described, each
    /// with the parameter's name, so a cast of one gives the parameter its type.
    pub(crate) placeholders: Vec<(ExprRef, String)>,
    /// Which of the outer scopes are a FROM entry's left neighbours rather than an enclosing query.
    ///
    /// The two are resolved the same way and refused differently. An aggregate may read a column of
    /// the query it is written in and may not read one a LATERAL brought in from the left, so the
    /// check needs to know which scope the name came out of. Each entry is a position in
    /// `outer_scopes`.
    pub(crate) lateral_scopes: Vec<usize>,
    pub(crate) correlations: Vec<Vec<ColumnBinding>>,
    /// The column a `COLUMNS` stands for while the expression it is in is bound for that column.
    pub(crate) star_entry: Option<crate::columns::Picked>,
    /// The FROM clause a `COLUMNS` argument is picking out of while it is bound, which is what a
    /// `*` inside it stands for. See `crate::columns`.
    pub(crate) columns_scope: Option<Scope>,
    /// The column name a star stands for while a pattern applied to it is tried on that name, as
    /// in `* LIKE 'a%'`.
    pub(crate) star_name: Option<String>,
    /// The lambdas whose bodies are being bound, innermost last. See `crate::lambda`.
    pub(crate) lambda_frames: Vec<crate::lambda::Frame>,
    /// Whether the expression being bound is inside a `TRY`, which refuses what it cannot rerun.
    pub(crate) trying: bool,
    /// Whether the call being bound wrote `EXPORT_STATE` after it, which an aggregate takes to
    /// mean it answers with its state rather than with its result.
    pub(crate) exporting: bool,
    /// Where we are, for an error message that says which clause the writer should look at.
    pub(crate) clause: &'static str,
    /// Whether a name that is no column is read as the string it spells, which is what the pin
    /// does in the arguments of most table functions. See [`TableFunction::takes_identifiers`].
    pub(crate) identifiers_as_strings: bool,
    /// Whether a Parquet file that could be read through a native mirror is bound from its outline
    /// alone, which is the columns and the row count and none of the row groups.
    ///
    /// Set by a bind whose plan is thrown away: a `CREATE VIEW`, and the first bind of a query that
    /// may be bound again once its mirrors are in. A plan bound this way knows no bounds and no
    /// distinct counts for the file, so the caller must not run it, and every read it did this for
    /// asked for a mirror, which is how the caller knows to bind again. See
    /// [`rudb_parquet::Outline`].
    pub(crate) outlined: bool,
    /// The views whose bodies are open on the stack, which is what catches a cycle.
    expanding: Vec<String>,
    /// The materialised `WITH` definitions whose bodies are being bound, innermost last.
    ///
    /// A stack rather than a map from what was written, because a plain `WITH` is put into every
    /// place it is named, so a materialised one written inside a plain one is bound once per use
    /// and each of those is a materialisation of its own with a number of its own.
    materialized: Vec<Materialized>,
    /// The recursive definitions whose recursive side is being bound, as indexes into
    /// `Ast::ctes`, innermost last.
    recursing: Vec<u32>,
    /// The block of the right side of a recursive definition with a `SEARCH` or `CYCLE` clause,
    /// while that side is being bound.
    pub(crate) passing: Option<crate::searchcycle::Passing>,
    /// How many materialisations have been numbered, which is where the next number comes from.
    next_cte: u32,
    /// When this statement started, read once and kept, which is what `now()` folds to.
    started: Option<i64>,
    /// The text of the statement, kept from the first tree bound so that a view or a macro body,
    /// which is parsed from text of its own, still answers `current_query()` with the statement.
    source: Option<Arc<str>>,
    /// The join sides being held so they run once, as their definitions, innermost last.
    ///
    /// See [`Binder::bind_held_side`]. Each is wrapped around the join that held it once the join is
    /// bound.
    held: Vec<HeldSide>,
    /// The written sources that are held, with what a read of one is, so binding the same source
    /// again reads what was held rather than running the source a second time.
    held_sources: Vec<(ast::SourceRef, HeldSide, Scope)>,
    /// The sources of the pivots being bound, innermost last, each under the name the query written
    /// for its pivot reads it by. See `crate::pivot`.
    pub(crate) pivot_sources: Vec<(String, NodeRef, Scope)>,
}

/// One side of a join that is materialised so it runs once.
#[derive(Debug, Clone, Copy)]
struct HeldSide {
    /// The side, projected onto the columns its scope has.
    definition: NodeRef,
    /// The number its reads are paired with.
    cte: u32,
    /// The name the printer shows.
    name: rudb_plan::StrRef,
    /// The held columns, into the field pool.
    columns: rudb_plan::Slice,
}

impl<'a> Binder<'a> {
    pub(crate) fn with(
        catalog: &'a Catalog,
        parameters: &'a Parameters,
        session: &'a Session,
    ) -> Self {
        Self {
            catalog,
            parameters,
            session,
            semantics: session.semantics(),
            plan: Plan::new(),
            next_index: 0,
            current_span: Span::new(0, 0),
            pinned_span: None,
            collated: crate::collate::Collated::default(),
            pin_collated: crate::collation::PinCollated::for_session(session),
            inlined: None,
            aggregation: None,
            want_ascending: false,
            upsert: false,
            unnumbered: false,
            counted: None,
            insert_defaults: None,
            insert_inputs: None,
            copy_into: None,
            default_as_null: false,
            unknowns_kept: false,
            in_aggregate: false,
            folding: false,
            in_filter: false,
            windows: Vec::new(),
            aliases: None,
            unnests: Vec::new(),
            unnest_index: None,
            unnest_here: false,
            in_unnest: false,
            unnest_root: false,
            unnest_struct: None,
            unnest_grouping: None,
            grouped_unnests: Vec::new(),
            sequences: Vec::new(),
            in_window: false,
            scalar_subqueries: Vec::new(),
            joined_above: Vec::new(),
            outer_scopes: Vec::new(),
            placeholders: Vec::new(),
            lateral_scopes: Vec::new(),
            correlations: Vec::new(),
            star_entry: None,
            columns_scope: None,
            star_name: None,
            lambda_frames: Vec::new(),
            trying: false,
            exporting: false,
            clause: "SELECT clause",
            identifiers_as_strings: false,
            outlined: false,
            expanding: Vec::new(),
            materialized: Vec::new(),
            recursing: Vec::new(),
            passing: None,
            next_cte: 0,
            started: None,
            source: None,
            held: Vec::new(),
            held_sources: Vec::new(),
            pivot_sources: Vec::new(),
        }
    }

    pub(crate) fn catalog(&self) -> &Catalog {
        self.catalog
    }

    /// When this statement started, in microseconds since the epoch.
    ///
    /// Read from the clock the first time something asks and kept after that, so a query that
    /// writes `now()` twice gets one answer for both. That is what the pin does and what it reports
    /// in the `stability` column of `duckdb_functions()`, where every one of these is
    /// `CONSISTENT_WITHIN_QUERY`. A query that never asks never reads the clock.
    pub(crate) fn instant(&mut self) -> i64 {
        let begun = self.session.begun().or(self.session.statement_start());
        *self.started.get_or_insert_with(|| begun.unwrap_or_else(crate::context::micros_now))
    }

    /// Notes, for a statement that is being described, that it calls a function whose answer is
    /// settled once per transaction. See [`crate::Described::per_transaction`].
    pub(crate) fn read_per_transaction(&self) {
        if let Some(placeholders) = self.parameters.placeholders() {
            placeholders.read_per_transaction();
        }
    }

    /// The text of the statement being bound, which is what `current_query()` folds to.
    pub(crate) fn statement_text(&mut self, ast: &Ast) -> Arc<str> {
        Arc::clone(self.source.get_or_insert_with(|| Arc::clone(&ast.source)))
    }

    /// Keeps the text of `ast` as the statement's, unless a tree was kept before it.
    ///
    /// Called with the tree a view or a macro is bound from, before its body is parsed, so that
    /// the tree of the body, which has text of its own, is not the first one to be asked.
    pub(crate) fn keep_statement(&mut self, ast: &Ast) {
        self.source.get_or_insert_with(|| Arc::clone(&ast.source));
    }

    pub(crate) fn plan(&self) -> &Plan {
        &self.plan
    }

    pub(crate) fn plan_mut(&mut self) -> &mut Plan {
        &mut self.plan
    }

    pub(crate) fn add_expr(&mut self, expr: Expr, ty: LogicalType) -> ExprRef {
        self.plan.add_expr_at(expr, ty, self.current_span)
    }

    pub(crate) fn add_constant(&mut self, value: Value) -> ExprRef {
        let ty = value.logical_type();
        let reference = self.plan.add_value(value);
        self.plan.add_expr_at(Expr::Constant(reference), ty, self.current_span)
    }

    pub(crate) fn add_node(&mut self, node: Node) -> NodeRef {
        self.plan.add_node_at(node, self.current_span)
    }

    pub(crate) fn into_plan(self) -> Plan {
        self.plan
    }

    /// A table index nothing else has.
    pub(crate) fn fresh_index(&mut self) -> u32 {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    /// A reference to one column of an operator's output.
    pub(crate) fn column(&mut self, index: u32, position: usize, ty: LogicalType) -> ExprRef {
        let binding = ColumnBinding::new(index, position as u32);
        self.plan.add_expr(Expr::Column(binding), ty)
    }

    /// Joins scalar query results into the row stream that contains their expressions.
    pub(crate) fn attach_scalar_subqueries(&mut self, mut input: NodeRef) -> NodeRef {
        let subqueries = std::mem::take(&mut self.scalar_subqueries);
        for pending in subqueries {
            input = self.attach_subquery(input, pending);
        }
        input
    }

    /// Joins one query's result into a row stream, which is where its columns come from.
    ///
    /// Split out from [`Self::attach_scalar_subqueries`] because a join's `ON` does not attach its
    /// queries to the rows the whole `FROM` produced. It attaches them to one of the join's two
    /// inputs, since a join condition is evaluated by the join and can only read what the join was
    /// given.
    pub(crate) fn attach_subquery(&mut self, input: NodeRef, pending: PendingSubquery) -> NodeRef {
        let PendingSubquery {
            node: mut right,
            kind,
            conditions,
            dependent,
            reads: _,
            index: _,
            inside_aggregate: _,
        } = pending;
        if kind == JoinKind::Single && !self.semantics.scalar_subquery_error_on_multiple_rows() {
            right = self.add_node(Node::Limit {
                input: right,
                count: Bound::Rows(1),
                offset: Bound::Rows(0),
            });
        }
        let conditions = self.plan.add_expr_list(&conditions);
        if dependent {
            self.add_node(Node::DependentJoin { left: input, right, kind, conditions })
        } else {
            self.add_node(Node::Join {
                left: input,
                right,
                kind,
                conditions,
                build: BuildSide::default(),
            })
        }
    }

    // ---------------------------------------------------------------- queries

    pub(crate) fn bind_query(
        &mut self,
        ast: &Ast,
        query: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let span = ast.query_span(query);
        let outer = std::mem::replace(&mut self.current_span, span);
        // A subquery in the arguments of a table function is bound the way any query is, so a name
        // there that is no column is still a missing column.
        let identifiers = std::mem::replace(&mut self.identifiers_as_strings, false);
        let result =
            self.bind_query_inner(ast, query).map_err(|error| error.with_fallback_span(span));
        self.identifiers_as_strings = identifiers;
        self.current_span = outer;
        result
    }

    fn bind_query_inner(&mut self, ast: &Ast, query: ast::QueryRef) -> Result<(NodeRef, Scope)> {
        let written = ast.query(query);
        if written.ctes.is_empty() {
            return self.bind_body(ast, &written);
        }
        // The names a query introduces are gone again once it is bound, and they go whether the
        // binding worked or not, which is why the stack is cut back here rather than at the end of
        // the call that pushed onto it.
        let depth = self.materialized.len();
        let result = self.bind_materialized(ast, &written);
        self.materialized.truncate(depth);
        result
    }

    /// A query with materialised `WITH` definitions in front of it.
    ///
    /// The definitions are bound first and in the order they were written, so that a later one can
    /// read an earlier one, and then the body. The wrapping runs backwards so that the first
    /// definition ends up outermost, which is the order they have to be filled in.
    fn bind_materialized(&mut self, ast: &Ast, written: &ast::Query) -> Result<(NodeRef, Scope)> {
        let depth = self.materialized.len();
        // A parameter of no type in a definition is a `text`, whatever the body keeps.
        let unknowns_kept = std::mem::replace(&mut self.unknowns_kept, false);
        let held = ast.cte_list(written.ctes).to_vec();
        let mut definitions = Vec::with_capacity(held.len());
        for &index in &held {
            definitions.push(self.bind_definition(ast, index)?);
        }
        self.unknowns_kept = unknowns_kept;
        let (mut node, scope) = self.bind_body(ast, written)?;
        for (at, definition) in definitions.into_iter().enumerate().rev() {
            let entry = &self.materialized[depth + at];
            let cte = entry.cte;
            let name = entry.name.clone();
            let fields = entry.fields.clone();
            let name = self.plan.intern(&name);
            let columns = self.plan.add_fields(&fields);
            node =
                self.add_node(Node::MaterializedCte { definition, body: node, name, cte, columns });
        }
        Ok((node, scope))
    }

    /// Binds one materialised `WITH` definition and makes its name readable from there on.
    ///
    /// The definition is projected onto exactly the columns a read of it sees, under the names the
    /// column list declared when there was one. That projection is not decoration: what is held is
    /// what a read gets back, so the held rows have to be the rows of the definition's own select
    /// list and nothing it happened to carry along underneath.
    ///
    /// A column list with more names in it than the definition has columns is not an error here,
    /// which is the pinned build's rule and is written out on [`Scope::rename_prefix`].
    fn bind_definition(&mut self, ast: &Ast, index: u32) -> Result<NodeRef> {
        let held = ast.cte(index);
        if held.recursive {
            return self.bind_recursive(ast, index);
        }
        let name = ast.string(held.name).to_string();
        let (node, mut scope) = if held.dml.is_some() {
            self.bind_written(index, &name)?
        } else {
            self.bind_query(ast, held.query)?
        };
        if !held.columns.is_empty() {
            let names: Vec<&str> = ast.name(held.columns).collect();
            scope.rename_prefix(&names);
        }
        let table = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.len());
        let mut names = Vec::with_capacity(scope.len());
        for column in &scope.columns {
            exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan.intern(&column.name));
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        let node = self.add_node(Node::Project { input: node, index: table, exprs, names });
        let cte = self.next_cte;
        self.next_cte += 1;
        let fields = scope.fields();
        let finished = fields.clone();
        self.materialized.push(Materialized {
            written: index,
            cte,
            name,
            fields,
            recurring: cte,
            finished,
            output: Some(table),
        });
        Ok(node)
    }

    /// The rows a data changing definition produced, as literal rows.
    ///
    /// The statement runs before anything reading it is bound, so its rows are known by now and are
    /// handed in with the parameters. A plan wanted without running it, which is what `EXPLAIN`
    /// asks for, has no rows to read.
    fn bind_written(&mut self, index: u32, name: &str) -> Result<(NodeRef, Scope)> {
        let Some(written) = self.parameters.written(index) else {
            return Err(Error::not_implemented(
                "a data-modifying WITH definition in a statement that does not run it",
            ));
        };
        let written = written.clone();
        self.bind_rows(&written, name)
    }

    /// Rows known before the statement was bound, as literal rows under `name`.
    fn bind_rows(&mut self, written: &Written, name: &str) -> Result<(NodeRef, Scope)> {
        // A definition with no `RETURNING` has a row for each row it changed and no columns, and
        // the rows are kept by a column of their own that nothing can name.
        let blank = written.names.is_empty();
        let mut rows = Vec::with_capacity(written.rows.len());
        for row in &written.rows {
            let mut items = Vec::with_capacity(row.len().max(1));
            for (value, ty) in row.iter().zip(&written.types) {
                let item = self.plan.add_constant(value.clone());
                items.push(self.cast_to(item, ty));
            }
            if blank {
                items.push(self.plan.add_constant(Value::Boolean(true)));
            }
            rows.push(self.plan.add_expr_list(&items));
        }
        let rows = self.plan.add_rows(&rows);
        let fields: Vec<Field> = written
            .names
            .iter()
            .zip(&written.types)
            .map(|(name, ty)| Field::new(name.clone(), ty.clone()))
            .collect();
        let mut columns = fields.clone();
        if blank {
            columns.push(Field::new("changed", LogicalType::Boolean));
        }
        let held = self.plan.add_fields(&columns);
        let table = self.fresh_index();
        let node = self.add_node(Node::Values { index: table, columns: held, rows });
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: name.to_string(),
                name: field.name.clone(),
                binding: ColumnBinding::new(table, at as u32),
                ty: field.ty.clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        Ok((node, scope))
    }

    /// Binds a definition that reads itself, which the parser has already checked is a `UNION` or
    /// `UNION ALL` whose right side holds every read of it.
    ///
    /// The left side is bound first and on its own, because it is what fixes the columns: their
    /// names, under the declared list when there is one, and their types, which the right side is
    /// cast to rather than met halfway as an ordinary union would. Only then is the name made
    /// readable, so the reads in the right side bind as scans of the rows the round before made.
    fn bind_recursive(&mut self, ast: &Ast, index: u32) -> Result<NodeRef> {
        let held = ast.cte(index);
        let name = ast.string(held.name).to_string();
        let written = ast.query(held.query);
        let ast::QueryBody::SetOp { quantifier, left, right, .. } = written.body else {
            unreachable!("the parser only marks a union as recursive")
        };
        if !written.ctes.is_empty() {
            return Err(Error::not_implemented(
                "a WITH clause inside a recursive definition is not supported yet",
            ));
        }
        // PostgreSQL reads the mark of a `CYCLE` clause before the query of the definition.
        let mark = match held.cycle {
            Some(cycle) => Some(self.cycle_mark(ast, &cycle)?),
            None => None,
        };
        let (anchor, mut scope) = self.bind_query(ast, left)?;
        if !held.columns.is_empty() {
            let names: Vec<&str> = ast.name(held.columns).collect();
            scope.rename_prefix(&names);
        }
        let fields = scope.fields();
        let width = fields.len();
        let (anchor, over) = self.project_onto(anchor, &scope, &fields, &name)?;
        let added = self.search_cycle(ast, &held, &fields, mark)?;
        let (anchor, over, fields) = match &added {
            Some(added) => {
                let (anchor, over) = self.with_added(anchor, &over, width, added, &name)?;
                let fields = over.fields();
                (anchor, over, fields)
            }
            None => (anchor, over, fields),
        };
        let (key, folds) = self.recursive_key(ast, held.key, &over, &fields)?;
        let args: Vec<ExprRef> = folds.iter().flat_map(|fold| fold.args.iter().copied()).collect();
        let anchor = self.with_arguments(anchor, &over, &args);
        // The table holds each aggregate's answer where its column was, typed as the answer. With
        // `UNION ALL` a round reads the rows the round before made, which are the anchor's types,
        // and with `UNION` it reads rows of the table.
        let mut table = fields.clone();
        for fold in &folds {
            table[fold.into as usize].ty = fold.ty.clone();
        }
        let all = quantifier == Quantifier::All;
        let working = if all { fields.clone() } else { table.clone() };
        let cte = self.next_cte;
        let recurring = cte + 1;
        self.next_cte += 2;
        let output = self.plan.node(anchor).table_index();
        self.materialized.push(Materialized {
            written: index,
            cte,
            name: name.clone(),
            fields: working,
            recurring,
            finished: table.clone(),
            output,
        });
        if let Some(added) = &added {
            self.passing = Some(self.passing(ast, index, (left, right), width, added)?);
        }
        self.recursing.push(index);
        let bound = self.bind_query(ast, right);
        self.recursing.pop();
        let passed = self.passing.take().map_or(0, |passing| passing.len());
        let (recursive, other) = bound?;
        if other.len() != width + passed {
            return Err(Error::binder(
                "Set operations can only apply to expressions with the same number of result columns",
            )
            .state(SqlState::SYNTAX_ERROR)
            .pg("each UNION query must have the same number of columns")
            .with_span(first_column(ast, right)));
        }
        let (recursive, over) = self.project_onto(recursive, &other, &fields, &name)?;
        let (recursive, over) = match &added {
            Some(added) => self.with_added(recursive, &over, width, added, &name)?,
            None => (recursive, over),
        };
        if !all {
            for (at, field) in fields.iter().enumerate().take(width) {
                let span = set_op_column(ast, left, at, fields.len());
                self.sort_group_operators(&field.ty, false, span)?;
            }
        }
        if !all
            && self.semantics.recursive_union() == RecursiveUnion::Postgres
            && !table.iter().all(|field| hashable(&field.ty))
        {
            return Err(Error::not_implemented("could not implement recursive UNION")
                .state(SqlState::FEATURE_NOT_SUPPORTED)
                .detail("All column datatypes must be hashable.")
                .unplaced());
        }
        let mut args = Vec::with_capacity(args.len());
        for fold in &folds {
            args.extend(self.fold_call(ast, fold.call, &over)?.args);
        }
        let recursive = self.with_arguments(recursive, &over, &args);
        // What reads the name from here on reads the finished table.
        if let Some(held) = self.materialized.iter_mut().rev().find(|held| held.written == index) {
            held.fields = table.clone();
        }
        let node_index = self.fresh_index();
        let name = self.plan.intern(&name);
        let columns = self.plan.add_fields(&table);
        let key = self.plan.add_positions(&key);
        let calls: Vec<Field> =
            folds.iter().map(|fold| Field::new(fold.name.clone(), fold.ty.clone())).collect();
        let aggregates = self.plan.add_fields(&calls);
        let into: Vec<u32> =
            folds.iter().flat_map(|fold| [fold.into, fold.args.len() as u32]).collect();
        let folds = self.plan.add_positions(&into);
        Ok(self.add_node(Node::RecursiveCte {
            anchor,
            recursive,
            index: node_index,
            cte,
            name,
            all,
            columns,
            recurring,
            key,
            aggregates,
            folds,
            wanted: None,
        }))
    }

    /// The positions of the columns `USING KEY (...)` names, in the order it names them, and the
    /// aggregates it names, bound against `over`, the left side's columns.
    ///
    /// A key is a column of the definition, by name, the case not mattering and a qualifier not
    /// looked at. An aggregate lands in the column its alias names, or failing an alias in the one
    /// its first argument is, when that argument is a bare column. The list is read in order and
    /// the pin's checks go with it, so a column named as a key after an aggregate already landed
    /// in it is taken, where the other order is refused.
    fn recursive_key(
        &mut self,
        ast: &Ast,
        key: ast::Slice,
        over: &Scope,
        fields: &[Field],
    ) -> Result<(Vec<u32>, Vec<Fold>)> {
        let mut positions = Vec::new();
        let mut folds: Vec<Fold> = Vec::new();
        for target in ast.target_list(key) {
            let span = ast.expr_span(target.expr);
            match ast.expr(target.expr) {
                ast::Expr::Column { .. } if target.alias != NONE => {
                    return Err(Error::binder(
                        "In USING KEY, only direct calls to an aggregate function can have an alias.",
                    )
                    .with_span(span));
                }
                ast::Expr::Column { name } => {
                    let written = ast.name(name).last().unwrap_or_default();
                    let Some(at) = fields.iter().position(|field| same_name(&field.name, written))
                    else {
                        let names: Vec<&str> =
                            fields.iter().map(|field| field.name.as_str()).collect();
                        return Err(Error::binder(format!(
                            "Referenced column \"{written}\" not found in FROM clause! Candidate bindings: \"{}\"",
                            names.join("\", \"")
                        ))
                        .with_span(span));
                    };
                    let at = at as u32;
                    if !positions.contains(&at) {
                        positions.push(at);
                    }
                }
                ast::Expr::Function { name, args, distinct, filter }
                    if Self::folded(&ast.name(name).collect::<Vec<_>>()) =>
                {
                    let refused = if filter != NONE {
                        Some("FILTER clause is not yet supported for aggregates in USING KEY")
                    } else if distinct {
                        Some("DISTINCT is not yet supported for aggregates in USING KEY")
                    } else if !ast.aggregate_order(target.expr).is_empty() {
                        Some("ORDER BY clause is not yet supported for aggregates in USING KEY")
                    } else {
                        None
                    };
                    if let Some(refused) = refused {
                        return Err(Error::binder(refused).with_span(span));
                    }
                    // A lone star is no argument at all to the pin, which finds the column before
                    // it looks for the function, so `count(*)` needs an alias and `avg(*)` with one
                    // is `avg()`.
                    let starred = matches!(ast.expr_list(args), [arg] if matches!(
                        ast.expr(*arg),
                        ast::Expr::Star { qualifier, replacements }
                            if qualifier.is_empty() && replacements.is_empty()
                    ));
                    if starred && target.alias == NONE {
                        return Err(Error::binder(
                            "In USING KEY, an aggregate must either have a column reference or an alias.",
                        )
                        .with_span(span));
                    }
                    let written = ast.name(name).last().unwrap_or_default();
                    if starred && !same_name(written, "count") {
                        let error = resolve(written, &[]).err().unwrap_or_else(|| {
                            Error::internal("an aggregate that takes no arguments")
                        });
                        return Err(Error::binder(format!(
                            "No matching aggregate function\n{error}"
                        ))
                        .with_span(span));
                    }
                    let mut fold = self.fold_call(ast, target.expr, over)?;
                    let first = ast.expr_list(args).first().map(|&arg| ast.expr(arg));
                    let written = if target.alias != NONE {
                        ast.string(target.alias)
                    } else if let Some(ast::Expr::Column { name }) = first {
                        ast.name(name).last().unwrap_or_default()
                    } else {
                        return Err(Error::binder(
                            "In USING KEY, an aggregate must either have a column reference or an alias.",
                        )
                        .with_span(span));
                    };
                    let Some(into) =
                        fields.iter().position(|field| same_name(&field.name, written))
                    else {
                        return Err(Error::binder(format!(
                            "Could not find column with name '\"{written}\"' to bind aggregate to."
                        ))
                        .with_span(span));
                    };
                    let into = into as u32;
                    if positions.contains(&into) {
                        return Err(Error::binder(format!(
                            "Column '\"{written}\"' cannot be used as both key and aggregate in USING KEY clause. Try using an alias for the aggregation."
                        ))
                        .with_span(span));
                    }
                    if folds.iter().any(|fold| fold.into == into) {
                        return Err(Error::binder(format!(
                            "Column '\"{written}\"' referenced multiple times in USING KEY clause. Try using an alias for one of the aggregates."
                        ))
                        .with_span(span));
                    }
                    fold.into = into;
                    folds.push(fold);
                }
                _ => {
                    return Err(Error::binder(format!(
                        "'{}' can't be used in the USING KEY clause. It has to be either a column name as a key or a direct call to an aggregate function.",
                        rudb_parse::deparse::expression(ast, target.expr)
                    ))
                    .with_span(span));
                }
            }
        }
        if positions.is_empty() && !folds.is_empty() {
            return Err(Error::binder("USING KEY clause requires at least one key column."));
        }
        Ok((positions, folds))
    }

    /// Whether a call in `USING KEY` names an aggregate. A catalog in front of the name has to be
    /// `system`, where the aggregates are, and a schema is not looked at, which is what the pin
    /// does.
    fn folded(name: &[&str]) -> bool {
        let catalog = name.len() < 3 || same_name(name[0], "system");
        catalog
            && kind_of(name.last().copied().unwrap_or_default()) == Some(FunctionKind::Aggregate)
    }

    /// An aggregate call of `USING KEY`, bound against one side's columns the way a call in a
    /// select list is, so it is resolved and checked by the same rules.
    ///
    /// It is bound into an aggregation of its own that is thrown away after, since what is kept is
    /// the call's name, its answer's type and its arguments, which the side computes as columns.
    fn fold_call(&mut self, ast: &Ast, call: ast::ExprRef, over: &Scope) -> Result<Fold> {
        let index = self.fresh_index();
        let aggregation = Aggregation { index, groups: Vec::new(), aggregates: Vec::new() };
        let outer = self.aggregation.replace(aggregation);
        self.folding = true;
        let bound = self.bind_expr(ast, call, over);
        self.folding = false;
        let aggregation = std::mem::replace(&mut self.aggregation, outer);
        let bound = bound?;
        let aggregates = aggregation.map(|held| held.aggregates).unwrap_or_default();
        let at = match self.plan.expr(bound) {
            Expr::Column(binding) if binding.table == index => binding.column as usize,
            _ => return Err(Error::internal("a USING KEY aggregate bound to something else")),
        };
        let Some(&found) = aggregates.get(at) else {
            return Err(Error::internal("a USING KEY aggregate that was not recorded"));
        };
        let Expr::Aggregate { name, args, .. } = *self.plan.expr(found) else {
            return Err(Error::internal("a USING KEY aggregate that is not an aggregate"));
        };
        Ok(Fold {
            into: 0,
            call,
            name: self.plan.string(name).to_string(),
            ty: self.plan.expr_type(found).clone(),
            args: self.plan.expr_list(args).to_vec(),
        })
    }

    /// Projects a side of a recursive definition onto the definition's columns by position, casting
    /// each to the type the left side gave it, and answers the scope of what it produces.
    fn project_onto(
        &mut self,
        node: NodeRef,
        scope: &Scope,
        fields: &[Field],
        name: &str,
    ) -> Result<(NodeRef, Scope)> {
        let table = self.fresh_index();
        let mut exprs = Vec::with_capacity(fields.len());
        let mut names = Vec::with_capacity(fields.len());
        let mut over = Scope::empty();
        for (at, (column, field)) in scope.columns.iter().zip(fields).enumerate() {
            let expr = self.plan.add_expr(Expr::Column(column.binding), column.ty.clone());
            exprs.push(self.checked_cast_to(expr, &field.ty, false)?);
            names.push(self.plan.intern(&field.name));
            over.push(Visible {
                table: name.to_string(),
                name: field.name.clone(),
                binding: ColumnBinding::new(table, at as u32),
                ty: field.ty.clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        let node = self.add_node(Node::Project { input: node, index: table, exprs, names });
        Ok((node, over))
    }

    /// A side of a recursive definition with the arguments of its `USING KEY` aggregates computed
    /// as columns after its own, or the side as it was when there are none.
    fn with_arguments(&mut self, node: NodeRef, over: &Scope, args: &[ExprRef]) -> NodeRef {
        if args.is_empty() {
            return node;
        }
        let table = self.fresh_index();
        let mut exprs = Vec::with_capacity(over.len() + args.len());
        let mut names = Vec::with_capacity(over.len() + args.len());
        for column in &over.columns {
            exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan.intern(&column.name));
        }
        for (at, &arg) in args.iter().enumerate() {
            exprs.push(arg);
            names.push(self.plan.intern(&format!("#{at}")));
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        self.add_node(Node::Project { input: node, index: table, exprs, names })
    }

    fn bind_body(&mut self, ast: &Ast, written: &ast::Query) -> Result<(NodeRef, Scope)> {
        match written.body {
            ast::QueryBody::Select(select) => self.bind_select(ast, select, written),
            ast::QueryBody::SetOp { op, quantifier, by_name, left, right } => {
                let operator = Operator { op, quantifier, by_name };
                self.bind_set_op(ast, written, operator, left, right)
            }
            ast::QueryBody::Values(rows) => self.bind_values(ast, written, rows),
            ast::QueryBody::Describe(inner) => self.bind_describe(ast, written, inner),
            ast::QueryBody::Show { name, relation } => self.bind_show(ast, written, name, relation),
        }
    }

    /// `SHOW name`, resolved while binding so execution receives an ordinary constant plan.
    fn bind_show(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        name: ast::Slice,
        relation: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let text = ast.name_text(name);
        let parts: Vec<&str> = ast.name(name).collect();
        let table_exists = self.catalog.resolve(&parts).is_ok();
        let as_table = match self.semantics.show_behavior() {
            ShowBehavior::Auto => table_exists,
            ShowBehavior::Setting => false,
            ShowBehavior::Table => true,
        };
        if as_table {
            return self.bind_describe(ast, query, relation);
        }
        // A name the session has no answer for is either a setting rudb has and DuckDB does not, in
        // which case [`Binder::beyond`] reads it, or it is nothing, in which case that says so in
        // upstream's words. `SHOW` prints and printing is text, so a rule's boolean comes back here
        // as the word it reads back as rather than as a boolean column.
        let shown = match self.session.iter().find(|(name, _)| name.eq_ignore_ascii_case(&text)) {
            Some((_, value)) => value.to_string(),
            None => match self.beyond(&text)? {
                Some(Value::Varchar(declared)) => declared,
                Some(other) => other.to_string(),
                None => {
                    return Err(Error::catalog(format!(
                        "Setting with name \"{text}\" does not exist"
                    )));
                }
            },
        };
        let field = Field::new(text, LogicalType::Varchar);
        let expr = self.plan.add_constant(Value::Varchar(shown));
        let row = self.plan.add_expr_list(&[expr]);
        let rows = self.plan.add_rows(&[row]);
        let columns = self.plan.add_fields(std::slice::from_ref(&field));
        let index = self.fresh_index();
        let node = self.add_node(Node::Values { index, columns, rows });
        let mut scope = Scope::empty();
        scope.push(Visible {
            table: String::new(),
            name: field.name,
            binding: ColumnBinding::new(index, 0),
            ty: LogicalType::Varchar,
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        });
        Ok((node, scope))
    }

    /// `DESCRIBE <query>`, which is six VARCHAR columns saying what the query returns.
    ///
    /// The query is bound and never run, because binding is the whole of the answer: the names and
    /// the types of a query's columns are settled by the time the binder is done with it, so the
    /// rows of a describe are a constant from there on. That is why this comes out as a `VALUES`
    /// whose rows were computed here rather than as an operator of its own, and it is what makes
    /// `SELECT column_name FROM (DESCRIBE ...) WHERE ...` an ordinary query over an ordinary
    /// relation with no special case above it.
    ///
    /// The six columns, their order and their types are the reference binary's. `key` says which
    /// key of its table a column passed straight through from one is in. `default` is the SQL of the column's `DEFAULT` in the pin's spelling, and
    /// `extra` is empty upstream as well on every table it was asked about. They are here rather
    /// than left out because the width of a result is part of the result, and a program that reads
    /// the fifth column has to find one.
    fn bind_describe(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        inner: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        let (_, described) = self.bind_query(ast, inner)?;
        let fields: Vec<Field> = ["column_name", "column_type", "null", "key", "default", "extra"]
            .iter()
            .map(|name| Field::new(*name, LogicalType::Varchar))
            .collect();
        let mut slices = Vec::with_capacity(described.columns.len());
        for column in described.columns.clone() {
            // `NO` and `YES` and not a boolean, because the column is VARCHAR upstream and a
            // client that prints the result has to get the same four or three characters.
            let written = [
                column.name.clone(),
                column.ty.to_string(),
                if column.not_null { "NO" } else { "YES" }.to_owned(),
            ];
            let mut items: Vec<ExprRef> = written
                .into_iter()
                .map(|text| self.plan.add_constant(Value::Varchar(text)))
                .collect();
            let mark = match column.key {
                Some(mark) => self.plan.add_constant(Value::Varchar(mark.to_owned())),
                None => {
                    let empty = self.plan.add_constant(Value::Null);
                    self.cast_to(empty, &LogicalType::Varchar)
                }
            };
            items.push(mark);
            if let Some(default) = &column.default {
                items.push(self.plan.add_constant(Value::Varchar(default.clone())));
            }
            while items.len() < 6 {
                let empty = self.plan.add_constant(Value::Null);
                items.push(self.cast_to(empty, &LogicalType::Varchar));
            }
            slices.push(self.plan.add_expr_list(&items));
        }
        let rows = self.plan.add_rows(&slices);
        let columns = self.plan.add_fields(&fields);
        let index = self.fresh_index();
        let mut node = self.add_node(Node::Values { index, columns, rows });
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut scope)?;
        Ok((node, scope))
    }

    /// A column's default as an expression of the column's type, or a null of that type for a
    /// column with none. A typed null rather than `add_constant`, which would give it the null
    /// type and make the column's type depend on whether a row happened to be inserted into it.
    pub(crate) fn bind_default(&mut self, text: Option<&str>, ty: &LogicalType) -> Result<ExprRef> {
        let Some(text) = text else {
            let value = self.plan.add_value(Value::Null);
            return Ok(self.plan.add_expr(Expr::Constant(value), ty.clone()));
        };
        let ast = rudb_parse::parse_ast(&format!("SELECT {text}"))?;
        let found = match ast.statements.first() {
            Some(&ast::Statement::Query(query)) => match ast.query(query).body {
                ast::QueryBody::Select(select) => {
                    ast.target_list(ast.select(select).targets).first().map(|target| target.expr)
                }
                _ => None,
            },
            _ => None,
        };
        let Some(expr) = found else {
            return Err(Error::internal(format!("a default that is not an expression: {text}")));
        };
        let expr = self.bind_expr(&ast, expr, &Scope::empty())?;
        self.checked_cast_to(expr, ty, false)
    }

    /// Whether a projected expression is a column passed straight through from below.
    ///
    /// Only `DESCRIBE` asks, and only to decide whether the `null` column says `NO`. Anything that
    /// is computed is nullable however strict its inputs were, which is both the safe reading and
    /// the one the reference binary gives.
    fn passes_through(&self, expr: ExprRef, input: &Scope) -> bool {
        let Expr::Column(binding) = *self.plan.expr(expr) else { return false };
        input.columns.iter().any(|column| column.binding == binding && column.not_null)
    }

    /// The key a projected expression is in, when it is a column passed straight through from a
    /// table that has one. The same question as [`Self::passes_through`], asked for `DESCRIBE`'s
    /// `key` column.
    fn key_through(&self, expr: ExprRef, input: &Scope) -> Option<&'static str> {
        self.through(expr, input).and_then(|column| column.key)
    }

    /// The column a projected expression passes straight through from below, if it is one.
    pub(crate) fn through<'s>(&self, expr: ExprRef, input: &'s Scope) -> Option<&'s Visible> {
        let Expr::Column(binding) = *self.plan.expr(expr) else { return None };
        input.columns.iter().find(|column| column.binding == binding)
    }

    /// `VALUES (1, 'a'), (2, 'b')`, as a query in its own right.
    ///
    /// The column names are `col0`, `col1` and so on, which is what DuckDB calls them, and the
    /// column types are what every row in that position promotes to. Promotion is the same rule a
    /// set operation uses, and for the same reason: a column has one type and the rows have to
    /// agree on it before anything downstream can read the column.
    fn bind_values(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        rows: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let written = ast.rows(rows).to_vec();
        let Some(first) = written.first() else {
            return Err(Error::binder("VALUES needs at least one row"));
        };
        let width = first.len as usize;
        for (at, row) in written.iter().enumerate() {
            if row.len as usize != width {
                let error = Error::binder(format!(
                    "VALUES lists must all be the same length, expected {width} columns but row {} has {}",
                    at + 1,
                    row.len
                ))
                .state(SqlState::SYNTAX_ERROR)
                .pg("VALUES lists must all be the same length");
                return Err(match ast.expr_list(*row).first() {
                    Some(&first) => error.with_span(ast.expr_span(first)),
                    None => error,
                });
            }
        }
        // A row of a `VALUES` cannot see a column, because there is nothing under it to see.
        let empty = Scope::empty();
        // A row can be narrower than the columns of the `INSERT`, when the values go to the leading
        // columns as in PostgreSQL. It lands in the first columns, so it reads only those.
        let mut defaults = self.insert_defaults.take();
        let mut inputs = self.insert_inputs.take().unwrap_or_default();
        if let Some(defaults) = defaults.as_mut() {
            defaults.truncate(width);
        }
        inputs.truncate(width);
        let previous = std::mem::replace(&mut self.clause, "VALUES clause");
        let mut bound: Vec<Vec<ExprRef>> = Vec::with_capacity(written.len());
        // The scalar queries of each row, which join into the one row that row is made from.
        let mut waiting: Vec<Vec<PendingSubquery>> = Vec::with_capacity(written.len());
        for row in &written {
            let before = self.scalar_subqueries.len();
            let mut items = Vec::with_capacity(width);
            for (at, &expr) in ast.expr_list(*row).iter().enumerate() {
                let column = defaults.as_ref().and_then(|defaults| defaults.get(at));
                items.push(match (ast.expr(expr), column) {
                    (ast::Expr::Default, Some((ty, default))) => {
                        self.bind_default(default.as_deref(), ty)?
                    }
                    _ => match inputs.get(at).and_then(|&oid| self.read_literal(ast, expr, oid)) {
                        Some(value) => value?,
                        None => self.bind_expr(ast, expr, &empty)?,
                    },
                });
            }
            bound.push(items);
            waiting.push(self.scalar_subqueries.split_off(before));
        }
        self.clause = previous;
        // The rows of an `INSERT ... VALUES` are cast to the columns they land in, one by one,
        // rather than to a type they all agree on first. So `('a'), (2)` goes into a VARCHAR
        // column and `('1'), (2.5)` into an INTEGER one, both of which the pin takes, though
        // neither pair has a type in common on its own.
        if let Some(defaults) = defaults.as_ref().filter(|defaults| defaults.len() == width) {
            let types = defaults.iter().map(|(ty, _)| ty.clone()).collect();
            return self.values_node(ast, query, &bound, waiting, types);
        }
        if self.semantics.common_types() == CommonTypes::Postgres {
            for at in 0..width {
                let column: Vec<ast::ExprRef> =
                    written.iter().map(|&row| ast.expr_list(row)[at]).collect();
                let mut values: Vec<ExprRef> = bound.iter().map(|row| row[at]).collect();
                self.common_type(ast, &column, &mut values, Some("VALUES"))?;
                for (row, value) in bound.iter_mut().zip(values) {
                    row[at] = value;
                }
            }
        }
        let mut types = Vec::with_capacity(width);
        for at in 0..width {
            let mut ty = self.plan.expr_type(bound[0][at]).clone();
            for row in &bound[1..] {
                let other = self.plan.expr_type(row[at]).clone();
                ty = ty.promote(&other).ok_or_else(|| {
                    Error::binder(format!(
                        "Cannot combine a value of type {ty} with a value of type {other} in column {} of a VALUES",
                        at + 1
                    ))
                })?;
            }
            types.push(ty);
        }
        self.values_node(ast, query, &bound, waiting, types)
    }

    /// The `VALUES` node over rows already bound, each cast to the type of its column.
    ///
    /// A row with a scalar query in it reads the result of that query, which a `VALUES` node has
    /// no input to read from. Such a row is a projection over one row that the queries join into
    /// instead, and the rows are put back together in the order they were written with a
    /// `UNION ALL`. The rows between two such rows stay one `VALUES` node.
    fn values_node(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        bound: &[Vec<ExprRef>],
        waiting: Vec<Vec<PendingSubquery>>,
        types: Vec<LogicalType>,
    ) -> Result<(NodeRef, Scope)> {
        let (prefix, first) = match self.semantics.values_names() {
            ValuesNames::FromZero => ("col", 0),
            ValuesNames::FromOne => ("column", 1),
        };
        let fields: Vec<Field> = types
            .iter()
            .enumerate()
            .map(|(at, ty)| Field::new(format!("{prefix}{}", at + first), ty.clone()))
            .collect();
        let columns = self.plan.add_fields(&fields);
        let names: Vec<_> = fields.iter().map(|field| self.plan.intern(&field.name)).collect();
        let names = self.plan.add_name_list(&names);
        let mut parts: Vec<(NodeRef, u32)> = Vec::new();
        let mut slices = Vec::with_capacity(bound.len());
        for (row, pending) in bound.iter().zip(waiting) {
            let items: Vec<ExprRef> = row
                .iter()
                .zip(&types)
                .map(|(&expr, ty)| self.checked_cast_to(expr, ty, false))
                .collect::<Result<_>>()?;
            if pending.is_empty() {
                slices.push(self.plan.add_expr_list(&items));
                continue;
            }
            if !slices.is_empty() {
                let rows = self.plan.add_rows(&std::mem::take(&mut slices));
                let index = self.fresh_index();
                parts.push((self.add_node(Node::Values { index, columns, rows }), index));
            }
            let mut input = self.add_node(Node::Dummy);
            for pending in pending {
                input = self.attach_subquery(input, pending);
            }
            let exprs = self.plan.add_expr_list(&items);
            let index = self.fresh_index();
            parts.push((self.add_node(Node::Project { input, index, exprs, names }), index));
        }
        if !slices.is_empty() || parts.is_empty() {
            let rows = self.plan.add_rows(&slices);
            let index = self.fresh_index();
            parts.push((self.add_node(Node::Values { index, columns, rows }), index));
        }
        let mut parts = parts.into_iter();
        let (mut node, mut index) = parts.next().expect("a VALUES has a part");
        for (right, _) in parts {
            index = self.fresh_index();
            let (kind, all) = (SetOpKind::Union, true);
            node = self.add_node(Node::SetOp { left: node, right, kind, all, index });
        }
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut scope)?;
        Ok((node, scope))
    }

    fn bind_set_op(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        operator: Operator,
        left: ast::QueryRef,
        right: ast::QueryRef,
    ) -> Result<(NodeRef, Scope)> {
        // A parameter of no type in one side takes the type of the column of the other side.
        self.unknowns_kept = true;
        let (left_node, mut left_scope) = self.bind_query(ast, left)?;
        self.unknowns_kept = true;
        let (right_node, mut right_scope) = self.bind_query(ast, right)?;
        let mut common = Vec::new();
        if !operator.by_name
            && left_scope.len() == right_scope.len()
            && self.semantics.common_types() == CommonTypes::Postgres
        {
            let sides = [(left, left_node, &mut left_scope), (right, right_node, &mut right_scope)];
            common = self.set_op_types(ast, operator.op, sides)?;
        }
        let mut merged = if operator.by_name {
            match_by_name(&left_scope, &right_scope)?
        } else {
            match_by_position(&left_scope, &right_scope, operator.op)
                .map_err(|error| error.with_fallback_span(first_column(ast, right)))?
        };
        let mut declared = vec![None; merged.len()];
        for ((column, (ty, typed)), declared) in merged.iter_mut().zip(common).zip(&mut declared) {
            if let Some(ty) = ty {
                column.ty = ty;
            }
            *declared = typed.map(Origin::typed);
        }
        let index = self.fresh_index();
        let all = operator.quantifier == Quantifier::All;
        if !all {
            for (at, column) in merged.iter().enumerate() {
                let span = set_op_column(ast, left, at, left_scope.len());
                self.sort_group_operators(&column.ty, false, span)?;
            }
        }
        let sides: Vec<[Option<(ColumnBinding, LogicalType)>; 2]> = merged
            .iter()
            .map(|column| {
                let side = |scope: &Scope, at: Option<usize>| {
                    at.map(|at| (scope.columns[at].binding, scope.columns[at].ty.clone()))
                };
                [side(&left_scope, column.left), side(&right_scope, column.right)]
            })
            .collect();
        self.set_op_collations(index, &sides, operator.op == SetOp::Union && all)?;
        let conformed = self.conform(left_node, &left_scope, &merged, |column| column.left)?;
        let left_columns = self.conformed(left_node, conformed, &left_scope, &merged, true);
        let left_node = conformed;
        let conformed = self.conform(right_node, &right_scope, &merged, |column| column.right)?;
        let right_columns = self.conformed(right_node, conformed, &right_scope, &merged, false);
        let right_node = conformed;
        let kind = match operator.op {
            SetOp::Union => SetOpKind::Union,
            SetOp::Except => SetOpKind::Except,
            SetOp::Intersect => SetOpKind::Intersect,
        };
        let columns: Vec<(String, LogicalType)> =
            merged.iter().map(|column| (column.name.clone(), column.ty.clone())).collect();
        let sides = [(left_node, left_columns), (right_node, right_columns)];
        // UNION alone removes duplicates and UNION ALL keeps them, which is the one place the
        // unwritten quantifier and ALL disagree.
        let mut node = match self.collated_set_op(kind, all, index, sides, &columns)? {
            Some(node) => node,
            None => {
                self.add_node(Node::SetOp { left: left_node, right: right_node, kind, all, index })
            }
        };
        let mut scope = Scope::empty();
        for (at, column) in merged.iter().enumerate() {
            scope.push(Visible {
                table: String::new(),
                name: column.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: column.ty.clone(),
                // A column of a set operation is nullable whatever the two sides were, because a
                // column that refuses nulls on one side and takes them on the other takes them.
                not_null: false,
                key: None,
                default: None,
                origin: declared[at],
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        // Above a set operation there is nothing but the output columns, so an ORDER BY term is
        // either a position, an output name, or an expression over the output, and never needs a
        // column projected for it that the query did not ask for.
        let keys = self.sort_keys(ast, query, &scope, &[])?;
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut scope)?;
        Ok((node, scope))
    }

    /// The types of the columns of a set operation in a PostgreSQL session, the rule of
    /// `transformSetOperationTree`, with one entry for each column.
    ///
    /// The two columns at a position take the type of `select_common_type`, and two categories
    /// are error 42804 `UNION types integer and boolean cannot be matched` at the column of the
    /// side that does not match. A string literal that a plain `SELECT` writes as its column has
    /// no type of its own, so it is read with the input function of the common type, and its error
    /// is at the literal. The entry is the common type when the two sides hold different types,
    /// and `None` when they hold the same one or when the rule leaves the column to the pin.
    ///
    /// A side reads as the type its column declares, so a `char(n)` column is a `bpchar` and not
    /// the `text` its value is. The second entry is the PostgreSQL type of the column when it says
    /// more than the type of its value: the common type, with the type modifier of the two sides
    /// when they have the same type and modifier, as `select_common_typmod` keeps it.
    fn set_op_types(
        &mut self,
        ast: &Ast,
        op: SetOp,
        mut sides: [(ast::QueryRef, NodeRef, &mut Scope); 2],
    ) -> Result<Vec<(Option<LogicalType>, Option<DeclaredType>)>> {
        let width = sides[0].2.len();
        let mut common = Vec::with_capacity(width);
        'column: for at in 0..width {
            let mut written = [None; 2];
            let mut types = [None; 2];
            let mut typmods = [None; 2];
            for (side, (query, _, scope)) in sides.iter().enumerate() {
                let ty = &scope.columns[at].ty;
                written[side] = written_column(ast, *query, at, scope.len());
                let unknown = match written[side].map(|expr| ast.expr(expr)) {
                    // A parameter of no type takes the type of the other side in `conform`.
                    Some(ast::Expr::Parameter { .. }) => {
                        common.push((None, None));
                        continue 'column;
                    }
                    Some(ast::Expr::Literal { kind: LiteralKind::String, .. }) => true,
                    _ => *ty == LogicalType::Null,
                };
                if unknown {
                    continue;
                }
                let declared =
                    scope.columns[at].origin.and_then(|origin| origin.ty).filter(|declared| {
                        rudb_pgtypes::logical_type(declared.oid).as_ref() == Some(ty)
                    });
                let oid = match (declared, written[side]) {
                    (Some(declared), _) => Some(declared.oid),
                    (None, Some(expr)) => written_oid(ast, expr, ty),
                    (None, None) => postgres_oid(ty),
                };
                let Some(oid) = oid else {
                    common.push((None, None));
                    continue 'column;
                };
                types[side] = Some(oid);
                typmods[side] = Some(declared.map_or(-1, |declared| declared.typmod));
            }
            let oid = rudb_pgtypes::common_type(&types).map_err(|mismatch| {
                let first = rudb_pgtypes::format_type(mismatch.first);
                let other = rudb_pgtypes::format_type(mismatch.other);
                let (query, _, _) = &sides[mismatch.at];
                let span = written[mismatch.at]
                    .map_or_else(|| first_column(ast, *query), |expr| ast.leftmost_span(expr));
                Error::binder(format!("{} types {first} and {other} cannot be matched", op.name()))
                    .state(SqlState::DATATYPE_MISMATCH)
                    .with_span(span)
            })?;
            let Some(target) = rudb_pgtypes::logical_type(oid) else {
                common.push((None, None));
                continue;
            };
            // A side of another type, or one of no type, leaves the column with no modifier.
            let typmod = match (types, typmods) {
                ([Some(left), Some(right)], [Some(one), Some(other)])
                    if left == oid && right == oid && one == other =>
                {
                    one
                }
                _ => -1,
            };
            let declared = (Some(oid) != postgres_oid(&target) || typmod != -1)
                .then_some(DeclaredType { oid, typmod });
            for (side, (_, node, scope)) in sides.iter_mut().enumerate() {
                let Some(expr) = written[side] else { continue };
                if scope.columns[at].ty == target {
                    continue;
                }
                let Some(value) = self.read_literal(ast, expr, oid) else { continue };
                let value = self.cast_to(value?, &target);
                // The literal is the column of the projection of its `SELECT`, which takes the
                // value read in its place. Under an `ORDER BY` or a `LIMIT` the side is cast to
                // the type in `conform`, which gives the same value.
                let binding = scope.columns[at].binding;
                let Node::Project { index, exprs, .. } = *self.plan.node(*node) else { continue };
                if index != binding.table {
                    continue;
                }
                let mut list = self.plan.expr_list(exprs).to_vec();
                list[binding.column as usize] = value;
                let list = self.plan.add_expr_list(&list);
                if let Node::Project { exprs, .. } = self.plan.node_mut(*node) {
                    *exprs = list;
                }
                scope.columns[at].ty = target.clone();
            }
            // Two nulls are the common type too, which is `text`.
            let (one, other) = (&sides[0].2.columns[at].ty, &sides[1].2.columns[at].ty);
            let same = one == other && *one != LogicalType::Null;
            common.push((if same { None } else { Some(target) }, declared));
        }
        Ok(common)
    }

    /// Projects one side of a set operation onto the columns the operation comes out with.
    ///
    /// `pick` says which column of this side each output column is. It answers nothing for a
    /// column only the other side wrote, which happens under `BY NAME` and which this side fills
    /// with a null, since that is the row it would have written if it had written the column.
    fn conform(
        &mut self,
        node: NodeRef,
        scope: &Scope,
        merged: &[Merged],
        pick: impl Fn(&Merged) -> Option<usize>,
    ) -> Result<NodeRef> {
        let unchanged = merged.len() == scope.len()
            && merged
                .iter()
                .enumerate()
                .all(|(at, column)| pick(column) == Some(at) && column.ty == scope.columns[at].ty);
        if unchanged {
            return Ok(node);
        }
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(merged.len());
        let mut names = Vec::with_capacity(merged.len());
        for column in merged {
            let expr = match pick(column) {
                Some(at) => {
                    let held = &scope.columns[at];
                    self.plan.add_expr(Expr::Column(held.binding), held.ty.clone())
                }
                None => self.plan.add_constant(Value::Null),
            };
            exprs.push(self.checked_cast_to(expr, &column.ty, false)?);
            names.push(self.plan.intern(&column.name));
        }
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        Ok(self.add_node(Node::Project { input: node, index, exprs, names }))
    }

    /// The columns `conform` left `before` with as `after`, which are the columns of the
    /// projection it added or, when it added none, the columns of the side as they were.
    fn conformed(
        &self,
        before: NodeRef,
        after: NodeRef,
        scope: &Scope,
        merged: &[Merged],
        left: bool,
    ) -> Vec<ColumnBinding> {
        if before != after
            && let Node::Project { index, .. } = *self.plan.node(after)
        {
            return (0..merged.len()).map(|at| ColumnBinding::new(index, at as u32)).collect();
        }
        merged
            .iter()
            .enumerate()
            .map(|(at, column)| {
                let picked = if left { column.left } else { column.right };
                scope.columns[picked.unwrap_or(at)].binding
            })
            .collect()
    }

    // ----------------------------------------------------------------- select

    /// The expression the alias `word` was written for, bound where the alias is named.
    pub(crate) fn bind_alias(
        &mut self,
        ast: &Ast,
        word: &str,
        (at, expr, target): (usize, ast::ExprRef, usize),
        scope: &Scope,
    ) -> Result<ExprRef> {
        let Some(aliases) = &mut self.aliases else {
            unreachable!("an alias was found without any")
        };
        if aliases.clause == AliasClause::Select {
            if target >= aliases.defined {
                return Err(Error::binder(format!(
                    "Column \"{word}\" referenced that exists in the SELECT clause - but this column cannot be referenced before it is defined"
                )));
            }
            if crate::columns::has_subquery(ast, expr) {
                return Err(Error::binder(format!(
                    "Alias \"{word}\" referenced in a SELECT clause - but the expression has a subquery. This is not yet supported."
                )));
            }
        }
        aliases.visiting.push(at);
        let bound = self.bind_expr(ast, expr, scope);
        if let Some(aliases) = &mut self.aliases {
            aliases.visiting.pop();
        }
        bound
    }

    /// Which clause the select block's aliases are being read from, if any.
    fn alias_clause(&mut self, clause: AliasClause) {
        if let Some(aliases) = &mut self.aliases {
            aliases.clause = clause;
        }
    }

    fn bind_select(
        &mut self,
        ast: &Ast,
        select: ast::SelectRef,
        query: &ast::Query,
    ) -> Result<(NodeRef, Scope)> {
        let written = ast.select(select);
        let unknowns_kept = std::mem::replace(&mut self.unknowns_kept, false);
        self.want_ascending |= !written.group_by.is_empty() || written.group_by_all;
        // A window belongs to the block that wrote it, and a block can be bound inside another one
        // without a subquery in between, so the outer block's runs are put aside for the duration
        // rather than left where a nested block would append to them.
        let outer_windows = std::mem::take(&mut self.windows);
        // The same for the unnests, which also run over this block's rows and nobody else's.
        let outer_unnests = std::mem::take(&mut self.unnests);
        let outer_unnest_index = self.unnest_index.take();
        let outer_unnest_here = std::mem::replace(&mut self.unnest_here, false);
        let outer_in_unnest = std::mem::replace(&mut self.in_unnest, false);
        let outer_unnest_grouping = self.unnest_grouping.take();
        let outer_grouped_unnests = std::mem::take(&mut self.grouped_unnests);
        // Same argument for the queries lifted over this block's grouping. They are recorded while
        // the select list is being bound and read until the sort keys are done, and a block bound
        // inside that stretch has its own set, so the outer block's is put aside rather than left
        // where the inner one would clear it.
        let outer_joined_above = std::mem::take(&mut self.joined_above);
        // A block reads its own aliases and not the ones of the block it sits in, which the pin
        // allows and this does not yet.
        let outer_aliases = self.aliases.take();
        let (mut node, input) = self.bind_from(ast, written.from)?;
        node = self.attach_scalar_subqueries(node);
        self.aliases = Some(Aliases::of(ast, &written));

        if written.filter != NONE {
            self.clause = "WHERE clause";
            self.alias_clause(AliasClause::Where);
            let predicate = if crate::columns::has_star(ast, written.filter) {
                self.bind_star_predicate(ast, written.filter, &input)?
            } else {
                let predicate = self.bind_expr(ast, written.filter, &input)?;
                self.as_boolean(ast, written.filter, predicate, "WHERE")?
            };
            node = self.attach_scalar_subqueries(node);
            node = self.add_node(Node::Filter { input: node, predicate });
        }
        self.alias_clause(AliasClause::None);

        let targets = ast.target_list(written.targets).to_vec();
        if targets.is_empty() && self.semantics.empty_targets() == EmptyTargets::Pin {
            return Err(Error::binder("a SELECT needs at least one expression to select"));
        }

        let group_items = self.group_items(ast, &written, &targets)?;
        let aggregating = !group_items.is_empty()
            || written.having != NONE
            || self.aggregates(ast, written.qualify)
            || targets.iter().any(|target| self.aggregates(ast, target.expr));
        if aggregating {
            self.clause = "GROUP BY clause";
            // An unnest in a grouping key runs under the grouping, over the rows of the `FROM`,
            // and makes the rows that are grouped. `SELECT unnest(tags) AS tag, count(*) ... GROUP
            // BY tag` counts the rows each tag appears in.
            self.unnest_here = true;
            let everything = groups_everything(ast, &written)?;
            self.unnest_grouping = Some(everything);
            let mut groups = Vec::with_capacity(group_items.len());
            for &(item, written) in &group_items {
                // `GROUP BY ALL` groups on what a star or a `COLUMNS` in the list stands for.
                if everything && let Some(expanded) = self.bind_star_each(ast, item, &input)? {
                    groups.extend(expanded);
                    continue;
                }
                let group = self.bind_expr(ast, item, &input)?;
                let ty = self.plan.expr_type(group).clone();
                self.sort_group_operators(&ty, false, ast.leftmost_span(written))?;
                self.check_sort_collation(group)?;
                groups.push(group);
            }
            self.unnest_here = false;
            self.unnest_grouping = None;
            let unnests = std::mem::take(&mut self.unnests);
            if let Some(index) = self.unnest_index.take() {
                node = self.plan_unnests(node, index, &unnests)?;
            }
            let index = self.fresh_index();
            let uncollated = self.collate_groups(&mut groups)?;
            self.aggregation = Some(Aggregation { index, groups, aggregates: Vec::new() });
            self.first_of_groups(uncollated)?;
        }

        // The queries this block's clauses wrote that are joined in above the grouping rather than
        // below it. TPC-H q11 is the case in a `HAVING`: `HAVING sum(ps_supplycost * ps_availqty) >
        // (SELECT sum(...))` compares one group's total against a total over the whole table, and
        // the second total is one row that has nothing to do with the groups. Joined underneath the
        // grouping it would be a column of every input row and the grouping rule would ask for it in
        // the GROUP BY, which is the complaint this used to make.
        let mut above = Vec::new();

        // The pin refuses a star in a `HAVING` while it expands the stars, which is before it binds
        // the select list and finds anything wrong with that.
        if written.having != NONE && crate::columns::has_star(ast, written.having) {
            return Err(Error::binder("STAR expression is not supported here"));
        }
        // `HAVING` and then `QUALIFY` are bound before the select list, which is the pin's order and
        // decides which of two mistakes is the one reported.
        let mut having = None;
        if written.having != NONE {
            self.clause = "HAVING clause";
            self.alias_clause(AliasClause::Having);
            let before = self.scalar_subqueries.len();
            let predicate = self.bind_expr(ast, written.having, &input)?;
            self.lift_over_aggregate(before, &mut above, &input)?;
            let predicate = self.over_aggregate(predicate, &input)?;
            having = Some(self.as_boolean(ast, written.having, predicate, "HAVING")?);
        }

        // `QUALIFY` filters the rows after the windows have run over them, so it is bound with the
        // windows allowed, and its filter and the queries it wrote go in above them. A column it
        // reads that is not grouped is reported after the select list's, as the pin does.
        let mut qualify = None;
        let mut over_windows = Vec::new();
        if written.qualify != NONE {
            if groups_everything(ast, &written)? {
                return Err(Error::binder(
                    "Combining QUALIFY with GROUP BY ALL is not supported yet",
                ));
            }
            self.clause = "QUALIFY clause";
            self.alias_clause(AliasClause::Qualify);
            let before = self.scalar_subqueries.len();
            qualify = Some(self.bind_expr(ast, written.qualify, &input)?);
            if self.aggregation.is_some() {
                self.lift_over_aggregate(before, &mut over_windows, &input)?;
            } else {
                over_windows = self.scalar_subqueries.split_off(before);
            }
        }

        self.clause = "SELECT clause";
        self.unnest_here = true;
        self.alias_clause(AliasClause::Select);
        self.unknowns_kept = unknowns_kept;
        let (mut exprs, mut names, mut origins) =
            self.bind_targets(ast, &targets, &input, &mut above)?;
        if self.passing.as_ref().is_some_and(|passing| passing.select == select) {
            self.pass_added(&input, &mut exprs, &mut names)?;
            origins.resize(exprs.len(), None);
        }
        self.unnest_here = false;
        let visible = exprs.len();
        self.aliases = outer_aliases;

        if let Some(predicate) = qualify {
            self.clause = "QUALIFY clause";
            let predicate = self.over_aggregate(predicate, &input)?;
            // Without a window the clause would be a `WHERE` or a `HAVING` written late.
            if self.windows.is_empty() {
                return Err(Error::binder(
                    "at least one window function must appear in the SELECT column or QUALIFY clause",
                ));
            }
            qualify = Some(self.as_boolean(ast, written.qualify, predicate, "QUALIFY")?);
        }

        // The projection's index has to exist before the sort keys are built, because a key is a
        // reference to a projected column even when the expression it sorts on is not selected.
        let project = self.fresh_index();
        let mut output = Scope::empty();
        for (at, (expr, name)) in exprs.iter().zip(&names).enumerate() {
            output.push(Visible {
                table: String::new(),
                name: name.clone(),
                binding: ColumnBinding::new(project, at as u32),
                ty: self.plan.expr_type(*expr).clone(),
                not_null: self.passes_through(*expr, &input),
                key: self.key_through(*expr, &input),
                default: self.through(*expr, &input).and_then(|column| column.default.clone()),
                origin: origins[at],
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }

        self.clause = "ORDER BY clause";
        let mut sorted = Vec::new();
        self.unnest_here = true;
        let mut keys = self.select_sort_keys(
            ast,
            query,
            &input,
            &output,
            project,
            &mut exprs,
            &mut names,
            &mut sorted,
            &mut above,
        )?;
        let extra = sorted.iter().any(|key| key.extra);
        // A column that is sorted on, or that `DISTINCT` compares, needs one collation.
        let compared = if written.distinct == Distinct::Yes { visible } else { 0 };
        let checked: Vec<usize> =
            (0..compared).chain(sorted.iter().map(|key| key.position)).collect();
        for at in checked {
            self.check_sort_collation(exprs[at])?;
        }
        self.unnest_here = outer_unnest_here;
        self.in_unnest = outer_in_unnest;
        self.unnest_grouping = outer_unnest_grouping;
        self.grouped_unnests = outer_grouped_unnests;
        self.joined_above = outer_joined_above;
        self.distinct_order(ast, written.distinct, &output, &sorted)?;
        let mut on = self.distinct_on(ast, written.distinct, &output)?;
        self.distinct_operators(ast, written.distinct, &targets, &exprs[..visible], &on)?;
        // A plain DISTINCT sorted on something it does not select is, as on the pin, a DISTINCT ON
        // what it selects, keeping the first row of each.
        if extra && written.distinct == Distinct::Yes {
            for column in &output.columns[..visible] {
                let (binding, ty) = (column.binding, column.ty.clone());
                on.push(self.plan.add_expr(Expr::Column(binding), ty));
            }
        }

        node = self.attach_scalar_subqueries(node);
        self.check_recursive_aggregates(ast, &written)?;

        // What PostgreSQL numbers before the keys of the windows, which is read before the grouping
        // is taken. See `crate::windoworder`.
        let postgres_windows =
            self.semantics.window_order() == WindowOrder::Postgres && self.windows.len() > 1;
        let numbered = if postgres_windows {
            let sorted = sorted.iter().map(|key| exprs[key.position]).collect::<Vec<_>>();
            self.numbered_before_windows(sorted, &written.distinct, &exprs[..visible])
        } else {
            Vec::new()
        };
        if let Some(aggregation) = self.aggregation.take() {
            let index = aggregation.index;
            let groups = self.plan.add_expr_list(&aggregation.groups);
            let aggregates = self.plan.add_expr_list(&aggregation.aggregates);
            node = self.add_node(Node::Aggregate { input: node, index, groups, aggregates });
        }
        if !above.is_empty() {
            debug_assert!(self.scalar_subqueries.is_empty(), "a query is waiting to be joined");
            self.scalar_subqueries = above;
            node = self.attach_scalar_subqueries(node);
        }
        if let Some(predicate) = having {
            node = self.add_node(Node::Filter { input: node, predicate });
        }

        // After the grouping and after `HAVING`, which is where the reference binary puts it:
        // `SELECT j, sum(count(i)) OVER () FROM t GROUP BY j HAVING count(i) > 1` totals only the
        // groups that survived the filter.
        let mut runs = std::mem::replace(&mut self.windows, outer_windows);
        if postgres_windows {
            runs = self.postgres_window_order(runs, &numbered);
        }
        for run in runs {
            let partition = self.plan.add_expr_list(&run.partition);
            let order = self.plan.add_sort_keys(&run.order);
            let expressions = self.plan.add_expr_list(&run.calls);
            node = self.add_node(Node::Window {
                input: node,
                index: run.index,
                partition,
                order,
                frame: run.frame,
                expressions,
            });
        }
        if !over_windows.is_empty() {
            let below = std::mem::replace(&mut self.scalar_subqueries, over_windows);
            node = self.attach_scalar_subqueries(node);
            self.scalar_subqueries = below;
        }
        if let Some(predicate) = qualify {
            node = self.add_node(Node::Filter { input: node, predicate });
        }
        // After the windows, which is also the pin's order: `SELECT unnest([1, 2]), count(*) OVER
        // ()` counts one row and then makes two of it.
        let unnests = std::mem::replace(&mut self.unnests, outer_unnests);
        if let Some(index) = std::mem::replace(&mut self.unnest_index, outer_unnest_index) {
            node = self.plan_unnests(node, index, &unnests)?;
        }

        let interned: Vec<u32> = names.iter().map(|name| self.plan.intern(name)).collect();
        let exprs_slice = self.plan.add_expr_list(&exprs);
        let names_slice = self.plan.add_name_list(&interned);
        node = self.add_node(Node::Project {
            input: node,
            index: project,
            exprs: exprs_slice,
            names: names_slice,
        });

        if written.distinct != Distinct::No {
            // `DISTINCT ON` keeps the first row of each key in the order of the `ORDER BY`, in
            // PostgreSQL and on the pin. The distinct keeps the first row it reads and gives the rows
            // it keeps in the order it read them, so the sort goes below it and no sort is needed
            // above it. A plain `DISTINCT` keeps whole rows, and which of two equal rows it keeps
            // makes no difference.
            if !on.is_empty() && !keys.is_empty() {
                let keys = self.plan.add_sort_keys(&std::mem::take(&mut keys));
                node = self.add_node(Node::Sort { input: node, keys });
            }
            let on = self.collate_distinct(on, &output.columns[..visible])?;
            let on = self.plan.add_expr_list(&on);
            node = self.add_node(Node::Distinct { input: node, on });
        }
        if !keys.is_empty() {
            let keys = self.plan.add_sort_keys(&keys);
            node = self.add_node(Node::Sort { input: node, keys });
        }
        node = self.apply_limit(ast, query, node, &mut output)?;

        if !extra {
            output.columns.truncate(visible);
            return Ok((node, output));
        }
        // An expression sorted on but not selected was carried this far to make the sort possible,
        // and now it goes, because the query did not ask for it.
        let index = self.fresh_index();
        let mut kept = Vec::with_capacity(visible);
        let mut kept_names = Vec::with_capacity(visible);
        let mut scope = Scope::empty();
        for (at, name) in names.iter().enumerate().take(visible) {
            let ty = output.columns[at].ty.clone();
            // Through the scope rather than through `project`, because a limit that had a query
            // joined in under it put a projection of its own over the top and these columns are
            // that projection's now.
            let binding = output.columns[at].binding;
            kept.push(self.plan.add_expr(Expr::Column(binding), ty.clone()));
            kept_names.push(self.plan.intern(name));
            scope.push(Visible {
                table: String::new(),
                name: name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty,
                not_null: output.columns[at].not_null,
                key: output.columns[at].key,
                default: output.columns[at].default.clone(),
                origin: output.columns[at].origin,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        let exprs = self.plan.add_expr_list(&kept);
        let names = self.plan.add_name_list(&kept_names);
        node = self.add_node(Node::Project { input: node, index, exprs, names });
        Ok((node, scope))
    }

    /// Binds the target list, expanding every star into the columns it stands for.
    /// Moves the queries a clause just wrote from under this block's grouping to over it.
    ///
    /// A query written in a select list, a `HAVING` or an `ORDER BY` is one row that has nothing to
    /// do with the groups, so it belongs on top of the grouping and not underneath it. Underneath,
    /// its column is a column of every row going into the aggregate, which the grouping rule then
    /// asks for in the `GROUP BY`, and the aggregate carries nothing but its groups and its
    /// aggregates upward, so the projection could not read the column even if the rule let it
    /// through. That is both halves of #1027.
    ///
    /// A correlated one goes over the grouping too when what it correlates to is a column the block
    /// groups by, which is [`Self::lift_correlated`], and stays underneath when it is not. One
    /// written inside an aggregate call stays underneath whatever it correlates to, since that is
    /// read once per row going into the aggregate and lifting it over would put it where the
    /// aggregate that reads it cannot.
    ///
    /// `before` is what [`Self::scalar_subqueries`] held before the clause was bound, so only the
    /// queries that clause wrote are considered.
    fn lift_over_aggregate(
        &mut self,
        before: usize,
        above: &mut Vec<PendingSubquery>,
        scope: &Scope,
    ) -> Result<()> {
        if self.aggregation.is_none() {
            return Ok(());
        }
        let mut lifted = Vec::new();
        for mut pending in self.scalar_subqueries.split_off(before) {
            let stays = pending.inside_aggregate
                || (pending.dependent && !self.lift_correlated(&mut pending));
            if stays {
                self.scalar_subqueries.push(pending);
            } else {
                self.joined_above.push(pending.index);
                lifted.push(pending);
            }
        }
        // A mark join carries its comparison rather than the expression carrying it, and that
        // comparison is written over the outer rows, so it needs the same rewrite the expression
        // gets. It is done in a second pass so that a comparison reading another query lifted by
        // the same clause finds that query's index already recorded.
        for pending in &mut lifted {
            let conditions = std::mem::take(&mut pending.conditions);
            let mut over = Vec::with_capacity(conditions.len());
            for condition in conditions {
                over.push(self.over_aggregate(condition, scope)?);
            }
            pending.conditions = over;
        }
        above.append(&mut lifted);
        Ok(())
    }

    /// Moves one correlated query over this block's grouping, if the grouping lets it.
    ///
    /// It does when every outer column the query reads is a column this block groups by. That value
    /// is the group's own column above the aggregate, the same value read from a different operator,
    /// so the query can be joined against the groups instead of against the rows going into them,
    /// and what the query answers per group is what it answered per row of a group since every row
    /// of a group agreed on it. The rewrite is the references inside the query's body, which were
    /// bound against the table underneath and have to read the aggregate's output instead.
    ///
    /// A correlation on a column that is neither grouped nor aggregated is a different question with
    /// a different answer and there is nothing above the grouping that holds it, so that query stays
    /// where it is and [`Self::over_aggregate`] reports it as the missing `GROUP BY` it is. That is
    /// #1032.
    ///
    /// The query stays a dependent join either way. What changed is which operator the outer rows
    /// come from, not that there are any.
    fn lift_correlated(&mut self, pending: &mut PendingSubquery) -> bool {
        let Some(index) = self.aggregation.as_ref().map(|aggregation| aggregation.index) else {
            return false;
        };
        let mut moved = Vec::with_capacity(pending.reads.len());
        for read in &pending.reads {
            let Some(at) = self.group_of(*read) else {
                return false;
            };
            moved.push((*read, ColumnBinding::new(index, at as u32)));
        }
        let mut rewrites = Vec::new();
        self.plan.subtree_columns(pending.node, &mut |reference, binding| {
            if let Some(&(_, to)) = moved.iter().find(|(from, _)| *from == binding) {
                rewrites.push((reference, to));
            }
        });
        for (reference, to) in rewrites {
            self.plan.rebind(reference, to);
        }
        pending.reads = moved.into_iter().map(|(_, to)| to).collect();
        true
    }

    fn bind_targets(
        &mut self,
        ast: &Ast,
        targets: &[ast::Target],
        input: &Scope,
        above: &mut Vec<PendingSubquery>,
    ) -> Result<Targets> {
        let unknowns_kept = std::mem::replace(&mut self.unknowns_kept, false);
        let mut exprs = Vec::with_capacity(targets.len());
        let mut names = Vec::with_capacity(targets.len());
        // The table column of each target that is a plain column, taken before the target is moved
        // over an aggregate, so a column of `GROUP BY` keeps it the way PostgreSQL keeps it.
        let mut origins = Vec::with_capacity(targets.len());
        for (at, target) in targets.iter().enumerate() {
            origins.resize(exprs.len(), None);
            if let Some(aliases) = &mut self.aliases {
                aliases.defined = at;
            }
            // `(r).*` is a column for each field of `r`, named by the fields whatever the alias.
            if let ast::Expr::Fields { record } = ast.expr(target.expr) {
                let before = self.scalar_subqueries.len();
                let fields = self.bind_fields(ast, record, input)?;
                self.lift_over_aggregate(before, above, input)?;
                for (expr, name) in fields {
                    exprs.push(self.over_aggregate(expr, input)?);
                    names.push(name);
                }
                continue;
            }
            if let Some(picks) = self.star_like(ast, target.expr, input)? {
                let alias = (target.alias != NONE).then(|| ast.string(target.alias));
                for picked in &picks.entries {
                    self.star_entry = Some(picked.clone());
                    let expr = self.bind_picked(ast, input);
                    self.star_entry = None;
                    let expr = expr?;
                    origins.push(self.through(expr, input).and_then(|column| column.origin));
                    exprs.push(self.over_aggregate(expr, input)?);
                    names.push(picks.name(picked, alias)?);
                }
                continue;
            }
            match crate::columns::find_star(ast, target.expr)? {
                None => {}
                Some(crate::columns::Found::Star) => {
                    for picked in self.star_columns(ast, target.expr, input)? {
                        // A replacement takes the column's place and its position.
                        let before = self.scalar_subqueries.len();
                        origins.push(picked.column.origin.filter(|_| picked.replacement == NONE));
                        let expr = if picked.replacement == NONE {
                            self.plan
                                .add_expr(Expr::Column(picked.column.binding), picked.column.ty)
                        } else {
                            self.bind_expr(ast, picked.replacement, input)?
                        };
                        self.lift_over_aggregate(before, above, input)?;
                        exprs.push(self.over_aggregate(expr, input)?);
                        names.push(picked.name);
                    }
                    continue;
                }
                Some(crate::columns::Found::Columns(columns)) => {
                    let picks = self.columns_picks(ast, columns, input)?;
                    let alias = (target.alias != NONE).then(|| ast.string(target.alias));
                    for picked in &picks.entries {
                        let before = self.scalar_subqueries.len();
                        self.star_entry = Some(picked.clone());
                        let expr = self.bind_expr(ast, target.expr, input);
                        self.star_entry = None;
                        let expr = expr?;
                        self.lift_over_aggregate(before, above, input)?;
                        origins.push(self.through(expr, input).and_then(|column| column.origin));
                        exprs.push(self.over_aggregate(expr, input)?);
                        names.push(picks.name(picked, alias)?);
                    }
                    continue;
                }
                Some(crate::columns::Found::Unpacked(columns)) => {
                    if matches!(ast.expr(target.expr), ast::Expr::Columns { .. }) {
                        return Err(Error::binder(
                            "*COLUMNS not allowed at the root level, use COLUMNS instead",
                        ));
                    }
                    let picks = self.columns_picks(ast, columns, input)?;
                    let copy = self.unpacked(ast, target.expr, &picks);
                    let before = self.scalar_subqueries.len();
                    let expr = self.bind_expr(&copy, target.expr, input)?;
                    self.lift_over_aggregate(before, above, input)?;
                    exprs.push(self.over_aggregate(expr, input)?);
                    names.push(if target.alias == NONE {
                        self.target_name(&copy, target.expr, input)
                    } else {
                        ast.string(target.alias).to_string()
                    });
                    continue;
                }
            }
            let before = self.scalar_subqueries.len();
            self.unnest_root = matches!(ast.expr(target.expr), ast::Expr::Function { name, .. }
                if name.len == 1 && same_name(ast.name(name).last().unwrap_or_default(), "unnest"));
            let expr = self.bind_expr(ast, target.expr, input);
            self.unnest_root = false;
            let mut expr = expr?;
            // PostgreSQL makes a result column of a parameter of no type a `text`.
            if self.semantics.unknown_types() == UnknownTypes::Postgres
                && !unknowns_kept
                && self.is_placeholder(expr)
            {
                expr = self.cast_to(expr, &LogicalType::Varchar);
            }
            self.lift_over_aggregate(before, above, input)?;
            if let Some(taking) = self.unnest_struct.take() {
                // A struct is a column per field, named by the fields whatever the target's alias.
                let expr = self.over_aggregate(expr, input)?;
                self.unnest_fields(expr, taking, None, &mut exprs, &mut names)?;
                continue;
            }
            // A cast that changes nothing, such as `b::text` of a `VARCHAR`, binds to the column
            // itself, and PostgreSQL gives it no table column, so the written target decides.
            let plain = matches!(
                ast.expr(target.expr),
                ast::Expr::Column { .. } | ast::Expr::Positional { .. }
            );
            let origin = self.through(expr, input).and_then(|column| column.origin);
            origins.push(match ast.expr(target.expr) {
                // A bare `current_user` is a one-part column to the parser, so this comes first.
                _ if origin.is_none() && crate::context::gives_name(ast, target.expr) => {
                    Some(Origin::typed(NAME))
                }
                // An advisory lock function such as `pg_advisory_lock` gives `void`.
                _ if self.semantics.function_rules() == FunctionRules::Postgres
                    && crate::advisory::gives_void(ast, target.expr) =>
                {
                    Some(Origin::typed(VOID))
                }
                _ if plain => origin,
                // A parameter is of the type that the client declared for it, as in PostgreSQL.
                ast::Expr::Parameter { name } => {
                    self.parameters.declared_type(ast.string(name)).map(Origin::typed)
                }
                // A cast keeps the type it wrote, with the typmod, and is no table column.
                ast::Expr::Cast { ty, .. } => {
                    rudb_pgtypes::declared_type(ast.string(ty)).map(Origin::typed)
                }
                // A `COLLATE` keeps the type of what it is written on and is no table column.
                ast::Expr::Binary { op: ast::BinaryOp::Collate, .. } => {
                    match ast.expr(crate::expr::uncollated(ast, target.expr)) {
                        ast::Expr::Cast { ty, .. } => {
                            rudb_pgtypes::declared_type(ast.string(ty)).map(Origin::typed)
                        }
                        ast::Expr::Column { .. } => {
                            origin.and_then(|origin| origin.ty).map(Origin::typed)
                        }
                        _ => None,
                    }
                }
                _ => None,
            });
            exprs.push(self.over_aggregate(expr, input)?);
            names.push(if target.alias == NONE {
                self.target_name(ast, target.expr, input)
            } else {
                ast.string(target.alias).to_string()
            });
        }
        if exprs.is_empty() && self.semantics.empty_targets() == EmptyTargets::Pin {
            return Err(Error::binder("SELECT list is empty after resolving * expressions!"));
        }
        origins.resize(exprs.len(), None);
        Ok((exprs, names, origins))
    }

    /// The name an unaliased target gets.
    ///
    /// A bare column keeps the spelling the table was created with rather than the spelling the
    /// query used, so `SELECT USERID FROM hits` has a column called `UserID`. Identifiers match
    /// without regard to case and the catalog is the one that holds the case.
    pub(crate) fn output_name(&self, ast: &Ast, target: ast::ExprRef, input: &Scope) -> String {
        if let ast::Expr::Column { name } = ast.expr(target) {
            let parts: Vec<&str> = ast.name(name).collect();
            let compare = self.semantics.identifier_compare();
            if let Ok(found) = input.resolve(compare, &parts) {
                // A column found by its second name is headed by that name, so `t.range` over
                // `range(2) t` is a column called `range` on the pin while `SELECT *` calls it `t`.
                let written = parts.last().copied().unwrap_or_default();
                if let Some(also) = &found.also
                    && !compare.same(&found.name, written)
                    && compare.same(also, written)
                {
                    return also.clone();
                }
                return found.name.clone();
            }
            return rudb_parse::quoted(parts.last().copied().unwrap_or_default());
        }
        if let ast::Expr::Positional { index } = ast.expr(target)
            && let Ok(found) = input.positional(index)
        {
            return found.name.clone();
        }
        describe(ast, target, self.semantics)
    }

    /// The expressions a `GROUP BY` clause names, with positions and output aliases followed,
    /// each with the item as written, where an error about the key is placed.
    fn group_items(
        &self,
        ast: &Ast,
        select: &ast::Select,
        targets: &[ast::Target],
    ) -> Result<Vec<(ast::ExprRef, ast::ExprRef)>> {
        if groups_everything(ast, select)? {
            // GROUP BY ALL means every target that is not itself an aggregate, which is the set
            // that would otherwise have to be written out again by hand.
            return Ok(targets
                .iter()
                .filter(|target| !self.aggregates(ast, target.expr))
                .map(|target| (target.expr, target.expr))
                .collect());
        }
        let mut items = Vec::new();
        for &item in ast.expr_list(select.group_by) {
            let named = self.output_reference(ast, item, targets, "GROUP BY")?;
            items.push((named.unwrap_or(item), item));
        }
        Ok(items)
    }

    /// The target a `GROUP BY` or `ORDER BY` term names, when it names one by position or alias.
    fn output_reference(
        &self,
        ast: &Ast,
        item: ast::ExprRef,
        targets: &[ast::Target],
        clause: &str,
    ) -> Result<Option<ast::ExprRef>> {
        match ast.expr(item) {
            ast::Expr::Literal { kind: LiteralKind::Number, text } => {
                let written = ast.string(text);
                let position: usize = written.parse().map_err(|_| {
                    Error::binder(format!("{clause} term {written} is not a column"))
                })?;
                if position == 0 || position > targets.len() {
                    return Err(Error::binder(format!(
                        "{clause} term out of range - should be between 1 and {}",
                        targets.len()
                    ))
                    .state(SqlState::INVALID_COLUMN_REFERENCE)
                    .pg(format!("{clause} position {position} is not in select list"))
                    .with_span(ast.expr_span(item)));
                }
                Ok(Some(targets[position - 1].expr))
            }
            ast::Expr::Column { name } => {
                let parts: Vec<&str> = ast.name(name).collect();
                let [written] = parts.as_slice() else { return Ok(None) };
                let mut found = None;
                for target in targets {
                    if target.alias != NONE
                        && self
                            .semantics
                            .identifier_compare()
                            .same(ast.string(target.alias), written)
                    {
                        if found.is_some() {
                            return Ok(None);
                        }
                        found = Some(target.expr);
                    }
                }
                Ok(found)
            }
            _ => Ok(None),
        }
    }

    // -------------------------------------------------------------- modifiers

    /// Sort keys for a select, projecting anything sorted on that is not already selected.
    #[allow(clippy::too_many_arguments)]
    fn select_sort_keys(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        input: &Scope,
        output: &Scope,
        project: u32,
        exprs: &mut Vec<ExprRef>,
        names: &mut Vec<String>,
        sorted: &mut Vec<Sorted>,
        above: &mut Vec<PendingSubquery>,
    ) -> Result<Vec<SortKey>> {
        if query.order_by_all {
            return self.every_column(ast, query, output, Some(&exprs[..]));
        }
        let items = ast.order_list(query.order_by).to_vec();
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            let (item, ordinal) = self.ordinal_collation(ast, item, output);
            if let Some(expanded) = self.bind_star_each(ast, item.expr, input)? {
                for bound in expanded {
                    let bound = self.over_aggregate(bound, input)?;
                    let held = exprs.iter().position(|&held| self.same_expr(held, bound));
                    let position = held.unwrap_or_else(|| {
                        exprs.push(bound);
                        names.push(describe(ast, item.expr, self.semantics));
                        exprs.len() - 1
                    });
                    sorted.push(Sorted { position, written: item.expr, extra: held.is_none() });
                    let ty = self.plan.expr_type(exprs[position]).clone();
                    let item = self.sort_operators(ast, item, &ty)?;
                    let expr = self.column(project, position, ty);
                    let expr = self.collate_key(expr, exprs[position])?;
                    keys.push(self.sort_key(expr, item));
                }
                continue;
            }
            self.check_order_literal(ast, item.expr)?;
            let (position, extra) = match self.output_position(ast, item.expr, output)? {
                Some(position) => (position, false),
                None => {
                    let before = self.scalar_subqueries.len();
                    let bound = self.bind_expr(ast, item.expr, input)?;
                    self.lift_over_aggregate(before, above, input)?;
                    let bound = self.over_aggregate(bound, input)?;
                    match exprs.iter().position(|&held| self.same_expr(held, bound)) {
                        Some(position) => (position, false),
                        None => {
                            exprs.push(bound);
                            names.push(describe(ast, item.expr, self.semantics));
                            (exprs.len() - 1, true)
                        }
                    }
                }
            };
            sorted.push(Sorted { position, written: item.expr, extra });
            let ty = self.plan.expr_type(exprs[position]).clone();
            let item = self.sort_operators(ast, item, &ty)?;
            let expr = self.column(project, position, ty);
            let expr = self.order_key(expr, exprs[position], ordinal.as_deref())?;
            keys.push(self.sort_key(expr, item));
        }
        Ok(keys)
    }

    /// Sort keys over an output that has nothing behind it to project, which is a set operation.
    fn sort_keys(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        output: &Scope,
        targets: &[ast::Target],
    ) -> Result<Vec<SortKey>> {
        if query.order_by_all {
            return self.every_column(ast, query, output, None);
        }
        let items = ast.order_list(query.order_by).to_vec();
        let mut keys = Vec::with_capacity(items.len());
        for item in items {
            let (item, ordinal) = self.ordinal_collation(ast, item, output);
            if let Some(expanded) = self.bind_star_each(ast, item.expr, output)? {
                for expr in expanded {
                    let ty = self.plan.expr_type(expr).clone();
                    let item = self.sort_operators(ast, item, &ty)?;
                    let expr = self.collate_key(expr, expr)?;
                    keys.push(self.sort_key(expr, item));
                }
                continue;
            }
            self.check_order_literal(ast, item.expr)?;
            let expr = match self.output_position(ast, item.expr, output)? {
                Some(position) => {
                    let column = &output.columns[position];
                    let (binding, ty) = (column.binding, column.ty.clone());
                    self.plan.add_expr(Expr::Column(binding), ty)
                }
                None => {
                    let _ = targets;
                    self.bind_expr(ast, item.expr, output)?
                }
            };
            let ty = self.plan.expr_type(expr).clone();
            let item = self.sort_operators(ast, item, &ty)?;
            let expr = self.order_key(expr, expr, ordinal.as_deref())?;
            keys.push(self.sort_key(expr, item));
        }
        Ok(keys)
    }

    /// The sort keys of `ORDER BY ALL`, one for each output column, each under the collation of
    /// the select list entry in `from` it was made from when there is one.
    fn every_column(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        output: &Scope,
        from: Option<&[ExprRef]>,
    ) -> Result<Vec<SortKey>> {
        // `ORDER BY ALL DESC` is one item with no expression, which carries the direction and the
        // null placement for every column.
        let written = ast.order_list(query.order_by).first().copied();
        let columns: Vec<(ColumnBinding, LogicalType)> =
            output.columns.iter().map(|column| (column.binding, column.ty.clone())).collect();
        let mut keys = Vec::with_capacity(columns.len());
        for (position, (binding, ty)) in columns.into_iter().enumerate() {
            let expr = self.plan.add_expr(Expr::Column(binding), ty);
            let made = from.and_then(|from| from.get(position).copied()).unwrap_or(expr);
            let expr = self.collate_key(expr, made)?;
            if let Some(item) = written {
                keys.push(self.sort_key(expr, item));
                continue;
            }
            let expr = self.by_position(expr);
            let descending = self.semantics.default_descending();
            keys.push(SortKey {
                expr,
                descending,
                nulls_first: self.semantics.nulls_first(descending),
            });
        }
        Ok(keys)
    }

    /// Whether a sort direction is descending, with the session default for none. `USING op` is
    /// made `ASC` or `DESC` by [`Binder::sort_operators`] once the type of the key is known.
    pub(crate) fn descending(&self, order: Order) -> bool {
        match order {
            Order::Unstated => self.semantics.default_descending(),
            Order::Ascending => false,
            Order::Descending => true,
            Order::Using { .. } => unreachable!("sort_operators resolves USING before a sort"),
        }
    }

    /// A sort key with the session defaults filled in.
    fn sort_key(&mut self, expr: ExprRef, item: ast::OrderItem) -> SortKey {
        let expr = self.by_position(expr);
        let descending = self.descending(item.order);
        let nulls_first = match item.nulls {
            Nulls::First => true,
            Nulls::Last => false,
            Nulls::Unstated => self.semantics.nulls_first(descending),
        };
        SortKey { expr, descending, nulls_first }
    }

    /// Which output column a term names, by position or by name.
    fn output_position(
        &self,
        ast: &Ast,
        item: ast::ExprRef,
        output: &Scope,
    ) -> Result<Option<usize>> {
        match ast.expr(item) {
            ast::Expr::Literal { kind: LiteralKind::Number, text } => {
                let written = ast.string(text);
                if written.contains(['.', 'e', 'E']) {
                    return Ok(None);
                }
                let position: usize = written.parse().map_err(|_| {
                    Error::binder(format!("ORDER BY term {written} is not a column"))
                })?;
                Self::output_ordinal(ast, item, position, output).map(Some)
            }
            // `#2` in an `ORDER BY` is the second column of the select list, not of the `FROM`
            // clause, as the pin's `OrderBinder` reads it.
            ast::Expr::Positional { index } => {
                let position = usize::try_from(index).unwrap_or(usize::MAX);
                Self::output_ordinal(ast, item, position, output).map(Some)
            }
            ast::Expr::Column { name } => {
                let parts: Vec<&str> = ast.name(name).collect();
                let [written] = parts.as_slice() else { return Ok(None) };
                Ok(output.position_of(self.semantics.identifier_compare(), None, written))
            }
            _ => Ok(None),
        }
    }

    /// The place in `output` of the column at `position`, counted from one.
    fn output_ordinal(
        ast: &Ast,
        item: ast::ExprRef,
        position: usize,
        output: &Scope,
    ) -> Result<usize> {
        if position == 0 || position > output.len() {
            return Err(Error::binder(format!(
                "ORDER term out of range - should be between 1 and {}",
                output.len()
            ))
            .state(SqlState::INVALID_COLUMN_REFERENCE)
            .pg(format!("ORDER BY position {position} is not in select list"))
            .with_span(ast.expr_span(item)));
        }
        Ok(position - 1)
    }

    /// Refuses a literal sort key unless the session explicitly accepts its no-op behavior.
    fn check_order_literal(&self, ast: &Ast, item: ast::ExprRef) -> Result<()> {
        if !self.semantics.order_by_non_integer_literal()
            && matches!(
                ast.expr(item),
                ast::Expr::Literal { kind, text }
                    if kind != LiteralKind::Number
                        || ast.string(text).contains(['.', 'e', 'E'])
            )
        {
            return Err(Error::binder(
                "ORDER BY non-integer literal has no effect.\n* SET order_by_non_integer_literal=true to allow this behavior.",
            ));
        }
        Ok(())
    }

    /// The rule of [`DistinctOrder::Postgres`] for what a `SELECT DISTINCT` sorts on, as
    /// `transformDistinctClause` and `transformDistinctOnClause` check it. A plain `DISTINCT`
    /// sorts only on the columns that it selects. The `ORDER BY` of a `DISTINCT ON` starts with
    /// the expressions of the `DISTINCT ON`, in any order, and the error is at the first one that
    /// comes after a key that is not one of them.
    fn distinct_order(
        &self,
        ast: &Ast,
        distinct: Distinct,
        output: &Scope,
        sorted: &[Sorted],
    ) -> Result<()> {
        if self.semantics.distinct_order() != DistinctOrder::Postgres {
            return Ok(());
        }
        let misplaced = |written: ast::ExprRef, message: &str| {
            Err(Error::binder(message.to_string())
                .state(SqlState::INVALID_COLUMN_REFERENCE)
                .with_span(ast.leftmost_span(written)))
        };
        let items = match distinct {
            Distinct::No => return Ok(()),
            Distinct::Yes => {
                return match sorted.iter().find(|key| key.extra) {
                    Some(key) => misplaced(
                        key.written,
                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                    ),
                    None => Ok(()),
                };
            }
            Distinct::On(items) => ast.expr_list(items),
        };
        let mut on = Vec::with_capacity(items.len());
        for &item in items {
            if let Some(position) = self.output_position(ast, item, output)? {
                on.push((position, item));
            }
        }
        let message = "SELECT DISTINCT ON expressions must match initial ORDER BY expressions";
        // A key that sorts on a column a second time is no key, as `addTargetToSortList` drops it.
        let mut keys: Vec<usize> = Vec::with_capacity(sorted.len());
        let mut skipped = false;
        for key in sorted {
            if keys.contains(&key.position) {
                continue;
            }
            keys.push(key.position);
            match on.iter().find(|(position, _)| *position == key.position) {
                Some(&(_, item)) if skipped => return misplaced(item, message),
                Some(_) => {}
                None => skipped = true,
            }
        }
        if skipped
            && let Some(&(_, item)) = on.iter().find(|(position, _)| !keys.contains(position))
        {
            return misplaced(item, message);
        }
        Ok(())
    }

    /// The rule of [`SortOperators::Postgres`] for a key, as `get_sort_group_operators` checks
    /// it: a key that sorts needs the ordering operator of its type, and each key needs the
    /// equality operator. The operators come from the default btree or hash operator class of
    /// the type, so a type with none, such as `json`, is error 42883 at `span`. A type that
    /// PostgreSQL does not have is left to the pin.
    pub(crate) fn sort_group_operators(
        &self,
        ty: &LogicalType,
        sorts: bool,
        span: Span,
    ) -> Result<()> {
        if self.semantics.sort_operators() != SortOperators::Postgres {
            return Ok(());
        }
        let Some(oid) = crate::pgcalls::exact_oid(ty) else {
            return Ok(());
        };
        let name = rudb_pgtypes::format_type(oid);
        let error = |missing: &str| {
            Error::binder(format!("could not identify {missing} operator for type {name}"))
                .state(SqlState::UNDEFINED_FUNCTION)
                .with_span(span)
        };
        if sorts && !rudb_pgtypes::has_ordering(oid) {
            return Err(
                error("an ordering").hint("Use an explicit ordering operator or modify the query.")
            );
        }
        if !rudb_pgtypes::has_equality(oid) {
            return Err(error("an equality"));
        }
        Ok(())
    }

    /// The rule of [`SortOperators::Postgres`] for the keys of a `DISTINCT`, as
    /// `transformDistinctClause` and `transformDistinctOnClause` check them: each column that a
    /// plain `DISTINCT` selects, or each expression of a `DISTINCT ON`, needs the equality
    /// operator of its type. `exprs` are the columns that the query selects and `on` the bound
    /// expressions of a `DISTINCT ON`.
    fn distinct_operators(
        &self,
        ast: &Ast,
        distinct: Distinct,
        targets: &[ast::Target],
        exprs: &[ExprRef],
        on: &[ExprRef],
    ) -> Result<()> {
        let keys: Vec<(ExprRef, ast::ExprRef)> = match distinct {
            Distinct::No => return Ok(()),
            // A star selects more than one column, and its columns are placed at the star.
            Distinct::Yes => {
                let written = |at: usize| match targets.len() == exprs.len() {
                    true => targets[at].expr,
                    false => targets.first().map_or(NONE, |target| target.expr),
                };
                exprs.iter().enumerate().map(|(at, &expr)| (expr, written(at))).collect()
            }
            Distinct::On(items) => {
                on.iter().copied().zip(ast.expr_list(items).iter().copied()).collect()
            }
        };
        for (expr, written) in keys {
            let ty = self.plan.expr_type(expr).clone();
            self.sort_group_operators(&ty, false, ast.leftmost_span(written))?;
        }
        Ok(())
    }

    /// The rule of [`SortOperators::Postgres`] for the arguments of a `DISTINCT` aggregate, as
    /// `transformAggregateCall` checks them: each one needs the equality operator of its type,
    /// and then the ordering operator, because the aggregate sorts its inputs to find the values
    /// that are the same.
    pub(crate) fn distinct_aggregate_operators(
        &self,
        ast: &Ast,
        args: &[ast::ExprRef],
        bound: &[ExprRef],
    ) -> Result<()> {
        for (&arg, &written) in bound.iter().zip(args) {
            let ty = self.plan.expr_type(arg).clone();
            self.sort_group_operators(&ty, false, ast.leftmost_span(written))?;
        }
        if self.semantics.sort_operators() != SortOperators::Postgres {
            return Ok(());
        }
        for (&arg, &written) in bound.iter().zip(args) {
            let ty = self.plan.expr_type(arg);
            if let Some(oid) = crate::pgcalls::exact_oid(ty)
                && !rudb_pgtypes::has_ordering(oid)
            {
                return Err(Error::binder(format!(
                    "could not identify an ordering operator for type {}",
                    rudb_pgtypes::format_type(oid)
                ))
                .state(SqlState::UNDEFINED_FUNCTION)
                .detail("Aggregates with DISTINCT must be able to sort their inputs.")
                .with_span(ast.leftmost_span(written)));
            }
        }
        Ok(())
    }

    /// The expressions a `DISTINCT ON` names, which have to be columns of the output.
    fn distinct_on(
        &mut self,
        ast: &Ast,
        distinct: Distinct,
        output: &Scope,
    ) -> Result<Vec<ExprRef>> {
        let Distinct::On(items) = distinct else {
            return Ok(Vec::new());
        };
        let items = ast.expr_list(items).to_vec();
        let mut on = Vec::with_capacity(items.len());
        for item in items {
            let Some(position) = self.output_position(ast, item, output)? else {
                return Err(Error::not_implemented(
                    "DISTINCT ON an expression that is not in the select list",
                ));
            };
            let column = &output.columns[position];
            let (binding, ty) = (column.binding, column.ty.clone());
            on.push(self.plan.add_expr(Expr::Column(binding), ty));
        }
        Ok(on)
    }

    /// The `LIMIT` and the `OFFSET`, over the rows everything else in the query produced.
    ///
    /// The scope is taken by reference because a limit the binder could not work out reads its
    /// number off a query joined in underneath, and that join puts a column in the rows which the
    /// query did not ask for. A projection over the limit drops it again, and the scope has to say
    /// so, since its bindings are what anything above this reads.
    fn apply_limit(
        &mut self,
        ast: &Ast,
        query: &ast::Query,
        input: NodeRef,
        scope: &mut Scope,
    ) -> Result<NodeRef> {
        let waiting = self.scalar_subqueries.len();
        if query.limit_percent {
            let percent = self.share(ast, query.limit)?;
            let offset = self.skipped(ast, query.offset)?;
            let node = |binder: &mut Self, input| match percent {
                Some(percent) => binder.add_node(Node::LimitPercent { input, percent, offset }),
                // A null share is no limit at all, the same as a null row count, so what is left
                // is whatever the offset asked for.
                None => binder.limited(input, Bound::All, offset),
            };
            return self.over_subqueries(waiting, input, scope, node);
        }
        let count = self.count_bound(ast, query.limit, "LIMIT")?;
        let offset = self.skipped(ast, query.offset)?;
        let node = |binder: &mut Self, input| binder.limited(input, count, offset);
        self.over_subqueries(waiting, input, scope, node)
    }

    /// The offset a query wrote, as nought rows skipped when it wrote none.
    ///
    /// An offset the query left off is nought rows skipped, where a limit it left off is every row
    /// emitted, so the two clauses read the same word differently.
    fn skipped(&mut self, ast: &Ast, written: ast::ExprRef) -> Result<Bound> {
        Ok(match self.count_bound(ast, written, "OFFSET")? {
            Bound::All => Bound::Rows(0),
            named => named,
        })
    }

    /// Builds a limit node over `input`, joining in whatever queries its bounds turned out to need.
    ///
    /// A bound the binder could not work out reads its number off a column, and that column comes
    /// from a query joined in underneath. The join puts a column in the rows nobody asked for, so a
    /// projection over the limit drops it again and the scope is told to read that projection. When
    /// no query had to be joined in there is nothing to drop and the limit stands on its own.
    fn over_subqueries(
        &mut self,
        waiting: usize,
        input: NodeRef,
        scope: &mut Scope,
        node: impl FnOnce(&mut Self, NodeRef) -> NodeRef,
    ) -> Result<NodeRef> {
        let joined = self.scalar_subqueries.split_off(waiting);
        if joined.is_empty() {
            return Ok(node(self, input));
        }
        let mut input = input;
        for pending in joined {
            input = self.attach_subquery(input, pending);
        }
        let limit = node(self, input);
        Ok(self.reproject(limit, scope))
    }

    /// A row count limit over `input`, or `input` itself when neither half of the clause asks for
    /// anything.
    fn limited(&mut self, input: NodeRef, count: Bound, offset: Bound) -> NodeRef {
        if count == Bound::All && offset == Bound::Rows(0) {
            return input;
        }
        self.add_node(Node::Limit { input, count, offset })
    }

    /// A projection over `node` handing back exactly the columns `scope` names.
    ///
    /// The scope's bindings are rewritten to this projection's, because its columns are the ones
    /// anything above reads. Only a limit that had a query joined in under it wants this, and only
    /// because there is not always a projection above to drop the column that join added.
    fn reproject(&mut self, node: NodeRef, scope: &mut Scope) -> NodeRef {
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.columns.len());
        let mut names = Vec::with_capacity(scope.columns.len());
        for column in &scope.columns {
            exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan.intern(&column.name));
        }
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
        }
        self.carry_rowids(scope, 0..usize::MAX, index, &mut exprs, &mut names);
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        self.add_node(Node::Project { input: node, index, exprs, names })
    }

    /// The share of the input a `LIMIT n PERCENT` names.
    ///
    /// The same evaluation as a row count and a different type at the end of it: the value is cast
    /// to `DOUBLE` rather than to `BIGINT`, so `LIMIT '30'%` is thirty percent and `LIMIT true%` is
    /// one percent, which is what the pin answers. A null is no limit at all.
    ///
    /// The range is checked here because the pin checks it here. `LIMIT 101 PERCENT` fails an
    /// `EXPLAIN` on the pinned binary, so it is refused while the query is planned and not when it
    /// is run, and a `NAN` is outside the range like any other value that is not between nought and
    /// a hundred.
    ///
    /// What the binder cannot work out is a subquery and a call that answers differently every
    /// time, the same two things a row count cannot work out, and those become a [`Share::Read`]
    /// over the expression. The value is checked where it turns up instead, which is the executor.
    /// Only the sign can be written that way, because the grammar refuses `PERCENT` after a closing
    /// bracket, but nothing below here depends on which of the two was typed.
    fn share(&mut self, ast: &Ast, written: ast::ExprRef) -> Result<Option<Share>> {
        if written == NONE {
            return Ok(None);
        }
        self.clause = "LIMIT clause";
        let scope = Scope::empty();
        let bound = self.bind_expr(ast, written, &scope)?;
        let Some(value) = fold::value_of(&self.plan, bound)? else {
            return Ok(Some(Share::Read(bound)));
        };
        if value.is_null() {
            return Ok(None);
        }
        let percent = percentage(&value)?;
        if !(0.0..=100.0).contains(&percent) {
            return Err(Error::out_of_range(
                "Limit percent out of range, should be between 0% and 100%",
            ));
        }
        Ok(Some(Share::Percent(percent)))
    }

    /// The row count a `LIMIT` or an `OFFSET` names.
    ///
    /// It does not have to be a literal. Anything whose value is settled before the first row is
    /// read will do, so `LIMIT 1 + 1` and `LIMIT CAST(3 AS BIGINT)` are both two, and that is what
    /// the pin does with them: its binder evaluates the expression and writes the number down.
    ///
    /// What is left over is an expression the binder cannot settle, which is a subquery, because it
    /// has to run first, and a call that answers differently every time it is made, such as
    /// `RANDOM()` or `nextval`. Those become a [`Bound::Read`] holding the expression, and the
    /// number comes off the first chunk that reaches the limit. The pin takes both and answers them
    /// the same way.
    ///
    /// The value is cast to `BIGINT` whatever it was written as, which is the whole of the type
    /// rule. `LIMIT '3'` is three rows because the string converts, `LIMIT 2.5` is three rows
    /// because the conversion rounds, `LIMIT true` is one row, and `LIMIT DATE '2020-01-01'` is the
    /// cast refusing a date. Every one of those messages is the cast's own, which is why there is
    /// no type check here to write a worse one. A limit that is read while the query runs is cast
    /// the same way by the operator that reads it, so the two paths answer alike.
    fn count_bound(&mut self, ast: &Ast, written: ast::ExprRef, clause: &str) -> Result<Bound> {
        if written == NONE {
            return Ok(Bound::All);
        }
        self.clause = if clause == "OFFSET" { "OFFSET clause" } else { "LIMIT clause" };
        let scope = Scope::empty();
        let bound = self.bind_expr(ast, written, &scope)?;
        // PostgreSQL casts the count to `bigint`, and a parameter takes that type.
        if self.semantics.unknown_types() == UnknownTypes::Postgres {
            self.resolve_placeholder(bound, &LogicalType::BigInt);
        }
        let bound = self.as_count(ast, written, bound, clause)?;
        let Some(value) = fold::value_of(&self.plan, bound)? else {
            return Ok(Bound::Read(bound));
        };
        // A null is no limit at all, the same as leaving the clause off, and the pin agrees:
        // `LIMIT NULL` and `LIMIT CAST(NULL AS INTEGER)` both answer every row.
        if value.is_null() {
            return Ok(Bound::All);
        }
        let texts = self.semantics.error_texts() == ErrorTexts::Postgres;
        row_count(&value, clause, texts).map(Bound::Rows)
    }

    // ------------------------------------------------------------------- from

    /// Refuses an aggregate in a block that reads, in its own `FROM`, a recursive definition whose
    /// recursive side is being bound. PostgreSQL makes this check last of its checks on the
    /// grouping, as `parseCheckAggregates` in `parse_agg.c` does, and gives the place of the first
    /// aggregate. A read inside a subquery of the `FROM` is a block of its own and does not count.
    fn check_recursive_aggregates(&self, ast: &Ast, written: &ast::Select) -> Result<()> {
        if self.semantics.recursive_union() != RecursiveUnion::Postgres
            || self.recursing.is_empty()
            || self.aggregation.as_ref().is_none_or(|held| held.aggregates.is_empty())
        {
            return Ok(());
        }
        fn reads(ast: &Ast, source: ast::SourceRef, recursing: &[u32]) -> bool {
            match ast.source(source) {
                ast::Source::Cte { cte, .. } => recursing.contains(&cte),
                ast::Source::Join { left, right, .. } => {
                    reads(ast, left, recursing) || reads(ast, right, recursing)
                }
                _ => false,
            }
        }
        if !ast.source_list(written.from).iter().any(|&source| reads(ast, source, &self.recursing))
        {
            return Ok(());
        }
        let first = ast
            .target_list(written.targets)
            .iter()
            .map(|target| target.expr)
            .chain([written.having])
            .find_map(|expr| crate::expr::first_aggregate(ast, expr, &|_| false));
        let error = Error::binder(
            "aggregate functions are not allowed in a recursive query's recursive term",
        )
        .state(SqlState::INVALID_RECURSION);
        Err(match first {
            Some(call) => error.with_span(ast.expr_span(call)),
            None => error.unplaced(),
        })
    }

    fn bind_from(&mut self, ast: &Ast, from: ast::Slice) -> Result<(NodeRef, Scope)> {
        let sources = ast.source_list(from).to_vec();
        let Some((first, rest)) = sources.split_first() else {
            // No FROM clause is one row of no columns, which is what SELECT 1 sits on. Not an
            // empty table: an empty table would make SELECT 1 return nothing.
            return Ok((self.add_node(Node::Dummy), Scope::empty()));
        };
        let (mut node, mut scope) = self.bind_source(ast, *first)?;
        for (at, source) in rest.iter().enumerate() {
            let (right, right_scope, correlations) = self.bind_lateral(ast, *source, &scope)?;
            self.distinct_table_names(ast, &sources[..=at], *source)?;
            node = if correlations.is_empty() {
                self.add_node(Node::CrossProduct { left: node, right })
            } else {
                let conditions = self.plan.add_expr_list(&[]);
                self.add_node(Node::DependentJoin {
                    left: node,
                    right,
                    kind: JoinKind::Inner,
                    conditions,
                })
            };
            scope = scope.concat(right_scope);
        }
        Ok((node, scope))
    }

    /// Binds one FROM entry with everything written to its left already visible.
    ///
    /// That is what LATERAL means, and it is what a comma separated FROM does here whether the word
    /// was written or not, because the pinned build resolves `FROM o, (SELECT o.k + 1)` without it.
    /// The keyword therefore changes nothing and is accepted rather than acted on.
    ///
    /// The columns of the left that the entry read come back with it, and an entry that read none
    /// is an ordinary product. The rest are somebody else's: a name that resolved past the left
    /// neighbours belongs to an enclosing query, so it is handed up to whichever frame is waiting
    /// for it rather than counted here, or the subquery this FROM sits in would lose track of its
    /// own correlation.
    fn bind_lateral(
        &mut self,
        ast: &Ast,
        source: ast::SourceRef,
        left: &Scope,
    ) -> Result<(NodeRef, Scope, Vec<ColumnBinding>)> {
        self.lateral_scopes.push(self.outer_scopes.len());
        self.outer_scopes.push(left.clone());
        self.correlations.push(Vec::new());
        let bound = self.bind_source(ast, source);
        let read = self.correlations.pop().expect("correlation frame");
        self.outer_scopes.pop();
        self.lateral_scopes.pop();
        let (node, scope) = bound?;

        let mut here = Vec::new();
        for binding in read {
            if left.holds(binding) {
                here.push(binding);
            } else if let Some(enclosing) = self.correlations.last_mut()
                && !enclosing.contains(&binding)
            {
                enclosing.push(binding);
            }
        }
        // A table function is allowed to read the left the same as anything else here. There is
        // nothing underneath one for the domain to be pushed into, since its arguments are what
        // produce its rows, so the unnesting pass turns it into a `LateralFunction` and the call is
        // made once per domain value. That is `domain.rs`.
        //
        // Nothing has to be turned down here for the functions that would not survive it. The only
        // table functions taking an argument that is not a name are the series family, `unnest` and
        // the two document walks, which are what that operator answers, and a name that is not a constant is refused where the
        // columns are settled, because settling them means opening the file or reading the catalog.
        Ok((node, scope, here))
    }

    pub(crate) fn bind_source(
        &mut self,
        ast: &Ast,
        source: ast::SourceRef,
    ) -> Result<(NodeRef, Scope)> {
        let (node, mut scope) = self.bind_one_source(ast, source)?;
        // A join is made of sources that went through here on their own, and a table already has
        // its row number to count by.
        let joined = matches!(ast.source(source), ast::Source::Join { .. });
        if joined || !scope.rowids.is_empty() || !self.counts_relations(ast) {
            return Ok((node, scope));
        }
        Ok((self.mark_rows(node, &mut scope), scope))
    }

    fn bind_one_source(&mut self, ast: &Ast, source: ast::SourceRef) -> Result<(NodeRef, Scope)> {
        match ast.source(source) {
            // An error about the table points at its name, as it does in both dialects.
            ast::Source::Table { name, alias, columns } => self
                .bind_table(ast, name, alias, columns)
                .map_err(|error| error.with_fallback_span(ast.source_span(source))),
            ast::Source::Function { name, args, alias, columns, pragma } => {
                let ordinality = ast.with_ordinality(source);
                if let Some(call) = self.value_call(ast, source, name) {
                    return self.bind_value_source(ast, &[call], alias, columns, ordinality);
                }
                self.bind_table_function(ast, name, args, alias, columns, pragma, ordinality)
            }
            ast::Source::Subquery { query, alias, columns } => {
                let (node, mut scope) = self.bind_query(ast, query)?;
                let label = if alias == NONE {
                    "unnamed_subquery".to_string()
                } else {
                    ast.string(alias).to_string()
                };
                scope.relabel(&label);
                if !columns.is_empty() {
                    let names: Vec<&str> = ast.name(columns).collect();
                    scope.rename(&names, &label)?;
                }
                Ok((node, scope))
            }
            ast::Source::Calls { calls, alias, columns } => {
                let calls = ast.expr_list(calls).to_vec();
                let ordinality = ast.with_ordinality(source);
                self.bind_value_source(ast, &calls, alias, columns, ordinality)
            }
            ast::Source::Values { rows, alias, columns } => {
                let bare = ast::Query::bare(ast::QueryBody::Values(rows));
                let (node, mut scope) = self.bind_values(ast, &bare, rows)?;
                let label =
                    if alias == NONE { String::new() } else { ast.string(alias).to_string() };
                scope.relabel(&label);
                if !columns.is_empty() {
                    let names: Vec<&str> = ast.name(columns).collect();
                    scope.rename(&names, &label)?;
                }
                Ok((node, scope))
            }
            ast::Source::Cte { cte, alias, columns, recurring } => {
                self.bind_cte_scan(ast, cte, alias, columns, recurring)
            }
            ast::Source::Join { left, right, kind, natural, on, using } => {
                self.bind_join(ast, left, right, kind, natural, on, using)
            }
            ast::Source::Pivot { pivot } => self.bind_pivot(ast, pivot),
        }
    }

    /// The check of `checkNameSpaceConflicts` in a PostgreSQL session: an item of `FROM`, bound
    /// after the items `held`, has no name that one of them has. This is the rule for the items
    /// of one `FROM` and for the two sides of a join.
    fn distinct_table_names(
        &self,
        ast: &Ast,
        held: &[ast::SourceRef],
        added: ast::SourceRef,
    ) -> Result<()> {
        if self.semantics.table_names() != TableNames::Postgres {
            return Ok(());
        }
        let mut before = Vec::new();
        for &source in held {
            self.table_names(ast, source, &mut before);
        }
        let mut after = Vec::new();
        self.table_names(ast, added, &mut after);
        for (name, table) in &after {
            let twice = before.iter().any(|(other, other_table)| {
                // Two tables with no alias can be two different tables of one name.
                name == other
                    && !matches!((table, other_table), (Some(one), Some(other)) if one != other)
            });
            if twice {
                return Err(Error::binder(format!(
                    "table name \"{name}\" specified more than once"
                ))
                .state(SqlState::DUPLICATE_ALIAS)
                .unplaced());
            }
        }
        Ok(())
    }

    /// The names that an item of `FROM` is reached by, the `refname` of PostgreSQL. Each one has the
    /// table that it names with it when the item is a table with no alias. A join has the names
    /// of both of its sides, and a subquery or a `VALUES` with no alias has none.
    fn table_names(
        &self,
        ast: &Ast,
        source: ast::SourceRef,
        names: &mut Vec<(String, Option<String>)>,
    ) {
        let alias = match ast.source(source) {
            ast::Source::Table { name, alias: NONE, .. } => {
                let parts: Vec<&str> = ast.name(name).collect();
                let table = match self.catalog.resolve(&parts) {
                    Ok(resolved) => {
                        format!("{}.{}.{}", resolved.catalog, resolved.schema, resolved.table)
                    }
                    Err(_) => parts.join("."),
                };
                if let Some(last) = parts.last() {
                    names.push(((*last).to_string(), Some(table)));
                }
                return;
            }
            ast::Source::Function { name, alias: NONE, .. } => {
                if let Some(last) = ast.name(name).last() {
                    names.push((last.to_string(), None));
                }
                return;
            }
            ast::Source::Cte { cte, alias: NONE, .. } => ast.cte(cte).name,
            ast::Source::Join { left, right, .. } => {
                self.table_names(ast, left, names);
                self.table_names(ast, right, names);
                return;
            }
            ast::Source::Pivot { pivot } => ast.pivot(pivot).alias,
            ast::Source::Table { alias, .. }
            | ast::Source::Function { alias, .. }
            | ast::Source::Calls { alias, .. }
            | ast::Source::Cte { alias, .. }
            | ast::Source::Subquery { alias, .. }
            | ast::Source::Values { alias, .. } => alias,
        };
        if alias != NONE {
            names.push((ast.string(alias).to_string(), None));
        }
    }

    /// A read of a materialised `WITH`, which is a leaf the same way a table scan is.
    ///
    /// Which definition it reads was settled by the parser, so there is no name to look up here and
    /// no shadowing left to think about. What is looked up is the materialisation that definition
    /// turned into, and the search runs backwards because the same definition is bound again for
    /// each use of a plain `WITH` it sits inside, and a read means the innermost of those.
    fn bind_cte_scan(
        &mut self,
        ast: &Ast,
        written: u32,
        alias: ast::StrRef,
        columns: ast::Slice,
        recurring: bool,
    ) -> Result<(NodeRef, Scope)> {
        let Some(held) = self.materialized.iter().rev().find(|held| held.written == written) else {
            let name = ast.string(ast.cte(written).name);
            return Err(Error::binder(format!("Table with name {name} does not exist!")));
        };
        let cte = if recurring { held.recurring } else { held.cte };
        let fields = if recurring { held.finished.clone() } else { held.fields.clone() };
        let text = held.name.clone();
        let output = held.output;
        let label = if alias == NONE { text.clone() } else { ast.string(alias).to_string() };
        let name = self.plan.intern(&text);
        let index = self.fresh_index();
        if let Some(output) = output {
            self.collated.reads.insert(index, output);
        }
        // The right side of a definition with a `SEARCH` or `CYCLE` clause passes the added
        // columns of this read through.
        if let Some(passing) = &mut self.passing
            && passing.written == written
            && !recurring
        {
            passing.reads.push(index);
        }
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.clone(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: field.not_null,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        let columns = self.plan.add_fields(&fields);
        let node = self.add_node(Node::CteScan { index, cte, name, columns });
        Ok((node, scope))
    }

    fn bind_table(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let parts: Vec<&str> = ast.name(name).collect();
        // The source of a pivot, read by the query written for the pivot.
        if let [single] = parts[..]
            && let Some((node, mut scope)) = self.pivot_source(single)
        {
            let label =
                if alias == NONE { single.to_string() } else { ast.string(alias).to_string() };
            scope.relabel(&label);
            return Ok((node, scope));
        }
        // Rows the statement was handed under a name, which a trigger's body reads the rows that
        // fired it through, and which hide a table of the same name the way a `WITH` does.
        if let [single] = parts[..]
            && let Some(rows) = self.parameters.relation(single)
        {
            let rows = rows.clone();
            let label =
                if alias == NONE { single.to_string() } else { ast.string(alias).to_string() };
            return self.bind_rows(&rows, &label);
        }
        let catalog = self.catalog;
        // The catalog is asked first and the file is the fallback, which is the order DuckDB uses:
        // a table really called `mixed.parquet` wins over a file of that name sitting next to it.
        let resolved = match catalog.resolve(&parts) {
            Ok(resolved) => resolved,
            Err(missing) => {
                let missing = missing
                    .state(SqlState::UNDEFINED_TABLE)
                    .pg(format!("relation \"{}\" does not exist", parts.join(".")));
                return self.bind_replacement_scan(ast, &parts, alias, columns, missing);
            }
        };
        if catalog.entry(&resolved)? == Entry::View {
            return self.bind_view(ast, &resolved, alias, columns);
        }
        let label =
            if alias == NONE { resolved.table.clone() } else { ast.string(alias).to_string() };
        self.bind_catalog_table(ast, &resolved, label, columns)
    }

    /// A table the catalog holds, under the name `label`, which is where [`Self::bind_table`] ends
    /// and where a Parquet file with a native mirror goes instead of to its reader.
    pub(crate) fn bind_catalog_table(
        &mut self,
        ast: &Ast,
        resolved: &QualifiedName,
        label: String,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let catalog = self.catalog;
        let table = catalog.table(resolved)?;
        let fields: &[Field] = table.columns();
        // The new rows of an `ON CONFLICT DO UPDATE`, which the write puts in a table of their own
        // before it runs the query. Nothing the table knows about its own rows holds for them.
        let excluded = self.upsert && same_name(&label, "excluded");
        // `PRI` for a column of the primary key and `UNI` for one of a unique key, the primary key
        // winning where a column is in both.
        let mut marks = vec![None; fields.len()];
        for key in table.keys() {
            for &column in &key.columns {
                if key.primary || marks[column].is_none() {
                    marks[column] = Some(if key.primary { "PRI" } else { "UNI" });
                }
            }
        }
        // A table of no catalog, such as the held rows of `excluded`, is no table to a client.
        let origin = |at: usize| {
            let oid = table.oid();
            (oid != DETACHED && !excluded)
                .then(|| Origin::column(oid, at as u32, table.declared_type(at)))
        };
        let index = self.fresh_index();
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.clone(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: field.not_null,
                key: marks[at],
                default: table.default(at).map(str::to_owned),
                origin: origin(at),
                qualified: excluded,
                also: None,
                hidden: false,
                using: None,
            });
        }
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        if self.semantics.collations() == Collations::Pin {
            let collations: Vec<(u32, String)> = (0..fields.len())
                .filter_map(|at| table.collation(at).map(|name| (at as u32, name.to_string())))
                .collect();
            self.collated_columns(index, collations);
        }
        let resolved = if excluded { &QualifiedName::excluded() } else { resolved };
        let catalog_name = self.plan.intern(&resolved.catalog);
        let schema = self.plan.intern(&resolved.schema);
        let table_name = self.plan.intern(&resolved.table);
        let alias = self.plan.intern(&label);
        let columns = self.plan.add_fields(fields);
        // What the store wrote down about itself, against the table index the same way a Parquet
        // footer is. A table with nothing to say records nothing and the estimate falls back to the
        // constants it used before, which is what every table did until the file had a directory
        // worth asking.
        if let Some(zones) = table.rows().zones().filter(|_| !excluded) {
            self.plan.set_zones(index, zones);
        }
        if let Some(frequencies) = table.frequencies().filter(|_| !excluded) {
            self.plan.set_frequencies(index, frequencies);
        }
        // A stored table hands over what it gathered once for the whole plan to share. Only a table
        // whose rows can change is asked column by column.
        let facts = table.facts().filter(|_| !excluded);
        if let Some(facts) = facts {
            self.plan.set_facts(index, facts, self.want_ascending);
        } else if !excluded {
            for (column, distinct) in table.distincts() {
                self.plan.measure_distinct(index, &column, distinct);
            }
            if self.want_ascending {
                for column in table.ascending() {
                    self.plan.mark_ascending(index, &column);
                }
            }
            for (column, bytes) in table.widths() {
                self.plan.measure_width(index, &column, bytes);
            }
        }
        let node = self.add_node(Node::Get {
            catalog: catalog_name,
            schema,
            table: table_name,
            alias,
            index,
            columns,
        });
        // The row number a scan counts, which is what the pin's `rowid` is until a row is deleted.
        // The pin keeps the number of a row that is gone unused and the store here moves the rows
        // after it down, so after a `DELETE` the two count differently.
        let compare = self.semantics.identifier_compare();
        let unnumbered = std::mem::take(&mut self.unnumbered);
        if !excluded && !unnumbered {
            let shadowed = fields.iter().any(|field| compare.same(&field.name, ROWID));
            let column = Visible {
                table: label.clone(),
                name: ROWID.to_string(),
                binding: ColumnBinding::new(index, fields.len() as u32),
                ty: LogicalType::BigInt,
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            };
            // A column of the table's own called `rowid` wins the name, and the row number is
            // still what `count(t.*)` counts the table's rows by.
            if shadowed {
                scope.add_marker(column, node, 0);
            } else {
                scope.add_rowid(column, node, 0);
            }
        }
        Ok((node, scope))
    }

    /// Has the scan `node` produce its row number, as the column after its last, once a `rowid`
    /// reads it.
    pub(crate) fn number_scan(&mut self, node: NodeRef) {
        let Node::Get { columns, .. } = *self.plan.node(node) else { return };
        let mut fields = self.plan.field_list(columns).to_vec();
        if fields.last().is_some_and(|field| field.name == FILE_ROW_NUMBER) {
            return;
        }
        fields.push(Field::required(FILE_ROW_NUMBER.to_string(), LogicalType::BigInt));
        let widened = self.plan.add_fields(&fields);
        if let Node::Get { columns, .. } = self.plan.node_mut(node) {
            *columns = widened;
        }
    }

    /// Reads the `rowid` of each table placed in `places` through the projection `index` that is
    /// being built over its scan, after the expressions already in `exprs`. The scan numbers its
    /// rows from here on, since nothing can tell yet whether a name above reads them.
    pub(crate) fn carry_rowids(
        &mut self,
        scope: &mut Scope,
        places: std::ops::Range<usize>,
        index: u32,
        exprs: &mut Vec<ExprRef>,
        names: &mut Vec<rudb_plan::StrRef>,
    ) {
        let width = exprs.len();
        for (binding, scan) in scope.rowids_in(places.clone()) {
            self.number_scan(scan);
            exprs.push(self.plan.add_expr(Expr::Column(binding), LogicalType::BigInt));
            names.push(self.plan.intern(ROWID));
        }
        scope.move_rowids(places, index, width);
    }

    /// A view where a table goes, which is the body bound again right here.
    ///
    /// Inline and not behind a node. The view is gone by the time the plan exists, so everything
    /// downstream sees the query somebody would have written by hand, and the column pruning that
    /// makes `SELECT COUNT(*) FROM 'hits.parquet'` read no columns at all keeps working through
    /// `FROM hits`. A `Node::View` would be a barrier with nothing on the other side of it.
    ///
    /// The scope this builds is a subquery's, right down to the name in the error message. duckdb
    /// v1.5.1 reports a view whose column list has gone stale as `table "unnamed_subquery" has 1
    /// columns available but 2 columns specified`, which is the sentence its subquery alias rule
    /// produces, so a view there is a subquery with the view's name written over it afterwards.
    fn bind_view(
        &mut self,
        ast: &Ast,
        name: &QualifiedName,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let view = self.catalog.view(name)?;
        let full = name.to_string();
        if self.expanding.contains(&full) {
            // Two quotes each side, which is what the binary prints. It quotes the name on the way
            // in and then formats the quoted name into a quoted slot, so a view called `a` comes
            // back as `""a""`. That is upstream's wart and copying it is the whole job here.
            return Err(Error::binder(format!(
                "infinite recursion detected: attempting to recursively bind view \"\"{}\"\"",
                name.table
            )));
        }
        self.keep_statement(ast);
        let body = parse_ast_with_case(view.sql(), self.semantics.identifier_case())?;
        let query = match body.statements.as_slice() {
            [ast::Statement::Query(query)] => *query,
            // Only a query can have got past the binder at creation, so this is a view the catalog
            // was handed some other way rather than anything a statement can produce.
            _ => return Err(Error::binder(format!("view \"{}\" is not a query", name.table))),
        };
        self.expanding.push(full);
        let bound = self.bind_query(&body, query);
        self.expanding.pop();
        let (node, mut scope) = bound?;

        let aliases: Vec<&str> = view.aliases().iter().map(String::as_str).collect();
        if !aliases.is_empty() {
            scope.rename(&aliases, "unnamed_subquery")?;
        }
        // What the catalog tables report as this view's columns, written down here because this is
        // the moment they are known. Upstream refreshes the same cache at the same point, which was
        // measured: both `duckdb_columns()` and `duckdb_views().column_count` keep reporting the old
        // list after an `ALTER TABLE` underneath until something reads the view, and then both move.
        // It is written before the label and before the `AS t(a, b)` list below, because those two
        // rename the view for one query and not for everyone.
        view.remember(scope.fields());
        // PostgreSQL gives the column of a view as the origin, not the table column under it.
        let shown = scope.columns.iter_mut().filter(|column| !column.hidden);
        for (at, column) in shown.enumerate() {
            let ty = column.origin.and_then(|origin| origin.ty);
            column.origin =
                (view.oid() != DETACHED).then(|| Origin::column(view.oid(), at as u32, ty));
        }
        let label = if alias == NONE { name.table.clone() } else { ast.string(alias).to_string() };
        scope.relabel(&label);
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok((node, scope))
    }

    /// A function call where a table goes, such as `range(10)`.
    ///
    /// The arguments are bound against an empty scope. A table function that can see the row on its
    /// left is `LATERAL`, and this is not it, so a column name in here is not resolved against
    /// whatever happens to be to the left in the `FROM` list. Letting it would mean `FROM t,
    /// range(t.n)` quietly binding to something whose meaning depends on the order the sources were
    /// written in.
    #[allow(clippy::too_many_arguments)]
    fn bind_table_function(
        &mut self,
        ast: &Ast,
        name: ast::Slice,
        args: ast::Slice,
        alias: ast::StrRef,
        columns: ast::Slice,
        pragma: bool,
        ordinality: bool,
    ) -> Result<(NodeRef, Scope)> {
        // A call that is not made by one operator has no place for each row to number.
        let numbered = |bound: (NodeRef, Scope)| {
            if ordinality {
                let name: Vec<&str> = ast.name(name).collect();
                return Err(unnumbered(&name.join(".")));
            }
            Ok(bound)
        };
        // The column names written after the alias, kept under a name of their own because the
        // match on what the function's columns are below binds `columns` to something else.
        let renamed = columns;
        if !pragma && let Some(bound) = self.table_macro(ast, name, args, alias, columns)? {
            return numbered(bound);
        }
        let parts: Vec<&str> = ast.name(name).collect();
        // A qualified call names a schema, and the two schemas that exist are the ones every
        // built-in lives in. Anything else is a name that has to fail rather than fall through to
        // the unqualified lookup and be found somewhere it was not asked for.
        let function_name = *parts.last().unwrap_or(&"");
        if let Some(schema) = parts.iter().rev().nth(1)
            && !schema.eq_ignore_ascii_case("main")
            && !schema.eq_ignore_ascii_case("system")
        {
            return Err(Error::catalog(format!(
                "Table Function with name {} does not exist!",
                parts.join(".")
            )));
        }
        if !pragma
            && let Some(bound) = self.query_function(ast, function_name, args, alias, columns)?
        {
            return numbered(bound);
        }
        if !pragma && let Some(bound) = self.all_types(ast, function_name, args, alias, columns)? {
            return numbered(bound);
        }
        if !pragma
            && let Some(bound) = self.vector_types(ast, function_name, args, alias, columns)?
        {
            return numbered(bound);
        }
        // The name is looked up before the arguments are bound so that a call of something that is
        // not a table function says that, rather than reporting whatever is wrong with the
        // arguments of a function that was never going to exist.
        let Some(called) = TableFunction::lookup(function_name) else {
            if pragma {
                // `PRAGMA database_list` is a view upstream and not a function, and the pragma
                // namespace holds both, so a name that is not a function gets one more look in the
                // catalog before it is turned down. It has to be the no argument form: a view
                // takes none, and `pragma_database_list()` with parentheses is a missing function
                // on the pin too.
                if args.is_empty() && self.catalog.resolve(&parts).is_ok() {
                    return self.bind_table(ast, name, alias, columns);
                }
                let spelled = function_name.strip_prefix("pragma_").unwrap_or(function_name);
                return Err(Error::catalog(format!(
                    "Pragma Function with name {spelled} does not exist!"
                )));
            }
            return Err(Error::catalog(format!(
                "Table Function with name {function_name} does not exist!"
            )));
        };
        let written = ast.target_list(args).to_vec();
        let empty = Scope::empty();
        let waiting = self.scalar_subqueries.len();
        let previous = std::mem::replace(&mut self.clause, "table function arguments");
        let mut bound = Vec::new();
        let mut written_options = Vec::new();
        for argument in written {
            let identifiers =
                std::mem::replace(&mut self.identifiers_as_strings, called.takes_identifiers());
            let expr = self.bind_expr(ast, argument.expr, &empty);
            self.identifiers_as_strings = identifiers;
            let expr = expr?;
            if argument.alias == NONE {
                bound.push(expr);
            } else {
                let name = ast.string(argument.alias).to_string();
                written_options.push(self.named_argument(called, function_name, &name, expr)?);
            }
        }
        self.clause = previous;
        let options = Options::of(&written_options)?;

        // The types are what resolve the call, not the count, because `read_parquet(3)` is a
        // different answer from `read_parquet('3')` and only the types tell them apart.
        let integers = !pragma && self.postgres_series(function_name, &mut bound);
        let given: Vec<LogicalType> =
            bound.iter().map(|&expr| self.plan.expr_type(expr).clone()).collect();
        let resolved = if pragma {
            resolve_pragma(function_name, &given)?
        } else {
            resolve_table(function_name, &given)?
        };
        let mut cast: Vec<ExprRef> = bound
            .iter()
            .zip(&resolved.arguments)
            .map(|(&expr, ty)| self.checked_cast_to(expr, ty, false))
            .collect::<Result<_>>()?;

        if resolved.function.answered_when_bound() {
            let Columns::Fixed(fields) = resolved.columns else {
                return Err(Error::internal("a pragma that resolved to a file"));
            };
            let [argument] = cast[..] else {
                return Err(Error::internal("a pragma that resolved to more than one name"));
            };
            return self.bind_pragma(ast, resolved.function, &fields, argument, alias, columns);
        }
        // Filled in by the arm below that has the file names, and left alone by a function whose
        // columns are fixed, because none of those reads a file to find out how tall it is.
        let mut measured = Stat::Unknown;
        let mut counted: Vec<(String, Stat<u64>)> = Vec::new();
        let mut bounded: Option<Arc<dyn Zones>> = None;
        let fields = match resolved.columns {
            // The whole file readers take their patterns the way the file readers do, and like
            // them hand the executor one name per file, though their columns never change.
            Columns::Fixed(fields) if resolved.function.reads_contents() => {
                let paths = self.content_paths(cast[0], resolved.function.name())?;
                cast = paths.iter().map(|path| self.path_constant(path)).collect();
                fields
            }
            Columns::Fixed(fields) => fields,
            columns => {
                // The one argument is a pattern, and what replaces it is one constant per file it
                // matched. The executor is handed names rather than a pattern, so it never walks a
                // directory and the answer cannot change between binding a prepared statement and
                // running it, which is the same reason the schema is settled here.
                let empty_allowed = resolved.function.json().is_some() && {
                    let named: Vec<(&str, Value)> = written_options
                        .iter()
                        .map(|(parameter, value, _)| (*parameter, value.clone()))
                        .collect();
                    scan::allows_empty(&named)?
                };
                let paths = if resolved.function == TableFunction::ReadSingleJsonFile {
                    self.single_path(cast[0], resolved.function.name())?
                } else if empty_allowed {
                    self.paths_or_none(cast[0], resolved.function.name())?
                } else {
                    self.file_paths(cast[0], resolved.function.name())?
                };
                let mut mirrorable = None;
                if resolved.function == TableFunction::ReadParquet
                    && !options.file_row_number
                    && let Some((path, stamp)) = mirror_target(&paths)
                {
                    if let Some(name) = self.catalog.mirror(&path, options.binary_as_string, stamp)
                    {
                        let name = name.clone();
                        let label = if alias == NONE {
                            resolved.function.name().to_string()
                        } else {
                            ast.string(alias).to_string()
                        };
                        return self.bind_catalog_table(ast, &name, label, renamed);
                    }
                    mirrorable = Some(path);
                }
                let copy_into = match columns {
                    Columns::Csv | Columns::Json => self.copy_into.take(),
                    _ => None,
                };
                let mut fields = match columns {
                    Columns::Csv if copy_into.is_some() => {
                        let into = copy_into.unwrap_or_default();
                        self.copy_fields(&paths, &options.given, &into, &mut written_options)?
                    }
                    // Parquet takes the first file's footer as the answer and CSV sniffs all of
                    // them, which is not a choice made here. See `csv_fields`.
                    Columns::Csv => csv_fields(&paths, options.given.clone())?,
                    Columns::Json => {
                        if let Some(into) = copy_into {
                            self.copy_json_columns(&into, &mut written_options);
                        }
                        self.json_fields(resolved.function, &paths, &mut written_options)?
                    }
                    _ => {
                        let footers = self.footers(&paths, mirrorable.as_deref())?;
                        if let Some(path) = mirrorable.as_deref() {
                            self.want_mirror(path, options.binary_as_string, &footers.rows);
                        }
                        measured = footers.rows;
                        counted = footers.distincts;
                        bounded = footers.zones;
                        footers.fields
                    }
                };
                if options.all_varchar {
                    // The sniffer still ran, because the names come out of the same pass over the
                    // front of the file and only the types are being overruled. The executor reads
                    // the text as VARCHAR because this is the schema it is told to read into, which
                    // is the same road a file in a glob takes when the set is wider than the file.
                    for field in &mut fields {
                        field.ty = LogicalType::Varchar;
                    }
                }
                if options.binary_as_string {
                    // A byte array column with no annotation on it is a BLOB, and this is the caller
                    // saying that the file's writer meant text. The reader already holds both in the
                    // same string column and already validates the bytes, so the whole of the option
                    // is what the column is called from here on.
                    for field in &mut fields {
                        if field.ty == LogicalType::Blob {
                            field.ty = LogicalType::Varchar;
                        }
                    }
                }
                if options.file_row_number {
                    // Not a column of the file, so it goes on the end where a projection cannot be
                    // confused about which one it is, and the executor counts it as the rows come
                    // out. A file that already has a column of that name is the one case where the
                    // option cannot be honoured, and saying so is better than handing back two
                    // columns with the same name and letting a reference to it pick one.
                    if fields.iter().any(|field| field.name == FILE_ROW_NUMBER) {
                        return Err(Error::binder(format!(
                            "Duplicate column name \"{FILE_ROW_NUMBER}\": the file already has a \
                             column of that name, so file_row_number cannot add one"
                        )));
                    }
                    fields.push(Field::required(FILE_ROW_NUMBER.to_string(), LogicalType::BigInt));
                }
                cast = paths.iter().map(|path| self.path_constant(path)).collect();
                fields
            }
        };
        let label = if alias == NONE {
            resolved.function.name().to_string()
        } else {
            ast.string(alias).to_string()
        };
        // The column list names the number too, so it goes on once the number is there.
        let names: Vec<&str> = ast.name(columns).collect();
        let written = if ordinality { &[][..] } else { &names[..] };
        let (node, mut scope) = self.table_function_source(
            resolved.function,
            &cast,
            &written_options,
            Read { fields, rows: measured, distincts: counted, zones: bounded },
            &label,
            written,
        )?;
        let node = self.lateral_over_subqueries(node, waiting);
        if ordinality {
            self.number_rows(node, &mut scope)?;
        }
        let (node, mut scope) =
            if integers { self.integer_series(node, scope) } else { (node, scope) };
        if ordinality && !names.is_empty() {
            scope.rename(&names, &label)?;
        }
        Ok((node, scope))
    }

    /// A series or an unnest whose arguments read a query, `range((SELECT 3))`, as the same call
    /// made laterally over the one row that query makes.
    ///
    /// The query cannot be joined in above the call the way it is above a table, because the call
    /// is what reads it. So it is joined into a row with nothing in it, and the call runs over that
    /// row the way it runs over the rows of a table to its left.
    fn lateral_over_subqueries(&mut self, node: NodeRef, waiting: usize) -> NodeRef {
        if self.scalar_subqueries.len() <= waiting {
            return node;
        }
        let Node::TableFunction { index, function, args, options, settings, columns, ordinality } =
            self.plan.node(node).clone()
        else {
            return node;
        };
        let series = matches!(
            TableFunction::lookup(self.plan.string(function)),
            Some(
                TableFunction::Range
                    | TableFunction::GenerateSeries
                    | TableFunction::Unnest
                    | TableFunction::JsonEach
                    | TableFunction::JsonTree
            )
        );
        if !series {
            return node;
        }
        let mut input = self.add_node(Node::Dummy);
        for pending in self.scalar_subqueries.split_off(waiting) {
            input = self.attach_subquery(input, pending);
        }
        self.add_node(Node::LateralFunction {
            input,
            index,
            function,
            args,
            options,
            settings,
            columns,
            ordinality,
        })
    }

    /// The columns of a JSON read, detected from the files or given by `columns`.
    ///
    /// The files are read here the way the executor will read them, because the columns come out
    /// of a sample of their documents, and how the detection settled goes into the plan as a hidden
    /// setting. The executor needs it to know which key each column is read from, since two keys
    /// that differ only in case answer as two names, and to word its errors the way the pin does.
    fn json_fields(
        &mut self,
        function: TableFunction,
        paths: &[String],
        written: &mut Vec<(&'static str, Value, ExprRef)>,
    ) -> Result<Vec<Field>> {
        let Some(kind) = function.json() else {
            return Err(Error::internal("a JSON read of a function that is not one"));
        };
        let named: Vec<(&str, Value)> =
            written.iter().map(|(parameter, value, _)| (*parameter, value.clone())).collect();
        let catalog = self.catalog;
        let mut resolve = |text: &str| crate::statement::read_type(catalog, text);
        let options = scan::Options::parse(kind, &named, Some(&mut resolve))?;
        let mut load = |path: &str| json_text(path, options.compression);
        let (fields, settled) = scan::bind(&options, paths, &mut load)?;
        let value = Value::Varchar(settled.written());
        let reference = self.plan.add_value(value.clone());
        let expr = self.plan.add_expr(Expr::Constant(reference), LogicalType::Varchar);
        written.push((scan::SETTLED, value, expr));
        Ok(fields)
    }

    /// The columns of the `read_json` a `COPY t FROM` became, which are the table's, given to it
    /// as a `columns` parameter in place of any the statement wrote. A key the table has no column
    /// for is skipped and a column the object has no key for is null, which is the pin's answer.
    fn copy_json_columns(
        &mut self,
        into: &[Field],
        written: &mut Vec<(&'static str, Value, ExprRef)>,
    ) {
        written.retain(|(parameter, _, _)| *parameter != "columns");
        let value = Value::Struct(
            into.iter()
                .map(|field| (field.name.clone(), Value::Varchar(field.ty.to_string())))
                .collect(),
        );
        let ty = LogicalType::Struct(
            into.iter().map(|field| Field::new(field.name.clone(), LogicalType::Varchar)).collect(),
        );
        let reference = self.plan.add_value(value.clone());
        let expr = self.plan.add_expr(Expr::Constant(reference), ty);
        written.push(("columns", value, expr));
    }

    /// The columns of the `read_csv` a `COPY t FROM` became, which are the table's.
    ///
    /// The file is still sniffed, since the delimiter and whether the first line is a header are
    /// still the file's to say when the statement did not, and so is how many columns it has. That
    /// has to be how many the statement loads, and a file that disagrees gets the line of DuckDB's
    /// sniffer error that says so. The rest of that error is a list of fixes for a sniffer this one
    /// is not, and is left out.
    ///
    /// The names go into the plan as a `names` parameter and the flag that the types were set as
    /// `types_set`, because the executor opens the file again from what the plan says, and the
    /// columns it finds have to be the ones the plan was built against. The types need nothing,
    /// since the executor already reads a CSV file as the types the plan holds.
    fn copy_fields(
        &mut self,
        paths: &[String],
        given: &Given,
        into: &[Field],
        written: &mut Vec<(&'static str, Value, ExprRef)>,
    ) -> Result<Vec<Field>> {
        let sniffed = csv_fields(paths, given.clone())?;
        if sniffed.len() != into.len() {
            let set: Vec<String> =
                into.iter().map(|field| format!("'{}' : '{}'", field.name, field.ty)).collect();
            return Err(Error::invalid_input(format!(
                "Error when sniffing file \"{}\".\nIt was not possible to automatically detect the \
                 CSV parsing dialect\n* Columns are set as: \"columns = {{ {}}}\", and they \
                 contain: {} columns. It does not match the number of columns found by the \
                 sniffer: {}. Verify the columns parameter is correctly set.",
                paths.first().map_or("", String::as_str),
                set.join(", "),
                into.len(),
                sniffed.len()
            )));
        }
        let names: Vec<Value> =
            into.iter().map(|field| Value::Varchar(field.name.clone())).collect();
        let names = Value::List { element: LogicalType::Varchar, values: names };
        let list = LogicalType::List(Box::new(LogicalType::Varchar));
        for (parameter, value, ty) in
            [("names", names, list), (TYPES_SET, Value::Boolean(true), LogicalType::Boolean)]
        {
            let reference = self.plan.add_value(value.clone());
            let expr = self.plan.add_expr(Expr::Constant(reference), ty);
            written.push((parameter, value, expr));
        }
        Ok(into.to_vec())
    }

    /// `pragma_table_info('t')` or `pragma_show('t')`, answered while it is bound.
    ///
    /// The same trick `DESCRIBE` uses and for the same reason: the columns of a table are settled by
    /// the time the name has resolved, so the rows are a constant from there on and this comes out
    /// as a `VALUES` rather than as an operator that reads a catalog while the query runs. It also
    /// means `SELECT name FROM pragma_table_info('t') WHERE notnull` is an ordinary query over an
    /// ordinary relation, which is the whole reason these exist as functions rather than only as
    /// statements.
    ///
    /// The name arrives as a string rather than as something the parser read, so it is split here
    /// under the identifier rule and then resolved like any other name. A name that is not there
    /// comes back as the catalog's own complaint, which is what the pin answers with too.
    fn bind_pragma(
        &mut self,
        ast: &Ast,
        function: TableFunction,
        fields: &[Field],
        argument: ExprRef,
        alias: ast::StrRef,
        columns: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let written = self.pragma_name(argument, function)?;
        let parts = identifier_parts(&written);
        let spelled: Vec<&str> = parts.iter().map(String::as_str).collect();
        let name = self.catalog.resolve(&spelled)?;
        let described = self.described(ast, &name)?;
        // A view has neither defaults nor keys, and a generated column shows its expression.
        let table = (self.catalog.entry(&name)? == Entry::Table)
            .then(|| self.catalog.table(&name))
            .transpose()?;
        let mut rows = Vec::with_capacity(described.len());
        for (at, field) in described.iter().enumerate() {
            let items = if matches!(function, TableFunction::PragmaShow) {
                self.describing(field)
            } else {
                let default = table.and_then(|table| {
                    table.generation(at).or_else(|| table.default(at).map(str::to_owned))
                });
                let key = table.is_some_and(|table| {
                    table.keys().iter().any(|key| key.primary && key.columns.contains(&at))
                });
                self.table_info(at, field, default, key)
            };
            rows.push(self.plan.add_expr_list(&items));
        }
        let rows = self.plan.add_rows(&rows);
        let held = self.plan.add_fields(fields);
        let index = self.fresh_index();
        let node = self.add_node(Node::Values { index, columns: held, rows });
        let label =
            if alias == NONE { function.name().to_string() } else { ast.string(alias).to_string() };
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.clone(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        if !columns.is_empty() {
            let names: Vec<&str> = ast.name(columns).collect();
            scope.rename(&names, &label)?;
        }
        Ok((node, scope))
    }

    /// The name a pragma was called with, which has to be a constant.
    ///
    /// A null is a name spelled `NULL` rather than an error about nulls, because the pin turns
    /// whatever it was handed into text before it goes looking and then says a table of that name
    /// does not exist. Writing `pragma_table_info(NULL)` is a mistake either way and this is the
    /// sentence the mistake already has.
    ///
    /// `pragma_table_info('t' || 'x')` is the pin's `tx` and is turned away here, which is the same
    /// missing constant folding [`Binder::named_argument`] writes about and closes the same day.
    fn pragma_name(&self, argument: ExprRef, function: TableFunction) -> Result<String> {
        let Expr::Constant(reference) = *self.plan.expr(argument) else {
            return Err(Error::not_implemented(format!(
                "{}() given a name that is not a constant",
                function.name()
            )));
        };
        match self.plan.value(reference) {
            Value::Varchar(name) => Ok(name.clone()),
            Value::Null => Ok("NULL".to_string()),
            other => {
                Err(Error::internal(format!("a pragma name bound as VARCHAR arrived as {other}")))
            }
        }
    }

    /// The columns of whatever a pragma was pointed at.
    ///
    /// A view is bound here, which is how it comes to have columns at all. Reading a view is what
    /// binds it and describing one counts as reading it, so a view the engine ships with reports a
    /// column count from this point on, the same as it would after a select. The node that binding
    /// produces is thrown away, because the answer is the scope and not the query.
    ///
    /// Every column of a view is nullable whatever the column underneath was declared as, which is
    /// the pin's answer through `pragma_table_info()`, `pragma_show()` and `duckdb_columns()` alike.
    /// [`Scope::fields`] drops the flag on its own, so there is nothing to clear here.
    fn described(&mut self, ast: &Ast, name: &QualifiedName) -> Result<Vec<Field>> {
        if self.catalog.entry(name)? == Entry::Table {
            return Ok(self.catalog.table(name)?.columns().to_vec());
        }
        let (_, scope) = self.bind_view(ast, name, NONE, ast::Slice::default())?;
        Ok(scope.fields())
    }

    /// One row of `pragma_show()`, which is one row of `DESCRIBE` written by the other caller.
    fn describing(&mut self, field: &Field) -> Vec<ExprRef> {
        let written = [
            field.name.clone(),
            field.ty.to_string(),
            if field.not_null { "NO" } else { "YES" }.to_owned(),
        ];
        let mut items: Vec<ExprRef> =
            written.into_iter().map(|text| self.plan.add_constant(Value::Varchar(text))).collect();
        for _ in 0..3 {
            let empty = self.plan.add_constant(Value::Null);
            items.push(self.cast_to(empty, &LogicalType::Varchar));
        }
        items
    }

    /// One row of `pragma_table_info()`, which is SQLite's six columns about the same column.
    ///
    /// `cid` counts from zero, which is SQLite's numbering and not the one based `ordinal_position`
    /// the standard views report. `dflt_value` is the default as SQL, or a generated column's
    /// expression cast to its type, and `pk` is whether the column is in the primary key.
    fn table_info(
        &mut self,
        at: usize,
        field: &Field,
        default: Option<String>,
        key: bool,
    ) -> Vec<ExprRef> {
        let cid = self.plan.add_constant(Value::Integer(i32::try_from(at).unwrap_or(i32::MAX)));
        let name = self.plan.add_constant(Value::Varchar(field.name.clone()));
        let ty = self.plan.add_constant(Value::Varchar(field.ty.to_string()));
        let not_null = self.plan.add_constant(Value::Boolean(field.not_null));
        let default = self.plan.add_constant(default.map_or(Value::Null, Value::Varchar));
        let default = self.cast_to(default, &LogicalType::Varchar);
        let key = self.plan.add_constant(Value::Boolean(key));
        vec![cid, name, ty, not_null, default, key]
    }

    /// One named parameter of a table function call, folded into what the call was given.
    ///
    /// The value has to be a constant, because an option can decide what the columns are and the
    /// columns are settled here. It is folded and cast to the type the parameter wants, so
    /// `header=2` and `header='true'` are both true, as they are on the pin.
    ///
    /// A name that is not a parameter of this function is the binary's sentence followed by what it
    /// could have been. The binary puts the candidates on their own indented lines and this puts
    /// them on the same line, because an error is one line here.
    fn named_argument(
        &mut self,
        function: TableFunction,
        spelled: &str,
        name: &str,
        expr: ExprRef,
    ) -> Result<(&'static str, Value, ExprRef)> {
        // The JSON readers name the function as it was called, `_auto` and all, and list a
        // parameter that takes any value as `ANY`.
        let called = if function.json().is_some() {
            spelled.to_ascii_lowercase()
        } else {
            function.name().to_string()
        };
        let known = function
            .parameters()
            .iter()
            .find(|(parameter, _)| parameter.eq_ignore_ascii_case(name));
        let Some((parameter, wanted)) = known else {
            let candidates: Vec<String> = function
                .parameters()
                .iter()
                .map(|(parameter, ty)| {
                    if *ty == LogicalType::Null {
                        format!("    {parameter} ANY")
                    } else {
                        format!("    {parameter} {ty}")
                    }
                })
                .collect();
            // A function with no named parameters at all says so rather than listing none.
            if candidates.is_empty() {
                return Err(Error::binder(format!(
                    "Invalid named parameter \"{name}\" for function {called}\nFunction does not \
                     accept any named parameters."
                )));
            }
            return Err(Error::binder(format!(
                "Invalid named parameter \"{name}\" for function {called}\nCandidates:\n{}\n",
                candidates.join("\n")
            )));
        };
        // Folded rather than read off a literal, for the reason [`Binder::file_patterns`] gives: a
        // list is a call to `list_value`, and `nullstr = ['NA', '-']` has to arrive as a list.
        let Some(value) = fold::value_of(&self.plan, expr)? else {
            return Err(Error::not_implemented(format!(
                "the named parameter {parameter} with a value that is not a constant"
            )));
        };
        let shared =
            (function.json().is_some() && *parameter == "filename") || function.reads_contents();
        if value == Value::Null && shared {
            // The ones the shared file options refuse rather than the reader's own list.
            return Err(Error::invalid_input(format!(
                "Cannot use NULL as argument for \"{parameter}\""
            )));
        }
        if value == Value::Null {
            return Err(Error::binder(null_parameter(function, parameter)));
        }
        // The JSON readers cast what they are given to the parameter's type themselves, and say
        // so in their own words when it does not cast.
        if function.json().is_some() {
            return Ok((parameter, value, expr));
        }
        let given = self.plan.expr_type(expr).clone();
        // `nullstr` takes one string or a list of them, which is the one parameter so far that
        // takes two types, and the parameter table has room for one.
        let listed = *parameter == "nullstr" && given == LogicalType::list(LogicalType::Varchar);
        if *parameter == "nullstr" && given != *wanted && !listed {
            return Err(Error::binder(
                "CSV Reader function option \"nullstr\" requires a string or a list as input",
            ));
        }
        if given == *wanted || listed {
            return Ok((parameter, value, expr));
        }
        // Anything else is cast to the type the parameter wants, so `header=0` is false and
        // `delim=1` is the string `1`, as the pin casts it.
        match rudb_kernels::cast_value(&value, wanted, false) {
            Ok(value) => {
                let at = self.plan.add_value(value.clone());
                let cast = self.plan.add_expr(Expr::Constant(at), wanted.clone());
                Ok((parameter, value, cast))
            }
            Err(_) if self.copy_into.is_some() => Err(Error::invalid_input(format!(
                "Copy option \"{parameter}\" expected an argument of type {wanted} - the argument \
                 \"{value}\" of type {given} could not be cast as this type"
            ))),
            Err(error) => {
                Err(Error::invalid_input(format!("Failed to cast value: {}", error.message())))
            }
        }
    }

    /// A file where a table name goes, which is what DuckDB calls a replacement scan.
    ///
    /// `SELECT * FROM 'hits.parquet'` is how most DuckDB queries in the wild are written, ClickBench
    /// among them, so this is not sugar over `read_parquet` so much as the spelling people use. The
    /// catalog has already been asked and has already said no, and `missing` is what it said, so a
    /// name that is not a file comes back with the catalog's own answer rather than with a complaint
    /// about files.
    ///
    /// Only a single unqualified name is a candidate. A qualified one names a schema and a schema
    /// that does not exist is not a path.
    fn bind_replacement_scan(
        &mut self,
        ast: &Ast,
        parts: &[&str],
        alias: ast::StrRef,
        columns: ast::Slice,
        missing: Error,
    ) -> Result<(NodeRef, Scope)> {
        let [path] = parts else { return Err(missing) };
        let path = *path;
        let extension = path.rsplit_once('.').map(|(_, after)| after).unwrap_or_default();
        let Some(function) = Self::reader_for(extension) else {
            if is_file(path) {
                // A file that is really there and that nothing here can read is a different mistake
                // from a name that is not a file, and DuckDB says so with both lines, the second of
                // which is the way out. A file with no dot in it lands here too, which is why the
                // test is on the extension having a reader rather than on there being an extension.
                return Err(Error::binder(format!(
                    "No extension found that is capable of reading the file \"{path}\"\n* If this \
                     file is a supported file format you can explicitly use the reader functions, \
                     such as read_csv, read_json or read_parquet"
                )));
            }
            return Err(missing);
        };
        // The pattern is expanded before it is known to match anything, so a name that ends in .csv
        // and is not there gives the reader's own message rather than the catalog's. That is
        // DuckDB's order and it is the helpful one: somebody who wrote a file name wants to hear
        // about the file.
        let paths = files(path)?;
        // The name the columns answer to is the file's stem, so `SELECT mixed.a FROM
        // 'data/mixed.parquet'` works. That is DuckDB's choice and it is the useful one, since the
        // alternative is a table name with a dot and a slash in it that nothing can write. A pattern
        // keeps the whole of what was written instead, which is DuckDB's choice too and was
        // measured: there is no stem to take when the name stands for a directory full of files.
        let label = if alias == NONE {
            if is_pattern(path) {
                path.to_string()
            } else {
                let file = path.rsplit_once('/').map_or(path, |(_, file)| file);
                file.rsplit_once('.').map_or(file, |(stem, _)| stem).to_string()
            }
        } else {
            ast.string(alias).to_string()
        };
        let mut mirrorable = None;
        if function == TableFunction::ReadParquet
            && let Some((canonical, stamp)) = mirror_target(&paths)
        {
            if let Some(name) = self.catalog.mirror(&canonical, false, stamp) {
                let name = name.clone();
                return self.bind_catalog_table(ast, &name, label, columns);
            }
            mirrorable = Some(canonical);
        }
        let mut written = Vec::new();
        let read = match function {
            TableFunction::ReadParquet => {
                let footers = self.footers(&paths, mirrorable.as_deref())?;
                if let Some(canonical) = mirrorable.as_deref() {
                    self.want_mirror(canonical, false, &footers.rows);
                }
                Read {
                    fields: footers.fields,
                    rows: footers.rows,
                    distincts: footers.distincts,
                    zones: footers.zones,
                }
            }
            TableFunction::ReadJson => {
                Read::uncounted(self.json_fields(function, &paths, &mut written)?)
            }
            _ => Read::uncounted(csv_fields(&paths, Given::default())?),
        };
        let arguments: Vec<ExprRef> = paths.iter().map(|path| self.path_constant(path)).collect();
        let names: Vec<&str> = ast.name(columns).collect();
        self.table_function_source(function, &arguments, &written, read, &label, &names)
    }

    /// What the footers of `paths` say, from the outline alone where this bind is outlined and the
    /// read could go through a mirror.
    ///
    /// An outline that does not state a row count is read again in full, because a read that asks
    /// for no mirror would leave the plan outlined with nothing telling the caller to bind again.
    fn footers(&self, paths: &[String], mirrorable: Option<&str>) -> Result<Footers> {
        if let Some(path) = mirrorable.filter(|_| self.outlined) {
            let outline = parquet_outline(path)?;
            if outline.rows.value().is_some() {
                return Ok(outline);
            }
        }
        parquet_footers(paths)
    }

    /// Says the Parquet file at `path` could have been read through a native mirror, when its
    /// footer says how many rows it holds, which is what the database decides whether one would
    /// repay itself by.
    fn want_mirror(&mut self, path: &str, binary_as_string: bool, rows: &Stat<u64>) {
        if let Some(&rows) = rows.value() {
            self.plan.want_mirror(path, binary_as_string, rows);
        }
    }

    /// One file name, as a constant expression in the plan.
    fn path_constant(&mut self, path: &str) -> ExprRef {
        let value = self.plan.add_value(Value::Varchar(path.to_string()));
        self.plan.add_expr(Expr::Constant(value), LogicalType::Varchar)
    }

    /// The table function a file with this extension is read by, and `None` for one nothing reads.
    ///
    /// Both spellings of a tab separated file go to the CSV reader, which is not a shortcut: the
    /// extension picks the reader and the reader sniffs the punctuation, so a `.tsv` file that holds
    /// commas is read as commas. That was measured rather than assumed. The comparison ignores case
    /// because `UP.CSV` reads in duckdb v1.4.1.
    fn reader_for(extension: &str) -> Option<TableFunction> {
        if extension.eq_ignore_ascii_case("parquet") {
            return Some(TableFunction::ReadParquet);
        }
        if extension.eq_ignore_ascii_case("csv") || extension.eq_ignore_ascii_case("tsv") {
            return Some(TableFunction::ReadCsv);
        }
        if ["json", "jsonl", "ndjson"].iter().any(|json| extension.eq_ignore_ascii_case(json)) {
            return Some(TableFunction::ReadJson);
        }
        None
    }

    /// The node and the scope of a table function call whose arguments and columns are settled.
    ///
    /// The half a written out call shares with a replacement scan, which is everything after the
    /// question of what the file is called has been answered one way or the other.
    ///
    /// `read` is what the caller found out about the files, which comes in here rather than being
    /// read here because this function has the names and not the files: a replacement scan has
    /// already expanded its pattern and a written out call has already cast its argument, and
    /// neither of them wants to do it twice.
    fn table_function_source(
        &mut self,
        function: TableFunction,
        args: &[ExprRef],
        written: &[(&'static str, Value, ExprRef)],
        read: Read,
        label: &str,
        names: &[&str],
    ) -> Result<(NodeRef, Scope)> {
        let Read { fields, rows, distincts, zones } = read;
        let index = self.fresh_index();
        // Against the table index rather than against the node, because a pass is free to move the
        // node and none of them can move an index: an index is what a column reference names and
        // rewriting one would mean rewriting every expression above it. Nothing is recorded for a
        // function nobody measured, since an absent entry already reads back as unknown.
        if rows.is_known() {
            self.plan.measure(index, rows);
        }
        for (column, distinct) in distincts {
            self.plan.measure_distinct(index, &column, distinct);
        }
        if let Some(zones) = zones {
            self.plan.set_zones(index, zones);
        }
        let mut scope = Scope::empty();
        for (at, field) in fields.iter().enumerate() {
            scope.push(Visible {
                table: label.to_string(),
                name: field.name.clone(),
                binding: ColumnBinding::new(index, at as u32),
                ty: field.ty.clone(),
                // A reader takes what the file has, and no file format this reads says a column
                // cannot be null. The reference binary answers YES for every column of a Parquet.
                not_null: false,
                key: None,
                default: None,
                origin: None,
                qualified: false,
                also: None,
                hidden: false,
                using: None,
            });
        }
        if !names.is_empty() {
            scope.rename(names, label)?;
        } else if matches!(
            function,
            TableFunction::Range | TableFunction::GenerateSeries | TableFunction::Unnest
        ) {
            // The PostgreSQL naming, which the pin follows for these three and for no reader: the
            // alias names the one column, and the column keeps answering to its own name too.
            for column in &mut scope.columns {
                column.also = Some(std::mem::replace(&mut column.name, label.to_string()));
            }
        }
        let function = self.plan.intern(function.name());
        let args = self.plan.add_expr_list(args);
        let named: Vec<u32> =
            written.iter().map(|(parameter, _, _)| self.plan.intern(parameter)).collect();
        let settings: Vec<ExprRef> = written.iter().map(|(_, _, expr)| *expr).collect();
        let options = self.plan.add_name_list(&named);
        let settings = self.plan.add_expr_list(&settings);
        let columns = self.plan.add_fields(&fields);
        let node = self.add_node(Node::TableFunction {
            index,
            function,
            args,
            options,
            settings,
            columns,
            ordinality: false,
        });
        Ok((node, scope))
    }

    /// Every file a table function's file argument names, in the order they were written.
    ///
    /// Each pattern has to find at least one file of its own, which is DuckDB's rule and is why
    /// this expands one at a time rather than gathering everything and looking at the total. A
    /// list keeps its written order and its duplicates, so a file named twice is read twice, which
    /// was measured: the sort and the dedup belong to one pattern rather than to the list.
    fn file_paths(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        for pattern in self.file_patterns(expr, name)? {
            paths.extend(files(&pattern)?);
        }
        Ok(paths)
    }

    /// The files a JSON read with `allow_empty` names, where a pattern that matches nothing adds
    /// nothing rather than failing, so the call can come to no files at all.
    fn paths_or_none(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        for pattern in self.file_patterns(expr, name)? {
            paths.extend(content_files(&pattern)?);
        }
        Ok(paths)
    }

    /// The one file `read_single_json_file` reads, taken as it is written. Nothing is expanded, so
    /// a glob is the name of a file that is not there, which is how the pin answers it.
    fn single_path(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let path = self.file_patterns(expr, name)?.into_iter().next().unwrap_or_default();
        if !std::path::Path::new(&path).is_file() {
            return Err(Error::io(format!(
                "Cannot open file \"{path}\": No such file or directory"
            )));
        }
        Ok(vec![path])
    }

    /// The patterns a table function argument names, which have to be constants.
    ///
    /// A table function that reads a file is resolved by opening the file, and that happens here
    /// rather than when the query runs, because the rest of the statement cannot bind until the
    /// column names are known. So the path has to be something this binder can work out without
    /// running anything, and a literal is that. DuckDB folds a constant expression first, so
    /// `read_parquet('a' || '.parquet')` works there, and folding is M1 work that this will pick up
    /// for free once the optimizer runs before the plan is finished rather than after.
    ///
    /// One string is one pattern and a list is one pattern an item, which is DuckDB's pair of
    /// overloads. A null is a different sentence in each of them, both of them measured.
    ///
    /// The argument is folded rather than required to be a literal. A list is a call to `list_value`
    /// as of the work on #467, so requiring a literal here would have turned every `read_parquet`
    /// over a list into the message about a name that is not a constant, and the sentence this
    /// comment used to carry about folding being picked up for free was the plan for exactly that.
    /// What it buys beyond keeping the list working is `read_parquet('a' || '.parquet')`, which the
    /// pin answers and which used to be refused here.
    fn file_patterns(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let Some(value) = fold::value_of(&self.plan, expr)? else {
            return Err(Error::not_implemented(
                "a table function file name that is not a constant",
            ));
        };
        match value {
            Value::Varchar(path) => Ok(vec![path]),
            // DuckDB's own wording, which says list because its other overload takes one.
            Value::Null => Err(Error::parser(format!("{name} cannot take NULL list as parameter"))),
            // An empty list reaches the reader rather than failing to bind, because `[]` carries an
            // element type of the untyped null and a null promotes to VARCHAR, so the call resolves.
            // The pin says this, and it says it as an IO error rather than as a binder one, since
            // the list was a fine list and the objection is that there is no file in it.
            Value::List { values, .. } if values.is_empty() => {
                Err(Error::io(format!("\"{name}\" needs at least one file to read")))
            }
            Value::List { values, .. } => values
                .iter()
                .map(|value| match value {
                    Value::Varchar(path) => Ok(path.clone()),
                    _ => Err(Error::parser(format!(
                        "{name} reader cannot take NULL input as parameter"
                    ))),
                })
                .collect(),
            other => {
                Err(Error::internal(format!("a file name bound as VARCHAR arrived as {other}")))
            }
        }
    }

    /// The files a `read_text` or a `read_blob` reads, each pattern expanded in the order the list
    /// gives them, so a file named twice is read twice.
    ///
    /// Nothing to read is no rows rather than an error, whether the path is missing, the pattern
    /// matches nothing or the list is empty, which are the pin's answers. A null and a list that is
    /// not of strings are turned away in the pin's words.
    fn content_paths(&self, expr: ExprRef, name: &str) -> Result<Vec<String>> {
        let Some(value) = fold::value_of(&self.plan, expr)? else {
            return Err(Error::not_implemented(
                "a table function file name that is not a constant",
            ));
        };
        let patterns = match value {
            Value::Varchar(path) => vec![path],
            Value::Null => {
                return Err(Error::parser(format!(
                    "\"{name}\" cannot take NULL list as parameter"
                )));
            }
            Value::List { values, .. } => values
                .into_iter()
                .map(|value| match value {
                    Value::Varchar(path) => Ok(path),
                    Value::Null => Err(Error::parser(format!(
                        "\"{name}\" reader cannot take NULL input as parameter"
                    ))),
                    _ => Err(Error::parser(format!(
                        "\"{name}\" reader can only take a list of strings, structs or variants \
                         as a parameter"
                    ))),
                })
                .collect::<Result<_>>()?,
            other => {
                return Err(Error::internal(format!(
                    "a file name bound as VARCHAR arrived as {other}"
                )));
            }
        };
        let mut paths = Vec::new();
        for pattern in patterns {
            paths.extend(content_files(&pattern)?);
        }
        Ok(paths)
    }

    /// Which input of a join a query written in its `ON` has to be joined into.
    ///
    /// A join condition is evaluated by the join, over the rows its two inputs handed it, so a
    /// column the condition reads has to be produced by one of those two. A query written in the
    /// `ON` produces columns the condition reads, which means the query cannot be joined in above
    /// the join the way one written in a `WHERE` or a `SELECT` is. It has to go underneath, into
    /// one input or the other.
    ///
    /// Which input is decided by what the query reads. A query whose body reads the right side can
    /// only be evaluated where those rows are, so it goes into the right input, and the same for
    /// the left. A query that reads neither could go into either and goes into the left, which is
    /// also where an `IN` puts one whose left hand side reads the left and whose body reads
    /// nothing.
    ///
    /// The one that has no answer is a query that reads both sides. There is no single input that
    /// produces what it needs, and the shape upstream calls a pair dependent join is what handles
    /// it. `None` is that case, and the caller turns it into a refusal rather than a plan.
    fn side_of(
        &self,
        pending: &PendingSubquery,
        left_tables: &[u32],
        right_tables: &[u32],
    ) -> Option<Side> {
        let mut needs_left = false;
        let mut needs_right = false;
        let mut note = |binding: ColumnBinding| {
            needs_left |= left_tables.contains(&binding.table);
            needs_right |= right_tables.contains(&binding.table);
        };
        for &binding in &pending.reads {
            note(binding);
        }
        // A mark join carries the comparison rather than the condition carrying it, and that
        // comparison is written over the join's own rows. `l.a IN (SELECT ...)` reads the left side
        // there and nowhere else, so leaving it out would put the query on whichever side its body
        // happened to name and let the comparison ask a join for a column it was not given.
        for &condition in &pending.conditions {
            self.plan.read_columns(condition, &mut |_, binding| note(binding));
        }
        match (needs_left, needs_right) {
            (true, true) => None,
            (_, true) => Some(Side::Right),
            _ => Some(Side::Left),
        }
    }

    /// A join whose condition holds a query that reads rows from both of its inputs.
    ///
    /// This is the one [`Binder::side_of`] has no side for. The query has to be evaluated once per
    /// pair of rows, and there is no input that produces a pair, so it cannot go into either input
    /// the way the other two cases do. What produces a pair is the join itself, so the join becomes
    /// a product, the query is joined into the product's rows the way a query in a `WHERE` is joined
    /// into the rows the whole `FROM` produced, and the condition becomes a filter above that.
    ///
    /// That rewrite is only the same query for an inner join. An inner join keeps the pairs its
    /// condition holds and drops the rest, which is what a product and a filter do. Every other kind
    /// does something with the pairs it dropped, a left join pads them and a semi join asks whether
    /// there were any, and a filter above a product has already thrown away which left row a
    /// dropped pair came from. So those kinds become a lateral instead: each row of the side that
    /// is kept is an outer row, and the other side filtered by the condition is what is evaluated
    /// for it. A full join keeps both sides and is built from two of these, in
    /// [`Binder::bind_full_pair_join`].
    ///
    /// The product is not the plan that runs. The condition goes back into the join as a condition
    /// when filter pushdown looks at it, which is the pass that already turns a filter over an inner
    /// join into a join condition, so an equality in the `ON` is still an equality the hash join can
    /// build on. What cannot be pushed back down is the part that reads the query's output, and that
    /// part could not have been a join condition in the first place.
    #[allow(clippy::too_many_arguments)]
    fn bind_pair_dependent_join(
        &mut self,
        kind: ast::JoinKind,
        independent: bool,
        split: usize,
        left: NodeRef,
        right: NodeRef,
        pair: Vec<PendingSubquery>,
        conditions: Vec<ExprRef>,
        scope: Scope,
    ) -> Result<(NodeRef, Scope)> {
        if kind == ast::JoinKind::Full {
            return Err(Error::not_implemented(
                "a subquery that reads both sides of that join, written in the condition of a full \
                 join"
                    .to_string(),
            ));
        }
        // A lateral right side is already evaluated per left row, so the product this would build is
        // not the product the query means.
        if !independent {
            return Err(Error::not_implemented(
                "a subquery that reads both sides of that join, written in the condition of a join \
                 whose right side is lateral"
                    .to_string(),
            ));
        }
        // `ON` and `USING` cannot both be written, and this is only reached from the `ON` path, so
        // the list is the one bound condition. The fold is here so that it stays right if that stops
        // being true rather than for a case that exists today.
        let mut conditions = conditions.into_iter();
        let mut predicate = conditions.next().expect("a join condition was bound");
        for next in conditions {
            let children = self.plan.add_expr_list(&[predicate, next]);
            let conjunction = Expr::Conjunction { op: ConjunctionOp::And, children };
            predicate = self.plan.add_expr(conjunction, LogicalType::Boolean);
        }
        if kind != ast::JoinKind::Inner {
            // The rows of the side a left join keeps whole are the outer rows of a lateral, and the
            // other side filtered by the condition is what is evaluated for each of them, which is
            // how the pin plans it too. A right join is the same with the sides the other way
            // round, and the order the two come out in does not matter because every column is
            // read through its binding. A semi or an anti join asks whether that filtered side has
            // a row at all.
            let (kept, other) =
                if kind == ast::JoinKind::Right { (right, left) } else { (left, right) };
            let mut node = other;
            for pending in pair {
                node = self.attach_subquery(node, pending);
            }
            let node = self.add_node(Node::Filter { input: node, predicate });
            // The filtered side is a relation of its own under a projection, the way a lateral
            // subquery would be written, and its columns are read from there. A semi or an anti
            // join reads none of them, so it keeps one constant.
            let index = self.fresh_index();
            let mut scope = scope;
            let others = match kind {
                ast::JoinKind::Right => 0..split,
                ast::JoinKind::Left => split..scope.len(),
                _ => 0..0,
            };
            let mut exprs = Vec::with_capacity(others.len().max(1));
            let mut names = Vec::with_capacity(others.len().max(1));
            for (at, column) in scope.columns[others.clone()].iter_mut().enumerate() {
                exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
                names.push(self.plan.intern(&column.name));
                column.binding = ColumnBinding::new(index, at as u32);
            }
            self.carry_rowids(&mut scope, others, index, &mut exprs, &mut names);
            if exprs.is_empty() {
                let yes = self.plan.add_value(Value::Boolean(true));
                exprs.push(self.plan.add_expr(Expr::Constant(yes), LogicalType::Boolean));
                names.push(self.plan.intern("exists"));
            }
            let exprs = self.plan.add_expr_list(&exprs);
            let names = self.plan.add_name_list(&names);
            let node = self.add_node(Node::Project { input: node, index, exprs, names });
            let kind = match kind {
                ast::JoinKind::Semi => JoinKind::Semi,
                ast::JoinKind::Anti => JoinKind::Anti,
                _ => JoinKind::Left,
            };
            let conditions = self.plan.add_expr_list(&[]);
            let node =
                self.add_node(Node::DependentJoin { left: kept, right: node, kind, conditions });
            return Ok((node, scope));
        }
        let mut node = self.add_node(Node::CrossProduct { left, right });
        for pending in pair {
            node = self.attach_subquery(node, pending);
        }
        let node = self.add_node(Node::Filter { input: node, predicate });
        Ok((node, scope))
    }

    /// A full join whose condition holds a query that reads rows from both of its inputs.
    ///
    /// A full join is the left join with the same condition and then the rows of the right side
    /// that no left row matched, padded with nulls on the left, and that is how this builds it.
    /// Each half is bound again from what was written, the first as a left join and the second as
    /// an anti join with the two sides the other way round, so both take the lateral plan in
    /// [`Binder::bind_pair_dependent_join`], and a `UNION ALL` puts them together. The scope is the
    /// one the full join was bound with, read from the union.
    fn bind_full_pair_join(
        &mut self,
        ast: &Ast,
        left: ast::SourceRef,
        right: ast::SourceRef,
        on: ast::ExprRef,
        scope: Scope,
    ) -> Result<(NodeRef, Scope)> {
        let none = ast::Slice::default();
        let (matched, matched_scope) =
            self.bind_join(ast, left, right, ast::JoinKind::Left, false, on, none)?;
        let (lone, lone_scope) =
            self.bind_join(ast, right, left, ast::JoinKind::Anti, false, on, none)?;
        let split = matched_scope.len() - lone_scope.len();
        let mut kept = Vec::with_capacity(matched_scope.len());
        let mut padded = Vec::with_capacity(matched_scope.len());
        let mut names = Vec::with_capacity(matched_scope.len());
        for (at, column) in matched_scope.columns.iter().enumerate() {
            kept.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            padded.push(if at < split {
                let null = self.plan.add_value(Value::Null);
                self.plan.add_expr(Expr::Constant(null), column.ty.clone())
            } else {
                let column = &lone_scope.columns[at - split];
                self.plan.add_expr(Expr::Column(column.binding), column.ty.clone())
            });
            names.push(self.plan.intern(&column.name));
        }
        // A `rowid` of the left side is null in the half that has no left row, as a column is.
        let (left_rowids, right_rowids) =
            (matched_scope.rowids_in(0..split), matched_scope.rowids_in(split..usize::MAX));
        let lone_rowids = lone_scope.rowids_in(0..usize::MAX);
        let carried = right_rowids.len() == lone_rowids.len()
            && left_rowids.len() + right_rowids.len() == scope.rowids.len();
        if carried {
            for (binding, scan) in left_rowids {
                self.number_scan(scan);
                kept.push(self.plan.add_expr(Expr::Column(binding), LogicalType::BigInt));
                let null = self.plan.add_value(Value::Null);
                padded.push(self.plan.add_expr(Expr::Constant(null), LogicalType::BigInt));
                names.push(self.plan.intern(ROWID));
            }
            for ((binding, scan), (lone_binding, lone_scan)) in
                right_rowids.into_iter().zip(lone_rowids)
            {
                self.number_scan(scan);
                self.number_scan(lone_scan);
                kept.push(self.plan.add_expr(Expr::Column(binding), LogicalType::BigInt));
                padded.push(self.plan.add_expr(Expr::Column(lone_binding), LogicalType::BigInt));
                names.push(self.plan.intern(ROWID));
            }
        }
        let names = self.plan.add_name_list(&names);
        let mut sides = [matched, lone];
        for (side, exprs) in sides.iter_mut().zip([kept, padded]) {
            let index = self.fresh_index();
            let exprs = self.plan.add_expr_list(&exprs);
            *side = self.add_node(Node::Project { input: *side, index, exprs, names });
        }
        let index = self.fresh_index();
        let [left, right] = sides;
        let node =
            self.add_node(Node::SetOp { left, right, kind: SetOpKind::Union, all: true, index });
        let mut scope = scope;
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
            column.not_null = false;
        }
        if carried {
            let width = scope.columns.len();
            scope.move_rowids(0..usize::MAX, index, width);
        } else {
            scope.rowids.clear();
        }
        Ok((node, scope))
    }

    /// A join, with any side it held wrapped around it, see [`Binder::bind_held_side`].
    #[allow(clippy::too_many_arguments)]
    fn bind_join(
        &mut self,
        ast: &Ast,
        left: ast::SourceRef,
        right: ast::SourceRef,
        kind: ast::JoinKind,
        natural: bool,
        on: ast::ExprRef,
        using: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        let held = self.held.len();
        let sources = self.held_sources.len();
        let result = self.bind_join_inner(ast, left, right, kind, natural, on, using);
        self.held_sources.truncate(sources);
        let definitions = self.held.split_off(held);
        let (mut node, scope) = result?;
        for side in definitions.into_iter().rev() {
            let HeldSide { definition, cte, name, columns } = side;
            node =
                self.add_node(Node::MaterializedCte { definition, body: node, name, cte, columns });
        }
        Ok((node, scope))
    }

    /// One side of a join whose condition has a subquery in it, held when it has to run once.
    ///
    /// A subquery that reads both sides of a join that is not inner is lowered into a plan that
    /// reads a side more than once: the rows it keeps and the values the subquery is asked about
    /// come from two reads of it, and a full join reads both sides twice more. That is the same
    /// answer as one read only while the side gives the same rows every time, and a side with
    /// `nextval` or `random()` in it does not. The pin holds such a side in a CTE, so `nextval` is
    /// called once per row and the rows the subquery sees are the rows that come out. This does
    /// the same, and a full join, which binds its sides again for each half, reads the one that
    /// was held here rather than binding a second one.
    fn bind_held_side(
        &mut self,
        source: ast::SourceRef,
        bound: Option<(NodeRef, Scope)>,
    ) -> Option<(NodeRef, Scope)> {
        let (side, scope) = match bound {
            Some((node, scope)) => {
                if !volatile_node(&self.plan, node) {
                    return Some((node, scope));
                }
                let mut exprs = Vec::with_capacity(scope.len());
                let mut names = Vec::with_capacity(scope.len());
                for column in &scope.columns {
                    exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
                    names.push(self.plan.intern(&column.name));
                }
                let index = self.fresh_index();
                let mut scope = scope;
                self.carry_rowids(&mut scope, 0..usize::MAX, index, &mut exprs, &mut names);
                let exprs = self.plan.add_expr_list(&exprs);
                let names = self.plan.add_name_list(&names);
                let definition = self.add_node(Node::Project { input: node, index, exprs, names });
                let cte = self.next_cte;
                self.next_cte += 1;
                let name = self.plan.intern("pair_side");
                let mut fields = scope.fields();
                for _ in scope.rowids_in(0..usize::MAX) {
                    fields.push(Field::required(ROWID.to_string(), LogicalType::BigInt));
                }
                let columns = self.plan.add_fields(&fields);
                let side = HeldSide { definition, cte, name, columns };
                self.held.push(side);
                self.held_sources.push((source, side, scope.clone()));
                (side, scope)
            }
            None => {
                let (_, side, scope) =
                    self.held_sources.iter().find(|(held, ..)| *held == source)?.clone();
                (side, scope)
            }
        };
        let index = self.fresh_index();
        let HeldSide { cte, name, columns, .. } = side;
        let node = self.add_node(Node::CteScan { index, cte, name, columns });
        let mut scope = scope;
        for (at, column) in scope.columns.iter_mut().enumerate() {
            column.binding = ColumnBinding::new(index, at as u32);
        }
        let width = scope.columns.len();
        scope.move_rowids(0..usize::MAX, index, width);
        Some((node, scope))
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_join_inner(
        &mut self,
        ast: &Ast,
        left: ast::SourceRef,
        right: ast::SourceRef,
        kind: ast::JoinKind,
        natural: bool,
        on: ast::ExprRef,
        using: ast::Slice,
    ) -> Result<(NodeRef, Scope)> {
        // A mark join compares the two sides and nothing else, so a query in its condition has no
        // place to run. The corpus refuses it in the same words as a condition of the wrong shape.
        if kind == ast::JoinKind::Mark && on != NONE && crate::columns::has_subquery(ast, on) {
            return Err(unsupported_mark());
        }
        let hold = !matches!(kind, ast::JoinKind::Inner | ast::JoinKind::Cross)
            && on != NONE
            && crate::columns::has_subquery(ast, on);
        let (left_node, left_scope) = match hold.then(|| self.bind_held_side(left, None)).flatten()
        {
            Some(held) => held,
            None => {
                let bound = self.bind_source(ast, left)?;
                if hold { self.bind_held_side(left, Some(bound)).expect("bound") } else { bound }
            }
        };
        let (right_node, right_scope, correlated) =
            match hold.then(|| self.bind_held_side(right, None)).flatten() {
                Some((node, scope)) => (node, scope, Vec::new()),
                None => {
                    let (node, scope, correlated) = self.bind_lateral(ast, right, &left_scope)?;
                    if hold && correlated.is_empty() {
                        let (node, scope) =
                            self.bind_held_side(right, Some((node, scope))).expect("bound");
                        (node, scope, correlated)
                    } else {
                        (node, scope, correlated)
                    }
                }
            };
        // A row of the right side exists only for the left row it was evaluated against, so a kind
        // that has to produce right rows with no left row has nothing to produce them from. The
        // pinned build says this and names only the two kinds that work.
        if !correlated.is_empty() && kind == ast::JoinKind::Mark {
            return Err(unsupported_mark());
        }
        if !correlated.is_empty()
            && !matches!(kind, ast::JoinKind::Inner | ast::JoinKind::Cross | ast::JoinKind::Left)
        {
            return Err(Error::binder(
                "The combining JOIN type must be INNER or LEFT for a LATERAL reference",
            ));
        }
        let (right_node, right_scope, marker) = if kind == ast::JoinKind::Mark {
            let (node, scope, marker) = self.mark_side(right_node, right_scope);
            (node, scope, Some(marker))
        } else {
            (right_node, right_scope, None)
        };
        self.distinct_table_names(ast, &[left], right)?;
        let split = left_scope.len();
        // Which table index came from which side, kept before the two scopes become one. A query
        // written in the `ON` is joined into one of the inputs rather than above the join, and this
        // is what says which. A `USING` drops the right side's copy of a joined-on column out of
        // the scope below, and dropping a column does not change the index it came from, so the
        // answer this gives is still right afterwards.
        let left_tables: Vec<u32> =
            left_scope.columns.iter().map(|column| column.binding.table).collect();
        let right_tables: Vec<u32> =
            right_scope.columns.iter().map(|column| column.binding.table).collect();
        let mut scope = left_scope.concat(right_scope);

        // NATURAL is USING over whatever both sides happen to call the same thing, which is why it
        // is resolved here and never reaches the plan as its own idea.
        let compare = self.semantics.identifier_compare();
        let first = self.semantics.join_columns() == JoinColumns::MergedFirst
            && matches!(
                kind,
                ast::JoinKind::Inner
                    | ast::JoinKind::Left
                    | ast::JoinKind::Right
                    | ast::JoinKind::Full
            );
        let merged: Vec<String> = if natural {
            let mut names = Vec::new();
            for (at, column) in scope.columns.iter().enumerate().take(split) {
                if !column.hidden
                    && scope.columns[split..]
                        .iter()
                        .any(|right| !right.hidden && compare.same(&right.name, &column.name))
                    && !names.iter().any(|held: &String| compare.same(held, &column.name))
                {
                    let _ = at;
                    names.push(column.name.clone());
                }
            }
            names
        } else {
            // A name written twice is one column, not two. `USING (id, id)` is legal and means what
            // `USING (id)` means, and the reference binary agrees. Taking it twice would build the
            // same equality twice and, worse, drop the right side's copy twice, which takes a
            // column out of the answer that nobody named and runs off the end of the scope when the
            // copy was the last column in it. PostgreSQL refuses the second name.
            let mut names: Vec<String> = Vec::new();
            for name in ast.name(using) {
                if !names.iter().any(|held| compare.same(held, name)) {
                    names.push(name.to_string());
                } else if first {
                    return Err(Error::binder(format!(
                        "column name \"{name}\" appears more than once in USING clause"
                    ))
                    .state(SqlState::DUPLICATE_COLUMN)
                    .unplaced());
                }
            }
            names
        };

        let mut conditions = Vec::new();
        let mut pairs = Vec::new();
        for name in &merged {
            let left_at = scope.columns[..split]
                .iter()
                .position(|column| !column.hidden && compare.same(&column.name, name))
                .ok_or_else(|| {
                    Error::binder(format!(
                        "column \"{name}\" specified in USING clause does not exist in left table"
                    ))
                    .state(SqlState::UNDEFINED_COLUMN)
                    .unplaced()
                })?;
            let right_at = scope.columns[split..]
                .iter()
                .position(|column| !column.hidden && compare.same(&column.name, name))
                .map(|at| at + split)
                .ok_or_else(|| {
                    Error::binder(format!(
                        "column \"{name}\" specified in USING clause does not exist in right table"
                    ))
                    .state(SqlState::UNDEFINED_COLUMN)
                    .unplaced()
                })?;
            let left_column = &scope.columns[left_at];
            let (left_binding, left_type) = (left_column.binding, left_column.ty.clone());
            let right_column = &scope.columns[right_at];
            let (right_binding, right_type) = (right_column.binding, right_column.ty.clone());
            let left_expr = self.plan.add_expr(Expr::Column(left_binding), left_type);
            let right_expr = self.plan.add_expr(Expr::Column(right_binding), right_type);
            conditions.push(self.compare(rudb_plan::CompareOp::Equal, left_expr, right_expr)?);
            pairs.push((left_at, right_at));
        }
        // A joined-on column appears once in `SELECT *` and for a bare name, so the right side's
        // copy is hidden there. It stays for `b.k` and `b.*`, which still read the right side's own
        // value on the pin. A `RIGHT` or `FULL` join hides the left copy as well, because there the
        // bare name is not the left value, and puts the column it does read in its place once the
        // join is built. PostgreSQL does this for each kind of join, and puts the merged columns
        // first. See `Binder::merged`.
        let outer = matches!(kind, ast::JoinKind::Right | ast::JoinKind::Full) || first;
        for &(left_at, right_at) in &pairs {
            // The two copies join one group, and so does anything already in a group with either,
            // which is a column joined on again in a chain of joins.
            let group =
                scope.columns[left_at].using.map_or(scope.columns[left_at].binding, Joined::group);
            let before = [&scope.columns[left_at], &scope.columns[right_at]]
                .map(|column| column.using.map(Joined::group));
            for column in &mut scope.columns {
                if let Some(joined) = column.using
                    && before.contains(&Some(joined.group()))
                {
                    column.using = Some(match joined {
                        Joined::Copy(_) => Joined::Copy(group),
                        Joined::Merged(_) => Joined::Merged(group),
                    });
                }
            }
            for at in [left_at, right_at] {
                if scope.columns[at].using.is_none() {
                    scope.columns[at].using = Some(Joined::Copy(group));
                }
            }
            scope.columns[right_at].hidden = true;
            if outer {
                scope.columns[left_at].hidden = true;
            }
        }

        let mut left_node = left_node;
        let mut right_node = right_node;
        let mut pair = Vec::new();
        if on != NONE {
            if !merged.is_empty() {
                return Err(Error::binder("a join cannot have both ON and USING"));
            }
            self.clause = "JOIN condition";
            let waiting = self.scalar_subqueries.len();
            let predicate = self.bind_expr(ast, on, &scope)?;
            conditions.push(self.as_boolean(ast, on, predicate, "JOIN/ON")?);
            for pending in self.scalar_subqueries.split_off(waiting) {
                match self.side_of(&pending, &left_tables, &right_tables) {
                    Some(Side::Right) => right_node = self.attach_subquery(right_node, pending),
                    Some(Side::Left) => left_node = self.attach_subquery(left_node, pending),
                    None => pair.push(pending),
                }
            }
        }

        if kind == ast::JoinKind::Cross && !conditions.is_empty() {
            return Err(Error::binder("a CROSS JOIN cannot have a condition"));
        }
        // A semi join and an anti join ask a question about the right side rather than producing
        // any of it, so what is in scope after one is the left side alone. The condition is bound
        // above and is the last thing that can name the right side. Without this, `SELECT *` over
        // one expanded to both sides and the projection asked a join whose output is the left side
        // for columns it does not have, which came out as an internal error about a column not
        // being in the schema. That is tamnd/rudb#847. The reference binary refuses `b.w` here with
        // a binder error naming `a` as the only candidate table, which is the same rule said from
        // the other end.
        if matches!(kind, ast::JoinKind::Semi | ast::JoinKind::Anti) {
            scope.truncate(split);
        }
        // A mark join is the left side and the one column saying whether a row matched, and a
        // right semi or anti join is the right side alone.
        if let Some(marker) = marker {
            self.mark_conditions(&conditions, &left_tables, &right_tables)?;
            for condition in &mut conditions {
                *condition = self.tuple_not_equal(*condition);
            }
            scope.truncate(split);
            scope.push(marker);
        }
        if matches!(kind, ast::JoinKind::RightSemi | ast::JoinKind::RightAnti) {
            scope.columns.drain(..split);
            scope.rowids.retain(|rowid| rowid.at >= split);
            for rowid in &mut scope.rowids {
                rowid.at -= split;
            }
        }
        if !pair.is_empty()
            && matches!(
                kind,
                ast::JoinKind::Single | ast::JoinKind::RightSemi | ast::JoinKind::RightAnti
            )
        {
            return Err(Error::not_implemented(
                "a subquery that reads both sides of a JOIN BY join, written in its condition"
                    .to_string(),
            ));
        }
        if !pair.is_empty() && kind == ast::JoinKind::Full && correlated.is_empty() {
            return self.bind_full_pair_join(ast, left, right, on, scope);
        }
        if !pair.is_empty() {
            return self.bind_pair_dependent_join(
                kind,
                correlated.is_empty(),
                split,
                left_node,
                right_node,
                pair,
                conditions,
                scope,
            );
        }
        // A product is the join with nothing to join on, and it is not one when the right side has
        // to be evaluated per left row, because then there is a dependency to lower even though
        // there is no condition to test.
        if correlated.is_empty()
            && conditions.is_empty()
            && matches!(kind, ast::JoinKind::Cross | ast::JoinKind::Inner)
        {
            let node = self.add_node(Node::CrossProduct { left: left_node, right: right_node });
            return Ok((node, scope));
        }
        let swap = matches!(kind, ast::JoinKind::RightSemi | ast::JoinKind::RightAnti);
        let kind = match kind {
            ast::JoinKind::Inner | ast::JoinKind::Cross => JoinKind::Inner,
            ast::JoinKind::Left => JoinKind::Left,
            ast::JoinKind::Right => JoinKind::Right,
            ast::JoinKind::Full => JoinKind::Full,
            ast::JoinKind::Semi => JoinKind::Semi,
            ast::JoinKind::Anti => JoinKind::Anti,
            ast::JoinKind::Positional => JoinKind::Positional,
            ast::JoinKind::Mark => JoinKind::Mark,
            ast::JoinKind::Single => JoinKind::Single,
            ast::JoinKind::RightSemi => JoinKind::Semi,
            ast::JoinKind::RightAnti => JoinKind::Anti,
        };
        // The right side is the one that is kept, so it is the left input of the join that keeps
        // it. Every column is read through its binding, so nothing else changes.
        let (left_node, right_node) =
            if swap { (right_node, left_node) } else { (left_node, right_node) };
        let conditions = self.plan.add_expr_list(&conditions);
        let node = if correlated.is_empty() {
            self.add_node(Node::Join {
                left: left_node,
                right: right_node,
                kind,
                conditions,
                build: BuildSide::default(),
            })
        } else {
            self.add_node(Node::DependentJoin {
                left: left_node,
                right: right_node,
                kind,
                conditions,
            })
        };
        if outer && !pairs.is_empty() {
            let node = self.merged(node, &mut scope, kind, &pairs, first)?;
            return Ok((node, scope));
        }
        Ok((node, scope))
    }

    /// The right side of a `JOIN BY (TYPE MARK)`, under a projection that adds the column the join
    /// sets to whether a left row matched, and that column as the scope will show it.
    ///
    /// The mark join takes its marker from the last column of its right input, which is how a
    /// subquery planned as one hands it over too. The column is named `__mark_join_marker` and
    /// its table is `__internal_mark_join_ref` and an index, as on the pin, so `t1.*` leaves it
    /// out. The right side's own columns stay visible to the condition and to nothing after it.
    fn mark_side(&mut self, node: NodeRef, mut scope: Scope) -> (NodeRef, Scope, Visible) {
        let index = self.fresh_index();
        let mut exprs = Vec::with_capacity(scope.columns.len() + 1);
        let mut names = Vec::with_capacity(scope.columns.len() + 1);
        for (at, column) in scope.columns.iter_mut().enumerate() {
            exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
            names.push(self.plan.intern(&column.name));
            column.binding = ColumnBinding::new(index, at as u32);
        }
        scope.rowids.clear();
        let marker = Visible {
            table: format!("__internal_mark_join_ref{index}"),
            name: "__mark_join_marker".to_string(),
            binding: ColumnBinding::new(index, exprs.len() as u32),
            ty: LogicalType::Boolean,
            not_null: false,
            key: None,
            default: None,
            origin: None,
            qualified: false,
            also: None,
            hidden: false,
            using: None,
        };
        let yes = self.plan.add_value(Value::Boolean(true));
        exprs.push(self.plan.add_expr(Expr::Constant(yes), LogicalType::Boolean));
        names.push(self.plan.intern(&marker.name));
        let exprs = self.plan.add_expr_list(&exprs);
        let names = self.plan.add_name_list(&names);
        let node = self.add_node(Node::Project { input: node, index, exprs, names });
        (node, scope, marker)
    }

    /// Whether the condition of a `JOIN BY (TYPE MARK)` has a shape a mark join answers.
    ///
    /// Each conjunct has to be a comparison whose operands read one side each, or no side, which
    /// is what a subquery planned as a mark join produces. Then the whole has to be one such
    /// comparison, equalities alone, `IS NOT DISTINCT FROM` alone, or a group: `IS NOT DISTINCT
    /// FROM` on operands of one type followed by one last comparison of any kind but the two
    /// distinct ones, on operands of one type that is not a union or an unnamed struct, and not
    /// nested at all for an ordered comparison. That is the list upstream accepts, in its order,
    /// so the group keys have to come first.
    fn mark_conditions(
        &self,
        conditions: &[ExprRef],
        left_tables: &[u32],
        right_tables: &[u32],
    ) -> Result<()> {
        use rudb_plan::CompareOp;
        let mut conjuncts = Vec::new();
        // Popped in the order they are written, since the group rule reads the last one.
        let mut pending: Vec<ExprRef> = conditions.iter().rev().copied().collect();
        while let Some(at) = pending.pop() {
            match self.plan.expr(at) {
                Expr::Conjunction { op: ConjunctionOp::And, children } => {
                    pending.extend(self.plan.expr_list(*children).iter().rev());
                }
                _ => conjuncts.push(at),
            }
        }
        // Which sides an operand reads, as (left, right).
        let sides = |expr: ExprRef| {
            let mut read = (false, false);
            self.plan.read_columns(expr, &mut |_, binding| {
                read.0 |= left_tables.contains(&binding.table);
                read.1 |= right_tables.contains(&binding.table);
            });
            read
        };
        let mut comparisons = Vec::with_capacity(conjuncts.len());
        for &conjunct in &conjuncts {
            let Expr::Compare { op, left, right } = *self.plan.expr(conjunct) else {
                return Err(unsupported_mark());
            };
            let (a, b) = (sides(left), sides(right));
            let one_each = |a: (bool, bool), b: (bool, bool)| !a.1 && !b.0;
            if !one_each(a, b) && !one_each(b, a) {
                return Err(unsupported_mark());
            }
            comparisons.push((op, self.plan.expr_type(left), self.plan.expr_type(right)));
        }
        let all = |wanted: CompareOp| comparisons.iter().all(|&(op, ..)| op == wanted);
        if comparisons.len() == 1 || all(CompareOp::Equal) || all(CompareOp::NotDistinctFrom) {
            return Ok(());
        }
        let Some((&(last, left, right), groups)) = comparisons.split_last() else {
            return Err(unsupported_mark());
        };
        let grouped = groups
            .iter()
            .all(|&(op, left, right)| op == CompareOp::NotDistinctFrom && left == right);
        let ordered = matches!(
            last,
            CompareOp::Less
                | CompareOp::LessOrEqual
                | CompareOp::Greater
                | CompareOp::GreaterOrEqual
        );
        let quantified = left == right
            && !matches!(left, LogicalType::Union(_))
            && !matches!(left, LogicalType::Struct(fields) if Field::unnamed(fields))
            && (matches!(last, CompareOp::Equal | CompareOp::NotEqual)
                || (ordered && !left.is_nested()));
        if grouped && quantified { Ok(()) } else { Err(unsupported_mark()) }
    }

    /// The column a bare name reads after a join `USING` it, when the left copy is not that
    /// column.
    ///
    /// After a `RIGHT` join it is the right side's value, which is there on every row, and its
    /// type is the right side's. After a `FULL` join it is `COALESCE(a.k, b.k)` at the type the two
    /// meet at, so a row from either side has its key, and that needs a projection above the join
    /// to compute it in. `SELECT typeof(k)` over an `INTEGER` and a `BIGINT` key is `BIGINT` on the
    /// pin for both kinds and `INTEGER` for an inner or a left join, which reads the left copy.
    /// The column has the place of the left copy.
    ///
    /// With `first`, which is the rule of PostgreSQL, this is done for an inner and a left join
    /// too, which read the left value. Each column has the common type of its two copies, so a
    /// projection casts a value that has another type. The columns come first, in the order of
    /// `pairs`.
    ///
    /// The column has no table name, so `a.k` and `b.k` still find each side's hidden copy.
    fn merged(
        &mut self,
        node: NodeRef,
        scope: &mut Scope,
        kind: JoinKind,
        pairs: &[(usize, usize)],
        first: bool,
    ) -> Result<NodeRef> {
        /// The value of one merged column: a copy that the join gives, or an expression that a
        /// projection above the join computes.
        enum Taken {
            Copy(usize),
            Computed(ExprRef),
        }
        let mut values = Vec::with_capacity(pairs.len());
        for &(left_at, right_at) in pairs {
            let (left, right) = (&scope.columns[left_at], &scope.columns[right_at]);
            let ty = if first { left.ty.promote(&right.ty) } else { None };
            let value = match kind {
                JoinKind::Full => {
                    let left = self.plan.add_expr(Expr::Column(left.binding), left.ty.clone());
                    let right = self.plan.add_expr(Expr::Column(right.binding), right.ty.clone());
                    Taken::Computed(self.call("coalesce", vec![left, right])?)
                }
                JoinKind::Right => Taken::Copy(right_at),
                _ => Taken::Copy(left_at),
            };
            values.push(match (value, ty) {
                (Taken::Copy(at), Some(ty)) if scope.columns[at].ty != ty => {
                    let column = &scope.columns[at];
                    let value = self.plan.add_expr(Expr::Column(column.binding), column.ty.clone());
                    Taken::Computed(self.cast_to(value, &ty))
                }
                (Taken::Computed(value), Some(ty)) => Taken::Computed(self.cast_to(value, &ty)),
                (value, _) => value,
            });
        }
        let mut node = node;
        let mut computed = Vec::new();
        if values.iter().any(|value| matches!(value, Taken::Computed(_))) {
            let index = self.fresh_index();
            let mut exprs = Vec::with_capacity(scope.columns.len() + values.len());
            let mut names = Vec::with_capacity(scope.columns.len() + values.len());
            for column in &scope.columns {
                exprs.push(self.plan.add_expr(Expr::Column(column.binding), column.ty.clone()));
                names.push(self.plan.intern(&column.name));
            }
            let width = scope.columns.len();
            for (at, column) in scope.columns.iter_mut().enumerate() {
                column.binding = ColumnBinding::new(index, at as u32);
            }
            for (value, &(left_at, _)) in values.iter().zip(pairs) {
                if let Taken::Computed(value) = *value {
                    let binding = ColumnBinding::new(index, (width + computed.len()) as u32);
                    exprs.push(value);
                    names.push(self.plan.intern(&scope.columns[left_at].name));
                    computed.push((binding, self.plan.expr_type(value).clone()));
                }
            }
            self.carry_rowids(scope, 0..usize::MAX, index, &mut exprs, &mut names);
            let exprs = self.plan.add_expr_list(&exprs);
            let names = self.plan.add_name_list(&names);
            node = self.add_node(Node::Project { input: node, index, exprs, names });
        }
        let mut computed = computed.into_iter();
        let mut made = Vec::with_capacity(pairs.len());
        for (value, &(left_at, _)) in values.iter().zip(pairs) {
            let (binding, ty, origin) = match *value {
                Taken::Copy(at) => {
                    let column = &scope.columns[at];
                    (column.binding, column.ty.clone(), column.origin)
                }
                Taken::Computed(_) => {
                    let (binding, ty) = computed.next().expect("one column for each value");
                    (binding, ty, None)
                }
            };
            let left = &scope.columns[left_at];
            let column = Visible {
                table: String::new(),
                name: left.name.clone(),
                binding,
                ty,
                not_null: false,
                key: None,
                default: None,
                origin,
                qualified: false,
                also: None,
                hidden: false,
                using: left.using.map(|joined| Joined::Merged(joined.group())),
            };
            made.push((left_at, column));
        }
        if first {
            scope.columns.splice(0..0, made.into_iter().map(|(_, column)| column));
        } else {
            // From the back, so the positions of the ones still to place are where they were.
            made.sort_by_key(|&(left_at, _)| left_at);
            for (left_at, column) in made.into_iter().rev() {
                scope.columns.insert(left_at, column);
            }
        }
        Ok(node)
    }

    // -------------------------------------------------------------- aggregates

    /// Binds a `FILTER (WHERE ...)` predicate, or says there was none.
    ///
    /// The predicate is a condition over the input rows and not over the answer, so it is bound in
    /// the scope the arguments are bound in, and it is cast to `BOOLEAN` the way a `WHERE` is:
    /// `FILTER (WHERE i)` over an integer column is a filter on whether the integer is not zero.
    /// PostgreSQL casts no condition, so a PostgreSQL session reads it as any other condition.
    fn bind_filter(
        &mut self,
        ast: &Ast,
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        if filter == NONE {
            return Ok(None);
        }
        let bound = self.bind_expr(ast, filter, scope)?;
        if self.semantics.condition_types() == ConditionTypes::Postgres {
            return Ok(Some(self.as_boolean(ast, filter, bound, "FILTER")?));
        }
        Ok(Some(self.checked_cast_to(bound, &LogicalType::Boolean, false)?))
    }

    /// Binds an aggregate call, records it, and hands back a reference to where its result lands.
    ///
    /// An aggregate inside a lambda's body is computed over the rows and not over the elements,
    /// so its arguments cannot see the lambda's parameters. See `crate::lambda`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_aggregate(
        &mut self,
        ast: &Ast,
        name: &str,
        args: &[ast::ExprRef],
        distinct: bool,
        filter: ast::ExprRef,
        sorted: &[ast::OrderItem],
        scope: &Scope,
    ) -> Result<ExprRef> {
        if self.trying {
            return Err(Error::binder("aggregates are not allowed inside the TRY expression"));
        }
        let frames = std::mem::take(&mut self.lambda_frames);
        let call = AggregateCall { name, args, distinct, filter, sorted };
        let bound = self.bind_aggregate_over_rows(ast, &call, scope);
        self.lambda_frames = frames;
        bound
    }

    fn bind_aggregate_over_rows(
        &mut self,
        ast: &Ast,
        written: &AggregateCall<'_>,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let filter = written.filter;
        let exporting = std::mem::take(&mut self.exporting);
        self.aggregate_allowed()?;
        // The predicate goes first, which is the order the messages come out in upstream: a call
        // whose argument and whose filter both name columns that are not there is refused over the
        // filter. It is bound as if it were inside the call, so an aggregate in it is caught, and a
        // window in it is refused with the words a window inside an aggregate is refused with.
        let filter = self.aggregate_filter(ast, filter, scope)?;
        self.bind_aggregate_after_filter(ast, written, filter, exporting, scope)
    }

    /// Refuses an aggregate where the query cannot compute one: inside another aggregate, inside a
    /// `FILTER`, or in a clause that is evaluated before the rows are grouped.
    pub(crate) fn aggregate_allowed(&self) -> Result<()> {
        if self.in_filter {
            return Err(Error::binder("aggregate functions are not allowed in FILTER")
                .state(SqlState::GROUPING_ERROR));
        }
        if self.in_aggregate && self.folding {
            return Err(Error::binder("Aggregate functions are not supported here"));
        }
        if self.in_aggregate {
            return Err(Error::binder("aggregate function calls cannot be nested")
                .state(SqlState::GROUPING_ERROR));
        }
        if self.aggregation.is_none() {
            let clause = pin_clause(self.clause);
            let error = Error::binder(format!("{clause} cannot contain aggregates!"))
                .state(SqlState::GROUPING_ERROR);
            return Err(match postgres_clause(self.clause) {
                Some(place) => error.pg(format!("aggregate functions are not allowed in {place}")),
                None => error,
            });
        }
        Ok(())
    }

    /// The `FILTER` of an aggregate call, bound as if it were inside the call.
    pub(crate) fn aggregate_filter(
        &mut self,
        ast: &Ast,
        filter: ast::ExprRef,
        scope: &Scope,
    ) -> Result<Option<ExprRef>> {
        self.in_aggregate = true;
        self.in_filter = true;
        let filter = self.bind_filter(ast, filter, scope);
        self.in_filter = false;
        self.in_aggregate = false;
        filter
    }

    fn bind_aggregate_after_filter(
        &mut self,
        ast: &Ast,
        written: &AggregateCall<'_>,
        filter: Option<ExprRef>,
        exporting: bool,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let AggregateCall { name, args, distinct, sorted, .. } = *written;
        // The ordered-set aggregates take the value they read from their `ORDER BY` when the call
        // does not write it, which is what `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)` is
        // parsed into, and a descending order counts their fractions from the top.
        let (ordered_set, taken) = ordered_set(name, args.len(), sorted);
        let injected = sorted.iter().map(|item| item.expr).take(usize::from(taken));
        let args: Vec<ast::ExprRef> = injected.chain(args.iter().copied()).collect();

        self.in_aggregate = true;
        let mut bound = Vec::with_capacity(args.len());
        let mut failure = None;
        let written_keys = sorted.iter().map(|item| item.expr);
        for (at, arg) in args.iter().copied().chain(written_keys).enumerate() {
            let expr = if at < args.len() {
                self.bind_counted(ast, name, &args, arg, scope)
            } else {
                self.bind_expr(ast, arg, scope)
            };
            match expr {
                Ok(expr) => bound.push(expr),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        self.in_aggregate = false;
        if let Some(error) = failure {
            return Err(error);
        }
        let keys = bound.split_off(args.len());
        let mut resolved = Vec::with_capacity(sorted.len());
        for (&key, &item) in keys.iter().zip(sorted) {
            let ty = self.plan.expr_type(key).clone();
            resolved.push(self.sort_operators(ast, item, &ty)?);
        }
        let sorted = &resolved[..];
        let from_top = ordered_set && sorted.len() == 1 && self.descending(sorted[0].order);
        // A distinct aggregate sees each value once, and a key that is not one of the values would
        // have more than one of them to sort that value by.
        if distinct && !keys.iter().all(|&key| bound.iter().any(|&arg| self.same_expr(arg, key))) {
            return Err(Error::binder(
                "In a DISTINCT aggregate, ORDER BY expressions must appear in the argument list",
            ));
        }
        if distinct {
            self.distinct_aggregate_operators(ast, &args, &bound)?;
        }

        // `min` and `max` of a parameter of no type read `text` in PostgreSQL.
        if self.semantics.unknown_types() == UnknownTypes::Postgres
            && matches!(name.to_ascii_lowercase().as_str(), "min" | "max")
            && let [only] = bound[..]
            && self.is_placeholder(only)
        {
            bound[0] = self.cast_to(only, &LogicalType::Varchar);
        }
        let types: Vec<LogicalType> =
            bound.iter().map(|&arg| self.plan.expr_type(arg).clone()).collect();
        let mut resolved = resolve(name, &types)?;
        if !exporting {
            resolved = self.spread_as_postgres(resolved, &types);
        }
        // The separator is read once per group and not once per row, so the pin wants it to be the
        // same on every row and says so in these words.
        if resolved.name == "string_agg"
            && bound.len() == 2
            && !matches!(fold::value_of(&self.plan, bound[1]), Ok(Some(_)))
        {
            return Err(Error::binder(
                "The \"separator\" argument in function \"string_agg\" must be a constant expression",
            ));
        }
        if matches!(resolved.name, "quantile_cont" | "quantile_disc") {
            let ordered = ordered_set && sorted.len() == 1;
            bound[1] = self.quantile_fraction(resolved.name, bound[1], ordered, from_top)?;
        }
        if resolved.name == "approx_quantile" {
            self.digest_arguments(&bound)?;
        }
        if resolved.name == "reservoir_quantile" {
            self.reservoir_arguments(&bound)?;
        }
        if resolved.name == "approx_top_k" {
            self.top_k_argument(&bound)?;
        }
        if resolved.name == "combine_aggr" {
            crate::state::merged_argument(self.plan.expr_type(bound[0]))?;
        }
        let mut cast = Vec::with_capacity(bound.len());
        for (arg, wanted) in bound.iter().zip(&resolved.arguments) {
            cast.push(self.checked_cast_to(*arg, wanted, false)?);
        }
        if resolved.name == "lttb" {
            self.lttb_points(cast[2])?;
        }
        let called = top_values(resolved.name, &mut cast);
        let given = cast.len();
        // An ordered call sorts an enum by where its labels were declared, as `ORDER BY` does.
        let keys: Vec<ExprRef> = if exporting {
            keys
        } else {
            keys.into_iter().map(|key| self.by_position(key)).collect()
        };
        let (mut name, order) = self.ordered_aggregate(called, sorted, &keys, &mut cast, exporting);
        let unordered = order.is_empty();
        // An enum orders by where its labels were declared and not by how they are spelled, and the
        // `arg_min` family answers its first argument, so the second can be weighed by its code.
        if called.starts_with("arg_") && given > 1 && !exporting {
            cast[1] = self.by_position(cast[1]);
        }
        let mut ty = resolved.returns;
        // An exported state is typed with the call it came from, so that `finalize` and `combine`
        // know what to read it back into, and the name says so, which keeps the executor's paths
        // for a plain `sum` or `count` away from a call that answers with something else.
        if exporting {
            let mut columns: Vec<LogicalType> =
                cast.iter().map(|&arg| self.plan.expr_type(arg).clone()).collect();
            let arguments = columns[..given].to_vec();
            // An ordered call keeps its rows until the end whatever the aggregate is, so its state
            // is those rows, though the aggregate still has to be one that has a state at all.
            let mut layout = rudb_kernels::state_layout(resolved.name, &arguments, &ty)?;
            if !order.is_empty() {
                layout = rudb_kernels::ordered_layout(&columns);
            }
            columns.truncate(given);
            let from = rudb_kernels::state_constants(resolved.name);
            let mut constants = Vec::with_capacity(given);
            for (at, &arg) in cast[..given].iter().enumerate() {
                constants.push(if at >= from { fold::value_of(&self.plan, arg)? } else { None });
            }
            ty = LogicalType::aggregate_state(resolved.name, columns, ty, layout, constants, order);
            name.push_str(rudb_kernels::EXPORTED);
        }
        // PostgreSQL takes the mean of an integer or a decimal as a `numeric`: `int8_avg` and
        // `numeric_avg` divide the exact sum by the count with `numeric_div`. The sum and the count
        // keep the fast paths that the executor has for them, where a mean as a `numeric` would not.
        let exact = |ty: &LogicalType| ty.is_integer() || matches!(ty, LogicalType::Decimal { .. });
        let postgres = self.semantics.aggregate_types() == AggregateTypes::Postgres;
        if postgres
            && !exporting
            && resolved.name == "avg"
            && unordered
            && let [argument] = bound[..]
            && exact(self.plan.expr_type(argument))
        {
            let summed = resolve("sum", &types)?;
            let argument = self.checked_cast_to(argument, &summed.arguments[0], false)?;
            let total =
                self.aggregate_call("sum", &[argument], distinct, filter, summed.returns)?;
            let count =
                self.aggregate_call("count", &[argument], distinct, filter, LogicalType::BigInt)?;
            let total = self.cast_to(total, &LogicalType::Numeric);
            // A group with no values has a null sum and a count of zero, and its mean is null.
            let count = self.cast_to(count, &LogicalType::Numeric);
            let count = self.zero_to_null(count);
            return self.call("/", vec![total, count]);
        }
        let column = self.aggregate_call(&name, &cast, distinct, filter, ty)?;
        if exporting {
            return Ok(column);
        }
        Ok(self.typed_as_postgres(resolved.name, &types, column))
    }

    /// The variance family with the types PostgreSQL gives it, over a group or over a window.
    /// PostgreSQL keeps `var_pop`, `var_samp`, `stddev_pop` and `stddev_samp` of an exact type in
    /// exact `numeric` sums and answers a `numeric`, and of a `real` or a `double precision` in the
    /// state of `float8_accum` and answers a `double precision`, where the pin answers a DOUBLE
    /// from a Welford state for both. The calls take the names of PostgreSQL's final functions,
    /// which are the states of [`rudb_kernels`] that keep them. Any other call is as resolved.
    fn spread_as_postgres(&self, resolved: Resolved, types: &[LogicalType]) -> Resolved {
        if self.semantics.aggregate_types() != AggregateTypes::Postgres {
            return resolved;
        }
        let ([argument], [_]) = (types, &resolved.arguments[..]) else { return resolved };
        let (argument, returns) = match argument {
            // An exact state reads the integers that fit an `i128` as they are.
            LogicalType::HugeInt | LogicalType::UHugeInt => {
                (LogicalType::Numeric, LogicalType::Numeric)
            }
            exact if exact.is_integer() => (exact.clone(), LogicalType::Numeric),
            LogicalType::Decimal { .. } | LogicalType::Numeric => {
                (argument.clone(), LogicalType::Numeric)
            }
            LogicalType::Float | LogicalType::Double => (LogicalType::Double, LogicalType::Double),
            _ => return resolved,
        };
        let exact = returns == LogicalType::Numeric;
        let name = match (resolved.name, exact) {
            ("var_pop", true) => "numeric_var_pop",
            ("var_samp", true) => "numeric_var_samp",
            ("stddev_pop", true) => "numeric_stddev_pop",
            ("stddev_samp", true) => "numeric_stddev_samp",
            ("var_pop", false) => "float8_var_pop",
            ("var_samp", false) => "float8_var_samp",
            ("stddev_pop", false) => "float8_stddev_pop",
            ("stddev_samp", false) => "float8_stddev_samp",
            _ => return resolved,
        };
        Resolved { name, arguments: vec![argument], returns, ..resolved }
    }

    /// A call with the type PostgreSQL gives it, over a group or over a window. PostgreSQL sums an
    /// `int2` or an `int4` into an `int8` and a `float4` into a `float4`, where the pin sums them
    /// into a HUGEINT and a DOUBLE, and its `ntile` is an `int4` where the pin's is a BIGINT. Any
    /// other call is the column as it is.
    fn typed_as_postgres(&mut self, name: &str, types: &[LogicalType], column: ExprRef) -> ExprRef {
        if self.semantics.aggregate_types() != AggregateTypes::Postgres {
            return column;
        }
        if name == "ntile" {
            return self.cast_to(column, &LogicalType::Integer);
        }
        if name != "sum" {
            return column;
        }
        match types.first() {
            Some(LogicalType::TinyInt | LogicalType::SmallInt | LogicalType::Integer) => {
                self.cast_to(column, &LogicalType::BigInt)
            }
            Some(LogicalType::Float) => self.cast_to(column, &LogicalType::Float),
            _ => column,
        }
    }

    /// The column of the aggregate output that holds the call `name` over `args`. Two identical
    /// aggregates are one column of the aggregate's output, so `SELECT sum(x), sum(x) / count(*)`
    /// computes one sum, not two.
    pub(crate) fn aggregate_call(
        &mut self,
        name: &str,
        args: &[ExprRef],
        distinct: bool,
        filter: Option<ExprRef>,
        ty: LogicalType,
    ) -> Result<ExprRef> {
        let args = self.plan.add_expr_list(args);
        let name = self.plan.intern(name);
        let call = self.plan.add_expr(Expr::Aggregate { name, args, distinct, filter }, ty.clone());
        let existing = self.aggregation.as_ref().map(|held| held.aggregates.clone());
        let existing = existing.unwrap_or_default();
        let at = match existing.iter().position(|&held| self.same_expr(held, call)) {
            Some(at) => at,
            None => {
                let aggregation = self.aggregation.as_mut().expect("checked by the caller");
                aggregation.aggregates.push(call);
                aggregation.aggregates.len() - 1
            }
        };
        let aggregation = self.aggregation.as_ref().expect("checked by the caller");
        let (index, groups) = (aggregation.index, aggregation.groups.len());
        let column = self.column(index, groups + at, ty);
        self.carry_collation(call, column)?;
        Ok(column)
    }

    /// The fraction of a quantile call, checked the way the pin checks it and counted from the top
    /// when the call's `ORDER BY` is descending.
    ///
    /// A negative fraction already means counting from the top, so a call that also writes an
    /// order may not have one, and a list may not mix the two directions.
    fn quantile_fraction(
        &mut self,
        name: &str,
        fraction: ExprRef,
        ordered: bool,
        from_top: bool,
    ) -> Result<ExprRef> {
        let Ok(Some(value)) = fold::value_of(&self.plan, fraction) else {
            return Err(Error::binder(format!(
                "The \"quantile\" argument in function \"{name}\" must be a constant expression"
            )));
        };
        if value.is_null() {
            return Err(Error::binder(format!(
                "The \"quantile\" argument in function '\"{name}\"' must not be NULL"
            )));
        }
        let each = match &value {
            Value::List { values, .. } => values.as_slice(),
            one => std::slice::from_ref(one),
        };
        let mut signs = (false, false);
        for one in each {
            if one.is_null() {
                return Err(Error::binder("QUANTILE parameter cannot be NULL"));
            }
            let share = share(one).unwrap_or(f64::NAN);
            if !(-1.0..=1.0).contains(&share) {
                return Err(Error::binder(
                    "QUANTILE can only take parameters in the range [-1, 1]",
                ));
            }
            if share < 0.0 {
                signs.0 = true;
            } else {
                signs.1 = true;
            }
        }
        if ordered && signs.0 {
            return Err(Error::binder("PERCENTILEs can only take parameters in the range [0, 1]"));
        }
        if signs.0 && signs.1 {
            return Err(Error::binder("QUANTILE parameters must have consistent signs"));
        }
        if !from_top {
            return Ok(fraction);
        }
        let negated = match value {
            Value::List { element, values } => {
                Value::List { element, values: values.iter().map(negated).collect() }
            }
            one => negated(&one),
        };
        Ok(self.add_constant(negated))
    }

    /// Refuses an `approx_top_k` whose `k` is not a constant, over a group or over a window.
    ///
    /// The pin names the argument `col1` whatever it was written as, and the sentence is its own.
    fn top_k_argument(&self, bound: &[ExprRef]) -> Result<()> {
        if matches!(fold::value_of(&self.plan, bound[1]), Ok(Some(_))) {
            return Ok(());
        }
        Err(Error::binder(
            "The \"col1\" argument in function \"approx_top_k\" must be a constant expression",
        ))
    }

    /// Checks the number of points an `lttb` call thins to, once it is a BIGINT, in the pin's words.
    fn lttb_points(&self, n: ExprRef) -> Result<()> {
        match fold::value_of(&self.plan, n)? {
            None => {
                Err(Error::binder("lttb: the number of points (third argument) must be a constant"))
            }
            Some(Value::Null) => Err(Error::binder("lttb: the number of points must not be NULL")),
            Some(Value::BigInt(n)) if n < 2 => {
                Err(Error::binder("lttb: the number of points must be at least 2"))
            }
            Some(_) => Ok(()),
        }
    }

    /// Checks the fraction and the sample size of a `reservoir_quantile` call the way the pin does,
    /// which is in words of its own rather than the ones the other quantiles use.
    fn reservoir_arguments(&self, bound: &[ExprRef]) -> Result<()> {
        let constant = |arg: ExprRef, parameter: &str| match fold::value_of(&self.plan, arg) {
            Ok(Some(value)) => Ok(value),
            _ => Err(Error::binder(format!(
                "The \"{parameter}\" argument in function \"reservoir_quantile\" must be a constant \
                 expression"
            ))),
        };
        let fraction = constant(bound[1], "quantile")?;
        let each = match &fraction {
            Value::List { values, .. } => values.as_slice(),
            one => std::slice::from_ref(one),
        };
        for one in each {
            if one.is_null() {
                return Err(Error::binder("RESERVOIR_QUANTILE QUANTILE parameter cannot be NULL"));
            }
            if !(0.0..=1.0).contains(&share(one).unwrap_or(f64::NAN)) {
                return Err(Error::binder(
                    "RESERVOIR_QUANTILE can only take parameters in the range [0, 1]",
                ));
            }
        }
        let Some(&size) = bound.get(2) else {
            return Ok(());
        };
        let size = constant(size, "sample_size")?;
        if size.is_null() {
            return Err(Error::binder(
                "The \"sample_size\" argument in function '\"reservoir_quantile\"' must not be NULL",
            ));
        }
        if share(&size).is_none_or(|n| n <= 0.0) {
            return Err(Error::binder(
                "Size of the RESERVOIR_QUANTILE sample must be bigger than 0",
            ));
        }
        Ok(())
    }

    /// Checks the fractions of an `approx_quantile` call the way the pin does, which is in words of
    /// its own again.
    fn digest_arguments(&self, bound: &[ExprRef]) -> Result<()> {
        let Ok(Some(fraction)) = fold::value_of(&self.plan, bound[1]) else {
            return Err(Error::binder(
                "The \"quantile\" argument in function \"approx_quantile\" must be a constant \
                 expression",
            ));
        };
        if fraction.is_null() {
            return Err(Error::binder(
                "The \"quantile\" argument in function '\"approx_quantile\"' must not be NULL",
            ));
        }
        let each = match &fraction {
            Value::List { values, .. } => values.as_slice(),
            one => std::slice::from_ref(one),
        };
        for one in each {
            if one.is_null() {
                return Err(Error::binder("APPROXIMATE QUANTILE parameter cannot be NULL"));
            }
            if !(0.0..=1.0).contains(&share(one).unwrap_or(f64::NAN)) {
                return Err(Error::binder(
                    "APPROXIMATE QUANTILE can only take parameters in range [0, 1]",
                ));
            }
        }
        Ok(())
    }

    /// The name of an aggregate with the `ORDER BY` of its call folded in, and its keys, with the
    /// keys that are not one of its arguments added to the end of them.
    ///
    /// Only the aggregates whose answer depends on the order the rows come in keep their keys, which
    /// is what the pin does too: `sum(x ORDER BY y)` is `sum(x)` there, named as written and computed
    /// without a sort. A key that is a constant orders nothing and is dropped, so `list(x ORDER BY
    /// 1)` is a plain `list` and not the first column, which is what a number means in the query's
    /// own `ORDER BY` and not what it means here.
    ///
    /// A call whose state is exported keeps every key, constant or not, whatever the aggregate,
    /// because the pin's state of any ordered call is the rows it saw with their keys.
    fn ordered_aggregate(
        &mut self,
        name: &str,
        sorted: &[ast::OrderItem],
        keys: &[ExprRef],
        args: &mut Vec<ExprRef>,
        exporting: bool,
    ) -> (String, Vec<StateKey>) {
        // The `arg_min` family keeps the row that came first of two that tie, so the order the rows
        // come in decides which one it answers.
        const DEPENDS_ON_ORDER: &[&str] = &[
            "list",
            "first",
            "last",
            "any_value",
            "string_agg",
            "lttb",
            "arg_min",
            "arg_max",
            "arg_min_null",
            "arg_max_null",
            "arg_min_nulls_last",
            "arg_max_nulls_last",
        ];
        if !exporting && !DEPENDS_ON_ORDER.contains(&name) {
            return (name.to_string(), Vec::new());
        }
        let given = args.len();
        let mut order = Vec::new();
        for (&key, item) in keys.iter().zip(sorted) {
            if !exporting && matches!(fold::value_of(&self.plan, key), Ok(Some(_))) {
                continue;
            }
            let descending = self.descending(item.order);
            let nulls_first = match item.nulls {
                Nulls::First => true,
                Nulls::Last => false,
                Nulls::Unstated => self.semantics.nulls_first(descending),
            };
            // The plot sorts on a column of its own, so its key is never one of its arguments.
            let argument = if name == "lttb" {
                None
            } else {
                args[..given].iter().position(|&arg| self.same_expr(arg, key))
            };
            let column = argument.unwrap_or_else(|| {
                args.push(key);
                args.len() - 1
            });
            order.push(StateKey { descending, nulls_first, column });
        }
        if order.is_empty() {
            return (name.to_string(), Vec::new());
        }
        (rudb_kernels::ordered_name(name, given, &order), order)
    }

    // ----------------------------------------------------------------- windows

    /// Binds a window call, files it under the run it belongs to, and hands back its column.
    ///
    /// The result is a column of a [`Node::Window`] rather than the call itself, for the reason the
    /// aggregate path returns a column too: the operator produces the value and everything above it
    /// reads the value, so a target that wraps a window in arithmetic is arithmetic over a column.
    ///
    /// A window inside a lambda's body is computed over the rows for the reason an aggregate is,
    /// so it cannot see the lambda's parameters either.
    pub(crate) fn bind_window(
        &mut self,
        ast: &Ast,
        written: &WindowCall<'_>,
        scope: &Scope,
    ) -> Result<ExprRef> {
        if self.trying {
            return Err(Error::binder("window functions are not allowed in try"));
        }
        let frames = std::mem::take(&mut self.lambda_frames);
        let bound = self.bind_window_over_rows(ast, written, scope);
        self.lambda_frames = frames;
        bound
    }

    fn bind_window_over_rows(
        &mut self,
        ast: &Ast,
        written: &WindowCall<'_>,
        scope: &Scope,
    ) -> Result<ExprRef> {
        let WindowCall { call, name, args, distinct, filter, ignore_nulls, spec, .. } = *written;
        let postgres = self.semantics.function_rules() == FunctionRules::Postgres;
        if self.in_aggregate {
            return Err(Error::binder(
                "aggregate function calls cannot contain window function calls",
            ));
        }
        if self.in_window {
            return Err(Error::binder("window function calls cannot be nested"));
        }
        if postgres && let Some(error) = self.within_group_required(name, args.len()) {
            return Err(error);
        }
        let clause = pin_clause(self.clause);
        if clause != "SELECT clause" && clause != "ORDER BY clause" && clause != "QUALIFY clause" {
            let error = Error::binder(format!("{clause} cannot contain window functions!"))
                .state(SqlState::WINDOWING_ERROR);
            return Err(match postgres_clause(self.clause) {
                Some(place) => error.pg(format!("window functions are not allowed in {place}")),
                None => error,
            });
        }

        // `count(*)` is a different function from `count(x)` here for the reason it is a different
        // function in an ordinary call: one counts rows and the other counts the rows where its
        // argument is not null. A star is not an expression and nothing below this binds one.
        let starred = args.iter().any(|&arg| {
            matches!(ast.expr(arg), ast::Expr::Star { qualifier, replacements }
                if qualifier.is_empty() && replacements.is_empty())
        });
        let (name, args): (&str, &[ast::ExprRef]) = if starred {
            if !same_name(name, "count") || args.len() != 1 {
                return Err(Error::binder(format!("* is not allowed in {name}()")));
            }
            ("count_star", &[])
        } else if same_name(name, "count") && args.is_empty() {
            // `count()` with nothing in it is upstream's other spelling of `count(*)`. It counts
            // rows the same way and it is not an arity mistake.
            if postgres {
                return Err(overcall::parameterless(name));
            }
            ("count_star", &[])
        } else {
            (name, args)
        };

        let held = ast.window(spec);
        self.in_window = true;
        let parts = self.window_parts(ast, written, args, held, scope);
        // The predicate goes last here, which is the other way round from an ordinary aggregate and
        // is again the order the messages come out in upstream. It is still inside the window, so a
        // window in it is a nested window, while an aggregate in it is an ordinary aggregate over
        // the same rows and is answered.
        let filter = if parts.is_ok() { self.bind_filter(ast, filter, scope) } else { Ok(None) };
        self.in_window = false;
        let mut parts = parts?;
        let filter = filter?;
        // PostgreSQL's `lag` and `lead` take the value and the default as `anycompatible`, so the
        // two meet at their common type, where the pin casts the default to the type of the value.
        if (same_name(name, "lag") || same_name(name, "lead"))
            && let ([value, _, default], [bound_value, _, bound_default]) =
                (args, &mut parts.args[..])
        {
            let mut pair = [*bound_value, *bound_default];
            self.common_type(ast, &[*value, *default], &mut pair, None)?;
            (*bound_value, *bound_default) = (pair[0], pair[1]);
        }
        // Upstream's rule, in its words. A `RANGE` offset is a distance from the current row's sort
        // key, so there has to be exactly one sort key for it to be a distance from.
        let offsets = [parts.frame.start, parts.frame.end]
            .iter()
            .any(|end| matches!(end, WindowBound::Preceding(_) | WindowBound::Following(_)));
        if parts.frame.unit == WindowUnit::Range && offsets && parts.order.len() != 1 {
            return Err(Error::binder("RANGE frames must have only one ORDER BY expression"));
        }

        let types: Vec<LogicalType> =
            parts.args.iter().map(|&arg| self.plan.expr_type(arg).clone()).collect();
        // Upstream refuses this once the arguments are bound and before it looks for an overload,
        // so it is said even of an aggregate the arguments do not fit. PostgreSQL looks for the
        // function first, and refuses `RESPECT NULLS` too.
        if !postgres && ignore_nulls && kind_of(name) == Some(FunctionKind::Aggregate) {
            return Err(Error::binder(
                "RESPECT/IGNORE NULLS is not supported for windowed aggregates",
            ));
        }
        let resolved = match window_signature(name, &types) {
            Err(error) if postgres => {
                return Err(overcall::not_windowed(ast, call, name, args, &types, error));
            }
            resolved => resolved?,
        };
        if postgres
            && (ignore_nulls || ast.null_treated(call))
            && let Some(error) = overcall::treated_window(name)
        {
            return Err(error);
        }
        // `fill` reads the sort key rather than the frame, so what it needs from the query is not
        // what any other window needs and it is refused on its own terms. An `ORDER BY` inside
        // its brackets is the key it reads in place of the one in the `OVER`.
        if resolved.name == "fill" {
            let order = if parts.inner.is_empty() { &parts.order } else { &parts.inner };
            let keys: Vec<LogicalType> =
                order.iter().map(|key| self.plan.expr_type(key.expr).clone()).collect();
            refuse_fill(&types[0], &keys, distinct, ignore_nulls)?;
        }
        // Upstream's sentence, doubled quotes and all. A DISTINCT over an aggregate inside an OVER
        // is ordinary and answered, and a DISTINCT over a ranking window is refused there, because
        // there is nothing for it to collapse when the call reads no values in the first place.
        if distinct && kind_of(resolved.name) == Some(FunctionKind::Window) {
            return Err(Error::binder(format!(
                "DISTINCT is not implemented for the window function \"\"{name}\"\""
            )));
        }
        // The same sentence for the same reason. A ranking window reads no values, so there is
        // nothing for a predicate over the values to keep or drop.
        if filter.is_some() && kind_of(resolved.name) == Some(FunctionKind::Window) {
            return Err(Error::binder(format!(
                "FILTER is not implemented for the window function \"\"{name}\"\""
            )));
        }
        // An `ORDER BY` inside the brackets puts the rows of the frame in a different order for
        // this one call to read them in. Every aggregate and the three that count through the
        // frame read the frame in that order, and the ranking windows, `lag` and `lead` stop
        // reading the partition and read the frame instead, ranked by those keys. `dense_rank` is
        // the one the reference binary turns down. The exclusion is refused first and in the
        // reference binary's own sentence, because that is the one it reaches for when both
        // apply, and it is refused for `fill` too, whose order is the key it reads. Per #1204.
        if !parts.inner.is_empty() && kind_of(resolved.name) == Some(FunctionKind::Window) {
            let counts = matches!(resolved.name, "first_value" | "last_value" | "nth_value");
            if !counts && parts.frame.exclude != WindowExclude::NoOthers {
                return Err(Error::binder(format!(
                    "EXCLUDE is not supported for the window function \"\"{}\"\"",
                    resolved.name
                )));
            }
            if resolved.name == "dense_rank" {
                return Err(Error::binder(
                    "ORDER BY is not supported for the window function \"\"dense_rank\"\"",
                ));
            }
        }
        if resolved.name == "approx_top_k" {
            self.top_k_argument(&parts.args)?;
        }
        // The mean of an integer or a decimal is the exact sum over the count as a `numeric`, as
        // it is for an aggregate in `bind_aggregate_over_rows`.
        let exact = |ty: &LogicalType| ty.is_integer() || matches!(ty, LogicalType::Decimal { .. });
        if self.semantics.aggregate_types() == AggregateTypes::Postgres
            && resolved.name == "avg"
            && parts.inner.is_empty()
            && let [argument] = parts.args[..]
            && exact(self.plan.expr_type(argument))
        {
            let summed = window_signature("sum", &types)?;
            let argument = self.checked_cast_to(argument, &summed.arguments[0], false)?;
            let args = self.plan.add_expr_list(&[argument]);
            let order = self.plan.add_sort_keys(&[]);
            let mut columns =
                [("sum", summed.returns), ("count", LogicalType::BigInt)].map(|(name, ty)| {
                    let name = self.plan.intern(name);
                    let window = Expr::Window { name, args, distinct, filter, ignore_nulls, order };
                    let call = self.plan.add_expr(window, ty.clone());
                    let keys = (parts.partition.clone(), parts.order.clone(), parts.frame);
                    let at = self.window_run(ast, spec, keys, call);
                    let index = self.windows.last().expect("the run was just filed").index;
                    let column = self.column(index, at, ty);
                    self.cast_to(column, &LogicalType::Numeric)
                });
            // A frame with no values has a null sum and a count of zero, and its mean is null.
            columns[1] = self.zero_to_null(columns[1]);
            return self.call("/", columns.to_vec());
        }
        let resolved = self.spread_as_postgres(resolved, &types);
        let mut cast = Vec::with_capacity(parts.args.len());
        for (arg, wanted) in parts.args.iter().zip(&resolved.arguments) {
            cast.push(self.checked_cast_to(*arg, wanted, false)?);
        }
        if resolved.name == "lttb" {
            self.lttb_points(cast[2])?;
        }
        let name = top_values(resolved.name, &mut cast);
        let args = self.plan.add_expr_list(&cast);
        let order = self.plan.add_sort_keys(&parts.inner);
        let name = self.plan.intern(name);
        let ty = resolved.returns;
        let call = self.plan.add_expr(
            Expr::Window { name, args, distinct, filter, ignore_nulls, order },
            ty.clone(),
        );

        let at = self.window_run(ast, spec, (parts.partition, parts.order, parts.frame), call);
        let index = self.windows.last().expect("the run was just filed").index;
        let column = self.column(index, at, ty);
        self.carry_collation(call, column)?;
        Ok(self.typed_as_postgres(resolved.name, &types, column))
    }

    /// Files a call under the run that matches it, or opens a new run, and says which column it is.
    ///
    /// The run that matches is only ever the last one, because a query that goes back to an earlier
    /// partitioning after using a different one in between wants the operators in the order it wrote
    /// them. Merging the two would be a rewrite, and a rewrite over a window is the optimizer's to
    /// make once it knows what the sort below each one costs.
    fn window_run(
        &mut self,
        ast: &Ast,
        spec: ast::WindowRef,
        parts: (Vec<ExprRef>, Vec<SortKey>, WindowFrame),
        call: ExprRef,
    ) -> usize {
        let (partition, order, frame) = parts;
        let named = ast.named_window(spec);
        let matches = self.windows.last().is_some_and(|run| {
            run.frame == frame
                && run.partition.len() == partition.len()
                && run.order.len() == order.len()
                && run.partition.iter().zip(&partition).all(|(&l, &r)| self.same_expr(l, r))
                && run.order.iter().zip(&order).all(|(l, r)| {
                    l.descending == r.descending
                        && l.nulls_first == r.nulls_first
                        && self.same_expr(l.expr, r.expr)
                })
        });
        if !matches {
            let index = self.fresh_index();
            let calls = Vec::new();
            self.windows.push(WindowRun { index, partition, order, frame, calls, spec, named });
        }
        let run = self.windows.last_mut().expect("a run is open");
        if named && (!run.named || spec < run.spec) {
            (run.spec, run.named) = (spec, true);
        }
        // Two identical calls over one run are one column, the same way two identical aggregates
        // over one grouping are. `SELECT sum(i) OVER (), sum(i) OVER () + 1` totals once.
        let calls = self.windows.last().expect("a run is open").calls.clone();
        if let Some(at) = calls.iter().position(|&held| self.same_expr(held, call)) {
            return at;
        }
        let run = self.windows.last_mut().expect("a run is open");
        run.calls.push(call);
        run.calls.len() - 1
    }

    /// Binds the arguments and everything inside the `OVER`, with the aggregate rule applied.
    ///
    /// The aggregate rule applies to all of it, which is measured rather than assumed: over a
    /// grouped block `sum(count(i)) OVER ()` binds and `sum(i) OVER ()` is the ungrouped column
    /// complaint, and the same pair of answers comes back for a partition key and for an order key.
    fn window_parts(
        &mut self,
        ast: &Ast,
        written: &WindowCall<'_>,
        args: &[ast::ExprRef],
        held: ast::WindowSpec,
        scope: &Scope,
    ) -> Result<WindowParts> {
        let mut bound = Vec::with_capacity(args.len());
        for &arg in args {
            let expr = self.bind_counted(ast, written.name, args, arg, scope)?;
            bound.push(self.over_aggregate(expr, scope)?);
        }
        // The keys inside the brackets are bound against the same rows the arguments are, because
        // that is what they sort: the call reads its frame in this order, and the frame is made of
        // the operator's input rows.
        let mut inner = Vec::new();
        for item in ast.order_list(written.order).to_vec() {
            let expr = self.bind_expr(ast, item.expr, scope)?;
            let expr = self.over_aggregate(expr, scope)?;
            let ty = self.plan.expr_type(expr).clone();
            let item = self.sort_operators(ast, item, &ty)?;
            let expr = self.collate_key(expr, expr)?;
            inner.push(self.sort_key(expr, item));
        }
        let mut partition = Vec::new();
        for &key in ast.expr_list(held.partition) {
            let expr = self.bind_expr(ast, key, scope)?;
            let expr = self.over_aggregate(expr, scope)?;
            let ty = self.plan.expr_type(expr).clone();
            self.sort_group_operators(&ty, false, ast.leftmost_span(key))?;
            partition.push(self.collate_key(expr, expr)?);
        }
        let mut order = Vec::new();
        for item in ast.order_list(held.order).to_vec() {
            let expr = self.bind_expr(ast, item.expr, scope)?;
            let expr = self.over_aggregate(expr, scope)?;
            let ty = self.plan.expr_type(expr).clone();
            let item = self.sort_operators(ast, item, &ty)?;
            let expr = self.collate_key(expr, expr)?;
            order.push(self.sort_key(expr, item));
        }
        let frame = WindowFrame {
            unit: match held.unit {
                ast::WindowUnit::Rows => WindowUnit::Rows,
                ast::WindowUnit::Range => WindowUnit::Range,
                ast::WindowUnit::Groups => WindowUnit::Groups,
            },
            start: self.window_bound(ast, held.start, scope)?,
            end: self.window_bound(ast, held.end, scope)?,
            exclude: match held.exclude {
                ast::WindowExclude::NoOthers => WindowExclude::NoOthers,
                ast::WindowExclude::CurrentRow => WindowExclude::CurrentRow,
                ast::WindowExclude::Group => WindowExclude::Group,
                ast::WindowExclude::Ties => WindowExclude::Ties,
            },
        };
        Ok(WindowParts { args: bound, partition, order, inner, frame })
    }

    /// One end of a frame, with its offset bound where it has one.
    fn window_bound(
        &mut self,
        ast: &Ast,
        bound: ast::WindowBound,
        scope: &Scope,
    ) -> Result<WindowBound> {
        let offset = |binder: &mut Self, written| {
            let expr = binder.bind_expr(ast, written, scope)?;
            binder.over_aggregate(expr, scope)
        };
        Ok(match bound {
            ast::WindowBound::UnboundedPreceding => WindowBound::UnboundedPreceding,
            ast::WindowBound::CurrentRow => WindowBound::CurrentRow,
            ast::WindowBound::UnboundedFollowing => WindowBound::UnboundedFollowing,
            ast::WindowBound::Preceding(written) => WindowBound::Preceding(offset(self, written)?),
            ast::WindowBound::Following(written) => WindowBound::Following(offset(self, written)?),
        })
    }

    /// Which of this block's groups is exactly that column, if one of them is.
    ///
    /// Exactly the column and not an expression over it, because the caller is looking for the same
    /// value read from the aggregate instead of from the table underneath it, and `GROUP BY k + 1`
    /// carries the sum and not the column.
    fn group_of(&self, read: ColumnBinding) -> Option<usize> {
        self.aggregation.as_ref()?.groups.iter().position(
            |group| matches!(*self.plan.expr(*group), Expr::Column(binding) if binding == read),
        )
    }

    /// The outer column a query still waiting under this grouping correlates to and the grouping
    /// does not carry upward, which is the column an error should name.
    ///
    /// `None` when the binding is not one of those queries, which is every ordinary case of a
    /// column read without a group.
    fn ungrouped_correlation(&self, binding: ColumnBinding) -> Option<ColumnBinding> {
        let pending =
            self.scalar_subqueries.iter().find(|pending| pending.index == binding.table)?;
        pending.reads.iter().copied().find(|read| self.group_of(*read).is_none())
    }

    /// Whether a column is the result of a window this block is building.
    fn is_window_output(&self, binding: ColumnBinding) -> bool {
        self.windows.iter().any(|run| run.index == binding.table)
    }

    /// Whether a column was resolved in an enclosing query rather than in this one.
    ///
    /// Every such read is written into the frame of the query being bound as it is resolved, and
    /// the frame is only handed up once that query's body is done, so while a select list or a
    /// `HAVING` is being bound the frame still holds everything this query read from outside it.
    fn is_correlation(&self, binding: ColumnBinding) -> bool {
        self.correlations.last().is_some_and(|frame| frame.contains(&binding))
    }

    /// The name a column is written under, for an error message to say which one it means.
    ///
    /// A column of an enclosing query is not in this query's scope, so the outer scopes are searched
    /// as well. Without that the message names no column at all, which is how `column a column must
    /// appear in the GROUP BY clause` came to be a sentence this engine printed.
    fn name_of(&self, binding: ColumnBinding, scope: &Scope) -> String {
        std::iter::once(scope)
            .chain(self.outer_scopes.iter().rev())
            .flat_map(|visible| visible.columns.iter())
            .find(|column| column.binding == binding)
            .map_or_else(|| "a column".to_string(), |column| format!("\"{}\"", column.name))
    }

    /// The name of a column with the name of its table, as `t.a`, or the column name alone when
    /// the column has no table.
    fn qualified_name_of(&self, binding: ColumnBinding, scope: &Scope) -> String {
        std::iter::once(scope)
            .chain(self.outer_scopes.iter().rev())
            .flat_map(|visible| visible.columns.iter())
            .find(|column| column.binding == binding)
            .map_or_else(String::new, |column| match column.table.as_str() {
                "" => column.name.clone(),
                table => format!("{table}.{}", column.name),
            })
    }

    /// Rewrites a bound expression into one the aggregate's output can answer.
    ///
    /// A subexpression that is one of the group expressions becomes a reference to that group. A
    /// column that is neither grouped nor inside an aggregate is the error every SQL user has seen,
    /// and it is reported here because this is the first point where it is knowable.
    pub(crate) fn over_aggregate(&mut self, expr: ExprRef, scope: &Scope) -> Result<ExprRef> {
        let Some(aggregation) = self.aggregation.as_ref() else {
            return Ok(expr);
        };
        let index = aggregation.index;
        let groups = aggregation.groups.clone();
        if let Some(first) = self.collated_group(expr) {
            return Ok(first);
        }
        for (at, group) in groups.iter().enumerate() {
            if self.same_expr(expr, *group) {
                let ty = self.plan.expr_type(*group).clone();
                return Ok(self.column(index, at, ty));
            }
        }
        let ty = self.plan.expr_type(expr).clone();
        match self.plan.expr(expr).clone() {
            Expr::Column(binding) if binding.table == index => Ok(expr),
            // A window result is not a column of the input and the grouping rule has nothing to say
            // about it. It reads the aggregate's output rather than the table's, which is why
            // `SELECT sum(count(i)) OVER () FROM t GROUP BY j` binds and `sum(i) OVER ()` over the
            // same block does not.
            Expr::Column(binding) if self.is_window_output(binding) => Ok(expr),
            // An unnest runs over the grouping too, and what it takes apart was checked against the
            // groups when it was bound.
            Expr::Column(binding) if self.is_unnest_output(binding) => Ok(expr),
            // The same argument for a query joined in above the grouping. `HAVING sum(x) > (SELECT
            // ...)` reads one row out of a query that has nothing to do with the groups, and the
            // join that produces it sits on top of the `Aggregate`, so what it produces is not one
            // of the grouped table's columns either.
            Expr::Column(binding) if self.joined_above.contains(&binding.table) => Ok(expr),
            // A column of an enclosing query is one value for the whole of this one, because this
            // query is evaluated once per outer row. It is a constant here in the sense the grouping
            // rule cares about, so it is allowed wherever a grouped column is and needs no group of
            // its own. The grouping rule is about columns of this query's own `FROM`, and a name
            // that resolved past it is not one of those. That is #995.
            Expr::Column(binding) if self.is_correlation(binding) => Ok(expr),
            // A query this block wrote that is still waiting to be joined in underneath the
            // grouping lands here as well, and the column the complaint should name is the one that
            // query correlates to rather than the column the query produces, which belongs to no
            // table anybody wrote. An uncorrelated query and a correlated one whose correlation is
            // grouped were both moved over the grouping by [`Self::lift_over_aggregate`] and are
            // not here, so what is left correlates to something this block neither grouped nor
            // aggregated, and that is an ordinary missing GROUP BY however far inside a query it
            // was written. That is #1032.
            Expr::Column(binding) => {
                let read = self.ungrouped_correlation(binding).unwrap_or(binding);
                let name = self.name_of(read, scope);
                // The pin words it differently in a `HAVING`, where it gives no hint.
                let error = if self.clause == "HAVING clause" {
                    Error::binder(format!(
                        "column {name} must appear in the GROUP BY clause or be used in an aggregate function"
                    ))
                } else {
                    Error::binder(format!(
                        "column {name} must appear in the GROUP BY clause or must be part of an aggregate function.\nEither add it to the GROUP BY list, or use ANY_VALUE({name}) if the exact value of {name} is not important."
                    ))
                };
                // PostgreSQL names the column with its table, and points at the column.
                let qualified = self.qualified_name_of(read, scope);
                Err(error
                    .state(SqlState::GROUPING_ERROR)
                    .pg(format!(
                        "column \"{qualified}\" must appear in the GROUP BY clause or be used in an aggregate function"
                    ))
                    .with_fallback_span(self.plan.expr_span(expr)))
            }
            Expr::Constant(_)
            | Expr::Aggregate { .. }
            | Expr::Window { .. }
            | Expr::LambdaParam(_) => Ok(expr),
            // The body is over the elements and the columns it captures, and a captured column is
            // held to the grouping rule like any other, which is the pin's error for
            // `list_transform(l, lambda x: x * k) ... GROUP BY l`.
            Expr::Lambda { table, params, body } => {
                let body = self.over_aggregate(body, scope)?;
                Ok(self.plan.add_expr(Expr::Lambda { table, params, body }, ty))
            }
            Expr::Cast { input, try_cast } => {
                let input = self.over_aggregate(input, scope)?;
                Ok(self.plan.add_expr(Expr::Cast { input, try_cast }, ty))
            }
            Expr::Compare { op, left, right } => {
                let left = self.over_aggregate(left, scope)?;
                let right = self.over_aggregate(right, scope)?;
                Ok(self.plan.add_expr(Expr::Compare { op, left, right }, ty))
            }
            Expr::Conjunction { op, children } => {
                let written = self.plan.expr_list(children).to_vec();
                let mut rewritten = Vec::with_capacity(written.len());
                for child in written {
                    rewritten.push(self.over_aggregate(child, scope)?);
                }
                let children = self.plan.add_expr_list(&rewritten);
                Ok(self.plan.add_expr(Expr::Conjunction { op, children }, ty))
            }
            Expr::Function { name, args } => {
                let written = self.plan.expr_list(args).to_vec();
                let mut rewritten = Vec::with_capacity(written.len());
                for arg in written {
                    rewritten.push(self.over_aggregate(arg, scope)?);
                }
                let args = self.plan.add_expr_list(&rewritten);
                Ok(self.plan.add_expr(Expr::Function { name, args }, ty))
            }
            Expr::Case { arms, otherwise } => {
                let written = self.plan.arm_list(arms).to_vec();
                let mut rewritten = Vec::with_capacity(written.len());
                for arm in written {
                    let when = self.over_aggregate(arm.when, scope)?;
                    let then = self.over_aggregate(arm.then, scope)?;
                    rewritten.push(rudb_plan::Arm { when, then });
                }
                let otherwise = match otherwise {
                    Some(expr) => Some(self.over_aggregate(expr, scope)?),
                    None => None,
                };
                let arms = self.plan.add_arms(&rewritten);
                Ok(self.plan.add_expr(Expr::Case { arms, otherwise }, ty))
            }
        }
    }

    /// Whether two bound expressions are the same expression, by shape rather than by reference.
    pub(crate) fn same_expr(&self, left: ExprRef, right: ExprRef) -> bool {
        self.plan.same_expr(left, right) && self.same_written_collation(left, right)
    }
}

/// The named parameters a table function call was written with.
///
/// A struct rather than the fields loose, because the seventeen DuckDB has on `read_parquet` and the
/// thirty on `read_csv` are all going to want somewhere to go, and because a call with none of them
/// written should read as the default of this rather than as a bare false somewhere.
///
/// The CSV half goes on to the reader and is opened with, here and again in the executor. The
/// Parquet half is answered here and nothing downstream sees it, which is what `binary_as_string`
/// turning a BLOB column into a VARCHAR one is.
#[derive(Debug, Default)]
struct Options {
    /// `binary_as_string`, which says an unannotated byte array column in a Parquet file holds
    /// text. The ClickBench file has twenty eight of those and every query reads them as strings.
    binary_as_string: bool,
    /// `all_varchar`, which reads every column of a CSV file as text rather than sniffing a type.
    all_varchar: bool,
    /// `file_row_number`, which adds a column holding each row's ordinal inside its own file.
    ///
    /// The one Parquet option here that the executor has to act on rather than the binder, since
    /// the column is not in the file and has to be counted as the rows come out of it.
    file_row_number: bool,
    /// `delim`, `sep`, `quote`, `escape` and `header`, which are what the sniffer would decide.
    given: Given,
}

impl Options {
    /// What these named parameters add up to.
    ///
    /// Each one was already checked against the function's list, so a name in here is a name that
    /// function takes and the value is already the type it wants. What is left is reading them, and
    /// the last one written wins, which is DuckDB's answer to `delim='|', delim=','` and was
    /// measured rather than assumed.
    fn of(written: &[(&'static str, Value, ExprRef)]) -> Result<Self> {
        let mut options = Self::default();
        for (parameter, value, _) in written {
            match (*parameter, value) {
                ("binary_as_string", Value::Boolean(on)) => options.binary_as_string = *on,
                ("all_varchar", Value::Boolean(on)) => options.all_varchar = *on,
                ("file_row_number", Value::Boolean(on)) => options.file_row_number = *on,
                _ => {}
            }
        }
        let named: Vec<(&str, Value)> =
            written.iter().map(|(parameter, value, _)| (*parameter, value.clone())).collect();
        options.given = csv_given(&named)?;
        Ok(options)
    }
}

/// The one file a read names, canonical, with what the file system says about it now, or `None`
/// for a read of several files or of something that is not a regular file with a UTF-8 name.
fn mirror_target(paths: &[String]) -> Option<(String, FileStamp)> {
    let [path] = paths else { return None };
    let canonical = std::fs::canonicalize(path).ok()?;
    let stamp = FileStamp::of(&canonical)?;
    Some((canonical.to_str()?.to_string(), stamp))
}

/// How the pin names the clause that the binder is in, in an error about what the clause cannot
/// contain. A join condition is the `WHERE` clause there, which is upstream's wording and not a
/// simplification: `ON sum(a.i) OVER () = b.i` is refused with the words a window in a `WHERE` is
/// refused with. An `OFFSET` is bound the way a `LIMIT` is and has its name.
fn pin_clause(clause: &'static str) -> &'static str {
    match clause {
        "JOIN condition" => "WHERE clause",
        "OFFSET clause" => "LIMIT clause",
        clause => clause,
    }
}

/// How PostgreSQL names the clause that the binder is in, in an error about what the clause cannot
/// contain. This is `ParseExprKindName` and the special cases of `check_agglevels_and_constraints`.
/// A clause that PostgreSQL does not have gives `None`.
fn postgres_clause(clause: &str) -> Option<&'static str> {
    Some(match clause {
        "WHERE clause" => "WHERE",
        "JOIN condition" => "JOIN conditions",
        "GROUP BY clause" => "GROUP BY",
        "HAVING clause" => "HAVING",
        "LIMIT clause" => "LIMIT",
        "OFFSET clause" => "OFFSET",
        "VALUES clause" => "VALUES",
        "table function arguments" => "functions in FROM",
        _ => return None,
    })
}

/// Whether a hash table can hold the values of a type, which the rows of a PostgreSQL recursive
/// `UNION` need, as `rudb_pgtypes::hashable` says for the PostgreSQL type of it.
fn hashable(ty: &LogicalType) -> bool {
    match ty {
        // `bit` and `bit varying`, which have no PostgreSQL type here yet. Their equality is the
        // one of a btree and not of a hash.
        LogicalType::Bit => false,
        LogicalType::List(element) | LogicalType::Array(element, _) => hashable(element),
        ty => crate::pgcalls::exact_oid(ty).is_none_or(rudb_pgtypes::hashable),
    }
}

/// The place of the first column that a query writes, or the place of the query when it writes no
/// column. PostgreSQL points there for an error about the columns of one side of a set operation.
fn first_column(ast: &Ast, query: ast::QueryRef) -> Span {
    let first = match ast.query(query).body {
        ast::QueryBody::Select(select) => {
            ast.target_list(ast.select(select).targets).first().map(|target| target.expr)
        }
        ast::QueryBody::SetOp { left, .. } => return first_column(ast, left),
        ast::QueryBody::Values(rows) => {
            ast.rows(rows).first().and_then(|&row| ast.expr_list(row).first().copied())
        }
        _ => None,
    };
    first.map_or_else(|| ast.query_span(query), |expr| ast.expr_span(expr))
}

/// Where PostgreSQL places an error about the column `at` of a set operation: at the column as
/// the leftmost `SELECT` under the operation writes it, or at its first column when the column
/// is not written one for one.
fn set_op_column(ast: &Ast, query: ast::QueryRef, at: usize, width: usize) -> Span {
    if let ast::QueryBody::SetOp { left, .. } = ast.query(query).body {
        return set_op_column(ast, left, at, width);
    }
    written_column(ast, query, at, width)
        .map_or_else(|| first_column(ast, query), |expr| ast.leftmost_span(expr))
}

/// A key of the `ORDER BY` of a `SELECT`: the column of the projection that it sorts on, the
/// expression as written, and whether the column is one that the query does not select.
struct Sorted {
    position: usize,
    written: ast::ExprRef,
    extra: bool,
}

/// The expression that a side of a set operation writes as its column `at`, when the side is a
/// plain `SELECT` whose targets are its columns one for one. A `UNION BY NAME` can have more
/// columns than either side, and a column past the end of this side is not written by it.
fn written_column(
    ast: &Ast,
    query: ast::QueryRef,
    at: usize,
    width: usize,
) -> Option<ast::ExprRef> {
    let ast::QueryBody::Select(select) = ast.query(query).body else {
        return None;
    };
    let targets = ast.target_list(ast.select(select).targets);
    let starred =
        targets.iter().any(|target| matches!(ast.expr(target.expr), ast::Expr::Star { .. }));
    if starred || targets.len() != width {
        return None;
    }
    targets.get(at).map(|target| target.expr)
}

/// What was written between the two sides of a set operation.
#[derive(Clone, Copy)]
struct Operator {
    /// `UNION`, `EXCEPT` or `INTERSECT`.
    op: SetOp,
    /// `ALL`, `DISTINCT`, or neither, which means `DISTINCT` everywhere it is allowed.
    quantifier: Quantifier,
    /// Whether `BY NAME` was written, which only `UNION` takes.
    by_name: bool,
}

/// One column of the result of a set operation, and where each side keeps it.
struct Merged {
    /// The name it comes out under, which is the left side's when both sides wrote it.
    name: String,
    /// What it is, after the two sides' types have met.
    ty: LogicalType,
    /// Which column of the left side it is, absent when only the right side wrote it.
    left: Option<usize>,
    /// Which column of the right side it is, absent when only the left side wrote it.
    right: Option<usize>,
}

/// Matches the two sides of an ordinary set operation, which is first column to first column.
///
/// The names are the left side's, so `SELECT a FROM t UNION SELECT b FROM u` comes out as `a`.
fn match_by_position(left: &Scope, right: &Scope, op: SetOp) -> Result<Vec<Merged>> {
    if left.len() != right.len() {
        let context = op.name();
        return Err(Error::binder(format!(
            "Set operations can only apply to expressions with the same number of result columns, but left side has {} and right side has {}",
            left.len(),
            right.len()
        ))
        .state(SqlState::SYNTAX_ERROR)
        .pg(format!("each {context} query must have the same number of columns")));
    }
    let mut merged = Vec::with_capacity(left.len());
    for (at, (held, other)) in left.columns.iter().zip(&right.columns).enumerate() {
        merged.push(Merged {
            name: held.name.clone(),
            ty: meet(&held.ty, &other.ty)?,
            left: Some(at),
            right: Some(at),
        });
    }
    Ok(merged)
}

/// Matches the two sides of a `UNION BY NAME`, which is by column name and not by position.
///
/// The result has the left side's columns in the order the left side wrote them, then the right
/// side's columns the left side did not write, in the order the right side wrote them. A column
/// only one side wrote is that side's type and the other side fills it with a null, which is why
/// nothing here needs the two sides to be the same width. Names match without regard to case, and
/// the spelling that comes out is the left side's, both of which follow the rest of the engine.
fn match_by_name(left: &Scope, right: &Scope) -> Result<Vec<Merged>> {
    named_once(left)?;
    named_once(right)?;
    let mut merged = Vec::with_capacity(left.len() + right.len());
    for (at, held) in left.columns.iter().enumerate() {
        let other = right.columns.iter().position(|column| same_name(&column.name, &held.name));
        let ty = match other {
            Some(other) => meet(&held.ty, &right.columns[other].ty)?,
            None => held.ty.clone(),
        };
        merged.push(Merged { name: held.name.clone(), ty, left: Some(at), right: other });
    }
    for (at, held) in right.columns.iter().enumerate() {
        if left.columns.iter().any(|column| same_name(&column.name, &held.name)) {
            continue;
        }
        merged.push(Merged {
            name: held.name.clone(),
            ty: held.ty.clone(),
            left: None,
            right: Some(at),
        });
    }
    Ok(merged)
}

/// Refuses a side of a `UNION BY NAME` that wrote one name twice.
///
/// Matching by name needs the name to say which column, and a side that wrote `a` twice has no
/// answer to give. An ordinary union does not care, because there the position says which column.
/// The doubled quotes around the name are the reference binary's and not a mistake here.
fn named_once(scope: &Scope) -> Result<()> {
    for (at, held) in scope.columns.iter().enumerate() {
        if scope.columns[..at].iter().any(|column| same_name(&column.name, &held.name)) {
            return Err(Error::binder(format!(
                "UNION (ALL) BY NAME operation doesn't support duplicate names in the SELECT list - the name \"\"{}\"\" occurs multiple times",
                held.name
            )));
        }
    }
    Ok(())
}

/// The one type a column of a set operation comes out as, given what each side wrote.
fn meet(left: &LogicalType, right: &LogicalType) -> Result<LogicalType> {
    forced(left, right).ok_or_else(|| {
        Error::binder(format!(
            "Cannot combine a column of type {left} with a column of type {right} in a set operation"
        ))
    })
}

/// The type two columns of a set operation are brought to, which is the pin's `ForceMaxLogicalType`.
///
/// Two types with a type in common meet there. Two without one are forced to the one the pin ranks
/// higher, or the left one when they rank the same, and a row of the other side then fails its
/// cast. So `SELECT 1 UNION ALL SELECT 'a'` is a `VARCHAR`, `SELECT 1 UNION ALL SELECT DATE
/// '2020-01-01'` is a `DATE` whose first row fails to cast, and a side with no rows or only nulls
/// never notices. Lists, maps and structs are forced child by child, an enum is forced as the
/// string it reads as and `JSON` is kept whatever the other side is, all of which was measured on
/// every pair of types the pin has a value of.
fn forced(left: &LogicalType, right: &LogicalType) -> Option<LogicalType> {
    if let Some(met) = left.promote(right) {
        return Some(met);
    }
    Some(match (left, right) {
        (LogicalType::Json, _) | (_, LogicalType::Json) => LogicalType::Json,
        (LogicalType::Enum(_), other) => return forced(&LogicalType::Varchar, other),
        (other, LogicalType::Enum(_)) => return forced(other, &LogicalType::Varchar),
        (LogicalType::List(one), LogicalType::List(other)) => {
            LogicalType::list(forced(one, other)?)
        }
        (LogicalType::Array(one, size), LogicalType::Array(other, length)) => {
            LogicalType::array(forced(one, other)?, *size.max(length))
        }
        (LogicalType::Map(key, value), LogicalType::Map(other_key, other_value)) => {
            LogicalType::map(forced(key, other_key)?, forced(value, other_value)?)
        }
        (LogicalType::Struct(one), LogicalType::Struct(other)) => {
            forced_struct(one, other).map_or_else(|| left.clone(), LogicalType::Struct)
        }
        (LogicalType::Union(_) | LogicalType::AggregateState(_) | LogicalType::Type, _)
        | (_, LogicalType::Union(_) | LogicalType::AggregateState(_) | LogicalType::Type) => {
            return None;
        }
        _ => crate::expr::forced_type(left, right),
    })
}

/// Two structs forced to one, field by field, the way [`LogicalType::promote`] meets them.
///
/// Named structs are matched by name and keep every field either has. An unnamed struct is matched
/// by position and takes the other side's names, and when the two are not the same size there is
/// no answer, which leaves the left side's type for the right side to fail its cast to, in the
/// pin's words.
fn forced_struct(left: &[Field], right: &[Field]) -> Option<Vec<Field>> {
    if Field::unnamed(left) || Field::unnamed(right) {
        if left.len() != right.len() {
            return None;
        }
        let named = if Field::unnamed(left) { right } else { left };
        let mut fields = Vec::with_capacity(left.len());
        for ((one, other), name) in left.iter().zip(right).zip(named) {
            fields.push(Field::new(name.name.clone(), forced(&one.ty, &other.ty)?));
        }
        return Some(fields);
    }
    let mut fields = left.to_vec();
    for field in right {
        match fields.iter_mut().find(|one| one.name.eq_ignore_ascii_case(&field.name)) {
            Some(one) => one.ty = forced(&one.ty, &field.ty)?,
            None => fields.push(field.clone()),
        }
    }
    Some(fields)
}

/// DuckDB's complaint about a named parameter that was given a null, which is a different sentence
/// for almost every parameter.
///
/// Three of them were measured on `v2.0.0-dev84237` and no two agree: `binary_as_string` is the
/// first, `all_varchar` is the second and `header` is the third. They read like three people each
/// writing the message in front of them, which is what they are, and a harness that compares error
/// text compares all of it. Anything not measured gets the first one, which is the most general of
/// the three.
fn null_parameter(function: TableFunction, parameter: &str) -> String {
    if function.json().is_some() {
        return format!("Cannot use NULL as argument to key \"{parameter}\"");
    }
    match parameter {
        "header" => format!("\"{parameter}\" expects a non-null boolean value (e.g. TRUE or 1)"),
        "all_varchar" => format!("{} \"{parameter}\" cannot be NULL", function.name()),
        _ => format!("Cannot use NULL as argument to \"{parameter}\""),
    }
}

/// The complaint about a `REPLACE` entry that named a column the star did not stand for.
///
/// It reads like the complaint about any other name that is not there, down to the list of names
/// that are, because from the writer's side it is the same mistake.
pub(crate) fn missing_replacement(name: &str, input: &Scope) -> Error {
    Error::binder(format!(
        "Column \"{name}\" in REPLACE list not found in FROM clause{}",
        input.candidates()
    ))
}

/// Whether a type is one `fill` can interpolate over, which is the pin's phrase for it.
///
/// The pin refuses `fill` with `FILL argument must support subtraction` and its sort key with
/// `FILL ordering must support subtraction`, and the two lists are not the same list, which is why
/// this takes a flag rather than answering one question. Every number is on both, so are `DATE`,
/// `TIME` and the two timestamps, and `TIME WITH TIME ZONE` is a sort key there but not an
/// argument. `INTERVAL` is on neither, which is worth saying out loud because an interval does
/// subtract: the sentence names subtraction and the rule is narrower than the sentence.
fn subtractable(ty: &LogicalType, ordering: bool) -> bool {
    if ty.is_numeric() {
        return true;
    }
    match ty {
        LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampS
        | LogicalType::TimestampMs
        | LogicalType::TimestampNs
        | LogicalType::TimestampTz => true,
        LogicalType::TimeTz => ordering,
        _ => false,
    }
}

/// Refuses a `fill` call the way the pin refuses one, in the pin's order.
///
/// The order was measured and it is not the order the clauses are written in. A `fill` over a
/// `VARCHAR` with no `ORDER BY` at all complains about the argument, so the argument is looked at
/// before the sort key is counted, and a `fill` with `DISTINCT` and no `ORDER BY` complains about
/// the `ORDER BY`, so the count comes before the clauses. `IGNORE NULLS` is refused here rather
/// than being answered as a no-op, since there is nothing for it to skip: `fill` is the one window
/// whose whole job is the nulls.
fn refuse_fill(
    argument: &LogicalType,
    order: &[LogicalType],
    distinct: bool,
    ignore_nulls: bool,
) -> Result<()> {
    if !subtractable(argument, false) {
        return Err(Error::binder("FILL argument must support subtraction"));
    }
    let [key] = order else {
        return Err(Error::binder("FILL functions must have only one ORDER BY expression"));
    };
    if !subtractable(key, true) {
        return Err(Error::binder("FILL ordering must support subtraction"));
    }
    if distinct {
        return Err(Error::binder(
            "DISTINCT is not implemented for the window function \"\"fill\"\"",
        ));
    }
    if ignore_nulls {
        return Err(Error::binder(
            "RESPECT/IGNORE NULLS is not supported for the window function \"fill\"",
        ));
    }
    Ok(())
}

/// Resolves the call written inside an `OVER`.
///
/// Every aggregate is also a window, which is why this goes through the same signature table the
/// aggregate path uses, and the ranking windows go through it too because they are rows in the same
/// table. Everything else is one of three refusals, and all three are the reference binary's: a name
/// it knows as a scalar and a name it does not know at all each get their own sentence there.
fn window_signature(name: &str, types: &[LogicalType]) -> Result<Resolved> {
    match kind_of(name) {
        Some(FunctionKind::Aggregate | FunctionKind::Window) => resolve(name, types),
        Some(FunctionKind::Scalar) => {
            Err(Error::catalog(format!("{name} is not an aggregate function")))
        }
        None => Err(Error::catalog(format!("Aggregate Function with name {name} does not exist!"))),
    }
}

/// The name the executor knows a call by, which is the name it was resolved to except for
/// `min(x, n)` and `max(x, n)`.
///
/// Those two are the pin's `arg_min(x, x, n)` and `arg_max(x, x, n)` with their own words for a bad
/// `n`, so the value goes in twice and the call goes to the executor under a name of its own,
/// because every path there that knows `max` knows it as one value over one argument.
fn top_values<'a>(name: &'a str, args: &mut Vec<ExprRef>) -> &'a str {
    let top = match name {
        "min" => "min_top",
        "max" => "max_top",
        _ => return name,
    };
    let &[value, n] = &args[..] else { return name };
    *args = vec![value, value, n];
    top
}

/// Whether a call is to one of the ordered-set aggregates, and whether it takes the value it reads
/// from its one `ORDER BY` key because it does not write one.
fn ordered_set(name: &str, written: usize, sorted: &[ast::OrderItem]) -> (bool, bool) {
    let name = name.to_ascii_lowercase();
    let wants = match name.as_str() {
        "quantile_cont" | "quantile_disc" | "quantile" => 1,
        "mode" => 0,
        _ => return (false, false),
    };
    (true, sorted.len() == 1 && written == wants)
}

/// A quantile fraction counted from the other end.
fn negated(value: &Value) -> Value {
    match *value {
        Value::Decimal { unscaled, width, scale } => {
            Value::Decimal { unscaled: -unscaled, width, scale }
        }
        Value::Double(share) => Value::Double(-share),
        Value::Float(share) => Value::Float(-share),
        ref whole => match share(whole) {
            Some(share) => Value::Double(-share),
            None => whole.clone(),
        },
    }
}

/// A numeric fraction as a double, or `None` for a value that is not a number.
#[expect(
    clippy::cast_precision_loss,
    reason = "a fraction is compared with -1 and 1, which a double holds exactly"
)]
fn share(value: &Value) -> Option<f64> {
    Some(match *value {
        Value::TinyInt(v) => f64::from(v),
        Value::SmallInt(v) => f64::from(v),
        Value::Integer(v) => f64::from(v),
        Value::BigInt(v) => v as f64,
        Value::HugeInt(v) => v as f64,
        Value::UTinyInt(v) => f64::from(v),
        Value::USmallInt(v) => f64::from(v),
        Value::UInteger(v) => f64::from(v),
        Value::UBigInt(v) => v as f64,
        Value::UHugeInt(v) => v as f64,
        Value::Float(v) => f64::from(v),
        Value::Double(v) => v,
        Value::Decimal { unscaled, scale, .. } => unscaled as f64 / 10f64.powi(i32::from(scale)),
        _ => return None,
    })
}

/// Whether a `GROUP BY` groups on every target that is not an aggregate, which `GROUP BY ALL` and
/// `GROUP BY *` on its own both ask for. A star with a list on it, or next to anything else, is
/// turned down the way the pin turns it down while it expands the stars.
fn groups_everything(ast: &Ast, select: &ast::Select) -> Result<bool> {
    let written = ast.expr_list(select.group_by);
    let alone = match *written {
        [only] => match ast.expr(only) {
            ast::Expr::Star { replacements, .. } => {
                replacements.is_empty() && ast.star_lists(only) == ast::StarLists::default()
            }
            _ => false,
        },
        _ => false,
    };
    if !alone && written.iter().any(|&item| crate::columns::has_star(ast, item)) {
        return Err(Error::binder("STAR expression is not supported here"));
    }
    Ok(select.group_by_all || alone)
}

/// Whether anything under `node` calls a function that answers differently each time.
///
/// It looks at the expressions a side of a join is written with, which is a projection, a filter,
/// a grouping, a list of rows, a table function's arguments or a join's condition, and at
/// everything under those. See [`Binder::bind_held_side`].
fn volatile_node(plan: &Plan, node: NodeRef) -> bool {
    let any = |slice| plan.expr_list(slice).iter().any(|&expr| crate::expr::volatile(plan, expr));
    let here = match *plan.node(node) {
        Node::Project { exprs, .. } => any(exprs),
        Node::Filter { predicate, .. } => crate::expr::volatile(plan, predicate),
        Node::Aggregate { groups, aggregates, .. } => any(groups) || any(aggregates),
        Node::Values { rows, .. } => plan.row_list(rows).iter().any(|&row| any(row)),
        Node::TableFunction { args, .. } => any(args),
        Node::Join { conditions, .. } | Node::DependentJoin { conditions, .. } => any(conditions),
        _ => false,
    };
    here || plan.node(node).children().into_iter().flatten().any(|child| volatile_node(plan, child))
}

/// What a `JOIN BY (TYPE MARK)` whose condition a mark join cannot answer says, in the corpus's
/// words.
fn unsupported_mark() -> Error {
    Error::not_implemented("Unsupported explicit MARK join conditions".to_string())
}
