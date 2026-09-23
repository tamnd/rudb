//! One CSV file read on many threads.
//!
//! A Parquet file comes cut into row groups and says where each one starts. A CSV file does not,
//! and where a record starts is only known by reading every byte before it, since a newline inside
//! a quoted field is part of a value and not the end of a line. Read that way a large file is read
//! on one thread however many the query was given.
//!
//! So the file is cut into ranges of about [`RANGE`] bytes and each range guesses. A range that
//! is not the first assumes its first record starts just after the first line ending in it, which
//! is right unless that line ending is inside a quoted field, and it splits records from there to
//! learn where its last record ends. That is where the next range really starts if the guess was
//! right, so the true starts are known a range at a time as fast as the guesses can be checked,
//! and a range converts nothing until its own start is one of them. Converting is most of the
//! work, and it is done once, from the right place, on every thread at the same time.
//!
//! A wrong guess costs a second split of that range from its true start, which is also what a
//! range that nobody guessed for gets, so a guess only ever saves time. A range whose owner is
//! slow to guess is split by whichever thread is waiting on it, which also means no range can be
//! left waiting for one that is never read.
//!
//! Errors are where the ranges have to agree with one thread exactly. A value that does not fit
//! its column is reported with its line number, which is a count of the records before it, and
//! when several ranges fail at once the one reported has to be the first a single thread would
//! have met. A range that fails therefore does not report its own error. The file is read again
//! on one thread from the end of the last range that converted without one, in the same chunks a
//! whole read makes, and the first error that read meets is the one every range reports. That is
//! slow, and it only happens to a query that is failing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use rudb_common::{Error, Result};
use rudb_vector::Chunk;

use crate::Reader;

/// How many bytes of the file a range covers.
///
/// Small enough that a few hundred megabytes gives every thread several ranges, so a thread that
/// is slowed by something else does not leave the rest waiting on its last range. Large enough
/// that the one record each range finishes past its end, and the guess at where it starts, are
/// nothing next to the range, and that a range of TPC-H lineitem is about the 131,072 rows a
/// native stripe holds, so ranges do not leave many short stripes behind them.
pub const RANGE: u64 = 16 << 20;

/// The range size [`size`] answers, which only tests change.
static SIZE: AtomicU64 = AtomicU64::new(RANGE);

/// How many bytes of a file a range covers, which is [`RANGE`] unless a test said otherwise.
#[must_use]
pub fn size() -> u64 {
    SIZE.load(Ordering::Relaxed)
}

/// Makes every later split use ranges of `bytes`, so that a test can cut a small file into many.
///
/// This is for the whole process, which is why a test that calls it lives in a test binary of its
/// own.
#[doc(hidden)]
pub fn set_size(bytes: u64) {
    SIZE.store(bytes.max(1), Ordering::Relaxed);
}

/// How much of the file is looked at a time for the line ending a guess starts after.
const LOOK: usize = 64 << 10;

impl Reader {
    /// How many ranges of about `size` bytes the rest of the file is read in, which is one for a
    /// file that is not at least two of them long.
    #[must_use]
    pub fn ranges(&self, size: u64) -> usize {
        let Ok(length) = self.file().len() else { return 1 };
        let rest = length.saturating_sub(self.here());
        if size == 0 || rest / 2 < size {
            return 1;
        }
        usize::try_from(rest.div_ceil(size)).unwrap_or(1)
    }
}

/// A file cut into ranges that are read on separate threads and come back in file order.
///
/// Each range is read through a [`Part`], and the rows of part 0, then part 1 and so on are the
/// rows a single [`Reader`] would have handed back, in the same order.
#[derive(Debug)]
pub struct Split {
    /// A reader positioned at the first record, which every range's reader is copied from.
    base: Reader,
    /// Where each range starts as a count of bytes, not as a record.
    starts: Vec<u64>,
    /// How long a range is, which is also how far past its end a guess is followed.
    length: u64,
    chain: Mutex<Chain>,
    moved: Condvar,
    failure: OnceLock<Error>,
}

/// What is known so far about where the ranges really start.
#[derive(Debug)]
struct Chain {
    /// The true start of each range from the first, as far as it is known.
    known: Vec<u64>,
    /// For each range that has guessed, where it guessed it starts and where that guess ends.
    guessed: Vec<Option<(u64, u64)>>,
    /// Whether some thread is splitting the range from its true start to learn the next one.
    taken: Vec<bool>,
    /// Whether the range has handed back all of its rows without an error.
    clean: Vec<bool>,
}

impl Chain {
    /// Takes every guess that turned out to start where the range before it ended.
    fn settle(&mut self) {
        while self.known.len() < self.guessed.len() {
            let last = self.known.len() - 1;
            match self.guessed[last] {
                Some((guess, stop)) if guess == self.known[last] => self.known.push(stop),
                _ => break,
            }
        }
    }
}

impl Split {
    /// Cuts what is left of `reader`'s file into `ranges` ranges of about the same size.
    ///
    /// A file whose length cannot be read is one range, which is the whole file read on one thread
    /// and is right whatever the file holds.
    #[must_use]
    pub fn new(reader: Reader, ranges: usize) -> Self {
        let first = reader.here();
        let length = reader.file().len().ok();
        let rest = length.unwrap_or(first).saturating_sub(first);
        let count = if length.is_some() { ranges.max(1) } else { 1 };
        let each = rest / count as u64;
        let starts = (0..count).map(|at| first + each * at as u64).collect();
        let mut known = Vec::with_capacity(count);
        known.push(first);
        let chain = Chain {
            known,
            guessed: vec![None; count],
            taken: vec![false; count],
            clean: vec![false; count],
        };
        Self {
            base: reader.stretch(first, u64::MAX, reader.line()),
            starts,
            length: each.max(1),
            chain: Mutex::new(chain),
            moved: Condvar::new(),
            failure: OnceLock::new(),
        }
    }

    /// How many ranges the file was cut into.
    #[must_use]
    pub fn ranges(&self) -> usize {
        self.starts.len()
    }

    /// The reader of range `index`, which reads nothing until it is first asked for a chunk.
    #[must_use]
    pub fn part(self: &Arc<Self>, index: usize) -> Part {
        Part { split: Arc::clone(self), index, reader: None, finished: false }
    }

    /// Where range `index` stops taking records, which for the last one is the end of the file.
    fn end(&self, index: usize) -> u64 {
        self.starts.get(index + 1).copied().unwrap_or(u64::MAX)
    }

    fn lock(&self) -> MutexGuard<'_, Chain> {
        self.chain.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Works out where range `index` really starts and hands back a reader of it.
    fn begin(&self, index: usize) -> Result<Reader> {
        let clock = Instant::now();
        if self.lock().known.len() <= index {
            self.speculate(index);
        }
        // Waiting as long as this thread's own guess took is about how long the range before it
        // takes to guess, so a thread only splits somebody else's range when that one is late.
        let from = self.wait(index, clock.elapsed())?;
        // A guess that was wrong ends in the wrong place too, so whoever reads the next range is
        // waiting on this one to be split from where it really starts.
        self.learn(index)?;
        Ok(self.base.stretch(from, self.end(index), 0))
    }

    /// Guesses where range `index` starts, follows the guess to where it ends, and writes both
    /// down.
    ///
    /// Nothing that goes wrong here is an error, because a guess that fails was a wrong guess and
    /// the range is split again from where it really starts, which reports the error if it is real.
    fn speculate(&self, index: usize) {
        let guess = if index == 0 { self.starts[0] } else { self.guess(index) };
        let end = self.end(index);
        let mut reader = self.base.stretch(guess, end, 0);
        reader.give_up_at(end.saturating_add(self.length));
        let Ok(stop) = reader.skim() else { return };
        let mut chain = self.lock();
        chain.guessed[index] = Some((guess, stop));
        chain.settle();
        self.moved.notify_all();
    }

    /// Where the first record of range `index` would start if no line ending near it is inside a
    /// quoted field, which is just after the first line ending that finishes in the range.
    ///
    /// A range with no line ending in it guesses that it holds no records, which is right for a
    /// range inside one enormous record and is checked like any other guess.
    fn guess(&self, index: usize) -> u64 {
        let end = self.end(index);
        let mut at = self.starts[index].saturating_sub(1);
        let mut bytes = vec![0; LOOK];
        while at < end {
            let Ok(read) = self.base.file().read_at(at, &mut bytes) else { return end };
            if read == 0 {
                return end;
            }
            if let Some(found) = bytes[..read].iter().position(|&b| b == b'\n' || b == b'\r') {
                let line = at + found as u64;
                if bytes[found] == b'\r' {
                    let mut next = [0];
                    let read = self.base.file().read_at(line + 1, &mut next).unwrap_or(0);
                    if read == 1 && next[0] == b'\n' {
                        return line + 2;
                    }
                }
                return line + 1;
            }
            at += read as u64;
        }
        end
    }

    /// Waits until the true start of range `index` is known and answers it.
    ///
    /// A wait that runs out splits the first range whose end is not known, from its true start,
    /// unless some thread is doing that already. The one it helps is at or before `index`, so every
    /// wait that runs out moves the known starts on by a range, and a range that no thread ever
    /// reads cannot hold up the ones after it.
    fn wait(&self, index: usize, patience: Duration) -> Result<u64> {
        let patience = patience.max(Duration::from_millis(1));
        let mut chain = self.lock();
        loop {
            if let Some(error) = self.failure.get() {
                return Err(error.clone());
            }
            if let Some(&from) = chain.known.get(index) {
                return Ok(from);
            }
            let (guard, waited) =
                self.moved.wait_timeout(chain, patience).unwrap_or_else(PoisonError::into_inner);
            chain = guard;
            if waited.timed_out() && chain.known.len() <= index {
                let frontier = chain.known.len() - 1;
                if !chain.taken[frontier] {
                    drop(chain);
                    self.learn(frontier)?;
                    chain = self.lock();
                }
            }
        }
    }

    /// Splits range `index` from its true start to learn where the next one starts, unless that is
    /// known already or another thread is on it.
    ///
    /// This is a range read from where it really starts, so an error here is a real one.
    fn learn(&self, index: usize) -> Result<()> {
        let from = {
            let mut chain = self.lock();
            if chain.known.len() != index + 1 || index + 1 == self.ranges() || chain.taken[index] {
                return Ok(());
            }
            chain.taken[index] = true;
            chain.known[index]
        };
        match self.base.stretch(from, self.end(index), 0).skim() {
            Ok(stop) => {
                let mut chain = self.lock();
                if chain.known.len() == index + 1 {
                    chain.known.push(stop);
                    chain.settle();
                }
                self.moved.notify_all();
                Ok(())
            }
            Err(found) => Err(self.fail(found)),
        }
    }

    /// Writes down that range `index` handed back all of its rows and stopped at `stop`.
    fn finish(&self, index: usize, stop: u64) {
        let mut chain = self.lock();
        chain.clean[index] = true;
        if chain.known.len() == index + 1 && index + 1 < self.ranges() {
            chain.known.push(stop);
            chain.settle();
            self.moved.notify_all();
        }
    }

    /// The error a single thread reading the whole file would have reported, given that some range
    /// met `found`.
    fn fail(&self, found: Error) -> Error {
        let error = self.failure.get_or_init(|| self.replay(found)).clone();
        let _chain = self.lock();
        self.moved.notify_all();
        error
    }

    /// Reads the file again on one thread from the end of the ranges that converted cleanly, in the
    /// chunks a whole read makes, and answers the first error it meets.
    ///
    /// The ranges before that point handed back every row without an error, so a whole read gets
    /// through them too and its first error is at or after it. `found` is what is reported if the
    /// read again somehow finds nothing, since some range did fail.
    fn replay(&self, found: Error) -> Error {
        let target = {
            let chain = self.lock();
            let clean = chain.clean.iter().take_while(|&&clean| clean).count();
            chain.known.get(clean).copied().unwrap_or(u64::MAX)
        };
        let mut reader = self.base.stretch(self.starts[0], u64::MAX, self.base.line());
        if let Err(error) = reader.skip_to(target) {
            return error;
        }
        loop {
            match reader.next_chunk() {
                Ok(Some(_)) => {}
                Ok(None) => return found,
                Err(error) => return error,
            }
        }
    }
}

/// One range of a [`Split`], read a chunk at a time like a whole [`Reader`].
#[derive(Debug)]
pub struct Part {
    split: Arc<Split>,
    index: usize,
    reader: Option<Reader>,
    finished: bool,
}

impl Part {
    /// The next chunk of this range, or `None` once it has handed back every record that starts in
    /// it.
    ///
    /// The first call waits until the range's true start is known, which usually means until the
    /// range before it has guessed.
    ///
    /// # Errors
    ///
    /// The error a single [`Reader`] over the whole file would have reported first, whichever range
    /// it is in, once any range meets one.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if let Some(error) = self.split.failure.get() {
            return Err(error.clone());
        }
        if self.finished {
            return Ok(None);
        }
        let reader = match &mut self.reader {
            Some(reader) => reader,
            None => self.reader.insert(self.split.begin(self.index)?),
        };
        match reader.next_chunk() {
            Ok(Some(chunk)) => Ok(Some(chunk)),
            Ok(None) => {
                self.finished = true;
                self.split.finish(self.index, reader.here());
                Ok(None)
            }
            Err(found) => Err(self.split.fail(found)),
        }
    }

    /// How many bytes of the file this range's reader has read.
    ///
    /// The guess and a second split after a wrong one read the range too, and are not counted,
    /// so that the ranges of a file add up to about the file.
    #[must_use]
    pub fn bytes_read(&self) -> u64 {
        self.reader.as_ref().map_or(0, Reader::bytes_read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rudb_common::LogicalType;
    use rudb_io::{Filesystem, OpenMode, SimFilesystem};
    use std::path::Path;

    /// A small generator, since the crate has no dependencies to take one from.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A file whose ranges are hard to guess: quoted fields with line endings and delimiters in
    /// them, all three line endings, empty lines, sometimes a header, and sometimes no line ending
    /// on the last line. Each column holds one kind of value, answered as its type, and now and
    /// then a value that kind cannot take or a quote with rubbish after it, so that a read at those
    /// types fails somewhere in the middle of the file.
    fn file(rng: &mut Rng) -> (Vec<u8>, Vec<LogicalType>) {
        const TEXT: [&str; 12] = [
            "plain text",
            "",
            "\"a,b\"",
            "\"one\ntwo\"",
            "\"\r\n\"",
            "\"\n\n\n\"",
            "\"q\"\"\n\"\"q\"",
            "\"\"",
            "\"x\ry\"",
            "\"a long quoted value that runs, over a line\nand then some more\"",
            "\"1\n2,3\n\"",
            "x",
        ];
        const ODD: [&str; 5] = ["abc", "1.5.5", "2020-13-01", "\"x\"y", "maybe"];
        let width = 1 + rng.below(5);
        let kinds: Vec<usize> = (0..width).map(|_| rng.below(5)).collect();
        let most = if rng.below(10) == 0 { 3000 } else { 300 };
        let rows = rng.below(most);
        let mut text = String::new();
        if rng.below(2) == 0 {
            let names: Vec<String> = (0..width).map(|at| format!("c{at}")).collect();
            text.push_str(&names.join(","));
            text.push('\n');
        }
        let ending = ["\n", "\r\n", "\r"][rng.below(3)];
        for _ in 0..rows {
            if rng.below(40) == 0 {
                text.push_str(ending);
                continue;
            }
            let fields: Vec<String> = kinds
                .iter()
                .map(|&kind| {
                    let n = rng.next();
                    if rng.below(2000) == 0 {
                        return ODD[rng.below(ODD.len())].to_string();
                    }
                    match kind {
                        0 => format!("{}", (n % 2001) as i64 - 1000),
                        1 => format!("\"{}.{:02}\"", n % 1000, n % 100),
                        2 => format!("{}-{:02}-{:02}", 1990 + n % 20, 1 + n % 12, 1 + n % 28),
                        3 => ["true", "false", ""][(n % 3) as usize].to_string(),
                        _ => TEXT[(n % TEXT.len() as u64) as usize].to_string(),
                    }
                })
                .collect();
            text.push_str(&fields.join(","));
            text.push_str(if rng.below(30) == 0 { "\r\n" } else { ending });
        }
        if rng.below(4) == 0 {
            text.pop();
        }
        let types = kinds
            .iter()
            .map(|&kind| match kind {
                0 => LogicalType::BigInt,
                1 => LogicalType::Double,
                2 => LogicalType::Date,
                3 => LogicalType::Boolean,
                _ => LogicalType::Varchar,
            })
            .collect();
        (text.into_bytes(), types)
    }

    fn open(text: &[u8], block: usize) -> Result<Reader> {
        let filesystem = SimFilesystem::new();
        let path = Path::new("/t.csv");
        let file = filesystem.open(path, OpenMode::Create).expect("creates");
        file.write_at(0, text).expect("writes");
        drop(file);
        let file = filesystem.open(path, OpenMode::Read).expect("opens");
        Reader::open_sized(file, "/t.csv", crate::Given::default(), block)
    }

    /// Every row as the debug text of its values, so that a NaN equals itself, and the error the
    /// read stopped on.
    type Read = (Vec<String>, Option<String>);

    /// What a read that pulls chunks from `next` until it stops comes to.
    fn rows(mut next: impl FnMut() -> Result<Option<Chunk>>) -> Read {
        let mut rows = Vec::new();
        loop {
            match next() {
                Ok(Some(chunk)) => {
                    for row in 0..chunk.len() {
                        let values: Vec<_> =
                            (0..chunk.width()).map(|at| chunk.value_at(row, at)).collect();
                        rows.push(format!("{values:?}"));
                    }
                }
                Ok(None) => return (rows, None),
                Err(error) => return (rows, Some(error.to_string())),
            }
        }
    }

    /// Opens the file twice, read at the types its columns were written as or at what the sniffer
    /// made of them, and sometimes projected, and answers the reader to split and what one reader
    /// over it gives.
    fn readers(
        rng: &mut Rng,
        text: &[u8],
        types: &[LogicalType],
        block: usize,
    ) -> Option<(Reader, Read)> {
        let (Ok(mut whole), Ok(mut split)) = (open(text, block), open(text, block)) else {
            return None;
        };
        let width = whole.fields().len();
        if width == types.len() && rng.below(4) > 0 {
            let columns: Vec<usize> = if rng.below(2) == 0 {
                (0..width).collect()
            } else {
                (0..1 + rng.below(width)).map(|_| rng.below(width)).collect()
            };
            let wanted: Vec<LogicalType> = columns.iter().map(|&at| types[at].clone()).collect();
            for reader in [&mut whole, &mut split] {
                reader.project(&columns).expect("projects");
                reader.retype(&wanted).expect("retypes");
            }
        }
        let expected = rows(|| whole.next_chunk());
        Some((split, expected))
    }

    /// Compares what the parts gave, in part order, with what one reader gave: the same rows when
    /// it read the whole file, and the same error from every part that failed when it did not.
    fn check(parts: Vec<Read>, expected: &Read) {
        let failures: Vec<&String> = parts.iter().filter_map(|part| part.1.as_ref()).collect();
        match &expected.1 {
            None => {
                assert!(failures.is_empty(), "{failures:?}");
                let found: Vec<String> = parts.into_iter().flat_map(|part| part.0).collect();
                assert_eq!(found.len(), expected.0.len());
                assert_eq!(&found, &expected.0);
            }
            Some(error) => {
                assert!(!failures.is_empty(), "no part failed, expected {error}");
                for failure in failures {
                    assert_eq!(failure, error);
                }
            }
        }
    }

    /// Split files read on a thread per part come back with the rows and the errors of one reader,
    /// whatever the ranges cut through.
    #[test]
    fn parts_on_many_threads_read_the_same_as_one_reader() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..400 {
            let (text, types) = file(&mut rng);
            let block = if rng.below(2) == 0 { 1 << 20 } else { 16 + rng.below(300) };
            let Some((reader, expected)) = readers(&mut rng, &text, &types, block) else {
                continue;
            };
            let split = Arc::new(Split::new(reader, 1 + rng.below(40)));
            let parts = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..split.ranges())
                    .map(|index| {
                        let mut part = split.part(index);
                        scope.spawn(move || rows(|| part.next_chunk()))
                    })
                    .collect();
                handles.into_iter().map(|handle| handle.join().expect("joins")).collect()
            });
            check(parts, &expected);
        }
    }

    /// Parts read one after another in any order, on one thread, which is a pipeline whose
    /// threads are all busy elsewhere. Each part waits, gives up waiting, and splits the ranges
    /// before it itself, so it still finishes and still agrees.
    #[test]
    fn parts_read_in_any_order_on_one_thread_read_the_same() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..100 {
            let (text, types) = file(&mut rng);
            let block = 16 + rng.below(300);
            let Some((reader, expected)) = readers(&mut rng, &text, &types, block) else {
                continue;
            };
            let split = Arc::new(Split::new(reader, 1 + rng.below(12)));
            let mut order: Vec<usize> = (0..split.ranges()).collect();
            for at in (1..order.len()).rev() {
                order.swap(at, rng.below(at + 1));
            }
            let mut parts = vec![(Vec::new(), None); split.ranges()];
            for index in order {
                let mut part = split.part(index);
                parts[index] = rows(|| part.next_chunk());
            }
            check(parts, &expected);
        }
    }

    /// A part whose range comes after ranges nobody reads still finishes, which is what happens
    /// when a query fails between being handed a range and reading it.
    #[test]
    fn a_part_finishes_when_the_ranges_before_it_are_never_read() {
        let mut text = String::from("a,b\n");
        for row in 0..2000 {
            text.push_str(&format!("{row},\"x\ny\"\n"));
        }
        let reader = open(text.as_bytes(), 1 << 20).expect("opens");
        let split = Arc::new(Split::new(reader, 10));
        let mut part = split.part(9);
        let (rows, error) = rows(|| part.next_chunk());
        assert_eq!(error, None);
        assert!(!rows.is_empty());
        assert!(rows.len() < 2000);
    }

    /// A value that fails in a late range is reported with the line a single reader gives it, and
    /// every range that fails reports the same one.
    #[test]
    fn an_error_in_a_late_range_names_the_line_one_reader_names() {
        let mut text = String::from("n\n");
        for row in 0..50_000 {
            text.push_str(if row == 41_234 { "oops\n" } else { "12\n" });
        }
        let mut whole = open(text.as_bytes(), 4096).expect("opens");
        whole.retype(&[LogicalType::Integer]).expect("retypes");
        let expected = rows(|| whole.next_chunk());
        let error = expected.1.clone().expect("fails");
        assert!(error.contains("Line: 41236"), "{error}");
        let mut reader = open(text.as_bytes(), 4096).expect("opens");
        reader.retype(&[LogicalType::Integer]).expect("retypes");
        let split = Arc::new(Split::new(reader, 7));
        let parts: Vec<_> = (0..7)
            .map(|index| {
                let mut part = split.part(index);
                rows(|| part.next_chunk())
            })
            .collect();
        check(parts, &expected);
    }

    /// A file too short for two ranges is read as one, and a longer one is cut about evenly.
    #[test]
    fn a_file_is_cut_into_ranges_only_when_it_is_long_enough() {
        let text = "a,b\n".to_string() + &"1,2\n".repeat(1000);
        let reader = open(text.as_bytes(), 1 << 20).expect("opens");
        assert_eq!(reader.ranges(4000), 1);
        assert_eq!(reader.ranges(2000), 2);
        assert_eq!(reader.ranges(1000), 4);
        assert_eq!(reader.ranges(0), 1);
    }
}
