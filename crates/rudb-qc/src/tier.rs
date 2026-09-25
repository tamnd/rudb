//! The tiers a pipeline function runs on, per section 8.1 of `spec/compiler/08-backends.md`:
//! `interp`, which always exists, and `clif`, the Cranelift backend, when the build has the
//! `qc-clif` feature and the platform has a code arena.
//!
//! Every function is lowered for the interpreter, whatever the tier. That is the fallback for a
//! function the second tier does not lower, and it is what lets a query move between the tiers at
//! a morsel boundary: the two agree on the state and the morsel to the byte, so either one can
//! pick up where the other stopped.
//!
//! [`Switch`] moves a query between the tiers at morsel boundaries, which is the tier differential
//! of section 15 of the spec: the same query with and without the moves has to give the same bits.
//!
//! A function `clif` refuses, or panics on, runs on `interp` and the refusal is kept in the
//! [`Report`]. A query never fails because the second tier could not compile it.

use std::fmt;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use rudb_qc_interp::Program;
use rudb_qc_ir::Module;
use rudb_qc_rt::Rt;

/// Which tier runs a query's pipelines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tier {
    /// `clif` when the build has it, `interp` otherwise.
    #[default]
    Auto,
    /// The interpreter only.
    Interp,
    /// Machine code through Cranelift, with `interp` for what it does not lower.
    Clif,
}

impl Tier {
    /// Every tier, in the order `SET qc_tier` lists them.
    pub const ALL: [Tier; 3] = [Tier::Auto, Tier::Interp, Tier::Clif];

    /// The name `SET qc_tier` takes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tier::Auto => "auto",
            Tier::Interp => "interp",
            Tier::Clif => "clif",
        }
    }

    /// The tier with this name, in any case.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.name().eq_ignore_ascii_case(name.trim()))
    }

    /// Whether this build can run the tier. `clif` needs the `qc-clif` feature.
    #[must_use]
    pub fn built(self) -> bool {
        self != Tier::Clif || cfg!(feature = "qc-clif")
    }

    /// Whether this compiles to machine code, once `auto` is decided.
    fn native(self) -> bool {
        match self {
            Tier::Interp => false,
            Tier::Clif => true,
            Tier::Auto => cfg!(feature = "qc-clif"),
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// When a query moves between the tiers, which the tier differential forces and nothing else asks
/// for.
///
/// A switch happens at a morsel boundary: the tier is picked once per morsel a pipeline is fed,
/// and a morsel runs to the end on the tier it started on. Since the two tiers agree on the state
/// to the byte, a query that moves between them at random morsels has to give the answer it gives
/// on either one alone, and a difference is a bug in one of them that a whole query on one tier
/// could hide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Switch {
    /// Every morsel on the query's tier.
    #[default]
    Off,
    /// `n` morsels on the second tier, then `n` on `interp`, and so on.
    Every(u64),
    /// Each morsel on a tier drawn from this seed, so that a failure comes back with the seed.
    Random(u64),
}

impl Switch {
    /// The switch `SET qc_switch` names: `off`, `every:<n>` with `n` at least 1, or
    /// `random:<seed>`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Switch> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("off") {
            return Some(Switch::Off);
        }
        let (kind, n) = name.split_once(':')?;
        let n: u64 = n.trim().parse().ok()?;
        match kind.trim().to_ascii_lowercase().as_str() {
            "every" if n > 0 => Some(Switch::Every(n)),
            "random" => Some(Switch::Random(n)),
            _ => None,
        }
    }

    /// Whether morsel `n` of a query runs on the second tier.
    fn native(self, n: u64) -> bool {
        match self {
            Switch::Off => true,
            Switch::Every(k) => (n / k).is_multiple_of(2),
            Switch::Random(seed) => mix(seed ^ mix(n)) & 1 == 0,
        }
    }
}

impl fmt::Display for Switch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Switch::Off => f.write_str("off"),
            Switch::Every(n) => write!(f, "every:{n}"),
            Switch::Random(seed) => write!(f, "random:{seed}"),
        }
    }
}

/// Where a pipeline's rows go, which is what says whether a switch landed inside live state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sink {
    /// Rows out, with nothing held from one morsel to the next but the row count.
    Result,
    /// A hash aggregate, whose groups are live across morsels.
    Aggregate,
    /// A join build, whose table is live until it is finalized.
    Build,
}

/// How many times a query moved between the tiers from one morsel of a pipeline to the next, by
/// what the pipeline was filling at the time.
///
/// Section 15.3 of `spec/compiler/15-correctness.md` asks that the tier differential check it hit
/// live state: a switch in the middle of an aggregate or a build is the one that tests something.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Switches {
    /// In a pipeline that produces rows.
    pub result: u64,
    /// In a pipeline that feeds a hash aggregate.
    pub aggregate: u64,
    /// In a pipeline that builds a join's table.
    pub build: u64,
}

impl Switches {
    /// All of them.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.result + self.aggregate + self.build
    }
}

/// splitmix64's finalizer, which is all a coin per morsel needs.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// How a query is compiled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The tier its pipelines run on.
    pub tier: Tier,
    /// Whether it is made to move between the tiers as it runs.
    pub switch: Switch,
}

/// What the second tier did with a query's module, for `EXPLAIN (CODEGEN)` and the query log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The tier the query asked for, with `auto` decided.
    pub tier: &'static str,
    /// The time spent turning the plan into the QIR module, on every tier.
    pub generate: Duration,
    /// The time spent lowering and loading machine code, zero on `interp`.
    pub compile: Duration,
    /// How many functions the module has.
    pub functions: usize,
    /// How many of them run as machine code.
    pub native: usize,
    /// The bytes of machine code loaded.
    pub bytes: usize,
    /// Each function that runs on `interp` although the tier is `clif`, and why.
    pub fallbacks: Vec<String>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tier {}: {} of {} functions native, {} bytes, generated in {:.3} ms, compiled in {:.3} ms",
            self.tier,
            self.native,
            self.functions,
            self.bytes,
            self.generate.as_secs_f64() * 1e3,
            self.compile.as_secs_f64() * 1e3,
        )?;
        for reason in &self.fallbacks {
            write!(f, "\n  interp: {reason}")?;
        }
        Ok(())
    }
}

/// A module on every tier it has.
pub(crate) struct Tiers {
    program: Program,
    #[cfg(feature = "qc-clif")]
    native: Vec<Option<rudb_qc_rt::code::Code>>,
    report: Report,
    switch: Switch,
    /// How many morsels have been fed, which numbers them for [`Switch`].
    morsels: AtomicU64,
    /// Per function, whether its last morsel ran as machine code, 2 before its first.
    last: Vec<AtomicU8>,
    /// How many times a morsel ran on another tier than the morsel of the same function before
    /// it, by [`Sink`].
    switches: [AtomicU64; 3],
}

impl fmt::Debug for Tiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tiers").field("report", &self.report).finish_non_exhaustive()
    }
}

impl Tiers {
    /// Lowers `module` for the interpreter and, when `options` asks for it, for the machine.
    pub(crate) fn new(module: &Module, options: Options) -> Tiers {
        let Options { tier, switch } = options;
        let program = Program::new(module);
        let counts = (
            AtomicU64::new(0),
            module.funcs.iter().map(|_| AtomicU8::new(2)).collect(),
            [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)],
        );
        let mut report = Report {
            tier: if tier.native() { Tier::Clif.name() } else { Tier::Interp.name() },
            functions: module.funcs.len(),
            ..Report::default()
        };
        #[cfg(feature = "qc-clif")]
        {
            let native = if tier.native() {
                clif::compile(module, &mut report)
            } else {
                module.funcs.iter().map(|_| None).collect()
            };
            let (morsels, last, switches) = counts;
            Tiers { program, native, report, switch, morsels, last, switches }
        }
        #[cfg(not(feature = "qc-clif"))]
        {
            if tier.native() {
                report.fallbacks.push("this build has no clif tier".to_string());
            }
            let (morsels, last, switches) = counts;
            Tiers { program, report, switch, morsels, last, switches }
        }
    }

    /// What the second tier did.
    pub(crate) fn report(&self) -> &Report {
        &self.report
    }

    /// Notes how long the module took to generate, which happened before the tiers saw it.
    pub(crate) fn generated_in(&mut self, took: Duration) {
        self.report.generate = took;
    }

    /// The index of the function with this name.
    pub(crate) fn func(&self, name: &str) -> Option<usize> {
        self.program.func(name)
    }

    /// How many times a morsel ran on another tier than the morsel of its pipeline before it.
    pub(crate) fn switches(&self) -> Switches {
        let [result, aggregate, build] =
            self.switches.each_ref().map(|n| n.load(Ordering::Relaxed));
        Switches { result, aggregate, build }
    }

    /// The tier the next morsel of function `f`, which fills `sink`, runs on, as the argument
    /// [`Tiers::call`] takes: machine code when there is some, unless [`Switch`] says otherwise for
    /// this morsel.
    pub(crate) fn morsel(&self, f: usize, sink: Sink) -> bool {
        let n = self.morsels.fetch_add(1, Ordering::Relaxed);
        let native = self.has_native(f) && self.switch.native(n);
        let last = self.last.get(f).map_or(2, |l| l.swap(u8::from(native), Ordering::Relaxed));
        if last != 2 && last != u8::from(native) {
            self.switches[sink as usize].fetch_add(1, Ordering::Relaxed);
        }
        native
    }

    /// Whether function `f` has machine code.
    fn has_native(&self, f: usize) -> bool {
        #[cfg(feature = "qc-clif")]
        {
            matches!(self.native.get(f), Some(Some(_)))
        }
        #[cfg(not(feature = "qc-clif"))]
        {
            let _ = f;
            false
        }
    }

    /// Runs function `f` on a state and a morsel, as machine code when `native` is set and there
    /// is some, and returns its status.
    pub(crate) fn call(
        &self,
        native: bool,
        f: usize,
        st: *mut u8,
        m: *const u8,
        rt: &mut Rt,
    ) -> u64 {
        #[cfg(feature = "qc-clif")]
        if native && let Some(Some(code)) = self.native.get(f) {
            return clif::call(code, st, m, rt);
        }
        #[cfg(not(feature = "qc-clif"))]
        let _ = native;
        self.program.call(f, st, m, rt)
    }
}

#[cfg(feature = "qc-clif")]
mod clif {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::OnceLock;
    use std::time::Instant;

    use rudb_qc_clif::Backend;
    use rudb_qc_ir::Module;
    use rudb_qc_rt::code::{Code, CodeArena, Reloc, RelocKind};
    use rudb_qc_rt::native::{Ctx, address};
    use rudb_qc_rt::{Rt, abi};

    use super::Report;

    /// The backend and the arena, made once per process, or why they could not be.
    fn machine() -> Result<&'static (Backend, CodeArena), &'static str> {
        static MACHINE: OnceLock<Result<(Backend, CodeArena), String>> = OnceLock::new();
        MACHINE
            .get_or_init(|| {
                let backend = Backend::host().map_err(|e| e.to_string())?;
                let arena = CodeArena::new().map_err(|e| format!("no code arena: {e}"))?;
                Ok((backend, arena))
            })
            .as_ref()
            .map_err(String::as_str)
    }

    /// Compiles and loads every function of `module`, and says in `report` what it did.
    pub(super) fn compile(module: &Module, report: &mut Report) -> Vec<Option<Code>> {
        let start = Instant::now();
        let machine = match machine() {
            Ok(m) => m,
            Err(why) => {
                report.fallbacks.push(why.to_string());
                return module.funcs.iter().map(|_| None).collect();
            }
        };
        let (backend, arena) = machine;
        let mut out = Vec::with_capacity(module.funcs.len());
        for f in &module.funcs {
            // Cranelift asserts what it believes about its input, and an assertion here is a
            // function this tier does not handle, not a reason to fail the query.
            let compiled = catch_unwind(AssertUnwindSafe(|| backend.compile(f)))
                .unwrap_or_else(|_| {
                    Err(rudb_qc_clif::Error {
                        func: f.name.clone(),
                        reason: "the code generator panicked".to_string(),
                    })
                })
                .map_err(|e| e.to_string())
                .and_then(|code| {
                    let relocs: Vec<Reloc> = code
                        .relocs
                        .iter()
                        .map(|r| Reloc {
                            offset: r.offset,
                            kind: RelocKind::Abs8,
                            target: address(r.entry),
                            addend: r.addend,
                        })
                        .collect();
                    arena
                        .load(&code.bytes, &relocs)
                        .map_err(|e| format!("clif: {}: loading: {e}", f.name))
                });
            match compiled {
                Ok(code) => {
                    report.native += 1;
                    report.bytes += code.len();
                    out.push(Some(code));
                }
                Err(why) => {
                    report.fallbacks.push(why);
                    out.push(None);
                }
            }
        }
        report.compile = start.elapsed();
        out
    }

    /// Calls machine code with the runtime reachable through the state header.
    pub(super) fn call(code: &Code, st: *mut u8, m: *const u8, rt: &mut Rt) -> u64 {
        let mut ctx = Ctx::new(rt);
        let word = ctx.word();
        let at = st.wrapping_add(abi::RT as usize).cast::<u64>();
        // SAFETY: `st` is a state block, which starts with the 64 byte header, and `rt` is the
        // pointer sized word at `abi::RT` in it. The word is the address of `ctx`, which lives
        // until after the call, and it is cleared again before `ctx` goes.
        unsafe { at.write(word) };
        // SAFETY: the code is a pipeline function the backend compiled from the module the state
        // and the morsel were laid out for, and the entries it calls find `ctx` through the word
        // just written.
        let status = unsafe { code.call(st, m) };
        // SAFETY: as for the write above.
        unsafe { at.write(0) };
        status
    }
}

#[cfg(all(test, feature = "qc-clif"))]
mod tests;
