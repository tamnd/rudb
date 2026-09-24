//! The section table: one general mechanism for carrying a graph structure in a rudb file.
//!
//! spec/graph/03-the-file-format.md section 3.2 asks for one mechanism and three section kinds
//! rather than three mechanisms. A section is an opaque payload with a kind, an identity, a
//! generation stamp and a list of extents, and this module is the whole of what the format knows
//! about one. What a key map or a forward link *means* lives in `rudb-graph` at rank 5, which is
//! below the format on purpose: a key map that could see a page would be a key map that could only
//! be tested through a file.
//!
//! Three rules make the mechanism the last one the format needs.
//!
//! A reader ignores a kind it does not know. That is what [`Section::kind`] being eight opaque
//! bytes rather than an enum is for: a build that meets `RUDBAJ1\0` before backward adjacency
//! exists carries the entry through, does not read the payload, and answers the query without it.
//! Section 3.1 guarantees the answer is the same either way, so ignoring is always available and no
//! future section kind needs another format bump.
//!
//! A section is a list of extents of at most [`MAX_EXTENT`] bytes, each independently checksummed
//! and readable. Issue #745 is what this rule is for: a single buffer works until it does not, and
//! an SF100 `lineitem` neighbour array is two gigabytes. Splitting is not an optimization here, it
//! is the difference between a structure that exists at scale and one that does not.
//!
//! Sections are written before the directory and committed by the two-generation header swap the
//! format already performs. So a crash during a section build leaves unreferenced trailing bytes in
//! the file and nothing else, and there is no new recovery path to write or to test.

use rudb_common::{Error, Result};

/// Bytes one section table entry takes on disk.
///
/// Fifty six, per section 3.2, and fixed rather than variable because the entry list is walked at
/// open to decide which sections this build understands and a fixed stride makes that a multiply.
pub(crate) const ENTRY_BYTES: usize = 56;

/// The largest one extent may be.
///
/// Sixty four megabytes. Small enough that a reader can hold one while it checksums it, and large
/// enough that even an SF100 `lineitem` forward link is tens of extents rather than thousands.
pub const MAX_EXTENT: u32 = 64 * 1024 * 1024;

/// The most extents one section may have.
///
/// Sixty four megabytes each, so this bounds a section at a terabyte. The bound exists so that a
/// torn directory naming four billion extents is refused at decode rather than turned into an
/// allocation.
pub const MAX_EXTENTS: u32 = 16 * 1024;

/// A key map, per section 3.3.
pub const KEY_MAP: &[u8; 8] = b"RUDBKM1\0";

/// A forward link column, per section 3.4.
pub const FORWARD_LINK: &[u8; 8] = b"RUDBFL1\0";

/// A backward adjacency list, per section 3.5.
pub const ADJACENCY: &[u8; 8] = b"RUDBAJ1\0";

/// A column summary, per `spec/stats/03-the-file-format.md` section 3.3.
///
/// The first kind here that is not from the graph document, which is the point of the mechanism
/// rather than a complication of it. A statistics section is carried, stamped, split and ignored by
/// exactly the rules above, and adding it took two constants and one arm below.
pub const SUMMARY: &[u8; 8] = b"RUDBCS1\0";

/// A column's sketches, per `spec/stats/03-the-file-format.md` section 3.4.
pub const SKETCHES: &[u8; 8] = b"RUDBSK1\0";

/// A relationship's degree distribution and certificates, per `spec/stats/07-graph-statistics.md`.
///
/// Written by the graph layer, because it comes out of the pass the forward link build is already
/// making, and owned by the statistics document, because nothing in it is needed to resolve a
/// relationship. Its id is the child column, the same as the forward link it describes, so the two
/// are found the same way and a rebuild replaces both.
pub const DEGREES: &[u8; 8] = b"RUDBGD1\0";

/// Rows sorted by one column and covering a second column. The payload holds row values,
/// not grouped counts. A changed table generation makes the section stale.
pub const SORTED_PROJECTION: &[u8; 8] = b"RUDBSP1\0";

/// Row-preserving run encoding of a projection ordered by one signed integer column.
pub const RUN_PROJECTION: &[u8; 8] = b"RUDBRP1\0";

/// The kinds the graph document owns, which share its ten percent of the column bytes.
pub const GRAPH_KINDS: &[&[u8; 8]] = &[KEY_MAP, FORWARD_LINK, ADJACENCY];

/// The kinds the statistics document owns, which share its two percent.
///
/// Ownership here is about which budget pays, not about which builder writes. [`DEGREES`] is
/// written by the link build and is on this list, because it is a planning hint that a reader can
/// drop without losing a relationship, which is the line the two documents are divided along.
///
/// Two lists rather than one because the two budgets are separate, and separate means each counts
/// only what it owns. A statistics build that counted the key maps as already spent would be a
/// statistics budget the graph layer eats: a TPC-H SF10 file's key maps are 7.7 MB against a two
/// percent allowance of 54 MB, so a seventh of the statistics budget would go to sections that have
/// their own.
///
/// A kind in neither list is one a later build wrote, and it counts against neither. There is no
/// better answer available, since this build cannot know which document invented it, and charging
/// it to both would make every budget here tighter than the document says by an amount that depends
/// on what some other build did.
pub const STATISTICS_KINDS: &[&[u8; 8]] = &[SUMMARY, SKETCHES, DEGREES];

/// One entry in a table's section table.
///
/// The payload is not here. This is the entry that says where the payload is, what it is, and
/// whether it is still current, and it is all a reader needs to decide whether to read the payload
/// at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Which kind of structure this is: one of [`KEY_MAP`], [`FORWARD_LINK`], [`ADJACENCY`], or
    /// something a later build wrote that this one carries through untouched.
    pub kind: [u8; 8],
    /// Which structure of that kind. For a key map this identifies the column, for a forward link
    /// the relationship. The format does not interpret it; `rudb-graph` assigns it.
    pub id: u64,
    /// The table generation this section was built against.
    ///
    /// A section whose stamp does not match the table's is stale, and section 3.1 says stale means
    /// ignored rather than repaired. So this field is the whole of the maintenance story: there is
    /// no repair path in this crate because a mismatch here removes the section from consideration
    /// and the query runs the way it ran before the section existed.
    pub generation: u64,
    /// How many extents the payload is split into.
    pub extents: u32,
    /// Where the extent table starts.
    pub extent_page: u64,
    /// How many bytes the extent table takes.
    pub extent_bytes: u32,
    /// Checksum over the extent table, so a torn one is found before it is believed.
    pub hash: u64,
    /// Kind-specific flags. For a key map this carries which of the three forms was chosen, which
    /// is why a reader never has to guess a form.
    pub flags: u32,
    /// Bytes of kind-specific header at the front of the first extent, or, when there are no
    /// extents, what the structure would have cost. See [`Self::refused`].
    pub header_bytes: u32,
}

impl Section {
    /// Appends this entry's fifty six bytes.
    ///
    /// # Errors
    ///
    /// If the entry describes something that cannot exist: more extents than [`MAX_EXTENTS`], or an
    /// extent table larger than one extent. Both are caught here rather than at decode because a
    /// writer that produced one has a bug, and the bug should stop at the write.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.extents > MAX_EXTENTS {
            return Err(malformed(format!(
                "a section of {} extents exceeds the bound of {MAX_EXTENTS}",
                self.extents
            )));
        }
        if self.extent_bytes > MAX_EXTENT {
            return Err(malformed("a section's extent table is larger than one extent"));
        }
        let before = out.len();
        out.extend_from_slice(&self.kind);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.extents.to_le_bytes());
        out.extend_from_slice(&self.extent_page.to_le_bytes());
        out.extend_from_slice(&self.extent_bytes.to_le_bytes());
        out.extend_from_slice(&self.hash.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.header_bytes.to_le_bytes());
        debug_assert_eq!(out.len() - before, ENTRY_BYTES, "a section entry is fifty six bytes");
        Ok(())
    }

    /// Reads one entry from exactly [`ENTRY_BYTES`] bytes.
    ///
    /// # Errors
    ///
    /// If the slice is the wrong length, or if the entry names more extents than [`MAX_EXTENTS`] or
    /// an extent table larger than one extent. A bad entry is an error and not a panic because the
    /// caller's answer to one is to drop the section and open the table anyway.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != ENTRY_BYTES {
            return Err(malformed("a section entry is not fifty six bytes"));
        }
        let section = Self {
            kind: bytes[0..8].try_into().expect("eight bytes"),
            id: u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes")),
            generation: u64::from_le_bytes(bytes[16..24].try_into().expect("eight bytes")),
            extents: u32::from_le_bytes(bytes[24..28].try_into().expect("four bytes")),
            extent_page: u64::from_le_bytes(bytes[28..36].try_into().expect("eight bytes")),
            extent_bytes: u32::from_le_bytes(bytes[36..40].try_into().expect("four bytes")),
            hash: u64::from_le_bytes(bytes[40..48].try_into().expect("eight bytes")),
            flags: u32::from_le_bytes(bytes[48..52].try_into().expect("four bytes")),
            header_bytes: u32::from_le_bytes(bytes[52..56].try_into().expect("four bytes")),
        };
        if section.extents > MAX_EXTENTS {
            return Err(malformed("a section names more extents than the bound allows"));
        }
        if section.extent_bytes > MAX_EXTENT {
            return Err(malformed("a section's extent table is larger than one extent"));
        }
        Ok(section)
    }

    /// Whether this build understands this section's kind.
    ///
    /// The five it knows are the three the graph document's section 3.2 names and the two the
    /// statistics document's sections 3.3 and 3.4 name. Everything else is a section a later build
    /// wrote, and the answer is to leave it alone: the entry is carried through a rewrite so that
    /// opening a file with an old build and closing it does not silently discard work, and the
    /// payload is never read.
    #[must_use]
    pub fn known(&self) -> bool {
        matches!(
            &self.kind,
            KEY_MAP
                | FORWARD_LINK
                | ADJACENCY
                | SUMMARY
                | SKETCHES
                | DEGREES
                | SORTED_PROJECTION
                | RUN_PROJECTION
        )
    }

    /// Whether this section's kind is one of these, which is how a budget finds what it owns.
    #[must_use]
    pub fn among(&self, kinds: &[&[u8; 8]]) -> bool {
        kinds.iter().any(|kind| self.kind == **kind)
    }

    /// Whether this section was built against this table generation.
    #[must_use]
    pub fn current(&self, generation: u64) -> bool {
        self.generation == generation
    }

    /// Whether this section is one this build should read: a kind it knows, at the current
    /// generation.
    #[must_use]
    pub fn usable(&self, generation: u64) -> bool {
        self.known() && self.current(generation)
    }

    /// What this structure would have cost, when the entry is a record of one that did not fit.
    ///
    /// Section 3.7 asks for a relationship that did not fit the budget to be recorded with its size
    /// rather than forgotten, so that raising `graph_budget` is a decision somebody can make from a
    /// number. An entry with no extents is that record, and the number is in [`Self::header_bytes`],
    /// which has nothing else to mean when there is no first extent to have a header at the front
    /// of. [`Self::flags`] keeps the meaning it has for a built section of the same kind, so a
    /// record says which form the structure would have taken as well as what it would have cost.
    ///
    /// `None` for a section that is in the file, which is the ordinary case and is the one where
    /// the size is the payload's own length.
    ///
    /// A size past four gigabytes saturates, because the field is a `u32`. The largest structure
    /// this project expects to refuse is a packed forward link over an SF100 `lineitem`, which is
    /// about 2.1 GB, so the saturation is a bound rather than a rounding, and a saturated record
    /// still says *far more than the budget* correctly.
    #[must_use]
    pub fn refused(&self) -> Option<u64> {
        (self.extents == 0).then(|| u64::from(self.header_bytes))
    }
}

/// One section to be written into a file, handed to [`crate::attach`].
///
/// The payload is bytes and the format keeps it that way. Which of the three key map forms is in
/// `flags`, and what the first `header_bytes` bytes mean, are questions `rudb-graph` answers and
/// this crate never asks, which is what makes the first of section 3.2's three rules true rather
/// than intended: a mechanism that had to understand a payload could not carry one it had never
/// heard of.
#[derive(Debug, Clone, Copy)]
pub struct Attachment<'a> {
    /// Which kind of structure this is, usually one of [`KEY_MAP`], [`FORWARD_LINK`],
    /// [`ADJACENCY`].
    pub kind: [u8; 8],
    /// Which structure of that kind. An attachment replaces any section already in the table with
    /// the same kind and id, which is what makes rebuilding a key map a write rather than a
    /// question about what to do with the old one.
    pub id: u64,
    /// Kind-specific flags, copied into the entry and not interpreted.
    pub flags: u32,
    /// How many bytes at the front of `bytes` are the kind's own header.
    pub header_bytes: u32,
    /// The payload. Empty is legal and is how section 3.7 records a relationship that did not fit
    /// the budget: an entry with no extents, its size reported by `rudb_links()`, and nothing in
    /// the file to read.
    pub bytes: &'a [u8],
}

/// Where one extent of a section's payload lives.
///
/// Each carries its own checksum, which is the second of section 3.2's three rules: an extent is
/// independently readable, so a reduction that only needs the third extent of a forward link reads
/// and verifies one extent rather than two gigabytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// Where the extent's bytes start.
    pub offset: u64,
    /// How many bytes it holds, at most [`MAX_EXTENT`].
    pub length: u32,
    /// Checksum over those bytes.
    pub hash: u64,
    /// How many logical elements precede this extent, so that a random access can find the extent
    /// holding an element without reading any of them.
    pub first: u64,
}

/// Bytes one extent entry takes in an extent table.
pub const EXTENT_BYTES: usize = 28;

impl Extent {
    /// Appends this extent's twenty eight bytes.
    ///
    /// # Errors
    ///
    /// If the extent is larger than [`MAX_EXTENT`], which is the rule the split exists to keep.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.length > MAX_EXTENT {
            return Err(malformed(format!(
                "an extent of {} bytes exceeds the maximum of {MAX_EXTENT}",
                self.length
            )));
        }
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.length.to_le_bytes());
        out.extend_from_slice(&self.hash.to_le_bytes());
        out.extend_from_slice(&self.first.to_le_bytes());
        Ok(())
    }

    /// Reads one extent from exactly [`EXTENT_BYTES`] bytes.
    ///
    /// # Errors
    ///
    /// If the slice is the wrong length or the extent is oversized.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != EXTENT_BYTES {
            return Err(malformed("an extent entry is not twenty eight bytes"));
        }
        let extent = Self {
            offset: u64::from_le_bytes(bytes[0..8].try_into().expect("eight bytes")),
            length: u32::from_le_bytes(bytes[8..12].try_into().expect("four bytes")),
            hash: u64::from_le_bytes(bytes[12..20].try_into().expect("eight bytes")),
            first: u64::from_le_bytes(bytes[20..28].try_into().expect("eight bytes")),
        };
        if extent.length > MAX_EXTENT {
            return Err(malformed("an extent is larger than the maximum extent"));
        }
        Ok(extent)
    }
}

/// Encodes a whole extent table, checking that it describes a contiguous run of elements.
///
/// # Errors
///
/// If an extent is oversized, if the `first` counts are not increasing, or if there are more
/// extents than [`MAX_EXTENTS`]. The increasing check is what makes a binary search over the table
/// meaningful, and an unchecked one would be a search that silently returned the wrong extent.
pub fn encode_extents(extents: &[Extent], out: &mut Vec<u8>) -> Result<()> {
    if extents.len() > MAX_EXTENTS as usize {
        return Err(malformed("a section names more extents than the bound allows"));
    }
    for (at, extent) in extents.iter().enumerate() {
        if at == 0 {
            if extent.first != 0 {
                return Err(malformed("a section's first extent does not start at element zero"));
            }
        } else if extent.first <= extents[at - 1].first {
            return Err(malformed("a section's extents are not in element order"));
        }
        extent.encode(out)?;
    }
    Ok(())
}

/// Decodes a whole extent table.
///
/// # Errors
///
/// If the byte count is not a multiple of an entry, if an entry is malformed, or if the entries are
/// not in element order.
pub fn decode_extents(bytes: &[u8]) -> Result<Vec<Extent>> {
    if bytes.len() % EXTENT_BYTES != 0 {
        return Err(malformed("an extent table is not a whole number of entries"));
    }
    let mut extents: Vec<Extent> = Vec::with_capacity(bytes.len() / EXTENT_BYTES);
    for chunk in bytes.chunks(EXTENT_BYTES) {
        let extent = Extent::decode(chunk)?;
        match extents.last() {
            None if extent.first != 0 => {
                return Err(malformed("a section's first extent does not start at element zero"));
            }
            Some(previous) if extent.first <= previous.first => {
                return Err(malformed("a section's extents are not in element order"));
            }
            _ => {}
        }
        extents.push(extent);
    }
    Ok(extents)
}

/// Which extent holds a given logical element, by binary search over the table.
///
/// Returns the index into `extents` and the element's offset within that extent's elements, or
/// `None` when there are no extents at all, which is the not-built entry of section 3.7.
///
/// It does not bound the element from above, because an extent table cannot: the last extent's
/// length is in bytes and only the caller knows how many elements a byte holds. So an element past
/// the end answers with an offset past the end of the last extent, and the caller checks that
/// against the count it already has. `None` rather than an error for the empty case because a
/// stale link may name a structure that is no longer there, and section 3.1 wants staleness
/// ignored.
#[must_use]
pub fn locate(extents: &[Extent], element: u64) -> Option<(usize, u64)> {
    let at = extents.partition_point(|extent| extent.first <= element);
    if at == 0 {
        return None;
    }
    Some((at - 1, element - extents[at - 1].first))
}

fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb section table: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Section {
        Section {
            kind: *KEY_MAP,
            id: 7,
            generation: 42,
            extents: 3,
            extent_page: 1 << 20,
            extent_bytes: 84,
            hash: 0xdead_beef_cafe_f00d,
            flags: 2,
            header_bytes: 24,
        }
    }

    #[test]
    fn an_entry_takes_fifty_six_bytes_and_round_trips() {
        let mut bytes = Vec::new();
        entry().encode(&mut bytes).expect("encode");
        assert_eq!(bytes.len(), ENTRY_BYTES, "section 3.2 says fifty six");
        assert_eq!(Section::decode(&bytes).expect("decode"), entry());
    }

    #[test]
    fn an_unknown_kind_is_carried_and_not_read() {
        // The rule that makes this the last bump the mechanism needs. A build that met this entry
        // before the kind existed has to be able to hold it, report it as not understood, and open
        // the table anyway.
        let mut unknown = entry();
        unknown.kind = *b"RUDBZZ9\0";
        let mut bytes = Vec::new();
        unknown.encode(&mut bytes).expect("an unknown kind still encodes");
        let read = Section::decode(&bytes).expect("an unknown kind still decodes");
        assert_eq!(read, unknown, "the entry survives a build that does not know it");
        assert!(!read.known());
        assert!(!read.usable(42), "a kind this build does not know is never read");
    }

    #[test]
    fn the_kinds_the_two_documents_name_are_known() {
        for kind in [KEY_MAP, FORWARD_LINK, ADJACENCY, SUMMARY, SKETCHES] {
            let mut section = entry();
            section.kind = *kind;
            assert!(section.known(), "{}", String::from_utf8_lossy(kind));
        }
    }

    #[test]
    fn no_two_kinds_share_a_tag() {
        // Worth a test now that two documents assign them. A collision would mean one kind's payload
        // read by the other's decoder, which is the one thing an opaque payload cannot defend
        // against by itself.
        let all = [KEY_MAP, FORWARD_LINK, ADJACENCY, SUMMARY, SKETCHES];
        for (at, one) in all.iter().enumerate() {
            for other in &all[at + 1..] {
                assert_ne!(one, other, "{}", String::from_utf8_lossy(*one));
            }
        }
    }

    #[test]
    fn a_stale_section_is_ignored_rather_than_repaired() {
        // Section 3.1's staleness rule, which is the whole of the maintenance story: the generation
        // stamp not matching removes the section from consideration, and there is no third state
        // between usable and ignored for a repair path to live in.
        let section = entry();
        assert!(section.usable(42));
        assert!(!section.usable(43), "a rewrite invalidates rather than corrupts");
        assert!(section.known(), "staleness is not the same question as familiarity");
    }

    #[test]
    fn an_entry_naming_more_extents_than_the_bound_is_refused_at_both_ends() {
        let mut oversized = entry();
        oversized.extents = MAX_EXTENTS + 1;
        assert!(oversized.encode(&mut Vec::new()).is_err(), "a writer's bug stops at the write");

        let mut bytes = Vec::new();
        entry().encode(&mut bytes).expect("encode");
        bytes[24..28].copy_from_slice(&(MAX_EXTENTS + 1).to_le_bytes());
        assert!(Section::decode(&bytes).is_err(), "a torn count is not turned into an allocation");
    }

    #[test]
    fn a_short_entry_is_refused_rather_than_read_past() {
        let mut bytes = Vec::new();
        entry().encode(&mut bytes).expect("encode");
        bytes.pop();
        assert!(Section::decode(&bytes).is_err());
        assert!(Section::decode(&[]).is_err());
    }

    #[test]
    fn an_extent_at_the_maximum_is_allowed_and_one_past_it_is_not() {
        // The bound is the point of the split, so the boundary is the case worth pinning: sixty
        // four megabytes exactly has to work, because a payload that is a multiple of it would
        // otherwise be unwritable.
        let at_bound = Extent { offset: 4096, length: MAX_EXTENT, hash: 9, first: 0 };
        let mut bytes = Vec::new();
        at_bound.encode(&mut bytes).expect("an extent at the bound encodes");
        assert_eq!(bytes.len(), EXTENT_BYTES);
        assert_eq!(Extent::decode(&bytes).expect("decode"), at_bound);

        let past = Extent { offset: 4096, length: MAX_EXTENT + 1, hash: 9, first: 0 };
        assert!(past.encode(&mut Vec::new()).is_err());
    }

    fn table() -> Vec<Extent> {
        vec![
            Extent { offset: 1024, length: MAX_EXTENT, hash: 1, first: 0 },
            Extent {
                offset: 1024 + u64::from(MAX_EXTENT),
                length: MAX_EXTENT,
                hash: 2,
                first: 100,
            },
            Extent { offset: 1024 + 2 * u64::from(MAX_EXTENT), length: 512, hash: 3, first: 250 },
        ]
    }

    #[test]
    fn an_extent_table_round_trips() {
        let mut bytes = Vec::new();
        encode_extents(&table(), &mut bytes).expect("encode");
        assert_eq!(bytes.len(), 3 * EXTENT_BYTES);
        assert_eq!(decode_extents(&bytes).expect("decode"), table());
    }

    #[test]
    fn an_extent_table_out_of_element_order_is_refused() {
        // The order is what makes the binary search in `locate` mean anything, so an unordered
        // table has to be refused rather than searched: a search over one would return a plausible
        // extent holding the wrong elements.
        let mut out_of_order = table();
        out_of_order.swap(1, 2);
        assert!(encode_extents(&out_of_order, &mut Vec::new()).is_err());

        let mut bytes = Vec::new();
        encode_extents(&table(), &mut bytes).expect("encode");
        bytes[EXTENT_BYTES + 20..EXTENT_BYTES + 28].copy_from_slice(&0_u64.to_le_bytes());
        assert!(decode_extents(&bytes).is_err(), "a torn element order is refused");
    }

    #[test]
    fn an_extent_table_not_starting_at_element_zero_is_refused() {
        let mut shifted = table();
        shifted[0].first = 1;
        assert!(encode_extents(&shifted, &mut Vec::new()).is_err());
    }

    #[test]
    fn a_partial_extent_table_is_refused_rather_than_truncated() {
        let mut bytes = Vec::new();
        encode_extents(&table(), &mut bytes).expect("encode");
        bytes.truncate(bytes.len() - 1);
        assert!(decode_extents(&bytes).is_err());
    }

    #[test]
    fn an_empty_extent_table_is_a_section_with_no_payload() {
        // A relationship recorded as not built, per section 3.7, is an entry with no extents. It
        // has to be legal, because that is how `rudb_links()` reports what a larger budget would
        // buy.
        let mut bytes = Vec::new();
        encode_extents(&[] as &[Extent], &mut bytes).expect("encode");
        assert!(bytes.is_empty());
        assert!(decode_extents(&bytes).expect("decode").is_empty());
        assert_eq!(locate(&[], 0), None);
    }

    #[test]
    fn an_element_resolves_to_the_extent_holding_it() {
        let extents = table();
        assert_eq!(locate(&extents, 0), Some((0, 0)));
        assert_eq!(locate(&extents, 99), Some((0, 99)));
        assert_eq!(locate(&extents, 100), Some((1, 0)), "the first element of the second extent");
        assert_eq!(locate(&extents, 249), Some((1, 149)));
        assert_eq!(locate(&extents, 250), Some((2, 0)));
        assert_eq!(locate(&extents, 1_000_000), Some((2, 999_750)), "past the end of the elements");
    }

    #[test]
    fn a_two_gigabyte_payload_is_tens_of_extents_and_not_one_buffer() {
        // The arithmetic issue #745 is about, and the reason the split is a rule rather than an
        // option. An SF100 lineitem forward link is 600,037,902 rows at 28 bits, which is 2.10 GB,
        // and no reader should be asked to hold that in one buffer to checksum it.
        let payload = 600_037_902_u64 * 28 / 8;
        let extents = payload.div_ceil(u64::from(MAX_EXTENT));
        assert!(extents > 30, "{extents} extents");
        assert!(extents < u64::from(MAX_EXTENTS), "{extents} extents is inside the bound");
    }
}
