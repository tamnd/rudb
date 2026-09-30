//! `format_bytes` and its spellings, and `parse_formatted_bytes`, which go between a count of bytes
//! and the way a person writes one.
//!
//! Both are ports of `StringUtil` on `v2.0.0-dev84237`, since the digits are the pin's and not a
//! rounding of the value. `format_bytes(1500)` is `1.4 KiB` because the digit after the point is the
//! remainder in tenths rounded down, and `format_bytes(-9223372036854775808)` is `-8192.0 PiB`,
//! which is the largest magnitude a BIGINT has in the largest unit there is.
//!
//! `parse_formatted_bytes` reads the number as a DOUBLE and truncates what the unit makes of it, so
//! `parse_formatted_bytes('1.9 b')` is 1. Anything after the unit is ignored, so `'1 KB extra'` is
//! 1000. A value that is exactly 2^64 after rounding passes the pin's range check and comes back as
//! 2^63, which is what converting that double to an unsigned integer gives on the machine the pin
//! was measured on, and is kept here so the two agree.

use rudb_common::{Error, Result};

/// `format_bytes(bytes)` in units of 1024 when `decimal` is false and of 1000 when it is true.
pub(crate) fn format_bytes(bytes: i64, decimal: bool) -> String {
    let sign = if bytes < 0 { "-" } else { "" };
    let (multiplier, units) = if decimal {
        (1000, ["bytes", "kB", "MB", "GB", "TB", "PB"])
    } else {
        (1024, ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"])
    };
    let magnitude = bytes.unsigned_abs();
    let mut parts = [0u64; 6];
    parts[0] = magnitude;
    for i in 1..6 {
        parts[i] = parts[i - 1] / multiplier;
        parts[i - 1] %= multiplier;
    }
    for i in (1..6).rev() {
        if parts[i] != 0 {
            let tenths = parts[i - 1] * 10 / multiplier;
            return format!("{sign}{}.{tenths} {}", parts[i], units[i]);
        }
    }
    let unit = if magnitude == 1 { "byte" } else { "bytes" };
    format!("{sign}{magnitude} {unit}")
}

/// `parse_formatted_bytes(text)`, the number of bytes a string like `1.5 GiB` stands for.
pub(crate) fn parse_formatted_bytes(text: &str) -> Result<u64> {
    let bytes = text.as_bytes();
    let space = |at: usize| bytes.get(at).is_some_and(|byte| is_space(*byte));
    let mut at = 0;
    while space(at) {
        at += 1;
    }
    let start = at;
    while bytes.get(at).is_some_and(|byte| matches!(byte, b'0'..=b'9' | b'.' | b'e' | b'E' | b'-'))
    {
        at += 1;
    }
    if at == start {
        return Err(Error::invalid_input("Memory must have a number (e.g. 1GB)"));
    }
    let number = &text[start..at];
    let limit: f64 = number
        .parse()
        .map_err(|_| Error::invalid_input(format!("Invalid memory limit: '{number}'")))?;
    while space(at) {
        at += 1;
    }
    let start = at;
    while at < bytes.len() && !space(at) {
        at += 1;
    }
    if limit < 0.0 {
        return Err(Error::invalid_input("Memory cannot be negative"));
    }
    let unit = text[start..at].to_ascii_lowercase();
    let multiplier: u64 = match unit.as_str() {
        "byte" | "bytes" | "b" => 1,
        "kilobyte" | "kilobytes" | "kb" | "k" => 1000,
        "megabyte" | "megabytes" | "mb" | "m" => 1000_u64.pow(2),
        "gigabyte" | "gigabytes" | "gb" | "g" => 1000_u64.pow(3),
        "terabyte" | "terabytes" | "tb" | "t" => 1000_u64.pow(4),
        "kib" => 1024,
        "mib" => 1024_u64.pow(2),
        "gib" => 1024_u64.pow(3),
        "tib" => 1024_u64.pow(4),
        _ => {
            return Err(Error::invalid_input(format!(
                "Unknown unit for memory: '{unit}' (expected: KB, MB, GB, TB for 1000^i units or KiB, MiB, GiB, TiB for 1024^i units)"
            )));
        }
    };
    let multiplier = multiplier as f64;
    if limit > u64::MAX as f64 / multiplier {
        return Err(Error::invalid_input("Memory value out of range: value is too large"));
    }
    let product = multiplier * limit;
    // 2^64 itself is the one value the check lets through that an unsigned integer cannot hold.
    if product >= u64::MAX as f64 {
        return Ok(1 << 63);
    }
    Ok(product as u64)
}

/// The pin's `CharacterIsSpace`, which is the six ASCII white space characters.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_of_bytes_is_written_the_way_the_pin_writes_it() {
        let cases = [
            (0, false, "0 bytes"),
            (1, false, "1 byte"),
            (-1, false, "-1 byte"),
            (1023, false, "1023 bytes"),
            (-1023, false, "-1023 bytes"),
            (1500, false, "1.4 KiB"),
            (1500, true, "1.5 kB"),
            (-999_999, true, "-999.9 kB"),
            (i64::MIN, false, "-8192.0 PiB"),
        ];
        for (bytes, decimal, expected) in cases {
            assert_eq!(format_bytes(bytes, decimal), expected, "{bytes} {decimal}");
        }
    }

    #[test]
    fn a_written_count_is_read_the_way_the_pin_reads_it() {
        let cases = [
            ("1 KB extra", 1000),
            (".5k", 500),
            ("1e3 b", 1000),
            ("-0 b", 0),
            ("1.9 b", 1),
            ("1.5kb", 1500),
            ("1 Kilobytes", 1000),
            ("1 T", 1_000_000_000_000),
            (" \t1\tKiB", 1024),
            ("4.3e3kb", 4_300_000),
            ("17000000000 GB", 17_000_000_000_000_000_000),
            ("18446744073709551615 b", 9_223_372_036_854_775_808),
            ("1.0000000000000002e19 b", 10_000_000_000_000_002_048),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_formatted_bytes(text).unwrap(), expected, "{text}");
        }
        let refusals = [
            ("abc", "Memory must have a number (e.g. 1GB)"),
            ("1-2 b", "Invalid memory limit: '1-2'"),
            ("1e b", "Invalid memory limit: '1e'"),
            ("-1 x", "Memory cannot be negative"),
            ("5.", "Unknown unit for memory: ''"),
            ("1 pb", "Unknown unit for memory: 'pb'"),
            ("1e30 tb", "Memory value out of range: value is too large"),
        ];
        for (text, expected) in refusals {
            let said = parse_formatted_bytes(text).unwrap_err().to_string();
            assert!(said.contains(expected), "{text}: {said}");
        }
    }
}
