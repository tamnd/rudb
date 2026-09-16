//! SQL parser dialects installed in this build.
//!
//! The registry has one entry today because the vendored grammar is DuckDB's grammar. Keeping the
//! name here rather than in the settings layer makes `current_dialect` a lookup whose behavior
//! changes when another parser is registered, not a string special case that has to be replaced.

/// One installed SQL parser dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dialect {
    name: &'static str,
}

impl Dialect {
    /// The name accepted by `SET current_dialect` and listed by `duckdb_dialects()`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.name
    }
}

/// Every SQL parser dialect installed in this build.
pub const DIALECTS: &[Dialect] = &[Dialect { name: "duckdb" }];

/// Finds an installed dialect without regard to identifier case.
#[must_use]
pub fn dialect_named(name: &str) -> Option<Dialect> {
    DIALECTS.iter().copied().find(|dialect| dialect.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::{DIALECTS, dialect_named};

    #[test]
    fn the_vendored_grammar_is_the_one_registered_dialect() {
        assert_eq!(DIALECTS.len(), 1);
        assert_eq!(dialect_named("DUCKDB").map(|dialect| dialect.name()), Some("duckdb"));
        assert_eq!(dialect_named("cypher"), None);
    }
}
