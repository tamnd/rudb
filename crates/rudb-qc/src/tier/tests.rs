//! `clif` against `interp`, one opcode at a time and then over control flow and memory: every
//! opcode on every type the verifier lets it take, on edge values and random bits, has to give the
//! same status and the same bytes on both tiers. This is what makes the second tier's workarounds
//! for what Cranelift lacks safe to rely on, and it runs in a second, which the query level
//! differential does not.

use rudb_common::Cancel;
use rudb_qc_ir::{Form, Module, Op, Ty, parse, verify};
use rudb_qc_rt::Rt;

use super::{Tier, Tiers};

const INTS: [Ty; 6] = [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128];
const ALL: [Ty; 10] =
    [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128, Ty::Ptr, Ty::F32, Ty::F64, Ty::Str16];

/// A small deterministic generator, so a failure names inputs that come back on the next run.
struct Bits(u64);

impl Bits {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn wide(&mut self) -> u128 {
        u128::from(self.next()) | (u128::from(self.next()) << 64)
    }
}

/// The values worth trying for a type: the edges, where the workarounds live, and random bits.
fn values(ty: Ty, bits: &mut Bits) -> Vec<u128> {
    let mask = ty.mask();
    let mut out: Vec<u128> = match ty {
        Ty::F64 => [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            1.5,
            2.5,
            -2.5,
            3.5,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            127.5,
            128.0,
            -128.5,
            32767.5,
            2147483647.4,
            2147483647.5,
            -2147483648.5,
            -2147483648.4,
            9.223372036854775e18,
            9.223372036854776e18,
            -9.223372036854776e18,
            1e19,
            1e300,
        ]
        .iter()
        .map(|x: &f64| u128::from(x.to_bits()))
        .chain([0x7ff8_0000_0000_0000, 0x7ff0_0000_0000_0001, 0xfff8_0000_0000_0000])
        .collect(),
        Ty::F32 => [
            0.0f32,
            -0.0,
            1.0,
            -1.0,
            0.5,
            2.5,
            -2.5,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MAX,
            1e-45,
            127.5,
            2147483600.0,
            2.1474836e9,
            -2.1474836e9,
            9.223372e18,
            -9.223372e18,
        ]
        .iter()
        .map(|x: &f32| u128::from(x.to_bits()))
        .chain([0x7fc0_0000, 0x7f80_0001, 0xffc0_0000])
        .collect(),
        _ => {
            let top = if ty.bits() == 0 { 0 } else { 1u128 << (ty.bits() - 1) };
            vec![
                0,
                1,
                2,
                3,
                7,
                10,
                12,
                13,
                100,
                mask,
                mask - 1,
                top,
                top - 1,
                top + 1,
                1 << 7,
                255,
                256,
            ]
        }
    };
    for _ in 0..48 {
        let r = bits.wide();
        // Small numbers find the ordinary paths, and the float types need their own randoms.
        out.push(match ty {
            Ty::F64 => u128::from((f64::from(r as i32) / 7.0).to_bits()),
            Ty::F32 => u128::from((f32::from(r as i16) / 3.0).to_bits()),
            _ if r & 1 == 0 => r >> (r as u32 % 128),
            _ => (r as i8 as i128) as u128,
        });
    }
    out.into_iter().map(|v| if ty.is_float() { v } else { v & mask }).collect()
}

fn load(ty: Ty) -> String {
    if ty == Ty::Str16 { "load.str".to_string() } else { format!("load.{}", ty.name()) }
}

fn store(ty: Ty) -> String {
    if ty == Ty::Str16 { "store.str".to_string() } else { format!("store.{}", ty.name()) }
}

/// A function that loads its operands from the state, runs one instruction and stores the
/// result, or `None` when the verifier refuses the instruction on these types.
fn one(op: Op, ty: Ty, to: Ty, k: u32) -> Option<Module> {
    let t = ty.name();
    let (body, result) = match op.form() {
        Form::Un => (format!("%r = {} {t} %a", op.name()), op.result(ty)),
        Form::Bin | Form::Cmp | Form::Wide => {
            (format!("%r = {} {t} %a, %b", op.name()), op.result(ty))
        }
        Form::TrapBin => (format!("%r = {} {t} %a, %b, !E0", op.name()), ty),
        Form::TrapUn => (format!("%r = {} {t} %a, !E0", op.name()), ty),
        Form::Conv => (format!("%r = {} {t} %a -> {}", op.name(), to.name()), to),
        Form::TrapConv => (format!("%r = {} {t} %a -> {}, !E0", op.name(), to.name()), to),
        Form::Scale => (format!("%r = {} {t} %a, {k}", op.name()), ty),
        Form::TrapScale => (format!("%r = {} {t} %a, {k}, !E0", op.name()), ty),
        Form::EdgeBin => {
            let text = format!(
                "module one\n  error !E0 overflow \"x\"\n\nfunc @one version=generic plan=#1\n\
                 block b0(ptr %st, ptr %m):\n  %a = {} [%st + 64]\n  %b = {} [%st + 80]\n  \
                 {} {t} %a, %b, b1, b2\nblock b1({t} %r):\n  {} [%st + 96], %r\n  ret 0\n\
                 cold block b2:\n  ret 9\n",
                load(ty),
                load(ty),
                op.name(),
                store(ty),
            );
            return parse(&text).ok().filter(|m| verify(m).is_ok());
        }
        _ => return None,
    };
    if result == Ty::Void {
        return None;
    }
    // The verifier refuses a value nothing uses, so `%b` is only loaded for the forms with two.
    let b = if body.contains("%b") {
        format!("\n  %b = {} [%st + 80]", load(ty))
    } else {
        String::new()
    };
    let text = format!(
        "module one\n  error !E0 overflow \"x\"\n\nfunc @one version=generic plan=#1\n\
         block b0(ptr %st, ptr %m):\n  %a = {} [%st + 64]{b}\n  {body}\n  \
         {} [%st + 96], %r\n  ret 0\n",
        load(ty),
        store(result),
    );
    parse(&text).ok().filter(|m| verify(m).is_ok())
}

/// Runs function 0 of `tiers` on a state holding `a` and `b`, and returns the status and the
/// result slot.
fn run(tiers: &Tiers, a: u128, b: u128) -> (u64, u128) {
    let mut state = [0u128; 16];
    state[4] = a;
    state[5] = b;
    let mut rt = Rt::new(Cancel::new());
    let status = tiers.call(0, state.as_mut_ptr().cast(), std::ptr::null(), &mut rt);
    (status, state[6])
}

/// Both tiers of one module, with the check that `clif` compiled it.
fn both(m: &Module) -> (Tiers, Tiers) {
    let clif = Tiers::new(m, Tier::Clif);
    assert_eq!(clif.report().native, m.funcs.len(), "{}", clif.report());
    (Tiers::new(m, Tier::Interp), clif)
}

#[test]
fn every_opcode_on_every_type_is_the_interpreters() {
    let mut bits = Bits(0x9e37_79b9_7f4a_7c15);
    let mut cases = 0usize;
    let mut modules = 0usize;
    let mut wrong = Vec::new();
    for &op in Op::all() {
        let ks: &[u32] = if matches!(op.form(), Form::Scale | Form::TrapScale) {
            &[0, 1, 2, 3, 5, 9, 18, 19, 20, 38]
        } else {
            &[0]
        };
        let tos: &[Ty] =
            if matches!(op.form(), Form::Conv | Form::TrapConv) { &ALL } else { &[Ty::Void] };
        for &ty in &ALL {
            for &to in tos {
                for &k in ks {
                    let Some(m) = one(op, ty, to, k) else { continue };
                    modules += 1;
                    let (interp, clif) = both(&m);
                    let xs = values(ty, &mut bits);
                    let ys = values(ty, &mut bits);
                    for (i, &a) in xs.iter().enumerate() {
                        // Every first operand against a spread of second operands, which keeps the
                        // count linear and still meets every edge on both sides.
                        for &b in ys.iter().skip(i % 5).step_by(5).chain(xs.get(i)) {
                            cases += 1;
                            let want = run(&interp, a, b);
                            let got = run(&clif, a, b);
                            if want != got && wrong.len() < 40 {
                                wrong.push(format!(
                                    "{} {ty} -> {to} k={k} on {a:#x}, {b:#x}: interp {want:x?}, \
                                     clif {got:x?}",
                                    op.name(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{} of {cases} differ:\n{}", wrong.len(), wrong.join("\n"));
    // The count is a floor so that a parser or verifier change that quietly rejects most of the
    // generated functions fails here instead of passing on nothing.
    assert!(modules > 400, "only {modules} functions were generated");
}

#[test]
fn integer_opcodes_cover_every_width() {
    // The narrow widths are where Cranelift and the interpreter's `u128` registers part ways, so
    // they are checked to be among the ones generated above.
    for ty in INTS {
        assert!(one(Op::Add, ty, Ty::Void, 0).is_some(), "add {ty}");
        assert!(one(Op::SaddT, ty, Ty::Void, 0).is_some() || ty == Ty::I1, "sadd.t {ty}");
    }
}

/// Memory, control flow and the cold exits, over a loop that polls.
const FLOW: &str = "module flow
  error !E0 overflow \"x\"
  guard !G0 \"never\" fallback=flow.generic

func @flow version=generic plan=#1
block b0(ptr %st, ptr %m):
  %n = load.i64 [%st + 64]
  %sel = load.i32 [%st + 80]
  br b1(0, 0)
loop(1) b1(i64 %i, i64 %acc):
  poll 3
  %done = icmp.uge i64 %i, %n
  brif %done, b5, b2
block b2:
  %bi = add i64 %i, 576
  %bit = load.bit [%st + %bi]
  %w = load.i64 [%st + %i*8 + 128]
  %x = select i64 %bit, %w, %i
  %sum = sadd.t i64 %acc, %x, !E0
  %next = add i64 %i, 1
  switch i32 %sel, b3(%next, %sum), [0: b4, 3: b4, 3: b6]
block b3(i64 %j, i64 %s):
  br b1(%j, %s)
block b4:
  %bad = icmp.eq i64 %acc, 77
  %ok = xor i1 %bad, true
  guard %ok, !G0
  br b1(%next, %sum)
block b5:
  %s0 = bitcast ptr %st -> i64
  %q0 = add i64 %s0, 128
  %q1 = add i64 %s0, 160
  %p0 = bitcast i64 %q0 -> ptr
  %p1 = bitcast i64 %q1 -> ptr
  memcpy %p1, %p0, 23
  %eq = memeq %p0, %p1, 23
  %old = atomic.add i64 %p1, %acc
  %won = cas i64 %p0, %old, 5
  %e = zext i1 %eq -> i64
  %wz = zext i1 %won -> i64
  %r0 = add i64 %e, %wz
  %r = add i64 %r0, %acc
  store.i64 [%st + 96], %r
  ret 0
cold block b6:
  ret 11
";

#[test]
fn memory_and_control_flow_are_the_interpreters() {
    let m = parse(FLOW).expect("the flow module parses");
    verify(&m).expect("the flow module verifies");
    let (interp, clif) = both(&m);
    let mut bits = Bits(7);
    for round in 0..400u64 {
        let mut state = [[0u128; 24]; 2];
        let n = u128::from(round % 9);
        let sel = u128::from([0u64, 1, 3, 9][(round % 4) as usize]);
        let words: Vec<u128> = (0..8).map(|_| bits.wide()).collect();
        let bitmap = u128::from(bits.next());
        for s in &mut state {
            s[4] = n | (bitmap << 64);
            s[5] = sel;
            s[8..16].copy_from_slice(&words);
            // Small words sometimes, so that the checked add does not always overflow.
            if round % 3 == 0 {
                for w in &mut s[8..16] {
                    *w &= 0xff;
                }
            }
        }
        let mut rt = Rt::new(Cancel::new());
        let [a, b] = &mut state;
        let want = interp.call(0, a.as_mut_ptr().cast(), std::ptr::null(), &mut rt);
        let got = clif.call(0, b.as_mut_ptr().cast(), std::ptr::null(), &mut rt);
        assert_eq!(want, got, "round {round}");
        // The header's `rt` word is the one thing only native code writes, and it clears it.
        assert_eq!(a[..], b[..], "round {round}");
    }
}

#[test]
fn a_cancelled_query_stops_at_a_poll_on_both_tiers() {
    let m = parse(FLOW).expect("the flow module parses");
    let (interp, clif) = both(&m);
    let cancel = Cancel::new();
    cancel.cancel();
    for tiers in [&interp, &clif] {
        let mut state = [0u128; 24];
        state[4] = 100;
        state[5] = 1;
        let mut rt = Rt::new(cancel.clone());
        let status = tiers.call(0, state.as_mut_ptr().cast(), std::ptr::null(), &mut rt);
        assert_eq!(
            rudb_qc_ir::status::kind(status),
            rudb_qc_ir::status::CANCELLED,
            "{}",
            tiers.report()
        );
    }
}
