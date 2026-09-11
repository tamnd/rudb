//! Walking the pages of a column chunk.
//!
//! The footer says where a column chunk starts and how many bytes it is, and that is all it says.
//! There is no page index inside the chunk and no table of contents, so the pages are found by
//! walking: decode a header, take the body the header sized, and start again at the byte after
//! it. A page that is a byte out puts every page after it a byte out too, which is why the walker
//! stops on the first thing that does not add up rather than resynchronising.
//!
//! # Where the bytes come from
//!
//! [`Pages`] takes the chunk as one slice rather than reading as it goes. That is the shape the
//! submission interface from `spec/engine/05-scan.md` section 5.3 wants: a scan states every
//! column chunk it intends to read in one `submit` call, the pool serves them concurrently, and
//! what comes back is a buffer per chunk. A walker that read a page at a time would be back to
//! one blocking read per page, which is the thing the interface exists to stop.
//!
//! # Decompression happens here
//!
//! A page's body is compressed with the chunk's codec and the header states both sizes, so this
//! is the one place that has both and the natural place to decompress. The uncompressed size is
//! passed to the codec as a requirement rather than a hint, which is what turns a page whose
//! metadata and body disagree into an error instead of a short column.
//!
//! A version two data page is the exception worth knowing about: its levels sit outside the
//! compressed region, so only the bytes after them are decompressed and the levels are copied
//! across as they are.

use rudb_common::{Error, Result};
use rudb_compress::Codec;

use crate::hybrid::{Hybrid, width_for};
use crate::metadata::Encoding;
use crate::page::{Body, Header};

/// One page, with its body decompressed.
#[derive(Debug)]
pub struct Page {
    /// What the header said.
    pub header: Header,
    /// The body, decompressed, including the levels for both page versions.
    pub body: Vec<u8>,
}

impl Page {
    /// Where the values start within [`Page::body`], and where the levels before them are.
    ///
    /// For a version two page the header states the two level lengths. For a version one page it
    /// does not, because the levels are run length encoded inside the same buffer and the only
    /// way to know how long they are is to decode them, which is the level reader's job and not
    /// this one's. So this returns nothing for version one and the caller reads from the front.
    pub fn level_lengths(&self) -> Option<(usize, usize)> {
        match &self.header.body {
            Body::DataV2(page) => {
                Some((page.repetition_bytes as usize, page.definition_bytes as usize))
            }
            _ => None,
        }
    }

    /// The definition levels of a data page, and where its values start in [`Page::body`].
    ///
    /// A definition level of one means the value is there and a zero means it is null, for a flat
    /// schema, which is the only shape this crate reads. A required column has no levels at all
    /// and gets an empty vector back, which is not the same as a column of zeroes and is the
    /// reason this returns the offset as well: the caller needs to know where the values start
    /// whether or not there were any levels in front of them.
    ///
    /// The two page versions put the levels in the same place and describe them differently. A
    /// version one page writes each level stream with a four byte little endian length in front of
    /// it, because nothing else in the header says how long they are. A version two page states
    /// both lengths in its header and writes no prefix, so the four bytes a version one reader
    /// would eat are the first four bytes of real level data.
    ///
    /// # Errors
    ///
    /// If the page is a dictionary or index page, which have no levels, if a level stream runs off
    /// the end of the body, or if a writer used the encoding the format deprecated.
    pub fn definitions(&self, optional: bool) -> Result<(Vec<u32>, usize)> {
        let (encoding, values, start, len) = match &self.header.body {
            Body::DataV1(page) => {
                // Flat only, so there are no repetition levels and the definition levels are at
                // the front. A nested column never gets this far: the schema reader refuses it.
                let (start, len) = if optional { self.prefixed(0)? } else { (0, 0) };
                (page.definition_encoding, page.values, start, len)
            }
            Body::DataV2(page) => {
                let rep = page.repetition_bytes as usize;
                let def = page.definition_bytes as usize;
                (Encoding::Rle, page.values, rep, def)
            }
            Body::Dictionary(_) | Body::Index => {
                return Err(Error::io(
                    "definition levels were asked of a page that holds no rows".to_string(),
                ));
            }
        };
        let values = usize::try_from(values)
            .map_err(|_| Error::io("a page with a negative value count".to_string()))?;
        let after = start
            .checked_add(len)
            .ok_or_else(|| Error::io("a page with impossible level lengths".to_string()))?;
        if !optional {
            return Ok((Vec::new(), after));
        }
        if encoding != Encoding::Rle {
            // `BIT_PACKED` is the one the format deprecated, and no writer has emitted it in years.
            // Guessing at it would be guessing at a bit order nothing in the corpus can check.
            return Err(Error::io(format!(
                "definition levels in {encoding:?}, which this reader does not read"
            )));
        }
        let bytes = self.body.get(start..after).ok_or_else(|| {
            Error::io(format!(
                "a page claiming {len} bytes of definition levels at {start} in a body of {}",
                self.body.len()
            ))
        })?;
        let mut levels = Vec::with_capacity(values);
        // One bit, because a flat optional column's largest definition level is one. A required
        // column took the early return above and never reaches here.
        Hybrid::new(bytes, width_for(1))?.read(&mut levels, values)?;
        Ok((levels, after))
    }

    /// Reads the four byte length a version one page writes in front of a level stream.
    ///
    /// Returns where the stream starts and how long it is, so the caller can find what follows it.
    fn prefixed(&self, at: usize) -> Result<(usize, usize)> {
        let head = self.body.get(at..at + 4).ok_or_else(|| {
            Error::io(format!(
                "a v1 page with no room for a level length in its {} bytes",
                self.body.len()
            ))
        })?;
        let len = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        Ok((at + 4, len))
    }
}

/// The pages of one column chunk, in order.
#[derive(Debug)]
pub struct Pages<'a> {
    bytes: &'a [u8],
    at: usize,
    codec: Codec,
    /// How many values the footer said the chunk holds, counted down as pages are read.
    ///
    /// Held so that the walker can stop at the end of the chunk rather than at the end of the
    /// buffer. A caller that read the whole row group in one request hands over a slice that runs
    /// on into the next column, and a walker that kept going would decode that column's first
    /// page as if it were this column's last.
    left: i64,
    /// Whether a page has already failed, which stops the walk for good.
    ///
    /// Nothing in a chunk can be resynchronised after a bad page, and without this a caller who
    /// keeps asking gets the same error forever, which in a `for` loop is not an error at all but
    /// a hang.
    failed: bool,
}

impl<'a> Pages<'a> {
    /// A walker over `bytes`, which is the chunk starting at its first page.
    pub fn new(bytes: &'a [u8], codec: Codec, values: i64) -> Self {
        Self { bytes, at: 0, codec, left: values, failed: false }
    }

    /// How many bytes of the chunk have been walked, headers included.
    pub fn position(&self) -> usize {
        self.at
    }

    /// The next page, or nothing when the chunk's values have all been accounted for.
    fn step(&mut self) -> Result<Option<Page>> {
        if self.left <= 0 {
            return Ok(None);
        }
        if self.at >= self.bytes.len() {
            // Running out of buffer with values still owed is the failure that has to be loud.
            // Returning nothing here reads as a chunk that ended, and a chunk that ended early is
            // a column with fewer rows than the one beside it, which is a wrong answer and not a
            // failed read. A short read from the I/O layer arrives here looking exactly like this.
            return Err(Error::io(format!(
                "this column chunk ran out with {} of its values unaccounted for",
                self.left
            )));
        }
        let (header, header_len) = Header::read(&self.bytes[self.at..])?;
        let start = self.at + header_len;
        let end = start.checked_add(header.compressed_size as usize).ok_or_else(|| {
            Error::io("a page whose body runs past the end of memory".to_string())
        })?;
        let raw = self.bytes.get(start..end).ok_or_else(|| {
            Error::io(format!(
                "a page of {} bytes at {start} with only {} bytes of chunk left",
                header.compressed_size,
                self.bytes.len().saturating_sub(start)
            ))
        })?;
        let body = self.decompress(&header, raw)?;
        self.at = end;
        // A dictionary page's values are the distinct ones and are not rows, so it does not count
        // against the chunk. A reader that decremented here would stop one page early on every
        // dictionary encoded column, which is most of `hits`.
        if !matches!(header.body, Body::Dictionary(_)) {
            self.left -= i64::from(header.values());
        }
        Ok(Some(Page { header, body }))
    }

    /// Decompresses a page body, taking the version two level split into account.
    fn decompress(&self, header: &Header, raw: &[u8]) -> Result<Vec<u8>> {
        let expected = header.uncompressed_size as usize;
        let (levels, compressed) = match &header.body {
            Body::DataV2(page) if page.compressed => {
                let levels = (page.repetition_bytes as usize)
                    .checked_add(page.definition_bytes as usize)
                    .ok_or_else(|| Error::io("a v2 page with impossible levels".to_string()))?;
                if levels > raw.len() {
                    return Err(Error::io(format!(
                        "a v2 page says {levels} bytes of levels and its body is {}",
                        raw.len()
                    )));
                }
                (levels, true)
            }
            // A v2 page that says it is not compressed is copied whole, and so is a v1 page whose
            // chunk codec is `UNCOMPRESSED`, which `Codec::decompress` handles as itself.
            Body::DataV2(_) => (0, false),
            _ => (0, true),
        };
        if !compressed || self.codec == Codec::Uncompressed {
            if raw.len() != expected {
                return Err(Error::io(format!(
                    "an uncompressed page of {} bytes whose header says {expected}",
                    raw.len()
                )));
            }
            return Ok(raw.to_vec());
        }
        if levels == 0 {
            return self.codec.decompress(raw, expected);
        }
        let mut body = Vec::with_capacity(expected);
        body.extend_from_slice(&raw[..levels]);
        body.extend(self.codec.decompress(&raw[levels..], expected - levels)?);
        Ok(body)
    }
}

impl Iterator for Pages<'_> {
    /// A page, or the reason the walk stopped.
    ///
    /// The reasons are a malformed header, a body that runs off the end of the buffer, a codec
    /// this build cannot read, and a page that decompressed to a different size than its header
    /// claimed. Every one of those is a file that cannot be read correctly, and the alternative to
    /// stopping is a column of values that came from somewhere else.
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let step = self.step().transpose();
        if matches!(step, Some(Err(_))) {
            self.failed = true;
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rudb_compress::Codec;
    use rudb_io::{File, Filesystem, OpenMode, RealFilesystem};

    use super::{Page, Pages};
    use crate::metadata::{Encoding, Metadata};
    use crate::page::Body;

    /// The file `Read the Parquet footer` committed, written by DuckDB with Snappy.
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join("mixed.parquet")
    }

    fn open() -> Box<dyn File> {
        RealFilesystem::new().open(&fixture(), OpenMode::Read).expect("the fixture is committed")
    }

    /// Every page of every column chunk of the fixture, walked and decompressed.
    fn walk() -> Vec<(usize, Vec<Page>)> {
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let mut out = Vec::new();
        for group in &metadata.row_groups {
            for chunk in &group.columns {
                let start = chunk.start();
                let mut bytes = vec![0u8; chunk.compressed_size as usize];
                file.read_at(start, &mut bytes).expect("the chunk is in the file");
                let mut pages = Pages::new(&bytes, chunk.compression, chunk.values);
                let mut found = Vec::new();
                for page in &mut pages {
                    let page = page.expect("every page of the fixture walks");
                    found.push(page);
                }
                out.push((chunk.column, found));
            }
        }
        out
    }

    #[test]
    fn every_page_of_a_file_duckdb_wrote_walks_and_decompresses() {
        // The test this module exists for. Hand built headers prove the decoder agrees with my
        // reading of the schema, and this proves it agrees with DuckDB, which is the only one of
        // the two that can be wrong in a way I would not have written into the test as well.
        let walked = walk();
        assert!(!walked.is_empty(), "the fixture has column chunks");
        for (column, pages) in &walked {
            assert!(!pages.is_empty(), "column {column} produced no pages");
            for page in pages {
                assert_eq!(
                    page.body.len(),
                    page.header.uncompressed_size as usize,
                    "column {column} has a page that decompressed to the wrong size"
                );
            }
        }
    }

    #[test]
    fn the_pages_of_a_chunk_account_for_exactly_the_rows_the_footer_claims() {
        // The count is the check that catches a walker which stopped a page early or ran a page
        // into the next column, and neither of those shows up as a decompression failure.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        for group in &metadata.row_groups {
            for chunk in &group.columns {
                let mut bytes = vec![0u8; chunk.compressed_size as usize];
                file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
                let mut pages = Pages::new(&bytes, chunk.compression, chunk.values);
                let mut values = 0i64;
                for page in &mut pages {
                    let page = page.expect("every page walks");
                    if !matches!(page.header.body, Body::Dictionary(_)) {
                        values += i64::from(page.header.values());
                    }
                }
                assert_eq!(values, chunk.values, "column {} of a row group", chunk.column);
                assert_eq!(values, group.rows, "column {} of a row group", chunk.column);
            }
        }
    }

    #[test]
    fn a_dictionary_encoded_column_has_its_dictionary_page_first() {
        // And it is not counted against the chunk's values, which is the off by one page this
        // walker would have if it treated every page the same.
        let walked = walk();
        let mut found = 0;
        for (_, pages) in &walked {
            if let Body::Dictionary(dictionary) = &pages[0].header.body {
                found += 1;
                assert!(dictionary.values > 0, "an empty dictionary page");
                assert!(
                    matches!(dictionary.encoding, Encoding::Plain | Encoding::PlainDictionary),
                    "a dictionary page encoded as {}",
                    dictionary.encoding.name()
                );
                for page in &pages[1..] {
                    assert!(
                        !matches!(page.header.body, Body::Dictionary(_)),
                        "a second dictionary page in one chunk"
                    );
                }
            }
        }
        assert!(found > 0, "the fixture has a dictionary encoded column and none was found");
    }

    #[test]
    fn the_walker_stops_at_the_end_of_the_chunk_and_not_at_the_end_of_the_buffer() {
        // A scan reads a whole row group in one request, so the slice a chunk is walked from runs
        // on into the next column. This is the case where a walker without a value count keeps
        // going and decodes the next column's pages as this one's.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let group = &metadata.row_groups[0];
        let chunk = &group.columns[0];
        let start = chunk.start();
        // Deliberately over long: the chunk plus whatever follows it, to the end of the data.
        let over = (chunk.compressed_size as usize) + 4096;
        let mut bytes = vec![0u8; over];
        file.read_at(start, &mut bytes).expect("the fixture is longer than this");
        let mut pages = Pages::new(&bytes, chunk.compression, chunk.values);
        let mut values = 0i64;
        for page in &mut pages {
            let page = page.expect("every page walks");
            if !matches!(page.header.body, Body::Dictionary(_)) {
                values += i64::from(page.header.values());
            }
        }
        assert_eq!(values, chunk.values);
        assert!(
            pages.position() <= chunk.compressed_size as usize,
            "the walker read {} bytes of a {} byte chunk",
            pages.position(),
            chunk.compressed_size
        );
    }

    #[test]
    fn a_chunk_cut_short_anywhere_is_an_error_rather_than_a_panic_or_a_short_column() {
        // Every prefix, at a stride, because a fault injected read that comes back short is one
        // of the things `sim.rs` will do to this reader and a short column is a wrong answer.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let chunk = &metadata.row_groups[0].columns[0];
        let mut bytes = vec![0u8; chunk.compressed_size as usize];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let mut checked = 0;
        for cut in (0..bytes.len()).step_by(7) {
            let mut pages = Pages::new(&bytes[..cut], chunk.compression, chunk.values);
            let mut values = 0i64;
            let mut failed = false;
            for page in &mut pages {
                match page {
                    Ok(page) => {
                        if !matches!(page.header.body, Body::Dictionary(_)) {
                            values += i64::from(page.header.values());
                        }
                    }
                    Err(_) => failed = true,
                }
            }
            assert!(
                failed || values == chunk.values,
                "a chunk cut at {cut} produced {values} of {} values and no error",
                chunk.values
            );
            checked += 1;
        }
        assert!(checked > 10, "the sweep should have looked at more than {checked} prefixes");
    }

    #[test]
    fn a_page_whose_body_is_not_there_is_an_error_naming_what_was_left() {
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let chunk = &metadata.row_groups[0].columns[0];
        let mut bytes = vec![0u8; 40.min(chunk.compressed_size as usize)];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let error = Pages::new(&bytes, chunk.compression, chunk.values)
            .next()
            .expect("a walk that fails yields its error")
            .unwrap_err();
        assert!(
            error.message().contains("bytes of chunk left")
                || error.message().contains("ran off the end"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_walk_that_failed_stays_failed() {
        // Nothing in a chunk can be picked up again after a bad page, so the second ask has to be
        // the end rather than the same error over again. A caller writing the obvious `for` loop
        // over a truncated chunk would otherwise never leave it.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let chunk = &metadata.row_groups[0].columns[0];
        let mut bytes = vec![0u8; 40.min(chunk.compressed_size as usize)];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let mut pages = Pages::new(&bytes, chunk.compression, chunk.values);
        assert!(pages.next().expect("the first ask fails").is_err());
        assert!(pages.next().is_none(), "the walk carried on after a failure");
    }

    #[test]
    fn a_page_in_a_codec_this_build_cannot_read_says_which_codec() {
        // The chunk's codec is a lie here, which is exactly what a file in gzip looks like to this
        // reader, and the answer has to name the codec rather than fail at decompression.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let chunk = &metadata.row_groups[0].columns[0];
        let mut bytes = vec![0u8; chunk.compressed_size as usize];
        file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
        let error = Pages::new(&bytes, Codec::Gzip, chunk.values)
            .next()
            .expect("a walk that fails yields its error")
            .unwrap_err();
        assert!(error.message().contains("GZIP"), "{}", error.message());
    }

    #[test]
    fn a_version_one_page_does_not_claim_to_know_where_its_levels_end() {
        // Because nothing records it. A caller that got a length here would be getting a guess.
        let walked = walk();
        for (_, pages) in &walked {
            for page in pages {
                match &page.header.body {
                    Body::DataV1(_) => assert!(page.level_lengths().is_none()),
                    Body::DataV2(_) => assert!(page.level_lengths().is_some()),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn the_definition_levels_of_a_column_find_the_nulls_the_footer_counted() {
        // Column `s` of the fixture is the only one with nulls in it, 293 per row group, and that
        // number comes from the footer rather than from this reader. Two independent parts of the
        // file agreeing is the whole point: a level decoder with the bit order backwards still
        // produces 2048 levels, and it does not produce 293 zeroes among them.
        let file = open();
        let metadata = Metadata::read(file.as_ref()).expect("the footer reads");
        let mut groups = 0;
        for group in &metadata.row_groups {
            for chunk in &group.columns {
                let optional = metadata.schema[chunk.column].optional;
                let mut bytes = vec![0u8; chunk.compressed_size as usize];
                file.read_at(chunk.start(), &mut bytes).expect("the chunk is in the file");
                let mut pages = Pages::new(&bytes, chunk.compression, chunk.values);
                let mut levels = 0usize;
                let mut nulls = 0i64;
                for page in &mut pages {
                    let page = page.expect("every page walks");
                    if matches!(page.header.body, Body::Dictionary(_)) {
                        continue;
                    }
                    let (found, values_at) =
                        page.definitions(optional).expect("the levels of a flat column decode");
                    assert_eq!(found.len(), page.header.values() as usize);
                    assert!(values_at <= page.body.len(), "values past the end of the body");
                    assert!(
                        found.iter().all(|&level| level <= 1),
                        "a flat column's levels are 0 or 1"
                    );
                    levels += found.len();
                    nulls += found.iter().filter(|&&level| level == 0).count() as i64;
                }
                assert_eq!(levels as i64, chunk.values, "column {}", chunk.column);
                let counted = chunk.stats.as_ref().and_then(|stats| stats.nulls);
                assert_eq!(counted, Some(nulls), "column {} of a row group", chunk.column);
                groups += 1;
            }
        }
        assert!(groups >= 14, "the fixture has two row groups of seven columns, saw {groups}");
    }

    #[test]
    fn a_dictionary_page_is_not_asked_for_levels_it_does_not_have() {
        // Returning an empty vector here would be the wrong kind of quiet. A caller asking a
        // dictionary page for its rows has the wrong page, and saying so beats handing back a
        // count of zero that reads like a page with no nulls.
        let walked = walk();
        let page = walked
            .iter()
            .flat_map(|(_, pages)| pages)
            .find(|page| matches!(page.header.body, Body::Dictionary(_)))
            .expect("the fixture has dictionary pages");
        let error = page.definitions(true).unwrap_err();
        assert!(error.message().contains("holds no rows"), "{}", error.message());
    }

    #[test]
    fn a_required_column_has_no_levels_and_its_values_start_at_the_front() {
        // The fixture is all optional, so this is the hand built half. What matters is that a
        // required column is not charged four bytes for a length prefix that was never written.
        let walked = walk();
        let page = walked
            .iter()
            .flat_map(|(_, pages)| pages)
            .find(|page| matches!(page.header.body, Body::DataV1(_)))
            .expect("the fixture has v1 data pages");
        let (levels, values_at) = page.definitions(false).expect("a required column decodes");
        assert!(levels.is_empty());
        assert_eq!(values_at, 0);
    }
}
