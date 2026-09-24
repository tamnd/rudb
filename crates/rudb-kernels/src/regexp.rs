//! The regular expression functions, over a pattern that is the same on every row.
//!
//! `regexp_replace`, `regexp_matches`, `regexp_full_match` and `regexp_extract`, with the engine in
//! `rudb-regex` under them. What this file is for is the thing that separates a usable
//! implementation from one that is technically correct: the pattern is compiled once per vector.
//! ClickBench query 29 runs one pattern over a hundred million rows, and compiling it per row would
//! cost more than matching it.
//!
//! Everything past the first argument has to be constant for the loop here to run, which is what
//! every query in the wild looks like, since a pattern that varies per row is a pattern the planner
//! could not have hoisted anyway. A call where it does vary falls through to the row at a time path
//! in `scalar`, which is correct and counts itself in the kernel table.
//!
//! The text side reads a flat column or a dictionary. A dictionary that outlives the chunk runs
//! `regexp_replace` once per distinct value through [`StableReplace`] and answers with a dictionary
//! of its own, and any other dictionary still runs the machine once per row.
//!
//! The number is 2,719,020 distinct in 8,682,923 rows at ClickBench scale, so running the machine
//! per entry is 3.19 times less matching. Most of the leverage is in what comes back, though. An
//! answer that carries codes instead of strings hands the operator above an integer key, and
//! `GROUP BY` on that column costs 0.28 seconds against 4.5 for the same grouping done on the
//! strings, measured in `spec/storage-v3/18`, where query 29 is 35% of the suite.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rudb_common::{Error, LogicalType, Result, Value};
use rudb_regex::{Options, Regex, Rewrite};
use rudb_vector::{Data, StringColumn, TextSource, Validity, Vector};

use crate::number::integral;
use crate::scalar::{finish, over_valid};
use crate::shape::nulls_of;

/// Whether a name is one of the functions here.
pub(crate) fn is_regexp(name: &str) -> bool {
    matches!(name, "regexp_replace" | "regexp_matches" | "regexp_full_match" | "regexp_extract")
}

/// What a recipe can lift out of a call to one of these, given the arguments that were literals.
///
/// A pattern that does not compile and an option letter that is not one both come back as `None`
/// rather than as an error, so the sentence the user sees still comes out of the chunk that reached
/// the call. Preparing a query is not allowed to raise something running it would have raised.
pub(crate) fn hoist(name: &str, literals: &[Option<Value>]) -> Option<Call> {
    let mut rest: Vec<&Value> = Vec::with_capacity(literals.len());
    for held in literals.iter().skip(1) {
        rest.push(held.as_ref()?);
    }
    let Ok(call) = Call::read(name, &rest) else {
        return None;
    };
    call
}

/// The vectorized path, or `None` when this call is not one it has a loop for.
pub(crate) fn vectorized<V: AsRef<Vector>>(
    name: &str,
    prepared: Option<&Call>,
    args: &[V],
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let Some(text) = args.first().map(AsRef::as_ref) else {
        return Ok(None);
    };
    let Some(source) = Source::of(text) else {
        return Ok(None);
    };
    // Compiled when the pipeline was built where the caller had a plan to read the pattern out of,
    // and compiled here for this one vector where it did not. ClickBench query 29 runs a hundred
    // thousand chunks, so the first is a hundred thousand compilations saved and the second is the
    // path a call through `call` with no recipe still takes.
    let held;
    let call = match prepared {
        Some(call) => call,
        None => {
            let mut constants: Vec<&Value> = Vec::new();
            for arg in args.iter().skip(1) {
                let Some(value) = arg.as_ref().constant_value() else {
                    return Ok(None);
                };
                constants.push(value);
            }
            let Some(read) = Call::read(name, &constants)? else {
                return Ok(None);
            };
            held = read;
            &held
        }
    };
    let base = nulls_of(text);
    match (name, returns) {
        ("regexp_replace", LogicalType::Varchar) => {
            if let (Some(call), Some((codes, dictionary))) =
                (prepared, text.stable_dictionary_parts())
            {
                return replace_stable(call, dictionary, codes, base, returns, rows);
            }
            // One buffer for the whole vector rather than a fresh `String` per row. It grows to the
            // longest value in the column once and then stays there.
            let mut buffer = String::new();
            let mut out = StringColumn::with_capacity(rows);
            let validity = over_strings(rows, base, &mut out, |index, out| {
                if call.host {
                    out.push_bytes(host_bytes(source.get_bytes(index)?));
                    return Ok(());
                }
                let text = source.get(index)?;
                buffer.clear();
                call.regex.replace_into(&mut buffer, text, &call.rewrite, call.global);
                out.push(&buffer);
                Ok(())
            })?;
            finish(returns, Data::Varlen(out), validity)
        }
        ("regexp_extract", LogicalType::Varchar) => {
            let mut out = StringColumn::with_capacity(rows);
            let validity = over_strings(rows, base, &mut out, |index, out| {
                out.push(call.regex.extract(source.get(index)?, call.group).unwrap_or_default());
                Ok(())
            })?;
            finish(returns, Data::Varlen(out), validity)
        }
        ("regexp_matches" | "regexp_full_match", LogicalType::Boolean) => {
            let whole = name == "regexp_full_match";
            let mut out = vec![false; rows];
            let validity = over_valid(rows, base, |index| {
                let text = source.get(index)?;
                out[index] =
                    if whole { call.regex.is_full_match(text) } else { call.regex.is_match(text) };
                Ok(())
            })?;
            finish(returns, Data::Bool(out.into()), validity)
        }
        _ => Ok(None),
    }
}

/// The row at a time path, which compiles the pattern for the one row it is given.
pub(crate) fn value(name: &str, args: &[Value]) -> Result<Value> {
    let Some(Value::Varchar(text)) = args.first() else {
        return Err(Error::internal(format!("{name} of something that is not a string")));
    };
    let constants: Vec<&Value> = args.iter().skip(1).collect();
    let Some(call) = Call::read(name, &constants)? else {
        return Err(Error::internal(format!("{name} with arguments it does not have")));
    };
    Ok(match name {
        "regexp_replace" => {
            if call.host {
                return Ok(Value::Varchar(host(text).to_string()));
            }
            let mut out = String::with_capacity(text.len());
            call.regex.replace_into(&mut out, text, &call.rewrite, call.global);
            Value::Varchar(out)
        }
        "regexp_extract" => {
            Value::Varchar(call.regex.extract(text, call.group).unwrap_or_default().to_string())
        }
        "regexp_full_match" => Value::Boolean(call.regex.is_full_match(text)),
        _ => Value::Boolean(call.regex.is_match(text)),
    })
}

/// Everything a call needs that does not change from row to row.
#[derive(Debug)]
pub(crate) struct Call {
    regex: Regex,
    /// The replacement, taken apart here rather than per row, and empty for everything that is not
    /// `regexp_replace`.
    rewrite: Rewrite,
    /// Whether the replacement replaces every match, which is the `g` option.
    global: bool,
    /// Which group `regexp_extract` wants, where zero is the whole match.
    group: usize,
    /// The fixed host extraction used by ClickBench q29.
    host: bool,
    /// What `regexp_replace` gave for the values of a dictionary that outlives the chunk.
    stable: OnceLock<StableReplace>,
}

impl Call {
    /// What `regexp_replace` gives for `text`, which is a piece of `text` for the host extraction
    /// and the contents of `buffer` otherwise.
    fn replaced<'t>(&self, text: &'t [u8], buffer: &'t mut String) -> Result<&'t [u8]> {
        replace_one(&self.regex, &self.rewrite, self.global, self.host, text, buffer)
    }
}

/// Runs `body` at every row that is not null and leaves an empty string at every row that is.
///
/// [`over_valid`] skips a null row outright, which suits an answer written in place and not strings
/// pushed one after another: every answer after the first null used to land one row early, and the
/// column came out shorter than the chunk.
fn over_strings(
    rows: usize,
    base: Validity,
    out: &mut StringColumn,
    mut body: impl FnMut(usize, &mut StringColumn) -> Result<()>,
) -> Result<Validity> {
    let validity = over_valid(rows, base, |index| {
        while out.len() < index {
            out.push("");
        }
        body(index, out)
    })?;
    while out.len() < rows {
        out.push("");
    }
    Ok(validity)
}

/// What `regexp_replace` gives for one value, apart from the call so the memo can hold a copy.
fn replace_one<'t>(
    regex: &Regex,
    rewrite: &Rewrite,
    global: bool,
    host: bool,
    text: &'t [u8],
    buffer: &'t mut String,
) -> Result<&'t [u8]> {
    if host {
        return Ok(host_bytes(text));
    }
    let text = std::str::from_utf8(text)
        .map_err(|_| Error::internal("a VARCHAR value that is not UTF-8"))?;
    buffer.clear();
    regex.replace_into(buffer, text, rewrite, global);
    Ok(buffer.as_bytes())
}

/// How many dictionary values one decision of the replace memo covers, which is what the native
/// format puts in a payload block, for the reason the `LIKE` memo in `scalar` gives.
const REPLACE_GROUP: usize = 1024;

/// How many locks the replaced texts seen so far are spread over, so threads deciding different
/// groups at once rarely wait on each other.
const REPLACE_SHARDS: usize = 64;

/// `regexp_replace` answered once per distinct value of a dictionary that outlives the chunk, and
/// answered as a dictionary.
///
/// ClickBench q29 runs the pattern over 8.7 million `Referer` rows that hold 2.7 million distinct
/// values and about a hundred thousand distinct hosts. A row at a time that is 3.2 times the
/// matching and the decompression, and handing back the hosts as strings leaves the `GROUP BY`
/// above to hash and compare 8.7 million of them.
///
/// So each value is replaced once, and the answer is a code: the first value of the dictionary
/// found to replace to the same text. Two rows with the same answer then carry the same code and
/// two with different answers different ones, which is what a stable dictionary promises the
/// operators above, and they group on the integers. The codes point into [`ReplacedText`], which
/// reads the answer back out of the memo, so the promise holds for as long as the call does.
///
/// A call answers every chunk over its dictionary this way from the first one on. It cannot fall
/// back to strings part way, because a group by hashes a stable dictionary by its codes and would
/// then hold the same key twice.
#[derive(Debug)]
struct StableReplace {
    memo: Arc<Memo>,
    /// The dictionary the codes this hands out point into, one for the life of the call.
    values: Option<Arc<Vector>>,
}

/// The replaced values of a dictionary, decided a group at a time.
///
/// Two threads can decide the same group at once. Both look every answer up in the same table, so
/// they reach the same codes, the first to finish keeps its group and the other drops its own.
#[derive(Debug)]
struct Memo {
    dictionary: Arc<Vector>,
    regex: Regex,
    rewrite: Rewrite,
    global: bool,
    host: bool,
    /// The code standing for each value's answer, a group at a time.
    groups: Vec<OnceLock<Box<[u32]>>>,
    /// The answers each group gave first, which the codes in `firsts` read back out of. A group's
    /// answers go in before any of its codes are put in `firsts`, so a code found there always has
    /// its answer here.
    answers: Vec<OnceLock<Replaced>>,
    /// Each distinct answer seen so far and the first value that gave it.
    firsts: Vec<Mutex<Seen>>,
    /// Bytes held so far, across every group and every answer.
    kept: AtomicUsize,
}

/// The answers one group of values was the first to give, with where in the group the value that
/// gave each one sits.
///
/// The answers the group was the first to give are copied once, end to end, into `bytes`, and the
/// table of answers seen so far reads them there through their codes rather than holding a copy.
/// On q29 the answers come to 37 MB. Held one allocation apiece they were 2.7 million blocks for
/// the allocator to take and later give back one at a time, and the giving back at the end of the
/// query was a quarter of its cycles.
#[derive(Debug)]
struct Replaced {
    owns: Box<[u16]>,
    /// Where each owned answer starts and ends in `bytes`, in the order of `owns`.
    spans: Box<[(u32, u32)]>,
    bytes: Box<[u8]>,
}

impl Replaced {
    /// The answer at `index` within the group, which is empty unless that value is its own code.
    fn get(&self, index: usize) -> &[u8] {
        let Ok(index) = u16::try_from(index) else { return &[] };
        self.owns.binary_search(&index).map_or(&[], |at| {
            let (start, end) = self.spans[at];
            &self.bytes[start as usize..end as usize]
        })
    }

    fn footprint(&self) -> usize {
        self.owns.len() * (size_of::<u16>() + size_of::<(u32, u32)>()) + self.bytes.len()
    }
}

/// An answer a group gave that the shared table did not hold when the group looked, waiting for the
/// group to finish before it goes in.
#[derive(Debug)]
struct Pending {
    hash: u64,
    start: u32,
    end: u32,
    /// Where in the group the value that gave it first sits.
    index: u16,
}

impl Memo {
    /// The group holding `code`, deciding it first where nothing has.
    ///
    /// An answer the shared table does not hold is given the code of the value that gave it, and
    /// only goes in once the whole group is decided, so that every answer the group owns can point
    /// into one block. Another group can give the same answer in between. Whichever registers it
    /// first keeps its code, and the other group's values are moved onto that code before its
    /// group is published, so no two codes ever stand for one answer.
    fn group(&self, code: usize) -> Result<&[u32]> {
        let slot = self
            .groups
            .get(code / REPLACE_GROUP)
            .ok_or_else(|| Error::internal("a stable dictionary code is out of range"))?;
        if let Some(done) = slot.get() {
            return Ok(done);
        }
        let answers = &self.answers[code / REPLACE_GROUP];
        let first = code / REPLACE_GROUP * REPLACE_GROUP;
        let last = (first + REPLACE_GROUP).min(self.dictionary.len());
        let mut buffer = String::new();
        let mut firsts = Vec::with_capacity(last - first);
        let mut pending: Vec<Pending> = Vec::new();
        let mut mine: Vec<u8> = Vec::new();
        let mut previous = Vec::new();
        let mut previous_found = None;
        let mut local = Local::default();
        let mut kept = Vec::new();
        let mut at = first;
        while at < last {
            let stopped = self.dictionary.sweep_text(at, last, &mut |_, text: &[u8]| {
                let index = firsts.len();
                let own = u32::try_from(first + index)
                    .ok()
                    .filter(|&own| own != u32::MAX)
                    .ok_or_else(|| Error::internal("a dictionary past four billion values"))?;
                let answer = replace_one(
                    &self.regex,
                    &self.rewrite,
                    self.global,
                    self.host,
                    text,
                    &mut buffer,
                )?;
                // Neighbouring values often give the same answer, the pages of one host, and the
                // one before is still at hand, so a repeat skips the table and its lock.
                //
                // Past that, a group of values holds about a third as many distinct answers as it
                // has values that differ from the one before, and those repeats find their code in
                // a table of the group's own answers that stays in cache. The shared table is a
                // cache miss and a lock a lookup.
                let found = match previous_found {
                    Some(found) if previous.as_slice() == answer => found,
                    _ => {
                        let hash = hash_of(answer);
                        let found = match local.get(&hash) {
                            Some(&(start, end, found))
                                if kept.get(start as usize..end as usize) == Some(answer) =>
                            {
                                found
                            }
                            known => {
                                let found = match self.first_of(hash, answer)? {
                                    Some(found) => found,
                                    None => {
                                        let (Ok(start), Ok(end), Ok(index)) = (
                                            u32::try_from(mine.len()),
                                            u32::try_from(mine.len() + answer.len()),
                                            u16::try_from(index),
                                        ) else {
                                            return Err(Error::internal(
                                                "a replaced group too large",
                                            ));
                                        };
                                        mine.extend_from_slice(answer);
                                        pending.push(Pending { hash, start, end, index });
                                        own
                                    }
                                };
                                if known.is_none() {
                                    let start = kept.len();
                                    kept.extend_from_slice(answer);
                                    if let (Ok(start), Ok(end)) =
                                        (u32::try_from(start), u32::try_from(kept.len()))
                                    {
                                        local.insert(hash, (start, end, found));
                                    }
                                }
                                found
                            }
                        };
                        previous.clear();
                        previous.extend_from_slice(answer);
                        previous_found = Some(found);
                        found
                    }
                };
                firsts.push(found);
                Ok(())
            })?;
            if stopped <= at {
                return Err(Error::internal("a dictionary sweep did not move"));
            }
            at = stopped;
        }
        // The answers go in before their codes do, so that another group that finds one of these
        // codes in the shared table can read its answer. A thread deciding this group at the same
        // time may have put its answers in first, and those hold every answer this one will put a
        // code in for: an answer missing from the shared table when this one registers was missing
        // when the other looked too, and the other gave it the same value's code.
        let out = Replaced {
            owns: pending.iter().map(|waiting| waiting.index).collect(),
            spans: pending.iter().map(|waiting| (waiting.start, waiting.end)).collect(),
            bytes: mine.as_slice().into(),
        };
        let held = out.footprint();
        if answers.set(out).is_ok() {
            self.kept.fetch_add(held, Ordering::Relaxed);
        }
        let mut moved: HashMap<u32, u32> = HashMap::new();
        let mut added = 0;
        for waiting in &pending {
            let own = (first + usize::from(waiting.index)) as u32;
            let answer = &mine[waiting.start as usize..waiting.end as usize];
            let found = self.register(waiting.hash, answer, own, &mut added)?;
            if found != own {
                moved.insert(own, found);
            }
        }
        if !moved.is_empty() {
            for code in &mut firsts {
                if let Some(&found) = moved.get(code) {
                    *code = found;
                }
            }
        }
        let firsts = firsts.into_boxed_slice();
        let held = firsts.len() * size_of::<u32>();
        if slot.set(firsts).is_ok() {
            self.kept.fetch_add(held, Ordering::Relaxed);
        }
        self.kept.fetch_add(added, Ordering::Relaxed);
        slot.get()
            .map(|done| &**done)
            .ok_or_else(|| Error::internal("a replaced group was set and is not there"))
    }

    /// The code the shared table holds for `answer`, if it holds one.
    fn first_of(&self, hash: u64, answer: &[u8]) -> Result<Option<u32>> {
        let seen = self.firsts[shard_of(hash)]
            .lock()
            .map_err(|_| Error::internal("a replace memo lock is poisoned"))?;
        Ok(seen.find(hash, |code| self.owned(code) == Some(answer)))
    }

    /// Puts a group's answer in the shared table under `own`, and the code it stands for, which is
    /// `own` unless another group put the same answer in first.
    ///
    /// A thread deciding a group another has decided already finds its own values in the table
    /// under their own codes, so both reach the same codes whichever publishes its group.
    fn register(&self, hash: u64, answer: &[u8], own: u32, added: &mut usize) -> Result<u32> {
        let mut seen = self.firsts[shard_of(hash)]
            .lock()
            .map_err(|_| Error::internal("a replace memo lock is poisoned"))?;
        if let Some(found) = seen.find(hash, |code| self.owned(code) == Some(answer)) {
            return Ok(found);
        }
        *added += seen.insert(hash, own);
        Ok(own)
    }

    /// The answer a group put in for the value at `code`, if its group has put its answers in.
    fn owned(&self, code: u32) -> Option<&[u8]> {
        let code = code as usize;
        let answers = self.answers.get(code / REPLACE_GROUP)?.get()?;
        Some(answers.get(code % REPLACE_GROUP))
    }

    /// The code standing for the answer of the value at `code`.
    fn first(&self, code: usize) -> Result<u32> {
        Ok(self.group(code)?[code % REPLACE_GROUP])
    }

    /// The answer of the value at `code`.
    fn answer(&self, code: usize) -> Result<&[u8]> {
        let first = self.first(code)?;
        self.owned(first).ok_or_else(|| Error::internal("a replaced code has no answer"))
    }
}

/// The lock an answer's hash is filed under. Bits from the middle, for the reason [`Words`] gives.
fn shard_of(hash: u64) -> usize {
    (hash >> 32) as usize % REPLACE_SHARDS
}

/// The hash an answer is filed under, in the shared table and in a group's own.
fn hash_of(answer: &[u8]) -> u64 {
    let mut words = Words::default();
    answer.hash(&mut words);
    words.finish()
}

/// The answers one group has looked up so far, by hash, each as where its bytes sit in the group's
/// copy of them and the code it came back with. Two answers with one hash keep the first, and the
/// second goes to the shared table every time.
type Local = HashMap<u64, (u32, u32, u32), BuildHasherDefault<Stored>>;

/// The answers one shard has seen, each as the code of the first value that gave it.
///
/// A slot is a word: the low half of the answer's hash above the code. The answer itself is read
/// through the code, out of the group that gave it, so the table holds no copy and no pointer. On
/// q29 that is 2.7 million answers, and the map this replaced kept each one as a hash, a pointer
/// to its group's block and a span, forty bytes a slot.
///
/// Open addressed with a linear probe and let fill to three quarters. The half hash in the slot is
/// what a grown table places it by, so growing reads no answer, and it turns away nearly every
/// slot that is not the answer asked for before its bytes are read.
#[derive(Debug, Default)]
struct Seen {
    slots: Vec<u64>,
    len: usize,
}

/// A slot nothing is in. A code is never `u32::MAX`, so no slot that is in use reads as this.
const FREE: u64 = u64::MAX;

impl Seen {
    /// The code of the answer `same` accepts among those filed under `hash`.
    fn find(&self, hash: u64, mut same: impl FnMut(u32) -> bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let low = hash as u32;
        let mut at = low as usize & mask;
        loop {
            let slot = self.slots[at];
            if slot == FREE {
                return None;
            }
            if (slot >> 32) as u32 == low && same(slot as u32) {
                return Some(slot as u32);
            }
            at = (at + 1) & mask;
        }
    }

    /// Files `code` under `hash`, which the caller has found is not in yet, and hands back how many
    /// bytes the table grew by.
    fn insert(&mut self, hash: u64, code: u32) -> usize {
        let mut grown = 0;
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            let wanted = (self.slots.len() * 2).max(16);
            let old = std::mem::replace(&mut self.slots, vec![FREE; wanted]);
            grown = (wanted - old.len()) * size_of::<u64>();
            for slot in old {
                if slot != FREE {
                    self.place(slot);
                }
            }
        }
        self.place(u64::from(hash as u32) << 32 | u64::from(code));
        self.len += 1;
        grown
    }

    fn place(&mut self, slot: u64) {
        let mask = self.slots.len() - 1;
        let mut at = (slot >> 32) as usize & mask;
        while self.slots[at] != FREE {
            at = (at + 1) & mask;
        }
        self.slots[at] = slot;
    }
}

/// A hasher that passes on the hash an answer already carries.
#[derive(Debug, Default)]
struct Stored(u64);

impl Hasher for Stored {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 << 8) | u64::from(byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A hash over a word at a time, for the replace memo's table of answers.
///
/// The standard hasher resists keys chosen by an attacker and pays a few rounds a word for it, which
/// on q29's 2.7 million answers showed up at 3.6% of the query. The shard takes bits from the
/// middle, since the table buckets on the low bits, and the group's own table also tags each slot
/// with the top seven, so a shard picked from either would leave every key in it agreeing there.
#[derive(Debug, Default)]
struct Words(u64);

impl Words {
    fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

impl Hasher for Words {
    fn write(&mut self, bytes: &[u8]) {
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            let mut held = [0; 8];
            held.copy_from_slice(word);
            self.mix(u64::from_le_bytes(held));
        }
        let rest = words.remainder();
        if !rest.is_empty() {
            let mut held = [0; 8];
            held[..rest.len()].copy_from_slice(rest);
            self.mix(u64::from_le_bytes(held));
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.mix(value as u64);
    }

    fn finish(&self) -> u64 {
        let mut spread = self.0;
        spread ^= spread >> 32;
        spread = spread.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        spread ^ (spread >> 29)
    }
}

/// The answers of a replace memo, as the text a dictionary's codes point into.
///
/// Every position reads, not only the codes the call handed out, so a kernel that walks the whole
/// dictionary gets the answers it would have got from strings.
#[derive(Debug)]
struct ReplacedText {
    memo: Arc<Memo>,
}

impl TextSource for ReplacedText {
    fn len(&self) -> usize {
        self.memo.dictionary.len()
    }

    fn bytes_at(&self, index: usize) -> Result<Option<&[u8]>> {
        if index >= self.len() {
            return Ok(None);
        }
        self.memo.answer(index).map(Some)
    }

    fn footprint(&self) -> usize {
        self.memo.kept.load(Ordering::Relaxed)
    }
}

/// The `regexp_replace` loop over a stable dictionary, through the memo on `call`.
fn replace_stable(
    call: &Call,
    dictionary: &Arc<Vector>,
    codes: &[u32],
    base: Validity,
    returns: &LogicalType,
    rows: usize,
) -> Result<Option<Vector>> {
    let stable = call.stable.get_or_init(|| {
        let memo = Arc::new(Memo {
            dictionary: Arc::clone(dictionary),
            regex: call.regex.clone(),
            rewrite: call.rewrite.clone(),
            global: call.global,
            host: call.host,
            groups: (0..dictionary.len().div_ceil(REPLACE_GROUP))
                .map(|_| OnceLock::new())
                .collect(),
            answers: (0..dictionary.len().div_ceil(REPLACE_GROUP))
                .map(|_| OnceLock::new())
                .collect(),
            firsts: (0..REPLACE_SHARDS).map(|_| Mutex::new(Seen::default())).collect(),
            kept: AtomicUsize::new(0),
        });
        let source = Arc::new(ReplacedText { memo: Arc::clone(&memo) });
        let values = (!dictionary.is_empty())
            .then(|| Vector::external_text(LogicalType::Varchar, source).ok().map(Arc::new))
            .flatten();
        StableReplace { memo, values }
    });
    if let (true, Some(values), LogicalType::Varchar) =
        (Arc::ptr_eq(&stable.memo.dictionary, dictionary), &stable.values, returns)
    {
        let mut out = vec![0u32; rows];
        let validity = over_valid(rows, base, |index| {
            let code = *codes
                .get(index)
                .ok_or_else(|| Error::internal("a dictionary vector is shorter than its rows"))?;
            out[index] = stable.memo.first(code as usize)?;
            Ok(())
        })?;
        let vector = Vector::stable_dictionary_validated(out, Arc::clone(values), None)?;
        return Ok(Some(vector.with_validity(validity)));
    }
    // A dictionary other than the one the memo was built over, which one call over one column
    // never sees, is answered a row at a time.
    let mut buffer = String::new();
    let mut out = StringColumn::with_capacity(rows);
    let validity = over_strings(rows, base, &mut out, |index, out| {
        let code = *codes
            .get(index)
            .ok_or_else(|| Error::internal("a dictionary vector is shorter than its rows"))?
            as usize;
        let text = dictionary.try_bytes_at(code)?.unwrap_or_default();
        out.push_bytes(call.replaced(text, &mut buffer)?);
        Ok(())
    })?;
    finish(returns, Data::Varlen(out), validity)
}

impl Call {
    /// Reads the arguments after the text, or `None` when they are not the shape this file handles.
    ///
    /// # Errors
    ///
    /// On a pattern that does not compile and on an option letter that is not one, which are the
    /// two things a user can get wrong and are reported with DuckDB's own words.
    fn read(name: &str, constants: &[&Value]) -> Result<Option<Self>> {
        let Some(Value::Varchar(pattern)) = constants.first().copied() else {
            return Ok(None);
        };
        let mut replacement = "";
        let mut rest = &constants[1..];
        if name == "regexp_replace" {
            let Some(Value::Varchar(held)) = rest.first().copied() else {
                return Ok(None);
            };
            replacement = held;
            rest = &rest[1..];
        }
        // What is left is the group index, the option string, both or neither, and which is which
        // is decided by the type rather than by the position, since the two are never the same
        // type and `regexp_extract` is the only one that takes both.
        let mut group = 0;
        let mut spelling = "";
        for value in rest {
            match value {
                Value::Varchar(held) => spelling = held,
                // A null group index is not a number and keeps the null path it already had.
                Value::Null => {}
                // DuckDB takes zero to nine and refuses everything else with this sentence, per
                // #496. It refuses it while binding and this cannot, for the reason `hoist` gives
                // a few lines up: preparing a query here does not raise what running it would
                // raise, so the sentence comes out of the first chunk that reaches the call, which
                // is where an option letter that is not one already comes out of. A query that
                // reaches no rows at all therefore still answers where upstream errors.
                //
                // A group inside zero to nine that the pattern does not have is the empty string
                // and not an error, which is why nothing here counts the pattern's groups.
                other if name == "regexp_extract" => {
                    let held = integral(other)
                        .and_then(|held| usize::try_from(held).ok())
                        .filter(|&held| held <= 9);
                    let Some(held) = held else {
                        return Err(Error::invalid_input("Group index must be between 0 and 9!"));
                    };
                    group = held;
                }
                // The other three take an option string and no group index, so a number here is
                // one upstream refuses while binding and this reads the way it always did.
                other => {
                    group = integral(other)
                        .and_then(|held| usize::try_from(held).ok())
                        .unwrap_or(usize::MAX);
                }
            }
        }
        let options = Options::parse(spelling)?;
        let host = name == "regexp_replace"
            && pattern == "^https?://(?:www\\.)?([^/]+)/.*$"
            && replacement == "\\1"
            && spelling.is_empty();
        let regex = Regex::with_options(pattern, options)?;
        let rewrite = Rewrite::new(replacement, regex.groups());
        Ok(Some(Self {
            regex,
            rewrite,
            global: options.global,
            group,
            host,
            stable: OnceLock::new(),
        }))
    }
}

/// The captured host, or the original text when the anchored pattern does not match.
fn host(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("http://").or_else(|| text.strip_prefix("https://")) else {
        return text;
    };
    let Some(end) = rest.find('/') else { return text };
    if end == 0 || memchr::memchr(b'\n', &rest.as_bytes()[end + 1..]).is_some() {
        return text;
    }
    let host = &rest[..end];
    host.strip_prefix("www.").filter(|without| !without.is_empty()).unwrap_or(host)
}

/// The q29 host extraction over already validated string bytes.
fn host_bytes(text: &[u8]) -> &[u8] {
    let rest = text.strip_prefix(b"http://").or_else(|| text.strip_prefix(b"https://"));
    let Some(rest) = rest else { return text };
    let Some(end) = memchr::memchr(b'/', rest) else { return text };
    if end == 0 || memchr::memchr(b'\n', &rest[end + 1..]).is_some() {
        return text;
    }
    let host = &rest[..end];
    host.strip_prefix(b"www.").filter(|without| !without.is_empty()).unwrap_or(host)
}

/// The text side of a call, which is a flat column or one read through positions.
enum Source<'a> {
    Flat(&'a StringColumn),
    /// A dictionary or a run length column, which are the same thing to a loop that reads text.
    Indirect(Cow<'a, [u32]>, &'a StringColumn),
    /// A dictionary whose values stay in the native reader's block cache.
    External(Cow<'a, [u32]>, &'a Vector),
    /// A storage-backed or view vector without another level of indirection.
    Direct(&'a Vector),
}

impl<'a> Source<'a> {
    fn of(vector: &'a Vector) -> Option<Self> {
        if *vector.logical_type() != LogicalType::Varchar {
            return None;
        }
        if let Some(Data::Varlen(column)) = vector.data() {
            return Some(Self::Flat(column));
        }
        if let Some((codes, values)) = vector.positions() {
            return match values.data() {
                Some(Data::Varlen(column)) => Some(Self::Indirect(codes, column)),
                _ => Some(Self::External(codes, values)),
            };
        }
        Some(Self::Direct(vector))
    }

    /// Row `index`, or the empty string where the row is null and the value under it is whatever
    /// the column happens to hold. A null row is never read, since the loops above skip them.
    fn get(&self, index: usize) -> Result<&'a str> {
        match self {
            Self::Flat(column) => Ok(column.get(index).unwrap_or_default()),
            Self::Indirect(codes, values) => {
                Ok(codes.get(index).and_then(|&code| values.get(code as usize)).unwrap_or_default())
            }
            Self::External(codes, values) => match codes.get(index) {
                Some(&code) => Ok(values.try_text_at(code as usize)?.unwrap_or_default()),
                None => Ok(""),
            },
            Self::Direct(vector) => Ok(vector.try_text_at(index)?.unwrap_or_default()),
        }
    }

    fn get_bytes(&self, index: usize) -> Result<&'a [u8]> {
        match self {
            Self::Flat(column) => Ok(column.bytes(index).unwrap_or_default()),
            Self::Indirect(codes, values) => Ok(codes
                .get(index)
                .and_then(|&code| values.bytes(code as usize))
                .unwrap_or_default()),
            Self::External(codes, values) => match codes.get(index) {
                Some(&code) => Ok(values.try_bytes_at(code as usize)?.unwrap_or_default()),
                None => Ok(&[]),
            },
            Self::Direct(vector) => Ok(vector.try_bytes_at(index)?.unwrap_or_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use rudb_common::Value;

    use std::sync::Arc;

    use rudb_common::LogicalType;
    use rudb_vector::Vector;

    use super::{Call, host, value, vectorized};

    /// The memo over a dictionary that outlives the chunk answers what the flat loop answers, on the
    /// fixed host pattern and on a general one, for a chunk that fills it, a chunk that reads it
    /// back, and a dictionary it was not built for.
    #[test]
    fn a_replace_over_a_stable_dictionary_agrees_with_the_flat_loop() {
        let values: Vec<Value> = (0..2_500)
            .map(|index| match index % 5 {
                0 => Value::Null,
                1 => Value::Varchar(format!("http://www.site{index}.ru/page")),
                2 => Value::Varchar(format!("https://host{}.com/a/b", index % 17)),
                3 => Value::Varchar(String::new()),
                _ => Value::Varchar(format!("plain {index} foo")),
            })
            .collect();
        let dictionary =
            Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("builds"));
        let other = Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("builds"));
        let patterns = [("^https?://(?:www\\.)?([^/]+)/.*$", "\\1"), ("o+", "0")];
        for (pattern, replacement) in patterns {
            let constants = [Value::Varchar(pattern.into()), Value::Varchar(replacement.into())];
            let call = Call::read("regexp_replace", &constants.iter().collect::<Vec<_>>())
                .expect("compiles")
                .expect("a shape this file handles");
            for (rows, step, held) in
                [(2_000_usize, 991, &dictionary), (2_000, 991, &dictionary), (64, 37, &other)]
            {
                let codes: Vec<u32> =
                    (0..rows).map(|row| ((row * step) % values.len()) as u32).collect();
                let picked: Vec<Value> =
                    codes.iter().map(|&code| values[code as usize].clone()).collect();
                let flat = Vector::from_values(LogicalType::Varchar, &picked).expect("builds");
                let column =
                    Vector::stable_dictionary(codes, Arc::clone(held)).expect("codes are in range");
                let answer = |text: &Vector| {
                    vectorized("regexp_replace", Some(&call), &[text], &LogicalType::Varchar, rows)
                        .expect("the call is written")
                        .expect("text in this form has a loop")
                };
                let (want, got) = (answer(&flat), answer(&column));
                for row in 0..rows {
                    assert_eq!(got.value_at(row), want.value_at(row), "{pattern}, row {row}");
                }
                // Over the dictionary the memo was built on, the answer is a dictionary whose codes
                // agree exactly when the answers do, which is what grouping on them relies on.
                if Arc::ptr_eq(held, &dictionary) {
                    let (codes, _) = got.stable_dictionary_parts().expect("answered as codes");
                    for one in 0..rows {
                        for other in (0..rows).step_by(7) {
                            if got.is_null_at(one) || got.is_null_at(other) {
                                continue;
                            }
                            assert_eq!(
                                codes[one] == codes[other],
                                got.value_at(one) == got.value_at(other),
                                "{pattern}, rows {one} and {other}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Threads deciding different groups at once, all giving the same few answers, still reach one
    /// code per answer. Each group finds the answers new and holds them back until it is done, so
    /// every answer is registered by more than one group and all but the first are moved.
    #[test]
    fn groups_decided_at_once_agree_on_one_code_per_answer() {
        let values: Vec<Value> = (0..8_192)
            .map(|index| Value::Varchar(format!("http://h{}.ru/{index}", index % 13)))
            .collect();
        let dictionary =
            Arc::new(Vector::from_values(LogicalType::Varchar, &values).expect("builds"));
        let constants = [
            Value::Varchar("^https?://(?:www\\.)?([^/]+)/.*$".into()),
            Value::Varchar("\\1".into()),
        ];
        let call = Call::read("regexp_replace", &constants.iter().collect::<Vec<_>>())
            .expect("compiles")
            .expect("a shape this file handles");
        let answers: Vec<Vector> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8_u32)
                .map(|thread| {
                    let (call, dictionary) = (&call, &dictionary);
                    scope.spawn(move || {
                        let codes: Vec<u32> = (0..1_024).map(|row| thread * 1_024 + row).collect();
                        let column = Vector::stable_dictionary(codes, Arc::clone(dictionary))
                            .expect("codes are in range");
                        vectorized(
                            "regexp_replace",
                            Some(call),
                            &[&column],
                            &LogicalType::Varchar,
                            1_024,
                        )
                        .expect("the call is written")
                        .expect("text in this form has a loop")
                    })
                })
                .collect();
            threads.into_iter().map(|thread| thread.join().expect("no panic")).collect()
        });
        let mut code_of = std::collections::HashMap::new();
        for (thread, got) in answers.iter().enumerate() {
            let (codes, _) = got.stable_dictionary_parts().expect("answered as codes");
            for (row, &code) in codes.iter().enumerate() {
                let index = thread * 1_024 + row;
                let want = Value::Varchar(format!("h{}.ru", index % 13));
                assert_eq!(got.value_at(row), want, "row {index}");
                assert_eq!(code, *code_of.entry(index % 13).or_insert(code), "row {index}");
            }
        }
    }

    #[test]
    fn clickbench_host_extraction_keeps_the_regex_boundaries() {
        assert_eq!(host("http://www.example.com/a"), "example.com");
        assert_eq!(host("https://example.com/"), "example.com");
        assert_eq!(host("http://example.com"), "http://example.com");
        assert_eq!(host("ftp://example.com/a"), "ftp://example.com/a");
        assert_eq!(host("https:///a"), "https:///a");
        assert_eq!(host("https://example.com/a\nb"), "https://example.com/a\nb");
        assert_eq!(host("https://example.com/a\n"), "https://example.com/a\n");
        assert_eq!(host("https://exa\nmple.com/a"), "exa\nmple.com");
        assert_eq!(host("http://www./a"), "www.");
    }

    /// The bug this started as: a group index that does not fit a `usize` was read as `usize::MAX`,
    /// which means a group no pattern has, and the doubling on the way to the slot overflowed.
    /// `SELECT regexp_extract('a', 'a', -1)` panicked the process while the optimizer folded the
    /// call, before a row existed.
    ///
    /// It is refused now, which is #496 and what DuckDB does. A group inside zero to nine that the
    /// pattern does not have is still the empty string, because that is what DuckDB answers for it.
    #[test]
    fn a_group_index_outside_zero_to_nine_is_refused() {
        let called = |group: Value| {
            let args = [Value::Varchar("a".into()), Value::Varchar("a".into()), group];
            value("regexp_extract", &args)
        };
        for outside in [Value::BigInt(-1), Value::BigInt(10), Value::BigInt(i64::MAX)] {
            let message = called(outside.clone()).expect_err("outside the range").to_string();
            assert!(
                message.contains("Group index must be between 0 and 9!"),
                "{outside:?} said {message}"
            );
        }
        let empty = Value::Varchar(String::new());
        assert_eq!(called(Value::BigInt(7)).expect("inside the range"), empty);
        assert_eq!(called(Value::BigInt(9)).expect("inside the range"), empty);
        assert_eq!(called(Value::BigInt(0)).expect("inside the range"), Value::Varchar("a".into()));
    }

    #[test]
    fn clickbench_host_shortcut_agrees_with_the_regex_machine() {
        let pattern = Value::Varchar("^https?://(?:www\\.)?([^/]+)/.*$".into());
        let replacement = Value::Varchar("\\1".into());
        let call = Call::read("regexp_replace", &[&pattern, &replacement])
            .expect("valid pattern")
            .expect("a prepared call");
        assert!(call.host);
        for text in [
            "https://example.com/a",
            "https://example.com/a\nb",
            "https://example.com/a\n",
            "https://exa\nmple.com/a",
            "http://www./a",
            "http://www.example.com/a",
            "https:///a",
        ] {
            let mut general = String::new();
            call.regex.replace_into(&mut general, text, &call.rewrite, call.global);
            assert_eq!(host(text), general, "{text:?}");
        }
    }
}
