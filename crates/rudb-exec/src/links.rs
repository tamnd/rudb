//! `rudb_links()`, the table that says what the graph layer knows about this database.
//!
//! One row per declared relationship. What is declared comes from the tables' foreign keys and from
//! the session, because `SET graph_links` is where a relationship is declared for tables that
//! arrived from Parquet and Parquet has no foreign keys to read one out of. What is stored comes from the tables themselves,
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
    let declared = declared(catalog, session.links());
    let mut rows = Vec::with_capacity(declared.len());
    for link in &declared {
        rows.push(row(catalog, link));
    }
    Metadata::new("rudb_links", &link_fields(), &rows, plan, index, columns)
}

/// Every relationship declared for this catalog: the ones `graph_links` names, then one for each
/// foreign key a table was created with that the setting does not already name.
///
/// Section 2.5 of spec/graph/02-the-data-model.md lists the foreign keys first, as the path that
/// needs nothing from the user, and the setting second, for data that arrived without constraints.
/// The two are one list because nothing downstream cares where a relationship came from: the build
/// verifies both the same way and a plan reads neither until it has.
///
/// A setting the parser refused contributes nothing, which is the silence `rudb_links()` has always
/// given one. A foreign key over no columns or into a table of another schema is left out.
#[must_use]
pub fn declared(catalog: &Catalog, setting: &str) -> Vec<Relationship> {
    let mut declared = parse_links(setting).unwrap_or_default();
    for table in catalog.tables() {
        for foreign in table.foreign() {
            if !foreign.table.schema.eq_ignore_ascii_case(&table.name().schema) {
                continue;
            }
            let Ok(parent) = catalog.table(&foreign.table) else { continue };
            let named = |on: &Table, columns: &[usize]| {
                columns
                    .iter()
                    .map(|&at| on.columns().get(at).map(|field| field.name.clone()))
                    .collect::<Option<Vec<String>>>()
            };
            let (Some(child_columns), Some(parent_columns)) =
                (named(table, &foreign.columns), named(parent, &foreign.referenced))
            else {
                continue;
            };
            let (Ok(child), Ok(parent)) = (
                Side::composite(table.name().table.clone(), child_columns),
                Side::composite(parent.name().table.clone(), parent_columns),
            ) else {
                continue;
            };
            let Ok(link) = Relationship::declare(child, parent) else { continue };
            let named_already = declared.iter().any(|held| {
                held.child.table.eq_ignore_ascii_case(&link.child.table)
                    && held.parent.table.eq_ignore_ascii_case(&link.parent.table)
                    && same_columns(&held.child.columns, &link.child.columns)
                    && same_columns(&held.parent.columns, &link.parent.columns)
            });
            if !named_already {
                declared.push(link);
            }
        }
    }
    declared
}

fn same_columns(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(l, r)| l.eq_ignore_ascii_case(r))
}

/// What one relationship reports.
fn row(catalog: &Catalog, link: &Relationship) -> Vec<Value> {
    let stored = key_map_of(catalog, &link.parent);
    let held = forward_link_of(catalog, link);
    let shape = degrees_of(catalog, link);
    // A structure is stored, or it was measured and turned away, or nothing has looked at the
    // column. The first two both have a size and the third does not, so the size column falls back
    // to the budget record and only goes null for the third.
    let map_bytes = stored
        .as_ref()
        .map(|map| map.bytes as u64)
        .or_else(|| refused_key_map_of(catalog, &link.parent));
    let link_bytes =
        held.as_ref().map(|held| held.bytes() as u64).or_else(|| refused_link_of(catalog, link));
    let (cardinality, note) = verdict(
        catalog,
        link,
        stored.as_ref(),
        held.as_ref(),
        map_bytes.is_some(),
        link_bytes.is_some(),
    );
    vec![
        text(&link.name()),
        text(&link.child.table),
        text(&link.child.columns.join(", ")),
        text(&link.parent.table),
        text(&link.parent.columns.join(", ")),
        text(cardinality),
        stored.as_ref().map_or(Value::Null, |map| text(map.form.label())),
        map_bytes.map_or(Value::Null, |bytes| Value::BigInt(clamp(bytes))),
        held.as_ref().map_or(Value::Null, |held| text(held.form().label())),
        link_bytes.map_or(Value::Null, |bytes| Value::BigInt(clamp(bytes))),
        shape.as_ref().map_or(Value::Null, |shape| Value::Double(shape.mean())),
        shape.as_ref().map_or(Value::Null, |shape| Value::BigInt(clamp(shape.highest()))),
        shape.as_ref().map_or(Value::Null, |shape| Value::BigInt(clamp(shape.percentile(0.99)))),
        shape.as_ref().and_then(Degrees::locality).map_or(Value::Null, Value::Double),
        shape.as_ref().map_or(Value::Null, |shape| Value::Boolean(shape.unique())),
        shape.as_ref().map_or(Value::Null, |shape| Value::Boolean(shape.total())),
        note.map_or(Value::Null, text),
    ]
}

/// A count that fits in the signed integer the column is.
///
/// A degree or a size past nine quintillion is one this codebase will not meet, and saturating is
/// the right answer for the one place either could come from: the last histogram bucket's bound,
/// which is already a bound rather than a count.
fn clamp(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
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

/// What a key map over a relationship's parent side would have cost, when a build measured one and
/// did not keep it.
///
/// The form it would have taken is on the record too and is not reported. A form column that named
/// a form no join can read would be the one thing this table does not do, which is to mix what is
/// believed with what is there. The size is different: it is a fact about a structure that does not
/// exist, and it is the number somebody raising `graph_budget` needs.
fn refused_key_map_of(catalog: &Catalog, parent: &Side) -> Option<u64> {
    if parent.columns.len() != 1 {
        return None;
    }
    let table = table_named(catalog, &parent.table)?;
    let column = table.column_index(&parent.columns[0])?;
    let Rows::Native(reader) = table.rows() else { return None };
    rudb_native::graph::refused_key_map(reader, column).map(|(_, bytes)| bytes)
}

/// What a forward link over a relationship's child column would have cost, when a build measured
/// one and did not keep it.
///
/// This asks the child table alone, where [`forward_link_of`] checks that the stored link names the
/// parent this declaration names. There is nothing to check against: the field a stored link keeps
/// its parent binding in is the field a budget record keeps its size in, so the record says what a
/// link over this column would have cost and not which parent it was measured against. The record
/// is keyed by the child column, the way a degree section is, so two declarations over the same
/// column and different parents read the same size, which is the size of whichever was built last.
fn refused_link_of(catalog: &Catalog, link: &Relationship) -> Option<u64> {
    let child = table_named(catalog, &link.child.table)?;
    let Rows::Native(rows) = child.rows() else { return None };
    rudb_native::graph::refused_link(rows, key_in(child, &link.child.columns)?)
        .map(|(_, bytes)| bytes)
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
    let child = table_named(catalog, &link.child.table)?;
    let parent = table_named(catalog, &link.parent.table)?;
    let (Rows::Native(child_rows), Rows::Native(parent_rows)) = (child.rows(), parent.rows())
    else {
        return None;
    };
    let edge = Edge {
        child: child.name().table.clone(),
        child_column: key_in(child, &link.child.columns)?,
        parent: parent.name().table.clone(),
        parent_column: key_in(parent, &link.parent.columns)?,
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
    let child = table_named(catalog, &link.child.table)?;
    let Rows::Native(rows) = child.rows() else { return None };
    rudb_native::graph::stored_degrees(rows, key_in(child, &link.child.columns)?)
}

/// The number the graph sections name a key over these columns by: the column's index for one, and
/// `rudb_native::graph::key_of`'s pair for two.
fn key_in(table: &Table, columns: &[String]) -> Option<usize> {
    let at = columns.iter().map(|column| table.column_index(column)).collect::<Option<Vec<_>>>()?;
    rudb_native::graph::key_of(&at)
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
    measured_map: bool,
    measured_link: bool,
) -> (&'static str, Option<&'static str>) {
    let Some(stored) = stored else {
        if table_named(catalog, &link.parent.table).is_none() {
            return (Cardinality::Unverified.label(), Some("no table of that name"));
        }
        // A link is only written over a parent key the build found distinct, with the map it
        // needed built for it when the file keeps none: always for a key over two columns, and for
        // one column when the budget turned the map away. So a held link is the whole answer.
        // Without a link there is nothing that counted the parent's keys either.
        if held.is_some() {
            return linked(held, measured_link);
        }
        if link.parent.columns.len() == 2 {
            if measured_link {
                return (
                    Cardinality::Unverified.label(),
                    Some("the link was measured and not kept, so link_bytes is what it would cost"),
                );
            }
            return (Cardinality::Unverified.label(), Some("no link is stored"));
        }
        if link.parent.columns.len() != 1 {
            return (
                Cardinality::Unverified.label(),
                Some("no link is built over a key this wide"),
            );
        }
        // A build that looked and decided against it is a fourth answer, and the note says so
        // without saying why, because the record keeps the size and not the reason. The two
        // reasons a build has are the budget and a key that repeats, and key_map_bytes against
        // graph_budget is what tells them apart.
        if measured_map {
            return (
                Cardinality::Unverified.label(),
                Some(
                    "the key map was measured and not kept, so key_map_bytes is what it would cost",
                ),
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
    linked(held, measured_link)
}

/// What the link says, for a parent key the build found distinct.
///
/// At most one until the link is built, because exactly one is a claim about the child side and the
/// key map only ever saw the parent's. The link is what observes the child: a link whose every child
/// found a parent is a relationship that is total in the direction the declaration claims, and one
/// that did not is still at most one and is why the monotone form was refused.
fn linked(
    held: Option<&rudb_graph::Link>,
    measured_link: bool,
) -> (&'static str, Option<&'static str>) {
    match held {
        Some(held) if held.linked() == held.children() => (Cardinality::ExactlyOne.label(), None),
        Some(_) => (
            Cardinality::AtMostOne.label(),
            Some("some child rows have no parent, so this is not exactly one"),
        ),
        None if measured_link => (
            Cardinality::AtMostOne.label(),
            Some("the link was measured and not kept, so link_bytes is what it would cost"),
        ),
        None => (Cardinality::AtMostOne.label(), Some("no link is stored")),
    }
}
