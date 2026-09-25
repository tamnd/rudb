//! The device card: what a sync costs on the device a directory is on, measured rather than
//! assumed.
//!
//! `notes/Spec/2140/engine-v4/16-measurement.md` section 16.3 says how it is measured and
//! `09-the-log.md` section 9.2 says what it is for, which is choosing the sync call and the number
//! of log lanes. It is also the thing every durable number in a report is stated next to. A commit
//! latency means nothing until it says what sync it paid for: on the M4 this was written on a plain
//! `fsync` returns in 24 µs and does not reach the flash, `F_FULLFSYNC` takes 3,347 µs and
//! `F_BARRIERFSYNC` 263 µs, and three engines that each picked one of those would report three
//! numbers a hundred times apart for the same work.
//!
//! # The probes
//!
//! All of them run on scratch files in the directory being asked about, which are removed after.
//!
//! 1. For every sync call the platform has, a 4 KiB `pwrite` at a sequential offset followed by that
//!    call, timed one iteration at a time. The p50 and p99 go on the card.
//! 2. The same at 64 KiB.
//! 3. Sequential write bandwidth: 256 MiB in 1 MiB writes, then one sync, timed as a whole.
//! 4. Parallel sync scaling: 1, 2, 4 and 8 threads, each writing and syncing its own file for a
//!    fixed window, in syncs a second. A device that scales has independent flush queues and log
//!    lanes help on it. One that does not gains nothing from them.
//! 5. A plausibility check. A sync that returns in under 10 µs is marked implausible, because no
//!    flash device commits a write to media that fast, and the ones with power-loss protection
//!    take about 30 µs. A memory-backed file system is implausible whatever it measures.
//!
//! # What it does not know
//!
//! Whether the drive has power-loss protection is read from `/sys/block/<disk>/queue/write_cache`
//! on Linux, where `write through` means the drive reports no volatile cache. Under a hypervisor
//! that file describes the virtual disk and not the one underneath, so a machine whose processor
//! says it is virtualised reports `unknown`, and macOS always does, because it has no reliable way
//! to ask. `09-the-log.md` only gives more than one lane to a card that says `yes`.
//!
//! # Why it does not go through [`crate::Filesystem`]
//!
//! The crate rule is that every file goes through the shim, and this is the one place that does
//! not. The card measures the device, so a simulated file system has nothing to tell it, and the
//! calls it compares (`F_BARRIERFSYNC`, `RWF_DSYNC`) are not in the shim's interface and should not
//! be until the log picks one. The scratch files are its own and are removed before it returns.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rudb_common::{Error, Result};

/// A sync under this many nanoseconds did not reach the media.
pub const IMPLAUSIBLE_NS: u64 = 10_000;

/// The thread counts the scaling probe runs at.
pub const SCALING_THREADS: [usize; 4] = [1, 2, 4, 8];

/// A way to make a write durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyncCall {
    /// `fsync(2)`. On Linux it flushes the drive cache. On macOS it hands the data to the drive and
    /// returns, which is why it is fast there.
    Fsync,
    /// `fdatasync(2)`, which skips the inode when the file size has not changed. Linux only.
    Fdatasync,
    /// `fcntl(F_FULLFSYNC)`, which on macOS is the call that asks the drive to flush its cache.
    FullFsync,
    /// `fcntl(F_BARRIERFSYNC)`, which on macOS orders the write before every later one without
    /// waiting for the flush.
    BarrierFsync,
    /// `pwritev2` with `RWF_DSYNC`, a write that is durable when it returns. Linux only.
    DsyncWrite,
    /// `FlushFileBuffers`, which is what Windows has.
    FlushFileBuffers,
}

impl SyncCall {
    /// The name the card prints, which is the name of the system call.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fsync => "fsync",
            Self::Fdatasync => "fdatasync",
            Self::FullFsync => "F_FULLFSYNC",
            Self::BarrierFsync => "F_BARRIERFSYNC",
            Self::DsyncWrite => "RWF_DSYNC",
            Self::FlushFileBuffers => "FlushFileBuffers",
        }
    }

    /// Every call this platform has, the one a full commit uses first.
    #[must_use]
    pub fn candidates() -> &'static [SyncCall] {
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            &[Self::FullFsync, Self::BarrierFsync, Self::Fsync]
        } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
            &[Self::Fdatasync, Self::Fsync, Self::DsyncWrite]
        } else if cfg!(target_os = "linux") {
            &[Self::Fdatasync, Self::Fsync]
        } else if cfg!(windows) {
            &[Self::FlushFileBuffers]
        } else {
            &[Self::Fsync]
        }
    }

    /// The byte a kept card stores for this call.
    const fn tag(self) -> u8 {
        match self {
            Self::Fsync => 0,
            Self::Fdatasync => 1,
            Self::FullFsync => 2,
            Self::BarrierFsync => 3,
            Self::DsyncWrite => 4,
            Self::FlushFileBuffers => 5,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        Ok(match tag {
            0 => Self::Fsync,
            1 => Self::Fdatasync,
            2 => Self::FullFsync,
            3 => Self::BarrierFsync,
            4 => Self::DsyncWrite,
            5 => Self::FlushFileBuffers,
            _ => return Err(Error::io(format!("device card: no sync call has tag {tag}"))),
        })
    }

    /// The call `commit_sync = full` uses on this platform.
    #[must_use]
    pub fn full() -> SyncCall {
        Self::candidates()[0]
    }
}

/// Whether the drive keeps acknowledged writes through a power loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plp {
    /// The drive reports no volatile write cache.
    Yes,
    /// The drive reports a volatile write cache.
    No,
    /// Nothing reliable says either way.
    Unknown,
}

impl Plp {
    /// `yes`, `no` or `unknown`, as the card prints it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

impl Plp {
    const fn tag(self) -> u8 {
        match self {
            Self::Yes => 0,
            Self::No => 1,
            Self::Unknown => 2,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        Ok(match tag {
            0 => Self::Yes,
            1 => Self::No,
            2 => Self::Unknown,
            _ => return Err(Error::io(format!("device card: no PLP answer has tag {tag}"))),
        })
    }
}

/// What one sync call cost.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncProbe {
    /// The call.
    pub call: SyncCall,
    /// Median and 99th percentile of a 4 KiB write plus the call, in nanoseconds.
    pub p50_4k_ns: u64,
    /// See [`Self::p50_4k_ns`].
    pub p99_4k_ns: u64,
    /// The same at 64 KiB.
    pub p50_64k_ns: u64,
    /// See [`Self::p50_64k_ns`].
    pub p99_64k_ns: u64,
    /// Whether the 4 KiB median is slow enough to have reached the media, and the file system is
    /// not in memory.
    pub plausible: bool,
}

/// The card for one directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    /// The directory that was probed, as it was given.
    pub path: PathBuf,
    /// The block device or volume it is on, as the operating system names it, or empty.
    pub device: String,
    /// The file system type, or empty when the platform would not say.
    pub filesystem: String,
    /// Whether that file system keeps its files in memory, which makes every sync on it a no-op.
    pub memory_backed: bool,
    /// One entry per call in [`SyncCall::candidates`], in that order.
    pub probes: Vec<SyncProbe>,
    /// Sequential write bandwidth in bytes a second, the final sync included.
    pub write_bytes_per_s: u64,
    /// Syncs a second at each of [`SCALING_THREADS`].
    pub syncs_per_s: [u64; 4],
    /// Syncs a second at 8 threads over 8 times the rate at one. 1.0 is perfect scaling.
    pub scaling: f64,
    /// Power-loss protection.
    pub plp: Plp,
    /// The log lane count the rule in `09-the-log.md` section 9.2 picks for this card.
    pub lanes: u32,
    /// How many timed iterations each sync probe ran.
    pub iterations: u32,
}

impl Card {
    /// The probe for the call a full commit uses.
    #[must_use]
    pub fn full(&self) -> &SyncProbe {
        &self.probes[0]
    }

    /// Whether a durable number measured on this device can be called durable.
    #[must_use]
    pub fn plausible(&self) -> bool {
        self.full().plausible
    }

    /// The card as bytes, for a database file to keep so that a later process does not measure
    /// the device again. The path is left out, since the card is about the device and the reader
    /// knows which directory it asked about.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![CARD_VERSION];
        let text = |out: &mut Vec<u8>, text: &str| {
            let bytes = &text.as_bytes()[..text.len().min(usize::from(u16::MAX))];
            out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(bytes);
        };
        text(&mut out, &self.device);
        text(&mut out, &self.filesystem);
        out.push(u8::from(self.memory_backed));
        out.push(u8::try_from(self.probes.len()).unwrap_or(u8::MAX));
        for probe in self.probes.iter().take(usize::from(u8::MAX)) {
            out.push(probe.call.tag());
            for ns in [probe.p50_4k_ns, probe.p99_4k_ns, probe.p50_64k_ns, probe.p99_64k_ns] {
                out.extend_from_slice(&ns.to_le_bytes());
            }
            out.push(u8::from(probe.plausible));
        }
        out.extend_from_slice(&self.write_bytes_per_s.to_le_bytes());
        for rate in self.syncs_per_s {
            out.extend_from_slice(&rate.to_le_bytes());
        }
        out.extend_from_slice(&self.scaling.to_le_bytes());
        out.push(self.plp.tag());
        out.extend_from_slice(&self.lanes.to_le_bytes());
        out.extend_from_slice(&self.iterations.to_le_bytes());
        out
    }

    /// A card [`Card::encode`] wrote, as measured for `path`.
    ///
    /// # Errors
    ///
    /// When the bytes are not a card this build wrote, which the caller treats as no card at all.
    pub fn decode(bytes: &[u8], path: &Path) -> Result<Card> {
        let mut read = Read(bytes);
        if read.byte()? != CARD_VERSION {
            return Err(Error::io("device card: the kept card is from another build"));
        }
        let device = read.text()?;
        let filesystem = read.text()?;
        let memory_backed = read.byte()? != 0;
        let count = read.byte()?;
        let mut probes = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let call = SyncCall::from_tag(read.byte()?)?;
            let p50_4k_ns = read.u64()?;
            let p99_4k_ns = read.u64()?;
            let p50_64k_ns = read.u64()?;
            let p99_64k_ns = read.u64()?;
            let plausible = read.byte()? != 0;
            probes.push(SyncProbe {
                call,
                p50_4k_ns,
                p99_4k_ns,
                p50_64k_ns,
                p99_64k_ns,
                plausible,
            });
        }
        if probes.is_empty() {
            return Err(Error::io("device card: the kept card has no probes"));
        }
        let write_bytes_per_s = read.u64()?;
        let mut syncs_per_s = [0; 4];
        for rate in &mut syncs_per_s {
            *rate = read.u64()?;
        }
        let scaling = f64::from_bits(read.u64()?);
        let plp = Plp::from_tag(read.byte()?)?;
        let lanes = read.u32()?;
        let iterations = read.u32()?;
        if !read.0.is_empty() {
            return Err(Error::io("device card: the kept card has bytes after its end"));
        }
        Ok(Card {
            path: path.to_path_buf(),
            device,
            filesystem,
            memory_backed,
            probes,
            write_bytes_per_s,
            syncs_per_s,
            scaling,
            plp,
            lanes,
            iterations,
        })
    }
}

/// Reads an encoded card front to back.
struct Read<'a>(&'a [u8]);

impl<'a> Read<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let (head, tail) = self
            .0
            .split_at_checked(n)
            .ok_or_else(|| Error::io("device card: the kept card is cut short"))?;
        self.0 = tail;
        Ok(head)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    fn text(&mut self) -> Result<String> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().expect("two bytes"));
        String::from_utf8(self.take(usize::from(len))?.to_vec())
            .map_err(|_| Error::io("device card: the kept card has a name that is not text"))
    }
}

/// The first byte of an encoded card, so a later build that changes what a card holds can tell an
/// old one apart and measure again rather than misread it.
const CARD_VERSION: u8 = 1;

/// How hard to probe.
#[derive(Debug, Clone)]
pub struct Options {
    /// Timed iterations per sync call and size. 200 is about a second on NVMe and the spec's
    /// figures for the Mac were taken at 2,000.
    pub iterations: u32,
    /// Bytes the bandwidth probe writes, in 1 MiB writes.
    pub bandwidth_bytes: u64,
    /// How long each step of the scaling probe runs.
    pub scaling_window: Duration,
    /// How many workers the lane rule can give lanes to.
    pub workers: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            iterations: 200,
            bandwidth_bytes: 256 << 20,
            scaling_window: Duration::from_millis(200),
            workers: crate::execution_cores(),
        }
    }
}

/// The size of the scratch file the sync probes write into, preallocated so that a write never
/// changes the file's size and `fdatasync` never has to write an inode.
const SCRATCH: u64 = 64 << 20;

/// Measure the card for `dir`.
///
/// # Errors
///
/// When `dir` is not a directory that can be written to, or a write or a sync fails.
pub fn measure(dir: &Path, options: &Options) -> Result<Card> {
    if !dir.is_dir() {
        return Err(Error::io(format!("device card: {} is not a directory", dir.display())));
    }
    let (device, filesystem) = platform::mount(dir);
    let memory_backed = matches!(filesystem.as_str(), "tmpfs" | "ramfs" | "devtmpfs");
    let scratch = Scratch::new(dir, "sync")?;
    let file = scratch.preallocated(SCRATCH)?;
    let iterations = options.iterations.max(1);
    let mut probes = Vec::new();
    for &call in SyncCall::candidates() {
        let small = timed(&file, call, 4 << 10, iterations)?;
        let large = timed(&file, call, 64 << 10, iterations)?;
        let p50_4k_ns = quantile(&small, 0.5);
        probes.push(SyncProbe {
            call,
            p50_4k_ns,
            p99_4k_ns: quantile(&small, 0.99),
            p50_64k_ns: quantile(&large, 0.5),
            p99_64k_ns: quantile(&large, 0.99),
            plausible: !memory_backed && p50_4k_ns >= IMPLAUSIBLE_NS,
        });
    }
    drop(file);
    let write_bytes_per_s = bandwidth(dir, options.bandwidth_bytes)?;
    let mut syncs_per_s = [0; 4];
    for (slot, threads) in syncs_per_s.iter_mut().zip(SCALING_THREADS) {
        *slot = parallel(dir, threads, options.scaling_window)?;
    }
    let scaling = ratio(syncs_per_s[3], syncs_per_s[0].saturating_mul(8));
    let plp = platform::plp(&device);
    let lanes = lanes(probes[0].p50_4k_ns, plp, scaling, options.workers);
    Ok(Card {
        path: dir.to_path_buf(),
        device,
        filesystem,
        memory_backed,
        probes,
        write_bytes_per_s,
        syncs_per_s,
        scaling,
        plp,
        lanes,
        iterations,
    })
}

/// The card for `dir`, measured once per device per process.
///
/// Probing takes a second or two and writes a few hundred megabytes, so the answer is kept and
/// every later call on the same device gets it back. The key is the device the directory is on
/// rather than the path, because two databases on one drive pay the same for a sync. `iterations`
/// set means the caller wants a fresh measurement at that count, and that result replaces the kept
/// one.
///
/// # Errors
///
/// Whatever [`measure`] says.
pub fn card(dir: &Path, iterations: Option<u32>) -> Result<Card> {
    let key = device_key(dir)?;
    if iterations.is_none() {
        if let Some(card) = kept(&key) {
            return Ok(Card { path: dir.to_path_buf(), ..card });
        }
    }
    let mut options = Options::default();
    if let Some(iterations) = iterations {
        options.iterations = iterations;
    }
    let card = measure(dir, &options)?;
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    kept.retain(|(have, _)| *have != key);
    kept.push((key, card.clone()));
    Ok(card)
}

/// The cards this process has, one per device.
static KEPT: Mutex<Vec<(String, Card)>> = Mutex::new(Vec::new());

/// The card this process has for the device `key` names, measured here or read out of a database
/// file on that device.
#[must_use]
pub fn kept(key: &str) -> Option<Card> {
    let kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    kept.iter().find(|(have, _)| have == key).map(|(_, card)| card.clone())
}

/// Takes a card a database file kept for the device `key` names, unless this process already has
/// one for it. A card measured here is newer than any a file could hold, so it stays.
pub fn remember(key: &str, card: Card) {
    let mut kept = KEPT.lock().unwrap_or_else(PoisonError::into_inner);
    if !kept.iter().any(|(have, _)| have == key) {
        kept.push((key.to_string(), card));
    }
}

/// What names the device a directory is on, which on Unix is the device number and elsewhere is
/// the directory itself.
///
/// # Errors
///
/// When the directory cannot be looked at.
pub fn device_key(dir: &Path) -> Result<String> {
    let metadata = std::fs::metadata(dir)
        .map_err(|e| Error::io(format!("device card: {}: {e}", dir.display())))?;
    #[cfg(unix)]
    let key = format!("dev:{}", std::os::unix::fs::MetadataExt::dev(&metadata));
    #[cfg(not(unix))]
    let key = {
        let _ = metadata;
        dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).display().to_string()
    };
    Ok(key)
}

/// The lane rule of `09-the-log.md` section 9.2.
///
/// More than one lane only when a sync is a fast queue round trip, the drive keeps what it
/// acknowledged, and syncs from different threads really do run at once. Anything else gets one
/// lane with large groups, which is the most a drive that flushes its whole cache per sync can do.
#[must_use]
pub fn lanes(p50_4k_ns: u64, plp: Plp, scaling: f64, workers: usize) -> u32 {
    if p50_4k_ns <= 100_000 && plp == Plp::Yes && scaling >= 0.7 {
        u32::try_from(workers.clamp(1, 16)).unwrap_or(16)
    } else {
        1
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "syncs a second on a real device are far under the 2^53 a double holds exactly"
)]
fn ratio(top: u64, bottom: u64) -> f64 {
    if bottom == 0 { 0.0 } else { top as f64 / bottom as f64 }
}

/// Sorted samples in, the value at `q` out, with the same upward rounding a histogram uses.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a sample count is far under 2^53 and the product is a positive index under it"
)]
fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Write `size` bytes at a sequential offset and make them durable with `call`, `iterations`
/// times, and return the time each took, sorted.
fn timed(file: &File, call: SyncCall, size: usize, iterations: u32) -> Result<Vec<u64>> {
    let block = vec![0x5a_u8; size];
    let mut offset = 0;
    // One untimed round first, so the first sample is not also the one that faults the page in.
    write_and_sync(file, call, &block, 0)?;
    let mut samples = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        offset = (offset + size as u64) % (SCRATCH - size as u64);
        let start = Instant::now();
        write_and_sync(file, call, &block, offset)?;
        samples.push(nanos(start.elapsed()));
    }
    samples.sort_unstable();
    Ok(samples)
}

fn nanos(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
}

fn write_and_sync(file: &File, call: SyncCall, block: &[u8], offset: u64) -> Result<()> {
    if call == SyncCall::DsyncWrite {
        return platform::dsync_write(file, block, offset);
    }
    write_all_at(file, block, offset)?;
    platform::sync(file, call)
}

/// The sequential write bandwidth of the directory's device, in bytes a second.
fn bandwidth(dir: &Path, bytes: u64) -> Result<u64> {
    let scratch = Scratch::new(dir, "bandwidth")?;
    let file = scratch.create()?;
    let block = vec![0xa5_u8; 1 << 20];
    let start = Instant::now();
    let mut written = 0;
    while written < bytes {
        write_all_at(&file, &block, written)?;
        written += block.len() as u64;
    }
    platform::sync(&file, SyncCall::full())?;
    let spent = start.elapsed().as_secs_f64();
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a byte count and a rate on a real device are far under 2^53, and both are positive"
    )]
    let rate = if spent > 0.0 { (written as f64 / spent) as u64 } else { 0 };
    Ok(rate)
}

/// Syncs a second with `threads` threads each writing and syncing its own file for `window`.
fn parallel(dir: &Path, threads: usize, window: Duration) -> Result<u64> {
    let call = SyncCall::full();
    let mut scratches = Vec::with_capacity(threads);
    let mut files = Vec::with_capacity(threads);
    for thread in 0..threads {
        let scratch = Scratch::new(dir, &format!("lane{thread}"))?;
        files.push(scratch.preallocated(4 << 20)?);
        scratches.push(scratch);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let handles: Vec<_> = files
        .into_iter()
        .map(|file| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || -> Result<u64> {
                let block = vec![0x3c_u8; 4 << 10];
                let mut done = 0u64;
                let mut offset = 0;
                while !stop.load(Ordering::Relaxed) {
                    write_and_sync(&file, call, &block, offset)?;
                    offset = (offset + 4096) % (4 << 20);
                    done += 1;
                }
                Ok(done)
            })
        })
        .collect();
    std::thread::sleep(window);
    stop.store(true, Ordering::Relaxed);
    let mut total = 0;
    for handle in handles {
        total += handle.join().map_err(|_| Error::internal("a device card thread panicked"))??;
    }
    let spent = start.elapsed().as_secs_f64();
    drop(scratches);
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a sync count is far under 2^53 and the rate is positive"
    )]
    let rate = if spent > 0.0 { (total as f64 / spent) as u64 } else { 0 };
    Ok(rate)
}

fn write_all_at(file: &File, mut block: &[u8], mut offset: u64) -> Result<()> {
    while !block.is_empty() {
        #[cfg(unix)]
        let written = std::os::unix::fs::FileExt::write_at(file, block, offset);
        #[cfg(windows)]
        let written = std::os::windows::fs::FileExt::seek_write(file, block, offset);
        let n = written
            .map_err(|e| Error::io(format!("device card: write at {offset} failed: {e}")))?;
        if n == 0 {
            return Err(Error::io("device card: a write wrote nothing"));
        }
        block = &block[n..];
        offset += n as u64;
    }
    Ok(())
}

/// A scratch file in the probed directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(dir: &Path, tag: &str) -> Result<Self> {
        let name = format!(".rudb-device-card-{}-{tag}", std::process::id());
        Ok(Self(dir.join(name)))
    }

    fn create(&self) -> Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.0)
            .map_err(|e| Error::io(format!("device card: {}: {e}", self.0.display())))
    }

    /// Created and written through once and synced, so every later write lands on blocks the file
    /// already owns.
    fn preallocated(&self, bytes: u64) -> Result<File> {
        let file = self.create()?;
        let block = vec![0_u8; 1 << 20];
        let mut at = 0;
        while at < bytes {
            write_all_at(&file, &block, at)?;
            at += block.len() as u64;
        }
        file.sync_all().map_err(|e| Error::io(format!("device card: {e}")))?;
        Ok(file)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs::File;
    use std::path::Path;

    use rudb_common::{Error, Result};

    use super::{Plp, SyncCall};

    pub(super) fn sync(file: &File, call: SyncCall) -> Result<()> {
        let done = match call {
            SyncCall::Fdatasync => file.sync_data(),
            _ => file.sync_all(),
        };
        done.map_err(|e| Error::io(format!("device card: {} failed: {e}", call.name())))
    }

    #[cfg(target_env = "gnu")]
    #[allow(unsafe_code, reason = "pwritev2 has no wrapper in std")]
    pub(super) fn dsync_write(file: &File, block: &[u8], offset: u64) -> Result<()> {
        use std::ffi::{c_int, c_void};
        use std::os::fd::AsRawFd;

        #[repr(C)]
        struct IoVec {
            base: *const c_void,
            len: usize,
        }
        unsafe extern "C" {
            fn pwritev2(
                fd: c_int,
                iov: *const IoVec,
                count: c_int,
                offset: i64,
                flags: c_int,
            ) -> isize;
        }
        const RWF_DSYNC: c_int = 0x2;
        let iov = IoVec { base: block.as_ptr().cast(), len: block.len() };
        let at = i64::try_from(offset).map_err(|_| Error::io("device card: offset too large"))?;
        // SAFETY: the descriptor is open for as long as `file` is borrowed, and the one vector
        // points at `block`, which is live and at least `len` bytes for the length of the call.
        let n = unsafe { pwritev2(file.as_raw_fd(), &raw const iov, 1, at, RWF_DSYNC) };
        if usize::try_from(n).ok() != Some(block.len()) {
            return Err(Error::io(format!(
                "device card: RWF_DSYNC failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    #[cfg(not(target_env = "gnu"))]
    pub(super) fn dsync_write(_: &File, _: &[u8], _: u64) -> Result<()> {
        Err(Error::io("device card: RWF_DSYNC is not available on this build"))
    }

    /// The mount the directory is on, as its source device and file system type, from the longest
    /// mount point in `/proc/self/mountinfo` that the directory sits under.
    pub(super) fn mount(dir: &Path) -> (String, String) {
        let Ok(dir) = dir.canonicalize() else { return (String::new(), String::new()) };
        let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return (String::new(), String::new());
        };
        best_mount(&info, &dir)
    }

    pub(super) fn best_mount(info: &str, dir: &Path) -> (String, String) {
        let mut best: Option<(usize, String, String)> = None;
        for line in info.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let Some(dash) = fields.iter().position(|field| *field == "-") else { continue };
            if fields.len() < dash + 3 || fields.len() < 5 {
                continue;
            }
            let point = unescape(fields[4]);
            if !dir.starts_with(&point) {
                continue;
            }
            let depth = point.len();
            if best.as_ref().is_none_or(|(have, ..)| depth >= *have) {
                best = Some((depth, fields[dash + 2].to_string(), fields[dash + 1].to_string()));
            }
        }
        best.map(|(_, device, kind)| (device, kind)).unwrap_or_default()
    }

    /// `/proc/self/mountinfo` writes a space in a path as `\040`, and the other three it escapes
    /// the same way.
    fn unescape(field: &str) -> String {
        field
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\")
    }

    pub(super) fn plp(device: &str) -> Plp {
        let virtualised = std::fs::read_to_string("/proc/cpuinfo").is_ok_and(|info| {
            info.lines().any(|line| line.starts_with("flags") && line.contains(" hypervisor"))
        });
        if virtualised {
            return Plp::Unknown;
        }
        let Some(name) = device.strip_prefix("/dev/") else { return Plp::Unknown };
        // A partition's directory sits inside its disk's, and only the disk has a queue.
        let Ok(node) = std::fs::canonicalize(format!("/sys/class/block/{name}")) else {
            return Plp::Unknown;
        };
        for dir in [node.as_path(), node.parent().unwrap_or(&node)] {
            if let Ok(cache) = std::fs::read_to_string(dir.join("queue/write_cache")) {
                return match cache.trim() {
                    "write through" => Plp::Yes,
                    "write back" => Plp::No,
                    _ => Plp::Unknown,
                };
            }
        }
        Plp::Unknown
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[allow(unsafe_code, reason = "the flush calls and statfs are C calls with no wrapper in std")]
mod platform {
    use std::ffi::{c_char, c_int};
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use rudb_common::{Error, Result};

    use super::{Plp, SyncCall};

    const F_FULLFSYNC: c_int = 51;
    const F_BARRIERFSYNC: c_int = 85;

    /// `struct statfs` with 64 bit inodes, which is the only layout on Apple silicon and the one
    /// the `$INODE64` symbol fills on Intel.
    #[repr(C)]
    struct StatFs {
        bsize: u32,
        iosize: i32,
        blocks: u64,
        bfree: u64,
        bavail: u64,
        files: u64,
        ffree: u64,
        fsid: [i32; 2],
        owner: u32,
        kind: u32,
        flags: u32,
        fssubtype: u32,
        fstypename: [c_char; 16],
        mntonname: [c_char; 1024],
        mntfromname: [c_char; 1024],
        flags_ext: u32,
        reserved: [u32; 7],
    }

    unsafe extern "C" {
        fn fsync(fd: c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        #[cfg_attr(target_arch = "x86_64", link_name = "statfs$INODE64")]
        fn statfs(path: *const c_char, buf: *mut StatFs) -> c_int;
    }

    pub(super) fn sync(file: &File, call: SyncCall) -> Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: the descriptor is open for as long as `file` is borrowed, and neither call reads
        // or writes memory of ours.
        let rc = unsafe {
            match call {
                SyncCall::FullFsync => fcntl(fd, F_FULLFSYNC),
                SyncCall::BarrierFsync => fcntl(fd, F_BARRIERFSYNC),
                _ => fsync(fd),
            }
        };
        if rc == -1 {
            return Err(Error::io(format!(
                "device card: {} failed: {}",
                call.name(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    pub(super) fn dsync_write(_: &File, _: &[u8], _: u64) -> Result<()> {
        Err(Error::io("device card: RWF_DSYNC is Linux only"))
    }

    fn text(field: &[c_char]) -> String {
        let bytes: Vec<u8> =
            field.iter().take_while(|c| **c != 0).map(|c| c.to_ne_bytes()[0]).collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub(super) fn mount(dir: &Path) -> (String, String) {
        let mut path = dir.as_os_str().as_bytes().to_vec();
        path.push(0);
        // SAFETY: an all zero `StatFs` is a valid value of a struct of integers and byte arrays.
        let mut buf: StatFs = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is nul terminated and outlives the call, and `buf` is a live struct of the
        // layout the call fills.
        let rc = unsafe { statfs(path.as_ptr().cast(), &raw mut buf) };
        if rc != 0 {
            return (String::new(), String::new());
        }
        (text(&buf.mntfromname), text(&buf.fstypename))
    }

    pub(super) fn plp(_: &str) -> Plp {
        Plp::Unknown
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
mod platform {
    use std::fs::File;
    use std::path::Path;

    use rudb_common::{Error, Result};

    use super::{Plp, SyncCall};

    pub(super) fn sync(file: &File, call: SyncCall) -> Result<()> {
        file.sync_all().map_err(|e| Error::io(format!("device card: {} failed: {e}", call.name())))
    }

    pub(super) fn dsync_write(_: &File, _: &[u8], _: u64) -> Result<()> {
        Err(Error::io("device card: RWF_DSYNC is Linux only"))
    }

    pub(super) fn mount(_: &Path) -> (String, String) {
        (String::new(), String::new())
    }

    pub(super) fn plp(_: &str) -> Plp {
        Plp::Unknown
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use std::path::Path;

    use super::{Card, Options, Plp, SyncCall, SyncProbe, lanes, measure, quantile};
    use crate::scratch::TempDir;

    #[test]
    fn the_quantile_rounds_up_to_a_sample() {
        let samples: Vec<u64> = (1..=100).collect();
        assert_eq!(quantile(&samples, 0.5), 50);
        assert_eq!(quantile(&samples, 0.99), 99);
        assert_eq!(quantile(&samples, 1.0), 100);
        assert_eq!(quantile(&[7], 0.99), 7);
        assert_eq!(quantile(&[], 0.5), 0);
    }

    #[test]
    fn the_lane_rule_wants_all_three() {
        assert_eq!(lanes(30_000, Plp::Yes, 0.9, 32), 16);
        assert_eq!(lanes(30_000, Plp::Yes, 0.9, 4), 4);
        assert_eq!(lanes(30_000, Plp::Unknown, 0.9, 32), 1);
        assert_eq!(lanes(30_000, Plp::Yes, 0.5, 32), 1);
        assert_eq!(lanes(3_347_000, Plp::Yes, 0.9, 32), 1);
    }

    #[test]
    fn a_card_comes_back_from_its_bytes() {
        let card = Card {
            path: "/a".into(),
            device: "/dev/nvme0n1p2".to_string(),
            filesystem: "ext4".to_string(),
            memory_backed: false,
            probes: vec![SyncProbe {
                call: SyncCall::Fdatasync,
                p50_4k_ns: 30_000,
                p99_4k_ns: 90_000,
                p50_64k_ns: 60_000,
                p99_64k_ns: 150_000,
                plausible: true,
            }],
            write_bytes_per_s: 2 << 30,
            syncs_per_s: [30_000, 55_000, 90_000, 120_000],
            scaling: 0.5,
            plp: Plp::Unknown,
            lanes: 1,
            iterations: 200,
        };
        let bytes = card.encode();
        let back = Card::decode(&bytes, Path::new("/b")).expect("decodes");
        assert_eq!(back, Card { path: "/b".into(), ..card });
        assert!(Card::decode(&bytes[..bytes.len() - 1], Path::new("/b")).is_err());
        let mut newer = bytes.clone();
        newer[0] = 9;
        assert!(Card::decode(&newer, Path::new("/b")).is_err());
    }

    #[test]
    fn a_small_card_measures_every_call_and_cleans_up() {
        let dir = TempDir::new("card");
        let options = Options {
            iterations: 5,
            bandwidth_bytes: 4 << 20,
            scaling_window: Duration::from_millis(20),
            workers: 4,
        };
        let card = measure(dir.path(), &options).expect("the temporary directory can be probed");
        let calls: Vec<SyncCall> = card.probes.iter().map(|probe| probe.call).collect();
        assert_eq!(calls, SyncCall::candidates());
        for probe in &card.probes {
            assert!(probe.p50_4k_ns > 0 && probe.p99_4k_ns >= probe.p50_4k_ns, "{probe:?}");
            assert!(probe.p99_64k_ns >= probe.p50_64k_ns, "{probe:?}");
        }
        assert!(card.write_bytes_per_s > 0);
        assert!(card.syncs_per_s.iter().all(|rate| *rate > 0), "{:?}", card.syncs_per_s);
        assert!(card.lanes >= 1);
        if card.memory_backed {
            assert!(!card.plausible());
        }
        let left = std::fs::read_dir(dir.path()).expect("the directory is still there").count();
        assert_eq!(left, 0, "the probe left its scratch files behind");
    }

    #[test]
    fn a_path_that_is_not_a_directory_is_refused() {
        let dir = TempDir::new("card-missing");
        let options = Options::default();
        assert!(measure(&dir.join("nothing-here"), &options).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_deepest_mount_wins() {
        let info = "22 1 8:2 / / rw - ext4 /dev/sda2 rw\n\
                    40 22 0:40 / /tmp rw - tmpfs tmpfs rw\n\
                    41 22 8:3 / /home/a\\040b rw - xfs /dev/sdb1 rw\n";
        let find = |p: &str| super::platform::best_mount(info, Path::new(p));
        assert_eq!(find("/tmp/x"), ("tmpfs".into(), "tmpfs".into()));
        assert_eq!(find("/home/a b/db"), ("/dev/sdb1".into(), "xfs".into()));
        assert_eq!(find("/var/lib"), ("/dev/sda2".into(), "ext4".into()));
    }
}
