//! Reading a real Parquet file a chunk at a time.
//!
//! The rule the whole lab is built around is that nothing holds a whole column. The URL column of
//! ClickBench `hits` is around ten gigabytes of characters and the machine it is being measured on
//! has five gigabytes of memory, so a pass that materialises a column does not run at all, and one
//! that does run on a bigger machine tells you nothing about what the write path will be allowed to
//! do. Everything here works in chunks of a fixed number of rows and keeps one chunk live.
//!
//! The chunk is 122,880 rows by default, which is DuckDB's row group. Using the same unit means the
//! sizes in the report can be put next to the sizes DuckDB produces without an argument about
//! whether the difference is the unit.

use std::path::{Path, PathBuf};

use arrow_schema::SchemaRef;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rudb_common::{Error, Result};

use crate::column::{self, Column, Losses};

/// One Parquet file, opened for its metadata and openable again for its data.
#[derive(Debug)]
pub struct Source {
    path: PathBuf,
    pub schema: SchemaRef,
    pub rows: usize,
    pub row_groups: usize,
    pub file_bytes: usize,
    /// Compressed size per column as Parquet itself stored it, which is the number rudb has to
    /// beat. Empty when the schema is nested, because then a field and a leaf are not the same
    /// thing and a comparison per field would be made up.
    pub parquet_bytes: Vec<usize>,
    /// The codecs the file uses, joined, so the report says what it beat rather than just that it
    /// beat Parquet.
    pub codecs: String,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).map_err(|error| {
            Error::invalid_input(format!("cannot open {}: {error}", path.display()))
        })?;
        let file_bytes = file.metadata().map(|meta| meta.len() as usize).unwrap_or_default();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(oops)?;
        let schema = builder.schema().clone();
        let meta = builder.metadata();
        let leaves = meta.file_metadata().schema_descr().num_columns();
        let flat = leaves == schema.fields().len();

        let mut parquet_bytes = if flat { vec![0usize; leaves] } else { Vec::new() };
        let mut codecs: Vec<String> = Vec::new();
        let mut rows = 0usize;
        for group in meta.row_groups() {
            rows += group.num_rows() as usize;
            for (index, chunk) in group.columns().iter().enumerate() {
                if flat {
                    parquet_bytes[index] += chunk.compressed_size().max(0) as usize;
                }
                let codec = format!("{}", chunk.compression());
                if !codecs.contains(&codec) {
                    codecs.push(codec);
                }
            }
        }
        codecs.sort();

        Ok(Self {
            path: path.to_path_buf(),
            schema,
            rows,
            row_groups: meta.num_row_groups(),
            file_bytes,
            parquet_bytes,
            codecs: codecs.join(", "),
        })
    }

    /// Field indices whose names match, or all of them when the list is empty.
    pub fn select(&self, names: &[String]) -> Result<Vec<usize>> {
        if names.is_empty() {
            return Ok((0..self.schema.fields().len()).collect());
        }
        let mut out = Vec::new();
        for name in names {
            let index = self
                .schema
                .fields()
                .iter()
                .position(|field| field.name() == name)
                .ok_or_else(|| Error::invalid_input(format!("no column named {name}")))?;
            out.push(index);
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Read the file, calling `each` once per full chunk and once more for the tail.
    ///
    /// The columns handed to the callback are in the order of `fields`, which is the projection in
    /// file order, and they are the same buffers every time. The callback must not keep them.
    pub fn stream(
        &self,
        fields: &[usize],
        chunk_rows: usize,
        limit: Option<usize>,
        each: &mut dyn FnMut(&[Column], usize) -> Result<()>,
    ) -> Result<Vec<Losses>> {
        let file = std::fs::File::open(&self.path).map_err(|error| {
            Error::invalid_input(format!("cannot open {}: {error}", self.path.display()))
        })?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(oops)?;
        let mask = ProjectionMask::roots(
            builder.metadata().file_metadata().schema_descr(),
            fields.iter().copied(),
        );
        // A batch is the unit the Parquet reader decodes into Arrow, not the unit the encoder sees.
        // Small batches keep the reader's own buffers small, which matters when the chunk is
        // already a hundred megabytes.
        let reader = builder.with_batch_size(8192).with_projection(mask).build().map_err(oops)?;

        let mut columns: Vec<Column> = fields
            .iter()
            .map(|&index| Column::for_type(self.schema.field(index).data_type()))
            .collect();
        let mut losses = vec![Losses::default(); columns.len()];
        let mut held = 0usize;
        let mut seen = 0usize;

        for batch in reader {
            let batch = batch.map_err(oops)?;
            let mut take = batch.num_rows();
            if let Some(limit) = limit {
                take = take.min(limit.saturating_sub(seen));
            }
            if take == 0 {
                break;
            }
            let batch = if take == batch.num_rows() { batch } else { batch.slice(0, take) };
            for (slot, array) in batch.columns().iter().enumerate() {
                let taken = column::append(&mut columns[slot], array)?;
                losses[slot].nulls += taken.nulls;
                losses[slot].narrowed += taken.narrowed;
            }
            held += take;
            seen += take;
            if held >= chunk_rows {
                each(&columns, held)?;
                for column in &mut columns {
                    column.clear();
                }
                held = 0;
            }
            if limit.is_some_and(|limit| seen >= limit) {
                break;
            }
        }
        if held > 0 {
            each(&columns, held)?;
        }
        Ok(losses)
    }
}

/// Parquet has its own error type, Arrow has another, and the lab has one, so this is where they
/// meet. Both of theirs display well enough that the text is all that is worth keeping.
pub fn oops(error: impl std::fmt::Display) -> Error {
    Error::internal(format!("parquet: {error}"))
}
