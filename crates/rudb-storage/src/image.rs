//! The hot image of a stripe, `engine-v4/10-freezing-and-checkpoint.md` section 10.5 step 3.
//!
//! A checkpoint round writes a hot stripe into the file as its state as of the round's cut S,
//! and only the (column, part) pairs that changed since the stripe's own checkpoint timestamp
//! C_s. The rest of the stripe's image is the pages an older round wrote, which the new root
//! points at again. So an image is a list of parts, each with the slots that hold a row at S, the
//! rows of those deleted at or before S, and the columns of it that changed.
//!
//! [`crate::hot::HotStripe::image`] takes one, [`HotImage::layered`] puts a stripe's images
//! together oldest first the way the newest root names their pages, and
//! [`crate::hot::HotStripe::restore`] makes the hot stripe an open starts from before it replays
//! the log above C_s.

use crate::deletes::{PART_ROWS, PART_WORDS};

/// A hot stripe's state as of a cut, or the part of it that changed since an older one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotImage {
    /// The stripe's id.
    pub stripe: u32,
    /// The cut it is the state as of, which becomes the stripe's C_s once the round publishes it.
    pub at: u64,
    /// Slots handed out when it was taken. A slot past them was never leased.
    pub rows: u32,
    /// The parts that changed, by number.
    pub parts: Vec<PartImage>,
}

/// One part of a [`HotImage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartImage {
    /// The part's number in the stripe.
    pub part: u32,
    /// One bit a slot of the part, set for a slot that holds a row committed at or before the
    /// cut. A slot left clear is a hole, or a row inserted after the cut or not committed, which
    /// the log brings back.
    pub present: Box<[u64; PART_WORDS]>,
    /// The slots of present rows deleted at or before the cut, in the stripe's numbering, sorted.
    pub deleted: Vec<u32>,
    /// The columns of it that changed, by column number, ascending.
    pub columns: Vec<ColumnImage>,
}

/// One column of a [`PartImage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnImage {
    /// The column's number.
    pub column: usize,
    /// One bit a slot, set for a value that is not null.
    pub valid: Box<[u64; PART_WORDS]>,
    /// One value a slot, from the first slot of the part up to the last one handed out.
    pub values: Values,
}

/// The values of a [`ColumnImage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Values {
    /// A fixed-width column's values, zero extended. A null or absent slot is 0.
    Fixed(Vec<u128>),
    /// A text column's strings, one after the other, and where each one ends. A null or absent
    /// slot is empty.
    Text {
        /// The strings' bytes.
        bytes: Vec<u8>,
        /// The end of each slot's string in `bytes`.
        ends: Vec<u64>,
    },
}

impl Values {
    /// How many slots it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Fixed(values) => values.len(),
            Self::Text { ends, .. } => ends.len(),
        }
    }

    /// Whether it holds no slot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes of slot `i` of a text column.
    ///
    /// # Panics
    ///
    /// If the column is not text or `i` is past its end.
    #[must_use]
    pub fn text(&self, i: usize) -> &[u8] {
        let Self::Text { bytes, ends } = self else { panic!("a text column") };
        let start = if i == 0 { 0 } else { ends[i - 1] };
        &bytes[start as usize..ends[i] as usize]
    }
}

impl PartImage {
    /// The slot of the part's first row.
    #[must_use]
    pub fn first(&self) -> u32 {
        self.part * PART_ROWS
    }

    /// Whether slot `i` of the part holds a row.
    #[must_use]
    pub fn is_present(&self, i: usize) -> bool {
        self.present[i / 64] & (1 << (i % 64)) != 0
    }

    /// How many rows it holds, deleted ones included.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.present.iter().map(|word| word.count_ones() as usize).sum()
    }
}

impl ColumnImage {
    /// Whether slot `i` of the part is not null.
    #[must_use]
    pub fn is_valid(&self, i: usize) -> bool {
        self.valid[i / 64] & (1 << (i % 64)) != 0
    }
}

impl HotImage {
    /// The stripe as the newest of `images` names it, oldest first: each part from the newest
    /// image that holds it, with each of its columns from the newest image that changed that
    /// column. The rows and deletes of a part are the newest image's, since an insert into a part
    /// changes every column of it and a delete is in every image of the part taken after it.
    ///
    /// # Panics
    ///
    /// If `images` is empty, or names more than one stripe.
    #[must_use]
    pub fn layered(images: &[Self]) -> Self {
        let (newest, older) = images.split_last().expect("at least one image");
        let mut parts: Vec<PartImage> = Vec::new();
        for image in images {
            assert_eq!(image.stripe, newest.stripe, "images of one stripe");
            for part in &image.parts {
                match parts.iter_mut().find(|held| held.part == part.part) {
                    None => parts.push(part.clone()),
                    Some(held) => {
                        let mut columns = std::mem::take(&mut held.columns);
                        for column in &part.columns {
                            match columns.iter_mut().find(|c| c.column == column.column) {
                                Some(c) => *c = column.clone(),
                                None => columns.push(column.clone()),
                            }
                        }
                        columns.sort_by_key(|column| column.column);
                        *held = PartImage { columns, ..part.clone() };
                    }
                }
            }
        }
        parts.sort_by_key(|part| part.part);
        let rows = older.iter().map(|image| image.rows).fold(newest.rows, u32::max);
        Self { stripe: newest.stripe, at: newest.at, rows, parts }
    }
}
