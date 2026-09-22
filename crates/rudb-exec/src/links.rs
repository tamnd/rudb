//! `rudb_links()`, the table that says what the graph layer knows about this database.
//!
//! One row per declared relationship. What is declared comes from the session, because
//! `SET graph_links` is where a relationship is declared for tables that arrived from Parquet and
//! Parquet has no foreign keys to read one out of. What is stored comes from the tables themselves,
//! so a row is a declaration on the left and a measurement on the right, and the two are never
//! mixed: section 2.3 of spec/graph/02-the-data-model.md says a declaration is what somebody
//! believes and a build is what is true.
//!
//! A reader asks this one of four questions. Is my relationship known at all, which is whether it
//! has a row. Was it verified, which is the cardinality column and is the difference between a join
//! that can use a structure and one that has to hash. What does it cost, which is the bytes, filled
//! whether or not the structure was kept so that section 3.7's budget is a number somebody can act
//! on rather than a silence. And what shape did it turn out to have, which is the degree columns
//! and is the only group here that says something about the data rather than about the file.

use rudb_catalog::{Catalog, Rows, Table};
use rudb_common::{Result, Session, Value};
use rudb_functions::link_fields;
use rudb_graph::{Cardinality, Degrees, Relationship, Side, parse_links};
use rudb_native::graph::Edge;
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
    let held = forward_link_of(catalog, link);
    let shape = degrees_of(catalog, link);
    let (cardinality, note) = verdict(catalog, link, stored.as_ref(), held.as_ref());
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
        held.as_ref().map_or(Value::Null, |held| text(held.form().label())),
        held.as_ref().map_or(Value::Null, |held| {
            Value::BigInt(i64::try_from(held.bytes()).unwrap_or(i64::MAX))
        }),
        shape.as_ref().map_or(Value::Null, |shape| Value::Double(shape.mean())),
        shape.as_ref().map_or(Value::Null, |shape| Value::BigInt(clamp(shape.highest()))),
        shape.as_ref().map_or(Value::Null, |shape| Value::BigInt(clamp(shape.percentile(0.99)))),
        shape.as_ref().and_then(Degrees::locality).map_or(Value::Null, Value::Double),
        shape.as_ref().map_or(Value::Null, |shape| Value::Boolean(shape.unique())),
        shape.as_ref().map_or(Value::Null, |shape| Value::Boolean(shape.total())),
        note.map_or(Value::Null, text),
    ]
}

/// A degree that fits in the signed integer the column is.
///
/// A degree past nine quintillion is one this codebase will not meet, and saturating is the right
/// answer for the one place it could come from: the last histogram bucket's bound, which is already
/// a bound rather than a count.
fn clamp(degree: u64) -> i64 {
    i64::try_from(degree).unwrap_or(i64::MAX)
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

/// The stored forward link of a relationship, when both of its tables are in the same file and the
/// link in the child's sections was built against the parent this declaration names.
///
/// Both sides in one file is not a limitation of the format so much as of what a relationship
/// across two files would mean: a `rid` is a row's position in a table, and a link is a column of
/// them, so a link stored in one file that names a parent in another would be resolvable only by a
/// reader that had both open and had checked that neither had moved since. Nothing declares one
/// today and this reports nothing for one rather than guessing.
fn forward_link_of(catalog: &Catalog, link: &Relationship) -> Option<rudb_graph::Link> {
    let ([child_key], [parent_key]) = (&link.child.columns[..], &link.parent.columns[..]) else {
        return None;
    };
    let child = table_named(catalog, &link.child.table)?;
    let parent = table_named(catalog, &link.parent.table)?;
    let (Rows::Native(child_rows), Rows::Native(parent_rows)) = (child.rows(), parent.rows())
    else {
        return None;
    };
    let edge = Edge {
        child: child.name().table.clone(),
        child_column: child.column_index(child_key)?,
        parent: parent.name().table.clone(),
        parent_column: parent.column_index(parent_key)?,
    };
    rudb_native::graph::stored_link(child_rows, parent_rows, &edge)
}

/// What the link build measured of a relationship's shape, when the child table carries it.
///
/// This asks the child table alone, because a degree section is about the child column and has no
/// parent binding to check. There is no separate check that the link is stored, and there does not
/// need to be: the build attaches the two together under the same id and stamps them with the same
/// generation, so a file cannot hold one of them current without the other.
fn degrees_of(catalog: &Catalog, link: &Relationship) -> Option<Degrees> {
    let [child_key] = &link.child.columns[..] else { return None };
    let child = table_named(catalog, &link.child.table)?;
    let Rows::Native(rows) = child.rows() else { return None };
    rudb_native::graph::stored_degrees(rows, child.column_index(child_key)?)
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
    held: Option<&rudb_graph::Link>,
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
    // At most one until the link is built, because exactly one is a claim about the child side and
    // the key map only ever saw the parent's. The link is what observes the child: a link whose
    // every child found a parent is a relationship that is total in the direction the declaration
    // claims, and one that did not is still at most one and is why the monotone form was refused.
    match held {
        Some(held) if held.linked() == held.children() => (Cardinality::ExactlyOne.label(), None),
        Some(_) => (
            Cardinality::AtMostOne.label(),
            Some("some child rows have no parent, so this is not exactly one"),
        ),
        None => (Cardinality::AtMostOne.label(), Some("no link is stored")),
    }
}
