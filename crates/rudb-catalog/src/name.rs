//! Names, and the one rule about comparing them.
//!
//! DuckDB is case insensitive and case preserving. `SELECT * FROM MyTable` finds a table created as
//! `mytable`, and `\d` still shows it spelled the way it was created. The parser does no folding at
//! all, deliberately and provably: reading `base_tokenizer.cpp` for the tokenizer work in 0.0.2
//! turned up that DuckDB does not lowercase an identifier at any point, including a quoted one. So
//! the folding has to happen here, at the comparison, which is also the only place it can happen
//! and still preserve the spelling.
//!
//! The comparison is ASCII only. DuckDB's is too for the path this replaces, and a Unicode aware
//! one is a different function with a different cost that is worth having only when there is a test
//! that fails without it.

/// Whether two identifiers name the same object.
#[must_use]
pub fn same_name(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

/// A three part name, as it is written in SQL and as the plan's `Get` operator carries it.
///
/// The first part is called the catalog rather than the database because that is the word SQL uses
/// and the word DuckDB uses in `catalog.schema.table`. It names an attached database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedName {
    /// The attached database.
    pub catalog: String,
    /// The schema inside it.
    pub schema: String,
    /// The object.
    pub table: String,
}

impl QualifiedName {
    /// A name from its three parts.
    pub fn new(
        catalog: impl Into<String>,
        schema: impl Into<String>,
        table: impl Into<String>,
    ) -> Self {
        Self { catalog: catalog.into(), schema: schema.into(), table: table.into() }
    }

    /// Whether this names the same object as `other`, under the identifier rule.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        same_name(&self.catalog, &other.catalog)
            && same_name(&self.schema, &other.schema)
            && same_name(&self.table, &other.table)
    }
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.catalog, self.schema, self.table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_matches_whatever_case_it_was_written_in() {
        assert!(same_name("MyTable", "mytable"));
        assert!(same_name("HITS", "hits"));
        assert!(!same_name("hits", "hit"));
    }

    /// The spelling that was used to create the object is the spelling that comes back out, which
    /// is what case preserving means and is why the folding is here and not in the parser.
    #[test]
    fn matching_a_name_does_not_change_it() {
        let name = QualifiedName::new("memory", "main", "MyTable");
        assert!(name.same_as(&QualifiedName::new("MEMORY", "Main", "mytable")));
        assert_eq!(name.to_string(), "memory.main.MyTable");
    }
}
