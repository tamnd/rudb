//! The runtime compiled code calls into, per section 6.5 of `spec/compiler/06-qir.md`.
//!
//! [`Rt`] owns everything a query's pipelines share: the heap strings are made in, the objects
//! a handle names, the kernels a `vcall` runs, the counters and the cancel flag. A handle is a
//! small integer the generator put in the code as a `ptr` constant, and it indexes the objects
//! the driver added before the query started. A row or accumulator address is a real address.

#![allow(unsafe_code)]

use rudb_common::{Cancel, Error, ErrorCode, civil_from_days, days_from_civil};
use rudb_qc_interp::Runtime;
use rudb_qc_ir::{CATALOGUE, status};
use rudb_regex::{Regex, Rewrite};

use crate::like::Like;
use crate::mem;
use crate::table::{Distinct, GroupTable, read_u128};
use crate::text::{self, Heap};

/// The error site payload that means the runtime failed and [`Rt::error`] says why.
pub const RUNTIME_ERROR: u64 = 0xff_ffff;

/// A first engine kernel over `n` rows of the buffers a `vcall` passes.
pub type Kernel = Box<dyn FnMut(u64, &[u128]) -> Result<(), Error> + Send>;

/// Something a handle names.
enum Object {
    Like(Like),
    Regex { regex: Regex, rewrite: Rewrite, global: bool },
    Table(GroupTable),
    Distinct(Distinct),
}

/// The runtime of one query.
pub struct Rt {
    heap: Heap,
    objects: Vec<Object>,
    kernels: Vec<Kernel>,
    counters: Vec<u64>,
    cancel: Cancel,
    error: Option<Error>,
    buffer: String,
}

impl std::fmt::Debug for Rt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rt")
            .field("objects", &self.objects.len())
            .field("kernels", &self.kernels.len())
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl Rt {
    /// A runtime that stops when `cancel` says so.
    #[must_use]
    pub fn new(cancel: Cancel) -> Rt {
        Rt {
            heap: Heap::new(),
            objects: Vec::new(),
            kernels: Vec::new(),
            counters: Vec::new(),
            cancel,
            error: None,
            buffer: String::new(),
        }
    }

    fn add(&mut self, object: Object) -> u64 {
        self.objects.push(object);
        self.objects.len() as u64 - 1
    }

    /// A handle on a `LIKE` pattern, folded for `ILIKE`.
    pub fn add_like(&mut self, pattern: &str, fold: bool) -> u64 {
        self.add(Object::Like(Like::new(pattern, fold)))
    }

    /// A handle on a regular expression and, for `regexp_replace`, its rewrite.
    ///
    /// # Errors
    ///
    /// When the pattern or the options do not parse.
    pub fn add_regex(&mut self, pattern: &str, rewrite: &str, options: &str) -> Result<u64, Error> {
        let options = rudb_regex::Options::parse(options)?;
        let global = options.global;
        let regex = Regex::with_options(pattern, options)?;
        let rewrite = Rewrite::new(rewrite, regex.groups());
        Ok(self.add(Object::Regex { regex, rewrite, global }))
    }

    /// A handle on a grouping table.
    pub fn add_table(&mut self, table: GroupTable) -> u64 {
        self.add(Object::Table(table))
    }

    /// A handle on a set of distinct values per group.
    pub fn add_distinct(&mut self) -> u64 {
        self.add(Object::Distinct(Distinct::new()))
    }

    /// The id a `vcall` uses for `kernel`.
    pub fn add_kernel(&mut self, kernel: Kernel) -> u32 {
        self.kernels.push(kernel);
        self.kernels.len() as u32 - 1
    }

    /// The table behind a handle.
    #[must_use]
    pub fn table(&self, handle: u64) -> Option<&GroupTable> {
        match self.objects.get(handle as usize) {
            Some(Object::Table(t)) => Some(t),
            _ => None,
        }
    }

    /// The distinct sets behind a handle.
    #[must_use]
    pub fn distinct(&self, handle: u64) -> Option<&Distinct> {
        match self.objects.get(handle as usize) {
            Some(Object::Distinct(d)) => Some(d),
            _ => None,
        }
    }

    /// Keeps `bytes` in the runtime heap for as long as the query runs.
    pub fn keep(&mut self, bytes: &[u8]) -> u128 {
        self.heap.keep(bytes)
    }

    /// The counters, by id.
    #[must_use]
    pub fn counters(&self) -> &[u64] {
        &self.counters
    }

    /// Why the last call that failed with [`RUNTIME_ERROR`] failed.
    pub fn take_error(&mut self) -> Option<Error> {
        self.error.take()
    }

    /// The bytes the heap holds.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.heap.footprint()
    }

    fn fail(&mut self, error: Error) -> u64 {
        self.error = Some(error);
        status::make(status::ERROR, RUNTIME_ERROR)
    }

    fn string(&mut self, bytes: &[u8]) -> u128 {
        if bytes.len() <= text::INLINE { text::make(bytes) } else { self.heap.keep(bytes) }
    }

    fn call(&mut self, name: &str, a: &[u128]) -> Result<u128, u64> {
        // SAFETY: every `str16` compiled code passes is inline or points at bytes the driver or
        // this heap keeps alive for the whole query, which is rule V10.
        let s = |i: usize| unsafe { text::bytes(&a[i]) };
        Ok(match name {
            "str_promote" => {
                let bytes = s(1).to_vec();
                self.string(&bytes)
            }
            "str_eq" => u128::from(a[0] == a[1] || s(0) == s(1)),
            "str_cmp" => u128::from(s(0).cmp(s(1)) as i32 as u32),
            "str_hash" => u128::from(hash(s(0), a[1] as u64)),
            "str_length" => u128::from(s(0).iter().filter(|b| (**b as i8) >= -0x40).count() as u64),
            "str_like" => match self.objects.get(a[0] as usize) {
                Some(Object::Like(like)) => u128::from(like.matches(s(1))),
                _ => return Err(self.fail(bad_handle(name))),
            },
            "str_regex" => {
                let Some(Object::Regex { regex, .. }) = self.objects.get(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                u128::from(regex.is_match(utf8(s(1))))
            }
            "str_regex_replace" => {
                let Some(Object::Regex { regex, rewrite, global }) =
                    self.objects.get(a[0] as usize)
                else {
                    return Err(self.fail(bad_handle(name)));
                };
                let mut out = std::mem::take(&mut self.buffer);
                out.clear();
                regex.replace_into(&mut out, utf8(s(1)), rewrite, *global);
                let v = self.string(out.as_bytes());
                self.buffer = out;
                v
            }
            "str_lower" | "str_upper" => {
                let t = utf8(s(0));
                let out = if name == "str_lower" { t.to_lowercase() } else { t.to_uppercase() };
                self.string(out.as_bytes())
            }
            "str_concat" => {
                let mut out = Vec::with_capacity(s(0).len() + s(1).len());
                out.extend_from_slice(s(0));
                out.extend_from_slice(s(1));
                self.string(&out)
            }
            "ht_insert" => {
                let Some(Object::Table(table)) = self.objects.get_mut(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                // SAFETY: the generator passes the key buffer in its state, laid out as the
                // table's layout says.
                let row = unsafe { table.insert(a[1] as usize, a[2] as u64, &mut self.heap) };
                row as u128
            }
            "agg_distinct" | "agg_distinct_int" => {
                // SAFETY: the second argument is a row `ht_insert` returned, and it starts with
                // the group id.
                let gid = u64::from_le_bytes(
                    unsafe { mem::slice(a[1] as usize, 8) }.try_into().unwrap_or_default(),
                );
                let Some(Object::Distinct(set)) = self.objects.get_mut(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                if name == "agg_distinct" {
                    set.add_text(gid as usize, s(2));
                } else {
                    set.add_int(gid as usize, a[2]);
                }
                0
            }
            "agg_min_str" | "agg_max_str" => {
                let at = a[0] as usize;
                // SAFETY: the first argument is the accumulator in a group row, a `str16` and a
                // byte that says whether it holds a value yet.
                let acc = unsafe { mem::slice(at, 17) };
                let (old, seen) = (read_u128(acc), acc[16] != 0);
                let better = !seen || {
                    // SAFETY: the accumulator holds a string this heap keeps.
                    let old = unsafe { text::bytes(&old) };
                    let ord = s(1).cmp(old);
                    if name == "agg_min_str" { ord.is_lt() } else { ord.is_gt() }
                };
                if better {
                    let bytes = s(1).to_vec();
                    let v = self.string(&bytes);
                    let mut w = [1u8; 17];
                    w[..16].copy_from_slice(&v.to_le_bytes());
                    // SAFETY: as above, and nothing else writes the row while this runs.
                    unsafe { mem::write(at, &w) };
                }
                0
            }
            "i128_div" => {
                let (x, y) = (a[0] as i128, a[1] as i128);
                match x.checked_div(y) {
                    Some(q) => q as u128,
                    None => {
                        let why = if y == 0 { "Division by zero" } else { "Decimal out of range" };
                        return Err(self.fail(Error::new(ErrorCode::OutOfRange, why)));
                    }
                }
            }
            "date_trunc_minute" => {
                let t = a[0] as u64 as i64;
                u128::from(t.div_euclid(MINUTE).wrapping_mul(MINUTE) as u64)
            }
            "date_extract_minute" => {
                let t = a[0] as u64 as i64;
                u128::from(t.div_euclid(MINUTE).rem_euclid(60) as u64)
            }
            "date_extract_year" => {
                let (y, _, _) = civil_from_days(a[0] as u32 as i32);
                u128::from(i64::from(y) as u64)
            }
            "date_trunc_month" => {
                let (y, m, _) = civil_from_days(a[0] as u32 as i32);
                u128::from(days_from_civil(y, m, 1) as u32)
            }
            _ => {
                return Err(self
                    .fail(Error::new(ErrorCode::Internal, format!("no runtime function {name}"))));
            }
        })
    }
}

const MINUTE: i64 = 60_000_000;

fn bad_handle(name: &str) -> Error {
    Error::new(ErrorCode::Internal, format!("{name} got a handle on the wrong kind of object"))
}

fn utf8(bytes: &[u8]) -> &str {
    // A varchar is UTF-8 everywhere it is made, so a string that is not came from a bug, and the
    // replacement keeps the bug visible without undefined behavior.
    std::str::from_utf8(bytes).unwrap_or("\u{fffd}")
}

/// The hash of a string's bytes, mixed with `seed` so a key of several columns chains.
#[must_use]
pub fn hash(bytes: &[u8], seed: u64) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut h = seed ^ (bytes.len() as u64).wrapping_mul(K);
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().unwrap_or_default());
        h = (h ^ w).wrapping_mul(K).rotate_left(29);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        h = (h ^ u64::from_le_bytes(w)).wrapping_mul(K).rotate_left(29);
    }
    h ^= h >> 32;
    h.wrapping_mul(K) ^ (h >> 29)
}

impl Runtime for Rt {
    fn rtcall(&mut self, proxy: u32, args: &[u128]) -> Result<u128, u64> {
        let Some(p) = CATALOGUE.get(proxy as usize) else {
            return Err(
                self.fail(Error::new(ErrorCode::Internal, format!("no runtime function {proxy}")))
            );
        };
        self.call(p.name, args)
    }

    fn vcall(&mut self, kernel: u32, n: u64, buffers: &[u128]) -> Result<(), u64> {
        let Some(k) = self.kernels.get_mut(kernel as usize) else {
            return Err(self.fail(Error::new(ErrorCode::Internal, format!("no kernel {kernel}"))));
        };
        match k(n, buffers) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.fail(e)),
        }
    }

    fn count(&mut self, k: u32, v: u64) {
        let k = k as usize;
        if self.counters.len() <= k {
            self.counters.resize(k + 1, 0);
        }
        self.counters[k] = self.counters[k].wrapping_add(v);
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudb_qc_ir::catalogue::proxy;

    fn call(rt: &mut Rt, name: &str, args: &[u128]) -> Result<u128, u64> {
        rt.rtcall(proxy(name).unwrap(), args)
    }

    #[test]
    fn every_proxy_in_the_catalogue_is_implemented() {
        let mut rt = Rt::new(Cancel::new());
        let like = rt.add_like("%a%", false);
        let re = rt.add_regex("^https?://(?:www\\.)?([^/]+)/.*$", "\\1", "").unwrap();
        let table = rt.add_table(GroupTable::new(crate::table::Layout {
            init: vec![0; 24],
            ..Default::default()
        }));
        let set = rt.add_distinct();
        let row = rt.table(table).unwrap().address(0) as u128;
        let s = text::make(b"abc");
        for (i, p) in CATALOGUE.iter().enumerate() {
            let args: Vec<u128> = match p.name {
                "str_like" => vec![u128::from(like), s],
                "str_regex" | "str_regex_replace" => vec![u128::from(re), s],
                "ht_insert" => vec![u128::from(table), 0, 5],
                "agg_distinct" | "agg_distinct_int" => vec![u128::from(set), row, s],
                "agg_min_str" | "agg_max_str" => vec![row + 8],
                "i128_div" => vec![10, 3],
                _ => vec![s; p.args.len()],
            };
            let mut args = args;
            args.resize(p.args.len(), s);
            let r = rt.rtcall(i as u32, &args);
            assert!(r.is_ok(), "{} failed: {:?}", p.name, rt.take_error());
        }
    }

    #[test]
    fn strings_and_dates_match_the_first_engine() {
        let mut rt = Rt::new(Cancel::new());
        let re = rt.add_regex("^https?://(?:www\\.)?([^/]+)/.*$", "\\1", "").unwrap();
        let url = rt.keep(b"http://www.example.com/page?x=1");
        let host = call(&mut rt, "str_regex_replace", &[u128::from(re), url]).unwrap();
        // SAFETY: the host is inline or in the runtime heap.
        assert_eq!(unsafe { text::bytes(&host) }, b"example.com");
        let long = rt.keep("a string in the heap, Ünïcode".as_bytes());
        assert_eq!(call(&mut rt, "str_length", &[long]).unwrap(), 29);
        let up = call(&mut rt, "str_upper", &[long]).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { text::bytes(&up) }, "A STRING IN THE HEAP, ÜNÏCODE".as_bytes());
        let same = rt.keep("a string in the heap, Ünïcode".as_bytes());
        assert_eq!(call(&mut rt, "str_eq", &[long, same]).unwrap(), 1);
        assert_eq!(
            call(&mut rt, "str_cmp", &[text::make(b"a"), text::make(b"b")]).unwrap(),
            u128::from(u32::MAX)
        );
        let micros = 1_373_846_400_000_000u128 + 61 * 60_000_000 + 5;
        assert_eq!(call(&mut rt, "date_extract_minute", &[micros]).unwrap(), 1);
        assert_eq!(call(&mut rt, "date_trunc_minute", &[micros]).unwrap(), micros - 5);
        let day = u128::from(days_from_civil(2013, 7, 15) as u32);
        assert_eq!(call(&mut rt, "date_extract_year", &[day]).unwrap(), 2013);
        assert_eq!(
            call(&mut rt, "date_trunc_month", &[day]).unwrap(),
            u128::from(days_from_civil(2013, 7, 1) as u32)
        );
        let e = call(&mut rt, "i128_div", &[1, 0]).unwrap_err();
        assert_eq!(status::kind(e), status::ERROR);
        assert!(rt.take_error().is_some());
    }

    #[test]
    fn min_and_max_of_strings_keep_the_extreme() {
        let mut rt = Rt::new(Cancel::new());
        let mut acc = [0u8; 17];
        let at = acc.as_mut_ptr().expose_provenance() as u128;
        for w in ["pear", "apple and a long tail", "zebra"] {
            let s = rt.keep(w.as_bytes());
            call(&mut rt, "agg_min_str", &[at, s]).unwrap();
        }
        let v = read_u128(&acc);
        // SAFETY: the accumulator holds a heap string.
        assert_eq!(unsafe { text::bytes(&v) }, b"apple and a long tail");
    }
}
