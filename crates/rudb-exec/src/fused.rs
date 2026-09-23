//! Decimal arithmetic run as one loop, which is tier one fusion from
//! `spec/engine/04-expressions.md` for the case TPC-H is made of.
//!
//! `sum(l_extendedprice * (1 - l_discount) * (1 + l_tax))` is prepared as seven steps: three casts
//! that widen a packed `DECIMAL(15, 2)` into a flat one, two additions and two products. Every one
//! of them writes a vector, and every product and sum checks each row against the width of its
//! answer, because a `DECIMAL(18, 4)` that went past eighteen digits has to raise. On q01 those
//! passes were most of what was left once the sums themselves had been made cheap.
//!
//! The check does not have to be per row. A packed column says what its smallest and largest value
//! can be before a code is read, a flat one says it after one pass, and a literal is one number. So
//! the range of every node of the tree over a chunk can be worked out from the ranges of its leaves
//! by interval arithmetic, and when every range fits the width its node is declared at, no row of
//! the chunk can overflow and none has to be asked. Then the tree is a handful of plain integer
//! operations a row, run a block of rows at a time so the intermediates stay in registers and L1,
//! and one vector comes out.
//!
//! When the ranges do not prove it, or a leaf has a null or a form this does not read, the chunk
//! goes through the same steps it always did, prepared on the side for exactly that. So every error
//! and every null is the one the unfused path gives, because it is the unfused path that gives it.

use rudb_common::{LogicalType, Value};
use rudb_plan::{Expr, ExprRef, Plan};
use rudb_vector::{Chunk, Data, Form, Packed, Vector};
use std::borrow::Cow;

use crate::schema::Schema;

/// Rows run through the program at a time, so the buffers of a block stay in L1.
const BLOCK: usize = 256;

/// The most packed rows a block of a dictionary lane unpacks at once. Four to a row the block reads,
/// which is the rule `Packed::codes_at` has for when unpacking a span beats reading codes one at a
/// time.
const SPAN: usize = 4 * BLOCK;

/// A tree of decimal additions, subtractions, products and widening casts over columns and
/// literals, every node of which fits a 64 bit integer.
#[derive(Debug)]
pub(crate) struct Fused {
    /// The chunk positions the leaves read, one per distinct column.
    columns: Vec<usize>,
    /// The nodes in post order. The last is the root.
    nodes: Vec<Node>,
    /// What each node is read from while the program runs.
    places: Vec<Place>,
    /// The operations, in the order they run, each writing one buffer.
    program: Vec<Instruction>,
    /// How many buffers a block wants: one per column and one per operation.
    buffers: usize,
    /// The type of the answer.
    output: LogicalType,
}

/// One node of the tree, with the bound its answer has to stay inside.
#[derive(Debug, Clone, Copy)]
struct Node {
    kind: Kind,
    /// Ten to the width of the node's type. An answer as large as this, either way, does not fit.
    cap: i128,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// The column at this index of [`Fused::columns`].
    Leaf(usize),
    /// A literal, unscaled.
    Fixed(i64),
    /// A cast that changes nothing but the width, so only the bound.
    Same(usize),
    /// A cast that moves the scale up, which is a product by this power of ten.
    Scale(usize, i64),
    Add(usize, usize),
    Subtract(usize, usize),
    Multiply(usize, usize),
}

/// Where a node's values are while a block runs.
#[derive(Debug, Clone, Copy)]
enum Place {
    Buffer(usize),
    Fixed(i64),
}

#[derive(Debug, Clone, Copy)]
struct Instruction {
    op: Op,
    left: Place,
    right: Place,
    into: usize,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Add,
    Subtract,
    Multiply,
}

/// A decimal type an `i64` holds, as its cap and scale.
fn narrow(ty: &LogicalType) -> Option<(i128, u8)> {
    match *ty {
        LogicalType::Decimal { width, scale } if width <= 18 => {
            Some((10_i128.pow(u32::from(width)), scale))
        }
        _ => None,
    }
}

impl Fused {
    /// The tree rooted at `expr` as one program, or `None` when it is not all decimal arithmetic
    /// this covers, when it reads no column, or when there is no arithmetic in it to save.
    pub(crate) fn compile(plan: &Plan, expr: ExprRef, schema: &Schema) -> Option<Self> {
        let mut built = Builder::default();
        built.node(plan, expr, schema)?;
        let arithmetic = built
            .nodes
            .iter()
            .filter(|node| !matches!(node.kind, Kind::Leaf(_) | Kind::Fixed(_) | Kind::Same(_)))
            .count();
        if arithmetic == 0 || built.columns.is_empty() {
            return None;
        }
        Some(built.finish(plan.expr_type(expr).clone()))
    }

    /// How many operations a row costs, for the ordering's weights.
    pub(crate) fn len(&self) -> usize {
        self.program.len()
    }

    /// The answer over `chunk`, or `None` when the leaves' ranges do not prove every node fits, when
    /// a leaf has a null, or when a leaf is in a form this does not read. `None` is not a failure:
    /// the caller runs the steps this replaced, which raise what there is to raise.
    pub(crate) fn run(&self, chunk: &Chunk) -> Option<Vector> {
        let rows = chunk.len();
        let mut lanes = Vec::with_capacity(self.columns.len());
        for &position in &self.columns {
            let vector = chunk.column(position).ok()?;
            if vector.len() != rows || !vector.never_null() {
                return None;
            }
            lanes.push(Lane::of(vector, rows)?);
        }
        if !self.proven(&lanes) {
            return None;
        }
        let mut out = Vec::with_capacity(rows);
        let mut buffers = vec![0_i64; self.buffers * BLOCK];
        // The span a block reads, and the two partial blocks of sixty four either side of it.
        let mut codes = vec![0_u64; SPAN + 128];
        let mut from = 0;
        while from < rows {
            let count = BLOCK.min(rows - from);
            for (index, lane) in lanes.iter().enumerate() {
                let buffer = &mut buffers[index * BLOCK..index * BLOCK + count];
                lane.fill(from, buffer, &mut codes);
            }
            for instruction in &self.program {
                instruction.run(&mut buffers, count);
            }
            match self.places[self.nodes.len() - 1] {
                Place::Buffer(buffer) => {
                    out.extend_from_slice(&buffers[buffer * BLOCK..buffer * BLOCK + count]);
                }
                Place::Fixed(value) => out.resize(from + count, value),
            }
            from += count;
        }
        let data = match self.output.physical() {
            rudb_common::PhysicalType::Int16 => {
                Data::Int16(out.iter().map(|&value| value as i16).collect::<Vec<_>>().into())
            }
            rudb_common::PhysicalType::Int32 => {
                Data::Int32(out.iter().map(|&value| value as i32).collect::<Vec<_>>().into())
            }
            _ => Data::Int64(out.into()),
        };
        Vector::flat(self.output.clone(), data).ok()
    }

    /// Whether every node's range over this chunk is inside its cap.
    ///
    /// The ranges are in `i128`. A node is only reached once both of its operands were found inside
    /// eighteen digits, so a product of two of them is inside thirty six and the arithmetic here
    /// cannot overflow either.
    fn proven(&self, lanes: &[Lane<'_>]) -> bool {
        let mut ranges: Vec<(i128, i128)> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let (low, high) = match node.kind {
                Kind::Leaf(index) => lanes[index].range(),
                Kind::Fixed(value) => (i128::from(value), i128::from(value)),
                Kind::Same(input) => ranges[input],
                Kind::Scale(input, factor) => {
                    let (low, high) = ranges[input];
                    (low * i128::from(factor), high * i128::from(factor))
                }
                Kind::Add(left, right) => {
                    (ranges[left].0 + ranges[right].0, ranges[left].1 + ranges[right].1)
                }
                Kind::Subtract(left, right) => {
                    (ranges[left].0 - ranges[right].1, ranges[left].1 - ranges[right].0)
                }
                Kind::Multiply(left, right) => {
                    let (a, b) = ranges[left];
                    let (c, d) = ranges[right];
                    let corners = [a * c, a * d, b * c, b * d];
                    (corners.into_iter().min().unwrap_or(0), corners.into_iter().max().unwrap_or(0))
                }
            };
            if low <= -node.cap || high >= node.cap {
                return false;
            }
            ranges.push((low, high));
        }
        true
    }
}

impl Instruction {
    fn run(&self, buffers: &mut [i64], count: usize) {
        let (head, tail) = buffers.split_at_mut(self.into * BLOCK);
        let out = &mut tail[..count];
        // An operand buffer is always an earlier one than the buffer written, because buffers are
        // handed out in post order, so it is in `head`.
        let read = |place: Place| match place {
            Place::Buffer(buffer) => Ok(&head[buffer * BLOCK..buffer * BLOCK + count]),
            Place::Fixed(value) => Err(value),
        };
        // Every value was proven inside eighteen digits, so none of these wraps. Wrapping rather than
        // checked keeps the loops free of branches and lets them vectorize.
        macro_rules! apply {
            ($f:expr) => {
                match (read(self.left), read(self.right)) {
                    (Ok(a), Ok(b)) => {
                        for ((out, &x), &y) in out.iter_mut().zip(a).zip(b) {
                            *out = $f(x, y);
                        }
                    }
                    (Ok(a), Err(y)) => {
                        for (out, &x) in out.iter_mut().zip(a) {
                            *out = $f(x, y);
                        }
                    }
                    (Err(x), Ok(b)) => {
                        for (out, &y) in out.iter_mut().zip(b) {
                            *out = $f(x, y);
                        }
                    }
                    (Err(x), Err(y)) => out.fill($f(x, y)),
                }
            };
        }
        match self.op {
            Op::Add => apply!(i64::wrapping_add),
            Op::Subtract => apply!(i64::wrapping_sub),
            Op::Multiply => apply!(i64::wrapping_mul),
        }
    }
}

/// How one leaf column is read.
enum Lane<'v> {
    /// Bit packed, read in row order.
    Packed { packed: Packed<'v>, base: i64, high: i64 },
    /// A dictionary over bit packed values, which is what a filter leaves of a packed column.
    Coded { at: Cow<'v, [u32]>, packed: Packed<'v>, base: i64, high: i64 },
    /// Flat values, with their smallest and largest.
    Plain { values: Cow<'v, [i64]>, low: i64, high: i64 },
    /// A dictionary over flat values, with the smallest and largest in the dictionary.
    Picked { at: Cow<'v, [u32]>, values: Cow<'v, [i64]>, low: i64, high: i64 },
    /// One value for every row.
    Fixed(i64),
}

impl<'v> Lane<'v> {
    fn of(vector: &'v Vector, rows: usize) -> Option<Self> {
        match vector.form() {
            Form::Flat => {
                let values = widened(vector.data()?, rows)?;
                let (low, high) = extent(&values)?;
                Some(Self::Plain { values, low, high })
            }
            Form::BitPacked => {
                let packed = vector.packed_parts()?;
                let (base, high) = bounds(&packed)?;
                Some(Self::Packed { packed, base, high })
            }
            Form::Dictionary | Form::Rle => {
                let (at, values) = vector.positions()?;
                if at.len() < rows {
                    return None;
                }
                if let Some(data) = values.data() {
                    let values = widened(data, values.len())?;
                    let (low, high) = extent(&values)?;
                    return Some(Self::Picked { at, values, low, high });
                }
                let packed = values.packed_parts()?;
                let (base, high) = bounds(&packed)?;
                Some(Self::Coded { at, packed, base, high })
            }
            Form::Constant => match vector.constant_value()? {
                Value::Decimal { unscaled, .. } => {
                    Some(Self::Fixed(i64::try_from(*unscaled).ok()?))
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn range(&self) -> (i128, i128) {
        let (low, high) = match *self {
            Self::Packed { base, high, .. } | Self::Coded { base, high, .. } => (base, high),
            Self::Plain { low, high, .. } | Self::Picked { low, high, .. } => (low, high),
            Self::Fixed(value) => (value, value),
        };
        (i128::from(low), i128::from(high))
    }

    /// The values of rows `from` to `from + out.len()`.
    ///
    /// A code is at most the distance from the base to the largest value, which fits an `i64`, so
    /// the add lands on the value without wrapping even though it is written as a wrapping one.
    #[expect(clippy::cast_possible_wrap, reason = "a code is below the span, which fits")]
    fn fill(&self, from: usize, out: &mut [i64], codes: &mut [u64]) {
        let rows = from..from + out.len();
        match self {
            Self::Packed { packed, base, .. } => {
                let codes = &mut codes[..out.len()];
                packed.unpack(from, codes);
                for (out, &code) in out.iter_mut().zip(codes.iter()) {
                    *out = base.wrapping_add(code as i64);
                }
            }
            Self::Coded { at, packed, base, .. } => {
                let at = &at[rows];
                let (low, top) = at
                    .iter()
                    .fold((u32::MAX, 0), |(low, top), &code| (low.min(code), top.max(code)));
                let (low, top) = (low as usize, top as usize);
                // A filter's survivors are in order and close together, so the rows a block reads
                // are a short run of the packed values, unpacked here into the stack rather than
                // a code at a time. Unpacking the whole chunk's span up front was a vector the size
                // of the chunk per column, out of L1 by the time the gather read it.
                if !at.is_empty() && top - low < SPAN {
                    // Widened out to whole blocks of sixty four at both ends, which is the unit
                    // `Packed::unpack` reads without asking about each code. Left as it was, a block
                    // of 256 rows read about a hundred of its codes one at a time. A row past the
                    // end of the column reads as zero there, and nothing here reads it.
                    let low = low - ((packed.offset() + low) % 64).min(low);
                    let end = (packed.offset() + top + 1).next_multiple_of(64) - packed.offset();
                    let span = &mut codes[..end - low];
                    packed.unpack(low, span);
                    for (out, &code) in out.iter_mut().zip(at) {
                        *out = base.wrapping_add(span[code as usize - low] as i64);
                    }
                } else {
                    for (out, &code) in out.iter_mut().zip(at) {
                        *out = base.wrapping_add(packed.code(code as usize) as i64);
                    }
                }
            }
            Self::Plain { values, .. } => out.copy_from_slice(&values[rows]),
            Self::Picked { at, values, .. } => {
                for (out, &code) in out.iter_mut().zip(&at[rows]) {
                    *out = values[code as usize];
                }
            }
            Self::Fixed(value) => out.fill(*value),
        }
    }
}

/// The smallest and largest value a packed run can hold, when both fit an `i64`.
fn bounds(packed: &Packed<'_>) -> Option<(i64, i64)> {
    Some((i64::try_from(packed.base()).ok()?, i64::try_from(packed.ceiling()).ok()?))
}

/// The first `rows` values of a flat decimal as `i64`, borrowed when they already are.
fn widened(data: &Data, rows: usize) -> Option<Cow<'_, [i64]>> {
    Some(match data {
        Data::Int64(values) => Cow::Borrowed(values.as_slice().get(..rows)?),
        Data::Int32(values) => {
            Cow::Owned(values.as_slice().get(..rows)?.iter().map(|&v| i64::from(v)).collect())
        }
        Data::Int16(values) => {
            Cow::Owned(values.as_slice().get(..rows)?.iter().map(|&v| i64::from(v)).collect())
        }
        _ => return None,
    })
}

/// The smallest and largest of some values, and `None` for none.
fn extent(values: &[i64]) -> Option<(i64, i64)> {
    if values.is_empty() {
        return Some((0, 0));
    }
    Some(values.iter().fold((i64::MAX, i64::MIN), |(low, high), &v| (low.min(v), high.max(v))))
}

#[derive(Default)]
struct Builder {
    columns: Vec<usize>,
    nodes: Vec<Node>,
    scales: Vec<u8>,
}

impl Builder {
    /// Adds the tree at `expr` and answers the index of its root, or `None` for anything this does
    /// not cover.
    fn node(&mut self, plan: &Plan, expr: ExprRef, schema: &Schema) -> Option<usize> {
        let (cap, scale) = narrow(plan.expr_type(expr))?;
        let kind = match *plan.expr(expr) {
            Expr::Column(binding) => {
                let position = schema.position_of(binding)?;
                let index = match self.columns.iter().position(|&at| at == position) {
                    Some(index) => index,
                    None => {
                        self.columns.push(position);
                        self.columns.len() - 1
                    }
                };
                Kind::Leaf(index)
            }
            Expr::Constant(reference) => match *plan.value(reference) {
                Value::Decimal { unscaled, scale: written, .. } if written == scale => {
                    let value = i64::try_from(unscaled).ok()?;
                    if i128::from(value).abs() >= cap {
                        return None;
                    }
                    Kind::Fixed(value)
                }
                _ => return None,
            },
            Expr::Cast { input, try_cast: false } => {
                let input = self.node(plan, input, schema)?;
                let was = self.scales[input];
                if was == scale {
                    Kind::Same(input)
                } else if was < scale {
                    Kind::Scale(input, 10_i64.checked_pow(u32::from(scale - was))?)
                } else {
                    return None;
                }
            }
            Expr::Function { name, args } => {
                let op = plan.string(name);
                let &[left, right] = plan.expr_list(args) else {
                    return None;
                };
                if !matches!(op, "+" | "-" | "*") {
                    return None;
                }
                let left = self.node(plan, left, schema)?;
                let right = self.node(plan, right, schema)?;
                let (a, b) = (self.scales[left], self.scales[right]);
                match op {
                    "+" if a == scale && b == scale => Kind::Add(left, right),
                    "-" if a == scale && b == scale => Kind::Subtract(left, right),
                    "*" if u16::from(a) + u16::from(b) == u16::from(scale) => {
                        Kind::Multiply(left, right)
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        self.nodes.push(Node { kind, cap });
        self.scales.push(scale);
        Some(self.nodes.len() - 1)
    }

    /// Lays the nodes out as buffers and a program: one buffer per column, then one per operation
    /// in post order, with a cast that only checks reading its input's place.
    fn finish(self, output: LogicalType) -> Fused {
        let mut buffers = self.columns.len();
        let mut places = Vec::with_capacity(self.nodes.len());
        let mut program = Vec::new();
        for node in &self.nodes {
            let (op, left, right) = match node.kind {
                Kind::Leaf(index) => {
                    places.push(Place::Buffer(index));
                    continue;
                }
                Kind::Fixed(value) => {
                    places.push(Place::Fixed(value));
                    continue;
                }
                Kind::Same(input) => {
                    places.push(places[input]);
                    continue;
                }
                Kind::Scale(input, factor) => (Op::Multiply, places[input], Place::Fixed(factor)),
                Kind::Add(left, right) => (Op::Add, places[left], places[right]),
                Kind::Subtract(left, right) => (Op::Subtract, places[left], places[right]),
                Kind::Multiply(left, right) => (Op::Multiply, places[left], places[right]),
            };
            program.push(Instruction { op, left, right, into: buffers });
            places.push(Place::Buffer(buffers));
            buffers += 1;
        }
        Fused { columns: self.columns, nodes: self.nodes, places, program, buffers, output }
    }
}
