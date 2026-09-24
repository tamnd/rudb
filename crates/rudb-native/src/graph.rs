//! Building a table's graph sections from the table's own columns.
//!
//! This is where the two halves meet. `rudb-graph` at rank 5 knows what a key map is and knows
//! nothing about a file; the rest of this crate knows how to put an opaque payload in a file and
//! nothing about what one means. Neither of them can build a key map for a real table, because
//! doing that means reading a column back, so it happens here, in the crate that is allowed to see
//! both.
//!
//! Everything here obeys spec/graph/03-the-file-format.md section 3.1. A column that cannot be
//! mapped is a column with no key map, not an error at open; a section that is stale, torn, or of a
//! form this build does not know is a section that is not there. That is why [`key_map`] answers
//! with an [`Option`] and not a [`Result`]: there is no failure it could report that is not
//! answered by running the query the way it ran before the section existed.

use std::path::Path;
use std::time::{Duration, Instant};

use rudb_common::{LogicalType, Result, Value};
use rudb_graph::{Degrees, Form, KeyMap, Keys, NO_PARENT, link, wire};
use rudb_vector::Chunk;

use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid, type_tag};

/// One column of a committed table, scanned in `rid` order.
///
/// A `rid` is a row's position in append order, and the parts of a table are in append order, so a
/// scan of the parts in order is a scan in `rid` order and there is nothing to look up. That is the
/// whole of the correspondence and it is worth stating, because a build that read the parts in any
/// other order would produce a map that resolved every key to the wrong row without failing.
///
/// Or two columns of it, when the key is a [`pair`]. Both are read from the same part in one call,
/// so the two values of a row are the same row's.
#[derive(Debug)]
pub struct KeyColumn<'a> {
    reader: &'a Reader,
    columns: Vec<usize>,
}

impl<'a> KeyColumn<'a> {
    /// Names a column of a table, or a [`pair`] of them, as the key of a relationship's side.
    ///
    /// # Errors
    ///
    /// If there is no such column, or if its type has no integer key form. `VARCHAR` is the second
    /// of those today: section 2.2 says a string key is mapped through its dictionary codes rather
    /// than its text, and the code path is not built yet. None of TPC-H's eight relationships needs
    /// it, so it is refused by name rather than approximated.
    pub fn new(reader: &'a Reader, key: usize) -> Result<Self> {
        let fields = reader.table().fields();
        let columns = columns_of(key);
        for &column in &columns {
            let Some(field) = fields.get(column) else {
                return Err(invalid(&format!(
                    "column {column} is past the {} of table {}",
                    fields.len(),
                    reader.table().name()
                )));
            };
            if !mappable(&field.ty) {
                return Err(invalid(&format!(
                    "a key map over {} needs an integer key form, and {} has none",
                    field.name, field.ty
                )));
            }
        }
        Ok(Self { reader, columns })
    }
}

impl Keys for KeyColumn<'_> {
    fn scan(&self, each: &mut dyn FnMut(Option<i128>) -> Result<()>) -> Result<()> {
        for part in 0..self.reader.parts() {
            let chunk = self.reader.read(part, &self.columns)?;
            let first = chunk.column(0)?;
            let second = if self.columns.len() == 2 { Some(chunk.column(1)?) } else { None };
            for row in 0..chunk.len() {
                let key = key_at(&chunk, first, 0, row)?;
                let key = match second {
                    None => key,
                    Some(second) => match (key, key_at(&chunk, second, 1, row)?) {
                        (Some(high), Some(low)) => Some(fold(high, low)?),
                        // A composite with a null in it matches nothing, the way SQL compares it.
                        _ => None,
                    },
                };
                each(key)?;
            }
        }
        Ok(())
    }
}

/// Where a key over two columns sits among the column numbers.
///
/// A relationship's key is named by a number everywhere it is stored: the id of a key map or a link
/// section, and the parent column in a link's binding. A key over one column is that column's index
/// and always was. A key over two is this bit, with the two indexes packed under it, so the files
/// written before there were two column keys read exactly as they did, and nothing that stores a
/// key needs a second field for the rare key that has two columns. TPC-H has one of them,
/// `partsupp(ps_partkey, ps_suppkey)`, which spec/graph/02-the-data-model.md section 2.3 names.
const PAIR: usize = 1 << 31;

/// How many bits each column index of a [`pair`] gets, which is room for 32,768 columns.
const PAIR_BITS: u32 = 15;

/// The number that names a key over these columns, or `None` for a list this cannot name.
///
/// One column is its own index. Two are a pair. More than two, or an index too large to pack, is a
/// key nothing is built for, which section 3.1 says is a slower query and never a wrong one.
#[must_use]
pub fn key_of(columns: &[usize]) -> Option<usize> {
    let fits = |column: usize| column < 1 << PAIR_BITS;
    match *columns {
        [column] if column < PAIR => Some(column),
        [first, second] if fits(first) && fits(second) => Some(PAIR | first << PAIR_BITS | second),
        _ => None,
    }
}

/// The two columns of a pair key, first then second.
#[must_use]
pub fn pair(first: usize, second: usize) -> Option<usize> {
    key_of(&[first, second])
}

/// The columns a key number names, which [`key_of`] made.
#[must_use]
pub fn columns_of(key: usize) -> Vec<usize> {
    if key & PAIR == 0 {
        return vec![key];
    }
    let mask = (1 << PAIR_BITS) - 1;
    vec![(key >> PAIR_BITS) & mask, key & mask]
}

/// Two key values as one, with nothing lost.
///
/// Each value has to fit a 32 bit integer. The second moves up by 2^31 into `0..2^32` and the first
/// is multiplied past that, so two different pairs never give the same number, which is the property
/// a key map needs: a hash would be smaller and would also let two keys meet, and a key map has no
/// second look at the row to tell them apart. The result fits an `i64`, because a key map's keys have
/// to span no more than a `u64` does. A value outside that range is an error and the relationship
/// gets no link, which section 3.1 says is a slower query and not a wrong one. TPC-H's keys are
/// under two hundred million at scale factor 1000.
fn fold(high: i128, low: i128) -> Result<i128> {
    const SHIFT: i128 = 1 << 32;
    let fits = |value: i128| i128::from(i32::MIN) <= value && value <= i128::from(i32::MAX);
    if !fits(high) || !fits(low) {
        return Err(invalid("a two column key holds a value too wide to fold into one key"));
    }
    Ok(high * SHIFT + (low - i128::from(i32::MIN)))
}

/// The type tag a key map over this key is stamped with, which is the column's own for one column.
///
/// A pair folds into an `i64`, so its map is stamped as a `BIGINT`, which is the type of the numbers
/// in it. What keeps it from passing for a map over one column is its id, which no column has.
fn key_tag(fields: &[rudb_common::Field], key: usize) -> Option<u8> {
    match *columns_of(key) {
        [column] => type_tag(&fields.get(column)?.ty).ok(),
        [first, second] => {
            fields.get(first)?;
            fields.get(second)?;
            type_tag(&LogicalType::BigInt).ok()
        }
        _ => None,
    }
}

/// Whether a column of this type can be a key at all.
fn mappable(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::TinyInt
            | LogicalType::SmallInt
            | LogicalType::Integer
            | LogicalType::BigInt
            | LogicalType::HugeInt
            | LogicalType::UTinyInt
            | LogicalType::USmallInt
            | LogicalType::UInteger
            | LogicalType::UBigInt
            | LogicalType::Date
            | LogicalType::Decimal { .. }
    )
}

/// One key out of a decoded part.
///
/// The fast answer first, because it covers the flat and dictionary forms and is a load. It hands
/// back `None` for a null and for a form it cannot read, and those two are not the same thing at
/// all: a null shifts every row after it and a value this could not read would shift nothing while
/// silently becoming one. So the slow path settles which it was, and a value that is neither is an
/// error rather than a null.
fn key_at(
    chunk: &Chunk,
    values: &rudb_vector::Vector,
    column: usize,
    row: usize,
) -> Result<Option<i128>> {
    if let Some(key) = values.signed_at(row) {
        return Ok(Some(key));
    }
    match chunk.value_at(row, column) {
        Value::Null => Ok(None),
        Value::TinyInt(key) => Ok(Some(i128::from(key))),
        Value::SmallInt(key) => Ok(Some(i128::from(key))),
        Value::Integer(key) | Value::Date(key) => Ok(Some(i128::from(key))),
        Value::BigInt(key) | Value::Time(key) | Value::Timestamp(key) => Ok(Some(i128::from(key))),
        Value::HugeInt(key) | Value::Decimal { unscaled: key, .. } => Ok(Some(key)),
        Value::UTinyInt(key) => Ok(Some(i128::from(key))),
        Value::USmallInt(key) => Ok(Some(i128::from(key))),
        Value::UInteger(key) => Ok(Some(i128::from(key))),
        Value::UBigInt(key) => Ok(Some(i128::from(key))),
        other => Err(invalid(&format!("a key column holds {other}, which is not a key"))),
    }
}

/// What building one key map cost and what it bought.
///
/// G1's exit measurement in spec/graph/10-milestones.md wants build time and bytes reported per
/// table, so the build reports them rather than being timed from outside. The form is here because
/// it is the number that explains the bytes: an identity map over fifteen million rows is the same
/// size as one over five.
#[derive(Debug, Clone, Copy)]
pub struct Built {
    /// Which column was mapped.
    pub column: usize,
    /// Which of the three forms the measurement chose.
    pub form: Form,
    /// Non-null keys in the column.
    pub rows: u64,
    /// Whether every key was distinct, which is section 2.3's verification and decides whether a
    /// link may be built on this column at all.
    pub distinct: bool,
    /// What the map takes in the file, header included, or would have taken when it was not kept.
    pub bytes: usize,
    /// What the column it maps takes in the file, which is what the budget is a share of.
    pub column_bytes: u64,
    /// Whether the map was kept. False means it was built, measured, and found to cost more than
    /// section 3.7 allows, so the file does not have it and the query plans as though key maps had
    /// never been implemented.
    pub built: bool,
    /// How long the build took, the reading of the column included.
    pub build: Duration,
}

/// Builds the key map for one column of a committed table.
///
/// # Errors
///
/// If the column cannot be read, is not a key type, or holds a value that is not a key.
pub fn build_key_map(reader: &Reader, column: usize) -> Result<KeyMap> {
    KeyMap::build_from(&KeyColumn::new(reader, column)?)
}

/// The share of a table's stored column bytes its graph sections are allowed to cost together.
///
/// Section 3.7. Ten percent, and the number matters less than the fact that there is one: a layer
/// that can only make queries faster is a layer with no reason to stop, and this is the reason.
/// What does not fit is not built, and the report says what it would have cost, so whether a larger
/// budget would buy anything is a measurement rather than an argument. The `graph_budget` setting
/// is what will move it, which is why the builder below takes it rather than reading this.
pub const BUDGET_SHARE: u64 = 10;

/// The size below which a table's graph sections always fit, whatever the share works out to.
///
/// A percentage of the stored bytes is the right rule for a structure whose size is worth arguing
/// about, and it stops making sense at the bottom. An identity key map is forty bytes on a table of
/// any size, and a key column of sequential integers is a constant delta, which encodes to almost
/// nothing: ten percent of almost nothing is less than forty bytes, so the pure rule throws away
/// the cheapest structure in the system for being expensive. What it would be measuring there is
/// how well the column compressed, not what the cache costs.
///
/// Sixty four kilobytes is the point below which no answer to "should this be kept" is worth the
/// cost of asking. It is four pages, it is invisible next to any table the graph layer is for, and
/// it leaves every budget decision that matters to the share above.
pub const BUDGET_FLOOR: u64 = 64 * 1024;

/// Builds a key map for each of these columns and attaches them all in one commit.
///
/// One commit and not one each, because a checkpoint that built six maps and published six
/// generations would be six chances to be interrupted halfway and six directories written where one
/// would do.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be mapped, or the attach fails.
pub fn build_key_maps(path: &Path, table: &str, columns: &[usize]) -> Result<Vec<Built>> {
    build_key_maps_within(path, table, columns, BUDGET_SHARE)
}

/// The same, against a budget of `share` percent of the table's stored column bytes.
///
/// The budget is over the table and not over a column, because that is what section 3.7 says and
/// because a per column rule would throw away the cheapest maps there are: an identity map is forty
/// bytes whatever the table, and a narrow, well compressed key column can be smaller than four
/// hundred. The sections already in the file that this call does not replace are counted as spent.
///
/// When the budget binds, the cheapest maps are admitted first. Section 3.7 orders by expected
/// value, child rows over section bytes, and for a key map on its own the numerator is not yet
/// known: nothing has declared a relationship over these columns, so no column is worth more than
/// another and the ordering degenerates to the denominator. Cheapest first is that, and it is also
/// the order that fits the most maps in the room there is. The forward link builder is where the
/// numerator arrives.
///
/// # Errors
///
/// If the file cannot be opened, a column cannot be mapped, or the attach fails.
pub fn build_key_maps_within(
    path: &Path,
    table: &str,
    columns: &[usize],
    share: u64,
) -> Result<Vec<Built>> {
    let reader = Catalog::open(path)?.table(table)?;
    let column_bytes = reader.layout().columns_total();
    let allowance = (column_bytes.saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let mut spent = held_bytes(&reader, columns)?;
    let mut report = Vec::with_capacity(columns.len());
    let mut payloads = Vec::with_capacity(columns.len());
    for &column in columns {
        let start = Instant::now();
        let map = build_key_map(&reader, column)?;
        let tag = key_tag(reader.table().fields(), column)
            .ok_or_else(|| invalid("a key map over a column the table does not have"))?;
        let payload = wire::encode(&map, tag)?;
        report.push(Built {
            column,
            form: map.form(),
            rows: map.observed().rows,
            distinct: map.observed().distinct,
            bytes: payload.bytes.len(),
            column_bytes,
            built: false,
            build: start.elapsed(),
        });
        payloads.push((column, payload));
    }
    // Cheapest first, and the report keeps the order it was asked in, so the two are walked through
    // an index rather than by sorting either of them.
    let mut order = (0..payloads.len()).collect::<Vec<_>>();
    order.sort_by_key(|&at| payloads[at].1.bytes.len());
    let mut keep = vec![false; payloads.len()];
    for at in order {
        // Before the budget, because this is not a budget decision. A key map over a column whose
        // key repeats cannot answer a rid for any of its keys, so keeping it would spend the
        // table's allowance on something no join may read, and the report already says what it
        // would have cost.
        if !report[at].distinct {
            continue;
        }
        let cost = payloads[at].1.bytes.len() as u64;
        if spent.saturating_add(cost) <= allowance {
            spent += cost;
            keep[at] = true;
            report[at].built = true;
        }
    }
    // The reader holds the file open and the attach opens it again to write. Dropping it first is
    // not required by any platform we build for, and it is done anyway so that the moment the
    // file is being written is a moment nothing else in this function is reading it.
    drop(reader);
    // Every column that was asked for gets an entry, and a column whose map was not kept gets one
    // with no bytes. That is section 3.7's budget record: what it would have cost is in the entry
    // rather than in a payload, so `rudb_links()` reports a number instead of a silence and the
    // file grows by fifty six bytes for the columns it decided against.
    let attachments = payloads
        .iter()
        .zip(&keep)
        .map(|((column, payload), &keep)| {
            Ok(Attachment {
                kind: *section::KEY_MAP,
                id: u64::try_from(*column).map_err(|_| invalid("column index overflow"))?,
                flags: payload.flags,
                header_bytes: if keep { payload.header_bytes } else { cost(payload.bytes.len()) },
                bytes: if keep { &payload.bytes } else { &[] },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    crate::attach(path, table, &attachments)?;
    Ok(report)
}

/// What the table's existing graph sections cost, leaving out the key maps this build is replacing.
///
/// Graph sections only. The statistics layer has its own two percent per `spec/stats` section 3.8,
/// and a budget that counted the other layer's sections would be a budget the other layer eats,
/// which is the thing the two shares being separate numbers exists to prevent.
///
/// Reading the extent tables is what this costs, which is one small read per section and not a read
/// of a payload. A section whose extent table does not checksum is counted as nothing, because it
/// is a section that is already not there.
fn held_bytes(reader: &Reader, replacing: &[usize]) -> Result<u64> {
    held_bytes_except(reader, *section::KEY_MAP, replacing)
}

/// The key map this table carries for a column, when it carries one this build can use.
///
/// `None` covers every reason there is not one, and covering them all is the point rather than an
/// omission. Section 3.1 says deleting every graph section changes no answer, only the time, so
/// there is no reason to distinguish *no map was ever built* from *the map is stale*, *the payload
/// does not checksum*, or *the form is one a later build invented*: the answer to all four is to
/// run the query the way it ran before key maps existed. A caller that wants to know which it was
/// reads the entry out of [`crate::Table::sections`], which is where `rudb_links()` will look.
#[must_use]
pub fn key_map(reader: &Reader, column: usize) -> Option<KeyMap> {
    let table = reader.table();
    let id = u64::try_from(column).ok()?;
    let held = table
        .sections()
        .iter()
        .find(|section| section.kind == *section::KEY_MAP && section.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    let (map, tag) = wire::decode(&reader.payload(held).ok()?).ok()?;
    // A map built against a different type than the column now has is a map built for a table that
    // is no longer this one. It should be unreachable, since changing a column's type rewrites the
    // table and moves its generation, and it is checked rather than assumed because the cost of
    // being wrong is every key resolving to a plausible wrong row.
    if tag != key_tag(table.fields(), column)? {
        return None;
    }
    Some(map)
}

/// One relationship, with both sides resolved to a table and a column of it.
///
/// Names and not [`rudb_graph::Relationship`], because by the time a build runs the caller has
/// already turned a declaration's column names into positions against the catalog, and doing it
/// again here would be a second place for the two to disagree.
#[derive(Debug, Clone)]
pub struct Edge {
    /// The many side, which is where the link is stored.
    pub child: String,
    /// Which column of it holds the key.
    pub child_column: usize,
    /// The one side, which is where the key map is.
    pub parent: String,
    /// Which column of it holds the key.
    pub parent_column: usize,
}

/// What building one forward link cost and what it bought.
#[derive(Debug, Clone)]
pub struct BuiltLink {
    /// The relationship this is a link for.
    pub edge: Edge,
    /// Which form section 3.4's measurement chose, or `None` when nothing was built.
    pub form: Option<link::Form>,
    /// Rows in the child table.
    pub children: u64,
    /// Rows in the parent table, or none when nothing was built.
    pub parents: u64,
    /// Children that found a parent. Below `children` means the foreign key is not total, which is
    /// legal and is also what keeps the relationship out of the monotone form.
    pub linked: u64,
    /// What the link takes in the file, header included, or would have taken when it was not kept.
    pub bytes: usize,
    /// The stored column bytes of the child table, which is what section 3.7's budget is a share
    /// of and what the size claim of section 9.1 is measured against.
    pub table_bytes: u64,
    /// What the build measured of the relationship's shape, or `None` when nothing was built.
    ///
    /// These ride along with the link rather than being computed for their own sake, because the
    /// pass that resolves every child's parent is the pass that counts degrees. They are stored in
    /// their own section and are what `rudb_links()` reports in its degree columns.
    pub degrees: Option<Degrees>,
    /// Whether it is in the file.
    pub built: bool,
    /// Why not, when not. `None` when it is.
    pub note: Option<String>,
    /// How long the build took, the reading of the child column included.
    pub build: Duration,
}

/// Builds a forward link for each relationship and attaches each child table's in one commit.
///
/// The parent's key map has to be in the file already. Section 3.8 is explicit that this is a
/// second pass at checkpoint time for exactly that reason, so a missing key map here is a note on
/// the report rather than an error: the relationship is one the file does not accelerate, and by
/// section 3.1 that changes no answer.
///
/// # Errors
///
/// If the file cannot be opened, a child key column cannot be read, or the attach fails.
pub fn build_links(path: &Path, edges: &[Edge]) -> Result<Vec<BuiltLink>> {
    build_links_within(path, edges, BUDGET_SHARE)
}

/// The same, against a budget of `share` percent of each child table's stored column bytes.
///
/// One commit per child table, for the reason [`build_key_maps`] commits once: a checkpoint that
/// published a generation per section would be a chance to be interrupted per section.
///
/// The budget is where a link differs from a key map. Section 3.7 orders by expected value, child
/// rows over section bytes, and for a link both numbers are in hand: the child rows are the rows
/// the link would skip a hash table for. So this sorts by rows over bytes descending, which admits
/// the monotone links first on any TPC-H sized file, because they are the ones with the most rows
/// behind the fewest bytes.
///
/// # Errors
///
/// If the file cannot be opened, a child key column cannot be read, or the attach fails.
pub fn build_links_within(path: &Path, edges: &[Edge], share: u64) -> Result<Vec<BuiltLink>> {
    let mut tables: Vec<&str> = Vec::new();
    for edge in edges {
        if !tables.iter().any(|held| *held == edge.child) {
            tables.push(&edge.child);
        }
    }
    let mut report = Vec::with_capacity(edges.len());
    for table in tables {
        let mine = edges.iter().filter(|edge| edge.child == table).cloned().collect::<Vec<Edge>>();
        report.extend(links_of_one_table(path, table, &mine, share)?);
    }
    Ok(report)
}

/// Every link stored in one child table, built and admitted and attached together.
fn links_of_one_table(
    path: &Path,
    table: &str,
    edges: &[Edge],
    share: u64,
) -> Result<Vec<BuiltLink>> {
    let catalog = Catalog::open(path)?;
    let child = catalog.table(table)?;
    let column_bytes = child.layout().columns_total();
    let allowance = (column_bytes.saturating_mul(share) / 100).max(BUDGET_FLOOR);
    let replacing = edges.iter().map(|edge| edge.child_column).collect::<Vec<usize>>();
    let mut spent = held_bytes_except(&child, *section::FORWARD_LINK, &replacing)?;
    let mut report = Vec::with_capacity(edges.len());
    let mut payloads: Vec<Option<Vec<u8>>> = Vec::with_capacity(edges.len());
    for edge in edges {
        let start = Instant::now();
        match one_link(&catalog, &child, edge) {
            Ok((built, bytes)) => {
                report.push(BuiltLink {
                    build: start.elapsed(),
                    table_bytes: column_bytes,
                    ..built
                });
                payloads.push(Some(bytes));
            }
            Err(note) => {
                report.push(BuiltLink {
                    edge: edge.clone(),
                    form: None,
                    children: child.table().rows() as u64,
                    parents: 0,
                    linked: 0,
                    bytes: 0,
                    table_bytes: column_bytes,
                    degrees: None,
                    built: false,
                    note: Some(note),
                    build: start.elapsed(),
                });
                payloads.push(None);
            }
        }
    }
    let mut order = (0..report.len()).filter(|at| payloads[*at].is_some()).collect::<Vec<_>>();
    // Most rows saved per byte first. What a link saves is the hash table the join would build
    // without it, and a hash join builds its smaller side, so the rows saved are the smaller of the
    // child and the parent. Counting the children alone ranks every link of one table by its size
    // and nothing else, since they all have the same children. On `lineitem` that kept the link to
    // `part`, which saves a table of 200,000 rows, over the one to `partsupp`, which saves 800,000
    // and costs a tenth more. A link over no rows is worth nothing per byte and sorts last rather
    // than dividing by zero.
    order.sort_by(|left, right| {
        let value = |at: &usize| -> f64 {
            let bytes = report[*at].bytes.max(1);
            report[*at].children.min(report[*at].parents) as f64 / bytes as f64
        };
        value(right).partial_cmp(&value(left)).unwrap_or(std::cmp::Ordering::Equal)
    });
    for at in order {
        let cost = report[at].bytes as u64;
        if spent.saturating_add(cost) <= allowance {
            spent += cost;
            report[at].built = true;
        } else {
            report[at].note = Some(format!("over the budget of {allowance} bytes"));
        }
    }
    drop(child);
    // The degree payloads are held here rather than built inside the loop below, because an
    // attachment borrows its bytes and a temporary would not outlive the call.
    let measured = report
        .iter()
        .filter(|built| built.built)
        .filter_map(|built| {
            let mut bytes = Vec::with_capacity(rudb_graph::degree::BYTES);
            built.degrees.as_ref()?.write(&mut bytes);
            Some((built.edge.child_column, bytes))
        })
        .collect::<Vec<_>>();
    // A relationship whose link was built gets the link. One that was measured and then turned away
    // gets an entry with no bytes, holding the form it would have taken and what it would have cost,
    // which is section 3.7's budget record and is what exit criterion 3 of G3 reads. One that could
    // not be built at all gets nothing, because there is no size to report: the note on the report
    // is the whole of what is known about it.
    let mut attachments = report
        .iter()
        .zip(&payloads)
        .filter(|(_, payload)| payload.is_some())
        .map(|(built, payload)| {
            let bytes = payload.as_ref().expect("filtered to the measured");
            Ok(Attachment {
                kind: *section::FORWARD_LINK,
                id: u64::try_from(built.edge.child_column)
                    .map_err(|_| invalid("column index overflow"))?,
                flags: built.form.map_or(0, |form| u32::from(form.tag())),
                header_bytes: if built.built {
                    u32::try_from(binding_bytes(&built.edge.parent))
                        .map_err(|_| invalid("a parent name longer than a section header"))?
                } else {
                    cost(bytes.len())
                },
                bytes: if built.built { bytes } else { &[] },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // The same id as the link, so that a rebuild replaces both and a reader that wants the shape of
    // a relationship it can resolve finds them the same way. The degrees are attached only for a
    // link that was kept: on their own they would describe a relationship the file cannot follow,
    // which is a planning hint for a plan that is not available.
    for (column, bytes) in &measured {
        attachments.push(Attachment {
            kind: *section::DEGREES,
            id: u64::try_from(*column).map_err(|_| invalid("column index overflow"))?,
            flags: 0,
            header_bytes: 0,
            bytes,
        });
    }
    crate::attach(path, table, &attachments)?;
    Ok(report)
}

/// Builds one link, or says in one sentence why there is not one.
///
/// The error type is a `String` and not an [`rudb_common::Error`] on purpose. Every reason a link
/// cannot be built here is a reason to not have one, which section 3.1 says is a slower query and
/// not a failed one, so the caller's response is the same for all of them and a message is what it
/// needs. A genuine I/O failure still arrives as an error, through the `?` on the scan.
fn one_link(
    catalog: &Catalog,
    child: &Reader,
    edge: &Edge,
) -> std::result::Result<(BuiltLink, Vec<u8>), String> {
    let parent =
        catalog.table(&edge.parent).map_err(|_| format!("no table named {}", edge.parent))?;
    let map = parent_map(&parent, edge)?;
    if !map.observed().usable_as_parent() {
        return Err(format!("the key of {} is not unique", edge.parent));
    }
    let keys = KeyColumn::new(child, edge.child_column).map_err(|error| error.to_string())?;
    let mut parents_of = Vec::with_capacity(child.table().rows());
    let mut failed = None;
    keys.scan(&mut |key| {
        let parent = match key {
            None => NO_PARENT,
            Some(key) => match map.lookup(key) {
                Ok(found) => found.unwrap_or(NO_PARENT),
                Err(error) => {
                    failed = Some(error.to_string());
                    NO_PARENT
                }
            },
        };
        parents_of.push(parent);
        Ok(())
    })
    .map_err(|error| error.to_string())?;
    if let Some(failed) = failed {
        return Err(failed);
    }
    let link = link::Link::build(&parents_of, map.len()).map_err(|error| error.to_string())?;
    // The parent key is unique, because the check above refused the relationship otherwise. So the
    // certificate is recorded here rather than discovered: a link only exists over a key map whose
    // parent side was counted and found distinct.
    //
    // Its own pass over the same slice rather than a loop fused into the one above. The cost of
    // measuring degrees is the scattered increment into a counter per parent and not the sequential
    // read of the child column, which the build makes twice already, so fusing would save the cheap
    // half and put a histogram inside a function whose job is to choose a form.
    let degrees = Degrees::of(&parents_of, map.len(), true);
    let bytes = encode_link(&link, &parent, edge).map_err(|error| error.to_string())?;
    Ok((
        BuiltLink {
            edge: edge.clone(),
            form: Some(link.form()),
            children: link.children(),
            parents: map.len(),
            linked: link.linked(),
            bytes: bytes.len(),
            table_bytes: 0,
            degrees: Some(degrees),
            built: false,
            note: None,
            build: Duration::ZERO,
        },
        bytes,
    ))
}

/// The parent's key map for one link: the stored one for a key over one column, and one built here
/// for a pair.
///
/// A pair's map is not kept, because nothing reads it but this build. The query follows the link and
/// never looks a key up, and the map a pair needs is the expensive kind: its keys are sparse, so it
/// is the sorted form, which on `partsupp` is several megabytes against a budget that is about four.
/// Kept, it would be refused by the budget and take the link down with it. Built here, it costs one
/// read of two parent columns per checkpoint, which is less than the child scan beside it.
fn parent_map(parent: &Reader, edge: &Edge) -> std::result::Result<KeyMap, String> {
    if columns_of(edge.parent_column).len() == 1 {
        return key_map(parent, edge.parent_column)
            .ok_or_else(|| format!("no key map is stored for {}", edge.parent));
    }
    KeyColumn::new(parent, edge.parent_column)
        .and_then(|keys| KeyMap::build_from(&keys))
        .map_err(|error| error.to_string())
}

/// What a structure that did not fit is recorded as having cost.
///
/// Saturating rather than erroring, because the number is a budget record and not a length: a
/// structure past four gigabytes did not fit any budget this project sets, and refusing to write the
/// record would turn a relationship that is merely too big into a build that fails.
fn cost(bytes: usize) -> u32 {
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

/// Bytes of binding in front of a link's payload: which parent table, column and generation.
///
/// Eight for the generation, four for the column, four for the name's length, then the name padded
/// out to eight so that the link's own header lands on a boundary.
fn binding_bytes(parent: &str) -> usize {
    16 + parent.len().div_ceil(8) * 8
}

/// The payload: the binding, then the link.
///
/// The binding is here and not in `rudb-graph`'s [`link::Link`], because a table name and a
/// generation are file concepts and that crate is not allowed to know what a file is. It exists
/// because the section's own id says only which child column the link is for, and a link resolved
/// against the wrong parent is the one failure in this layer that is a wrong answer rather than a
/// slow one. Section 3.1's staleness rule is *ignore, do not repair*, and this is what gives
/// [`stored_link`] something to check before it believes a payload.
fn encode_link(link: &link::Link, parent: &Reader, edge: &Edge) -> Result<Vec<u8>> {
    let name = edge.parent.as_bytes();
    let mut bytes = Vec::with_capacity(binding_bytes(&edge.parent) + link.bytes());
    bytes.extend_from_slice(&parent.table().generation().to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(edge.parent_column)
            .map_err(|_| invalid("column index overflow"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(name.len())
            .map_err(|_| invalid("a parent name longer than a u32"))?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(name);
    bytes.resize(binding_bytes(&edge.parent), 0);
    link.write(&mut bytes)?;
    Ok(bytes)
}

/// The forward link this child table carries for a column, when it carries one this build can use
/// and the parent it was built against is still the parent being asked about.
///
/// `None` for every reason there might not be one, for the reason [`key_map`] answers the same way.
/// The extra check here is the binding: a link whose stored parent name, column or generation is
/// not the one the caller is asking for is a link built against a table that has since been
/// rewritten, and resolving through it would produce a plausible wrong row rather than an error.
#[must_use]
pub fn stored_link(child: &Reader, parent: &Reader, edge: &Edge) -> Option<link::Link> {
    let table = child.table();
    let id = u64::try_from(edge.child_column).ok()?;
    let held = table
        .sections()
        .iter()
        .find(|section| section.kind == *section::FORWARD_LINK && section.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    let bytes = child.payload(held).ok()?;
    let binding = binding_bytes(&edge.parent);
    if bytes.len() < binding {
        return None;
    }
    let generation = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
    let column = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let length = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    if generation != parent.table().generation()
        || column as usize != edge.parent_column
        || length != edge.parent.len()
        || &bytes[16..16 + length] != edge.parent.as_bytes()
    {
        return None;
    }
    link::Link::read(&bytes[binding..]).ok()
}

/// What the build measured of a relationship's shape, when the child table carries it.
///
/// There is no binding to check, unlike [`stored_link`], because there is nothing here to resolve
/// against the parent. Every number is about the child column and the generation stamp is the whole
/// of what makes one of these current. A caller that wants to know the relationship is still the
/// one it means asks [`stored_link`] as well, which it is doing anyway if it plans to follow it.
#[must_use]
pub fn stored_degrees(child: &Reader, child_column: usize) -> Option<Degrees> {
    let table = child.table();
    let id = u64::try_from(child_column).ok()?;
    let held = table
        .sections()
        .iter()
        .find(|section| section.kind == *section::DEGREES && section.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    Degrees::read(&child.payload(held).ok()?).ok()
}

/// What a key map over this column would have cost, when a build measured one and did not keep it.
///
/// This and [`key_map`] are exclusive: an entry either holds a map or records the absence of one,
/// and which it is comes off the entry rather than out of a payload, so asking this costs nothing.
/// Both answer `None` for a column no build has looked at, which is the third state and is the one
/// where `rudb_links()` should say nothing rather than zero.
#[must_use]
pub fn refused_key_map(reader: &Reader, column: usize) -> Option<(Form, u64)> {
    let (form, bytes) = refused(reader, *section::KEY_MAP, column)?;
    Some((Form::from_tag(form).ok()?, bytes))
}

/// What a forward link for this column would have cost, when a build measured one and did not keep
/// it. The counterpart of [`stored_link`], the way [`refused_key_map`] is the counterpart of
/// [`key_map`].
#[must_use]
pub fn refused_link(child: &Reader, child_column: usize) -> Option<(link::Form, u64)> {
    let (form, bytes) = refused(child, *section::FORWARD_LINK, child_column)?;
    Some((link::Form::from_tag(form).ok()?, bytes))
}

/// The form tag and the size out of a budget record, when the table holds one for this id.
fn refused(reader: &Reader, kind: [u8; 8], id: usize) -> Option<(u8, u64)> {
    let table = reader.table();
    let id = u64::try_from(id).ok()?;
    let held = table.sections().iter().find(|section| section.kind == kind && section.id == id)?;
    if !held.usable(table.generation()) {
        return None;
    }
    Some((u8::try_from(held.flags).ok()?, held.refused()?))
}

/// What the table's sections of one kind cost, leaving out the ids this build is replacing.
fn held_bytes_except(reader: &Reader, kind: [u8; 8], replacing: &[usize]) -> Result<u64> {
    let mut total = 0;
    for held in reader.table().sections() {
        if !held.among(section::GRAPH_KINDS) {
            continue;
        }
        let replaced =
            held.kind == kind && replacing.iter().any(|&id| u64::try_from(id) == Ok(held.id));
        if replaced || !held.usable(reader.table().generation()) {
            continue;
        }
        let Ok(extents) = reader.extents(held) else { continue };
        total += extents.iter().map(|extent| u64::from(extent.length)).sum::<u64>();
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rudb_common::Field;
    use rudb_graph::Rid;
    use rudb_vector::Vector;

    use super::*;
    use crate::Writer;

    fn path(label: &str) -> PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("time advances").as_nanos();
        std::env::temp_dir().join(format!("rudb-graph-{label}-{}-{stamp}.rdb", std::process::id()))
    }

    /// The graph sections of a table, which is every section this module could have written.
    ///
    /// A table carries a summary and a sketch per column out of the write itself now, and a test
    /// about key maps is not about those. Filtering by kind rather than subtracting a count, so a
    /// table whose summaries did not fit the budget does not quietly change what is asserted.
    fn graph_sections(reader: &Reader) -> Vec<&section::Section> {
        reader.table().sections().iter().filter(|held| held.among(section::GRAPH_KINDS)).collect()
    }

    /// A one column table of these keys, written a thousand rows to a part.
    fn table_of(label: &str, keys: &[Option<i64>]) -> PathBuf {
        let path = path(label);
        let mut writer =
            Writer::create(&path, "parent", vec![Field::new("key", LogicalType::BigInt)])
                .expect("new file");
        for part in keys.chunks(1000) {
            let values =
                part.iter().map(|key| key.map_or(Value::Null, Value::BigInt)).collect::<Vec<_>>();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values).expect("keys")])
                    .expect("one column");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        path
    }

    /// Every key in the column resolves to the row that holds it.
    fn resolves(keys: &[Option<i64>], map: &KeyMap) {
        for (rid, key) in keys.iter().enumerate() {
            let Some(key) = *key else { continue };
            let found =
                map.lookup(i128::from(key)).expect("lookup").expect("a key in the column resolves");
            assert_eq!(found, rid as Rid, "key {key} resolved to {found} rather than {rid}");
        }
    }

    #[test]
    fn a_key_map_built_over_a_file_resolves_every_key_to_its_own_row() {
        // The whole point, end to end: the column goes to disk, comes back through the reader, and
        // every key finds the row it was written in. Three thousand rows so that the scan crosses
        // part boundaries, because a build that read the parts in the wrong order would be right
        // for one part and wrong for the rest.
        let keys = (1..=3000_i64).map(Some).collect::<Vec<_>>();
        let path = table_of("identity", &keys);
        let built = build_key_maps(&path, "parent", &[0]).expect("build");
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].form, Form::Identity);
        assert_eq!(built[0].rows, 3000);
        assert!(built[0].distinct);
        assert_eq!(built[0].bytes, wire::HEADER_BYTES, "identity is a header and nothing else");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let map = key_map(&reader, 0).expect("the map is in the file");
        assert_eq!(map.form(), Form::Identity);
        resolves(&keys, &map);
        assert_eq!(map.lookup(0).expect("a key below the column"), None);
        assert_eq!(map.lookup(3001).expect("a key past the column"), None);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_with_gaps_takes_the_bitmap_form_and_still_resolves() {
        let keys = (0..2000_i64).map(|value| Some(value * 4 + 7)).collect::<Vec<_>>();
        let path = table_of("dense", &keys);
        let built = build_key_maps(&path, "parent", &[0]).expect("build");
        assert_eq!(built[0].form, Form::Dense);

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let map = key_map(&reader, 0).expect("the map is in the file");
        resolves(&keys, &map);
        assert_eq!(map.lookup(8).expect("a value in the range but not the column"), None);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_out_of_order_takes_the_sorted_form_and_still_resolves() {
        let keys = (0..1500_i64).map(|value| Some((value * 7919) % 100_003)).collect::<Vec<_>>();
        let path = table_of("sorted", &keys);
        let built = build_key_maps(&path, "parent", &[0]).expect("build");
        assert_eq!(built[0].form, Form::Sorted);
        assert!(built[0].distinct, "the sort settles distinctness for an unordered column");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let map = key_map(&reader, 0).expect("the map is in the file");
        resolves(&keys, &map);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_null_in_the_key_column_does_not_shift_the_rows_after_it() {
        // The failure this whole crate is most exposed to. A null is not a key, but it is a row, so
        // a form that answers with a count of keys below a value answers one short for every row
        // after it. It does not crash and it does not look wrong: it resolves every key to a
        // neighbour of the right row.
        let mut keys = (1..=1200_i64).map(Some).collect::<Vec<_>>();
        keys[3] = None;
        keys[900] = None;
        let path = table_of("nulls", &keys);
        let built = build_key_maps(&path, "parent", &[0]).expect("build");
        assert_eq!(built[0].rows, 1198, "a null is not a key");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let map = key_map(&reader, 0).expect("the map is in the file");
        resolves(&keys, &map);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_with_a_repeat_in_it_is_mapped_and_reported_as_no_parent() {
        // Section 2.3: a parent side that is not unique is not an error and is not a link. It is
        // also not a key map. The repeat here is not next to itself, so only the sort can find it
        // and the bytes are spent before anybody knows, which is why the report carries what it
        // cost and the file does not.
        let mut keys = (1..=500_i64).map(Some).collect::<Vec<_>>();
        keys[200] = Some(7);
        let path = table_of("repeat", &keys);
        let built = build_key_maps(&path, "parent", &[0]).expect("build");
        assert!(!built[0].distinct, "a repeat is observed rather than declared away");
        assert!(!built[0].built, "and a map no rid can be resolved through is not kept");
        assert!(built[0].bytes > 0, "what it would have cost is still reported");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert!(key_map(&reader, 0).is_none(), "no map was written to read back");
        // What is written is the entry that says so, with no bytes behind it. Section 3.7 wants the
        // size to survive the build that decided against it, and fifty six bytes of entry is the
        // whole of what a refusal costs.
        let (form, bytes) = refused_key_map(&reader, 0).expect("the record of what it would cost");
        assert_eq!(form, built[0].form);
        assert_eq!(bytes, built[0].bytes as u64);
        assert_eq!(graph_sections(&reader).len(), 1, "one entry, and no payload");
        assert_eq!(graph_sections(&reader)[0].extents, 0);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_table_with_no_key_map_answers_with_none_rather_than_an_error() {
        // Section 3.1 at the API. Every query has to be answerable with no section in the file, so
        // asking for a map that is not there is a question with an answer and not a failure.
        let path = table_of("absent", &[Some(1), Some(2)]);
        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert!(key_map(&reader, 0).is_none());
        assert!(key_map(&reader, 99).is_none(), "a column that does not exist is not a panic");
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_stale_key_map_is_ignored_and_the_table_still_reads() {
        let path = table_of("stale", &(1..=100_i64).map(Some).collect::<Vec<_>>());
        build_key_maps(&path, "parent", &[0]).expect("build");

        // A second table in the same file moves the file's generation and not this table's, so the
        // map stays current: that is the distinction `Table::generation` exists to make.
        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert!(key_map(&reader, 0).is_some());
        let generation = reader.table().generation();
        drop(reader);

        // And a map stamped against a generation this table is not at is dropped rather than used.
        let held = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let mut entry = *graph_sections(&held).first().copied().expect("the key map");
        assert!(entry.usable(generation));
        entry.generation = generation + 1;
        assert!(!entry.usable(generation), "a rewrite invalidates rather than corrupts");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_torn_key_map_costs_the_shortcut_and_not_the_query() {
        let keys = (1..=200_i64).map(Some).collect::<Vec<_>>();
        let path = table_of("torn", &keys);
        build_key_maps(&path, "parent", &[0]).expect("build");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let extent = reader
            .extents(graph_sections(&reader).first().copied().expect("the key map"))
            .expect("extent table")
            .first()
            .copied()
            .expect("one extent");
        drop(reader);
        let file = fs::OpenOptions::new().write(true).open(&path).expect("reopen to corrupt");
        crate::write_at(&file, extent.offset, &[0xff; 8]).expect("flip the header");
        drop(file);

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert!(key_map(&reader, 0).is_none(), "a payload that does not checksum is not a map");
        assert_eq!(reader.table().rows(), 200, "and the table is untouched");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_column_with_no_integer_key_form_is_refused_by_name() {
        let path = path("varchar");
        let mut writer =
            Writer::create(&path, "parent", vec![Field::new("name", LogicalType::Varchar)])
                .expect("new file");
        let chunk = Chunk::new(vec![
            Vector::from_values(LogicalType::Varchar, &[Value::Varchar("a".into())])
                .expect("one name"),
        ])
        .expect("one column");
        writer.append(&chunk).expect("a part");
        writer.finish().expect("commit");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let error = KeyColumn::new(&reader, 0).expect_err("a string key needs its codes");
        assert!(error.to_string().contains("integer key form"), "{error}");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn several_columns_are_mapped_in_one_commit() {
        let path = path("two_columns");
        let mut writer = Writer::create(
            &path,
            "parent",
            vec![
                Field::required("id", LogicalType::BigInt),
                Field::required("code", LogicalType::Integer),
            ],
        )
        .expect("new file");
        let ids = (1..=400_i64).map(Value::BigInt).collect::<Vec<_>>();
        let codes = (1..=400_i32).map(|code| Value::Integer(code * 3)).collect::<Vec<_>>();
        let chunk = Chunk::new(vec![
            Vector::from_values(LogicalType::BigInt, &ids).expect("ids"),
            Vector::from_values(LogicalType::Integer, &codes).expect("codes"),
        ])
        .expect("two columns");
        writer.append(&chunk).expect("a part");
        writer.finish().expect("commit");

        let built = build_key_maps(&path, "parent", &[0, 1]).expect("build both");
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].form, Form::Identity);
        assert_eq!(built[1].form, Form::Dense);

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert_eq!(graph_sections(&reader).len(), 2, "one commit and two entries");
        assert_eq!(key_map(&reader, 0).expect("the id map").form(), Form::Identity);
        assert_eq!(key_map(&reader, 1).expect("the code map").form(), Form::Dense);
        assert_eq!(
            key_map(&reader, 1).expect("the code map").lookup(9).expect("lookup"),
            Some(2),
            "the third code is the third row"
        );

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_statistics_sections_do_not_count_against_the_graph_budget() {
        // The two shares are ten percent and two percent of the same column bytes, and separate
        // means each counts only what it owns. A graph build that counted summaries would be a
        // graph budget the statistics layer eats, and a table would lose key maps for a reason
        // that has nothing to do with key maps. The kind lists in `section` are what keeps the two
        // apart, and this is the direction of that which lives in this file.
        let keys = (1..=3000_i64).map(Some).collect::<Vec<_>>();
        let path = table_of("apart", &keys);
        crate::stats::build_stats(&path, "parent", &[0]).expect("summaries first");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let statistics = reader
            .table()
            .sections()
            .iter()
            .filter(|held| held.among(section::STATISTICS_KINDS))
            .count();
        assert_eq!(statistics, 2, "a summary and a sketch are in the file");
        assert_eq!(held_bytes(&reader, &[0]).expect("held"), 0, "and neither is the graph's");

        drop(reader);
        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_map_that_does_not_fit_the_budget_is_measured_and_not_written() {
        // A column of twenty thousand even numbers is about the worst case there is for this: the
        // column encodes to a few hundred bytes because it is a run of a constant delta, and the
        // bitmap over it cannot be smaller than one bit per value in its range. So the map is an
        // order of magnitude larger than the column it maps and section 3.7 says it does not go in
        // the file. What comes back is the number, which is the point: a budget that silently drops
        // things teaches nobody anything.
        let keys = (0..100_000_i64).map(|value| Some(value * 8)).collect::<Vec<_>>();
        let path = table_of("budget", &keys);
        let built = build_key_maps(&path, "parent", &[0]).expect("build");
        assert_eq!(built[0].form, Form::Dense);
        assert!(!built[0].built, "a map ten times its column does not fit a tenth of it");
        assert!(built[0].bytes as u64 > built[0].column_bytes, "{built:?}");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert!(key_map(&reader, 0).is_none(), "and no map was written");
        // The record of what it would have cost is what somebody raising `graph_budget` reads, and
        // it is the number the build reported rather than a rounding of it.
        assert_eq!(refused_key_map(&reader, 0), Some((Form::Dense, built[0].bytes as u64)));
        assert_eq!(held_bytes(&reader, &[]).expect("held"), 0, "a record costs the budget nothing");
        drop(reader);

        // The same build against a budget that allows it keeps it, which is what `graph_budget`
        // will be for. Nothing else about the build changes.
        let built = build_key_maps_within(&path, "parent", &[0], 100_000).expect("build");
        assert!(built[0].built);
        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        let map = key_map(&reader, 0).expect("the map is in the file");
        resolves(&keys, &map);

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_budget_admits_the_cheapest_maps_it_can_fit() {
        // Two columns and room for one of them. The ids take the identity form, which is forty
        // bytes whatever the row count, and the scattered keys take the sorted form, which is the
        // keys and a permutation and so is larger than the tenth of the table it would need. So the
        // budget keeps the first and reports what the second would have cost, and it does that
        // although the second was asked for first.
        let path = path("budget_order");
        let mut writer = Writer::create(
            &path,
            "parent",
            vec![
                Field::required("id", LogicalType::BigInt),
                Field::required("code", LogicalType::BigInt),
            ],
        )
        .expect("new file");
        let ids = (1..=100_000_i64).map(Value::BigInt).collect::<Vec<_>>();
        let codes = (1..=100_000_i64)
            .map(|code| Value::BigInt((code * 2_147_483_647) % 999_999_937))
            .collect::<Vec<_>>();
        for part in 0..100 {
            let at = part * 1000;
            let chunk = Chunk::new(vec![
                Vector::from_values(LogicalType::BigInt, &ids[at..at + 1000]).expect("ids"),
                Vector::from_values(LogicalType::BigInt, &codes[at..at + 1000]).expect("codes"),
            ])
            .expect("two columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");

        let built = build_key_maps(&path, "parent", &[1, 0]).expect("build");
        assert_eq!(built[0].column, 1, "the report is in the order it was asked in");
        assert_eq!(built[0].form, Form::Sorted);
        assert!(!built[0].built, "the sorted map did not fit: {built:?}");
        assert!(built[1].built, "the identity map did, and was reached second: {built:?}");

        let reader = Catalog::open(&path).expect("reopen").table("parent").expect("the table");
        assert!(key_map(&reader, 0).is_some());
        assert!(key_map(&reader, 1).is_none());

        fs::remove_file(&path).expect("clean up");
    }

    /// A parent table of `parents` sequential keys and a child table of these foreign keys, with
    /// the parent's key map already built, which is the state section 3.8 says a link build starts
    /// from.
    fn related(label: &str, parents: i64, foreign: &[Option<i64>]) -> PathBuf {
        let path = table_of(label, &(1..=parents).map(Some).collect::<Vec<_>>());
        let mut writer = Writer::open(&path, "child", vec![Field::new("fk", LogicalType::BigInt)])
            .expect("a second table");
        for part in foreign.chunks(1000) {
            let values =
                part.iter().map(|key| key.map_or(Value::Null, Value::BigInt)).collect::<Vec<_>>();
            let chunk =
                Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values).expect("keys")])
                    .expect("one column");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
        build_key_maps(&path, "parent", &[0]).expect("the parent's key map");
        path
    }

    fn edge() -> Edge {
        Edge { child: "child".into(), child_column: 0, parent: "parent".into(), parent_column: 0 }
    }

    /// Reads the link back out of the file and checks every child against the key it was built
    /// from, which is the only assertion that catches a link that is off by a row.
    fn links(path: &PathBuf, foreign: &[Option<i64>]) -> link::Link {
        let catalog = Catalog::open(path).expect("reopen");
        let child = catalog.table("child").expect("the child");
        let parent = catalog.table("parent").expect("the parent");
        let link = stored_link(&child, &parent, &edge()).expect("the link is in the file");
        let map = key_map(&parent, 0).expect("the parent's key map");
        for (rid, key) in foreign.iter().enumerate() {
            let want = key.and_then(|key| map.lookup(i128::from(key)).expect("lookup"));
            assert_eq!(link.forward(rid as Rid), want, "child {rid}");
        }
        link
    }

    /// One row of a two column key, either half of which may be null.
    type Pair = (Option<i64>, Option<i64>);

    /// A two column table of these pairs, a thousand rows to a part.
    fn pairs_into(mut writer: Writer, rows: &[Pair]) {
        let values = |pick: fn(&Pair) -> Option<i64>, part: &[Pair]| {
            let values = part
                .iter()
                .map(|row| pick(row).map_or(Value::Null, Value::BigInt))
                .collect::<Vec<_>>();
            Vector::from_values(LogicalType::BigInt, &values).expect("keys")
        };
        for part in rows.chunks(1000) {
            let chunk = Chunk::new(vec![values(|row| row.0, part), values(|row| row.1, part)])
                .expect("two columns");
            writer.append(&chunk).expect("a part");
        }
        writer.finish().expect("commit");
    }

    #[test]
    fn a_key_over_one_column_is_named_the_way_it_always_was() {
        // The files written before there were pairs name every key by its column's index, so a
        // single column has to come out as exactly that or every one of them stops resolving.
        assert_eq!(key_of(&[0]), Some(0));
        assert_eq!(key_of(&[17]), Some(17));
        assert_eq!(columns_of(17), vec![17]);
        let pair = key_of(&[1, 2]).expect("a pair");
        assert_ne!(pair, key_of(&[2, 1]).expect("a pair"), "the order is part of the key");
        assert_eq!(columns_of(pair), vec![1, 2]);
        assert!(pair > u32::MAX as usize / 2, "a pair never reads as a column index");
        assert_eq!(key_of(&[]), None);
        assert_eq!(key_of(&[0, 1, 2]), None, "nothing is built over three columns");
        assert_eq!(key_of(&[1 << 15, 0]), None, "an index too wide to pack is refused");
    }

    #[test]
    fn two_values_fold_into_one_key_without_two_pairs_ever_meeting() {
        let values = [i128::from(i32::MIN), -1, 0, 1, i128::from(i32::MAX)];
        let mut seen = std::collections::HashSet::new();
        for &high in &values {
            for &low in &values {
                assert!(seen.insert(fold(high, low).expect("fits")), "({high}, {low}) met another");
            }
        }
        let wide = i128::from(i32::MAX) + 1;
        assert!(fold(0, wide).is_err(), "a second value past 32 bits");
        assert!(fold(wide, 0).is_err(), "a first value past 32 bits");
        let span = fold(i128::from(i32::MAX), i128::from(i32::MAX)).expect("fits")
            - fold(i128::from(i32::MIN), i128::from(i32::MIN)).expect("fits");
        assert!(span <= i128::from(u64::MAX), "a key map's keys span no more than a u64");
    }

    #[test]
    fn a_link_over_a_two_column_key_finds_the_row_holding_both_values() {
        // `lineitem(l_partkey, l_suppkey) -> partsupp(ps_partkey, ps_suppkey)` in small: four
        // suppliers for each of five hundred parts, and a child that names a pair of them. The
        // first column alone repeats four times, so this is the case a link over it cannot answer.
        let path = path("pair");
        let fields =
            vec![Field::new("part", LogicalType::BigInt), Field::new("supp", LogicalType::BigInt)];
        let parents = (1..=500_i64)
            .flat_map(|part| (0..4).map(move |at| (Some(part), Some((part + at * 125) % 1000 + 1))))
            .collect::<Vec<_>>();
        pairs_into(Writer::create(&path, "parent", fields.clone()).expect("new file"), &parents);
        let mut children = (0..3000_i64)
            .map(|at| parents[usize::try_from((at * 7) % 2000).expect("small")])
            .collect::<Vec<_>>();
        children[5] = (Some(3), Some(999)); // a part and a supplier that are never paired
        children[6] = (None, Some(4));
        children[7] = (Some(4), None);
        pairs_into(Writer::open(&path, "child", fields).expect("a second table"), &children);

        let key = pair(0, 1).expect("a pair");
        let edge = Edge {
            child: "child".into(),
            child_column: key,
            parent: "parent".into(),
            parent_column: key,
        };
        let report = build_links(&path, std::slice::from_ref(&edge)).expect("build");
        assert!(report[0].built, "{:?}", report[0].note);
        assert_eq!(report[0].linked, 2997, "three children name no parent");

        let catalog = Catalog::open(&path).expect("reopen");
        let parent = catalog.table("parent").expect("the parent");
        let child = catalog.table("child").expect("the child");
        assert!(key_map(&parent, key).is_none(), "a pair's map is built for the link and not kept");
        let link = stored_link(&child, &parent, &edge).expect("the link is in the file");
        for (rid, row) in children.iter().enumerate() {
            let want = parents.iter().position(|held| held == row).map(|at| at as Rid);
            assert_eq!(link.forward(rid as Rid), want, "child {rid} is {row:?}");
        }
        let one = Edge { child_column: 0, parent_column: 0, ..edge };
        assert!(stored_link(&child, &parent, &one).is_none(), "half of the key is not the key");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_clustered_foreign_key_takes_the_monotone_form_and_answers_both_directions() {
        // The shape `lineitem` has against `orders`, which is the relationship section 3.4's
        // arithmetic is about. Four children each of a thousand parents, in order.
        let foreign = (0..4000_i64).map(|child| Some(child / 4 + 1)).collect::<Vec<_>>();
        let path = related("monotone", 1000, &foreign);
        let report = build_links(&path, &[edge()]).expect("build");
        assert_eq!(report.len(), 1);
        assert!(report[0].built, "{:?}", report[0].note);
        assert_eq!(report[0].form, Some(link::Form::Monotone));
        assert_eq!(report[0].children, 4000);
        assert_eq!(report[0].linked, 4000);

        let link = links(&path, &foreign);
        assert_eq!(link.form(), link::Form::Monotone);
        assert_eq!(link.backward(0), Some(0..4), "the first parent's four children");
        assert_eq!(link.backward(999), Some(3996..4000));
        assert_eq!(link.backward(1000), None, "past the last parent");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn an_unclustered_foreign_key_takes_the_packed_form_and_still_resolves() {
        let foreign = (0..3000_i64).map(|child| Some((child * 7) % 1000 + 1)).collect::<Vec<_>>();
        let path = related("packed", 1000, &foreign);
        let report = build_links(&path, &[edge()]).expect("build");
        assert!(report[0].built, "{:?}", report[0].note);
        assert_eq!(report[0].form, Some(link::Form::Packed));

        let link = links(&path, &foreign);
        assert_eq!(link.backward(0), None, "the packed form answers one direction");
        // Ten bits a child, a min and a max per part, and the header. The check is that it is a rid
        // per child and not a byte per child, because a link stored as a u64 array would also pass
        // every assertion above it.
        assert!(link.bytes() < 3000 * 2 + 3 * 16, "{} bytes is not bit-packed", link.bytes());

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_built_link_leaves_the_shape_of_the_relationship_beside_it() {
        // The same clustered shape as the monotone test, so the expected numbers are arithmetic
        // rather than an observation: four children each of a thousand parents, in order.
        let foreign = (0..4000_i64).map(|child| Some(child / 4 + 1)).collect::<Vec<_>>();
        let path = related("degrees", 1000, &foreign);
        let report = build_links(&path, &[edge()]).expect("build");
        assert!(report[0].built, "{:?}", report[0].note);
        let measured = report[0].degrees.as_ref().expect("the build measured it");
        assert!((measured.mean() - 4.0).abs() < 1e-9);

        let catalog = Catalog::open(&path).expect("reopen");
        let child = catalog.table("child").expect("the child");
        let held = stored_degrees(&child, 0).expect("it is in the file");
        assert_eq!(&held, measured, "what the build measured is what the file holds");
        assert_eq!(held.parents(), 1000);
        assert_eq!(held.highest(), 4);
        assert!(held.total(), "every child found a parent");
        assert!(held.unique(), "and the parent key is why there is a link at all");
        // Three thousand nine hundred and ninety nine steps between adjacent children, of which the
        // nine hundred and ninety nine that cross into the next parent move by one and the rest
        // stay put. Which is what a clustered foreign key is, expressed as a number.
        let near = held.locality().expect("something to gather");
        assert!((near - 999.0 / 3999.0).abs() < 1e-9, "{near}");
        assert!(stored_degrees(&child, 1).is_none(), "and no other column has one");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_foreign_key_that_matches_nothing_is_a_child_with_no_parent() {
        // Not an error and not a refusal. A foreign key that is not total is legal, and what it
        // costs is the monotone form, because every bit of that vector is already spoken for.
        let foreign = vec![Some(1), Some(2), None, Some(9999), Some(3)];
        let path = related("orphans", 10, &foreign);
        let report = build_links(&path, &[edge()]).expect("build");
        assert!(report[0].built, "{:?}", report[0].note);
        assert_eq!(report[0].form, Some(link::Form::Packed));
        assert_eq!(report[0].children, 5);
        assert_eq!(report[0].linked, 3, "the null and the key that matches nothing are not links");

        let link = links(&path, &foreign);
        assert_eq!(link.forward(2), None, "a null is not a link");
        assert_eq!(link.forward(3), None, "a key that matches nothing is not a link");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_parent_with_no_key_map_is_a_relationship_with_no_link_rather_than_an_error() {
        // Section 3.8's ordering is the reason: the key map has to exist first, and a checkpoint
        // that has not built one yet is a normal state rather than a broken one.
        let path = table_of("unmapped", &(1..=100_i64).map(Some).collect::<Vec<_>>());
        let mut writer = Writer::open(&path, "child", vec![Field::new("fk", LogicalType::BigInt)])
            .expect("a second table");
        let values = (1..=100_i64).map(Value::BigInt).collect::<Vec<_>>();
        writer
            .append(
                &Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values).expect("keys")])
                    .expect("one column"),
            )
            .expect("a part");
        writer.finish().expect("commit");

        let report = build_links(&path, &[edge()]).expect("build");
        assert!(!report[0].built);
        assert_eq!(report[0].note.as_deref(), Some("no key map is stored for parent"));

        let catalog = Catalog::open(&path).expect("reopen");
        let child = catalog.table("child").expect("the child");
        let parent = catalog.table("parent").expect("the parent");
        assert!(stored_link(&child, &parent, &edge()).is_none());

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_link_asked_for_against_the_wrong_parent_is_not_handed_over() {
        // The binding check. The section's own id says which child column the link is for and
        // nothing about which table it points into, so a caller that asked with a different parent
        // would otherwise be handed rids of a table it never named.
        let foreign = (0..500_i64).map(|child| Some(child / 5 + 1)).collect::<Vec<_>>();
        let path = related("binding", 100, &foreign);
        build_links(&path, &[edge()]).expect("build");

        let catalog = Catalog::open(&path).expect("reopen");
        let child = catalog.table("child").expect("the child");
        let parent = catalog.table("parent").expect("the parent");
        assert!(stored_link(&child, &parent, &edge()).is_some());
        let wrong = Edge { parent: "child".into(), ..edge() };
        assert!(stored_link(&child, &parent, &wrong).is_none(), "a different parent name");
        let wrong = Edge { parent_column: 1, ..edge() };
        assert!(stored_link(&child, &parent, &wrong).is_none(), "a different parent column");
        let wrong = Edge { child_column: 1, ..edge() };
        assert!(stored_link(&child, &parent, &wrong).is_none(), "a different child column");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_link_that_does_not_fit_the_budget_is_reported_rather_than_stored() {
        // Zero percent, which the floor lifts to sixty four kilobytes, against a packed link over
        // sixty thousand children at ten bits each, which is seventy five.
        let foreign = (0..60_000_i64).map(|child| Some((child * 7) % 1000 + 1)).collect::<Vec<_>>();
        let path = related("budget", 1000, &foreign);
        let report = build_links_within(&path, &[edge()], 0).expect("build");
        assert!(!report[0].built);
        assert!(report[0].bytes > 0, "the report says what a larger budget would buy");
        assert!(report[0].note.as_deref().unwrap_or_default().contains("budget"), "{report:?}");

        let catalog = Catalog::open(&path).expect("reopen");
        let child = catalog.table("child").expect("the child");
        let parent = catalog.table("parent").expect("the parent");
        assert!(stored_link(&child, &parent, &edge()).is_none());
        // Measured before it was refused, and not written, because the shape of a relationship the
        // file cannot follow describes a plan nobody can make.
        assert!(report[0].degrees.is_some(), "it was measured");
        assert!(stored_degrees(&child, 0).is_none(), "and not written");
        // What does survive is the size and the form, which is exit criterion 3 of G3: somebody
        // deciding whether to raise `graph_budget` reads this rather than rebuilding to find out.
        assert_eq!(refused_link(&child, 0), Some((link::Form::Packed, report[0].bytes as u64)));

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn the_budget_keeps_the_link_that_saves_the_larger_hash_table() {
        // Two links out of one child that cannot both fit under the sixty four kilobyte floor. The
        // one to a thousand parents is ten bits a child and about 57 kilobytes, the one to four is
        // two bits and about 12. By child rows per byte the small one wins, and it saves a hash
        // table of four rows. The large one saves a thousand, which is what the budget is for.
        let path = table_of("rank", &(1..=1000).map(Some).collect::<Vec<_>>());
        let small = [Field::new("key", LogicalType::BigInt)];
        let mut writer = Writer::open(&path, "small", small.to_vec()).expect("a second table");
        let keys = (1..=4).map(Value::BigInt).collect::<Vec<_>>();
        let chunk =
            Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &keys).expect("keys")])
                .expect("one column");
        writer.append(&chunk).expect("a part");
        writer.finish().expect("commit");
        let rows = (0..45_000_i64)
            .map(|child| (Some((child * 7) % 1000 + 1), Some(child % 4 + 1)))
            .collect::<Vec<_>>();
        let fields = vec![
            Field::new("large", LogicalType::BigInt),
            Field::new("small", LogicalType::BigInt),
        ];
        pairs_into(Writer::open(&path, "child", fields).expect("a third table"), &rows);
        build_key_maps(&path, "parent", &[0]).expect("the large key map");
        build_key_maps(&path, "small", &[0]).expect("the small key map");

        let edges = [
            edge(),
            Edge {
                child: "child".into(),
                child_column: 1,
                parent: "small".into(),
                parent_column: 0,
            },
        ];
        let report = build_links_within(&path, &edges, 0).expect("build");
        assert!(report[1].bytes < report[0].bytes, "the small link is the cheaper one");
        assert!(
            (report[0].bytes + report[1].bytes) as u64 > BUDGET_FLOOR,
            "the two have to not fit together for this to test anything"
        );
        assert_eq!((report[0].parents, report[1].parents), (1000, 4));
        assert!(report[0].built, "the link that saves a thousand rows was turned away: {report:?}");
        assert!(!report[1].built, "the link that saves four rows was kept instead");

        fs::remove_file(&path).expect("clean up");
    }

    #[test]
    fn a_parent_whose_key_repeats_gets_no_link_at_all() {
        // Section 2.3's verification, which is the one check in this layer that is about
        // correctness rather than speed: a link over a non-unique parent resolves to one of the
        // rows that held the key, and which one is an accident of the build.
        let path = table_of("repeats", &[Some(1), Some(1), Some(2)]);
        let mut writer = Writer::open(&path, "child", vec![Field::new("fk", LogicalType::BigInt)])
            .expect("a second table");
        let values = [Value::BigInt(1), Value::BigInt(2)];
        writer
            .append(
                &Chunk::new(vec![Vector::from_values(LogicalType::BigInt, &values).expect("keys")])
                    .expect("one column"),
            )
            .expect("a part");
        writer.finish().expect("commit");
        build_key_maps(&path, "parent", &[0]).expect("the parent's key map");

        let report = build_links(&path, &[edge()]).expect("build");
        assert!(!report[0].built);
        assert_eq!(report[0].note.as_deref(), Some("no key map is stored for parent"));

        fs::remove_file(&path).expect("clean up");
    }
}
