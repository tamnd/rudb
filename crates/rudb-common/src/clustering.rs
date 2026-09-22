//! What order a table's rows are meant to be stored in, as a declaration the catalog keeps.
//!
//! Stage 1 of `$HOME/notes/Spec/2140/tenx`, written up in `04-the-clustered-layout.md`. The short
//! version is that both we and DuckDB keep a low and a high value per fragment, and on TPC-H in
//! the order `dbgen` writes it neither of us can skip a single fragment, because the dates are
//! scattered and every fragment's range is the whole table's range. Sort the rows first and q6,
//! q14, q15 and q12 come out 6.13x, 3.85x, 3.50x and 2.38x cheaper with no engine change at all.
//!
//! The engine already gets that far on its own: the fragment ranges are built from whatever order
//! the rows arrive in, so `CREATE TABLE ... AS SELECT ... ORDER BY` prunes today. What is missing
//! is that nothing writes the order down. A checkpoint rewrites the file, an insert appends to the
//! end, and the order is gone with nothing having said so. This type is the thing that is written
//! down.
//!
//! It is a declaration and not a measurement. It says what the table is supposed to look like, not
//! what it currently does look like. `spec/storage-v3/13-order-as-a-committed-fact.md` is the other
//! one, and the two want the same column list for different reasons.

use std::fmt;

use crate::error::{Error, Result};

/// How coarsely the leading column is bucketed before the columns after it break the tie.
///
/// Sorting lineitem by `l_shipdate` exactly and sorting it by the month of `l_shipdate` prune the
/// same way for a predicate a month wide or wider, and the second one leaves `l_orderkey` in order
/// inside each bucket, which is what the joins want. That trade is the whole reason this exists
/// rather than the declaration being a plain column list.
///
/// Stage 0 measured the month. `11-the-order.md` section 11.1 asks for the quarter and the year to
/// be measured too, because four queries regressed at a month and nobody yet knows whether that is
/// the lost key locality or something else. The widths are here so that the experiment has
/// something to ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Width {
    /// The value itself, which is an ordinary `ORDER BY` on the column.
    #[default]
    Exact,
    /// The calendar month the value falls in.
    Month,
    /// The calendar quarter the value falls in.
    Quarter,
    /// The calendar year the value falls in.
    Year,
}

impl Width {
    /// The byte this is written as in a native file directory. Never reordered, only appended to.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Month => 1,
            Self::Quarter => 2,
            Self::Year => 3,
        }
    }

    /// The width a directory byte means.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Exact),
            1 => Some(Self::Month),
            2 => Some(Self::Quarter),
            3 => Some(Self::Year),
            _ => None,
        }
    }
}

impl fmt::Display for Width {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::Exact => "EXACT",
            Self::Month => "MONTH",
            Self::Quarter => "QUARTER",
            Self::Year => "YEAR",
        })
    }
}

/// The order a table's rows are meant to be stored in.
///
/// Columns outermost first, by index into the table's column list, with the leading one bucketed
/// at [`Clustering::width`]. The stage 0 layout for lineitem is columns `l_shipdate`, `l_orderkey`,
/// `l_linenumber` at a width of a month, which is exactly the `ORDER BY date_trunc('month',
/// l_shipdate), l_orderkey, l_linenumber` that produced the measured numbers.
///
/// Indexes and not names, because the catalog and the stored directory both already have the column
/// list beside this and a name here would be a second copy that a rename could put out of step. The
/// cost is that this has to be validated against a column count wherever it is built, which
/// [`Clustering::new`] does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Clustering {
    columns: Vec<u32>,
    width: Width,
}

impl Clustering {
    /// A declaration over `columns` of a table that has `width_of_table` columns.
    ///
    /// # Errors
    ///
    /// If the list is empty, names a column the table does not have, or names one twice. All three
    /// are declarations that could be stored and could never be satisfied, and the only place they
    /// can be caught is before they go in.
    pub fn new(columns: Vec<u32>, width: Width, width_of_table: usize) -> Result<Self> {
        if columns.is_empty() {
            return Err(Error::invalid_input("a clustering declaration names no column"));
        }
        for (at, &column) in columns.iter().enumerate() {
            if column as usize >= width_of_table {
                return Err(Error::invalid_input(
                    "a clustering declaration names a column the table does not have",
                ));
            }
            if columns[..at].contains(&column) {
                return Err(Error::invalid_input(
                    "a clustering declaration names the same column twice",
                ));
            }
        }
        Ok(Self { columns, width })
    }

    /// The columns, outermost first, as indexes into the table's column list.
    #[must_use]
    pub fn columns(&self) -> &[u32] {
        &self.columns
    }

    /// How coarsely the leading column is bucketed.
    #[must_use]
    pub fn width(&self) -> Width {
        self.width
    }

    /// The leading column, which is the one the pruning is about.
    #[must_use]
    pub fn partition(&self) -> u32 {
        self.columns[0]
    }

    /// How this reads with the table's column names filled in, for an error or a `SHOW`.
    #[must_use]
    pub fn describe(&self, names: &[String]) -> String {
        let named =
            |at: u32| names.get(at as usize).cloned().unwrap_or_else(|| format!("column {at}"));
        let leading = match self.width {
            Width::Exact => named(self.partition()),
            other => format!("{}({})", other.to_string().to_lowercase(), named(self.partition())),
        };
        let rest = self.columns[1..].iter().map(|&at| named(at)).collect::<Vec<_>>();
        std::iter::once(leading).chain(rest).collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{Clustering, Width};

    #[test]
    fn a_declaration_that_could_never_be_satisfied_is_refused() {
        assert!(Clustering::new(Vec::new(), Width::Exact, 3).is_err(), "no column at all");
        assert!(Clustering::new(vec![3], Width::Exact, 3).is_err(), "past the end of the table");
        assert!(Clustering::new(vec![0, 1, 0], Width::Exact, 3).is_err(), "the same column twice");
        assert!(Clustering::new(vec![2, 0], Width::Month, 3).is_ok());
    }

    #[test]
    fn every_width_survives_its_byte() {
        for width in [Width::Exact, Width::Month, Width::Quarter, Width::Year] {
            assert_eq!(Width::from_tag(width.tag()), Some(width), "{width}");
        }
        assert_eq!(Width::from_tag(4), None, "a tag from a build that knows more than this one");
    }

    #[test]
    fn a_declaration_reads_back_the_way_it_was_written() {
        // The one thing this string is for is that somebody can check the layout is what they
        // asked for without reading a column index against a schema by hand.
        let names = ["l_orderkey", "l_linenumber", "l_shipdate"].map(str::to_string).to_vec();
        let stage_zero = Clustering::new(vec![2, 0, 1], Width::Month, 3).expect("valid");
        assert_eq!(stage_zero.describe(&names), "month(l_shipdate), l_orderkey, l_linenumber");
        let plain = Clustering::new(vec![0], Width::Exact, 3).expect("valid");
        assert_eq!(plain.describe(&names), "l_orderkey");
    }
}
