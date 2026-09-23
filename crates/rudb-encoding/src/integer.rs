//! The single column integer encodings and the cascade over them.
//!
//! `spec/06-compression.md` section 6.2 lists the encoding set and section 6.3 says the ratios are
//! in the cascade rather than in any one encoding. This module is both: the seven candidate shapes
//! for an integer column, each of which encodes its own output by calling back into the chooser, so
//! that RLE over a dictionary over a bit packed code array is a thing that happens by construction
//! rather than a case somebody wrote out.
//!
//! Everything here works on `i64`. A narrower column is widened on the way in and nothing is lost
//! by it, because every encoding's size comes from the range of the values rather than from the
//! declared width of the type: a `SMALLINT` column of values 100 to 130 packs to 5 bits whether it
//! arrived as `i16` or as `i64`. The one place the widening would cost something is a raw copy, and
//! there is no raw copy, because a bit packed unit at width 64 is exactly that and the chooser
//! reaches it on its own when nothing else fits.
//!
//! ## The unit
//!
//! Bit packing is per 1024 values, per [`crate::bitpack`]. Everything else is per chunk, where a
//! chunk is however many values the caller passes in and is meant to be a row group. The two
//! granularities are the point rather than an accident. A frame of reference base that is chosen
//! per 1024 values tracks a column that drifts, which is what a timestamp column and an
//! autoincrementing key both do, and one base per row group would pay the whole range of the row
//! group on every value. A dictionary, on the other hand, is worth more the larger the unit it
//! covers, which is the argument section 6.5 takes all the way to a dictionary per table.
//!
//! ## The serialized form
//!
//! A chunk is a tag byte, a value count, and a body whose shape depends on the tag. Bodies that
//! contain another array of integers contain a whole chunk, tag and all, which is what makes the
//! decoder a fold and what makes the cascade free: nothing in `Rle` knows what its run lengths are
//! encoded as. The header is fixed width little endian rather than a varint, because 5 bytes per
//! chunk against a chunk that holds a row group is not worth the branch on the decode path.
//!
//! ## What the chooser does, and what it will have to do instead
//!
//! It encodes every candidate and keeps the smallest. That is the honest baseline for M1, which is
//! a measurement of what the format can do rather than of how fast a writer can decide, and it is
//! not what a write path can afford. Section 6.3 describes the real thing: evaluate the candidates
//! on a systematic sample, not the first N rows, because column data is frequently clustered and
//! the first 1024 rows of a sorted column look constant. Building that first would mean the numbers
//! this milestone produces are the sampler's numbers rather than the format's, and there would be
//! no way to tell how much the sampler is leaving behind.

use rudb_common::{Error, Result};

use crate::chooser::{Chooser, EXHAUSTIVE};
use crate::reader::Reader;

use crate::bitpack::{self, VALUES};

/// How deep a cascade is allowed to go.
///
/// Three levels is what section 6.3 says captures most of what a general compressor would find:
/// dictionary, then bit packed codes, then nothing left worth doing. The limit exists because the
/// chooser is exhaustive and a cascade that could nest forever would be exponential, and because a
/// fourth level has never once been the smallest candidate in anything measured so far.
const MAX_DEPTH: u8 = 3;

/// How many values a run writes at once, whatever the run is.
///
/// A run length decode used to write a value at a time for the length of the run, which reads well
/// and is the wrong shape for the data: a clustered join key runs two or three long, so the loop
/// spent its time mispredicting its own exit and the branch cost more than the stores did. Writing
/// a fixed eight and then moving on by the run's real length has no exit to predict, and whatever
/// of the eight was surplus is overwritten by the run that follows, because every run writes at
/// least its own length. Eight because it is two vector stores on every machine this runs on and
/// longer than nearly every run in a column worth run length encoding at all.
const RUN: usize = 8;

/// What a chunk is encoded as. The discriminant is the tag byte in the serialized form and is part
/// of the format, so the numbers are written down rather than left to the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One value repeated. The whole chunk is the tag, the count and the value.
    Constant = 0,
    /// Frame of reference then bit packed, per 1024 values. Covers plain bit packing at base zero
    /// and a raw copy at width 64.
    Packed = 1,
    /// Differences between neighbours, zigzagged so a decreasing column is as cheap as an
    /// increasing one, then encoded as a chunk in its own right.
    Delta = 2,
    /// Run values and run lengths, each encoded as a chunk in its own right.
    Rle = 3,
    /// A dictionary of the distinct values and an array of codes into it, both encoded as chunks in
    /// their own right.
    Dict = 4,
    /// One dominant value with an exception list of positions and values.
    Sparse = 5,
    /// A base and a common step, with the number of steps to each value encoded as a chunk in its
    /// own right.
    Strided = 6,
}

impl Kind {
    /// Every kind, in tag order.
    pub const ALL: [Self; 7] = [
        Self::Constant,
        Self::Packed,
        Self::Delta,
        Self::Rle,
        Self::Dict,
        Self::Sparse,
        Self::Strided,
    ];

    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Constant),
            1 => Ok(Self::Packed),
            2 => Ok(Self::Delta),
            3 => Ok(Self::Rle),
            4 => Ok(Self::Dict),
            5 => Ok(Self::Sparse),
            6 => Ok(Self::Strided),
            other => Err(Error::internal(format!("unknown encoding tag {other}"))),
        }
    }

    /// The name that goes in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Constant => "CONSTANT",
            Self::Packed => "FOR+BITPACK",
            Self::Delta => "DELTA",
            Self::Rle => "RLE",
            Self::Dict => "DICT",
            Self::Sparse => "SPARSE",
            Self::Strided => "STRIDE",
        }
    }
}

/// Encodes a chunk of integers, choosing the cascade that comes out smallest.
///
/// # Errors
///
/// If the chunk is longer than `u32::MAX`, or if an encoding produces something its own decoder
/// would not accept, which is an internal inconsistency rather than a caller error.
pub fn encode(values: &[i64]) -> Result<Vec<u8>> {
    encode_with(values, &EXHAUSTIVE)
}

/// [`encode`] with somebody else deciding which candidates are worth encoding in full.
///
/// A chooser narrows the list and nothing else. It cannot offer a candidate that does not apply, so
/// whatever it picks still has to encode the whole chunk and still has to decode, and the worst a
/// bad one can do is come out bigger than [`encode`] would have.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_with(values: &[i64], chooser: &dyn Chooser) -> Result<Vec<u8>> {
    encode_at(values, 0, chooser)
}

/// Decodes a chunk written by [`encode`].
///
/// # Errors
///
/// If the bytes are truncated, carry an unknown tag, or describe a chunk whose parts do not agree
/// with each other.
pub fn decode(bytes: &[u8]) -> Result<Vec<i64>> {
    let mut reader = Reader::new(bytes);
    let values = with_decoding(|scratch| decode_chunk(&mut reader, scratch))?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a chunk",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// Decodes selected row positions from a chunk written by [`encode`].
///
/// Positions must be sorted and unique. Packed chunks read only the words holding those positions,
/// and run length chunks walk their run boundaries without expanding the output. Other cascade
/// shapes use the full decoder and select afterward until they have a point form of their own.
///
/// # Errors
///
/// As [`decode`], or if a position is outside the chunk or the positions are not strictly
/// increasing.
pub fn decode_selected(bytes: &[u8], positions: &[usize]) -> Result<Vec<i64>> {
    if positions.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::internal("selected integer positions are not sorted and unique"));
    }
    let mut reader = Reader::new(bytes);
    let values = with_decoding(|scratch| decode_selected_chunk(&mut reader, positions, scratch))?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding selected values",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// Decodes a chunk that sits at the front of a longer buffer, and says how many bytes it took.
///
/// A string column holds integer chunks inside its own body, and the reader on that side cannot
/// know where the nested chunk ends until it has been read. A chunk is self delimiting, so this is
/// the same work [`decode`] does without the check that nothing follows.
///
/// # Errors
///
/// As [`decode`], except that trailing bytes are what the caller asked about rather than an error.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Vec<i64>, usize)> {
    let mut reader = Reader::new(bytes);
    let values = with_decoding(|scratch| decode_chunk(&mut reader, scratch))?;
    Ok((values, reader.used()))
}

/// [`describe`] over a chunk at the front of a longer buffer, and how many bytes it took.
///
/// # Errors
///
/// As [`decode_prefix`].
pub fn describe_prefix(bytes: &[u8]) -> Result<(String, usize)> {
    let mut reader = Reader::new(bytes);
    let text = describe_chunk(&mut reader)?;
    Ok((text, reader.used()))
}

/// The size in bytes of every candidate, for a report that wants to say what the cascade was
/// chosen over rather than only what it chose. A candidate that does not apply is absent.
///
/// # Errors
///
/// As [`encode`].
pub fn candidate_sizes(values: &[i64]) -> Result<Vec<(Kind, usize)>> {
    let mut sizes = Vec::new();
    for kind in candidates(values, 0, &EXHAUSTIVE) {
        if let Some(bytes) = encode_as(kind, values, 0, &EXHAUSTIVE)? {
            sizes.push((kind, bytes.len()));
        }
    }
    Ok(sizes)
}

/// Which candidates [`encode`] would try on this chunk, in the order it tries them.
///
/// The chooser is exhaustive, so this is also the list of encodes it pays for to return one of
/// them. A caller measuring where the encode time goes needs the list separately from the sizes,
/// because a candidate that is offered and turns out not to apply still costs whatever it spent
/// finding that out.
#[must_use]
pub fn offered(values: &[i64]) -> Vec<Kind> {
    candidates(values, 0, &EXHAUSTIVE)
}

/// One candidate on its own, which is what the chooser calls once per entry in [`offered`].
///
/// `None` when the encoding does not apply. This is here so that the time the chooser spends can be
/// attributed to the candidate that spent it, which is the measurement F2 wants before anybody
/// replaces the exhaustive search with a sampled one. It is not how a writer encodes a chunk:
/// [`encode`] is, and picking a kind by hand gives up the only thing the chooser is for.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_only(kind: Kind, values: &[i64]) -> Result<Option<Vec<u8>>> {
    encode_as(kind, values, 0, &EXHAUSTIVE)
}

/// How big one candidate comes out, which is all a sampling chooser needs from it.
///
/// The bytes are thrown away, so this says nothing [`encode_only`] does not. It is `pub(crate)` and
/// separate so that the sampler in [`crate::chooser`] is not handing back buffers it will not read.
pub(crate) fn size_as(kind: Kind, values: &[i64], depth: u8) -> Result<Option<usize>> {
    Ok(encode_as(kind, values, depth, &EXHAUSTIVE)?.map(|bytes| bytes.len()))
}

/// The kind at every level of an encoded chunk, in the order the encoder chose them.
///
/// The order is the one [`encode_with`] asks its chooser in: a level, then everything under its
/// first inner chunk, then everything under its second. So a chooser that hands these back one per
/// question gets the same cascade on a chunk that offers the same kinds, without searching any of
/// it. That is what a writer with many small parts of one column wants, because the search is
/// most of what the encode costs and neighbouring parts nearly always come out the same shape.
///
/// # Errors
///
/// As [`decode`].
pub fn shape(bytes: &[u8]) -> Result<Vec<Kind>> {
    let mut reader = Reader::new(bytes);
    let mut kinds = Vec::new();
    shape_chunk(&mut reader, &mut kinds)?;
    Ok(kinds)
}

/// The cascade a chunk was encoded as, as a line of text like `DICT(PACKED, PACKED)`.
///
/// # Errors
///
/// As [`decode`].
pub fn describe(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    describe_chunk(&mut reader)
}

fn encode_at(values: &[i64], depth: u8, chooser: &dyn Chooser) -> Result<Vec<u8>> {
    let offered = candidates(values, depth, chooser);
    let mut best: Option<Vec<u8>> = None;
    for kind in chooser.narrow_integers(values, &offered, depth) {
        let Some(bytes) = encode_as(kind, values, depth, chooser)? else {
            continue;
        };
        if best.as_ref().is_none_or(|current| bytes.len() < current.len()) {
            best = Some(bytes);
        }
    }
    // `Packed` applies to every input including the empty one, so the chooser always has at least
    // one candidate and this cannot be reached without a bug in `candidates`.
    best.ok_or_else(|| Error::internal("no encoding applied to the chunk"))
}

/// Which candidates are worth encoding for this input.
///
/// The filters here are not the cost model. They are the cases where the encoding cannot be
/// expressed at all, or is provably larger than `Packed` on the same data, so that the exhaustive
/// chooser does not spend a dictionary build on a column of 100,000 distinct values to discover
/// what its distinct count already said.
///
/// A kind the chooser says it will never keep is not tested for at all. The test for a dictionary
/// sorts a copy of the chunk, and this runs at every level of the cascade, so a chooser that never
/// keeps a dictionary was paying for a sort per level to find out something it would ignore.
fn candidates(values: &[i64], depth: u8, chooser: &dyn Chooser) -> Vec<Kind> {
    let mut kinds = vec![Kind::Packed];
    if depth >= MAX_DEPTH || values.is_empty() {
        return kinds;
    }
    if values.iter().all(|value| *value == values[0]) {
        // Nothing else can beat 13 bytes, so this is the whole answer rather than a candidate.
        return vec![Kind::Constant];
    }
    let considered = |kind| chooser.considers_integer(kind, depth);
    if considered(Kind::Delta) && values.len() >= 2 && deltas_fit(values) {
        kinds.push(Kind::Delta);
    }
    if considered(Kind::Rle) && run_count(values) * 4 <= values.len() * 3 {
        kinds.push(Kind::Rle);
    }
    if considered(Kind::Dict) && spread_of(values).0 * 2 <= values.len() {
        kinds.push(Kind::Dict);
    }
    if considered(Kind::Sparse)
        && majority(values).is_some_and(|(_, count)| count * 10 >= values.len() * 8)
    {
        kinds.push(Kind::Sparse);
    }
    if considered(Kind::Strided) && stride_of(values).is_some() {
        kinds.push(Kind::Strided);
    }
    kinds
}

/// `None` when the encoding does not apply to this input, which the caller treats as a candidate
/// that did not run rather than as a failure.
fn encode_as(
    kind: Kind,
    values: &[i64],
    depth: u8,
    chooser: &dyn Chooser,
) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    put_u8(&mut out, kind.tag());
    put_u32(&mut out, u32::try_from(values.len()).map_err(|_| too_long(values.len()))?);
    match kind {
        Kind::Constant => {
            let Some(first) = values.first() else {
                return Ok(None);
            };
            if values.iter().any(|value| value != first) {
                return Ok(None);
            }
            put_i64(&mut out, *first);
        }
        Kind::Packed => encode_packed(values, &mut out)?,
        Kind::Delta => {
            // An empty chunk has no first value to hang the differences off. The search never asks
            // for one because `candidates` rules it out, but `encode_only` goes straight past that
            // and used to index into the chunk anyway.
            let (Some(first), Some(deltas)) = (values.first(), deltas(values)) else {
                return Ok(None);
            };
            put_i64(&mut out, *first);
            out.extend_from_slice(&encode_at(&deltas, depth + 1, chooser)?);
        }
        Kind::Rle => {
            let (run_values, run_lengths) = runs(values);
            if run_values.is_empty() {
                return Ok(None);
            }
            out.extend_from_slice(&encode_at(&run_values, depth + 1, chooser)?);
            out.extend_from_slice(&encode_at(&run_lengths, depth + 1, chooser)?);
        }
        Kind::Dict => {
            let dictionary = distinct_values(values);
            if dictionary.is_empty() {
                return Ok(None);
            }
            let codes = codes_over(values, &dictionary);
            out.extend_from_slice(&encode_at(&dictionary, depth + 1, chooser)?);
            out.extend_from_slice(&encode_at(&codes, depth + 1, chooser)?);
        }
        Kind::Sparse => {
            // The majority is the most frequent value whenever there is one, and a chunk the search
            // offers this for always has one. `encode_only` can ask about any chunk, so the sort is
            // still there for a chunk with no majority.
            let Some((value, _)) = majority(values).or_else(|| spread_of(values).1) else {
                return Ok(None);
            };
            let mut positions = Vec::new();
            let mut exceptions = Vec::new();
            for (index, other) in values.iter().enumerate() {
                if *other != value {
                    positions.push(index as i64);
                    exceptions.push(*other);
                }
            }
            put_i64(&mut out, value);
            put_u32(
                &mut out,
                u32::try_from(positions.len()).map_err(|_| too_long(positions.len()))?,
            );
            out.extend_from_slice(&encode_at(&positions, depth + 1, chooser)?);
            out.extend_from_slice(&encode_at(&exceptions, depth + 1, chooser)?);
        }
        Kind::Strided => {
            let (Some(base), Some(stride)) = (values.iter().min().copied(), stride_of(values))
            else {
                return Ok(None);
            };
            let mut steps = Vec::with_capacity(values.len());
            for value in values {
                let step = offset_from(*value, base) / stride;
                // A step count the recursion cannot hold. An offset is at most 65 bits because both
                // ends came from an `i64`, and only a stride of one leaves it that wide, which is a
                // stride this never offers. Refused rather than wrapped, because a candidate that
                // does not apply is one the chooser skips.
                let Ok(step) = i64::try_from(step) else {
                    return Ok(None);
                };
                steps.push(step);
            }
            put_i64(&mut out, base);
            put_u64(&mut out, stride);
            out.extend_from_slice(&encode_at(&steps, depth + 1, chooser)?);
        }
    }
    Ok(Some(out))
}

/// Frame of reference and bit packing, one base and one width per 1024 values.
///
/// A base per unit rather than per chunk is most of what makes this work on real columns. A
/// timestamp column over a day drifts across a range that needs 47 bits, and the same column inside
/// any one unit spans a few seconds and needs 12. One base per row group would pay the 47 on every
/// value.
///
/// A unit shorter than 1024 values, which is the last one of any chunk whose length is not a
/// multiple of the unit and is the only one of every short array in a cascade, goes through
/// [`bitpack::pack_tail`] instead. The transposed layout has no partial form and would charge a
/// five entry dictionary for 1024 entries.
fn encode_packed(values: &[i64], out: &mut Vec<u8>) -> Result<()> {
    // The same three buffers for every unit, for the reason written on `Decoding` on the other side.
    // The chooser encodes every candidate it is offered before it picks one, so this loop runs more
    // often on the way in than the decoding loop does on the way out.
    let mut offsets: Vec<u64> = Vec::with_capacity(VALUES);
    // Held at the width 64 length for the reason written on `Decoding`, so a narrower unit writes
    // the front of it and the resize per unit goes away.
    let mut packed: Vec<u64> = vec![0; bitpack::packed_len::<u64>(64)];
    let mut transposed = bitpack::Scratch::<u64>::new();
    for unit in values.chunks(VALUES) {
        let base = unit.iter().copied().min().unwrap_or(0);
        offsets.clear();
        offsets.extend(unit.iter().map(|value| offset_from(*value, base)));
        let width = bitpack::required_width(&offsets);
        put_i64(out, base);
        put_u8(out, u8::try_from(width).map_err(|_| Error::internal("impossible width"))?);
        if unit.len() == VALUES {
            let words = bitpack::packed_len::<u64>(width);
            bitpack::pack_with(&offsets, width, &mut packed[..words], &mut transposed)?;
            for word in &packed[..words] {
                put_u64(out, *word);
            }
        } else {
            bitpack::pack_tail(&offsets, width, out)?;
        }
    }
    Ok(())
}

/// The buffer a decode reuses from one unit of 1024 values to the next.
///
/// This used to be allocated inside the loop, and because it was allocated with a value rather than
/// grown, the allocator zeroed it and then the decode overwrote every byte. In a ClickBench profile
/// that zeroing was the single largest item, ahead of the unpacking it was making room for, because
/// a scan pays it once per 1024 rows of every packed integer column it reads.
///
/// It is threaded through the recursion rather than made per call because a chunk is a cascade. A
/// dictionary of deltas is three nested decodes, and each of them would otherwise make its own.
///
/// It starts empty and is grown on the first unit that needs it, to its largest size rather than to
/// the size that unit wants, so that every unit after the first finds it the right length already
/// and nothing is zeroed or resized again.
///
/// There used to be a second buffer here holding one unit of unpacked offsets, which the decode
/// then walked to add the frame of reference base back on. The unpackers take the base now and
/// write into the chunk directly, so that buffer and the pass over it are both gone.
///
/// It lives on the thread rather than in the caller, which is worth saying why. A chunk is a row
/// group, and a row group in the native format is about a thousand rows, which is one unit. So there
/// is no second unit in a chunk to reuse anything and holding this per call is strictly worse than
/// allocating per unit was: it was tried, and it cost more in the growing than it saved in the
/// zeroing. What there are many of is chunks, one per part per column, and the thread that reads
/// them reads them one after another. That is the loop the reuse belongs to, and reaching it by
/// passing a buffer down would mean a parameter through every page decoder in the storage layer for
/// a buffer none of them has an opinion about.
struct Decoding {
    /// The packed words of one unit, as read off the wire. Held at the width 64 length, which is the
    /// largest a unit can be, so a narrower unit uses the front of it.
    packed: Vec<u64>,
}

thread_local! {
    /// The buffers this thread decodes through. See [`Decoding`].
    static DECODING: std::cell::RefCell<Decoding> =
        const { std::cell::RefCell::new(Decoding::new()) };
}

/// Runs a decode over this thread's buffers.
///
/// Nothing inside a decode calls back into one, so the borrow is never already taken. It is asked
/// for rather than assumed anyway, and a decode that somehow arrives while another is running gets
/// buffers of its own rather than a panic, because the alternative is a crash in a reader on a
/// path nobody exercised.
fn with_decoding<T>(run: impl FnOnce(&mut Decoding) -> T) -> T {
    DECODING.with(|cell| match cell.try_borrow_mut() {
        Ok(mut scratch) => run(&mut scratch),
        Err(_) => run(&mut Decoding::new()),
    })
}

impl Decoding {
    /// A buffer that has not made room for anything yet.
    const fn new() -> Self {
        Self { packed: Vec::new() }
    }

    /// Makes room for one unit. A no op every time after the first.
    fn ready(&mut self) {
        if self.packed.len() != bitpack::packed_len::<u64>(64) {
            self.packed.resize(bitpack::packed_len::<u64>(64), 0);
        }
    }
}

fn decode_chunk(reader: &mut Reader<'_>, scratch: &mut Decoding) -> Result<Vec<i64>> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => Ok(vec![reader.i64()?; count]),
        Kind::Packed => {
            // One buffer for the chunk, and every value written into it once. Both unpackers take
            // the frame of reference base and put the value it belongs to where it goes, so there
            // is no unit of raw offsets in between and no second pass to fold the base back in.
            let mut values = vec![0i64; count];
            scratch.ready();
            let mut done = 0;
            while done < count {
                let base = reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - done).min(VALUES);
                let into = &mut values[done..done + wanted];
                if wanted == VALUES {
                    let words = bitpack::packed_len::<u64>(width);
                    for word in &mut scratch.packed[..words] {
                        *word = reader.u64()?;
                    }
                    bitpack::unpack_mapped(&scratch.packed[..words], width, into, |offset| {
                        value_from(offset, base)
                    })?;
                } else {
                    let bytes = reader.bytes(bitpack::tail_len(wanted, width))?;
                    bitpack::unpack_tail_into(bytes, width, into, |offset| {
                        value_from(offset, base)
                    })?;
                }
                done += wanted;
            }
            Ok(values)
        }
        Kind::Delta => {
            let first = reader.i64()?;
            let deltas = decode_chunk(reader, scratch)?;
            let mut values = Vec::with_capacity(count);
            values.push(first);
            let mut current = first;
            for delta in deltas {
                current = current.wrapping_add(unzigzag(delta as u64));
                values.push(current);
            }
            check_count(values.len(), count)?;
            Ok(values)
        }
        Kind::Rle => {
            let run_values = decode_chunk(reader, scratch)?;
            let run_lengths = decode_chunk(reader, scratch)?;
            if run_values.len() != run_lengths.len() {
                return Err(Error::internal("an RLE chunk has more runs than run lengths"));
            }
            // Room for one run past the end, so the write below never has to ask how much of its
            // fixed width landed inside the chunk. Room rather than values: a short run appends its
            // fixed eight and then cuts back to its real length, and a long one appends itself, so
            // every value is written by the run it belongs to and nothing is zeroed first. On a
            // sorted column the runs are thousands long and the zeroing was a second write of the
            // whole chunk.
            let mut values = Vec::with_capacity(count + RUN);
            let mut at = 0usize;
            for (value, length) in run_values.into_iter().zip(run_lengths) {
                let length = usize::try_from(length)
                    .map_err(|_| Error::internal("a negative RLE run length"))?;
                let end = at
                    .checked_add(length)
                    .filter(|end| *end <= count)
                    .ok_or_else(|| Error::internal("an RLE run ends past its chunk"))?;
                if length <= RUN {
                    values.extend_from_slice(&[value; RUN]);
                    values.truncate(end);
                } else {
                    values.resize(end, value);
                }
                at = end;
            }
            check_count(at, count)?;
            Ok(values)
        }
        Kind::Dict => {
            let dictionary = decode_chunk(reader, scratch)?;
            let codes = decode_chunk(reader, scratch)?;
            let mut values = Vec::with_capacity(count);
            for code in codes {
                let index =
                    usize::try_from(code).ok().and_then(|index| dictionary.get(index)).ok_or_else(
                        || Error::internal(format!("code {code} is not in the dictionary")),
                    )?;
                values.push(*index);
            }
            check_count(values.len(), count)?;
            Ok(values)
        }
        Kind::Sparse => {
            let value = reader.i64()?;
            let exception_count = reader.u32()? as usize;
            let positions = decode_chunk(reader, scratch)?;
            let exceptions = decode_chunk(reader, scratch)?;
            if positions.len() != exception_count || exceptions.len() != exception_count {
                return Err(Error::internal("a sparse chunk disagrees about its exception count"));
            }
            let mut values = vec![value; count];
            for (position, exception) in positions.into_iter().zip(exceptions) {
                let position = usize::try_from(position)
                    .ok()
                    .filter(|position| *position < count)
                    .ok_or_else(|| {
                        Error::internal(format!("exception at {position} is outside the chunk"))
                    })?;
                values[position] = exception;
            }
            Ok(values)
        }
        Kind::Strided => {
            let base = reader.i64()?;
            let stride = reader.u64()?;
            let steps = decode_chunk(reader, scratch)?;
            check_count(steps.len(), count)?;
            let mut values = Vec::with_capacity(count);
            for step in steps {
                let step = u64::try_from(step)
                    .map_err(|_| Error::internal("a negative number of strides"))?;
                values.push(value_from(step.wrapping_mul(stride), base));
            }
            Ok(values)
        }
    }
}

fn decode_selected_chunk(
    reader: &mut Reader<'_>,
    positions: &[usize],
    scratch: &mut Decoding,
) -> Result<Vec<i64>> {
    let Some(&tag) = reader.rest().first() else {
        return Err(Error::internal("a chunk ended before its encoding tag"));
    };
    let kind = Kind::from_tag(tag)?;
    if !matches!(kind, Kind::Constant | Kind::Packed | Kind::Rle) {
        let values = decode_chunk(reader, scratch)?;
        return positions
            .iter()
            .map(|&position| {
                values.get(position).copied().ok_or_else(|| {
                    Error::internal(format!(
                        "selected integer position {position} is outside {} values",
                        values.len()
                    ))
                })
            })
            .collect();
    }

    let decoded = Kind::from_tag(reader.u8()?)?;
    debug_assert_eq!(decoded, kind);
    let count = reader.u32()? as usize;
    if positions.last().is_some_and(|&position| position >= count) {
        return Err(Error::internal(format!(
            "selected integer position {} is outside {count} values",
            positions.last().expect("a last position exists")
        )));
    }
    match kind {
        Kind::Constant => {
            let value = reader.i64()?;
            Ok(vec![value; positions.len()])
        }
        Kind::Packed => {
            let mut out = Vec::with_capacity(positions.len());
            let mut from = 0;
            let mut done = 0;
            while done < count {
                let base = reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - done).min(VALUES);
                let upto = positions.partition_point(|&position| position < done + wanted);
                if wanted == VALUES {
                    let bytes = reader.bytes(bitpack::packed_len::<u64>(width) * 8)?;
                    for &position in &positions[from..upto] {
                        let offset = bitpack::unpack_u64_at(bytes, width, position - done)?;
                        out.push(value_from(offset, base));
                    }
                } else {
                    let bytes = reader.bytes(bitpack::tail_len(wanted, width))?;
                    for &position in &positions[from..upto] {
                        let offset = bitpack::tail_at(bytes, width, position - done)?;
                        out.push(value_from(offset, base));
                    }
                }
                from = upto;
                done += wanted;
            }
            Ok(out)
        }
        Kind::Rle => {
            let run_value_bytes = reader.rest();
            let mut run_value_reader = Reader::new(run_value_bytes);
            let run_value_count = skip_chunk(&mut run_value_reader)?;
            let run_value_len = run_value_reader.used();
            reader.skip(run_value_len)?;
            let run_lengths = decode_chunk(reader, scratch)?;
            if run_value_count != run_lengths.len() {
                return Err(Error::internal("an RLE chunk has more runs than run lengths"));
            }
            let mut wanted_runs = Vec::new();
            let mut selected_per_run = Vec::new();
            let mut selected = 0;
            let mut at = 0usize;
            for (run, length) in run_lengths.into_iter().enumerate() {
                let length = usize::try_from(length)
                    .map_err(|_| Error::internal("a negative RLE run length"))?;
                let end = at
                    .checked_add(length)
                    .filter(|end| *end <= count)
                    .ok_or_else(|| Error::internal("an RLE run ends past its chunk"))?;
                let before = selected;
                while selected < positions.len() && positions[selected] < end {
                    if positions[selected] < at {
                        return Err(Error::internal("selected integer positions went backwards"));
                    }
                    selected += 1;
                }
                if selected != before {
                    wanted_runs.push(run);
                    selected_per_run.push(selected - before);
                }
                at = end;
            }
            check_count(at, count)?;
            if selected != positions.len() {
                return Err(Error::internal("an RLE chunk ended before a selected position"));
            }
            let run_values = decode_selected(&run_value_bytes[..run_value_len], &wanted_runs)?;
            let mut out = Vec::with_capacity(positions.len());
            for (value, repeat) in run_values.into_iter().zip(selected_per_run) {
                out.extend(std::iter::repeat_n(value, repeat));
            }
            Ok(out)
        }
        _ => unreachable!("unsupported kinds used the full decoder"),
    }
}

/// Advances over one encoded chunk without materializing its values and returns its row count.
fn skip_chunk(reader: &mut Reader<'_>) -> Result<usize> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => reader.skip(8)?,
        Kind::Packed => skip_packed(reader, count)?,
        Kind::Delta => {
            reader.skip(8)?;
            skip_chunk(reader)?;
        }
        Kind::Rle | Kind::Dict => {
            skip_chunk(reader)?;
            skip_chunk(reader)?;
        }
        Kind::Sparse => {
            reader.skip(12)?;
            skip_chunk(reader)?;
            skip_chunk(reader)?;
        }
        Kind::Strided => {
            reader.skip(16)?;
            skip_chunk(reader)?;
        }
    }
    Ok(count)
}

/// Advances over the units of a `Packed` body of `count` values.
fn skip_packed(reader: &mut Reader<'_>, count: usize) -> Result<()> {
    let mut done = 0;
    while done < count {
        reader.skip(8)?;
        let width = reader.u8()? as usize;
        if width > 64 {
            return Err(Error::internal(format!("a packed integer width of {width} is past 64")));
        }
        let wanted = (count - done).min(VALUES);
        let bytes = if wanted == VALUES {
            bitpack::packed_len::<u64>(width)
                .checked_mul(8)
                .ok_or_else(|| Error::internal("packed integer size overflow"))?
        } else {
            bitpack::tail_len(wanted, width)
        };
        reader.skip(bytes)?;
        done += wanted;
    }
    Ok(())
}

/// [`shape`] for one chunk and everything inside it.
fn shape_chunk(reader: &mut Reader<'_>, kinds: &mut Vec<Kind>) -> Result<()> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    kinds.push(kind);
    match kind {
        Kind::Constant => reader.skip(8)?,
        Kind::Packed => skip_packed(reader, count)?,
        Kind::Delta => {
            reader.skip(8)?;
            shape_chunk(reader, kinds)?;
        }
        Kind::Rle | Kind::Dict => {
            shape_chunk(reader, kinds)?;
            shape_chunk(reader, kinds)?;
        }
        Kind::Sparse => {
            reader.skip(12)?;
            shape_chunk(reader, kinds)?;
            shape_chunk(reader, kinds)?;
        }
        Kind::Strided => {
            reader.skip(16)?;
            shape_chunk(reader, kinds)?;
        }
    }
    Ok(())
}

fn describe_chunk(reader: &mut Reader<'_>) -> Result<String> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    Ok(match kind {
        Kind::Constant => {
            reader.i64()?;
            "CONSTANT".to_string()
        }
        Kind::Packed => {
            let mut widths = Vec::new();
            let mut seen = 0;
            while seen < count {
                reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - seen).min(VALUES);
                if wanted == VALUES {
                    for _ in 0..bitpack::packed_len::<u64>(width) {
                        reader.u64()?;
                    }
                } else {
                    reader.bytes(bitpack::tail_len(wanted, width))?;
                }
                widths.push(width);
                seen += wanted;
            }
            let low = widths.iter().copied().min().unwrap_or(0);
            let high = widths.iter().copied().max().unwrap_or(0);
            // Square brackets rather than round ones, so that a reader and a test can both take a
            // parenthesis to mean one more level of cascade and nothing else.
            if low == high {
                format!("FOR+BITPACK[{low}]")
            } else {
                format!("FOR+BITPACK[{low}..{high}]")
            }
        }
        Kind::Delta => {
            reader.i64()?;
            format!("DELTA({})", describe_chunk(reader)?)
        }
        Kind::Rle => {
            let values = describe_chunk(reader)?;
            let lengths = describe_chunk(reader)?;
            format!("RLE({values}, {lengths})")
        }
        Kind::Dict => {
            let dictionary = describe_chunk(reader)?;
            let codes = describe_chunk(reader)?;
            format!("DICT({dictionary}, {codes})")
        }
        Kind::Sparse => {
            reader.i64()?;
            reader.u32()?;
            let positions = describe_chunk(reader)?;
            let exceptions = describe_chunk(reader)?;
            format!("SPARSE({positions}, {exceptions})")
        }
        Kind::Strided => {
            reader.i64()?;
            let stride = reader.u64()?;
            format!("STRIDE[{stride}]({})", describe_chunk(reader)?)
        }
    })
}

/// The step every value of the chunk is a whole number of, or `None` when there is not one worth
/// having.
///
/// This is the greatest common divisor of every value's distance from the smallest one. A timestamp
/// column loaded from a source that recorded whole seconds holds microseconds that are all multiples
/// of a million, and without this the frame of reference pays twenty bits a value to write down the
/// twenty zero bits at the bottom of every one of them.
///
/// The walk stops the moment the divisor reaches one, which is what makes this affordable to ask on
/// every chunk. Two values that share no factor are enough to answer, and on a column of arbitrary
/// numbers that is almost always the first pair.
fn stride_of(values: &[i64]) -> Option<u64> {
    let base = values.iter().min().copied()?;
    let mut divisor = 0u64;
    for value in values {
        divisor = gcd(divisor, offset_from(*value, base));
        if divisor == 1 {
            return None;
        }
    }
    // Zero is every value being the base, which `Constant` already holds for nothing, and one is
    // the frame of reference on its own with two extra words of header.
    (divisor > 1).then_some(divisor)
}

/// Binary GCD, which is the one without a division in it.
fn gcd(mut left: u64, mut right: u64) -> u64 {
    if left == 0 {
        return right;
    }
    if right == 0 {
        return left;
    }
    let shift = (left | right).trailing_zeros();
    left >>= left.trailing_zeros();
    loop {
        right >>= right.trailing_zeros();
        if left > right {
            std::mem::swap(&mut left, &mut right);
        }
        right -= left;
        if right == 0 {
            return left << shift;
        }
    }
}

/// The distance from the frame of reference base, which is always representable in a `u64` because
/// both ends came from an `i64` and the width of the difference is at most 65 bits minus the sign.
fn offset_from(value: i64, base: i64) -> u64 {
    (i128::from(value) - i128::from(base)) as u64
}

fn value_from(offset: u64, base: i64) -> i64 {
    (i128::from(base) + i128::from(offset)) as i64
}

/// Zigzag, so that a column that counts down packs as narrowly as one that counts up. Without it a
/// delta of -1 is 64 bits of ones.
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The zigzagged differences, or `None` if any difference is too wide to be one.
///
/// A column holding both `i64::MIN` and `i64::MAX` has a difference that does not fit in an `i64`,
/// and rather than widening every delta array to 128 bits for a case that does not occur in data,
/// the encoding declines to apply. `Packed` covers it.
/// Whether every neighbouring difference fits in an `i64`, which is the only thing the candidate
/// list needs to know about deltas.
///
/// The candidate list used to answer this by building the whole delta array and checking that it
/// came back, which is an allocation and a pass over the chunk thrown away on every chunk, and then
/// `Kind::Delta` built it again. This is the same pass with nothing kept.
fn deltas_fit(values: &[i64]) -> bool {
    values.windows(2).all(|pair| i64::try_from(i128::from(pair[1]) - i128::from(pair[0])).is_ok())
}

fn deltas(values: &[i64]) -> Option<Vec<i64>> {
    let mut deltas = Vec::with_capacity(values.len().saturating_sub(1));
    for pair in values.windows(2) {
        let difference = i128::from(pair[1]) - i128::from(pair[0]);
        let difference = i64::try_from(difference).ok()?;
        deltas.push(zigzag(difference) as i64);
    }
    Some(deltas)
}

fn run_count(values: &[i64]) -> usize {
    let mut runs = 0;
    let mut previous = None;
    for value in values {
        if previous != Some(value) {
            runs += 1;
            previous = Some(value);
        }
    }
    runs
}

fn runs(values: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let mut run_values: Vec<i64> = Vec::new();
    let mut run_lengths: Vec<i64> = Vec::new();
    for value in values {
        if run_values.last() == Some(value) {
            *run_lengths.last_mut().expect("a run length exists beside every run value") += 1;
        } else {
            run_values.push(*value);
            run_lengths.push(1);
        }
    }
    (run_values, run_lengths)
}

/// The distinct values in sorted order.
///
/// Sorted rather than in order of first appearance, because an ordered dictionary is what lets a
/// range predicate become a code range instead of a code set, per section 6.7, and because the
/// codes of a clustered column then run in order and delta encode.
/// How many distinct values there are and which one occurs most often, from one sort.
///
/// Both questions are about the histogram of the chunk and neither needs the histogram itself, so
/// one sorted copy and one walk over it answers both. They used to be two functions that each sorted
/// their own copy and threw it away, which is a chunk sorted twice on every chunk at every level of
/// the cascade before a single candidate has been encoded.
///
/// No hash map, because the sort is what makes the walk a scan of equal runs, and a hash map would
/// pay a lookup per value to learn the same thing.
fn spread_of(values: &[i64]) -> (usize, Option<(i64, usize)>) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mut distinct = 0;
    let mut best: Option<(i64, usize)> = None;
    let mut index = 0;
    while index < sorted.len() {
        let value = sorted[index];
        let mut end = index;
        while end < sorted.len() && sorted[end] == value {
            end += 1;
        }
        distinct += 1;
        let count = end - index;
        if best.is_none_or(|(_, seen)| count > seen) {
            best = Some((value, count));
        }
        index = end;
    }
    (distinct, best)
}

/// The value more than half of the chunk holds, and how many times, found in two passes without
/// sorting anything.
///
/// This is the vote that keeps one candidate and a lead: a value that holds more than half the
/// chunk outlasts every other value put together, so it is the candidate left at the end, and the
/// second pass checks that the candidate really does hold more than half. When it does it is the
/// value [`spread_of`] would name as the most frequent, since a value over half the chunk has no tie.
fn majority(values: &[i64]) -> Option<(i64, usize)> {
    let mut candidate = *values.first()?;
    let mut lead = 0usize;
    for value in values {
        if lead == 0 {
            candidate = *value;
            lead = 1;
        } else if *value == candidate {
            lead += 1;
        } else {
            lead -= 1;
        }
    }
    let count = values.iter().filter(|value| **value == candidate).count();
    (count * 2 > values.len()).then_some((candidate, count))
}

/// The distinct values in sorted order, for the same reason the string dictionary is sorted: an
/// ordered dictionary turns a range predicate into a code range rather than a code set.
fn distinct_values(values: &[i64]) -> Vec<i64> {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    distinct
}

/// Where each value sits in the dictionary.
///
/// The string side builds its dictionary and its codes together from one sort of a permutation,
/// because the alternative there is a copy of every value onto the heap and a `memcmp` per level of
/// a binary search per row. This side was changed to match and it measured slower, so it was changed
/// back. An integer dictionary only exists when the distinct count is at most half the row count, so
/// the search is over something small and cache resident, the comparison is one integer rather than
/// a string, and carrying the source index through the sort means sorting a padded sixteen byte pair
/// instead of an eight byte value. The search is cheaper than the wider sort.
fn codes_over(values: &[i64], dictionary: &[i64]) -> Vec<i64> {
    values
        .iter()
        .map(|value| {
            dictionary
                .binary_search(value)
                .expect("the dictionary is the distinct values of this chunk") as i64
        })
        .collect()
}

fn check_count(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::internal(format!(
            "a chunk says it holds {expected} values and decoded to {actual}"
        )))
    }
}

fn too_long(len: usize) -> Error {
    Error::internal(format!("a chunk of {len} values is longer than the format allows"))
}

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[i64]) -> Vec<u8> {
        let bytes = encode(values).unwrap();
        assert_eq!(decode(&bytes).unwrap(), values, "{}", describe(&bytes).unwrap());
        bytes
    }

    fn kind_of(bytes: &[u8]) -> Kind {
        Kind::from_tag(bytes[0]).unwrap()
    }

    /// The same xorshift the bit packing tests use, for the same reason.
    struct Random(u64);

    impl Random {
        fn new() -> Self {
            Self(0x9e37_79b9_7f4a_7c15)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn the_dictionary_is_sorted_and_the_codes_point_back_at_the_values() {
        let values = vec![30i64, 10, 30, 20, 10, -5];
        let dictionary = distinct_values(&values);
        let codes = codes_over(&values, &dictionary);
        assert_eq!(dictionary, vec![-5, 10, 20, 30]);
        assert_eq!(codes, vec![3, 1, 3, 2, 1, 0]);
        for (code, value) in codes.iter().zip(&values) {
            assert_eq!(dictionary[*code as usize], *value);
        }
    }

    #[test]
    fn one_sort_gives_the_distinct_count_and_the_most_frequent_value() {
        let values = vec![7i64, 7, 7, 1, 2, 2];
        assert_eq!(spread_of(&values), (3, Some((7, 3))));
        assert_eq!(spread_of(&[]), (0, None));
        assert_eq!(spread_of(&[9]), (1, Some((9, 1))));

        // A tie goes to the value that sorts first, which is arbitrary but has to be stable,
        // because Sparse writes the dominant value into the chunk and the size depends on it.
        assert_eq!(spread_of(&[4i64, 4, 8, 8]), (2, Some((4, 2))));
    }

    #[test]
    fn the_majority_is_the_most_frequent_value_whenever_there_is_one() {
        let chunks: Vec<Vec<i64>> = vec![
            vec![],
            vec![3],
            vec![1, 2],
            vec![1, 1, 2],
            vec![2, 1, 1],
            vec![4, 4, 8, 8],
            vec![7, 1, 7, 2, 7, 3, 7],
            vec![1, 2, 3, 9, 9, 9, 9],
            (0..1000).map(|index| if index % 5 == 0 { index } else { -4 }).collect(),
            (0..1000).map(|index| index % 3).collect(),
        ];
        for chunk in chunks {
            let (_, dominant) = spread_of(&chunk);
            let expected = dominant.filter(|(_, count)| count * 2 > chunk.len());
            assert_eq!(majority(&chunk), expected, "{chunk:?}");
        }
    }

    #[test]
    fn deltas_that_do_not_fit_are_refused_before_they_are_built() {
        assert!(deltas_fit(&[1i64, 2, 3]));
        assert!(deltas_fit(&[i64::MAX, i64::MAX]));
        assert!(!deltas_fit(&[i64::MIN, i64::MAX]));
        assert_eq!(deltas_fit(&[i64::MIN, i64::MAX]), deltas(&[i64::MIN, i64::MAX]).is_some());
        assert_eq!(deltas_fit(&[1i64, 2, 3]), deltas(&[1i64, 2, 3]).is_some());
    }

    #[test]
    fn what_the_chooser_returns_is_the_smallest_of_what_it_was_offered() {
        // `offered` and `encode_only` are what `cargo xtask encode` splits the chooser's seconds
        // with, so they have to describe the chooser that actually runs rather than a second copy
        // of its rules that drifts. This is the assertion that keeps the two the same thing.
        let mut random = Random::new();
        let noise: Vec<i64> = (0..2000).map(|_| (random.next() % 5000) as i64).collect();
        let runs: Vec<i64> = (0..2000).map(|index: i64| index / 100).collect();
        let climbing: Vec<i64> = (0..2000).map(|index| 1_700_000_000 + index).collect();
        for values in [noise, runs, climbing, vec![7; 300], Vec::new()] {
            let chosen = encode(&values).unwrap();
            let mut smallest: Option<Vec<u8>> = None;
            for kind in offered(&values) {
                let Some(bytes) = encode_only(kind, &values).unwrap() else {
                    continue;
                };
                if smallest.as_ref().is_none_or(|best| bytes.len() < best.len()) {
                    smallest = Some(bytes);
                }
            }
            assert_eq!(smallest.as_deref(), Some(chosen.as_slice()), "{}", values.len());
        }
    }

    #[test]
    fn a_column_of_whole_seconds_in_microseconds_pays_nothing_for_the_zeroes() {
        // What three ClickBench columns are. `epoch_ms(EventTime * 1000)` on a source that recorded
        // whole seconds gives microseconds with twenty zero bits under every value, and a frame of
        // reference over a part that spans a working day needs 36 bits to write them down.
        let mut random = Random::new();
        let day = 1_374_000_000_000_000i64;
        let values: Vec<i64> =
            (0..100_000).map(|_| day + (random.next() % 68_400) as i64 * 1_000_000).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Strided);
        assert!(describe(&bytes).unwrap().starts_with("STRIDE[1000000]"), "{:?}", describe(&bytes));
        // 17 bits a value for the range of seconds, against the 36 the microseconds need.
        let strided = 100_000 * 17 / 8;
        assert!(bytes.len() < strided + 2000, "{} bytes for {strided} of payload", bytes.len());

        let plain = encode_only(Kind::Packed, &values).unwrap().expect("packing always applies");
        assert!(
            bytes.len() * 2 < plain.len(),
            "{} strided against {} packed",
            bytes.len(),
            plain.len()
        );
    }

    #[test]
    fn a_stride_is_the_common_factor_of_the_distances_from_the_smallest_value() {
        assert_eq!(stride_of(&[10i64, 20, 40]), Some(10));
        // The base is the smallest value and not zero, so a column that does not start on a
        // multiple of its own step still has one.
        assert_eq!(stride_of(&[7i64, 17, 37]), Some(10));
        assert_eq!(stride_of(&[10i64, 20, 23]), None);
        // Every value the same is `Constant`'s case and this declines it rather than dividing by a
        // stride of zero.
        assert_eq!(stride_of(&[5i64; 100]), None);
        assert_eq!(stride_of(&[]), None);
        // The two ends of the type, where the distance needs 65 bits and only a `u64` holds it.
        assert_eq!(stride_of(&[i64::MIN, i64::MAX]), Some(u64::MAX));
    }

    #[test]
    fn a_stride_across_the_whole_of_the_type_round_trips() {
        // The distance is 65 bits, so the step count is one and the offset it comes back as is a
        // number no `i64` holds. This is the arithmetic the encoder has to do in `u64`.
        for values in [vec![i64::MIN, i64::MAX], vec![i64::MIN, 0, i64::MAX]] {
            let bytes = round_trip(&values);
            assert_eq!(decode(&bytes).unwrap(), values);
        }
    }

    #[test]
    fn a_column_with_no_common_factor_is_not_offered_a_stride() {
        let mut random = Random::new();
        let values: Vec<i64> = (0..2000).map(|_| (random.next() % 1_000_000) as i64).collect();
        assert!(!offered(&values).contains(&Kind::Strided));
        assert!(encode_only(Kind::Strided, &values).unwrap().is_none());
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let bytes = round_trip(&[]);
        assert_eq!(bytes.len(), 5);
    }

    #[test]
    fn a_constant_column_costs_thirteen_bytes_however_long_it_is() {
        let bytes = round_trip(&vec![42; 1_000_000]);
        assert_eq!(kind_of(&bytes), Kind::Constant);
        assert_eq!(bytes.len(), 13);
    }

    #[test]
    fn a_narrow_range_is_packed_at_the_width_of_the_range_and_not_of_the_type() {
        // 100_000 values between 1000 and 1063 is 6 bits each, plus 9 bytes of header per 1024.
        let mut random = Random::new();
        let values: Vec<i64> = (0..100_000).map(|_| 1000 + (random.next() % 64) as i64).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Packed);
        let packed = 100_000 * 6 / 8;
        assert!(bytes.len() < packed + 2000, "{} bytes for {packed} of payload", bytes.len());
        assert!(bytes.len() > packed, "{} bytes cannot hold {packed}", bytes.len());
    }

    #[test]
    fn a_counter_becomes_deltas_and_then_a_constant() {
        // The classic case and the reason DELTA exists. A million consecutive integers is a
        // difference of 1 a million times, which is a constant chunk under the delta.
        let values: Vec<i64> = (0..1_000_000).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Delta);
        assert_eq!(describe(&bytes).unwrap(), "DELTA(CONSTANT)");
        assert!(bytes.len() < 40, "{} bytes for a counter", bytes.len());
    }

    #[test]
    fn a_column_that_counts_down_is_as_cheap_as_one_that_counts_up() {
        // What zigzag is for. Without it every delta is -1, which is 64 bits of ones.
        let up: Vec<i64> = (0..100_000).collect();
        let down: Vec<i64> = (0..100_000).rev().collect();
        assert_eq!(round_trip(&up).len(), round_trip(&down).len());
    }

    #[test]
    fn long_runs_become_rle() {
        let mut values = Vec::new();
        for run in 0..1000 {
            values.extend(std::iter::repeat_n(run % 7, 200));
        }
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Rle);
        assert!(bytes.len() < 2000, "{} bytes for 1000 runs", bytes.len());
    }

    #[test]
    fn a_low_cardinality_column_becomes_a_dictionary() {
        // Values that are far apart so that packing them directly is 30 bits each, and only 40 of
        // them so that the codes are 6 bits each. The dictionary has to win by a factor of five.
        //
        // Drawn at random rather than laid out at a fixed interval, because a fixed interval is a
        // stride and STRIDE writes the same codes without a dictionary to point them at.
        let mut random = Random::new();
        let dictionary: Vec<i64> =
            (0..40).map(|_| 1_000_000_000 + (random.next() % (1 << 30)) as i64).collect();
        let values: Vec<i64> =
            (0..100_000).map(|_| dictionary[(random.next() % 40) as usize]).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Dict);
        assert!(bytes.len() < 100_000, "{} bytes", bytes.len());
    }

    #[test]
    fn a_nearly_constant_column_becomes_sparse() {
        let mut values = vec![0i64; 100_000];
        for index in 0..300 {
            values[index * 331] = 1 << 40;
        }
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Sparse);
        assert!(bytes.len() < 3000, "{} bytes for 300 exceptions", bytes.len());
    }

    #[test]
    fn the_cascade_goes_more_than_one_level_deep() {
        // The whole point of section 6.3. A dictionary over a clustered column produces codes that
        // run in long stretches, and the run lengths of those are themselves compressible.
        let mut values = Vec::new();
        for index in 0..2000i64 {
            values.extend(std::iter::repeat_n(1_000_000 + (index % 5) * 104_729, 100));
        }
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        assert!(shape.contains('('), "{shape} is not a cascade");
        assert!(bytes.len() < 4000, "{} bytes: {shape}", bytes.len());
    }

    #[test]
    fn random_data_is_packed_at_full_width_and_costs_what_it_costs() {
        // The case where nothing works, which has to come out at eight bytes a value plus change
        // rather than at eight bytes a value plus a dictionary of every value in the column.
        let mut random = Random::new();
        let values: Vec<i64> = (0..10_000).map(|_| random.next() as i64).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Packed);
        assert!(bytes.len() < 10_000 * 8 + 1000, "{} bytes", bytes.len());
    }

    #[test]
    fn the_extremes_of_the_type_survive() {
        // Every offset and every delta in here overflows something if the arithmetic is done in 64
        // bits, which is why it is done in 128.
        let values = vec![i64::MIN, i64::MAX, 0, -1, i64::MIN, i64::MAX];
        round_trip(&values);
        round_trip(&[i64::MIN; 3]);
        round_trip(&[i64::MIN, i64::MIN + 1]);
    }

    #[test]
    fn a_chunk_that_is_not_a_multiple_of_the_unit_round_trips() {
        for len in [1, 2, 1023, 1024, 1025, 2047, 2049] {
            let values: Vec<i64> = (0..len).map(|index| (index * 31 % 97) as i64).collect();
            round_trip(&values);
        }
    }

    #[test]
    fn units_of_different_widths_in_one_chunk_do_not_read_each_others_leftovers() {
        // A decode reuses its buffers from one unit to the next instead of getting a zeroed one
        // each time, so a unit that wrote fewer bits than the unit before it would come back with
        // the older unit's values in the bits it did not write. Each run of 1024 here needs a
        // different width and the widths go up and down, and the last run repeats the first, which
        // is the pair that would agree by accident if the reuse were wrong in the obvious way.
        //
        // The values are random rather than written out because this has to stay one packed chunk
        // of six units to be testing anything, and the first version of it was arithmetic and got
        // cascaded into a delta of runs where every nested array was under a unit long. That was
        // caught by gating a panic on the second unit and rerunning, which this version reaches and
        // the old one did not, and the assertion on the shape below is there so it stays reached.
        let mut random = Random::new();
        let mut values = Vec::new();
        for width in [40u32, 3, 61, 1, 17, 40] {
            for _ in 0..1024 {
                values.push((random.next() & ((1u64 << width) - 1)) as i64);
            }
        }
        let bytes = encode(&values).unwrap();
        let described = describe(&bytes).unwrap();
        assert!(described.starts_with("FOR+BITPACK"), "expected one packed chunk, got {described}");
        assert_eq!(decode(&bytes).unwrap(), values, "{described}");
    }

    #[test]
    fn a_cascade_decodes_the_same_through_a_shared_scratch_as_through_its_own() {
        // The scratch is threaded through the recursion, so a dictionary of deltas is three nested
        // decodes sharing one set of buffers. Nothing in the nesting arms holds a buffer across the
        // call it makes, and this is the test that says so: a chunk long enough to cascade and wide
        // enough to bit pack at more than one level, decoded whole.
        let mut values = Vec::new();
        for index in 0..8192i64 {
            values.push(1_600_000_000 + index / 4 + (index % 7) * 1_000);
        }
        let bytes = encode(&values).unwrap();
        let described = describe(&bytes).unwrap();
        assert!(described.contains('('), "expected a cascade, got {described}");
        assert_eq!(decode(&bytes).unwrap(), values, "{described}");
    }

    #[test]
    fn selected_positions_agree_with_a_full_decode_for_packed_and_run_length_chunks() {
        let positions = [0, 1, 17, 1023, 1024, 4097, 8191];
        let packed: Vec<i64> = (0..8192).map(|index| index * 31 % 1_000_003).collect();
        let mut runs = Vec::new();
        for run in 0..160i64 {
            runs.extend(std::iter::repeat_n(run * 13, (run as usize % 71) + 2));
        }
        runs.resize(8192, -7);

        for (kind, values) in [(Kind::Packed, packed), (Kind::Rle, runs)] {
            let bytes = encode_only(kind, &values).unwrap().expect("encoding applies");
            let selected = decode_selected(&bytes, &positions).unwrap();
            let expected = positions.iter().map(|&position| values[position]).collect::<Vec<_>>();
            assert_eq!(selected, expected, "{}", kind.name());
        }
    }

    #[test]
    fn selected_positions_must_be_ordered_and_inside_the_chunk() {
        let bytes = encode_only(Kind::Packed, &(0..2048).collect::<Vec<_>>())
            .unwrap()
            .expect("packed applies");
        assert!(decode_selected(&bytes, &[7, 7]).is_err());
        assert!(decode_selected(&bytes, &[8, 3]).is_err());
        assert!(decode_selected(&bytes, &[2048]).is_err());
    }

    #[test]
    fn a_partial_unit_costs_its_own_values_and_not_a_whole_unit() {
        // Three values that need 40 bits each. In the transposed layout a unit is 1024 values
        // whether it holds them or not, so this would be 5 KB, and every nested array in a cascade
        // is this short. It is 15 bytes of payload and 14 of header.
        let values = vec![1i64 << 39, (1 << 39) + 7, 1 << 38];
        let bytes = encode_only(Kind::Packed, &values).unwrap().unwrap();
        assert_eq!(bytes.len(), 5 + 9 + 15);
        assert_eq!(decode(&bytes).unwrap(), values);
    }

    #[test]
    fn the_frame_of_reference_is_per_unit_and_not_per_chunk() {
        // A column that drifts, which is what a timestamp column and a clustered key both do. Each
        // unit here spans 1023 and packs at 10 bits, and a base per chunk would pay the 22 bits the
        // whole chunk spans on every value in it.
        let values: Vec<i64> =
            (0..4096i64).map(|index| (index / 1024) * 1_000_000 + (index % 1024)).collect();
        let bytes = encode_only(Kind::Packed, &values).unwrap().unwrap();
        assert_eq!(describe(&bytes).unwrap(), "FOR+BITPACK[10]");
        assert_eq!(decode(&bytes).unwrap(), values);
    }

    #[test]
    fn every_candidate_that_applies_decodes_to_the_input() {
        // The chooser only ever hands back the smallest, so without this the other five are only
        // tested when they happen to win. Any of them being wrong is a wrong answer that appears
        // when a column's distribution shifts.
        let mut values = vec![5i64; 3000];
        for (index, value) in values.iter_mut().enumerate() {
            if index % 500 == 0 {
                *value = index as i64;
            }
        }
        let applicable = candidates(&values, 0, &EXHAUSTIVE);
        assert!(applicable.len() >= 4, "{applicable:?}");
        for kind in applicable {
            let bytes = encode_only(kind, &values).unwrap().unwrap();
            assert_eq!(decode(&bytes).unwrap(), values, "{}", kind.name());
        }
    }

    /// The test above only asks the kinds `candidates` offered, so between them the two cover the
    /// encoders on input the search would give them and nothing else. `encode_only` does not go
    /// through `candidates` at all, so every one of its callers can hand an encoder a shape the
    /// filter would have refused, and the empty chunk is the shape that used to panic.
    #[test]
    fn every_kind_that_applies_decodes_to_what_it_was_given() {
        let shapes: Vec<Vec<i64>> = vec![
            Vec::new(),
            vec![5; 1024],
            vec![i64::MIN, i64::MAX, 0, -1],
            (0..1024).map(|at| at * 7).collect(),
            (0..1024).map(|at| at % 17).collect(),
            (0..1024).map(|at| if at % 100 == 0 { at } else { 3 }).collect(),
            (0..1024).map(|at| -at * 1_000_003).collect(),
            (0..1024_i64)
                .map(|at| {
                    at.wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407)
                })
                .collect(),
        ];
        let kinds =
            [Kind::Constant, Kind::Packed, Kind::Delta, Kind::Rle, Kind::Dict, Kind::Sparse];
        for values in &shapes {
            for kind in kinds {
                let Some(bytes) = encode_only(kind, values).unwrap() else {
                    continue;
                };
                assert_eq!(
                    &decode(&bytes).unwrap(),
                    values,
                    "{} over {} values",
                    kind.name(),
                    values.len()
                );
            }
        }
    }

    #[test]
    fn the_chooser_picks_the_smallest_candidate_rather_than_the_first_that_applies() {
        let mut values = vec![5i64; 3000];
        values[1500] = 9;
        let chosen = encode(&values).unwrap();
        for (_, size) in candidate_sizes(&values).unwrap() {
            assert!(chosen.len() <= size);
        }
    }

    #[test]
    fn a_truncated_chunk_is_an_error_and_not_a_panic() {
        let bytes = encode(&[1, 2, 3, 4, 5]).unwrap();
        for len in 0..bytes.len() {
            let error = decode(&bytes[..len]).unwrap_err();
            assert!(error.message().contains("chunk"), "{error}");
        }
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = encode(&[1, 2, 3]).unwrap();
        bytes.push(0);
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("left over"), "{error}");
    }

    #[test]
    fn an_unknown_tag_is_an_error() {
        let error = decode(&[99, 0, 0, 0, 0]).unwrap_err();
        assert!(error.message().contains("unknown encoding tag"), "{error}");
    }

    #[test]
    fn a_dictionary_code_outside_the_dictionary_is_an_error() {
        // A corrupted or malicious chunk must not index out of bounds, and this is the one place in
        // the decoder where a number that came off the disk is used as an index. Built by hand
        // rather than by corrupting a real chunk, because a byte offset into an encoding that the
        // chooser is free to change is a test that breaks for the wrong reason.
        let mut bytes = vec![Kind::Dict.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&encode(&[10]).unwrap());
        bytes.extend_from_slice(&encode(&[5]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("not in the dictionary"), "{error}");
    }

    #[test]
    fn a_negative_run_length_is_an_error() {
        // The other number off the disk that the decoder would otherwise trust, and the one that
        // would turn into an allocation of nine quintillion values.
        let mut bytes = vec![Kind::Rle.tag()];
        put_u32(&mut bytes, 4);
        bytes.extend_from_slice(&encode(&[7]).unwrap());
        bytes.extend_from_slice(&encode(&[-4]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative"), "{error}");
    }

    /// A run that ends past the chunk it is in is an error and not a write past the end.
    #[test]
    fn a_run_that_runs_past_its_chunk_is_an_error() {
        // The decode writes a fixed eight values per run and moves on by the run's own length, so
        // the buffer carries eight values of slack and a run that claims more rows than the chunk
        // holds would be the one way to reach past it. It is refused before the write rather than
        // caught by the count afterwards.
        let mut bytes = vec![Kind::Rle.tag()];
        put_u32(&mut bytes, 4);
        bytes.extend_from_slice(&encode(&[7]).unwrap());
        bytes.extend_from_slice(&encode(&[9]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("past its chunk"), "{error}");
    }

    /// Runs of every length around the eight that a run is written in, in one chunk.
    #[test]
    fn runs_shorter_and_longer_than_the_width_they_are_written_in_all_come_back() {
        // A run of one, several shorter than eight, one of exactly eight and two longer, with the
        // shortest run last so that the surplus of the write before it has nothing after it to be
        // overwritten by. The values differ from each other, because a surplus that was left in
        // place would be invisible against a neighbour holding the same value.
        let lengths = [1, 3, 7, 8, 9, 40, 2, 1];
        let mut values = Vec::new();
        for (at, length) in lengths.iter().enumerate() {
            let value = i64::try_from(at).expect("eight runs") * 1000 - 3;
            values.extend(std::iter::repeat_n(value, *length));
        }
        let bytes = encode(&values).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), values, "runs around the write width");
        // And the same rows a run at a time, which is the run length encoder's worst case and the
        // shape a column with no runs in it decodes as.
        let singles: Vec<i64> = (0..300).map(|index| index * 7 % 11).collect();
        let bytes = encode(&singles).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), singles, "no run longer than one");
    }

    #[test]
    fn the_cascade_depth_is_bounded() {
        // Without the limit a chooser that finds a dictionary of a dictionary of a dictionary would
        // recurse until the values ran out, and the encode time of a wide column would be a
        // surprise rather than a number.
        let values: Vec<i64> = (0..50_000).map(|index| (index / 100) % 250).collect();
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        let depth = shape.matches('(').count();
        assert!(depth <= MAX_DEPTH as usize, "{shape} is {depth} deep");
    }

    #[test]
    fn candidate_sizes_reports_what_the_chooser_looked_at() {
        let values: Vec<i64> = (0..5000).map(|index| index % 17).collect();
        let sizes = candidate_sizes(&values).unwrap();
        assert!(sizes.iter().any(|(kind, _)| *kind == Kind::Dict));
        assert!(sizes.iter().any(|(kind, _)| *kind == Kind::Packed));
        assert!(sizes.iter().all(|(_, size)| *size > 0));
    }

    #[test]
    fn a_chunk_can_be_read_from_the_front_of_a_longer_buffer() {
        // What a string column does. It writes an integer chunk of lengths into the middle of its
        // own body and has to find the end of it again on the way back.
        let first = encode(&[1, 2, 3]).unwrap();
        let second: Vec<i64> = (0..3000).map(|index| index % 11).collect();
        let second_bytes = encode(&second).unwrap();
        let mut joined = first.clone();
        joined.extend_from_slice(&second_bytes);
        joined.extend_from_slice(b"and then something else");

        let (values, used) = decode_prefix(&joined).unwrap();
        assert_eq!(values, vec![1, 2, 3]);
        assert_eq!(used, first.len());
        let (more, used_again) = decode_prefix(&joined[used..]).unwrap();
        assert_eq!(more, second);
        assert_eq!(used_again, second_bytes.len());

        let (text, described) = describe_prefix(&joined).unwrap();
        assert_eq!(described, first.len());
        assert_eq!(text, describe(&first).unwrap());
    }

    #[test]
    fn a_truncated_chunk_is_still_an_error_when_read_as_a_prefix() {
        let bytes = encode(&(0..2000).collect::<Vec<i64>>()).unwrap();
        for len in 0..bytes.len() {
            assert!(decode_prefix(&bytes[..len]).is_err(), "{len} bytes decoded");
        }
    }
}
