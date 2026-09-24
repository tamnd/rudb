//! The catalog tables: what is attached, what is in it, and what somebody created there.
//!
//! `duckdb_databases()`, `duckdb_schemas()`, `duckdb_tables()`, `duckdb_views()` and
//! `duckdb_columns()`, the metadata tables whose rows come out of the catalog rather than out of a
//! list this binary was compiled with.
//! `rudb_functions::entrycatalog` has their columns, because the binder resolves the call and needs
//! the columns before there is a catalog in reach, and the rows are here because this is where one is.
//!
//! `duckdb_columns()` lists a view's columns as well as a table's. What it reads is the list the
//! binder wrote down the last time the view was bound, which is a cache that goes stale, and that is
//! upstream's design rather than a shortcut taken here. See `rudb_catalog::View` for the measurement.
//!
//! `duckdb_sequences()` sits beside them and lists each sequence with the counter as it is now.
//!
//! `duckdb_views()` is the fifth and it reports the statement written back out, which the binder
//! wrote down at `CREATE VIEW` using `rudb_parse::deparse`. The pin reports a deparse there too
//! rather than the text somebody typed, which is measured, so the column is a comparison like any
//! other and not a place where the two are allowed to differ.
//!
//! Every table is walked in catalog order, which is the order things were attached and created in.
//! That is not sorted and it is not reproduced from the pin either, whose order is its own catalog's.
//! A client that wants an order writes one, which is what the two corpus records that read these
//! tables already do.
//!
//! Most of the columns are a fact about a database rudb does not have yet. `path` is null because
//! every database here is in memory, `readonly`, `encrypted` and `cipher` say so, and `options` is
//! empty because `ATTACH` takes none. The day there is a file behind a database these columns say
//! something.
//!
//! `internal` says whether the engine made the entry rather than a person. A session has three
//! databases, `memory` to create in and `system` and `temp` that the engine owns, and for two of
//! them the answer is the database's own: nothing in `memory` is internal and everything in
//! `system` is. `temp` is the third and it is the one that comes apart, because the database is the
//! engine's and every table in it was written by somebody, so an entry there is not internal even
//! though the database and its schema are. The pin answers the same way and `entry_internal` below
//! is where that is said once. See `rudb_catalog::system` for what is in `system`.
//!
//! `sql`, `parent_schema` and `parent_schema_oid` are null on every schema row. Upstream's are null
//! too on everything it returns from a fresh session, because a schema created by `CREATE SCHEMA`
//! has no stored text and nothing nests schemas.

use rudb_catalog::{Catalog, Constraint, Database, Schema, TEMP_CATALOG, Table};
use rudb_common::{LogicalType, Result, Value};
use rudb_functions::{
    DUCKDB, canonical, column_fields, constraint_fields, database_fields, index_fields,
    numeric_facts, schema_fields, sequence_fields, show_database_fields, show_expanded_fields,
    show_table_fields, table_fields, type_oid, view_fields,
};
use rudb_parse::{Kind, quoted, tokenize};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every attached database, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn databasenames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::with_capacity(catalog.databases().len());
    for database in catalog.databases() {
        rows.push(vec![
            text(database.name()),
            Value::BigInt(database.oid()),
            database.path().map_or(Value::Null, text),
            Value::Null,
            empty(),
            Value::Boolean(database.internal()),
            text(DUCKDB),
            Value::Boolean(database.read_only()),
            Value::Boolean(false),
            Value::Null,
            empty(),
        ]);
    }
    Metadata::new("duckdb_databases", &database_fields(), &rows, plan, index, columns)
}

/// Every schema in every attached database, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn schemanames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for schema in database.schemas() {
            rows.push(vec![
                Value::BigInt(schema.oid()),
                text(database.name()),
                Value::BigInt(database.oid()),
                text(schema.name()),
                Value::Null,
                empty(),
                // True on every schema upstream returns from a fresh session, including `memory.main`
                // which is the one rudb has. A schema somebody made with `CREATE SCHEMA` is false
                // there, and the day rudb can tell the two apart this stops being a constant.
                Value::Boolean(true),
                Value::Null,
                Value::Null,
                Value::Null,
            ]);
        }
    }
    Metadata::new("duckdb_schemas", &schema_fields(), &rows, plan, index, columns)
}

/// Every base table in the catalog, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn tablenames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for (database, schema, table) in entries(catalog) {
        let count = i64::try_from(table.columns().len()).unwrap_or(i64::MAX);
        rows.push(vec![
            text(database.name()),
            Value::BigInt(database.oid()),
            text(schema.name()),
            Value::BigInt(schema.oid()),
            text(&table.name().table),
            Value::BigInt(table.oid()),
            Value::Null,
            empty(),
            Value::Boolean(entry_internal(database)),
            Value::Boolean(table.name().temporary()),
            Value::Boolean(table.keys().iter().any(|key| key.primary)),
            Value::BigInt(i64::try_from(table.rows().len()).unwrap_or(i64::MAX)),
            Value::BigInt(count),
            // The pin backs every key and every foreign key with an index of its own, and counts
            // those along with the ones `CREATE INDEX` made.
            Value::BigInt(
                i64::try_from(table.keys().len() + table.foreign().len() + table.indexes().len())
                    .unwrap_or(i64::MAX),
            ),
            Value::BigInt(i64::try_from(table.checks().len()).unwrap_or(i64::MAX)),
            text(&create_table(table)),
        ]);
    }
    Metadata::new("duckdb_tables", &table_fields(), &rows, plan, index, columns)
}

/// Every view in the catalog, in the columns the plan asked for.
///
/// `sql` is the statement written back out, which the binder wrote down at creation. `column_count`
/// is the length of the column cache on the entry, so it is as stale as the cache is, which is what
/// upstream reports too.
///
/// `is_bound` says whether that cache holds anything, and `column_count` is null when it does not.
/// A view somebody wrote is bound at `CREATE VIEW` and cannot get into the catalog without one, so
/// the two of them only say no for a view the engine ships with, which goes in unbound and is bound
/// at the first read. That is the pin's answer as well, measured on a fresh session and again after
/// reading one of them.
///
/// `temporary` is read off the database the view is in, which is true for `temp` where it means
/// what it says and true for `system` where it reads oddly, and the pin prints true for all 47 of
/// those. `internal` is the same question asked of the entry rather than of the database, so a
/// temporary view is not internal and a shipped one is.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn viewnames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for schema in database.schemas() {
            for view in schema.views() {
                let columns = view.columns();
                rows.push(vec![
                    text(database.name()),
                    Value::BigInt(database.oid()),
                    text(schema.name()),
                    Value::BigInt(schema.oid()),
                    text(&view.name().table),
                    Value::BigInt(view.oid()),
                    Value::Null,
                    empty(),
                    Value::Boolean(entry_internal(database)),
                    Value::Boolean(database.internal()),
                    if columns.is_empty() {
                        Value::Null
                    } else {
                        Value::BigInt(i64::try_from(columns.len()).unwrap_or(i64::MAX))
                    },
                    text(view.statement()),
                    Value::Boolean(!columns.is_empty()),
                ]);
            }
        }
    }
    Metadata::new("duckdb_views", &view_fields(), &rows, plan, index, columns)
}

/// Every index `CREATE INDEX` made, in the columns the plan asked for.
///
/// The indexes behind keys are not here, which is the pin's rule too: `is_primary` is false on
/// every row there is.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn indexnames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for (database, schema, table) in entries(catalog) {
        for held in table.indexes() {
            rows.push(vec![
                text(database.name()),
                Value::BigInt(database.oid()),
                text(schema.name()),
                Value::BigInt(schema.oid()),
                text(&held.name),
                Value::BigInt(held.oid),
                text(&table.name().table),
                Value::BigInt(table.oid()),
                Value::Null,
                empty(),
                Value::Boolean(held.unique),
                Value::Boolean(false),
                text(&held.expressions),
                text(&held.sql),
            ]);
        }
    }
    Metadata::new("duckdb_indexes", &index_fields(), &rows, plan, index, columns)
}

/// Every constraint of every table, in the columns the plan asked for.
///
/// The pin's order, which is its tables by name within each schema, each table's constraints in
/// the order they were written with the `NOT NULL` nobody wrote on a primary key's columns last,
/// and `constraint_index` counting across all of it rather than starting again at each table.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn constraintnames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for schema in database.schemas() {
            let mut tables: Vec<&Table> = schema.tables().iter().collect();
            tables.sort_by(|left, right| left.name().table.cmp(&right.name().table));
            for table in tables {
                for held in table.constraints() {
                    let mut row = vec![
                        text(database.name()),
                        Value::BigInt(database.oid()),
                        text(schema.name()),
                        Value::BigInt(schema.oid()),
                        text(&table.name().table),
                        Value::BigInt(table.oid()),
                        Value::BigInt(rows.len() as i64),
                    ];
                    row.extend(constraint_row(catalog, table, held));
                    rows.push(row);
                }
            }
        }
    }
    Metadata::new("duckdb_constraints", &constraint_fields(), &rows, plan, index, columns)
}

/// The columns of one constraint from `constraint_type` on.
fn constraint_row(catalog: &Catalog, table: &Table, held: Constraint) -> Vec<Value> {
    let name_of = |at: usize| table.columns()[at].name.clone();
    let list = |names: &[String]| names.iter().map(|name| quoted(name)).collect::<Vec<_>>();
    let (kind, words, expression, places, referenced) = match held {
        Constraint::Key(at) => {
            let key = &table.keys()[at];
            let names: Vec<String> = key.columns.iter().map(|&at| name_of(at)).collect();
            let (kind, word) =
                if key.primary { ("PRIMARY KEY", "pkey") } else { ("UNIQUE", "key") };
            let text = format!("{kind}({})", list(&names).join(", "));
            ((kind, text), word, None, key.columns.clone(), None)
        }
        Constraint::Check(at) => {
            let expression = table.checks()[at].clone();
            let places = check_columns(table, &expression);
            let text = format!("CHECK({expression})");
            (("CHECK", text), "check", Some(expression), places, None)
        }
        Constraint::Foreign(at) => {
            let foreign = &table.foreign()[at];
            let target = catalog.table(&foreign.table).ok().unwrap_or(table);
            let wanted: Vec<String> =
                foreign.referenced.iter().map(|&at| target.columns()[at].name.clone()).collect();
            let names: Vec<String> = foreign.columns.iter().map(|&at| name_of(at)).collect();
            // The pin names the schema of the held table unless it is `main`.
            let mut held = quoted(&foreign.table.table);
            if !foreign.table.schema.eq_ignore_ascii_case("main") {
                held = format!("{}.{held}", quoted(&foreign.table.schema));
            }
            let text = format!(
                "FOREIGN KEY ({}) REFERENCES {}({})",
                list(&names).join(", "),
                held,
                list(&wanted).join(", ")
            );
            let referenced = (foreign.table.table.clone(), wanted);
            (("FOREIGN KEY", text), "fkey", None, foreign.columns.clone(), Some(referenced))
        }
        Constraint::NotNull(at) => {
            (("NOT NULL", "NOT NULL".to_string()), "not_null", None, vec![at], None)
        }
    };
    let names: Vec<String> = places.iter().map(|&at| name_of(at)).collect();
    let mut constraint = format!("{}_", table.name().table);
    for name in &names {
        constraint.push_str(&name.to_lowercase());
        constraint.push('_');
    }
    for name in referenced.iter().flat_map(|(_, wanted)| wanted) {
        constraint.push_str(&name.to_lowercase());
        constraint.push('_');
    }
    constraint.push_str(words);
    let (referenced_table, wanted) = match referenced {
        Some((table, wanted)) => (text(&table), wanted),
        None => (Value::Null, Vec::new()),
    };
    vec![
        text(kind.0),
        text(&kind.1),
        expression.map_or(Value::Null, Value::Varchar),
        Value::List {
            element: LogicalType::BigInt,
            values: places.iter().map(|&at| Value::BigInt(at as i64)).collect(),
        },
        names_of(names.into_iter()),
        text(&constraint),
        referenced_table,
        names_of(wanted.into_iter()),
    ]
}

/// The places of the columns a `CHECK` names, in the order it names them and once per mention.
///
/// Read off the tokens of its text: a word that names a column and is not followed by a `(`,
/// which would make it a function.
fn check_columns(table: &Table, expression: &str) -> Vec<usize> {
    let Ok(tokens) = tokenize(expression) else {
        return Vec::new();
    };
    let mut places = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        let word = &expression[token.start as usize..token.end as usize];
        let word = match token.kind {
            Kind::Identifier | Kind::Keyword => word.to_string(),
            Kind::QuotedIdentifier => word.trim_matches('"').replace("\"\"", "\""),
            _ => continue,
        };
        let call = tokens.get(at + 1).is_some_and(|next| {
            next.kind == Kind::Operator
                && &expression[next.start as usize..next.end as usize] == "("
        });
        if call {
            continue;
        }
        if let Some(place) =
            table.columns().iter().position(|field| field.name.eq_ignore_ascii_case(&word))
        {
            places.push(place);
        }
    }
    places
}

/// Every sequence in the catalog, in the columns the plan asked for.
///
/// `start_value` is what the sequence was created with while `sql` writes the counter as it is now
/// in its `START`, so the statement makes a sequence that carries on from where this one got to.
/// The name goes into `sql` bare, without quotes and without its schema, which is what the pin
/// writes as well.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn sequencenames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for schema in database.schemas() {
            for sequence in schema.sequences() {
                let counter = sequence.counter();
                let options = counter.options();
                let sql = format!(
                    "CREATE SEQUENCE {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START {} {};",
                    sequence.name().table,
                    options.increment,
                    options.min,
                    options.max,
                    counter.counter(),
                    if options.cycle { "CYCLE" } else { "NO CYCLE" }
                );
                rows.push(vec![
                    text(database.name()),
                    Value::BigInt(database.oid()),
                    text(schema.name()),
                    Value::BigInt(schema.oid()),
                    text(&sequence.name().table),
                    Value::BigInt(sequence.oid()),
                    Value::Null,
                    empty(),
                    Value::Boolean(database.name() == TEMP_CATALOG),
                    Value::BigInt(options.start),
                    Value::BigInt(options.min),
                    Value::BigInt(options.max),
                    Value::BigInt(options.increment),
                    Value::Boolean(options.cycle),
                    counter.last().map_or(Value::Null, Value::BigInt),
                    text(&sql),
                ]);
            }
        }
    }
    Metadata::new("duckdb_sequences", &sequence_fields(), &rows, plan, index, columns)
}

/// Every column of every base table, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn columnnames(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        for schema in database.schemas() {
            for table in schema.tables() {
                for (at, column) in table.columns().iter().enumerate() {
                    rows.push(column_row(
                        database,
                        schema,
                        &table.name().table,
                        table.oid(),
                        at,
                        &column.name,
                        &column.ty,
                        !column.not_null,
                        table.default(at),
                    ));
                }
            }
            for view in schema.views() {
                for (at, field) in view.columns().iter().enumerate() {
                    // Nullable on every column of every view the pin returns, including one that
                    // reads a `NOT NULL` column straight through, so it is a constant here and not a
                    // fact carried over from the table underneath.
                    rows.push(column_row(
                        database,
                        schema,
                        &view.name().table,
                        view.oid(),
                        at,
                        &field.name,
                        &field.ty,
                        true,
                        None,
                    ));
                }
            }
        }
    }
    Metadata::new("duckdb_columns", &column_fields(), &rows, plan, index, columns)
}

/// Whether an entry in this database is one the engine made rather than one somebody wrote.
///
/// Not the same question as whether the database is internal, and `temp` is the whole of the
/// difference. See the note at the top of this file.
fn entry_internal(database: &Database) -> bool {
    database.internal() && !database.name().eq_ignore_ascii_case(TEMP_CATALOG)
}

/// One row of `duckdb_columns()`, which is the same twenty one columns for a table and for a view.
#[expect(clippy::too_many_arguments, reason = "a row of a twenty one column table")]
fn column_row(
    database: &Database,
    schema: &Schema,
    table: &str,
    oid: i64,
    at: usize,
    name: &str,
    ty: &LogicalType,
    nullable: bool,
    default: Option<&str>,
) -> Vec<Value> {
    let (precision, radix, scale) = numeric_facts(ty);
    vec![
        text(database.name()),
        Value::BigInt(database.oid()),
        text(schema.name()),
        Value::BigInt(schema.oid()),
        text(table),
        Value::BigInt(oid),
        text(name),
        // One based, which is the pin's answer and not the position in the vector.
        Value::Integer(i32::try_from(at + 1).unwrap_or(i32::MAX)),
        Value::Null,
        Value::Boolean(entry_internal(database)),
        default.map_or(Value::Null, text),
        Value::Boolean(nullable),
        text(&ty.to_string()),
        type_oid(&canonical(ty)).map_or(Value::Null, Value::BigInt),
        // Null even on a VARCHAR the DDL gave a length, because the pin reports null there too:
        // DuckDB parses the length modifier and then drops it, so by the time a column is in a
        // catalog there is no length left to report.
        Value::Null,
        precision.map_or(Value::Null, Value::Integer),
        radix.map_or(Value::Null, Value::Integer),
        scale.map_or(Value::Null, Value::Integer),
        empty(),
        Value::Boolean(false),
        Value::Null,
    ]
}

/// The name of everything an unqualified name can reach, which is what `PRAGMA show_tables` is.
///
/// Sorted by name, and views sit among the tables rather than after them, which is the pin's answer
/// and the only sensible one for a list whose whole purpose is to say what a name will find.
///
/// The search path is `temp.main` and then the default schema of the default database, which is
/// the front of the path `Catalog::candidates` walks and the two schemas of it that can hold
/// anything a person wrote. The pin lists both here too, so a temporary table shows up in this list
/// beside a stored one and the writer sees the name that a bare `SELECT` is going to find. The day
/// `CREATE SCHEMA` lands this is the function that has to grow a real search path rather than a
/// lookup of two names.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn showtables(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut names = Vec::new();
    for database in catalog.databases() {
        let reachable = database.name().eq_ignore_ascii_case(catalog.default_catalog())
            || database.name().eq_ignore_ascii_case(TEMP_CATALOG);
        if !reachable {
            continue;
        }
        for schema in database.schemas() {
            if !schema.name().eq_ignore_ascii_case(catalog.default_schema()) {
                continue;
            }
            names.extend(schema.tables().iter().map(|table| table.name().table.clone()));
            names.extend(schema.views().iter().map(|view| view.name().table.clone()));
        }
    }
    names.sort();
    let rows: Vec<Vec<Value>> = names.iter().map(|name| vec![text(name)]).collect();
    Metadata::new("pragma_show_tables", &show_table_fields(), &rows, plan, index, columns)
}

/// The name of everything that is attached, which is what `PRAGMA show_databases` is.
///
/// The databases the engine owns are left out, because the pin leaves its own out too and the
/// question the statement asks is which databases a client can write a name into.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn showdatabases(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut names: Vec<&str> = catalog
        .databases()
        .iter()
        .filter(|database| !database.internal())
        .map(Database::name)
        .collect();
    names.sort_unstable();
    let rows: Vec<Vec<Value>> = names.iter().map(|name| vec![text(name)]).collect();
    Metadata::new("pragma_show_databases", &show_database_fields(), &rows, plan, index, columns)
}

/// Every table and view anywhere with its columns beside it, which is `PRAGMA show_tables_expanded`.
///
/// Sorted by database, then schema, then name, and the engine's own databases are left out the way
/// `PRAGMA show_databases` leaves them out. The two list columns are the one place in these tables
/// where a table's shape is reported without a join, and the types are written the way the type
/// prints rather than the way it was declared.
///
/// `temporary` is read off the database the entry is in. `temp` is listed here even though
/// `PRAGMA show_databases` leaves it out, which is the pin's answer to both and is the difference
/// between asking which databases a name can be written into and asking what is in them.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn showtablesexpanded(
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    let mut rows = Vec::new();
    for database in catalog.databases() {
        if entry_internal(database) {
            continue;
        }
        for schema in database.schemas() {
            for table in schema.tables() {
                rows.push(expanded_row(database, schema, &table.name().table, table.columns()));
            }
            for view in schema.views() {
                rows.push(expanded_row(database, schema, &view.name().table, &view.columns()));
            }
        }
    }
    // On the three name columns, which are the three the pin orders by. They are varchars on every
    // row, so the key is those three strings and nothing in it can be null.
    rows.sort_by_key(|row| {
        let at = |index: usize| match &row[index] {
            Value::Varchar(name) => name.clone(),
            _ => String::new(),
        };
        (at(0), at(1), at(2))
    });
    Metadata::new(
        "pragma_show_tables_expanded",
        &show_expanded_fields(),
        &rows,
        plan,
        index,
        columns,
    )
}

/// One row of `PRAGMA show_tables_expanded`, for a table or for a view.
fn expanded_row(
    database: &Database,
    schema: &Schema,
    name: &str,
    columns: &[rudb_common::Field],
) -> Vec<Value> {
    vec![
        text(database.name()),
        text(schema.name()),
        text(name),
        names_of(columns.iter().map(|field| field.name.clone())),
        names_of(columns.iter().map(|field| field.ty.to_string())),
        Value::Boolean(database.internal()),
    ]
}

/// A `VARCHAR[]` of whatever was handed in, which is how both list columns are built.
fn names_of(values: impl Iterator<Item = String>) -> Value {
    Value::List { element: LogicalType::Varchar, values: values.map(Value::Varchar).collect() }
}

/// Every base table in the catalog with the database and schema it is in.
fn entries(catalog: &Catalog) -> impl Iterator<Item = (&Database, &Schema, &Table)> {
    catalog.databases().iter().flat_map(|database| {
        database.schemas().iter().flat_map(move |schema| {
            schema.tables().iter().map(move |table| (database, schema, table))
        })
    })
}

/// The `CREATE TABLE` a table would be made by, which is what `duckdb_tables()` reports as `sql`.
///
/// Written back out rather than stored. The pin does the same, which is measured: a table created
/// with odd spacing and lower case type names comes back normalised, so the column is a deparse of
/// the entry and not the text somebody typed. Identifiers go through [`rudb_parse::quoted`], which
/// is the rule the binder already uses for a generated column name.
fn create_table(table: &Table) -> String {
    let columns: Vec<String> = table
        .columns()
        .iter()
        .map(|column| {
            let null = if column.not_null { " NOT NULL" } else { "" };
            format!("{} {}{null}", quoted(&column.name), column.ty)
        })
        .collect();
    format!("CREATE TABLE {}({});", quoted(&table.name().table), columns.join(", "))
}

/// An empty `MAP(VARCHAR, VARCHAR)`, which is what `tags` and `options` are on every row.
fn empty() -> Value {
    Value::map(LogicalType::Varchar, LogicalType::Varchar, Vec::new())
}
