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
use rudb_graph::{Form, KeyMap, Keys, wire};
use rudb_vector::Chunk;

use crate::section::{self, Attachment};
use crate::{Catalog, Reader, invalid, type_tag};

/// One column of a committed table, scanned in `rid` order.
///
/// A `rid` is a row's position in append order, and the parts of a table are in append order, so a
/// scan of the parts in order is a scan in `rid` order and there is nothing to look up. That is the
/// whole of the correspondence and it is worth stating, because a build that read the parts in any
/// other order would produce a map that resolved every key to the wrong row without failing.
#[derive(Debug)]
pub struct KeyColumn<'a> {
    reader: &'a Reader,
    column: usize,
}

impl<'a> KeyColumn<'a> {
    /// Names a column of a table as the key column of a relationship's parent side.
    ///
    /// # Errors
    ///
    /// If there is no such column, or if its type has no integer key form. `VARCHAR` is the second
    /// of those today: section 2.2 says a string key is mapped through its dictionary codes rather
    /// than its text, and the code path is not built yet. None of TPC-H's eight relationships needs
    /// it, so it is refused by name rather than approximated.
    pub fn new(reader: &'a Reader, column: usize) -> Result<Self> {
        let fields = reader.table().fields();
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
        Ok(Self { reader, column })
    }
}

impl Keys for KeyColumn<'_> {
    fn scan(&self, each: &mut dyn FnMut(Option<i128>) -> Result<()>) -> Result<()> {
        for part in 0..self.reader.parts() {
            let chunk = self.reader.read(part, &[self.column])?;
            let values = chunk.column(0)?;
            for row in 0..chunk.len() {
                each(key_at(&chunk, values, row)?)?;
            }
        }
        Ok(())
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
fn key_at(chunk: &Chunk, values: &rudb_vector::Vector, row: usize) -> Result<Option<i128>> {
    if let Some(key) = values.signed_at(row) {
        return Ok(Some(key));
    }
    match chunk.value_at(row, 0) {
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
        let payload = wire::encode(&map, type_tag(&reader.table().fields()[column].ty)?)?;
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
    let attachments = payloads
        .iter()
        .zip(&keep)
        .filter(|&(_, &keep)| keep)
        .map(|((column, payload), _)| {
            Ok(Attachment {
                kind: *section::KEY_MAP,
                id: u64::try_from(*column).map_err(|_| invalid("column index overflow"))?,
                flags: payload.flags,
                header_bytes: payload.header_bytes,
                bytes: &payload.bytes,
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
    let mut total = 0;
    for held in reader.table().sections() {
        if !held.among(section::GRAPH_KINDS) {
            continue;
        }
        let replaced = held.kind == *section::KEY_MAP
            && replacing.iter().any(|&column| u64::try_from(column) == Ok(held.id));
        if replaced || !held.usable(reader.table().generation()) {
            continue;
        }
        let Ok(extents) = reader.extents(held) else { continue };
        total += extents.iter().map(|extent| u64::from(extent.length)).sum::<u64>();
    }
    Ok(total)
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
    if tag != type_tag(&table.fields().get(column)?.ty).ok()? {
        return None;
    }
    Some(map)
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
        assert!(key_map(&reader, 0).is_none(), "nothing was written to read back");
        assert!(reader.table().sections().is_empty());

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
        let mut entry = Catalog::open(&path)
            .expect("reopen")
            .table("parent")
            .expect("the table")
            .table()
            .sections()[0];
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
            .extents(&reader.table().sections()[0])
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
        assert_eq!(reader.table().sections().len(), 2, "one commit and two entries");
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
        assert!(reader.table().sections().is_empty(), "and nothing was written");
        assert!(key_map(&reader, 0).is_none());
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
}
