//! `rudb_strategies()`, the table that says what the engine can be made of.
//!
//! One row per registered implementation of a seam, and one row for every seam that has no
//! implementations yet, which today is twenty six of the twenty seven. A reader who wants to know
//! what rudb will let them swap runs this, and a reader who wants to know what it lets them swap
//! now reads the same table and finds the implementation columns null.
//!
//! The rows are built when the operator is, because there are a few dozen of them and they come
//! from a list that cannot change while the process runs.

use rudb_common::{Error, Result, Value};
use rudb_functions::strategy_fields;
use rudb_plan::{Plan, Slice};
use rudb_seam::{SeamId, StrategyRow};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::operator::Operator;
use crate::register::registries;
use crate::schema::Schema;

/// The rows of `rudb_strategies()`.
#[derive(Debug)]
pub(crate) struct Strategies {
    schema: Schema,
    chunks: Vec<Chunk>,
    at: usize,
}

impl Strategies {
    /// Every seam and everything registered against it, in the columns the plan asked for.
    ///
    /// The plan's column list is resolved against this table's by name, which is what [`Scan`] and
    /// [`FileScan`] both do and for the same reason: the binder projects every column in order
    /// today, and the moment a pass trims the list the operator has to hand back the trimmed one
    /// rather than its own first few columns.
    ///
    /// [`Scan`]: crate::source
    /// [`FileScan`]: crate::source
    ///
    /// # Errors
    ///
    /// If the plan asks for a column this table does not have, which is a bug in the binder rather
    /// than anything a query can write.
    pub(crate) fn new(plan: &Plan, index: u32, columns: Slice) -> Result<Self> {
        let all = strategy_fields();
        let wanted = plan.field_list(columns).to_vec();
        let mut positions = Vec::with_capacity(wanted.len());
        for field in &wanted {
            let position =
                all.iter().position(|held| held.name == field.name).ok_or_else(|| {
                    Error::internal(format!("rudb_strategies() has no column named {}", field.name))
                })?;
            positions.push(position);
        }
        let schema = Schema::numbered(wanted, index);

        let registered = registries().rows();
        let mut rows = Vec::new();
        for seam in SeamId::ALL.iter().copied() {
            let mut any = false;
            for row in registered.iter().filter(|row| row.seam == seam) {
                rows.push(implemented(row));
                any = true;
            }
            if !any {
                rows.push(planned(seam));
            }
        }

        let types = schema.types();
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < rows.len() {
            let end = (start + VECTOR_SIZE).min(rows.len());
            let mut built = Vec::with_capacity(types.len());
            for (wanted, ty) in positions.iter().zip(&types) {
                let column: Vec<Value> =
                    rows[start..end].iter().map(|row| row[*wanted].clone()).collect();
                built.push(Vector::from_values(ty.clone(), &column)?);
            }
            chunks.push(Chunk::with_rows(built, end - start)?);
            start = end;
        }
        Ok(Self { schema, chunks, at: 0 })
    }
}

impl Operator for Strategies {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> Result<Option<Chunk>> {
        let chunk = self.chunks.get(self.at).cloned();
        self.at += 1;
        Ok(chunk)
    }
}

/// The row an implementation produces.
fn implemented(row: &StrategyRow) -> Vec<Value> {
    vec![
        text(row.seam.name()),
        text(row.seam.milestone()),
        text(row.seam.describe()),
        text(row.name),
        text(row.describe),
        text(&row.provenance.to_string()),
        text(&row.determinism.to_string()),
        Value::Boolean(row.is_reference),
        Value::Boolean(row.is_default),
    ]
}

/// The row a seam with no registry produces.
///
/// The three columns that describe the seam are filled and the six that describe an implementation
/// are null, rather than the seam being left out of the table. A seam nobody has built is a
/// commitment somebody has made, the milestone column says who owes it, and a table that listed
/// only what exists would make the engine look finished.
fn planned(seam: SeamId) -> Vec<Value> {
    vec![
        text(seam.name()),
        text(seam.milestone()),
        text(seam.describe()),
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
        Value::Null,
    ]
}

fn text(value: &str) -> Value {
    Value::Varchar(value.to_string())
}
