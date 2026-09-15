//! The codec table: whether decompression can keep up with the reads feeding it.
//!
//! There is one question worth asking of a decompressor in a scan and it is not how fast it is in
//! the abstract. It is whether it is the bottleneck. A page arrives off the device, gets
//! decompressed, and gets decoded. If the device delivers 1.4 GB/s, which is what `cargo xtask io`
//! measures on `server3`, then a decompressor above that number is free and a decompressor below
//! it sets the pace for the whole scan no matter what the pages do afterwards.
//!
//! So the unit here is megabytes a second of output, and there are two numbers to compare it
//! against. The I/O table says what the device delivers. The scan's stage split in the metrics
//! document says what this decoder does inside a real query, which is the end to end number rule
//! ten wants next to a micro one, and the first row of this table is the row that has to agree
//! with it.
//!
//! # The payloads are shaped like columns and not like text
//!
//! A compression benchmark run on English prose says something about English prose. What goes
//! through this decoder is Parquet pages, so the payloads are what a column looks like: a run of
//! one repeated value, a dictionary of URLs, integers that barely vary, two byte dictionary codes,
//! and bytes with nothing to find in them. Those five decompress at quite different rates, because
//! a block that is almost all copies runs a different loop from a block that is almost all
//! literals, and quoting one number over an average of them would hide which.
//!
//! None of the five has a real page's element mix, which is a thing this table got wrong once and
//! cost a working optimisation for it. So the first row is not generated at all. It is a page out
//! of a ClickBench `hits.parquet`, committed as `crates/rudb-compress/tests/data/hits-page.snappy`
//! and read back compressed, and it is the row to watch when a change to the decoder is meant to
//! show up in a query.

use std::path::{Path, PathBuf};

use rudb_compress::snappy;

use crate::timing::{build_line, shared_caveats, time};

/// One payload, shaped like something that turns up in a column.
struct Payload {
    name: &'static str,
    /// What it holds, before compression.
    raw: Vec<u8>,
    /// The block the timed loop reads, when the payload came as one rather than as plaintext.
    ///
    /// A payload built here is compressed here, at whatever block size the row is timed at. The
    /// real page cannot be: it arrived compressed, by a compressor that is not this one, and
    /// recompressing its plaintext would throw away the only thing it is here for.
    block: Option<Vec<u8>>,
}

/// How much of each payload goes through the decoder, whatever size the blocks are cut to.
///
/// Fixed so that the rates across the table are the same measurement at different block sizes, and
/// large enough that neither the payload nor the output is sitting in a cache from the last call.
const TOTAL: usize = 16 << 20;

/// The block sizes every payload is cut into and timed at.
///
/// A Snappy block in Parquet is one page, and a page is small. Walking the first two row groups of
/// the ten million row ClickBench sample gives 391 pages whose uncompressed sizes run 5 bytes at the
/// tenth percentile, 1300 at the median, 180 kilobytes at the ninetieth and 15.5 megabytes at the
/// top, so both ends of that are worth a row. Four kilobytes is a page near the middle of the file,
/// where the block and everything it reaches back into stay in a cache and the per block work is
/// most of the cost. One megabyte is a page from a wide string column, where the buffer has to come
/// from somewhere and the copies reach back further than a cache holds.
///
/// This is the number that was wrong for a while. The table timed one block of the whole payload,
/// which is a shape no Parquet file has, and it disagreed with what the reader measured on a real
/// file for exactly that reason.
const BLOCKS: [usize; 2] = [4 << 10, 1 << 20];

fn payloads(size: usize) -> Vec<Payload> {
    let mut runs = Vec::with_capacity(size);
    while runs.len() < size {
        let value = (runs.len() / 512) as u8;
        runs.extend(std::iter::repeat_n(value, 512.min(size - runs.len())));
    }

    let one = b"https://example.com/a/page?utm_source=clickbench&utm_campaign=q3 ";
    let urls: Vec<u8> = one.iter().copied().cycle().take(size).collect();

    // Integers that barely vary, which is what a sorted or clustered column looks like before the
    // encoding layer gets to it. Little-endian, because that is how a plain page holds them.
    let mut integers = Vec::with_capacity(size);
    let mut value: u32 = 1_700_000_000;
    while integers.len() < size {
        integers.extend_from_slice(&value.to_le_bytes());
        value += u32::from(integers.len() as u8 % 3);
    }
    integers.truncate(size);

    // Dictionary codes, which is what most of a ClickBench page actually holds: two byte indexes
    // into the chunk's dictionary, drawn from a few hundred distinct values with no run structure
    // and no locality. Snappy finds four byte matches in that all day, a long way back and only a
    // few bytes long, and that element mix is the one the four payloads above never produce. The
    // real file decompresses at about the rate this row does, and the rows above it are two to ten
    // times faster, which is why a decoder tuned on them is tuned on the wrong thing.
    let mut state = 0xd1c7_0000_d1c7_0000u64;
    let mut codes = Vec::with_capacity(size);
    while codes.len() < size {
        state = mix(state);
        let code = (state >> 40) as u16 % 331;
        codes.extend_from_slice(&code.to_le_bytes());
    }
    codes.truncate(size);

    let mut state = 0x5eed_5eed_5eed_5eedu64;
    let random: Vec<u8> = (0..size)
        .map(|_| {
            state = mix(state);
            state as u8
        })
        .collect();

    vec![
        Payload { name: "runs of 512", raw: runs, block: None },
        Payload { name: "repeated urls", raw: urls, block: None },
        Payload { name: "near constant i32", raw: integers, block: None },
        Payload { name: "dictionary codes", raw: codes, block: None },
        Payload { name: "incompressible", raw: random, block: None },
    ]
}

/// The page out of `hits.parquet`, as a payload.
///
/// Everything above is a guess at what a column looks like, and the guesses were wrong. Walking the
/// first two row groups of the ten million row ClickBench sample says its blocks are 72.6 percent
/// copies, that 72.3 percent of those copies are four to seven bytes long, and that 96 percent of
/// them reach back sixteen bytes or more. None of the five payloads above produce that mix, and a
/// change measured only on them is measured on the wrong thing, which has already happened once:
/// the fixed width copy path was written, measured against these payloads, found to lose and taken
/// out, and it is worth six percent on the real file.
///
/// So this row is a page a real writer produced, decompressed by the same decoder that reads the
/// file, with its own compressor's choices intact. It is the row to watch.
///
/// # It is a small page, and the pages that set the pace of a scan are not
///
/// The committed one is fifteen kilobytes, which is a page from the middle of the size
/// distribution. The pages that a ClickBench query actually waits on are the string columns, and
/// there one column chunk of one row group is a single page: `URL` in `hits-1m-snappy.parquet` is
/// 4.6 megabytes compressed and 10.5 uncompressed, one `DATA_PAGE` per row group, `PLAIN`. Those
/// two sizes do not decompress at the same rate and it is not close, because at fifteen kilobytes
/// every byte a copy reaches back to is in L1 and at ten megabytes almost none of them are. The
/// committed row reads 2859 MiB/s on the bench host and the decompress stage of the query that
/// reads those pages reads 786 MB/s, so the row rule ten says has to track the query is out by 3.6
/// times.
///
/// `--page PATH` is how to point this row at one of them. A real page cannot be committed at that
/// size, so the fixture stays small and the flag is what makes the honest measurement available to
/// anyone who has a Parquet file.
fn page(root: &Path, given: Option<&str>) -> Result<Payload, String> {
    let (name, path) = match given {
        Some(path) => ("the page given", PathBuf::from(path)),
        None => {
            ("a hits.parquet page", root.join("crates/rudb-compress/tests/data/hits-page.snappy"))
        }
    };
    let block = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let limit = snappy::decompressed_len(&block).map_err(|e| e.message().to_string())?;
    let raw = snappy::decompress(&block, limit).map_err(|e| e.message().to_string())?;
    Ok(Payload { name, raw, block: Some(block) })
}

/// One step of the mixer the payloads that want unpredictable bytes are built from.
///
/// Written out rather than taken from anywhere because this crate has no dependencies and a
/// benchmark that cannot say where its bytes came from is a benchmark nobody can reproduce.
fn mix(state: u64) -> u64 {
    let state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let z = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// A block size as the column that names it prints it.
fn block_size(size: usize) -> String {
    if size >= 1 << 20 { format!("{}M", size >> 20) } else { format!("{}K", size >> 10) }
}

/// Runs the table.
///
/// # Errors
///
/// If a payload cannot be compressed for the table to decompress, which would mean the fixture
/// generator and the decoder disagree and is a bug rather than a condition.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return crate::timing::rebuild(root, "compress", args);
    }

    let given = args.iter().position(|arg| arg == "--page").and_then(|at| args.get(at + 1));
    // The real page once, at the size it is, and every built payload at each block size.
    let mut work: Vec<(usize, Payload)> = vec![(0, page(root, given.map(String::as_str))?)];
    for block_bytes in BLOCKS {
        work.extend(payloads(TOTAL).into_iter().map(|payload| (block_bytes, payload)));
    }

    let mut rows = Vec::new();
    for (block_bytes, payload) in work {
        // Compressed here rather than committed, because unlike the correctness fixtures in
        // `crates/rudb-compress/tests/data` this only needs blocks that are shaped right, and
        // blocks built by the code in this file are ones nobody has to go and look up.
        // The real page is timed at the size it is, once per row, because a block cannot be
        // re-cut without recompressing it. Every built payload is cut to the row's block size.
        let (blocks, sizes) = match &payload.block {
            Some(block) => {
                let copies = TOTAL / payload.raw.len().max(1);
                (vec![block.clone(); copies], vec![payload.raw.len(); copies])
            }
            None => (
                payload.raw.chunks(block_bytes).map(compress).collect(),
                payload.raw.chunks(block_bytes).map(<[u8]>::len).collect(),
            ),
        };
        let compressed: usize = blocks.iter().map(Vec::len).sum();
        let produced: usize = sizes.iter().sum();

        // A block at a time with a fresh buffer each, which is what the reader did before it
        // learned to hand one back.
        let fresh = time(|| {
            for (block, &size) in blocks.iter().zip(&sizes) {
                let out = snappy::decompress(block, size)
                    .expect("a block this file built did not decode");
                std::hint::black_box(&out);
            }
        });
        // The same blocks through one buffer, which is what walking a column chunk looks like.
        let mut buffer = Vec::new();
        let kept = time(|| {
            for (block, &size) in blocks.iter().zip(&sizes) {
                let len = snappy::decompress_into(block, size, &mut buffer)
                    .expect("a block this file built did not decode");
                std::hint::black_box(len);
            }
        });
        // Checked once outside the timed loops, because a decoder that is fast and wrong is
        // not a result and this table should not be the place that fails to notice. Both entry
        // points, since one of them is where a stale buffer would show up.
        let mut buffer = Vec::new();
        for (block, &size) in blocks.iter().zip(&sizes) {
            let out = snappy::decompress(block, size).unwrap();
            assert_eq!(out.len(), size, "{}", payload.name);
            let len = snappy::decompress_into(block, size, &mut buffer).unwrap();
            assert_eq!(&buffer[..len], &out[..], "{} kept", payload.name);
        }
        let size = payload.block.as_ref().map_or(block_bytes, |_| payload.raw.len());
        rows.push((payload.name, size, compressed, produced, fresh, kept));
    }

    println!();
    println!("what Snappy costs to decompress");
    println!("  {}", build_line());
    println!();
    println!(
        "{:<20} {:>6} {:>8} {:>12} {:>12} {:>7}",
        "payload", "block", "ratio", "fresh MiB/s", "kept MiB/s", "IQR"
    );
    for (name, size, compressed, raw, fresh, kept) in &rows {
        let rate = |time: f64| (*raw as f64 / (1 << 20) as f64) / (time / 1e9);
        println!(
            "{:<20} {:>6} {:>8.1} {:>12.0} {:>12.0} {:>6.1}%",
            name,
            block_size(*size),
            *raw as f64 / *compressed as f64,
            rate(fresh.median),
            rate(kept.median),
            fresh.relative() * 100.0
        );
    }
    println!();
    println!("caveats");
    for line in shared_caveats() {
        println!("{line}");
    }
    println!("  the number to compare these against is the cold read rate in `cargo xtask io`,");
    println!("    which is about 1400 MiB/s on server3. A payload above that line is not the");
    println!("    bottleneck in a scan and a payload below it sets the pace for one.");
    println!("  rule ten: this is a micro number. The end to end number it explains is the");
    println!("    decompress stage of a FileScan, which `rudb --metrics` prints per operator.");
    println!("    The hits.parquet row is the one that tracks it. The generated rows are two to");
    println!("    ten times faster than any real page and are here to say which loop moved.");
    Ok(())
}

/// A Snappy compressor good enough to produce a block worth timing.
///
/// Not shipped, and deliberately not in `rudb-compress`. The write path is M2m and a compressor
/// that only this table calls is a compressor only this table tests. What it needs to be is
/// correct, since the decoder is timed on its output, and representative enough that the block
/// has the mix of literals and copies a real one would have. It is not required to be fast or to
/// compress well.
fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 2 + 32);
    let mut value = input.len();
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);

    // A hash table from a four byte sequence to where it last appeared, which is the shape the
    // real one has. Sixteen bits of table is enough to find the matches these payloads contain.
    let mut table = vec![u32::MAX; 1 << 16];
    let mut at = 0usize;
    let mut literal_from = 0usize;

    while at + 4 <= input.len() {
        let word = u32::from_le_bytes([input[at], input[at + 1], input[at + 2], input[at + 3]]);
        let slot = ((word.wrapping_mul(0x1e35_a7bd)) >> 16) as usize & 0xffff;
        let candidate = table[slot] as usize;
        table[slot] = at as u32;

        let matched = candidate != u32::MAX as usize
            && at > candidate
            && at - candidate < (1 << 16)
            && input[candidate..candidate + 4] == input[at..at + 4];
        if !matched {
            at += 1;
            continue;
        }

        // How far the match runs, capped at 64 because that is the longest a copy can express.
        let mut len = 4;
        while len < 64 && at + len < input.len() && input[candidate + len] == input[at + len] {
            len += 1;
        }
        emit_literal(&mut out, &input[literal_from..at]);
        emit_copy(&mut out, len, at - candidate);
        at += len;
        literal_from = at;
    }
    emit_literal(&mut out, &input[literal_from..]);
    out
}

fn emit_literal(out: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let len = bytes.len() - 1;
    if len < 60 {
        out.push((len as u8) << 2);
    } else if len < 1 << 8 {
        out.push(60 << 2);
        out.push(len as u8);
    } else if len < 1 << 16 {
        out.push(61 << 2);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        out.push(62 << 2);
        out.extend_from_slice(&(len as u32).to_le_bytes()[..3]);
    }
    out.extend_from_slice(bytes);
}

fn emit_copy(out: &mut Vec<u8>, len: usize, offset: usize) {
    if len <= 11 && offset < 1 << 11 {
        out.push(0b01 | (((len - 4) as u8) << 2) | (((offset >> 8) as u8) << 5));
        out.push(offset as u8);
    } else {
        out.push(0b10 | (((len - 1) as u8) << 2));
        out.extend_from_slice(&(offset as u16).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{compress, payloads};
    use rudb_compress::snappy;

    #[test]
    fn every_payload_this_table_times_round_trips() {
        // The table asserts this too, but only under the bench profile, which the gate does not
        // build. Without this the compressor here could rot into producing blocks that do not
        // decode and nobody would find out until somebody ran the table.
        for payload in payloads(1 << 20) {
            let block = compress(&payload.raw);
            let out = snappy::decompress(&block, payload.raw.len())
                .unwrap_or_else(|e| panic!("{} did not decode: {e}", payload.name));
            assert_eq!(out, payload.raw, "{}", payload.name);
        }
    }

    #[test]
    fn the_compressible_payloads_actually_compress() {
        // Otherwise the table would be four rows of the literal path wearing different names.
        let all = payloads(1 << 20);
        for payload in all.iter().take(2) {
            let ratio = payload.raw.len() as f64 / compress(&payload.raw).len() as f64;
            assert!(ratio > 2.0, "{} only reached {ratio:.1} to one", payload.name);
        }
    }

    #[test]
    fn near_constant_integers_barely_compress_which_is_why_the_encoding_layer_exists() {
        // A column of timestamps a second apart is about as compressible as data gets, and Snappy
        // gets roughly 1.5 to one on it, because it looks for repeated byte sequences and the low
        // byte of every integer is different. Delta encoding gets an order of magnitude more on
        // the same column by knowing it is looking at integers. That is the whole argument for
        // `rudb-encoding` sitting above this crate, and it is worth an assertion rather than a
        // paragraph, because somebody will eventually wonder why this row of the table is slow.
        let integers = &payloads(1 << 20)[2];
        let ratio = integers.raw.len() as f64 / compress(&integers.raw).len() as f64;
        assert!(ratio < 2.0, "snappy found more in raw integers than expected: {ratio:.1} to one");
    }

    #[test]
    fn a_payload_with_nothing_to_find_does_not_shrink() {
        let random = payloads(1 << 20).pop().expect("there is a fourth payload");
        assert!(compress(&random.raw).len() >= random.raw.len());
    }
}
