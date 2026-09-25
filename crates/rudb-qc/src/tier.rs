//! The tiers a pipeline function runs on, per section 8.1 of `spec/compiler/08-backends.md`:
//! `interp`, which always exists, and `clif`, the Cranelift backend, when the build has the
//! `qc-clif` feature and the platform has a code arena.
//!
//! Every function is lowered for the interpreter, whatever the tier. That is the fallback for a
//! function the second tier does not lower, and it is what lets a query move between the tiers at
//! a morsel boundary: the two agree on the state and the morsel to the byte, so either one can
//! pick up where the other stopped.
//!
//! A function `clif` refuses, or panics on, runs on `interp` and the refusal is kept in the
//! [`Report`]. A query never fails because the second tier could not compile it.

use std::fmt;
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

/// How a query is compiled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The tier its pipelines run on.
    pub tier: Tier,
}

/// What the second tier did with a query's module, for `EXPLAIN (CODEGEN)` and the query log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The tier the query asked for, with `auto` decided.
    pub tier: &'static str,
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
            "tier {}: {} of {} functions native, {} bytes, compiled in {:.3} ms",
            self.tier,
            self.native,
            self.functions,
            self.bytes,
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
}

impl fmt::Debug for Tiers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tiers").field("report", &self.report).finish_non_exhaustive()
    }
}

impl Tiers {
    /// Lowers `module` for the interpreter and, when `tier` asks for it, for the machine.
    pub(crate) fn new(module: &Module, tier: Tier) -> Tiers {
        let program = Program::new(module);
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
            Tiers { program, native, report }
        }
        #[cfg(not(feature = "qc-clif"))]
        {
            if tier.native() {
                report.fallbacks.push("this build has no clif tier".to_string());
            }
            Tiers { program, report }
        }
    }

    /// What the second tier did.
    pub(crate) fn report(&self) -> &Report {
        &self.report
    }

    /// The index of the function with this name.
    pub(crate) fn func(&self, name: &str) -> Option<usize> {
        self.program.func(name)
    }

    /// Runs function `f` on a state and a morsel, as machine code when there is some, and
    /// returns its status.
    pub(crate) fn call(&self, f: usize, st: *mut u8, m: *const u8, rt: &mut Rt) -> u64 {
        #[cfg(feature = "qc-clif")]
        if let Some(Some(code)) = self.native.get(f) {
            return clif::call(code, st, m, rt);
        }
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
