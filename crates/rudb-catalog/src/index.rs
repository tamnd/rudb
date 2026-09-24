//! `CREATE INDEX`, held on the table it is over.
//!
//! An index answers no query differently, so what it is here is what it changes about the table: a
//! unique one over plain columns is one more key every write is checked against, and any index
//! stops most `ALTER TABLE` changes the way the pin's does, because its entries depend on the
//! table. Index names are a namespace of their own in a schema, apart from tables and views, which
//! is why `CREATE INDEX t ON t(a)` is fine there.

/// One index over a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// The name, unique among the indexes of the schema the table is in.
    pub name: String,
    /// Whether it is a `UNIQUE` index.
    pub unique: bool,
    /// The columns it reads, by place in the table. For an index whose elements are all bare
    /// columns this is those columns in the order written, which is its key.
    pub columns: Vec<usize>,
    /// Whether every element is a bare column.
    pub plain: bool,
    /// What `duckdb_indexes()` reports as `expressions`, such as `[b, '((a + 1))']`.
    pub expressions: String,
    /// The statement written back out, the way `duckdb_indexes()` reports it.
    pub sql: String,
    /// What `duckdb_indexes()` reports as `index_oid`, stamped by the catalog when this goes in.
    pub oid: i64,
}
