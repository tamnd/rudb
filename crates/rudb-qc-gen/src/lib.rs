//! The generator, per `spec/compiler/07-code-generation.md`: one QIR function per pipeline.
//!
//! A body runs over the rows `begin..end` of one morsel. It reads the source columns through the
//! morsel's column table, runs the operators in order and jumps to the next row at the first filter
//! that is not true, and then hands the row to the sink. A join probe is a loop of its own inside
//! the row loop, over the entries of the one directory slot the row's hash picks, and everything
//! after it runs once per match, per section 10.5 of `spec/compiler/10-joins.md`. Everything the
//! body keeps between calls is in its state, whose layout [`Body`] describes so that the driver can
//! set it up and read it:
//!
//! ```text
//! [header: 64 bytes][sink fields]
//! ```
//!
//! A result sink has a row count and, per output column, the address of a values buffer and of a
//! validity buffer with one byte per row. The driver points them at buffers as long as the morsel
//! and turns what the body wrote into a chunk after each call. Past a probe a row can make any
//! number of rows, so there the sink also has the buffers' capacity, and a body that fills them
//! returns `NeedMemory` for the driver to grow them and run the morsel again. An aggregate sink has
//! the key buffer the body builds each row's key in before `ht_insert`, or for an aggregate with no
//! groups the address of the one group row, which the driver writes there before the first call.
//! When every key is fixed width it also has the row of the last key and four words `ht_insert`
//! publishes the table in, so that the body finds a key's group itself and calls only for a new one. A
//! join build has the record buffer the body builds each row's record in before `jt_append`. After
//! the sink come three words per probe, which the driver fills from the built table at init: the
//! directory's address, the shift that takes a hash to its slot and the tag table's address.
//!
//! A `LIKE` of a source column against a constant is not asked a row at a time. The body reads it
//! as one more column of the morsel, a byte a row, which the driver fills before the call by
//! asking the pattern of the whole morsel at once, and [`Body::likes`] says which ones those are.
//!
//! Handles are made here, in the query's [`Rt`], because the code carries them as constants: the
//! `LIKE` patterns and regular expressions, the grouping tables, the distinct sets. String
//! literals are kept in the runtime's heap for the same reason. That is the literal table of
//! document 07.
//!
//! A scalar function with no translator here is not refused. It becomes a `vcall` to a kernel that
//! runs the first engine's own implementation of it, which the `vcall` module describes, so every
//! function the first engine has is reachable and gives that engine's answers and errors.
//!
//! Every value is a pair of a value and an `i1` that says whether it is valid, and the value under
//! an invalid one is harmless: a column read puts a zero there, and every operation on harmless
//! inputs gives a harmless output. That is what lets a trapping add run on a null without a branch
//! around it, and it is why this generator never records a validity for rule V12 to check.

use std::collections::HashMap;

use rudb_common::{LogicalType, PhysicalType, Value};
use rudb_plan::CompareOp;
use rudb_qc_ir::catalogue::proxy;
use rudb_qc_ir::eval::pow10;
use rudb_qc_ir::func::INV;
use rudb_qc_ir::status::NEED_MEMORY;
use rudb_qc_ir::{Block, Builder, ErrorKind, Field, Func, Module, Op, Ty, Val, dce, verify};
use rudb_qc_pipe::{Graph, Op as PipeOp, Pipeline, Probe, Sink, Stage};
use rudb_qc_plan::{Aggregate, Column, Expr, Kind, Refusal, Result};
use rudb_qc_rt::abi::{COL_SIZE, COL_VALID, HEADER, MORSEL_BEGIN, MORSEL_COLS, MORSEL_END};
use rudb_qc_rt::join::{ADDRESS, FOLD, JoinLayout, JoinTable};
use rudb_qc_rt::table::{GroupTable, KeyField, Layout};
use rudb_qc_rt::{Rt, text};

/// Where the sink's fields start.
const SINK: u32 = HEADER;

/// The module and what the driver needs to run each pipeline in it.
#[derive(Debug)]
pub struct Query {
    /// One function per pipeline.
    pub module: Module,
    /// One entry per stage of the graph, `None` for a stage that is not a pipeline.
    pub bodies: Vec<Option<Body>>,
}

/// One pipeline function and its state.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    /// The function's name in the module.
    pub func: String,
    /// The source columns the body reads, in the order of the morsel's column table.
    pub reads: Vec<usize>,
    /// The bytes of state the body needs, header included.
    pub state: u32,
    /// Where the rows go.
    pub sink: Out,
    /// The join tables the body probes, in the order of its probes.
    pub probes: Vec<Probing>,
    /// The function for a morsel with no NULL in any column the body reads, which skips every
    /// validity check. It has the same state, sink and probes as [`Body::func`], so the driver
    /// can call either one on any morsel, and it is a pre-check guard of section 9.6 of
    /// `spec/compiler/09-tiering-and-caching.md` with `func` as its fallback.
    pub nonull: Option<String>,
    /// The `LIKE` answers the body reads after its source columns in the morsel's column table,
    /// in that order.
    pub likes: Vec<Matched>,
}

/// A `LIKE` of a source column against a constant, which the driver answers for the whole morsel
/// and hands the body as a column of one byte a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Matched {
    /// The source column.
    pub column: usize,
    /// The pattern as written.
    pub pattern: String,
    /// Whether it is `ILIKE`.
    pub fold: bool,
    /// The pattern's handle in the runtime.
    pub like: u64,
}

/// Where the body reads a join table it probes, which the driver fills in before the first call.
#[derive(Clone, Debug, PartialEq)]
pub struct Probing {
    /// The handle of the table in the query's runtime.
    pub table: u64,
    /// The state offset of the directory's address.
    pub directory: u32,
    /// The state offset of the shift, an `i64`.
    pub shift: u32,
    /// The state offset of the tag table's address.
    pub tags: u32,
}

/// The state layout of a sink.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// Rows out.
    Result {
        /// The state offset of the row count, an `i64`.
        count: u32,
        /// The output columns.
        columns: Vec<Slot>,
        /// Past a probe, the state offset of how many rows the buffers hold, an `i64`.
        capacity: Option<u32>,
    },
    /// A hash aggregate.
    Aggregate(Grouping),
    /// A join build.
    Build(Building),
}

/// A join build's table and the record the body builds for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Building {
    /// The handle of the table in the query's runtime.
    pub table: u64,
    /// The record's shape.
    pub layout: JoinLayout,
    /// The state offset of the record buffer.
    pub record: u32,
}

/// One output column of a result sink.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// The state offset of the address of the values buffer.
    pub values: u32,
    /// The state offset of the address of the validity buffer, one byte per row.
    pub valid: u32,
    /// The type the values are written at.
    pub ty: Ty,
    /// The column's type.
    pub logical: LogicalType,
}

/// A hash aggregate's table and accumulators.
#[derive(Clone, Debug, PartialEq)]
pub struct Grouping {
    /// The handle of the table in the query's runtime.
    pub table: u64,
    /// The key columns and their types.
    pub keys: Vec<(KeyField, LogicalType)>,
    /// Where the accumulators start in a row.
    pub acc_offset: u32,
    /// The accumulators, in the order of the aggregate calls.
    pub accs: Vec<Acc>,
    /// For an aggregate with no groups, the state offset where the driver writes the address of
    /// the one row.
    pub row: Option<u32>,
    /// For an aggregate grouped by keys that are all fixed width, the state offset of the row the
    /// last key went to, zero before the first. A row whose key is the last one's goes to the same
    /// row without hashing or probing, which is most rows of a file sorted on its keys.
    pub last: Option<u32>,
    /// For the same aggregates, the state offset of four words `ht_insert` publishes the table in,
    /// which the body probes itself before it calls: the address of the slots, their mask, the
    /// address of the rows by group id, and how many keys it found that way.
    pub probe: Option<u32>,
}

/// One accumulator in a group row.
#[derive(Clone, Debug, PartialEq)]
pub struct Acc {
    /// Where it is, from the start of the accumulators.
    pub offset: u32,
    /// What it keeps.
    pub op: AccOp,
    /// The argument's type, or the result's for `count_star`.
    pub arg: LogicalType,
    /// The result's type.
    pub ty: LogicalType,
}

/// What an accumulator keeps and how the driver finishes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccOp {
    /// An `i64` row count.
    CountStar,
    /// An `i64` count of valid values.
    Count,
    /// An `i128` total and a seen byte after it.
    SumInt,
    /// An `f64` total and a seen byte after it.
    SumFloat,
    /// An `i128` total and an `i64` count after it.
    AvgInt,
    /// An `i64` total and a seen byte after it, for a sum over values of at most sixteen bits,
    /// which no group can overflow before it holds 2^48 rows.
    SumNarrow,
    /// An `i64` total and an `i64` count after it, for an average over the same.
    AvgNarrow,
    /// An `f64` total and an `i64` count after it.
    AvgFloat,
    /// The least value at its width and a seen byte after it.
    Min,
    /// The greatest value at its width and a seen byte after it.
    Max,
    /// The first valid value at its width and a seen byte after it. A string is promoted to the
    /// runtime heap first.
    AnyValue,
    /// The least string, a `str16` and a seen byte, kept by `agg_min_str`.
    MinStr,
    /// The greatest string, kept by `agg_max_str`.
    MaxStr,
    /// A count of distinct values kept in the distinct sets behind the handle. No bytes in the row.
    Distinct(u64),
}

/// Generates a function for every pipeline of the graph.
///
/// # Errors
///
/// A refusal when a pipeline needs something the generator does not know yet, or when what it
/// made does not verify, which is a bug and is refused rather than run.
pub fn generate(graph: &Graph, rt: &mut Rt) -> Result<Query> {
    let mut module = Module::new("query");
    let mut bodies: Vec<Option<Body>> = Vec::with_capacity(graph.stages.len());
    for (stage, s) in graph.stages.iter().enumerate() {
        bodies.push(match s {
            Stage::Pipeline(p) => {
                let mut joins = Vec::new();
                for probe in p.probes() {
                    match bodies.get(probe.build).and_then(Option::as_ref).map(|b| &b.sink) {
                        Some(Out::Build(b)) => joins.push(b.clone()),
                        _ => {
                            return Err(Refusal::new(
                                "HashJoin",
                                "a probe of a stage that is not an earlier build",
                            ));
                        }
                    }
                }
                Some(pipeline(stage, p, &joins, &mut module, rt)?)
            }
            _ => None,
        });
    }
    if let Err(errors) = verify(&module) {
        let why = errors.iter().map(|e| format!("{} {}", e.rule, e.message)).collect::<Vec<_>>();
        return Err(Refusal::new("the generated code", why.join("; ")));
    }
    Ok(Query { module, bodies })
}

/// The QIR type of a column of type `ty`.
///
/// # Errors
///
/// For a type with no one QIR type.
pub fn qir_type(ty: &LogicalType) -> Result<Ty> {
    Ok(match ty.physical() {
        PhysicalType::Bool => Ty::I1,
        PhysicalType::Int8 | PhysicalType::UInt8 => Ty::I8,
        PhysicalType::Int16 | PhysicalType::UInt16 => Ty::I16,
        PhysicalType::Int32 | PhysicalType::UInt32 => Ty::I32,
        PhysicalType::Int64 | PhysicalType::UInt64 => Ty::I64,
        PhysicalType::Int128 | PhysicalType::UInt128 => Ty::I128,
        PhysicalType::Float32 => Ty::F32,
        PhysicalType::Float64 => Ty::F64,
        PhysicalType::Varlen => Ty::Str16,
        _ => return Err(Refusal::new(ty.to_string(), "the type has no QIR type")),
    })
}

/// Whether values of `ty` compare as unsigned numbers.
#[must_use]
pub fn unsigned(ty: &LogicalType) -> bool {
    matches!(
        ty.physical(),
        PhysicalType::Bool
            | PhysicalType::UInt8
            | PhysicalType::UInt16
            | PhysicalType::UInt32
            | PhysicalType::UInt64
            | PhysicalType::UInt128
    )
}

fn pipeline(
    stage: usize,
    p: &Pipeline,
    joins: &[Building],
    module: &mut Module,
    rt: &mut Rt,
) -> Result<Body> {
    let name = format!("p{stage}");
    let source = p.source.columns();
    // Only the first operator sees every row of the morsel. A `LIKE` past a filter is asked only of
    // the rows that got through, and answering it for the whole morsel would ask all of them.
    let mut likes: Vec<Matched> = Vec::new();
    if let Some(PipeOp::Filter(f)) = p.ops.first() {
        matched(f, source.len(), &mut likes);
    }
    for m in &mut likes {
        m.like = rt.add_like(&m.pattern, m.fold);
    }
    let mut reads = Vec::new();
    // Only the source's columns are read from the morsel. The rest are what the probes bring, and
    // a column only an answered `LIKE` reads is not read itself.
    let mut note = |e: &Expr| read_by(e, source.len(), &likes, &mut reads);
    for op in &p.ops {
        match op {
            PipeOp::Filter(f) => note(f),
            PipeOp::Probe(probe) => probe.keys.iter().for_each(&mut note),
        }
    }
    match &p.sink {
        Sink::Result { exprs, .. } => exprs.iter().for_each(&mut note),
        Sink::Build { keys, payload, .. } => {
            keys.iter().for_each(&mut note);
            payload.iter().for_each(&mut note);
        }
        Sink::Aggregate { groups, aggregates, .. } => {
            groups.iter().for_each(&mut note);
            for a in aggregates {
                a.args.iter().for_each(&mut note);
                if let Some(f) = &a.filter {
                    note(f);
                }
            }
        }
    }
    reads.sort_unstable();
    let generic = Pass { name: &name, version: "generic", nonull: false, replay: None };
    let (func, out, state, probes, made) =
        emit(stage, p, joins, module, rt, &reads, &likes, generic)?;
    module.funcs.push(func);
    // The version for a morsel with no NULL in the columns it reads. It shares the generic one's
    // tables and kernels and has to come out with the same state, or it is left out.
    let mut nonull = None;
    if !reads.is_empty() || !likes.is_empty() {
        let variant = format!("{name}_nonull");
        let pass = Pass { name: &variant, version: "nonull", nonull: true, replay: Some(&made) };
        if let Ok((f, o, st, pr, _)) = emit(stage, p, joins, module, rt, &reads, &likes, pass)
            && (&o, st, &pr) == (&out, state, &probes)
        {
            module.funcs.push(f);
            module.guard("no NULL in the columns the morsel reads", &name, true);
            nonull = Some(variant);
        }
    }
    Ok(Body { func: name, reads, state, sink: out, probes, nonull, likes })
}

/// Adds every source column `e` reads outside the `LIKE`s in `likes` to `reads`, each once.
fn read_by(e: &Expr, sources: usize, likes: &[Matched], reads: &mut Vec<usize>) {
    match &e.kind {
        Kind::Column(c) if *c < sources && !reads.contains(c) => reads.push(*c),
        Kind::Function { .. } if answered(e, likes).is_some() => {}
        _ => e.children(|c| read_by(c, sources, likes, reads)),
    }
}

/// The index in `likes` of the `LIKE` that `e` is, if it is one of them.
fn answered(e: &Expr, likes: &[Matched]) -> Option<usize> {
    let Kind::Function { name, args } = &e.kind else { return None };
    let ("~~" | "!~~" | "~~*" | "!~~*", [s, pattern]) = (name.as_str(), args.as_slice()) else {
        return None;
    };
    let (Kind::Column(column), Kind::Constant(Value::Varchar(p))) = (&s.kind, &pattern.kind) else {
        return None;
    };
    let fold = name.contains('*');
    likes.iter().position(|m| (m.column, m.pattern.as_str(), m.fold) == (*column, p, fold))
}

/// Adds every `LIKE` in `e` of a source column against a constant to `likes`, each once.
fn matched(e: &Expr, sources: usize, likes: &mut Vec<Matched>) {
    if let Kind::Function { name, args } = &e.kind
        && let ("~~" | "!~~" | "~~*" | "!~~*", [s, pattern]) = (name.as_str(), args.as_slice())
        && let (Kind::Column(column), Kind::Constant(Value::Varchar(p))) = (&s.kind, &pattern.kind)
        && *column < sources
        && s.ty == LogicalType::Varchar
    {
        if answered(e, likes).is_none() {
            let fold = name.contains('*');
            likes.push(Matched { column: *column, pattern: p.clone(), fold, like: 0 });
        }
        return;
    }
    e.children(|c| matched(c, sources, likes));
}

/// One pass over a pipeline: the function's name and version, whether it reads every column as
/// valid, and on the second pass the handles the first one made.
#[derive(Clone, Copy)]
struct Pass<'a> {
    name: &'a str,
    version: &'a str,
    nonull: bool,
    replay: Option<&'a [u64]>,
}

/// A pipeline's function, its sink, the bytes of state it needs, its probes, and the handles it
/// made in the runtime.
type Emitted = (Func, Out, u32, Vec<Probing>, Vec<u64>);

#[allow(clippy::too_many_arguments)]
fn emit(
    stage: usize,
    p: &Pipeline,
    joins: &[Building],
    module: &mut Module,
    rt: &mut Rt,
    reads: &[usize],
    likes: &[Matched],
    pass: Pass<'_>,
) -> Result<Emitted> {
    let source = p.source.columns();
    let mut g = Gen {
        b: Builder::new(pass.name, pass.version, stage as u32),
        module,
        rt,
        cols: HashMap::new(),
        likes: Vec::new(),
        matched: Vec::new(),
        loaded: HashMap::new(),
        row: Val::NONE,
        ptrs: Vec::new(),
        columns: p.columns(),
        next: 0,
        joins: joins.to_vec(),
        tables: Vec::new(),
        nonull: pass.nonull,
        made: Vec::new(),
        replay: pass.replay.map(|made| (made, 0)),
    };
    g.b.func_mut().state.push(Field { offset: 0, size: HEADER, name: "header".into() });

    // The entry block reads the morsel and the column table once.
    let m = g.b.m();
    let begin = g.b.load(Ty::I32, m, Val::NONE, 1, MORSEL_BEGIN, INV);
    let end = g.b.load(Ty::I32, m, Val::NONE, 1, MORSEL_END, INV);
    let begin = g.b.conv(Op::Zext, begin, Ty::I64);
    let end = g.b.conv(Op::Zext, end, Ty::I64);
    let table = g.b.load(Ty::Ptr, m, Val::NONE, 1, MORSEL_COLS, INV);
    for (j, &c) in reads.iter().enumerate() {
        let at = j as i32 * COL_SIZE;
        let values = g.b.load(Ty::Ptr, table, Val::NONE, 1, at, INV);
        let valid = g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_VALID, INV);
        let ty = qir_type(&source[c].ty)?;
        g.cols.insert(c, (values, valid, ty));
    }
    for k in 0..likes.len() {
        let at = (reads.len() + k) as i32 * COL_SIZE;
        let values = g.b.load(Ty::Ptr, table, Val::NONE, 1, at, INV);
        let valid = g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_VALID, INV);
        g.likes.push((values, valid));
    }
    g.matched = likes.to_vec();
    let fans_out = p.probes().next().is_some();
    let (out, state) = g.prepare_sink(&p.sink, fans_out)?;
    g.next = state;
    let st = g.b.st();
    let mut probes = Vec::with_capacity(joins.len());
    for (k, j) in joins.iter().enumerate() {
        let at = g.next.next_multiple_of(8);
        g.field(at, 8, &format!("probe{k}.directory"));
        g.field(at + 8, 8, &format!("probe{k}.shift"));
        g.field(at + 16, 8, &format!("probe{k}.tags"));
        g.next = at + 24;
        let directory = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, INV);
        let shift = g.b.load(Ty::I64, st, Val::NONE, 1, at as i32 + 8, INV);
        let tags = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32 + 16, INV);
        g.tables.push((directory, shift, tags));
        probes.push(Probing { table: j.table, directory: at, shift: at + 8, tags: at + 16 });
    }
    match &out {
        Out::Result { columns, .. } => {
            for slot in columns {
                let values = g.b.load(Ty::Ptr, st, Val::NONE, 1, slot.values as i32, INV);
                let valid = g.b.load(Ty::Ptr, st, Val::NONE, 1, slot.valid as i32, INV);
                g.ptrs.extend([values, valid]);
            }
        }
        Out::Aggregate(grouping) => {
            if let Some(at) = grouping.row {
                let row = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, INV);
                g.ptrs.push(row);
            }
        }
        Out::Build(_) => {}
    }

    let head = g.b.block(&[(Ty::I64, "i")]);
    let body = g.b.block(&[]);
    let next = g.b.block(&[]);
    let exit = g.b.block(&[]);
    g.b.br(head, &[begin]);

    g.b.switch_to(head);
    g.b.set_loop(head, 1);
    let i = g.b.param(head, 0);
    g.row = i;
    g.b.poll(1024);
    let more = g.b.bin(Op::IcmpSlt, i, end);
    g.b.brif(more, body, &[], exit, &[]);

    g.b.switch_to(body);
    g.ops(&p.ops, 0, next, 1, &p.sink, &out)?;

    g.b.switch_to(next);
    let one = g.b.int(Ty::I64, 1);
    let i1 = g.b.bin(Op::Add, i, one);
    g.b.br(head, &[i1]);

    g.b.switch_to(exit);
    let ok = g.b.int(Ty::I64, 0);
    g.b.ret(ok);

    if let Some((made, used)) = g.replay
        && used != made.len()
    {
        return Err(Refusal::new("the second version of a pipeline", "it made fewer handles"));
    }
    let state = g.next.next_multiple_of(8);
    let made = std::mem::take(&mut g.made);
    let mut func = g.b.finish();
    dce(&mut func);
    Ok((func, out, state, probes, made))
}

struct Gen<'a> {
    b: Builder,
    module: &'a mut Module,
    rt: &'a mut Rt,
    /// Source column to its values address, validity address and type.
    cols: HashMap<usize, (Val, Val, Ty)>,
    /// The `LIKE`s the driver answers, and the address of each one's answers and of its column's
    /// validity.
    matched: Vec<Matched>,
    likes: Vec<(Val, Val)>,
    /// Source columns already read for the current row, in a block every later use is dominated
    /// by.
    loaded: HashMap<usize, (Val, Val)>,
    /// The row number.
    row: Val,
    /// The sink's buffer addresses, read once in the entry block: a result's values and validity
    /// per column, or an aggregate's one row.
    ptrs: Vec<Val>,
    /// The source's columns, for the names an error message quotes.
    columns: Vec<Column>,
    /// Where the next field goes in the state, past the sink's and every `vcall` slot so far.
    next: u32,
    /// The builds of the tables the probes read, in the order of the probes.
    joins: Vec<Building>,
    /// Per probe, the directory's address, the shift and the tag table's address, read once in
    /// the entry block.
    tables: Vec<(Val, Val, Val)>,
    /// Whether every column the body reads is taken as valid, for the `nonull` version.
    nonull: bool,
    /// The handles this pass made in the runtime, in order.
    made: Vec<u64>,
    /// On the second pass, the first one's handles and how many of them are handed out so far.
    replay: Option<(&'a [u64], usize)>,
}

/// A value and whether it is valid.
type Pair = (Val, Val);

impl Gen<'_> {
    fn field(&mut self, offset: u32, size: u32, name: &str) {
        self.b.func_mut().state.push(Field { offset, size, name: name.into() });
    }

    /// Lays out the sink's state and makes its table, and returns the layout and the state size.
    fn prepare_sink(&mut self, sink: &Sink, fans_out: bool) -> Result<(Out, u32)> {
        match sink {
            Sink::Result { exprs, columns } => {
                self.field(SINK, 8, "count");
                let mut slots = Vec::with_capacity(exprs.len());
                for (k, (e, c)) in exprs.iter().zip(columns).enumerate() {
                    let values = SINK + 8 + 16 * k as u32;
                    self.field(values, 8, &format!("out{k}.values"));
                    self.field(values + 8, 8, &format!("out{k}.valid"));
                    slots.push(Slot {
                        values,
                        valid: values + 8,
                        ty: qir_type(&e.ty)?,
                        logical: c.ty.clone(),
                    });
                }
                let mut state = SINK + 8 + 16 * exprs.len() as u32;
                let capacity = fans_out.then(|| {
                    self.field(state, 8, "capacity");
                    state += 8;
                    state - 8
                });
                Ok((Out::Result { count: SINK, columns: slots, capacity }, state))
            }
            Sink::Build { keys, payload, .. } => {
                let mut size = 0u32;
                let mut field = |e: &Expr, what: &str| -> Result<KeyField> {
                    let ty = qir_type(&e.ty)?;
                    if ty.is_float() && what == "key" {
                        return Err(Refusal::new(
                            "HashJoin",
                            "a floating point key, which the table would compare by its bits",
                        ));
                    }
                    let f = KeyField { offset: size, width: ty.bytes(), text: ty == Ty::Str16 };
                    size += f.width + 1;
                    Ok(f)
                };
                let keys = keys.iter().map(|e| field(e, "key")).collect::<Result<Vec<_>>>()?;
                let payload =
                    payload.iter().map(|e| field(e, "payload")).collect::<Result<Vec<_>>>()?;
                let layout = JoinLayout { keys, payload, size };
                let table = self.once(|g| g.rt.add_join(JoinTable::new(layout.clone())))?;
                let buffer = size.next_multiple_of(8);
                self.field(SINK, buffer, "record");
                Ok((Out::Build(Building { table, layout, record: SINK }), SINK + buffer))
            }
            Sink::Aggregate { groups, aggregates, .. } => {
                let mut keys = Vec::with_capacity(groups.len());
                let mut size = 0u32;
                for e in groups {
                    let ty = qir_type(&e.ty)?;
                    if ty.is_float() {
                        return Err(Refusal::new(
                            "grouping by a floating point value",
                            "the table compares keys as bytes, and 0.0 and -0.0 are one group",
                        ));
                    }
                    let width = ty.bytes();
                    keys.push((
                        KeyField { offset: size, width, text: ty == Ty::Str16 },
                        e.ty.clone(),
                    ));
                    size += width + 1;
                }
                let mut accs = Vec::with_capacity(aggregates.len());
                let mut at = 0u32;
                for a in aggregates {
                    let acc = self.accumulator(a, at)?;
                    at += acc_size(&acc)?;
                    accs.push(acc);
                }
                let layout = Layout {
                    keys: keys.iter().map(|(k, _)| *k).collect(),
                    key_size: size,
                    init: vec![0; at as usize],
                };
                let acc_offset = Layout::acc_offset(size);
                let table = self.once(|g| g.rt.add_table(GroupTable::new(layout)))?;
                let (row, last, state) = if groups.is_empty() {
                    self.field(SINK, 8, "row");
                    (Some(SINK), None, SINK + 8)
                } else {
                    let size = size.next_multiple_of(8);
                    self.field(SINK, size, "key");
                    if keys.iter().any(|(k, _)| k.text) {
                        (None, None, SINK + size)
                    } else {
                        self.field(SINK + size, 8, "last");
                        self.field(SINK + size + 8, 32, "probe");
                        (None, Some(SINK + size), SINK + size + 40)
                    }
                };
                let probe = last.map(|at| at + 8);
                let grouping = Grouping { table, keys, acc_offset, accs, row, last, probe };
                Ok((Out::Aggregate(grouping), state))
            }
        }
    }

    fn accumulator(&mut self, a: &Aggregate, offset: u32) -> Result<Acc> {
        let arg = a.args.first().map_or_else(|| a.ty.clone(), |e| e.ty.clone());
        let argty = qir_type(&arg)?;
        let refuse = || {
            Refusal::new(format!("{}({arg})", a.name), "the generator has no accumulator for it")
        };
        let op = match (a.name.as_str(), a.distinct) {
            ("count_star", _) => AccOp::CountStar,
            ("count", true) => {
                if argty.is_float() {
                    return Err(refuse());
                }
                AccOp::Distinct(self.once(|g| g.rt.add_distinct())?)
            }
            ("count", false) => AccOp::Count,
            ("sum", _) if argty.is_int() && qir_type(&a.ty)? == Ty::I128 && !unsigned(&arg) => {
                if argty.bytes() <= 2 {
                    AccOp::SumNarrow
                } else {
                    AccOp::SumInt
                }
            }
            ("sum", _) if argty.is_float() && qir_type(&a.ty)? == Ty::F64 => AccOp::SumFloat,
            ("avg", _) if argty.is_int() && !unsigned(&arg) && qir_type(&a.ty)? == Ty::F64 => {
                if argty.bytes() <= 2 {
                    AccOp::AvgNarrow
                } else {
                    AccOp::AvgInt
                }
            }
            ("avg", _) if argty.is_float() && qir_type(&a.ty)? == Ty::F64 => AccOp::AvgFloat,
            ("min", _) if argty == Ty::Str16 => AccOp::MinStr,
            ("max", _) if argty == Ty::Str16 => AccOp::MaxStr,
            ("min", _) => AccOp::Min,
            ("max", _) => AccOp::Max,
            ("any_value", _) => AccOp::AnyValue,
            _ => return Err(refuse()),
        };
        Ok(Acc { offset, op, arg, ty: a.ty.clone() })
    }

    /// Reads every source column `e` uses that is not read yet, in the current block. In the
    /// `nonull` version every value is valid, and what the builder folds away with that is every
    /// validity check downstream of the read.
    fn load_columns(&mut self, e: &Expr) {
        for c in e.columns() {
            if self.loaded.contains_key(&c) {
                continue;
            }
            // A column only an answered `LIKE` reads is not in the morsel's table.
            let Some(&(values, valid, ty)) = self.cols.get(&c) else { continue };
            let v = self.b.load(ty, values, self.row, ty.bytes(), 0, 0);
            if self.nonull {
                let ok = self.truth();
                self.loaded.insert(c, (v, ok));
                continue;
            }
            let ok = self.b.load_bit(valid, self.row);
            let zero = self.zero(ty);
            let v = self.b.select(ok, v, zero);
            self.loaded.insert(c, (v, ok));
        }
    }

    /// A handle the first pass over a pipeline makes in the runtime with `make`, and the second
    /// hands out again, so that both versions of the function share their tables and kernels.
    fn once(&mut self, make: impl FnOnce(&mut Self) -> u64) -> Result<u64> {
        let Some((made, used)) = self.replay else {
            let h = make(self);
            self.made.push(h);
            return Ok(h);
        };
        self.replay = Some((made, used + 1));
        made.get(used).copied().ok_or_else(|| {
            Refusal::new("the second version of a pipeline", "it made more handles than the first")
        })
    }

    fn zero(&mut self, ty: Ty) -> Val {
        self.b.konst(ty, 0)
    }

    fn truth(&mut self) -> Val {
        self.b.bool(true)
    }

    /// Translates `e` with its columns read first.
    fn expr(&mut self, e: &Expr) -> Result<Pair> {
        self.load_columns(e);
        self.translate(e)
    }

    fn translate(&mut self, e: &Expr) -> Result<Pair> {
        let ty = qir_type(&e.ty)?;
        match &e.kind {
            Kind::Column(c) => Ok(self.loaded[c]),
            Kind::Constant(v) => self.constant(v, &e.ty, ty),
            Kind::Cast { input, try_cast } => self.cast(input, &e.ty, ty, *try_cast),
            Kind::Compare { op, left, right } => self.compare(*op, left, right),
            Kind::And(children) => {
                let (mut all_valid, mut any_false, mut value) =
                    (self.truth(), self.b.bool(false), self.truth());
                for c in children {
                    let (v, ok) = self.translate(c)?;
                    let v = self.b.bin(Op::And, v, ok);
                    let not_v = self.b.un(Op::Not, v);
                    let is_false = self.b.bin(Op::And, ok, not_v);
                    any_false = self.b.bin(Op::Or, any_false, is_false);
                    all_valid = self.b.bin(Op::And, all_valid, ok);
                    value = self.b.bin(Op::And, value, v);
                }
                let valid = self.b.bin(Op::Or, any_false, all_valid);
                let value = self.b.bin(Op::And, value, all_valid);
                Ok((value, valid))
            }
            Kind::Or(children) => {
                let (mut all_valid, mut any_true) = (self.truth(), self.b.bool(false));
                for c in children {
                    let (v, ok) = self.translate(c)?;
                    let v = self.b.bin(Op::And, v, ok);
                    any_true = self.b.bin(Op::Or, any_true, v);
                    all_valid = self.b.bin(Op::And, all_valid, ok);
                }
                let valid = self.b.bin(Op::Or, any_true, all_valid);
                Ok((any_true, valid))
            }
            Kind::Function { name, args } => self.function(name, args, e),
            Kind::Case { arms, otherwise } => {
                let join = self.b.block(&[(ty, "case"), (Ty::I1, "case.ok")]);
                for (when, then) in arms {
                    let (c, ok) = self.translate(when)?;
                    let c = self.b.bin(Op::And, c, ok);
                    let (yes, no) = (self.b.block(&[]), self.b.block(&[]));
                    self.b.brif(c, yes, &[], no, &[]);
                    self.b.switch_to(yes);
                    let (v, ok) = self.translate(then)?;
                    self.b.br(join, &[v, ok]);
                    self.b.switch_to(no);
                }
                let (v, ok) = match otherwise {
                    Some(o) => self.translate(o)?,
                    None => (self.zero(ty), self.b.bool(false)),
                };
                self.b.br(join, &[v, ok]);
                self.b.switch_to(join);
                Ok((self.b.param(join, 0), self.b.param(join, 1)))
            }
        }
    }

    fn constant(&mut self, v: &Value, logical: &LogicalType, ty: Ty) -> Result<Pair> {
        let bits: u128 = match v {
            Value::Null => return Ok((self.zero(ty), self.b.bool(false))),
            Value::Boolean(b) => u128::from(*b),
            Value::TinyInt(x) => *x as u128,
            Value::SmallInt(x) => *x as u128,
            Value::Integer(x) | Value::Date(x) => *x as u128,
            Value::BigInt(x) | Value::Time(x) | Value::Timestamp(x) | Value::TimestampTz(x) => {
                *x as u128
            }
            Value::HugeInt(x) => *x as u128,
            Value::UTinyInt(x) => u128::from(*x),
            Value::USmallInt(x) => u128::from(*x),
            Value::UInteger(x) => u128::from(*x),
            Value::UBigInt(x) => u128::from(*x),
            Value::UHugeInt(x) => *x,
            Value::Float(x) => u128::from(x.to_bits()),
            Value::Double(x) => u128::from(x.to_bits()),
            Value::Decimal { unscaled, .. } => *unscaled as u128,
            Value::Varchar(s) => self.literal(s.as_bytes()),
            Value::Blob(b) => self.literal(b),
            other => {
                return Err(Refusal::new(
                    format!("the constant {other}"),
                    format!("{logical} constants are not generated"),
                ));
            }
        };
        let valid = self.truth();
        Ok((self.b.konst(ty, bits), valid))
    }

    /// A string constant, inline or kept in the runtime's heap for the whole query.
    fn literal(&mut self, bytes: &[u8]) -> u128 {
        if bytes.len() <= text::INLINE { text::make(bytes) } else { self.rt.keep(bytes) }
    }

    fn error(&mut self, kind: ErrorKind, text: String) -> u32 {
        self.module.error(kind, &text)
    }

    fn cast(&mut self, input: &Expr, to: &LogicalType, ty: Ty, try_cast: bool) -> Result<Pair> {
        let from = qir_type(&input.ty)?;
        let (v, ok) = self.translate(input)?;
        let refuse =
            || Refusal::new(format!("CAST({} AS {to})", input.ty), "the cast is not generated");
        if matches!(input.ty, LogicalType::Decimal { .. })
            || matches!(to, LogicalType::Decimal { .. })
        {
            return self.decimal_cast(&input.ty, to, (v, ok), try_cast)?.ok_or_else(refuse);
        }
        let numeric = |t: &LogicalType| {
            matches!(
                t,
                LogicalType::TinyInt
                    | LogicalType::SmallInt
                    | LogicalType::Integer
                    | LogicalType::BigInt
                    | LogicalType::HugeInt
                    | LogicalType::UTinyInt
                    | LogicalType::USmallInt
                    | LogicalType::UInteger
                    | LogicalType::UBigInt
                    | LogicalType::Float
                    | LogicalType::Double
            )
        };
        if input.ty == *to {
            return Ok((v, ok));
        }
        if !numeric(&input.ty) || !numeric(to) {
            return Err(refuse());
        }
        let out = match (from.is_float(), ty.is_float()) {
            (false, true) => {
                let op = if unsigned(&input.ty) { Op::Uitof } else { Op::Sitof };
                self.b.conv(op, v, ty)
            }
            (true, true) => {
                let op = if ty.bits() > from.bits() { Op::Fext } else { Op::Ftrunc };
                self.b.conv(op, v, ty)
            }
            (true, false) => return Err(refuse()),
            (false, false) => {
                if unsigned(&input.ty) != unsigned(to) {
                    return Err(refuse());
                }
                let ext = if unsigned(&input.ty) { Op::Zext } else { Op::Sext };
                if ty.bits() > from.bits() {
                    self.b.conv(ext, v, ty)
                } else if ty.bits() == from.bits() {
                    v
                } else {
                    let narrow = self.b.conv(Op::Trunc, v, ty);
                    let back = self.b.conv(ext, narrow, from);
                    let fits = self.b.bin(Op::IcmpEq, back, v);
                    if try_cast {
                        let ok = self.b.bin(Op::And, ok, fits);
                        let zero = self.zero(ty);
                        let narrow = self.b.select(ok, narrow, zero);
                        return Ok((narrow, ok));
                    }
                    let err = self.error(
                        ErrorKind::Conversion,
                        format!("Type {} with value out of range for {to}", input.ty),
                    );
                    let fail = self.b.block(&[]);
                    let good = self.b.block(&[]);
                    self.b.set_cold(fail);
                    self.b.brif(fits, good, &[], fail, &[]);
                    self.b.switch_to(fail);
                    self.b.trap(err);
                    self.b.switch_to(good);
                    narrow
                }
            }
        };
        Ok((out, ok))
    }

    /// A cast into a decimal, or out of one into a double, the way the first engine's `to_decimal`
    /// and `approximate` do it. `None` for any other cast with a decimal on one side.
    ///
    /// A decimal is its unscaled integer, so a cast into one is a widening, a multiply or a divide
    /// by a power of ten for the change of scale, rounded half away from zero the way `rescale`
    /// rounds, a check that the answer has no more digits than the width, and a narrowing. The
    /// check is left out where the digits the input can have already fit, which is every cast a
    /// plan makes to line the two sides of an operator up.
    fn decimal_cast(
        &mut self,
        input: &LogicalType,
        to: &LogicalType,
        (v, ok): Pair,
        try_cast: bool,
    ) -> Result<Option<Pair>> {
        let from = qir_type(input)?;
        let ty = qir_type(to)?;
        if let (LogicalType::Decimal { scale, .. }, LogicalType::Double) = (input, to) {
            let x = self.b.conv(Op::Sitof, v, Ty::F64);
            let p = self.b.f64(pow10(u32::from(*scale)) as f64);
            return Ok(Some((self.b.bin(Op::Fdiv, x, p), ok)));
        }
        let LogicalType::Decimal { width, scale } = *to else { return Ok(None) };
        if unsigned(input) || from == Ty::I128 && !matches!(input, LogicalType::Decimal { .. }) {
            return Ok(None);
        }
        // The digits before the point the input can have, and the scale it is held at.
        let Some((digits, held)) = input.decimal_shape() else { return Ok(None) };
        let whole = digits.saturating_sub(held);
        // Rounding a scale down can carry into one more digit, 9.99 to 10.0.
        let carry = u8::from(scale < held);
        let check = whole + carry > width - scale;
        if check && try_cast {
            return Ok(None);
        }
        let work = if ty.bits() > from.bits() { ty } else { from };
        let mut v = if work == from { v } else { self.b.conv(Op::Sext, v, work) };
        let text =
            format!("Casting a value of type {input} to type {to} failed: value is out of range!");
        if scale > held {
            // Overflowing the working type is past ten to the width too, so the trap is the same
            // error the check below would have raised.
            let err = self.error(ErrorKind::Conversion, text.clone());
            v = self.b.dup(v, u32::from(scale - held), err);
        } else if scale < held {
            v = self.b.ddown(v, u32::from(held - scale));
        }
        if check {
            let err = self.error(ErrorKind::Conversion, text);
            self.within(v, work, width, err);
        }
        if ty.bits() < work.bits() {
            v = self.b.conv(Op::Trunc, v, ty);
        }
        Ok(Some((v, ok)))
    }

    /// Decimal `+`, `-` and `*` on the unscaled integers, the way the first engine's decimal runs
    /// do them. `None`, with nothing emitted, for a call whose scales are not lined up the way the
    /// binder lines them up, which the kernel takes instead.
    ///
    /// A sum or a difference wants both sides at the answer's scale and a product has the two
    /// scales added, so neither is rescaled. What is left is the add, the subtract or the multiply
    /// in the answer's type, trapped when that type overflows, and a check that the answer has no
    /// more digits than its width. The check is left out when the widths of the two sides already
    /// say it cannot, which is how the binder picks an answer's width for everything but a width
    /// capped at 38.
    fn decimal_arithmetic(
        &mut self,
        name: &str,
        l: &Expr,
        r: &Expr,
        e: &Expr,
    ) -> Result<Option<Pair>> {
        let LogicalType::Decimal { width, scale } = e.ty else { return Ok(None) };
        let (Some((lw, ls)), Some((rw, rs))) = (l.ty.decimal_shape(), r.ty.decimal_shape()) else {
            return Ok(None);
        };
        let ty = qir_type(&e.ty)?;
        let (lt, rt) = (qir_type(&l.ty)?, qir_type(&r.ty)?);
        let product = name == "*";
        let lined = if product { ls + rs == scale } else { ls == scale && rs == scale };
        if !lined
            || unsigned(&l.ty)
            || unsigned(&r.ty)
            || lt.bits() > ty.bits()
            || rt.bits() > ty.bits()
        {
            return Ok(None);
        }
        let (a, va) = self.translate(l)?;
        let (b, vb) = self.translate(r)?;
        let a = if lt == ty { a } else { self.b.conv(Op::Sext, a, ty) };
        let b = if rt == ty { b } else { self.b.conv(Op::Sext, b, ty) };
        let (op, word) = match name {
            "+" => (Op::SaddT, "addition"),
            "-" => (Op::SsubT, "subtract"),
            _ => (Op::SmulT, "multiplication"),
        };
        let text = format!("Overflow in {word} of {}", e.ty.physical_name());
        let err = self.error(ErrorKind::Overflow, text);
        let v = self.b.checked(op, a, b, err);
        let reach = if product { lw.saturating_add(rw) } else { lw.max(rw).saturating_add(1) };
        if reach > width {
            self.within(v, ty, width, err);
        }
        let valid = self.b.bin(Op::And, va, vb);
        Ok(Some((v, valid)))
    }

    /// Traps with `err` unless `v`, a decimal's unscaled integer in `ty`, is under ten to `width`
    /// either way, which is the first engine's test for a value having no more digits than that.
    fn within(&mut self, v: Val, ty: Ty, width: u8, err: u32) {
        let limit = pow10(u32::from(width));
        let high = self.b.int(ty, limit);
        let low = self.b.int(ty, -limit);
        let below = self.b.bin(Op::IcmpSlt, v, high);
        let above = self.b.bin(Op::IcmpSgt, v, low);
        let fits = self.b.bin(Op::And, below, above);
        let fail = self.b.block(&[]);
        let good = self.b.block(&[]);
        self.b.set_cold(fail);
        self.b.brif(fits, good, &[], fail, &[]);
        self.b.switch_to(fail);
        self.b.trap(err);
        self.b.switch_to(good);
    }

    fn compare(&mut self, op: CompareOp, left: &Expr, right: &Expr) -> Result<Pair> {
        let ty = qir_type(&left.ty)?;
        // A decimal is its unscaled integer, so two of them only compare as integers at one scale,
        // and a decimal and an integer only at scale zero, which the binder does not leave.
        let scale = |t: &LogicalType| match *t {
            LogicalType::Decimal { scale, .. } => Some(scale),
            _ => None,
        };
        if scale(&left.ty) != scale(&right.ty) {
            return Err(Refusal::new(
                format!("comparing {} with {}", left.ty, right.ty),
                "the two sides are decimals at different scales",
            ));
        }
        if qir_type(&right.ty)? != ty {
            return Err(Refusal::new(
                format!("comparing {} with {}", left.ty, right.ty),
                "the two sides have different types",
            ));
        }
        let (a, va) = self.translate(left)?;
        let (b, vb) = self.translate(right)?;
        let eq = |g: &mut Gen<'_>| -> Val {
            if ty == Ty::Str16 {
                if is_empty_string(right) || is_empty_string(left) {
                    let s = if is_empty_string(right) { a } else { b };
                    let len = g.b.un(Op::StrLen, s);
                    let zero = g.b.int(Ty::I32, 0);
                    return g.b.bin(Op::IcmpEq, len, zero);
                }
                return g.rt(proxy_id("str_eq"), &[a, b]);
            }
            let op = if ty.is_float() { Op::FcmpEq } else { Op::IcmpEq };
            g.b.bin(op, a, b)
        };
        let both = self.b.bin(Op::And, va, vb);
        let value = match op {
            CompareOp::Equal => eq(self),
            CompareOp::NotEqual => {
                let e = eq(self);
                self.b.un(Op::Not, e)
            }
            CompareOp::DistinctFrom | CompareOp::NotDistinctFrom => {
                // Two nulls are not distinct, a null and a value are, and two values are when
                // they are not equal.
                let e = eq(self);
                let neither = self.b.bin(Op::Or, va, vb);
                let neither = self.b.un(Op::Not, neither);
                let equal = self.b.select(both, e, neither);
                let v = if matches!(op, CompareOp::DistinctFrom) {
                    self.b.un(Op::Not, equal)
                } else {
                    equal
                };
                let valid = self.truth();
                return Ok((v, valid));
            }
            CompareOp::Less
            | CompareOp::LessOrEqual
            | CompareOp::Greater
            | CompareOp::GreaterOrEqual => {
                // Greater is Less with the sides swapped, so there are two orders to generate.
                let (x, y) = if matches!(op, CompareOp::Less | CompareOp::LessOrEqual) {
                    (a, b)
                } else {
                    (b, a)
                };
                let or_equal = matches!(op, CompareOp::LessOrEqual | CompareOp::GreaterOrEqual);
                if ty == Ty::Str16 {
                    let c = self.rt(proxy_id("str_cmp"), &[x, y]);
                    let zero = self.b.int(Ty::I32, 0);
                    self.b.bin(if or_equal { Op::IcmpSle } else { Op::IcmpSlt }, c, zero)
                } else if ty.is_float() {
                    self.b.bin(if or_equal { Op::FcmpLe } else { Op::FcmpLt }, x, y)
                } else if unsigned(&left.ty) {
                    self.b.bin(if or_equal { Op::IcmpUle } else { Op::IcmpUlt }, x, y)
                } else {
                    self.b.bin(if or_equal { Op::IcmpSle } else { Op::IcmpSlt }, x, y)
                }
            }
        };
        Ok((value, both))
    }

    /// An `rtcall` of a proxy with a result.
    fn rt(&mut self, id: u32, args: &[Val]) -> Val {
        self.b.rtcall(id, args).unwrap_or(Val::NONE)
    }

    fn handle(&mut self, h: u64) -> Val {
        self.b.konst(Ty::Ptr, u128::from(h))
    }

    /// A scalar function: inline when there is a translator for this call and a `vcall` to the
    /// first engine's kernel when there is not.
    fn function(&mut self, name: &str, args: &[Expr], e: &Expr) -> Result<Pair> {
        match self.inline(name, args, e)? {
            Some(pair) => Ok(pair),
            None => self.vcall(name, args, e),
        }
    }

    /// The call translated inline, or `None` when no translator takes it. Every check that can say
    /// no is made before an argument is translated, so a call that falls through to the `vcall`
    /// has emitted nothing here.
    fn inline(&mut self, name: &str, args: &[Expr], e: &Expr) -> Result<Option<Pair>> {
        let ty = qir_type(&e.ty)?;
        // The two's complement add, subtract, multiply and negate with an overflow trap are
        // what the first engine does for a signed integer, and IEEE arithmetic is what it does for
        // a float. A decimal and an unsigned integer are left to the kernel.
        let signed = ty.is_int()
            && ty != Ty::I1
            && !unsigned(&e.ty)
            && !matches!(e.ty, LogicalType::Decimal { .. });
        let arithmetic = ty.is_float() || signed;
        Ok(Some(match (name, args) {
            ("+" | "-" | "*", [l, r]) if matches!(e.ty, LogicalType::Decimal { .. }) => {
                return self.decimal_arithmetic(name, l, r, e);
            }
            ("-", [x]) if matches!(e.ty, LogicalType::Decimal { .. }) && x.ty == e.ty => {
                // A decimal's range is the same either side of zero, so this only traps where
                // the type itself does.
                let (a, ok) = self.translate(x)?;
                let text = format!("Overflow in negation of {}", e.ty.physical_name());
                let err = self.error(ErrorKind::Overflow, text);
                (self.b.checked_neg(a, err), ok)
            }
            ("+" | "-" | "*", [l, r]) => {
                if !arithmetic || qir_type(&l.ty)? != ty || qir_type(&r.ty)? != ty {
                    return Ok(None);
                }
                let (a, va) = self.translate(l)?;
                let (b, vb) = self.translate(r)?;
                let valid = self.b.bin(Op::And, va, vb);
                let v = if ty.is_float() {
                    let op = match name {
                        "+" => Op::Fadd,
                        "-" => Op::Fsub,
                        _ => Op::Fmul,
                    };
                    self.b.bin(op, a, b)
                } else {
                    let (op, word) = match name {
                        "+" => (Op::SaddT, "addition"),
                        "-" => (Op::SsubT, "subtraction"),
                        _ => (Op::SmulT, "multiplication"),
                    };
                    let err = self.error(
                        ErrorKind::Overflow,
                        format!("Overflow in {word} of {}", e.ty.physical_name()),
                    );
                    self.b.checked(op, a, b, err)
                };
                (v, valid)
            }
            ("-", [x]) => {
                if !arithmetic || qir_type(&x.ty)? != ty {
                    return Ok(None);
                }
                let (a, ok) = self.translate(x)?;
                if ty.is_float() {
                    (self.b.un(Op::Fneg, a), ok)
                } else {
                    let err = self.error(
                        ErrorKind::Overflow,
                        format!("Overflow in negation of {}", e.ty.physical_name()),
                    );
                    (self.b.checked_neg(a, err), ok)
                }
            }
            ("~~" | "!~~" | "~~*" | "!~~*", [s, pattern]) => {
                let Kind::Constant(Value::Varchar(p)) = &pattern.kind else { return Ok(None) };
                let fold = name.contains('*');
                if let Some(at) = answered(e, &self.matched) {
                    let (answers, valid) = self.likes[at];
                    let ok =
                        if self.nonull { self.truth() } else { self.b.load_bit(valid, self.row) };
                    let m = self.b.load(Ty::I1, answers, self.row, 1, 0, 0);
                    let no = self.b.bool(false);
                    let m = self.b.select(ok, m, no);
                    let m = if name.starts_with('!') { self.b.un(Op::Not, m) } else { m };
                    return Ok(Some((m, ok)));
                }
                let h = self.rt.add_like(p, fold);
                let (s, ok) = self.translate(s)?;
                let h = self.handle(h);
                let m = self.rt(proxy_id("str_like"), &[h, s]);
                let m = if name.starts_with('!') { self.b.un(Op::Not, m) } else { m };
                (m, ok)
            }
            ("strlen" | "octet_length", [s])
                if s.ty == LogicalType::Varchar || s.ty == LogicalType::Blob =>
            {
                let (s, ok) = self.translate(s)?;
                let n = self.b.un(Op::StrLen, s);
                (self.b.conv(Op::Zext, n, Ty::I64), ok)
            }
            ("length" | "char_length", [s]) if s.ty == LogicalType::Varchar => {
                let (s, ok) = self.translate(s)?;
                (self.rt(proxy_id("str_length"), &[s]), ok)
            }
            ("lower" | "upper", [s]) if s.ty == LogicalType::Varchar => {
                let (s, ok) = self.translate(s)?;
                let id = proxy_id(if name == "lower" { "str_lower" } else { "str_upper" });
                (self.rt(id, &[s]), ok)
            }
            ("||", [l, r]) if l.ty == LogicalType::Varchar && r.ty == LogicalType::Varchar => {
                let (a, va) = self.translate(l)?;
                let (b, vb) = self.translate(r)?;
                let valid = self.b.bin(Op::And, va, vb);
                (self.rt(proxy_id("str_concat"), &[a, b]), valid)
            }
            ("regexp_replace" | "regexp_matches", [s, pattern, rest @ ..]) => {
                let text = |e: &Expr| match &e.kind {
                    Kind::Constant(Value::Varchar(p)) => Some(p.clone()),
                    _ => None,
                };
                let Some(pattern) = text(pattern) else { return Ok(None) };
                let replace = name == "regexp_replace";
                let (rewrite, options) = match (replace, rest) {
                    (true, [r]) => (text(r), Some(String::new())),
                    (true, [r, o]) => (text(r), text(o)),
                    (false, []) => (Some(String::new()), Some(String::new())),
                    (false, [o]) => (Some(String::new()), text(o)),
                    _ => (None, None),
                };
                let (Some(rewrite), Some(options)) = (rewrite, options) else {
                    return Ok(None);
                };
                // A pattern that does not compile is the kernel's to report, with the first
                // engine's error, on the first row that reaches it.
                let Ok(h) = self.rt.add_regex(&pattern, &rewrite, &options) else {
                    return Ok(None);
                };
                let (s, ok) = self.translate(s)?;
                let h = self.handle(h);
                let id = proxy_id(if replace { "str_regex_replace" } else { "str_regex" });
                (self.rt(id, &[h, s]), ok)
            }
            ("date_part" | "datepart" | "extract", [part, x]) => {
                let Kind::Constant(Value::Varchar(part)) = &part.kind else { return Ok(None) };
                let id = match (part.to_ascii_lowercase().as_str(), &x.ty) {
                    ("minute", LogicalType::Timestamp) => "date_extract_minute",
                    ("year", LogicalType::Date) => "date_extract_year",
                    _ => return Ok(None),
                };
                if ty != Ty::I64 {
                    return Ok(None);
                }
                let (x, ok) = self.translate(x)?;
                (self.rt(proxy_id(id), &[x]), ok)
            }
            ("date_trunc" | "datetrunc", [part, x]) => {
                let Kind::Constant(Value::Varchar(part)) = &part.kind else { return Ok(None) };
                let id = match (part.to_ascii_lowercase().as_str(), &x.ty, &e.ty) {
                    ("minute", LogicalType::Timestamp, LogicalType::Timestamp) => {
                        "date_trunc_minute"
                    }
                    ("month", LogicalType::Date, LogicalType::Date) => "date_trunc_month",
                    _ => return Ok(None),
                };
                let (x, ok) = self.translate(x)?;
                (self.rt(proxy_id(id), &[x]), ok)
            }
            _ => return Ok(None),
        }))
    }

    /// A call with no translator, run by the first engine's own kernel one row at a time.
    ///
    /// Each argument is stored in a slot of the state, a sixteen byte value and a validity byte,
    /// and the kernel writes the answer into one more slot, which is read back after the call. The
    /// kernel is made here and registered in the runtime and the module together, so the id the
    /// code carries names the same kernel in both.
    fn vcall(&mut self, name: &str, args: &[Expr], e: &Expr) -> Result<Pair> {
        let refuse = |why: &str| {
            let types = args.iter().map(|a| a.ty.to_string()).collect::<Vec<_>>().join(", ");
            Refusal::new(format!("{name}({types})"), why)
        };
        // The first engine takes a row count from the arguments, and a call with none, `random()`
        // being the one there is, gets the chunk's instead. A `TRY` is not a function at all but
        // an expression whose errors become nulls, and a kernel cannot see the expression.
        if args.is_empty() {
            return Err(refuse("a function with no arguments is not generated"));
        }
        if name == "try" {
            return Err(refuse("TRY is not generated"));
        }
        let ty = qir_type(&e.ty)?;
        for a in args {
            qir_type(&a.ty)?;
        }
        let mut pairs = Vec::with_capacity(args.len());
        for a in args {
            pairs.push(self.translate(a)?);
        }
        // Every worker of a parallel pipeline makes its own, because a call keeps the strings it
        // answered with.
        let (name_, args_, ty_, columns_) =
            (name.to_string(), args.to_vec(), e.ty.clone(), self.columns.clone());
        let made = self.once(|g| {
            let id = g.rt.add_kernel(std::sync::Arc::new(move || {
                let mut call = vcall::Call::new(&name_, &args_, &ty_, &columns_);
                Box::new(move |n, buffers| call.run(n, buffers))
            }));
            if g.module.kernel(name) == id { u64::from(id) } else { u64::MAX }
        })?;
        let Ok(id) = u32::try_from(made) else {
            return Err(Refusal::new(
                format!("the kernel for {name}"),
                "the runtime and the module number their kernels differently",
            ));
        };
        let st = self.b.st();
        let mut buffers = Vec::with_capacity(2 * args.len() + 2);
        for (k, (v, ok)) in pairs.into_iter().enumerate() {
            let at = self.slot(&format!("k{id}.arg{k}"));
            self.b.store(st, Val::NONE, 1, at as i32, v, 0);
            self.b.store(st, Val::NONE, 1, at as i32 + 16, ok, 0);
            buffers.push(self.offset(st, at));
            buffers.push(self.offset(st, at + 16));
        }
        let at = self.slot(&format!("k{id}.out"));
        buffers.push(self.offset(st, at));
        buffers.push(self.offset(st, at + 16));
        let one = self.b.int(Ty::I64, 1);
        self.b.vcall(id, one, &buffers);
        let v = self.b.load(ty, st, Val::NONE, 1, at as i32, 0);
        let ok = self.b.load(Ty::I1, st, Val::NONE, 1, at as i32 + 16, 0);
        Ok((v, ok))
    }

    /// A value slot of sixteen bytes and a validity byte after it, past everything in the state so
    /// far.
    fn slot(&mut self, name: &str) -> u32 {
        let at = self.next.next_multiple_of(16);
        self.field(at, 16, name);
        self.field(at + 16, 1, &format!("{name}.valid"));
        self.next = at + 17;
        at
    }

    /// The address `base + offset`.
    fn offset(&mut self, base: Val, offset: u32) -> Val {
        let x = self.b.conv(Op::Bitcast, base, Ty::I64);
        let k = self.b.int(Ty::I64, i128::from(offset));
        let x = self.b.bin(Op::Add, x, k);
        self.b.conv(Op::Bitcast, x, Ty::Ptr)
    }

    /// Runs `ops` on the current row and then the sink, and goes to `skip` when done with it. The
    /// first probe in `ops` is probe number `probe` of the pipeline, and `depth` is the loop depth
    /// of the code so far.
    fn ops(
        &mut self,
        ops: &[PipeOp],
        probe: usize,
        skip: Block,
        depth: u8,
        sink: &Sink,
        out: &Out,
    ) -> Result<()> {
        let Some((op, rest)) = ops.split_first() else {
            self.sink(sink, out, skip, depth)?;
            self.b.br(skip, &[]);
            return Ok(());
        };
        match op {
            PipeOp::Filter(f) => {
                let (v, ok) = self.expr(f)?;
                let pass = self.b.bin(Op::And, v, ok);
                let then = self.b.block(&[]);
                self.b.brif(pass, then, &[], skip, &[]);
                self.b.switch_to(then);
                self.ops(rest, probe, skip, depth, sink, out)
            }
            PipeOp::Probe(p) => {
                let (head, e, stride, advance) = self.probe(p, probe, skip, depth + 1)?;
                self.ops(rest, probe + 1, advance, depth + 1, sink, out)?;
                self.b.switch_to(advance);
                let stride = self.b.int(Ty::I64, i128::from(stride));
                let e = self.b.bin(Op::Add, e, stride);
                self.b.br(head, &[e]);
                Ok(())
            }
        }
    }

    /// The fused probe of section 10.5: hashes the row's keys, tests the tag of the slot the hash
    /// picks, and starts a loop over the slot's entries that leaves the builder in the block of a
    /// match, with the payload read. A row with a null key, or whose tag says the slot cannot hold
    /// it, goes to `skip`. Returns the loop's header, the entry address it carries, the stride
    /// and the block that goes on to the next entry, which the caller ends once the code for a
    /// match is written.
    fn probe(
        &mut self,
        p: &Probe,
        n: usize,
        skip: Block,
        depth: u8,
    ) -> Result<(Block, Val, u32, Block)> {
        let layout = self.joins[n].layout.clone();
        let (directory, shift, tags) = self.tables[n];
        // A null key matches nothing, and the build never stored one.
        let mut hash = self.b.int(Ty::I64, 0);
        let mut keys = Vec::with_capacity(p.keys.len());
        for e in &p.keys {
            let (v, ok) = self.expr(e)?;
            let then = self.b.block(&[]);
            self.b.brif(ok, then, &[], skip, &[]);
            self.b.switch_to(then);
            hash = self.hash(hash, v, &e.ty)?;
            keys.push(v);
        }
        let fold = self.b.konst(Ty::I64, u128::from(FOLD));
        let h = self.b.bin(Op::Mul, hash, fold);
        let slot = self.b.bin(Op::Lshr, h, shift);
        let word = self.b.load(Ty::I64, directory, slot, 8, 0, 0);
        let low = self.b.int(Ty::I64, 2047);
        let at = self.b.bin(Op::And, h, low);
        let tag = self.b.load(Ty::I16, tags, at, 2, 0, 0);
        let tag = self.b.conv(Op::Zext, tag, Ty::I64);
        let k48 = self.b.int(Ty::I64, 48);
        let bloom = self.b.bin(Op::Lshr, word, k48);
        let seen = self.b.bin(Op::And, bloom, tag);
        let maybe = self.b.bin(Op::IcmpEq, seen, tag);
        let address = self.b.konst(Ty::I64, u128::from(ADDRESS));
        let lo = self.b.bin(Op::And, word, address);
        let hi = self.b.load(Ty::I64, directory, slot, 8, 8, 0);
        let hi = self.b.bin(Op::And, hi, address);

        let head = self.b.block(&[(Ty::I64, "entry")]);
        self.b.brif(maybe, head, &[lo], skip, &[]);
        self.b.switch_to(head);
        self.b.set_loop(head, depth);
        let e = self.b.param(head, 0);
        self.b.poll(1024);
        let more = self.b.bin(Op::IcmpUlt, e, hi);
        let candidate = self.b.block(&[]);
        self.b.brif(more, candidate, &[], skip, &[]);

        self.b.switch_to(candidate);
        let advance = self.b.block(&[]);
        let entry = self.b.conv(Op::Bitcast, e, Ty::Ptr);
        let stored = self.b.load(Ty::I64, entry, Val::NONE, 1, 0, 0);
        let same = self.b.bin(Op::IcmpEq, stored, hash);
        let check = self.b.block(&[]);
        self.b.brif(same, check, &[], advance, &[]);
        self.b.switch_to(check);
        for ((v, e), f) in keys.into_iter().zip(&p.keys).zip(&layout.keys) {
            let ty = qir_type(&e.ty)?;
            let x = self.b.load(ty, entry, Val::NONE, 1, 8 + f.offset as i32, 0);
            let eq = if ty == Ty::Str16 {
                self.rt(proxy_id("str_eq"), &[x, v])
            } else {
                self.b.bin(Op::IcmpEq, x, v)
            };
            let then = self.b.block(&[]);
            self.b.brif(eq, then, &[], advance, &[]);
            self.b.switch_to(then);
        }
        for (j, (f, c)) in layout.payload.iter().zip(&p.columns).enumerate() {
            let ty = qir_type(&c.ty)?;
            let v = self.b.load(ty, entry, Val::NONE, 1, 8 + f.offset as i32, 0);
            let ok = self.b.load(Ty::I1, entry, Val::NONE, 1, 8 + f.null() as i32, 0);
            self.loaded.insert(p.first + j, (v, ok));
        }
        Ok((head, e, layout.stride(), advance))
    }

    /// Looks for the group of the key in the key buffer through the words `ht_insert` published
    /// at `at`, the way [`GroupTable`] probes its slots. Returns the block to call `ht_insert` from
    /// when the key is not found or nothing is published yet, and a block with the row as its
    /// parameter that both ways go to. Keys are compared as values and null bytes, which is
    /// enough because a null key's value is zero.
    fn find_group(
        &mut self,
        g: &Grouping,
        values: &[Pair],
        hash: Val,
        at: u32,
        depth: u8,
    ) -> Result<(Block, Option<Block>)> {
        let st = self.b.st();
        let at = at as i32;
        let (slow, found) = (self.b.block(&[]), self.b.block(&[(Ty::Ptr, "row")]));
        let slots = self.b.load(Ty::Ptr, st, Val::NONE, 1, at, 0);
        let word = self.b.conv(Op::Bitcast, slots, Ty::I64);
        let zero = self.b.int(Ty::I64, 0);
        let published = self.b.bin(Op::IcmpNe, word, zero);
        let look = self.b.block(&[]);
        self.b.brif(published, look, &[], slow, &[]);
        self.b.switch_to(look);
        let mask = self.b.load(Ty::I64, st, Val::NONE, 1, at + 8, 0);
        let rows = self.b.load(Ty::Ptr, st, Val::NONE, 1, at + 16, 0);
        let k32 = self.b.int(Ty::I64, 32);
        let tag = self.b.bin(Op::Shl, hash, k32);
        let first = self.b.bin(Op::And, hash, mask);
        let head = self.b.block(&[(Ty::I64, "at")]);
        self.b.br(head, &[first]);
        self.b.switch_to(head);
        // The slots are never more than half full, so the loop ends at an empty one.
        self.b.set_loop(head, depth + 1);
        self.b.set_bounded(head);
        let i = self.b.param(head, 0);
        let slot = self.b.load(Ty::I64, slots, i, 8, 0, 0);
        let empty = self.b.bin(Op::IcmpEq, slot, zero);
        let (test, next) = (self.b.block(&[]), self.b.block(&[]));
        self.b.brif(empty, slow, &[], test, &[]);
        self.b.switch_to(test);
        let high = self.b.konst(Ty::I64, u128::from(!0xffff_ffffu64));
        let hi = self.b.bin(Op::And, slot, high);
        let tagged = self.b.bin(Op::IcmpEq, hi, tag);
        let check = self.b.block(&[]);
        self.b.brif(tagged, check, &[], next, &[]);
        self.b.switch_to(check);
        let low = self.b.int(Ty::I64, 0xffff_ffff);
        let gid = self.b.bin(Op::And, slot, low);
        let row = self.b.load(Ty::Ptr, rows, gid, 8, -8, 0);
        let mut same = self.b.bool(true);
        for (&(v, ok), (k, _)) in values.iter().zip(&g.keys) {
            let off = (8 + k.offset) as i32;
            let ty = self.b.ty(v);
            let x = self.b.load(ty, row, Val::NONE, 1, off, 0);
            let eq = self.b.bin(Op::IcmpEq, x, v);
            let null = self.b.load(Ty::I1, row, Val::NONE, 1, off + k.width as i32, 0);
            // The null byte is set where `ok` is not.
            let flip = self.b.bin(Op::Xor, null, ok);
            same = self.b.bin(Op::And, same, eq);
            same = self.b.bin(Op::And, same, flip);
        }
        let hit = self.b.block(&[]);
        self.b.brif(same, hit, &[], next, &[]);
        self.b.switch_to(hit);
        let n = self.b.load(Ty::I64, st, Val::NONE, 1, at + 24, 0);
        let one = self.b.int(Ty::I64, 1);
        let n = self.b.bin(Op::Add, n, one);
        self.b.store(st, Val::NONE, 1, at + 24, n, 0);
        self.b.br(found, &[row]);
        self.b.switch_to(next);
        let j = self.b.bin(Op::Add, i, one);
        let j = self.b.bin(Op::And, j, mask);
        self.b.br(head, &[j]);
        Ok((slow, Some(found)))
    }

    fn sink(&mut self, sink: &Sink, out: &Out, skip: Block, depth: u8) -> Result<()> {
        let st = self.b.st();
        match (sink, out) {
            (Sink::Result { exprs, .. }, Out::Result { count, columns, capacity }) => {
                let n = self.b.load(Ty::I64, st, Val::NONE, 1, *count as i32, 0);
                if let Some(at) = capacity {
                    // A probe can make more rows than the morsel has. When the buffers are full,
                    // the driver grows them and runs the morsel again from the start.
                    let cap = self.b.load(Ty::I64, st, Val::NONE, 1, *at as i32, 0);
                    let room = self.b.bin(Op::IcmpUlt, n, cap);
                    let (fits, full) = (self.b.block(&[]), self.b.block(&[]));
                    self.b.set_cold(full);
                    self.b.brif(room, fits, &[], full, &[]);
                    self.b.switch_to(full);
                    let status = self.b.int(Ty::I64, i128::from(NEED_MEMORY));
                    self.b.ret(status);
                    self.b.switch_to(fits);
                }
                for (k, (e, slot)) in exprs.iter().zip(columns).enumerate() {
                    let (v, ok) = self.expr(e)?;
                    let (values, valid) = (self.ptrs[2 * k], self.ptrs[2 * k + 1]);
                    self.b.store(values, n, slot.ty.bytes(), 0, v, 0);
                    self.b.store(valid, n, 1, 0, ok, 0);
                }
                let one = self.b.int(Ty::I64, 1);
                let n1 = self.b.bin(Op::Add, n, one);
                self.b.store(st, Val::NONE, 1, *count as i32, n1, 0);
                Ok(())
            }
            (Sink::Aggregate { groups, aggregates, .. }, Out::Aggregate(g)) => {
                let row = match g.row {
                    Some(at) => self.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, 4),
                    None => {
                        let mut values = Vec::with_capacity(groups.len());
                        for e in groups {
                            let (v, ok) = self.expr(e)?;
                            // A null key's value is zeroed, so that keys the same are the same
                            // bytes for the probe below.
                            let v = if g.probe.is_some() {
                                let zero = self.b.konst(self.b.ty(v), 0);
                                self.b.select(ok, v, zero)
                            } else {
                                v
                            };
                            values.push((v, ok));
                        }
                        // The key buffer still holds the last row's key, which a key the same
                        // bytes as it finds the same group, so its row is taken again.
                        let done = self.b.block(&[(Ty::Ptr, "row")]);
                        if let Some(last) = g.last {
                            let seen = self.b.load(Ty::I64, st, Val::NONE, 1, last as i32, 0);
                            let zero = self.b.int(Ty::I64, 0);
                            let mut same = self.b.bin(Op::IcmpNe, seen, zero);
                            for (&(v, ok), (k, _)) in values.iter().zip(&g.keys) {
                                let at = (SINK + k.offset) as i32;
                                let ty = self.b.ty(v);
                                let old = self.b.load(ty, st, Val::NONE, 1, at, 0);
                                let eq = self.b.bin(Op::IcmpEq, old, v);
                                let null =
                                    self.b.load(Ty::I1, st, Val::NONE, 1, at + k.width as i32, 0);
                                // The null byte is set where `ok` is not.
                                let flip = self.b.bin(Op::Xor, null, ok);
                                same = self.b.bin(Op::And, same, eq);
                                same = self.b.bin(Op::And, same, flip);
                            }
                            let seen = self.b.conv(Op::Bitcast, seen, Ty::Ptr);
                            let new = self.b.block(&[]);
                            self.b.brif(same, done, &[seen], new, &[]);
                            self.b.switch_to(new);
                        }
                        let mut hash = self.b.int(Ty::I64, 0);
                        for (&(v, ok), (e, (k, _))) in values.iter().zip(groups.iter().zip(&g.keys))
                        {
                            let at = (SINK + k.offset) as i32;
                            self.b.store(st, Val::NONE, 1, at, v, 0);
                            let null = self.b.un(Op::Not, ok);
                            self.b.store(st, Val::NONE, 1, at + k.width as i32, null, 0);
                            hash = self.hash(hash, v, &e.ty)?;
                            let ok = self.b.conv(Op::Zext, ok, Ty::I64);
                            hash = self.b.bin(Op::Crc32c, hash, ok);
                        }
                        let key = self.offset(st, SINK);
                        let table = self.handle(g.table);
                        let (slow, found) = match g.probe {
                            Some(at) => self.find_group(g, &values, hash, at, depth)?,
                            None => (self.b.current(), None),
                        };
                        self.b.switch_to(slow);
                        let published = match g.probe {
                            Some(at) => self.offset(st, at),
                            None => self.b.konst(Ty::Ptr, 0),
                        };
                        let row = self.rt(proxy_id("ht_insert"), &[table, key, hash, published]);
                        let row = match found {
                            Some(found) => {
                                self.b.br(found, &[row]);
                                self.b.switch_to(found);
                                self.b.param(found, 0)
                            }
                            None => row,
                        };
                        if let Some(last) = g.last {
                            self.b.store(st, Val::NONE, 1, last as i32, row, 0);
                        }
                        self.b.br(done, &[row]);
                        self.b.switch_to(done);
                        self.b.param(done, 0)
                    }
                };
                for (a, acc) in aggregates.iter().zip(&g.accs) {
                    self.update(a, acc, row, g.acc_offset + acc.offset)?;
                }
                Ok(())
            }
            (Sink::Build { keys, payload, .. }, Out::Build(b)) => {
                // A row with a null key can never match, so it stays out of the table.
                let mut hash = self.b.int(Ty::I64, 0);
                let mut values = Vec::with_capacity(keys.len() + payload.len());
                for e in keys {
                    let (v, ok) = self.expr(e)?;
                    let then = self.b.block(&[]);
                    self.b.brif(ok, then, &[], skip, &[]);
                    self.b.switch_to(then);
                    hash = self.hash(hash, v, &e.ty)?;
                    values.push((v, ok));
                }
                for e in payload {
                    values.push(self.expr(e)?);
                }
                let fields = b.layout.keys.iter().chain(&b.layout.payload);
                for ((v, ok), f) in values.into_iter().zip(fields) {
                    let at = (b.record + f.offset) as i32;
                    self.b.store(st, Val::NONE, 1, at, v, 0);
                    self.b.store(st, Val::NONE, 1, at + f.width as i32, ok, 0);
                }
                let record = self.offset(st, b.record);
                let table = self.handle(b.table);
                self.rt(proxy_id("jt_append"), &[table, record, hash]);
                Ok(())
            }
            _ => Err(Refusal::new("the sink", "its layout is of the other kind")),
        }
    }

    fn hash(&mut self, hash: Val, v: Val, logical: &LogicalType) -> Result<Val> {
        let ty = self.b.ty(v);
        Ok(match ty {
            Ty::Str16 => self.rt(proxy_id("str_hash"), &[v, hash]),
            Ty::I128 => {
                let lo = self.b.conv(Op::Trunc, v, Ty::I64);
                let k = self.b.int(Ty::I128, 64);
                let hi = self.b.bin(Op::Lshr, v, k);
                let hi = self.b.conv(Op::Trunc, hi, Ty::I64);
                let h = self.b.bin(Op::Crc32c, hash, lo);
                self.b.bin(Op::Crc32c, h, hi)
            }
            Ty::I64 => self.b.bin(Op::Crc32c, hash, v),
            Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 => {
                let op = if unsigned(logical) { Op::Zext } else { Op::Sext };
                let x = self.b.conv(op, v, Ty::I64);
                self.b.bin(Op::Crc32c, hash, x)
            }
            _ => return Err(Refusal::new(format!("grouping by {logical}"), "the key has no hash")),
        })
    }

    /// Runs `f` only when `c` holds, and carries on after it either way.
    fn when(&mut self, c: Val, f: impl FnOnce(&mut Gen<'_>) -> Result<()>) -> Result<()> {
        let (yes, after) = (self.b.block(&[]), self.b.block(&[]));
        self.b.brif(c, yes, &[], after, &[]);
        self.b.switch_to(yes);
        f(self)?;
        self.b.br(after, &[]);
        self.b.switch_to(after);
        Ok(())
    }

    fn update(&mut self, a: &Aggregate, acc: &Acc, row: Val, at: u32) -> Result<()> {
        let d = at as i32;
        // The filter clause, and then the argument's validity, decide whether the row counts.
        let mut take = self.truth();
        if let Some(f) = &a.filter {
            let (v, ok) = self.expr(f)?;
            let pass = self.b.bin(Op::And, v, ok);
            take = self.b.bin(Op::And, take, pass);
        }
        let (v, ok) = match a.args.first() {
            Some(x) => self.expr(x)?,
            None => (Val::NONE, self.truth()),
        };
        let take = self.b.bin(Op::And, take, ok);
        match acc.op {
            AccOp::CountStar | AccOp::Count => {
                let n = self.b.load(Ty::I64, row, Val::NONE, 1, d, 0);
                let add = self.b.conv(Op::Zext, take, Ty::I64);
                let n = self.b.bin(Op::Add, n, add);
                self.b.store(row, Val::NONE, 1, d, n, 0);
            }
            AccOp::SumInt | AccOp::AvgInt => {
                let x = self.widen(v, &acc.arg, Ty::I128);
                let zero = self.zero(Ty::I128);
                let x = self.b.select(take, x, zero);
                let total = self.b.load(Ty::I128, row, Val::NONE, 1, d, 0);
                let total = self.b.bin(Op::Add, total, x);
                self.b.store(row, Val::NONE, 1, d, total, 0);
                self.bump(acc.op == AccOp::AvgInt, take, row, d + 16);
            }
            AccOp::SumNarrow | AccOp::AvgNarrow => {
                let x = self.widen(v, &acc.arg, Ty::I64);
                let zero = self.zero(Ty::I64);
                let x = self.b.select(take, x, zero);
                let total = self.b.load(Ty::I64, row, Val::NONE, 1, d, 0);
                let total = self.b.bin(Op::Add, total, x);
                self.b.store(row, Val::NONE, 1, d, total, 0);
                self.bump(acc.op == AccOp::AvgNarrow, take, row, d + 8);
            }
            AccOp::SumFloat | AccOp::AvgFloat => {
                let x = if self.b.ty(v) == Ty::F32 { self.b.conv(Op::Fext, v, Ty::F64) } else { v };
                let total = self.b.load(Ty::F64, row, Val::NONE, 1, d, 0);
                let sum = self.b.bin(Op::Fadd, total, x);
                let total = self.b.select(take, sum, total);
                self.b.store(row, Val::NONE, 1, d, total, 0);
                self.bump(acc.op == AccOp::AvgFloat, take, row, d + 8);
            }
            AccOp::Min | AccOp::Max | AccOp::AnyValue => {
                let ty = self.b.ty(v);
                let w = ty.bytes() as i32;
                let seen = self.b.load(Ty::I1, row, Val::NONE, 1, d + w, 0);
                let first = self.b.un(Op::Not, seen);
                let better = match acc.op {
                    AccOp::AnyValue => self.b.bool(false),
                    op => {
                        let old = self.b.load(ty, row, Val::NONE, 1, d, 0);
                        let (x, y) = if op == AccOp::Min { (v, old) } else { (old, v) };
                        if ty == Ty::Str16 {
                            unreachable!("strings have their own accumulators");
                        } else if ty.is_float() {
                            self.b.bin(Op::FcmpLt, x, y)
                        } else if unsigned(&acc.arg) {
                            self.b.bin(Op::IcmpUlt, x, y)
                        } else {
                            self.b.bin(Op::IcmpSlt, x, y)
                        }
                    }
                };
                let replace = self.b.bin(Op::Or, first, better);
                let replace = self.b.bin(Op::And, take, replace);
                if ty == Ty::Str16 {
                    self.when(replace, |g| {
                        let none = g.handle(0);
                        let kept = g.rt(proxy_id("str_promote"), &[none, v]);
                        g.b.store(row, Val::NONE, 1, d, kept, 0);
                        let yes = g.truth();
                        g.b.store(row, Val::NONE, 1, d + w, yes, 0);
                        Ok(())
                    })?;
                } else {
                    let old = self.b.load(ty, row, Val::NONE, 1, d, 0);
                    let new = self.b.select(replace, v, old);
                    self.b.store(row, Val::NONE, 1, d, new, 0);
                    let seen = self.b.bin(Op::Or, seen, take);
                    self.b.store(row, Val::NONE, 1, d + w, seen, 0);
                }
            }
            AccOp::MinStr | AccOp::MaxStr => {
                let id =
                    proxy_id(if acc.op == AccOp::MinStr { "agg_min_str" } else { "agg_max_str" });
                self.when(take, |g| {
                    let at = g.offset(row, at);
                    g.b.rtcall(id, &[at, v]);
                    Ok(())
                })?;
            }
            AccOp::Distinct(h) => {
                let ty = self.b.ty(v);
                self.when(take, |g| {
                    let set = g.handle(h);
                    if ty == Ty::Str16 {
                        g.b.rtcall(proxy_id("agg_distinct"), &[set, row, v]);
                    } else {
                        let x = g.widen(v, &acc.arg, Ty::I128);
                        g.b.rtcall(proxy_id("agg_distinct_int"), &[set, row, x]);
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    /// Adds one to the count at `d` when `take`, or for a sum marks the total seen.
    fn bump(&mut self, count: bool, take: Val, row: Val, d: i32) {
        if count {
            let n = self.b.load(Ty::I64, row, Val::NONE, 1, d, 0);
            let add = self.b.conv(Op::Zext, take, Ty::I64);
            let n = self.b.bin(Op::Add, n, add);
            self.b.store(row, Val::NONE, 1, d, n, 0);
        } else {
            let seen = self.b.load(Ty::I1, row, Val::NONE, 1, d, 0);
            let seen = self.b.bin(Op::Or, seen, take);
            self.b.store(row, Val::NONE, 1, d, seen, 0);
        }
    }

    fn widen(&mut self, v: Val, logical: &LogicalType, to: Ty) -> Val {
        if self.b.ty(v) == to {
            return v;
        }
        let op = if unsigned(logical) { Op::Zext } else { Op::Sext };
        self.b.conv(op, v, to)
    }
}

fn is_empty_string(e: &Expr) -> bool {
    matches!(&e.kind, Kind::Constant(Value::Varchar(s)) if s.is_empty())
}

fn proxy_id(name: &str) -> u32 {
    proxy(name).unwrap_or_else(|| unreachable!("{name} is in the catalogue"))
}

/// The bytes an accumulator takes in a row.
fn acc_size(acc: &Acc) -> Result<u32> {
    let w = qir_type(&acc.arg)?.bytes();
    Ok(match acc.op {
        AccOp::CountStar | AccOp::Count => 8,
        AccOp::SumInt | AccOp::AvgInt => 24,
        AccOp::SumNarrow | AccOp::AvgNarrow | AccOp::SumFloat | AccOp::AvgFloat => 16,
        AccOp::Min | AccOp::Max | AccOp::AnyValue => (w + 1).next_multiple_of(8),
        AccOp::MinStr | AccOp::MaxStr => 24,
        AccOp::Distinct(_) => 0,
    })
}

mod vcall;

#[cfg(test)]
mod tests;
