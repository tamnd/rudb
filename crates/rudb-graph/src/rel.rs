//! What a relationship is, and how one gets declared.
//!
//! A relationship is a named triple: a child table with a column list, a parent table with a column
//! list, and a cardinality. It is always many-to-one from child to parent, per section 2.3, and
//! many-to-many is two of them through the link table rather than a fourth case: `partsupp` is the
//! child of both `part` and `supplier` and there is no `part` to `supplier` relationship anywhere.
//!
//! Section 2.5 gives three declaration paths. This module holds the second, the session setting,
//! because it is the one TPC-H needs: the tables arrive from Parquet and Parquet has no foreign
//! keys, so there is no constraint in the catalog for the first path to read and nothing for the
//! third path to have inferred yet. The setting takes a list of `child(col) -> parent(col)` and
//! that is the whole grammar.
//!
//! Nothing here is trusted. A declaration says what the author believes, and section 2.3 says the
//! cardinality is observed at build time instead: a declared many-to-one whose parent side turns
//! out not to be unique becomes [`Cardinality::Unverified`], the link is not built, and the planner
//! is told rather than left to produce a wrong answer from a wrong declaration.

use std::fmt;

use rudb_common::{Error, Result};

/// How many parent rows a child row can match.
///
/// The three cases are not decoration. Each licenses a different rewrite in section 5, and the
/// difference between the first two is the difference between a LEFT JOIN that can be turned into
/// an inner one and one that cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// Every child row matches exactly one parent row: the key is not null anywhere and every value
    /// is present in the parent. This licenses pushing an aggregate through the relationship and
    /// turns a LEFT JOIN into an inner join.
    ExactlyOne,
    /// Every child row matches at most one parent row, which is the ordinary case: a null key or a
    /// key with no parent row matches nothing.
    AtMostOne,
    /// The parent side was not unique, or verification did not run. No link is built. The planner
    /// is told so that it plans an ordinary hash join rather than waiting for a structure that is
    /// never going to arrive.
    Unverified,
}

impl Cardinality {
    /// Whether a link may be built for a relationship of this cardinality.
    #[must_use]
    pub fn links(self) -> bool {
        matches!(self, Self::ExactlyOne | Self::AtMostOne)
    }

    /// Whether every child row is guaranteed a parent row, which is what licenses the aggregate
    /// push-through and the LEFT JOIN rewrite.
    #[must_use]
    pub fn total(self) -> bool {
        matches!(self, Self::ExactlyOne)
    }

    /// The tag this cardinality takes in a section header.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::ExactlyOne => 0,
            Self::AtMostOne => 1,
            Self::Unverified => 2,
        }
    }

    /// The cardinality a header tag names.
    ///
    /// # Errors
    ///
    /// If the tag is not one of the three, which means a file from a later build. The caller drops
    /// the section, per the rule in section 3.2 that a reader ignores what it does not know.
    pub fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::ExactlyOne),
            1 => Ok(Self::AtMostOne),
            2 => Ok(Self::Unverified),
            _ => Err(malformed(format!("cardinality {tag} is not one this build knows"))),
        }
    }
}

impl fmt::Display for Cardinality {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::ExactlyOne => "exactly one",
            Self::AtMostOne => "at most one",
            Self::Unverified => "unverified",
        };
        formatter.write_str(text)
    }
}

/// One side of a relationship: a table and the columns of its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    /// The table's name as the catalog holds it.
    pub table: String,
    /// The key columns, in order. More than one is a composite key, resolved by the same key map
    /// over a folded composite, a column at a time, the way `rudb-exec` already folds a group key.
    pub columns: Vec<String>,
}

impl Side {
    /// A side over a single column, which is every relationship TPC-H needs.
    pub fn new(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self { table: table.into(), columns: vec![column.into()] }
    }

    /// A side over a composite key.
    ///
    /// # Errors
    ///
    /// If the column list is empty. A key over no columns is not a key, and catching it here is
    /// what keeps it from becoming a relationship that matches every row against every row.
    pub fn composite(table: impl Into<String>, columns: Vec<String>) -> Result<Self> {
        if columns.is_empty() {
            return Err(malformed("a relationship side needs at least one key column"));
        }
        Ok(Self { table: table.into(), columns })
    }
}

impl fmt::Display for Side {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}({})", self.table, self.columns.join(", "))
    }
}

/// A declared many-to-one relationship from a child table to a parent table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    /// The many side, which is where the forward link lives.
    pub child: Side,
    /// The one side, which is where the key map lives.
    pub parent: Side,
    /// What the build observed, or [`Cardinality::Unverified`] before it ran.
    pub cardinality: Cardinality,
}

impl Relationship {
    /// A relationship not yet verified, which is what a declaration produces.
    ///
    /// # Errors
    ///
    /// If the two sides have different numbers of key columns, or if a side names a table as its
    /// own parent through the same columns. The first is a declaration that cannot be equated
    /// column by column. The second is a self relationship on identical columns, which is the
    /// identity and carries no information.
    pub fn declare(child: Side, parent: Side) -> Result<Self> {
        if child.columns.len() != parent.columns.len() {
            return Err(malformed(format!(
                "the child key {child} has {} columns and the parent key {parent} has {}",
                child.columns.len(),
                parent.columns.len()
            )));
        }
        if child.table == parent.table && child.columns == parent.columns {
            return Err(malformed(format!("{child} references itself through its own columns")));
        }
        Ok(Self { child, parent, cardinality: Cardinality::Unverified })
    }

    /// The name this relationship reports under in `rudb_links()`.
    ///
    /// Derived rather than given, because section 2.3 names a relationship by what it relates and
    /// there is nowhere in the `graph_links` grammar for an author to supply a name. A derived name
    /// is also stable across a reload, which is what lets a query log entry from yesterday still
    /// name a relationship today.
    #[must_use]
    pub fn name(&self) -> String {
        format!("{} -> {}", self.child, self.parent)
    }

    /// Whether the two sides are keyed on one column each.
    #[must_use]
    pub fn single_column(&self) -> bool {
        self.child.columns.len() == 1
    }
}

impl fmt::Display for Relationship {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} -> {}", self.child, self.parent)
    }
}

/// Parses the `graph_links` session setting.
///
/// The grammar is a comma separated list of `child(col) -> parent(col)`, with a composite key
/// written as `child(a, b) -> parent(c, d)`. Whitespace between tokens is free. A trailing comma is
/// allowed because a setting long enough to need one line per link is a setting somebody will edit,
/// and refusing the trailing comma would make every edit a two line diff.
///
/// TPC-H's eight relationships in this grammar, which is the string the G1 measurement runs with:
///
/// ```text
/// nation(n_regionkey) -> region(r_regionkey),
/// supplier(s_nationkey) -> nation(n_nationkey),
/// customer(c_nationkey) -> nation(n_nationkey),
/// partsupp(ps_partkey) -> part(p_partkey),
/// partsupp(ps_suppkey) -> supplier(s_suppkey),
/// orders(o_custkey) -> customer(c_custkey),
/// lineitem(l_orderkey) -> orders(o_orderkey),
/// lineitem(l_partkey) -> part(p_partkey)
/// ```
///
/// # Errors
///
/// If a link is malformed. A setting is typed by a person, so the error names what was expected at
/// the position it stopped rather than reporting that the setting is invalid: a link list that
/// silently dropped the one entry with a typo in it would be a setting that looks like it worked.
pub fn parse_links(setting: &str) -> Result<Vec<Relationship>> {
    let mut links = Vec::new();
    for entry in setting.split(',').map(str::trim) {
        // `split(',')` also splits the inside of a composite key, so an entry with no arrow in it
        // is either a continuation of the previous one or blank. Rejoining is done by the arrow: an
        // entry without one attaches to whichever side of the previous link is still open.
        if entry.is_empty() {
            continue;
        }
        links.push(entry.to_owned());
    }
    // Rejoin the pieces a composite key was split into. An entry is complete when it holds an
    // arrow and its parentheses balance; until then the next piece belongs to it.
    let mut joined: Vec<String> = Vec::new();
    for piece in links {
        match joined.last_mut() {
            Some(open) if !balanced(open) => {
                open.push_str(", ");
                open.push_str(&piece);
            }
            _ => joined.push(piece),
        }
    }

    let mut parsed = Vec::with_capacity(joined.len());
    for entry in &joined {
        parsed.push(parse_link(entry)?);
    }
    Ok(parsed)
}

fn balanced(entry: &str) -> bool {
    let opens = entry.matches('(').count();
    let closes = entry.matches(')').count();
    opens == closes && entry.contains("->") && closes == 2
}

fn parse_link(entry: &str) -> Result<Relationship> {
    let Some((child, parent)) = entry.split_once("->") else {
        return Err(malformed(format!(
            "expected `child(column) -> parent(column)` and found `{entry}`"
        )));
    };
    Relationship::declare(parse_side(child.trim())?, parse_side(parent.trim())?)
}

fn parse_side(side: &str) -> Result<Side> {
    let Some((table, rest)) = side.split_once('(') else {
        return Err(malformed(format!("expected `table(column)` and found `{side}`")));
    };
    let Some(columns) = rest.strip_suffix(')') else {
        return Err(malformed(format!("`{side}` is missing its closing parenthesis")));
    };
    let table = table.trim();
    if table.is_empty() {
        return Err(malformed(format!("`{side}` names no table")));
    }
    let columns: Vec<String> = columns
        .split(',')
        .map(str::trim)
        .filter(|column| !column.is_empty())
        .map(str::to_owned)
        .collect();
    Side::composite(table, columns)
}

fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb relationship: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eight_tpch_relationships_parse_into_eight_relationships() {
        // The string the G1 measurement runs with, so a change to the grammar that broke it would
        // fail here rather than in a benchmark two milestones later.
        let setting = "nation(n_regionkey) -> region(r_regionkey), \
             supplier(s_nationkey) -> nation(n_nationkey), \
             customer(c_nationkey) -> nation(n_nationkey), \
             partsupp(ps_partkey) -> part(p_partkey), \
             partsupp(ps_suppkey) -> supplier(s_suppkey), \
             orders(o_custkey) -> customer(c_custkey), \
             lineitem(l_orderkey) -> orders(o_orderkey), \
             lineitem(l_partkey) -> part(p_partkey)";
        let links = parse_links(setting).expect("parse");
        assert_eq!(links.len(), 8);
        assert_eq!(links[0].child, Side::new("nation", "n_regionkey"));
        assert_eq!(links[0].parent, Side::new("region", "r_regionkey"));
        assert_eq!(links[7].name(), "lineitem(l_partkey) -> part(p_partkey)");
        assert!(links.iter().all(Relationship::single_column));
        assert!(
            links.iter().all(|link| link.cardinality == Cardinality::Unverified),
            "a declaration is unverified until a build has looked at the column"
        );
    }

    #[test]
    fn a_composite_key_survives_the_comma_that_separates_links() {
        // The one real ambiguity in the grammar: a comma separates two links and also separates two
        // columns of the same key, so a parser that split on commas and stopped would produce four
        // broken links out of these two.
        let setting = "child(a, b) -> parent(c, d), other(e) -> parent2(f)";
        let links = parse_links(setting).expect("parse");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].child.columns, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(links[0].parent.columns, vec!["c".to_owned(), "d".to_owned()]);
        assert!(!links[0].single_column());
        assert_eq!(links[1].child.columns, vec!["e".to_owned()]);
    }

    #[test]
    fn whitespace_and_a_trailing_comma_are_free() {
        let setting = "  orders( o_custkey )   ->   customer( c_custkey )  ,  ";
        let links = parse_links(setting).expect("parse");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].child, Side::new("orders", "o_custkey"));
    }

    #[test]
    fn an_empty_setting_declares_nothing_rather_than_failing() {
        assert!(parse_links("").expect("parse").is_empty());
        assert!(parse_links("   ").expect("parse").is_empty());
    }

    #[test]
    fn a_link_with_no_arrow_is_refused_and_says_what_was_expected() {
        let error = parse_links("orders(o_custkey) customer(c_custkey)").expect_err("refused");
        let text = error.to_string();
        assert!(text.contains("child(column) -> parent(column)"), "{text}");
    }

    #[test]
    fn a_side_with_no_parenthesis_is_refused() {
        assert!(parse_links("orders -> customer(c_custkey)").is_err());
        assert!(parse_links("orders(o_custkey -> customer(c_custkey)").is_err());
    }

    #[test]
    fn a_side_with_no_table_is_refused() {
        assert!(parse_links("(o_custkey) -> customer(c_custkey)").is_err());
    }

    #[test]
    fn a_side_with_no_columns_is_refused() {
        assert!(parse_links("orders() -> customer(c_custkey)").is_err());
    }

    #[test]
    fn a_declaration_whose_sides_have_different_widths_is_refused() {
        // The equality is column by column, so two sides of different widths is a declaration with
        // no meaning rather than one that happens to be wrong.
        let error = parse_links("child(a, b) -> parent(c)").expect_err("refused");
        assert!(error.to_string().contains("columns"), "{error}");
    }

    #[test]
    fn a_table_referencing_itself_through_its_own_columns_is_refused() {
        let error = parse_links("orders(o_orderkey) -> orders(o_orderkey)").expect_err("refused");
        assert!(error.to_string().contains("itself"), "{error}");
    }

    #[test]
    fn a_table_referencing_itself_through_a_different_column_is_allowed() {
        // A manager column against an employee key is a real relationship and section 2.3 does not
        // exclude it, so the self check is about identical columns and nothing more.
        let links = parse_links("employee(manager) -> employee(id)").expect("parse");
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn only_exactly_one_licenses_the_rewrites_that_need_a_parent_for_every_child() {
        assert!(Cardinality::ExactlyOne.links());
        assert!(Cardinality::ExactlyOne.total());
        assert!(Cardinality::AtMostOne.links());
        assert!(!Cardinality::AtMostOne.total(), "a null key matches no parent row");
        assert!(!Cardinality::Unverified.links(), "an unverified side takes an ordinary join");
        assert!(!Cardinality::Unverified.total());
    }

    #[test]
    fn the_cardinality_tag_round_trips_and_an_unknown_one_is_refused() {
        for cardinality in
            [Cardinality::ExactlyOne, Cardinality::AtMostOne, Cardinality::Unverified]
        {
            assert_eq!(Cardinality::from_tag(cardinality.tag()).expect("a known tag"), cardinality);
        }
        assert!(Cardinality::from_tag(7).is_err());
    }

    #[test]
    fn many_to_many_is_declared_as_two_many_to_one_through_the_link_table() {
        // Section 2.3, checked as documentation as much as as behaviour: there is no way to write a
        // `part` to `supplier` relationship in this grammar, and `partsupp` being the child of both
        // is the only shape available.
        let links = parse_links(
            "partsupp(ps_partkey) -> part(p_partkey), partsupp(ps_suppkey) -> supplier(s_suppkey)",
        )
        .expect("parse");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].child.table, "partsupp");
        assert_eq!(links[1].child.table, "partsupp");
    }
}
