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
use crate::types::{Field, LogicalType};

/// How coarsely the leading column is bucketed before the columns after it break the tie.
///
/// Sorting lineitem by `l_shipdate` exactly and sorting it by the month of `l_shipdate` prune the
/// same way for a predicate a month wide or wider, and the second one leaves `l_orderkey` in order
/// inside each bucket, which is what the joins want. That trade is the whole reason this exists
/// rather than the declaration being a plain column list.
///
/// All four are measured, in `14-the-partition-width.md`. The month is the worst of them: five SF1
/// files built by one loader in one sitting come out at 0.891 of the unsorted file's instructions
/// sorted exactly, 0.898 at a quarter, 0.899 at a year and 0.917 at a month, because the narrower
/// the bucket the more often a join key's hash entry is revisited and the wider the delta the sort
/// key encodes to, while the pruning a narrow bucket buys stops mattering above a quarter. So a
/// declaration that names no width gets [`Width::Auto`], which picks from the data and lands on the
/// quarter at the scale that table was measured at. Note how little separates the middle three on
/// that suite and how much separates them on the queries with a narrow date predicate in them,
/// which is why the rule that picks is written the way it is.
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
    /// The month or the quarter, whichever the data asks for, decided when the rows are in front
    /// of us.
    ///
    /// This is a declaration and not a bucket, which is the whole of the difference. Nothing sorts
    /// by it and nothing writes a sort key from it: [`Width::for_span`] turns it into a real one at
    /// the load, with the row count and the column's range in hand, and it is that answer the rows
    /// are put in order by. A declaration that still says this after a load is a table saying it
    /// wants whatever fits, not a table saying its rows are in no order.
    ///
    /// Only two of the four are reachable this way. The exact width and the year both have to be
    /// asked for by name, for reasons [`Width::for_span`] gives.
    Auto,
}

impl Width {
    /// The bucket a date or a timestamp gets when nobody says which and nothing can be counted.
    ///
    /// Not [`Default::default`], which stays the exact value: that one is the width of a column
    /// with no calendar in it, and a struct deriving `Default` has no column to look at. This is
    /// the fallback for [`Width::for_span`] when the row count or the range is not there to read,
    /// and it is the quarter because that is what SF1 measured at, in `14-the-partition-width.md`.
    pub const DEFAULT: Self = Self::Quarter;

    /// How many rows a partition should hold.
    ///
    /// The number the whole of [`Width::for_span`] turns on, and the one thing in this file that is
    /// a constant fitted to a measurement rather than a fact. Section 14.6 of
    /// `14-the-partition-width.md` says a partition of a few hundred thousand rows is the shape
    /// that measured well, and this is the bottom of that range.
    ///
    /// Two hundred thousand because of what it has to separate. SF1 lineitem is six million rows
    /// over about seven years, so a month there is seventy thousand rows a partition and a quarter
    /// is two hundred and fourteen thousand, and the quarter is the width that suite measured best.
    /// Any target between those two numbers reproduces that answer. SF10 lineitem is ten times the
    /// rows over the same seven years, so a month there is seven hundred thousand, which clears the
    /// target comfortably, and that is the case the rule exists for.
    pub const TARGET: u64 = 200_000;

    /// The width for a table of `rows` rows whose leading column runs across `days`.
    ///
    /// The month when a month's worth of rows clears [`Width::TARGET`] and the quarter otherwise.
    /// Two candidates and not four, and which two is the part that was measured rather than
    /// reasoned.
    ///
    /// The month is what the row count is for. A narrow partition prunes better and costs locality:
    /// the more partitions the leading column is cut into, the more often a join key's hash entry is
    /// revisited and the wider the deltas the sort key encodes to. Section 14.5 measured both sides
    /// and the crossing point is a partition size rather than a calendar unit, which is the entire
    /// reason this is a function and not a constant. At SF1 a month is under the target and loses,
    /// at SF10 it is three times over it and the same calendar word is a different proposition.
    ///
    /// The year is not a candidate, and the first cut of this rule had it as one. It measured 0.899
    /// of the unsorted file at SF1 against the quarter's 0.898, so it was never winning anything,
    /// and letting it in cost real numbers: with the year available the rule cut `orders` yearly at
    /// SF1, because orders is a quarter of lineitem's size and falls under the target at every
    /// calendar width, and q4 went from 0.879 to 0.914, q10 from 0.825 to 0.841 and q3 from 0.686
    /// to 0.701. Those three read `o_orderdate` through a predicate a quarter wide, and no
    /// partition wider than the predicate can prune inside it however many rows it holds. That is
    /// the limit of the row count as a rule: it says how fine to cut before locality starts costing
    /// and it says nothing about how coarse is too coarse, because that end is set by the width of
    /// the predicates and nothing at load time knows those.
    ///
    /// The exact width is not a candidate either, for a different reason. It measured two tenths of
    /// a percent better than the quarter on SF1 and it is still the wrong answer, because it throws
    /// away the run length layer on the join key and costs eight times the bytes on `l_orderkey`,
    /// which loses on any workload without TPC-H's date predicates in it. Somebody who wants either
    /// of the two can write `exact(...)` or `year(...)` and get it, and this is about what to do
    /// when nobody said.
    ///
    /// `days` is the span of the column and not the number of distinct values in it, because what
    /// matters is how many buckets the range is cut into. A table whose dates are seven years apart
    /// and has three of them still has seven years of buckets to write down.
    ///
    /// A row count or a span of zero gets [`Width::DEFAULT`]. There is nothing to divide and an
    /// empty table has no shape to fit.
    #[must_use]
    pub fn for_span(rows: u64, days: u64) -> Self {
        if rows == 0 || days == 0 {
            return Self::DEFAULT;
        }
        // Thirty days, which is all the arithmetic needs: the answer is how many partitions a range
        // is cut into and a long month either way cannot move that across the target.
        let partitions = days / 30 + 1;
        if rows / partitions >= Self::TARGET { Self::Month } else { Self::Quarter }
    }

    /// The byte this is written as in a native file directory. Never reordered, only appended to.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Month => 1,
            Self::Quarter => 2,
            Self::Year => 3,
            Self::Auto => 4,
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
            4 => Some(Self::Auto),
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
            Self::Auto => "AUTO",
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
    /// A declaration over `columns` of a table whose columns are `fields`.
    ///
    /// # Errors
    ///
    /// If the list is empty, names a column the table does not have, or names one twice. All three
    /// are declarations that could be stored and could never be satisfied, and the only place they
    /// can be caught is before they go in.
    ///
    /// And if a width other than [`Width::Exact`] lands on a column that is not a date or a
    /// timestamp. The width is a calendar bucket and there is no calendar in an integer, so the
    /// loader would have nothing to sort by. Checked here rather than at the load, because a
    /// declaration is stored once and read every time the table is written.
    pub fn new(columns: Vec<u32>, width: Width, fields: &[Field]) -> Result<Self> {
        if columns.is_empty() {
            return Err(Error::invalid_input("a clustering declaration names no column"));
        }
        for (at, &column) in columns.iter().enumerate() {
            if column as usize >= fields.len() {
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
        let leading = &fields[columns[0] as usize];
        if width != Width::Exact
            && !matches!(leading.ty, LogicalType::Date | LogicalType::Timestamp)
        {
            return Err(Error::invalid_input(format!(
                "a clustering declaration buckets {} by {width}, which only a date or a timestamp \
                 has",
                leading.name
            )));
        }
        Ok(Self { columns, width })
    }

    /// A declaration over `columns` that leaves the width to the leading column's type.
    ///
    /// A date or a timestamp gets [`Width::Auto`] and anything else is taken exactly, which is the
    /// only width an integer or a string has. This is what a loader clustering a table it was
    /// handed should call, since the alternative is every caller writing the same two line match
    /// and the constant living in as many places as there are callers.
    ///
    /// [`Width::Auto`] and not a fixed bucket because a fixed bucket is a constant fitted at one
    /// scale and applied at every other. A quarter of SF1 lineitem is a quarter of a million rows
    /// and a quarter of SF100 is twenty four million, and there is no reason the second one lands
    /// anywhere near the first on the curve section 14.5 measured. [`Clustering::fitted`] is where
    /// the declaration meets the row count and turns into a bucket.
    ///
    /// # Errors
    ///
    /// The ones [`Clustering::new`] gives, which this is checked by. The width it picks is legal
    /// for the column it picked it for, so the type error is not one of them.
    pub fn over(columns: Vec<u32>, fields: &[Field]) -> Result<Self> {
        let leading = columns.first().and_then(|&at| fields.get(at as usize));
        let width = match leading.map(|field| &field.ty) {
            Some(LogicalType::Date | LogicalType::Timestamp) => Width::Auto,
            // Including the column list that is empty or out of range, which has no type to look
            // at and is about to be refused for that rather than for its width.
            _ => Width::Exact,
        };
        Self::new(columns, width, fields)
    }

    /// This declaration with [`Width::Auto`] turned into the bucket `rows` and `days` ask for.
    ///
    /// The one place an automatic width becomes a real one, and the reason it is a method rather
    /// than something the loader does inline: the sort key is built from the width, so a width of
    /// [`Width::Auto`] reaching the sort would be a `date_trunc` by a unit no calendar has. Whoever
    /// is about to sort calls this first and what comes back can be sorted by.
    ///
    /// A width somebody wrote down is left exactly as they wrote it. `exact(l_shipdate)` stays
    /// exact on a table of any size, because the declaration is what the table is asked to be and
    /// a loader quietly widening it would make the setting a suggestion.
    ///
    /// `rows` is how many rows are about to be written and `days` is the span of the leading column
    /// across them, both as well as the caller can tell. Neither has to be right: they pick between
    /// three layouts that hold the same rows and answer the same queries, so being wrong costs some
    /// pruning or some locality and cannot cost an answer. A caller that cannot tell at all passes
    /// zero and gets [`Width::DEFAULT`].
    #[must_use]
    pub fn fitted(&self, rows: u64, days: u64) -> Self {
        match self.width {
            Width::Auto => {
                Self { columns: self.columns.clone(), width: Width::for_span(rows, days) }
            }
            _ => self.clone(),
        }
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

/// A declaration as somebody wrote it, before a catalog turned the names into column indexes.
///
/// [`Clustering`] holds indexes, which means it cannot be built without the table in hand, and the
/// text is typed in a session that may name a table this database does not have. So the parse
/// produces this and whoever has the catalog turns it into the real thing, which is also where the
/// name errors come from and where they can say which table they are about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    table: String,
    width: Option<Width>,
    columns: Vec<String>,
}

impl Declared {
    /// The table the declaration is about, as it was written.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The width the text named, or `None` when it named none and the column's type decides.
    #[must_use]
    pub fn width(&self) -> Option<Width> {
        self.width
    }

    /// The columns, outermost first, as they were written.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// Parses the `cluster_by` session setting.
///
/// The grammar is a comma separated list of `table(column, column, ...)`, with the leading column
/// optionally wrapped in the width it is bucketed at: `lineitem(quarter(l_shipdate), l_orderkey)`.
/// That is what [`Clustering::describe`] prints with the table name put in front of it, so a
/// declaration read back out of a table can be pasted straight back into the setting.
///
/// A leading column with no wrapper around it leaves the width to the column's type, which is
/// [`Clustering::over`], so `lineitem(l_shipdate, l_orderkey)` gets [`Width::Auto`] and
/// `orders(o_orderkey)` gets the exact value. Whitespace between tokens is free and a trailing
/// comma is allowed, for the reason the relationship grammar allows one: a setting long enough to
/// want a line per table is a setting somebody will edit.
///
/// The two tables the stage 0 measurement clusters, in this grammar:
///
/// ```text
/// lineitem(quarter(l_shipdate), l_orderkey, l_linenumber),
/// orders(quarter(o_orderdate), o_orderkey)
/// ```
///
/// # Errors
///
/// If an entry is malformed. Nothing here can say whether a table or a column exists, since there
/// is no catalog at this layer, so those are the caller's errors and this one's are about shape.
pub fn parse_clustering(setting: &str) -> Result<Vec<Declared>> {
    let mut declared = Vec::new();
    for entry in entries(setting) {
        declared.push(parse_entry(&entry)?);
    }
    Ok(declared)
}

/// The setting cut at the commas that separate tables, leaving the ones inside a column list.
///
/// The one real ambiguity in the grammar, the same one the relationship grammar has: a comma
/// separates two declarations and also separates two columns of the same one. Depth tells them
/// apart, and the width wrapper means the depth goes to two rather than one.
fn entries(setting: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for character in setting.chars() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            // A comma outside every parenthesis ends a declaration. One inside a list belongs to
            // the list, and a closing parenthesis with nothing open is left for the parse below to
            // complain about rather than being treated as a separator.
            ',' if depth == 0 => {
                entries.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    entries.push(current);
    entries
        .into_iter()
        .map(|entry| entry.trim().to_owned())
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn parse_entry(entry: &str) -> Result<Declared> {
    let Some((table, rest)) = entry.split_once('(') else {
        return Err(malformed(format!("expected `table(column, ...)` and found `{entry}`")));
    };
    let Some(inside) = rest.trim_end().strip_suffix(')') else {
        return Err(malformed(format!("`{entry}` is missing its closing parenthesis")));
    };
    let table = table.trim();
    if table.is_empty() {
        return Err(malformed(format!("`{entry}` names no table")));
    }
    let mut columns = Vec::new();
    for column in inside.split(',').map(str::trim).filter(|column| !column.is_empty()) {
        columns.push(column.to_owned());
    }
    if columns.is_empty() {
        return Err(malformed(format!("`{entry}` names no column")));
    }
    // The width rides on the leading column and nowhere else, since it is the column the bucketing
    // is about, so a wrapper anywhere after the first is a declaration nobody can honour.
    let (width, leading) = split_width(&columns[0])?;
    for column in &columns[1..] {
        if column.contains('(') {
            return Err(malformed(format!(
                "`{column}` is bucketed and only the leading column of `{table}` can be"
            )));
        }
    }
    columns[0] = leading;
    Ok(Declared { table: table.to_owned(), width, columns })
}

/// A leading column as the width it was wrapped in, if it was wrapped, and the column itself.
fn split_width(leading: &str) -> Result<(Option<Width>, String)> {
    let Some((word, rest)) = leading.split_once('(') else {
        return Ok((None, leading.to_owned()));
    };
    let Some(column) = rest.trim_end().strip_suffix(')') else {
        return Err(malformed(format!("`{leading}` is missing its closing parenthesis")));
    };
    let column = column.trim();
    if column.is_empty() {
        return Err(malformed(format!("`{leading}` names no column")));
    }
    let word = word.trim();
    let width = [Width::Exact, Width::Month, Width::Quarter, Width::Year, Width::Auto]
        .into_iter()
        .find(|width| width.to_string().eq_ignore_ascii_case(word))
        .ok_or_else(|| {
            malformed(format!(
                "`{word}` is not a partition width, which is one of exact, month, quarter, year or \
                 auto"
            ))
        })?;
    Ok((Some(width), column.to_owned()))
}

fn malformed(message: impl Into<String>) -> Error {
    Error::invalid_input(format!("invalid rudb clustering: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::{Clustering, Width, parse_clustering};
    use crate::types::{Field, LogicalType};

    fn lineitem() -> Vec<Field> {
        vec![
            Field::new("l_orderkey", LogicalType::BigInt),
            Field::new("l_linenumber", LogicalType::Integer),
            Field::new("l_shipdate", LogicalType::Date),
        ]
    }

    #[test]
    fn a_declaration_that_could_never_be_satisfied_is_refused() {
        let fields = lineitem();
        assert!(Clustering::new(Vec::new(), Width::Exact, &fields).is_err(), "no column at all");
        assert!(Clustering::new(vec![3], Width::Exact, &fields).is_err(), "past the end");
        assert!(Clustering::new(vec![0, 1, 0], Width::Exact, &fields).is_err(), "twice");
        assert!(Clustering::new(vec![2, 0], Width::Month, &fields).is_ok());
    }

    #[test]
    fn a_calendar_bucket_on_a_column_with_no_calendar_in_it_is_refused() {
        // There is no month of an order key, so a loader handed this would have nothing to sort
        // by. The plain width is fine on the same column, which is what makes this worth checking
        // rather than refusing every leading column that is not a date.
        let fields = lineitem();
        let complaint = Clustering::new(vec![0, 2], Width::Month, &fields)
            .expect_err("a bigint has no months")
            .to_string();
        assert!(complaint.contains("l_orderkey"), "{complaint}");
        assert!(complaint.contains("MONTH"), "{complaint}");
        assert!(
            Clustering::new(vec![0, 2], Width::Exact, &fields).is_ok(),
            "no bucket, no problem"
        );
    }

    /// A declaration that names no width leaves it to the data on a date, and is exact elsewhere.
    #[test]
    fn a_declaration_with_no_width_takes_the_quarter_on_a_date_and_nothing_elsewhere() {
        let fields = lineitem();
        let dated = Clustering::over(vec![2, 0, 1], &fields).expect("a date leads");
        assert_eq!(dated.width(), Width::Auto, "nobody said, so the rows will say");
        assert_eq!(dated.columns(), [2, 0, 1], "the columns are the ones asked for, in order");
        // A bigint has no quarters, so the same call on one has to come back exact rather than
        // come back an error, which is the whole reason the width is picked from the column.
        let keyed = Clustering::over(vec![0, 2], &fields).expect("a bigint leads");
        assert_eq!(keyed.width(), Width::Exact);
        // The checks are the ones a written out declaration gets, since it is the same call.
        assert!(Clustering::over(Vec::new(), &fields).is_err(), "no column at all");
        assert!(Clustering::over(vec![7], &fields).is_err(), "past the end");
    }

    #[test]
    fn every_width_survives_its_byte() {
        for width in [Width::Exact, Width::Month, Width::Quarter, Width::Year, Width::Auto] {
            assert_eq!(Width::from_tag(width.tag()), Some(width), "{width}");
        }
        assert_eq!(Width::from_tag(5), None, "a tag from a build that knows more than this one");
    }

    /// The rule picks a different calendar width at two scale factors of the same table.
    ///
    /// The whole point of the rule, and the thing a constant cannot do. Both rows are TPC-H
    /// lineitem over the same seven years of ship dates, six million rows at SF1 and sixty million
    /// at SF10, and the numbers are the ones `dbgen` actually produces rather than round figures.
    ///
    /// SF1 lands on the quarter, which is what `14-the-partition-width.md` measured best there.
    /// SF10 lands on the month, because a month of SF10 holds seven hundred thousand rows, which is
    /// three times what SF1's quarter held: the width that was too narrow at one scale is
    /// comfortable at the next one up, and the calendar word never moved.
    #[test]
    fn the_same_table_at_two_scales_gets_two_widths() {
        let span = 2525;
        assert_eq!(Width::for_span(6_001_215, span), Width::Quarter, "SF1");
        assert_eq!(Width::for_span(59_986_052, span), Width::Month, "SF10");
        // Smaller than SF1 does not go on getting coarser. A tenth of the rows is a long way under
        // the target at every width, and the answer is still the quarter, because what is under the
        // target is the case for not cutting finer and says nothing about cutting coarser.
        assert_eq!(Width::for_span(600_572, span), Width::Quarter, "SF0.1");
        // SF1 orders is the row that made the year a mistake. A quarter of lineitem's size over the
        // same span, under the target at every width, and measured best on a quarter all the same.
        assert_eq!(Width::for_span(1_500_000, span), Width::Quarter, "SF1 orders");
        // Nothing to divide. An empty table and a table whose dates are all the same day both have
        // no shape to fit, so both get the measured default.
        assert_eq!(Width::for_span(0, span), Width::DEFAULT);
        assert_eq!(Width::for_span(6_001_215, 0), Width::DEFAULT);
    }

    /// An automatic width becomes a real one and a written one is left alone.
    #[test]
    fn fitting_a_declaration_resolves_the_automatic_width_and_only_that_one() {
        let fields = lineitem();
        let auto = Clustering::over(vec![2, 0, 1], &fields).expect("a date leads");
        assert_eq!(auto.width(), Width::Auto);
        let fitted = auto.fitted(6_001_215, 2525);
        assert_eq!(fitted.width(), Width::Quarter, "the rows decided");
        assert_eq!(fitted.columns(), auto.columns(), "and nothing else moved");
        // Written down is written down. A table of any size declared exact stays exact, because the
        // declaration is what the table is asked to be rather than a hint the loader may improve on.
        for width in [Width::Exact, Width::Month, Width::Quarter, Width::Year] {
            let asked = Clustering::new(vec![2, 0, 1], width, &fields).expect("valid");
            assert_eq!(asked.fitted(59_986_052, 2525), asked, "{width}");
        }
    }

    /// The two tables the stage 0 measurement clusters, written the way the setting takes them.
    #[test]
    fn the_two_clustered_tpch_tables_parse_into_two_declarations() {
        let setting = "lineitem(quarter(l_shipdate), l_orderkey, l_linenumber), \
                       orders(quarter(o_orderdate), o_orderkey)";
        let declared = parse_clustering(setting).expect("parse");
        assert_eq!(declared.len(), 2, "a comma inside a column list is not a separator");
        assert_eq!(declared[0].table(), "lineitem");
        assert_eq!(declared[0].width(), Some(Width::Quarter));
        assert_eq!(declared[0].columns(), ["l_shipdate", "l_orderkey", "l_linenumber"]);
        assert_eq!(declared[1].table(), "orders");
        assert_eq!(declared[1].columns(), ["o_orderdate", "o_orderkey"]);
        // A trailing comma and a line per table, which is how a setting this long gets edited.
        let spread = "lineitem(month(l_shipdate), l_orderkey),\n  orders(o_orderkey),\n";
        let declared = parse_clustering(spread).expect("parse");
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0].width(), Some(Width::Month));
        // No wrapper means no width was named, which is not the same as naming the exact value:
        // the first leaves the width to the column's type and the second overrides it.
        assert_eq!(declared[1].width(), None);
        assert_eq!(parse_clustering("t(exact(d))").expect("parse")[0].width(), Some(Width::Exact));
        assert!(parse_clustering("").expect("parse").is_empty(), "a reset names no table");
    }

    /// What a declaration reads back as is what the setting takes, so one can be pasted into it.
    ///
    /// The three calendar widths and the automatic one round trip. The exact one does not, and that
    /// is worth a test of its own rather than a carve out in a loop: [`Clustering::describe`] prints
    /// no wrapper for it, the parse of a bare leading column names no width, and a bare date column
    /// then gets the automatic width. So pasting an exactly sorted date declaration back into the
    /// setting gives one that leaves the width to the data. Whoever wants the exact value back says
    /// `exact(...)`, which is why that word is in the grammar at all given that no printer produces
    /// it.
    #[test]
    fn what_a_declaration_describes_itself_as_parses_back_into_the_same_declaration() {
        let fields = lineitem();
        let names = fields.iter().map(|field| field.name.clone()).collect::<Vec<_>>();
        let at = |name: &String| names.iter().position(|it| it == name).expect("a column") as u32;
        for width in [Width::Month, Width::Quarter, Width::Year, Width::Auto] {
            let asked = Clustering::new(vec![2, 0, 1], width, &fields).expect("valid");
            let written = format!("lineitem({})", asked.describe(&names));
            let read = parse_clustering(&written).expect("parse");
            let columns: Vec<u32> = read[0].columns().iter().map(at).collect();
            let again = Clustering::new(columns, read[0].width().expect("a width"), &fields)
                .expect("valid");
            assert_eq!(again, asked, "{written}");
        }
        let exact = Clustering::new(vec![2, 0, 1], Width::Exact, &fields).expect("valid");
        let written = format!("lineitem({})", exact.describe(&names));
        assert_eq!(written, "lineitem(l_shipdate, l_orderkey, l_linenumber)");
        let read = parse_clustering(&written).expect("parse");
        assert_eq!(read[0].width(), None, "a bare date column names no width");
        let columns: Vec<u32> = read[0].columns().iter().map(at).collect();
        assert_eq!(
            Clustering::over(columns, &fields).expect("valid").width(),
            Width::Auto,
            "and a silent date declaration leaves the width to the data"
        );
    }

    /// The shapes the parse turns away, which are the ones a catalog could never make sense of.
    #[test]
    fn a_declaration_that_is_not_a_table_and_a_column_list_is_refused() {
        for bad in [
            "lineitem",
            "lineitem(",
            "(l_shipdate)",
            "lineitem()",
            "lineitem(day(l_shipdate))",
            "lineitem(l_shipdate, month(l_orderkey))",
            "lineitem(month())",
        ] {
            let complaint = parse_clustering(bad).expect_err(bad).message().to_owned();
            assert!(complaint.contains("clustering"), "{bad}: {complaint}");
        }
        // The one that reads like a mistake and is not: a width word is only a width in front of
        // the leading column, so a column actually called `year` is still a column.
        let declared = parse_clustering("t(year)").expect("parse");
        assert_eq!(declared[0].columns(), ["year"]);
        assert_eq!(declared[0].width(), None);
    }

    #[test]
    fn a_declaration_reads_back_the_way_it_was_written() {
        // The one thing this string is for is that somebody can check the layout is what they
        // asked for without reading a column index against a schema by hand.
        let fields = lineitem();
        let names = fields.iter().map(|field| field.name.clone()).collect::<Vec<_>>();
        let stage_zero = Clustering::new(vec![2, 0, 1], Width::Month, &fields).expect("valid");
        assert_eq!(stage_zero.describe(&names), "month(l_shipdate), l_orderkey, l_linenumber");
        let plain = Clustering::new(vec![0], Width::Exact, &fields).expect("valid");
        assert_eq!(plain.describe(&names), "l_orderkey");
    }
}
