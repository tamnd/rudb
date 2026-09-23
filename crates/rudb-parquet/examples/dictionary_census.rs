//! How much of a Parquet file's values sit in dictionary encoded pages, column by column.
//!
//! W2 transcodes a Parquet dictionary page into global codes once, rather than decoding every value
//! and hashing it again, and it can only win on the values that were dictionary encoded in the
//! first place. The footer does not say that. A column chunk lists every encoding any of its pages
//! used, and a writer whose dictionary grew past its limit falls back to plain pages part way
//! through the chunk, so a chunk that lists `PLAIN_DICTIONARY` can be mostly plain. This walks the
//! page headers of every chunk, which is a read of the headers and not of the values, and counts
//! the values each kind of page holds.
//!
//! A value's raw size is its physical width, or for a byte array the length the page gives it. A
//! dictionary encoded page does not say how long its values are, so they are counted at the mean
//! length of the chunk's dictionary. That is an estimate and it is labelled as one in the output.
//!
//! Usage: `cargo run --release -p rudb-parquet --example dictionary_census -- FILE`

use std::path::Path;

use rudb_io::{Filesystem, OpenMode, RealFilesystem};
use rudb_parquet::{Body, Encoding, Header, Metadata, Physical};

/// What one column's pages came to across every row group.
#[derive(Debug, Default, Clone)]
struct Tally {
    name: String,
    /// Values in pages whose indices point into a dictionary, and in pages of any other encoding.
    coded_values: u64,
    other_values: u64,
    /// Uncompressed page bytes of each kind, and of the dictionary pages themselves.
    coded_bytes: u64,
    other_bytes: u64,
    dictionary_bytes: u64,
    /// Estimated raw bytes of the values in each kind of page.
    coded_raw: f64,
    other_raw: f64,
}

fn width(physical: Physical, fixed: i32) -> Option<u64> {
    match physical {
        Physical::Boolean => Some(1),
        Physical::Int32 | Physical::Float => Some(4),
        Physical::Int64 | Physical::Double => Some(8),
        Physical::Int96 => Some(12),
        Physical::FixedLenByteArray => u64::try_from(fixed).ok(),
        Physical::ByteArray => None,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: dictionary_census FILE")?;
    let file = RealFilesystem::new().open(Path::new(&path), OpenMode::Read)?;
    let metadata = Metadata::read(file.as_ref())?;
    let mut tallies: Vec<Tally> = metadata
        .schema
        .iter()
        .map(|column| Tally { name: column.name.clone(), ..Tally::default() })
        .collect();
    let mut bytes = Vec::new();
    for group in &metadata.row_groups {
        for chunk in &group.columns {
            let start = chunk.dictionary_page_offset.unwrap_or(chunk.data_page_offset);
            bytes.resize(usize::try_from(chunk.compressed_size)?, 0);
            file.read_exact_at(start, &mut bytes)?;
            let schema = &metadata.schema[chunk.column];
            let fixed = width(chunk.physical, schema.width);
            let tally = &mut tallies[chunk.column];
            let mut mean = 0.0;
            let mut at = 0;
            while at < bytes.len() {
                let (header, used) = Header::read(&bytes[at..])?;
                let size = u64::try_from(header.uncompressed_size)?;
                let (values, encoding) = match &header.body {
                    Body::Dictionary(page) => {
                        tally.dictionary_bytes += size;
                        let values = u64::try_from(page.values)?.max(1);
                        // A plain byte array is a four byte length in front of every value.
                        mean = match fixed {
                            Some(width) => width as f64,
                            None => size.saturating_sub(4 * values) as f64 / values as f64,
                        };
                        at += used + usize::try_from(header.compressed_size)?;
                        continue;
                    }
                    Body::DataV1(page) => (u64::try_from(page.values)?, page.encoding),
                    Body::DataV2(page) => (u64::try_from(page.values)?, page.encoding),
                    Body::Index => (0, Encoding::Plain),
                };
                if matches!(encoding, Encoding::PlainDictionary | Encoding::RleDictionary) {
                    tally.coded_values += values;
                    tally.coded_bytes += size;
                    tally.coded_raw += values as f64 * mean;
                } else {
                    tally.other_values += values;
                    tally.other_bytes += size;
                    tally.other_raw += match fixed {
                        Some(width) => (values * width) as f64,
                        None => size.saturating_sub(4 * values) as f64,
                    };
                }
                at += used + usize::try_from(header.compressed_size)?;
            }
        }
    }
    let mib = |bytes: f64| bytes / f64::from(1 << 20);
    println!(
        "{:<24} {:>7} {:>12} {:>12} {:>10} {:>10}",
        "column", "coded%", "coded MiB~", "other MiB", "dict MiB", "pages MiB"
    );
    tallies.sort_by(|a, b| {
        (b.coded_raw + b.other_raw)
            .partial_cmp(&(a.coded_raw + a.other_raw))
            .unwrap_or_else(|| a.name.cmp(&b.name))
    });
    let mut total = Tally::default();
    for tally in &tallies {
        let values = tally.coded_values + tally.other_values;
        println!(
            "{:<24} {:>6.1}% {:>12.1} {:>12.1} {:>10.1} {:>10.1}",
            tally.name,
            100.0 * tally.coded_values as f64 / values.max(1) as f64,
            mib(tally.coded_raw),
            mib(tally.other_raw),
            mib(tally.dictionary_bytes as f64),
            mib((tally.coded_bytes + tally.other_bytes + tally.dictionary_bytes) as f64),
        );
        total.coded_values += tally.coded_values;
        total.other_values += tally.other_values;
        total.coded_raw += tally.coded_raw;
        total.other_raw += tally.other_raw;
        total.dictionary_bytes += tally.dictionary_bytes;
        total.coded_bytes += tally.coded_bytes;
        total.other_bytes += tally.other_bytes;
    }
    println!(
        "{:<24} {:>6.1}% {:>12.1} {:>12.1} {:>10.1} {:>10.1}",
        "total",
        100.0 * total.coded_values as f64 / (total.coded_values + total.other_values).max(1) as f64,
        mib(total.coded_raw),
        mib(total.other_raw),
        mib(total.dictionary_bytes as f64),
        mib((total.coded_bytes + total.other_bytes + total.dictionary_bytes) as f64),
    );
    println!(
        "raw values in dictionary pages: {:.1}% (estimated at each chunk's mean dictionary length)",
        100.0 * total.coded_raw / (total.coded_raw + total.other_raw).max(1.0)
    );
    Ok(())
}
