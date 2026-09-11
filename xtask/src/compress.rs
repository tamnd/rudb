//! The codec table: whether decompression can keep up with the reads feeding it.
//!
//! There is one question worth asking of a decompressor in a scan and it is not how fast it is in
//! the abstract. It is whether it is the bottleneck. A page arrives off the device, gets
//! decompressed, and gets decoded. If the device delivers 1.4 GB/s, which is what `cargo xtask io`
//! measures on `server3`, then a decompressor above that number is free and a decompressor below
//! it sets the pace for the whole scan no matter what the pages do afterwards.
//!
//! So the unit here is megabytes a second of output, and the number to compare it against is the
//! one in the I/O table. Rule ten wants a micro number next to the end to end number it explains,
//! and until there is a Parquet reader the I/O table is the nearest thing to one.
//!
//! # The payloads are shaped like columns and not like text
//!
//! A compression benchmark run on English prose says something about English prose. What goes
//! through this decoder is Parquet pages, so the payloads are what a column looks like: a run of
//! one repeated value, a dictionary of URLs, integers that barely vary, and bytes with nothing to
//! find in them. Those four decompress at quite different rates, because a block that is almost
//! all copies runs a different loop from a block that is almost all literals, and quoting one
//! number over an average of them would hide which.

use std::path::Path;

use rudb_compress::snappy;

use crate::timing::{build_line, shared_caveats, show, time};

/// One payload, shaped like something that turns up in a column.
struct Payload {
    name: &'static str,
    /// What it holds, before compression.
    raw: Vec<u8>,
}

/// A megabyte of each, so that the numbers are rates rather than cache effects.
const SIZE: usize = 1 << 20;

fn payloads() -> Vec<Payload> {
    let mut runs = Vec::with_capacity(SIZE);
    while runs.len() < SIZE {
        let value = (runs.len() / 512) as u8;
        runs.extend(std::iter::repeat_n(value, 512.min(SIZE - runs.len())));
    }

    let one = b"https://example.com/a/page?utm_source=clickbench&utm_campaign=q3 ";
    let urls: Vec<u8> = one.iter().copied().cycle().take(SIZE).collect();

    // Integers that barely vary, which is what a sorted or clustered column looks like before the
    // encoding layer gets to it. Little-endian, because that is how a plain page holds them.
    let mut integers = Vec::with_capacity(SIZE);
    let mut value: u32 = 1_700_000_000;
    while integers.len() < SIZE {
        integers.extend_from_slice(&value.to_le_bytes());
        value += u32::from(integers.len() as u8 % 3);
    }
    integers.truncate(SIZE);

    let mut state = 0x5eed_5eed_5eed_5eedu64;
    let random: Vec<u8> = (0..SIZE)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) as u8
        })
        .collect();

    vec![
        Payload { name: "runs of 512", raw: runs },
        Payload { name: "repeated urls", raw: urls },
        Payload { name: "near constant i32", raw: integers },
        Payload { name: "incompressible", raw: random },
    ]
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

    let mut rows = Vec::new();
    for payload in payloads() {
        // Compressed here rather than committed, because unlike the correctness fixtures in
        // `crates/rudb-compress/tests/data` this only needs a block that is shaped right, and a
        // block built by the code in this file is one nobody has to go and look up.
        let block = compress(&payload.raw);
        let expected = payload.raw.clone();
        let time = time(|| {
            let out = snappy::decompress(&block).expect("a block this file built did not decode");
            std::hint::black_box(&out);
        });
        // Checked once outside the timed loop, because a decoder that is fast and wrong is not a
        // result and this table should not be the place that fails to notice.
        assert_eq!(snappy::decompress(&block).unwrap(), expected, "{}", payload.name);
        rows.push((payload.name, block.len(), payload.raw.len(), time));
    }

    println!();
    println!("what a megabyte of Snappy costs to decompress");
    println!("  {}", build_line());
    println!();
    println!("{:<20} {:>10} {:>8} {:>12} {:>7}", "payload", "time", "ratio", "MiB/s out", "IQR");
    for (name, compressed, raw, time) in &rows {
        let rate = (*raw as f64 / (1 << 20) as f64) / (time.median / 1e9);
        println!(
            "{:<20} {:>10} {:>8.1} {:>12.0} {:>6.1}%",
            name,
            show(time.median),
            *raw as f64 / *compressed as f64,
            rate,
            time.relative() * 100.0
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
    println!("  rule ten: this is a micro number. The end to end number it explains is a");
    println!("    ClickBench query over hits.parquet, and it does not exist until the reader");
    println!("    that sits on top of this does.");
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
        for payload in payloads() {
            let block = compress(&payload.raw);
            let out = snappy::decompress(&block)
                .unwrap_or_else(|e| panic!("{} did not decode: {e}", payload.name));
            assert_eq!(out, payload.raw, "{}", payload.name);
        }
    }

    #[test]
    fn the_compressible_payloads_actually_compress() {
        // Otherwise the table would be four rows of the literal path wearing different names.
        let all = payloads();
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
        let integers = &payloads()[2];
        let ratio = integers.raw.len() as f64 / compress(&integers.raw).len() as f64;
        assert!(ratio < 2.0, "snappy found more in raw integers than expected: {ratio:.1} to one");
    }

    #[test]
    fn a_payload_with_nothing_to_find_does_not_shrink() {
        let random = payloads().pop().expect("there is a fourth payload");
        assert!(compress(&random.raw).len() >= random.raw.len());
    }
}
