//! The Appender, `07-the-head.md` section 7.11: rows a value at a time, with no SQL in between.
//!
//! A value is cast to its column's type when it is appended, so a string that is not a number
//! fails on the call that handed it over rather than at the flush. A finished row goes into one
//! buffer per column, and every [`VECTOR_SIZE`] rows the buffers become a chunk. The chunks are
//! written at [`Appender::flush`], at [`Appender::close`], and on their own once a quarter million
//! rows are waiting, the way an `INSERT` of the same rows would be: constraints, then the log,
//! then the commit. So rows become visible and durable at a flush and not before, which is the
//! pin's behaviour.
//!
//! A flush that fails throws away what it was writing, and the table is left as it was before it.

use rudb_catalog::QualifiedName;
use rudb_common::{Error, Field, Result, Value};
use rudb_vector::{Chunk, VECTOR_SIZE, Vector};

use crate::Database;

/// How many rows wait in chunks before the Appender writes them without being asked.
const WAITING: usize = 262_144;

/// Appends rows to one table, a value at a time.
///
/// Rows are written at [`Appender::flush`] and [`Appender::close`]. Dropping an Appender without
/// closing it flushes what it holds and drops any error, so a caller that wants to know closes it.
#[derive(Debug)]
pub struct Appender {
    db: Database,
    name: QualifiedName,
    fields: Vec<Field>,
    defaults: Vec<Option<String>>,
    /// One buffer per column, each holding the values of the rows not yet in a chunk.
    columns: Vec<Vec<Value>>,
    /// The column the next value lands in.
    next: usize,
    chunks: Vec<Chunk>,
    waiting: usize,
    closed: bool,
}

impl Appender {
    pub(crate) fn new(
        db: Database,
        name: QualifiedName,
        fields: Vec<Field>,
        defaults: Vec<Option<String>>,
    ) -> Self {
        let columns = fields.iter().map(|_| Vec::with_capacity(VECTOR_SIZE)).collect();
        Self {
            db,
            name,
            fields,
            defaults,
            columns,
            next: 0,
            chunks: Vec::new(),
            waiting: 0,
            closed: false,
        }
    }

    /// The table's columns, in the order a row's values are appended.
    #[must_use]
    pub fn columns(&self) -> &[Field] {
        &self.fields
    }

    /// Appends the next value of the row being built, cast to its column's type.
    ///
    /// # Errors
    ///
    /// If the row already has a value for every column, or if the value does not cast.
    pub fn append(&mut self, value: Value) -> Result<()> {
        let Some(field) = self.fields.get(self.next) else {
            return Err(Error::invalid_input("Too many appends for chunk!"));
        };
        let value = if value.is_null() || value.logical_type() == field.ty {
            value
        } else {
            rudb_kernels::cast::cast_value(&value, &field.ty, false)?
        };
        self.columns[self.next].push(value);
        self.next += 1;
        Ok(())
    }

    /// Appends the column's default as the next value, or a null for a column with none.
    ///
    /// # Errors
    ///
    /// The same as [`Appender::append`], and whatever evaluating the default raises.
    pub fn append_default(&mut self) -> Result<()> {
        let Some(field) = self.fields.get(self.next) else {
            return Err(Error::invalid_input("Too many appends for chunk!"));
        };
        let value = match &self.defaults[self.next] {
            Some(default) => self.db.value(&format!("SELECT CAST(({default}) AS {})", field.ty))?,
            None => Value::Null,
        };
        self.append(value)
    }

    /// Finishes the row being built.
    ///
    /// # Errors
    ///
    /// If the row does not have a value for every column, or if the rows waiting were due to be
    /// written and the write failed.
    pub fn end_row(&mut self) -> Result<()> {
        if self.next != self.fields.len() {
            return Err(Error::invalid_input(
                "Call to EndRow before all columns have been appended to!",
            ));
        }
        self.next = 0;
        if self.columns.first().is_some_and(|column| column.len() >= VECTOR_SIZE) {
            self.seal()?;
            if self.waiting >= WAITING {
                self.write()?;
            }
        }
        Ok(())
    }

    /// Appends a whole row, one value per column, and finishes it.
    ///
    /// # Errors
    ///
    /// The same as [`Appender::append`] and [`Appender::end_row`].
    pub fn append_row(&mut self, row: impl IntoIterator<Item = Value>) -> Result<()> {
        for value in row {
            self.append(value)?;
        }
        self.end_row()
    }

    /// Writes every finished row, so it is visible and durable when this returns.
    ///
    /// A row half built stays where it is and is finished and written later.
    ///
    /// # Errors
    ///
    /// If a row breaks a constraint of the table, in which case none of the rows since the last
    /// flush are kept.
    pub fn flush(&mut self) -> Result<()> {
        self.seal()?;
        self.write()
    }

    /// Flushes and closes the Appender.
    ///
    /// # Errors
    ///
    /// If a row is half built, or the same as [`Appender::flush`].
    pub fn close(mut self) -> Result<()> {
        self.closed = true;
        if self.next != 0 {
            return Err(Error::invalid_input(
                "Failed to close appender: a row was started and not ended",
            ));
        }
        self.flush()
    }

    /// The finished rows in the column buffers, as a chunk.
    fn seal(&mut self) -> Result<()> {
        // A half built row has a value in the columns before `next` and none after, and one with
        // every value but no `end_row` yet has one in all of them.
        let shortest = self.columns.iter().map(Vec::len).min().unwrap_or(0);
        let finished = if self.next == self.fields.len() && self.next > 0 {
            shortest.saturating_sub(1)
        } else {
            shortest
        };
        if finished == 0 {
            return Ok(());
        }
        let mut vectors = Vec::with_capacity(self.fields.len());
        for (at, field) in self.fields.iter().enumerate() {
            let column = &mut self.columns[at];
            let started = column.split_off(finished);
            let values = std::mem::replace(column, started);
            vectors.push(Vector::from_values(field.ty.clone(), &values)?);
        }
        self.chunks.push(Chunk::new(vectors)?);
        self.waiting += finished;
        Ok(())
    }

    /// Writes the chunks waiting.
    fn write(&mut self) -> Result<()> {
        let chunks = std::mem::take(&mut self.chunks);
        self.waiting = 0;
        self.db.append_chunks(&self.name, chunks)
    }
}

impl Drop for Appender {
    fn drop(&mut self) {
        if !self.closed && self.next == 0 {
            let _ = self.flush();
        }
    }
}
