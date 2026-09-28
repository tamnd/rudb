//! The runtime compiled code calls into, per section 6.5 of `spec/compiler/06-qir.md`.
//!
//! [`Rt`] owns everything a query's pipelines share: the heap strings are made in, the objects
//! a handle names, the kernels a `vcall` runs, the counters and the cancel flag. A handle is a
//! small integer the generator put in the code as a `ptr` constant, and it indexes the objects
//! the driver added before the query started. A row or accumulator address is a real address.

#![allow(unsafe_code)]

use std::sync::Arc;

use rudb_common::{Cancel, Error, ErrorCode, civil_from_days, days_from_civil};
use rudb_qc_interp::Runtime;
use rudb_qc_ir::{CATALOGUE, status};
use rudb_regex::{Regex, Rewrite};

use crate::join::{JoinTable, Published};
use crate::like::Like;
use crate::mem;
use crate::table::{Distinct, GroupTable, read_u128};
use crate::text::{self, Heap};

/// The proxies of `ht_insert`, `agg_distinct` and `agg_distinct_int`, which compiled code calls
/// once a row.
static HOT: std::sync::LazyLock<[u32; 3]> = std::sync::LazyLock::new(|| {
    let at =
        |name: &str| CATALOGUE.iter().position(|p| p.name == name).map_or(u32::MAX, |i| i as u32);
    [at("ht_insert"), at("agg_distinct"), at("agg_distinct_int")]
});

/// The error site payload that means the runtime failed and `Rt::error` says why.
pub const RUNTIME_ERROR: u64 = 0xff_ffff;

/// A first engine kernel over `n` rows of the buffers a `vcall` passes.
pub type Kernel = Box<dyn FnMut(u64, &[u128]) -> Result<(), Error> + Send>;

/// Makes a [`Kernel`]. A kernel keeps state between calls, so every worker needs its own.
pub type Maker = Arc<dyn Fn() -> Kernel + Send + Sync>;

/// Something a handle names.
enum Object {
    Like(Like),
    Regex {
        regex: Regex,
        rewrite: Rewrite,
        global: bool,
    },
    Table(GroupTable),
    Distinct(Distinct),
    Join(JoinTable),
    /// What a worker has where the query's runtime has a join table. A probe reads the table
    /// through the addresses its state was given and never through the handle, and a build runs
    /// on one worker, so a worker never needs one.
    Absent,
}

/// The runtime of one query.
pub struct Rt {
    heap: Heap,
    objects: Vec<Object>,
    kernels: Vec<Kernel>,
    makers: Vec<Maker>,
    counters: Vec<u64>,
    cancel: Cancel,
    error: Option<Error>,
    buffer: String,
    /// The runtimes of the workers folded into this one. Their heaps hold strings the rows of
    /// this one's tables point at, so they live as long as it does.
    held: Vec<Rt>,
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
            makers: Vec::new(),
            counters: Vec::new(),
            cancel,
            error: None,
            buffer: String::new(),
            held: Vec::new(),
        }
    }

    /// A runtime for one worker of a parallel pipeline. Every handle names the same kind of
    /// object as here, so the same code runs against it: the patterns are copies, the kernels are
    /// new, and the group tables and distinct sets are empty ones of the same shape, for the
    /// worker to fill and [`Rt::absorb`] to fold back in.
    #[must_use]
    pub fn worker(&self) -> Rt {
        let objects = self
            .objects
            .iter()
            .map(|o| match o {
                Object::Like(like) => Object::Like(like.clone()),
                Object::Regex { regex, rewrite, global } => Object::Regex {
                    regex: regex.clone(),
                    rewrite: rewrite.clone(),
                    global: *global,
                },
                Object::Table(t) => Object::Table(GroupTable::new(t.layout().clone())),
                Object::Distinct(_) => Object::Distinct(Distinct::new()),
                Object::Join(_) | Object::Absent => Object::Absent,
            })
            .collect();
        Rt {
            heap: Heap::new(),
            objects,
            kernels: self.makers.iter().map(|make| make()).collect(),
            makers: self.makers.clone(),
            counters: Vec::new(),
            cancel: self.cancel.clone(),
            error: None,
            buffer: String::new(),
            held: Vec::new(),
        }
    }

    /// Takes a worker's group table `table` and distinct sets `sets` in place of this runtime's,
    /// which is what the first worker of a parallel pipeline does. Nothing was written to this
    /// runtime's while the workers ran, so there is nothing in them to fold.
    pub fn adopt(&mut self, mut worker: Rt, table: u64, sets: &[u64]) {
        for &h in std::iter::once(&table).chain(sets) {
            let h = h as usize;
            if let (Some(mine), Some(theirs)) = (self.objects.get_mut(h), worker.objects.get_mut(h))
            {
                std::mem::swap(mine, theirs);
            }
        }
        self.keep_worker(worker);
    }

    /// Folds a worker's group table `table` and distinct sets `sets` into this runtime's, and says
    /// which group of this table each of the worker's became. `combine` gets a row of this table
    /// and the worker's row of the same group, and folds the accumulators of the second into the
    /// first.
    ///
    /// # Errors
    ///
    /// When a handle does not name a table or a set in both runtimes.
    pub fn absorb(
        &mut self,
        mut worker: Rt,
        table: u64,
        sets: &[u64],
        combine: impl FnMut(&mut [u8], &[u8]),
    ) -> Result<Vec<usize>, Error> {
        let mut map = Vec::new();
        {
            let (Some(Object::Table(mine)), Some(Object::Table(theirs))) =
                (self.objects.get_mut(table as usize), worker.objects.get(table as usize))
            else {
                return Err(bad_handle("absorb"));
            };
            mine.absorb(theirs, &mut map, combine);
        }
        for &h in sets {
            let theirs = match worker.objects.get_mut(h as usize) {
                Some(o @ Object::Distinct(_)) => std::mem::replace(o, Object::Absent),
                _ => return Err(bad_handle("absorb")),
            };
            let (Some(Object::Distinct(mine)), Object::Distinct(theirs)) =
                (self.objects.get_mut(h as usize), theirs)
            else {
                return Err(bad_handle("absorb"));
            };
            mine.absorb(theirs, &map);
        }
        self.keep_worker(worker);
        Ok(map)
    }

    /// Takes the distinct sets behind a handle out of this runtime, and leaves nothing there.
    pub fn take_distinct(&mut self, handle: u64) -> Option<Distinct> {
        match self.objects.get_mut(handle as usize) {
            Some(o @ Object::Distinct(_)) => match std::mem::replace(o, Object::Absent) {
                Object::Distinct(d) => Some(d),
                _ => None,
            },
            _ => None,
        }
    }

    /// Puts `sets` behind a handle that named distinct sets before [`take_distinct`](Rt::take_distinct)
    /// took them.
    ///
    /// # Errors
    ///
    /// When the handle names something else.
    pub fn put_distinct(&mut self, handle: u64, sets: Distinct) -> Result<(), Error> {
        match self.objects.get_mut(handle as usize) {
            Some(o @ (Object::Distinct(_) | Object::Absent)) => {
                *o = Object::Distinct(sets);
                Ok(())
            }
            _ => Err(bad_handle("put_distinct")),
        }
    }

    /// Puts `merged` in place of group table `table`, and keeps `workers` the way a fold does. The
    /// table is what the workers' tables of the same handle became when they were merged, and its
    /// strings point into their heaps, which is why they are kept.
    ///
    /// # Errors
    ///
    /// When `table` does not name a group table.
    pub fn settle(
        &mut self,
        workers: Vec<Rt>,
        table: u64,
        merged: GroupTable,
    ) -> Result<(), Error> {
        let Some(Object::Table(mine)) = self.objects.get_mut(table as usize) else {
            return Err(bad_handle("settle"));
        };
        *mine = merged;
        for worker in workers {
            self.keep_worker(worker);
        }
        Ok(())
    }

    /// Adds the counters of a worker that has nothing left to fold to these, and keeps it.
    pub fn retire(&mut self, worker: Rt) {
        self.keep_worker(worker);
    }

    /// Adds a worker's counters to these and keeps its heap alive.
    fn keep_worker(&mut self, mut worker: Rt) {
        let counters = std::mem::take(&mut worker.counters);
        if self.counters.len() < counters.len() {
            self.counters.resize(counters.len(), 0);
        }
        for (mine, theirs) in self.counters.iter_mut().zip(counters) {
            *mine = mine.wrapping_add(theirs);
        }
        self.held.push(worker);
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

    /// A handle on a join hash table.
    pub fn add_join(&mut self, table: JoinTable) -> u64 {
        self.add(Object::Join(table))
    }

    /// The join table behind a handle.
    #[must_use]
    pub fn join(&self, handle: u64) -> Option<&JoinTable> {
        match self.objects.get(handle as usize) {
            Some(Object::Join(t)) => Some(t),
            _ => None,
        }
    }

    /// Runs the finalize step of a join build: lays out the table behind `handle` so that probes
    /// can read it, and returns what they read.
    ///
    /// # Errors
    ///
    /// When the handle is not a join table, or the table cannot be laid out.
    pub fn finish_join(&mut self, handle: u64) -> Result<Published, Error> {
        let Some(Object::Join(table)) = self.objects.get_mut(handle as usize) else {
            return Err(bad_handle("finish_join"));
        };
        table.finish().map_err(|e| Error::new(ErrorCode::Internal, e))?;
        Ok(table.published())
    }

    /// A handle on a set of distinct values per group.
    pub fn add_distinct(&mut self) -> u64 {
        self.add(Object::Distinct(Distinct::new()))
    }

    /// The id a `vcall` uses for the kernel `make` makes.
    pub fn add_kernel(&mut self, make: Maker) -> u32 {
        self.kernels.push(make());
        self.makers.push(make);
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

    /// The group table behind a handle, to change.
    pub fn table_mut(&mut self, handle: u64) -> Option<&mut GroupTable> {
        match self.objects.get_mut(handle as usize) {
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
        self.heap.footprint() + self.held.iter().map(Rt::footprint).sum::<usize>()
    }

    fn fail(&mut self, error: Error) -> u64 {
        self.error = Some(error);
        status::make(status::ERROR, RUNTIME_ERROR)
    }

    fn string(&mut self, bytes: &[u8]) -> u128 {
        if bytes.len() <= text::INLINE { text::make(bytes) } else { self.heap.keep(bytes) }
    }

    fn ht_insert(&mut self, a: &[u128]) -> Result<u128, u64> {
        let Some(Object::Table(table)) = self.objects.get_mut(a[0] as usize) else {
            return Err(self.fail(bad_handle("ht_insert")));
        };
        // SAFETY: the generator passes the key buffer in its state, laid out as the table's
        // layout says.
        let row = unsafe { table.insert(a[1] as usize, a[2] as u64, &mut self.heap) };
        Ok(row as u128)
    }

    fn agg_distinct(&mut self, a: &[u128], text: bool) -> Result<u128, u64> {
        // SAFETY: the second argument is a row `ht_insert` returned, and it starts with the group
        // id.
        let gid = u64::from_le_bytes(
            unsafe { mem::slice(a[1] as usize, 8) }.try_into().unwrap_or_default(),
        );
        let Some(Object::Distinct(set)) = self.objects.get_mut(a[0] as usize) else {
            return Err(self.fail(bad_handle("agg_distinct")));
        };
        if text {
            // SAFETY: as for every `str16` compiled code passes, see `call`.
            set.add_text(gid as usize, unsafe { text::bytes(&a[2]) });
        } else {
            set.add_int(gid as usize, a[2]);
        }
        Ok(0)
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
            "ht_insert" => self.ht_insert(a)?,
            "jt_append" => {
                let Some(Object::Join(table)) = self.objects.get_mut(a[0] as usize) else {
                    return Err(self.fail(bad_handle(name)));
                };
                // SAFETY: the generator passes the record buffer in its state, laid out as the
                // table's layout says.
                unsafe { table.append(a[1] as usize, a[2] as u64, &mut self.heap) };
                0
            }
            "agg_distinct" => self.agg_distinct(a, true)?,
            "agg_distinct_int" => self.agg_distinct(a, false)?,
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
        // The calls made once a row go straight to their code, and the rest by name.
        let hot = &*HOT;
        if proxy == hot[0] {
            return self.ht_insert(args);
        } else if proxy == hot[1] {
            return self.agg_distinct(args, true);
        } else if proxy == hot[2] {
            return self.agg_distinct(args, false);
        }
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
        let join = rt.add_join(JoinTable::new(crate::join::JoinLayout::default()));
        let row = rt.table(table).unwrap().address(0) as u128;
        let s = text::make(b"abc");
        for (i, p) in CATALOGUE.iter().enumerate() {
            let args: Vec<u128> = match p.name {
                "str_like" => vec![u128::from(like), s],
                "str_regex" | "str_regex_replace" => vec![u128::from(re), s],
                "ht_insert" => vec![u128::from(table), 0, 5],
                "jt_append" => vec![u128::from(join), 0, 5],
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
