//! The kernel table: what one row costs, per physical layout, per form pair, per null rate.
//!
//! This is the measurement sub-milestone 2b closes on. The five kernel files were written one at a
//! time, each with a number quoted in the pull request that landed it, and a number quoted in a
//! pull request is a number that exists once. What 2b asks for instead is a table that can be run
//! again on a machine of record, which is what makes a regression something the next change trips
//! over rather than something somebody notices two milestones later.
//!
//! Five tables, and each of them answers a question the kernels were written to have an answer to.
//!
//! Comparison and arithmetic, every physical layout, four form pairs, three null rates. The target
//! in 2b is fixed width flat against flat comparison under one nanosecond a row, and the reason
//! the null rates are a column rather than a footnote is lesson five: matching on the validity enum
//! inside the row loop cost about three quarters of a nanosecond a row before the three way
//! dispatch was hoisted out of it, so a change that puts it back shows up here as the zero percent
//! column and the fifty percent column converging.
//!
//! String comparison, at prefix decides and at prefix ties. The four byte prefix in a
//! [`StringView`](rudb_vector::StringView) is the whole reason the string comparison is not a
//! pointer chase, and the two cases are the one where it answers and the one where it does not.
//!
//! The compaction surface, swept over selectivity by chunk shape. The doc comment on
//! [`Chunk::compact`](rudb_vector::Chunk::compact) states the answer this table produces, which is
//! that the deciding variable is how many later passes there are over the kept rows rather than the
//! selectivity, and until now that answer lived only in that comment. Here it is a table with a
//! crossover depth in the last column, which is the number the operator that has to choose wants.
//!
//! The vector size sweep, 256 through 4096. `VECTOR_SIZE` is 1024 and the reason it is 1024 rather
//! than 4096 is supposed to be that three vectors of a comparison at 1024 rows fit in 32 KB of L1d
//! and at 4096 rows do not. That is a claim about a machine, so it is measured on the machine
//! rather than asserted.
//!
//! # What this is not
//!
//! It is not `tamnd/rudb-bench`, and rule ten from `spec/15-rudb-bench.md` is printed under every
//! run of it for that reason. A kernel number is an explanation of a query number. On its own it is
//! a fact about a loop.

use std::hint::black_box;
use std::path::Path;

use rudb_common::{LogicalType, Value};
use rudb_kernels::{Comparison, call, compare, fallback};
use rudb_vector::{Chunk, Data, Selection, StringColumn, VECTOR_SIZE, Validity, Vector};

use crate::timing::{Number, build_line, rebuild, shared_caveats, time};

/// The row count every table but the sweep uses, which is the one the engine runs at.
const ROWS: usize = VECTOR_SIZE;

/// How many distinct values a dictionary vector in these tables points at.
///
/// Sixty four rather than two or a thousand because it is the shape a real dictionary column has
/// after a scan of a low cardinality column, and because it keeps the value vector inside L1 so the
/// dictionary columns are measuring the indirection rather than measuring a cache miss.
const DISTINCT: usize = 64;

/// The null rates the tables sweep, as percentages.
///
/// Zero because it is the path with no validity work at all, one because it is what a real column
/// looks like and it is the rate at which a kernel that branches per row is at its worst, and fifty
/// because it is where a word at a time validity read stops being able to skip anything.
const NULL_RATES: &[usize] = &[0, 1, 50];

/// The form pairs the tables have a column for.
///
/// There are sixteen. Four of these are the ones a scan and a binder produce today and that the
/// comparison kernel has a loop for: two scanned columns, a scanned column against a literal, a
/// literal against a scanned column, and a low cardinality or filtered column against a literal.
///
/// The fifth, a dictionary against a flat column, is here because it has no loop. A column is
/// worth a fifth of the table if what the table is for is saying which loop to write next, and a
/// pair that is missing is much easier to act on as a number in a column than as a zero in a
/// counter nobody printed. Every cell that fell through to the row at a time path is printed with
/// a star next to it, from [`fallback`], so the ones that are the oracle's numbers rather than a
/// kernel's say so.
const PAIRS: &[(&str, Pair)] = &[
    ("flat/flat", Pair::FlatFlat),
    ("flat/const", Pair::FlatConstant),
    ("const/flat", Pair::ConstantFlat),
    ("dict/const", Pair::DictionaryConstant),
    ("dict/flat", Pair::DictionaryFlat),
];

/// The selectivities the compaction surface is swept over.
const KEEP: &[f64] = &[0.1, 1.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0];

/// The vector sizes the last table sweeps.
const SIZES: &[usize] = &[256, 512, 1024, 2048, 4096];

/// Builds a run of values: how many rows, and an offset that shifts every one of them.
type Build = fn(usize, i64) -> Data;

/// Which two forms a cell compares.
#[derive(Clone, Copy, Debug)]
enum Pair {
    FlatFlat,
    FlatConstant,
    ConstantFlat,
    DictionaryConstant,
    DictionaryFlat,
}

/// One physical layout, and the material for building a vector of it.
struct Case {
    /// The [`Data`] variant a vector of this type has. The coverage test checks this list against
    /// `for_each_layout!(all, ...)`, so a layout added to the vector crate and not added here fails
    /// a test rather than quietly going unmeasured.
    layout: &'static str,
    /// What the type column prints.
    sql: &'static str,
    /// The type a vector of the run below carries.
    ty: LogicalType,
    /// Whether `+` is defined on it. Boolean, interval and varchar are the three that it is not,
    /// and the arithmetic table leaves them out rather than printing a row of dashes.
    arithmetic: bool,
    /// Builds a run of `rows` values. The offset shifts them, so the right side of a comparison is
    /// not bit for bit the left side, which would make every row's answer the same and hand the
    /// branch predictor a workload no column has.
    data: Build,
}

/// One measured cell, in the flat form the tables are printed from and the JSON is written from.
///
/// Flat rather than a table shaped structure per table, because there are five tables and a reader
/// that wants to diff two runs wants one list of records with keys on it, not five shapes.
struct Cell {
    /// Which table, so a consumer can group without knowing what order they were printed in.
    table: &'static str,
    /// What was measured: the layout name, the string case, the chunk shape, the operation.
    case: String,
    /// The second key: the SQL type, the selectivity, the vector size.
    detail: String,
    /// The form pair, or the stage of the select against compact comparison.
    variant: String,
    /// Percent of the rows that are null.
    nulls: usize,
    /// How many rows the time was divided by. For the compaction surface this is the kept rows,
    /// because a cost per row of a chunk that was thrown away is not a cost anybody pays.
    rows: usize,
    /// Nanoseconds per one of those rows.
    nanos: f64,
    /// The interquartile range as a fraction of the median.
    spread: f64,
    /// Whether the kernel under this cell took the row at a time path. A true here means the
    /// number is the oracle's number and not a kernel's, which is a different fact entirely.
    fell_back: bool,
}

/// Produce the tables.
///
/// A debug build of this would be measuring the borrow checker's leftovers rather than a kernel, so
/// it re-runs itself under the `bench` profile, the same way the front end table does.
pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|arg| arg == "--json");
    if cfg!(debug_assertions) {
        return rebuild(root, "kernels", args);
    }

    let mut cells = Vec::new();
    binary_tables(&mut cells);
    string_table(&mut cells);
    compaction_surface(&mut cells);
    size_sweep(&mut cells);

    if json {
        print!("{}", as_json(&cells));
    } else {
        report(&cells);
    }
    Ok(())
}

/// The layouts, in the order the tables print them.
///
/// A function rather than a `const` so the entries can be written with the type constructors rather
/// than with whatever subset of them is const evaluable, which would put the decision of which
/// layouts are measured in the hands of the language rather than in this list.
fn cases() -> Vec<Case> {
    /// One numeric layout, whose run is the pattern cast to its element type.
    macro_rules! numeric {
        ($layout:ident, $sql:expr, $ty:expr, $native:ty) => {
            Case {
                layout: stringify!($layout),
                sql: $sql,
                ty: $ty,
                arithmetic: true,
                data: |rows, offset| {
                    Data::$layout(
                        (0..rows)
                            .map(|row| {
                                // One of the fifteen layouts is the one the pattern is already
                                // written in, so its cast is a no-op and the lint is right about
                                // it. Writing that one out by hand to avoid the allow would be
                                // going back to a list per layout to save a line.
                                #[allow(clippy::unnecessary_cast)]
                                let value = pattern(row, offset) as $native;
                                value
                            })
                            .collect::<Vec<$native>>()
                            .into(),
                    )
                },
            }
        };
    }

    vec![
        Case {
            layout: "Bool",
            sql: "BOOLEAN",
            ty: LogicalType::Boolean,
            arithmetic: false,
            data: |rows, offset| {
                Data::Bool(
                    (0..rows)
                        .map(|row| pattern(row, offset) % 2 == 0)
                        .collect::<Vec<bool>>()
                        .into(),
                )
            },
        },
        numeric!(Int8, "TINYINT", LogicalType::TinyInt, i8),
        numeric!(Int16, "SMALLINT", LogicalType::SmallInt, i16),
        numeric!(Int32, "INTEGER", LogicalType::Integer, i32),
        numeric!(Int64, "BIGINT", LogicalType::BigInt, i64),
        numeric!(Int128, "HUGEINT", LogicalType::HugeInt, i128),
        numeric!(UInt8, "UTINYINT", LogicalType::UTinyInt, u8),
        numeric!(UInt16, "USMALLINT", LogicalType::USmallInt, u16),
        numeric!(UInt32, "UINTEGER", LogicalType::UInteger, u32),
        numeric!(UInt64, "UBIGINT", LogicalType::UBigInt, u64),
        numeric!(UInt128, "UHUGEINT", LogicalType::UHugeInt, u128),
        numeric!(Float32, "FLOAT", LogicalType::Float, f32),
        numeric!(Float64, "DOUBLE", LogicalType::Double, f64),
        Case {
            layout: "Interval",
            sql: "INTERVAL",
            ty: LogicalType::Interval,
            arithmetic: false,
            data: |rows, offset| {
                Data::Interval(
                    (0..rows)
                        .map(|row| {
                            let value = pattern(row, offset);
                            (value as i32, value as i32, value)
                        })
                        .collect::<Vec<(i32, i32, i64)>>()
                        .into(),
                )
            },
        },
        Case {
            layout: "Varlen",
            sql: "VARCHAR",
            ty: LogicalType::Varchar,
            arithmetic: false,
            // The same case the string table calls prefix decides and inline, so the varchar row of
            // the comparison table and the first row of the string table are the same measurement
            // twice and should print the same number. They are both here because one of them
            // belongs in a table of every layout and the other belongs next to the case it is being
            // contrasted with.
            data: |rows, offset| strings(rows, offset, Prefix::Decides, Width::Inline),
        },
    ]
}

/// A deterministic value for a row, in zero to sixty three, shifted by an offset.
///
/// Deterministic because a table that changes between two runs of the same binary is a table nobody
/// can use to compare two binaries. Scrambled rather than sequential because a sequential run makes
/// every comparison in the vector answer the same way, which is a branch predictor's best case and
/// no column's. Sixty four wide because every integer width in the list holds it, and because the
/// sum of two of them fits in the narrowest one, so the arithmetic table is measuring an addition
/// rather than measuring an overflow error path.
fn pattern(row: usize, offset: i64) -> i64 {
    (((row as u64).wrapping_mul(2_654_435_761) >> 7) % DISTINCT as u64) as i64 + offset
}

/// A scramble in zero to ten thousand, which is what the null rates are drawn against.
fn scramble(row: usize) -> usize {
    ((row as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize % 10_000
}

/// A validity with a percentage of the rows null, spread rather than in a block.
///
/// Spread because a block of nulls is a run of validity words that are all zero, and a kernel that
/// reads a word at a time skips the lot, which measures the one case a real column almost never
/// has. Drawn against a scramble in ten thousand rather than in sixty four so that one percent is
/// one percent rather than rounding to nothing.
fn validity(rows: usize, percent: usize) -> Validity {
    if percent == 0 {
        return Validity::AllValid;
    }
    Validity::from_iter(rows, |row| scramble(row) >= percent * 100)
}

/// How the two sides of a string comparison differ.
#[derive(Clone, Copy)]
enum Prefix {
    /// They differ inside the first four bytes, so the prefix in the view decides and the kernel
    /// never follows a pointer.
    Decides,
    /// They share more than four bytes, so the prefix ties and the comparison reads the bytes.
    Ties,
}

/// Whether a string fits in its view or lives in the arena.
#[derive(Clone, Copy)]
enum Width {
    /// Twelve bytes or fewer, so the whole string is in the sixteen byte view.
    Inline,
    /// Longer, so the view holds a prefix and an offset and the bytes are a cache line away.
    Heap,
}

/// A run of strings of one shape.
fn strings(rows: usize, offset: i64, prefix: Prefix, width: Width) -> Data {
    let mut column = StringColumn::with_capacity(rows);
    for row in 0..rows {
        let value = pattern(row, offset);
        let mut text = match prefix {
            Prefix::Decides => format!("{value:04}"),
            Prefix::Ties => format!("prefix-{value:04}"),
        };
        if matches!(width, Width::Heap) {
            text.push_str("-and-enough-bytes-after-it-to-reach-the-arena");
        }
        column.push(&text);
    }
    Data::Varlen(column)
}

/// A flat vector of a case, with the null rate applied.
fn flat(case: &Case, rows: usize, offset: i64, nulls: usize) -> Vector {
    Vector::flat(case.ty.clone(), (case.data)(rows, offset))
        .expect("a case builds the layout its own type calls for")
        .with_validity(validity(rows, nulls))
}

/// A dictionary vector of a case over [`DISTINCT`] values, with the null rate applied.
///
/// The validity goes on the dictionary rather than on the values it points at, which is the case
/// that cannot be composed away and the case a filter produces.
fn dictionary(case: &Case, rows: usize, offset: i64, nulls: usize) -> Vector {
    let values = Vector::flat(case.ty.clone(), (case.data)(DISTINCT, offset))
        .expect("a case builds the layout its own type calls for");
    let codes = (0..rows).map(|row| pattern(row, 0) as u32).collect();
    Vector::dictionary(codes, values)
        .expect("every code is inside a dictionary of DISTINCT values")
        .with_validity(validity(rows, nulls))
}

/// The two operands of one form pair.
///
/// A constant operand is never null, because a null constant is a whole vector of nulls and the
/// answer to any comparison with it is known without a loop, which is a fast path rather than a
/// measurement. Every flat and dictionary operand in the pair carries the row's null rate.
fn operands(case: &Case, which: Pair, rows: usize, nulls: usize) -> (Vector, Vector) {
    // Read out of the run with no nulls in it rather than out of the one the row is using. A
    // constant built from a null row is a constant whose validity is all invalid, every comparison
    // with one is null without the data being read at all, and the cell would have been a
    // measurement of that fast path filed under the fifty percent row of a kernel it never called.
    let middle = flat(case, rows, 0, 0).value_at(rows / 2);
    let constant = || Vector::constant(case.ty.clone(), middle.clone(), rows);
    match which {
        Pair::FlatFlat => (flat(case, rows, 0, nulls), flat(case, rows, 1, nulls)),
        Pair::FlatConstant => (flat(case, rows, 0, nulls), constant()),
        Pair::ConstantFlat => (constant(), flat(case, rows, 1, nulls)),
        Pair::DictionaryConstant => (dictionary(case, rows, 0, nulls), constant()),
        Pair::DictionaryFlat => (dictionary(case, rows, 0, nulls), flat(case, rows, 1, nulls)),
    }
}

/// Time one call, once it has been shown to answer.
///
/// The call is made once outside the timed region and its result is checked, so a cell is never a
/// time for an error return, which is a fast path that does none of the work the column claims to
/// be timing. The fallback counters are reset around that one call, so the cell also knows whether
/// the kernel it is about to time has a loop for this pair at all.
fn cell(rows: usize, mut once: impl FnMut() -> Result<Vector, String>) -> Option<(Number, bool)> {
    fallback::reset();
    let answer = once().ok()?;
    if answer.len() != rows {
        return None;
    }
    let fell_back = !fallback::hot().is_empty();
    let number = time(|| {
        drop(black_box(once()));
    });
    Some((number, fell_back))
}

/// The comparison table and the arithmetic table.
fn binary_tables(cells: &mut Vec<Cell>) {
    for case in cases() {
        for &nulls in NULL_RATES {
            for &(name, which) in PAIRS {
                let (left, right) = operands(&case, which, ROWS, nulls);
                if let Some((number, fell_back)) = cell(ROWS, || {
                    compare(Comparison::Less, black_box(&left), black_box(&right))
                        .map_err(|e| e.to_string())
                }) {
                    cells.push(Cell {
                        table: "comparison",
                        case: case.layout.to_string(),
                        detail: case.sql.to_string(),
                        variant: name.to_string(),
                        nulls,
                        rows: ROWS,
                        nanos: number.per(ROWS),
                        spread: number.relative(),
                        fell_back,
                    });
                }

                if !case.arithmetic {
                    continue;
                }
                let args = vec![left, right];
                let returns = case.ty.clone();
                if let Some((number, fell_back)) = cell(ROWS, || {
                    call("+", black_box(&args), black_box(&returns)).map_err(|e| e.to_string())
                }) {
                    cells.push(Cell {
                        table: "arithmetic",
                        case: case.layout.to_string(),
                        detail: case.sql.to_string(),
                        variant: name.to_string(),
                        nulls,
                        rows: ROWS,
                        nanos: number.per(ROWS),
                        spread: number.relative(),
                        fell_back,
                    });
                }
            }
        }
    }
}

/// The string table, which is the prefix question.
fn string_table(cells: &mut Vec<Cell>) {
    let shapes: &[(&str, Build)] = &[
        ("prefix decides, inline", |rows, offset| {
            strings(rows, offset, Prefix::Decides, Width::Inline)
        }),
        ("prefix ties, inline", |rows, offset| strings(rows, offset, Prefix::Ties, Width::Inline)),
        ("prefix decides, arena", |rows, offset| {
            strings(rows, offset, Prefix::Decides, Width::Heap)
        }),
        ("prefix ties, arena", |rows, offset| strings(rows, offset, Prefix::Ties, Width::Heap)),
    ];
    for &(name, data) in shapes {
        let case = Case {
            layout: "Varlen",
            sql: "VARCHAR",
            ty: LogicalType::Varchar,
            arithmetic: false,
            data,
        };
        for &nulls in NULL_RATES {
            for &(pair, which) in PAIRS {
                let (left, right) = operands(&case, which, ROWS, nulls);
                if let Some((number, fell_back)) = cell(ROWS, || {
                    compare(Comparison::Less, black_box(&left), black_box(&right))
                        .map_err(|e| e.to_string())
                }) {
                    cells.push(Cell {
                        table: "strings",
                        case: name.to_string(),
                        detail: "VARCHAR".to_string(),
                        variant: pair.to_string(),
                        nulls,
                        rows: ROWS,
                        nanos: number.per(ROWS),
                        spread: number.relative(),
                        fell_back,
                    });
                }
            }
        }
    }
}

/// The four columns the compaction surface measures, which are the four numbers the decision needs.
///
/// Build and pass are separated rather than measured together at each pipeline depth for two
/// reasons. One is that a build has to clone the source chunk, because both [`Chunk::select`] and
/// [`Chunk::compact`] take it by value, and a clone inside a timed region that also contains the
/// passes would put a memcpy of the whole chunk into every depth. The other is that depth is then
/// an axis of the answer rather than an axis of the measurement, so the crossover depth comes out
/// exactly instead of being bracketed by whichever depths happened to be swept.
struct Surface {
    select_build: Stage,
    select_pass: Stage,
    compact_build: Stage,
    compact_pass: Stage,
}

/// One of those four numbers, with the spread that decides whether a difference between two of them
/// is a difference at all.
#[derive(Clone, Copy, Debug)]
struct Stage {
    /// Nanoseconds a kept row.
    nanos: f64,
    /// The interquartile range of the same measurement, in the same unit.
    iqr: f64,
}

/// What the last column of the compaction table says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Crossover {
    /// Compacting never pays for itself, however many passes there are. This is what a chunk with a
    /// string column in it says at almost every selectivity, because copying the bytes is most of
    /// what compaction costs and the indirection it removes is worth less than that however many
    /// times it is read.
    Never,
    /// The two pass columns are closer together than the spread of the measurement that produced
    /// them, so there is no crossover to report rather than a large one or a small one.
    Noise,
    /// Compacting has paid for itself after this many later passes over the kept rows. Zero means
    /// it was already cheaper to build.
    At(u32),
}

impl Surface {
    /// How many later passes over the kept rows it takes before compacting has paid for itself.
    ///
    /// The gap between the two pass columns is a difference of two separately timed numbers that
    /// are often close, and a crossover depth is that gap in a denominator, so a gap the size of the
    /// measurement's own spread produces a depth that swings between seven and never between two
    /// runs of the same binary. That is not a slow kernel, it is a number that was not measured, and
    /// a table that prints one is worse than a table that says so.
    fn crossover(&self) -> Crossover {
        let saved = self.select_pass.nanos - self.compact_pass.nanos;
        if saved <= 0.0 {
            return Crossover::Never;
        }
        if saved < self.select_pass.iqr + self.compact_pass.iqr {
            return Crossover::Noise;
        }
        let owed = self.compact_build.nanos - self.select_build.nanos;
        if owed <= 0.0 {
            return Crossover::At(0);
        }
        Crossover::At((owed / saved).ceil() as u32)
    }
}

/// One timing divided down to the kept row, spread and all.
fn stage(number: Number, kept: usize) -> Stage {
    Stage { nanos: number.per(kept), iqr: if kept == 0 { 0.0 } else { number.iqr / kept as f64 } }
}

/// The compaction surface, swept over selectivity by chunk shape.
fn compaction_surface(cells: &mut Vec<Cell>) {
    let shapes: &[(&str, &[LogicalType])] = &[
        ("1 bigint", &[LogicalType::BigInt]),
        ("2 bigint", &[LogicalType::BigInt, LogicalType::BigInt]),
        (
            "4 bigint",
            &[LogicalType::BigInt, LogicalType::BigInt, LogicalType::BigInt, LogicalType::BigInt],
        ),
        ("2 bigint + varchar", &[LogicalType::BigInt, LogicalType::BigInt, LogicalType::Varchar]),
    ];

    for &(shape, types) in shapes {
        let chunk = chunk_of(types, ROWS);
        for &keep in KEEP {
            let selection = keeping(ROWS, keep);
            let kept = selection.len();
            if kept == 0 {
                continue;
            }
            // Built at the kept length rather than at the chunk length, so that a pass is one
            // comparison and not one comparison plus the construction of the thing it compares
            // against.
            let probes: Vec<Vector> =
                types.iter().map(|ty| Vector::constant(ty.clone(), probe(ty), kept)).collect();
            let selected = chunk.clone().select(&selection).expect("the selection is in range");
            let compacted = chunk.clone().compact(&selection).expect("the selection is in range");

            let surface = Surface {
                select_build: stage(
                    time(|| {
                        drop(black_box(black_box(chunk.clone()).select(black_box(&selection))));
                    }),
                    kept,
                ),
                select_pass: stage(
                    time(|| {
                        pass(black_box(&selected), black_box(&probes));
                    }),
                    kept,
                ),
                compact_build: stage(
                    time(|| {
                        drop(black_box(black_box(chunk.clone()).compact(black_box(&selection))));
                    }),
                    kept,
                ),
                compact_pass: stage(
                    time(|| {
                        pass(black_box(&compacted), black_box(&probes));
                    }),
                    kept,
                ),
            };

            let stages = [
                ("select build", surface.select_build),
                ("select pass", surface.select_pass),
                ("compact build", surface.compact_build),
                ("compact pass", surface.compact_pass),
            ];
            for (name, measured) in stages {
                cells.push(Cell {
                    table: "compaction",
                    case: shape.to_string(),
                    detail: format!("{keep}"),
                    variant: name.to_string(),
                    nulls: 0,
                    rows: kept,
                    nanos: measured.nanos,
                    // Each of the four stages carries its own spread rather than the row carrying
                    // one, because the last column of this table is a difference between two of
                    // them and the question it has to answer is whether that difference is larger
                    // than the spread of the two numbers it came out of.
                    spread: if measured.nanos == 0.0 { 0.0 } else { measured.iqr / measured.nanos },
                    fell_back: false,
                });
            }
        }
    }
}

/// A chunk of a given shape.
fn chunk_of(types: &[LogicalType], rows: usize) -> Chunk {
    let cases = cases();
    let columns: Vec<Vector> = types
        .iter()
        .map(|ty| {
            let case = cases
                .iter()
                .find(|case| &case.ty == ty)
                .expect("the shapes use types the case list has");
            flat(case, rows, 0, 0)
        })
        .collect();
    Chunk::new(columns).expect("every column is the same length")
}

/// The value a pass over a chunk compares its columns against.
fn probe(ty: &LogicalType) -> Value {
    match ty {
        LogicalType::Varchar => Value::Varchar("0032".to_string()),
        _ => Value::BigInt(32),
    }
}

/// One later pass over a filtered chunk, which is one comparison per column against a constant.
///
/// A comparison rather than an aggregate because it is the read every operator above a filter
/// makes, it is defined on every type in the shapes including the string one, and it is the kernel
/// whose dictionary path is the thing selecting is trading against.
/// The probes have to be the length of the filtered chunk, because a comparison of two vectors of
/// different lengths is an error and an error is not a pass.
fn pass(chunk: &Chunk, probes: &[Vector]) {
    for (column, probe) in chunk.columns().iter().zip(probes) {
        drop(black_box(compare(Comparison::Less, column, probe)));
    }
}

/// A selection keeping a percentage of the rows, spread rather than in a block.
fn keeping(rows: usize, percent: f64) -> Selection {
    let threshold = (percent * 100.0) as usize;
    Selection::from_predicate(rows, |row| scramble(row) < threshold)
}

/// Which of the two ways of keeping some rows a sweep row is measuring.
#[derive(Clone, Copy)]
enum Filter {
    /// Keep the indices and let the reader follow them, which is what a chunk select does.
    Select,
    /// Copy the kept rows out, which is what a chunk compact does.
    Compact,
}

/// Filter some columns one way or the other and read every one of them once.
///
/// The clone is the same clone the chunk path makes, because building a dictionary over a column
/// consumes the column, and both sides of the comparison pay it.
fn filtered(columns: &[Vector], selection: &Selection, probes: &[Vector], how: Filter) {
    for (column, probe) in columns.iter().zip(probes) {
        let kept = match how {
            Filter::Select => Vector::dictionary(selection.indices().to_vec(), column.clone())
                .expect("the selection is in range"),
            Filter::Compact => {
                column.gather(selection.indices()).expect("the selection is in range")
            }
        };
        drop(black_box(compare(Comparison::Less, &kept, probe)));
    }
}

/// The vector size sweep.
fn size_sweep(cells: &mut Vec<Cell>) {
    let all = cases();
    let case = all.iter().find(|case| case.layout == "Int64").expect("the list has a bigint");
    for &size in SIZES {
        let left = flat(case, size, 0, 0);
        let right = flat(case, size, 1, 0);
        let dict = dictionary(case, size, 0, 0);
        // Against a constant rather than against the flat column, because the comparison kernel has
        // no loop for a dictionary against a flat vector and that row would have been the row at a
        // time path at seventy five nanoseconds a row at every size, which says nothing about how
        // many rows fit in L1 and everything about a loop that has not been written. The grid table
        // above is where that pair is reported.
        let literal = Vector::constant(case.ty.clone(), Value::BigInt(32), size);
        let args = vec![left.clone(), right.clone()];
        let returns = case.ty.clone();
        let selection = keeping(size, 50.0);
        let kept = selection.len();
        // A column at a time rather than through a chunk, because a chunk is at most VECTOR_SIZE
        // rows by construction and two of the five sizes here are larger than that on purpose. The
        // work is the same work: a filtered column built one way or the other, then read once.
        let columns = [left.clone(), right.clone()];
        let probes = vec![
            Vector::constant(LogicalType::BigInt, Value::BigInt(32), kept),
            Vector::constant(LogicalType::BigInt, Value::BigInt(32), kept),
        ];

        let measured: Vec<(&str, Number, usize)> = vec![
            (
                "compare flat/flat",
                time(|| {
                    drop(black_box(compare(Comparison::Less, black_box(&left), black_box(&right))));
                }),
                size,
            ),
            (
                "compare dict/const",
                time(|| {
                    drop(black_box(compare(
                        Comparison::Less,
                        black_box(&dict),
                        black_box(&literal),
                    )));
                }),
                size,
            ),
            (
                "add flat/flat",
                time(|| {
                    drop(black_box(call("+", black_box(&args), black_box(&returns))));
                }),
                size,
            ),
            (
                "select then one pass",
                time(|| {
                    filtered(black_box(&columns), black_box(&selection), &probes, Filter::Select);
                }),
                kept,
            ),
            (
                "compact then one pass",
                time(|| {
                    filtered(black_box(&columns), black_box(&selection), &probes, Filter::Compact);
                }),
                kept,
            ),
        ];

        for (name, number, rows) in measured {
            cells.push(Cell {
                table: "sizes",
                case: name.to_string(),
                detail: format!("{size}"),
                variant: "flat".to_string(),
                nulls: 0,
                rows,
                nanos: number.per(rows),
                spread: number.relative(),
                fell_back: false,
            });
        }
    }
}

/// Print the tables.
fn report(cells: &[Cell]) {
    println!("rudb kernels, nanoseconds a row, {ROWS} rows a chunk unless a column says otherwise");
    println!("{}", build_line());

    grid(cells, "comparison", "comparison, left < right");
    grid(cells, "arithmetic", "arithmetic, left + right");
    grid(cells, "strings", "string comparison, left < right");
    surface_table(cells);
    sweep_table(cells);

    println!();
    let starred = cells.iter().filter(|cell| cell.fell_back).count();
    if starred == 0 {
        println!("every cell above took a specialized path");
    } else {
        println!("{starred} of {} cells fell through to the row at a time path", cells.len());
    }
    println!();
    for line in caveats() {
        println!("{line}");
    }
}

/// One of the three tables that is layouts by form pairs by null rate.
fn grid(cells: &[Cell], table: &'static str, title: &str) {
    println!();
    println!("{title}");
    println!();
    print!("{:<22}  {:<9}  {:>5}", "case", "type", "nulls");
    for &(name, _) in PAIRS {
        print!("  {name:>11}");
    }
    println!("  {:>5}", "IQR");

    let mut seen: Vec<(String, String)> = Vec::new();
    for row in cells.iter().filter(|cell| cell.table == table) {
        let key = (row.case.clone(), row.detail.clone());
        if !seen.contains(&key) {
            seen.push(key);
        }
    }
    for (case, detail) in seen {
        for &nulls in NULL_RATES {
            print!("{case:<22}  {detail:<9}  {nulls:>4}%");
            let mut worst: f64 = 0.0;
            for &(name, _) in PAIRS {
                let found = cells.iter().find(|cell| {
                    cell.table == table
                        && cell.case == case
                        && cell.variant == name
                        && cell.nulls == nulls
                });
                match found {
                    Some(cell) => {
                        // The starred cells are left out of the row's spread. A cell that fell
                        // through allocates per row, so its spread is the allocator's and it is
                        // three times the spread of the four kernel cells next to it. Letting it
                        // set the row's number would mean every row in the table says do not trust
                        // this, about a column that already says it is not a kernel's number.
                        if !cell.fell_back {
                            worst = worst.max(cell.spread);
                        }
                        let star = if cell.fell_back { "*" } else { " " };
                        print!("  {:>10.2}{star}", cell.nanos);
                    }
                    None => print!("  {:>11}", "-"),
                }
            }
            println!("  {:>4.1}%", worst * 100.0);
        }
    }
}

/// The compaction surface.
fn surface_table(cells: &[Cell]) {
    println!();
    println!("select against compact, nanoseconds a kept row");
    println!("a pass is one comparison against a constant over every column of the filtered chunk");
    println!();
    println!(
        "{:<20}  {:>6}  {:>6}  {:>9}  {:>9}  {:>9}  {:>9}  {:>9}",
        "shape", "keep%", "kept", "sel build", "sel pass", "cmp build", "cmp pass", "crossover"
    );

    let mut shapes: Vec<String> = Vec::new();
    for row in cells.iter().filter(|cell| cell.table == "compaction") {
        if !shapes.contains(&row.case) {
            shapes.push(row.case.clone());
        }
    }
    for shape in shapes {
        for &keep in KEEP {
            let detail = format!("{keep}");
            let at = |name: &str| {
                cells
                    .iter()
                    .find(|cell| {
                        cell.table == "compaction"
                            && cell.case == shape
                            && cell.detail == detail
                            && cell.variant == name
                    })
                    .map(|cell| {
                        (Stage { nanos: cell.nanos, iqr: cell.nanos * cell.spread }, cell.rows)
                    })
            };
            let (Some(sb), Some(sp), Some(cb), Some(cp)) =
                (at("select build"), at("select pass"), at("compact build"), at("compact pass"))
            else {
                continue;
            };
            let surface = Surface {
                select_build: sb.0,
                select_pass: sp.0,
                compact_build: cb.0,
                compact_pass: cp.0,
            };
            let crossover = match surface.crossover() {
                Crossover::Never => "never".to_string(),
                Crossover::Noise => "noise".to_string(),
                Crossover::At(depth) => depth.to_string(),
            };
            println!(
                "{shape:<20}  {keep:>6}  {:>6}  {:>9.2}  {:>9.2}  {:>9.2}  {:>9.2}  {crossover:>9}",
                sb.1,
                surface.select_build.nanos,
                surface.select_pass.nanos,
                surface.compact_build.nanos,
                surface.compact_pass.nanos
            );
        }
    }
}

/// The vector size sweep.
fn sweep_table(cells: &[Cell]) {
    println!();
    println!("vector size, nanoseconds a row, BIGINT, no nulls");
    println!();
    print!("{:<22}", "operation");
    for size in SIZES {
        print!("  {size:>8}");
    }
    println!();

    let mut operations: Vec<String> = Vec::new();
    for row in cells.iter().filter(|cell| cell.table == "sizes") {
        if !operations.contains(&row.case) {
            operations.push(row.case.clone());
        }
    }
    for operation in operations {
        print!("{operation:<22}");
        for size in SIZES {
            let detail = format!("{size}");
            let found = cells.iter().find(|cell| {
                cell.table == "sizes" && cell.case == operation && cell.detail == detail
            });
            match found {
                Some(cell) => print!("  {:>8.2}", cell.nanos),
                None => print!("  {:>8}", "-"),
            }
        }
        println!();
    }
}

/// The things that have to be read with the tables and not after them.
fn caveats() -> Vec<String> {
    let mut lines = vec!["Read these with the following, and not on their own:".to_string()];
    lines.extend(shared_caveats());
    lines.extend([
        "  rule ten: a micro number never appears without the end to end number it explains,"
            .to_string(),
        "    and nothing here times a query. A kernel number is why a query number is what it"
            .to_string(),
        "    is, and on its own it is a fact about a loop. The query numbers are in".to_string(),
        "    tamnd/rudb-bench, against whole engines, and that is where a number anybody quotes"
            .to_string(),
        "    comes from.".to_string(),
        "  a star next to a number means that kernel has no loop for that form pair and the"
            .to_string(),
        "    cell timed the row at a time path that exists to be correct. It is a real cost and"
            .to_string(),
        "    not a broken measurement, and it is the list of loops worth writing next.".to_string(),
        "  the IQR column is the worst of the pairs on the row, not their average, because one"
            .to_string(),
        "    unstable cell is enough to make a row not worth reading. The starred cells are left"
            .to_string(),
        "    out of it: they allocate per row, their spread is the allocator's rather than a"
            .to_string(),
        "    kernel's, and they are already marked as not being a kernel's number.".to_string(),
        "  every cell includes one allocation of the result vector, because the kernels return a"
            .to_string(),
        "    new vector rather than filling one that was handed to them. On the fastest rows that"
            .to_string(),
        "    is a real share of the number and it is most of why the spread is what it is. It is"
            .to_string(),
        "    also a cost the engine pays today, so leaving it in is the honest thing until the"
            .to_string(),
        "    expression layer reuses buffers, and this table is how that change gets measured."
            .to_string(),
        "  a spread in double figures did not go away when the run was pinned to one core, so on"
            .to_string(),
        "    a shared machine read this table for factors rather than for percentages. Four"
            .to_string(),
        "    tenths of a nanosecond against seventy is a finding. Two point two against two point"
            .to_string(),
        "    six between two runs is not.".to_string(),
        "  the crossover column is computed from the four stages rather than measured, and it"
            .to_string(),
        "    divides by the gap between the two pass columns. It says noise when that gap is"
            .to_string(),
        "    smaller than the spread of the two timings it came out of, because a depth from a"
            .to_string(),
        "    gap that size swings between a small number and never between two runs of the same"
            .to_string(),
        "    binary, and never when compacting does not make a pass cheaper at all.".to_string(),
        "  the build columns of the compaction table include one clone of the source chunk,"
            .to_string(),
        "    because both select and compact consume it. Both sides pay exactly the same clone,"
            .to_string(),
        "    so it cancels in the crossover and does not cancel in the two build columns."
            .to_string(),
    ]);
    lines
}

/// The cells as JSON, one object a line inside one array.
///
/// Hand written rather than through a serializer because this workspace has no dependencies and a
/// benchmark output format is not the place to acquire the first one. Every string that reaches
/// here is a case name, a type name or a stage name, all of which are written in this file, and the
/// test below pins them to the characters that need no escaping so that staying inside that is a
/// thing the build checks rather than a thing a reader remembers.
fn as_json(cells: &[Cell]) -> String {
    let mut out = String::from("[\n");
    for (at, cell) in cells.iter().enumerate() {
        let comma = if at + 1 == cells.len() { "" } else { "," };
        out.push_str(&format!(
            "  {{\"table\":\"{}\",\"case\":\"{}\",\"detail\":\"{}\",\"variant\":\"{}\",\
             \"nulls\":{},\"rows\":{},\"nanos\":{:.4},\"spread\":{:.4},\"fell_back\":{}}}{comma}\n",
            cell.table,
            cell.case,
            cell.detail,
            cell.variant,
            cell.nulls,
            cell.rows,
            cell.nanos,
            cell.spread,
            cell.fell_back
        ));
    }
    out.push_str("]\n");
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Cell, Crossover, PAIRS, Prefix, Stage, Surface, Width, as_json, cases, caveats, keeping,
        pattern, scramble, strings, validity,
    };
    use rudb_vector::{Data, StringView, Validity, Vector, for_each_layout};

    /// The variant names of a layout group, which is how the coverage test below reaches the list
    /// the vector crate keeps.
    macro_rules! names {
        ($(($variant:ident, $native:ty, $zero:expr)),+ $(,)?) => {
            &[$(stringify!($variant)),+]
        };
    }

    #[test]
    fn every_physical_layout_has_a_row() {
        // The thing this catches is a layout added to the vector crate that nobody adds here. It
        // would not be a compile error and it would not be a wrong answer. It would be a table that
        // says every layout and means every layout but one, which is exactly the shape of the
        // problem the one list in rudb-vector was written to end.
        let all: &[&str] = for_each_layout!(all, names);
        let measured: Vec<&str> = cases().iter().map(|case| case.layout).collect();
        for layout in all {
            assert!(measured.contains(layout), "{layout} has no row in the kernel table");
        }
        assert_eq!(measured.len(), all.len(), "the kernel table has a row for something else");
    }

    #[test]
    fn a_case_builds_the_layout_its_type_calls_for() {
        // Every case is built once here rather than only inside a bench run, so a case whose type
        // and run disagree fails in the gate rather than two minutes into a benchmark.
        for case in cases() {
            let vector = Vector::flat(case.ty.clone(), (case.data)(16, 0))
                .unwrap_or_else(|e| panic!("{}: {e}", case.layout));
            assert_eq!(vector.len(), 16, "{}", case.layout);
        }
    }

    #[test]
    fn the_pattern_stays_inside_the_narrowest_width_when_two_of_them_are_added() {
        // The arithmetic table adds a left row to a right row, and the narrowest layout it does
        // that on is an i8. A pattern that overflowed one would turn that table into a measurement
        // of an error path.
        let widest = (0..4096).map(|row| pattern(row, 0) + pattern(row, 1)).max().expect("rows");
        assert!(widest <= i64::from(i8::MAX), "two rows added overflow a tinyint: {widest}");
    }

    #[test]
    fn the_null_rate_is_the_rate_that_was_asked_for() {
        for percent in [1, 50] {
            let Validity::Mask(mask) = validity(4096, percent) else {
                panic!("a rate of {percent} produced no mask");
            };
            let valid = mask.count_valid(4096);
            let nulls = 4096 - valid;
            let wanted = 4096 * percent / 100;
            assert!(
                nulls.abs_diff(wanted) <= wanted / 5 + 8,
                "asked for {percent} percent of 4096 and got {nulls} nulls"
            );
        }
    }

    #[test]
    fn no_nulls_is_the_form_that_costs_nothing_to_read() {
        // Not a detail. `AllValid` is the case the kernels hoist the validity dispatch for, and a
        // zero percent row that arrived as a mask of all ones would be measuring the mask path and
        // reporting it as the no nulls path.
        assert!(matches!(validity(1024, 0), Validity::AllValid));
    }

    #[test]
    fn a_selection_keeps_about_the_share_it_was_asked_for() {
        assert_eq!(keeping(1024, 100.0).len(), 1024);
        let half = keeping(4096, 50.0).len();
        assert!(half.abs_diff(2048) <= 256, "half of 4096 came out as {half}");
        let tenth = keeping(4096, 0.1).len();
        assert!(tenth <= 32, "a tenth of a percent of 4096 came out as {tenth}");
    }

    #[test]
    fn the_scramble_is_spread_rather_than_in_a_block() {
        // A block of nulls is a run of validity words that are all zero and a kernel skips the lot,
        // which is the one case a real column almost never has.
        let first = (0..64).filter(|&row| scramble(row) < 5_000).count();
        let last = (4032..4096).filter(|&row| scramble(row) < 5_000).count();
        assert!(first > 16 && first < 48, "the first word is not half full: {first}");
        assert!(last > 16 && last < 48, "the last word is not half full: {last}");
    }

    #[test]
    fn a_tie_in_the_prefix_really_ties_and_a_decision_really_decides() {
        let ties = strings(8, 0, Prefix::Ties, Width::Inline);
        let decides = strings(8, 0, Prefix::Decides, Width::Inline);
        let prefixes = |data: &Data| match data {
            Data::Varlen(column) => {
                let mut seen: Vec<[u8; 4]> =
                    column.views().iter().map(StringView::prefix).collect();
                seen.sort_unstable();
                seen.dedup();
                seen.len()
            }
            _ => panic!("strings did not produce a varlen run"),
        };
        assert_eq!(prefixes(&ties), 1, "the tie case has more than one prefix");
        assert!(prefixes(&decides) > 1, "the decides case has one prefix");
    }

    #[test]
    fn a_string_meant_to_be_inline_is_inline_and_one_meant_for_the_arena_is_not() {
        let check = |data: Data, inline: bool| match data {
            Data::Varlen(column) => {
                assert!(column.views().iter().all(|view| view.is_inline() == inline));
            }
            _ => panic!("strings did not produce a varlen run"),
        };
        check(strings(8, 0, Prefix::Ties, Width::Inline), true);
        check(strings(8, 0, Prefix::Ties, Width::Heap), false);
        check(strings(8, 0, Prefix::Decides, Width::Heap), false);
    }

    /// A stage that was measured cleanly, which is the case the crossover arithmetic is about.
    fn firm(nanos: f64) -> Stage {
        Stage { nanos, iqr: nanos * 0.01 }
    }

    #[test]
    fn a_crossover_says_never_when_the_passes_do_not_get_cheaper() {
        let surface = Surface {
            select_build: firm(1.0),
            select_pass: firm(2.0),
            compact_build: firm(40.0),
            compact_pass: firm(2.0),
        };
        assert_eq!(surface.crossover(), Crossover::Never);
    }

    #[test]
    fn a_crossover_is_zero_when_compacting_was_cheaper_to_build_too() {
        let surface = Surface {
            select_build: firm(10.0),
            select_pass: firm(4.0),
            compact_build: firm(8.0),
            compact_pass: firm(1.0),
        };
        assert_eq!(surface.crossover(), Crossover::At(0));
    }

    #[test]
    fn a_crossover_rounds_up_to_the_pass_that_pays_it_off() {
        // Nine nanoseconds owed and four saved a pass is two and a quarter passes, and two passes
        // have not paid it off yet, so the answer a caller can act on is three.
        let surface = Surface {
            select_build: firm(1.0),
            select_pass: firm(5.0),
            compact_build: firm(10.0),
            compact_pass: firm(1.0),
        };
        assert_eq!(surface.crossover(), Crossover::At(3));
    }

    #[test]
    fn a_gap_inside_the_spread_is_not_a_crossover() {
        // The case the first run of this table got wrong. A pass column of 1.34 against one of
        // 1.30, each measured with a spread of a tenth of a nanosecond, produces a depth of twenty
        // something that would have been a different twenty something an hour later. The two
        // columns are the same number and the honest answer is that this was not measured.
        let surface = Surface {
            select_build: firm(3.0),
            select_pass: Stage { nanos: 1.34, iqr: 0.1 },
            compact_build: firm(4.0),
            compact_pass: Stage { nanos: 1.30, iqr: 0.1 },
        };
        assert_eq!(surface.crossover(), Crossover::Noise);
    }

    #[test]
    fn a_gap_larger_than_the_spread_is_still_a_crossover() {
        // The other side of the same boundary, so the noise rule cannot quietly grow until it
        // swallows the answers the table exists to give.
        let surface = Surface {
            select_build: firm(3.0),
            select_pass: Stage { nanos: 5.0, iqr: 0.1 },
            compact_build: Stage { nanos: 11.0, iqr: 0.1 },
            compact_pass: Stage { nanos: 1.0, iqr: 0.1 },
        };
        assert_eq!(surface.crossover(), Crossover::At(2));
    }

    #[test]
    fn nothing_that_reaches_the_json_needs_escaping() {
        // The writer below does no escaping, which is fine exactly as long as this holds, and this
        // is the check that says so rather than a comment asking somebody to remember.
        let plain = |text: &str, what: &str| {
            assert!(
                text.chars().all(|c| c.is_ascii_alphanumeric() || " /+<.,-_%".contains(c)),
                "{what} has a character the JSON writer would have to escape: {text}"
            );
        };
        for case in cases() {
            plain(case.layout, "a layout name");
            plain(case.sql, "a type name");
        }
        for line in PAIRS {
            plain(line.0, "a form pair name");
        }
    }

    #[test]
    fn the_json_is_one_array_of_one_object_a_cell() {
        let cells = vec![
            Cell {
                table: "comparison",
                case: "Int32".to_string(),
                detail: "INTEGER".to_string(),
                variant: "flat/flat".to_string(),
                nulls: 0,
                rows: 1024,
                nanos: 0.35,
                spread: 0.012,
                fell_back: false,
            },
            Cell {
                table: "comparison",
                case: "Int32".to_string(),
                detail: "INTEGER".to_string(),
                variant: "dict/flat".to_string(),
                nulls: 50,
                rows: 1024,
                nanos: 3.51,
                spread: 0.02,
                fell_back: true,
            },
        ];
        let text = as_json(&cells);
        assert!(text.starts_with("[\n"), "{text}");
        assert!(text.ends_with("]\n"), "{text}");
        assert_eq!(text.matches("\"table\"").count(), 2, "{text}");
        assert_eq!(text.matches("},\n").count(), 1, "a trailing comma or a missing one: {text}");
        assert!(text.contains("\"fell_back\":true"), "{text}");
        assert!(text.contains("\"nanos\":0.3500"), "{text}");
    }

    #[test]
    fn an_empty_run_is_still_an_array() {
        assert_eq!(as_json(&[]), "[\n]\n");
    }

    #[test]
    fn the_rules_are_printed_every_time_and_not_remembered() {
        let text = caveats().join("\n");
        assert!(text.contains("rule ten"), "the tables can be read as a result without this");
        assert!(text.contains("rule seven"));
        assert!(text.contains("rule two"));
        assert!(text.contains("star"), "the fallback marker is not explained anywhere else");
    }

    #[test]
    fn a_case_is_the_size_the_columns_print() {
        for case in cases() {
            assert!(case.layout.len() <= 22, "{} overflows the case column", case.layout);
            assert!(case.sql.len() <= 9, "{} overflows the type column", case.sql);
        }
        for name in ["prefix decides, inline", "prefix ties, arena"] {
            assert!(name.len() <= 22, "{name} overflows the case column");
        }
    }
}
