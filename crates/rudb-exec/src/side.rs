//! The gathered side of a join, held as columns so that a match is a position rather than a row.
//!
//! A join finds pairs and then has to say what the pairs are. What this replaces said it by building
//! a `Vec<Value>` per output row: one heap allocation holding one boxed value per column, assembled
//! by cloning the driving row and the gathered row into it, and transposed back into columns at the
//! end by [`rows::pack`](crate::rows::pack). On the shape #880 measured that was 825ns per output
//! row, against a `SELECT sum(i) FROM range(...)` producing the same number of rows in a twentieth
//! of the time, so the boxing was the whole of the gap and none of it was the join.
//!
//! What a pair actually is, once the lookup has found it, is two numbers: which driving row and
//! which gathered row. So the probe writes two lists of numbers and the answer is built by gathering
//! each output column at those positions, which is one typed loop per column over a run of `u32`
//! rather than one allocation per row. [`Vector::gather`] is that loop and it was already here.
//!
//! For the driving side the positions index the chunk in hand. For the gathered side they have to
//! index the whole of it at once, and the whole of it arrives as a list of chunks, so this lays
//! those chunks end to end into one vector per column. [`Assembly`] is what lays them: it appends a
//! run of data after another run of data, which is a `memcpy` for a fixed width column and one arena
//! growth for a string one.
//!
//! # The row that is not there
//!
//! A `LEFT` join pads a driving row that matched nothing with nulls, and a `SINGLE` join does the
//! same. That is a row of the gathered side that does not exist, and the obvious way to write it is
//! a branch in the gather saying this one is padding. There is no branch: [`Vector::gather`] answers
//! null for a position past the end of the vector, so [`PAD`] is a position past the end and the
//! padded rows go through the same loop as the matched ones.

use std::borrow::Cow;
use std::sync::Arc;

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_pipeline::Lease;
use rudb_vector::{Assembly, Chunk, Form, Validity, Vector};

use crate::pairs::in_parallel;

/// The position that gathers as null, which is what an unmatched driving row is paired with.
///
/// Past the end of any side, because a side with this many rows in it is refused by [`Build::new`]
/// before it is built.
pub(crate) const PAD: u32 = u32::MAX;

/// The gathered side of a join, one vector per column.
#[derive(Debug, Default)]
pub(crate) struct Build {
    columns: Vec<Vector>,
    rows: usize,
}

impl Build {
    /// The chunks laid end to end, a column at a time.
    ///
    /// `types` rather than the chunks' own types because a side with no chunks in it still has
    /// columns, and a join against an empty side still has to produce the right number of null
    /// columns for the driving rows a `LEFT` join keeps.
    ///
    /// A column at a time is also a thread at a time. See [`laid_out`], which is where that is.
    ///
    /// # Errors
    ///
    /// [`rudb_common::ErrorCode::OutOfRange`] when the side has more rows than a position can name,
    /// and whatever the assembly says when a column is of a type it has no layout for.
    pub(crate) fn new(
        types: &[LogicalType],
        chunks: &[Chunk],
        threads: &Lease<'_>,
    ) -> Result<Self> {
        let rows: usize = chunks.iter().map(Chunk::len).sum();
        if rows >= PAD as usize {
            return Err(Error::out_of_range(format!(
                "a join cannot gather {rows} rows, which is more than a position can name"
            )));
        }
        let columns = laid_out(types, chunks, threads)?;
        Ok(Self { columns, rows })
    }

    /// How many rows are in there.
    pub(crate) fn rows(&self) -> usize {
        self.rows
    }

    /// Column `index` of the side, laid end to end.
    pub(crate) fn column(&self, index: usize) -> Option<&Vector> {
        self.columns.get(index)
    }

    /// How many bytes the columns are holding.
    pub(crate) fn footprint(&self) -> u64 {
        self.columns
            .iter()
            .map(|column| u64::try_from(column.footprint()).unwrap_or(u64::MAX))
            .sum()
    }

    /// Every column read at those positions, [`PAD`] reading as null.
    ///
    /// # Errors
    ///
    /// Whatever the gather says about a column of a type it has no layout for.
    pub(crate) fn gather(&self, at: &[u32]) -> Result<Vec<Vector>> {
        self.columns.iter().map(|column| column.gather(at)).collect()
    }

    /// The same, gathering only the columns `wanted` marks and standing in for the rest.
    ///
    /// A residual condition reads a column or two of a side that may be twenty wide, and the ones
    /// it does not read are only there so that the column numbers line up. A constant lines them up
    /// without a pass over the pairs. A `wanted` shorter than the side gathers whatever it does not
    /// cover, which is the answer that is never wrong.
    ///
    /// # Errors
    ///
    /// The same as [`Build::gather`].
    pub(crate) fn gather_wanted(&self, at: &[u32], wanted: &[bool]) -> Result<Vec<Vector>> {
        self.columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                if wanted.get(index).copied().unwrap_or(true) {
                    column.gather(at)
                } else {
                    Ok(Vector::constant(column.logical_type().clone(), Value::Null, at.len()))
                }
            })
            .collect()
    }

    /// The chunk those positions make, which is what a residual condition is evaluated over.
    ///
    /// # Errors
    ///
    /// The same as [`Build::gather`].
    pub(crate) fn chunk(&self, at: &[u32]) -> Result<Chunk> {
        Chunk::with_rows(self.gather(at)?, at.len())
    }

    /// One row as values, for the sink that still pairs rows up one at a time.
    ///
    /// The row major path out of #880 rather than the columnar one. [`Build::gather`] is what the
    /// stream uses and this is what the `RIGHT` and `FULL` kinds use until they are moved over too.
    pub(crate) fn row(&self, at: u32) -> Vec<Value> {
        self.columns.iter().map(|column| column.value_at(at as usize)).collect()
    }
}

/// A list of chunks laid end to end, one vector per column, a column per thread.
///
/// Both of this operator's two runs over the gathered side want it: the pairs the join answers with
/// are gathered out of it, and the table that finds the pairs is built over the key columns in the
/// same shape. A partition of that build reads rows from anywhere in the side, so the keys have to
/// be one run rather than a list of chunks before it can start.
///
/// A column at a time is also a thread at a time. One column's assembly reads one column of each
/// chunk and writes one vector, and two of them share nothing, so the lease the calling pipeline
/// already holds gets a column each. What that does not get is more threads than there are columns,
/// which is the limit worth naming: splitting a column across threads would mean an arena per piece
/// and then a join of the arenas, and the copy that would cost is the one #947 took out.
///
/// # Errors
///
/// Whatever the assembly says when a column is of a type it has no layout for.
pub(crate) fn laid_out(
    types: &[LogicalType],
    chunks: &[Chunk],
    threads: &Lease<'_>,
) -> Result<Vec<Vector>> {
    let rows: usize = chunks.iter().map(Chunk::len).sum();
    let one = |index: usize| -> Result<Vector> {
        if let Some(coded) = coded(chunks, index)? {
            return Ok(coded);
        }
        if let Some(laid) = end_to_end(&types[index], chunks, index)? {
            return Ok(laid);
        }
        let mut assembly = Assembly::new(types[index].clone(), rows)?;
        let mut at: Vec<u32> = Vec::new();
        let mut base: u32 = 0;
        for chunk in chunks {
            let len = u32::try_from(chunk.len()).unwrap_or(PAD);
            at.clear();
            at.extend(base..base + len);
            assembly.place(&at, chunk.column(index)?)?;
            base += len;
        }
        assembly.finish()
    };
    in_parallel(threads, types.len(), threads.degree(), "gathered column", one)
}

/// One column of the chunks laid end to end in a single copy, or `None` to leave it to an assembly.
///
/// An assembly is a scatter. It is built to put rows wherever a caller says, so it flattens each
/// piece, copies it into its own run, and then walks every row to record where the row went and
/// whether it is null, only for the finish to find that every row went where it already was. On
/// TPC-H q9 that was three passes over the 800,000 rows of `partsupp` and the 319,404 joined rows
/// the last join builds on, and it was a quarter of the time the query spent building its tables.
/// Laying the chunks end to end is one copy, and none at all when they are windows of one page.
///
/// A piece that is not flat is flattened on its own first, which is the copy it would have had in
/// the assembly anyway. A nested type, and a piece whose flattened run is shorter than the piece
/// (an untyped null), go to the assembly, which is written for both.
fn end_to_end(ty: &LogicalType, chunks: &[Chunk], index: usize) -> Result<Option<Vector>> {
    if matches!(ty, LogicalType::List(_) | LogicalType::Struct(_) | LogicalType::Map(_, _)) {
        return Ok(None);
    }
    let mut pieces: Vec<Cow<'_, Vector>> = Vec::with_capacity(chunks.len());
    for chunk in chunks.iter().filter(|chunk| !chunk.is_empty()) {
        let column = chunk.column(index)?;
        let piece = match column.form() {
            Form::Flat | Form::StringView => Cow::Borrowed(column),
            // flatten: a build side is probed by row, so a piece that is a dictionary, a constant or
            // packed is decoded once here rather than once per probe, the copy the assembly made.
            _ => Cow::Owned(column.flatten()?),
        };
        if piece.form() == Form::Flat && short(&piece) {
            return Ok(None);
        }
        pieces.push(piece);
    }
    // Views over one shared arena lay without a copy, and anything else that lays is flat.
    if let Some(laid) = rudb_vector::concat(ty, &pieces)? {
        return Ok(Some(laid));
    }
    for piece in &mut pieces {
        if piece.form() == Form::StringView {
            // flatten: views over different arenas cannot share one, so they are copied into a
            // flat run, which is what the assembly this replaced did for every piece.
            *piece = Cow::Owned(piece.flatten()?);
            if short(piece) {
                return Ok(None);
            }
        }
    }
    rudb_vector::concat(ty, &pieces)
}

/// One column of the chunks as codes into the one dictionary every piece of it shares, or `None`
/// when the pieces do not all share one.
///
/// A column a native table keeps under a table wide dictionary is scanned as codes into it, and
/// every chunk of the scan points at the same values. Flattening those here turned the codes back
/// into strings, and everything above the join paid for the strings: on TPC-H q16 the grouping on
/// `p_brand` and `p_type` hashed and compared their bytes for 118,000 rows, where two codes would
/// have done. Laid end to end the codes are four bytes a row, a probe gathers them the way it
/// gathers any dictionary and keeps them stable, and the grouping and the sort above take codes
/// they already know how to read.
fn coded(chunks: &[Chunk], index: usize) -> Result<Option<Vector>> {
    let mut shared: Option<&Arc<Vector>> = None;
    let mut rows = 0;
    let mut nulls = false;
    for chunk in chunks.iter().filter(|chunk| !chunk.is_empty()) {
        let column = chunk.column(index)?;
        let Some((codes, values)) = column.stable_dictionary_parts() else { return Ok(None) };
        if codes.len() != column.len() || shared.is_some_and(|held| !Arc::ptr_eq(held, values)) {
            return Ok(None);
        }
        shared = Some(values);
        rows += codes.len();
        nulls |= column.validity().has_nulls(column.len());
    }
    let Some(values) = shared else { return Ok(None) };
    let mut codes = Vec::with_capacity(rows);
    let mut valid = Vec::with_capacity(if nulls { rows } else { 0 });
    for chunk in chunks.iter().filter(|chunk| !chunk.is_empty()) {
        let column = chunk.column(index)?;
        let Some((run, _)) = column.stable_dictionary_parts() else { return Ok(None) };
        codes.extend_from_slice(run);
        if nulls {
            let validity = column.validity();
            valid.extend((0..run.len()).map(|row| validity.is_valid(row)));
        }
    }
    let vector = Vector::stable_dictionary(codes, Arc::clone(values))?;
    Ok(Some(if nulls {
        vector.with_validity(Validity::from_iter(rows, |row| valid[row]))
    } else {
        vector
    }))
}

/// Whether a flat piece holds fewer values than it has rows, which is what an untyped null is.
fn short(piece: &Vector) -> bool {
    piece.data().is_none_or(|data| data.len() != piece.len())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rudb_common::{LogicalType, Value};
    use rudb_pipeline::Lease;
    use rudb_vector::{Chunk, Data, Validity, Vector};

    use super::{Build, PAD};

    fn chunk(values: &[i32], text: &[&str]) -> Chunk {
        let numbers = Vector::flat(LogicalType::Integer, Data::Int32(values.to_vec().into()))
            .expect("integers are an i32 layout");
        let strings = Vector::from_values(
            LogicalType::Varchar,
            &text.iter().map(|&word| Value::Varchar(word.to_string())).collect::<Vec<_>>(),
        )
        .expect("strings are a varlen layout");
        Chunk::new(vec![numbers, strings]).expect("two columns of the same length")
    }

    /// One thread, because a test is checking what comes out and not how many threads it took.
    fn alone() -> Lease<'static> {
        Lease::alone()
    }

    fn types() -> Vec<LogicalType> {
        vec![LogicalType::Integer, LogicalType::Varchar]
    }

    #[test]
    fn chunks_laid_end_to_end_read_back_in_the_order_they_were_given() {
        let side =
            Build::new(&types(), &[chunk(&[1, 2], &["a", "b"]), chunk(&[3], &["c"])], &alone())
                .expect("two chunks of two columns");
        assert_eq!(side.rows(), 3);
        let gathered = side.gather(&[0, 1, 2]).expect("three positions in range");
        assert_eq!(gathered[0].value_at(0), Value::Integer(1));
        assert_eq!(gathered[0].value_at(2), Value::Integer(3));
        assert_eq!(gathered[1].value_at(1), Value::Varchar("b".to_string()));
        assert_eq!(gathered[1].value_at(2), Value::Varchar("c".to_string()));
    }

    #[test]
    fn a_position_may_be_asked_for_more_than_once_and_in_any_order() {
        let side = Build::new(&types(), &[chunk(&[10, 20], &["x", "y"])], &alone())
            .expect("one chunk of two columns");
        let gathered = side.gather(&[1, 1, 0]).expect("three positions in range");
        assert_eq!(gathered[0].value_at(0), Value::Integer(20));
        assert_eq!(gathered[0].value_at(1), Value::Integer(20));
        assert_eq!(gathered[0].value_at(2), Value::Integer(10));
    }

    #[test]
    fn the_padding_position_reads_as_null_in_every_column() {
        let side = Build::new(&types(), &[chunk(&[7], &["z"])], &alone())
            .expect("one chunk of two columns");
        let gathered = side.gather(&[PAD, 0]).expect("a padded position and a real one");
        assert_eq!(gathered[0].value_at(0), Value::Null);
        assert_eq!(gathered[1].value_at(0), Value::Null);
        assert_eq!(gathered[0].value_at(1), Value::Integer(7));
    }

    #[test]
    fn a_side_with_no_chunks_still_has_its_columns_and_every_one_of_them_is_null() {
        let side = Build::new(&types(), &[], &alone()).expect("no chunks at all");
        assert_eq!(side.rows(), 0);
        let gathered = side.gather(&[PAD, PAD]).expect("two padded positions");
        assert_eq!(gathered.len(), 2);
        assert_eq!(gathered[0].value_at(0), Value::Null);
        assert_eq!(gathered[1].value_at(1), Value::Null);
    }

    /// Codes into `values`, as a scan of a table wide dictionary hands them up, with the rows in
    /// `nulls` null.
    fn coded(codes: &[u32], values: &Arc<Vector>, nulls: &[usize]) -> Chunk {
        let column = Vector::stable_dictionary(codes.to_vec(), Arc::clone(values))
            .expect("codes inside the dictionary")
            .with_validity(Validity::from_iter(codes.len(), |row| !nulls.contains(&row)));
        Chunk::new(vec![column]).expect("one column")
    }

    fn words(words: &[&str]) -> Arc<Vector> {
        let values: Vec<Value> = words.iter().map(|&word| Value::Varchar(word.into())).collect();
        Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("strings"))
    }

    #[test]
    fn codes_into_one_shared_dictionary_stay_codes_and_read_back_as_the_strings() {
        let values = words(&["MEDIUM", "LARGE", "SMALL"]);
        let chunks = [coded(&[2, 0], &values, &[]), coded(&[1, 1, 0], &values, &[1])];
        let side = Build::new(&[LogicalType::Varchar], &chunks, &alone()).expect("two chunks");
        let gathered = side.gather(&[4, PAD, 0, 3, 2]).expect("positions in range and a pad");
        let (_, held) = gathered[0].stable_dictionary_parts().expect("still codes after a gather");
        assert!(Arc::ptr_eq(held, &values), "the codes point somewhere else");
        let read: Vec<Value> = (0..5).map(|row| gathered[0].value_at(row)).collect();
        let text = |word: &str| Value::Varchar(word.into());
        assert_eq!(read, [text("MEDIUM"), Value::Null, text("SMALL"), Value::Null, text("LARGE")]);
    }

    #[test]
    fn codes_into_two_different_dictionaries_are_laid_out_as_strings() {
        let (one, two) = (words(&["a", "b"]), words(&["b", "c"]));
        let chunks = [coded(&[1], &one, &[]), coded(&[1, 0], &two, &[])];
        let side = Build::new(&[LogicalType::Varchar], &chunks, &alone()).expect("two chunks");
        let gathered = side.gather(&[0, 1, 2]).expect("three positions");
        assert!(gathered[0].stable_dictionary_parts().is_none(), "codes of two code spaces mixed");
        let read: Vec<Value> = (0..3).map(|row| gathered[0].value_at(row)).collect();
        assert_eq!(read, ["b", "c", "b"].map(|word| Value::Varchar(word.into())));
    }

    #[test]
    fn a_row_read_as_values_is_the_row_that_went_in() {
        let side =
            Build::new(&types(), &[chunk(&[4, 5], &["p", "q"])], &alone()).expect("one chunk");
        assert_eq!(side.row(1), vec![Value::Integer(5), Value::Varchar("q".to_string())]);
    }

    /// Every form a piece of the gathered side can arrive in, laid end to end and read back.
    ///
    /// The flat pieces and the string views over one arena lay without an assembly, the others are
    /// flattened on the way, and a null constant with no values in it goes to the assembly. Each
    /// mix has to read back as the values that went in, in order.
    #[test]
    fn a_side_laid_out_of_pieces_of_every_form_reads_back_as_the_values_that_went_in() {
        let int = LogicalType::Integer;
        let text = LogicalType::Varchar;
        let words = |list: &[&str]| {
            let values: Vec<Value> =
                list.iter().map(|&word| Value::Varchar(word.to_string())).collect();
            Vector::from_values(LogicalType::Varchar, &values).expect("strings build")
        };
        let page = rudb_vector::concat(
            &text,
            &[words(&["one", "a string longer than twelve", "three", "four"])],
        )
        .expect("a flat piece lays")
        .expect("and comes back as views");
        let numbers = |list: &[Option<i32>]| {
            let values: Vec<Value> =
                list.iter().map(|value| value.map_or(Value::Null, Value::Integer)).collect();
            Vector::from_values(LogicalType::Integer, &values).expect("integers build")
        };
        let coded =
            Vector::dictionary(vec![1, 0], numbers(&[Some(5), Some(6)])).expect("codes in range");
        let shared = [
            Chunk::new(vec![numbers(&[Some(1), None]), page.gather(&[0, 1]).expect("in range")]),
            Chunk::new(vec![coded, page.gather(&[3, 2]).expect("in range")]),
        ];
        let mixed = [
            Chunk::new(vec![
                Vector::constant(int.clone(), Value::Integer(7), 2),
                page.gather(&[1, 0]).expect("in range"),
            ]),
            Chunk::new(vec![Vector::constant(int.clone(), Value::Null, 2), words(&["x", "y"])]),
        ];
        let want_shared = (
            vec![Value::Integer(1), Value::Null, Value::Integer(6), Value::Integer(5)],
            ["one", "a string longer than twelve", "four", "three"],
        );
        let want_mixed = (
            vec![Value::Integer(7), Value::Integer(7), Value::Null, Value::Null],
            ["a string longer than twelve", "one", "x", "y"],
        );
        for (chunks, (ints, strings)) in [(shared, want_shared), (mixed, want_mixed)] {
            let chunks: Vec<Chunk> =
                chunks.into_iter().map(|chunk| chunk.expect("two columns of two rows")).collect();
            let side = Build::new(&[int.clone(), text.clone()], &chunks, &alone()).expect("lays");
            let got = side.gather(&[0, 1, 2, 3]).expect("four positions in range");
            let read: Vec<Value> = (0..4).map(|at| got[0].value_at(at)).collect();
            assert_eq!(read, ints);
            let read: Vec<Value> = (0..4).map(|at| got[1].value_at(at)).collect();
            let strings: Vec<Value> =
                strings.iter().map(|&word| Value::Varchar(word.to_string())).collect();
            assert_eq!(read, strings);
        }
    }
}
