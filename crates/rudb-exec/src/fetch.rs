//! Reading the rest of a row back out of the file once something below has picked the row.
//!
//! This is the top half of late materialisation and the reason [`Node::Fetch`](rudb_plan::Node)
//! exists. `SELECT * FROM hits ORDER BY EventTime LIMIT 10` needs one column of every row to decide
//! which ten rows win and all hundred and five columns of the ten that did. A plan that carries the
//! wide rows through the top N reads the whole file to throw almost all of it away, and measured on
//! one million ClickBench rows that was 1.08 seconds against DuckDB's 0.12.
//!
//! What arrives here is the winners with their ordinals, and what goes out is those same rows read
//! back from the file. The ordinals are sorted before the read and the answer is put back into the
//! order it arrived in afterwards, because the reader walks the file forwards once and the operator
//! above this one is entitled to the order the top N produced.
//!
//! # Why it reads every column rather than the ones that were left behind
//!
//! Stitching the carried columns together with the fetched ones would save reading the ordering
//! column twice, which at ten rows is a few pages. It would cost this operator a map from each
//! output column to either its input position or its file position, and the rewrite below a way to
//! describe that map in the plan. Reading the whole row is one call and the plan node is a file, a
//! column list and where the ordinals are.

use std::sync::Arc;

use rudb_common::{Error, Field, LogicalType, Result};
use rudb_functions::open_parquet;
use rudb_kernels::cast;
use rudb_metrics::Counters;
use rudb_parquet::Reader;
use rudb_pipeline::{Progress, Stream};
use rudb_plan::{ExprRef, Plan, Slice};
use rudb_vector::{Chunk, Data};

use crate::prepared::{Prepared, Scratch};
use crate::schema::Schema;
use crate::source::{file_arguments, positions};

/// Reads the rows its input names out of one Parquet file.
#[derive(Debug)]
pub(crate) struct Fetch {
    path: String,
    wanted: Vec<Field>,
    row: Prepared,
    schema: Schema,
    counters: Option<Arc<Counters>>,
}

/// Everything one instance of a fetch mutates, which is the ordinal's scratch and its open file.
///
/// The reader is opened on the first chunk rather than when the operator is built, because
/// [`Stream::local`] cannot report a failure and a missing file has to be reported rather than
/// unwrapped. A fetch that is never handed a row never opens anything.
#[derive(Debug)]
pub(crate) struct Fetching {
    scratch: Scratch,
    reader: Option<Reader>,
}

impl Fetch {
    /// # Errors
    ///
    /// If the plan names other than one file, if the ordinal expression does not resolve against
    /// the input's schema, or if the ordinals are not `BIGINT`.
    pub(crate) fn new(
        plan: &Plan,
        input: &Schema,
        index: u32,
        args: Slice,
        columns: Slice,
        row: ExprRef,
    ) -> Result<Self> {
        let mut paths = file_arguments(plan, args, rudb_functions::TableFunction::ReadParquet)?;
        if paths.len() != 1 {
            return Err(Error::internal(format!(
                "a fetch over {} files, where a row ordinal names no row at all",
                paths.len()
            )));
        }
        let wanted = plan.field_list(columns).to_vec();
        Ok(Self {
            path: paths.remove(0),
            schema: Schema::numbered(wanted.clone(), index),
            wanted,
            row: Prepared::one(plan, row, input)?,
            counters: None,
        })
    }

    /// Connects this operator's file counters to the row that owns it.
    pub(crate) fn watched(mut self, counters: Arc<Counters>) -> Self {
        self.counters = Some(counters);
        self
    }

    /// What this fetch produces, which is what the node it replaced produced.
    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Opens the file and narrows it to the columns this fetch produces.
    fn open(&self) -> Result<Reader> {
        let mut reader = open_parquet(&self.path)?;
        let held = reader.fields();
        reader.project(&positions(
            rudb_functions::TableFunction::ReadParquet,
            &self.wanted,
            &held,
            &self.path,
            None,
        )?)?;
        let text: Vec<bool> =
            self.wanted.iter().map(|field| field.ty == LogicalType::Varchar).collect();
        reader.as_string(&text);
        Ok(reader)
    }

    /// The chunk with every column in the type the plan was bound against.
    ///
    /// Almost always nothing, because the plan's types came from this file's own footer. It is not
    /// nothing when the file was replaced between binding and running, which is the same case
    /// [`FileScan::conform`](crate::source::FileScan) covers and is covered the same way.
    fn conform(&self, chunk: Chunk) -> Result<Chunk> {
        let rows = chunk.len();
        let settled = self.wanted.iter().enumerate().all(|(at, field)| {
            chunk.column(at).is_ok_and(|column| column.logical_type() == &field.ty)
        });
        if settled {
            return Ok(chunk);
        }
        let mut columns = Vec::with_capacity(self.wanted.len());
        for (at, field) in self.wanted.iter().enumerate() {
            let column = chunk.column(at)?;
            if column.logical_type() == &field.ty {
                columns.push(column.clone());
                continue;
            }
            columns.push(cast(column, &field.ty, false).map_err(|error| {
                Error::conversion(format!(
                    "Error while reading file \"{}\": failed to cast column \"{}\" from type {} \
                     to {}: {}",
                    self.path,
                    field.name,
                    column.logical_type(),
                    field.ty,
                    error.message()
                ))
            })?);
        }
        Chunk::with_rows(columns, rows)
    }
}

impl Stream for Fetch {
    type Local = Fetching;

    fn local(&self) -> Fetching {
        Fetching { scratch: self.row.scratch(), reader: None }
    }

    fn push(&self, chunk: &mut Chunk, local: &mut Fetching) -> Result<Progress> {
        if chunk.is_empty() {
            *chunk = Chunk::empty(&self.schema.types());
            return Ok(Progress::More);
        }
        let ordinals = self.row.evaluate_one(chunk, &mut local.scratch)?;
        let count = ordinals.len();
        // flatten: the ordinals get sorted and looked at out of order, which is what a flat buffer
        // is for, and there are at most a few thousand of them because a top N is what feeds this.
        let flat = ordinals.flatten()?;
        let held: &[i64] = match flat.data() {
            Some(Data::Int64(values)) if !flat.validity().has_nulls(count) => values.as_slice(),
            _ => {
                return Err(Error::internal(
                    "a fetch was handed something other than a row ordinal in every row",
                ));
            }
        };
        // Sorted for the reader and deduplicated because it asks for strictly increasing ordinals,
        // then mapped back, so the rows come out in the order they went in however they arrived.
        let mut order: Vec<usize> = (0..count).collect();
        order.sort_by_key(|&at| held[at]);
        let mut rows: Vec<u64> = Vec::with_capacity(count);
        let mut taken = vec![0_u32; count];
        for &at in &order {
            let row = u64::try_from(held[at]).map_err(|_| {
                Error::internal(format!(
                    "a fetch was handed the row ordinal {}, which is before the file starts",
                    held[at]
                ))
            })?;
            if rows.last() != Some(&row) {
                rows.push(row);
            }
            taken[at] = u32::try_from(rows.len() - 1).unwrap_or(u32::MAX);
        }

        let reader = match local.reader.as_mut() {
            Some(reader) => reader,
            None => local.reader.insert(self.open()?),
        };
        let before = reader.bytes_read();
        let fetched = reader.rows_at(&rows)?;
        if let Some(counters) = &self.counters {
            counters.read(reader.bytes_read().saturating_sub(before));
        }

        let fetched = self.conform(fetched)?;
        let width = fetched.width();
        let mut columns = Vec::with_capacity(width);
        for at in 0..width {
            columns.push(fetched.column(at)?.gather(&taken)?);
        }
        *chunk = Chunk::with_rows(columns, taken.len())?;
        Ok(Progress::More)
    }
}
