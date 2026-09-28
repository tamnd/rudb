//! The function cache, level L2 of section 9.8 of `spec/compiler/09-tiering-and-caching.md`.
//!
//! Machine code is kept per process, keyed on the backend and the whole function with its name,
//! version and plan number left out, so the same pipeline in another query, or in the same query
//! run again, takes the code instead of compiling it. The key is the function's full `Debug` text
//! and a lookup compares all of it, so two functions that differ in anything a backend could read
//! never share code. The cache holds at most [`BUDGET`] bytes of code and forgets the entry used
//! longest ago past that. A query holds the code it took by an `Arc`, so forgetting an entry never
//! pulls code out from under a query that runs it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rudb_qc_ir::Func;
use rudb_qc_rt::code::Code;

/// The bytes of code the cache keeps, `qc.code_cache_bytes` of the spec.
pub(crate) const BUDGET: usize = 64 << 20;

/// What a function's code is found by: the backend that made it and every part of the function
/// the backend reads.
pub(crate) fn key(backend: &str, f: &Func) -> String {
    let bare = Func { name: String::new(), version: String::new(), plan: 0, ..f.clone() };
    format!("{backend}\n{bare:?}")
}

struct Entry {
    code: Arc<Code>,
    bytes: usize,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    bytes: usize,
    clock: u64,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// The code cached under `key`, if any.
pub(crate) fn get(key: &str) -> Option<Arc<Code>> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_with(Cache::default);
    cache.clock += 1;
    let clock = cache.clock;
    let entry = cache.entries.get_mut(key)?;
    entry.used = clock;
    Some(Arc::clone(&entry.code))
}

/// Keeps `code` under `key`, and forgets the entries used longest ago while the cache is over its
/// budget. Code bigger than the whole budget is not kept.
pub(crate) fn put(key: String, code: &Arc<Code>) {
    let bytes = code.len();
    if bytes > BUDGET {
        return;
    }
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let cache = cache.get_or_insert_with(Cache::default);
    cache.clock += 1;
    let entry = Entry { code: Arc::clone(code), bytes, used: cache.clock };
    if let Some(old) = cache.entries.insert(key, entry) {
        cache.bytes -= old.bytes;
    }
    cache.bytes += bytes;
    while cache.bytes > BUDGET {
        let Some(oldest) = cache.entries.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone())
        else {
            break;
        };
        if let Some(gone) = cache.entries.remove(&oldest) {
            cache.bytes -= gone.bytes;
        }
    }
}
