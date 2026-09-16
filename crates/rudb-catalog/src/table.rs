//! A table: a name, some columns, and the rows.

use rudb_common::{Error, Field, LogicalType, Result, Value};
use rudb_native::Reader as NativeReader;
use rudb_storage::{MemoryTable, Probe};
use rudb_vector::{Chunk, Form, Vector};

use crate::catalog::DETACHED;
use crate::name::{QualifiedName, same_name};

/// Refuses a column list that names the same column twice.
///
/// Exported because the binder makes the same check before anything is created. `CREATE OR REPLACE
/// TABLE` drops the old table on its way to creating the new one, so a check that only happened
/// inside [`Table::new`] would report the duplicate after the old table was already gone.
///
/// # Errors
///
/// If two of the columns have the same name, compared the way SQL compares names, which is without
/// regard to case.
pub fn duplicate_check(columns: &[Field]) -> Result<()> {
    for (at, column) in columns.iter().enumerate() {
        if columns[..at].iter().any(|held| same_name(&held.name, &column.name)) {
            // The one that arrived second is the one named, spelled the way it was written rather
            // than the way the first one was. `CREATE TABLE t (Abc INTEGER, aBC VARCHAR)` says aBC.
            return Err(Error::catalog(format!(
                "Column with name {} already exists!",
                column.name
            )));
        }
    }
    Ok(())
}

/// Rows held while a table is being built or read from a committed native snapshot.
#[derive(Debug, Clone)]
pub enum Rows {
    /// Mutable chunks owned by this process.
    Memory(MemoryTable),
    /// Immutable stripes read by projected column from one file.
    Native(NativeReader),
}

impl Rows {
    /// Exact leading value frequencies from a committed native snapshot.
    ///
    /// In-memory tables have no persisted synopsis and return `None`.
    pub fn top_frequencies(&self, column: usize, top: usize) -> Result<Option<Vec<(Value, u64)>>> {
        match self {
            Self::Memory(_) => Ok(None),
            Self::Native(reader) => reader.top_frequencies(column, top),
        }
    }

    /// Number of rows in one independently readable chunk.
    pub fn chunk_len(&self, at: usize) -> Result<usize> {
        Ok(match self {
            Self::Memory(rows) => rows
                .chunk(at)
                .ok_or_else(|| Error::internal("row ordinal names a missing chunk"))?
                .len(),
            Self::Native(reader) => reader
                .table()
                .stripes()
                .get(at)
                .ok_or_else(|| Error::internal("row ordinal names a missing stripe"))?
                .rows(),
        })
    }

    /// Reads selected rows by table-wide ordinal in the order requested.
    pub fn rows_at(
        &self,
        types: &[LogicalType],
        columns: &[usize],
        ordinals: &[u64],
    ) -> Result<Chunk> {
        if columns.len() != types.len() {
            return Err(Error::internal("a row fetch has a different number of columns and types"));
        }
        if let Self::Native(reader) = self {
            return Self::native_rows_at(reader, types, columns, ordinals);
        }
        let mut values = vec![Vec::with_capacity(ordinals.len()); columns.len()];
        let mut cached: Option<(usize, Chunk)> = None;
        for &ordinal in ordinals {
            let ordinal = usize::try_from(ordinal)
                .map_err(|_| Error::internal("row ordinal does not fit this platform"))?;
            let mut start = 0_usize;
            let mut found = None;
            for chunk in 0..self.chunk_count() {
                let len = self.chunk_len(chunk)?;
                if ordinal < start.saturating_add(len) {
                    found = Some((chunk, ordinal - start));
                    break;
                }
                start = start.saturating_add(len);
            }
            let (chunk, row) =
                found.ok_or_else(|| Error::internal("row ordinal is past the table"))?;
            if cached.as_ref().is_none_or(|(held, _)| *held != chunk) {
                cached = Some((chunk, self.read(chunk, columns)?));
            }
            let Some((_, held)) = &cached else {
                return Err(Error::internal("row chunk was not cached"));
            };
            for (at, values) in values.iter_mut().enumerate() {
                values.push(held.value_at(row, at));
            }
        }
        let vectors = values
            .into_iter()
            .zip(types)
            .map(|(values, ty)| Vector::from_values(ty.clone(), &values))
            .collect::<Result<Vec<_>>>()?;
        Chunk::with_rows(vectors, ordinals.len())
    }

    /// Reads a native row fetch across all requested stripes and columns in one worker fan-out.
    ///
    /// Ordinary scans already parallelize by stripe in the pipeline above the reader. A late fetch
    /// is deliberately one pipeline instance and commonly asks for all hundred ClickBench columns
    /// from rows in several stripes. Keeping the workers alive across those stripes avoids a
    /// scoped thread launch and join for every winning stripe.
    fn native_rows_at(
        reader: &NativeReader,
        types: &[LogicalType],
        columns: &[usize],
        ordinals: &[u64],
    ) -> Result<Chunk> {
        if columns.is_empty() {
            return Chunk::with_rows(Vec::new(), ordinals.len());
        }
        let mut ends = Vec::with_capacity(reader.table().stripes().len());
        let mut end = 0_usize;
        for stripe in reader.table().stripes() {
            end = end.saturating_add(stripe.rows());
            ends.push(end);
        }
        let mut locations = Vec::with_capacity(ordinals.len());
        for &ordinal in ordinals {
            let ordinal = usize::try_from(ordinal)
                .map_err(|_| Error::internal("row ordinal does not fit this platform"))?;
            let stripe = ends.partition_point(|&end| end <= ordinal);
            if stripe == ends.len() {
                return Err(Error::internal("row ordinal is past the table"));
            }
            let start = stripe.checked_sub(1).map_or(0, |before| ends[before]);
            locations.push((stripe, ordinal - start));
        }
        const MIN_COLUMNS_PER_WORKER: usize = 16;
        const MAX_WORKERS: usize = 8;
        let workers = columns.len().div_ceil(MIN_COLUMNS_PER_WORKER).min(MAX_WORKERS);
        if workers <= 1 {
            let vectors = Self::read_native_columns(reader, columns, types, &locations)?;
            return Chunk::with_rows(vectors, ordinals.len());
        }
        let width = columns.len().div_ceil(workers);
        let pieces = std::thread::scope(|scope| {
            let handles = columns
                .chunks(width)
                .zip(types.chunks(width))
                .map(|(columns, types)| {
                    scope.spawn(|| Self::read_native_columns(reader, columns, types, &locations))
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| Error::internal("a native row fetch worker panicked"))?
                })
                .collect::<Result<Vec<_>>>()
        })?;
        let mut vectors = Vec::with_capacity(columns.len());
        for piece in pieces {
            vectors.extend(piece);
        }
        Chunk::with_rows(vectors, ordinals.len())
    }

    fn read_native_columns(
        reader: &NativeReader,
        columns: &[usize],
        types: &[LogicalType],
        locations: &[(usize, usize)],
    ) -> Result<Vec<Vector>> {
        let mut values = vec![Vec::with_capacity(locations.len()); columns.len()];
        let mut from = 0;
        while from < locations.len() {
            let stripe = locations[from].0;
            let mut upto = from + 1;
            while upto < locations.len() && locations[upto].0 == stripe {
                upto += 1;
            }
            let held = reader.read_sparse(stripe, columns)?;
            for &(_, row) in &locations[from..upto] {
                for (at, values) in values.iter_mut().enumerate() {
                    values.push(held.value_at(row, at));
                }
            }
            from = upto;
        }
        values
            .into_iter()
            .zip(types)
            .map(|(values, ty)| Vector::from_values(ty.clone(), &values))
            .collect()
    }

    /// Column types.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        match self {
            Self::Memory(rows) => rows.types().to_vec(),
            Self::Native(reader) => {
                reader.table().fields().iter().map(|field| field.ty.clone()).collect()
            }
        }
    }

    /// Total row count.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Memory(rows) => rows.len(),
            Self::Native(reader) => reader.table().rows(),
        }
    }

    /// Whether there are no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether these rows already come from a committed native snapshot.
    #[must_use]
    pub fn is_native(&self) -> bool {
        matches!(self, Self::Native(_))
    }

    /// Number of independently readable chunks or stripes.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        match self {
            Self::Memory(rows) => rows.chunk_count(),
            Self::Native(reader) => reader.table().stripes().len(),
        }
    }

    /// Reads only projected columns.
    pub fn read(&self, at: usize, columns: &[usize]) -> Result<Chunk> {
        match self {
            Self::Memory(rows) => rows.read(at, columns),
            Self::Native(reader) => reader.read(at, columns),
        }
    }

    /// Whether statistics prove this chunk cannot match.
    #[must_use]
    pub fn skips(&self, at: usize, probes: &[Probe]) -> bool {
        match self {
            Self::Memory(rows) => rows.skips(at, probes),
            Self::Native(reader) => reader.skips(at, probes),
        }
    }

    /// One whole in-memory chunk, used by checkpointing and tests.
    #[must_use]
    pub fn chunk(&self, at: usize) -> Option<&Chunk> {
        match self {
            Self::Memory(rows) => rows.chunk(at),
            Self::Native(_) => None,
        }
    }
}

/// One table.
///
/// The rows are a [`MemoryTable`] because that is what M0 has. When the storage format arrives the
/// field changes and this type does not, which is the reason the catalog holds the rows behind a
/// handle rather than being the rows.
#[derive(Debug, Clone)]
pub struct Table {
    name: QualifiedName,
    columns: Vec<Field>,
    rows: Rows,
    /// What `duckdb_tables()` reports as `table_oid`, stamped by the catalog when this goes in.
    oid: i64,
}

impl Table {
    /// A table with no rows in it.
    ///
    /// # Errors
    ///
    /// If two columns have the same name, which SQL does not allow and which would make a column
    /// reference ambiguous in a way no error message could explain later. The message is DuckDB's,
    /// which names the column and not the table and is a catalog error rather than a binder one,
    /// because the same sentence comes out of `CREATE TABLE t (a INT, a INT)` and out of a
    /// `CREATE TABLE ... AS` whose column list repeats a name.
    pub fn new(name: QualifiedName, columns: Vec<Field>) -> Result<Self> {
        duplicate_check(&columns)?;
        let types = columns.iter().map(|column| column.ty.clone()).collect();
        Ok(Self { name, columns, rows: Rows::Memory(MemoryTable::new(types)), oid: DETACHED })
    }

    /// A table whose stripes are read from one committed native file.
    ///
    /// # Errors
    ///
    /// If the reader's stored schema has duplicate column names.
    pub fn native(name: QualifiedName, reader: NativeReader) -> Result<Self> {
        let columns = reader.table().fields().to_vec();
        duplicate_check(&columns)?;
        Ok(Self { name, columns, rows: Rows::Native(reader), oid: DETACHED })
    }

    /// The number the catalog tables join on, and [`DETACHED`] for a table not in a catalog.
    #[must_use]
    pub fn oid(&self) -> i64 {
        self.oid
    }

    /// Stamps the oid, which only [`crate::Catalog::create_table`] does.
    pub(crate) fn stamp(&mut self, oid: i64) {
        self.oid = oid;
    }

    /// The three part name.
    #[must_use]
    pub fn name(&self) -> &QualifiedName {
        &self.name
    }

    /// The columns, in order.
    #[must_use]
    pub fn columns(&self) -> &[Field] {
        &self.columns
    }

    /// The column types, in order.
    #[must_use]
    pub fn types(&self) -> Vec<LogicalType> {
        self.columns.iter().map(|column| column.ty.clone()).collect()
    }

    /// Where a column sits, by name, under the identifier rule.
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| same_name(&column.name, name))
    }

    /// The rows.
    #[must_use]
    pub fn rows(&self) -> &Rows {
        &self.rows
    }

    /// Replaces an empty mutable table with its committed native snapshot.
    ///
    /// # Errors
    ///
    /// If rows are already present or the stored schema differs from this table.
    pub fn commit_native(&mut self, reader: NativeReader) -> Result<()> {
        if !self.rows.is_empty() {
            return Err(Error::not_implemented(
                "streaming a native insert into a table that already has rows",
            ));
        }
        if reader.table().fields() != self.columns {
            return Err(Error::internal("a committed native snapshot changed its table schema"));
        }
        self.rows = Rows::Native(reader);
        Ok(())
    }

    /// The rows, to add to.
    ///
    /// This is the way past the constraint check, and the two `append` methods here are the way
    /// through it. A caller that already knows what it is holding, such as the loader that built
    /// the chunk out of a file the table was declared from, can take this one.
    ///
    /// # Panics
    ///
    /// If called for an immutable table opened from a committed native file.
    pub fn rows_mut(&mut self) -> &mut MemoryTable {
        match &mut self.rows {
            Rows::Memory(rows) => rows,
            Rows::Native(_) => panic!("a committed native table is immutable"),
        }
    }

    /// Adds a chunk, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If the chunk does not match the table, or if a `NOT NULL` column is handed a null. DuckDB
    /// raises a constraint error there and so does this, with the same shape of message, because a
    /// program that catches one by its text is a program rudb has to not surprise.
    pub fn append(&mut self, chunk: Chunk) -> Result<()> {
        self.refuse_nulls(&chunk)?;
        match &mut self.rows {
            Rows::Memory(rows) => rows.append(chunk),
            Rows::Native(_) => Err(Error::not_implemented("appending to a committed native table")),
        }
    }

    /// Adds rows of single values, refusing a null in a column that said it would not have one.
    ///
    /// # Errors
    ///
    /// If a row is not as wide as the table, if a value will not convert to its column's type, or
    /// if a `NOT NULL` column is handed a null.
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        for row in rows {
            for (at, column) in self.columns.iter().enumerate() {
                if column.not_null && row.get(at).is_some_and(Value::is_null) {
                    return Err(self.null_in(&column.name));
                }
            }
        }
        match &mut self.rows {
            Rows::Memory(held) => held.append_rows(rows),
            Rows::Native(_) => Err(Error::not_implemented("appending to a committed native table")),
        }
    }

    /// Checks a chunk against the `NOT NULL` columns before any of it is kept.
    ///
    /// A table with no such column pays one walk of the column list and touches no data, which is
    /// most tables. A column that does refuse nulls is checked through its validity mask when the
    /// mask is the whole story, which is one word per sixty four rows rather than a read per row.
    /// A dictionary or a constant can hold the null in the body it points at instead, where the
    /// mask cannot see it, so those two are asked value by value.
    fn refuse_nulls(&self, chunk: &Chunk) -> Result<()> {
        for (at, column) in self.columns.iter().enumerate() {
            if !column.not_null {
                continue;
            }
            let vector = chunk.column(at)?;
            let found = match vector.form() {
                Form::Flat | Form::Sequence => {
                    vector.validity().has_nulls(vector.len())
                        && (0..vector.len()).any(|row| !vector.validity().is_valid(row))
                }
                _ => (0..vector.len()).any(|row| vector.value_at(row).is_null()),
            };
            if found {
                return Err(self.null_in(&column.name));
            }
        }
        Ok(())
    }

    /// The error DuckDB raises when a null reaches a column that refuses them.
    fn null_in(&self, column: &str) -> Error {
        Error::constraint(format!("NOT NULL constraint failed: {}.{}", self.name.table, column))
    }
}

#[cfg(test)]
mod tests {
    use rudb_vector::Vector;

    use super::*;

    fn hits() -> Table {
        Table::new(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::new("UserID", LogicalType::BigInt),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns with different names")
    }

    #[test]
    fn a_column_is_found_however_it_is_spelled() {
        let table = hits();
        assert_eq!(table.column_index("userid"), Some(0));
        assert_eq!(table.column_index("SEARCHPHRASE"), Some(1));
        assert_eq!(table.column_index("nope"), None);
    }

    #[test]
    fn two_columns_with_one_name_is_caught() {
        let error = Table::new(
            QualifiedName::new("memory", "main", "t"),
            vec![Field::new("a", LogicalType::Integer), Field::new("A", LogicalType::Varchar)],
        )
        .expect_err("two columns called a");
        // Named after the second of the two and spelled the way it was written there, which is what
        // duckdb v1.4.1 says for `CREATE TABLE t (a INTEGER, A VARCHAR)`.
        assert_eq!(error.to_string(), "Catalog Error: Column with name A already exists!");
    }

    #[test]
    fn a_new_table_is_empty_and_typed() {
        let mut table = hits();
        assert!(table.rows().is_empty());
        assert_eq!(table.rows().types(), table.types());
        table
            .rows_mut()
            .append_rows(&[vec![Value::BigInt(1), Value::Varchar("a".to_string())]])
            .expect("a row of the table's own types");
        assert_eq!(table.rows().len(), 1);
    }

    /// A table whose first column refuses nulls and whose second does not.
    fn required() -> Table {
        Table::new(
            QualifiedName::new("memory", "main", "hits"),
            vec![
                Field::required("UserID", LogicalType::BigInt),
                Field::new("SearchPhrase", LogicalType::Varchar),
            ],
        )
        .expect("two columns with different names")
    }

    #[test]
    fn a_null_in_a_not_null_column_is_refused() {
        let mut table = required();
        let error = table
            .append_rows(&[vec![Value::Null, Value::Varchar("a".to_string())]])
            .expect_err("a null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert!(table.rows().is_empty(), "the row was kept anyway");
    }

    #[test]
    fn a_null_in_a_column_that_allows_them_is_kept() {
        let mut table = required();
        table.append_rows(&[vec![Value::BigInt(7), Value::Null]]).expect("a null in SearchPhrase");
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn a_chunk_is_checked_through_its_mask() {
        let mut table = required();
        let phrase = Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 2);
        let good = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::BigInt(2)])
                .expect("two ids"),
            phrase.clone(),
        ])
        .expect("two columns of two rows");
        table.append(good).expect("no nulls anywhere");
        let bad = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &[Value::BigInt(1), Value::Null])
                .expect("an id and a null"),
            phrase,
        ])
        .expect("two columns of two rows");
        let error = table.append(bad).expect_err("a null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
        assert_eq!(table.rows().len(), 2, "the bad chunk was kept anyway");
    }

    #[test]
    fn a_null_hiding_in_a_constant_is_found() {
        let mut table = required();
        let chunk = Chunk::new(vec![
            Vector::constant(LogicalType::BigInt, Value::Null, 4),
            Vector::constant(LogicalType::Varchar, Value::Varchar("a".to_string()), 4),
        ])
        .expect("two columns of four rows");
        let error = table.append(chunk).expect_err("a constant null in UserID");
        assert_eq!(error.message(), "NOT NULL constraint failed: hits.UserID");
    }
}
