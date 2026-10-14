//! The two ends, the null count and the total of every column of a stripe, packed into bytes.
//!
//! The directory gives each stripe one of these for every column, and a reader holds all of them
//! from the moment the file is opened. As [`Range`]s that is 112 bytes a column, because a bound is
//! sized for a 128 bit integer and the total is one too, so the 105 columns of ClickBench's `hits`
//! came to 11.7 KB a stripe and 530 KB over the 45 stripes of the ten million row table. Every query
//! paid that before it read a row, `SELECT COUNT(*)` included.
//!
//! Nearly all of those numbers are small. An end of an integer column that fits in a byte or two is
//! a byte or two here, a null count of zero is one byte, and a column with no total says so in a
//! bit. The 105 columns of a `hits` stripe come to about two kilobytes. A query unpacks the ranges
//! of the columns it compares, when it compares them, and the unpacking is a few shifts a number.
//!
//! The bytes are written and read only here, in memory, and never go to the file, so the layout can
//! change whenever it is worth changing. What the file holds is what [`crate::Writer`] writes.

use rudb_common::bounds::Bound;
use rudb_common::{Error, Result};
use rudb_storage::{Probe, Range};

/// Tags for an end, three bits each in the first byte of a range.
const ABSENT: u8 = 0;
const INT: u8 = 1;
const REAL: u8 = 2;
const BYTES: u8 = 3;
const SCALED: u8 = 4;

/// The two bits after the two tags.
const EXACT: u8 = 1 << 6;
const SUMMED: u8 = 1 << 7;

/// Every column's range in one stripe, packed one after another.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ranges {
    bytes: Box<[u8]>,
    /// Where each column's range starts in `bytes`.
    at: Box<[u32]>,
}

impl Ranges {
    /// `ranges` packed, in column order.
    pub(crate) fn pack(ranges: &[Range]) -> Result<Self> {
        let mut bytes = Vec::new();
        let mut at = Vec::with_capacity(ranges.len());
        for range in ranges {
            at.push(
                u32::try_from(bytes.len())
                    .map_err(|_| Error::internal("the ranges of a stripe past four gigabytes"))?,
            );
            let mut head = tag(range.low.as_ref()) | tag(range.high.as_ref()) << 3;
            if range.exact {
                head |= EXACT;
            }
            if range.sum.is_some() {
                head |= SUMMED;
            }
            bytes.push(head);
            unsigned(&mut bytes, range.nulls as u128);
            end(&mut bytes, range.low.as_ref());
            end(&mut bytes, range.high.as_ref());
            if let Some(sum) = range.sum {
                signed(&mut bytes, sum);
            }
        }
        // Copied out rather than shrunk in place. The allocator keeps a block it is asked to shrink
        // by less than half, so a stripe would hold up to twice its bytes, where a copy holds what
        // they take and hands the buffer they grew in to the next stripe.
        Ok(Self { bytes: Box::from(bytes.as_slice()), at: at.into_boxed_slice() })
    }

    /// How many columns this describes.
    pub(crate) fn width(&self) -> usize {
        self.at.len()
    }

    /// The range of one column, or `None` past the end.
    pub(crate) fn column(&self, column: usize) -> Option<Range> {
        let mut at = *self.at.get(column)? as usize;
        let head = *self.bytes.get(at)?;
        at += 1;
        let nulls = usize::try_from(read_unsigned(&self.bytes, &mut at)?).ok()?;
        let low = read_end(&self.bytes, &mut at, head & 7)?;
        let high = read_end(&self.bytes, &mut at, head >> 3 & 7)?;
        let sum = if head & SUMMED == 0 { None } else { Some(read_signed(&self.bytes, &mut at)?) };
        Some(Range { low, high, nulls, exact: head & EXACT != 0, sum })
    }

    /// Every column's range, in column order.
    pub(crate) fn columns(&self) -> impl Iterator<Item = Range> + '_ {
        (0..self.width()).map_while(|column| self.column(column))
    }

    /// Whether some one of `probes` rules out every row of the stripe, as [`rudb_storage::Zone`]
    /// answers it.
    pub(crate) fn skips(&self, probes: &[Probe]) -> bool {
        probes.iter().any(|probe| {
            self.column(probe.column).is_some_and(|range| range.excludes(probe.op, &probe.value))
        })
    }

    /// Whether `probes` taken together keep every row of the stripe, as [`rudb_storage::Zone`]
    /// answers it.
    pub(crate) fn certain(&self, probes: &[Probe]) -> bool {
        probes.iter().all(|probe| {
            self.column(probe.column).is_some_and(|range| range.certain(probe.op, &probe.value))
        })
    }

    /// The bytes this holds on the heap.
    #[cfg(test)]
    pub(crate) fn footprint(&self) -> usize {
        self.bytes.len() + self.at.len() * size_of::<u32>()
    }
}

fn tag(bound: Option<&Bound>) -> u8 {
    match bound {
        None => ABSENT,
        Some(Bound::Int(_)) => INT,
        Some(Bound::Real(_)) => REAL,
        Some(Bound::Bytes(_)) => BYTES,
        Some(Bound::Scaled { .. }) => SCALED,
    }
}

fn end(out: &mut Vec<u8>, bound: Option<&Bound>) {
    match bound {
        None => {}
        Some(Bound::Int(value)) => signed(out, *value),
        Some(Bound::Real(value)) => out.extend_from_slice(&value.to_le_bytes()),
        Some(Bound::Bytes(value)) => {
            unsigned(out, value.len() as u128);
            out.extend_from_slice(value);
        }
        Some(Bound::Scaled { unscaled, scale }) => {
            signed(out, *unscaled);
            out.push(*scale);
        }
    }
}

fn read_end(bytes: &[u8], at: &mut usize, tag: u8) -> Option<Option<Bound>> {
    Some(Some(match tag {
        ABSENT => return Some(None),
        INT => Bound::Int(read_signed(bytes, at)?),
        REAL => {
            let value = f64::from_le_bytes(bytes.get(*at..*at + 8)?.try_into().ok()?);
            *at += 8;
            Bound::Real(value)
        }
        BYTES => {
            let length = usize::try_from(read_unsigned(bytes, at)?).ok()?;
            let value = bytes.get(*at..at.checked_add(length)?)?.to_vec();
            *at += length;
            Bound::Bytes(value)
        }
        SCALED => {
            let unscaled = read_signed(bytes, at)?;
            let scale = *bytes.get(*at)?;
            *at += 1;
            Bound::Scaled { unscaled, scale }
        }
        _ => return None,
    }))
}

/// Seven bits a byte, low first, with the top bit saying another byte follows.
fn unsigned(out: &mut Vec<u8>, mut value: u128) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Zigzag first, so that a small negative number is a short one too.
fn signed(out: &mut Vec<u8>, value: i128) {
    unsigned(out, (value << 1 ^ value >> 127) as u128);
}

fn read_unsigned(bytes: &[u8], at: &mut usize) -> Option<u128> {
    let mut value = 0_u128;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        if shift >= 128 {
            return None;
        }
        value |= u128::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

fn read_signed(bytes: &[u8], at: &mut usize) -> Option<i128> {
    let value = read_unsigned(bytes, at)?;
    Some((value >> 1) as i128 ^ -((value & 1) as i128))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(low: Option<Bound>, high: Option<Bound>, nulls: usize, exact: bool) -> Range {
        Range { low, high, nulls, exact, sum: None }
    }

    #[test]
    fn every_range_comes_back_as_it_went_in() {
        let ranges = vec![
            range(None, None, 0, false),
            range(None, None, 7, true),
            Range { sum: Some(0), ..range(Some(Bound::Int(0)), Some(Bound::Int(0)), 0, true) },
            Range {
                sum: Some(i128::MIN),
                ..range(Some(Bound::Int(i128::MIN)), Some(Bound::Int(i128::MAX)), 1, true)
            },
            Range {
                sum: Some(-12_345_678_901_234),
                ..range(Some(Bound::Int(-1)), Some(Bound::Int(300)), usize::MAX, true)
            },
            range(Some(Bound::Real(-0.5)), Some(Bound::Real(f64::INFINITY)), 3, false),
            range(Some(Bound::Bytes(Vec::new())), Some(Bound::Bytes(vec![0xff; 300])), 0, true),
            range(
                Some(Bound::Scaled { unscaled: -1_234, scale: 2 }),
                Some(Bound::Scaled { unscaled: i128::MAX, scale: 9 }),
                2,
                false,
            ),
            range(Some(Bound::Int(5)), None, 0, false),
        ];
        let packed = Ranges::pack(&ranges).unwrap();
        assert_eq!(packed.width(), ranges.len());
        let back: Vec<Range> = packed.columns().collect();
        assert_eq!(back.len(), ranges.len());
        for (back, range) in back.iter().zip(&ranges) {
            assert_eq!(format!("{back:?}"), format!("{range:?}"));
        }
        assert!(packed.column(ranges.len()).is_none());
    }

    #[test]
    fn a_stripe_of_small_integers_packs_into_a_few_bytes_a_column() {
        let ranges: Vec<Range> = (0..105)
            .map(|column| Range {
                sum: Some(column * 1_000),
                ..range(Some(Bound::Int(0)), Some(Bound::Int(column)), 0, true)
            })
            .collect();
        let packed = Ranges::pack(&ranges).unwrap();
        assert!(packed.footprint() < 105 * 12, "{} bytes", packed.footprint());
    }
}
