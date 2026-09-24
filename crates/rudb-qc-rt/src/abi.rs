//! The memory a pipeline function reads: the morsel, the column table it points at, and the state
//! header, per `spec/compiler/05-pipelines-and-state.md` sections 5.3 and 5.5.
//!
//! These are the offsets the generator bakes into loads, so they are constants here and the
//! structs exist to make the layout checkable, not to be read field by field from Rust.

/// One unit of work handed to a pipeline body.
///
/// The first 32 bytes are the spec's morsel. `cols` is the one addition: in the interpreter tier
/// the driver has already decoded the source chunk, so the body reads flat column buffers through
/// this table rather than asking a scan for them.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Morsel {
    /// Which source of the pipeline the rows come from.
    pub source: u32,
    /// The chunk of that source.
    pub chunk: u32,
    /// The first row, relative to the chunk.
    pub begin: u32,
    /// One past the last row.
    pub end: u32,
    /// The global order key.
    pub seq: u64,
    /// The encoding tag of the columns.
    pub enc: u32,
    /// Per morsel facts.
    pub flags: u32,
    /// The column table, one [`Col`] per source column the pipeline reads.
    pub cols: *const Col,
}

/// Where one column of a morsel is.
///
/// `values` holds the column at its physical width, with strings as `str16`. `valid` is a bitmap,
/// least significant bit first, and is never null: a column with no nulls points at a buffer of
/// ones, so the body has one way to read validity and the fast path is a later tier's business.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Col {
    /// The values.
    pub values: *const u8,
    /// The validity bitmap.
    pub valid: *const u8,
}

/// The byte offset of [`Morsel::begin`].
pub const MORSEL_BEGIN: i32 = 8;
/// The byte offset of [`Morsel::end`].
pub const MORSEL_END: i32 = 12;
/// The byte offset of [`Morsel::cols`].
pub const MORSEL_COLS: i32 = 32;
/// The size of a [`Col`].
pub const COL_SIZE: i32 = 16;
/// The byte offset of [`Col::valid`].
pub const COL_VALID: i32 = 8;

/// The size of the state header. Local slots start here.
pub const HEADER: u32 = 64;
/// The header's `cursor` word, where a body that yields records how far it got.
pub const CURSOR: i32 = 24;
/// The header's deferred error word.
pub const ERROR: i32 = 32;
/// The header's `poll_left` countdown.
pub const POLL_LEFT: i32 = 44;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offsets_are_the_layout() {
        assert_eq!(size_of::<Morsel>(), 40);
        assert_eq!(std::mem::offset_of!(Morsel, begin), MORSEL_BEGIN as usize);
        assert_eq!(std::mem::offset_of!(Morsel, end), MORSEL_END as usize);
        assert_eq!(std::mem::offset_of!(Morsel, cols), MORSEL_COLS as usize);
        assert_eq!(size_of::<Col>(), COL_SIZE as usize);
        assert_eq!(std::mem::offset_of!(Col, valid), COL_VALID as usize);
    }
}
