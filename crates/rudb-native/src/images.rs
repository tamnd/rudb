//! Hot images in the file, `engine-v4/10-freezing-and-checkpoint.md` section 10.5 step 3.
//!
//! A checkpoint round writes a hot stripe it does not freeze as the (column, part) pairs that
//! changed since the stripe's last image, see `rudb_storage::image`. Each image is a section of
//! its table, [`section::HOT_IMAGE`], with the stripe in the high half of its id and the image's
//! place in the stripe's line of images in the low half, so a new one is attached beside the
//! older ones rather than over them. An image of the whole stripe is flagged [`WHOLE`], and an
//! open puts a stripe together from its newest whole image and every image after it, the way the
//! newest root names the pages of the rounds before.
//!
//! The values use the two codecs a hot image is allowed, plain and bitpack: a fixed-width column
//! is packed as offsets from its smallest value when they fit in 64 bits and written plain
//! otherwise, and a text column is its lengths packed and its bytes plain. A round pays for an
//! image in time it holds a snapshot, and the full chooser is the freezer's to run.

use std::path::Path;

use rudb_common::Result;
use rudb_encoding::bitpack;
use rudb_storage::deletes::{PART_ROWS, PART_WORDS};
use rudb_storage::image::{ColumnImage, HotImage, PartImage, Values};

use crate::{Reader, invalid, section};

/// The flag of an image of the whole stripe.
pub const WHOLE: u32 = 1;

/// The version of the payload's layout.
const VERSION: u8 = 1;

/// A bitmap with no bit set.
const NONE_SET: u8 = 0;

/// A bitmap whose set bits are the first `n`.
const FIRST_SET: u8 = 1;

/// A bitmap written whole.
const RAW: u8 = 2;

/// Fixed-width values written plain, 16 bytes each.
const PLAIN: u8 = 0;

/// Fixed-width values packed as offsets from the smallest.
const PACKED: u8 = 1;

/// Text, its lengths packed and its bytes plain.
const TEXT: u8 = 2;

/// Bits a slot's place in its part takes.
const PLACE_BITS: usize = PART_ROWS.trailing_zeros() as usize;

/// The bytes of `image`.
///
/// # Errors
///
/// If a value does not fit the width worked out for it, which does not happen.
pub fn encode(image: &HotImage) -> Result<Vec<u8>> {
    let mut out = vec![VERSION];
    out.extend_from_slice(&image.stripe.to_le_bytes());
    out.extend_from_slice(&image.at.to_le_bytes());
    out.extend_from_slice(&image.rows.to_le_bytes());
    out.extend_from_slice(&len32(image.parts.len()).to_le_bytes());
    for part in &image.parts {
        out.extend_from_slice(&part.part.to_le_bytes());
        bits(&part.present, &mut out);
        out.extend_from_slice(&len32(part.deleted.len()).to_le_bytes());
        let places: Vec<u64> =
            part.deleted.iter().map(|&slot| u64::from(slot - part.first())).collect();
        bitpack::pack_linear(&places, PLACE_BITS, &mut out)?;
        out.extend_from_slice(&len32(part.columns.len()).to_le_bytes());
        for column in &part.columns {
            out.extend_from_slice(&len32(column.column).to_le_bytes());
            bits(&column.valid, &mut out);
            values(&column.values, &mut out)?;
        }
    }
    Ok(out)
}

/// The image [`encode`] wrote into `bytes`.
///
/// # Errors
///
/// If the bytes are not an image this build wrote, or are cut short.
pub fn decode(bytes: &[u8]) -> Result<HotImage> {
    let mut at = Cursor { bytes, at: 0 };
    if at.u8()? != VERSION {
        return Err(invalid("a hot image of a version this build does not read"));
    }
    let (stripe, cut, rows) = (at.u32()?, at.u64()?, at.u32()?);
    let mut parts = Vec::new();
    for _ in 0..at.count(9)? {
        let part = at.u32()?;
        if part >= rudb_storage::hot::PARTS as u32 {
            return Err(invalid("a hot image names a part past the end of its stripe"));
        }
        let present = at.bits()?;
        let deleted = at.count(0)?;
        if deleted > PART_ROWS as usize {
            return Err(invalid("a hot image deletes more rows than a part holds"));
        }
        let packed = at.take(bitpack::tail_len(deleted, PLACE_BITS))?;
        let first = part * PART_ROWS;
        let deleted = (0..deleted)
            .map(|i| Ok(first + bitpack::tail_at(packed, PLACE_BITS, i)? as u32))
            .collect::<Result<Vec<u32>>>()?;
        let mut columns = Vec::new();
        for _ in 0..at.count(6)? {
            let column = at.u32()? as usize;
            let valid = at.bits()?;
            columns.push(ColumnImage { column, valid, values: at.values()? });
        }
        parts.push(PartImage { part, present, deleted, columns });
    }
    if at.at != bytes.len() {
        return Err(invalid("a hot image has bytes past its end"));
    }
    Ok(HotImage { stripe, at: cut, rows, parts })
}

/// Attaches `image` to table `table` in the file at `path` as image `layer` of its stripe, the
/// next after the stripe's newest, flagged [`WHOLE`] when it is of every part.
///
/// # Errors
///
/// If the file cannot be attached to, see [`crate::attach`].
pub fn attach_image(
    path: impl AsRef<Path>,
    table: &str,
    image: &HotImage,
    layer: u32,
    whole: bool,
) -> Result<()> {
    let bytes = encode(image)?;
    let attachment = section::Attachment {
        kind: *section::HOT_IMAGE,
        id: (u64::from(image.stripe) << 32) | u64::from(layer),
        flags: if whole { WHOLE } else { 0 },
        header_bytes: 0,
        bytes: &bytes,
    };
    crate::attach(path, table, &[attachment])?;
    Ok(())
}

/// Each stripe of the table `reader` reads that has hot images, put together from its newest
/// whole image and every one after it, with the layer of the newest, by stripe.
///
/// # Errors
///
/// If an image cannot be read or decoded.
pub fn images(reader: &Reader) -> Result<Vec<(HotImage, u32)>> {
    let table = reader.table();
    let mut held: Vec<(u32, u32, bool, &section::Section)> = table
        .sections()
        .iter()
        .filter(|held| held.kind == *section::HOT_IMAGE && held.usable(table.generation()))
        .map(|held| ((held.id >> 32) as u32, held.id as u32, held.flags & WHOLE != 0, held))
        .collect();
    held.sort_by_key(|&(stripe, layer, ..)| (stripe, layer));
    let mut stripes = Vec::new();
    for line in held.chunk_by(|a, b| a.0 == b.0) {
        let from = line.iter().rposition(|&(_, _, whole, _)| whole).unwrap_or(0);
        let images = line[from..]
            .iter()
            .map(|&(.., held)| decode(&reader.payload(held)?))
            .collect::<Result<Vec<_>>>()?;
        let layer = line[line.len() - 1].1;
        stripes.push((HotImage::layered(&images), layer));
    }
    Ok(stripes)
}

fn len32(len: usize) -> u32 {
    u32::try_from(len).expect("a hot image's counts fit in 32 bits")
}

/// Writes a bitmap of a part.
fn bits(words: &[u64; PART_WORDS], out: &mut Vec<u8>) {
    let set: u32 = words.iter().map(|word| word.count_ones()).sum();
    let first = (0..set as usize).all(|i| words[i / 64] & (1 << (i % 64)) != 0);
    if set == 0 {
        out.push(NONE_SET);
    } else if first {
        out.push(FIRST_SET);
        out.extend_from_slice(&set.to_le_bytes());
    } else {
        out.push(RAW);
        for word in words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
}

/// Writes a column's values.
fn values(values: &Values, out: &mut Vec<u8>) -> Result<()> {
    match values {
        Values::Fixed(values) => {
            let low = values.iter().copied().min().unwrap_or(0);
            let high = values.iter().copied().max().unwrap_or(0);
            match u64::try_from(high - low) {
                Ok(range) => {
                    out.push(PACKED);
                    out.extend_from_slice(&len32(values.len()).to_le_bytes());
                    out.extend_from_slice(&low.to_le_bytes());
                    let width = (64 - range.leading_zeros()) as usize;
                    out.push(width as u8);
                    let offsets: Vec<u64> = values.iter().map(|&v| (v - low) as u64).collect();
                    bitpack::pack_linear(&offsets, width, out)?;
                }
                Err(_) => {
                    out.push(PLAIN);
                    out.extend_from_slice(&len32(values.len()).to_le_bytes());
                    for value in values {
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
        }
        Values::Text { bytes, ends } => {
            out.push(TEXT);
            out.extend_from_slice(&len32(ends.len()).to_le_bytes());
            let mut start = 0;
            let lengths: Vec<u64> = ends
                .iter()
                .map(|&end| {
                    let length = end - start;
                    start = end;
                    length
                })
                .collect();
            let width = (64 - lengths.iter().max().unwrap_or(&0).leading_zeros()) as usize;
            out.push(width as u8);
            bitpack::pack_linear(&lengths, width, out)?;
            out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
    Ok(())
}

/// A place in an image's bytes.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|&end| end <= self.bytes.len());
        let end = end.ok_or_else(|| invalid("a hot image is cut short"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    fn u128(&mut self) -> Result<u128> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().expect("sixteen bytes")))
    }

    /// A count of things that each take at least `each` bytes, refused when the bytes left
    /// cannot hold that many, so a torn count is not turned into an allocation.
    fn count(&mut self, each: usize) -> Result<usize> {
        let count = self.u32()? as usize;
        if count.saturating_mul(each) > self.bytes.len() - self.at {
            return Err(invalid("a hot image counts more than it holds"));
        }
        Ok(count)
    }

    fn bits(&mut self) -> Result<Box<[u64; PART_WORDS]>> {
        let mut words = Box::new([0_u64; PART_WORDS]);
        match self.u8()? {
            NONE_SET => {}
            FIRST_SET => {
                let set = self.u32()? as usize;
                if set > PART_ROWS as usize {
                    return Err(invalid("a hot image sets more bits than a part has"));
                }
                for i in 0..set {
                    words[i / 64] |= 1 << (i % 64);
                }
            }
            RAW => {
                for word in words.iter_mut() {
                    *word = self.u64()?;
                }
            }
            _ => return Err(invalid("a hot image bitmap of an unknown form")),
        }
        Ok(words)
    }

    fn values(&mut self) -> Result<Values> {
        let form = self.u8()?;
        let count = self.u32()? as usize;
        if count > PART_ROWS as usize {
            return Err(invalid("a hot image holds more values than a part has"));
        }
        match form {
            PLAIN => Ok(Values::Fixed((0..count).map(|_| self.u128()).collect::<Result<_>>()?)),
            PACKED => {
                let low = self.u128()?;
                let width = usize::from(self.u8()?);
                let packed = self.packed(count, width)?;
                let values = (0..count)
                    .map(|i| Ok(low + u128::from(bitpack::tail_at(packed, width, i)?)))
                    .collect::<Result<_>>()?;
                Ok(Values::Fixed(values))
            }
            TEXT => {
                let width = usize::from(self.u8()?);
                let packed = self.packed(count, width)?;
                let mut end = 0_u64;
                let ends = (0..count)
                    .map(|i| {
                        end = end.saturating_add(bitpack::tail_at(packed, width, i)?);
                        Ok(end)
                    })
                    .collect::<Result<Vec<u64>>>()?;
                let total = self.u64()?;
                if Some(&total) != ends.last().or(Some(&0)) {
                    return Err(invalid("a hot image's text lengths do not add up"));
                }
                let bytes = self.take(usize::try_from(total).map_err(|_| invalid("too long"))?)?;
                Ok(Values::Text { bytes: bytes.to_vec(), ends })
            }
            _ => Err(invalid("hot image values of an unknown form")),
        }
    }

    fn packed(&mut self, count: usize, width: usize) -> Result<&'a [u8]> {
        if width > 64 {
            return Err(invalid("a hot image packs values wider than 64 bits"));
        }
        self.take(bitpack::tail_len(count, width))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::{Field, LogicalType, Value};
    use rudb_storage::deletes::PART_WORDS;
    use rudb_storage::image::{ColumnImage, HotImage, PartImage, Values};
    use rudb_vector::{Chunk, Vector};

    use super::{attach_image, decode, encode, images};
    use crate::{Catalog, Writer};

    fn bits(set: impl IntoIterator<Item = usize>) -> Box<[u64; PART_WORDS]> {
        let mut words = Box::new([0_u64; PART_WORDS]);
        for i in set {
            words[i / 64] |= 1 << (i % 64);
        }
        words
    }

    fn image() -> HotImage {
        let text: Vec<Vec<u8>> = (0..300)
            .map(|i| if i % 3 == 0 { Vec::new() } else { format!("v{i}").into_bytes() })
            .collect();
        let mut ends = Vec::new();
        let mut bytes = Vec::new();
        for value in &text {
            bytes.extend_from_slice(value);
            ends.push(bytes.len() as u64);
        }
        HotImage {
            stripe: 12,
            at: 99,
            rows: 8_192 + 300,
            parts: vec![
                PartImage {
                    part: 0,
                    present: bits(0..8_192),
                    deleted: vec![5, 8_000],
                    columns: vec![],
                },
                PartImage {
                    part: 1,
                    present: bits((0..300).filter(|i| i % 7 != 3)),
                    deleted: vec![8_192 + 1],
                    columns: vec![
                        ColumnImage {
                            column: 0,
                            valid: bits((0..300).filter(|i| i % 5 != 0)),
                            values: Values::Fixed((0..300).map(|i| 1_000_000 + i * 3).collect()),
                        },
                        ColumnImage {
                            column: 1,
                            valid: bits(0..300),
                            values: Values::Fixed((0..300).map(|i| u128::MAX - i).collect()),
                        },
                        ColumnImage {
                            column: 2,
                            valid: bits(0..0),
                            values: Values::Fixed(vec![0; 300]),
                        },
                        ColumnImage {
                            column: 3,
                            valid: bits((0..300).filter(|i| i % 3 != 0)),
                            values: Values::Text { bytes, ends },
                        },
                        ColumnImage {
                            column: 4,
                            valid: bits(0..300),
                            values: Values::Fixed((0..300).map(|i| i << 100).collect()),
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn an_image_comes_back_as_it_was_written() {
        let image = image();
        let bytes = encode(&image).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), image);
        // The run of a part with every slot present is five bytes rather than a kilobyte, and the
        // column of a single value is no bits a value. Most of what is left is the three bitmaps
        // written whole, a kilobyte each, and the column too wide to pack, 4,800 bytes plain.
        assert!(bytes.len() < 10_000, "{} bytes", bytes.len());
    }

    #[test]
    fn a_cut_or_torn_image_is_refused() {
        let bytes = encode(&image()).expect("encodes");
        for end in [0, 1, 20, bytes.len() / 2, bytes.len() - 1] {
            assert!(decode(&bytes[..end]).is_err(), "cut at {end}");
        }
        let mut torn = bytes.clone();
        torn[17] = 0xff;
        torn[18] = 0xff;
        assert!(decode(&torn).is_err());
        let mut longer = bytes;
        longer.push(0);
        assert!(decode(&longer).is_err());
    }

    fn file(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        let path = std::env::temp_dir()
            .join(format!("rudb-images-{label}-{}-{stamp}.rdb", std::process::id()));
        let mut writer =
            Writer::create(&path, "t", vec![Field::new("id", LogicalType::BigInt)]).expect("new");
        let ids = (0..100).map(Value::BigInt).collect::<Vec<_>>();
        let chunk = Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &ids).expect("ids")])
            .expect("a column");
        writer.append(&chunk).expect("a part");
        writer.finish().expect("commit");
        path
    }

    /// An image of one part of `stripe` at `at` whose one column holds `value` in each of `rows`.
    fn one(stripe: u32, at: u64, part: u32, rows: usize, value: u128) -> HotImage {
        HotImage {
            stripe,
            at,
            rows: part * 8_192 + rows as u32,
            parts: vec![PartImage {
                part,
                present: bits(0..rows),
                deleted: vec![],
                columns: vec![ColumnImage {
                    column: 0,
                    valid: bits(0..rows),
                    values: Values::Fixed(vec![value; rows]),
                }],
            }],
        }
    }

    #[test]
    fn a_stripe_opens_from_its_newest_whole_image_and_the_ones_after() {
        let path = file("layers");
        let reader = || Catalog::open(&path).expect("open").table("t").expect("the table");
        assert!(images(&reader()).expect("none").is_empty());
        attach_image(&path, "t", &one(0, 10, 0, 50, 1), 0, true).expect("whole");
        attach_image(&path, "t", &one(0, 11, 1, 20, 2), 1, false).expect("a part");
        attach_image(&path, "t", &one(3, 11, 0, 5, 9), 0, true).expect("another stripe");
        let held = images(&reader()).expect("read");
        assert_eq!(held.len(), 2);
        let (zero, layer) = &held[0];
        assert_eq!((zero.stripe, zero.at, zero.rows, *layer), (0, 11, 8_192 + 20, 1));
        assert_eq!(zero.parts.len(), 2, "the whole image's part and the later one's");
        assert_eq!((held[1].0.stripe, held[1].1), (3, 0));

        // A later whole image is all an open needs, though the ones before it are still there
        // until a round recycles them.
        attach_image(&path, "t", &one(0, 12, 0, 60, 3), 2, true).expect("whole again");
        let held = images(&reader()).expect("read");
        assert_eq!(held[0].0, one(0, 12, 0, 60, 3));
        assert_eq!(held[0].1, 2);
        let _ = std::fs::remove_file(&path);
    }
}
