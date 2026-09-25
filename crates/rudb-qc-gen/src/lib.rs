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
//! groups the address of the one group row, which the driver writes there before the first call. A
//! join build has the record buffer the body builds each row's record in before `jt_append`. After
//! the sink come three words per probe, which the driver fills from the built table at init: the
//! directory's address, the shift that takes a hash to its slot and the tag table's address.
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
use rudb_qc_ir::func::INV;
use rudb_qc_ir::status::NEED_MEMORY;
use rudb_qc_ir::{Block, Builder, ErrorKind, Field, Module, Op, Ty, Val, dce, verify};
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

fn unsigned(ty: &LogicalType) -> bool {
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
    let mut reads = Vec::new();
    // Only the source's columns are read from the morsel. The rest are what the probes bring.
    let mut note = |e: &Expr| {
        for c in e.columns() {
            if c < source.len() && !reads.contains(&c) {
                reads.push(c);
            }
        }
    };
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
    let mut g = Gen {
        b: Builder::new(&name, "generic", stage as u32),
        module,
        rt,
        cols: HashMap::new(),
        loaded: HashMap::new(),
        row: Val::NONE,
        ptrs: Vec::new(),
        columns: p.columns(),
        next: 0,
        joins: joins.to_vec(),
        tables: Vec::new(),
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

    let state = g.next.next_multiple_of(8);
    let mut func = g.b.finish();
    dce(&mut func);
    module.funcs.push(func);
    Ok(Body { func: name, reads, state, sink: out, probes })
}

struct Gen<'a> {
    b: Builder,
    module: &'a mut Module,
    rt: &'a mut Rt,
    /// Source column to its values address, validity address and type.
    cols: HashMap<usize, (Val, Val, Ty)>,
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
                let table = self.rt.add_join(JoinTable::new(layout.clone()));
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
                let table = self.rt.add_table(GroupTable::new(layout));
                let (row, state) = if groups.is_empty() {
                    self.field(SINK, 8, "row");
                    (Some(SINK), SINK + 8)
                } else {
                    let size = size.next_multiple_of(8);
                    self.field(SINK, size, "key");
                    (None, SINK + size)
                };
                let grouping = Grouping { table, keys, acc_offset, accs, row };
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
                AccOp::Distinct(self.rt.add_distinct())
            }
            ("count", false) => AccOp::Count,
            ("sum", _) if argty.is_int() && qir_type(&a.ty)? == Ty::I128 && !unsigned(&arg) => {
                AccOp::SumInt
            }
            ("sum", _) if argty.is_float() && qir_type(&a.ty)? == Ty::F64 => AccOp::SumFloat,
            ("avg", _)
                if argty.is_int()
                    && !unsigned(&arg)
                    && !matches!(arg, LogicalType::Decimal { .. })
                    && qir_type(&a.ty)? == Ty::F64 =>
            {
                AccOp::AvgInt
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

    /// Reads every source column `e` uses that is not read yet, in the current block.
    fn load_columns(&mut self, e: &Expr) {
        for c in e.columns() {
            if self.loaded.contains_key(&c) {
                continue;
            }
            let (values, valid, ty) = self.cols[&c];
            let v = self.b.load(ty, values, self.row, ty.bytes(), 0, 0);
            let ok = self.b.load_bit(valid, self.row);
            let zero = self.zero(ty);
            let v = self.b.select(ok, v, zero);
            self.loaded.insert(c, (v, ok));
        }
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
            return Err(refuse());
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

    fn compare(&mut self, op: CompareOp, left: &Expr, right: &Expr) -> Result<Pair> {
        let ty = qir_type(&left.ty)?;
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
                let h = self.rt.add_like(p, name.contains('*'));
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
        let mut call = vcall::Call::new(name, args, &e.ty, &self.columns);
        let id = self.rt.add_kernel(Box::new(move |n, buffers| call.run(n, buffers)));
        if self.module.kernel(name) != id {
            return Err(Refusal::new(
                format!("the kernel for {name}"),
                "the runtime and the module number their kernels differently",
            ));
        }
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
            self.sink(sink, out, skip)?;
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

    fn sink(&mut self, sink: &Sink, out: &Out, skip: Block) -> Result<()> {
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
                        let mut hash = self.b.int(Ty::I64, 0);
                        for (e, (k, _)) in groups.iter().zip(&g.keys) {
                            let (v, ok) = self.expr(e)?;
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
                        self.rt(proxy_id("ht_insert"), &[table, key, hash])
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
        AccOp::SumFloat | AccOp::AvgFloat => 16,
        AccOp::Min | AccOp::Max | AccOp::AnyValue => (w + 1).next_multiple_of(8),
        AccOp::MinStr | AccOp::MaxStr => 24,
        AccOp::Distinct(_) => 0,
    })
}

mod vcall;

#[cfg(test)]
mod tests;
