//! `rudb_links()`, the table that says what the graph layer knows about this database.
//!
//! One row per declared relationship. What is declared comes from the session, because
//! `SET graph_links` is where a relationship is declared for tables that arrived from Parquet and
//! Parquet has no foreign keys to read one out of. What is stored comes from the tables themselves,
//! so a row is a declaration on the left and a measurement on the right, and the two are never
//! mixed: section 2.3 of spec/graph/02-the-data-model.md says a declaration is what somebody
//! believes and a build is what is true.
//!
//! A reader asks this one of three questions. Is my relationship known at all, which is whether it
//! has a row. Was it verified, which is the cardinality column and is the difference between a join
//! that can use a structure and one that has to hash. And what does it cost, which is the bytes,
//! filled whether or not the structure was kept so that section 3.7's budget is a number somebody
//! can act on rather than a silence.

use rudb_catalog::{Catalog, Rows, Table};
use rudb_common::{Result, Session, Value};
use rudb_functions::link_fields;
use rudb_graph::{Cardinality, Relationship, Side, parse_links};
use rudb_plan::{Plan, Slice};

use crate::metadata::{Metadata, text};

/// Every declared relationship and what is stored for it, in the columns the plan asked for.
///
/// # Errors
///
/// If the plan asks for a column this table does not have.
pub(crate) fn links(
    session: &Session,
    catalog: &Catalog,
    plan: &Plan,
    index: u32,
    columns: Slice,
) -> Result<Metadata> {
    // The setting was parsed once when it was set, so this cannot fail on anything a `SET` let
    // through. A session filled in by something other than the settings layer is the other caller,
    // and a declaration it could not parse is one no build will have acted on either, so the table
    // says nothing about it rather than refusing to be read.
    let declared = parse_links(session.links()).unwrap_or_default();
    let mut rows = Vec::with_capacity(declared.len());
    for link in &declared {
        rows.push(row(catalog, link));
    }
    Metadata::new("rudb_links", &link_fields(), &rows, plan, index, columns)
}

/// What one relationship reports.
fn row(catalog: &Catalog, link: &Relationship) -> Vec<Value> {
    let stored = key_map_of(catalog, &link.parent);
    let (cardinality, note) = verdict(catalog, link, stored.as_ref());
    vec![
        text(&link.name()),
        text(&link.child.table),
        text(&link.child.columns.join(", ")),
        text(&link.parent.table),
        text(&link.parent.columns.join(", ")),
        text(cardinality),
        stored.as_ref().map_or(Value::Null, |map| text(map.form.label())),
        stored
            .as_ref()
            .map_or(Value::Null, |map| Value::BigInt(i64::try_from(map.bytes).unwrap_or(i64::MAX))),
        // The link columns are what G2 fills. Nothing writes a forward link yet, so a value here
        // would be a claim about a structure that does not exist.
        Value::Null,
        Value::Null,
        note.map_or(Value::Null, text),
    ]
}

/// A key map found in a table, reduced to what the table reports.
struct Stored {
    form: rudb_graph::Form,
    bytes: usize,
    distinct: bool,
}

/// The stored key map of a relationship's parent side, when there is one this build can read.
///
/// One column only. A composite parent key is two key maps over a folded key and nothing folds one
/// yet, so a composite relationship reports no map rather than the map of its first column, which
/// would be a true statement about a column and a false one about the relationship.
fn key_map_of(catalog: &Catalog, parent: &Side) -> Option<Stored> {
    if parent.columns.len() != 1 {
        return None;
    }
    let table = table_named(catalog, &parent.table)?;
    let column = table.column_index(&parent.columns[0])?;
    let Rows::Native(reader) = table.rows() else { return None };
    let map = rudb_native::graph::key_map(reader, column)?;
    Some(Stored { form: map.form(), bytes: map.bytes(), distinct: map.observed().distinct })
}

/// The first table of that name in any schema of any database.
///
/// A relationship names a table and not a qualified name, because the `graph_links` grammar has no
/// dot in it and the tables of one benchmark are in one schema. A name that is ambiguous across two
/// databases resolves to the first, which is the same order `duckdb_tables()` lists them in, and
/// the day the grammar takes a qualified name this stops guessing.
fn table_named<'a>(catalog: &'a Catalog, name: &str) -> Option<&'a Table> {
    catalog
        .databases()
        .iter()
        .flat_map(|database| database.schemas().iter().flat_map(rudb_catalog::Schema::tables))
        .find(|table| table.name().table.eq_ignore_ascii_case(name))
}

/// What the build observed, and why it is not more than that.
///
/// The note is the column that keeps this table honest. Every relationship starts unverified, and a
/// reader who sees that wants to know whether it is unverified because the key repeats, because
/// nothing has been built, or because the tables are not in a file yet. Those three have different
/// answers and only one of them is a problem with the declaration.
fn verdict(
    catalog: &Catalog,
    link: &Relationship,
    stored: Option<&Stored>,
) -> (&'static str, Option<&'static str>) {
    let Some(stored) = stored else {
        if table_named(catalog, &link.parent.table).is_none() {
            return (Cardinality::Unverified.label(), Some("no table of that name"));
        }
        if link.parent.columns.len() != 1 {
            return (
                Cardinality::Unverified.label(),
                Some("a composite key needs a folded key map, which is not built"),
            );
        }
        return (Cardinality::Unverified.label(), Some("no key map is stored"));
    };
    if !stored.distinct {
        return (
            Cardinality::Unverified.label(),
            Some("the parent key repeats, so this is not a many to one relationship"),
        );
    }
    // At most one and not exactly one. Exactly one needs the child side observed as well, which is
    // what the forward link build settles, so claiming it here would be claiming something nothing
    // has checked.
    (Cardinality::AtMostOne.label(), None)
}
